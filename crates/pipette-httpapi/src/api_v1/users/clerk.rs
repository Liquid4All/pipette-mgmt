//! Signing in through Clerk: the account a Clerk token's verified address names, and
//! `POST /api/v1/users/register/clerk`, which registers a key to it for a browser that
//! holds none.

use std::collections::BTreeSet;

use axum::Json;
use axum::extract::State;
use axum::http::header::AUTHORIZATION;
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use chrono::{TimeDelta, Utc};

use pipette_core::user::{User, UserKey};
use pipette_core::user_permission::UserPermissionGroup;
use pipette_pipeline::org;
use pipette_pipeline::org::OrgLookupError;
use pipette_pipeline::user_admin;
use pipette_pipeline::user_admin::Provisioned;
use pipette_plan_types::{
    AuditAction, AuditActor, AuditTarget, ContactEmail, FirstClerkSignIn, OrgStatus,
    RegisterClerkUserKeyRequest, UserAccountType, UserId, UserKeyId, UserKeyRegistered,
};

use crate::api_v1::users::own::register_key;
use crate::audit::Audit;
use crate::auth::require_active;
use crate::clerk::ClerkSignIn;
use crate::error::AppError;
use crate::extract::ApiJson;
use crate::state::AppState;

/// How long a key registered through Clerk signs. The token proved a sign-in,
/// not a machine, so the key lapses like a session rather than staying for good.
pub const CLERK_KEY_LIFETIME: TimeDelta = TimeDelta::days(7);

// POST /api/v1/users/register/clerk
//
// The body is read first, so a malformed one leaves the token unspent.
pub async fn register_clerk_key(
    State(state): State<AppState>,
    audit: Audit,
    headers: HeaderMap,
    ApiJson(body): ApiJson<RegisterClerkUserKeyRequest>,
) -> Result<impl IntoResponse, AppError> {
    let user = clerk_account(&state, &audit, &headers).await?;
    let expires_at = Utc::now() + CLERK_KEY_LIFETIME;
    let key = register_key(
        &state,
        &user,
        body.public_key,
        body.key_details,
        Some(expires_at),
    )
    .await?;
    let actor = AuditActor::User {
        user_id: user.user_id.clone(),
        key_id: key.key_id.clone(),
    };
    audit.acted_as(actor.clone());
    tracing::info!(user_id = %user.user_id, key_id = %key.key_id, %expires_at,
        "registered a key through Clerk");
    // Best effort: the key is written, so a failed record only leaves the next
    // sign-in the first.
    if let Err(e) = record_first_sign_in(&state, &user.user_id, actor, &key.key_id).await {
        tracing::error!(user_id = %user.user_id, error = format!("{e:#}"),
            "failed to record the first Clerk sign-in");
    }
    Ok((
        StatusCode::CREATED,
        Json(UserKeyRegistered {
            user_id: user.user_id,
            status: user.status,
            key_id: key.key_id,
        }),
    ))
}

/// On the account's first Clerk sign-in, record the keys it already held, which
/// someone else may have registered: its owner is shown them, and the log names
/// them. Nothing is revoked. Runs after the key, so a refused registration
/// leaves the next sign-in the first.
async fn record_first_sign_in(
    state: &AppState,
    user_id: &UserId,
    actor: AuditActor,
    signed_in: &UserKeyId,
) -> anyhow::Result<()> {
    let users = &*state.user_store;
    // Every later sign-in stops at this read, before listing the keys.
    if users.get_user_first_clerk_sign_in(user_id).await?.is_some() {
        return Ok(());
    }
    let prior: Vec<UserKey> = users
        .list_user_keys(user_id)
        .await?
        .into_iter()
        .filter(|key| key.key_id != *signed_in)
        .collect();
    let first = FirstClerkSignIn {
        at: Utc::now(),
        prior_keys: prior.iter().map(|key| key.key_id.clone()).collect(),
    };
    if !users
        .record_user_first_clerk_sign_in(user_id, &first)
        .await?
        || prior.is_empty()
    {
        return Ok(());
    }
    tracing::warn!(%user_id, keys = prior.len(),
        "first Clerk sign-in to an account that already held keys");
    let target = AuditTarget::User {
        user_id: user_id.clone(),
    };
    let action = AuditAction::FirstClerkSignIn {
        prior_keys: prior.into_iter().map(Into::into).collect(),
    };
    crate::audit::append(state, actor, action, BTreeSet::from([target])).await;
    Ok(())
}

/// The active account the request's Clerk token names, created first when a
/// provision rule covers the address. Spends the token.
async fn clerk_account(
    state: &AppState,
    audit: &Audit,
    headers: &HeaderMap,
) -> Result<User, AppError> {
    let clerk = state
        .clerk
        .as_deref()
        .ok_or_else(|| AppError::NotFound("this server signs in with keys alone".into()))?;
    let jwt = headers
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .ok_or_else(|| {
            tracing::warn!("refused a Clerk sign-in: no Bearer token");
            AppError::Unauthorized("missing Authorization: Bearer header".into())
        })?;
    let email = clerk.verify(jwt.trim(), &state.replay_cache, Utc::now())?;
    // Before the account lookup: an instance shared with other apps vouches for
    // addresses this deployment does not admit, accounts included.
    if !clerk.admits(&email) {
        tracing::warn!("refused: the address a Clerk token names is outside allowed_domains");
        return Err(AppError::Forbidden(format!(
            "Clerk sign-in is limited to addresses at {}",
            clerk.allowed_domains().join(", ")
        )));
    }

    let user = account(state, audit, clerk, email).await?;
    // Named now: the writes below, and provisioning before them, stand.
    audit
        .involve(
            state,
            [AuditTarget::User {
                user_id: user.user_id.clone(),
            }],
        )
        .await;
    tracing::Span::current().record("client_id", tracing::field::display(&user.user_id));
    require_active(&user)?;
    if user.account_type == UserAccountType::Service {
        tracing::warn!("refused: a Clerk sign-in as a service account");
        return Err(AppError::Forbidden(
            "a service account signs with its own keys, not through Clerk".into(),
        ));
    }
    // A Clerk sign-in may hold no key, so the mark records it: no pre-auth key
    // can then be minted that signs in as this account.
    state
        .user_store
        .clear_user_awaiting_sign_in(&user.user_id)
        .await
        .map_err(AppError::internal)?;
    Ok(user)
}

/// The account holding `email`, or the one its provision rule creates.
async fn account(
    state: &AppState,
    audit: &Audit,
    clerk: &ClerkSignIn,
    email: ContactEmail,
) -> Result<User, AppError> {
    if let Some(holder) = state
        .user_store
        .lookup_user_by_email(email.as_str())
        .await
        .map_err(AppError::internal)?
    {
        return holder_record(state, &holder).await;
    }

    let Some(rule) = clerk.provision_rule(&email) else {
        tracing::warn!("refused: no account holds the address a Clerk token names");
        return Err(AppError::Forbidden(format!(
            "no account holds {}; ask an operator to create one",
            email.as_str()
        )));
    };
    let org = org::require(&*state.org_store, &rule.org)
        .await
        .map_err(|e| match e {
            OrgLookupError::NotFound(reference) => AppError::Internal(format!(
                "[clerk] provision names {reference}, which does not exist"
            )),
            OrgLookupError::Store(e) => AppError::internal(e),
        })?;
    // A disabled organization's members keep their reach, but it should not grow.
    if org.status == OrgStatus::Disabled {
        tracing::warn!(org_id = %org.org_id, "refused: provisioning into a disabled organization");
        return Err(AppError::Forbidden(format!(
            "accounts at {} join {}, which is disabled; ask an operator",
            rule.domain, org.slug
        )));
    }

    // A provisioning that fails partway still wrote into this organization.
    audit
        .involve(
            state,
            [AuditTarget::Org {
                org_id: org.org_id.clone(),
            }],
        )
        .await;
    match user_admin::provision(
        &*state.user_store,
        &*state.org_store,
        email,
        &org.org_id,
        UserPermissionGroup::OrgMember.permissions(),
    )
    .await
    .map_err(AppError::internal)?
    {
        Provisioned::Created(user) => Ok(user),
        // A sign-in racing this one claimed the address first.
        Provisioned::Taken(holder) => holder_record(state, &holder).await,
    }
}

/// The record behind an address claim; a create still in flight has none yet.
async fn holder_record(state: &AppState, holder: &UserId) -> Result<User, AppError> {
    state
        .user_store
        .get_user(holder)
        .await
        .map_err(AppError::internal)?
        .ok_or_else(|| {
            tracing::warn!(%holder, "refused: the address is claimed by no account record");
            AppError::Conflict(format!(
                "the address is claimed by {holder}, which has no account record yet; try \
                 again, or ask an operator to finish it"
            ))
        })
}
