//! What a pre-auth key prune deletes, over either keyspace: the one rule the
//! client and user implementations share, generic over the key id. Listing,
//! deleting and reporting stay with the caller.

use std::collections::HashSet;
use std::hash::Hash;

/// What a `preauth prune` deletes.
pub struct Targets<Id> {
    /// Key records to delete. Expiry is the only reason a *record* is prunable:
    /// a spent single-use key and a revoked one already lost theirs.
    pub expired: Vec<Id>,
    /// Spent markers to delete: those left with no consumable record once the
    /// records above are gone.
    pub orphaned: Vec<Id>,
}

impl<Id> Targets<Id> {
    pub fn is_empty(&self) -> bool {
        self.expired.is_empty() && self.orphaned.is_empty()
    }
}

/// Decide what a prune deletes, from the key records (each paired with whether
/// it has expired) and the spent markers.
///
/// A marker keeps a single-use key spent, so it goes only once no consumable
/// record is left; an expired record is not consumable, so it and its marker go
/// in one pass. Records are listed before markers, so a key spent in between
/// keeps its marker: the safe direction.
pub fn targets<Id: Clone + Eq + Hash>(
    keys: impl IntoIterator<Item = (Id, bool)>,
    markers: impl IntoIterator<Item = Id>,
) -> Targets<Id> {
    let keys: Vec<(Id, bool)> = keys.into_iter().collect();
    let expired: Vec<Id> = keys
        .iter()
        .filter(|(_, expired)| *expired)
        .map(|(id, _)| id.clone())
        .collect();
    let held: HashSet<&Id> = keys
        .iter()
        .filter(|(_, expired)| !*expired)
        .map(|(id, _)| id)
        .collect();
    let orphaned: Vec<Id> = markers
        .into_iter()
        .filter(|id| !held.contains(id))
        .collect();
    Targets { expired, orphaned }
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use pipette_plan_types::{IdError, UserPreauthKeyId};

    use super::*;

    /// A marker is kept only while a consumable record is left; an expired
    /// one's record and marker go together.
    #[rstest]
    #[case::a_held_marker_survives_with_its_record(
        &[("held", false), ("live", false), ("stale", true)],
        &["held", "orphan"],
        &["stale"],
        &["orphan"]
    )]
    #[case::a_healthy_keyspace_sweeps_nothing(&[("live", false)], &["live"], &[], &[])]
    #[case::an_expired_record_takes_its_marker(
        &[("stale", true)],
        &["stale"],
        &["stale"],
        &["stale"]
    )]
    fn prune_targets_cases(
        #[case] keys: &[(&str, bool)],
        #[case] markers: &[&str],
        #[case] expected_expired: &[&str],
        #[case] expected_orphaned: &[&str],
    ) -> anyhow::Result<()> {
        fn ids(names: &[&str]) -> Result<Vec<UserPreauthKeyId>, IdError> {
            names
                .iter()
                .map(|n| UserPreauthKeyId::try_new(*n))
                .collect()
        }
        let records: Vec<_> = keys
            .iter()
            .map(|(name, expired)| UserPreauthKeyId::try_new(*name).map(|id| (id, *expired)))
            .collect::<Result<_, _>>()?;

        let found = targets(records, ids(markers)?);

        assert_eq!(found.expired, ids(expected_expired)?);
        assert_eq!(found.orphaned, ids(expected_orphaned)?);
        assert_eq!(
            found.is_empty(),
            expected_expired.is_empty() && expected_orphaned.is_empty()
        );
        Ok(())
    }
}
