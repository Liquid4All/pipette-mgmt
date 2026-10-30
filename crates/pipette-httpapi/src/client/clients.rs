//! A device's own record: `POST /clients/register`, `GET /clients/me` and
//! `PATCH /clients/me`.

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use chrono::Utc;
use futures::{StreamExt, TryStreamExt};

use pipette_core::client::{Client, ClientStatus};
use pipette_core::client_permission::ClientPermissionSet;
use pipette_core::client_preauth;
use pipette_core::client_preauth::{
    ClientPreauthConsumeOutcome, ClientPreauthRejectionError, ClientPreauthToken,
};
use pipette_core::types::DeriveFromKey;
use pipette_pipeline::client_admin;
use pipette_pipeline::submission::ValidationError;
use pipette_plan_types::{
    AuditActor, ClientId, ClientOwner, ClientPrivateKey, ClientRecord, ClientRegistered,
    DeviceFormFactor, DeviceProfile, DeviceProfileFields, NonEmptyTrimmedString, PublicKeyHex,
    Refused, RegisterClient, UpdateClient,
};
use pipette_store::STORAGE_CONCURRENCY;

use crate::audit::Audit;
use crate::auth::AuthenticatedClient;
use crate::client::benchmarks::submissions::relinquish_client_leases;
use crate::error::AppError;
use crate::extract::ApiJson;
use crate::registration_throttle::ClientIp;
use crate::state::AppState;

const INVALID_FORM_FACTOR: &str = "Invalid device_form_factor (must be one of: \
     phone, tablet, laptop, desktop, server, embedded)";

struct ProfileUpdate(DeviceProfileFields);

impl ProfileUpdate {
    /// Absent fields leave the stored value unchanged.
    fn apply_to(self, base: &mut DeviceProfile) -> Result<(), AppError> {
        if let Some(raw) = self.0.device_form_factor {
            let parsed = raw
                .trim()
                .parse::<DeviceFormFactor>()
                .map_err(|_| AppError::BadRequest(INVALID_FORM_FACTOR.to_string()))?;
            base.device_form_factor = Some(parsed);
        }
        macro_rules! merge {
            ($field:ident) => {
                if let Some(v) = self.0.$field {
                    base.$field = Some(v);
                }
            };
        }
        merge!(device_name);
        merge!(device_os_name);
        merge!(device_os_version);
        merge!(device_cpu_arch);
        merge!(device_os_family);
        merge!(device_chip_model);
        merge!(device_ram_bytes);
        merge!(device_gpu_model);
        merge!(device_gpu_vram_bytes);
        merge!(device_npu_model);
        merge!(device_npu_vram_bytes);
        Ok(())
    }
}

/// Dependency checks on the **merged** profile. The os-version rule has no
/// [`ValidationError`] variant: a submission requires both fields.
fn validate_device_profile(profile: &DeviceProfile) -> Result<(), AppError> {
    if profile.device_os_version.is_some() && profile.device_os_name.is_none() {
        return Err(AppError::BadRequest(
            "device_os_version requires device_os_name".into(),
        ));
    }
    if profile.device_gpu_vram_bytes.is_some() && profile.device_gpu_model.is_none() {
        return Err(AppError::BadRequest(
            ValidationError::GpuVramRequiresGpuModel.to_string(),
        ));
    }
    if profile.device_npu_vram_bytes.is_some() && profile.device_npu_model.is_none() {
        return Err(AppError::BadRequest(
            ValidationError::NpuVramRequiresNpuModel.to_string(),
        ));
    }
    Ok(())
}

/// The canonical-form check is security, not cosmetics: it makes the reserved
/// check exact, so `OS:ios` cannot smuggle a server-owned flag past it.
fn validate_capabilities(
    capabilities: &std::collections::BTreeSet<String>,
) -> Result<(), AppError> {
    capabilities.iter().try_for_each(|cap| {
        if cap.trim().is_empty() {
            return Err(AppError::BadRequest(
                "capability flag must not be empty".into(),
            ));
        }
        if *cap != pipette_core::client::slugify(cap) {
            return Err(AppError::BadRequest(format!(
                "capability flag '{cap}' must be lowercase with no whitespace"
            )));
        }
        if pipette_core::client::is_reserved_capability(cap) {
            return Err(AppError::BadRequest(format!(
                "capability flag '{cap}' uses a reserved namespace (server-owned, derived from the device profile)"
            )));
        }
        if pipette_core::client::is_tag_capability(cap) {
            return Err(AppError::BadRequest(format!(
                "capability flag '{cap}' names a tag; an operator assigns tags (pre-auth key, `clients tag add`)"
            )));
        }
        Ok(())
    })
}

fn client_profile_response(
    client: Client,
    tags: std::collections::BTreeSet<pipette_plan_types::Tag>,
    permissions: ClientPermissionSet,
    reindex_pending: bool,
) -> ClientRecord {
    // Exhaustive, so a new device field won't compile until it is surfaced here.
    let Client {
        client_id,
        organization,
        client_details,
        contact_email,
        status,
        device_profile,
        capabilities,
        ..
    } = client;
    let DeviceProfile {
        device_name,
        device_form_factor,
        device_os_name,
        device_os_version,
        device_cpu_arch,
        device_os_family,
        device_chip_model,
        device_ram_bytes,
        device_gpu_model,
        device_gpu_vram_bytes,
        device_npu_model,
        device_npu_vram_bytes,
    } = device_profile;
    let text = |value: Option<NonEmptyTrimmedString>| value.map(NonEmptyTrimmedString::into_inner);
    ClientRecord {
        client_id,
        organization: organization.into_inner(),
        client_details: client_details.into_inner(),
        contact_email: contact_email.into_inner(),
        status: status.into(),
        tags: tags.into_iter().map(|tag| tag.into_inner()).collect(),
        permissions: permissions
            .names()
            .into_iter()
            .map(str::to_string)
            .collect(),
        reindex_pending,
        capabilities: capabilities.into_iter().collect(),
        device_name: text(device_name),
        device_form_factor,
        device_os_name: text(device_os_name),
        device_os_version: text(device_os_version),
        device_cpu_arch: text(device_cpu_arch),
        device_os_family: text(device_os_family),
        device_chip_model: text(device_chip_model),
        device_ram_bytes,
        device_gpu_model: text(device_gpu_model),
        device_gpu_vram_bytes,
        device_npu_model: text(device_npu_model),
        device_npu_vram_bytes,
    }
}

/// Refused rather than dropped, so a client never believes it took effect. Not
/// `deny_unknown_fields`: serde rejects it beside the flattened device profile.
fn reject_client_supplied_permissions(permissions: Option<Refused>) -> Result<(), AppError> {
    match permissions {
        Some(_) => Err(AppError::BadRequest(
            "permissions are assigned by an operator".into(),
        )),
        None => Ok(()),
    }
}

// POST /clients/register

pub async fn register_client(
    State(state): State<AppState>,
    audit: Audit,
    ClientIp(client_ip): ClientIp,
    ApiJson(body): ApiJson<RegisterClient>,
) -> Result<impl IntoResponse, AppError> {
    let (public_key, private_key_hex) = match (body.public_key, body.generate_key.unwrap_or(false))
    {
        (Some(pk), false) => (pk, None),
        (None, true) => {
            let signing_key = crate::auth::generate_signing_key()?;
            let pk = PublicKeyHex::try_new(hex::encode(signing_key.verifying_key().as_bytes()))
                .map_err(|e| AppError::Internal(format!("generated invalid key: {e}")))?;
            let sk_hex = ClientPrivateKey::new(hex::encode(signing_key.to_bytes()));
            (pk, Some(sk_hex))
        }
        _ => {
            return Err(AppError::BadRequest(
                "exactly one of public_key or generate_key must be provided".into(),
            ));
        }
    };

    let client_id = ClientId::derive(&public_key);
    // Registration is unauthenticated, so `AuthenticatedClient` never runs and
    // the derived id is the only `client_id` the request span can carry.
    tracing::Span::current().record("client_id", tracing::field::display(&client_id));
    // Unproven until the device is on record: a key in a body proves nothing.
    audit.acted_as(AuditActor::Unverified {
        claimed_id: Some(client_id.to_string()),
    });

    // Idempotent, before any key is consumed: a client that failed to persist
    // its registration locally retries with the same keypair to recover its id.
    if let Some(existing) = state
        .client_store
        .get_client(&client_id)
        .await
        .map_err(|e| {
            AppError::Internal(format!(
                "failed to look up the client record during registration: {e:#}"
            ))
        })?
    {
        tracing::info!(
            status = %existing.status,
            "idempotent re-registration; returning existing client without consuming a pre-auth key"
        );
        audit.unchanged();
        return Ok((
            StatusCode::OK,
            Json(ClientRegistered {
                client_id: existing.client_id,
                status: existing.status.into(),
                private_key: None,
            }),
        ));
    }

    // Before consuming any pre-auth key, so a `400` never burns a use.
    let mut device_profile = DeviceProfile::default();
    ProfileUpdate(body.device).apply_to(&mut device_profile)?;
    validate_device_profile(&device_profile)?;
    validate_capabilities(&body.capabilities)?;
    reject_client_supplied_permissions(body.permissions)?;

    // Refused before the keypair claim, with the key only read: a claim an
    // anonymous refusal left behind would bar the keypair from user registration.
    let client_preauth_key = body
        .client_preauth_key
        .map(|key| ClientPreauthToken::new(key.expose().to_owned()));
    let presented = match client_preauth_key.as_ref() {
        Some(token) => {
            let (key_id, secret) = token
                .parse_parts()
                .ok_or_else(|| AppError::Unauthorized("malformed pre-auth key".into()))?;
            let stored = state
                .client_store
                .get_client_preauth_key(&key_id)
                .await
                .map_err(|e| {
                    AppError::Internal(format!("failed to read pre-auth key {key_id}: {e:#}"))
                })?;
            let key = stored
                .ok_or(ClientPreauthRejectionError::NotFound)
                .and_then(|key| {
                    client_preauth::validate(&key, &secret, Utc::now()).map(|_| key)
                })
                .map_err(|rejection| {
                    tracing::warn!(%key_id, reason = %rejection, "pre-auth key rejected at registration");
                    AppError::Unauthorized(rejection.to_string())
                })?;
            // After the secret, so only its holder learns the key was delegated.
            if let Some(minter) = &key.minted_by {
                crate::api_v1::orgs::client_preauth::admit_delegated(
                    &state,
                    minter,
                    key.default_owner.as_ref(),
                )
                .await?;
            }
            Some((key_id, secret))
        }
        None if state.config.require_client_preauth_key => {
            tracing::warn!("refused: registration requires a pre-auth key");
            return Err(AppError::Forbidden(
                "registration requires a pre-auth key".into(),
            ));
        }
        None => {
            state.registration_throttle.admit(client_ip)?;
            None
        }
    };

    // One keypair must not be both a client and a user key, or its holder picks
    // a principal per request. Claimed before the consume and the record writes.
    let digest = pipette_core::key_index::digest_of(&public_key);
    let holder = pipette_core::key_index::KeyHolderPrincipalId::Client(client_id.clone());
    let claim = state
        .key_index_store
        .claim_public_key(&digest, &holder)
        .await
        .map_err(|e| AppError::Internal(format!("failed to claim the public key: {e:#}")))?;
    if let pipette_core::key_index::KeyClaim::Taken(incumbent) = &claim
        && !claim.is_held_by(&holder)
    {
        tracing::warn!(%incumbent, "refused a registration for a keypair another principal holds");
        return Err(AppError::Conflict(
            "this keypair is already registered to another principal".into(),
        ));
    }

    let mut organization = body.organization;
    let mut owner = None;
    let mut seeded_tags: std::collections::BTreeSet<pipette_plan_types::Tag> =
        std::collections::BTreeSet::new();
    let mut permissions = pipette_core::client_permission::REGISTRATION_DEFAULT.clone();
    let status = if let Some((key_id, secret)) = presented {
        match state
            .client_store
            .consume_client_preauth_key(&key_id, &secret)
            .await
            .map_err(|e| {
                AppError::Internal(format!("failed to consume pre-auth key {key_id}: {e:#}"))
            })? {
            ClientPreauthConsumeOutcome::Granted(grant) => {
                if let Some(org) = grant.default_organization {
                    organization = org;
                }
                // After the consume: a gone owner never resolves again, so
                // burning a single-use key loses nothing.
                if let Some(key_owner) = grant.default_owner {
                    let exists = pipette_pipeline::org::owner_exists(
                        state.org_store.as_ref(),
                        state.user_store.as_ref(),
                        &key_owner,
                    )
                    .await
                    .map_err(|e| {
                        AppError::Internal(format!(
                            "failed to resolve the pre-auth key's owner: {e:#}"
                        ))
                    })?;
                    if !exists {
                        tracing::warn!(%key_id, owner = %key_owner.label(), "pre-auth key names an owner that no longer exists");
                        return Err(AppError::Unauthorized(
                            "pre-auth key names an owner that no longer exists; ask for a new key"
                                .into(),
                        ));
                    }
                    owner = Some(key_owner);
                }
                seeded_tags = grant.default_tags;
                if let Some(template) = grant.default_permissions {
                    permissions = template.into_iter().collect();
                }
                tracing::info!(
                    %key_id,
                    seeded_tags = seeded_tags.len(),
                    seeded_permissions = permissions.len(),
                    seeded_owner = ?owner.as_ref().map(ClientOwner::label),
                    "pre-auth key consumed at registration"
                );
                ClientStatus::Approved
            }
            ClientPreauthConsumeOutcome::Rejected(rejection) => {
                // Lost a concurrent spend. The claim stays: a same-keypair registration may own it.
                tracing::warn!(%key_id, reason = %rejection, "pre-auth key rejected at registration");
                return Err(AppError::Unauthorized(rejection.to_string()));
            }
        }
    } else if state
        .config
        .auto_approve
        .approves(body.contact_email.as_str())
    {
        ClientStatus::Approved
    } else {
        ClientStatus::Pending
    };

    let client = Client {
        client_id: client_id.clone(),
        public_key,
        organization,
        client_details: body.client_details,
        contact_email: body.contact_email,
        status,
        registered_at: Utc::now(),
        device_profile,
        capabilities: body.capabilities,
    };

    // Before the record, and not best-effort: a later failure leaves only an
    // inert object that a retry overwrites.
    state
        .client_store
        .put_client_permissions(&client_id, &permissions)
        .await
        .map_err(|e| {
            AppError::Internal(format!(
                "failed to write the permissions during registration: {e:#}"
            ))
        })?;

    // Before the record, like the permissions: a key-owned device never exists
    // unowned, and the id never inherits an owner a failed teardown left behind.
    state
        .org_store
        .set_ownership(&client_id, owner)
        .await
        .map_err(|e| {
            AppError::Internal(format!(
                "failed to write the owner during registration: {e:#}"
            ))
        })?;

    // Before the record, like the owner. Not best-effort: a job can require a
    // seeded tag.
    state
        .tag_store
        .delete_client_tags(&client_id)
        .await
        .map_err(|e| {
            AppError::Internal(format!(
                "failed to clear leftover tags during registration: {e:#}"
            ))
        })?;
    futures::stream::iter(seeded_tags.iter().cloned().map(|tag| {
        let tag_store = state.tag_store.clone();
        let client_id = client_id.clone();
        async move { tag_store.add_client_tag(&client_id, &tag).await }
    }))
    .buffer_unordered(STORAGE_CONCURRENCY)
    .try_collect::<Vec<()>>()
    .await
    .map_err(|e| {
        AppError::Internal(format!(
            "failed to write the seeded tags during registration: {e:#}"
        ))
    })?;

    state.client_store.put_client(&client).await.map_err(|e| {
        AppError::Internal(format!(
            "failed to write the client record during registration: {e:#}"
        ))
    })?;

    tracing::info!(
        status = %status,
        has_device_profile = !client.device_profile.is_empty(),
        capabilities = client.capabilities.len(),
        seeded_tags = seeded_tags.len(),
        permissions = ?permissions.names(),
        "client registered"
    );

    // The one path where a credential crosses the wire; its secrecy rests on a
    // TLS terminator this process cannot check.
    if private_key_hex.is_some() {
        tracing::warn!("returned a server-generated private key in the registration response");
    }

    // Covers an id deleted and re-registered between two `queue-maintenance`
    // runs, which does not count as joining. Best-effort: `flag-reindex` repairs.
    if (!client.device_profile.is_empty()
        || !client.capabilities.is_empty()
        || !seeded_tags.is_empty())
        && let Err(e) = state.todo_store.write_pending_reindex(&client_id).await
    {
        tracing::warn!(
            error = %e,
            "failed to queue newly registered client for eligible-index reindex"
        );
    }

    audit.acted_as(AuditActor::Client {
        client_id: client_id.clone(),
    });
    Ok((
        StatusCode::CREATED,
        Json(ClientRegistered {
            client_id,
            status: status.into(),
            private_key: private_key_hex,
        }),
    ))
}

// GET /clients/me
pub async fn get_own_client(
    State(state): State<AppState>,
    AuthenticatedClient(client): AuthenticatedClient,
) -> Result<impl IntoResponse, AppError> {
    let tags = state
        .tag_store
        .get_client_tags(&client.client_id)
        .await
        .map_err(|e| AppError::Internal(format!("failed to read the client tags: {e:#}")))?;
    // Lets a client watch for its queue standing instead of blind-polling `claim`.
    let reindex_pending = state
        .todo_store
        .has_pending_reindex(&client.client_id)
        .await
        .map_err(|e| {
            AppError::Internal(format!("failed to read the pending-reindex flag: {e:#}"))
        })?;
    let permissions =
        client_admin::resolved_permissions(state.client_store.as_ref(), &client.client_id).await?;
    Ok(Json(client_profile_response(
        client,
        tags,
        permissions,
        reindex_pending,
    )))
}

// PATCH /clients/me

pub async fn update_own_client(
    State(state): State<AppState>,
    AuthenticatedClient(read): AuthenticatedClient,
    audit: Audit,
    ApiJson(body): ApiJson<UpdateClient>,
) -> Result<impl IntoResponse, AppError> {
    reject_client_supplied_permissions(body.permissions)?;
    let mut client = read.clone();

    let details_changed = body
        .client_details
        .as_ref()
        .is_some_and(|details| *details != client.client_details);
    if let Some(details) = body.client_details {
        client.client_details = details;
    }

    // Validate the merged result, so a lone `device_gpu_vram_bytes` is checked
    // against the stored `device_gpu_model`.
    let profile_before = client.device_profile.clone();
    ProfileUpdate(body.device).apply_to(&mut client.device_profile)?;
    validate_device_profile(&client.device_profile)?;
    let profile_changed = client.device_profile != profile_before;

    let capabilities_changed = if let Some(capabilities) = body.capabilities {
        validate_capabilities(&capabilities)?;
        let changed = capabilities != client.capabilities;
        client.capabilities = capabilities;
        changed
    } else {
        false
    };
    let matching_input_changed = profile_changed || capabilities_changed;
    // A device repeats its profile at every start; only a change is an event.
    audit.changed(details_changed || matching_input_changed);

    // Both repairs run before the record is persisted, so a failed PATCH
    // retries with the same diff. The flag goes first: it gates `claim`, so a
    // concurrent claim cannot re-acquire a just-relinquished job.
    if matching_input_changed {
        state
            .todo_store
            .write_pending_reindex(&client.client_id)
            .await
            .map_err(|e| {
                AppError::Internal(format!(
                    "failed to queue the client for reindex before the profile write: {e:#}"
                ))
            })?;
        relinquish_client_leases(
            &*state.todo_store,
            &*state.submission_store,
            &client.client_id,
        )
        .await?;
    }

    let replaced = state
        .client_store
        .replace_client(&read, &client)
        .await
        .map_err(|e| {
            AppError::Internal(format!("failed to write the updated client record: {e:#}"))
        })?;
    if !replaced {
        return Err(AppError::Conflict(
            "the client record changed or was deleted during the update; the record was not updated, resend the request"
                .to_string(),
        ));
    }

    if matching_input_changed {
        // A flag postdating the durable record, so a rebuild that read the old
        // one cannot consume it. A failure here is covered by the recheck in
        // `reindex_flagged_clients` and `flag-reindex`.
        state
            .todo_store
            .write_pending_reindex(&client.client_id)
            .await
            .map_err(|e| {
                AppError::Internal(format!(
                    "failed to queue the client for reindex after the profile write: {e:#}"
                ))
            })?;
        tracing::info!(
            profile_changed,
            capabilities_changed,
            details_changed,
            "client matching profile updated"
        );
    } else if details_changed {
        tracing::info!("client details updated");
    }

    let tags = state
        .tag_store
        .get_client_tags(&client.client_id)
        .await
        .map_err(|e| {
            AppError::Internal(format!(
                "failed to read the client tags after the profile write: {e:#}"
            ))
        })?;
    // Read, not `profile_changed`: an earlier change's flag may still be pending.
    let reindex_pending = state
        .todo_store
        .has_pending_reindex(&client.client_id)
        .await
        .map_err(|e| {
            AppError::Internal(format!(
                "failed to read the pending-reindex flag after the profile write: {e:#}"
            ))
        })?;
    let permissions =
        client_admin::resolved_permissions(state.client_store.as_ref(), &client.client_id).await?;
    Ok(Json(client_profile_response(
        client,
        tags,
        permissions,
        reindex_pending,
    )))
}
