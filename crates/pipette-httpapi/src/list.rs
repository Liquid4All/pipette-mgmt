//! The cursor-paginated listing shape, the loop that fills one page, and the
//! query readers every list route shares.

use std::num::NonZeroUsize;

use pipette_core::config::Config;
use pipette_plan_types::ListPage;
use pipette_store::{DRAIN_PAGE_SIZE, Page, UnreadableObjectError};

use crate::error::AppError;

/// One page of `limit` rows that pass `admits`, resumed from `after`. Asks the
/// store until the page is full, since a short filtered page reads as the end.
/// The cursor is the store's `resume_after`: the last key *examined*.
pub async fn page_filtered<T, Id, F, Fut>(
    limit: NonZeroUsize,
    after: Option<Id>,
    what: &str,
    admits: impl Fn(&T) -> bool,
    mut fetch: F,
) -> Result<ListPage<T>, AppError>
where
    Id: std::fmt::Display,
    F: FnMut(Option<Id>, NonZeroUsize) -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<Page<T, Id>>>,
{
    let mut after = after;
    let mut items: Vec<T> = Vec::new();
    let mut unreadable = 0usize;

    loop {
        // The loop breaks on a full page, so a zero remainder is unreachable;
        // `MIN` keeps the type honest without panicking if that stops holding.
        let remaining =
            NonZeroUsize::new(limit.get().saturating_sub(items.len())).unwrap_or(NonZeroUsize::MIN);
        let page = fetch(after, remaining)
            .await
            .map_err(|e| AppError::internal(e.context(format!("failed to list the {what}"))))?;
        log_unreadable(what, &page.unreadable);
        unreadable = unreadable.saturating_add(page.unreadable.len());
        items.extend(page.items.into_iter().filter(&admits));

        if items.len() >= limit.get() || page.resume_after.is_none() {
            return Ok(ListPage {
                items,
                next_cursor: page.resume_after.map(|cursor| cursor.to_string()),
                unreadable,
            });
        }
        after = page.resume_after;
    }
}

/// One newest-first page from a keyspace drained whole: the store pages only
/// ascending. The cursor is an exclusive upper bound, so it outlives its row.
pub async fn page_newest<T, Id, F, Fut>(
    limit: NonZeroUsize,
    before: Option<Id>,
    what: &str,
    admits: impl Fn(&T) -> bool,
    key_of: impl Fn(&T) -> &Id,
    mut fetch: F,
) -> Result<NewestPage<T>, AppError>
where
    T: Clone,
    Id: Ord + std::fmt::Display,
    F: FnMut(Option<Id>, NonZeroUsize) -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<Page<T, Id>>>,
{
    let mut after = None;
    let mut drained = Vec::new();
    let mut unreadable = 0usize;
    loop {
        let page = fetch(after, DRAIN_PAGE_SIZE)
            .await
            .map_err(|e| AppError::internal(e.context(format!("failed to list the {what}"))))?;
        log_unreadable(what, &page.unreadable);
        unreadable = unreadable.saturating_add(page.unreadable.len());
        drained.extend(page.items);
        match page.resume_after {
            Some(cursor) => after = Some(cursor),
            None => break,
        }
    }
    let page = newest_page(&drained, unreadable, limit, before.as_ref(), admits, key_of);
    Ok(NewestPage { page, drained })
}

/// A newest-first page, and the keyspace it was cut from, for a caller that
/// judges its rows against every record.
pub struct NewestPage<T> {
    pub page: ListPage<T>,
    pub drained: Vec<T>,
}

/// Only the count travels to the caller: a key names storage it has no
/// business seeing, so the key and the reason go to the log.
fn log_unreadable(what: &str, unreadable: &[UnreadableObjectError]) {
    unreadable.iter().for_each(|bad| {
        tracing::warn!(
            listing = what,
            path = %bad.key,
            error = %bad.reason,
            "unreadable record in a listing"
        );
    });
}

/// The pure half of [`page_newest`]: `drained` in ascending key order, read
/// from its end.
pub fn newest_page<T, Id>(
    drained: &[T],
    unreadable: usize,
    limit: NonZeroUsize,
    before: Option<&Id>,
    admits: impl Fn(&T) -> bool,
    key_of: impl Fn(&T) -> &Id,
) -> ListPage<T>
where
    T: Clone,
    Id: Ord + std::fmt::Display,
{
    let first = before.is_none();
    let mut rows = drained
        .iter()
        .rev()
        .filter(|row| before.is_none_or(|bound| key_of(row) < bound))
        .filter(|row| admits(row));
    let items: Vec<T> = rows.by_ref().take(limit.get()).cloned().collect();
    // A cursor only where a row is left, so the last page says it is the last.
    let next_cursor = rows
        .next()
        .and_then(|_| items.last())
        .map(|last| key_of(last).to_string());
    ListPage {
        items,
        next_cursor,
        unreadable: if first { unreadable } else { 0 },
    }
}

pub fn list_limit(requested: Option<usize>, config: &Config) -> Result<NonZeroUsize, AppError> {
    let value = requested.unwrap_or_else(|| config.list_default_limit.get());
    let limit = NonZeroUsize::new(value)
        .ok_or_else(|| AppError::BadRequest("limit must be greater than zero".to_string()))?;
    if limit > config.list_max_limit {
        return Err(AppError::BadRequest(format!(
            "limit must not exceed {}",
            config.list_max_limit
        )));
    }
    Ok(limit)
}

/// A malformed cursor is refused as a cursor: the key's own error would name an
/// id the caller never sent.
pub fn list_cursor<Id, E>(
    requested: Option<String>,
    parse: impl FnOnce(String) -> Result<Id, E>,
) -> Result<Option<Id>, AppError> {
    requested.map(parse).transpose().map_err(|_| {
        AppError::BadRequest("`cursor` is not a `next_cursor` this listing issued".into())
    })
}

/// The `[offset, offset + limit)` window of `items`. An offset past the end is
/// an empty page, as for a listing that shrank underneath its caller.
pub fn page_of<T>(items: &[T], offset: usize, limit: usize) -> &[T] {
    let rest = items.get(offset..).unwrap_or_default();
    rest.get(..limit).unwrap_or(rest)
}

#[cfg(test)]
mod tests {
    use anyhow::Context;
    use rstest::rstest;

    use pipette_store::UnreadableObjectError;

    use super::*;

    fn limit(n: usize) -> anyhow::Result<NonZeroUsize> {
        NonZeroUsize::new(n).context("a test limit is never zero")
    }

    /// One store page over the keyspace `0..n`, as a real store answers it:
    /// `after` is exclusive, the limit bounds the keys *examined*, and
    /// `resume_after` names the last of those rather than the last row returned.
    fn keyspace_page(after: Option<u32>, examine: NonZeroUsize, n: u32) -> Page<u32, u32> {
        let first = after.map_or(0, |key| key + 1);
        let items: Vec<u32> = (first..n).take(examine.get()).collect();
        let examined = items.last().copied();
        Page {
            resume_after: examined.filter(|key| key + 1 < n),
            items,
            unreadable: Vec::new(),
        }
    }

    /// Whether a row is admitted, as a case can carry it.
    type Admits = fn(&u32) -> bool;

    #[rstest]
    // One row in ten: several store pages carry nothing, which must not end the
    // page.
    #[case::filled_past_an_empty_page(5, None, 100, (|n| n.is_multiple_of(10)) as Admits, vec![0, 10, 20, 30, 40], Some("40"))]
    // The end of the keyspace offers no cursor, short page or not.
    #[case::the_end_offers_no_cursor(5, None, 3, (|_| true) as Admits, vec![0, 1, 2], None)]
    // A cursor resumes *after* the key it names, never at it.
    #[case::a_cursor_resumes_after_its_key(2, Some(5), 10, (|_| true) as Admits, vec![6, 7], Some("7"))]
    #[tokio::test]
    async fn page_filtered_walks_the_keyspace(
        #[case] wanted: usize,
        #[case] after: Option<u32>,
        #[case] keys: u32,
        #[case] admits: Admits,
        #[case] items: Vec<u32>,
        #[case] next_cursor: Option<&str>,
    ) -> anyhow::Result<()> {
        let page = page_filtered(
            limit(wanted)?,
            after,
            "numbers",
            admits,
            |after, examine| async move { Ok(keyspace_page(after, examine, keys)) },
        )
        .await?;
        assert_eq!(page.items, items);
        assert_eq!(page.next_cursor.as_deref(), next_cursor);
        assert_eq!(page.unreadable, 0);
        Ok(())
    }

    #[rstest]
    #[case::the_first_page(None, 3, (|_| true) as Admits, vec![9, 8, 7], Some("7"), 2)]
    // The cursor's own row is gone or filtered out: the bound still holds.
    #[case::a_cursor_whose_row_left(Some(5), 3, (|n| *n != 5) as Admits, vec![4, 3, 2], Some("2"), 0)]
    #[case::filtered(None, 2, (|n| n.is_multiple_of(4)) as Admits, vec![8, 4], Some("4"), 2)]
    // A full page that ends the keyspace says so.
    #[case::the_last_page_is_full(Some(3), 3, (|_| true) as Admits, vec![2, 1, 0], None, 0)]
    #[case::past_the_start(Some(0), 3, (|_| true) as Admits, vec![], None, 0)]
    fn newest_page_reads_the_keyspace_backwards(
        #[case] before: Option<u32>,
        #[case] wanted: usize,
        #[case] admits: Admits,
        #[case] items: Vec<u32>,
        #[case] next_cursor: Option<&str>,
        #[case] unreadable: usize,
    ) -> anyhow::Result<()> {
        let keyspace: Vec<u32> = (0..10).collect();
        let page = newest_page(&keyspace, 2, limit(wanted)?, before.as_ref(), admits, |n| n);
        assert_eq!(page.items, items);
        assert_eq!(page.next_cursor.as_deref(), next_cursor);
        // Counted on the first page alone, so the pages still sum to the keyspace.
        assert_eq!(page.unreadable, unreadable);
        Ok(())
    }

    /// The drain reads every store page, oldest first, and answers newest first.
    #[tokio::test]
    async fn page_newest_drains_the_keyspace() -> anyhow::Result<()> {
        let page = page_newest(
            limit(2)?,
            None,
            "numbers",
            |_: &u32| true,
            |n| n,
            |after, examine| async move {
                let mut page = keyspace_page(after, examine.min(limit(3)?), 7);
                page.unreadable.push(UnreadableObjectError {
                    key: "numbers/broken.json".into(),
                    reason: "not a number".into(),
                });
                Ok(page)
            },
        )
        .await?;
        assert_eq!(page.drained, (0..7).collect::<Vec<u32>>());
        let page = page.page;
        assert_eq!(page.items, vec![6, 5]);
        assert_eq!(page.next_cursor.as_deref(), Some("5"));
        // Three store pages of three keys, one unreadable object each.
        assert_eq!(page.unreadable, 3);
        Ok(())
    }

    #[tokio::test]
    async fn unreadable_objects_are_counted_across_pages() -> anyhow::Result<()> {
        let page = page_filtered(
            limit(2)?,
            None,
            "numbers",
            |n: &u32| n.is_multiple_of(2),
            |after, examine| async move {
                let mut page = keyspace_page(after, examine, 10);
                page.unreadable.push(UnreadableObjectError {
                    key: "numbers/broken.json".into(),
                    reason: "not a number".into(),
                });
                Ok(page)
            },
        )
        .await?;
        assert_eq!(page.items, vec![0, 2]);
        // Two store pages, one unreadable object each.
        assert_eq!(page.unreadable, 2);
        Ok(())
    }

    #[tokio::test]
    async fn a_store_failure_names_what_was_being_listed() -> anyhow::Result<()> {
        let failed = page_filtered(
            limit(1)?,
            None,
            "widgets",
            |_: &u32| true,
            |_, _| async { Err::<Page<u32, u32>, _>(anyhow::anyhow!("the disk went away")) },
        )
        .await
        .err()
        .context("a failing store must not answer a page")?;
        assert_eq!(
            failed.status(),
            axum::http::StatusCode::INTERNAL_SERVER_ERROR
        );
        let recorded = failed.to_string();
        assert!(
            recorded.contains("failed to list the widgets"),
            "{recorded}"
        );
        assert!(recorded.contains("the disk went away"), "{recorded}");
        assert_eq!(failed.message(), "internal server error");
        Ok(())
    }

    #[rstest]
    // Whole slice, exact fit, and the tail short of a full page.
    #[case(0, 10, &[1, 2, 3])]
    #[case(0, 3, &[1, 2, 3])]
    #[case(1, 2, &[2, 3])]
    #[case(2, 5, &[3])]
    // Past the end, and a zero-width page: empty, never a panic.
    #[case(3, 5, &[])]
    #[case(99, 5, &[])]
    #[case(0, 0, &[])]
    fn page_of_windows(#[case] offset: usize, #[case] limit: usize, #[case] expected: &[i32]) {
        assert_eq!(page_of(&[1, 2, 3], offset, limit), expected);
    }

    /// `offset + limit` near the integer ceiling saturates instead of wrapping,
    /// which is what keeps a hostile query string from indexing backwards.
    #[test]
    fn page_of_saturates() {
        assert_eq!(page_of(&[1, 2, 3], 1, usize::MAX), &[2, 3]);
    }
}
