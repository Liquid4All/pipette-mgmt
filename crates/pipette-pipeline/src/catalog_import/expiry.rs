//! Automatic expiry: the active importer entries that move to hidden.

use chrono::{DateTime, TimeDelta, Utc};

use pipette_core::config::ExpiryConfig;
use pipette_plan_types::{DescriptorEntry, DescriptorEntryKind, DescriptorEntryState};

/// The active entries in `entries` that are past expiry at `now`. `entries`
/// holds the entries of one upstream.
///
/// - Keep the `stable_keep_newest` active or pinned stable entries with the
///   newest release times. Other active stable entries expire at
///   `stable_max_age_days` after release.
/// - A daily entry expires if its release is at least `daily_max_age_days` old.
/// - A hand-made, pinned or hidden entry never expires.
pub(crate) fn expired<'a>(
    entries: &'a [DescriptorEntry],
    config: &ExpiryConfig,
    now: DateTime<Utc>,
) -> Vec<&'a DescriptorEntry> {
    let mut newest = entries
        .iter()
        .filter(|entry| {
            entry.kind == DescriptorEntryKind::Stable
                && matches!(
                    entry.state,
                    DescriptorEntryState::Active | DescriptorEntryState::Pinned
                )
        })
        .collect::<Vec<_>>();
    newest.sort_by(|a, b| {
        b.release_time
            .cmp(&a.release_time)
            .then_with(|| a.name.as_str().cmp(b.name.as_str()))
    });
    newest.truncate(config.stable_keep_newest);
    let stable_cutoff = cutoff(now, config.stable_max_age_days);
    let daily_cutoff = cutoff(now, config.daily_max_age_days);
    entries
        .iter()
        .filter(|entry| entry.state == DescriptorEntryState::Active)
        .filter(|entry| match entry.kind {
            DescriptorEntryKind::Stable => {
                entry.release_time <= stable_cutoff
                    && !newest.iter().any(|kept| kept.name == entry.name)
            }
            DescriptorEntryKind::Daily => entry.release_time <= daily_cutoff,
            DescriptorEntryKind::HandMade => false,
        })
        .collect()
}

/// The newest release time that is at least `days` before `now`. A threshold
/// beyond the range of a date gives the earliest date, so nothing expires.
fn cutoff(now: DateTime<Utc>, days: u32) -> DateTime<Utc> {
    TimeDelta::try_days(i64::from(days))
        .and_then(|age| now.checked_sub_signed(age))
        .unwrap_or(DateTime::<Utc>::MIN_UTC)
}

#[cfg(test)]
mod tests {
    use anyhow::Context;
    use chrono::TimeZone;
    use rstest::rstest;

    use pipette_plan_types::DescriptorEntryKind::{Daily, HandMade, Stable};
    use pipette_plan_types::DescriptorEntryState::{Active, Hidden, Pinned};
    use pipette_plan_types::{
        CatalogName, CatalogScope, DescriptorEntryCreator, DescriptorMember, LlamaCppFlavor,
        LlamacppCliStockTools, LlamacppCliStockToolsRelease, LlamacppCliStockToolsSource,
        NonEmptyString, NonEmptyVec, RepositoryUrl, Runtime,
    };

    use super::*;

    fn now() -> anyhow::Result<DateTime<Utc>> {
        Utc.with_ymd_and_hms(2026, 10, 21, 3, 0, 0)
            .single()
            .context("test time is invalid")
    }

    fn entry(
        name: &str,
        kind: DescriptorEntryKind,
        state: DescriptorEntryState,
        age_days: i64,
    ) -> anyhow::Result<DescriptorEntry> {
        let now = now()?;
        let released = now - TimeDelta::days(age_days);
        let runtime = Runtime::LlamacppCliStockTools(LlamacppCliStockTools {
            source: LlamacppCliStockToolsSource::GithubRelease(LlamacppCliStockToolsRelease {
                repository_url: RepositoryUrl::new("github.com/ggml-org/llama.cpp".to_owned()),
                repository_version: NonEmptyString::try_new("b11669".to_owned())?,
                asset: None,
            }),
            flavor: LlamaCppFlavor::known()
                .next()
                .context("no known llama.cpp flavor")?,
        });
        let creator = match kind {
            HandMade => DescriptorEntryCreator::Operator,
            Stable | Daily => DescriptorEntryCreator::ImporterLlamacpp,
        };
        Ok(DescriptorEntry::new(
            CatalogScope::Global,
            CatalogName::try_new(name)?,
            kind,
            state,
            released,
            now,
            creator,
            NonEmptyVec::try_new(vec![DescriptorMember::new(runtime, now)?])?,
        )?)
    }

    #[rstest]
    #[case::slow_upstream_keeps_its_two_newest(
        &[("s1", Stable, Active, 400), ("s2", Stable, Active, 300), ("s3", Stable, Active, 200)],
        &["s1"]
    )]
    #[case::fast_upstream_keeps_sixty_days(
        &[
            ("s1", Stable, Active, 5),
            ("s2", Stable, Active, 20),
            ("s3", Stable, Active, 40),
            ("s4", Stable, Active, 59),
            ("s5", Stable, Active, 60),
            ("s6", Stable, Active, 90),
        ],
        &["s5", "s6"]
    )]
    #[case::daily_keeps_fourteen_days(
        &[("d13", Daily, Active, 13), ("d14", Daily, Active, 14), ("d30", Daily, Active, 30)],
        &["d14", "d30"]
    )]
    #[case::pinned_never_expires(
        &[("s1", Stable, Pinned, 900), ("d1", Daily, Pinned, 900)],
        &[]
    )]
    #[case::hand_made_never_expires(&[("fork", HandMade, Active, 900)], &[])]
    #[case::hidden_stable_takes_no_floor_slot(
        &[("s1", Stable, Active, 400), ("s2", Stable, Active, 300), ("s3", Stable, Hidden, 200)],
        &[]
    )]
    #[case::pinned_stable_takes_a_floor_slot(
        &[("s1", Stable, Active, 400), ("s2", Stable, Active, 300), ("s3", Stable, Pinned, 200)],
        &["s1"]
    )]
    #[case::hidden_entry_is_not_hidden_again(&[("d30", Daily, Hidden, 30)], &[])]
    fn expiry_hides_by_kind_and_release_time(
        #[case] entries: &[(&str, DescriptorEntryKind, DescriptorEntryState, i64)],
        #[case] expected: &[&str],
    ) -> anyhow::Result<()> {
        let entries = entries
            .iter()
            .map(|(name, kind, state, age)| entry(name, *kind, *state, *age))
            .collect::<anyhow::Result<Vec<_>>>()?;
        let mut hidden = expired(&entries, &ExpiryConfig::default(), now()?)
            .into_iter()
            .map(|entry| entry.name.as_str())
            .collect::<Vec<_>>();
        hidden.sort_unstable();
        assert_eq!(hidden, expected);
        Ok(())
    }

    /// The importer passes the stored entries from the same upstream and the
    /// new entry. A backfilled entry that is past expiry starts hidden.
    #[rstest]
    #[case::old_daily(&[], ("d30", Daily, 30), true)]
    #[case::stable_older_than_two_stored(
        &[("s1", Stable, Active, 10), ("s2", Stable, Pinned, 5)],
        ("s0", Stable, 70),
        true
    )]
    #[case::stable_with_one_stored(&[("s1", Stable, Active, 10)], ("s0", Stable, 70), false)]
    #[case::recent_daily(&[], ("d1", Daily, 1), false)]
    fn a_backfilled_entry_starts_hidden(
        #[case] stored: &[(&str, DescriptorEntryKind, DescriptorEntryState, i64)],
        #[case] candidate: (&str, DescriptorEntryKind, i64),
        #[case] starts_hidden: bool,
    ) -> anyhow::Result<()> {
        let (name, kind, age) = candidate;
        let mut upstream = stored
            .iter()
            .map(|(name, kind, state, age)| entry(name, *kind, *state, *age))
            .collect::<anyhow::Result<Vec<_>>>()?;
        upstream.push(entry(name, kind, Active, age)?);
        let hidden = expired(&upstream, &ExpiryConfig::default(), now()?)
            .iter()
            .any(|entry| entry.name.as_str() == name);
        assert_eq!(hidden, starts_hidden);
        Ok(())
    }

    #[test]
    fn an_out_of_range_threshold_expires_nothing() -> anyhow::Result<()> {
        let entries = [entry("d1", Daily, Active, 900)?];
        let config = ExpiryConfig {
            daily_max_age_days: u32::MAX,
            ..ExpiryConfig::default()
        };
        assert!(expired(&entries, &config, now()?).is_empty());
        Ok(())
    }
}
