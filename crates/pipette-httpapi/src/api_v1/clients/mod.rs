//! HTTP handlers for the device-administration routes on
//! `/api/v1/clients/{client_id}`. The operations live in
//! [`pipette_pipeline::client_admin`], shared with the `pipette-mgmt clients` CLI.

pub mod filters;
pub mod reads;

use std::collections::BTreeSet;

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};

use pipette_core::client::{Client, ClientStatus};
use pipette_core::organization::Organization;
use pipette_core::user::User;
use pipette_pipeline::client_admin::{Applied, ClientUpdate, StatusChange};
use pipette_pipeline::org::{DeviceScope, OrgRef};
use pipette_pipeline::{client_admin, org};
use pipette_plan_types::{
    AmendClientPermissionsRequest, Changed, ClientApproved, ClientId, ClientOwner,
    ClientPermission, ClientPermissions, ClientView, SetClientOwnerRequest,
    SetClientPermissionsRequest, UpdateClientRequest, UserPermission,
};
use pipette_store::ClientPermissionMutation;

use crate::api_v1::orgs::assign_owner;
use crate::audit::Audit;
use crate::auth::{Caller, HumanUser, UserRights, user_org};
use crate::error::AppError;
use crate::extract::{ApiJson, ApiPath, Parsed};
use crate::state::AppState;

fn changed_permissions(
    client_id: ClientId,
    mutation: ClientPermissionMutation,
) -> Changed<ClientPermissions> {
    Changed {
        record: ClientPermissions {
            client_id,
            permissions: pipette_plan_types::ClientPermissionSet::from(
                &mutation.resulting.resolved(),
            ),
        },
        changed: mutation.changed,
    }
}

/// How far this caller's device authority reaches, or a `403` without
/// `manage_clients`, re-checked so the scope never outlives a loosened gate.
pub async fn device_scope(
    state: &AppState,
    user: &User,
    rights: &UserRights,
) -> Result<DeviceScope, AppError> {
    rights.require(UserPermission::ManageClients)?;
    if rights.holds(UserPermission::ManageAnyClients) {
        return Ok(DeviceScope::Fleet);
    }
    Ok(user_org(state, &user.user_id)
        .await?
        .map_or(DeviceScope::Unaffiliated, DeviceScope::Org))
}

/// [`device_scope`] without the `403`, for a page that draws its forms rather
/// than refusing them: `None` without `manage_clients`.
pub async fn device_reach(
    state: &AppState,
    caller: Caller<'_>,
) -> Result<Option<DeviceScope>, AppError> {
    let user = caller.user;
    let rights = caller.rights;
    if !rights.holds(UserPermission::ManageClients) {
        return Ok(None);
    }
    device_scope(state, user, rights).await.map(Some)
}

/// Whether `reach` administers the device `owner` holds, by [`in_scope`]'s
/// rule: who reads its `contact_email`.
pub(crate) async fn administers(
    state: &AppState,
    reach: Option<&DeviceScope>,
    owner: Option<&ClientOwner>,
) -> Result<bool, AppError> {
    let Some(scope) = reach else {
        return Ok(false);
    };
    // Only an organization's scope asks where the device rolls up.
    let owning = match (scope, owner) {
        (DeviceScope::Org(_), Some(owner)) => org::org_of(&*state.org_store, owner)
            .await
            .map_err(|e| AppError::internal(e.context("failed to resolve the owner")))?,
        _ => None,
    };
    Ok(scope.may_administer(owning.as_ref()))
}

/// Refuse unless `scope` reaches the device `client_id` names, and hand back the
/// record. The `403` names the scope, not the owner, so probes map no roster.
pub async fn in_scope(
    state: &AppState,
    scope: &DeviceScope,
    client_id: &ClientId,
) -> Result<Client, AppError> {
    let client = client_admin::require(&*state.client_store, client_id).await?;
    let owning_org = org::owning_org(&*state.org_store, &client.client_id)
        .await
        .map_err(|e| AppError::Internal(format!("failed to resolve the owner: {e:#}")))?;
    if !scope.may_administer(owning_org.as_ref()) {
        tracing::warn!(%client_id, "refused: the device is outside the caller's scope");
        return Err(AppError::Forbidden(
            "this device is outside your organization".into(),
        ));
    }
    Ok(client)
}

/// The organization a path names, when `scope` may put a device down in it.
/// Absent and out of scope both read `404`, so a caller learns no handles.
pub(crate) async fn assignable_org(
    state: &AppState,
    scope: &DeviceScope,
    reference: &OrgRef,
) -> Result<Organization, AppError> {
    let org = org::require(&*state.org_store, reference).await?;
    if !scope.may_assign_to(Some(&org.org_id)) {
        tracing::warn!(org_id = %org.org_id, "refused: assignment target outside the caller's scope");
        return Err(AppError::OutOfScope(
            org::OrgLookupError::NotFound(reference.clone()).to_string(),
        ));
    }
    Ok(org)
}

// PATCH /api/v1/clients/{client_id}
pub async fn update_client(
    State(state): State<AppState>,
    HumanUser { user, rights, .. }: HumanUser,
    audit: Audit,
    ApiPath(Parsed(client_id)): ApiPath<Parsed<ClientId>>,
    ApiJson(body): ApiJson<UpdateClientRequest>,
) -> Result<impl IntoResponse, AppError> {
    let scope = device_scope(&state, &user, &rights).await?;
    in_scope(&state, &scope, &client_id).await?;

    let update = ClientUpdate {
        organization: body.organization,
        client_details: body.client_details,
        contact_email: body.contact_email,
    };
    if update.is_empty() {
        return Err(AppError::BadRequest(
            "nothing to update: send organization, client_details or contact_email".into(),
        ));
    }

    // `Unchanged` still answers with the record: the caller asked for a state,
    // and it holds, which is a success rather than a `304`.
    let (client, changed) =
        match client_admin::update(&*state.client_store, &client_id, update).await? {
            Applied::Changed((client, _)) => (client, true),
            Applied::Unchanged => (
                client_admin::require(&*state.client_store, &client_id).await?,
                false,
            ),
        };
    audit.changed(changed);
    Ok(Json(Changed {
        record: written_view(&state, client).await?,
        changed,
    }))
}

/// The device as a write answers it, with its owner. Only an administrator of
/// the device reaches a write.
async fn written_view(state: &AppState, client: Client) -> Result<ClientView, AppError> {
    let owner = state
        .org_store
        .get_ownership(&client.client_id)
        .await
        .map_err(|e| AppError::Internal(format!("failed to read the owner: {e:#}")))?;
    Ok(ClientView {
        client: client.view(true),
        owner,
    })
}

// POST /api/v1/clients/{client_id}/approve
//
// Idempotent, so a retry whose response was lost costs nothing.
pub async fn approve_client(
    State(state): State<AppState>,
    caller: HumanUser,
    audit: Audit,
    ApiPath(Parsed(client_id)): ApiPath<Parsed<ClientId>>,
) -> Result<impl IntoResponse, AppError> {
    // `Unchanged` still answers with the record, as a no-op `PATCH` does.
    let (client, previous_status) = match approve(&state, &caller, &client_id).await? {
        Applied::Changed((client, change)) => (client, Some(change.from.into())),
        Applied::Unchanged => (
            client_admin::require(&*state.client_store, &client_id).await?,
            None,
        ),
    };
    audit.changed(previous_status.is_some());
    Ok(Json(ClientApproved {
        view: written_view(&state, client).await?,
        changed: previous_status.is_some(),
        previous_status,
    }))
}

/// Admit the device `client_id` names, as `caller`.
async fn approve(
    state: &AppState,
    caller: &HumanUser,
    client_id: &ClientId,
) -> Result<Applied<(Client, StatusChange)>, AppError> {
    let scope = device_scope(state, &caller.user, &caller.rights).await?;
    in_scope(state, &scope, client_id).await?;
    Ok(client_admin::set_status(&*state.client_store, client_id, ClientStatus::Approved).await?)
}

// POST /api/v1/clients/{client_id}/reject
//
// A delete, not a status. Refused unless pending, so it cannot remove a device
// that is doing work.
pub async fn reject_client(
    State(state): State<AppState>,
    caller: HumanUser,
    ApiPath(Parsed(client_id)): ApiPath<Parsed<ClientId>>,
) -> Result<impl IntoResponse, AppError> {
    reject(&state, &caller, &client_id).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Turn away the pending registration `client_id` names, as `caller`.
async fn reject(
    state: &AppState,
    caller: &HumanUser,
    client_id: &ClientId,
) -> Result<(), AppError> {
    let scope = device_scope(state, &caller.user, &caller.rights).await?;
    in_scope(state, &scope, client_id).await?;

    client_admin::reject(
        &*state.client_store,
        &*state.key_index_store,
        &*state.org_store,
        &*state.tag_store,
        client_id,
    )
    .await?;
    Ok(())
}

// POST /api/v1/clients/{client_id}/unsuspend
pub async fn unsuspend_client(
    State(state): State<AppState>,
    caller: HumanUser,
    audit: Audit,
    ApiPath(Parsed(client_id)): ApiPath<Parsed<ClientId>>,
) -> Result<Response, AppError> {
    let (changed, client) = unsuspend(&state, &caller, &client_id).await?;
    audit.changed(changed);
    // The fleet scope clears a marker no record answers to; with no device to
    // show, it answers as `DELETE` does for one that is not there.
    let Some(client) = client else {
        return Ok(StatusCode::NO_CONTENT.into_response());
    };
    Ok(Json(Changed {
        record: written_view(&state, client).await?,
        changed,
    })
    .into_response())
}

/// Clear the suspension on `client_id`, reporting whether there was one, with
/// the device's record, which only the fleet scope may find absent.
async fn unsuspend(
    state: &AppState,
    caller: &HumanUser,
    client_id: &ClientId,
) -> Result<(bool, Option<Client>), AppError> {
    let scope = device_scope(state, &caller.user, &caller.rights).await?;
    // A suspension is queue state, so a stranded marker has no record; a scoped
    // caller still needs one to show the device was ever theirs.
    let client = if scope == DeviceScope::Fleet {
        state
            .client_store
            .get_client(client_id)
            .await
            .map_err(|e| AppError::internal(e.context("failed to read the device")))?
    } else {
        Some(in_scope(state, &scope, client_id).await?)
    };

    let changed = client_admin::unsuspend(&*state.todo_store, client_id)
        .await
        .map_err(|e| AppError::Internal(format!("failed to clear the suspension: {e:#}")))?;
    Ok((changed, client))
}

// DELETE /api/v1/clients/{client_id}
//
// Idempotent for the fleet scope, so a teardown that stopped part-way finishes
// on a repeat.
pub async fn delete_client(
    State(state): State<AppState>,
    caller: HumanUser,
    ApiPath(Parsed(client_id)): ApiPath<Parsed<ClientId>>,
) -> Result<impl IntoResponse, AppError> {
    delete(&state, &caller, &client_id).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Remove the device `client_id` names, as `caller`.
async fn delete(
    state: &AppState,
    caller: &HumanUser,
    client_id: &ClientId,
) -> Result<(), AppError> {
    let scope = device_scope(state, &caller.user, &caller.rights).await?;
    // A scoped caller's check needs the record, so only the fleet scope gets the
    // idempotent `204`.
    if scope != DeviceScope::Fleet {
        in_scope(state, &scope, client_id).await?;
    }

    client_admin::delete(
        &*state.client_store,
        &*state.key_index_store,
        &*state.org_store,
        &*state.tag_store,
        &*state.todo_store,
        client_id,
    )
    .await?;
    Ok(())
}

// PUT /api/v1/clients/{client_id}/permissions
pub async fn set_client_permissions(
    State(state): State<AppState>,
    caller: HumanUser,
    audit: Audit,
    ApiPath(Parsed(client_id)): ApiPath<Parsed<ClientId>>,
    ApiJson(body): ApiJson<SetClientPermissionsRequest>,
) -> Result<impl IntoResponse, AppError> {
    let mutation = set_permissions(&state, &caller, &client_id, &body.permissions).await?;
    audit.changed(mutation.changed);
    Ok(Json(changed_permissions(client_id, mutation)))
}

/// Replace what the device `client_id` names may do, as `caller`.
pub async fn set_permissions(
    state: &AppState,
    caller: &HumanUser,
    client_id: &ClientId,
    permissions: &BTreeSet<ClientPermission>,
) -> Result<ClientPermissionMutation, AppError> {
    let scope = device_scope(state, &caller.user, &caller.rights).await?;
    in_scope(state, &scope, client_id).await?;
    Ok(client_admin::set_permissions(&*state.client_store, client_id, permissions).await?)
}

// PATCH /api/v1/clients/{client_id}/permissions
pub async fn amend_client_permissions(
    State(state): State<AppState>,
    caller: HumanUser,
    audit: Audit,
    ApiPath(Parsed(client_id)): ApiPath<Parsed<ClientId>>,
    ApiJson(body): ApiJson<AmendClientPermissionsRequest>,
) -> Result<impl IntoResponse, AppError> {
    let mutation = amend_permissions(&state, &caller, &client_id, &body.add, &body.remove).await?;
    audit.changed(mutation.changed);
    Ok(Json(changed_permissions(client_id, mutation)))
}

/// Grant `add` to the device `client_id` names and revoke `remove`, as
/// `caller`.
pub async fn amend_permissions(
    state: &AppState,
    caller: &HumanUser,
    client_id: &ClientId,
    add: &BTreeSet<ClientPermission>,
    remove: &BTreeSet<ClientPermission>,
) -> Result<ClientPermissionMutation, AppError> {
    let scope = device_scope(state, &caller.user, &caller.rights).await?;
    in_scope(state, &scope, client_id).await?;

    if add.is_empty() && remove.is_empty() {
        return Err(AppError::BadRequest(
            "nothing to amend: send add, remove, or both".into(),
        ));
    }
    // Refused rather than ordered: either reading leaves the caller believing
    // the other half took effect.
    if let Some(both) = add.intersection(remove).next() {
        return Err(AppError::BadRequest(format!(
            "{both} is in both add and remove"
        )));
    }
    Ok(client_admin::amend_permissions(&*state.client_store, client_id, add, remove).await?)
}

// PUT /api/v1/clients/{client_id}/owner
pub async fn set_client_owner(
    State(state): State<AppState>,
    caller: HumanUser,
    audit: Audit,
    ApiPath(Parsed(client_id)): ApiPath<Parsed<ClientId>>,
    ApiJson(body): ApiJson<SetClientOwnerRequest>,
) -> Result<impl IntoResponse, AppError> {
    let owner = body.owner;
    let (client, changed) = assign_owner(&state, &caller, &client_id, owner.clone()).await?;
    audit.changed(changed);
    Ok(Json(Changed {
        record: ClientView {
            // `assign_owner` admitted only an administrator, through `in_scope`.
            client: client.view(true),
            owner,
        },
        changed,
    }))
}
