//! Campaigns: the stable id for work whose plan keeps being revised. A campaign
//! mints its plans and points at the head; the plans' `previous_plan_id` chain
//! is the history. It holds no jobs, and gates only new plans while archived.

use std::collections::BTreeSet;

use jiff::Timestamp;
use serde::{Deserialize, Serialize};

use pipette_plan_types::{
    CampaignGroupId, CampaignGroupPolicy, CampaignId, CampaignState, JobId, Labels,
    NonEmptyTrimmedString, PlanId, PolicyDeferral, PolicyKind, PrincipalOwner,
};

use crate::types::Mint;

/// Who a campaign belongs to, and therefore who may administer it. Read as it
/// stands; unlike a device's owner, never absent.
pub type CampaignOwner = PrincipalOwner;

/// A campaign — the body of `campaigns/{campaign_id}.json` in the `[storage]`
/// backend.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Campaign {
    pub campaign_id: CampaignId,
    /// What to print for a human; mutable, and nothing keys on it. No slug
    /// index: a campaign is reached from a listing or a link, so the id is the
    /// handle.
    pub name: NonEmptyTrimmedString,
    /// The organization that owns this, and therefore who may repoint it.
    pub owner: CampaignOwner,
    /// The plan being executed; `None` only on a record from before a campaign
    /// was created with its first plan. Moved only by [`advance`](Self::advance),
    /// so it never disagrees with the plan's `campaign_id`. A claim, not a proof:
    /// disclosure is decided where the plan is read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_plan_id: Option<PlanId>,
    /// What its readers group and filter it by. Nothing on the server reads a
    /// label: no route, grant or plan depends on one.
    #[serde(default, skip_serializing_if = "Labels::is_empty")]
    pub labels: Labels,
    /// Absent while `active`, so a record nobody archived reads as it did before.
    #[serde(default, skip_serializing_if = "CampaignState::is_active")]
    pub state: CampaignState,
    /// Why the campaign is locked; set only while `locked`, by
    /// [`set_state`](Self::set_state).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state_note: Option<NonEmptyTrimmedString>,
    pub created_at: Timestamp,
    /// When any field here last changed: a rename, new labels, a state change,
    /// or a hand-off to another owner. Equal to `created_at` on a fresh record.
    pub updated_at: Timestamp,
}

impl From<Campaign> for pipette_plan_types::Campaign {
    fn from(campaign: Campaign) -> Self {
        Self {
            campaign_id: campaign.campaign_id,
            name: campaign.name,
            owner: campaign.owner,
            current_plan_id: campaign.current_plan_id,
            labels: campaign.labels,
            state: campaign.state,
            state_note: campaign.state_note,
            created_at: campaign.created_at,
            updated_at: campaign.updated_at,
        }
    }
}

/// A campaign being created: the id exists before its first plan, which records
/// it, and [`around`](Self::around) needs that plan to build the record.
///
/// Not `Clone`: a copy could build a second record under the same minted id.
#[derive(Debug)]
pub struct NewCampaign {
    campaign_id: CampaignId,
    name: NonEmptyTrimmedString,
    owner: CampaignOwner,
}

impl NewCampaign {
    /// Minted here so no caller can choose an id that collides, the rule
    /// [`crate::organization::Organization::new`] follows.
    pub fn new(name: NonEmptyTrimmedString, owner: CampaignOwner) -> Self {
        Self {
            campaign_id: CampaignId::mint(),
            name,
            owner,
        }
    }

    /// The id the first plan is filed under.
    pub fn campaign_id(&self) -> &CampaignId {
        &self.campaign_id
    }

    pub fn owner(&self) -> &CampaignOwner {
        &self.owner
    }

    /// The campaign, running `plan_id`: the plan just filed under
    /// [`campaign_id`](Self::campaign_id). Unchecked here; a plan filed anywhere
    /// else would gain a second campaign.
    pub fn around(self, plan_id: PlanId, created_at: Timestamp) -> Campaign {
        Campaign {
            campaign_id: self.campaign_id,
            name: self.name,
            owner: self.owner,
            current_plan_id: Some(plan_id),
            labels: Labels::default(),
            state: CampaignState::Active,
            state_note: None,
            created_at,
            updated_at: created_at,
        }
    }
}

impl Campaign {
    /// Move onto `plan_id`, the plan ingestion just filed into this campaign;
    /// nothing hands this an arbitrary plan. Re-advancing onto the current plan,
    /// as a retried ingest does, is a no-op.
    pub fn advance(&mut self, plan_id: PlanId, at: Timestamp) -> bool {
        if self.current_plan_id.as_ref() == Some(&plan_id) {
            return false;
        }
        self.current_plan_id = Some(plan_id);
        self.updated_at = at;
        true
    }

    /// Rename the campaign, returning whether anything changed. Renaming to the
    /// current name writes nothing and keeps `updated_at`, so a retry is safe.
    pub fn rename(&mut self, name: NonEmptyTrimmedString, at: Timestamp) -> bool {
        if self.name == name {
            return false;
        }
        self.name = name;
        self.updated_at = at;
        true
    }

    /// Replace the campaign's labels with `labels`, on the same no-op rule.
    pub fn set_labels(&mut self, labels: Labels, at: Timestamp) -> bool {
        if self.labels == labels {
            return false;
        }
        self.labels = labels;
        self.updated_at = at;
        true
    }

    /// Lock, archive or reactivate the campaign, on the same no-op rule.
    /// `note` is kept only with `locked`; any other state clears it.
    pub fn set_state(
        &mut self,
        state: CampaignState,
        note: Option<NonEmptyTrimmedString>,
        at: Timestamp,
    ) -> bool {
        let note = note.filter(|_| state == CampaignState::Locked);
        if self.state == state && self.state_note == note {
            return false;
        }
        self.state = state;
        self.state_note = note;
        self.updated_at = at;
        true
    }

    /// Hand the campaign to a new owner, on the same no-op rule. Keeps the
    /// pointer: a team taking over inherits the work mid-flight.
    pub fn set_owner(&mut self, owner: CampaignOwner, at: Timestamp) -> bool {
        if self.owner == owner {
            return false;
        }
        self.owner = owner;
        self.updated_at = at;
        true
    }
}

/// Per-device campaigns listed together, so they can be checked against each
/// other: the body of a `campaign_groups` row. The members are rows of their own.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CampaignGroup {
    pub group_id: CampaignGroupId,
    /// What to print for a human; nothing keys on it.
    pub name: NonEmptyTrimmedString,
    /// The owner of its member campaigns: the routes add only its campaigns and a
    /// hand-off takes a campaign out (`campaign_admin::leave_group`). The store
    /// checks neither.
    pub owner: CampaignOwner,
    pub created_at: Timestamp,
    /// When the name last changed; `created_at` on a fresh record.
    pub updated_at: Timestamp,
}

impl CampaignGroup {
    /// Minted here so no caller can choose an id that collides.
    pub fn new(name: NonEmptyTrimmedString, owner: CampaignOwner, at: Timestamp) -> Self {
        Self {
            group_id: CampaignGroupId::mint(),
            name,
            owner,
            created_at: at,
            updated_at: at,
        }
    }

    /// Returns whether anything changed; the current name writes nothing and keeps `updated_at`.
    pub fn rename(&mut self, name: NonEmptyTrimmedString, at: Timestamp) -> bool {
        if self.name == name {
            return false;
        }
        self.name = name;
        self.updated_at = at;
        true
    }
}

impl From<CampaignGroup> for pipette_plan_types::CampaignGroup {
    fn from(group: CampaignGroup) -> Self {
        Self {
            group_id: group.group_id,
            name: group.name,
            owner: group.owner,
            created_at: group.created_at,
            updated_at: group.updated_at,
        }
    }
}

/// What queue-maintenance's policy pass keeps for one member campaign of a
/// group: the JSON of its `campaign_group_members` row's `policy_state`. The
/// row, and this with it, goes when the campaign leaves the group. The default
/// is a row the pass has not written.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct GroupPolicyState {
    /// The acting policies, as read, when the member's [`JobPolicyState`]s,
    /// `triggered` and `refusal` were written.
    pub policies: Vec<CampaignGroupPolicy>,
    /// The kinds of `policies` that flag a job of the head, or whose re-runs
    /// filed into the campaign wait for their rows; the others are cleared.
    pub triggered: BTreeSet<PolicyKind>,
    /// The pass's last filing into the campaign.
    pub filed: Option<GroupFiling>,
    /// The head whose filing was refused, held while the build stays the same.
    pub refusal: Option<Refusal>,
    /// When pipette-duckdb last failed on the member's rows, which takes it
    /// behind the others in the next round; cleared once they read.
    pub unreadable_at: Option<Timestamp>,
    /// The version of the pass's rules for what a policy does with a job it
    /// flags that the job states were kept under; 0 on a state kept before the
    /// pass recorded one. Under another, the pass reads every answered job
    /// again.
    #[serde(default)]
    pub rules: u32,
    /// The deferral from the last evaluation that updated this state; dropped
    /// with `policies` once the group has no policy on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deferred: Option<PolicyDeferral>,
}

/// What the policy pass found of one job of a member's head under the member's
/// `policies`: the JSON of a `campaign_group_job_states` row, which goes when
/// the campaign leaves the group.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobPolicyState {
    pub job_id: JobId,
    /// The checks of the acting policies that flag the job.
    pub flagged: BTreeSet<PolicyKind>,
    /// The pass is done with the job: kept, or left for good. A job it re-runs,
    /// marks or skips until the next read is read again each round.
    pub decided: bool,
    /// The recorded models of the job's rows that resolve to no model params
    /// entry, which each round checks against the table again.
    pub missing_params: BTreeSet<RecordedModel>,
    /// The job's family and sizes, when smaller_failed acts.
    pub family: Option<(String, Vec<i64>)>,
    /// A row of the job failed.
    pub failed: bool,
}

/// A result row's `model_descriptor` and `model_vault_descriptor`, by which
/// the model params table resolves it.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct RecordedModel {
    pub descriptor: Option<String>,
    pub vault_descriptor: Option<String>,
}

/// A revision the policy pass filed into a member campaign.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupFiling {
    pub plan_id: PlanId,
    /// The jobs the filed plan holds that the plan it replaced did not: its
    /// re-runs, its marks, and the lapsed cells it placed afresh.
    pub queued: BTreeSet<JobId>,
    /// The filing ended in `CommitUnknown`, so only an operator may cancel the
    /// plan.
    pub commit_unknown: bool,
    /// With `unreadable_at`, when the pass last filed into or failed to read the
    /// member: a round takes the longest ago first.
    pub filed_at: Timestamp,
}

/// A head the policy pass could not file again, and why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Refusal {
    pub plan_id: PlanId,
    pub reason: String,
    /// [`crate::BUILD_VERSION`] when recorded: a new release may fix the cause.
    pub build: String,
}

#[cfg(test)]
mod tests {
    use anyhow::Context;
    use jiff::Timestamp;
    use rstest::rstest;

    use pipette_plan_types::OrgId;

    use super::*;

    fn a_plan() -> PlanId {
        PlanId::from_uuid(uuid::Uuid::now_v7())
    }

    fn a_new_campaign() -> anyhow::Result<NewCampaign> {
        Ok(NewCampaign::new(
            NonEmptyTrimmedString::try_new("Nightly LFM2 regression")?,
            CampaignOwner::Org {
                org_id: OrgId::mint(),
            },
        ))
    }

    fn a_campaign() -> anyhow::Result<Campaign> {
        Ok(a_new_campaign()?.around(a_plan(), Timestamp::now()))
    }

    /// A campaign is created around its first plan, under the id that plan was
    /// filed with.
    #[test]
    fn a_new_campaign_runs_its_first_plan_under_a_minted_id() -> anyhow::Result<()> {
        let new = a_new_campaign()?;
        let campaign_id = new.campaign_id().clone();
        assert!(campaign_id.as_str().starts_with(CampaignId::PREFIX));
        assert_ne!(
            &campaign_id,
            a_new_campaign()?.campaign_id(),
            "each create mints its own id"
        );

        let first = a_plan();
        let campaign = new.around(first.clone(), Timestamp::now());
        assert_eq!(campaign.campaign_id, campaign_id);
        assert_eq!(campaign.current_plan_id, Some(first));
        assert_eq!(campaign.created_at, campaign.updated_at);
        Ok(())
    }

    /// The pointer moves onto each plan filed into the campaign, and the id a
    /// caller holds does not move with it. That is the entity in one assertion.
    #[test]
    fn advancing_moves_the_head_and_not_the_id() -> anyhow::Result<()> {
        let mut campaign = a_campaign()?;
        let id = campaign.campaign_id.clone();
        let second = a_plan();

        assert!(campaign.advance(second.clone(), Timestamp::now()));
        assert_eq!(campaign.current_plan_id, Some(second));
        assert_eq!(campaign.campaign_id, id, "the identity never moves");
        Ok(())
    }

    /// A rename and a hand-off leave the identity and the head alone.
    #[test]
    fn the_head_survives_the_edits() -> anyhow::Result<()> {
        let mut campaign = a_campaign()?;
        let plan_id = campaign.current_plan_id.clone();

        assert!(campaign.rename(
            NonEmptyTrimmedString::try_new("Nightly v2")?,
            Timestamp::now()
        ));
        let org_id = OrgId::mint();
        assert!(campaign.set_owner(
            CampaignOwner::Org {
                org_id: org_id.clone()
            },
            Timestamp::now()
        ));

        assert_eq!(
            campaign.current_plan_id, plan_id,
            "a hand-off keeps what the campaign is running"
        );
        assert_eq!(campaign.owner.org_id(), &org_id);
        Ok(())
    }

    /// A write asking for the state the record is already in changes nothing,
    /// `updated_at` included. That is what makes a retried ingest safe rather
    /// than a second edit.
    #[test]
    fn a_write_to_the_current_value_is_a_no_op() -> anyhow::Result<()> {
        let mut campaign = a_campaign()?;
        let plan_id = campaign
            .current_plan_id
            .clone()
            .context("created with a plan")?;
        let before = campaign.clone();

        assert!(!campaign.advance(plan_id, Timestamp::now()));
        assert!(!campaign.rename(campaign.name.clone(), Timestamp::now()));
        assert!(!campaign.set_labels(campaign.labels.clone(), Timestamp::now()));
        assert!(!campaign.set_owner(campaign.owner.clone(), Timestamp::now()));
        assert!(!campaign.set_state(campaign.state, None, Timestamp::now()));
        assert_eq!(
            campaign, before,
            "a no-op moves nothing, updated_at included"
        );
        Ok(())
    }

    /// New labels replace the old set whole, and a record nobody labelled
    /// stores no `labels` key, as every record before labels.
    #[test]
    fn labels_replace_the_set_and_an_empty_set_is_not_stored() -> anyhow::Result<()> {
        let mut campaign = a_campaign()?;
        assert!(serde_json::to_value(&campaign)?.get("labels").is_none());
        let created = campaign.updated_at;

        let labels = |of: serde_json::Value| serde_json::from_value::<Labels>(of);
        let later = created + jiff::SignedDuration::from_secs(5);
        assert!(campaign.set_labels(
            labels(serde_json::json!({"study": "acmelm", "kind": "canary"}))?,
            later
        ));
        assert!(campaign.set_labels(labels(serde_json::json!({"study": "acmelm-text"}))?, later));
        assert_eq!(
            serde_json::to_value(&campaign)?.get("labels"),
            Some(&serde_json::json!({"study": "acmelm-text"}))
        );
        assert_eq!(campaign.updated_at, later);
        assert_eq!(
            serde_json::from_value::<Campaign>(serde_json::to_value(&campaign)?)?,
            campaign
        );
        Ok(())
    }

    /// An archived campaign stores `"state": "archived"`; an active one stores
    /// no `state` key.
    #[test]
    fn an_archive_is_stored_and_an_unarchive_is_not() -> anyhow::Result<()> {
        let mut campaign = a_campaign()?;
        assert!(serde_json::to_value(&campaign)?.get("state").is_none());
        let later = campaign.updated_at + jiff::SignedDuration::from_secs(5);

        assert!(campaign.set_state(CampaignState::Archived, None, later));
        assert_eq!(
            serde_json::to_value(&campaign)?.get("state"),
            Some(&serde_json::json!("archived"))
        );
        assert_eq!(campaign.updated_at, later);
        assert_eq!(
            serde_json::from_value::<Campaign>(serde_json::to_value(&campaign)?)?,
            campaign
        );

        assert!(campaign.set_state(CampaignState::Active, None, later));
        assert!(serde_json::to_value(&campaign)?.get("state").is_none());
        Ok(())
    }

    /// A lock keeps its note, a new note on a locked campaign is a change, and
    /// any other state drops the note.
    #[test]
    fn a_lock_keeps_its_note_until_the_state_changes() -> anyhow::Result<()> {
        let mut campaign = a_campaign()?;
        let later = campaign.updated_at + jiff::SignedDuration::from_secs(5);
        let note = |text: &str| NonEmptyTrimmedString::try_new(text);

        assert!(campaign.set_state(CampaignState::Locked, Some(note("release")?), later));
        assert_eq!(
            serde_json::to_value(&campaign)?.get("state_note"),
            Some(&serde_json::json!("release"))
        );
        assert!(!campaign.set_state(CampaignState::Locked, Some(note("release")?), later));
        assert!(campaign.set_state(CampaignState::Locked, Some(note("audit")?), later));

        assert!(campaign.set_state(CampaignState::Archived, Some(note("ignored")?), later));
        assert_eq!(campaign.state_note, None);
        assert!(campaign.set_state(CampaignState::Locked, Some(note("again")?), later));
        assert!(campaign.set_state(CampaignState::Active, None, later));
        assert!(serde_json::to_value(&campaign)?.get("state_note").is_none());
        Ok(())
    }

    /// The stored shape, pinned in both directions: written and read under
    /// `type`.
    #[test]
    fn the_owner_shape_is_pinned() -> anyhow::Result<()> {
        let org_id = OrgId::mint();
        assert_eq!(
            serde_json::to_value(CampaignOwner::Org {
                org_id: org_id.clone()
            })?,
            serde_json::json!({ "type": "org", "org_id": org_id.as_str() }),
        );
        assert_eq!(
            serde_json::from_value::<CampaignOwner>(
                serde_json::json!({ "type": "org", "org_id": org_id.as_str() })
            )?
            .org_id(),
            &org_id,
        );
        Ok(())
    }

    /// A record written before a campaign needed a first plan has no
    /// `current_plan_id`, and still reads: the field is skipped when absent in
    /// both directions.
    #[test]
    fn a_record_running_nothing_still_reads() -> anyhow::Result<()> {
        let mut campaign = a_campaign()?;
        campaign.current_plan_id = None;
        let value = serde_json::to_value(&campaign)?;
        assert!(value.get("current_plan_id").is_none());
        assert_eq!(serde_json::from_value::<Campaign>(value)?, campaign);
        Ok(())
    }

    /// The body answers what serializing the record used to, key for key,
    /// including the `current_plan_id` a planless record skips.
    #[rstest]
    #[case::with_a_plan(Some("plan-0192b3c4-0000-7000-8000-000000000001"), serde_json::json!({}), CampaignState::Active)]
    #[case::without_a_plan(None, serde_json::json!({}), CampaignState::Active)]
    #[case::labelled(None, serde_json::json!({"study": "acmelm-text", "device": "s26"}), CampaignState::Active)]
    #[case::archived(None, serde_json::json!({}), CampaignState::Archived)]
    fn a_campaign_serializes_as_its_record(
        #[case] current_plan_id: Option<&str>,
        #[case] labels: serde_json::Value,
        #[case] state: CampaignState,
    ) -> anyhow::Result<()> {
        let now = Timestamp::now();
        let record = Campaign {
            campaign_id: CampaignId::mint(),
            name: NonEmptyTrimmedString::try_new("Nightly")?,
            owner: PrincipalOwner::Org {
                org_id: OrgId::mint(),
            },
            current_plan_id: current_plan_id.map(PlanId::try_new).transpose()?,
            labels: serde_json::from_value(labels)?,
            state,
            state_note: None,
            created_at: now,
            updated_at: now,
        };
        assert_eq!(
            serde_json::to_value(pipette_plan_types::Campaign::from(record.clone()))?,
            serde_json::to_value(record)?
        );
        Ok(())
    }
}
