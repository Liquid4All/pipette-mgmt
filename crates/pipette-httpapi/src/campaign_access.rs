//! Who may read a campaign, who may run one, and who may change its record.
//! No permission names of its own: campaigns ride the plan grants. A plan has
//! no owner; its campaign's owner decides who reads and cancels it.

use std::collections::BTreeSet;

use pipette_core::campaign::{Campaign, CampaignOwner};
use pipette_core::user::User;
use pipette_plan_types::{CampaignId, OrgId, UserId, UserPermission};

use crate::auth::{Caller, UserRights, user_org};
use crate::error::AppError;
use crate::owner::HandOffScope;
use crate::state::AppState;

const ANOTHER_OWNER: &str = "this campaign belongs to another owner";

/// A record created with no `owner` lands on the caller's organization.
pub(crate) const NO_ORGANIZATION: &str = "you belong to no organization, so name an owner";

/// Which campaigns a caller may **read**: opened by `read_plans` or
/// `manage_orgs`, so an org admin can read back a campaign it may repoint.
pub enum CampaignReach {
    /// `read_any_plans`, or `manage_any_orgs` over the records: every campaign.
    Any,
    /// The campaigns of `org`, and the plans `user` filed outside any.
    Own { user: UserId, org: Option<OrgId> },
    /// A campaign reader's ([`User::campaign_scope`]): these campaigns while
    /// `org` owns them, so one handed to another organization drops out.
    Listed {
        org: Option<OrgId>,
        campaigns: BTreeSet<CampaignId>,
    },
}

impl CampaignReach {
    /// The reach over campaign **records**, once per request: wider than
    /// [`over_plans`](Self::over_plans) by `manage_any_orgs`, never for plans.
    pub async fn over_records(state: &AppState, caller: Caller<'_>) -> Result<Self, AppError> {
        let user = caller.user;
        let rights = caller.rights;
        if rights.holds(UserPermission::ManageAnyOrgs) {
            return Ok(Self::Any);
        }
        Self::over_plans(user, rights, user_org(state, &user.user_id).await?)
    }

    /// Like [`over_records`](Self::over_records), but return `None` if the
    /// caller has no campaign read grant. Log no refusal in that case.
    pub async fn over_records_if_granted(
        state: &AppState,
        caller: Caller<'_>,
    ) -> Result<Option<Self>, AppError> {
        if !crate::router::READ_CAMPAIGN.allows(caller.rights) {
            return Ok(None);
        }
        Self::over_records(state, caller).await.map(Some)
    }

    /// The reach over a campaign's **plans**, from a permission set and
    /// membership the caller already loaded.
    pub fn over_plans(
        user: &User,
        rights: &UserRights,
        org: Option<OrgId>,
    ) -> Result<Self, AppError> {
        if rights.holds(UserPermission::ReadAnyPlans) {
            return Ok(Self::Any);
        }
        if !rights.holds(UserPermission::ReadPlans) && !rights.holds(UserPermission::ManageOrgs) {
            tracing::warn!("refused: caller may read no campaigns");
            return Err(AppError::Forbidden(
                "reading campaigns needs read_plans, or manage_orgs for your organization's".into(),
            ));
        }
        Ok(Self::own(user, org))
    }

    /// The caller's organization's campaigns, whatever wider grant it holds:
    /// what a plan listing under `scope=mine` asks for. A campaign reader's are
    /// its listed ones.
    pub fn own(user: &User, org: Option<OrgId>) -> Self {
        match &user.campaign_scope {
            Some(listed) => Self::Listed {
                org,
                campaigns: listed.clone(),
            },
            None => Self::Own {
                user: user.user_id.clone(),
                org,
            },
        }
    }

    /// Who files the campaignless plans this reach reads: the caller under
    /// [`Self::Own`], nobody for a campaign reader.
    pub fn filer(&self) -> Option<&UserId> {
        match self {
            Self::Own { user, .. } => Some(user),
            Self::Any | Self::Listed { .. } => None,
        }
    }

    /// A `403`, not a `404`, for [`CampaignAuthority::require`]'s reason.
    pub fn require(&self, campaign: &Campaign) -> Result<(), AppError> {
        if self.admits(campaign) {
            return Ok(());
        }
        tracing::warn!(
            campaign_id = %campaign.campaign_id,
            "refused: the campaign is outside the caller's reach"
        );
        Err(AppError::Forbidden(match self {
            Self::Listed { .. } => "this campaign is not one this account reads".into(),
            Self::Any | Self::Own { .. } => "this campaign belongs to another owner".into(),
        }))
    }

    pub fn admits(&self, campaign: &Campaign) -> bool {
        match self {
            Self::Listed { org, campaigns } => {
                org.as_ref() == Some(campaign.owner.org_id())
                    && campaigns.contains(&campaign.campaign_id)
            }
            Self::Any | Self::Own { .. } => self.admits_owner(&campaign.owner),
        }
    }

    /// Whether the campaigns of `owner` are in reach, for a record they share,
    /// such as a campaign group. A campaign reader reaches listed campaigns alone.
    pub fn admits_owner(&self, owner: &CampaignOwner) -> bool {
        match self {
            Self::Any => true,
            Self::Listed { .. } => false,
            Self::Own { org, .. } => org.as_ref() == Some(owner.org_id()),
        }
    }
}

/// How far a caller's authority to **run** campaigns and change their records
/// reaches. Independent facts, not one enum: a caller can hold several at once.
#[derive(Clone)]
pub struct CampaignAuthority {
    /// `manage_any_orgs`: every campaign, whoever owns it.
    server: bool,
    /// `submit_plans`: with a membership, creates its organization's campaigns.
    own: bool,
    /// `submit_plans` on a person's account: with a membership, administers its
    /// organization's campaigns, as it did the ones it owned itself.
    member_admin: bool,
    /// `manage_orgs` **and** membership. `None` without a membership: it
    /// administers nothing, which is not an error.
    admin_of: Option<OrgId>,
    /// Membership alone runs its organization's campaigns.
    member_of: Option<OrgId>,
}

impl HandOffScope for CampaignAuthority {
    fn is_server(&self) -> bool {
        self.server
    }
    fn admin_of(&self) -> Option<&OrgId> {
        self.admin_of.as_ref()
    }
    fn gives_to(&self) -> Option<&OrgId> {
        self.member_of.as_ref()
    }
}

impl CampaignAuthority {
    /// Resolve the authority, or a `403` naming both grants if the caller holds
    /// neither.
    pub async fn resolve(state: &AppState, caller: Caller<'_>) -> Result<Self, AppError> {
        let user = caller.user;
        let rights = caller.rights;
        let authority = Self::of(rights, user_org(state, &user.user_id).await?);
        if !authority.server && !authority.own && authority.admin_of.is_none() {
            tracing::warn!("refused: caller administers no campaigns");
            return Err(AppError::Forbidden(
                "campaigns need submit_plans to own one, or manage_orgs to administer your \
                 organization's"
                    .into(),
            ));
        }
        Ok(authority)
    }

    /// The authority `rights` and the caller's membership give, even none: a
    /// plan route refuses on its own grant rather than with
    /// [`resolve`](Self::resolve)'s `403`.
    pub fn of(rights: &UserRights, member_of: Option<OrgId>) -> Self {
        Self {
            server: rights.holds(UserPermission::ManageAnyOrgs),
            own: rights.holds(UserPermission::SubmitPlans),
            member_admin: rights.holds(UserPermission::SubmitPlans) && !rights.is_service(),
            admin_of: rights
                .holds(UserPermission::ManageOrgs)
                .then(|| member_of.clone())
                .flatten(),
            member_of,
        }
    }

    /// Whether this caller may rename, hand over or delete a campaign owned by
    /// `owner`: its organization's, holding `submit_plans` or `manage_orgs`.
    pub fn may_administer(&self, owner: &CampaignOwner) -> bool {
        self.server
            || self.admin_of.as_ref() == Some(owner.org_id())
            || (self.member_admin && self.member_of.as_ref() == Some(owner.org_id()))
    }

    /// Whether this caller may file into a campaign owned by `owner`. Wider
    /// than [`Self::may_administer`] by the members of an organization, who run
    /// its campaigns without owning their records.
    pub fn may_run(&self, owner: &CampaignOwner) -> bool {
        self.server || self.runs(owner)
    }

    /// [`Self::may_run`] without the server's reach: where the bare plan grants
    /// apply; any other campaign takes the `_any_` ones.
    pub fn runs(&self, owner: &CampaignOwner) -> bool {
        self.member_of.as_ref() == Some(owner.org_id())
    }

    /// The default owner of a new campaign: the caller's organization, or a
    /// `400` for a caller in none.
    pub fn own_owner(&self) -> Result<CampaignOwner, AppError> {
        self.member_of
            .clone()
            .map(|org_id| CampaignOwner::Org { org_id })
            .ok_or_else(|| AppError::BadRequest(NO_ORGANIZATION.into()))
    }

    /// A `403`, not a `404`: a campaign id is a minted UUIDv7, so a caller that
    /// names one was given it and the true answer discloses nothing.
    pub fn require(&self, campaign: &Campaign) -> Result<(), AppError> {
        self.require_owner(
            &campaign.owner,
            campaign.campaign_id.as_str(),
            ANOTHER_OWNER,
        )
    }

    /// Refuse unless this caller may file into `campaign`, with
    /// [`Self::require`]'s answer.
    pub fn require_run(&self, campaign: &Campaign) -> Result<(), AppError> {
        self.require_run_owner(
            &campaign.owner,
            campaign.campaign_id.as_str(),
            ANOTHER_OWNER,
        )
    }

    /// [`Self::require_run`] for `record`, a record of `owner`'s other than a
    /// campaign: `another_owner` replaces the campaign wording.
    pub fn require_run_owner(
        &self,
        owner: &CampaignOwner,
        record: &str,
        another_owner: &'static str,
    ) -> Result<(), AppError> {
        self.require_if(self.may_run(owner), owner, record, another_owner)
    }

    fn require_if(
        &self,
        admitted: bool,
        owner: &CampaignOwner,
        record: &str,
        another_owner: &'static str,
    ) -> Result<(), AppError> {
        if admitted {
            return Ok(());
        }
        tracing::warn!(
            record,
            owner = %owner.label(),
            "refused: the record is outside the caller's scope"
        );
        Err(AppError::Forbidden(
            self.refusal(owner, another_owner).into(),
        ))
    }

    /// [`Self::require`] for a record of `owner`'s other than a campaign, as
    /// [`Self::require_run_owner`] takes one.
    pub fn require_owner(
        &self,
        owner: &CampaignOwner,
        record: &str,
        another_owner: &'static str,
    ) -> Result<(), AppError> {
        self.require_if(self.may_administer(owner), owner, record, another_owner)
    }

    /// Why a check refused `owner`: for its organization's campaign, the
    /// caller is told the grant it lacks, not `another_owner`.
    fn refusal(&self, owner: &CampaignOwner, another_owner: &'static str) -> &'static str {
        if self.member_of.as_ref() == Some(owner.org_id()) {
            "administering your organization's campaigns needs submit_plans or manage_orgs"
        } else {
            another_owner
        }
    }
}

#[cfg(test)]
mod tests {
    use jiff::Timestamp;
    use rstest::rstest;

    use pipette_core::campaign::NewCampaign;
    use pipette_core::types::Mint;
    use pipette_plan_types::{NonEmptyTrimmedString, PlanId};

    use super::*;

    fn org(id: &OrgId) -> CampaignOwner {
        CampaignOwner::Org { org_id: id.clone() }
    }

    /// Where the caller stands on its organization: an administrator holds
    /// `manage_orgs` and the membership.
    #[derive(Clone, Copy, PartialEq)]
    enum Standing {
        Outside,
        Member,
        Admin,
    }

    /// The rules, as a table against the owners they are not about.
    #[rstest]
    // A plain operator runs and administers its organization's campaigns.
    #[case::own(true, Standing::Member, false, true, true, false)]
    // An org admin administers the team's campaigns.
    #[case::org_admin(false, Standing::Admin, false, true, true, false)]
    #[case::both(true, Standing::Admin, false, true, true, false)]
    // A server admin reaches every campaign, a stranger's included.
    #[case::server(false, Standing::Member, true, true, true, true)]
    // Membership with no grant runs the team's campaigns and owns nothing.
    #[case::member(false, Standing::Member, false, false, true, false)]
    // Without the membership, the organization's campaigns are a stranger's.
    #[case::unaffiliated(true, Standing::Outside, false, false, false, false)]
    fn the_authority_rules_hold(
        #[case] own: bool,
        #[case] standing: Standing,
        #[case] server: bool,
        #[case] reaches_org: bool,
        #[case] runs_org: bool,
        #[case] reaches_stranger: bool,
    ) {
        let my_org = OrgId::mint();
        let authority = CampaignAuthority {
            server,
            own,
            member_admin: own,
            admin_of: (standing == Standing::Admin).then(|| my_org.clone()),
            member_of: (standing != Standing::Outside).then(|| my_org.clone()),
        };

        assert_eq!(authority.may_administer(&org(&my_org)), reaches_org);
        assert_eq!(authority.may_run(&org(&my_org)), runs_org);
        let stranger = org(&OrgId::mint());
        assert_eq!(authority.may_administer(&stranger), reaches_stranger);
        assert_eq!(authority.may_run(&stranger), reaches_stranger);
    }

    /// A job's `submit_plans` creates and runs the team's campaigns and
    /// administers none of them.
    #[test]
    fn a_service_account_runs_and_never_administers() {
        let my_org = OrgId::mint();
        let service = CampaignAuthority {
            server: false,
            own: true,
            member_admin: false,
            admin_of: None,
            member_of: Some(my_org.clone()),
        };
        assert!(service.may_run(&org(&my_org)));
        assert!(!service.may_administer(&org(&my_org)));
    }

    #[test]
    fn a_refusal_names_the_grant_the_caller_lacks() {
        let my_org = OrgId::mint();
        let authority = CampaignAuthority {
            server: false,
            own: false,
            member_admin: false,
            admin_of: None,
            member_of: Some(my_org.clone()),
        };
        assert!(
            authority
                .refusal(&org(&my_org), ANOTHER_OWNER)
                .contains("needs submit_plans or manage_orgs")
        );
        assert_eq!(
            authority.refusal(&org(&OrgId::mint()), ANOTHER_OWNER),
            "this campaign belongs to another owner"
        );
    }

    /// A campaign under `owner`, as a reach judges it.
    fn campaign(owner: CampaignOwner) -> anyhow::Result<Campaign> {
        Ok(
            NewCampaign::new(NonEmptyTrimmedString::try_new("eval")?, owner).around(
                PlanId::try_new("plan-0192b3c4-0000-7000-8000-000000000001")?,
                Timestamp::now(),
            ),
        )
    }

    /// A member sees the team's campaigns; an account on no organization sees
    /// none.
    #[test]
    fn a_member_reads_its_organizations_campaigns() -> anyhow::Result<()> {
        let me = UserId::mint();
        let my_org = OrgId::mint();
        let reach = CampaignReach::Own {
            user: me.clone(),
            org: Some(my_org.clone()),
        };
        assert!(reach.admits(&campaign(org(&my_org))?));
        assert!(!reach.admits(&campaign(org(&OrgId::mint()))?));

        let unaffiliated = CampaignReach::Own {
            user: me.clone(),
            org: None,
        };
        assert!(
            !unaffiliated.admits(&campaign(org(&my_org))?),
            "no membership reaches no organization's campaigns"
        );

        assert!(CampaignReach::Any.admits(&campaign(org(&OrgId::mint()))?));
        Ok(())
    }

    /// A campaign reader reads its listed campaigns while its organization owns
    /// them: neither one handed to another organization, nor its
    /// organization's others, nor any plan filed outside a campaign.
    #[test]
    fn a_campaign_reader_reads_its_organizations_listed_campaigns_alone() -> anyhow::Result<()> {
        let my_org = OrgId::mint();
        let listed = campaign(org(&my_org))?;
        let handed_over = Campaign {
            owner: org(&OrgId::mint()),
            ..listed.clone()
        };
        let mut reader = User::try_new(
            pipette_plan_types::ContactEmail::try_new("publish+site@example.com")?,
            Timestamp::now(),
        )?;
        reader.campaign_scope = Some(BTreeSet::from([listed.campaign_id.clone()]));
        let reach = CampaignReach::own(&reader, Some(my_org.clone()));

        assert!(reach.admits(&listed));
        assert!(
            !reach.admits(&handed_over),
            "a transfer takes it out of reach"
        );
        assert!(
            !reach.admits(&campaign(org(&my_org))?),
            "an unlisted team campaign"
        );
        assert!(
            !reach.admits_owner(&org(&my_org)),
            "its organization's campaign groups"
        );
        assert_eq!(reach.filer(), None);

        reader.campaign_scope = None;
        let member = CampaignReach::own(&reader, Some(my_org.clone()));
        assert!(member.admits(&campaign(org(&my_org))?));
        assert_eq!(member.filer(), Some(&reader.user_id));
        Ok(())
    }
}
