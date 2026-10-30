//! HTTP handlers for account administration, the delegation surface: a scoped
//! caller acts only on its organization's accounts, grants only names it holds,
//! and changes no account holding a server-wide name it lacks. No route deletes
//! an account: what it owns would name an id that never resolves again, so
//! offboarding disables.

pub mod clerk;
pub mod own;

use std::collections::{BTreeSet, HashMap};

use axum::Json;
use axum::extract::State;
use axum::http::{StatusCode, header};
use axum::response::IntoResponse;
use chrono::Utc;
use futures::stream;
use futures::stream::{StreamExt, TryStreamExt};

use pipette_core::user::{User, UserKey};
use pipette_core::user_permission::{
    Holder, StoredUserPermission, UserPermissionSet, effective, refused_for, stored_closure,
};
use pipette_core::user_preauth;
use pipette_core::user_preauth::{UserPreauthKey, UserPreauthParams};
use pipette_pipeline::org::RosterScope;
use pipette_plan_types::{
    AmendUserPermissionsRequest, AuditTarget, CampaignId, Changed, ContactEmail, CreateUserRequest,
    ListPage, MintUserPreauthRequest, NonEmptyTrimmedString, OrgId, PrincipalOwner,
    SetUserOrgRequest, SetUserPermissionsRequest, UpdateUserRequest, UserAccountType, UserDetail,
    UserId, UserKeyId, UserPermission, UserPermissionsAmended, UserPreauthKeyId,
    UserPreauthKeyMinted, UserPreauthKeyView, UserPreauthUsage, UserStatus, UserView,
};
use pipette_store::{EmailClaim, STORAGE_CONCURRENCY, UserPermissionMutation};

use crate::api_v1::orgs::assign_membership;
use crate::audit::Audit;
use crate::auth::{ActiveUser, Caller, HumanUser, UserRights, user_org};
use crate::campaign_access::CampaignReach;
use crate::error::AppError;
use crate::extract::{ApiJson, ApiPath, Parsed};
use crate::state::AppState;

/// The longest a delegated pre-auth key stands, in days: the host's default.
const PREAUTH_MAX_DAYS: u16 = 90;

/// Bounds a note that every listing of the account's keys carries.
const PREAUTH_NOTE_MAX_CHARS: usize = 4096;

/// The most campaigns a campaign scope names: every write reads each one.
const CAMPAIGN_SCOPE_MAX: usize = 100;

/// The names that make an account an administrator, which it cannot revoke
/// from itself.
const OWN_ADMINISTRATION: [UserPermission; 2] =
    [UserPermission::ManageOrgs, UserPermission::ManageAnyOrgs];

/// Why a permission swap wrote nothing.
enum SwapRefusal {
    /// The stored set holds these server-wide names the caller lacks.
    Outranked(Vec<UserPermission>),
    /// The write would take these from the caller's own account.
    OwnAdministration(Vec<UserPermission>),
}

/// The days the key stands, after every check a mint makes on the body alone.
fn validated_days(request: &MintUserPreauthRequest) -> Result<u16, AppError> {
    preauth_days(request.expires_in_days, request.note.as_ref())
}

/// The bounds a pre-auth key of either kind is minted under over HTTP.
pub(crate) fn preauth_days(
    expires_in_days: Option<u16>,
    note: Option<&NonEmptyTrimmedString>,
) -> Result<u16, AppError> {
    let days = expires_in_days.unwrap_or(PREAUTH_MAX_DAYS);
    if !(1..=PREAUTH_MAX_DAYS).contains(&days) {
        return Err(AppError::BadRequest(format!(
            "expires_in_days must be between 1 and {PREAUTH_MAX_DAYS}"
        )));
    }
    if let Some(note) = note
        && note.chars().count() > PREAUTH_NOTE_MAX_CHARS
    {
        return Err(AppError::BadRequest(format!(
            "note is longer than {PREAUTH_NOTE_MAX_CHARS} characters"
        )));
    }
    Ok(days)
}

/// How far `user`'s roster **read** reaches: [`RosterScope::Unaffiliated`],
/// reaching nothing, without `read_orgs`.
pub async fn read_reach(state: &AppState, caller: Caller<'_>) -> Result<RosterScope, AppError> {
    crate::api_v1::orgs::roster_reach(
        state,
        caller,
        UserPermission::ReadOrgs,
        UserPermission::ReadAnyOrgs,
    )
    .await
}

/// The account `user_id` names, refused when it is outside `scope`. Absent and
/// out-of-scope both read `404`, so an org admin cannot probe the roster.
pub async fn in_scope(
    state: &AppState,
    scope: &RosterScope,
    user_id: &UserId,
) -> Result<(User, Option<OrgId>), AppError> {
    scoped(state, scope, user_id, true).await
}

/// [`in_scope`], warning on an out-of-scope id only when `warn`: a read of
/// another roster's account is a followed link, not a refusal worth a warning.
async fn scoped(
    state: &AppState,
    scope: &RosterScope,
    user_id: &UserId,
    warn: bool,
) -> Result<(User, Option<OrgId>), AppError> {
    let user = state
        .user_store
        .get_user(user_id)
        .await
        .map_err(|e| AppError::Internal(format!("failed to read the user: {e:#}")))?;
    let org_id = user_org(state, user_id).await?;
    let user = user.ok_or_else(|| AppError::NotFound("no such user".into()))?;
    if !scope.covers(org_id.as_ref()) {
        if warn {
            tracing::warn!(%user_id, "refused: the account is outside the caller's scope");
        }
        return Err(AppError::OutOfScope("no such user".into()));
    }
    Ok((user, org_id))
}

/// Refuse a scoped caller a write to an account holding a server-wide name it
/// lacks. A `403`, not the out-of-scope `404`: the caller already reads it on its roster.
/// Only a permission write checks again as it swaps: any other lands over a grant made since.
pub(crate) async fn within_rank(
    state: &AppState,
    rights: &UserRights,
    target: &User,
) -> Result<(), AppError> {
    let held = stored_closure(
        state
            .user_store
            .get_user_permissions(&target.user_id)
            .await
            .map_err(|e| AppError::Internal(format!("failed to read the permissions: {e:#}")))?,
    );
    outranked(rights, &target.user_id, &held)
}

/// Whether `target` holds a server-wide name `rights` lacks: [`within_rank`]
/// as an answer, for a rule that reads it without refusing.
pub(crate) async fn outranks(
    state: &AppState,
    rights: &UserRights,
    target: &User,
) -> Result<bool, AppError> {
    let held = stored_closure(
        state
            .user_store
            .get_user_permissions(&target.user_id)
            .await
            .map_err(|e| AppError::Internal(format!("failed to read the permissions: {e:#}")))?,
    );
    Ok(!outranking(rights, &held).is_empty())
}

/// [`within_rank`] over a set already read.
fn outranked(
    rights: &UserRights,
    user_id: &UserId,
    held: &UserPermissionSet,
) -> Result<(), AppError> {
    let wider = outranking(rights, held);
    if wider.is_empty() {
        return Ok(());
    }
    Err(outranked_by(user_id, &wider))
}

/// The refusal of a write to an account holding `wider`, which the caller lacks.
fn outranked_by(user_id: &UserId, wider: &[UserPermission]) -> AppError {
    let names: Vec<&str> = wider.iter().map(AsRef::as_ref).collect();
    tracing::warn!(%user_id, wider = ?names,
        "refused: the account holds server-wide names the caller lacks");
    AppError::Forbidden(format!(
        "you cannot change an account holding server-wide names you do not: {}",
        names.join(", ")
    ))
}

/// The server-wide names `held` has that keep `rights` from changing it, none
/// under `manage_any_orgs`. Silent, so a swap the store replays logs once.
fn outranking(rights: &UserRights, held: &UserPermissionSet) -> Vec<UserPermission> {
    if rights.holds(UserPermission::ManageAnyOrgs) {
        return Vec::new();
    }
    rights.server_wide_lacking(held)
}

/// A `400`, not a `403`: no caller could grant `holder` these names.
fn holdable(holder: Holder, wanted: &[UserPermission]) -> Result<(), AppError> {
    let refused = refused_for(holder, wanted.iter().copied());
    if refused.is_empty() {
        return Ok(());
    }
    let names: Vec<&str> = refused.iter().map(AsRef::as_ref).collect();
    tracing::warn!(refused = ?names, "refused: a name the account may not hold");
    Err(AppError::BadRequest(format!(
        "{} cannot hold {}",
        holder.noun(),
        names.join(", ")
    )))
}

/// Refuse making `user_id` into `holder` while its stored set holds a name
/// `holder` cannot.
async fn holds_only_holdable(
    state: &AppState,
    user_id: &UserId,
    holder: Holder,
) -> Result<(), AppError> {
    let stored = state
        .user_store
        .get_user_permissions(user_id)
        .await
        .map_err(|e| AppError::Internal(format!("failed to read the permissions: {e:#}")))?
        .unwrap_or_default();
    let refused = refused_for(holder, stored.known());
    if refused.is_empty() {
        return Ok(());
    }
    let names: Vec<&str> = refused.iter().map(AsRef::as_ref).collect();
    tracing::warn!(%user_id, held = ?names, "refused: an account holding what it would no longer hold");
    Err(AppError::Conflict(format!(
        "this account holds {}, which {} cannot; remove it first",
        names.join(", "),
        holder.noun()
    )))
}

/// Refuse a campaign scope naming a campaign whose plans `caller` does not
/// read (a caller grants only what it holds), or one that `org`, the reader's
/// organization, does not own.
async fn within_campaign_reach(
    state: &AppState,
    caller: Caller<'_>,
    scope: &BTreeSet<CampaignId>,
    org: &OrgId,
) -> Result<(), AppError> {
    if scope.len() > CAMPAIGN_SCOPE_MAX {
        return Err(AppError::BadRequest(format!(
            "a campaign scope names at most {CAMPAIGN_SCOPE_MAX} campaigns"
        )));
    }
    let gaps = scope_gaps(state, caller, scope, Some(org)).await?;
    if let Some(campaign_id) = gaps.absent.first() {
        return Err(AppError::BadRequest(format!(
            "no such campaign: {campaign_id}"
        )));
    }
    if let Some(campaign_id) = gaps.unread.first() {
        tracing::warn!(%campaign_id, "refused: a campaign scope beyond the caller's reach");
        return Err(AppError::Forbidden(format!(
            "you do not read campaign {campaign_id}, so no account you scope may"
        )));
    }
    if let Some(campaign_id) = gaps.foreign.first() {
        return Err(AppError::BadRequest(format!(
            "campaign {campaign_id} is not the account's organization's: a campaign reader \
             reads its own organization's campaigns alone"
        )));
    }
    Ok(())
}

/// Its first sign-in lands in the account `own` made, so the address must be `own`'s.
async fn refuse_foreign_provision(
    state: &AppState,
    own: &OrgId,
    email: &ContactEmail,
) -> Result<(), AppError> {
    let Some(rule) = state
        .clerk
        .as_deref()
        .and_then(|clerk| clerk.provision_rule(email))
    else {
        return Ok(());
    };
    let provisioned = pipette_pipeline::org::resolve(&*state.org_store, &rule.org)
        .await
        .map_err(AppError::internal)?;
    if provisioned.is_some_and(|org| org.org_id == *own) {
        return Ok(());
    }
    tracing::warn!("refused: an address another organization's provision rule covers");
    Err(AppError::Forbidden(format!(
        "addresses at {} belong to the organization Clerk provisions them into",
        rule.domain
    )))
}

/// The campaigns of a scope that `caller` cannot vouch for.
#[derive(Default)]
struct ScopeGaps {
    /// No record: deleted, or never minted.
    absent: Vec<CampaignId>,
    /// Outside the plans `caller` reads.
    unread: Vec<CampaignId>,
    /// Not owned by the organization checked against.
    foreign: Vec<CampaignId>,
}

/// `owner`, when given, is the organization each campaign must belong to.
async fn scope_gaps(
    state: &AppState,
    caller: Caller<'_>,
    scope: &BTreeSet<CampaignId>,
    owner: Option<&OrgId>,
) -> Result<ScopeGaps, AppError> {
    let caller_org = user_org(state, &caller.user.user_id).await?;
    let reach = CampaignReach::over_plans(caller.user, caller.rights, caller_org)?;
    // Owned ids, for the handler's `Send` bound.
    let campaigns: Vec<_> = stream::iter(scope.iter().cloned())
        .map(|campaign_id| async move {
            state
                .campaign_store
                .get_campaign(&campaign_id)
                .await
                .map_err(|e| AppError::internal(e.context("failed to read a scoped campaign")))
        })
        .buffered(STORAGE_CONCURRENCY)
        .try_collect()
        .await?;
    Ok(scope.iter().zip(campaigns).fold(
        ScopeGaps::default(),
        |mut gaps, (campaign_id, campaign)| {
            match campaign {
                None => gaps.absent.push(campaign_id.clone()),
                Some(campaign) => {
                    if !reach.admits(&campaign) {
                        gaps.unread.push(campaign_id.clone());
                    }
                    let owned = matches!(&campaign.owner,
                        PrincipalOwner::Org { org_id } if Some(org_id) == owner);
                    if owner.is_some() && !owned {
                        gaps.foreign.push(campaign_id.clone());
                    }
                }
            }
            gaps
        },
    ))
}

/// The record plus its resolved and stored sets, the shape every read here returns.
pub async fn view(
    state: &AppState,
    user: User,
    org_id: Option<OrgId>,
) -> Result<UserView, AppError> {
    let stored = state
        .user_store
        .get_user_permissions(&user.user_id)
        .await
        .map_err(|e| AppError::Internal(format!("failed to read the permissions: {e:#}")))?;
    let holder = Holder::of(&user);
    Ok(UserView {
        user: user.into(),
        org_id,
        permissions: pipette_plan_types::UserPermissionSet::from(&effective(
            stored.clone(),
            holder,
        )),
        stored_permissions: Some((&stored.unwrap_or_default()).into()),
    })
}

/// Drop the stored set unless `admin` reaches the account: unknown names and
/// which names are stored are the administrator's to see, not a roster reader's.
fn administered_only(admin: Option<&AccountAdmin>, shown: &mut UserView) {
    if !admin.is_some_and(|admin| admin.scope.covers(shown.org_id.as_ref())) {
        shown.stored_permissions = None;
    }
}

/// The accounts `scope` covers with their memberships: two listings, and no
/// read per account.
pub async fn roster(
    state: &AppState,
    scope: &RosterScope,
) -> Result<Vec<(User, Option<OrgId>)>, AppError> {
    let users = state
        .user_store
        .list_users()
        .await
        .map_err(|e| AppError::Internal(format!("failed to list the accounts: {e:#}")))?;
    let memberships: HashMap<UserId, OrgId> = state
        .org_store
        .list_memberships()
        .await
        .map_err(|e| AppError::Internal(format!("failed to list the memberships: {e:#}")))?
        .into_iter()
        .map(|membership| (membership.user_id, membership.org_id))
        .collect();
    Ok(users
        .into_iter()
        .filter_map(|user| {
            let org_id = memberships.get(&user.user_id).cloned();
            scope.covers(org_id.as_ref()).then_some((user, org_id))
        })
        .collect())
}

pub async fn views(
    state: &AppState,
    accounts: Vec<(User, Option<OrgId>)>,
) -> Result<Vec<UserView>, AppError> {
    stream::iter(accounts)
        .map(|(user, org_id)| view(state, user, org_id))
        .buffered(STORAGE_CONCURRENCY)
        .try_collect()
        .await
}

/// One caller's authority over accounts, resolved once so every check reads
/// one set.
struct AccountAdmin {
    rights: UserRights,
    scope: RosterScope,
    caller: UserId,
}

impl AccountAdmin {
    /// `manage_orgs`, bounded by membership unless the caller holds
    /// `manage_any_orgs`. A `403` without it, behind the route's gate.
    async fn authorize(
        state: &AppState,
        caller: &User,
        rights: UserRights,
    ) -> Result<Self, AppError> {
        rights.require(UserPermission::ManageOrgs)?;
        Self::resolved(state, caller, rights).await
    }

    /// [`Self::authorize`] without the `403`, for a read that says what the
    /// caller may do rather than refusing it.
    async fn reach(state: &AppState, caller: Caller<'_>) -> Result<Option<Self>, AppError> {
        if !caller.rights.holds(UserPermission::ManageOrgs) {
            return Ok(None);
        }
        Self::resolved(state, caller.user, caller.rights.clone())
            .await
            .map(Some)
    }

    async fn resolved(
        state: &AppState,
        caller: &User,
        rights: UserRights,
    ) -> Result<Self, AppError> {
        let scope = crate::api_v1::orgs::reach_of(
            state,
            &caller.user_id,
            &rights,
            UserPermission::ManageAnyOrgs,
        )
        .await?;
        Ok(Self {
            rights,
            scope,
            caller: caller.user_id.clone(),
        })
    }

    /// The caller as a campaign check reads it, with the set this admin holds.
    fn as_caller<'a>(&'a self, caller: &'a User) -> Caller<'a> {
        Caller {
            user: caller,
            rights: &self.rights,
        }
    }

    /// The account `user_id` names, inside this caller's scope.
    async fn account(
        &self,
        state: &AppState,
        user_id: &UserId,
    ) -> Result<(User, Option<OrgId>), AppError> {
        in_scope(state, &self.scope, user_id).await
    }

    /// [`Self::account`], held to [`within_rank`]: every write here reaches
    /// its account through this, and every read through `account`.
    async fn writable(
        &self,
        state: &AppState,
        user_id: &UserId,
    ) -> Result<(User, Option<OrgId>), AppError> {
        let found = self.account(state, user_id).await?;
        within_rank(state, &self.rights, &found.0).await?;
        Ok(found)
    }

    /// Scope and rank over an account already read: what [`Self::writable`]
    /// enforces, for a read that says so.
    fn reaches(&self, org_id: Option<&OrgId>, stored: &UserPermissionSet) -> bool {
        self.scope.covers(org_id)
            && outranking(&self.rights, &stored_closure(Some(stored.clone()))).is_empty()
    }

    /// The names this caller may grant `target`, and the stored ones it may
    /// revoke: what the permission form offers.
    fn grants_on(
        &self,
        target: &User,
        stored: &UserPermissionSet,
    ) -> (Vec<UserPermission>, Vec<UserPermission>) {
        let own = target.user_id == self.caller;
        let kept = |name: &UserPermission| own && OWN_ADMINISTRATION.contains(name);
        let grant = <UserPermission as strum::IntoEnumIterator>::iter()
            .filter(|name| {
                self.rights.refused_grants([*name]).is_empty()
                    && Holder::of(target).may_hold(*name)
                    && !kept(name)
            })
            .collect();
        let revoke = stored.known().filter(|name| !kept(name)).collect();
        (grant, revoke)
    }

    /// Rewrite the account's stored set through `apply`, held to [`within_rank`]
    /// against the set the swap reads, so a server-wide grant landing after
    /// [`Self::writable`] is refused rather than overwritten.
    async fn swap_permissions(
        &self,
        state: &AppState,
        user_id: &UserId,
        apply: &(dyn Fn(&mut UserPermissionSet) -> bool + Send + Sync),
    ) -> Result<UserPermissionMutation, AppError> {
        let own = *user_id == self.caller;
        // The store may replay the closure, so the last run's verdict stands.
        let refusal = std::sync::Mutex::new(None);
        let mutation = state
            .user_store
            .mutate_user_permissions(user_id, &|set: &mut UserPermissionSet| {
                let verdict = self.judge_swap(own, set, apply);
                let changed = matches!(verdict, Ok(true));
                *refusal
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = verdict.err();
                changed
            })
            .await
            .map_err(|e| AppError::Internal(format!("failed to write the permissions: {e:#}")))?;
        let refusal = refusal
            .into_inner()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match refusal {
            None => Ok(mutation),
            Some(SwapRefusal::Outranked(wider)) => Err(outranked_by(user_id, &wider)),
            Some(SwapRefusal::OwnAdministration(dropped)) => {
                let names: Vec<&str> = dropped.iter().map(AsRef::as_ref).collect();
                tracing::warn!(%user_id, dropped = ?names, "refused: a caller removing its own administration");
                Err(AppError::BadRequest(format!(
                    "an account cannot remove its own {}",
                    names.join(", ")
                )))
            }
        }
    }

    /// Apply `apply` to `set` unless the write is refused, leaving `set` as it
    /// was then. `Ok` says whether the set changed.
    fn judge_swap(
        &self,
        own: bool,
        set: &mut UserPermissionSet,
        apply: &(dyn Fn(&mut UserPermissionSet) -> bool + Send + Sync),
    ) -> Result<bool, SwapRefusal> {
        let wider = outranking(&self.rights, set);
        if !wider.is_empty() {
            return Err(SwapRefusal::Outranked(wider));
        }
        let before = set.clone();
        let changed = apply(set);
        let dropped: Vec<UserPermission> = OWN_ADMINISTRATION
            .into_iter()
            .filter(|name| own && before.holds(*name) && !set.holds(*name))
            .collect();
        if dropped.is_empty() {
            return Ok(changed);
        }
        *set = before;
        Err(SwapRefusal::OwnAdministration(dropped))
    }

    fn within_ceiling(&self, wanted: &[UserPermission]) -> Result<(), AppError> {
        let refused = self.rights.refused_grants(wanted.iter().copied());
        if refused.is_empty() {
            return Ok(());
        }
        let names: Vec<&str> = refused.iter().map(AsRef::as_ref).collect();
        tracing::warn!(refused = ?names, "refused: a grant beyond the caller's own set");
        Err(AppError::Forbidden(format!(
            "you cannot grant what you do not hold: {}",
            names.join(", ")
        )))
    }

    /// `active` at once, since somebody decided it should exist. Writes in the
    /// order `docs/storage.md` gives.
    async fn create(
        &self,
        state: &AppState,
        caller: &User,
        request: CreateUserRequest,
    ) -> Result<UserView, AppError> {
        let permissions = request.permissions;
        self.within_ceiling(&permissions)?;
        let holder = Holder {
            account_type: request.account_type,
            campaign_reader: request.campaign_scope.is_some(),
        };
        if holder.campaign_reader && holder.account_type != UserAccountType::Service {
            return Err(AppError::BadRequest(
                "a campaign scope is a service account's: send account_type service".into(),
            ));
        }
        holdable(holder, &permissions)?;

        // Matched on the scope itself: `own_org()` is `None` for the fleet
        // *and* for an unaffiliated caller, and the two must not share an arm.
        let org_id = match (&self.scope, request.org_id) {
            (RosterScope::Unaffiliated, _) => {
                tracing::warn!("refused: the caller belongs to no organization");
                return Err(AppError::Forbidden(
                    "you belong to no organization, so there is nowhere to create an account"
                        .into(),
                ));
            }
            (RosterScope::Org(own), Some(named)) if named != *own => {
                tracing::warn!(org_id = %named, "refused: account creation outside the caller's scope");
                return Err(AppError::Forbidden(
                    "you may only create accounts in your own organization".into(),
                ));
            }
            (RosterScope::Org(own), _) => Some(own.clone()),
            (RosterScope::Fleet, named) => named,
        };
        match &org_id {
            // Resolved, so a membership can only name a record that exists.
            Some(org_id) => crate::api_v1::orgs::refuse_disabled(
                &pipette_pipeline::org::require(
                    &*state.org_store,
                    &pipette_pipeline::org::OrgRef::Id(org_id.clone()),
                )
                .await?,
            )?,
            None if request.account_type == UserAccountType::Service => {
                return Err(AppError::BadRequest(
                    "a service account belongs to an organization: name its org_id".into(),
                ));
            }
            None => {}
        }
        // A service account has an organization by now.
        if let (Some(scope), Some(org_id)) = (&request.campaign_scope, &org_id) {
            within_campaign_reach(state, self.as_caller(caller), scope, org_id).await?;
        }

        let created = User {
            account_type: request.account_type,
            campaign_scope: request.campaign_scope,
            ..User::try_new(request.contact_email, Utc::now())
                .map_err(|e| AppError::BadRequest(format!("invalid contact_email: {e}")))?
        };
        if let RosterScope::Org(own) = &self.scope {
            refuse_foreign_provision(state, own, &created.contact_email).await?;
        }

        match state
            .user_store
            .claim_user_email(created.contact_email.as_str(), &created.user_id)
            .await
            .map_err(|e| AppError::Internal(format!("failed to claim the address: {e:#}")))?
        {
            EmailClaim::Claimed => {}
            // A scoped caller may not learn ids outside its roster.
            EmailClaim::Taken(holder) => {
                return Err(AppError::Conflict(match self.scope {
                    RosterScope::Fleet => {
                        format!("an account already holds that address: {holder}")
                    }
                    RosterScope::Org(_) | RosterScope::Unaffiliated => {
                        "an account already holds that address".into()
                    }
                }));
            }
        }

        // The membership before the record: an account left outside its
        // organization is one its creator cannot reach.
        let written = async {
            if let Some(org_id) = &org_id {
                state
                    .org_store
                    .set_membership(&created.user_id, Some(org_id.clone()))
                    .await?;
            }
            // Before the record, so an account that exists can be onboarded.
            state
                .user_store
                .mark_user_awaiting_sign_in(&created.user_id)
                .await?;
            state.user_store.put_user(&created).await
        }
        .await;
        if let Err(e) = written {
            if let Err(release) = state
                .user_store
                .release_user_email(created.contact_email.as_str())
                .await
            {
                tracing::warn!(
                    user_id = %created.user_id,
                    error = %release,
                    "failed to release the address claim after a create failed; \
                     `users release-claim <email>` will clear it"
                );
            }
            if org_id.is_some()
                && let Err(delete) = state.org_store.delete_membership(&created.user_id).await
            {
                tracing::warn!(
                    user_id = %created.user_id,
                    error = %delete,
                    "failed to delete the membership after a create failed"
                );
            }
            if let Err(clear) = state
                .user_store
                .clear_user_awaiting_sign_in(&created.user_id)
                .await
            {
                tracing::warn!(
                    user_id = %created.user_id,
                    error = %clear,
                    "failed to clear the first sign-in mark after a create failed"
                );
            }
            return Err(AppError::Internal(format!(
                "failed to write the account: {e:#}"
            )));
        }

        // Only once the record exists: an object for an id no record answers
        // to is one a delete would never reach.
        let set = UserPermissionSet::from_iter(permissions).normalized();
        if !set.is_empty() {
            state
                .user_store
                .put_user_permissions(&created.user_id, &set)
                .await
                .map_err(|e| {
                    AppError::Internal(format!("the account was created but holds nothing: {e:#}"))
                })?;
        }

        tracing::info!(
            user_id = %created.user_id,
            org_id = org_id.as_ref().map(tracing::field::display),
            granted = ?set.names(),
            "user created"
        );
        view(state, created, org_id).await
    }

    /// An account cannot take itself out of service, and a campaign reader
    /// stays one: its scope is replaced, never removed.
    async fn update(
        &self,
        state: &AppState,
        caller: &User,
        user_id: &UserId,
        request: UpdateUserRequest,
    ) -> Result<Changed<UserView>, AppError> {
        if request.status.is_none()
            && request.account_type.is_none()
            && request.campaign_scope.is_none()
        {
            return Err(AppError::BadRequest(
                "name a status, an account_type or a campaign_scope to change".into(),
            ));
        }
        let status = request.status;
        let account_type = request.account_type;
        let campaign_scope = request.campaign_scope;
        let (mut target, org_id) = self.writable(state, user_id).await?;
        let reader = campaign_scope.is_some() || target.campaign_scope.is_some();
        if reader && account_type.unwrap_or(target.account_type) != UserAccountType::Service {
            return Err(AppError::BadRequest(
                "a campaign reader is a service account; a person's account is a new account"
                    .into(),
            ));
        }
        if let Some(scope) = &campaign_scope {
            if target.campaign_scope.is_none() {
                holds_only_holdable(state, user_id, Holder::campaign_reader()).await?;
            }
            let org = org_id.as_ref().ok_or_else(|| {
                AppError::BadRequest("a campaign reader belongs to an organization".into())
            })?;
            within_campaign_reach(state, self.as_caller(caller), scope, org).await?;
        }
        if target.user_id == self.caller
            && status.is_some_and(|status| status != UserStatus::Active)
        {
            return Err(AppError::BadRequest(
                "an account cannot disable itself".into(),
            ));
        }
        // Its administrator may mint a service account a key at any time, so
        // retyping a person's account it does not hold would be a takeover.
        if account_type == Some(UserAccountType::Service)
            && target.account_type != UserAccountType::Service
        {
            if org_id.is_none() {
                return Err(AppError::BadRequest(
                    "a service account belongs to an organization: assign one first".into(),
                ));
            }
            holds_only_holdable(state, user_id, Holder::unscoped(UserAccountType::Service)).await?;
            if !awaits_sign_in(state, user_id).await? {
                tracing::warn!(%user_id, "refused: retyping a signed-in account as a service");
                return Err(AppError::Conflict(
                    "somebody has signed in as this account, so it stays a person's here; the \
                     host's `users set-type` covers the rest"
                        .into(),
                ));
            }
        }
        let stored = (
            target.status,
            target.account_type,
            target.campaign_scope.clone(),
        );
        target.status = status.unwrap_or(target.status);
        target.account_type = account_type.unwrap_or(target.account_type);
        target.campaign_scope = campaign_scope.or(target.campaign_scope);
        // Nothing is written for a no-op, so it cannot revert a concurrent edit
        // with the copy read above.
        let changed = (
            target.status,
            target.account_type,
            target.campaign_scope.clone(),
        ) != stored;
        if changed {
            state
                .user_store
                .put_user(&target)
                .await
                .map_err(|e| AppError::Internal(format!("failed to write the user: {e:#}")))?;
            tracing::info!(%user_id, status = %target.status, account_type = %target.account_type,
                campaign_scope = target.campaign_scope.as_ref().map(BTreeSet::len),
                "user updated");
        }
        Ok(Changed {
            record: view(state, target, org_id).await?,
            changed,
        })
    }

    /// An empty list clears the set.
    async fn set_permissions(
        &self,
        state: &AppState,
        user_id: &UserId,
        permissions: Vec<UserPermission>,
    ) -> Result<Changed<UserView>, AppError> {
        let (target, org_id) = self.writable(state, user_id).await?;
        self.within_ceiling(&permissions)?;
        let holder = Holder::of(&target);
        holdable(holder, &permissions)?;
        let set = UserPermissionSet::from_iter(permissions).normalized();
        let apply = |stored: &mut UserPermissionSet| {
            let changed = *stored != set;
            *stored = set.clone();
            changed
        };
        let mutation = self.swap_permissions(state, user_id, &apply).await?;
        tracing::info!(%user_id, granted = ?set.names(), changed = mutation.changed,
            "user permissions set");
        Ok(Changed {
            record: UserView {
                user: target.into(),
                org_id,
                permissions: pipette_plan_types::UserPermissionSet::from(&effective(
                    Some(mutation.resulting.clone()),
                    holder,
                )),
                stored_permissions: Some((&mutation.resulting).into()),
            },
            changed: mutation.changed,
        })
    }

    /// A read-modify-write, so concurrent amends keep each other's work. The
    /// ceiling bounds `add` only: an org admin must be able to revoke a grant it
    /// did not make.
    async fn amend_permissions(
        &self,
        state: &AppState,
        user_id: &UserId,
        add: Vec<UserPermission>,
        remove: Vec<UserPermission>,
    ) -> Result<UserPermissionsAmended, AppError> {
        let (target, org_id) = self.writable(state, user_id).await?;
        if add.is_empty() && remove.is_empty() {
            return Err(AppError::BadRequest(
                "nothing to amend: send add, remove, or both".into(),
            ));
        }
        self.within_ceiling(&add)?;
        let holder = Holder::of(&target);
        holdable(holder, &add)?;

        // The stored set, as `users permission add` / `remove` write it: an
        // unknown name is carried through.
        let apply = |set: &mut UserPermissionSet| {
            let before = set.clone();
            set.remove_all(remove.iter().map(AsRef::as_ref));
            set.insert_all(add.iter().copied());
            *set = set.normalized();
            *set != before
        };
        let mutation = self.swap_permissions(state, user_id, &apply).await?;
        tracing::info!(%user_id, ?add, ?remove, changed = mutation.changed,
            "user permissions amended");

        let still_granted = remove
            .into_iter()
            .filter(|permission| mutation.resulting.holds(*permission))
            .collect();
        Ok(UserPermissionsAmended {
            view: UserView {
                user: target.into(),
                org_id,
                permissions: pipette_plan_types::UserPermissionSet::from(&effective(
                    Some(mutation.resulting.clone()),
                    holder,
                )),
                stored_permissions: Some((&mutation.resulting).into()),
            },
            changed: mutation.changed,
            still_granted,
        })
    }

    async fn keys(&self, state: &AppState, user_id: &UserId) -> Result<Vec<UserKey>, AppError> {
        self.account(state, user_id).await?;
        state
            .user_store
            .list_user_keys(user_id)
            .await
            .map_err(|e| AppError::Internal(format!("failed to list the keys: {e:#}")))
    }

    /// The caller's own keys included, as `DELETE /api/v1/users/me/keys/{key_id}`
    /// allows.
    async fn revoke_key(
        &self,
        state: &AppState,
        user_id: &UserId,
        key_id: &UserKeyId,
    ) -> Result<(), AppError> {
        self.writable(state, user_id).await?;
        crate::api_v1::users::own::revoke_key(state, user_id, key_id, &self.caller).await
    }

    async fn preauth_keys(
        &self,
        state: &AppState,
        user_id: &UserId,
    ) -> Result<Vec<UserPreauthKeyView>, AppError> {
        self.account(state, user_id).await?;
        let now = Utc::now();
        let mut keys: Vec<UserPreauthKey> = naming(state, user_id).await?;
        keys.sort_by_key(|key| std::cmp::Reverse(key.created_at));
        Ok(keys.into_iter().map(|key| key.view(now)).collect())
    }

    /// Held to [`onboardable`] now and again at registration ([`admit_delegated`]),
    /// and replaces any key minted here before it, so one is outstanding. Two
    /// mints racing can each revoke the other's, leaving none: mint again.
    async fn mint_preauth_key(
        &self,
        state: &AppState,
        caller: &User,
        user_id: &UserId,
        request: MintUserPreauthRequest,
    ) -> Result<UserPreauthKeyMinted, AppError> {
        let days = validated_days(&request)?;
        let (target, _) = self.writable(state, user_id).await?;
        onboardable(state, self.as_caller(caller), user_id).await?;

        let now = Utc::now();
        let mut minted = user_preauth::mint(
            UserPreauthParams {
                user_id: target.user_id.clone(),
                usage: UserPreauthUsage::SingleUse,
                expires_at: Some(now + chrono::Duration::days(i64::from(days))),
                note: request.note,
            },
            now,
        )
        .map_err(|e| AppError::Internal(format!("failed to mint the pre-auth key: {e}")))?;
        minted.key.minted_by = Some(self.caller.clone());
        state
            .user_store
            .put_user_preauth_key(&minted.key)
            .await
            .map_err(|e| AppError::Internal(format!("failed to store the pre-auth key: {e:#}")))?;

        // Best-effort: the key is minted either way, and one left behind is
        // refused at registration once the account holds a key.
        let superseded = match naming(state, user_id).await {
            Ok(keys) => keys,
            Err(e) => {
                e.log_if_server_error();
                Vec::new()
            }
        };
        let superseded = superseded
            .into_iter()
            .filter(|key| key.minted_by.is_some() && key.key_id != minted.key.key_id)
            .map(|key| key.key_id);
        stream::iter(superseded)
            .for_each(|key_id| async move {
                if let Err(e) = state.user_store.delete_user_preauth_key(&key_id).await {
                    tracing::warn!(%user_id, %key_id, error = %format!("{e:#}"),
                        "failed to revoke a superseded user pre-auth key");
                }
            })
            .await;

        tracing::info!(%user_id, key_id = %minted.key.key_id, "user pre-auth key minted");
        Ok(UserPreauthKeyMinted {
            key: minted.key.view(now),
            token: pipette_plan_types::UserPreauthKey::new(minted.token.expose()),
        })
    }

    async fn revoke_preauth_key(
        &self,
        state: &AppState,
        user_id: &UserId,
        key_id: &UserPreauthKeyId,
    ) -> Result<(), AppError> {
        self.writable(state, user_id).await?;
        if !naming(state, user_id)
            .await?
            .iter()
            .any(|key| &key.key_id == key_id)
        {
            return Err(AppError::NotFound(
                "no such pre-auth key on this account".into(),
            ));
        }
        state
            .user_store
            .delete_user_preauth_key(key_id)
            .await
            .map_err(|e| AppError::Internal(format!("failed to revoke the pre-auth key: {e:#}")))?;
        tracing::info!(%user_id, %key_id, "user pre-auth key revoked");
        Ok(())
    }
}

/// Why [`onboardable`] refused, so each caller chooses what to tell whom.
enum NotOnboardable {
    /// The mark is gone: a key registration or a Clerk sign-in cleared it, or
    /// the account predates it.
    SignedIn,
    HoldsKey,
    /// Stored names this build cannot compare with the minter's.
    Unknown(Vec<String>),
    /// Names the account holds and the minter does not.
    Wider(Vec<UserPermission>),
    /// Campaigns a campaign reader reads and the minter does not.
    Unread(Vec<CampaignId>),
    Failed(AppError),
}

impl From<NotOnboardable> for AppError {
    fn from(refusal: NotOnboardable) -> Self {
        match refusal {
            NotOnboardable::SignedIn => AppError::Conflict(
                "the account has signed in, or was created before first sign-ins were recorded: \
                 a pre-auth key minted here admits only an account nobody has signed in as, and \
                 the host's `users preauth create` covers the rest"
                    .into(),
            ),
            NotOnboardable::HoldsKey => AppError::Conflict(
                "the account already holds a key: a pre-auth key minted here admits only its \
                 first machine, and its holder adds the others itself"
                    .into(),
            ),
            NotOnboardable::Unknown(names) => AppError::Forbidden(format!(
                "the account holds names this build does not know, so it cannot compare them \
                 with yours: {}",
                names.join(", ")
            )),
            NotOnboardable::Wider(names) => AppError::Forbidden(format!(
                "a pre-auth key signs in as the account, and it holds what you do not: {}",
                names
                    .iter()
                    .map(AsRef::as_ref)
                    .collect::<Vec<&str>>()
                    .join(", ")
            )),
            NotOnboardable::Unread(campaigns) => AppError::Forbidden(format!(
                "a pre-auth key signs in as the account, and it reads campaigns you do not: {}",
                campaigns
                    .iter()
                    .map(CampaignId::as_str)
                    .collect::<Vec<&str>>()
                    .join(", ")
            )),
            NotOnboardable::Failed(e) => e,
        }
    }
}

/// Whether a mint here could admit the account's first machine: nobody has
/// signed in as it and it holds no key, the two checks [`onboardable`] makes first.
async fn awaits_sign_in(state: &AppState, user_id: &UserId) -> Result<bool, AppError> {
    let (marked, keys) = futures::try_join!(
        state.user_store.user_awaits_sign_in(user_id),
        state.user_store.list_user_keys(user_id),
    )
    .map_err(|e| AppError::Internal(format!("failed to read the sign-in state: {e:#}")))?;
    Ok(marked && keys.is_empty())
}

/// Refuse unless the account is a service account or nobody has signed in as
/// it, and it holds no name `rights` lacks or this build cannot compare: a key
/// signs as the account, so this stops a takeover.
async fn onboardable(
    state: &AppState,
    minter: Caller<'_>,
    user_id: &UserId,
) -> Result<(), NotOnboardable> {
    let rights = minter.rights;
    let failed = |what: &str, e: anyhow::Error| {
        NotOnboardable::Failed(AppError::Internal(format!("failed to {what}: {e:#}")))
    };
    // A service account is its organization's, so its administrator may hand
    // the job a key at any time; a person's account only until they sign in.
    let account = state
        .user_store
        .get_user(user_id)
        .await
        .map_err(|e| failed("read the account", e))?;
    let service = account
        .as_ref()
        .is_some_and(|user| user.account_type == UserAccountType::Service);
    if !service {
        // An account created before the mark existed carries none, so it is
        // refused rather than trusted: its first sign-in may have been through
        // Clerk.
        let awaits = state
            .user_store
            .user_awaits_sign_in(user_id)
            .await
            .map_err(|e| failed("read the first sign-in mark", e))?;
        if !awaits {
            return Err(NotOnboardable::SignedIn);
        }
        let keys = state
            .user_store
            .list_user_keys(user_id)
            .await
            .map_err(|e| failed("list the keys", e))?;
        if !keys.is_empty() {
            return Err(NotOnboardable::HoldsKey);
        }
    }
    let held = stored_closure(
        state
            .user_store
            .get_user_permissions(user_id)
            .await
            .map_err(|e| failed("read the permissions", e))?,
    );
    let unknown: Vec<String> = held
        .iter()
        .filter(|member| matches!(member, StoredUserPermission::Unknown(_)))
        .map(|member| member.wire_name().to_string())
        .collect();
    if !unknown.is_empty() {
        tracing::warn!(%user_id, ?unknown, "refused: a pre-auth key for names this build cannot compare");
        return Err(NotOnboardable::Unknown(unknown));
    }
    let refused = rights.refused_grants(held.known());
    if !refused.is_empty() {
        let names: Vec<&str> = refused.iter().map(AsRef::as_ref).collect();
        tracing::warn!(%user_id, refused = ?names,
            "refused: a pre-auth key for an account wider than its minter");
        return Err(NotOnboardable::Wider(refused));
    }
    // A deleted campaign reads nothing, so only one the minter cannot read refuses.
    if let Some(scope) = account.and_then(|user| user.campaign_scope) {
        let unread = scope_gaps(state, minter, &scope, None)
            .await
            .map_err(NotOnboardable::Failed)?
            .unread;
        if !unread.is_empty() {
            tracing::warn!(%user_id, ?unread,
                "refused: a pre-auth key for a campaign reader beyond its minter");
            return Err(NotOnboardable::Unread(unread));
        }
    }
    Ok(())
}

/// Refuse a registration with a key its minter could not mint now, so a kept copy,
/// a widened account or a removed minter admits nothing.
pub async fn admit_delegated(
    state: &AppState,
    minter: &UserId,
    user_id: &UserId,
) -> Result<(), AppError> {
    let refused = |reason: &str| {
        tracing::warn!(%user_id, %minter, reason, "refused a registration with a pre-auth key its minter no longer vouches for");
        AppError::Forbidden(
            "this pre-auth key no longer admits the account; ask its administrator for a new one"
                .into(),
        )
    };
    let active = state
        .user_store
        .get_user(minter)
        .await
        .map_err(|e| AppError::Internal(format!("failed to read the minter: {e:#}")))?
        .filter(|minter| minter.status == UserStatus::Active);
    let Some(active) = active else {
        return Err(refused("the minter is gone or not active"));
    };
    let rights = UserRights::of_account(state, &active).await?;
    let as_minter = Caller {
        user: &active,
        rights: &rights,
    };
    let Some(admin) = AccountAdmin::reach(state, as_minter).await? else {
        return Err(refused("the minter no longer holds manage_orgs"));
    };
    if !admin.scope.covers(user_org(state, user_id).await?.as_ref()) {
        return Err(refused("the account is outside the minter's scope"));
    }
    let as_admin = Caller {
        user: &active,
        rights: &admin.rights,
    };
    match onboardable(state, as_admin, user_id).await {
        Ok(()) => Ok(()),
        // Somebody signed in first, by key or by Clerk: the one refusal worth
        // naming to the holder.
        Err(refusal @ (NotOnboardable::SignedIn | NotOnboardable::HoldsKey)) => {
            tracing::warn!(%user_id, %minter, "refused a registration with a pre-auth key for an account somebody has signed in as");
            Err(refusal.into())
        }
        Err(NotOnboardable::Failed(e)) => Err(e),
        Err(NotOnboardable::Unknown(_)) => {
            Err(refused("the account holds names this build cannot compare"))
        }
        Err(NotOnboardable::Wider(_)) => {
            Err(refused("the account now holds what the minter does not"))
        }
        Err(NotOnboardable::Unread(_)) => {
            Err(refused("the account reads campaigns the minter does not"))
        }
    }
}

/// The store keys pre-auth keys by id alone, so this reads the operator-sized
/// keyspace.
async fn naming(state: &AppState, user_id: &UserId) -> Result<Vec<UserPreauthKey>, AppError> {
    Ok(state
        .user_store
        .list_user_preauth_keys()
        .await
        .map_err(|e| AppError::Internal(format!("failed to list the pre-auth keys: {e:#}")))?
        .into_iter()
        .filter(|key| &key.user_id == user_id)
        .collect())
}

// GET /api/v1/users
pub async fn list_users(
    State(state): State<AppState>,
    active: ActiveUser,
) -> Result<impl IntoResponse, AppError> {
    let caller = active.caller();
    let scope = read_reach(&state, caller).await?;
    let accounts = roster(&state, &scope).await?;
    let admin = AccountAdmin::reach(&state, caller).await?;
    let mut users = views(&state, accounts).await?;
    for shown in &mut users {
        administered_only(admin.as_ref(), shown);
    }
    Ok(Json(ListPage::whole(users, 0)))
}

// GET /api/v1/users/{user_id}
pub async fn get_user(
    State(state): State<AppState>,
    active: ActiveUser,
    ApiPath(Parsed(user_id)): ApiPath<Parsed<UserId>>,
) -> Result<impl IntoResponse, AppError> {
    let caller = active.caller();
    let scope = read_reach(&state, caller).await?;
    let (target, org_id) = scoped(&state, &scope, &user_id, false).await?;
    let mut account = view(&state, target.clone(), org_id).await?;
    let admin = AccountAdmin::reach(&state, caller).await?;
    administered_only(admin.as_ref(), &mut account);
    let (stored, awaits) = match &admin {
        Some(admin) if admin.scope.covers(account.org_id.as_ref()) => {
            let (stored, awaits) = futures::try_join!(
                async {
                    state
                        .user_store
                        .get_user_permissions(&user_id)
                        .await
                        .map_err(|e| {
                            AppError::Internal(format!("failed to read the permissions: {e:#}"))
                        })
                },
                awaits_sign_in(&state, &user_id),
            )?;
            (Some(stored.unwrap_or_default()), Some(awaits))
        }
        _ => (None, None),
    };
    let reached = match (&admin, &stored) {
        (Some(admin), Some(stored)) if admin.reaches(account.org_id.as_ref(), stored) => {
            Some((admin, stored))
        }
        _ => None,
    };
    let may_administer = reached.is_some();
    let (may_grant, may_revoke) = reached
        .map(|(admin, stored)| admin.grants_on(&target, stored))
        .unwrap_or_default();
    Ok(Json(UserDetail {
        account,
        may_administer,
        may_grant,
        may_revoke,
        awaits_sign_in: awaits,
    }))
}

// POST /api/v1/users
pub async fn create_user(
    State(state): State<AppState>,
    HumanUser { user, rights, .. }: HumanUser,
    audit: Audit,
    ApiJson(body): ApiJson<CreateUserRequest>,
) -> Result<impl IntoResponse, AppError> {
    let admin = AccountAdmin::authorize(&state, &user, rights).await?;
    let created = admin.create(&state, &user, body).await?;
    let made = AuditTarget::User {
        user_id: created.user.user_id.clone(),
    };
    audit.involve(&state, [made]).await;
    Ok((StatusCode::CREATED, Json(created)))
}

// PATCH /api/v1/users/{user_id}
pub async fn update_user(
    State(state): State<AppState>,
    HumanUser { user, rights, .. }: HumanUser,
    audit: Audit,
    ApiPath(Parsed(user_id)): ApiPath<Parsed<UserId>>,
    ApiJson(body): ApiJson<UpdateUserRequest>,
) -> Result<impl IntoResponse, AppError> {
    let admin = AccountAdmin::authorize(&state, &user, rights).await?;
    let updated = admin.update(&state, &user, &user_id, body).await?;
    audit.changed(updated.changed);
    Ok(Json(updated))
}

// PUT /api/v1/users/{user_id}/permissions
pub async fn set_permissions(
    State(state): State<AppState>,
    HumanUser { user, rights, .. }: HumanUser,
    audit: Audit,
    ApiPath(Parsed(user_id)): ApiPath<Parsed<UserId>>,
    ApiJson(body): ApiJson<SetUserPermissionsRequest>,
) -> Result<impl IntoResponse, AppError> {
    let admin = AccountAdmin::authorize(&state, &user, rights).await?;
    let set = admin
        .set_permissions(&state, &user_id, body.permissions)
        .await?;
    audit.changed(set.changed);
    Ok(Json(set))
}

// PATCH /api/v1/users/{user_id}/permissions
pub async fn amend_permissions(
    State(state): State<AppState>,
    HumanUser { user, rights, .. }: HumanUser,
    audit: Audit,
    ApiPath(Parsed(user_id)): ApiPath<Parsed<UserId>>,
    ApiJson(body): ApiJson<AmendUserPermissionsRequest>,
) -> Result<impl IntoResponse, AppError> {
    let admin = AccountAdmin::authorize(&state, &user, rights).await?;
    let amended = admin
        .amend_permissions(&state, &user_id, body.add, body.remove)
        .await?;
    audit.changed(amended.changed);
    Ok(Json(amended))
}

// GET /api/v1/users/{user_id}/keys
pub async fn list_user_keys(
    State(state): State<AppState>,
    HumanUser { user, rights, .. }: HumanUser,
    ApiPath(Parsed(user_id)): ApiPath<Parsed<UserId>>,
) -> Result<impl IntoResponse, AppError> {
    let keys = AccountAdmin::authorize(&state, &user, rights)
        .await?
        .keys(&state, &user_id)
        .await?;
    let keys: Vec<pipette_plan_types::UserKey> = keys.into_iter().map(Into::into).collect();
    Ok(Json(ListPage::whole(keys, 0)))
}

// DELETE /api/v1/users/{user_id}/keys/{key_id}
pub async fn revoke_user_key(
    State(state): State<AppState>,
    HumanUser { user, rights, .. }: HumanUser,
    ApiPath((Parsed(user_id), Parsed(key_id))): ApiPath<(Parsed<UserId>, Parsed<UserKeyId>)>,
) -> Result<impl IntoResponse, AppError> {
    AccountAdmin::authorize(&state, &user, rights)
        .await?
        .revoke_key(&state, &user_id, &key_id)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

// GET /api/v1/users/{user_id}/preauth-keys
pub async fn list_preauth_keys(
    State(state): State<AppState>,
    HumanUser { user, rights, .. }: HumanUser,
    ApiPath(Parsed(user_id)): ApiPath<Parsed<UserId>>,
) -> Result<impl IntoResponse, AppError> {
    let preauth_keys = AccountAdmin::authorize(&state, &user, rights)
        .await?
        .preauth_keys(&state, &user_id)
        .await?;
    Ok(Json(ListPage::whole(preauth_keys, 0)))
}

// POST /api/v1/users/{user_id}/preauth-keys
pub async fn mint_preauth_key(
    State(state): State<AppState>,
    HumanUser { user, rights, .. }: HumanUser,
    ApiPath(Parsed(user_id)): ApiPath<Parsed<UserId>>,
    ApiJson(body): ApiJson<MintUserPreauthRequest>,
) -> Result<impl IntoResponse, AppError> {
    let admin = AccountAdmin::authorize(&state, &user, rights).await?;
    let minted = admin
        .mint_preauth_key(&state, &user, &user_id, body)
        .await?;
    // The answer is a credential, so nothing on the way back keeps a copy.
    Ok((
        StatusCode::CREATED,
        [(header::CACHE_CONTROL, "no-store")],
        Json(minted),
    ))
}

// DELETE /api/v1/users/{user_id}/preauth-keys/{key_id}
pub async fn revoke_preauth_key(
    State(state): State<AppState>,
    HumanUser { user, rights, .. }: HumanUser,
    ApiPath((Parsed(user_id), Parsed(key_id))): ApiPath<(Parsed<UserId>, Parsed<UserPreauthKeyId>)>,
) -> Result<impl IntoResponse, AppError> {
    AccountAdmin::authorize(&state, &user, rights)
        .await?
        .revoke_preauth_key(&state, &user_id, &key_id)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

// PUT /api/v1/users/{user_id}/org
pub async fn set_user_org(
    State(state): State<AppState>,
    HumanUser {
        user: caller,
        rights,
        ..
    }: HumanUser,
    audit: Audit,
    ApiPath(Parsed(user_id)): ApiPath<Parsed<UserId>>,
    ApiJson(body): ApiJson<SetUserOrgRequest>,
) -> Result<impl IntoResponse, AppError> {
    let assigned = assign_membership(&state, &caller, &rights, &user_id, body.org_id).await?;
    audit.changed(assigned.changed);
    Ok(Json(assigned))
}
