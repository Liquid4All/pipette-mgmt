//! Campaigns: the stable id for work whose plan keeps being revised. A campaign
//! mints its plans and points at the head; the plans' `previous_plan_id` chain
//! is the history. It holds no jobs, and gates only new plans while archived.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use pipette_plan_types::{
    CampaignId, CampaignState, Labels, NonEmptyTrimmedString, PlanId, PrincipalOwner,
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
    /// Who owns this, and therefore who may repoint it. Read as it stands, not
    /// derived through the owner's membership: moving an account between
    /// organizations must not hand its campaigns to a team's administrators.
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
    pub created_at: DateTime<Utc>,
    /// When any field here last changed: a rename, new labels, a state change,
    /// or a hand-off to another owner. Equal to `created_at` on a fresh record.
    pub updated_at: DateTime<Utc>,
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
    pub fn around(self, plan_id: PlanId, created_at: DateTime<Utc>) -> Campaign {
        Campaign {
            campaign_id: self.campaign_id,
            name: self.name,
            owner: self.owner,
            current_plan_id: Some(plan_id),
            labels: Labels::default(),
            state: CampaignState::Active,
            created_at,
            updated_at: created_at,
        }
    }
}

impl Campaign {
    /// Move onto `plan_id`, the plan ingestion just filed into this campaign;
    /// nothing hands this an arbitrary plan. Re-advancing onto the current plan,
    /// as a retried ingest does, is a no-op.
    pub fn advance(&mut self, plan_id: PlanId, at: DateTime<Utc>) -> bool {
        if self.current_plan_id.as_ref() == Some(&plan_id) {
            return false;
        }
        self.current_plan_id = Some(plan_id);
        self.updated_at = at;
        true
    }

    /// Rename the campaign, returning whether anything changed. Renaming to the
    /// current name writes nothing and keeps `updated_at`, so a retry is safe.
    pub fn rename(&mut self, name: NonEmptyTrimmedString, at: DateTime<Utc>) -> bool {
        if self.name == name {
            return false;
        }
        self.name = name;
        self.updated_at = at;
        true
    }

    /// Replace the campaign's labels with `labels`, on the same no-op rule.
    pub fn set_labels(&mut self, labels: Labels, at: DateTime<Utc>) -> bool {
        if self.labels == labels {
            return false;
        }
        self.labels = labels;
        self.updated_at = at;
        true
    }

    /// Archive the campaign or unarchive it, on the same no-op rule.
    pub fn set_state(&mut self, state: CampaignState, at: DateTime<Utc>) -> bool {
        if self.state == state {
            return false;
        }
        self.state = state;
        self.updated_at = at;
        true
    }

    /// Hand the campaign to a new owner, on the same no-op rule. Keeps the
    /// pointer: a team taking over inherits the work mid-flight.
    pub fn set_owner(&mut self, owner: CampaignOwner, at: DateTime<Utc>) -> bool {
        if self.owner == owner {
            return false;
        }
        self.owner = owner;
        self.updated_at = at;
        true
    }
}

#[cfg(test)]
mod tests {
    use anyhow::Context;
    use chrono::Utc;
    use rstest::rstest;

    use pipette_plan_types::{OrgId, UserId};

    use super::*;

    fn a_plan() -> PlanId {
        PlanId::from_uuid(uuid::Uuid::now_v7())
    }

    fn a_new_campaign() -> anyhow::Result<NewCampaign> {
        Ok(NewCampaign::new(
            NonEmptyTrimmedString::try_new("Nightly LFM2 regression")?,
            CampaignOwner::User {
                user_id: UserId::mint(),
            },
        ))
    }

    fn a_campaign() -> anyhow::Result<Campaign> {
        Ok(a_new_campaign()?.around(a_plan(), Utc::now()))
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
        let campaign = new.around(first.clone(), Utc::now());
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

        assert!(campaign.advance(second.clone(), Utc::now()));
        assert_eq!(campaign.current_plan_id, Some(second));
        assert_eq!(campaign.campaign_id, id, "the identity never moves");
        Ok(())
    }

    /// A rename and a hand-off leave the identity and the head alone.
    #[test]
    fn the_head_survives_the_edits() -> anyhow::Result<()> {
        let mut campaign = a_campaign()?;
        let plan_id = campaign.current_plan_id.clone();

        assert!(campaign.rename(NonEmptyTrimmedString::try_new("Nightly v2")?, Utc::now()));
        let org_id = OrgId::mint();
        assert!(campaign.set_owner(
            CampaignOwner::Org {
                org_id: org_id.clone()
            },
            Utc::now()
        ));

        assert_eq!(
            campaign.current_plan_id, plan_id,
            "a hand-off keeps what the campaign is running"
        );
        assert_eq!(campaign.owner.org_id(), Some(&org_id));
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

        assert!(!campaign.advance(plan_id, Utc::now()));
        assert!(!campaign.rename(campaign.name.clone(), Utc::now()));
        assert!(!campaign.set_labels(campaign.labels.clone(), Utc::now()));
        assert!(!campaign.set_owner(campaign.owner.clone(), Utc::now()));
        assert!(!campaign.set_state(campaign.state, Utc::now()));
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
        let later = created + chrono::Duration::seconds(5);
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
        let later = campaign.updated_at + chrono::Duration::seconds(5);

        assert!(campaign.set_state(CampaignState::Archived, later));
        assert_eq!(
            serde_json::to_value(&campaign)?.get("state"),
            Some(&serde_json::json!("archived"))
        );
        assert_eq!(campaign.updated_at, later);
        assert_eq!(
            serde_json::from_value::<Campaign>(serde_json::to_value(&campaign)?)?,
            campaign
        );

        assert!(campaign.set_state(CampaignState::Active, later));
        assert!(serde_json::to_value(&campaign)?.get("state").is_none());
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
            serde_json::from_value::<CampaignOwner>(serde_json::json!({
                "type": "user",
                "user_id": "user_0195c3e2-a4f3-7b8c-9d1e-2f3a4b5c6d7e",
            }))?
            .user_id()
            .map(UserId::as_str)
            .map(str::to_string),
            Some("user_0195c3e2-a4f3-7b8c-9d1e-2f3a4b5c6d7e".to_string()),
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
        let now = Utc::now();
        let record = Campaign {
            campaign_id: CampaignId::mint(),
            name: NonEmptyTrimmedString::try_new("Nightly")?,
            owner: PrincipalOwner::User {
                user_id: UserId::try_new("user_0195c3e2-a4f3-7b8c-9d1e-2f3a4b5c6d7e")?,
            },
            current_plan_id: current_plan_id.map(PlanId::try_new).transpose()?,
            labels: serde_json::from_value(labels)?,
            state,
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
