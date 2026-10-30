//! Who may read a campaign, who may run one, and who may change its record.
//! No permission names of its own: campaigns ride the plan grants. A plan has
//! no owner; its campaign's owner decides who reads and cancels it.

use std::collections::BTreeSet;

use pipette_core::campaign::{Campaign, CampaignOwner};
use pipette_core::user::User;
use pipette_plan_types::{CampaignId, OrgId, UserId, UserPermission, UserStatus};

use crate::auth::{Caller, UserRights, user_org};
use crate::error::AppError;
use crate::owner::HandOffScope;
use crate::state::AppState;

/// Which campaigns a caller may **read**: opened by `read_plans` or
/// `manage_orgs`, so an org admin can read back a campaign it may repoint.
pub enum CampaignReach {
    /// `read_any_plans`, or `manage_any_orgs` over the records: every campaign.
    Any,
    /// `offboarded` holds the disabled members whose records the caller winds
    /// down; their plans are not read through it.
    Own {
        user: UserId,
        org: Option<OrgId>,
        offboarded: BTreeSet<UserId>,
    },
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
        let member_of = user_org(state, &user.user_id).await?;
        let reach = Self::over_plans(user, rights, member_of.clone())?;
        let (Self::Own { user, org, .. }, Some(admin_of), true) =
            (&reach, &member_of, rights.holds(UserPermission::ManageOrgs))
        else {
            return Ok(reach);
        };
        Ok(Self::Own {
            user: user.clone(),
            org: org.clone(),
            offboarded: offboarded_members(state, rights, admin_of).await?,
        })
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

    /// The caller's own campaigns and its organization's, whatever wider grant
    /// it holds: what a plan listing under `scope=mine` asks for. A campaign
    /// reader's are its listed ones.
    pub fn own(user: &User, org: Option<OrgId>) -> Self {
        match &user.campaign_scope {
            Some(listed) => Self::Listed {
                org,
                campaigns: listed.clone(),
            },
            None => Self::Own {
                user: user.user_id.clone(),
                org,
                offboarded: BTreeSet::new(),
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
        match (self, &campaign.owner) {
            (Self::Any, _) => true,
            (Self::Listed { org, campaigns }, CampaignOwner::Org { org_id }) => {
                org.as_ref() == Some(org_id) && campaigns.contains(&campaign.campaign_id)
            }
            (Self::Listed { .. }, CampaignOwner::User { .. }) => false,
            (
                Self::Own {
                    user, offboarded, ..
                },
                CampaignOwner::User { user_id },
            ) => user_id == user || offboarded.contains(user_id),
            (Self::Own { org, .. }, CampaignOwner::Org { org_id }) => {
                org.as_ref().is_some_and(|mine| mine == org_id)
            }
        }
    }
}

/// How far a caller's authority to **run** campaigns and change their records
/// reaches. Independent facts, not one enum: a caller can hold several at once.
#[derive(Clone)]
pub struct CampaignAuthority {
    user_id: UserId,
    /// `manage_any_orgs`: every campaign, whoever owns it.
    server: bool,
    /// `submit_plans`: may own campaigns itself.
    own: bool,
    /// `manage_orgs` **and** membership. `None` without a membership: it
    /// administers nothing, which is not an error.
    admin_of: Option<OrgId>,
    /// Membership alone: a member runs its organization's campaigns and gives
    /// its own to the team, which is disposal, not administration.
    member_of: Option<OrgId>,
}

impl HandOffScope for CampaignAuthority {
    fn user_id(&self) -> &UserId {
        &self.user_id
    }
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
        let authority = Self::of(user, rights, user_org(state, &user.user_id).await?);
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
    pub fn of(user: &User, rights: &UserRights, member_of: Option<OrgId>) -> Self {
        Self {
            user_id: user.user_id.clone(),
            server: rights.holds(UserPermission::ManageAnyOrgs),
            own: rights.holds(UserPermission::SubmitPlans),
            admin_of: rights
                .holds(UserPermission::ManageOrgs)
                .then(|| member_of.clone())
                .flatten(),
            member_of,
        }
    }

    /// Whether this caller may rename, hand over or delete a campaign owned by
    /// `owner`. A personal campaign stays personal when its owner joins a team.
    pub fn may_administer(&self, owner: &CampaignOwner) -> bool {
        self.server || self.owns(owner)
    }

    /// [`Self::may_administer`] without the server's reach.
    fn owns(&self, owner: &CampaignOwner) -> bool {
        match owner {
            CampaignOwner::User { user_id } => self.own && *user_id == self.user_id,
            CampaignOwner::Org { org_id } => self.admin_of.as_ref() == Some(org_id),
        }
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
        match owner {
            CampaignOwner::User { .. } => self.owns(owner),
            CampaignOwner::Org { org_id } => self.member_of.as_ref() == Some(org_id),
        }
    }

    /// The default owner of a new campaign: the caller, never its organization,
    /// which would hand a campaign to a team over an omitted field.
    pub fn own_owner(&self) -> CampaignOwner {
        CampaignOwner::User {
            user_id: self.user_id.clone(),
        }
    }

    /// A `403`, not a `404`: a campaign id is a minted UUIDv7, so a caller that
    /// names one was given it and the true answer discloses nothing.
    pub fn require(&self, campaign: &Campaign) -> Result<(), AppError> {
        self.require_if(self.may_administer(&campaign.owner), campaign)
    }

    /// Refuse unless this caller may file into `campaign`, with
    /// [`Self::require`]'s answer.
    pub fn require_run(&self, campaign: &Campaign) -> Result<(), AppError> {
        self.require_if(self.may_run(&campaign.owner), campaign)
    }

    fn require_if(&self, admitted: bool, campaign: &Campaign) -> Result<(), AppError> {
        if admitted {
            return Ok(());
        }
        tracing::warn!(
            campaign_id = %campaign.campaign_id,
            owner = %campaign.owner.label(),
            "refused: the campaign is outside the caller's scope"
        );
        Err(AppError::Forbidden(self.refusal(&campaign.owner).into()))
    }

    /// Whether `owner` is a disabled member of the organization this caller
    /// administers. Its administrator winds the records down, but neither files
    /// into them nor reads their plans before a hand-over.
    pub async fn offboards(
        &self,
        state: &AppState,
        rights: &UserRights,
        owner: &CampaignOwner,
    ) -> Result<bool, AppError> {
        let (Some(admin_of), CampaignOwner::User { user_id }) = (&self.admin_of, owner) else {
            return Ok(false);
        };
        if user_org(state, user_id).await?.as_ref() != Some(admin_of) {
            return Ok(false);
        }
        let member =
            state.user_store.get_user(user_id).await.map_err(|e| {
                AppError::internal(e.context("failed to read the campaign's owner"))
            })?;
        match member {
            Some(member) => offboardable(state, rights, &member).await,
            None => Ok(false),
        }
    }

    /// [`Self::require`] for a write to the record itself, which
    /// [`Self::offboards`] opens too.
    pub async fn require_record(
        &self,
        state: &AppState,
        rights: &UserRights,
        campaign: &Campaign,
    ) -> Result<(), AppError> {
        if self.may_administer(&campaign.owner)
            || self.offboards(state, rights, &campaign.owner).await?
        {
            return Ok(());
        }
        self.require(campaign)
    }

    /// Why a check refused `owner`: for its own or its organization's campaign,
    /// the caller is told the grant it lacks, not "another owner".
    fn refusal(&self, owner: &CampaignOwner) -> &'static str {
        match owner {
            CampaignOwner::User { user_id } if *user_id == self.user_id => {
                "administering your own campaigns needs submit_plans"
            }
            CampaignOwner::Org { org_id } if self.member_of.as_ref() == Some(org_id) => {
                "administering your organization's campaigns needs manage_orgs"
            }
            CampaignOwner::Org { .. } | CampaignOwner::User { .. } => {
                "this campaign belongs to another owner"
            }
        }
    }
}

/// Whether `member`'s personal campaigns are its administrator's to wind
/// down: it is disabled, and holds no server-wide name `rights` lacks.
async fn offboardable(
    state: &AppState,
    rights: &UserRights,
    member: &User,
) -> Result<bool, AppError> {
    Ok(member.status == UserStatus::Disabled
        && !crate::api_v1::users::outranks(state, rights, member).await?)
}

/// The members of `org_id` that [`CampaignAuthority::offboards`] admits, for a
/// listing that judges each row without a read.
async fn offboarded_members(
    state: &AppState,
    rights: &UserRights,
    org_id: &OrgId,
) -> Result<BTreeSet<UserId>, AppError> {
    let members = pipette_pipeline::org::members(&*state.org_store, &*state.user_store, org_id)
        .await
        .map_err(|e| AppError::internal(e.context("failed to read the organization's members")))?;
    let mut offboarded = BTreeSet::new();
    for member in members {
        if offboardable(state, rights, &member).await? {
            offboarded.insert(member.user_id);
        }
    }
    Ok(offboarded)
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use rstest::rstest;

    use pipette_core::campaign::NewCampaign;
    use pipette_core::types::Mint;
    use pipette_plan_types::{NonEmptyTrimmedString, PlanId};

    use super::*;

    fn org(id: &OrgId) -> CampaignOwner {
        CampaignOwner::Org { org_id: id.clone() }
    }

    fn user(id: &UserId) -> CampaignOwner {
        CampaignOwner::User {
            user_id: id.clone(),
        }
    }

    /// Where the caller stands on its organization: an administrator holds
    /// `manage_orgs` and the membership.
    #[derive(Clone, Copy, PartialEq)]
    enum Standing {
        Outside,
        Member,
        Admin,
    }

    /// The three rules, as a table against the owners they are not about.
    #[rstest]
    // A plain operator owns its own campaigns and runs its organization's,
    // whose records are the administrators'.
    #[case::own(true, Standing::Member, false, true, false, true, false)]
    // An org admin administers the team's campaigns. It holds no `submit_plans`
    // here, so it owns none personally: the two grants are independent.
    #[case::org_admin(false, Standing::Admin, false, false, true, true, false)]
    // Holding both is the ordinary case for someone who runs a team and files
    // their own work.
    #[case::both(true, Standing::Admin, false, true, true, true, false)]
    // A server admin reaches every campaign, a stranger's included.
    #[case::server(false, Standing::Member, true, true, true, true, true)]
    // Membership with no grant runs the team's campaigns and owns nothing.
    #[case::member(false, Standing::Member, false, false, false, true, false)]
    // Without the membership, the organization's campaigns are a stranger's.
    #[case::unaffiliated(true, Standing::Outside, false, true, false, false, false)]
    fn the_authority_rules_hold(
        #[case] own: bool,
        #[case] standing: Standing,
        #[case] server: bool,
        #[case] reaches_own: bool,
        #[case] reaches_org: bool,
        #[case] runs_org: bool,
        #[case] reaches_stranger: bool,
    ) {
        let me = UserId::mint();
        let my_org = OrgId::mint();
        let authority = CampaignAuthority {
            user_id: me.clone(),
            server,
            own,
            admin_of: (standing == Standing::Admin).then(|| my_org.clone()),
            member_of: (standing != Standing::Outside).then(|| my_org.clone()),
        };

        assert_eq!(authority.may_administer(&user(&me)), reaches_own);
        assert_eq!(authority.may_administer(&org(&my_org)), reaches_org);
        assert_eq!(authority.may_run(&user(&me)), reaches_own);
        assert_eq!(authority.may_run(&org(&my_org)), runs_org);
        [user(&UserId::mint()), org(&OrgId::mint())]
            .iter()
            .for_each(|stranger| {
                assert_eq!(authority.may_administer(stranger), reaches_stranger);
                assert_eq!(authority.may_run(stranger), reaches_stranger);
            });
    }

    #[test]
    fn a_refusal_names_the_grant_the_caller_lacks() {
        let me = UserId::mint();
        let my_org = OrgId::mint();
        let authority = CampaignAuthority {
            user_id: me.clone(),
            server: false,
            own: false,
            admin_of: None,
            member_of: Some(my_org.clone()),
        };
        assert!(authority.refusal(&user(&me)).contains("needs submit_plans"));
        assert!(
            authority
                .refusal(&org(&my_org))
                .contains("needs manage_orgs")
        );
        for stranger in [org(&OrgId::mint()), user(&UserId::mint())] {
            assert_eq!(
                authority.refusal(&stranger),
                "this campaign belongs to another owner"
            );
        }
    }

    /// A campaign under `owner`, as a reach judges it.
    fn campaign(owner: CampaignOwner) -> anyhow::Result<Campaign> {
        Ok(
            NewCampaign::new(NonEmptyTrimmedString::try_new("eval")?, owner).around(
                PlanId::try_new("plan-0192b3c4-0000-7000-8000-000000000001")?,
                Utc::now(),
            ),
        )
    }

    /// A member sees the team's campaigns; an administrator's reach adds a
    /// disabled member's record. An account on no organization sees its own alone.
    #[test]
    fn a_member_reads_its_organizations_campaigns() -> anyhow::Result<()> {
        let me = UserId::mint();
        let my_org = OrgId::mint();
        let gone = UserId::mint();
        let reach = CampaignReach::Own {
            user: me.clone(),
            org: Some(my_org.clone()),
            offboarded: BTreeSet::from([gone.clone()]),
        };
        assert!(reach.admits(&campaign(user(&me))?));
        assert!(reach.admits(&campaign(org(&my_org))?));
        assert!(
            reach.admits(&campaign(user(&gone))?),
            "a disabled member's record"
        );
        assert!(!reach.admits(&campaign(user(&UserId::mint()))?));
        assert!(!reach.admits(&campaign(org(&OrgId::mint()))?));

        let unaffiliated = CampaignReach::Own {
            user: me.clone(),
            org: None,
            offboarded: BTreeSet::new(),
        };
        assert!(unaffiliated.admits(&campaign(user(&me))?));
        assert!(
            !unaffiliated.admits(&campaign(org(&my_org))?),
            "no membership reaches no organization's campaigns"
        );

        assert!(CampaignReach::Any.admits(&campaign(org(&OrgId::mint()))?));
        Ok(())
    }

    /// A campaign reader reads its listed campaigns while its organization owns
    /// them: neither one handed to another organization, nor a personal one,
    /// nor its organization's others, nor any plan filed outside a campaign.
    #[test]
    fn a_campaign_reader_reads_its_organizations_listed_campaigns_alone() -> anyhow::Result<()> {
        let my_org = OrgId::mint();
        let listed = campaign(org(&my_org))?;
        let handed_over = Campaign {
            owner: org(&OrgId::mint()),
            ..listed.clone()
        };
        let personal = campaign(user(&UserId::mint()))?;
        let mut reader = User::try_new(
            pipette_plan_types::ContactEmail::try_new("publish+site@example.com")?,
            Utc::now(),
        )?;
        reader.campaign_scope = Some(BTreeSet::from([
            listed.campaign_id.clone(),
            personal.campaign_id.clone(),
        ]));
        let reach = CampaignReach::own(&reader, Some(my_org.clone()));

        assert!(reach.admits(&listed));
        assert!(
            !reach.admits(&handed_over),
            "a transfer takes it out of reach"
        );
        assert!(
            !reach.admits(&personal),
            "a member's own campaign is not the org's"
        );
        assert!(
            !reach.admits(&campaign(org(&my_org))?),
            "an unlisted team campaign"
        );
        assert!(!reach.admits(&campaign(user(&reader.user_id))?));
        assert_eq!(reach.filer(), None);

        reader.campaign_scope = None;
        let member = CampaignReach::own(&reader, Some(my_org.clone()));
        assert!(member.admits(&campaign(org(&my_org))?));
        assert_eq!(member.filer(), Some(&reader.user_id));
        Ok(())
    }
}
