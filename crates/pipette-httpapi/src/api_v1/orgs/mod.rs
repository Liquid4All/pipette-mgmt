//! HTTP handlers for the organization routes: the roster CRUD under `/api/v1/orgs`,
//! plus `PUT /api/v1/users/{user_id}/org` and
//! `PUT /api/v1/clients/{client_id}/owner`, which both resolve an organization first.

pub mod catalog;
pub mod client_preauth;
pub mod reads;

use std::collections::{HashMap, HashSet};
use std::str::FromStr;

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use chrono::Utc;
use futures::{StreamExt, TryStreamExt};

use pipette_core::client::Client;
use pipette_core::organization::{Organization, Ownership};
use pipette_core::user::User;
use pipette_pipeline::org;
use pipette_pipeline::org::{DeviceScope, OrgRef, OrgRefError, RosterScope};
use pipette_plan_types::{
    AssignClientsRequest, AssignedClient, AuditTarget, Changed, ClientId, ClientOwner, ClientView,
    ClientsAssigned, CreateOrgRequest, ListPage, NonEmptyTrimmedString, OrgId, OrgListQuery,
    OrgSlug, OrgStatus, OrgUserRow, OrgView, UpdateOrgRequest, UserAccountType, UserId,
    UserPermission,
};
use pipette_store::SlugClaim;

use crate::api_v1::clients::{assignable_org, device_reach, device_scope, in_scope};
use crate::audit::Audit;
use crate::auth::{ActiveUser, Caller, HumanUser, UserRights, user_org};
use crate::error::AppError;
use crate::extract::{ApiJson, ApiPath, ApiQuery, Parsed};
use crate::state::AppState;

/// Where a device is put down, as the scope check reads it. A user owner folds
/// into that person's organization, which may be none: not the same as detaching.
enum Destination {
    Detach,
    Org(Option<OrgId>),
    /// An owner no record answers to, carrying the `404` a caller hears once
    /// its scope admits the move.
    Absent(AppError),
}

/// The fleet for a holder of `any`, otherwise the organization `user_id`
/// belongs to: the one derivation every scope here shares.
pub async fn reach_of(
    state: &AppState,
    user_id: &UserId,
    rights: &UserRights,
    any: UserPermission,
) -> Result<RosterScope, AppError> {
    if rights.holds(any) {
        return Ok(RosterScope::Fleet);
    }
    Ok(user_org(state, user_id)
        .await?
        .map_or(RosterScope::Unaffiliated, RosterScope::Org))
}

/// How far this caller's roster authority reaches: the fleet when it holds
/// `any`, otherwise its own membership ([`RosterScope::Unaffiliated`] for none).
pub async fn roster_reach(
    state: &AppState,
    caller: Caller<'_>,
    bare: UserPermission,
    any: UserPermission,
) -> Result<RosterScope, AppError> {
    let user = caller.user;
    let rights = caller.rights;
    // `any` implies `bare`, so one check covers both.
    if !rights.holds(bare) {
        return Ok(RosterScope::Unaffiliated);
    }
    reach_of(state, &user.user_id, rights, any).await
}

/// Refuse a new member for a disabled organization: it keeps the members it
/// has, and takes no more until it is enabled.
pub(crate) fn refuse_disabled(org: &Organization) -> Result<(), AppError> {
    if org.status == OrgStatus::Disabled {
        tracing::warn!(org_id = %org.org_id, "refused: a new member for a disabled organization");
        return Err(AppError::Conflict(format!(
            "organization {} is disabled and takes no new members",
            org.slug
        )));
    }
    Ok(())
}

/// An organization path segment on a read, by handle or id ([`OrgRef`]); a
/// mutation takes `Parsed<OrgId>` instead. A refusal leads with
/// `invalid organization reference`, as `OrgRefError` does not.
pub struct OrgPath(pub OrgRef);

#[derive(Debug, thiserror::Error)]
#[error("invalid organization reference: {0}")]
pub struct OrgPathError(#[from] OrgRefError);

impl FromStr for OrgPath {
    type Err = OrgPathError;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        Ok(Self(raw.parse()?))
    }
}

/// One organization's counts, from the two relation listings, counted as
/// `GET /api/v1/orgs` counts.
pub(crate) async fn counts_of(
    state: &AppState,
    org: &Organization,
) -> Result<org::OrgCounts, AppError> {
    let (memberships, ownerships) = futures::try_join!(
        state.org_store.list_memberships(),
        state.org_store.list_ownerships(),
    )
    .map_err(|e| AppError::Internal(format!("failed to read the relations: {e:#}")))?;
    Ok(
        org::tally(std::slice::from_ref(org), &memberships, &ownerships)
            .remove(&org.org_id)
            .unwrap_or_default(),
    )
}

async fn counted(state: &AppState, org: Organization) -> Result<OrgView, AppError> {
    let counts = counts_of(state, &org).await?;
    Ok(OrgView {
        org: org.into(),
        member_count: Some(counts.members),
        client_count: Some(counts.clients),
    })
}

/// The organization a path names, refused when outside `scope`. Absent and out
/// of scope both read `404`, so an org admin cannot map the roster by probing.
pub(crate) async fn require_in_scope(
    state: &AppState,
    scope: &RosterScope,
    reference: &OrgRef,
) -> Result<Organization, AppError> {
    let org = org::require(&*state.org_store, reference).await?;
    if !scope.covers(Some(&org.org_id)) {
        tracing::warn!(
            org_id = %org.org_id,
            "refused: the organization is outside the caller's scope"
        );
        return Err(AppError::OutOfScope(
            org::OrgLookupError::NotFound(reference.clone()).to_string(),
        ));
    }
    Ok(org)
}

/// The organizations `scope` reaches, in handle order.
pub async fn orgs_in(state: &AppState, scope: &RosterScope) -> Result<Vec<Organization>, AppError> {
    let read = match scope {
        RosterScope::Fleet => state.org_store.list_orgs().await,
        RosterScope::Org(org_id) => state
            .org_store
            .get_org(org_id)
            .await
            .map(|org| org.into_iter().collect()),
        RosterScope::Unaffiliated => Ok(Vec::new()),
    };
    let mut orgs =
        read.map_err(|e| AppError::internal(e.context("failed to read the organizations")))?;
    orgs.sort_by(|a, b| a.slug.as_str().cmp(b.slug.as_str()));
    Ok(orgs)
}

// GET /api/v1/orgs
pub async fn list_orgs(
    State(state): State<AppState>,
    active: ActiveUser,
    ApiQuery(query): ApiQuery<OrgListQuery>,
) -> Result<impl IntoResponse, AppError> {
    let caller = active.caller();
    Ok(Json(ListPage::whole(
        listing(&state, caller, query.counts.unwrap_or(true)).await?,
        0,
    )))
}

/// The organizations `caller` reads, each with its counts when `counted`.
/// Filtered rather than refused: a scoped reader gets a one-row listing.
async fn listing(
    state: &AppState,
    caller: Caller<'_>,
    counted: bool,
) -> Result<Vec<OrgView>, AppError> {
    // One read of each relation keyspace for the whole listing, not one per row.
    // All at once, the scope still checked first.
    let (scope, orgs, relations) = tokio::join!(
        roster_reach(
            state,
            caller,
            UserPermission::ReadOrgs,
            UserPermission::ReadAnyOrgs,
        ),
        state.org_store.list_orgs(),
        async {
            if !counted {
                return Ok(None);
            }
            futures::try_join!(
                state.org_store.list_memberships(),
                state.org_store.list_ownerships(),
            )
            .map(Some)
            .map_err(|e| AppError::Internal(format!("failed to read the relations: {e:#}")))
        },
    );
    let scope = scope?;
    let orgs: Vec<Organization> = orgs
        .map_err(|e| AppError::Internal(format!("failed to list the organizations: {e:#}")))?
        .into_iter()
        .filter(|org| scope.covers(Some(&org.org_id)))
        .collect();

    let counts =
        relations?.map(|(memberships, ownerships)| org::tally(&orgs, &memberships, &ownerships));
    Ok(orgs
        .into_iter()
        .map(|org| {
            let tallied = counts
                .as_ref()
                .map(|counts| counts.get(&org.org_id).copied().unwrap_or_default());
            OrgView {
                member_count: tallied.map(|t| t.members),
                client_count: tallied.map(|t| t.clients),
                org: org.into(),
            }
        })
        .collect())
}

// POST /api/v1/orgs
pub async fn create_org(
    State(state): State<AppState>,
    caller: HumanUser,
    audit: Audit,
    ApiJson(body): ApiJson<CreateOrgRequest>,
) -> Result<impl IntoResponse, AppError> {
    // Re-checked here, so loosening the route's gate cannot open the create.
    caller.rights.require(UserPermission::ManageAnyOrgs)?;
    let created = create(&state, body.slug, body.display_name).await?;
    let made = AuditTarget::Org {
        org_id: created.org.org_id.clone(),
    };
    audit.involve(&state, [made]).await;
    Ok((StatusCode::CREATED, Json(created)))
}

/// Create an organization under `slug`. The handle claim comes first and is
/// exclusive, so two operators creating one organization produce one.
pub async fn create(
    state: &AppState,
    slug: OrgSlug,
    display_name: Option<NonEmptyTrimmedString>,
) -> Result<OrgView, AppError> {
    let display_name = match display_name {
        Some(name) => name,
        // Infallible in practice: a slug's invariant is `NonEmptyTrimmedString`'s.
        None => NonEmptyTrimmedString::try_new(slug.as_str())
            .map_err(|e| AppError::Internal(format!("a slug is a non-empty string: {e}")))?,
    };
    let org = Organization::new(slug, display_name, Utc::now());

    match state
        .org_store
        .claim_org_slug(org.slug.as_str(), &org.org_id)
        .await
        .map_err(|e| AppError::Internal(format!("failed to claim the handle: {e:#}")))?
    {
        SlugClaim::Claimed => {}
        SlugClaim::Taken(holder) => {
            return Err(AppError::Conflict(format!(
                "the handle {} is already held by {holder}",
                org.slug
            )));
        }
    }

    // A failure releases the handle rather than stranding it: the caller is an
    // operator who will simply retry.
    if let Err(e) = state.org_store.put_org(&org).await {
        if let Err(release) = state.org_store.release_org_slug(org.slug.as_str()).await {
            tracing::warn!(
                org_id = %org.org_id,
                slug = %org.slug,
                error = %release,
                "failed to release the handle claim after a create failed; \
                 `orgs release-claim <slug>` will clear it"
            );
        }
        return Err(AppError::Internal(format!(
            "failed to write the organization record: {e:#}"
        )));
    }

    tracing::info!(org_id = %org.org_id, slug = %org.slug, "organization created");
    Ok(OrgView {
        org: org.into(),
        member_count: Some(0),
        client_count: Some(0),
    })
}

// GET /api/v1/orgs/{org_id}
pub async fn get_org(
    State(state): State<AppState>,
    active: ActiveUser,
    ApiPath(Parsed(OrgPath(reference))): ApiPath<Parsed<OrgPath>>,
) -> Result<impl IntoResponse, AppError> {
    let caller = active.caller();
    Ok(Json(one(&state, caller, &reference).await?))
}

/// The organization `reference` names, with its counts, as `caller` reads it.
pub async fn one(
    state: &AppState,
    caller: Caller<'_>,
    reference: &OrgRef,
) -> Result<OrgView, AppError> {
    let org = record(state, caller, reference).await?;
    counted(state, org).await
}

/// The organization `reference` names, refused outside what `caller` reads.
pub(crate) async fn record(
    state: &AppState,
    caller: Caller<'_>,
    reference: &OrgRef,
) -> Result<Organization, AppError> {
    let scope = roster_reach(
        state,
        caller,
        UserPermission::ReadOrgs,
        UserPermission::ReadAnyOrgs,
    )
    .await?;
    require_in_scope(state, &scope, reference).await
}

// PATCH /api/v1/orgs/{org_id}
pub async fn update_org(
    State(state): State<AppState>,
    active: HumanUser,
    audit: Audit,
    ApiPath(Parsed(org_id)): ApiPath<Parsed<OrgId>>,
    ApiJson(body): ApiJson<UpdateOrgRequest>,
) -> Result<impl IntoResponse, AppError> {
    let reference = OrgRef::Id(org_id);
    let caller = active.caller();
    let updated = update(&state, caller, &reference, body.display_name, body.status).await?;
    audit.changed(updated.changed);
    Ok(Json(updated))
}

/// Rename the organization `reference` names, or set its status, as `caller`.
/// Read-modify-write: two operators can lose an update, as `clients update` can.
pub async fn update(
    state: &AppState,
    caller: Caller<'_>,
    reference: &OrgRef,
    display_name: Option<NonEmptyTrimmedString>,
    status: Option<OrgStatus>,
) -> Result<Changed<OrgView>, AppError> {
    let scope = roster_reach(
        state,
        caller,
        UserPermission::ManageOrgs,
        UserPermission::ManageAnyOrgs,
    )
    .await?;

    if display_name.is_none() && status.is_none() {
        return Err(AppError::BadRequest(
            "nothing to update: send display_name, status, or both".into(),
        ));
    }

    // A scoped caller renames its own organization and no other.
    let mut org = require_in_scope(state, &scope, reference).await?;
    let stored = org.clone();
    if let Some(name) = display_name {
        org.display_name = name;
    }
    if let Some(status) = status
        && status != org.status
    {
        // Disabling is a server decision, so an org admin cannot undo it.
        caller.rights.require(UserPermission::ManageAnyOrgs)?;
        org.status = status;
    }
    // Nothing is written for a no-op, so it cannot revert a concurrent edit
    // with the copy read above.
    let changed = org != stored;
    if changed {
        state
            .org_store
            .put_org(&org)
            .await
            .map_err(|e| AppError::Internal(format!("failed to write the organization: {e:#}")))?;
        tracing::info!(org_id = %org.org_id, status = %org.status, "organization updated");
    }
    // Counted after the write lands, so a failed count is a `500` over a
    // write that stands; the request is idempotent, so a retry is safe.
    Ok(Changed {
        record: counted(state, org).await?,
        changed,
    })
}

// GET /api/v1/orgs/{org_id}/users
pub async fn list_org_users(
    State(state): State<AppState>,
    active: ActiveUser,
    ApiPath(Parsed(OrgPath(reference))): ApiPath<Parsed<OrgPath>>,
) -> Result<impl IntoResponse, AppError> {
    let caller = active.caller();
    let (org, users) = members(&state, caller, &reference).await?;
    Ok(Json(ListPage::whole(
        users
            .into_iter()
            .map(|user| OrgUserRow {
                user: user.into(),
                org_id: Some(org.org_id.clone()),
            })
            .collect(),
        0,
    )))
}

/// The accounts belonging to the organization `reference` names, as `caller`
/// reads them.
pub async fn members(
    state: &AppState,
    caller: Caller<'_>,
    reference: &OrgRef,
) -> Result<(Organization, Vec<User>), AppError> {
    let scope = roster_reach(
        state,
        caller,
        UserPermission::ReadOrgs,
        UserPermission::ReadAnyOrgs,
    )
    .await?;
    let org = require_in_scope(state, &scope, reference).await?;
    let users = org::members(&*state.org_store, &*state.user_store, &org.org_id)
        .await
        .map_err(|e| AppError::Internal(format!("failed to read the members: {e:#}")))?;
    Ok((org, users))
}

// GET /api/v1/orgs/{org_id}/clients
pub async fn list_org_clients(
    State(state): State<AppState>,
    active: ActiveUser,
    ApiPath(Parsed(OrgPath(reference))): ApiPath<Parsed<OrgPath>>,
) -> Result<impl IntoResponse, AppError> {
    let caller = active.caller();
    let (org, clients) = devices(&state, caller, &reference).await?;
    let administered = administers_org(&state, caller, &org.org_id).await?;
    Ok(Json(ListPage::whole(
        clients
            .into_iter()
            .map(|(client, owner)| ClientView {
                client: client.view(administered),
                owner: Some(owner),
            })
            .collect(),
        0,
    )))
}

/// Whether `caller` administers the devices `org_id` holds, which is who reads
/// their `contact_email`: every one rolls up to it.
pub(crate) async fn administers_org(
    state: &AppState,
    caller: Caller<'_>,
    org_id: &OrgId,
) -> Result<bool, AppError> {
    let owner = ClientOwner::Org {
        org_id: org_id.clone(),
    };
    let reach = device_reach(state, caller).await?;
    crate::api_v1::clients::administers(state, reach.as_ref(), Some(&owner)).await
}

/// The devices the organization `reference` names owns, directly or through a
/// member. Two grants: the rows are whole device records, beyond `read_orgs`.
pub async fn devices(
    state: &AppState,
    caller: Caller<'_>,
    reference: &OrgRef,
) -> Result<(Organization, Vec<(Client, ClientOwner)>), AppError> {
    let org = inventory_org(state, caller, reference).await?;
    let clients = org::owned_clients(&*state.client_store, &*state.org_store, &org.org_id)
        .await
        .map_err(|e| AppError::Internal(format!("failed to read the clients: {e:#}")))?;
    Ok((org, clients))
}

/// [`devices`] by id alone, each with the ownership that puts it there, on the
/// same two grants: for a listing that reads only the records its page draws.
pub async fn device_owners(
    state: &AppState,
    caller: Caller<'_>,
    reference: &OrgRef,
) -> Result<(Organization, HashMap<ClientId, ClientOwner>), AppError> {
    let org = inventory_org(state, caller, reference).await?;
    let owners = org::ownerships_of(&*state.org_store, &org.org_id)
        .await
        .map_err(|e| AppError::Internal(format!("failed to read the ownerships: {e:#}")))?;
    Ok((org, owners))
}

/// The organization whose inventory `caller` may read: [`devices`]'s two grants.
async fn inventory_org(
    state: &AppState,
    caller: Caller<'_>,
    reference: &OrgRef,
) -> Result<Organization, AppError> {
    // Checked here rather than by a gate: the fleet listing reaches this only
    // through its `org_id` filter, and the overview only for its device section.
    let rights = caller.rights;
    rights.require(UserPermission::ReadOrgs)?;
    rights.require(UserPermission::ReadClients)?;
    let scope = reach_of(
        state,
        &caller.user.user_id,
        rights,
        UserPermission::ReadAnyOrgs,
    )
    .await?;
    require_in_scope(state, &scope, reference).await
}

/// Set or clear which organization `user_id` belongs to, as `caller`. Both ends
/// resolve before the write, so a membership names a record that exists.
pub(crate) async fn assign_membership(
    state: &AppState,
    caller: &User,
    rights: &UserRights,
    user_id: &UserId,
    org_id: Option<OrgId>,
) -> Result<Changed<OrgUserRow>, AppError> {
    rights.require(UserPermission::ManageOrgs)?;
    let scope = reach_of(
        state,
        &caller.user_id,
        rights,
        UserPermission::ManageAnyOrgs,
    )
    .await?;

    // Both ends read `404` when absent or out of scope, so ids cannot be
    // probed. Only an account the caller already administers moves: adopting
    // an unaffiliated one could take over a server admin.
    let (target, current) = crate::api_v1::users::in_scope(state, &scope, user_id).await?;
    let org = match org_id {
        Some(org_id) => Some(require_in_scope(state, &scope, &OrgRef::Id(org_id)).await?),
        None => None,
    };
    match &org {
        // Naming the organization it already belongs to adds nobody.
        Some(org) if current.as_ref() != Some(&org.org_id) => refuse_disabled(org)?,
        Some(_) => {}
        None if target.account_type == UserAccountType::Service => {
            return Err(AppError::BadRequest(
                "a service account belongs to an organization; move it to another instead".into(),
            ));
        }
        // Out of its organization a scoped caller administers nothing, itself
        // included, so nobody could put it back.
        None if target.user_id == caller.user_id && matches!(scope, RosterScope::Org(_)) => {
            tracing::warn!(user_id = %target.user_id, "refused: a caller releasing itself from the organization it administers");
            return Err(AppError::BadRequest(
                "an account cannot release itself from the organization it administers".into(),
            ));
        }
        None => {}
    }
    // A Clerk provision rule makes server admins members too, so scope alone does not guard them.
    crate::api_v1::users::within_rank(state, rights, &target).await?;
    let org_id = org.map(|org| org.org_id);
    // Nothing is written for a no-op, so it cannot revert a move made since the read.
    let changed = org_id != current;
    if changed {
        state
            .org_store
            .set_membership(user_id, org_id.clone())
            .await
            .map_err(|e| AppError::Internal(format!("failed to write the membership: {e:#}")))?;
        tracing::info!(
            user_id = %target.user_id,
            org_id = org_id.as_ref().map(tracing::field::display),
            "user organization set"
        );
    }
    Ok(Changed {
        record: OrgUserRow {
            user: target.into(),
            org_id,
        },
        changed,
    })
}

/// Set or clear who owns `client_id`, as `caller`, and say whether it moved.
/// Owners resolve before the write, so `owning_org` reads an absent one as "no
/// organization", not "gone".
pub(crate) async fn assign_owner(
    state: &AppState,
    caller: &HumanUser,
    client_id: &ClientId,
    owner: Option<ClientOwner>,
) -> Result<(Client, bool), AppError> {
    // `manage_clients`, not `manage_orgs`: this writes the device's owner.
    let scope = crate::api_v1::clients::device_scope(state, &caller.user, &caller.rights).await?;

    // The near end first, as on every device write, so a device out of reach
    // refuses before the owner the body names is looked up.
    let client = crate::api_v1::clients::in_scope(state, &scope, client_id).await?;

    // A device a person owns fetches that person's vault, so a scoped caller names only itself.
    let for_another = owner
        .as_ref()
        .and_then(ClientOwner::user_id)
        .is_some_and(|user_id| user_id != &caller.user.user_id);
    if for_another && scope != DeviceScope::Fleet {
        tracing::warn!(%client_id, "refused: the device would go to another person");
        return Err(AppError::Forbidden(
            "only manage_any_clients may make a device another person's".into(),
        ));
    }

    // `Detach` is kept apart from a user in no organization: that is a hand-over
    // to someone outside any scoped caller's roster.
    let to = match owner.as_ref() {
        None => Destination::Detach,
        Some(ClientOwner::Org { org_id }) => {
            match org::require(&*state.org_store, &OrgRef::Id(org_id.clone())).await {
                Ok(_) => Destination::Org(Some(org_id.clone())),
                Err(missing @ org::OrgLookupError::NotFound(_)) => {
                    Destination::Absent(missing.into())
                }
                Err(e @ org::OrgLookupError::Store(_)) => return Err(e.into()),
            }
        }
        Some(ClientOwner::User { user_id }) => {
            let exists = state
                .user_store
                .get_user(user_id)
                .await
                .map_err(|e| AppError::Internal(format!("failed to read the owner: {e:#}")))?
                .is_some();
            if exists {
                Destination::Org(user_org(state, user_id).await?)
            } else {
                Destination::Absent(AppError::NotFound(format!("no user {user_id}")))
            }
        }
    };

    let allowed = match &to {
        Destination::Detach => scope.may_assign_to(None),
        // A user owner is a hand-over to that person's organization, and one
        // belonging to none is outside every scoped caller's reach.
        Destination::Org(org_id) => scope.may_hand_to(org_id.as_ref()),
        // On no roster, so a scoped caller cannot tell it from a real owner
        // outside its reach.
        Destination::Absent(_) => scope.may_hand_to(None),
    };
    if !allowed {
        tracing::warn!(%client_id, "refused: the move leaves the caller's scope");
        return Err(AppError::Forbidden(
            "this move is outside your organization".into(),
        ));
    }
    if let Destination::Absent(missing) = to {
        return Err(missing);
    }

    // Nothing is written for a no-op, so it cannot revert a move made since the read.
    let current = state
        .org_store
        .get_ownership(client_id)
        .await
        .map_err(|e| AppError::Internal(format!("failed to read the owner: {e:#}")))?;
    let changed = current != owner;
    if changed {
        state
            .org_store
            .set_ownership(client_id, owner.clone())
            .await
            .map_err(|e| AppError::Internal(format!("failed to write the owner: {e:#}")))?;
        tracing::info!(
            owner = owner.as_ref().map(ClientOwner::label),
            "client owner set"
        );
    }
    Ok((client, changed))
}

// PUT /api/v1/orgs/{org_id}/clients
//
// Not atomic: each device is its own write, reported per device, and a retry
// converges.
pub async fn assign_clients_to_org(
    State(state): State<AppState>,
    HumanUser { user, rights, .. }: HumanUser,
    audit: Audit,
    ApiPath(Parsed(org_id)): ApiPath<Parsed<OrgId>>,
    ApiJson(body): ApiJson<AssignClientsRequest>,
) -> Result<impl IntoResponse, AppError> {
    let reference = OrgRef::Id(org_id);
    let scope = device_scope(&state, &user, &rights).await?;
    if body.client_ids.is_empty() {
        return Err(AppError::BadRequest(
            "client_ids is empty: name at least one device".into(),
        ));
    }
    // A device named twice is assigned once, in first-seen order: two copies
    // would be two snapshots of one record, each reporting the same move.
    let mut seen = HashSet::new();
    let client_ids: Vec<ClientId> = body
        .client_ids
        .into_iter()
        .filter(|client_id| seen.insert(client_id.clone()))
        .collect();

    // Before the scope checks and the move, so the organizations that own the
    // devices see the move, or the refused attempt.
    let moved = client_ids
        .iter()
        .cloned()
        .map(|client_id| AuditTarget::Client { client_id });
    audit.involve(&state, moved).await;
    let org = assignable_org(&state, &scope, &reference).await?;

    // Every device is resolved and scope-checked before any is written, so one
    // out of reach fails the whole call rather than half-assigning a rack.
    let owner = ClientOwner::Org {
        org_id: org.org_id.clone(),
    };
    let clients = futures::future::try_join_all(
        client_ids
            .iter()
            .map(|client_id| in_scope(&state, &scope, client_id)),
    )
    .await?;

    let assigned = futures::stream::iter(clients)
        .then(|client| {
            let owner = &owner;
            let state = &state;
            async move {
                let current = state
                    .org_store
                    .get_ownership(&client.client_id)
                    .await
                    .map_err(|e| AppError::Internal(format!("failed to read the owner: {e:#}")))?;
                let changed = current.as_ref() != Some(owner);
                if changed {
                    let ownership = Ownership {
                        client_id: client.client_id.clone(),
                        owner: owner.clone(),
                    };
                    state
                        .org_store
                        .put_ownership(&ownership)
                        .await
                        .map_err(|e| {
                            AppError::Internal(format!("failed to write the owner: {e:#}"))
                        })?;
                }
                Ok::<_, AppError>(AssignedClient {
                    client_id: client.client_id,
                    changed,
                })
            }
        })
        .try_collect::<Vec<_>>()
        .await?;

    tracing::info!(
        org_id = %org.org_id,
        assigned = assigned.iter().filter(|a| a.changed).count(),
        "clients assigned to an organization"
    );
    audit.changed(assigned.iter().any(|a| a.changed));
    Ok(Json(ClientsAssigned {
        org_id: org.org_id,
        clients: assigned,
    }))
}
