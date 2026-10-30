//! HTTP handlers for the user routes: `POST /api/v1/users/register`, `GET /api/v1/users/me`,
//! `POST /api/v1/users/me/keys` and `DELETE /api/v1/users/me/keys/{key_id}`.

use std::net::IpAddr;

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use chrono::{DateTime, TimeDelta, Utc};

use pipette_core::key_index::{KeyClaim, KeyHolderPrincipalId, digest_of};
use pipette_core::types::DeriveFromKey;
use pipette_core::user::{User, UserKey};
use pipette_core::user_preauth::{UserPreauthConsumeOutcome, UserPreauthToken};
use pipette_pipeline::org::{DeviceScope, RosterScope};
use pipette_plan_types::{
    AddUserKeyRequest, AuditActor, AuditTarget, ContactEmail, NonEmptyTrimmedString, OwnUserView,
    PublicKeyHex, Reach, RegisterUserKeyRequest, UserAbilities, UserAccountType, UserId, UserKeyId,
    UserKeyRegistered, UserPermission, UserPermissionSet, UserStatus,
};
use pipette_store::EmailClaim;

use crate::audit::Audit;
use crate::auth::{ActiveUser, AuthenticatedUser, Caller, KeyOnlyUser, UserRights, user_org};
use crate::error::AppError;
use crate::extract::{ApiJson, ApiPath, Parsed};
use crate::registration_throttle::ClientIp;
use crate::state::AppState;

/// Ceiling on a submitted `key_details`, in characters: the authentication path
/// deserializes it on every request from the account.
const KEY_DETAILS_MAX_CHARS: usize = 4096;

/// The furthest out a key may expire: an expiring key is a browser's sign-in.
pub const MAX_KEY_LIFETIME: TimeDelta = TimeDelta::days(30);

/// Slack on [`MAX_KEY_LIFETIME`] for a caller whose clock runs ahead, as wide as
/// the signature's own timestamp tolerance.
const KEY_LIFETIME_SKEW: TimeDelta = TimeDelta::minutes(5);

// POST /api/v1/users/register
//
// Unauthenticated: the pre-auth secret is the credential where there is one, and
// without it the account created may do nothing until an operator admits it.
pub async fn register_user_key(
    State(state): State<AppState>,
    audit: Audit,
    ClientIp(client_ip): ClientIp,
    ApiJson(body): ApiJson<RegisterUserKeyRequest>,
) -> Result<impl IntoResponse, AppError> {
    match (body.user_preauth_key, body.contact_email) {
        (Some(token), None) => {
            let token = UserPreauthToken::new(token.expose());
            register_against_invite(&state, &audit, token, body.public_key, body.key_details).await
        }
        (None, Some(email)) => {
            let (key, details) = (body.public_key, body.key_details);
            request_account(&state, &audit, client_ip, email, key, details).await
        }
        (Some(_), Some(_)) => Err(AppError::BadRequest(
            "user_preauth_key and contact_email are exclusive: an invite already names the \
             account this key joins"
                .into(),
        )),
        (None, None) => Err(AppError::BadRequest(
            "one of user_preauth_key or contact_email is required".into(),
        )),
    }
}

/// The invited path: the pre-auth key names an existing account, and the
/// registration it admits is approved already.
async fn register_against_invite(
    state: &AppState,
    audit: &Audit,
    token: UserPreauthToken,
    public_key: PublicKeyHex,
    key_details: NonEmptyTrimmedString,
) -> Result<axum::response::Response, AppError> {
    let (key_id, secret) = token
        .parse_parts()
        .ok_or_else(|| AppError::Unauthorized("malformed pre-auth key".into()))?;

    // Consumed before anything is looked up: reading the claim first would
    // publish an account id to anyone holding a public key. So a retry needs a
    // freshly minted key.
    let grant = match state
        .user_store
        .consume_user_preauth_key(&key_id, &secret)
        .await
        .map_err(|e| {
            AppError::Internal(format!("failed to consume pre-auth key {key_id}: {e:#}"))
        })? {
        UserPreauthConsumeOutcome::Granted(grant) => grant,
        UserPreauthConsumeOutcome::Rejected(rejection) => {
            tracing::warn!(%key_id, reason = %rejection, "user pre-auth key rejected at registration");
            return Err(AppError::Unauthorized(rejection.to_string()));
        }
    };
    // Named now: the key is spent whatever follows.
    audit
        .involve(
            state,
            [AuditTarget::User {
                user_id: grant.user_id.clone(),
            }],
        )
        .await;

    // A key can outlive the account it names, so a consume is a precondition
    // for loading the account, never a promise that it is there.
    let user = state
        .user_store
        .get_user(&grant.user_id)
        .await
        .map_err(|e| AppError::Internal(format!("failed to load the user: {e:#}")))?
        .ok_or_else(|| {
            tracing::warn!(
                user_id = %grant.user_id,
                %key_id,
                "pre-auth key names an account that no longer exists"
            );
            AppError::Unauthorized("invalid pre-auth key".into())
        })?;

    // A key an administrator minted over HTTP carries that administrator's
    // authority, which must still hold now that it is being used.
    if let Some(minter) = &grant.minted_by {
        crate::api_v1::users::admit_delegated(state, minter, &user.user_id).await?;
    }

    let registration = register_key(state, &user, public_key, key_details, None).await?;
    audit.acted_as(AuditActor::User {
        user_id: user.user_id.clone(),
        key_id: registration.key_id.clone(),
    });

    tracing::info!(
        user_id = %user.user_id,
        key_id = %registration.key_id,
        preauth_key_id = %key_id,
        status = %user.status,
        "user key registered"
    );

    Ok((
        StatusCode::CREATED,
        Json(UserKeyRegistered {
            user_id: user.user_id,
            status: user.status,
            key_id: registration.key_id,
        }),
    )
        .into_response())
}

/// The request path: no invite, so this creates the account the address names and
/// leaves it [`UserStatus::Pending`](pipette_plan_types::UserStatus::Pending) for
/// an operator to admit. Claim, key, then record, so each failure is recoverable.
async fn request_account(
    state: &AppState,
    audit: &Audit,
    client_ip: Option<IpAddr>,
    contact_email: ContactEmail,
    public_key: PublicKeyHex,
    key_details: NonEmptyTrimmedString,
) -> Result<axum::response::Response, AppError> {
    // As `POST /clients/register` refuses: a deployment that mints every identity
    // by hand closes the keyless path for both kinds.
    if state.config.require_user_preauth_key {
        tracing::warn!("refused: registration requires a pre-auth key");
        return Err(AppError::Forbidden(
            "registration requires a pre-auth key".into(),
        ));
    }

    // Mints the id and folds the address, which is what makes the record's
    // `contact_email` the index key claimed just below.
    let user = User::requested(contact_email, Utc::now())
        .map_err(|e| AppError::BadRequest(format!("invalid contact_email: {e}")))?;

    // An unverified claim here would hold the address against the Clerk sign-in
    // that verifies it.
    if let Some(rule) = state
        .clerk
        .as_deref()
        .and_then(|clerk| clerk.provision_rule(&user.contact_email))
    {
        tracing::warn!("refused: the address is at a domain Clerk provisions");
        return Err(AppError::Forbidden(format!(
            "addresses at {} sign in to the console through Clerk, which creates the account",
            rule.domain
        )));
    }
    state.registration_throttle.admit(client_ip)?;

    // First, and exclusively: an address names one account, so two machines
    // asking for one address contend for this key and exactly one wins.
    match state
        .user_store
        .claim_user_email(user.contact_email.as_str(), &user.user_id)
        .await
        .map_err(|e| AppError::Internal(format!("failed to claim the address: {e:#}")))?
    {
        EmailClaim::Claimed => {}
        EmailClaim::Taken(holder) => {
            return address_taken(state, audit, &holder, &public_key).await;
        }
    }

    // A failure between the next two writes leaves an inert key registration and
    // frees the address; the caller mints a fresh keypair per attempt to retry.
    let created = create_requested_account(state, &user, public_key, key_details).await;
    let registration = match created {
        Ok(registration) => registration,
        Err(error) => {
            if let Err(release) = state
                .user_store
                .release_user_email(user.contact_email.as_str())
                .await
            {
                tracing::warn!(
                    user_id = %user.user_id,
                    error = %release,
                    "failed to release the address claim after a requested registration failed; \
                     `users release-claim <email>` will clear it"
                );
            }
            return Err(error);
        }
    };
    audit.acted_as(AuditActor::User {
        user_id: user.user_id.clone(),
        key_id: registration.key_id.clone(),
    });

    tracing::info!(
        user_id = %user.user_id,
        key_id = %registration.key_id,
        status = %user.status,
        "user account requested"
    );

    Ok((
        StatusCode::CREATED,
        Json(UserKeyRegistered {
            user_id: user.user_id,
            status: user.status,
            key_id: registration.key_id,
        }),
    )
        .into_response())
}

/// The two writes that make a requested account. No permissions object: an
/// absent one is the empty set, and the operator grants at `users approve`.
async fn create_requested_account(
    state: &AppState,
    user: &User,
    public_key: PublicKeyHex,
    key_details: NonEmptyTrimmedString,
) -> Result<UserKey, AppError> {
    let registration = register_key(state, user, public_key, key_details, None).await?;
    state
        .user_store
        .put_user(user)
        .await
        .map_err(|e| AppError::Internal(format!("failed to write the user record: {e:#}")))?;
    Ok(registration)
}

/// What to answer a request for an address some account already holds: idempotent
/// only when this keypair is registered there, a retried `201`. Anything else would
/// let a stranger have an operator approve their key.
async fn address_taken(
    state: &AppState,
    audit: &Audit,
    holder: &UserId,
    public_key: &PublicKeyHex,
) -> Result<axum::response::Response, AppError> {
    let existing = state
        .user_store
        .get_user(holder)
        .await
        .map_err(|e| AppError::Internal(format!("failed to load the user: {e:#}")))?;
    let Some(existing) = existing else {
        // A claim whose record is missing: a failed create or one in flight, which
        // look identical from here, so only an operator resolves it.
        tracing::warn!(%holder, "a registration asked for an address held by a stranded claim");
        return Err(AppError::Conflict(
            "that address is claimed by an account that does not exist yet; retry, or ask an \
             operator to clear the claim"
                .into(),
        ));
    };

    let key_id = UserKeyId::derive(public_key);
    if state
        .user_store
        .get_user_key(&existing.user_id, &key_id)
        .await
        .map_err(|e| AppError::Internal(format!("failed to load the user key: {e:#}")))?
        .is_none()
    {
        tracing::warn!(
            user_id = %existing.user_id,
            %key_id,
            "refused an account request for an address another account holds"
        );
        return Err(AppError::Conflict(
            "an account already holds that address".into(),
        ));
    }

    tracing::info!(
        user_id = %existing.user_id,
        %key_id,
        status = %existing.status,
        "idempotent account request; the key is already registered to the account"
    );
    audit.acted_as(AuditActor::User {
        user_id: existing.user_id.clone(),
        key_id: key_id.clone(),
    });
    audit.unchanged();
    Ok((
        StatusCode::OK,
        Json(UserKeyRegistered {
            user_id: existing.user_id,
            status: existing.status,
            key_id,
        }),
    )
        .into_response())
}

// POST /api/v1/users/me/keys
//
// No cap on keys per account: each registration is signed by a key it holds, so
// an account can only flood itself.
pub async fn add_own_key(
    State(state): State<AppState>,
    ActiveUser { user, .. }: ActiveUser,
    ApiJson(body): ApiJson<AddUserKeyRequest>,
) -> Result<impl IntoResponse, AppError> {
    // Its organization decides which machines a job signs from, so a leaked
    // key cannot mint itself company.
    if user.account_type == UserAccountType::Service {
        tracing::warn!(user_id = %user.user_id, "refused: a service account adding its own key");
        return Err(AppError::Forbidden(
            "a service account adds no key itself; its administrator mints it a pre-auth key"
                .into(),
        ));
    }
    let registration = register_key(
        &state,
        &user,
        body.public_key,
        body.key_details,
        body.expires_at,
    )
    .await?;

    tracing::info!(
        user_id = %user.user_id,
        key_id = %registration.key_id,
        "user registered an additional key"
    );

    Ok((
        StatusCode::CREATED,
        Json(UserKeyRegistered {
            user_id: user.user_id,
            status: user.status,
            key_id: registration.key_id,
        }),
    ))
}

// GET /api/v1/users/me
//
// The one route that may name only its key, which is how a holder of nothing
// but the key learns its account.
pub async fn get_own_user(
    State(state): State<AppState>,
    KeyOnlyUser(AuthenticatedUser { user, .. }): KeyOnlyUser,
) -> Result<impl IntoResponse, AppError> {
    let stored = state
        .user_store
        .get_user_permissions(&user.user_id)
        .await
        .map_err(|e| AppError::Internal(format!("failed to read the user's permissions: {e:#}")))?;
    let keys = state
        .user_store
        .list_user_keys(&user.user_id)
        .await
        .map_err(|e| AppError::Internal(format!("failed to list the user's keys: {e:#}")))?;
    let first_clerk_sign_in = state
        .user_store
        .get_user_first_clerk_sign_in(&user.user_id)
        .await
        .map_err(|e| AppError::Internal(format!("failed to read the first sign-in: {e:#}")))?;

    let org_id = user_org(&state, &user.user_id).await?;
    // A membership naming a deleted organization has no handle to show.
    let org_slug = match &org_id {
        Some(org_id) => state
            .org_store
            .get_org(org_id)
            .await
            .map_err(|e| {
                AppError::Internal(format!("failed to read the account's organization: {e:#}"))
            })?
            .map(|org| org.slug),
        None => None,
    };

    let permissions = pipette_core::user_permission::effective(
        stored,
        pipette_core::user_permission::Holder::of(&user),
    );
    let may = abilities(&state, &user).await?;
    Ok(Json(OwnUserView {
        user: user.into(),
        org_id,
        org_slug,
        permissions: UserPermissionSet::from(&permissions),
        keys: keys.into_iter().map(Into::into).collect(),
        first_clerk_sign_in,
        may,
    }))
}

/// What `user` may do, from the gates and scope functions its routes judge it
/// by, so no rule is stated twice. An account that may not act yet may do
/// nothing.
async fn abilities(state: &AppState, user: &User) -> Result<UserAbilities, AppError> {
    if user.status != UserStatus::Active {
        return Ok(UserAbilities::default());
    }
    let rights = UserRights::of_account(state, user).await?;
    let caller = Caller {
        user,
        rights: &rights,
    };
    let roster = |scope: RosterScope| match scope {
        RosterScope::Fleet => Reach::Every,
        RosterScope::Org(_) => Reach::Own,
        RosterScope::Unaffiliated => Reach::None,
    };
    let grant: Vec<UserPermission> = <UserPermission as strum::IntoEnumIterator>::iter()
        .filter(|name| rights.refused_grants([*name]).is_empty())
        .collect();
    Ok(UserAbilities {
        read_roster: roster(crate::api_v1::users::read_reach(state, caller).await?),
        administer_roster: roster(crate::api_v1::orgs::catalog::write_reach(state, caller).await?),
        read_fleet: crate::router::READ_FLEET.allows(&rights),
        administer_fleet: match crate::api_v1::clients::device_reach(state, caller).await? {
            Some(DeviceScope::Fleet) => Reach::Every,
            Some(DeviceScope::Org(_)) => Reach::Own,
            Some(DeviceScope::Unaffiliated) | None => Reach::None,
        },
        edit_catalog: roster(crate::api_v1::descriptor_catalog::edit_reach(state, caller).await?),
        read_plans: crate::api_v1::plans::reads::PlanRead::reach(state, caller).await?,
        file_plans: crate::router::FILE_PLANS.allows(&rights),
        create_orgs: crate::router::CREATE_ORGS.allows(&rights),
        grant_service: grant
            .iter()
            .copied()
            .filter(|name| UserAccountType::Service.may_hold(*name))
            .collect(),
        grant,
    })
}

// DELETE /api/v1/users/me/keys/{key_id}
//
// Revoking the last key, or the signing one, is permitted: a compromised key
// must retire at once.
pub async fn revoke_own_key(
    State(state): State<AppState>,
    ActiveUser { user, .. }: ActiveUser,
    ApiPath(Parsed(key_id)): ApiPath<Parsed<UserKeyId>>,
) -> Result<impl IntoResponse, AppError> {
    revoke_key(&state, &user.user_id, &key_id, &user.user_id).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Retire one of `user_id`'s keys, logging `signer` (the account or its
/// administrator) as `revoked_by`. The caller admits both; this checks the
/// key is `user_id`'s.
pub(crate) async fn revoke_key(
    state: &AppState,
    user_id: &UserId,
    key_id: &UserKeyId,
    signer: &UserId,
) -> Result<(), AppError> {
    let revoked = pipette_pipeline::user_admin::revoke_key(
        &*state.user_store,
        &*state.key_index_store,
        user_id,
        key_id,
    )
    .await
    .map_err(|e| AppError::Internal(format!("failed to revoke the user key: {e:#}")))?;
    // Reported rather than silently accepted: a `204` for a key this account
    // never held would report a revocation that did not happen.
    if !revoked {
        return Err(AppError::NotFound("no such key on this account".into()));
    }
    tracing::info!(%user_id, %key_id, revoked_by = %signer, "user key revoked");
    Ok(())
}

/// Claim the keypair, then write the registration: a lost race leaves no
/// registration, and a crash between leaves a key that authenticates nothing.
pub(crate) async fn register_key(
    state: &AppState,
    user: &User,
    public_key: PublicKeyHex,
    key_details: NonEmptyTrimmedString,
    expires_at: Option<DateTime<Utc>>,
) -> Result<UserKey, AppError> {
    // Bounded here so both registration paths inherit it.
    if key_details.chars().count() > KEY_DETAILS_MAX_CHARS {
        return Err(AppError::BadRequest(format!(
            "key_details is longer than {KEY_DETAILS_MAX_CHARS} characters"
        )));
    }
    let now = Utc::now();
    if expires_at.is_some_and(|at| at <= now) {
        return Err(AppError::BadRequest("expires_at has already passed".into()));
    }
    if expires_at.is_some_and(|at| at > now + MAX_KEY_LIFETIME + KEY_LIFETIME_SKEW) {
        return Err(AppError::BadRequest(format!(
            "expires_at is more than {} days away",
            MAX_KEY_LIFETIME.num_days()
        )));
    }

    let digest = digest_of(&public_key);
    let holder = KeyHolderPrincipalId::User(user.user_id.clone());

    let claim = state
        .key_index_store
        .claim_public_key(&digest, &holder)
        .await
        .map_err(|e| AppError::Internal(format!("failed to claim the public key: {e:#}")))?;

    // Held by *this* account it is the idempotent retry, rewritten below. Held
    // by anyone else it is refused: registering one keypair under both kinds is
    // the permission-partition bypass this claim exists to prevent.
    if let KeyClaim::Taken(incumbent) = &claim
        && !claim.is_held_by(&holder)
    {
        tracing::warn!(
            user_id = %user.user_id,
            %incumbent,
            "refused a key registration for a keypair another principal holds"
        );
        return Err(AppError::Conflict(
            "this keypair is already registered to another principal".into(),
        ));
    }

    // Before the key exists, so an account never holds a key while still
    // looking unsigned-in to an administrator minting its pre-auth key.
    state
        .user_store
        .clear_user_awaiting_sign_in(&user.user_id)
        .await
        .map_err(|e| AppError::Internal(format!("failed to record the first sign-in: {e:#}")))?;
    let registration = UserKey {
        expires_at,
        ..UserKey::new(public_key, key_details, now)
    };
    state
        .user_store
        .put_user_key(&user.user_id, &registration)
        .await
        .map_err(|e| AppError::Internal(format!("failed to write the key registration: {e:#}")))?;

    Ok(registration)
}
