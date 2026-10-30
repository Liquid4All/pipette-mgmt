//! Walking a campaign's history: its head, then each plan that one supersedes.
//! Shared so `campaigns show` and the console give the same answer.

use pipette_core::campaign::Campaign;
use pipette_core::plan::PlanManifest;
use pipette_plan_types::PlanId;
use pipette_store::PlanStore;

/// How many plans a walk returns: each revision is a sequential store read,
/// so this keeps a page's latency off the campaign's age.
pub const CHAIN_MAX: usize = 50;

/// The plans a campaign has run, newest first: its head, then what that
/// supersedes, as far back as [`CHAIN_MAX`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CampaignChain {
    /// The head first, so `plans[0]` is what the campaign is running now and
    /// `plans[1..]` is what it superseded.
    pub plans: Vec<PlanId>,
    /// Whether the walk stopped at the ceiling with links still behind it.
    pub truncated: bool,
}

/// [`walk`], keeping the manifest each link was read from: `None` for the
/// link whose manifest is gone, which is always the last.
#[derive(Debug, Clone)]
pub struct ChainLinks {
    pub links: Vec<(PlanId, Option<PlanManifest>)>,
    pub truncated: bool,
}

/// Walk `campaign`'s chain back from its head.
///
/// A link whose manifest is gone ends the walk: a shorter history, not a
/// failed read. A plan already seen ends it too, against a corrupt manifest.
pub async fn walk(plans: &dyn PlanStore, campaign: &Campaign) -> anyhow::Result<CampaignChain> {
    let ChainLinks { links, truncated } = walk_links(plans, campaign).await?;
    Ok(CampaignChain {
        plans: links.into_iter().map(|(plan_id, _)| plan_id).collect(),
        truncated,
    })
}

/// [`walk`], for a surface that draws each plan as well as naming it.
pub async fn walk_links(plans: &dyn PlanStore, campaign: &Campaign) -> anyhow::Result<ChainLinks> {
    let (chain, _) = links(plans, campaign, CHAIN_MAX).await?;
    Ok(chain)
}

/// Every readable plan filed into the campaign, unbounded: the whole chain and
/// any plan the store files under it that the chain misses.
pub async fn every_plan(
    plans: &dyn PlanStore,
    campaign: &Campaign,
) -> anyhow::Result<Vec<PlanManifest>> {
    let (ChainLinks { links, .. }, off_chain) = links(plans, campaign, usize::MAX).await?;
    Ok(links
        .into_iter()
        .filter_map(|(_, manifest)| manifest)
        .chain(off_chain)
        .collect())
}

/// Find `target` in the chain of `campaign`, at any depth. Return `None` if the
/// walk does not reach `target`. Otherwise return the manifest of `target`, or
/// `Some(None)` if that manifest is gone. The walk does not reach a plan of the
/// campaign that is not on its chain. It also does not reach a plan behind a
/// link with no manifest. A read error propagates: it does not mean "not
/// reached".
pub async fn find(
    plans: &dyn PlanStore,
    campaign: &Campaign,
    target: &PlanId,
) -> anyhow::Result<Option<Option<PlanManifest>>> {
    let (ChainLinks { links, .. }, _) = links(plans, campaign, usize::MAX).await?;
    Ok(links
        .into_iter()
        .find(|(plan_id, _)| plan_id == target)
        .map(|(_, manifest)| manifest))
}

/// The place of `plan` in its campaign's history: 1 for the campaign's first
/// plan, 2 for the plan that supersedes it, and so on. The walk follows
/// `previous_plan_id` back from `plan`, so a later filing does not change the
/// count. `None` if the walk reaches a plan whose manifest is gone, or a plan
/// that it already read. A read error propagates.
pub async fn position(plans: &dyn PlanStore, plan: &PlanManifest) -> anyhow::Result<Option<usize>> {
    let mut known: std::collections::HashMap<PlanId, PlanManifest> = match &plan.campaign_id {
        Some(campaign_id) => plans
            .get_campaign_plans(campaign_id)
            .await?
            .unwrap_or_default()
            .into_iter()
            .map(|manifest| (manifest.plan_id.clone(), manifest))
            .collect(),
        None => std::collections::HashMap::new(),
    };
    let mut seen = std::collections::HashSet::from([plan.plan_id.clone()]);
    let mut next = plan.previous_plan_id.clone();
    while let Some(plan_id) = next {
        if !seen.insert(plan_id.clone()) {
            return Ok(None);
        }
        let manifest = match known.remove(&plan_id) {
            Some(manifest) => manifest,
            None => match plans.get_plan(&plan_id).await? {
                Some(manifest) => manifest,
                None => return Ok(None),
            },
        };
        next = manifest.previous_plan_id;
    }
    Ok(Some(seen.len()))
}

/// The chain as far back as `max`, and the campaign's plans the walk did not
/// reach.
async fn links(
    plans: &dyn PlanStore,
    campaign: &Campaign,
    max: usize,
) -> anyhow::Result<(ChainLinks, Vec<PlanManifest>)> {
    let mut chain = ChainLinks {
        links: Vec::new(),
        truncated: false,
    };
    let mut seen = std::collections::HashSet::new();
    let mut next = campaign.current_plan_id.clone();
    // The campaign's plans in one read where the store offers it; the walk reads
    // any plan it lacks, one that left the campaign or did not assemble, itself.
    let mut known: std::collections::HashMap<PlanId, PlanManifest> = plans
        .get_campaign_plans(&campaign.campaign_id)
        .await?
        .unwrap_or_default()
        .into_iter()
        .map(|manifest| (manifest.plan_id.clone(), manifest))
        .collect();

    while let Some(plan_id) = next {
        if !seen.insert(plan_id.clone()) {
            tracing::warn!(%plan_id, "campaign chain revisits a plan; stopping the walk");
            break;
        }
        let manifest = match known.remove(&plan_id) {
            Some(manifest) => Some(manifest),
            None => plans.get_plan(&plan_id).await?,
        };
        next = manifest
            .as_ref()
            .and_then(|manifest| manifest.previous_plan_id.clone());
        chain.links.push((plan_id, manifest));
        if chain.links.len() == max {
            // `next`, not the length, says whether there is more: a chain of exactly
            // CHAIN_MAX is not truncated.
            chain.truncated = next.is_some();
            break;
        }
    }
    Ok((chain, known.into_values().collect()))
}

impl CampaignChain {
    /// The plans the head supersedes, newest first — the chain without its head.
    pub fn superseded(&self) -> &[PlanId] {
        self.plans.split_first().map_or(&[], |(_, rest)| rest)
    }
}

#[cfg(test)]
mod tests {
    use chrono::Utc;

    use pipette_core::campaign::{Campaign, CampaignOwner, NewCampaign};
    use pipette_core::plan::PlanManifest;
    use pipette_core::types::Mint;
    use pipette_plan_types::{NonEmptyTrimmedString, PlanStatus, UserId};
    use pipette_store_fs::build_local_fs_plan_store;

    use super::*;

    /// A campaign running `plans.last()`, with each plan superseding the one
    /// before it. Returns the ids oldest first.
    async fn chain_of(store: &dyn PlanStore, depth: usize) -> anyhow::Result<Vec<PlanId>> {
        let mut ids: Vec<PlanId> = Vec::new();
        for _ in 0..depth {
            let plan_id = PlanId::from_uuid(uuid::Uuid::now_v7());
            store
                .put_plan(&PlanManifest {
                    plan_id: plan_id.clone(),
                    status: PlanStatus::Active,
                    created_at: Utc::now(),
                    submitted_by: None,
                    campaign_id: None,
                    previous_plan_id: ids.last().cloned(),
                    source_sha256: None,
                    submitted_sha256: None,
                    plan_warnings: Vec::new(),
                    benchmark_types: Default::default(),
                    job_ids: Vec::new(),
                    content_keys: Default::default(),
                    rerun_of: Default::default(),
                    warnings: Vec::new(),
                    job_expiry: None,
                    expired_jobs: std::collections::BTreeMap::new(),
                    settled_jobs: Default::default(),
                    progress_snapshot: None,
                    terminal_at: None,
                })
                .await?;
            ids.push(plan_id);
        }
        Ok(ids)
    }

    fn campaign_on(head: Option<PlanId>) -> anyhow::Result<Campaign> {
        let campaign = NewCampaign::new(
            NonEmptyTrimmedString::try_new("Nightly")?,
            CampaignOwner::User {
                user_id: UserId::mint(),
            },
        )
        .around(PlanId::from_uuid(uuid::Uuid::now_v7()), Utc::now());
        // `None` is a record written before a campaign needed a first plan.
        Ok(Campaign {
            current_plan_id: head,
            labels: Default::default(),
            ..campaign
        })
    }

    fn store() -> anyhow::Result<(tempfile::TempDir, std::sync::Arc<dyn PlanStore>)> {
        let dir = tempfile::tempdir()?;
        let store = build_local_fs_plan_store(&pipette_core::config::StorageConfig::local_fs(
            dir.path().to_path_buf(),
        ))?;
        Ok((dir, store))
    }

    #[tokio::test]
    async fn the_walk_returns_the_head_then_what_it_supersedes() -> anyhow::Result<()> {
        let (_dir, store) = store()?;
        let ids = chain_of(store.as_ref(), 3).await?;
        let campaign = campaign_on(ids.last().cloned())?;

        let chain = walk(store.as_ref(), &campaign).await?;
        let newest_first: Vec<PlanId> = ids.iter().rev().cloned().collect();
        assert_eq!(chain.plans, newest_first);
        assert_eq!(chain.superseded(), &newest_first[1..]);
        assert!(!chain.truncated);
        Ok(())
    }

    #[tokio::test]
    async fn a_campaign_running_nothing_walks_to_nothing() -> anyhow::Result<()> {
        let (_dir, store) = store()?;
        let chain = walk(store.as_ref(), &campaign_on(None)?).await?;
        assert!(chain.plans.is_empty());
        assert!(chain.superseded().is_empty());
        assert!(!chain.truncated);
        Ok(())
    }

    /// A chain of exactly the ceiling is not truncated.
    #[tokio::test]
    async fn the_ceiling_bounds_the_walk_and_reports_it() -> anyhow::Result<()> {
        let (_dir, store) = store()?;
        let ids = chain_of(store.as_ref(), CHAIN_MAX + 2).await?;
        let chain = walk(store.as_ref(), &campaign_on(ids.last().cloned())?).await?;
        assert_eq!(chain.plans.len(), CHAIN_MAX);
        assert!(chain.truncated);

        let exact = chain_of(store.as_ref(), CHAIN_MAX).await?;
        let chain = walk(store.as_ref(), &campaign_on(exact.last().cloned())?).await?;
        assert_eq!(chain.plans.len(), CHAIN_MAX);
        assert!(!chain.truncated, "nothing was cut off");
        Ok(())
    }

    /// It ends the walk rather than failing it.
    #[tokio::test]
    async fn a_missing_link_ends_the_walk() -> anyhow::Result<()> {
        let (dir, store) = store()?;
        let ids = chain_of(store.as_ref(), 3).await?;
        pipette_store_testkit::local_fs_remove_plan(dir.path(), &ids[1])?;

        let chain = walk(store.as_ref(), &campaign_on(ids.last().cloned())?).await?;
        assert_eq!(
            chain.plans,
            vec![ids[2].clone(), ids[1].clone()],
            "the walk stops at the missing plan"
        );

        let links = walk_links(store.as_ref(), &campaign_on(ids.last().cloned())?).await?;
        let read: Vec<bool> = links.links.iter().map(|(_, m)| m.is_some()).collect();
        assert_eq!(read, vec![true, false], "the missing plan is named, unread");
        Ok(())
    }

    /// What a [`find`] or [`position`] case does to the chain it builds before
    /// the lookup.
    #[derive(Debug, Clone, Copy)]
    enum Damage {
        Intact,
        /// The manifest of the plan between the head and the oldest is gone.
        RemoveMiddle,
        /// The oldest plan names the head as its previous plan.
        CloseCycle,
    }

    impl Damage {
        /// Damage the chain `ids`, oldest first, that `chain_of` built.
        async fn apply(
            self,
            dir: &std::path::Path,
            store: &dyn PlanStore,
            ids: &[PlanId],
        ) -> anyhow::Result<()> {
            match self {
                Damage::Intact => {}
                Damage::RemoveMiddle => pipette_store_testkit::local_fs_remove_plan(dir, &ids[1])?,
                Damage::CloseCycle => {
                    let oldest = store
                        .get_plan(&ids[0])
                        .await?
                        .ok_or_else(|| anyhow::anyhow!("the oldest plan is gone"))?;
                    store
                        .put_plan(&PlanManifest {
                            previous_plan_id: ids.last().cloned(),
                            ..oldest
                        })
                        .await?;
                }
            }
            Ok(())
        }
    }

    /// What a [`find`] case expects of the target.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Reach {
        /// The chain reaches the target and reads its manifest.
        Read,
        /// The chain reaches the target, but its manifest is gone.
        Gone,
        /// The chain does not reach the target.
        No,
    }

    /// `target` indexes the chain oldest first; `None` is a plan of another
    /// chain.
    #[rstest::rstest]
    #[case::the_head(3, Damage::Intact, Some(2), Reach::Read)]
    #[case::an_earlier_plan(3, Damage::Intact, Some(0), Reach::Read)]
    #[case::a_plan_past_the_ceiling(CHAIN_MAX + 2, Damage::Intact, Some(0), Reach::Read)]
    #[case::a_plan_of_another_chain(3, Damage::Intact, None, Reach::No)]
    #[case::a_missing_link(3, Damage::RemoveMiddle, Some(1), Reach::Gone)]
    #[case::a_plan_behind_a_missing_link(3, Damage::RemoveMiddle, Some(0), Reach::No)]
    #[case::a_plan_in_a_cycle(3, Damage::CloseCycle, Some(0), Reach::Read)]
    #[case::a_plan_a_cycle_never_reaches(3, Damage::CloseCycle, None, Reach::No)]
    #[tokio::test]
    async fn find_reaches_only_the_campaigns_chain(
        #[case] depth: usize,
        #[case] damage: Damage,
        #[case] target: Option<usize>,
        #[case] expected: Reach,
    ) -> anyhow::Result<()> {
        let (dir, store) = store()?;
        let ids = chain_of(store.as_ref(), depth).await?;
        let other = chain_of(store.as_ref(), 1).await?;
        damage.apply(dir.path(), store.as_ref(), &ids).await?;
        let target = target.map_or(&other[0], |index| &ids[index]);

        let found = find(store.as_ref(), &campaign_on(ids.last().cloned())?, target).await?;
        let reach = match &found {
            Some(Some(_)) => Reach::Read,
            Some(None) => Reach::Gone,
            None => Reach::No,
        };
        assert_eq!(reach, expected);
        if let Some(Some(manifest)) = found {
            assert_eq!(&manifest.plan_id, target);
        }
        Ok(())
    }

    /// `None` is a count that the walk cannot finish.
    #[rstest::rstest]
    #[case::the_first_plan(3, Damage::Intact, 0, Some(1))]
    #[case::the_head(3, Damage::Intact, 2, Some(3))]
    #[case::a_plan_past_the_ceiling(CHAIN_MAX + 2, Damage::Intact, CHAIN_MAX + 1, Some(CHAIN_MAX + 2))]
    #[case::a_plan_before_a_missing_link(3, Damage::RemoveMiddle, 0, Some(1))]
    #[case::a_plan_behind_a_missing_link(3, Damage::RemoveMiddle, 2, None)]
    #[case::a_plan_in_a_cycle(3, Damage::CloseCycle, 2, None)]
    #[tokio::test]
    async fn position_counts_the_plans_up_to_this_one(
        #[case] depth: usize,
        #[case] damage: Damage,
        #[case] target: usize,
        #[case] expected: Option<usize>,
    ) -> anyhow::Result<()> {
        let (dir, store) = store()?;
        let ids = chain_of(store.as_ref(), depth).await?;
        damage.apply(dir.path(), store.as_ref(), &ids).await?;
        let plan = store
            .get_plan(&ids[target])
            .await?
            .ok_or_else(|| anyhow::anyhow!("the target plan is gone"))?;

        assert_eq!(position(store.as_ref(), &plan).await?, expected);
        Ok(())
    }

    #[tokio::test]
    async fn every_plan_reads_past_the_ceiling() -> anyhow::Result<()> {
        let (_dir, store) = store()?;
        let ids = chain_of(store.as_ref(), CHAIN_MAX + 2).await?;
        let every = every_plan(store.as_ref(), &campaign_on(ids.last().cloned())?).await?;
        let read: std::collections::HashSet<PlanId> =
            every.into_iter().map(|manifest| manifest.plan_id).collect();
        assert_eq!(read, ids.into_iter().collect());
        Ok(())
    }
}
