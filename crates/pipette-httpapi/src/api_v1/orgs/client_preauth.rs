//! HTTP handlers for the device pre-auth keys an organization's administrator
//! mints under `/api/v1/orgs/{org}/client-preauth-keys`. A key approves a
//! device and seeds its owner, and nothing else.

use std::collections::BTreeSet;

use axum::Json;
use axum::extract::State;
use axum::http::{StatusCode, header};
use axum::response::IntoResponse;
use chrono::Utc;

use pipette_core::client_preauth;
use pipette_core::client_preauth::{ClientPreauthKey, ClientPreauthParams};
use pipette_core::organization::Organization;
use pipette_pipeline::org::OrgRef;
use pipette_plan_types::{
    ClientOwner, ClientPreauthKeyId, ClientPreauthKeyMinted, ClientPreauthKeyView,
    ClientPreauthUsage, ListPage, MintClientPreauthRequest, OrgId, UserId, UserStatus,
};

use crate::api_v1::clients::{assignable_org, device_reach, device_scope};
use crate::api_v1::orgs::OrgPath;
use crate::api_v1::users::preauth_days;
use crate::auth::{Caller, HumanUser, UserRights};
use crate::error::AppError;
use crate::extract::{ApiJson, ApiPath, Parsed};
use crate::state::AppState;

/// Whether `caller` may mint a key for `org_id`, for a page that draws the form
/// rather than refusing it.
pub async fn may_mint_for(
    state: &AppState,
    caller: Caller<'_>,
    org_id: &OrgId,
) -> Result<bool, AppError> {
    Ok(device_reach(state, caller)
        .await?
        .is_some_and(|scope| scope.may_assign_to(Some(org_id))))
}

/// The organization `reference` names, when `caller` may onboard devices into
/// it: a `403` without `manage_clients`, a `404` outside its scope.
async fn onboarding_org(
    state: &AppState,
    caller: &HumanUser,
    reference: &OrgRef,
) -> Result<Organization, AppError> {
    let scope = device_scope(state, &caller.user, &caller.rights).await?;
    assignable_org(state, &scope, reference).await
}

/// The store keys pre-auth keys by id alone, so this reads the operator-sized
/// keyspace. A key naming a member rather than the organization is that
/// member's, and is not here.
async fn naming(state: &AppState, org_id: &OrgId) -> Result<Vec<ClientPreauthKey>, AppError> {
    Ok(state
        .client_store
        .list_client_preauth_keys()
        .await
        .map_err(|e| AppError::internal(e.context("failed to list the pre-auth keys")))?
        .into_iter()
        .filter(|key| key.default_owner.as_ref().and_then(ClientOwner::org_id) == Some(org_id))
        .collect())
}

/// The keys naming `org_id`, newest first, the host's among them.
pub async fn keys(state: &AppState, org_id: &OrgId) -> Result<Vec<ClientPreauthKeyView>, AppError> {
    let now = Utc::now();
    let mut keys = naming(state, org_id).await?;
    keys.sort_by_key(|key| std::cmp::Reverse(key.created_at));
    Ok(keys
        .into_iter()
        .map(|key| key.view(org_id.clone(), now))
        .collect())
}

/// Registration holds the minter to the same scope again ([`admit_delegated`]).
async fn mint(
    state: &AppState,
    caller: &HumanUser,
    reference: &OrgRef,
    request: MintClientPreauthRequest,
) -> Result<(Organization, ClientPreauthKeyMinted), AppError> {
    let days = preauth_days(request.expires_in_days, request.note.as_ref())?;
    let usage = request.usage.unwrap_or(ClientPreauthUsage::SingleUse);
    let org = onboarding_org(state, caller, reference).await?;

    let now = Utc::now();
    let mut minted = client_preauth::mint(
        ClientPreauthParams {
            usage,
            expires_at: Some(now + chrono::Duration::days(i64::from(days))),
            default_tags: BTreeSet::new(),
            default_organization: None,
            default_owner: Some(ClientOwner::Org {
                org_id: org.org_id.clone(),
            }),
            default_permissions: None,
            note: request.note,
        },
        now,
    )
    .map_err(|e| AppError::Internal(format!("failed to mint the pre-auth key: {e}")))?;
    minted.key.minted_by = Some(caller.user.user_id.clone());
    state
        .client_store
        .put_client_preauth_key(&minted.key)
        .await
        .map_err(|e| AppError::internal(e.context("failed to store the pre-auth key")))?;

    tracing::info!(org_id = %org.org_id, key_id = %minted.key.key_id, "client pre-auth key minted");
    let key = minted.key.view(org.org_id.clone(), now);
    Ok((
        org,
        ClientPreauthKeyMinted {
            key,
            token: pipette_plan_types::ClientPreauthKey::new(minted.token.expose()),
        },
    ))
}

/// Revoke a key through the organization it names, so another organization's
/// path answers `404`.
async fn revoke(
    state: &AppState,
    caller: &HumanUser,
    reference: &OrgRef,
    key_id: &ClientPreauthKeyId,
) -> Result<Organization, AppError> {
    let org = onboarding_org(state, caller, reference).await?;
    if !naming(state, &org.org_id)
        .await?
        .iter()
        .any(|key| &key.key_id == key_id)
    {
        return Err(AppError::NotFound(
            "no such pre-auth key on this organization".into(),
        ));
    }
    state
        .client_store
        .delete_client_preauth_key(key_id)
        .await
        .map_err(|e| AppError::internal(e.context("failed to revoke the pre-auth key")))?;
    tracing::info!(org_id = %org.org_id, %key_id, "client pre-auth key revoked");
    Ok(org)
}

/// Refuse a registration with a key its minter could not mint now, so a copy
/// kept by an administrator who has since left the organization admits nothing.
pub async fn admit_delegated(
    state: &AppState,
    minter: &UserId,
    owner: Option<&ClientOwner>,
) -> Result<(), AppError> {
    let refused = |reason: &str| {
        tracing::warn!(%minter, reason, "refused a registration with a pre-auth key its minter no longer vouches for");
        AppError::Forbidden(
            "this pre-auth key no longer admits a device; ask the organization's administrator \
             for a new one"
                .into(),
        )
    };
    let active = state
        .user_store
        .get_user(minter)
        .await
        .map_err(|e| AppError::internal(e.context("failed to read the minter")))?
        .filter(|minter| minter.status == UserStatus::Active);
    let Some(active) = active else {
        return Err(refused("the minter is gone or not active"));
    };
    let rights = UserRights::of_account(state, &active).await?;
    let as_minter = Caller {
        user: &active,
        rights: &rights,
    };
    let Some(scope) = device_reach(state, as_minter).await? else {
        return Err(refused("the minter no longer holds manage_clients"));
    };
    // Every key minted here names an organization, so one that does not was
    // not minted here.
    let Some(org_id) = owner.and_then(ClientOwner::org_id) else {
        return Err(refused("the key names no organization"));
    };
    if !scope.may_assign_to(Some(org_id)) {
        return Err(refused("the organization is outside the minter's scope"));
    }
    Ok(())
}

// GET /api/v1/orgs/{org_id}/client-preauth-keys
pub async fn list_client_preauth_keys(
    State(state): State<AppState>,
    caller: HumanUser,
    ApiPath(Parsed(OrgPath(reference))): ApiPath<Parsed<OrgPath>>,
) -> Result<impl IntoResponse, AppError> {
    let org = onboarding_org(&state, &caller, &reference).await?;
    let preauth_keys = keys(&state, &org.org_id).await?;
    Ok(Json(ListPage::whole(preauth_keys, 0)))
}

// POST /api/v1/orgs/{org_id}/client-preauth-keys
pub async fn mint_client_preauth_key(
    State(state): State<AppState>,
    caller: HumanUser,
    ApiPath(Parsed(org_id)): ApiPath<Parsed<OrgId>>,
    ApiJson(body): ApiJson<MintClientPreauthRequest>,
) -> Result<impl IntoResponse, AppError> {
    let reference = OrgRef::Id(org_id);
    let (_, minted) = mint(&state, &caller, &reference, body).await?;
    // The answer is a credential, so nothing on the way back keeps a copy.
    Ok((
        StatusCode::CREATED,
        [(header::CACHE_CONTROL, "no-store")],
        Json(minted),
    ))
}

// DELETE /api/v1/orgs/{org_id}/client-preauth-keys/{key_id}
pub async fn revoke_client_preauth_key(
    State(state): State<AppState>,
    caller: HumanUser,
    ApiPath((Parsed(org_id), Parsed(key_id))): ApiPath<(Parsed<OrgId>, Parsed<ClientPreauthKeyId>)>,
) -> Result<impl IntoResponse, AppError> {
    let reference = OrgRef::Id(org_id);
    revoke(&state, &caller, &reference, &key_id).await?;
    Ok(StatusCode::NO_CONTENT)
}
