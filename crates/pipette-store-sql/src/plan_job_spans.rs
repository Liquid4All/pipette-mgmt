//! Plan membership as spans. A `plan_job_spans` row says that the plans of a
//! lineage from `from_plan` up to `to_plan` (exclusive) list a job with the same
//! per-job fields. `put_plan` writes the spans in the plan transaction.
//!
//! A lineage is the plans of one campaign. A plan with no campaign is a lineage
//! of its own, named by its plan id. Plan ids sort in creation order, so the span
//! bounds are plan ids.
//!
//! Each transaction that writes the spans of a lineage first writes the
//! lineage's `plan_lineages` row. At `REPEATABLE READ`, two such transactions
//! conflict with `40001` if their views overlap. Then `retry` repeats the failed
//! transaction with the committed spans visible.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::Value;
use sqlx::PgConnection;

/// The lineage of a plan.
pub(crate) fn lineage_of<'a>(plan_id: &'a str, campaign_id: Option<&'a str>) -> &'a str {
    campaign_id.unwrap_or(plan_id)
}

/// The per-job fields of a span: the manifest's per-job values for a job.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Fields {
    pub(crate) ordinal: Option<i32>,
    pub(crate) content_key: Option<String>,
    pub(crate) rerun_of: Option<String>,
    pub(crate) expired: Option<Expired>,
    pub(crate) outcome: Option<String>,
}

/// An `expired` value as JSON text. Equality follows JSON content, as with
/// `jsonb`. Key order and whitespace do not end a span.
#[derive(Clone, Debug)]
pub(crate) struct Expired {
    text: String,
    value: Value,
}

impl Expired {
    pub(crate) fn parse(text: String) -> serde_json::Result<Self> {
        let value = serde_json::from_str(&text)?;
        Ok(Self { text, value })
    }
}

impl PartialEq for Expired {
    fn eq(&self, other: &Self) -> bool {
        self.value == other.value
    }
}

/// The fields of each job of a manifest, by job id.
pub(crate) type ManifestJobs = BTreeMap<String, Fields>;

/// One `plan_job_spans` row of a lineage.
#[derive(Clone, Debug, PartialEq)]
struct Span {
    job_id: String,
    from_plan: String,
    to_plan: Option<String>,
    fields: Fields,
}

/// The rows that [`apply`] changes.
#[derive(Debug, Default, PartialEq)]
struct SpanWrites {
    /// The `(job_id, from_plan)` key of each row to delete.
    deleted: Vec<(String, String)>,
    /// Each row to insert, or to update by its key.
    written: Vec<Span>,
}

/// The spans of one job near a plan.
#[derive(Default)]
struct Near {
    /// The span that lists the plan.
    cover: Option<Span>,
    /// The span that ends at the plan.
    before: Option<Span>,
    /// The span that starts at the next plan of the lineage.
    after: Option<Span>,
}

/// Apply the jobs of plan `plan` to the spans of its lineage. `next` is the
/// smallest plan id of the lineage that is greater than `plan`. `read` holds
/// the spans that list `plan`, and the spans of the manifest's jobs that end at
/// `plan` or start at `next`.
///
/// A span whose job the manifest lists with equal fields stays as it is. The
/// function removes `plan` from each other span that lists it. It gives each
/// manifest job without a matching span the interval `[plan, next)`. That span
/// joins an adjacent span of the job with equal fields.
fn apply(plan: &str, next: Option<&str>, read: Vec<Span>, manifest: &ManifestJobs) -> SpanWrites {
    let mut near = read
        .into_iter()
        .fold(BTreeMap::<String, Near>::new(), |mut near, span| {
            let slot = near.entry(span.job_id.clone()).or_default();
            if span.to_plan.as_deref() == Some(plan) {
                slot.before = Some(span);
            } else if span.from_plan.as_str() > plan {
                slot.after = Some(span);
            } else {
                slot.cover = Some(span);
            }
            near
        });
    let jobs: BTreeSet<String> = manifest.keys().chain(near.keys()).cloned().collect();
    jobs.into_iter()
        .fold(SpanWrites::default(), |mut writes, job_id| {
            let Near {
                cover,
                mut before,
                mut after,
            } = near.remove(&job_id).unwrap_or_default();
            let want = manifest.get(&job_id);
            if cover.as_ref().map(|span| &span.fields) == want {
                return writes;
            }
            let old: Vec<Span> = [&cover, &before, &after]
                .into_iter()
                .flatten()
                .cloned()
                .collect();
            let mut new = Vec::new();
            if let Some(cover) = cover {
                if cover.from_plan.as_str() < plan {
                    new.push(Span {
                        to_plan: Some(plan.to_owned()),
                        ..cover.clone()
                    });
                }
                if let Some(next) = next
                    && cover.to_plan.as_deref().is_none_or(|to| next < to)
                {
                    new.push(Span {
                        from_plan: next.to_owned(),
                        ..cover
                    });
                }
            }
            if let Some(fields) = want {
                let from_plan = before
                    .take_if(|span| &span.fields == fields)
                    .map_or_else(|| plan.to_owned(), |span| span.from_plan);
                let to_plan = after
                    .take_if(|span| &span.fields == fields)
                    .map_or_else(|| next.map(str::to_owned), |span| span.to_plan);
                new.push(Span {
                    job_id: job_id.clone(),
                    from_plan,
                    to_plan,
                    fields: fields.clone(),
                });
            }
            new.extend(before);
            new.extend(after);
            writes.deleted.extend(
                old.iter()
                    .filter(|span| !new.iter().any(|kept| kept.from_plan == span.from_plan))
                    .map(|span| (job_id.clone(), span.from_plan.clone())),
            );
            writes
                .written
                .extend(new.into_iter().filter(|span| !old.contains(span)));
            writes
        })
}

/// Write the guard row of `lineage`. Run it first in each transaction that
/// writes the lineage's spans.
pub(crate) async fn guard(tx: &mut PgConnection, lineage: &str) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO plan_lineages (lineage, writes) VALUES ($1, 1)
         ON CONFLICT (lineage) DO UPDATE SET writes = plan_lineages.writes + 1",
    )
    .bind(lineage)
    .execute(tx)
    .await?;
    Ok(())
}

/// A `plan_job_spans` row as the span read gives it.
type SpanRow = (
    String,
    String,
    Option<String>,
    Option<i32>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
);

/// The columns of the span read. Each branch uses an index range:
///
/// - Open spans that list the plan.
/// - Closed spans that list the plan.
/// - Spans of the manifest's jobs that end at the plan.
/// - Spans of the manifest's jobs that start at the next plan.
const SPANS_NEAR: &str = "
    SELECT job_id, from_plan, to_plan, ordinal, content_key, rerun_of, expired::text, outcome
    FROM plan_job_spans WHERE lineage = $1 AND to_plan IS NULL AND from_plan <= $2
    UNION ALL
    SELECT job_id, from_plan, to_plan, ordinal, content_key, rerun_of, expired::text, outcome
    FROM plan_job_spans WHERE lineage = $1 AND to_plan > $2 AND from_plan <= $2
    UNION ALL
    SELECT job_id, from_plan, to_plan, ordinal, content_key, rerun_of, expired::text, outcome
    FROM plan_job_spans WHERE lineage = $1 AND to_plan = $2 AND job_id = ANY($3)
    UNION ALL
    SELECT job_id, from_plan, to_plan, ordinal, content_key, rerun_of, expired::text, outcome
    FROM plan_job_spans WHERE lineage = $1 AND from_plan = $4 AND job_id = ANY($3)";

/// What [`write_spans`] changed, for the log.
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct SpanCounts {
    pub(crate) deleted: usize,
    pub(crate) written: usize,
}

/// Apply the manifest jobs of `plan_id` to the spans of its lineage, in `tx`.
/// The caller wrote the guard row and the `plans` row first in `tx`.
pub(crate) async fn write_spans(
    tx: &mut PgConnection,
    plan_id: &str,
    campaign_id: Option<&str>,
    manifest: &ManifestJobs,
) -> Result<SpanCounts, sqlx::Error> {
    let lineage = lineage_of(plan_id, campaign_id);
    // A plan with no campaign is the only plan of its lineage.
    let next: Option<String> = match campaign_id {
        Some(campaign_id) => {
            sqlx::query_scalar(
                "SELECT min(plan_id) FROM plans WHERE campaign_id = $1 AND plan_id > $2",
            )
            .bind(campaign_id)
            .bind(plan_id)
            .fetch_one(&mut *tx)
            .await?
        }
        None => None,
    };
    let jobs: Vec<&str> = manifest.keys().map(String::as_str).collect();
    let rows: Vec<SpanRow> = sqlx::query_as(SPANS_NEAR)
        .bind(lineage)
        .bind(plan_id)
        .bind(jobs.as_slice())
        .bind(next.as_deref())
        .fetch_all(&mut *tx)
        .await?;
    let read = rows
        .into_iter()
        .map(
            |(job_id, from_plan, to_plan, ordinal, content_key, rerun_of, expired, outcome)| {
                Ok(Span {
                    job_id,
                    from_plan,
                    to_plan,
                    fields: Fields {
                        ordinal,
                        content_key,
                        rerun_of,
                        expired: expired.map(Expired::parse).transpose()?,
                        outcome,
                    },
                })
            },
        )
        .collect::<serde_json::Result<Vec<_>>>()
        .map_err(|e| sqlx::Error::Decode(Box::new(e)))?;
    let writes = apply(plan_id, next.as_deref(), read, manifest);

    if !writes.deleted.is_empty() {
        let (jobs, froms): (Vec<&str>, Vec<&str>) = writes
            .deleted
            .iter()
            .map(|(job, from)| (job.as_str(), from.as_str()))
            .unzip();
        sqlx::query(
            "DELETE FROM plan_job_spans
             WHERE lineage = $1
               AND (job_id, from_plan) IN (SELECT * FROM unnest($2::text[], $3::text[]))",
        )
        .bind(lineage)
        .bind(jobs.as_slice())
        .bind(froms.as_slice())
        .execute(&mut *tx)
        .await?;
    }
    if !writes.written.is_empty() {
        let spans = &writes.written;
        let column = |f: fn(&Span) -> Option<&str>| spans.iter().map(f).collect::<Vec<_>>();
        sqlx::query(
            "INSERT INTO plan_job_spans
                 (lineage, job_id, from_plan, to_plan, ordinal, content_key, rerun_of,
                  expired, outcome)
             SELECT $1, s.job_id, s.from_plan, s.to_plan, s.ordinal, s.content_key,
                    s.rerun_of, s.expired::jsonb, s.outcome
             FROM unnest($2::text[], $3::text[], $4::text[], $5::integer[], $6::text[],
                         $7::text[], $8::text[], $9::text[])
                 AS s (job_id, from_plan, to_plan, ordinal, content_key, rerun_of, expired,
                       outcome)
             ON CONFLICT (lineage, job_id, from_plan) DO UPDATE SET
                 to_plan = EXCLUDED.to_plan,
                 ordinal = EXCLUDED.ordinal,
                 content_key = EXCLUDED.content_key,
                 rerun_of = EXCLUDED.rerun_of,
                 expired = EXCLUDED.expired,
                 outcome = EXCLUDED.outcome",
        )
        .bind(lineage)
        .bind(column(|s| Some(s.job_id.as_str())))
        .bind(column(|s| Some(s.from_plan.as_str())))
        .bind(column(|s| s.to_plan.as_deref()))
        .bind(spans.iter().map(|s| s.fields.ordinal).collect::<Vec<_>>())
        .bind(column(|s| s.fields.content_key.as_deref()))
        .bind(column(|s| s.fields.rerun_of.as_deref()))
        .bind(column(|s| {
            s.fields.expired.as_ref().map(|e| e.text.as_str())
        }))
        .bind(column(|s| s.fields.outcome.as_deref()))
        .execute(&mut *tx)
        .await?;
    }
    Ok(SpanCounts {
        deleted: writes.deleted.len(),
        written: writes.written.len(),
    })
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use anyhow::Context;
    use futures::{StreamExt, TryStreamExt, stream};
    use rstest::rstest;

    use pipette_core::plan::{ExpiredJob, PlanManifest, SettledOutcome, sample_manifest};
    use pipette_plan_types::{CampaignId, JobId, PlanId, PlanStatus};
    use pipette_store::PlanStore as _;

    use super::*;
    use crate::db::SqlDatabase;
    use crate::plans::{SqlPlanStore, manifest_jobs, write_in};
    use crate::retry::is_conflict;
    use crate::testing::test_db_or_skip;

    const CAMPAIGN: Option<&str> = Some("campaign_0195c3e2-a4f3-7b8c-9d1e-2f3a4b5c6d7e");

    /// One `plan_job_spans` row.
    type StoredSpan = (
        String,
        String,
        String,
        Option<String>,
        Option<i32>,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
    );

    /// One `plan_job_rows` row of a plan: the job and its stored fields.
    type ViewRow = (
        String,
        Option<i32>,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
    );

    fn plan(n: u128) -> PlanId {
        PlanId::from_uuid(uuid::Uuid::from_u128(n))
    }

    /// Plan `n` of `campaign`, which lists `jobs` in order, each with a content
    /// key and no other field.
    fn manifest(n: u128, campaign: Option<&str>, jobs: &[&str]) -> anyhow::Result<PlanManifest> {
        let mut manifest = sample_manifest(plan(n), PlanStatus::Active, false)?;
        manifest.campaign_id = campaign.map(CampaignId::try_new).transpose()?;
        manifest.job_ids = jobs.iter().copied().map(JobId::new_unchecked).collect();
        manifest.content_keys = jobs
            .iter()
            .map(|job| (JobId::new_unchecked(*job), format!("key-{job}")))
            .collect();
        manifest.rerun_of.clear();
        manifest.expired_jobs.clear();
        manifest.settled_jobs.clear();
        Ok(manifest)
    }

    fn finished(mut manifest: PlanManifest, job: &str) -> PlanManifest {
        manifest
            .settled_jobs
            .insert(JobId::new_unchecked(job), SettledOutcome::Finished);
        manifest
    }

    fn lapsed(mut manifest: PlanManifest, job: &str) -> anyhow::Result<PlanManifest> {
        manifest.expired_jobs.insert(
            JobId::new_unchecked(job),
            ExpiredJob {
                horizon: "2026-08-19T17:55:00Z".parse()?,
                recorded_at: "2026-08-20T09:00:00Z".parse()?,
            },
        );
        Ok(manifest)
    }

    fn rerun(mut manifest: PlanManifest, job: &str, of: &str) -> PlanManifest {
        manifest
            .rerun_of
            .insert(JobId::new_unchecked(job), JobId::new_unchecked(of));
        manifest
    }

    fn carried() -> anyhow::Result<Vec<PlanManifest>> {
        Ok(vec![
            manifest(1, CAMPAIGN, &["a", "b"])?,
            manifest(2, CAMPAIGN, &["a", "b"])?,
            manifest(3, CAMPAIGN, &["a", "b"])?,
        ])
    }

    /// Plan 2 drops `c` and adds `d`. Plan 3 replaces `d` with its re-run `e`.
    fn dropped_added_replaced() -> anyhow::Result<Vec<PlanManifest>> {
        Ok(vec![
            manifest(1, CAMPAIGN, &["a", "b", "c"])?,
            manifest(2, CAMPAIGN, &["a", "b", "d"])?,
            rerun(manifest(3, CAMPAIGN, &["a", "b", "e"])?, "e", "d"),
        ])
    }

    fn changed_on_the_head() -> anyhow::Result<Vec<PlanManifest>> {
        Ok(vec![
            manifest(1, CAMPAIGN, &["a", "b"])?,
            manifest(2, CAMPAIGN, &["a", "b"])?,
            finished(manifest(2, CAMPAIGN, &["a", "b"])?, "a"),
            lapsed(finished(manifest(2, CAMPAIGN, &["a", "b"])?, "a"), "b")?,
        ])
    }

    fn changed_on_a_superseded_plan() -> anyhow::Result<Vec<PlanManifest>> {
        Ok(vec![
            manifest(1, CAMPAIGN, &["a", "b"])?,
            manifest(2, CAMPAIGN, &["a", "b"])?,
            manifest(3, CAMPAIGN, &["a", "b"])?,
            finished(manifest(2, CAMPAIGN, &["a", "b"])?, "a"),
        ])
    }

    /// Plan 2 commits after plan 3. It first drops `b`, then lists it again,
    /// which joins the spans on both sides of plan 2.
    fn written_after_a_greater_id() -> anyhow::Result<Vec<PlanManifest>> {
        Ok(vec![
            manifest(1, CAMPAIGN, &["a", "b"])?,
            manifest(3, CAMPAIGN, &["a", "b"])?,
            manifest(2, CAMPAIGN, &["a"])?,
            manifest(2, CAMPAIGN, &["a", "b"])?,
        ])
    }

    /// Two plans with no campaign, each a lineage of its own.
    fn without_a_campaign() -> anyhow::Result<Vec<PlanManifest>> {
        Ok(vec![
            manifest(11, None, &["a", "b"])?,
            finished(manifest(11, None, &["a", "b"])?, "a"),
            manifest(12, None, &["a"])?,
        ])
    }

    fn changed_and_changed_back() -> anyhow::Result<Vec<PlanManifest>> {
        Ok(vec![
            manifest(1, CAMPAIGN, &["a"])?,
            manifest(2, CAMPAIGN, &["a"])?,
            manifest(3, CAMPAIGN, &["a"])?,
            finished(manifest(2, CAMPAIGN, &["a"])?, "a"),
            manifest(2, CAMPAIGN, &["a"])?,
        ])
    }

    async fn stored_spans(db: &SqlDatabase) -> anyhow::Result<Vec<StoredSpan>> {
        Ok(sqlx::query_as(
            "SELECT lineage, job_id, from_plan, to_plan, ordinal, content_key, rerun_of,
                    expired::text, outcome
             FROM plan_job_spans ORDER BY lineage, job_id, from_plan",
        )
        .fetch_all(db.pool())
        .await?)
    }

    /// `get_plan` reads back each manifest in `expected`, and the
    /// `plan_job_rows` view lists the stored fields of each of its jobs.
    async fn assert_reads_back(
        store: &SqlPlanStore,
        expected: &BTreeMap<PlanId, PlanManifest>,
    ) -> anyhow::Result<()> {
        for (id, manifest) in expected {
            assert_eq!(store.get_plan(id).await?.as_ref(), Some(manifest));
            let rows: Vec<ViewRow> = sqlx::query_as(
                "SELECT job_id, ordinal, content_key, rerun_of, expired::text, outcome
                 FROM plan_job_rows WHERE plan_id = $1",
            )
            .bind(id.as_str())
            .fetch_all(store.db().pool())
            .await?;
            let by_view = rows
                .into_iter()
                .map(
                    |(job_id, ordinal, content_key, rerun_of, expired, outcome)| {
                        let expired = expired.map(Expired::parse).transpose()?;
                        let fields = Fields {
                            ordinal,
                            content_key,
                            rerun_of,
                            expired,
                            outcome,
                        };
                        Ok((job_id, fields))
                    },
                )
                .collect::<anyhow::Result<ManifestJobs>>()?;
            assert_eq!(by_view, manifest_jobs(manifest)?, "plan {id}");
        }
        Ok(())
    }

    /// After a race on the guard row, plan 1 reads back `change` and plan 2
    /// reads back the head that the race tests write.
    async fn assert_change_and_head_read_back(
        store: &SqlPlanStore,
        change: PlanManifest,
    ) -> anyhow::Result<()> {
        assert_reads_back(
            store,
            &BTreeMap::from([
                (plan(1), change),
                (plan(2), manifest(2, CAMPAIGN, &["a", "b"])?),
            ]),
        )
        .await
    }

    /// The holders of `ids`, in `(plan_id, job_id)` order.
    async fn sorted_holders(
        store: &SqlPlanStore,
        campaign: &CampaignId,
        ids: &[JobId],
    ) -> anyhow::Result<pipette_store::JobHolderListing> {
        let mut holders = store.job_holders(campaign, ids).await?;
        holders
            .items
            .sort_by(|a, b| (&a.plan_id, &a.job_id).cmp(&(&b.plan_id, &b.job_id)));
        Ok(holders)
    }

    /// After each write, every plan reads back its last manifest, and the span
    /// count is the minimum that the writes need.
    #[rstest]
    #[case::carried(carried(), 2)]
    #[case::dropped_added_replaced(dropped_added_replaced(), 5)]
    #[case::changed_on_the_head(changed_on_the_head(), 4)]
    #[case::changed_on_a_superseded_plan(changed_on_a_superseded_plan(), 4)]
    #[case::written_after_a_greater_id(written_after_a_greater_id(), 2)]
    #[case::without_a_campaign(without_a_campaign(), 3)]
    #[case::changed_and_changed_back(changed_and_changed_back(), 1)]
    #[tokio::test]
    async fn each_write_reads_back_the_last_manifest(
        #[case] writes: anyhow::Result<Vec<PlanManifest>>,
        #[case] spans: usize,
    ) -> anyhow::Result<()> {
        let test_db = test_db_or_skip!();
        let db = test_db.db();
        let store = SqlPlanStore::new(db.clone());
        let mut expected = BTreeMap::new();
        for manifest in writes? {
            store.put_plan(&manifest).await?;
            expected.insert(manifest.plan_id.clone(), manifest);
            assert_reads_back(&store, &expected).await?;
        }
        assert_eq!(stored_spans(db).await?.len(), spans);
        Ok(())
    }

    /// `get_plan`, the view and `job_holders` list the jobs of each manifest
    /// from the spans.
    #[tokio::test]
    async fn membership_reads_list_the_manifest_jobs() -> anyhow::Result<()> {
        let test_db = test_db_or_skip!();
        let db = test_db.db();
        let store = SqlPlanStore::new(db.clone());
        let mut expected = BTreeMap::new();
        for manifest in dropped_added_replaced()? {
            store.put_plan(&manifest).await?;
            for job in &manifest.job_ids {
                store.put_job_plan_link(job, &manifest.plan_id).await?;
            }
            expected.insert(manifest.plan_id.clone(), manifest);
        }
        let ids: Vec<_> = ["a", "b", "c", "d", "e"]
            .into_iter()
            .map(JobId::new_unchecked)
            .collect();
        let campaign = CampaignId::try_new(CAMPAIGN.context("campaign")?)?;
        let before = sorted_holders(&store, &campaign, &ids).await?;
        assert!(!before.items.is_empty());
        let mut wanted: Vec<_> = expected
            .values()
            .flat_map(|manifest| {
                manifest.job_ids.iter().map(move |job| {
                    (
                        manifest.plan_id.as_str().to_owned(),
                        job.as_str().to_owned(),
                        manifest.rerun_of.get(job).map(|id| id.as_str().to_owned()),
                    )
                })
            })
            .collect();
        wanted.sort();
        let by_view: Vec<(String, String, Option<String>)> = sqlx::query_as(
            "SELECT plan_id, job_id, rerun_of FROM plan_job_rows
             WHERE ordinal IS NOT NULL ORDER BY plan_id, job_id",
        )
        .fetch_all(db.pool())
        .await?;
        assert_eq!(by_view, wanted);
        let held: Vec<_> = before
            .items
            .iter()
            .map(|h| {
                (
                    h.plan_id.as_str().to_owned(),
                    h.job_id.as_str().to_owned(),
                    h.rerun_of.as_ref().map(|id| id.as_str().to_owned()),
                )
            })
            .collect();
        assert_eq!(held, wanted);
        assert!(before.unreadable.is_empty());
        assert_reads_back(&store, &expected).await?;
        Ok(())
    }

    #[tokio::test]
    async fn carried_revisions_and_refreshes_write_no_span_rows() -> anyhow::Result<()> {
        let test_db = test_db_or_skip!();
        let db = test_db.db();
        let store = SqlPlanStore::new(db.clone());
        let mut revisions = carried()?.into_iter();
        store
            .put_plan(&revisions.next().context("first revision")?)
            .await?;
        test_db
            .execute(
                "CREATE FUNCTION reject_span_write() RETURNS trigger LANGUAGE plpgsql AS $$
                 BEGIN RAISE EXCEPTION 'a carried job must not write a span row'; END $$;
                 CREATE TRIGGER reject_span_write BEFORE INSERT OR UPDATE OR DELETE
                 ON plan_job_spans FOR EACH ROW EXECUTE FUNCTION reject_span_write();",
            )
            .await?;
        stream::iter(revisions)
            .map(Ok::<_, anyhow::Error>)
            .try_for_each(|manifest| {
                let store = &store;
                async move {
                    store.put_plan(&manifest).await?;
                    store.put_plan(&manifest).await?;
                    assert_eq!(store.get_plan(&manifest.plan_id).await?, Some(manifest));
                    Ok(())
                }
            })
            .await?;
        Ok(())
    }

    /// The down script of migration `0044` fills `plan_jobs` with exactly the
    /// rows of `plan_job_rows`, and the move up drops the table again.
    #[tokio::test]
    async fn the_drop_s_down_fills_plan_jobs_from_the_view() -> anyhow::Result<()> {
        let test_db = test_db_or_skip!();
        let db = test_db.db();
        let store = SqlPlanStore::new(db.clone());
        for manifest in dropped_added_replaced()?
            .into_iter()
            .chain([
                lapsed(finished(manifest(1, CAMPAIGN, &["a", "b"])?, "a"), "b")?,
                finished(manifest(3, CAMPAIGN, &["a", "b", "e"])?, "e"),
            ])
            .chain(without_a_campaign()?)
        {
            store.put_plan(&manifest).await?;
        }
        let count = |sql| async move {
            sqlx::query_scalar::<_, i64>(sql)
                .fetch_one(db.pool())
                .await
                .context(sql)
        };
        let has_plan_jobs = || async {
            sqlx::query_scalar::<_, bool>("SELECT to_regclass('plan_jobs') IS NOT NULL")
                .fetch_one(db.pool())
                .await
        };
        assert!(!has_plan_jobs().await?);

        crate::migrate::migrate(db, Some(43), false).await?;
        assert_eq!(
            count("SELECT count(*) FROM plan_jobs").await?,
            count("SELECT count(*) FROM plan_job_rows").await?
        );
        assert!(count("SELECT count(*) FROM plan_jobs").await? > 0);
        for differ in [
            "SELECT count(*) FROM (SELECT * FROM plan_jobs EXCEPT ALL
                 SELECT plan_id, job_id, ordinal, content_key, rerun_of, expired, outcome
                 FROM plan_job_rows) d",
            "SELECT count(*) FROM (
                 SELECT plan_id, job_id, ordinal, content_key, rerun_of, expired, outcome
                 FROM plan_job_rows EXCEPT ALL SELECT * FROM plan_jobs) d",
        ] {
            assert_eq!(count(differ).await?, 0, "{differ}");
        }

        crate::migrate::migrate(db, None, false).await?;
        assert!(!has_plan_jobs().await?);
        Ok(())
    }

    /// A write whose view predates the commit of a new head fails with a
    /// conflict at the guard row. Without the guard, it would end the open
    /// spans at the old head and open spans that the new head lists by mistake.
    #[tokio::test]
    async fn a_write_with_an_older_view_conflicts() -> anyhow::Result<()> {
        let test_db = test_db_or_skip!();
        let db = test_db.db();
        let store = SqlPlanStore::new(db.clone());
        store.put_plan(&manifest(1, CAMPAIGN, &["a", "b"])?).await?;
        let mut stale = db.begin().await?;
        sqlx::query("SELECT 1").execute(&mut *stale).await?;
        store.put_plan(&manifest(2, CAMPAIGN, &["a", "b"])?).await?;

        let change = finished(manifest(1, CAMPAIGN, &["a", "b"])?, "a");
        let lost = write_in(&mut stale, &change).await?;
        assert!(lost.is_err_and(|e| is_conflict(&e)));
        stale.rollback().await?;
        store.put_plan(&change).await?;
        assert_change_and_head_read_back(&store, change).await
    }

    /// A write that waits on the guard row of another write fails with a
    /// conflict when that write commits.
    #[tokio::test]
    async fn a_write_that_waits_on_the_guard_row_conflicts() -> anyhow::Result<()> {
        let test_db = test_db_or_skip!();
        let db = test_db.db();
        let store = SqlPlanStore::new(db.clone());
        store.put_plan(&manifest(1, CAMPAIGN, &["a", "b"])?).await?;
        let mut first = db.begin().await?;
        let pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
            .fetch_one(&mut *first)
            .await?;
        write_in(&mut first, &manifest(2, CAMPAIGN, &["a", "b"])?).await??;

        let change = finished(manifest(1, CAMPAIGN, &["a", "b"])?, "a");
        let waiting = tokio::spawn({
            let (db, change) = (db.clone(), change.clone());
            async move {
                let mut tx = db.begin().await?;
                anyhow::Ok(
                    write_in(&mut tx, &change)
                        .await?
                        .is_err_and(|e| is_conflict(&e)),
                )
            }
        });
        let mut blocked = false;
        for _ in 0..200 {
            blocked = sqlx::query_scalar(
                "SELECT EXISTS (SELECT 1 FROM pg_stat_activity
                                WHERE $1 = ANY (pg_blocking_pids(pid)))",
            )
            .bind(pid)
            .fetch_one(db.pool())
            .await?;
            if blocked {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(blocked, "the second write never waited on the first");
        first.commit().await?;
        assert!(waiting.await??);

        store.put_plan(&change).await?;
        assert_change_and_head_read_back(&store, change).await
    }
}
