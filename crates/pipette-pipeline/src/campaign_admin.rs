//! How a campaign is created and how its pointer moves: a plan enters a
//! campaign and becomes its head. The CLI and the API both write through here,
//! and always in one order: the plan first, the campaign after.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::Path;

use anyhow::Context;
use chrono::Utc;
use futures::{StreamExt, TryStreamExt};
use serde::Serialize;

use pipette_core::campaign::{Campaign, CampaignOwner, NewCampaign};
use pipette_core::canonical_json::sha256_hex;
use pipette_core::plan::{PlanManifest, SettledOutcome};
use pipette_core::vault::VaultOwner;
use pipette_plan_types::{
    Answered, BenchmarkId, BenchmarkType, CampaignId, JobId, PlanId, PlanStatus, RestoredVariant,
    RevisionChange, UserId,
};
use pipette_store::{
    CampaignStore, OrgStore, PLAN_JOB_CONCURRENCY, PlanFilter, PlanStore, Stores, SubmissionStore,
};

use crate::campaign_chain;
use crate::content_key::{content_digest, legacy_content_digest};
use crate::org;
use crate::org::OrgRef;
use crate::plan_cli;
use crate::plan_cli::PlanInput;
use crate::plan_document;
use crate::plan_document::{DocumentDryRun, DryRunReport, SourceRead};
use crate::plan_ingestion::{
    CampaignMembership, CarryDraw, IngestContext, IngestReport, IngestSettings, Offered, draw_carry,
};
use crate::recalculation;
use crate::variant_edit;
use crate::variant_edit::{VariantEdit, VariantEditError};

/// What a filed revision did to the plan it replaced.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Revised {
    /// The plan the campaign ran before the revision.
    pub superseded: PlanId,
    /// The superseded plan's jobs that the new plan holds.
    pub carried_job_count: usize,
    /// The superseded plan's jobs that the new plan runs again as new jobs.
    pub rerun_job_count: usize,
    /// The superseded plan's jobs whose completions the new plan scores again
    /// as new jobs.
    pub recalculated_job_count: usize,
    /// The superseded plan's jobs that the new plan neither holds nor runs
    /// again.
    pub dropped_job_count: usize,
    /// `false` when the superseded plan had already ended, or was already
    /// being cancelled.
    pub cancel_requested: bool,
}

impl From<Revised> for pipette_plan_types::Revised {
    fn from(revised: Revised) -> Self {
        Self {
            previous_plan_id: revised.superseded,
            carried_job_count: revised.carried_job_count,
            rerun_job_count: revised.rerun_job_count,
            recalculated_job_count: revised.recalculated_job_count,
            dropped_job_count: revised.dropped_job_count,
            cancel_requested: revised.cancel_requested,
        }
    }
}

/// Why a campaign write could not run. Typed so the HTTP surface maps each arm
/// to a status and the CLI prints it, without either parsing a string.
#[derive(Debug, thiserror::Error)]
pub enum CampaignAdminError {
    #[error("no campaign {0}")]
    NotFound(CampaignId),
    #[error("no account {0}")]
    NoSuchUser(UserId),
    #[error("no organization {0}")]
    NoSuchOrg(OrgRef),
    #[error("no plan {0}")]
    NoSuchPlan(PlanId),
    /// Worded for neither surface: the CLI's flags are not the route's fields.
    #[error("name an owner: an account or an organization")]
    NoOwnerNamed,
    /// The plan is staged and claimable; only the head is stale, and the repair
    /// is to file again.
    #[error(
        "plan {plan_id} was filed into {campaign_id} but the campaign still runs its previous plan"
    )]
    NotAdvanced {
        campaign_id: CampaignId,
        plan_id: PlanId,
        source: anyhow::Error,
    },
    /// The first plan is staged, and the campaign it names was never written.
    /// Its own arm so the plan is named, and whether it still needs stopping.
    #[error(
        "plan {plan_id} was filed as the first plan of {campaign_id} but the campaign was not created"
    )]
    NotCreated {
        campaign_id: CampaignId,
        plan_id: PlanId,
        /// Whether [`create_around`] cancelled the plan. `false` leaves it
        /// claimable until an operator runs `plans cancel`.
        cancelled: bool,
        source: anyhow::Error,
    },
    /// A record written before a campaign had to have a first plan.
    #[error("campaign {0} runs no plan to revise")]
    NoPlan(CampaignId),
    #[error("campaign {0} is archived: unarchive it to file a plan into it")]
    Archived(CampaignId),
    /// The campaign moved off the plan the caller read its indices from.
    /// Nothing was filed.
    #[error(
        "campaign {campaign_id} runs {current}, not {expected}: read what it runs now and make \
         your change to that"
    )]
    NotOnPlan {
        campaign_id: CampaignId,
        expected: PlanId,
        current: PlanId,
    },
    #[error(
        "plan {0} has no stored plan document to revise: it was filed as job bodies, or it is gone"
    )]
    NoDocument(PlanId),
    #[error("the stored plan document of plan {0} does not match its manifest's digest")]
    SourceMismatch(PlanId),
    /// A revert names a plan that the campaign's chain does not reach.
    #[error("plan {plan_id} is not in the history of campaign {campaign_id}")]
    NotInCampaign {
        campaign_id: CampaignId,
        plan_id: PlanId,
    },
    /// A variant edit's `restore` names an index past the end of its plan's
    /// variants. Not [`VariantEditError::NoSuchVariant`], which numbers the
    /// head's.
    #[error("plan {plan_id} has {count} variant(s), so it has no variant {index} to restore")]
    NoSuchSourceVariant {
        plan_id: PlanId,
        index: usize,
        count: usize,
    },
    #[error(transparent)]
    Edit(#[from] VariantEditError),
    #[error("a re-run or a recalculation names no job")]
    NothingToRerun,
    /// Not a job of the head, or one of a head filed before its jobs' digests
    /// were recorded, which carries nothing over.
    #[error("plan {plan_id} holds no job {job_id} it can re-run or recalculate")]
    NotInPlan { plan_id: PlanId, job_id: JobId },
    /// No result recorded. The job may still run, for another plan too once it
    /// lapsed for this one, so re-running it could duplicate work in flight.
    #[error("job {0} has no result yet, so there is nothing to re-run or recalculate")]
    Unanswered(JobId),
    #[error("plan {plan_id} holds no {} job to re-run", answered_phrase(.which))]
    NoneAnswered { plan_id: PlanId, which: Answered },
    /// The job's record is a failure, or not an eval's: it holds no
    /// completions to score again.
    #[error("job {0} recorded no eval completions to recalculate")]
    NotRescorable(JobId),
    #[error("plan {0} holds no eval job with completions to recalculate")]
    NoneRescorable(PlanId),
    #[error(
        "campaign {campaign_id} moved off {revised} or changed its owner while plan {plan_id} was filed, so it did not advance"
    )]
    Overtaken {
        campaign_id: CampaignId,
        plan_id: PlanId,
        revised: PlanId,
        /// Whether [`complete_revision`] cancelled `plan_id`. `false` leaves
        /// it claimable until an operator runs `plans cancel`.
        cancelled: bool,
    },
    #[error(
        "campaign {campaign_id} was deleted while plan {plan_id} was filed into it, so the plan did not advance{}",
        cancel_outcome(.cancelled)
    )]
    DeletedDuringFiling {
        campaign_id: CampaignId,
        plan_id: PlanId,
        /// Whether [`advance`] cancelled `plan_id`. `false` leaves it
        /// claimable until an operator runs `plans cancel`.
        cancelled: bool,
    },
    /// The revision advanced, and the replaced plan was not cancelled: its
    /// dropped jobs still run.
    #[error("campaign {campaign_id} now runs {plan_id}, but plan {superseded} was not cancelled")]
    NotCancelled {
        campaign_id: CampaignId,
        plan_id: PlanId,
        superseded: PlanId,
        source: anyhow::Error,
    },
    /// These plans of the campaign still run, and a delete would leave them with
    /// no campaign. The caller decides whether to cancel them.
    #[error("campaign {campaign_id} still runs {}: cancel them before you delete the campaign", plan_list(.plans))]
    StillRunning {
        campaign_id: CampaignId,
        plans: Vec<PlanId>,
    },
    #[error(transparent)]
    Store(#[from] anyhow::Error),
}

fn cancel_outcome(cancelled: &bool) -> &'static str {
    if *cancelled {
        "; the filing cancelled the plan"
    } else {
        "; stop the plan with `plans cancel`"
    }
}

fn plan_list(plans: &[PlanId]) -> String {
    plans
        .iter()
        .map(PlanId::as_str)
        .collect::<Vec<_>>()
        .join(", ")
}

/// What advancing a campaign did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Advanced {
    /// The plan the campaign ran before, if it ran one. Untouched by the move:
    /// stopping it is `plans cancel`.
    pub superseded: Option<PlanId>,
    /// False when the campaign already names this plan. An ingest mints its own
    /// plan id, so re-running a half-completed file reports `true`.
    pub changed: bool,
}

pub async fn require(
    campaigns: &dyn CampaignStore,
    campaign_id: &CampaignId,
) -> Result<Campaign, CampaignAdminError> {
    campaigns
        .get_campaign(campaign_id)
        .await?
        .ok_or_else(|| CampaignAdminError::NotFound(campaign_id.clone()))
}

/// Resolve the `--user` / `--org` pair into the one owner a campaign must have.
/// Both resolve against their roster, so an owner always names a real record.
pub async fn owner(
    stores: &Stores,
    user: Option<UserId>,
    org: Option<OrgRef>,
) -> Result<CampaignOwner, CampaignAdminError> {
    match (user, org) {
        (Some(user_id), None) => {
            if stores.users.get_user(&user_id).await?.is_none() {
                return Err(CampaignAdminError::NoSuchUser(user_id));
            }
            Ok(CampaignOwner::User { user_id })
        }
        (None, Some(reference)) => {
            let found = org::resolve(&*stores.orgs, &reference).await?;
            let org = found.ok_or(CampaignAdminError::NoSuchOrg(reference))?;
            Ok(CampaignOwner::Org { org_id: org.org_id })
        }
        // Neither named, or both; the CLI's `conflicts_with` refuses both
        // before here, so one refusal serves.
        _ => Err(CampaignAdminError::NoOwnerNamed),
    }
}

/// Write the campaign `new` stands for, running `plan_id`: the plan just filed
/// under its id. A failed write cancels the plan, which no campaign now names.
pub async fn create_around(
    campaigns: &dyn CampaignStore,
    plans: &dyn PlanStore,
    new: NewCampaign,
    plan_id: PlanId,
) -> Result<Campaign, CampaignAdminError> {
    let campaign = new.around(plan_id.clone(), Utc::now());
    if let Err(source) = campaigns.put_campaign(&campaign).await {
        // The marker `plans cancel` writes; `queue-maintenance` tears the plan down.
        let cancelled = plans
            .write_cancel_marker(&plan_id)
            .await
            .inspect_err(|e| {
                tracing::error!(
                    campaign_id = %campaign.campaign_id,
                    %plan_id,
                    error = %format_args!("{e:#}"),
                    "could not cancel the first plan of a campaign that was not created"
                );
            })
            .is_ok();
        tracing::warn!(
            campaign_id = %campaign.campaign_id,
            %plan_id,
            cancelled,
            "campaign not created around its first plan"
        );
        return Err(CampaignAdminError::NotCreated {
            campaign_id: campaign.campaign_id,
            plan_id,
            cancelled,
            source,
        });
    }
    tracing::info!(
        campaign_id = %campaign.campaign_id,
        %plan_id,
        owner = %campaign.owner.label(),
        "campaign created around its first plan"
    );
    Ok(campaign)
}

/// Point `campaign_id` at `plan_id`, and report what it superseded.
///
/// Call this after the plan is written: a failure between leaves a filed plan
/// that never took effect, never a campaign on a missing plan. The record is
/// re-read, not taken from the caller, so `put_campaign` cannot clobber a
/// rename that landed during the ingest. A campaign gone by then cancels the
/// plan, since a [`delete`] scan can miss it.
pub async fn advance(
    campaigns: &dyn CampaignStore,
    plans: &dyn PlanStore,
    campaign_id: &CampaignId,
    plan_id: PlanId,
) -> Result<Advanced, CampaignAdminError> {
    let campaign = reread(campaigns, plans, campaign_id, &plan_id).await?;
    write_head(campaigns, campaign, plan_id).await
}

/// [`advance`], only while the campaign still runs `from` and `owner` still
/// owns it; `None`, writing nothing, when either changed. A revision imports
/// gated models for `owner`, so a new owner could not fetch them. Not atomic:
/// `put_campaign` has no CAS.
async fn advance_from(
    campaigns: &dyn CampaignStore,
    plans: &dyn PlanStore,
    campaign_id: &CampaignId,
    owner: &CampaignOwner,
    from: &PlanId,
    plan_id: PlanId,
) -> Result<Option<Advanced>, CampaignAdminError> {
    let campaign = reread(campaigns, plans, campaign_id, &plan_id).await?;
    if campaign.current_plan_id.as_ref() != Some(from) || campaign.owner != *owner {
        return Ok(None);
    }
    write_head(campaigns, campaign, plan_id).await.map(Some)
}

/// The record [`advance`] moves, read fresh. A failure names the staged plan.
/// If the campaign is gone, the re-read also cancels the plan.
async fn reread(
    campaigns: &dyn CampaignStore,
    plans: &dyn PlanStore,
    campaign_id: &CampaignId,
    plan_id: &PlanId,
) -> Result<Campaign, CampaignAdminError> {
    match require(campaigns, campaign_id).await {
        Ok(campaign) => Ok(campaign),
        Err(CampaignAdminError::NotFound(_)) => {
            let cancelled = plans
                .write_cancel_marker(plan_id)
                .await
                .inspect_err(|e| {
                    tracing::error!(
                        %campaign_id,
                        %plan_id,
                        error = %format_args!("{e:#}"),
                        "could not cancel a filed plan whose campaign was deleted"
                    );
                })
                .is_ok();
            if cancelled {
                tracing::warn!(
                    %campaign_id,
                    %plan_id,
                    "filed plan cancelled: its campaign was deleted before it advanced"
                );
            }
            Err(CampaignAdminError::DeletedDuringFiling {
                campaign_id: campaign_id.clone(),
                plan_id: plan_id.clone(),
                cancelled,
            })
        }
        Err(source) => Err(CampaignAdminError::NotAdvanced {
            campaign_id: campaign_id.clone(),
            plan_id: plan_id.clone(),
            source: source.into(),
        }),
    }
}

/// Point `campaign` at `plan_id` and write it, unless it already runs it.
async fn write_head(
    campaigns: &dyn CampaignStore,
    mut campaign: Campaign,
    plan_id: PlanId,
) -> Result<Advanced, CampaignAdminError> {
    let campaign_id = campaign.campaign_id.clone();
    let superseded = campaign.current_plan_id.clone();

    let changed = campaign.advance(plan_id.clone(), Utc::now());
    if changed {
        campaigns.put_campaign(&campaign).await.map_err(|source| {
            CampaignAdminError::NotAdvanced {
                campaign_id: campaign_id.clone(),
                plan_id: plan_id.clone(),
                source,
            }
        })?;
    }
    tracing::info!(
        %campaign_id,
        %plan_id,
        supersedes = superseded.as_ref().map(PlanId::as_str),
        changed,
        "campaign advanced onto a new plan"
    );
    Ok(Advanced {
        superseded,
        changed,
    })
}

/// Delete the record of `campaign` and keep its plans; refused while one of
/// its plans runs. A filing that advances during the scan is caught by a
/// re-read. With no CAS, a filing that advances between the re-read and the
/// delete leaves its plan running.
pub async fn delete(
    campaigns: &dyn CampaignStore,
    plans: &dyn PlanStore,
    campaign: &Campaign,
) -> Result<(), CampaignAdminError> {
    let mut running = running_plans(plans, &campaign.campaign_id).await?;
    // Only a filing moves the head, so a new head is a plan that the scan can
    // miss. It was filed a moment ago and counts as running.
    let current = require(campaigns, &campaign.campaign_id).await?;
    if let Some(head) = current
        .current_plan_id
        .filter(|head| campaign.current_plan_id.as_ref() != Some(head) && !running.contains(head))
    {
        running.push(head);
    }
    if !running.is_empty() {
        tracing::warn!(
            campaign_id = %campaign.campaign_id,
            plans = ?running,
            "campaign delete refused: plans still run"
        );
        return Err(CampaignAdminError::StillRunning {
            campaign_id: campaign.campaign_id.clone(),
            plans: running,
        });
    }
    campaigns.delete_campaign(&campaign.campaign_id).await?;
    tracing::info!(campaign_id = %campaign.campaign_id, "campaign deleted");
    Ok(())
}

/// The plans that `campaign_id` filed that have not ended and have no cancel
/// requested. An unparseable manifest is skipped: it names no campaign, and a
/// refusal on it would block every delete.
async fn running_plans(
    plans: &dyn PlanStore,
    campaign_id: &CampaignId,
) -> anyhow::Result<Vec<PlanId>> {
    let cancel_requested: HashSet<PlanId> = plans
        .list_cancel_markers()
        .await
        .context("listing cancel markers")?
        .into_iter()
        .collect();
    let filter = PlanFilter {
        campaign_id: Some(campaign_id.clone()),
        ..PlanFilter::default()
    };
    let listing = pipette_store::drain_matching_plans(plans, &filter)
        .await
        .context("listing the campaign's plans")?;
    listing.unreadable.iter().for_each(|bad| {
        tracing::warn!(
            %campaign_id,
            manifest = %bad.key,
            reason = %bad.reason,
            "unreadable plan manifest skipped by a campaign delete"
        );
    });
    Ok(listing
        .items
        .into_iter()
        .filter(|plan| {
            !plan.status.is_terminal()
                && plan.status != PlanStatus::Cancelling
                && !cancel_requested.contains(&plan.plan_id)
        })
        .map(|plan| plan.plan_id)
        .collect())
}

/// Create a campaign from a plan document on disk: file the plan under the new
/// campaign's id, then write the campaign around it.
pub async fn create(
    stores: &Stores,
    settings: IngestSettings<'_>,
    new: NewCampaign,
    document: &Path,
    note: Option<String>,
) -> Result<(Campaign, IngestReport), CampaignAdminError> {
    // The host is past every gate, so every organization's entries count.
    let catalog = crate::org_catalog::load_server_catalog(&*stores.catalog).await?;
    let ctx = IngestContext::with_settings(stores, &catalog, settings);
    let membership = CampaignMembership::first(&new);
    let report =
        plan_cli::ingest(&ctx, PlanInput::Document(document), note, None, membership).await?;
    let campaign = create_around(
        &*stores.campaigns,
        &*stores.plans,
        new,
        report.plan_id.clone(),
    )
    .await?;
    Ok((campaign, report))
}

/// Expand and validate a plan document, and write nothing: the dry run of
/// [`create`], and of [`dry_run_into`] once the campaign is found.
pub async fn dry_run_document(
    stores: &Stores,
    settings: IngestSettings<'_>,
    owner: &VaultOwner,
    document: &Path,
    note: Option<String>,
) -> Result<DryRunReport, CampaignAdminError> {
    let catalog = crate::org_catalog::load_server_catalog(&*stores.catalog).await?;
    let ctx = IngestContext::with_settings(stores, &catalog, settings);
    Ok(plan_cli::dry_run_document(&ctx, owner, document, note)
        .await?
        .report)
}

/// File a plan from disk into a campaign, then advance it onto the plan. The
/// API ingests a request body instead and calls [`advance`] itself.
pub async fn ingest_into(
    stores: &Stores,
    settings: IngestSettings<'_>,
    campaign_id: &CampaignId,
    input: PlanInput<'_>,
    note: Option<String>,
) -> Result<(IngestReport, Advanced), CampaignAdminError> {
    let campaign = require(&*stores.campaigns, campaign_id).await?;
    require_active(&campaign)?;
    let catalog = crate::org_catalog::load_server_catalog(&*stores.catalog).await?;
    // Read off the campaign, never from an argument: a revision supersedes
    // the plan the campaign is on.
    let membership = CampaignMembership::of(&campaign);

    let ctx = IngestContext::with_settings(stores, &catalog, settings);
    let report = plan_cli::ingest(&ctx, input, note, None, membership).await?;

    let advanced = advance(
        &*stores.campaigns,
        &*stores.plans,
        campaign_id,
        report.plan_id.clone(),
    )
    .await?;
    Ok((report, advanced))
}

/// Expand and validate a plan document for a campaign, and write nothing.
pub async fn dry_run_into(
    stores: &Stores,
    settings: IngestSettings<'_>,
    campaign_id: &CampaignId,
    path: &Path,
    note: Option<String>,
) -> Result<DryRunReport, CampaignAdminError> {
    let campaign = require(&*stores.campaigns, campaign_id).await?;
    require_active(&campaign)?;
    dry_run_document(stores, settings, &campaign.owner, path, note).await
}

/// The plan a campaign runs: what a variant edit revises. Read apart from its
/// document and the edit, so a surface can gate on the plan before either.
#[derive(Debug)]
pub struct Head {
    campaign: Campaign,
    manifest: PlanManifest,
}

impl Head {
    /// Read the manifest of the plan `campaign` runs. `base`, when given, is
    /// the plan the caller read its indices from; a campaign that moved off it
    /// is refused.
    pub async fn read(
        plans: &dyn PlanStore,
        campaign: Campaign,
        base: Option<&PlanId>,
    ) -> Result<Self, CampaignAdminError> {
        require_active(&campaign)?;
        let plan_id = campaign
            .current_plan_id
            .clone()
            .ok_or_else(|| CampaignAdminError::NoPlan(campaign.campaign_id.clone()))?;
        if let Some(base) = base {
            on_base(&campaign, &plan_id, base)?;
        }
        let Some(manifest) = plans.get_plan(&plan_id).await? else {
            return Err(CampaignAdminError::NoDocument(plan_id));
        };
        Ok(Self { campaign, manifest })
    }

    /// Refuse an edit whose indices were read off `base` when the head is
    /// another plan: [`Head::read`]'s check, for a caller that learns `base`
    /// after the read.
    pub fn require_base(&self, base: &PlanId) -> Result<(), CampaignAdminError> {
        on_base(&self.campaign, &self.manifest.plan_id, base)
    }

    pub fn campaign(&self) -> &Campaign {
        &self.campaign
    }

    pub fn manifest(&self) -> &PlanManifest {
        &self.manifest
    }

    /// Whether the head is the revision of `base` filed from `document`: what
    /// a retried document revision finds once its first attempt landed.
    pub async fn landed(
        &self,
        plans: &dyn PlanStore,
        base: &PlanId,
        document: &str,
    ) -> Result<bool, CampaignAdminError> {
        if self.manifest.previous_plan_id.as_ref() != Some(base) {
            return Ok(false);
        }
        Ok(filed_from(
            &self.manifest,
            &self.document(plans).await?,
            document,
        ))
    }

    pub async fn document(&self, plans: &dyn PlanStore) -> Result<String, CampaignAdminError> {
        document_of(plans, &self.manifest).await
    }

    /// Apply `edit` to the head's document: the revision to file.
    pub async fn revise(
        self,
        plans: &dyn PlanStore,
        edit: &VariantEdit,
    ) -> Result<Revision, CampaignAdminError> {
        let previous_document = self.document(plans).await?;
        let revised = edit.apply(&previous_document)?;
        let document = format!(
            "# A variant edit of {}: {} variant(s) removed, {} added.\n{revised}",
            self.manifest.plan_id,
            edit.remove.len(),
            edit.add.len(),
        );
        Ok(Revision {
            campaign: self.campaign,
            previous: self.manifest,
            previous_document,
            document,
            change: Change::Variants {
                removed: edit.remove.len(),
                added: edit.add.len(),
                restored: edit.restored.clone(),
            },
            rerun: BTreeSet::new(),
            recalculate: BTreeSet::new(),
            offer: Offer::default(),
        })
    }

    /// `document` in place of the head's own: the revision to file. A head filed
    /// as job bodies has no document and is refused; the document also rekeys
    /// its legacy job digests.
    pub async fn replace(
        self,
        plans: &dyn PlanStore,
        document: String,
    ) -> Result<Revision, CampaignAdminError> {
        let previous_document = self.document(plans).await?;
        Ok(Revision {
            campaign: self.campaign,
            previous: self.manifest,
            previous_document,
            document,
            change: Change::Document,
            rerun: BTreeSet::new(),
            recalculate: BTreeSet::new(),
            offer: Offer::default(),
        })
    }

    /// Build the revision to file. It holds the document of `target`, an
    /// earlier plan of the campaign's chain, in place of the head's own.
    /// Unless the target has the head's own document, the revision offers the
    /// finished runs of every plan behind the head, so the target's results
    /// come back.
    pub async fn revert(
        self,
        plans: &dyn PlanStore,
        target: &PlanId,
    ) -> Result<Revision, CampaignAdminError> {
        let manifest = match campaign_chain::find(plans, &self.campaign, target).await? {
            Some(Some(manifest)) => manifest,
            Some(None) => {
                tracing::warn!(
                    campaign_id = %self.campaign.campaign_id,
                    plan_id = %target,
                    "campaign revert refused: the plan's manifest is gone"
                );
                return Err(CampaignAdminError::NoDocument(target.clone()));
            }
            None => {
                tracing::warn!(
                    campaign_id = %self.campaign.campaign_id,
                    plan_id = %target,
                    "campaign revert refused: plan not in the campaign's history"
                );
                return Err(CampaignAdminError::NotInCampaign {
                    campaign_id: self.campaign.campaign_id.clone(),
                    plan_id: target.clone(),
                });
            }
        };
        let document = document_of(plans, &manifest).await?;
        let previous_document = self.document(plans).await?;
        // A revert to the head's own document asks for the plan that the
        // campaign already runs, so it offers nothing and files nothing.
        let offer = if filed_from(&self.manifest, &previous_document, &document) {
            Offer::default()
        } else {
            Offer {
                earlier: true,
                ..Offer::default()
            }
        };
        Ok(Revision {
            campaign: self.campaign,
            previous: self.manifest,
            previous_document,
            document,
            change: Change::Revert(manifest.plan_id),
            rerun: BTreeSet::new(),
            recalculate: BTreeSet::new(),
            offer,
        })
    }

    /// The head's document as it is, running `job_ids` again: the revision to
    /// file. Each must be a job of the head with a recorded result. A lapse is
    /// not one: the job may still run for another plan.
    pub async fn rerun(
        self,
        plans: &dyn PlanStore,
        submissions: &dyn SubmissionStore,
        job_ids: BTreeSet<JobId>,
    ) -> Result<Revision, CampaignAdminError> {
        self.require_held(&job_ids)?;
        let document = self.document(plans).await?;
        // Owned ids: a stream of borrowed ones trips higher-ranked `Send`
        // inference in the handlers that await this.
        futures::stream::iter(job_ids.clone())
            .map(|job_id| async move {
                match submissions.find_job(&job_id).await? {
                    Some(_) => Ok(()),
                    None => Err(CampaignAdminError::Unanswered(job_id)),
                }
            })
            .buffer_unordered(PLAN_JOB_CONCURRENCY)
            .try_collect::<()>()
            .await?;
        Ok(self.filed_again(document, Change::Rerun(None), job_ids, BTreeSet::new()))
    }

    /// [`Self::rerun`] over every job of the head with a recorded result that
    /// `which` takes. A job still pending carries over, as it would anyway.
    pub async fn rerun_answered(
        self,
        plans: &dyn PlanStore,
        submissions: &dyn SubmissionStore,
        which: Answered,
    ) -> Result<Revision, CampaignAdminError> {
        // `content_keys`, not `job_ids`: a job with no digest cannot be re-run.
        let job_ids: Vec<JobId> = self.manifest.content_keys.keys().cloned().collect();
        let rerun: BTreeSet<JobId> = futures::stream::iter(job_ids)
            .map(|job_id| async move {
                let record = submissions.find_job(&job_id).await?;
                Ok::<_, CampaignAdminError>(
                    record
                        .filter(|record| answered_takes(which, &record.body))
                        .map(|_| job_id),
                )
            })
            .buffer_unordered(PLAN_JOB_CONCURRENCY)
            .try_filter_map(|job_id| async move { Ok(job_id) })
            .try_collect()
            .await?;
        if rerun.is_empty() {
            return Err(CampaignAdminError::NoneAnswered {
                plan_id: self.manifest.plan_id.clone(),
                which,
            });
        }
        let document = self.document(plans).await?;
        Ok(self.filed_again(document, Change::Rerun(Some(which)), rerun, BTreeSet::new()))
    }

    /// The head's document as it is, scoring the completions `job_ids`
    /// recorded again under new jobs that no device runs: the revision to
    /// file. Each must be a job of the head whose record is an eval's success.
    pub async fn recalculate(
        self,
        plans: &dyn PlanStore,
        submissions: &dyn SubmissionStore,
        job_ids: BTreeSet<JobId>,
    ) -> Result<Revision, CampaignAdminError> {
        self.require_held(&job_ids)?;
        let document = self.document(plans).await?;
        futures::stream::iter(job_ids.clone())
            .map(|job_id| async move {
                match submissions.find_job(&job_id).await? {
                    None => Err(CampaignAdminError::Unanswered(job_id)),
                    Some(record) if !recalculation::rescorable(&record.body) => {
                        Err(CampaignAdminError::NotRescorable(job_id))
                    }
                    Some(_) => Ok(()),
                }
            })
            .buffer_unordered(PLAN_JOB_CONCURRENCY)
            .try_collect::<()>()
            .await?;
        Ok(self.filed_again(
            document,
            Change::Recalculate { all: false },
            BTreeSet::new(),
            job_ids,
        ))
    }

    /// [`Self::recalculate`] over every job of the head whose record is an
    /// eval's success.
    pub async fn recalculate_evals(
        self,
        plans: &dyn PlanStore,
        submissions: &dyn SubmissionStore,
    ) -> Result<Revision, CampaignAdminError> {
        // `content_keys`, not `job_ids`: a job with no digest cannot be placed.
        let job_ids: Vec<JobId> = self.manifest.content_keys.keys().cloned().collect();
        let recalculate: BTreeSet<JobId> = futures::stream::iter(job_ids)
            .map(|job_id| async move {
                let record = submissions.find_job(&job_id).await?;
                Ok::<_, CampaignAdminError>(
                    record
                        .filter(|record| recalculation::rescorable(&record.body))
                        .map(|_| job_id),
                )
            })
            .buffer_unordered(PLAN_JOB_CONCURRENCY)
            .try_filter_map(|job_id| async move { Ok(job_id) })
            .try_collect()
            .await?;
        if recalculate.is_empty() {
            return Err(CampaignAdminError::NoneRescorable(
                self.manifest.plan_id.clone(),
            ));
        }
        let document = self.document(plans).await?;
        Ok(self.filed_again(
            document,
            Change::Recalculate { all: true },
            BTreeSet::new(),
            recalculate,
        ))
    }

    /// Refuse a re-run or a recalculation that names no job, or one the head
    /// cannot place again.
    fn require_held(&self, job_ids: &BTreeSet<JobId>) -> Result<(), CampaignAdminError> {
        if job_ids.is_empty() {
            return Err(CampaignAdminError::NothingToRerun);
        }
        match job_ids
            .iter()
            .find(|job_id| !self.manifest.content_keys.contains_key(*job_id))
        {
            Some(job_id) => Err(CampaignAdminError::NotInPlan {
                plan_id: self.manifest.plan_id.clone(),
                job_id: job_id.clone(),
            }),
            None => Ok(()),
        }
    }

    /// The head's own `document`, filed again with the named jobs placed afresh.
    fn filed_again(
        self,
        document: String,
        change: Change,
        rerun: BTreeSet<JobId>,
        recalculate: BTreeSet<JobId>,
    ) -> Revision {
        Revision {
            campaign: self.campaign,
            previous: self.manifest,
            previous_document: document.clone(),
            document,
            change,
            rerun,
            recalculate,
            offer: Offer::default(),
        }
    }
}

/// Reads the outcome as the plan page does ([`SettledOutcome::from_message_type`]).
fn answered_takes(which: Answered, body: &serde_json::Value) -> bool {
    match which {
        Answered::All => true,
        Answered::Failed => {
            let message_type = body.get("message_type").and_then(serde_json::Value::as_str);
            SettledOutcome::from_message_type(message_type) == SettledOutcome::Failed
        }
    }
}

fn answered_phrase(which: &Answered) -> &'static str {
    match which {
        Answered::Failed => "failed",
        Answered::All => "finished or failed",
    }
}

/// Whether `manifest`'s plan, whose stored document is `stored`, was filed from
/// `document`. A document with a model token is stored in another text, so
/// such a document matches by the digest of the text that the client sent.
fn filed_from(manifest: &PlanManifest, stored: &str, document: &str) -> bool {
    stored == document
        || manifest
            .submitted_sha256
            .as_ref()
            .is_some_and(|submitted| submitted.as_ref() == sha256_hex(document))
}

/// Whether `current`, the campaign after a second read, has another owner or
/// head than `before`: an import's records belong to `before`'s owner.
pub fn changed_under(before: &Campaign, current: &Campaign) -> bool {
    current.owner != before.owner || current.current_plan_id != before.current_plan_id
}

/// Refuse `base` unless it is `current`, the plan `campaign` runs.
/// Refuse to file a plan into an archived campaign, dry runs included. Checked
/// before the filing, so an archive that lands during one does not stop it.
pub fn require_active(campaign: &Campaign) -> Result<(), CampaignAdminError> {
    if campaign.state.is_active() {
        return Ok(());
    }
    Err(CampaignAdminError::Archived(campaign.campaign_id.clone()))
}

fn on_base(campaign: &Campaign, current: &PlanId, base: &PlanId) -> Result<(), CampaignAdminError> {
    if base == current {
        return Ok(());
    }
    Err(CampaignAdminError::NotOnPlan {
        campaign_id: campaign.campaign_id.clone(),
        expected: base.clone(),
        current: current.clone(),
    })
}

/// A variant edit, a document revision or a re-run of the head, ready to
/// file. Only [`Head`] builds one, so `previous` is the head.
#[derive(Debug)]
pub struct Revision {
    campaign: Campaign,
    previous: PlanManifest,
    /// `previous`'s document, which rekeys a job filed before job sources were
    /// stored.
    previous_document: String,
    document: String,
    /// What the filer asked for, which [`Self::change`] records.
    change: Change,
    /// The head's jobs to run again rather than carry. Empty for an edit.
    rerun: BTreeSet<JobId>,
    /// The head's jobs to score again under new jobs rather than carry. Empty
    /// but for a recalculation.
    recalculate: BTreeSet<JobId>,
    /// The plans beside the head whose finished runs may answer cells too
    /// ([`Self::carrying`]).
    offer: Offer,
}

/// The plans beside the head whose finished runs a revision offers to the cells
/// the head has not answered, as it offers the head's own.
#[derive(Debug, Clone, Default)]
pub struct Offer {
    /// The plans before the head (`carry_earlier`).
    earlier: bool,
    /// Plans named to inherit from, of any campaign (`inherit_from`), in the
    /// order offered.
    plans: Vec<PlanManifest>,
    /// The plans each named plan's campaign filed after it, whose re-runs and
    /// lapses give up a named plan's job too.
    later: Vec<PlanManifest>,
}

/// `inherit_from`, then each plan of `add_from` and of `restore` that it does
/// not name yet: a plan whose variants an edit adds offers its runs too.
pub fn inheriting(
    inherit_from: &[PlanId],
    add_from: &[PlanId],
    restore: &[RestoredVariant],
) -> Vec<PlanId> {
    add_from
        .iter()
        .chain(restore.iter().map(|variant| &variant.plan_id))
        .fold(inherit_from.to_vec(), |mut named, plan_id| {
            if !named.contains(plan_id) {
                named.push(plan_id.clone());
            }
            named
        })
}

impl Offer {
    /// `inherit_from` read off `plans`, refused when one is not there.
    pub async fn read(
        plans: &dyn PlanStore,
        campaigns: &dyn CampaignStore,
        earlier: bool,
        inherit_from: &[PlanId],
    ) -> Result<Self, CampaignAdminError> {
        let named: Vec<PlanManifest> = futures::stream::iter(inherit_from)
            .then(|plan_id| async move {
                plans
                    .get_plan(plan_id)
                    .await?
                    .ok_or_else(|| CampaignAdminError::NoSuchPlan(plan_id.clone()))
            })
            .try_collect()
            .await?;
        // A loop: a stream here breaks the `Send` bound of the handlers.
        let mut later = Vec::new();
        for manifest in &named {
            later.extend(filed_after(plans, campaigns, manifest).await?);
        }
        Ok(Self {
            earlier,
            plans: named,
            later,
        })
    }

    /// The named plans, which a surface checks the caller may read.
    pub fn plans(&self) -> &[PlanManifest] {
        &self.plans
    }

    /// The variants of each plan of `add_from`, in order, for a variant edit to
    /// append ([`variant_edit::standalone_variants`]). Each must be a named
    /// plan, so the caller's read of it was checked.
    pub async fn variants_of(
        &self,
        plans: &dyn PlanStore,
        add_from: &[PlanId],
    ) -> Result<Vec<toml::Table>, CampaignAdminError> {
        // A loop: a stream here breaks the `Send` bound of the handlers.
        let mut variants = Vec::new();
        for plan_id in add_from {
            let manifest = self
                .plans
                .iter()
                .find(|manifest| &manifest.plan_id == plan_id)
                .ok_or_else(|| CampaignAdminError::NoSuchPlan(plan_id.clone()))?;
            let document = document_of(plans, manifest).await?;
            variants.extend(variant_edit::standalone_variants(&document)?);
        }
        Ok(variants)
    }

    /// The variants that `restore` names, in order, for a variant edit to
    /// append ([`variant_edit::standalone_variants`]). Each plan must be a
    /// named plan so the caller's read check applies.
    pub async fn restored_variants(
        &self,
        plans: &dyn PlanStore,
        restore: &[RestoredVariant],
    ) -> Result<Vec<toml::Table>, CampaignAdminError> {
        // A loop: a stream here breaks the `Send` bound of the handlers.
        let mut variants = Vec::new();
        let mut sources = HashMap::new();
        for RestoredVariant {
            plan_id,
            variant_index,
        } in restore
        {
            if !sources.contains_key(plan_id) {
                let manifest = self
                    .plans
                    .iter()
                    .find(|manifest| &manifest.plan_id == plan_id)
                    .ok_or_else(|| CampaignAdminError::NoSuchPlan(plan_id.clone()))?;
                let source =
                    variant_edit::standalone_variants(&document_of(plans, manifest).await?)?;
                sources.insert(plan_id.clone(), source);
            }
            let source: &Vec<toml::Table> = sources
                .get(plan_id)
                .ok_or_else(|| CampaignAdminError::NoSuchPlan(plan_id.clone()))?;
            let count = source.len();
            let Some(variant) = source.get(*variant_index).cloned() else {
                tracing::warn!(
                    plan_id = %plan_id,
                    variant_index,
                    variant_count = count,
                    "refused: the variant to restore is not in its plan"
                );
                return Err(CampaignAdminError::NoSuchSourceVariant {
                    plan_id: plan_id.clone(),
                    index: *variant_index,
                    count,
                });
            };
            variants.push(variant);
        }
        Ok(variants)
    }

    fn is_empty(&self) -> bool {
        !self.earlier && self.plans.is_empty()
    }
}

async fn document_of(
    plans: &dyn PlanStore,
    manifest: &PlanManifest,
) -> Result<String, CampaignAdminError> {
    let plan_id = &manifest.plan_id;
    match plan_document::read_source(plans, manifest).await? {
        SourceRead::Present(source) => Ok(source.text().to_string()),
        SourceRead::NoSource | SourceRead::Collected => {
            Err(CampaignAdminError::NoDocument(plan_id.clone()))
        }
        SourceRead::Mismatch => Err(CampaignAdminError::SourceMismatch(plan_id.clone())),
    }
}

/// The plans `named`'s campaign filed after it, newest first. None where the
/// campaign's chain does not reach `named`, which then says nothing of it.
async fn filed_after(
    plans: &dyn PlanStore,
    campaigns: &dyn CampaignStore,
    named: &PlanManifest,
) -> anyhow::Result<Vec<PlanManifest>> {
    let Some(campaign_id) = &named.campaign_id else {
        return Ok(Vec::new());
    };
    let Some(campaign) = campaigns.get_campaign(campaign_id).await? else {
        return Ok(Vec::new());
    };
    let mut later = Vec::new();
    let mut visited = HashSet::new();
    let mut next = campaign.current_plan_id;
    while let Some(plan_id) = next {
        if plan_id == named.plan_id {
            return Ok(later);
        }
        if !visited.insert(plan_id.clone()) {
            break;
        }
        let Some(manifest) = plans.get_plan(&plan_id).await? else {
            break;
        };
        next = manifest.previous_plan_id.clone();
        later.push(manifest);
    }
    Ok(Vec::new())
}

#[derive(Debug, Clone)]
enum Change {
    Variants {
        removed: usize,
        added: usize,
        restored: Vec<RestoredVariant>,
    },
    Document,
    /// `None` for the jobs the filer named.
    Rerun(Option<Answered>),
    /// `all` for every finished eval of the head.
    Recalculate {
        all: bool,
    },
    /// The plan whose document the revision copies.
    Revert(PlanId),
}

impl Revision {
    /// The change that the revision makes, with counts of what the filer
    /// requested. The new plan stores the change next to its note.
    pub fn change(&self) -> RevisionChange {
        let carry_earlier = self.offer.earlier;
        let inherit_from = || {
            self.offer
                .plans
                .iter()
                .map(|plan| plan.plan_id.clone())
                .collect()
        };
        match &self.change {
            Change::Variants {
                removed,
                added,
                restored,
            } => RevisionChange::Variants {
                removed_variant_count: *removed,
                added_variant_count: *added,
                carry_earlier,
                inherit_from: inherit_from(),
                restored: restored.clone(),
            },
            Change::Document => RevisionChange::Document {
                carry_earlier,
                inherit_from: inherit_from(),
            },
            Change::Rerun(answered) => RevisionChange::Rerun {
                answered: *answered,
                job_count: self.rerun.len(),
            },
            Change::Recalculate { all } => RevisionChange::Recalculate {
                all_evals: *all,
                job_count: self.recalculate.len(),
            },
            Change::Revert(to) => RevisionChange::Revert {
                to_plan_id: to.clone(),
            },
        }
    }

    /// Let a cell the head has not answered take a finished run of the plans
    /// `offer` names, as it takes one of the head's.
    pub fn carrying(self, offer: Offer) -> Self {
        Self { offer, ..self }
    }

    pub fn document(&self) -> &str {
        &self.document
    }

    pub fn campaign_id(&self) -> &CampaignId {
        &self.campaign.campaign_id
    }

    /// [`changed_under`] against the campaign this revision read.
    pub fn changed_under(&self, current: &Campaign) -> bool {
        changed_under(&self.campaign, current)
    }

    /// Whether filing this would only repeat the head: its own document, with
    /// no job placed afresh. A surface answers that as done, and files nothing.
    pub fn changes_nothing(&self) -> bool {
        filed_from(&self.previous, &self.previous_document, &self.document)
            && self.rerun.is_empty()
            && self.recalculate.is_empty()
            && self.offer.is_empty()
    }

    /// What filing `bodies`, this revision's dry-run bodies, would do to the
    /// head. `membership` is [`Self::membership`]'s.
    pub async fn preview(
        &self,
        membership: CampaignMembership,
        bodies: &[serde_json::Value],
        plans: &dyn PlanStore,
        submissions: &dyn SubmissionStore,
        orgs: &dyn OrgStore,
    ) -> Result<RevisionPreview, crate::plan_ingestion::IngestError> {
        let CarryDraw {
            carried,
            new_cells,
            requirements_rerun,
            satisfied,
            offered,
            held,
        } = draw_carry(membership, bodies, plans, submissions, orgs).await?;
        let named: HashSet<&PlanId> = self
            .offer
            .plans
            .iter()
            .map(|manifest| &manifest.plan_id)
            .collect();
        let inherited = offered
            .values()
            .filter(|plan_id| named.contains(plan_id))
            .count();
        let kept: HashSet<&JobId> = carried
            .iter()
            .chain(&self.rerun)
            .chain(&self.recalculate)
            .collect();
        Ok(RevisionPreview {
            unchanged: false,
            carried_job_count: carried.len(),
            new_job_count: new_cells,
            requirements_rerun_job_count: requirements_rerun,
            satisfied_job_count: satisfied,
            restored_job_count: offered.len() - inherited,
            inherited_job_count: inherited,
            rerun_job_count: self.rerun.len(),
            recalculated_job_count: self.recalculate.len(),
            dropped_job_ids: self
                .previous
                .job_ids
                .iter()
                .filter(|job_id| !kept.contains(job_id))
                .cloned()
                .collect(),
            held_jobs: held,
            repeated_variants: variant_edit::repeated_variants(&self.document)
                .map_err(|e| crate::plan_ingestion::IngestError::Other(e.into()))?,
        })
    }

    /// Superseding the head, and carrying its jobs over but the re-run and
    /// recalculated ones. v1 digests are rekeyed from stored sources, or the
    /// head's document, so only jobs matching the v2 rule carry; an unknown
    /// digest, such as a drift marker, stays. A sourceless job is assumed to
    /// have run the head's body, which one joined from another plan under v1
    /// may not have.
    pub async fn membership(&self, plans: &dyn PlanStore) -> anyhow::Result<CampaignMembership> {
        let mut previous = self.previous.clone();
        let recorded: Vec<(JobId, String)> = previous
            .content_keys
            .iter()
            .map(|(job_id, digest)| (job_id.clone(), digest.clone()))
            .collect();
        let sources: Vec<(JobId, String, Option<serde_json::Value>)> =
            futures::stream::iter(recorded)
                .map(|(job_id, recorded)| async move {
                    let source = plans.get_job_source(&job_id).await?;
                    anyhow::Ok((job_id, recorded, source))
                })
                .buffer_unordered(PLAN_JOB_CONCURRENCY)
                .try_collect()
                .await?;
        let from_document = if sources.iter().any(|(_, _, source)| source.is_none()) {
            rekeyed_by_document(
                self.previous_document.clone(),
                self.previous.benchmark_types.clone(),
            )
            .await
        } else {
            HashMap::new()
        };
        let mut bodies = HashMap::new();
        sources.into_iter().for_each(|(job_id, recorded, source)| {
            let digest = match &source {
                Some(body) => {
                    (legacy_content_digest(body) == recorded).then(|| content_digest(body))
                }
                None => from_document.get(&recorded).cloned(),
            };
            if let Some(digest) = digest {
                previous.content_keys.insert(job_id.clone(), digest);
            }
            if let Some(body) = source {
                bodies.insert(job_id, body);
            }
        });
        let mut membership =
            CampaignMembership::carrying(&self.campaign, &previous, &self.rerun, &self.recalculate);
        let offered = if self.offer.is_empty() {
            Vec::new()
        } else {
            self.offered_jobs(plans).await?
        };
        membership.carry = membership.carry.with_sources(bodies).with_offered(offered);
        Ok(membership)
    }

    /// The jobs of the plans [`Offer`] names, with their filed bodies: the
    /// earlier plans newest first, then the named ones. Leaves out the head's
    /// jobs and those it re-ran, and any job one of those plans, or a plan
    /// filed after a named one, re-ran or let lapse.
    async fn offered_jobs(&self, plans: &dyn PlanStore) -> anyhow::Result<Vec<Offered>> {
        let mut chain = Vec::new();
        let mut visited = HashSet::from([self.previous.plan_id.clone()]);
        let mut next = self
            .offer
            .earlier
            .then(|| self.previous.previous_plan_id.clone())
            .flatten();
        while let Some(plan_id) = next.filter(|plan_id| visited.insert(plan_id.clone())) {
            let Some(manifest) = plans.get_plan(&plan_id).await? else {
                break;
            };
            next = manifest.previous_plan_id.clone();
            chain.push(manifest);
        }
        let offering: Vec<&PlanManifest> = chain.iter().chain(&self.offer.plans).collect();
        let rerun: HashSet<&JobId> = std::iter::once(&self.previous)
            .chain(offering.iter().copied())
            .chain(&self.offer.later)
            .flat_map(|manifest| manifest.rerun_of.values())
            .collect();
        let lapsed: HashSet<&JobId> = offering
            .iter()
            .copied()
            .chain(&self.offer.later)
            .flat_map(|manifest| manifest.expired_jobs.keys())
            .collect();
        let mut seen: HashSet<&JobId> = self.previous.job_ids.iter().collect();
        let job_ids: Vec<(PlanId, JobId)> = offering
            .iter()
            .flat_map(|manifest| {
                manifest
                    .job_ids
                    .iter()
                    .map(|job_id| (&manifest.plan_id, job_id))
            })
            .filter(|(_, job_id)| {
                !rerun.contains(job_id) && !lapsed.contains(job_id) && seen.insert(job_id)
            })
            .map(|(plan_id, job_id)| (plan_id.clone(), job_id.clone()))
            .collect();
        // `buffered`, so the jobs keep the chain's order.
        let sources: Vec<Option<Offered>> = futures::stream::iter(job_ids)
            .map(|(plan_id, job_id)| async move {
                let source = plans.get_job_source(&job_id).await?;
                anyhow::Ok(source.map(|body| Offered {
                    plan_id,
                    job_id,
                    body,
                }))
            })
            .buffered(PLAN_JOB_CONCURRENCY)
            .try_collect()
            .await?;
        Ok(sources.into_iter().flatten().collect())
    }
}

/// [`rekeyed`] over the bodies `document` expands to, typed by `recorded`, its
/// manifest's `benchmark_types`. Empty if the document no longer expands.
async fn rekeyed_by_document(
    document: String,
    recorded: BTreeMap<BenchmarkId, BenchmarkType>,
) -> HashMap<String, String> {
    let expanded = tokio::task::spawn_blocking(move || {
        let ids = pipette_planner::benchmark_ids(&document)?;
        let types = crate::plan_cells::filed_types(&recorded, &ids);
        let plan = pipette_planner::parse(&document, &types)?;
        anyhow::Ok(pipette_planner::expand(&plan)?.bodies)
    })
    .await
    .map_err(anyhow::Error::new)
    .flatten();
    match expanded {
        Ok(bodies) => rekeyed(&bodies),
        Err(error) => {
            tracing::warn!(error = %format!("{error:#}"), "head document no longer expands");
            HashMap::new()
        }
    }
}

/// The v1 digest of each body, mapped to its v2 digest. A v1 digest two
/// different bodies share is left out: it cannot say which one a job was.
fn rekeyed(bodies: &[serde_json::Value]) -> HashMap<String, String> {
    let mut by_legacy: HashMap<String, Option<String>> = HashMap::new();
    bodies.iter().for_each(|body| {
        let digest = content_digest(body);
        by_legacy
            .entry(legacy_content_digest(body))
            .and_modify(|known| {
                if known.as_ref() != Some(&digest) {
                    *known = None;
                }
            })
            .or_insert(Some(digest));
    });
    by_legacy
        .into_iter()
        .filter_map(|(legacy, digest)| Some((legacy, digest?)))
        .collect()
}

/// A dry run of a revision: the document it would file, the dry-run report on
/// that document, and what it would do to the plan it replaces.
#[derive(Debug, serde::Serialize)]
pub struct RevisionDryRun {
    /// The document without the token of any model.
    pub document: String,
    #[serde(flatten)]
    pub report: DryRunReport,
    #[serde(flatten)]
    pub preview: RevisionPreview,
}

impl From<RevisionDryRun> for pipette_plan_types::RevisionDryRun {
    fn from(dry: RevisionDryRun) -> Self {
        Self {
            document: dry.document,
            report: dry.report.into(),
            preview: dry.preview.into(),
        }
    }
}

impl RevisionDryRun {
    /// The dry run of `document`, with every model token removed.
    pub fn new(
        document: &str,
        report: DryRunReport,
        preview: RevisionPreview,
    ) -> Result<Self, plan_document::StoredDocumentError> {
        Ok(Self {
            document: plan_document::without_model_tokens(document)?,
            report,
            preview,
        })
    }

    /// The dry run of a revision whose document is the head's own, which
    /// expands to `jobs` and `plan_warnings`. It runs no check and no import.
    pub fn unchanged(
        document: &str,
        jobs: &[serde_json::Value],
        plan_warnings: Vec<String>,
    ) -> Result<Self, plan_document::StoredDocumentError> {
        tracing::info!("revision dry run found the head's own document and ran no checks");
        let report = DryRunReport::unchecked(jobs, plan_warnings);
        Self::new(document, report, RevisionPreview::unchanged())
    }
}

/// What filing a revision would do to the plan it replaces, counted as
/// [`Revised`] counts what it did.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct RevisionPreview {
    /// The document is the head's own, so filing it files nothing and every
    /// count is zero.
    pub unchanged: bool,
    /// That plan's jobs the revision would hold, finished ones included.
    pub carried_job_count: usize,
    /// Cells that plan never ran, which the revision would queue.
    pub new_job_count: usize,
    /// Cells that plan ran under other requirements, which the revision would
    /// run again: the run failed, or its device or build is not known to meet them.
    pub requirements_rerun_job_count: usize,
    /// Cells that an earlier finished run answers, which the revision would
    /// hold that run for and not queue.
    pub satisfied_job_count: usize,
    /// Finished jobs of the campaign's earlier plans the revision would hold.
    pub restored_job_count: usize,
    /// Finished jobs of the `inherit_from` plans the revision would hold.
    pub inherited_job_count: usize,
    pub rerun_job_count: usize,
    pub recalculated_job_count: usize,
    /// That plan's jobs the revision would stop: neither held, run again, nor
    /// scored again.
    pub dropped_job_ids: Vec<JobId>,
    /// Each body that would hold an existing job, and the plan it comes from.
    pub held_jobs: Vec<pipette_plan_types::HeldJob>,
    /// The revised document's variants that repeat an earlier one.
    pub repeated_variants: Vec<usize>,
}

impl From<RevisionPreview> for pipette_plan_types::RevisionPreview {
    fn from(preview: RevisionPreview) -> Self {
        Self {
            unchanged: preview.unchanged,
            carried_job_count: preview.carried_job_count,
            new_job_count: preview.new_job_count,
            requirements_rerun_job_count: preview.requirements_rerun_job_count,
            satisfied_job_count: preview.satisfied_job_count,
            restored_job_count: preview.restored_job_count,
            inherited_job_count: preview.inherited_job_count,
            rerun_job_count: preview.rerun_job_count,
            recalculated_job_count: preview.recalculated_job_count,
            dropped_job_ids: preview.dropped_job_ids,
            held_jobs: preview.held_jobs,
            repeated_variants: preview.repeated_variants,
        }
    }
}

impl RevisionPreview {
    fn unchanged() -> Self {
        Self {
            unchanged: true,
            carried_job_count: 0,
            new_job_count: 0,
            requirements_rerun_job_count: 0,
            satisfied_job_count: 0,
            restored_job_count: 0,
            inherited_job_count: 0,
            rerun_job_count: 0,
            recalculated_job_count: 0,
            dropped_job_ids: Vec::new(),
            held_jobs: Vec::new(),
            repeated_variants: Vec::new(),
        }
    }
}

/// The variants of the plan `campaign_id` runs, numbered as a variant edit's
/// `remove` counts them.
pub async fn current_variants(
    stores: &Stores,
    campaign_id: &CampaignId,
) -> Result<(PlanId, String), CampaignAdminError> {
    let campaign = require(&*stores.campaigns, campaign_id).await?;
    let head = Head::read(&*stores.plans, campaign, None).await?;
    let variants = variant_edit::numbered_variants(&head.document(&*stores.plans).await?)?;
    Ok((head.manifest.plan_id, variants))
}

/// Advance onto `plan_id`, filed from `revision` with `jobs`, then cancel the
/// plan it replaced: a failure between leaves both running, never the campaign
/// on a cancelled plan. A campaign that another edit moved, or that changed its
/// owner, does not advance, and `plan_id` is cancelled instead.
pub async fn complete_revision(
    campaigns: &dyn CampaignStore,
    plans: &dyn PlanStore,
    revision: Revision,
    plan_id: &PlanId,
    jobs: &[(String, JobId)],
) -> Result<Revised, CampaignAdminError> {
    let change = revision.change();
    let Revision {
        campaign,
        previous,
        rerun,
        recalculate,
        ..
    } = revision;
    let campaign_id = campaign.campaign_id;

    if advance_from(
        campaigns,
        plans,
        &campaign_id,
        &campaign.owner,
        &previous.plan_id,
        plan_id.clone(),
    )
    .await?
    .is_none()
    {
        let cancelled = plans
            .write_cancel_marker(plan_id)
            .await
            .inspect_err(|e| {
                tracing::error!(
                    %campaign_id,
                    %plan_id,
                    error = %format_args!("{e:#}"),
                    "could not cancel a campaign revision that lost the race to another"
                );
            })
            .is_ok();
        if cancelled {
            tracing::warn!(
                %campaign_id,
                %plan_id,
                revised = %previous.plan_id,
                "filed revision cancelled: its campaign moved off the plan it revised or changed its owner"
            );
        }
        return Err(CampaignAdminError::Overtaken {
            campaign_id,
            plan_id: plan_id.clone(),
            revised: previous.plan_id,
            cancelled,
        });
    }

    let cancel_requested = cancel_superseded(plans, &previous.plan_id)
        .await
        .map_err(|source| CampaignAdminError::NotCancelled {
            campaign_id: campaign_id.clone(),
            plan_id: plan_id.clone(),
            superseded: previous.plan_id.clone(),
            source,
        })?;

    let held: std::collections::HashSet<&JobId> = jobs.iter().map(|(_, job_id)| job_id).collect();
    let carried_job_count = previous
        .job_ids
        .iter()
        .filter(|job_id| held.contains(job_id))
        .count();
    // A re-run or recalculated job is minted afresh, so the revision never
    // holds it.
    let rerun_job_count = rerun.len();
    let recalculated_job_count = recalculate.len();
    let revised = Revised {
        superseded: previous.plan_id,
        carried_job_count,
        rerun_job_count,
        recalculated_job_count,
        dropped_job_count: previous
            .job_ids
            .len()
            .saturating_sub(carried_job_count + rerun_job_count + recalculated_job_count),
        cancel_requested,
    };
    tracing::info!(
        %campaign_id,
        %plan_id,
        superseded = %revised.superseded,
        carried_job_count,
        rerun_job_count,
        rerun_job_ids = ?rerun,
        recalculated_job_count,
        recalculated_job_ids = ?recalculate,
        dropped_job_count = revised.dropped_job_count,
        cancel_requested,
        change = %change,
        "campaign revised"
    );
    Ok(revised)
}

/// Request the cancel of the plan a revision replaced, as `plans cancel` does,
/// and say whether a marker was written. Read fresh: the plan may have ended,
/// or begun cancelling, while the revision was filed.
async fn cancel_superseded(plans: &dyn PlanStore, plan_id: &PlanId) -> anyhow::Result<bool> {
    let Some(manifest) = plans.get_plan(plan_id).await? else {
        return Ok(false);
    };
    Ok(matches!(
        plan_cli::request_cancel(plans, &manifest).await?,
        plan_cli::CancelOutcome::Requested
    ))
}

/// The document the campaign's head was filed from, and that head: what a
/// document revision starts from.
pub async fn head_document(
    stores: &Stores,
    campaign_id: &CampaignId,
) -> Result<(PlanId, String), CampaignAdminError> {
    let campaign = require(&*stores.campaigns, campaign_id).await?;
    let head = Head::read(&*stores.plans, campaign, None).await?;
    let document = head.document(&*stores.plans).await?;
    Ok((head.manifest.plan_id, document))
}

/// The CLI's variant edit: [`Head::revise`], then [`file_cli_revision`]. The
/// route files through its own ingest between the same two calls.
pub async fn edit_variants(
    stores: &Stores,
    settings: IngestSettings<'_>,
    campaign_id: &CampaignId,
    base: &PlanId,
    edit: &VariantEdit,
    note: Option<String>,
    offer: Offer,
) -> Result<(IngestReport, Revised), CampaignAdminError> {
    let campaign = require(&*stores.campaigns, campaign_id).await?;
    let revision = Head::read(&*stores.plans, campaign, Some(base))
        .await?
        .revise(&*stores.plans, edit)
        .await?
        .carrying(offer);
    file_cli_revision(stores, settings, revision, note).await
}

/// The CLI's document revision: [`Head::replace`], then [`file_cli_revision`],
/// as [`edit_variants`] runs an edit. `None` when it files nothing: the
/// document is the head's own, or a retry finds it [`Head::landed`].
pub async fn revise_document(
    stores: &Stores,
    settings: IngestSettings<'_>,
    campaign_id: &CampaignId,
    base: &PlanId,
    document: String,
    note: Option<String>,
    offer: Offer,
) -> Result<Option<(IngestReport, Revised)>, CampaignAdminError> {
    let campaign = require(&*stores.campaigns, campaign_id).await?;
    let head = Head::read(&*stores.plans, campaign, None).await?;
    if let Err(stale) = head.require_base(base) {
        return if head.landed(&*stores.plans, base, &document).await? {
            Ok(None)
        } else {
            Err(stale)
        };
    }
    let revision = head
        .replace(&*stores.plans, document)
        .await?
        .carrying(offer);
    if revision.changes_nothing() {
        return Ok(None);
    }
    file_cli_revision(stores, settings, revision, note)
        .await
        .map(Some)
}

/// The CLI's re-run: [`Head::rerun`], then [`file_cli_revision`], as
/// [`edit_variants`] runs an edit.
pub async fn rerun_jobs(
    stores: &Stores,
    settings: IngestSettings<'_>,
    campaign_id: &CampaignId,
    job_ids: BTreeSet<JobId>,
    note: Option<String>,
) -> Result<(IngestReport, Revised), CampaignAdminError> {
    let campaign = require(&*stores.campaigns, campaign_id).await?;
    let revision = Head::read(&*stores.plans, campaign, None)
        .await?
        .rerun(&*stores.plans, &*stores.submissions, job_ids)
        .await?;
    // The head's document names each imported model as its vault arm, so a
    // re-run imports nothing.
    file_cli_revision(
        stores,
        IngestSettings {
            import: None,
            ..settings
        },
        revision,
        note,
    )
    .await
}

/// The CLI's revert: [`Head::revert`], then [`file_cli_revision`], as
/// [`rerun_jobs`] runs a re-run. `None` when it files nothing: `target`'s
/// document is the head's own.
pub async fn revert_to(
    stores: &Stores,
    settings: IngestSettings<'_>,
    campaign_id: &CampaignId,
    base: &PlanId,
    target: &PlanId,
    note: Option<String>,
) -> Result<Option<(IngestReport, Revised)>, CampaignAdminError> {
    let campaign = require(&*stores.campaigns, campaign_id).await?;
    let revision = Head::read(&*stores.plans, campaign, Some(base))
        .await?
        .revert(&*stores.plans, target)
        .await?;
    if revision.changes_nothing() {
        return Ok(None);
    }
    // A stored document names each imported model as its vault arm, so a
    // revert imports nothing.
    file_cli_revision(
        stores,
        IngestSettings {
            import: None,
            ..settings
        },
        revision,
        note,
    )
    .await
    .map(Some)
}

/// Ingest `revision`'s document carrying the head's jobs over, then
/// [`complete_revision`].
async fn file_cli_revision(
    stores: &Stores,
    settings: IngestSettings<'_>,
    revision: Revision,
    note: Option<String>,
) -> Result<(IngestReport, Revised), CampaignAdminError> {
    let catalog = crate::org_catalog::load_server_catalog(&*stores.catalog).await?;
    let ctx = IngestContext::with_settings(stores, &catalog, settings);
    let change = Some(revision.change());
    let membership = revision.membership(&*stores.plans).await?;
    let report = plan_cli::ingest(
        &ctx,
        PlanInput::Text(revision.document()),
        note,
        change,
        membership,
    )
    .await?;
    let revised = complete_revision(
        &*stores.campaigns,
        &*stores.plans,
        revision,
        &report.plan_id,
        &report.job_ids,
    )
    .await?;
    Ok((report, revised))
}

/// Check a re-run's jobs as [`rerun_jobs`] does and expand the head's
/// document, and write nothing. The head's document holds no token, so the
/// dry run resolves no import.
pub async fn dry_run_rerun(
    stores: &Stores,
    settings: IngestSettings<'_>,
    campaign_id: &CampaignId,
    job_ids: BTreeSet<JobId>,
    note: Option<String>,
) -> Result<RevisionDryRun, CampaignAdminError> {
    let campaign = require(&*stores.campaigns, campaign_id).await?;
    let revision = Head::read(&*stores.plans, campaign, None)
        .await?
        .rerun(&*stores.plans, &*stores.submissions, job_ids)
        .await?;
    dry_run_revision(
        stores,
        IngestSettings {
            import: None,
            ..settings
        },
        revision,
        note,
    )
    .await
}

/// Check a revert as [`revert_to`] does and expand `target`'s document, and
/// write nothing.
pub async fn dry_run_revert(
    stores: &Stores,
    settings: IngestSettings<'_>,
    campaign_id: &CampaignId,
    base: &PlanId,
    target: &PlanId,
    note: Option<String>,
) -> Result<RevisionDryRun, CampaignAdminError> {
    let campaign = require(&*stores.campaigns, campaign_id).await?;
    let revision = Head::read(&*stores.plans, campaign, Some(base))
        .await?
        .revert(&*stores.plans, target)
        .await?;
    // A revert imports nothing, as in `revert_to`.
    dry_run_revision(
        stores,
        IngestSettings {
            import: None,
            ..settings
        },
        revision,
        note,
    )
    .await
}

/// Apply a variant edit and expand the result, and write nothing.
pub async fn dry_run_variants(
    stores: &Stores,
    settings: IngestSettings<'_>,
    campaign_id: &CampaignId,
    base: &PlanId,
    edit: &VariantEdit,
    note: Option<String>,
    offer: Offer,
) -> Result<RevisionDryRun, CampaignAdminError> {
    let campaign = require(&*stores.campaigns, campaign_id).await?;
    let revision = Head::read(&*stores.plans, campaign, Some(base))
        .await?
        .revise(&*stores.plans, edit)
        .await?
        .carrying(offer);
    dry_run_revision(stores, settings, revision, note).await
}

/// Expand `document` as a revision of the campaign's head, and write nothing.
pub async fn dry_run_revise_document(
    stores: &Stores,
    settings: IngestSettings<'_>,
    campaign_id: &CampaignId,
    base: &PlanId,
    document: String,
    note: Option<String>,
    offer: Offer,
) -> Result<RevisionDryRun, CampaignAdminError> {
    let campaign = require(&*stores.campaigns, campaign_id).await?;
    let revision = Head::read(&*stores.plans, campaign, Some(base))
        .await?
        .replace(&*stores.plans, document)
        .await?
        .carrying(offer);
    dry_run_revision(stores, settings, revision, note).await
}

/// Expand `revision`'s document and run every check filing it would, and
/// write nothing. Token models resolve for the owner, so the preview counts a
/// model the owner holds as its vault arm, as a real filing stages it.
async fn dry_run_revision(
    stores: &Stores,
    settings: IngestSettings<'_>,
    revision: Revision,
    note: Option<String>,
) -> Result<RevisionDryRun, CampaignAdminError> {
    let catalog = crate::org_catalog::load_server_catalog(&*stores.catalog).await?;
    let ctx = IngestContext::with_settings(stores, &catalog, settings);
    if revision.changes_nothing() {
        let expanded = plan_cli::expand_text(&ctx, revision.document(), note).await?;
        return Ok(RevisionDryRun::unchanged(
            revision.document(),
            &expanded.jobs,
            expanded.plan_warnings,
        )
        .map_err(anyhow::Error::new)?);
    }
    let membership = revision.membership(&*stores.plans).await?;
    let DocumentDryRun {
        report,
        preview_bodies,
    } = plan_cli::dry_run_text(&ctx, &membership.owner, revision.document(), note).await?;
    let preview = revision
        .preview(
            membership,
            &preview_bodies,
            &*stores.plans,
            &*stores.submissions,
            &*stores.orgs,
        )
        .await
        .map_err(anyhow::Error::new)?;
    Ok(RevisionDryRun::new(revision.document(), report, preview).map_err(anyhow::Error::new)?)
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroUsize;
    use std::time::Duration;

    use anyhow::Context;

    use pipette_core::organization::Organization;
    use pipette_core::user::User;
    use pipette_plan_types::{ContactEmail, ExpiresAt, NonEmptyTrimmedString, OrgSlug};
    use pipette_store::{ContentKeyCas, ContentKeyClaim, ContentKeyRecord, ContentKeyVersion};
    use pipette_store_fs::build_local_fs_stores;

    use super::*;
    use crate::campaign_chain::CHAIN_MAX;
    use crate::hf_import::HfImports;
    use crate::plan_ingestion::{IngestError, ModelCheck, RejectionError};

    /// Which arm of [`owner`] a case expects. Named so a failure reports the
    /// arm rather than a boolean pair.
    #[derive(Debug, Clone, Copy)]
    enum Expect {
        User,
        Org,
        NoSuchUser,
        NoSuchOrg,
    }

    fn stores() -> anyhow::Result<(tempfile::TempDir, Stores)> {
        let dir = tempfile::tempdir()?;
        let config = pipette_core::config::Config {
            storage: pipette_core::config::StorageConfig::local_fs(dir.path().to_path_buf()),
            auth_storage: pipette_core::config::StorageConfig::local_fs(dir.path().to_path_buf()),
            ..Default::default()
        };
        Ok((dir, build_local_fs_stores(&config)?))
    }

    fn a_new_campaign(name: &str) -> anyhow::Result<NewCampaign> {
        Ok(NewCampaign::new(
            NonEmptyTrimmedString::try_new(name)?,
            CampaignOwner::User {
                user_id: UserId::try_new("user_nightly")?,
            },
        ))
    }

    /// A campaign created around `plan-seed`, the plan the first advance in
    /// each test supersedes.
    async fn seed(stores: &Stores, name: &str) -> anyhow::Result<Campaign> {
        let campaign = a_new_campaign(name)?.around(PlanId::try_new("plan-seed")?, Utc::now());
        stores.campaigns.put_campaign(&campaign).await?;
        Ok(campaign)
    }

    /// A plan store whose cancel markers cannot be written; everything else
    /// passes through.
    struct CancelFails(std::sync::Arc<dyn PlanStore>);

    #[async_trait::async_trait]
    impl PlanStore for CancelFails {
        async fn write_cancel_marker(&self, _plan_id: &PlanId) -> anyhow::Result<()> {
            anyhow::bail!("simulated cancel marker write failure")
        }
        async fn put_plan(&self, manifest: &PlanManifest) -> anyhow::Result<()> {
            self.0.put_plan(manifest).await
        }
        async fn get_plan(&self, plan_id: &PlanId) -> anyhow::Result<Option<PlanManifest>> {
            self.0.get_plan(plan_id).await
        }
        async fn list_matching_plans(
            &self,
            filter: &pipette_store::PlanFilter,
            after: Option<&PlanId>,
            limit: NonZeroUsize,
        ) -> anyhow::Result<pipette_store::PlanPage> {
            self.0.list_matching_plans(filter, after, limit).await
        }
        async fn has_cancel_marker(&self, plan_id: &PlanId) -> anyhow::Result<bool> {
            self.0.has_cancel_marker(plan_id).await
        }
        async fn list_cancel_markers(&self) -> anyhow::Result<Vec<PlanId>> {
            self.0.list_cancel_markers().await
        }
        async fn delete_cancel_marker(&self, plan_id: &PlanId) -> anyhow::Result<()> {
            self.0.delete_cancel_marker(plan_id).await
        }
        async fn get_content_key(
            &self,
            digest: &str,
        ) -> anyhow::Result<Option<(ContentKeyRecord, ContentKeyVersion)>> {
            self.0.get_content_key(digest).await
        }
        async fn create_content_key(
            &self,
            digest: &str,
            record: &ContentKeyRecord,
        ) -> anyhow::Result<ContentKeyClaim> {
            self.0.create_content_key(digest, record).await
        }
        async fn replace_content_key(
            &self,
            digest: &str,
            version: &ContentKeyVersion,
            record: &ContentKeyRecord,
        ) -> anyhow::Result<ContentKeyCas> {
            self.0.replace_content_key(digest, version, record).await
        }
        async fn delete_content_key(
            &self,
            digest: &str,
            version: &ContentKeyVersion,
        ) -> anyhow::Result<ContentKeyCas> {
            self.0.delete_content_key(digest, version).await
        }
        async fn list_content_keys(
            &self,
        ) -> anyhow::Result<Vec<(String, ContentKeyRecord, ContentKeyVersion)>> {
            self.0.list_content_keys().await
        }
        async fn write_job_plan_marker(
            &self,
            job_id: &JobId,
            plan_id: &PlanId,
            expires_at: ExpiresAt,
        ) -> anyhow::Result<()> {
            self.0
                .write_job_plan_marker(job_id, plan_id, expires_at)
                .await
        }
        async fn list_job_plan_markers(
            &self,
            job_id: &JobId,
        ) -> anyhow::Result<Vec<(PlanId, ExpiresAt)>> {
            self.0.list_job_plan_markers(job_id).await
        }
        async fn list_all_job_plan_markers(
            &self,
        ) -> anyhow::Result<Vec<pipette_store::JobPlanMarker>> {
            self.0.list_all_job_plan_markers().await
        }
        async fn delete_job_plan_marker(
            &self,
            job_id: &JobId,
            plan_id: &PlanId,
            expires_at: ExpiresAt,
        ) -> anyhow::Result<()> {
            self.0
                .delete_job_plan_marker(job_id, plan_id, expires_at)
                .await
        }
        async fn put_plan_source(
            &self,
            plan_id: &PlanId,
            source: &pipette_core::plan::PlanSource,
        ) -> anyhow::Result<pipette_store::CreateOutcome> {
            self.0.put_plan_source(plan_id, source).await
        }
        async fn get_plan_source(
            &self,
            plan_id: &PlanId,
        ) -> anyhow::Result<Option<pipette_core::plan::PlanSource>> {
            self.0.get_plan_source(plan_id).await
        }
        async fn delete_plan_source(&self, plan_id: &PlanId) -> anyhow::Result<()> {
            self.0.delete_plan_source(plan_id).await
        }
        async fn write_pending_source_marker(&self, plan_id: &PlanId) -> anyhow::Result<()> {
            self.0.write_pending_source_marker(plan_id).await
        }
        async fn list_pending_source_markers(
            &self,
        ) -> anyhow::Result<Vec<pipette_store::PendingSourceMarker>> {
            self.0.list_pending_source_markers().await
        }
        async fn delete_pending_source_marker(&self, plan_id: &PlanId) -> anyhow::Result<()> {
            self.0.delete_pending_source_marker(plan_id).await
        }
        async fn put_job_source(
            &self,
            job_id: &JobId,
            body: &serde_json::Value,
        ) -> anyhow::Result<pipette_store::CreateOutcome> {
            self.0.put_job_source(job_id, body).await
        }
        async fn get_job_source(
            &self,
            job_id: &JobId,
        ) -> anyhow::Result<Option<serde_json::Value>> {
            self.0.get_job_source(job_id).await
        }
        async fn put_job_plan_link(
            &self,
            job_id: &JobId,
            plan_id: &PlanId,
        ) -> anyhow::Result<pipette_store::CreateOutcome> {
            self.0.put_job_plan_link(job_id, plan_id).await
        }
        async fn list_job_plan_links(&self, job_id: &JobId) -> anyhow::Result<Vec<PlanId>> {
            self.0.list_job_plan_links(job_id).await
        }
    }

    /// A campaign store whose writes fail; reads and deletes pass through.
    struct PutFails(std::sync::Arc<dyn CampaignStore>);

    #[async_trait::async_trait]
    impl CampaignStore for PutFails {
        async fn get_campaign(&self, campaign_id: &CampaignId) -> anyhow::Result<Option<Campaign>> {
            self.0.get_campaign(campaign_id).await
        }
        async fn put_campaign(&self, _campaign: &Campaign) -> anyhow::Result<()> {
            anyhow::bail!("down")
        }
        async fn list_campaigns(
            &self,
            after: Option<&CampaignId>,
            limit: NonZeroUsize,
        ) -> anyhow::Result<pipette_store::CampaignPage> {
            self.0.list_campaigns(after, limit).await
        }
        async fn delete_campaign(&self, campaign_id: &CampaignId) -> anyhow::Result<()> {
            self.0.delete_campaign(campaign_id).await
        }
    }

    #[tokio::test]
    async fn advance_keeps_a_rename_that_lands_during_the_ingest() -> anyhow::Result<()> {
        let (_dir, stores) = stores()?;
        let campaign = seed(&stores, "nightly").await?;
        let campaign_id = campaign.campaign_id.clone();

        // The caller now holds `campaign`, fetched before the ingest. Someone
        // renames the campaign while those jobs are staged.
        let mut renamed = campaign.clone();
        assert!(renamed.rename(NonEmptyTrimmedString::try_new("nightly-eu")?, Utc::now()));
        stores.campaigns.put_campaign(&renamed).await?;

        let plan_id = PlanId::try_new("plan-first")?;
        let advanced = advance(
            &*stores.campaigns,
            &*stores.plans,
            &campaign_id,
            plan_id.clone(),
        )
        .await?;
        assert!(advanced.changed);
        assert_eq!(advanced.superseded, campaign.current_plan_id);

        let stored = require(&*stores.campaigns, &campaign_id).await?;
        assert_eq!(stored.current_plan_id, Some(plan_id));
        assert_eq!(stored.name.as_str(), "nightly-eu");
        Ok(())
    }

    /// The displaced plan, which both surfaces render, is left alone.
    #[tokio::test]
    async fn advance_reports_the_plan_it_supersedes() -> anyhow::Result<()> {
        let (_dir, stores) = stores()?;
        let campaign_id = seed(&stores, "nightly").await?.campaign_id;
        let first = PlanId::try_new("plan-first")?;
        let second = PlanId::try_new("plan-second")?;

        advance(
            &*stores.campaigns,
            &*stores.plans,
            &campaign_id,
            first.clone(),
        )
        .await?;
        let advanced = advance(
            &*stores.campaigns,
            &*stores.plans,
            &campaign_id,
            second.clone(),
        )
        .await?;

        assert!(advanced.changed);
        assert_eq!(advanced.superseded, Some(first));
        let stored = require(&*stores.campaigns, &campaign_id).await?;
        assert_eq!(stored.current_plan_id, Some(second));
        Ok(())
    }

    #[tokio::test]
    async fn advance_onto_the_current_plan_is_a_no_op() -> anyhow::Result<()> {
        let (_dir, stores) = stores()?;
        let campaign_id = seed(&stores, "nightly").await?.campaign_id;
        let plan_id = PlanId::try_new("plan-first")?;

        advance(
            &*stores.campaigns,
            &*stores.plans,
            &campaign_id,
            plan_id.clone(),
        )
        .await?;
        let before = require(&*stores.campaigns, &campaign_id).await?;
        let advanced = advance(
            &*stores.campaigns,
            &*stores.plans,
            &campaign_id,
            plan_id.clone(),
        )
        .await?;

        assert!(!advanced.changed);
        assert_eq!(advanced.superseded, Some(plan_id));
        let after = require(&*stores.campaigns, &campaign_id).await?;
        assert_eq!(after.updated_at, before.updated_at);
        Ok(())
    }

    /// Every arm of [`owner`], including the two the CLI's `conflicts_with`
    /// keeps it from reaching.
    #[rstest::rstest]
    // (name, user, org, expected)
    #[case::a_user_the_roster_holds("known", false, Expect::User)]
    #[case::a_user_the_roster_does_not_hold("user_missing", false, Expect::NoSuchUser)]
    #[case::an_organization_the_roster_holds("", true, Expect::Org)]
    #[case::an_organization_the_roster_does_not_hold("", false, Expect::NoSuchOrg)]
    #[tokio::test]
    async fn owner_answers_each_way_a_pair_can_name_one(
        #[case] user: &str,
        #[case] org_exists: bool,
        #[case] expected: Expect,
    ) -> anyhow::Result<()> {
        let (_dir, stores) = stores()?;
        let known = User::try_new(ContactEmail::try_new("op@example.test")?, Utc::now())?;
        stores.users.put_user(&known).await?;
        let org = Organization::new(
            OrgSlug::try_new("acme")?,
            NonEmptyTrimmedString::try_new("Acme")?,
            Utc::now(),
        );
        if org_exists {
            stores
                .orgs
                .claim_org_slug(org.slug.as_str(), &org.org_id)
                .await?;
            stores.orgs.put_org(&org).await?;
        }

        let (user_arg, org_arg) = match expected {
            Expect::User => (Some(known.user_id.clone()), None),
            Expect::NoSuchUser => (Some(UserId::try_new(user)?), None),
            Expect::Org | Expect::NoSuchOrg => {
                (None, Some(OrgRef::Slug(OrgSlug::try_new("acme")?)))
            }
        };
        let outcome = owner(&stores, user_arg, org_arg).await;

        match expected {
            Expect::User => assert_eq!(
                outcome.ok(),
                Some(CampaignOwner::User {
                    user_id: known.user_id.clone()
                })
            ),
            Expect::Org => assert_eq!(
                outcome.ok(),
                Some(CampaignOwner::Org {
                    org_id: org.org_id.clone()
                })
            ),
            Expect::NoSuchUser => {
                assert!(matches!(outcome, Err(CampaignAdminError::NoSuchUser(_))))
            }
            Expect::NoSuchOrg => assert!(matches!(outcome, Err(CampaignAdminError::NoSuchOrg(_)))),
        }
        Ok(())
    }

    #[rstest::rstest]
    #[case::neither(false)]
    #[case::both(true)]
    #[tokio::test]
    async fn owner_refuses_a_pair_that_names_no_single_owner(
        #[case] both: bool,
    ) -> anyhow::Result<()> {
        let (_dir, stores) = stores()?;
        let (user, org) = if both {
            (
                Some(UserId::try_new("user_known")?),
                Some(OrgRef::Slug(OrgSlug::try_new("acme")?)),
            )
        } else {
            (None, None)
        };

        let outcome = owner(&stores, user, org).await;

        assert!(matches!(outcome, Err(CampaignAdminError::NoOwnerNamed)));
        Ok(())
    }

    #[rstest::rstest]
    #[case::creating(PlanStatus::Creating, false, true)]
    #[case::pending_clients(PlanStatus::PendingClients, false, true)]
    #[case::active(PlanStatus::Active, false, true)]
    #[case::cancel_requested(PlanStatus::Active, true, false)]
    #[case::cancelling(PlanStatus::Cancelling, false, false)]
    #[case::cancelled(PlanStatus::Cancelled, false, false)]
    #[case::complete(PlanStatus::Complete, false, false)]
    #[tokio::test]
    async fn delete_waits_for_the_campaigns_plans(
        #[case] status: PlanStatus,
        #[case] cancel_requested: bool,
        #[case] refused: bool,
    ) -> anyhow::Result<()> {
        let (_dir, stores) = stores()?;
        let campaign = seed(&stores, "nightly").await?;
        let plan_id = PlanId::try_new("plan-seed")?;
        stores
            .plans
            .put_plan(&PlanManifest {
                campaign_id: Some(campaign.campaign_id.clone()),
                ..pipette_core::plan::sample_manifest(plan_id.clone(), status, false)?
            })
            .await?;
        // Another campaign's running plan does not hold this one's delete.
        stores
            .plans
            .put_plan(&pipette_core::plan::sample_manifest(
                PlanId::try_new("plan-other")?,
                PlanStatus::Active,
                false,
            )?)
            .await?;
        if cancel_requested {
            stores.plans.write_cancel_marker(&plan_id).await?;
        }

        let outcome = delete(&*stores.campaigns, &*stores.plans, &campaign).await;

        let kept = stores.campaigns.get_campaign(&campaign.campaign_id).await?;
        if refused {
            let Err(CampaignAdminError::StillRunning { plans, .. }) = outcome else {
                anyhow::bail!("a running plan did not hold the delete: {outcome:?}");
            };
            assert_eq!(plans, vec![plan_id]);
            assert!(kept.is_some(), "a refused delete deletes nothing");
        } else {
            outcome?;
            assert!(kept.is_none());
            assert!(
                stores.plans.get_plan(&plan_id).await?.is_some(),
                "the plan outlives its campaign"
            );
        }
        Ok(())
    }

    /// The new head has no manifest, which is what a missed plan looks like to
    /// the scan.
    #[tokio::test]
    async fn delete_refuses_when_a_filing_advances_during_the_scan() -> anyhow::Result<()> {
        let (_dir, stores) = stores()?;
        let campaign = seed(&stores, "nightly").await?;
        let filed = PlanId::try_new("plan-filed")?;
        stores
            .campaigns
            .put_campaign(&Campaign {
                current_plan_id: Some(filed.clone()),
                labels: Default::default(),
                ..campaign.clone()
            })
            .await?;

        let outcome = delete(&*stores.campaigns, &*stores.plans, &campaign).await;

        let Err(CampaignAdminError::StillRunning { plans, .. }) = outcome else {
            anyhow::bail!("a moved head did not hold the delete: {outcome:?}");
        };
        assert_eq!(plans, vec![filed]);
        assert!(
            stores
                .campaigns
                .get_campaign(&campaign.campaign_id)
                .await?
                .is_some()
        );
        Ok(())
    }

    #[tokio::test]
    async fn delete_skips_an_unreadable_manifest() -> anyhow::Result<()> {
        let (dir, stores) = stores()?;
        let campaign = seed(&stores, "nightly").await?;
        std::fs::write(dir.path().join("plans/plan-broken.json"), b"{")?;
        let listing = pipette_store::drain_plans(&*stores.plans, None).await?;
        assert_eq!(
            listing.unreadable.len(),
            1,
            "the manifest lists as unreadable"
        );

        delete(&*stores.campaigns, &*stores.plans, &campaign).await?;

        assert!(
            stores
                .campaigns
                .get_campaign(&campaign.campaign_id)
                .await?
                .is_none()
        );
        Ok(())
    }

    /// A delete during the ingest: the advance cancels the plan the scan can
    /// miss, and says whether the cancel landed.
    #[rstest::rstest]
    #[case::cancelled(true)]
    #[case::cancel_fails(false)]
    #[tokio::test]
    async fn advance_cancels_the_plan_when_the_campaign_goes_during_the_ingest(
        #[case] cancel_works: bool,
    ) -> anyhow::Result<()> {
        let (_dir, stores) = stores()?;
        let campaign_id = seed(&stores, "nightly").await?.campaign_id;
        let plan_id = PlanId::try_new("plan-first")?;
        let failing = CancelFails(stores.plans.clone());
        let plans: &dyn PlanStore = if cancel_works {
            &*stores.plans
        } else {
            &failing
        };

        // The caller has staged its jobs. The campaign goes before it advances.
        stores.campaigns.delete_campaign(&campaign_id).await?;

        let outcome = advance(&*stores.campaigns, plans, &campaign_id, plan_id.clone()).await;

        let Err(CampaignAdminError::DeletedDuringFiling {
            plan_id: named_plan,
            cancelled,
            ..
        }) = outcome
        else {
            anyhow::bail!("a deleted campaign did not report the staged plan: {outcome:?}");
        };
        assert_eq!(named_plan, plan_id);
        assert_eq!(cancelled, cancel_works);
        assert_eq!(
            stores.plans.has_cancel_marker(&plan_id).await?,
            cancel_works
        );
        Ok(())
    }

    /// The example plan document that `campaigns ingest --plan` files, with the
    /// two benchmarks it names written into the store's catalog.
    fn afm_mlx_plan(store_dir: &std::path::Path) -> anyhow::Result<std::path::PathBuf> {
        let benchmarks = store_dir.join("benchmarks");
        std::fs::create_dir_all(&benchmarks)?;
        std::fs::write(
            benchmarks.join("decode_throughput_512_100.toml"),
            "benchmark_type = \"decode_throughput\"\nparameter_prefill_tokens = 512\n\
             parameter_decode_tokens = 100",
        )?;
        std::fs::write(
            benchmarks.join("end_to_end_latency_512_256.toml"),
            "benchmark_type = \"end_to_end_latency\"\nparameter_prefill_tokens = 512\n\
             parameter_decode_tokens = 256",
        )?;
        let path = store_dir.join("afm-mlx.toml");
        std::fs::write(
            &path,
            include_str!("../../../examples/plans/scheduler/afm-mlx.toml"),
        )?;
        Ok(path)
    }

    const MAX_JOBS: std::num::NonZeroUsize = match std::num::NonZeroUsize::new(5000) {
        Some(n) => n,
        None => std::num::NonZeroUsize::MIN,
    };
    const CREATING_MAX_AGE: std::time::Duration = std::time::Duration::from_secs(21_600);

    /// The settings of a test ingest under `models`: the limits above, and no
    /// import.
    fn settings(models: ModelCheck<'_>) -> IngestSettings<'_> {
        IngestSettings {
            models,
            max_jobs: MAX_JOBS,
            creating_max_age: CREATING_MAX_AGE,
            import: None,
        }
    }

    /// `campaigns create --plan` files the document as the campaign's first
    /// plan, under the id the campaign is then written with.
    #[tokio::test]
    async fn create_files_the_first_plan_then_the_campaign() -> anyhow::Result<()> {
        let (dir, stores) = stores()?;
        let path = afm_mlx_plan(dir.path())?;
        let new = a_new_campaign("nightly")?;
        let campaign_id = new.campaign_id().clone();

        let (campaign, report) = create(
            &stores,
            settings(ModelCheck::none()),
            new,
            &path,
            Some("nightly-1".to_string()),
        )
        .await?;
        assert_eq!(report.job_count, 3);
        assert_eq!(campaign.campaign_id, campaign_id);
        assert_eq!(campaign.current_plan_id, Some(report.plan_id.clone()));
        assert_eq!(require(&*stores.campaigns, &campaign_id).await?, campaign);

        let manifest = stores
            .plans
            .get_plan(&report.plan_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("the manifest exists"))?;
        assert_eq!(manifest.campaign_id, Some(campaign_id));
        assert_eq!(manifest.previous_plan_id, None);
        assert!(manifest.source_sha256.is_some());
        Ok(())
    }

    /// A campaign write that fails after the first plan is filed names that
    /// plan and cancels it: it is staged under a campaign id nothing holds.
    #[tokio::test]
    async fn create_cancels_the_plan_when_the_campaign_write_fails() -> anyhow::Result<()> {
        let (dir, mut stores) = stores()?;
        stores.campaigns = std::sync::Arc::new(PutFails(stores.campaigns.clone()));
        let path = afm_mlx_plan(dir.path())?;
        let new = a_new_campaign("nightly")?;
        let campaign_id = new.campaign_id().clone();

        let outcome = create(&stores, settings(ModelCheck::none()), new, &path, None).await;

        let Err(CampaignAdminError::NotCreated {
            campaign_id: named_campaign,
            plan_id,
            cancelled,
            ..
        }) = outcome
        else {
            anyhow::bail!("a failed campaign write did not report the staged plan");
        };
        assert_eq!(named_campaign, campaign_id);
        let manifest = stores
            .plans
            .get_plan(&plan_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("the plan was filed"))?;
        assert_eq!(manifest.campaign_id, Some(campaign_id.clone()));
        assert_eq!(stores.campaigns.get_campaign(&campaign_id).await?, None);
        assert!(cancelled);
        assert!(stores.plans.has_cancel_marker(&plan_id).await?);
        Ok(())
    }

    #[tokio::test]
    async fn create_with_strict_models_refuses_an_unknown_model() -> anyhow::Result<()> {
        let (dir, stores) = stores()?;
        let path = afm_mlx_plan(dir.path())?;
        // Non-empty, or every model resolves: the table names none of the plan's.
        let table = crate::model_params::ModelParamTable::from_toml(r#""LFM2-700M" = 742"#)?;

        let outcome = create(
            &stores,
            settings(ModelCheck::strict(&table)),
            a_new_campaign("nightly")?,
            &path,
            None,
        )
        .await;

        let Err(CampaignAdminError::Store(e)) = outcome else {
            anyhow::bail!("an unknown model under --strict-models was not refused");
        };
        assert!(format!("{e:#}").contains("model"), "{e:#}");
        assert!(
            pipette_store::drain_campaigns(&*stores.campaigns)
                .await?
                .items
                .is_empty()
        );
        assert!(stores.todo.list_avail().await?.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn dry_run_document_writes_nothing() -> anyhow::Result<()> {
        let (dir, stores) = stores()?;
        let path = afm_mlx_plan(dir.path())?;

        let report = dry_run_document(
            &stores,
            settings(ModelCheck::none()),
            a_new_campaign("Dry")?.owner(),
            &path,
            None,
        )
        .await?;
        assert_eq!(report.bodies.len(), 3);
        assert!(
            pipette_store::drain_campaigns(&*stores.campaigns)
                .await?
                .items
                .is_empty()
        );
        assert!(stores.todo.list_avail().await?.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn ingest_into_files_a_document_and_advances() -> anyhow::Result<()> {
        let (dir, stores) = stores()?;
        let campaign_id = seed(&stores, "nightly").await?.campaign_id;
        let path = afm_mlx_plan(dir.path())?;

        let (report, advanced) = ingest_into(
            &stores,
            settings(ModelCheck::none()),
            &campaign_id,
            PlanInput::Document(&path),
            Some("nightly-2026.09.24".to_string()),
        )
        .await?;
        assert_eq!(report.job_count, 3);
        assert!(advanced.changed);

        let stored = require(&*stores.campaigns, &campaign_id).await?;
        assert_eq!(stored.current_plan_id, Some(report.plan_id.clone()));
        let manifest = stores
            .plans
            .get_plan(&report.plan_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("the manifest exists"))?;
        assert_eq!(manifest.campaign_id, Some(campaign_id));
        assert!(manifest.source_sha256.is_some());
        pipette_store_testkit::assert_each_job_has_its_source_and_link(
            stores.plans.as_ref(),
            stores.todo.as_ref(),
            &report.plan_id,
            &manifest.job_ids,
        )
        .await?;
        Ok(())
    }

    /// A mock Hub that holds one gated GGUF repository, and an import client
    /// for it.
    async fn gated_imports() -> anyhow::Result<(pipette_hub_testkit::MockHub, HfImports)> {
        use pipette_hub_testkit::{MockFile, MockGate, MockHub, MockRepo};

        let hub = MockHub::start(HashMap::from([(
            "LiquidAI/LFM2-700M-GGUF".to_string(),
            MockRepo {
                commit: "a".repeat(40),
                public: false,
                rate_limited: false,
                gate: MockGate::Open,
                files: vec![MockFile::lfs("m.gguf", b"GGUF weights")],
            },
        )]))
        .await?;
        let imports = HfImports::new(
            hub.endpoint.as_str(),
            NonZeroUsize::MIN,
            NonZeroUsize::MIN.saturating_add(1),
            std::num::NonZeroU64::MAX,
        )?;
        Ok((hub, imports))
    }

    /// A plan document of one macOS cell, whose gguf model on the mock Hub of
    /// [`gated_imports`] carries `token`.
    fn gated_plan(token: &str) -> String {
        format!(
            r#"benchmarks = ["decode_throughput_512_100"]

[[variants]]
requires = ["os:macos"]
models   = [{{ type = "gguf_text", source = "huggingface", org = "LiquidAI", repo_name = "LFM2-700M-GGUF", path = "m.gguf", auth_token = "{token}" }}]
runtimes = [{{ type = "llamacpp_cli_stock_tools", source = "github_release", repository_url = "github.com/ggml-org/llama.cpp", version = "b9050", flavor = "macos-arm64" }}]
"#
        )
    }

    /// A campaign created from `document`, under `models`. Its stores and mock
    /// Hub live as long as the value does.
    struct GatedCampaign {
        _dir: tempfile::TempDir,
        stores: Stores,
        hub: pipette_hub_testkit::MockHub,
        imports: HfImports,
        campaign: Campaign,
        first: IngestReport,
    }

    async fn gated_campaign(
        models: ModelCheck<'_>,
        document: &str,
    ) -> anyhow::Result<GatedCampaign> {
        let (dir, stores) = stores()?;
        afm_mlx_plan(dir.path())?;
        let (hub, imports) = gated_imports().await?;
        let plans = tempfile::tempdir()?;
        let path = plans.path().join("gated.toml");
        std::fs::write(&path, document)?;
        let (campaign, first) = create(
            &stores,
            IngestSettings {
                import: Some(&imports),
                ..settings(models)
            },
            a_new_campaign("gated")?,
            &path,
            None,
        )
        .await?;
        Ok(GatedCampaign {
            _dir: dir,
            stores,
            hub,
            imports,
            campaign,
            first,
        })
    }

    /// `campaigns revise --dry-run` counts the head's gated job as carried,
    /// reusing the model's records, and downloads nothing.
    #[tokio::test]
    async fn a_gated_revision_dry_run_carries_the_gated_job() -> anyhow::Result<()> {
        use pipette_hub_testkit::GOOD_TOKEN;

        let GatedCampaign {
            _dir,
            stores,
            hub,
            imports,
            campaign,
            first,
        } = gated_campaign(ModelCheck::none(), &gated_plan(GOOD_TOKEN)).await?;
        let downloads = hub.downloads();

        let dry_run = dry_run_revise_document(
            &stores,
            IngestSettings {
                import: Some(&imports),
                ..settings(ModelCheck::none())
            },
            &campaign.campaign_id,
            &first.plan_id,
            format!("{}{MLX_700M_VARIANT}", gated_plan(GOOD_TOKEN)),
            None,
            Offer::default(),
        )
        .await?;

        assert_eq!(dry_run.preview.carried_job_count, 1, "{dry_run:?}");
        assert_eq!(dry_run.preview.new_job_count, 1, "{dry_run:?}");
        assert!(dry_run.preview.dropped_job_ids.is_empty(), "{dry_run:?}");
        assert!(
            matches!(
                dry_run.report.imports.as_slice(),
                [pipette_plan_types::PlannedImport {
                    action: pipette_plan_types::ImportAction::Reuse { .. },
                    ..
                }]
            ),
            "{:?}",
            dry_run.report.imports
        );
        assert_eq!(hub.downloads(), downloads);
        Ok(())
    }

    /// Skips the import, as a real revision does, so needs no Hub endpoint.
    #[tokio::test]
    async fn an_unchanged_gated_revision_dry_run_needs_no_import() -> anyhow::Result<()> {
        use pipette_hub_testkit::GOOD_TOKEN;

        let GatedCampaign {
            _dir,
            stores,
            hub: _hub,
            imports: _,
            campaign,
            first,
        } = gated_campaign(ModelCheck::none(), &gated_plan(GOOD_TOKEN)).await?;

        let dry_run = dry_run_revise_document(
            &stores,
            settings(ModelCheck::none()),
            &campaign.campaign_id,
            &first.plan_id,
            gated_plan(GOOD_TOKEN),
            None,
            Offer::default(),
        )
        .await?;

        assert!(dry_run.preview.unchanged, "{dry_run:?}");
        assert!(dry_run.report.imports.is_empty(), "{dry_run:?}");
        assert_eq!(dry_run.report.bodies.len(), 1, "{dry_run:?}");
        Ok(())
    }

    /// Giving the head's tokenless model a token imports a new vault arm, so the
    /// dry run counts the head's job stopped and a new one queued.
    #[tokio::test]
    async fn a_revision_dry_run_that_gates_a_model_counts_as_filing_does() -> anyhow::Result<()> {
        use pipette_hub_testkit::GOOD_TOKEN;

        let gated = gated_plan(GOOD_TOKEN);
        let GatedCampaign {
            _dir,
            stores,
            hub: _hub,
            imports,
            campaign,
            first,
        } = gated_campaign(
            ModelCheck::none(),
            &gated.replace(&format!(r#", auth_token = "{GOOD_TOKEN}""#), ""),
        )
        .await?;

        let dry_run = dry_run_revise_document(
            &stores,
            IngestSettings {
                import: Some(&imports),
                ..settings(ModelCheck::none())
            },
            &campaign.campaign_id,
            &first.plan_id,
            gated.clone(),
            None,
            Offer::default(),
        )
        .await?;
        let (report, revised) = revise_document(
            &stores,
            IngestSettings {
                import: Some(&imports),
                ..settings(ModelCheck::none())
            },
            &campaign.campaign_id,
            &first.plan_id,
            gated,
            None,
            Offer::default(),
        )
        .await?
        .context("a changed document files a revision")?;

        assert_eq!(
            (
                dry_run.preview.carried_job_count,
                dry_run.preview.new_job_count,
                dry_run.preview.dropped_job_ids.len(),
            ),
            (0, 1, 1),
            "{dry_run:?}"
        );
        assert_eq!(
            (
                revised.carried_job_count,
                report.job_ids.len() - revised.carried_job_count,
                revised.dropped_job_count,
            ),
            (0, 1, 1),
            "{revised:?}"
        );
        Ok(())
    }

    /// The job source names the vault copy, with no token.
    #[tokio::test]
    async fn ingest_into_imports_a_gated_model_for_the_campaign_owner() -> anyhow::Result<()> {
        use pipette_core::vault::VaultOwner;
        use pipette_hub_testkit::GOOD_TOKEN;

        let (dir, stores) = stores()?;
        let campaign = seed(&stores, "gated").await?;
        afm_mlx_plan(dir.path())?;
        // The mock Hub serves until it drops.
        let (_hub, imports) = gated_imports().await?;
        let imports = imports.with_progress(crate::hf_import::Progress::logged());
        let plans = tempfile::tempdir()?;
        let path = plans.path().join("gated.toml");
        std::fs::write(&path, gated_plan(GOOD_TOKEN))?;

        let (report, _) = ingest_into(
            &stores,
            IngestSettings {
                import: Some(&imports),
                ..settings(ModelCheck::none())
            },
            &campaign.campaign_id,
            PlanInput::Document(&path),
            None,
        )
        .await?;

        let (_, job_id) = report.job_ids.first().context("one job was filed")?;
        let stored = stores
            .plans
            .get_job_source(job_id)
            .await?
            .context("the job source was written")?;
        assert_eq!(stored["spec"]["model"]["source"], "vault_url", "{stored}");
        assert_eq!(
            stored["spec"]["model"]["principal_id"], "user_nightly",
            "{stored}"
        );
        assert!(!stored.to_string().contains("SECRET"), "{stored}");
        let owners: Vec<_> = pipette_store::drain_vault_resources(stores.vault.as_ref())
            .await?
            .items
            .into_iter()
            .map(|record| record.owner)
            .collect();
        let nightly = VaultOwner::User {
            user_id: UserId::try_new("user_nightly")?,
        };
        assert!(
            !owners.is_empty() && owners.iter().all(|owner| *owner == nightly),
            "{owners:?}"
        );
        Ok(())
    }

    /// The stored document names the vault arm with no token, so a re-run files
    /// it without the Hub, and the strict check reads it as the imported model.
    #[tokio::test]
    async fn a_gated_document_reruns_from_the_vault_under_strict_models() -> anyhow::Result<()> {
        let table = crate::model_params::ModelParamTable::from_toml(r#""LFM2-700M-GGUF" = 742"#)?;
        let GatedCampaign {
            _dir,
            stores,
            hub,
            imports: _,
            campaign,
            first,
        } = gated_campaign(
            ModelCheck::strict(&table),
            &gated_plan(pipette_hub_testkit::GOOD_TOKEN),
        )
        .await?;
        let source = stores
            .plans
            .get_plan_source(&first.plan_id)
            .await?
            .context("the plan source was stored")?;
        assert!(source.text().contains("vault_url"), "{}", source.text());
        assert!(!source.text().contains("auth_token"), "{}", source.text());
        assert!(!source.text().contains("SECRET"), "{}", source.text());

        let (_, job_id) = first.job_ids.first().context("one job was filed")?;
        pipette_store_testkit::finish_job(&*stores.submissions, &*stores.todo, job_id).await?;
        let calls = (hub.revision_requests(), hub.downloads());
        let (report, _) = rerun_jobs(
            &stores,
            settings(ModelCheck::strict(&table)),
            &campaign.campaign_id,
            BTreeSet::from([job_id.clone()]),
            None,
        )
        .await?;

        assert_eq!((hub.revision_requests(), hub.downloads()), calls);
        let (_, rerun) = report.job_ids.first().context("the re-run filed one job")?;
        let stored = stores
            .plans
            .get_job_source(rerun)
            .await?
            .context("the job source was written")?;
        assert_eq!(stored["spec"]["model"]["source"], "vault_url", "{stored}");
        Ok(())
    }

    /// A second macOS variant for the example plan: one more job, on a model the
    /// example does not run.
    const MLX_700M_VARIANT: &str = r#"
[[variants]]
requires   = ["os:macos"]
models     = [{ type = "mlx", source = "huggingface", org = "LiquidAI", repo_name = "LFM2-700M-MLX-4bit" }]
runtimes   = [{ type = "mlx_macos_pipette", version = "0.31.3", flavor = "macos-arm64", source = { type = "pip_requirements_text", contents = "mlx-lm==0.31.3\n" } }]
benchmarks = ["decode_throughput_512_100"]
"#;

    /// A campaign created from the example document, whose variant 0 runs one
    /// job on iOS and variant 1 two on macOS.
    async fn created(
        stores: &Stores,
        dir: &std::path::Path,
    ) -> anyhow::Result<(Campaign, IngestReport)> {
        create(
            stores,
            settings(ModelCheck::none()),
            a_new_campaign("nightly")?,
            &afm_mlx_plan(dir)?,
            None,
        )
        .await
        .map_err(anyhow::Error::new)
    }

    /// The queued jobs of `report` whose body `pred` takes.
    async fn queued_where(
        stores: &Stores,
        report: &IngestReport,
        pred: impl Fn(&serde_json::Value) -> bool,
    ) -> anyhow::Result<Vec<JobId>> {
        let bodies =
            futures::future::try_join_all(report.job_ids.iter().map(|(_, job_id)| async move {
                anyhow::Ok((job_id, stores.todo.get_avail_by_job(job_id).await?))
            }))
            .await?;
        Ok(bodies
            .into_iter()
            .filter(|(_, body)| body.as_ref().is_some_and(&pred))
            .map(|(job_id, _)| job_id.clone())
            .collect())
    }

    /// The queued jobs of `report` whose body requires `flag`.
    async fn queued_requiring(
        stores: &Stores,
        report: &IngestReport,
        flag: &str,
    ) -> anyhow::Result<Vec<JobId>> {
        queued_where(stores, report, |body| {
            body.get("requires")
                .and_then(serde_json::Value::as_array)
                .is_some_and(|flags| flags.iter().any(|f| f.as_str() == Some(flag)))
        })
        .await
    }

    /// [`Head::read`] and [`Head::revise`], as a surface runs them.
    async fn revise(
        stores: &Stores,
        campaign: Campaign,
        base: Option<&PlanId>,
        edit: &VariantEdit,
    ) -> Result<Revision, CampaignAdminError> {
        Head::read(&*stores.plans, campaign, base)
            .await?
            .revise(&*stores.plans, edit)
            .await
    }

    async fn maintain(stores: &Stores) -> anyhow::Result<()> {
        let catalog = crate::org_catalog::load_server_catalog(&*stores.catalog).await?;
        crate::queue_maintenance::run(
            &*stores.todo,
            &*stores.clients,
            &*stores.tags,
            &*stores.submissions,
            &*stores.plans,
            &catalog,
            crate::queue_maintenance::MaintenanceSettings {
                tmp_max: Duration::from_secs(86_400),
                plan_creating_max: CREATING_MAX_AGE,
                lost_lease_limit: 3,
            },
        )
        .await
    }

    /// Dropping the iOS variant and adding a macOS one carries both macOS jobs,
    /// the finished one included, queues one, and teardown retires the iOS job.
    #[tokio::test]
    async fn a_variant_edit_keeps_what_it_kept_and_stops_what_it_dropped() -> anyhow::Result<()> {
        let (dir, stores) = stores()?;
        let (campaign, first) = created(&stores, dir.path()).await?;
        let dropped = queued_requiring(&stores, &first, "os:ios").await?;
        let kept = queued_requiring(&stores, &first, "os:macos").await?;
        assert_eq!((dropped.len(), kept.len()), (1, 2));
        stores
            .submissions
            .write_processed(&kept[0], &serde_json::json!({"job_id": kept[0].as_str()}))
            .await?;
        stores.todo.delete_avail_by_job(&kept[0]).await?;

        let edit = VariantEdit::parse(&format!("remove = [0]\n{MLX_700M_VARIANT}"))?;
        let (report, revised) = edit_variants(
            &stores,
            settings(ModelCheck::none()),
            &campaign.campaign_id,
            &first.plan_id,
            &edit,
            Some("nightly-2".to_string()),
            Offer::default(),
        )
        .await?;

        assert_eq!(
            revised,
            Revised {
                superseded: first.plan_id.clone(),
                carried_job_count: 2,
                rerun_job_count: 0,
                recalculated_job_count: 0,
                dropped_job_count: 1,
                cancel_requested: true,
            }
        );
        assert_eq!(report.job_count, 3);
        let held: Vec<&JobId> = report.job_ids.iter().map(|(_, job_id)| job_id).collect();
        assert!(kept.iter().all(|job_id| held.contains(&job_id)));
        assert!(!held.contains(&&dropped[0]));

        let head = require(&*stores.campaigns, &campaign.campaign_id).await?;
        assert_eq!(head.current_plan_id, Some(report.plan_id.clone()));
        let manifest = stores
            .plans
            .get_plan(&report.plan_id)
            .await?
            .context("the revision's manifest")?;
        assert_eq!(manifest.previous_plan_id, Some(first.plan_id.clone()));
        let source = stores
            .plans
            .get_plan_source(&report.plan_id)
            .await?
            .context("the revision's document")?;
        assert!(source.text().contains("LFM2-700M-MLX-4bit"));
        assert!(!source.text().contains("apple_foundation"));
        assert!(stores.plans.has_cancel_marker(&first.plan_id).await?);

        // Teardown retires the dropped job and releases the kept one. A pass
        // that retired a job settles the plan on the next.
        maintain(&stores).await?;
        maintain(&stores).await?;
        assert_eq!(stores.todo.get_avail_by_job(&dropped[0]).await?, None);
        assert!(stores.todo.get_avail_by_job(&kept[1]).await?.is_some());
        let members: Vec<PlanId> = stores
            .plans
            .list_job_plan_markers(&kept[1])
            .await?
            .into_iter()
            .map(|(plan_id, _)| plan_id)
            .collect();
        assert_eq!(members, vec![report.plan_id.clone()]);
        let replaced = stores
            .plans
            .get_plan(&first.plan_id)
            .await?
            .context("the replaced manifest")?;
        assert_eq!(replaced.status, pipette_plan_types::PlanStatus::Cancelled);
        let counts = stores
            .plans
            .get_plan(&report.plan_id)
            .await?
            .and_then(|manifest| manifest.progress_snapshot)
            .context("the revision's snapshot")?
            .counts;
        assert_eq!((counts.finished, counts.available), (1, 2));
        Ok(())
    }

    /// Dropping a top-level benchmark drops its macOS job and carries the rest.
    /// The revision is filed as written, comments and all.
    #[tokio::test]
    async fn a_document_revision_carries_what_it_still_describes() -> anyhow::Result<()> {
        let (dir, stores) = stores()?;
        let (campaign, first) = created(&stores, dir.path()).await?;
        let runs = |benchmark: &'static str| {
            move |body: &serde_json::Value| {
                body.pointer("/spec/benchmark")
                    .and_then(serde_json::Value::as_str)
                    == Some(benchmark)
            }
        };
        let dropped = queued_where(&stores, &first, runs("end_to_end_latency_512_256")).await?;
        let kept = queued_where(&stores, &first, runs("decode_throughput_512_100")).await?;
        assert_eq!((dropped.len(), kept.len()), (1, 2));
        stores
            .submissions
            .write_processed(&kept[0], &serde_json::json!({"job_id": kept[0].as_str()}))
            .await?;
        stores.todo.delete_avail_by_job(&kept[0]).await?;

        let (base, head) = head_document(&stores, &campaign.campaign_id).await?;
        assert_eq!(base, first.plan_id);
        let document = format!(
            "# decode only\n{}",
            head.replace("  \"end_to_end_latency_512_256\",\n", "")
        );
        let (report, revised) = revise_document(
            &stores,
            settings(ModelCheck::none()),
            &campaign.campaign_id,
            &first.plan_id,
            document.clone(),
            None,
            Offer::default(),
        )
        .await?
        .context("a changed document files a revision")?;

        assert_eq!(
            revised,
            Revised {
                superseded: first.plan_id.clone(),
                carried_job_count: 2,
                rerun_job_count: 0,
                recalculated_job_count: 0,
                dropped_job_count: 1,
                cancel_requested: true,
            }
        );
        let held: Vec<&JobId> = report.job_ids.iter().map(|(_, job_id)| job_id).collect();
        assert_eq!(held.len(), 2);
        assert!(kept.iter().all(|job_id| held.contains(&job_id)));
        let head = require(&*stores.campaigns, &campaign.campaign_id).await?;
        assert_eq!(head.current_plan_id, Some(report.plan_id.clone()));
        let source = stores
            .plans
            .get_plan_source(&report.plan_id)
            .await?
            .context("the revision's document")?;
        assert_eq!(source.text(), document);
        assert!(stores.plans.has_cancel_marker(&first.plan_id).await?);

        // A retry of the same revision finds it filed, and files nothing more.
        let retried = revise_document(
            &stores,
            settings(ModelCheck::none()),
            &campaign.campaign_id,
            &first.plan_id,
            document,
            None,
            Offer::default(),
        )
        .await?;
        assert!(retried.is_none());
        let after = require(&*stores.campaigns, &campaign.campaign_id).await?;
        assert_eq!(after.current_plan_id, Some(report.plan_id.clone()));
        Ok(())
    }

    /// A document revision files nothing when the document is the head's own,
    /// and is refused when the campaign has moved off the plan it was read
    /// from.
    #[tokio::test]
    async fn a_document_revision_files_nothing_it_need_not() -> anyhow::Result<()> {
        let (dir, stores) = stores()?;
        let (campaign, first) = created(&stores, dir.path()).await?;
        let (_, head) = head_document(&stores, &campaign.campaign_id).await?;
        let revise = |base: PlanId, document: String| {
            let stores = &stores;
            let campaign_id = campaign.campaign_id.clone();
            async move {
                revise_document(
                    stores,
                    settings(ModelCheck::none()),
                    &campaign_id,
                    &base,
                    document,
                    None,
                    Offer::default(),
                )
                .await
            }
        };

        assert!(revise(first.plan_id.clone(), head.clone()).await?.is_none());
        let stale = revise(
            PlanId::try_new("plan-elsewhere")?,
            format!("# moved\n{head}"),
        )
        .await;
        assert!(
            matches!(stale, Err(CampaignAdminError::NotOnPlan { .. })),
            "{stale:?}"
        );
        let after = require(&*stores.campaigns, &campaign.campaign_id).await?;
        assert_eq!(after.current_plan_id, Some(first.plan_id.clone()));
        assert!(!stores.plans.has_cancel_marker(&first.plan_id).await?);
        Ok(())
    }

    /// Record `plan_id`'s digests under the previous merge rule, as a head filed
    /// before the rule widened. `unstored` also deletes the job sources, as a
    /// head filed before they were kept.
    async fn as_filed_under_v1(
        stores: &Stores,
        dir: &std::path::Path,
        plan_id: &PlanId,
        unstored: bool,
    ) -> anyhow::Result<()> {
        let mut manifest = stores
            .plans
            .get_plan(plan_id)
            .await?
            .context("the head manifest")?;
        let job_ids: Vec<JobId> = manifest.content_keys.keys().cloned().collect();
        manifest.content_keys = futures::stream::iter(job_ids)
            .then(|job_id| async move {
                let source = stores
                    .plans
                    .get_job_source(&job_id)
                    .await?
                    .context("the old job source")?;
                if unstored {
                    pipette_store_testkit::local_fs_remove_job_source(dir, &job_id)?;
                }
                anyhow::Ok((job_id, legacy_content_digest(&source)))
            })
            .try_collect()
            .await?;
        stores.plans.put_plan(&manifest).await?;
        Ok(())
    }

    /// A head that recorded digests under the previous merge rule carries its
    /// finished jobs that still match the revised plan, rekeyed from their
    /// stored bodies or, with none stored, from the head's own document.
    #[rstest::rstest]
    #[case::stored(false)]
    #[case::unstored(true)]
    #[tokio::test]
    async fn a_revision_rekeys_an_old_heads_jobs_before_carrying(
        #[case] unstored: bool,
    ) -> anyhow::Result<()> {
        let (dir, stores) = stores()?;
        let (campaign, first) = created(&stores, dir.path()).await?;
        let kept = queued_requiring(&stores, &first, "os:macos").await?;
        // Finished, so only the revision's carry keeps them: the merge joins
        // live jobs alone.
        futures::stream::iter(&kept)
            .map(Ok)
            .try_for_each(|job_id| {
                pipette_store_testkit::finish_job(&*stores.submissions, &*stores.todo, job_id)
            })
            .await?;
        as_filed_under_v1(&stores, dir.path(), &first.plan_id, unstored).await?;

        let revision = revise(
            &stores,
            campaign,
            Some(&first.plan_id),
            &VariantEdit::parse("remove = [0]")?,
        )
        .await?;
        let report = file_revision(&stores, &revision).await?;
        let revised_jobs: BTreeSet<JobId> = report
            .job_ids
            .into_iter()
            .map(|(_, job_id)| job_id)
            .collect();
        assert_eq!(revised_jobs, kept.into_iter().collect());
        Ok(())
    }

    /// A success recorded on `build` for each of `job_ids`.
    async fn finish_on_build(
        stores: &Stores,
        job_ids: &[JobId],
        build: Option<&str>,
    ) -> anyhow::Result<()> {
        futures::stream::iter(job_ids)
            .map(anyhow::Ok)
            .try_for_each(|job_id| {
                pipette_store_testkit::finish_job_with_success_on_build(
                    &*stores.submissions,
                    &*stores.todo,
                    job_id,
                    "decode_throughput",
                    build,
                )
            })
            .await
    }

    /// A failure recorded for each of `job_ids`.
    async fn fail_all(stores: &Stores, job_ids: &[JobId]) -> anyhow::Result<()> {
        futures::stream::iter(job_ids)
            .map(anyhow::Ok)
            .try_for_each(|job_id| {
                pipette_store_testkit::finish_job_with_failure(
                    &*stores.submissions,
                    &*stores.todo,
                    job_id,
                )
            })
            .await
    }

    /// [`revise_document`] under the limits these tests file with.
    async fn revise_to(
        stores: &Stores,
        campaign: &Campaign,
        base: &PlanId,
        document: &str,
        earlier: bool,
    ) -> Result<Option<IngestReport>, CampaignAdminError> {
        let offer = Offer {
            earlier,
            ..Offer::default()
        };
        revise_offering(stores, campaign, base, document, offer).await
    }

    /// [`revise_to`] offering `offer`.
    async fn revise_offering(
        stores: &Stores,
        campaign: &Campaign,
        base: &PlanId,
        document: &str,
        offer: Offer,
    ) -> Result<Option<IngestReport>, CampaignAdminError> {
        let filed = revise_document(
            stores,
            settings(ModelCheck::none()),
            &campaign.campaign_id,
            base,
            document.to_string(),
            None,
            offer,
        )
        .await?;
        Ok(filed.map(|(report, _)| report))
    }

    /// What [`revise_to`] with `carry_earlier` would do, by its dry run.
    async fn carrying_earlier_preview(
        stores: &Stores,
        campaign: &Campaign,
        base: &PlanId,
        document: &str,
    ) -> Result<RevisionPreview, CampaignAdminError> {
        let offer = Offer {
            earlier: true,
            ..Offer::default()
        };
        preview_offering(stores, campaign, base, document, offer).await
    }

    /// What [`revise_offering`] would do, by its dry run.
    async fn preview_offering(
        stores: &Stores,
        campaign: &Campaign,
        base: &PlanId,
        document: &str,
        offer: Offer,
    ) -> Result<RevisionPreview, CampaignAdminError> {
        let dry_run = dry_run_revise_document(
            stores,
            settings(ModelCheck::none()),
            &campaign.campaign_id,
            base,
            document.to_string(),
            None,
            offer,
        )
        .await?;
        Ok(dry_run.preview)
    }

    /// The example document with its macOS variant routed to `tag`.
    fn routed_to(dir: &std::path::Path, tag: &str) -> anyhow::Result<String> {
        Ok(std::fs::read_to_string(afm_mlx_plan(dir)?)?.replacen(
            "requires = [\"os:macos\"]",
            &format!("requires = [\"os:macos\", \"tag:{tag}\"]"),
            1,
        ))
    }

    fn held(report: &IngestReport) -> BTreeSet<JobId> {
        report
            .job_ids
            .iter()
            .map(|(_, job_id)| job_id.clone())
            .collect()
    }

    /// `carry_earlier` takes back runs that finished after a revision queued
    /// their cells again; without it the head's document files nothing.
    #[tokio::test]
    async fn carrying_earlier_brings_back_runs_the_head_queued_again() -> anyhow::Result<()> {
        let (dir, stores) = stores()?;
        let (campaign, first) = created(&stores, dir.path()).await?;
        let ran = queued_requiring(&stores, &first, "os:macos").await?;
        let routed = routed_to(dir.path(), "lab-east")?;
        let second = revise_to(&stores, &campaign, &first.plan_id, &routed, false)
            .await?
            .context("a changed document files a revision")?;
        assert!(ran.iter().all(|job_id| !held(&second).contains(job_id)));
        finish_on_build(&stores, &ran, Some("2026.10.05-old")).await?;

        let unswitched = revise_to(&stores, &campaign, &second.plan_id, &routed, false).await?;
        assert!(unswitched.is_none());
        let preview =
            carrying_earlier_preview(&stores, &campaign, &second.plan_id, &routed).await?;
        assert_eq!(preview.restored_job_count, ran.len());
        assert_eq!(preview.requirements_rerun_job_count, 0);
        assert_eq!(preview.dropped_job_ids.len(), ran.len());
        let third = revise_to(&stores, &campaign, &second.plan_id, &routed, true)
            .await?
            .context("carrying earlier runs files a revision")?;

        assert!(ran.iter().all(|job_id| held(&third).contains(job_id)));
        assert_eq!(held(&third).len(), held(&second).len());
        Ok(())
    }

    /// Two earlier plans ran the cells the head queued again: each cell takes
    /// one run, the newer plan's.
    #[tokio::test]
    async fn carrying_earlier_takes_the_newest_earlier_run() -> anyhow::Result<()> {
        let (dir, stores) = stores()?;
        let (campaign, first) = created(&stores, dir.path()).await?;
        let oldest = queued_requiring(&stores, &first, "os:macos").await?;
        let second = revise_to(
            &stores,
            &campaign,
            &first.plan_id,
            &routed_to(dir.path(), "lab-east")?,
            false,
        )
        .await?
        .context("the second plan")?;
        let newer = queued_requiring(&stores, &second, "os:macos").await?;
        let head_document = routed_to(dir.path(), "lab-sf")?;
        let third = revise_to(&stores, &campaign, &second.plan_id, &head_document, false)
            .await?
            .context("the third plan")?;
        finish_on_build(&stores, &oldest, None).await?;
        finish_on_build(&stores, &newer, None).await?;

        let preview =
            carrying_earlier_preview(&stores, &campaign, &third.plan_id, &head_document).await?;
        assert_eq!(preview.restored_job_count, newer.len());
        let fourth = revise_to(&stores, &campaign, &third.plan_id, &head_document, true)
            .await?
            .context("the fourth plan")?;

        assert_eq!(newer.len(), 2);
        assert!(newer.iter().all(|job_id| held(&fourth).contains(job_id)));
        assert!(oldest.iter().all(|job_id| !held(&fourth).contains(job_id)));
        Ok(())
    }

    /// A cell only an earlier plan ran, and failed, is new to the head: the dry
    /// run counts it with the cells to queue, not the head's runs to repeat.
    #[tokio::test]
    async fn carrying_earlier_counts_a_failed_earlier_run_as_new() -> anyhow::Result<()> {
        let (dir, stores) = stores()?;
        let (campaign, first) = created(&stores, dir.path()).await?;
        let ran = queued_requiring(&stores, &first, "os:macos").await?;
        let (second, _) = edit_variants(
            &stores,
            settings(ModelCheck::none()),
            &campaign.campaign_id,
            &first.plan_id,
            &VariantEdit::parse("remove = [1]")?,
            None,
            Offer::default(),
        )
        .await?;
        assert!(ran.iter().all(|job_id| !held(&second).contains(job_id)));
        fail_all(&stores, &ran).await?;
        let document = std::fs::read_to_string(afm_mlx_plan(dir.path())?)?;

        let preview =
            carrying_earlier_preview(&stores, &campaign, &second.plan_id, &document).await?;

        assert_eq!(preview.restored_job_count, 0);
        assert_eq!(preview.requirements_rerun_job_count, 0);
        assert_eq!(preview.new_job_count, ran.len());
        Ok(())
    }

    /// A job a re-run ran again stays out: the re-run was asked for.
    #[tokio::test]
    async fn carrying_earlier_leaves_out_a_job_a_rerun_replaced() -> anyhow::Result<()> {
        let (dir, stores) = stores()?;
        let (campaign, first) = created(&stores, dir.path()).await?;
        let ran = queued_requiring(&stores, &first, "os:macos").await?;
        finish_on_build(&stores, &ran, None).await?;
        let (second, _) = rerun_jobs(
            &stores,
            settings(ModelCheck::none()),
            &campaign.campaign_id,
            ran.iter().cloned().collect(),
            None,
        )
        .await?;
        let (_, document) = head_document(&stores, &campaign.campaign_id).await?;

        let preview =
            carrying_earlier_preview(&stores, &campaign, &second.plan_id, &document).await?;

        assert_eq!(preview.restored_job_count, 0);
        assert!(preview.dropped_job_ids.is_empty());
        Ok(())
    }

    /// A revision files the `--note` of the operator, trimmed. A revision also
    /// files the change that it made, with the runs that it offers. An empty
    /// `--note` is an error, as on a first plan.
    #[tokio::test]
    async fn a_revision_states_what_it_changed_beside_its_note() -> anyhow::Result<()> {
        let (dir, stores) = stores()?;
        let (campaign, first) = created(&stores, dir.path()).await?;
        let ran = queued_requiring(&stores, &first, "os:macos").await?;
        finish_on_build(&stores, &ran, None).await?;
        let job = ran.first().context("a macOS job")?;
        let rerun = |note: &str| {
            rerun_jobs(
                &stores,
                settings(ModelCheck::none()),
                &campaign.campaign_id,
                BTreeSet::from([job.clone()]),
                Some(note.to_owned()),
            )
        };
        assert!(rerun("  ").await.is_err());
        let (second, _) = rerun(" flaky thermals ").await?;
        assert_eq!(
            (second.note.clone(), second.change.clone()),
            (
                Some(NonEmptyTrimmedString::try_new("flaky thermals")?),
                Some(RevisionChange::Rerun {
                    answered: None,
                    job_count: 1
                })
            )
        );

        let (third, _) = edit_variants(
            &stores,
            settings(ModelCheck::none()),
            &campaign.campaign_id,
            &second.plan_id,
            &VariantEdit::parse("remove = [1]")?,
            None,
            Offer::read(
                &*stores.plans,
                &*stores.campaigns,
                true,
                std::slice::from_ref(&first.plan_id),
            )
            .await?,
        )
        .await?;
        assert_eq!(
            (third.note, third.change),
            (
                None,
                Some(RevisionChange::Variants {
                    removed_variant_count: 1,
                    added_variant_count: 0,
                    carry_earlier: true,
                    inherit_from: vec![first.plan_id],
                    restored: Vec::new(),
                })
            )
        );
        Ok(())
    }

    /// Which plan a revert case names.
    #[derive(Debug, Clone, Copy)]
    enum RevertTarget {
        /// The first plan, whose macOS jobs finished.
        First,
        /// The head, which dropped the macOS variant.
        Head,
        /// The oldest plan spliced between the first plan and the head, which
        /// has no stored document, as a plan filed as job bodies has none.
        Spliced,
        /// The oldest spliced plan, with its manifest removed from the store.
        Gone,
        /// The first plan of another campaign.
        Foreign,
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum RevertOutcome {
        Filed,
        Unchanged,
        NotInCampaign,
        NotOnPlan,
        NoDocument,
    }

    /// A campaign runs its first plan's macOS jobs to the end, then drops the
    /// macOS variant. A revert to the first plan brings those runs back and
    /// queues nothing again. The `spliced` plans lie between the first plan
    /// and the head. A case can then name a plan past [`CHAIN_MAX`], or a plan
    /// with no document.
    #[rstest::rstest]
    #[case::an_earlier_plan(0, RevertTarget::First, false, RevertOutcome::Filed)]
    #[case::a_plan_past_the_ceiling(CHAIN_MAX, RevertTarget::First, false, RevertOutcome::Filed)]
    #[case::the_head(0, RevertTarget::Head, false, RevertOutcome::Unchanged)]
    #[case::a_plan_of_another_campaign(
        0,
        RevertTarget::Foreign,
        false,
        RevertOutcome::NotInCampaign
    )]
    #[case::a_stale_base(0, RevertTarget::First, true, RevertOutcome::NotOnPlan)]
    #[case::a_plan_with_no_document(1, RevertTarget::Spliced, false, RevertOutcome::NoDocument)]
    #[case::a_plan_whose_manifest_is_gone(1, RevertTarget::Gone, false, RevertOutcome::NoDocument)]
    #[tokio::test]
    async fn a_revert_files_a_copy_of_an_earlier_plan(
        #[case] spliced: usize,
        #[case] target: RevertTarget,
        #[case] stale: bool,
        #[case] expected: RevertOutcome,
    ) -> anyhow::Result<()> {
        let (dir, stores) = stores()?;
        let (campaign, first) = created(&stores, dir.path()).await?;
        let ran = queued_requiring(&stores, &first, "os:macos").await?;
        finish_on_build(&stores, &ran, None).await?;
        let (second, _) = edit_variants(
            &stores,
            settings(ModelCheck::none()),
            &campaign.campaign_id,
            &first.plan_id,
            &VariantEdit::parse("remove = [1]")?,
            None,
            Offer::default(),
        )
        .await?;
        assert!(ran.iter().all(|job_id| !held(&second).contains(job_id)));
        let spliced = pipette_store_testkit::splice_without_document(
            stores.plans.as_ref(),
            &second.plan_id,
            spliced,
        )
        .await?;
        let target = match target {
            RevertTarget::First => first.plan_id.clone(),
            RevertTarget::Head => second.plan_id.clone(),
            RevertTarget::Spliced => spliced.first().context("a spliced plan")?.clone(),
            RevertTarget::Gone => {
                let gone = spliced.first().context("a spliced plan")?.clone();
                pipette_store_testkit::local_fs_remove_plan(dir.path(), &gone)?;
                gone
            }
            RevertTarget::Foreign => created(&stores, dir.path()).await?.1.plan_id,
        };
        let base = if stale {
            &first.plan_id
        } else {
            &second.plan_id
        };
        let (stores, target, campaign_id) = (&stores, &target, &campaign.campaign_id);
        let revert = |base: PlanId| async move {
            revert_to(
                stores,
                settings(ModelCheck::none()),
                campaign_id,
                &base,
                target,
                None,
            )
            .await
        };
        let dry_run = dry_run_revert(
            stores,
            settings(ModelCheck::none()),
            campaign_id,
            base,
            target,
            None,
        )
        .await;

        let mut filed = None;
        let outcome = match revert(base.clone()).await {
            Ok(Some((report, _))) => {
                filed = Some(report);
                RevertOutcome::Filed
            }
            Ok(None) => RevertOutcome::Unchanged,
            Err(CampaignAdminError::NotInCampaign { plan_id, .. }) if plan_id == *target => {
                RevertOutcome::NotInCampaign
            }
            Err(CampaignAdminError::NotOnPlan { .. }) => RevertOutcome::NotOnPlan,
            Err(CampaignAdminError::NoDocument(plan_id)) if plan_id == *target => {
                RevertOutcome::NoDocument
            }
            Err(other) => return Err(other.into()),
        };
        assert_eq!(outcome, expected);
        let preview = match (expected, dry_run) {
            (RevertOutcome::Filed | RevertOutcome::Unchanged, dry_run) => dry_run?.preview,
            (_, dry_run) => {
                assert!(dry_run.is_err(), "the dry run refuses it too");
                return Ok(());
            }
        };
        if expected == RevertOutcome::Unchanged {
            assert!(preview.unchanged);
            return Ok(());
        }
        assert_eq!(preview.restored_job_count, ran.len());

        let filed = filed.context("the revert filed a plan")?;
        let current = stores
            .campaigns
            .get_campaign(campaign_id)
            .await?
            .and_then(|campaign| campaign.current_plan_id)
            .context("the campaign runs a plan")?;
        assert_eq!(current, filed.plan_id);
        let reverted = stores
            .plans
            .get_plan(&current)
            .await?
            .context("the revert")?;
        assert!(ran.iter().all(|job_id| reverted.job_ids.contains(job_id)));
        assert_eq!(reverted.previous_plan_id.as_ref(), Some(&second.plan_id));
        assert_eq!(
            filed.change,
            Some(RevisionChange::Revert {
                to_plan_id: first.plan_id.clone()
            })
        );
        assert!(stores.plans.has_cancel_marker(&second.plan_id).await?);
        assert!(
            revert(current).await?.is_none(),
            "a second revert to the same plan changes nothing"
        );
        Ok(())
    }

    /// A job that lapsed for a later plan stays out, though the plan it was
    /// carried from lists it and its run was recorded since.
    #[tokio::test]
    async fn carrying_earlier_leaves_out_a_job_that_lapsed_later() -> anyhow::Result<()> {
        let (dir, stores) = stores()?;
        let (campaign, first) = created(&stores, dir.path()).await?;
        let ran = queued_requiring(&stores, &first, "os:macos").await?;
        let widened = format!(
            "{}{MLX_700M_VARIANT}",
            std::fs::read_to_string(afm_mlx_plan(dir.path())?)?
        );
        let second = revise_to(&stores, &campaign, &first.plan_id, &widened, false)
            .await?
            .context("the second plan")?;
        assert!(ran.iter().all(|job_id| held(&second).contains(job_id)));
        let mut manifest = stores
            .plans
            .get_plan(&second.plan_id)
            .await?
            .context("the second manifest")?;
        manifest
            .expired_jobs
            .extend(ran.iter().cloned().map(|job_id| {
                let lapsed = pipette_core::plan::ExpiredJob {
                    horizon: Utc::now() - chrono::Duration::days(1),
                    recorded_at: Utc::now(),
                };
                (job_id, lapsed)
            }));
        stores.plans.put_plan(&manifest).await?;
        let routed = widened.replacen(
            "requires = [\"os:macos\"]",
            "requires = [\"os:macos\", \"tag:lab-east\"]",
            1,
        );
        let third = revise_to(&stores, &campaign, &second.plan_id, &routed, false)
            .await?
            .context("the third plan")?;
        assert!(ran.iter().all(|job_id| !held(&third).contains(job_id)));
        finish_on_build(&stores, &ran, None).await?;

        let preview = carrying_earlier_preview(&stores, &campaign, &third.plan_id, &routed).await?;

        assert_eq!(preview.restored_job_count, 0);
        Ok(())
    }

    /// A campaign that runs the cells under its own routing, so its jobs are
    /// its own, and the plan it runs.
    async fn donor(
        stores: &Stores,
        dir: &std::path::Path,
    ) -> anyhow::Result<(Campaign, IngestReport)> {
        donor_routed(stores, dir, "lab-east").await
    }

    /// [`donor`] with its macOS variant routed to `tag`.
    async fn donor_routed(
        stores: &Stores,
        dir: &std::path::Path,
        tag: &str,
    ) -> anyhow::Result<(Campaign, IngestReport)> {
        donor_of(stores, dir, tag, &routed_to(dir, tag)?).await
    }

    /// A campaign named `name` running `document`, and the plan it runs.
    async fn donor_of(
        stores: &Stores,
        dir: &std::path::Path,
        name: &str,
        document: &str,
    ) -> anyhow::Result<(Campaign, IngestReport)> {
        let path = dir.join(format!("{name}.toml"));
        std::fs::write(&path, document)?;
        let created = create(
            stores,
            settings(ModelCheck::none()),
            a_new_campaign(name)?,
            &path,
            None,
        )
        .await?;
        Ok(created)
    }

    async fn inheriting(stores: &Stores, named: &PlanId) -> anyhow::Result<Offer> {
        inheriting_from(stores, std::slice::from_ref(named)).await
    }

    async fn inheriting_from(stores: &Stores, named: &[PlanId]) -> anyhow::Result<Offer> {
        Ok(Offer::read(&*stores.plans, &*stores.campaigns, false, named).await?)
    }

    /// The plans other than `head` that `preview` holds jobs from.
    fn held_from(preview: &RevisionPreview, head: &PlanId) -> BTreeSet<PlanId> {
        preview
            .held_jobs
            .iter()
            .map(|held| held.plan_id.clone())
            .filter(|plan_id| plan_id != head)
            .collect()
    }

    /// `inherit_from` lets a cell take another campaign's finished run in
    /// place of its own queued job, which stops with the head.
    #[tokio::test]
    async fn inheriting_takes_another_campaigns_finished_runs() -> anyhow::Result<()> {
        let (dir, stores) = stores()?;
        let (campaign, first) = created(&stores, dir.path()).await?;
        let queued = queued_requiring(&stores, &first, "os:macos").await?;
        let (_, donor) = donor(&stores, dir.path()).await?;
        let ran = queued_requiring(&stores, &donor, "os:macos").await?;
        assert!(ran.iter().all(|job_id| !queued.contains(job_id)));
        finish_on_build(&stores, &ran, None).await?;
        let document = std::fs::read_to_string(afm_mlx_plan(dir.path())?)?;
        let offer = inheriting(&stores, &donor.plan_id).await?;

        let unswitched = revise_to(&stores, &campaign, &first.plan_id, &document, false).await?;
        assert!(unswitched.is_none());
        let preview =
            preview_offering(&stores, &campaign, &first.plan_id, &document, offer.clone()).await?;
        assert_eq!(preview.inherited_job_count, ran.len());
        assert_eq!(preview.restored_job_count, 0);
        assert_eq!(
            preview.dropped_job_ids.iter().collect::<BTreeSet<_>>(),
            queued.iter().collect()
        );
        let sources: std::collections::BTreeMap<&JobId, &PlanId> = preview
            .held_jobs
            .iter()
            .map(|held| (&held.job_id, &held.plan_id))
            .collect();
        let expected: std::collections::BTreeMap<&JobId, &PlanId> = first
            .job_ids
            .iter()
            .map(|(_, job_id)| job_id)
            .filter(|job_id| !queued.contains(job_id))
            .map(|job_id| (job_id, &first.plan_id))
            .chain(ran.iter().map(|job_id| (job_id, &donor.plan_id)))
            .collect();
        assert_eq!(sources, expected);
        let second = revise_offering(&stores, &campaign, &first.plan_id, &document, offer)
            .await?
            .context("inheriting files a revision")?;

        assert!(ran.iter().all(|job_id| held(&second).contains(job_id)));
        assert!(queued.iter().all(|job_id| !held(&second).contains(job_id)));
        assert_eq!(held(&second).len(), held(&first).len());
        Ok(())
    }

    /// Named plans offer their runs in the order named: a cell takes the first
    /// run that succeeded, past a named plan whose run failed.
    #[tokio::test]
    async fn inheriting_takes_the_first_named_success() -> anyhow::Result<()> {
        let (dir, stores) = stores()?;
        let (campaign, first) = created(&stores, dir.path()).await?;
        let (_, failed) = donor_routed(&stores, dir.path(), "lab-sf").await?;
        let (_, east) = donor_routed(&stores, dir.path(), "lab-east").await?;
        let (_, ny) = donor_routed(&stores, dir.path(), "lab-ny").await?;
        let lost = queued_requiring(&stores, &failed, "os:macos").await?;
        fail_all(&stores, &lost).await?;
        finish_on_build(
            &stores,
            &queued_requiring(&stores, &east, "os:macos").await?,
            None,
        )
        .await?;
        finish_on_build(
            &stores,
            &queued_requiring(&stores, &ny, "os:macos").await?,
            None,
        )
        .await?;
        let document = std::fs::read_to_string(afm_mlx_plan(dir.path())?)?;

        let named = [
            failed.plan_id.clone(),
            ny.plan_id.clone(),
            east.plan_id.clone(),
        ];
        let offer = inheriting_from(&stores, &named).await?;
        let preview =
            preview_offering(&stores, &campaign, &first.plan_id, &document, offer).await?;
        assert_eq!(
            held_from(&preview, &first.plan_id),
            BTreeSet::from([ny.plan_id.clone()])
        );
        assert_eq!(preview.inherited_job_count, lost.len());

        let named = [east.plan_id.clone(), ny.plan_id.clone()];
        let offer = inheriting_from(&stores, &named).await?;
        let preview =
            preview_offering(&stores, &campaign, &first.plan_id, &document, offer).await?;
        assert_eq!(
            held_from(&preview, &first.plan_id),
            BTreeSet::from([east.plan_id])
        );
        Ok(())
    }

    #[tokio::test]
    async fn inheriting_queues_a_cell_no_named_plan_ran() -> anyhow::Result<()> {
        let (dir, stores) = stores()?;
        let (campaign, first) = created(&stores, dir.path()).await?;
        let (_, donor) = donor(&stores, dir.path()).await?;
        let ran = queued_requiring(&stores, &donor, "os:macos").await?;
        finish_on_build(&stores, &ran, None).await?;
        let document = format!(
            "{}{MLX_700M_VARIANT}",
            std::fs::read_to_string(afm_mlx_plan(dir.path())?)?
        );
        let offer = inheriting(&stores, &donor.plan_id).await?;

        let preview =
            preview_offering(&stores, &campaign, &first.plan_id, &document, offer.clone()).await?;
        assert_eq!(
            (preview.inherited_job_count, preview.new_job_count),
            (ran.len(), 1)
        );
        assert_eq!(preview.held_jobs.len(), 3);
        let second = revise_offering(&stores, &campaign, &first.plan_id, &document, offer)
            .await?
            .context("inheriting files a revision")?;

        let minted: Vec<JobId> = held(&second)
            .into_iter()
            .filter(|job_id| !held(&first).contains(job_id) && !ran.contains(job_id))
            .collect();
        assert_eq!(minted.len(), 1, "{minted:?}");
        Ok(())
    }

    /// A named plan's job that is still queued is not inherited: the content
    /// key merge joins it, as it joins any live job of the cell.
    #[tokio::test]
    async fn inheriting_leaves_a_live_job_to_the_merge() -> anyhow::Result<()> {
        let (dir, stores) = stores()?;
        let (campaign, first) = created(&stores, dir.path()).await?;
        let document = format!(
            "{}{MLX_700M_VARIANT}",
            std::fs::read_to_string(afm_mlx_plan(dir.path())?)?
        );
        let (_, donor) = donor_of(&stores, dir.path(), "wider", &document).await?;
        let live: Vec<JobId> = held(&donor)
            .into_iter()
            .filter(|job_id| !held(&first).contains(job_id))
            .collect();
        assert_eq!(live.len(), 1, "{live:?}");
        let offer = inheriting(&stores, &donor.plan_id).await?;

        let preview =
            preview_offering(&stores, &campaign, &first.plan_id, &document, offer.clone()).await?;
        assert_eq!(preview.inherited_job_count, 0);
        let second = revise_offering(&stores, &campaign, &first.plan_id, &document, offer)
            .await?
            .context("inheriting files a revision")?;

        assert_eq!(held(&second), held(&donor));
        Ok(())
    }

    /// `add_from` appends a plan's variants, each with the plan's benchmark
    /// list where it names none, so the cells are the ones that plan ran and
    /// take its finished runs: nothing queues.
    #[tokio::test]
    async fn adding_a_plans_variants_takes_its_finished_runs() -> anyhow::Result<()> {
        let (dir, stores) = stores()?;
        let (campaign, first) = created(&stores, dir.path()).await?;
        // Its one variant names no benchmarks, and the plan's list is not the
        // campaign's.
        let document = format!(
            "benchmarks = [\"end_to_end_latency_512_256\"]\n{}",
            MLX_700M_VARIANT.replace("benchmarks = [\"decode_throughput_512_100\"]\n", "")
        );
        let (_, donor) = donor_of(&stores, dir.path(), "donor", &document).await?;
        let ran: Vec<JobId> = held(&donor).into_iter().collect();
        assert_eq!(ran.len(), 1, "{ran:?}");
        finish_on_build(&stores, &ran, None).await?;
        let add_from = [donor.plan_id.clone()];
        let offer = inheriting_from(&stores, &super::inheriting(&[], &add_from, &[])).await?;
        let edit =
            VariantEdit::default().adding(offer.variants_of(&*stores.plans, &add_from).await?);

        let dry_run = dry_run_variants(
            &stores,
            settings(ModelCheck::none()),
            &campaign.campaign_id,
            &first.plan_id,
            &edit,
            None,
            offer.clone(),
        )
        .await?;
        assert_eq!(
            (
                dry_run.preview.inherited_job_count,
                dry_run.preview.new_job_count
            ),
            (1, 0)
        );
        assert!(dry_run.preview.repeated_variants.is_empty());
        let (second, _) = edit_variants(
            &stores,
            settings(ModelCheck::none()),
            &campaign.campaign_id,
            &first.plan_id,
            &edit,
            None,
            offer,
        )
        .await?;

        let expected: BTreeSet<JobId> = held(&first).into_iter().chain(ran).collect();
        assert_eq!(held(&second), expected);
        Ok(())
    }

    /// A copied variant the campaign already runs is reported as repeated,
    /// and adds no job.
    #[tokio::test]
    async fn adding_a_variant_the_campaign_runs_reports_it_repeated() -> anyhow::Result<()> {
        let (dir, stores) = stores()?;
        let (campaign, first) = created(&stores, dir.path()).await?;
        let document = format!(
            "{}{MLX_700M_VARIANT}",
            std::fs::read_to_string(afm_mlx_plan(dir.path())?)?
        );
        let (_, wider) = donor_of(&stores, dir.path(), "wider", &document).await?;
        let add_from = [wider.plan_id.clone()];
        let offer = inheriting_from(&stores, &add_from).await?;
        let edit =
            VariantEdit::default().adding(offer.variants_of(&*stores.plans, &add_from).await?);

        let dry_run = dry_run_variants(
            &stores,
            settings(ModelCheck::none()),
            &campaign.campaign_id,
            &first.plan_id,
            &edit,
            None,
            offer,
        )
        .await?;

        assert_eq!(dry_run.preview.repeated_variants, vec![2, 3]);
        assert_eq!(dry_run.report.bodies.len(), held(&wider).len());
        Ok(())
    }

    /// `add_from` names a plan the offer read, so its read was checked.
    #[tokio::test]
    async fn adding_from_a_plan_not_offered_is_refused() -> anyhow::Result<()> {
        let (dir, stores) = stores()?;
        let (_, first) = created(&stores, dir.path()).await?;

        let refused = Offer::default()
            .variants_of(&*stores.plans, std::slice::from_ref(&first.plan_id))
            .await;

        assert!(
            matches!(refused, Err(CampaignAdminError::NoSuchPlan(plan_id)) if plan_id == first.plan_id)
        );
        Ok(())
    }

    /// `restore` adds the macOS variant that a variant edit removed
    /// after its jobs finished. It appends the variant, or replaces the iOS
    /// variant with it. The restored cells take the first plan's runs, so
    /// nothing queues, and the change names the variant.
    #[rstest::rstest]
    #[case::append(&[])]
    #[case::replace(&[0])]
    #[tokio::test]
    async fn restoring_a_variant_takes_its_finished_runs(
        #[case] remove: &[usize],
    ) -> anyhow::Result<()> {
        let (dir, stores) = stores()?;
        let (campaign, first) = created(&stores, dir.path()).await?;
        let ran = queued_requiring(&stores, &first, "os:macos").await?;
        finish_on_build(&stores, &ran, None).await?;
        let ios: BTreeSet<JobId> = held(&first)
            .into_iter()
            .filter(|job_id| !ran.contains(job_id))
            .collect();
        let (second, _) = edit_variants(
            &stores,
            settings(ModelCheck::none()),
            &campaign.campaign_id,
            &first.plan_id,
            &VariantEdit::parse("remove = [1]")?,
            None,
            Offer::default(),
        )
        .await?;
        assert_eq!(held(&second), ios);
        let restore = [RestoredVariant {
            plan_id: first.plan_id.clone(),
            variant_index: 1,
        }];
        let offer = inheriting_from(&stores, &super::inheriting(&[], &[], &restore)).await?;
        let edit = VariantEdit {
            remove: remove.to_vec(),
            ..VariantEdit::default()
        }
        .restoring(
            offer.restored_variants(&*stores.plans, &restore).await?,
            restore.to_vec(),
        );

        let dry_run = dry_run_variants(
            &stores,
            settings(ModelCheck::none()),
            &campaign.campaign_id,
            &second.plan_id,
            &edit,
            None,
            offer.clone(),
        )
        .await?;
        assert_eq!(
            (
                dry_run.preview.inherited_job_count,
                dry_run.preview.new_job_count
            ),
            (ran.len(), 0)
        );
        let (third, _) = edit_variants(
            &stores,
            settings(ModelCheck::none()),
            &campaign.campaign_id,
            &second.plan_id,
            &edit,
            None,
            offer,
        )
        .await?;

        let kept = if remove.is_empty() {
            ios
        } else {
            BTreeSet::new()
        };
        assert_eq!(held(&third), kept.into_iter().chain(ran).collect());
        assert_eq!(
            third.change,
            Some(RevisionChange::Variants {
                removed_variant_count: remove.len(),
                added_variant_count: 1,
                carry_earlier: false,
                inherit_from: vec![first.plan_id],
                restored: restore.to_vec(),
            })
        );
        Ok(())
    }

    #[derive(Debug, Clone, Copy)]
    enum RestoreRefusal {
        OutOfRange,
        /// A plan that the offer does not name, so its read was not checked.
        NotOffered,
    }

    #[rstest::rstest]
    #[case::out_of_range(RestoreRefusal::OutOfRange)]
    #[case::not_offered(RestoreRefusal::NotOffered)]
    #[tokio::test]
    async fn restoring_a_variant_that_is_not_there_is_refused(
        #[case] refusal: RestoreRefusal,
    ) -> anyhow::Result<()> {
        let (dir, stores) = stores()?;
        let (_, first) = created(&stores, dir.path()).await?;
        let (offer, variant_index) = match refusal {
            RestoreRefusal::OutOfRange => (inheriting(&stores, &first.plan_id).await?, 2),
            RestoreRefusal::NotOffered => (Offer::default(), 0),
        };
        let restore = [RestoredVariant {
            plan_id: first.plan_id.clone(),
            variant_index,
        }];

        let refused = offer.restored_variants(&*stores.plans, &restore).await;

        match (refusal, refused) {
            (
                RestoreRefusal::OutOfRange,
                Err(CampaignAdminError::NoSuchSourceVariant {
                    plan_id,
                    index: 2,
                    count: 2,
                }),
            )
            | (RestoreRefusal::NotOffered, Err(CampaignAdminError::NoSuchPlan(plan_id))) => {
                assert_eq!(plan_id, first.plan_id);
            }
            (refusal, other) => anyhow::bail!("{refusal:?} answered {other:?}"),
        }
        Ok(())
    }

    /// How a named plan's campaign gave up the run of its job.
    #[derive(Debug, Clone, Copy)]
    enum GivenUp {
        Failed,
        RanAgain,
        Lapsed,
    }

    /// A named plan's run its campaign gave up is not taken: the run failed,
    /// or a later plan of that campaign re-ran the job or let it lapse. The
    /// head's queued job stays.
    #[rstest::rstest]
    #[case::failed(GivenUp::Failed)]
    #[case::ran_again(GivenUp::RanAgain)]
    #[case::lapsed(GivenUp::Lapsed)]
    #[tokio::test]
    async fn inheriting_leaves_out_a_run_its_campaign_gave_up(
        #[case] given_up: GivenUp,
    ) -> anyhow::Result<()> {
        let (dir, stores) = stores()?;
        let (campaign, first) = created(&stores, dir.path()).await?;
        let (donor_campaign, donor) = donor(&stores, dir.path()).await?;
        let ran = queued_requiring(&stores, &donor, "os:macos").await?;
        match given_up {
            GivenUp::Failed => fail_all(&stores, &ran).await?,
            GivenUp::RanAgain => {
                finish_on_build(&stores, &ran, None).await?;
                rerun_jobs(
                    &stores,
                    settings(ModelCheck::none()),
                    &donor_campaign.campaign_id,
                    ran.iter().cloned().collect(),
                    None,
                )
                .await?;
            }
            GivenUp::Lapsed => {
                let widened = format!("{}{MLX_700M_VARIANT}", routed_to(dir.path(), "lab-east")?);
                let later = revise_to(&stores, &donor_campaign, &donor.plan_id, &widened, false)
                    .await?
                    .context("the donor's second plan")?;
                assert!(ran.iter().all(|job_id| held(&later).contains(job_id)));
                let mut manifest = stores
                    .plans
                    .get_plan(&later.plan_id)
                    .await?
                    .context("the donor's second manifest")?;
                manifest
                    .expired_jobs
                    .extend(ran.iter().cloned().map(|job_id| {
                        let lapsed = pipette_core::plan::ExpiredJob {
                            horizon: Utc::now() - chrono::Duration::days(1),
                            recorded_at: Utc::now(),
                        };
                        (job_id, lapsed)
                    }));
                stores.plans.put_plan(&manifest).await?;
                finish_on_build(&stores, &ran, None).await?;
            }
        }
        let document = std::fs::read_to_string(afm_mlx_plan(dir.path())?)?;

        let preview = preview_offering(
            &stores,
            &campaign,
            &first.plan_id,
            &document,
            inheriting(&stores, &donor.plan_id).await?,
        )
        .await?;

        assert_eq!(preview.inherited_job_count, 0);
        assert!(preview.dropped_job_ids.is_empty());
        Ok(())
    }

    /// The campaign's own earlier runs answer a cell before a named plan's,
    /// and each is counted apart.
    #[tokio::test]
    async fn inheriting_comes_after_the_campaigns_earlier_plans() -> anyhow::Result<()> {
        let (dir, stores) = stores()?;
        let (campaign, first) = created(&stores, dir.path()).await?;
        let earlier = queued_requiring(&stores, &first, "os:macos").await?;
        let routed = routed_to(dir.path(), "lab-sf")?;
        let second = revise_to(&stores, &campaign, &first.plan_id, &routed, false)
            .await?
            .context("the second plan")?;
        finish_on_build(&stores, &earlier, None).await?;
        let (_, donor) = donor(&stores, dir.path()).await?;
        let donated = queued_requiring(&stores, &donor, "os:macos").await?;
        finish_on_build(&stores, &donated, None).await?;
        let both = Offer {
            earlier: true,
            ..inheriting(&stores, &donor.plan_id).await?
        };

        let preview = preview_offering(&stores, &campaign, &second.plan_id, &routed, both).await?;
        assert_eq!(preview.restored_job_count, earlier.len());
        assert_eq!(preview.inherited_job_count, 0);

        let named = inheriting(&stores, &donor.plan_id).await?;
        let preview = preview_offering(&stores, &campaign, &second.plan_id, &routed, named).await?;
        assert_eq!(preview.restored_job_count, 0);
        assert_eq!(preview.inherited_job_count, donated.len());
        Ok(())
    }

    #[tokio::test]
    async fn inheriting_from_a_missing_plan_is_refused() -> anyhow::Result<()> {
        let (_dir, stores) = stores()?;
        let missing = PlanId::try_new("plan-missing")?;

        let refused = Offer::read(
            &*stores.plans,
            &*stores.campaigns,
            false,
            std::slice::from_ref(&missing),
        )
        .await;

        assert!(
            matches!(refused, Err(CampaignAdminError::NoSuchPlan(plan_id)) if plan_id == missing)
        );
        Ok(())
    }

    /// A document's pin or tag routes, so the head's runs elsewhere carry,
    /// unless it is marked `:required`. The marked document files the plain
    /// `flag`, so the same devices match it.
    #[rstest::rstest]
    #[case::a_build(
        "client_versions = [\"2026.10.12-new\"]",
        "client_version:2026.10.12-new",
        false,
        2
    )]
    #[case::a_required_build(
        "client_versions = [\"2026.10.12-new:required\"]",
        "client_version:2026.10.12-new",
        true,
        0
    )]
    #[case::a_tag(
        "requires = [\"os:macos\", \"tag:lab-east\"]",
        "tag:lab-east",
        false,
        2
    )]
    #[case::a_required_tag(
        "requires = [\"os:macos\", \"tag:lab-east:required\"]",
        "tag:lab-east",
        true,
        0
    )]
    #[tokio::test]
    async fn a_document_marks_a_routing_flag_required(
        #[case] lines: &str,
        #[case] flag: &str,
        #[case] required: bool,
        #[case] carried: usize,
    ) -> anyhow::Result<()> {
        let (dir, stores) = stores()?;
        let (campaign, first) = created(&stores, dir.path()).await?;
        let ran = queued_requiring(&stores, &first, "os:macos").await?;
        finish_on_build(&stores, &ran, Some("2026.10.05-old")).await?;
        let lines = if lines.starts_with("requires") {
            lines.to_string()
        } else {
            format!("requires = [\"os:macos\"]\n{lines}")
        };
        let document = std::fs::read_to_string(afm_mlx_plan(dir.path())?)?.replacen(
            "requires = [\"os:macos\"]",
            &lines,
            1,
        );

        let revision = Head::read(&*stores.plans, campaign, None)
            .await?
            .replace(&*stores.plans, document)
            .await?;
        let report = file_revision(&stores, &revision).await?;

        let revised: BTreeSet<JobId> = report
            .job_ids
            .into_iter()
            .map(|(_, job_id)| job_id)
            .collect();
        assert_eq!(ran.len(), 2);
        assert_eq!(
            ran.iter()
                .filter(|job_id| revised.contains(*job_id))
                .count(),
            carried
        );
        let plans = &*stores.plans;
        futures::stream::iter(&revised)
            .map(anyhow::Ok)
            .try_for_each(|job_id| async move {
                let body = plans.get_job_source(job_id).await?.context("a source")?;
                let text = body.to_string();
                assert!(!text.contains(":required"), "{body}");
                let listed = body.get("required_flags") == Some(&serde_json::json!([flag]));
                assert_eq!(listed, required && text.contains(flag), "{body}");
                Ok(())
            })
            .await?;
        Ok(())
    }

    /// A body whose `auth_token`, which v1 skips and v2 keys, is `token`. Built by
    /// hand: no planner document expands to bodies that differ only there.
    fn token_body(token: &str) -> serde_json::Value {
        serde_json::json!({
            "requires": ["os:macos"],
            "spec": {"benchmark": "decode_throughput_512_100"},
            "auth_token": token,
        })
    }

    /// A v1 digest rekeys to its body's v2 digest, and to none when bodies
    /// that differ under v2 share it.
    #[rstest::rstest]
    #[case::one_body(&["a"], true)]
    #[case::two_alike(&["a", "a"], true)]
    #[case::two_apart(&["a", "b"], false)]
    fn a_v1_digest_rekeys_only_to_one_body(#[case] tokens: &[&str], #[case] rekeys: bool) {
        let bodies: Vec<serde_json::Value> = tokens.iter().copied().map(token_body).collect();
        let expected = rekeys.then(|| content_digest(&token_body("a")));
        assert_eq!(
            rekeyed(&bodies).get(&legacy_content_digest(&token_body("a"))),
            expected.as_ref()
        );
    }

    /// A head that has ended, or is ending, is revised all the same, and the
    /// edit writes no cancel marker: an ended plan has nothing left to stop,
    /// and the pass owns a latched teardown.
    #[rstest::rstest]
    #[case::cancelling(pipette_plan_types::PlanStatus::Cancelling)]
    #[case::cancelled(pipette_plan_types::PlanStatus::Cancelled)]
    #[case::complete(pipette_plan_types::PlanStatus::Complete)]
    #[tokio::test]
    async fn a_variant_edit_of_an_ending_plan_requests_no_cancel(
        #[case] status: pipette_plan_types::PlanStatus,
    ) -> anyhow::Result<()> {
        let (dir, stores) = stores()?;
        let (campaign, first) = created(&stores, dir.path()).await?;
        let mut manifest = stores
            .plans
            .get_plan(&first.plan_id)
            .await?
            .context("the first manifest")?;
        manifest.status = status;
        stores.plans.put_plan(&manifest).await?;

        let (_, revised) = edit_variants(
            &stores,
            settings(ModelCheck::none()),
            &campaign.campaign_id,
            &first.plan_id,
            &VariantEdit::parse("remove = [0]")?,
            None,
            Offer::default(),
        )
        .await?;

        assert!(!revised.cancel_requested);
        assert!(!stores.plans.has_cancel_marker(&first.plan_id).await?);
        Ok(())
    }

    /// File `revision` as the CLI does, and nothing after it.
    async fn file_revision(stores: &Stores, revision: &Revision) -> anyhow::Result<IngestReport> {
        let catalog = crate::org_catalog::load_server_catalog(&*stores.catalog).await?;
        let ctx = IngestContext::from_stores(
            stores,
            &catalog,
            ModelCheck::none(),
            MAX_JOBS,
            CREATING_MAX_AGE,
        );
        plan_cli::ingest(
            &ctx,
            PlanInput::Text(revision.document()),
            None,
            None,
            revision.membership(&*stores.plans).await?,
        )
        .await
    }

    /// The loser of two racing edits cancels its own plan and leaves the
    /// campaign on the winner's. A cancel that fails says so.
    #[rstest::rstest]
    #[case::cancelled(false)]
    #[case::cancel_fails(true)]
    #[tokio::test]
    async fn a_variant_edit_that_loses_a_race_cancels_its_own_plan(
        #[case] cancel_fails: bool,
    ) -> anyhow::Result<()> {
        let (dir, stores) = stores()?;
        let (campaign, first) = created(&stores, dir.path()).await?;
        let edit = VariantEdit::parse("remove = [0]")?;
        let revision = revise(&stores, campaign.clone(), Some(&first.plan_id), &edit).await?;

        let winner = PlanId::try_new("plan-winner")?;
        advance(
            &*stores.campaigns,
            &*stores.plans,
            &campaign.campaign_id,
            winner.clone(),
        )
        .await?;

        let report = file_revision(&stores, &revision).await?;
        let plans: std::sync::Arc<dyn PlanStore> = if cancel_fails {
            std::sync::Arc::new(CancelFails(stores.plans.clone()))
        } else {
            stores.plans.clone()
        };
        let outcome = complete_revision(
            &*stores.campaigns,
            &*plans,
            revision,
            &report.plan_id,
            &report.job_ids,
        )
        .await;

        assert!(
            matches!(
                &outcome,
                Err(CampaignAdminError::Overtaken { plan_id, cancelled, .. })
                    if *plan_id == report.plan_id && *cancelled == !cancel_fails
            ),
            "{outcome:?}"
        );
        let head = require(&*stores.campaigns, &campaign.campaign_id).await?;
        assert_eq!(head.current_plan_id, Some(winner));
        assert_eq!(
            stores.plans.has_cancel_marker(&report.plan_id).await?,
            !cancel_fails
        );
        assert!(!stores.plans.has_cancel_marker(&first.plan_id).await?);
        Ok(())
    }

    /// The revision imported for the old owner, so it cancels its own plan.
    #[tokio::test]
    async fn a_revision_whose_campaign_changed_its_owner_cancels_its_own_plan() -> anyhow::Result<()>
    {
        let (dir, stores) = stores()?;
        let (campaign, first) = created(&stores, dir.path()).await?;
        let edit = VariantEdit::parse("remove = [0]")?;
        let revision = revise(&stores, campaign.clone(), Some(&first.plan_id), &edit).await?;
        let report = file_revision(&stores, &revision).await?;

        let handed = Campaign {
            owner: CampaignOwner::User {
                user_id: UserId::try_new("user_heir")?,
            },
            ..require(&*stores.campaigns, &campaign.campaign_id).await?
        };
        stores.campaigns.put_campaign(&handed).await?;
        let outcome = complete_revision(
            &*stores.campaigns,
            &*stores.plans,
            revision,
            &report.plan_id,
            &report.job_ids,
        )
        .await;

        assert!(
            matches!(
                &outcome,
                Err(CampaignAdminError::Overtaken { plan_id, cancelled: true, .. })
                    if *plan_id == report.plan_id
            ),
            "{outcome:?}"
        );
        let head = require(&*stores.campaigns, &campaign.campaign_id).await?;
        assert_eq!(head.current_plan_id, Some(first.plan_id.clone()));
        assert!(stores.plans.has_cancel_marker(&report.plan_id).await?);
        assert!(!stores.plans.has_cancel_marker(&first.plan_id).await?);
        Ok(())
    }

    /// The campaign write fails after the revision is filed. The campaign stays
    /// on its plan, and neither plan is cancelled: nothing the edit kept stops,
    /// and the error names the revision for an operator to stop.
    #[tokio::test]
    async fn a_variant_edit_that_cannot_advance_leaves_both_plans_running() -> anyhow::Result<()> {
        let (dir, mut stores) = stores()?;
        let (campaign, first) = created(&stores, dir.path()).await?;
        stores.campaigns = std::sync::Arc::new(PutFails(stores.campaigns.clone()));

        let outcome = edit_variants(
            &stores,
            settings(ModelCheck::none()),
            &campaign.campaign_id,
            &first.plan_id,
            &VariantEdit::parse("remove = [0]")?,
            None,
            Offer::default(),
        )
        .await;

        let Err(CampaignAdminError::NotAdvanced { plan_id, .. }) = outcome else {
            anyhow::bail!("expected NotAdvanced, got {outcome:?}");
        };
        assert_ne!(plan_id, first.plan_id);
        let revision = stores
            .plans
            .get_plan(&plan_id)
            .await?
            .context("the revision was filed")?;
        assert_eq!(revision.previous_plan_id, Some(first.plan_id.clone()));
        let head = require(&*stores.campaigns, &campaign.campaign_id).await?;
        assert_eq!(head.current_plan_id, Some(first.plan_id.clone()));
        assert!(!stores.plans.has_cancel_marker(&plan_id).await?);
        assert!(!stores.plans.has_cancel_marker(&first.plan_id).await?);
        Ok(())
    }

    /// The replaced plan's cancel fails after the campaign advanced. The
    /// campaign runs the revision, the replaced plan keeps running, and the
    /// error names it for an operator to stop.
    #[tokio::test]
    async fn a_variant_edit_whose_cancel_fails_leaves_both_plans_running() -> anyhow::Result<()> {
        let (dir, stores) = stores()?;
        let (campaign, first) = created(&stores, dir.path()).await?;
        let edit = VariantEdit::parse("remove = [0]")?;
        let revision = revise(&stores, campaign.clone(), Some(&first.plan_id), &edit).await?;
        let report = file_revision(&stores, &revision).await?;

        let outcome = complete_revision(
            &*stores.campaigns,
            &CancelFails(stores.plans.clone()),
            revision,
            &report.plan_id,
            &report.job_ids,
        )
        .await;

        assert!(
            matches!(
                &outcome,
                Err(CampaignAdminError::NotCancelled { plan_id, superseded, .. })
                    if *plan_id == report.plan_id && *superseded == first.plan_id
            ),
            "{outcome:?}"
        );
        let head = require(&*stores.campaigns, &campaign.campaign_id).await?;
        assert_eq!(head.current_plan_id, Some(report.plan_id.clone()));
        assert!(!stores.plans.has_cancel_marker(&first.plan_id).await?);
        Ok(())
    }

    /// A job running when the edit lands keeps running: the revision carries
    /// it, and the replaced plan's teardown releases its lease rather than
    /// retiring it.
    #[tokio::test]
    async fn a_variant_edit_keeps_a_running_job_running() -> anyhow::Result<()> {
        let (dir, stores) = stores()?;
        let (campaign, first) = created(&stores, dir.path()).await?;
        let running = queued_requiring(&stores, &first, "os:macos")
            .await?
            .first()
            .cloned()
            .context("a macOS job")?;
        let name = stores
            .todo
            .avail_key_for_job(&running)
            .await?
            .context("the job is queued")?;
        let (_, expires_at) = pipette_core::todo_filename::parse_avail_filename(&name)?;
        // A registered holder: maintenance recycles a lease whose client the
        // roster does not know, which would read as the edit stopping the job.
        let holder = pipette_core::client::Client {
            client_id: pipette_plan_types::ClientId::try_new("ev1_a")?,
            public_key: pipette_plan_types::PublicKeyHex::try_new(
                "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789",
            )?,
            organization: NonEmptyTrimmedString::try_new("lab")?,
            client_details: NonEmptyTrimmedString::try_new("mac mini")?,
            contact_email: ContactEmail::try_new("lab@example.test")?,
            status: pipette_core::client::ClientStatus::Approved,
            registered_at: Utc::now(),
            device_profile: Default::default(),
            capabilities: Default::default(),
        };
        stores.clients.put_client(&holder).await?;
        let claimed = stores
            .todo
            .claim_job(
                &running,
                expires_at,
                &holder.client_id,
                Utc::now() + chrono::Duration::hours(1),
            )
            .await?;
        assert!(matches!(claimed, pipette_store::ClaimResult::Claimed(_)));

        let (report, revised) = edit_variants(
            &stores,
            settings(ModelCheck::none()),
            &campaign.campaign_id,
            &first.plan_id,
            &VariantEdit::parse("remove = [0]")?,
            None,
            Offer::default(),
        )
        .await?;
        assert!(revised.cancel_requested);
        maintain(&stores).await?;
        maintain(&stores).await?;

        let leased: Vec<JobId> = stores
            .todo
            .list_leased()
            .await?
            .iter()
            .filter_map(|key| pipette_core::todo_filename::parse_leased_key(key).ok())
            .map(|lease| lease.job_id)
            .collect();
        assert_eq!(leased, vec![running.clone()]);
        let members: Vec<PlanId> = stores
            .plans
            .list_job_plan_markers(&running)
            .await?
            .into_iter()
            .map(|(plan_id, _)| plan_id)
            .collect();
        assert_eq!(members, vec![report.plan_id.clone()]);
        let replaced = stores
            .plans
            .get_plan(&first.plan_id)
            .await?
            .context("the replaced manifest")?;
        assert_eq!(replaced.status, pipette_plan_types::PlanStatus::Cancelled);
        Ok(())
    }

    /// A finished job carries through a chain of edits. Each revision records
    /// the job's digest, so the next one finds it though the job left the queue
    /// two plans back, and the document, re-serialized twice, still expands to
    /// the same cell.
    #[tokio::test]
    async fn a_finished_job_carries_through_a_chain_of_edits() -> anyhow::Result<()> {
        let (dir, stores) = stores()?;
        let (campaign, first) = created(&stores, dir.path()).await?;
        let done = queued_requiring(&stores, &first, "os:macos")
            .await?
            .first()
            .cloned()
            .context("a macOS job")?;
        stores
            .submissions
            .write_processed(&done, &serde_json::json!({"job_id": done.as_str()}))
            .await?;
        stores.todo.delete_avail_by_job(&done).await?;

        let (second, _) = edit_variants(
            &stores,
            settings(ModelCheck::none()),
            &campaign.campaign_id,
            &first.plan_id,
            &VariantEdit::parse("remove = [0]")?,
            None,
            Offer::default(),
        )
        .await?;
        let (third, revised) = edit_variants(
            &stores,
            settings(ModelCheck::none()),
            &campaign.campaign_id,
            &second.plan_id,
            &VariantEdit::parse(MLX_700M_VARIANT)?,
            None,
            Offer::default(),
        )
        .await?;

        let holds =
            |report: &IngestReport| report.job_ids.iter().any(|(_, job_id)| *job_id == done);
        assert!(holds(&second) && holds(&third));
        assert_eq!(revised.superseded, second.plan_id);
        assert_eq!(
            (revised.carried_job_count, revised.dropped_job_count),
            (2, 0)
        );
        // Neither revision marks the finished job: nothing is left of it to lapse.
        let members: Vec<PlanId> = stores
            .plans
            .list_job_plan_markers(&done)
            .await?
            .into_iter()
            .map(|(plan_id, _)| plan_id)
            .collect();
        assert_eq!(members, vec![first.plan_id.clone()]);

        maintain(&stores).await?;
        maintain(&stores).await?;
        let counts = stores
            .plans
            .get_plan(&third.plan_id)
            .await?
            .and_then(|manifest| manifest.progress_snapshot)
            .context("the last revision's snapshot")?
            .counts;
        assert_eq!((counts.finished, counts.available), (1, 2));
        Ok(())
    }

    /// The other two finished jobs carry and the named cell queues anew. A head
    /// filed under v1, sources stored or not, re-runs alike, not as drifted.
    #[rstest::rstest]
    #[case::current(None)]
    #[case::v1_stored(Some(false))]
    #[case::v1_unstored(Some(true))]
    #[tokio::test]
    async fn a_rerun_runs_one_cell_again_and_keeps_the_rest(
        #[case] v1_unstored: Option<bool>,
    ) -> anyhow::Result<()> {
        let (dir, stores) = stores()?;
        let (campaign, first) = created(&stores, dir.path()).await?;
        let jobs: Vec<JobId> = first
            .job_ids
            .iter()
            .map(|(_, job_id)| job_id.clone())
            .collect();
        futures::stream::iter(&jobs)
            .map(Ok)
            .try_for_each(|job_id| {
                pipette_store_testkit::finish_job(&*stores.submissions, &*stores.todo, job_id)
            })
            .await?;
        if let Some(unstored) = v1_unstored {
            as_filed_under_v1(&stores, dir.path(), &first.plan_id, unstored).await?;
        }
        let again = jobs[0].clone();

        let (report, revised) = rerun_jobs(
            &stores,
            settings(ModelCheck::none()),
            &campaign.campaign_id,
            BTreeSet::from([again.clone()]),
            Some("nightly-rerun".to_string()),
        )
        .await?;

        assert_eq!(
            revised,
            Revised {
                superseded: first.plan_id.clone(),
                carried_job_count: 2,
                rerun_job_count: 1,
                recalculated_job_count: 0,
                dropped_job_count: 0,
                cancel_requested: true,
            }
        );
        let held: BTreeSet<JobId> = report
            .job_ids
            .iter()
            .map(|(_, job_id)| job_id.clone())
            .collect();
        assert_eq!(held.len(), 3);
        assert!(!held.contains(&again));
        let fresh: Vec<&JobId> = held
            .iter()
            .filter(|job_id| !jobs.contains(job_id))
            .collect();
        assert_eq!(fresh.len(), 1);
        assert!(stores.todo.get_avail_by_job(fresh[0]).await?.is_some());

        let head = require(&*stores.campaigns, &campaign.campaign_id).await?;
        assert_eq!(head.current_plan_id, Some(report.plan_id.clone()));
        let source = stores
            .plans
            .get_plan_source(&report.plan_id)
            .await?
            .context("the revision's document")?;
        assert_eq!(
            source.text(),
            include_str!("../../../examples/plans/scheduler/afm-mlx.toml"),
            "a re-run files the document it re-runs as it is"
        );

        maintain(&stores).await?;
        maintain(&stores).await?;
        let replaced = stores
            .plans
            .get_plan(&first.plan_id)
            .await?
            .context("the replaced manifest")?;
        assert_eq!(replaced.status, pipette_plan_types::PlanStatus::Cancelled);
        assert!(stores.todo.get_avail_by_job(fresh[0]).await?.is_some());
        let counts = stores
            .plans
            .get_plan(&report.plan_id)
            .await?
            .and_then(|manifest| manifest.progress_snapshot)
            .context("the revision's snapshot")?
            .counts;
        assert_eq!((counts.finished, counts.available), (2, 1));
        Ok(())
    }

    /// A re-run names jobs of the head with a recorded result, and files nothing
    /// otherwise. A lapse for the head is not a result: the job may still run
    /// for another plan.
    #[tokio::test]
    async fn a_rerun_refuses_a_job_it_cannot_run_again() -> anyhow::Result<()> {
        let (dir, stores) = stores()?;
        let (campaign, first) = created(&stores, dir.path()).await?;
        let queued = first.job_ids[0].1.clone();
        let rerun = |job_ids: BTreeSet<JobId>| {
            rerun_jobs(
                &stores,
                settings(ModelCheck::none()),
                &campaign.campaign_id,
                job_ids,
                None,
            )
        };

        let empty = rerun(BTreeSet::new()).await;
        assert!(
            matches!(empty, Err(CampaignAdminError::NothingToRerun)),
            "{empty:?}"
        );
        let stranger = JobId::try_new("job-0199aaaa-0000-7000-8000-000000000000")?;
        let elsewhere = rerun(BTreeSet::from([stranger.clone()])).await;
        assert!(
            matches!(&elsewhere, Err(CampaignAdminError::NotInPlan { job_id, .. }) if *job_id == stranger),
            "{elsewhere:?}"
        );
        let unanswered = rerun(BTreeSet::from([queued.clone()])).await;
        assert!(
            matches!(&unanswered, Err(CampaignAdminError::Unanswered(job_id)) if *job_id == queued),
            "{unanswered:?}"
        );
        assert_eq!(
            pipette_store::drain_plans(&*stores.plans, None)
                .await?
                .items
                .len(),
            1,
            "none of the three filed a plan"
        );

        let mut manifest = stores
            .plans
            .get_plan(&first.plan_id)
            .await?
            .context("the first manifest")?;
        manifest.expired_jobs.insert(
            queued.clone(),
            pipette_core::plan::ExpiredJob {
                horizon: Utc::now() - chrono::Duration::days(1),
                recorded_at: Utc::now(),
            },
        );
        stores.plans.put_plan(&manifest).await?;
        let lapsed = rerun(BTreeSet::from([queued.clone()])).await;
        assert!(
            matches!(&lapsed, Err(CampaignAdminError::Unanswered(job_id)) if *job_id == queued),
            "{lapsed:?}"
        );

        pipette_store_testkit::finish_job(&*stores.submissions, &*stores.todo, &queued).await?;
        let (_, revised) = rerun(BTreeSet::from([queued])).await?;
        assert_eq!(revised.rerun_job_count, 1);
        Ok(())
    }

    /// A whole-plan re-run takes the head's answered jobs, or its failed ones
    /// alone, and never a job still queued.
    #[tokio::test]
    async fn a_whole_plan_rerun_takes_the_answered_jobs() -> anyhow::Result<()> {
        let (dir, stores) = stores()?;
        let (campaign, first) = created(&stores, dir.path()).await?;
        let jobs: Vec<JobId> = first
            .job_ids
            .iter()
            .map(|(_, job_id)| job_id.clone())
            .collect();
        let stores = &stores;
        let picked = |which| {
            let campaign = campaign.clone();
            async move {
                Head::read(&*stores.plans, campaign, None)
                    .await?
                    .rerun_answered(&*stores.plans, &*stores.submissions, which)
                    .await
                    .map(|revision| revision.rerun)
            }
        };

        let none = picked(Answered::All).await;
        assert!(
            matches!(
                &none,
                Err(CampaignAdminError::NoneAnswered {
                    which: Answered::All,
                    ..
                })
            ),
            "{none:?}"
        );

        pipette_store_testkit::finish_job(&*stores.submissions, &*stores.todo, &jobs[0]).await?;
        let failed = picked(Answered::Failed).await;
        assert!(
            matches!(
                &failed,
                Err(CampaignAdminError::NoneAnswered {
                    which: Answered::Failed,
                    ..
                })
            ),
            "{failed:?}"
        );

        stores
            .submissions
            .write_processed(
                &jobs[1],
                &serde_json::json!({"job_id": jobs[1].as_str(), "message_type": "failure"}),
            )
            .await?;
        stores.todo.delete_avail_by_job(&jobs[1]).await?;
        assert_eq!(
            picked(Answered::Failed).await?,
            BTreeSet::from([jobs[1].clone()])
        );
        assert_eq!(
            picked(Answered::All).await?,
            BTreeSet::from([jobs[0].clone(), jobs[1].clone()])
        );
        Ok(())
    }

    /// `campaigns rerun --dry-run` checks the jobs as a re-run does and answers
    /// the head's document and its bodies, and writes nothing.
    #[tokio::test]
    async fn a_dry_run_rerun_checks_the_jobs_and_writes_nothing() -> anyhow::Result<()> {
        let (dir, stores) = stores()?;
        let (campaign, first) = created(&stores, dir.path()).await?;
        let queued = first.job_ids[0].1.clone();
        let dry_run = |job_ids: BTreeSet<JobId>| {
            dry_run_rerun(
                &stores,
                settings(ModelCheck::none()),
                &campaign.campaign_id,
                job_ids,
                None,
            )
        };

        let unanswered = dry_run(BTreeSet::from([queued.clone()])).await;
        assert!(
            matches!(&unanswered, Err(CampaignAdminError::Unanswered(job_id)) if *job_id == queued),
            "{unanswered:?}"
        );
        pipette_store_testkit::finish_job(&*stores.submissions, &*stores.todo, &queued).await?;
        let report = dry_run(BTreeSet::from([queued.clone()])).await?;

        assert_eq!(
            report.document,
            include_str!("../../../examples/plans/scheduler/afm-mlx.toml")
        );
        assert_eq!(report.report.bodies.len(), first.job_ids.len());
        assert_eq!(
            (
                report.preview.carried_job_count,
                report.preview.new_job_count,
                report.preview.rerun_job_count,
                report.preview.dropped_job_ids.len()
            ),
            (first.job_ids.len() - 1, 0, 1, 0)
        );

        // The head records another digest for the cell, as if the catalog moved.
        let mut manifest = stores
            .plans
            .get_plan(&first.plan_id)
            .await?
            .context("the head")?;
        manifest.content_keys.insert(queued.clone(), "0".repeat(64));
        stores.plans.put_plan(&manifest).await?;
        let drifted = dry_run(BTreeSet::from([queued])).await;
        let Err(CampaignAdminError::Store(e)) = &drifted else {
            anyhow::bail!("a drifted dry run was not refused: {drifted:?}");
        };
        assert!(
            matches!(
                e.downcast_ref::<IngestError>(),
                Some(IngestError::Rejected(RejectionError::RerunDrift {
                    cells: 1
                }))
            ),
            "{e:#}"
        );

        let head = require(&*stores.campaigns, &campaign.campaign_id).await?;
        assert_eq!(head.current_plan_id, Some(first.plan_id.clone()));
        assert!(!stores.plans.has_cancel_marker(&first.plan_id).await?);
        assert_eq!(
            pipette_store::drain_plans(&*stores.plans, None)
                .await?
                .items
                .len(),
            1,
            "a dry run filed a plan"
        );
        Ok(())
    }

    /// What the routes run for a recalculation of `job_ids`: [`Head::recalculate`],
    /// ingest, [`complete_revision`].
    async fn recalculate(
        stores: &Stores,
        campaign: &Campaign,
        job_ids: BTreeSet<JobId>,
    ) -> Result<(IngestReport, Revised), CampaignAdminError> {
        let revision = Head::read(&*stores.plans, campaign.clone(), None)
            .await?
            .recalculate(&*stores.plans, &*stores.submissions, job_ids)
            .await?;
        let catalog = crate::org_catalog::load_server_catalog(&*stores.catalog).await?;
        let ctx = IngestContext::from_stores(
            stores,
            &catalog,
            ModelCheck::none(),
            MAX_JOBS,
            CREATING_MAX_AGE,
        );
        let membership = revision.membership(&*stores.plans).await?;
        let report = plan_cli::ingest(
            &ctx,
            PlanInput::Text(revision.document()),
            None,
            None,
            membership,
        )
        .await?;
        let revised = complete_revision(
            &*stores.campaigns,
            &*stores.plans,
            revision,
            &report.plan_id,
            &report.job_ids,
        )
        .await?;
        Ok((report, revised))
    }

    /// Record a success of `benchmark_type` for `job_id`, as a client that ran
    /// it would.
    async fn finish(stores: &Stores, job_id: &JobId, benchmark_type: &str) -> anyhow::Result<()> {
        pipette_store_testkit::finish_job_with_success(
            &*stores.submissions,
            &*stores.todo,
            job_id,
            benchmark_type,
        )
        .await
    }

    /// The named cell becomes an unclaimable new job answered by a copy of the
    /// named job's record; the other jobs carry over.
    #[tokio::test]
    async fn a_recalculation_scores_one_eval_again_and_keeps_the_rest() -> anyhow::Result<()> {
        let (dir, stores) = stores()?;
        let (campaign, first) = created(&stores, dir.path()).await?;
        let jobs: Vec<JobId> = first
            .job_ids
            .iter()
            .map(|(_, job_id)| job_id.clone())
            .collect();
        futures::stream::iter(&jobs)
            .map(Ok)
            .try_for_each(|job_id| finish(&stores, job_id, "eval"))
            .await?;
        let again = jobs[0].clone();
        let before = Utc::now();

        let (report, revised) =
            recalculate(&stores, &campaign, BTreeSet::from([again.clone()])).await?;

        assert_eq!(
            revised,
            Revised {
                superseded: first.plan_id.clone(),
                carried_job_count: 2,
                rerun_job_count: 0,
                recalculated_job_count: 1,
                dropped_job_count: 0,
                cancel_requested: true,
            }
        );
        let held: BTreeSet<JobId> = report
            .job_ids
            .iter()
            .map(|(_, job_id)| job_id.clone())
            .collect();
        assert_eq!(held.len(), 3);
        assert!(!held.contains(&again));
        let fresh: Vec<&JobId> = held
            .iter()
            .filter(|job_id| !jobs.contains(job_id))
            .collect();
        let [fresh] = fresh.as_slice() else {
            anyhow::bail!("not one new job: {fresh:?}");
        };
        assert!(stores.todo.get_avail_by_job(fresh).await?.is_none());
        let copy = stores
            .submissions
            .find_job(fresh)
            .await?
            .context("the new job's record")?;
        assert_eq!(copy.state, pipette_store::JobState::Incoming);
        assert_eq!(copy.body["job_id"], fresh.as_str());
        assert_eq!(copy.body["rescored_from"], again.as_str());
        assert_eq!(
            copy.body["completions"],
            serde_json::json!([{"id": "s1", "completion": "@@@@"}])
        );
        let submitted_at: chrono::DateTime<Utc> =
            serde_json::from_value(copy.body["submitted_at"].clone())?;
        assert!(submitted_at >= before, "{submitted_at}");
        // Filed now, the copy still records the time of the run it copies.
        assert_eq!(
            copy.body["run_submitted_at"],
            pipette_store_testkit::success_record(&again, "eval")["submitted_at"]
        );
        let kept = stores
            .submissions
            .find_job(&again)
            .await?
            .context("the named job's record")?;
        assert_eq!(
            kept.body,
            pipette_store_testkit::success_record(&again, "eval")
        );

        let head = require(&*stores.campaigns, &campaign.campaign_id).await?;
        assert_eq!(head.current_plan_id, Some(report.plan_id.clone()));
        maintain(&stores).await?;
        maintain(&stores).await?;
        let replaced = stores
            .plans
            .get_plan(&first.plan_id)
            .await?
            .context("the replaced manifest")?;
        assert_eq!(replaced.status, pipette_plan_types::PlanStatus::Cancelled);
        let counts = stores
            .plans
            .get_plan(&report.plan_id)
            .await?
            .and_then(|manifest| manifest.progress_snapshot)
            .context("the revision's snapshot")?
            .counts;
        assert_eq!((counts.finished, counts.available), (3, 0));
        Ok(())
    }

    /// What the named job recorded before
    /// [`a_recalculation_refuses_a_job_with_no_eval_completions`] names it.
    #[derive(Clone, Copy, Debug)]
    enum Recorded {
        Nothing,
        Failure,
        OtherBenchmark,
    }

    /// Only an eval's success holds completions to score again; naming any other
    /// job files nothing, and a whole-head recalculation skips it.
    #[rstest::rstest]
    #[case::unanswered(Recorded::Nothing)]
    #[case::failed(Recorded::Failure)]
    #[case::not_an_eval(Recorded::OtherBenchmark)]
    #[tokio::test]
    async fn a_recalculation_refuses_a_job_with_no_eval_completions(
        #[case] recorded: Recorded,
    ) -> anyhow::Result<()> {
        let (dir, stores) = stores()?;
        let (campaign, first) = created(&stores, dir.path()).await?;
        let job_id = first.job_ids[0].1.clone();
        match recorded {
            Recorded::Nothing => {}
            Recorded::Failure => {
                pipette_store_testkit::finish_job_with_failure(
                    &*stores.submissions,
                    &*stores.todo,
                    &job_id,
                )
                .await?;
            }
            Recorded::OtherBenchmark => finish(&stores, &job_id, "decode_throughput").await?,
        }

        let named = recalculate(&stores, &campaign, BTreeSet::from([job_id.clone()])).await;
        let whole = Head::read(&*stores.plans, campaign.clone(), None)
            .await?
            .recalculate_evals(&*stores.plans, &*stores.submissions)
            .await;

        match recorded {
            Recorded::Nothing => assert!(
                matches!(&named, Err(CampaignAdminError::Unanswered(id)) if *id == job_id),
                "{named:?}"
            ),
            Recorded::Failure | Recorded::OtherBenchmark => assert!(
                matches!(&named, Err(CampaignAdminError::NotRescorable(id)) if *id == job_id),
                "{named:?}"
            ),
        }
        assert!(
            matches!(&whole, Err(CampaignAdminError::NoneRescorable(plan_id)) if *plan_id == first.plan_id),
            "{whole:?}"
        );
        assert_eq!(
            pipette_store::drain_plans(&*stores.plans, None)
                .await?
                .items
                .len(),
            1,
            "nothing filed a plan"
        );
        Ok(())
    }

    /// A recalculation that names no job is refused before the head is read.
    #[tokio::test]
    async fn a_recalculation_of_no_job_is_refused() -> anyhow::Result<()> {
        let (dir, stores) = stores()?;
        let (campaign, _) = created(&stores, dir.path()).await?;

        let empty = recalculate(&stores, &campaign, BTreeSet::new()).await;

        assert!(
            matches!(empty, Err(CampaignAdminError::NothingToRerun)),
            "{empty:?}"
        );
        Ok(())
    }

    /// A whole-head recalculation takes the jobs whose record is an eval's
    /// success, and leaves a failure and another benchmark's result alone.
    #[tokio::test]
    async fn a_whole_head_recalculation_takes_only_the_finished_evals() -> anyhow::Result<()> {
        let (dir, stores) = stores()?;
        let (campaign, first) = created(&stores, dir.path()).await?;
        let [eval, failed, measured] = [0, 1, 2].map(|i| first.job_ids[i].1.clone());
        finish(&stores, &eval, "eval").await?;
        pipette_store_testkit::finish_job_with_failure(
            &*stores.submissions,
            &*stores.todo,
            &failed,
        )
        .await?;
        finish(&stores, &measured, "decode_throughput").await?;

        let picked = Head::read(&*stores.plans, campaign.clone(), None)
            .await?
            .recalculate_evals(&*stores.plans, &*stores.submissions)
            .await?;

        assert_eq!(picked.recalculate, BTreeSet::from([eval]));
        Ok(())
    }

    /// `campaigns variants` with no edit lists the head's variants numbered
    /// from 0, and `--dry-run` answers the revised document and its bodies.
    /// Neither writes anything.
    #[tokio::test]
    async fn the_cli_lists_and_dry_runs_an_edit_without_writing() -> anyhow::Result<()> {
        let (dir, stores) = stores()?;
        let (campaign, first) = created(&stores, dir.path()).await?;

        let (plan_id, listing) = current_variants(&stores, &campaign.campaign_id).await?;
        assert_eq!(plan_id, first.plan_id);
        assert!(listing.contains("# variant 0\n"), "{listing}");
        assert!(listing.contains("# variant 1\n"), "{listing}");
        assert!(!listing.contains("# variant 2"), "{listing}");

        let dry_run = dry_run_variants(
            &stores,
            settings(ModelCheck::none()),
            &campaign.campaign_id,
            &first.plan_id,
            &VariantEdit::parse(&format!("remove = [0]\n{MLX_700M_VARIANT}"))?,
            None,
            Offer::default(),
        )
        .await?;
        assert!(dry_run.document.contains("LFM2-700M-MLX-4bit"));
        assert_eq!(dry_run.report.bodies.len(), 3);
        let ios = queued_requiring(&stores, &first, "os:ios").await?;
        assert_eq!(
            (
                dry_run.preview.carried_job_count,
                dry_run.preview.new_job_count,
                dry_run.preview.dropped_job_ids
            ),
            (2, 1, ios)
        );

        let head = require(&*stores.campaigns, &campaign.campaign_id).await?;
        assert_eq!(head.current_plan_id, Some(first.plan_id.clone()));
        assert!(!stores.plans.has_cancel_marker(&first.plan_id).await?);
        assert_eq!(
            pipette_store::drain_plans(&*stores.plans, None)
                .await?
                .items
                .len(),
            1
        );
        Ok(())
    }

    /// A dry-run document revision expands the document as sent, and writes
    /// nothing: the campaign stays on its plan, which is not cancelled.
    #[tokio::test]
    async fn a_dry_run_document_revision_writes_nothing() -> anyhow::Result<()> {
        let (dir, stores) = stores()?;
        let (campaign, first) = created(&stores, dir.path()).await?;
        let (_, head) = head_document(&stores, &campaign.campaign_id).await?;
        let document = format!("# with the 700M model\n{head}{MLX_700M_VARIANT}");

        let dry_run = dry_run_revise_document(
            &stores,
            settings(ModelCheck::none()),
            &campaign.campaign_id,
            &first.plan_id,
            document.clone(),
            None,
            Offer::default(),
        )
        .await?;
        assert_eq!(dry_run.document, document);
        assert_eq!(dry_run.report.bodies.len(), 4);
        let unchanged = dry_run_revise_document(
            &stores,
            settings(ModelCheck::none()),
            &campaign.campaign_id,
            &first.plan_id,
            head,
            None,
            Offer::default(),
        )
        .await?;
        assert!(unchanged.preview.unchanged);
        assert!(unchanged.preview.dropped_job_ids.is_empty());
        assert_eq!(
            dry_run.preview,
            RevisionPreview {
                unchanged: false,
                carried_job_count: 3,
                new_job_count: 1,
                requirements_rerun_job_count: 0,
                satisfied_job_count: 0,
                restored_job_count: 0,
                inherited_job_count: 0,
                rerun_job_count: 0,
                recalculated_job_count: 0,
                dropped_job_ids: Vec::new(),
                ..dry_run.preview.clone()
            }
        );
        // The one body that holds no job is the variant the document adds.
        let listed: BTreeSet<usize> = dry_run
            .preview
            .held_jobs
            .iter()
            .map(|job| job.body_index)
            .collect();
        let new: Vec<&serde_json::Value> = (0..dry_run.report.bodies.len())
            .filter(|body| !listed.contains(body))
            .filter_map(|body| dry_run.report.bodies.get(body))
            .collect();
        assert!(
            matches!(new.as_slice(), [body] if body["spec"]["model"]["repo_name"] == "LFM2-700M-MLX-4bit"),
            "{new:?}"
        );
        assert!(
            dry_run
                .preview
                .held_jobs
                .iter()
                .all(|job| job.plan_id == first.plan_id && held(&first).contains(&job.job_id))
        );

        let after = require(&*stores.campaigns, &campaign.campaign_id).await?;
        assert_eq!(after.current_plan_id, Some(first.plan_id.clone()));
        assert!(!stores.plans.has_cancel_marker(&first.plan_id).await?);
        assert_eq!(
            pipette_store::drain_plans(&*stores.plans, None)
                .await?
                .items
                .len(),
            1
        );
        Ok(())
    }

    /// Each campaign an edit cannot revise is refused before anything is filed:
    /// one that moved off the plan the caller read, one whose plan has no
    /// document to edit, and one whose document fails its digest.
    #[tokio::test]
    async fn a_variant_edit_files_nothing_it_cannot_apply() -> anyhow::Result<()> {
        let (dir, stores) = stores()?;
        let (campaign, first) = created(&stores, dir.path()).await?;
        let edit = VariantEdit::parse("remove = [0]")?;

        let elsewhere = PlanId::try_new("plan-elsewhere")?;
        let moved = revise(&stores, campaign.clone(), Some(&elsewhere), &edit).await;
        assert!(
            matches!(&moved, Err(CampaignAdminError::NotOnPlan { current, .. }) if *current == first.plan_id),
            "{moved:?}"
        );

        // `plan-seed` has no manifest, as a plan filed as job bodies has no
        // document.
        let seeded = seed(&stores, "bodies").await?;
        let undocumented = revise(&stores, seeded, None, &edit).await;
        assert!(
            matches!(&undocumented, Err(CampaignAdminError::NoDocument(_))),
            "{undocumented:?}"
        );

        let out_of_range = VariantEdit::parse("remove = [5]")?;
        let refused = revise(&stores, campaign.clone(), None, &out_of_range).await;
        assert!(
            matches!(&refused, Err(CampaignAdminError::Edit(_))),
            "{refused:?}"
        );

        let mut manifest = stores
            .plans
            .get_plan(&first.plan_id)
            .await?
            .context("the first manifest")?;
        manifest.source_sha256 = Some(pipette_plan_types::Sha256::try_new("0".repeat(64))?);
        stores.plans.put_plan(&manifest).await?;
        let mismatched = revise(&stores, campaign, None, &edit).await;
        assert!(
            matches!(&mismatched, Err(CampaignAdminError::SourceMismatch(plan_id)) if *plan_id == first.plan_id),
            "{mismatched:?}"
        );
        // Only the first plan: none of the four filed one.
        assert_eq!(
            pipette_store::drain_plans(&*stores.plans, None)
                .await?
                .items
                .len(),
            1
        );
        Ok(())
    }

    /// `campaigns ingest --plan --dry-run` writes nothing and leaves the
    /// campaign where it is. An unknown campaign is still refused.
    #[tokio::test]
    async fn dry_run_into_leaves_the_campaign_alone() -> anyhow::Result<()> {
        let (dir, stores) = stores()?;
        let seeded = seed(&stores, "nightly").await?;
        let campaign_id = seeded.campaign_id.clone();
        let path = afm_mlx_plan(dir.path())?;

        let report = dry_run_into(
            &stores,
            settings(ModelCheck::none()),
            &campaign_id,
            &path,
            None,
        )
        .await?;
        assert_eq!(report.bodies.len(), 3);
        assert_eq!(
            require(&*stores.campaigns, &campaign_id)
                .await?
                .current_plan_id,
            seeded.current_plan_id
        );
        assert!(stores.todo.list_avail().await?.is_empty());
        assert_eq!(
            pipette_store_testkit::local_fs_job_record_count(dir.path()),
            0
        );

        let unknown = CampaignId::try_new("campaign_00000000-0000-0000-0000-000000000000")?;
        let refused =
            dry_run_into(&stores, settings(ModelCheck::none()), &unknown, &path, None).await;
        assert!(
            matches!(refused, Err(CampaignAdminError::NotFound(ref id)) if *id == unknown),
            "{refused:?}"
        );
        Ok(())
    }
}
