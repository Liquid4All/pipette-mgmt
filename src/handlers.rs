use std::collections::HashMap;
use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use chrono::{DateTime, Utc};
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::auth::{AuthenticatedClient, MigratedClients, ReplayCache};
use crate::benchmark::{BenchmarkDef, BenchmarkType};
use crate::catalog_cache::CatalogCache;
use crate::client::{self, Client, ClientStatus, DeviceProfile};
use crate::config::Config;
use crate::error::AppError;
use crate::extract::ApiJson;
use crate::scoring_service::ChatMessage;
use crate::stores::{
    AuthStore, EvalSampleResultStore, JobState, RecycleResult, RenewLeaseResult,
    STORAGE_CONCURRENCY, SubmissionStore, TodoStore, WarehouseStore,
};
use crate::submission::{self, Submission, SubmissionInput, ValidationError};
use crate::types::{BenchmarkId, ClientId, JobId};
use crate::validated::{ContactEmail, NonEmptyTrimmedString, PublicKeyHex};
use crate::warehouse::{DeviceFormFactor, JobMetric};

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub catalog_cache: Arc<CatalogCache>,
    pub http_client: reqwest::Client,
    /// Signatures already spent, so each one authenticates a single request
    /// (see [`crate::auth::ReplayCache`]). Process-local: a multi-instance
    /// deployment protects each instance, not the fleet.
    pub replay_cache: Arc<ReplayCache>,
    /// Clients known to have migrated to `v1` signatures (see
    /// [`crate::auth::MigratedClients`]). Process-local and positives-only, so
    /// an instance that has not seen a client migrate consults the store rather
    /// than assuming it has not.
    pub migrated_clients: Arc<MigratedClients>,
    pub auth_store: Arc<dyn AuthStore>,
    pub submission_store: Arc<dyn SubmissionStore>,
    pub warehouse_store: Arc<dyn WarehouseStore>,
    pub eval_sample_result_store: Arc<dyn EvalSampleResultStore>,
    /// `todo/` job-queue tree (includes suspension methods). Backed by
    /// `config.todo_storage()` (see `planner.md` for the tree layout).
    pub todo_store: Arc<dyn TodoStore>,
}

// GET /
pub async fn index() -> impl IntoResponse {
    let html = include_str!("../static/index.html").replace("{version}", crate::BUILD_VERSION);
    (StatusCode::OK, [(header::CONTENT_TYPE, "text/html")], html)
}

/// `GET /health` response (httpapi.md §1). A fixed liveness marker.
#[derive(Serialize)]
struct HealthResponse {
    status: &'static str,
}

// GET /health
pub async fn health() -> impl IntoResponse {
    Json(HealthResponse { status: "ok" })
}

fn json_etag(value: &serde_json::Value) -> Result<String, AppError> {
    let body = serde_json::to_vec(value)
        .map_err(|e| AppError::Internal(format!("failed to serialize response for etag: {e}")))?;
    Ok(format!("\"{}\"", hex::encode(Sha256::digest(&body))))
}

fn if_none_match_matches(headers: &HeaderMap, etag: &str) -> bool {
    let Some(value) = headers.get(header::IF_NONE_MATCH) else {
        return false;
    };
    let Ok(value) = value.to_str() else {
        return false;
    };
    value.split(',').any(|candidate| {
        let candidate = candidate.trim();
        candidate == "*" || candidate == etag || candidate.strip_prefix("W/") == Some(etag)
    })
}

fn json_with_etag(headers: &HeaderMap, value: serde_json::Value) -> Result<Response, AppError> {
    let etag = json_etag(&value)?;
    let etag_header = HeaderValue::from_str(&etag)
        .map_err(|e| AppError::Internal(format!("failed to build etag header: {e}")))?;
    if if_none_match_matches(headers, &etag) {
        let mut response = StatusCode::NOT_MODIFIED.into_response();
        response.headers_mut().insert(header::ETAG, etag_header);
        return Ok(response);
    }
    let mut response = Json(value).into_response();
    response.headers_mut().insert(header::ETAG, etag_header);
    Ok(response)
}

/// The `device_*` profile fields shared by `POST /clients/register` and
/// `PATCH /clients/me`. The form factor arrives as a **string** and is parsed
/// in the handler: a typed enum field would fail inside axum's `Json` extractor
/// before we could map it to the documented `400`. Byte counts are `u64`,
/// matching [`DeviceProfile`].
///
/// **No per-field clear** (httpapi.md §2.4.1): a field present with a value is
/// set, an absent field leaves the stored value unchanged. `null` deserializes
/// to `None` here and is therefore treated as absent.
#[derive(Debug, Default, Deserialize)]
struct DeviceProfileInput {
    device_name: Option<NonEmptyTrimmedString>,
    /// Plain `String` (not `NonEmptyTrimmedString`) so an empty/whitespace value
    /// reaches the handler's `parse` below and yields the documented
    /// `400 must be one of …` rather than a generic deserialize rejection.
    device_form_factor: Option<String>,
    device_os_name: Option<NonEmptyTrimmedString>,
    device_os_version: Option<NonEmptyTrimmedString>,
    device_chip_model: Option<NonEmptyTrimmedString>,
    device_ram_bytes: Option<u64>,
    device_gpu_model: Option<NonEmptyTrimmedString>,
    device_gpu_vram_bytes: Option<u64>,
    device_npu_model: Option<NonEmptyTrimmedString>,
    device_npu_vram_bytes: Option<u64>,
}

/// 400 body for an unparseable form factor, verbatim from httpapi.md
/// §2.2.3 / §2.4.3.
const INVALID_FORM_FACTOR: &str = "Invalid device_form_factor (must be one of: \
     phone, tablet, laptop, desktop, server, embedded)";

impl DeviceProfileInput {
    /// Merge the present fields into `base`, parsing the wire form-factor string
    /// into the typed [`DeviceFormFactor`] (bad value → `400`). Absent fields
    /// leave the stored value unchanged.
    fn apply_to(self, base: &mut DeviceProfile) -> Result<(), AppError> {
        if let Some(raw) = self.device_form_factor {
            let parsed = raw
                .trim()
                .parse::<DeviceFormFactor>()
                .map_err(|_| AppError::BadRequest(INVALID_FORM_FACTOR.to_string()))?;
            base.device_form_factor = Some(parsed);
        }
        // Each remaining field overwrites the stored value only when present;
        // an absent (`None`) field leaves it untouched.
        macro_rules! merge {
            ($field:ident) => {
                if let Some(v) = self.$field {
                    base.$field = Some(v);
                }
            };
        }
        merge!(device_name);
        merge!(device_os_name);
        merge!(device_os_version);
        merge!(device_chip_model);
        merge!(device_ram_bytes);
        merge!(device_gpu_model);
        merge!(device_gpu_vram_bytes);
        merge!(device_npu_model);
        merge!(device_npu_vram_bytes);
        Ok(())
    }
}

/// Dependency checks on the **merged** profile (httpapi.md §2.2.3 / §2.4.3).
///
/// The GPU/NPU rules reuse the submission path's [`ValidationError`] variants so
/// both paths emit identical messages. The os-version rule is **client-profile
/// only** — on the submission path `device_os_name`/`device_os_version` are both
/// required, so the dependency can't fire there and there is no shared variant;
/// hence the inline message rather than a (submission-side) `ValidationError`.
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

/// Reject client-reported capability flags that are empty/whitespace-only, not
/// in canonical form, or use a server-owned reserved namespace (httpapi.md
/// §2.2.3 / §2.4.3).
///
/// The **canonical-form** check (a flag must equal its own `slugify`: lowercase,
/// no whitespace) is load-bearing, not cosmetic. It is the shape
/// `DeviceProfile::normalized_flags` and a plan's `requires` take, so a
/// non-canonical flag could never match anything anyway — but more importantly
/// it makes the reserved check exact: without it a client could smuggle a
/// reserved flag past `is_reserved_capability` with a non-canonical spelling
/// (`OS:ios`, `" os:ios"`) and thereby assert a device property it does not have.
/// Canonicalizing first closes that bypass. The `runtime:` namespace and any
/// other free-form (but canonical) flag are the client's to report.
fn validate_capabilities(
    capabilities: &std::collections::BTreeSet<String>,
) -> Result<(), AppError> {
    capabilities.iter().try_for_each(|cap| {
        if cap.trim().is_empty() {
            return Err(AppError::BadRequest(
                "capability flag must not be empty".into(),
            ));
        }
        if *cap != crate::client::slugify(cap) {
            return Err(AppError::BadRequest(format!(
                "capability flag '{cap}' must be lowercase with no whitespace"
            )));
        }
        if crate::client::is_reserved_capability(cap) {
            return Err(AppError::BadRequest(format!(
                "capability flag '{cap}' uses a reserved namespace (server-owned, derived from the device profile)"
            )));
        }
        Ok(())
    })
}

/// The `GET`/`PATCH /clients/me` response body (httpapi.md §2.3.1 / §2.4.2).
/// Device fields are always present, `null` when unset — distinct from the
/// stored record, whose `skip_serializing_if` omits unset `device_*` keys.
/// The `GET`/`PATCH /clients/me` response (httpapi.md §2.3). Unlike the other
/// handler responses this is a contract the `pipette-clients` crate
/// deserializes, so it's a typed struct — the single source of truth for the
/// shape — rather than an ad-hoc `json!`.
///
/// Built by *moving* fields out of the `Client` and its tag set, so it clones
/// nothing (a borrowed struct can't be returned through axum's `Json`, which
/// serializes after the handler returns). The `device_*` fields are `Option<…>`
/// **without** `skip_serializing_if`: they are always emitted (`null` when
/// unset), which is why this can't just serialize the `Client` record — that one
/// omits unset `device_*` keys. Field order mirrors the previous `json!` so the
/// wire bytes are unchanged.
#[derive(Serialize)]
struct ClientProfileResponse {
    client_id: ClientId,
    organization: NonEmptyTrimmedString,
    client_details: NonEmptyTrimmedString,
    contact_email: ContactEmail,
    status: ClientStatus,
    /// Read-only: tags are mgmt-assigned, read from the tag marker tree (not the
    /// record). Always present (empty array when untagged) so clients can render
    /// them without a null-check.
    tags: std::collections::BTreeSet<crate::validated::Tag>,
    /// `true` while the client's eligible-index re-evaluation is pending (set
    /// by a device-profile change or a fresh registration; cleared by
    /// `queue-maintenance`). This is the client's signal that its queue
    /// standing was voided: a profile change relinquishes every held lease,
    /// so on `true` the client must discard any locally persisted in-flight
    /// work (job ids, unsubmitted results) and poll `claim`.
    reindex_pending: bool,
    /// The capability flags the client reports directly. Always present (empty
    /// array when none); the server-derived `device_*` flags are *not* echoed
    /// here — they are visible through the `device_*` fields below.
    capabilities: std::collections::BTreeSet<String>,
    device_name: Option<NonEmptyTrimmedString>,
    device_form_factor: Option<crate::warehouse::DeviceFormFactor>,
    device_os_name: Option<NonEmptyTrimmedString>,
    device_os_version: Option<NonEmptyTrimmedString>,
    device_chip_model: Option<NonEmptyTrimmedString>,
    device_ram_bytes: Option<u64>,
    device_gpu_model: Option<NonEmptyTrimmedString>,
    device_gpu_vram_bytes: Option<u64>,
    device_npu_model: Option<NonEmptyTrimmedString>,
    device_npu_vram_bytes: Option<u64>,
}

fn client_profile_response(
    client: Client,
    tags: std::collections::BTreeSet<crate::validated::Tag>,
    reindex_pending: bool,
) -> ClientProfileResponse {
    // Destructure to move every field out (no clones). The exhaustive
    // `DeviceProfile` match means a new device field won't compile until it's
    // surfaced here too.
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
        device_chip_model,
        device_ram_bytes,
        device_gpu_model,
        device_gpu_vram_bytes,
        device_npu_model,
        device_npu_vram_bytes,
    } = device_profile;
    ClientProfileResponse {
        client_id,
        organization,
        client_details,
        contact_email,
        status,
        tags,
        reindex_pending,
        capabilities,
        device_name,
        device_form_factor,
        device_os_name,
        device_os_version,
        device_chip_model,
        device_ram_bytes,
        device_gpu_model,
        device_gpu_vram_bytes,
        device_npu_model,
        device_npu_vram_bytes,
    }
}

// POST /clients/register
#[derive(Deserialize)]
pub struct RegisterRequest {
    /// `PublicKeyHex` validates "valid hex, 32 bytes" at deserialize.
    pub public_key: Option<PublicKeyHex>,
    pub generate_key: Option<bool>,
    /// Required identity strings. `NonEmptyTrimmedString` rejects
    /// `""` / whitespace-only at the wire.
    pub organization: NonEmptyTrimmedString,
    pub client_details: NonEmptyTrimmedString,
    /// Email — also validated for the obvious shape (`@`, non-empty
    /// parts, `.` in domain). Not RFC 5322 conformance; just enough
    /// to catch `"contact_email": "Joe"` and friends.
    pub contact_email: ContactEmail,
    /// Optional pre-auth key (`preauth_{key_id}.{secret}`). When present and
    /// valid, the client is auto-approved and may be seeded with the key's
    /// default tags / organization; when invalid it is rejected and no client
    /// is created. See `docs/authentication.md` §6.
    #[serde(default)]
    pub preauth_key: Option<String>,
    /// Optional device profile supplied at registration (httpapi.md §2.2.1).
    #[serde(flatten)]
    device: DeviceProfileInput,
    /// Optional capability flags the client reports directly (e.g.
    /// `runtime:llama_cpp`). Reserved-namespace flags are rejected
    /// (`validate_capabilities`); absent → empty.
    #[serde(default)]
    capabilities: std::collections::BTreeSet<String>,
}

/// `POST /clients/register` receipt (httpapi.md §2.2.2). `private_key` is
/// present only when the server generated the keypair (`generate_key: true`),
/// so it's `skip_serializing_if` — absent, not `null`, when the client supplied
/// its own public key. `status` is the resolved approval state.
#[derive(Serialize)]
struct RegisterResponse {
    client_id: ClientId,
    status: ClientStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    private_key: Option<String>,
}

pub async fn register_client(
    State(state): State<AppState>,
    ApiJson(body): ApiJson<RegisterRequest>,
) -> Result<impl IntoResponse, AppError> {
    let (public_key, private_key_hex) = match (body.public_key, body.generate_key.unwrap_or(false))
    {
        (Some(pk), false) => (pk, None),
        (None, true) => {
            use ed25519_dalek::SigningKey;
            let mut csprng = rand_core::OsRng;
            let signing_key = SigningKey::generate(&mut csprng);
            let pk = PublicKeyHex::try_new(hex::encode(signing_key.verifying_key().as_bytes()))
                .map_err(|e| AppError::Internal(format!("generated invalid key: {e}")))?;
            let sk_hex = hex::encode(signing_key.to_bytes());
            (pk, Some(sk_hex))
        }
        _ => {
            return Err(AppError::BadRequest(
                "exactly one of public_key or generate_key must be provided".into(),
            ));
        }
    };

    let client_id =
        client::derive_client_id(&public_key).map_err(|e| AppError::Internal(e.to_string()))?;

    // Idempotent re-registration: a repeat with the same public key returns the
    // existing client rather than a 409, without consuming another pre-auth key —
    // so a client that registered but failed to persist locally (the
    // pipette-clients `RegistrationPersisted` state) can retry with the same keypair and recover
    // its client_id. Checked before key consumption. (client_id derives from the
    // public key, so this is the exact equivalent of the old has_public_key check.)
    if let Some(existing) = state
        .auth_store
        .get_client(&client_id)
        .await
        .map_err(|e| AppError::Internal(e.to_string()))?
    {
        tracing::info!(
            client_id = %existing.client_id,
            status = %existing.status,
            "idempotent re-registration; returning existing client without consuming a pre-auth key"
        );
        return Ok((
            StatusCode::OK,
            Json(RegisterResponse {
                client_id: existing.client_id,
                status: existing.status,
                private_key: None,
            }),
        ));
    }

    // Build and validate the (optional) device profile from the flat `device_*`
    // fields, mirroring `PATCH /clients/me` (httpapi.md §2.2.1). Done *before*
    // consuming any pre-auth key so a device-profile `400` never burns a use.
    let mut device_profile = DeviceProfile::default();
    body.device.apply_to(&mut device_profile)?;
    validate_device_profile(&device_profile)?;
    validate_capabilities(&body.capabilities)?;

    // Resolve approval status plus any key-seeded organization / tags. A valid
    // pre-auth key always approves and may seed org/tags; without one, fall back
    // to the email auto-approve rule — unless a key is required.
    let mut organization = body.organization;
    let mut seeded_tags: std::collections::BTreeSet<crate::validated::Tag> =
        std::collections::BTreeSet::new();
    let status = if let Some(token) = body.preauth_key.as_deref() {
        let (key_id, secret) = crate::preauth::parse_token(token)
            .ok_or_else(|| AppError::Unauthorized("malformed pre-auth key".into()))?;
        match state
            .auth_store
            .consume_preauth_key(&key_id, &secret)
            .await
            .map_err(|e| AppError::Internal(e.to_string()))?
        {
            crate::preauth::PreauthConsumeOutcome::Granted(grant) => {
                if let Some(org) = grant.default_organization {
                    organization = org;
                }
                seeded_tags = grant.default_tags;
                tracing::info!(
                    client_id = %client_id,
                    %key_id,
                    seeded_tags = seeded_tags.len(),
                    "pre-auth key consumed at registration"
                );
                // A valid pre-auth key always approves — that's its purpose.
                ClientStatus::Approved
            }
            crate::preauth::PreauthConsumeOutcome::Rejected(rejection) => {
                tracing::warn!(%key_id, reason = %rejection, "pre-auth key rejected at registration");
                return Err(AppError::Unauthorized(rejection.to_string()));
            }
        }
    } else if state.config.require_preauth_key {
        return Err(AppError::Forbidden(
            "registration requires a pre-auth key".into(),
        ));
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

    state
        .auth_store
        .put_client(&client)
        .await
        .map_err(|e| AppError::Internal(e.to_string()))?;

    // Apply any key-seeded tags concurrently. Best-effort: the client is
    // already persisted, so a marker write failing shouldn't fail registration
    // — it's recoverable by re-tagging or `reindex`. Each failure is logged so
    // it isn't silent; a failed write never aborts the others.
    futures::stream::iter(seeded_tags.iter().cloned().map(|tag| {
        let auth_store = state.auth_store.clone();
        let client_id = client_id.clone();
        async move {
            let result = auth_store.add_client_tag(&client_id, &tag).await;
            (client_id, tag, result)
        }
    }))
    .buffer_unordered(STORAGE_CONCURRENCY)
    .filter_map(|(client_id, tag, result)| async move { result.err().map(|e| (client_id, tag, e)) })
    .for_each(|(client_id, tag, e)| async move {
        tracing::warn!(
            client_id = %client_id,
            tag = %tag,
            error = %e,
            "failed to apply pre-auth-key seeded tag"
        );
    })
    .await;

    tracing::info!(
        client_id = %client_id,
        status = %status,
        has_device_profile = !client.device_profile.is_empty(),
        capabilities = client.capabilities.len(),
        seeded_tags = seeded_tags.len(),
        "client registered"
    );

    // Whether the key in this response stays confidential rests on the
    // deployment terminating TLS in front of the listen port (operations.md
    // §5.6), which this process cannot check. Clients that send their own
    // `public_key` never reach here, so an entry names the one path where a
    // credential crosses the wire.
    if private_key_hex.is_some() {
        tracing::warn!(
            client_id = %client_id,
            "returned a server-generated private key in the registration response"
        );
    }

    // Reindex the new client against existing `avail/` jobs only when it has
    // something to match on — an empty profile *and* no reported capabilities
    // yield an empty effective capability set, which no `requires` can be a
    // subset of. Best-effort: a dropped flag is repaired by the client's next
    // profile/capability refresh.
    if (!client.device_profile.is_empty() || !client.capabilities.is_empty())
        && let Err(e) = state.todo_store.write_pending_reindex(&client_id).await
    {
        tracing::warn!(
            client_id = %client_id,
            error = %e,
            "failed to queue newly registered client for eligible-index reindex"
        );
    }

    Ok((
        StatusCode::CREATED,
        Json(RegisterResponse {
            client_id,
            status,
            private_key: private_key_hex,
        }),
    ))
}

// GET /clients/me
pub async fn get_me(
    State(state): State<AppState>,
    AuthenticatedClient(client): AuthenticatedClient,
) -> Result<impl IntoResponse, AppError> {
    // Tags live in the marker tree, not on the record — read them separately.
    let tags = state
        .auth_store
        .get_client_tags(&client.client_id)
        .await
        .map_err(|e| AppError::Internal(e.to_string()))?;
    // Surfacing the reindex flag lets a client watch for its queue standing
    // to be restored instead of blind-polling `claim`.
    let reindex_pending = state
        .todo_store
        .has_pending_reindex(&client.client_id)
        .await
        .map_err(|e| AppError::Internal(e.to_string()))?;
    Ok(Json(client_profile_response(client, tags, reindex_pending)))
}

// PATCH /clients/me
#[derive(Deserialize)]
pub struct UpdateClientRequest {
    pub client_details: Option<NonEmptyTrimmedString>,
    /// Mutable `device_*` profile fields (httpapi.md §2.4.1).
    #[serde(flatten)]
    device: DeviceProfileInput,
    /// Replacement capability set (httpapi.md §2.4.1). Unlike the per-field
    /// `device_*` merge, this is set-granular: when present it **replaces** the
    /// stored set wholesale (the client reports its full current set); when
    /// absent (or `null`) the stored set is left unchanged.
    #[serde(default)]
    capabilities: Option<std::collections::BTreeSet<String>>,
}

pub async fn update_me(
    State(state): State<AppState>,
    AuthenticatedClient(mut client): AuthenticatedClient,
    ApiJson(body): ApiJson<UpdateClientRequest>,
) -> Result<impl IntoResponse, AppError> {
    if let Some(details) = body.client_details {
        client.client_details = details;
    }

    // Merge device fields into the loaded profile, then validate the *result*
    // (so a PATCH of only `device_gpu_vram_bytes` is checked against the stored
    // `device_gpu_model`). Both steps can 400 before anything is persisted.
    let profile_before = client.device_profile.clone();
    body.device.apply_to(&mut client.device_profile)?;
    validate_device_profile(&client.device_profile)?;
    let profile_changed = client.device_profile != profile_before;

    // A present `capabilities` replaces the stored set wholesale; absent leaves
    // it. Validated before persistence so a bad flag 400s alongside the profile
    // checks above.
    let capabilities_changed = if let Some(capabilities) = body.capabilities {
        validate_capabilities(&capabilities)?;
        let changed = capabilities != client.capabilities;
        client.capabilities = capabilities;
        changed
    } else {
        false
    };
    // Either the device profile or the reported capabilities feed the effective
    // capability set the matcher reads, so a change to *either* voids the
    // client's queue standing and must trigger the relinquish/reindex below.
    let matching_input_changed = profile_changed || capabilities_changed;

    // A change to the matching input (a `client_details`-only PATCH doesn't
    // count) voids the client's standing in the queue, and both repairs run
    // *before* the new record is
    // persisted. Persisted-first, a failure would leave the record durable
    // while the retry's diff comes up empty, so neither repair would ever
    // re-run; this way a failure fails the whole PATCH and the retry
    // re-detects the change, while a crash *after* the repairs at worst
    // relinquishes leases and triggers one reindex under the old profile —
    // harmless.
    //
    // - Reindex flag: queues the marker re-evaluation, and gates `claim`/
    //   `reclaim` until it happens — load-bearing, so its write is not
    //   best-effort. It goes first so the gate is closed for the whole
    //   relinquish window: with the gate still open, a concurrent `claim`
    //   from this client could read the stale old-profile eligible markers
    //   and re-acquire a just-relinquished job, leaving a live, renewable
    //   lease nothing would ever reconcile. A claim already past the gate
    //   when this flag lands creates its lease *after* the relinquish's
    //   listing, where no re-list can see it — that side is closed from the
    //   claim path itself (`revert_claim_if_pending_reindex`).
    // - Relinquish: a lease is granted against the profile at claim time; the
    //   client must not continue a job it may no longer be eligible for.
    if matching_input_changed {
        state
            .todo_store
            .write_pending_reindex(&client.client_id)
            .await
            .map_err(|e| AppError::Internal(format!("failed to queue client for reindex: {e}")))?;
        relinquish_client_leases(
            &*state.todo_store,
            &*state.submission_store,
            &client.client_id,
        )
        .await?;
    }

    state
        .auth_store
        .put_client(&client)
        .await
        .map_err(|e| AppError::Internal(e.to_string()))?;

    if matching_input_changed {
        // Second flag write, now that the record is durable. Flag writes
        // never overwrite (each mints a distinct key), and the reindex pass
        // deletes exactly the keys it captured before rebuilding — so a
        // rebuild that captured only the pre-persist flag above (and may
        // have read the old record) cannot consume this one. It guarantees
        // a flag postdating the durable record: some rebuild that reads
        // this record, or a newer one, will be the one to clear it.
        state
            .todo_store
            .write_pending_reindex(&client.client_id)
            .await
            .map_err(|e| AppError::Internal(format!("failed to queue client for reindex: {e}")))?;
        tracing::info!(
            client_id = %client.client_id,
            profile_changed,
            capabilities_changed,
            "client matching profile updated"
        );
    }

    let tags = state
        .auth_store
        .get_client_tags(&client.client_id)
        .await
        .map_err(|e| AppError::Internal(e.to_string()))?;
    // `reindex_pending` in the response is how the caller learns whether this
    // PATCH voided its queue standing — the client can't compute the profile
    // diff itself. Read from the store rather than echoing `profile_changed`:
    // an earlier change's flag may still be pending even when this PATCH
    // changed nothing.
    let reindex_pending = state
        .todo_store
        .has_pending_reindex(&client.client_id)
        .await
        .map_err(|e| AppError::Internal(e.to_string()))?;
    Ok(Json(client_profile_response(client, tags, reindex_pending)))
}

// GET /benchmarks
#[derive(Deserialize, Default)]
pub struct BenchmarkListQuery {
    #[serde(rename = "type")]
    pub type_filter: Option<String>,
}

pub async fn list_benchmarks(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<BenchmarkListQuery>,
) -> Result<Response, AppError> {
    let catalog = state
        .catalog_cache
        .get()
        .await
        .map_err(|e| AppError::Internal(format!("failed to load benchmark catalog: {e}")))?;
    let mut benchmarks: Vec<serde_json::Value> = catalog
        .values()
        .filter(|b| {
            if let Some(ref t) = query.type_filter
                && b.benchmark_type().as_ref() != t
            {
                return false;
            }
            true
        })
        .map(|b| {
            serde_json::to_value(b)
                .map_err(|e| AppError::Internal(format!("failed to serialize benchmark: {e}")))
        })
        .collect::<Result<Vec<_>, _>>()?;

    benchmarks.sort_by(|a, b| a["benchmark_id"].as_str().cmp(&b["benchmark_id"].as_str()));

    json_with_etag(&headers, serde_json::Value::Array(benchmarks))
}

// GET /benchmarks/{benchmark_id}
pub async fn get_benchmark(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(benchmark_id): Path<BenchmarkId>,
) -> Result<Response, AppError> {
    let catalog = state
        .catalog_cache
        .get()
        .await
        .map_err(|e| AppError::Internal(format!("failed to load benchmark catalog: {e}")))?;

    let benchmark = catalog
        .get(&benchmark_id)
        .ok_or_else(|| AppError::NotFound("benchmark not found".into()))?;

    let mut val = serde_json::to_value(benchmark)
        .map_err(|e| AppError::Internal(format!("failed to serialize benchmark: {e}")))?;

    // For eval benchmarks, proxy to evals server to get samples
    if let BenchmarkDef::Eval {
        parameter_eval_id,
        parameter_dataset_name,
        ..
    } = &benchmark.def
    {
        let samples = crate::scoring_service::fetch_samples(
            &state.http_client,
            &state.config.evals_server_url,
            parameter_eval_id,
            parameter_dataset_name,
        )
        .await
        .map_err(|e| AppError::BadGateway(e.to_string()))?;
        val["samples"] = serde_json::to_value(&samples.samples)
            .map_err(|e| AppError::Internal(format!("failed to serialize samples: {e}")))?;
    }

    json_with_etag(&headers, val)
}

/// Validate a single submission body and attach server-side fields.
/// Returns the typed [`Submission`] ready for storage. The strict
/// `SubmissionInput` enum is the single source of truth for "what
/// does a valid submission look like"; this function only adds the
/// pieces serde can't express:
///
/// 1. **Catalog lookup** — 404 cleanly on unknown `benchmark_id`
///    instead of a generic schema-mismatch 400.
/// 2. **Pre-deserialize raw-body check** — the `max_*_bytes`
///    legacy-alias collision is invisible after serde merges the
///    aliases, so it has to be detected on the un-deserialized
///    `serde_json::Value`.
/// 3. **Strict deserialize** — serde rejects missing required
///    fields, wrong types, unknown `message_type`.
/// 4. **Domain validation** ([`SuccessInput::validate`]) — form
///    factor enum parse, mill_params bounds, GPU/NPU dependency
///    rules, per-`benchmark_type` metric presence, completion-id
///    uniqueness on Eval.
/// 5. **Server-field attach** — wraps the wire input in the
///    storage [`Submission`] with `client_id`, `job_id`,
///    `submitted_at`, `benchmark_type`.
///
/// `client_id` is the authenticated caller's id — the same value
/// whether the submission lands in `incoming/` (approved client) or is
/// held in `unverified/{client_id}/` (pending client). The validation
/// rules are identical in both cases; only the write destination
/// differs (see [`write_submission_record`]).
/// Returns the prepared [`Submission`] and whether the client *supplied* the
/// `job_id` (a planner run echoing its claim) versus the server minting one
/// (ad-hoc). The flag gates the `todo/` job-completion teardown: only a
/// client-supplied id can correspond to a queued job, so ad-hoc submissions
/// skip the teardown's `avail/`/`leased/`/`denied/` scans entirely.
fn validate_and_prepare_submission(
    mut body: serde_json::Value,
    client_id: &ClientId,
    catalog: &HashMap<BenchmarkId, crate::benchmark::Benchmark>,
) -> Result<(Submission, bool), AppError> {
    // Default `message_type` so clients that predate the failure
    // variant keep working. The stored body is always
    // self-describing because the typed serialize at the end emits
    // the discriminator.
    if body.get("message_type").is_none() {
        body["message_type"] = json!("success");
    }

    // Peek `benchmark_id` to look up the catalog entry — 404 on
    // unknown benchmark is friendlier than a generic schema error.
    let benchmark_id = BenchmarkId::try_new(
        body.get("benchmark_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| AppError::BadRequest("missing benchmark_id".into()))?,
    )
    .map_err(|e| AppError::BadRequest(format!("invalid benchmark_id: {e}")))?;
    let benchmark = catalog
        .get(&benchmark_id)
        .ok_or_else(|| AppError::NotFound("benchmark not found".into()))?;

    // Resolve the job id before the typed deserialize. A plan-attached run
    // echoes the `job_id` it claimed; ad-hoc success runs omit it (or send
    // `null`) and the server mints a fresh UUIDv7. Peeked from the raw body —
    // like `benchmark_id` above — because `job_id` is a server-controlled
    // identity field that the storage `Submission` carries top-level, not a
    // field on the `*Input` structs. A present-but-non-UUID value is a `400`
    // (httpapi.md §2.7.3); a client-echoed id is accepted in any UUID version.
    //
    // Failures are the exception: only plan-attached runs report failures, so
    // `job_id` is *required* on a `message_type: "failure"` body — an absent id
    // is a `400`, never a server mint (httpapi.md §2.7.2). `message_type` was
    // defaulted to `"success"` above, so this branch is exact.
    let is_failure = body.get("message_type").and_then(|v| v.as_str()) == Some("failure");
    let client_supplied_job_id = matches!(body.get("job_id"), Some(v) if !v.is_null());
    let job_id = match body.get("job_id") {
        None | Some(serde_json::Value::Null) if is_failure => {
            return Err(AppError::BadRequest(
                "failure submission requires job_id".into(),
            ));
        }
        None | Some(serde_json::Value::Null) => JobId::from_uuid(Uuid::now_v7()),
        Some(v) => {
            let s = v
                .as_str()
                .ok_or_else(|| AppError::BadRequest("job_id must be a string".into()))?;
            // `JobId::try_new` is the single source of truth for job-id format
            // (safe charset). The claim it belongs to is enforced separately by
            // `verify_claim` (the lease check), so the id need not be a UUID here
            // — only well-formed. Preserves the client's exact echoed string.
            JobId::try_new(s)?
        }
    };

    // Raw-body check: aliases silently merge in serde, so this
    // rule lives outside the typed deserialize path. Gated on
    // benchmark type because the fields are only meaningful for
    // MaxMemoryUsage.
    if matches!(&benchmark.def, BenchmarkDef::MaxMemoryUsage { .. }) {
        submission::reject_max_alias_collisions(&body)
            .map_err(|e| AppError::BadRequest(e.to_string()))?;
    }

    let input: SubmissionInput = serde_json::from_value(body)
        .map_err(|e| AppError::BadRequest(format!("invalid submission: {e}")))?;

    input
        .validate(benchmark)
        .map_err(|e| AppError::BadRequest(e.to_string()))?;

    // `TrimmedString` fields on `SubmissionInput` have already
    // stripped whitespace during deserialize — nothing further to do
    // at the ingest boundary.
    Ok((
        input.into_submission(
            client_id.clone(),
            job_id,
            Utc::now(),
            benchmark.benchmark_type(),
        ),
        client_supplied_job_id,
    ))
}

/// `202 Accepted` receipt for a single submission (`POST /benchmarks`) — echoes
/// the job id the result was recorded under, whether client-supplied or
/// server-minted (httpapi.md §2.7).
#[derive(Serialize)]
struct JobAcceptedResponse {
    job_id: JobId,
}

// POST /benchmarks (submit results)
pub async fn submit_benchmark(
    State(state): State<AppState>,
    AuthenticatedClient(client): AuthenticatedClient,
    ApiJson(body): ApiJson<serde_json::Value>,
) -> Result<impl IntoResponse, AppError> {
    // A pending (unapproved) client is held rather than rejected when
    // the unverified archive is enabled; otherwise it is `403` as
    // before. Approved clients flow straight through.
    let held = resolve_submission_disposition(&client, &state.config)?;

    let catalog = state
        .catalog_cache
        .get()
        .await
        .map_err(|e| AppError::Internal(format!("failed to load benchmark catalog: {e}")))?;

    let (submission, client_supplied_job_id) =
        validate_and_prepare_submission(body, &client.client_id, &catalog)?;

    let job_id = submission_job_id(&submission).clone();

    // Bind a client-supplied `job_id` to this client's live claim before
    // writing to the client-unpartitioned `incoming/`/`processed/` key — an
    // unbound or foreign id would otherwise clobber another client's record.
    // The bind renews the lease (see `verify_claim`), so the claim stays live
    // for the duration of the result write. Ad-hoc (server-minted) and held
    // (client-partitioned) submissions skip this: there is no claim to bind and
    // no shared key to hijack.
    if !held && client_supplied_job_id {
        let new_expiry = state.config.lease_expiry_from(Utc::now());
        verify_claim(&*state.todo_store, &job_id, &client.client_id, new_expiry).await?;
    }

    // A retriable failure diverges before the result write: the run failed on
    // this device only, so no job result is recorded — the job is denied for
    // this client and returned to `avail/` for others (see `planner.md`,
    // "Consequences of Failure"). Failures always carry a client-supplied
    // `job_id`, so this only fires on the verified, approved path.
    if let Some(f) = retriable_failure(&submission, held, client_supplied_job_id) {
        handle_retriable_failure(&*state.todo_store, f, &client.client_id, &job_id).await?;
        log_accepted_submission(&submission);
        return Ok((StatusCode::ACCEPTED, Json(JobAcceptedResponse { job_id })));
    }

    let body_to_write = serde_json::to_value(&submission)
        .map_err(|e| AppError::Internal(format!("failed to serialize submission: {e}")))?;

    write_submission_record(&*state.submission_store, &submission, &body_to_write, held).await?;
    log_accepted_submission(&submission);

    // A terminal planner-job completion — a success or a non-retriable failure
    // (retriable failures returned above) — tears down the job's `todo/` state
    // now that its result is persisted. Gated to approved submissions (`!held`)
    // that echo a claimed `job_id`; ad-hoc and held submissions never correspond
    // to a queued job.
    if !held && client_supplied_job_id {
        teardown_completed_job(&*state.todo_store, &job_id, &client.client_id).await;
    }

    Ok((StatusCode::ACCEPTED, Json(JobAcceptedResponse { job_id })))
}

/// Decide whether an authenticated submission flows through the normal
/// pipeline or is held in the unverified archive, given the client's
/// approval status and the server config. Returns `true` when the
/// submission should be **held** (pending client, feature enabled).
///
/// - Approved client → `Ok(false)` (normal `incoming/`/`processed/`).
/// - Pending client, `[unverified_submissions] enabled = true` →
///   `Ok(true)` (held under `unverified/{client_id}/`).
/// - Pending client, feature disabled → `Err(403)`, the historical
///   behavior.
fn resolve_submission_disposition(client: &Client, config: &Config) -> Result<bool, AppError> {
    if client.status == ClientStatus::Approved {
        return Ok(false);
    }
    if config.unverified_submissions.enabled {
        tracing::info!(
            client_id = %client.client_id,
            "holding submission from unapproved client in unverified archive"
        );
        Ok(true)
    } else {
        tracing::warn!(client_id = %client.client_id, "submission rejected: client not approved");
        Err(AppError::Forbidden("client is not approved".into()))
    }
}

async fn write_submission_record(
    store: &dyn SubmissionStore,
    submission: &Submission,
    body: &serde_json::Value,
    held: bool,
) -> Result<(), AppError> {
    submission::write_submission_record(store, submission, body, held)
        .await
        .map_err(|e| AppError::Internal(format!("{e:#}")))
}

/// Authorize a client-supplied `job_id` against this client's live claim
/// before its result is written to the (client-unpartitioned)
/// `incoming/`/`processed/` key. Only the lease holder may submit a result —
/// an unbound or foreign id would otherwise clobber another client's record.
///
/// Authorization is an atomic lease *renewal* to `new_expiry`, not a read: the
/// same `renew_lease` rename that confirms the caller holds the lease also
/// pushes its expiry forward. This closes the write-in-flight race — a
/// submission that passes now holds a lease that expires a full lease-duration
/// out, so `queue-maintenance` cannot recycle it (and then expire the job)
/// while the result write is still landing. The renewal predicate is lease
/// *existence*, not expiry, so a client that ran through a brief outage and
/// submits against an expired-but-not-yet-recycled lease is still authorized
/// (its lease is renewed) — completed work isn't discarded.
///
/// - Client has a pending reindex → `404` without renewing. The flag voids
///   the client's standing outright: the profile change that set it
///   relinquished every lease the client held, so any claim it submits
///   against is one it already gave up — and the renewal below would
///   otherwise resurrect that lease out from under the relinquish.
/// - Lease held by this client → renewed → `Ok` (proceed).
/// - Leased by a *different* client → `409 Conflict` (the caller was
///   superseded; it should abort).
/// - No lease anywhere → `404` (no live claim: the job was recycled to
///   `avail/`, completed, expired-and-swept, or never claimed). The client
///   should `POST /plans/{job_id}/reclaim` and, if that succeeds, re-submit.
///   See `planner.md`.
///
/// A store failure propagates as `500` rather than being read as "no lease" —
/// silently treating an unreadable store as unclaimed could both reject valid
/// work and (worse) green-light a clobber.
async fn verify_claim(
    todo: &dyn TodoStore,
    job_id: &JobId,
    client_id: &ClientId,
    new_expiry: DateTime<Utc>,
) -> Result<(), AppError> {
    if todo
        .has_pending_reindex(client_id)
        .await
        .map_err(|e| AppError::Internal(format!("failed to verify claim: {e}")))?
    {
        tracing::warn!(
            %job_id,
            %client_id,
            reason = "pending_reindex",
            "refused submission during pending reindex — the client's profile change relinquished the claim; the result is forfeited"
        );
        return Err(AppError::NotFound(format!(
            "no active claim for job {job_id}; client re-evaluation pending"
        )));
    }
    match todo
        .renew_lease(job_id, client_id, new_expiry)
        .await
        .map_err(|e| AppError::Internal(format!("failed to verify claim: {e}")))?
    {
        RenewLeaseResult::Renewed => Ok(()),
        RenewLeaseResult::WrongClient => {
            tracing::warn!(%job_id, %client_id, reason = "leased_elsewhere", "claim verification rejected submission");
            Err(AppError::Conflict(format!(
                "job {job_id} is leased to another client"
            )))
        }
        RenewLeaseResult::NotFound => {
            tracing::warn!(%job_id, %client_id, reason = "no_active_claim", "claim verification rejected submission");
            Err(AppError::NotFound(format!(
                "no active claim for job {job_id}; reclaim before submitting"
            )))
        }
    }
}

/// Tear down a completed job's claimable `todo/` state after its result has
/// been persisted, so the job is never handed out again. Best-effort: every
/// failure is logged and swallowed so a `todo/`-store hiccup never fails the
/// already-committed submission, and `queue-maintenance` reconciles anything
/// left behind.
///
/// Deletes, concurrently:
/// - this client's lease, located via the targeted `leased/{client_id}/`
///   prefix scan. Leases are partitioned by client, so that partition is
///   authoritative for the client's own lease: a miss means it holds none
///   (recycled, or never leased) and there is nothing to delete — we never
///   touch another client's lease. Deleting it also stops `queue-maintenance`
///   from recycling an expired lease back into `avail/`.
/// - any `avail/` entry for the job — a defensive backstop. This teardown is
///   reached only after `verify_claim` renewed the caller's lease, so the job
///   is leased (not in `avail/`) and the delete is normally a no-op; it remains
///   so that no stray `avail/` entry can outlive a torn-down job.
///
/// It deliberately does **not** touch `denied/` or `eligible/` markers: those
/// only affect claim *eligibility*, not re-handout, and are reconciled by the
/// `queue-maintenance` GC sweep (the sole writer of `eligible/`).
///
/// This is the **terminal** teardown — for a successful run or an inherent
/// (non-retriable) failure. A client-specific (retriable) failure keeps the
/// job in `avail/` for other clients and must *not* call this; see
/// `planner.md` ("Consequences of Failure").
async fn teardown_completed_job(todo: &dyn TodoStore, job_id: &JobId, client_id: &ClientId) {
    let drop_lease = async {
        match own_lease_expiry(todo, client_id, job_id).await {
            Ok(Some(expiry)) => {
                if let Err(e) = todo.delete_lease(job_id, client_id, expiry).await {
                    tracing::warn!(%job_id, %client_id, error = %e, "todo teardown: delete_lease failed");
                }
            }
            Ok(None) => {}
            Err(e) => {
                tracing::warn!(%job_id, %client_id, error = %e, "todo teardown: list_leased_for_client failed");
            }
        }
    };
    let drop_avail = async {
        if let Err(e) = todo.delete_avail_by_job(job_id).await {
            tracing::warn!(%job_id, error = %e, "todo teardown: delete_avail_by_job failed");
        }
    };
    tokio::join!(drop_lease, drop_avail);
}

/// Classify a verified submission as a retriable failure that must divert from
/// the normal result write: only on the approved (`!held`), claim-bound
/// (`client_supplied_job_id`) path, and only for a `Failure` whose `retriable`
/// flag is set. Returns the inner failure for the caller to hand to
/// `handle_retriable_failure`, or `None` for every other disposition (held,
/// ad-hoc, success, or a terminal non-retriable failure).
fn retriable_failure(
    submission: &Submission,
    held: bool,
    client_supplied_job_id: bool,
) -> Option<&crate::submission::FailureSubmission> {
    if held || !client_supplied_job_id {
        return None;
    }
    match submission {
        Submission::Failure(f) if f.wire.retriable => Some(f.as_ref()),
        _ => None,
    }
}

/// Find this client's own lease expiry for `job_id` by scanning its
/// `leased/{client_id}/` partition — authoritative for the client's own lease,
/// so a miss means it holds none (recycled, or never leased). A failure to list
/// propagates rather than reading as "no lease"; callers decide whether that is
/// fatal (`500`) or best-effort.
async fn own_lease_expiry(
    todo: &dyn TodoStore,
    client_id: &ClientId,
    job_id: &JobId,
) -> anyhow::Result<Option<chrono::DateTime<Utc>>> {
    Ok(todo
        .list_leased_for_client(client_id)
        .await?
        .iter()
        .find_map(|k| {
            let (kj, _kc, expiry) = crate::todo_filename::parse_leased_key(k).ok()?;
            (kj == *job_id).then_some(expiry)
        }))
}

/// Relinquish every lease this client holds, because its device profile is
/// changing: a lease is granted against the profile at claim time, so the
/// change voids the grant. Each job without a submission record returns to
/// `avail/` for re-claiming — deliberately without a `denied/` marker, since
/// the client may still match under its new profile and is then free to claim
/// the job fresh; a stale lease whose job already has a record is deleted
/// instead (see [`queue_maintenance::resolve_or_recycle_lease`]).
///
/// Failure propagates as `500` and must abort the profile update: a lease that
/// survives under the new profile would let the client keep running a job it
/// may no longer be eligible for.
///
/// Every renewal path (`heartbeat`, `reclaim`, `verify_claim`) refuses while
/// the client's pending-reindex flag is up, and the flag is written before
/// this runs — so no *new* renewal can rename a lease during the relinquish.
/// A renewal already past its flag check when the flag landed still can:
/// its rename makes the resolution here report `Gone` while the lease lives
/// on under a new expiry key. The outer loop closes that window by
/// re-listing until the partition is clean, bounded — stragglers are only
/// requests that were in flight at flag time, so a second pass is normally
/// the last. A lease that persists past the bound means renewals keep
/// racing (a protocol violation on the client's part) and fails the PATCH.
async fn relinquish_client_leases(
    todo: &dyn TodoStore,
    submissions: &dyn SubmissionStore,
    client_id: &ClientId,
) -> Result<(), AppError> {
    // This client's current leases, parsed. Unparseable keys are foreign
    // cruft, not system-created leases (see the rationale in
    // `queue_maintenance::recycle_expired_leases`) — there is no lease to
    // relinquish, and they must not count against the clean-partition check.
    async fn parsed_leases(
        todo: &dyn TodoStore,
        client_id: &ClientId,
    ) -> Result<Vec<(JobId, ClientId, chrono::DateTime<Utc>)>, AppError> {
        Ok(todo
            .list_leased_for_client(client_id)
            .await
            .map_err(|e| AppError::Internal(format!("failed to list leases for relinquish: {e}")))?
            .into_iter()
            .filter_map(|key| match crate::todo_filename::parse_leased_key(&key) {
                Ok(parsed) => Some(parsed),
                Err(_) => {
                    tracing::warn!(key = %key, "skipping unparseable leased key during relinquish");
                    None
                }
            })
            .collect())
    }

    const RELINQUISH_PASSES: usize = 2;
    // Each iteration starts with a re-list, so the last (`pass ==
    // RELINQUISH_PASSES`) is verification only: the resolve rounds are
    // complete exactly when a re-list comes back clean.
    for pass in 0..=RELINQUISH_PASSES {
        let leases = parsed_leases(todo, client_id).await?;
        if leases.is_empty() {
            return Ok(());
        }
        if pass == RELINQUISH_PASSES {
            break;
        }
        if pass > 0 {
            tracing::warn!(
                client_id = %client_id,
                remaining = leases.len(),
                "protocol violation: lease renewed mid-relinquish — a broken client is renewing while its own profile update relinquishes; re-resolving"
            );
        }
        for (job_id, key_client, lease_expiry) in leases {
            match crate::queue_maintenance::resolve_or_recycle_lease(
                todo,
                submissions,
                &job_id,
                &key_client,
                lease_expiry,
            )
            .await
            .map_err(|e| AppError::Internal(format!("failed to relinquish lease: {e}")))?
            {
                crate::queue_maintenance::LeaseResolution::Recycled => {
                    if !delete_recycled_entry_if_recorded(todo, submissions, client_id, &job_id)
                        .await?
                    {
                        tracing::info!(
                            client_id = %client_id,
                            job_id = %job_id,
                            "profile change: relinquished lease; job returned to avail/"
                        );
                    }
                }
                crate::queue_maintenance::LeaseResolution::ResolvedStale => {
                    tracing::info!(
                        client_id = %client_id,
                        job_id = %job_id,
                        "profile change: job already has a submission record; deleted stale lease"
                    );
                }
                crate::queue_maintenance::LeaseResolution::Gone => {
                    // Either another actor resolved it (fine) or an in-flight
                    // renewal renamed it — the next re-list decides.
                    tracing::debug!(client_id = %client_id, job_id = %job_id, "lease already resolved elsewhere during relinquish");
                }
            }
        }
    }
    Err(AppError::Internal(format!(
        "client {client_id} still holds leases after {RELINQUISH_PASSES} relinquish passes; \
         failing profile update"
    )))
}

/// Post-recycle record recheck for one relinquished lease, returning `true`
/// when the recycled `avail/` entry was removed because the job already has a
/// record.
///
/// `resolve_or_recycle_lease` decides to recycle from a record check that
/// runs *before* the rename, so a result write racing the relinquish — one
/// that passed `verify_claim` before the pending-reindex flag landed — can
/// land its record while the recycle is in flight. Left alone, that strands a
/// claimable `avail/` entry for a job that already has a record: a state only
/// this path creates, that no maintenance pass repairs (the expiry pass's
/// leftover cleanup runs only for expired or all-denied jobs), and whose only
/// other cleaner is the submitter's best-effort teardown. Deleting the entry
/// here finishes that teardown deterministically, so the relinquish never
/// completes having left the job both recorded and claimable. Logged as a
/// protocol anomaly — the client PATCHed its profile with a submission in
/// flight (or shares its keypair across devices).
async fn delete_recycled_entry_if_recorded(
    todo: &dyn TodoStore,
    submissions: &dyn SubmissionStore,
    client_id: &ClientId,
    job_id: &JobId,
) -> Result<bool, AppError> {
    if submissions
        .find_job(job_id)
        .await
        .map_err(|e| {
            AppError::Internal(format!("failed to re-check job record after recycle: {e}"))
        })?
        .is_none()
    {
        return Ok(false);
    }
    todo.delete_avail_by_job(job_id)
        .await
        .map_err(|e| AppError::Internal(format!("failed to remove recycled avail entry: {e}")))?;
    tracing::warn!(
        client_id = %client_id,
        job_id = %job_id,
        "relinquish raced an in-flight submission: record landed during recycle; removed the recycled avail/ entry"
    );
    Ok(true)
}

/// Handle a verified, approved (`!held`) **retriable** failure: the run failed
/// on *this* device only, so no job result is recorded. The job is denied for
/// the reporting client and returned to `avail/` for other eligible clients.
/// If every eligible client of a `clients`-only job has now denied it, the job
/// can never succeed — the `queue-maintenance` all-denied reconciliation pass
/// owns that rule and escalates the job to a terminal system failure within
/// one cron interval (nothing can claim it in the meantime: every listed
/// client has a `denied/` marker, and `claim` skips denied candidates). See
/// `planner.md` ("Consequences of Failure").
///
/// The denial and recycle *are* the handling, so their failures propagate as
/// `500` — both are idempotent, so the client's retry is safe.
async fn handle_retriable_failure(
    todo: &dyn TodoStore,
    failure: &crate::submission::FailureSubmission,
    client_id: &ClientId,
    job_id: &JobId,
) -> Result<(), AppError> {
    // Record the denial and locate this client's lease concurrently — both are
    // independent and both must complete before the recycle below (which returns
    // the job to `avail/`), so the denial is durable the instant the job can be
    // re-claimed (claim skips denied candidates).
    //
    // Source the lease expiry from this client's partition (`verify_claim`
    // confirmed the lease but discarded its expiry). A miss means the lease was
    // already recycled between verify and here (e.g. `queue-maintenance` beat
    // us); the denial stands and there is nothing to do. Recycle (not
    // `delete_lease`) is the correct primitive — the job body lives only under
    // `leased/` while claimed, and `delete_lease` would destroy it.
    let (denied, leased) = tokio::join!(
        todo.write_denied(job_id, client_id),
        own_lease_expiry(todo, client_id, job_id),
    );
    denied.map_err(|e| AppError::Internal(format!("failed to write denied marker: {e}")))?;
    let Some(lease_expiry) =
        leased.map_err(|e| AppError::Internal(format!("failed to list client leases: {e}")))?
    else {
        return Ok(());
    };
    // `Gone`: the lease vanished between the scan above and the rename. That
    // has exactly one kind of cause: the client raced itself. `verify_claim`
    // just renewed the lease, so `queue-maintenance` cannot have recycled it
    // (it touches only *expired* leases) — the lease can only have moved by
    // the client's own hand: a concurrent heartbeat renaming the lease file,
    // a duplicate of this submission resolving it, or a concurrent profile
    // change relinquishing it. A well-behaved client does none of these while
    // a submission is in flight; the warning below is that sloppiness
    // surfacing in the logs. The server is resilient to it regardless: the
    // denial is already durably recorded, and whatever state the race left
    // the lease in, `queue-maintenance` reconciles it — recycling it once it
    // expires and running the all-denied escalation.
    if let RecycleResult::Gone = todo
        .recycle_lease(job_id, client_id, lease_expiry)
        .await
        .map_err(|e| AppError::Internal(format!("failed to recycle lease: {e}")))?
    {
        tracing::warn!(%job_id, %client_id, "lease vanished mid-recycle: client raced its own failure submission; left to queue-maintenance reconciliation");
    }

    // Read the recycled body to sanity-check the client's report. Absent → the
    // job was completed, expired, or already claimed by another eligible
    // client; nothing to check against.
    let Some(job_body) = todo
        .get_avail_by_job(job_id)
        .await
        .map_err(|e| AppError::Internal(format!("failed to read job body: {e}")))?
    else {
        return Ok(());
    };

    // Sanity-check that the client reported against the job it actually holds.
    // `verify_claim` already authorized the caller, so a mismatch can't corrupt
    // queue state — it is only a signal worth logging.
    if let Some((reported, expected)) = benchmark_mismatch(failure, &job_body) {
        tracing::warn!(
            %job_id, %client_id, reported,
            expected = expected.unwrap_or("<absent>"),
            "retriable failure report names a different benchmark than the claimed job"
        );
    }
    Ok(())
}

/// The benchmark a retriable failure names, paired with the one the job's spec
/// declares, when the two disagree — `None` when they match.
///
/// Only `benchmark_id` is compared: a failure report carries nothing else
/// describing the work, since clients stopped echoing the `model_*` / `runtime_*`
/// labels once the claim began carrying a typed `spec`, and the server already
/// holds the job body this `job_id` names. Comparing a field the wire no longer
/// sends would report a mismatch on every failure.
///
/// The caller is already authorized (`verify_claim`), so a disagreement cannot
/// corrupt queue state; it is a correctness/abuse signal to log. A job body with
/// no readable `spec.benchmark` yields `expected: None` and counts as a
/// disagreement rather than a match — hence the pair rather than a bare bool, so
/// the caller can log which value was missing.
fn benchmark_mismatch<'a>(
    failure: &'a crate::submission::FailureSubmission,
    job_body: &'a serde_json::Value,
) -> Option<(&'a str, Option<&'a str>)> {
    let reported = failure.wire.benchmark_id.as_str();
    let expected = job_body
        .get("spec")
        .and_then(|spec| spec.get("benchmark"))
        .and_then(|v| v.as_str());
    (expected != Some(reported)).then_some((reported, expected))
}

fn submission_job_id(s: &Submission) -> &JobId {
    match s {
        Submission::Success(s) => &s.job_id,
        Submission::Failure(f) => &f.job_id,
    }
}

fn submission_benchmark_id(s: &Submission) -> &BenchmarkId {
    match s {
        Submission::Success(s) => &s.wire.benchmark_id,
        Submission::Failure(f) => &f.wire.benchmark_id,
    }
}

/// Record an accepted submission.
///
/// Called once the submission's outcome is durable — the result written, or a
/// retriable failure's denial recorded — and so past every check that could
/// still turn it away. The claim check in particular rejects a `job_id` the
/// client does not hold, which at parse time has only been checked for shape, so
/// an event emitted earlier would announce the acceptance of requests that go on
/// to be refused.
///
/// Client-supplied free text is recorded with `Debug` rather than `Display`, so
/// the escaping keeps one event on one line. `tracing`'s formatter is
/// line-oriented, and a client chooses these strings: a raw newline reaching the
/// output would be read back as a separate record, with a timestamp, level, and
/// target of the client's choosing. Identifier fields (`job_id`, `client_id`,
/// `benchmark_id`) carry their own charset validation and need no escaping.
fn log_accepted_submission(submission: &Submission) {
    match submission {
        Submission::Success(s) => {
            tracing::info!(
                job_id = %s.job_id,
                benchmark_id = %s.wire.benchmark_id,
                client_id = %s.client_id,
                benchmark_type = %s.benchmark_type,
                model_name = ?s.wire.model_name,
                model_quant = ?s.wire.model_quant,
                runtime_name = ?s.wire.runtime_name,
                "accepted submission"
            );
        }
        Submission::Failure(f) => {
            tracing::info!(
                job_id = %f.job_id,
                benchmark_id = %f.wire.benchmark_id,
                client_id = %f.client_id,
                benchmark_type = %f.benchmark_type,
                model_name = ?f.wire.model_name,
                model_quant = ?f.wire.model_quant,
                runtime_name = ?f.wire.runtime_name,
                failure_reason = ?f.wire.failure_reason.as_str(),
                "accepted failure submission"
            );
        }
    }
}

const BATCH_MAX_SUBMISSIONS: usize = 1000;

/// Max in-flight `write_incoming` calls per batch request. Each item has a
/// unique job_id so there's no contention on keys; the cap just bounds
/// fan-out to the submission store.
const BATCH_CONCURRENCY: usize = 16;

/// One element of the `POST /benchmarks/batch` `results` array: either
/// `{index, job_id}` (accepted) or `{index, error}` (rejected). Modeled as an
/// untagged enum so "exactly one of job_id/error" is a type invariant; each
/// variant serializes to the shape of the matching prior `json!` form
/// (httpapi.md §2.8).
#[derive(Serialize)]
#[serde(untagged)]
enum BatchItemResult {
    Accepted { index: usize, job_id: JobId },
    Rejected { index: usize, error: String },
}

impl BatchItemResult {
    fn accepted(index: usize, job_id: JobId) -> Self {
        Self::Accepted { index, job_id }
    }

    fn error(index: usize, error: impl Into<String>) -> Self {
        Self::Rejected {
            index,
            error: error.into(),
        }
    }
}

/// `POST /benchmarks/batch` response envelope: a per-item `results` array (see
/// [`BatchItemResult`]).
#[derive(Serialize)]
struct BatchResponse {
    results: Vec<BatchItemResult>,
}

/// Reject one batch item. A rejected item is reported inside a `200` body, so
/// it never passes through `AppError`'s `IntoResponse` — this is where the
/// error gets logged in full. The per-item `error` string carries only the
/// caller-facing message (see [`AppError::message`]).
fn reject_batch_item(index: usize, e: &AppError) -> BatchItemResult {
    // A `200` body hides whose fault the item was, so the level carries it.
    if e.is_service_fault() {
        tracing::error!(batch_index = index, error = %e, "failed to process batch submission");
    } else {
        tracing::warn!(batch_index = index, error = %e, "rejected batch submission");
    }
    BatchItemResult::error(index, e.message())
}

/// Validate, prepare, and write a single item in a batch. Returns a
/// per-index [`BatchItemResult`] — either `{index, job_id}` on success or
/// `{index, error}` on failure. Errors are intentionally swallowed
/// here so one bad item doesn't fail the whole batch; see the docstring
/// on `submit_benchmark_batch`.
async fn process_batch_item(
    state: &AppState,
    client_id: &ClientId,
    catalog: &HashMap<BenchmarkId, crate::benchmark::Benchmark>,
    held: bool,
    index: usize,
    item: serde_json::Value,
) -> BatchItemResult {
    let (submission, client_supplied_job_id) =
        match validate_and_prepare_submission(item, client_id, catalog) {
            Ok(s) => s,
            Err(e) => return reject_batch_item(index, &e),
        };

    let job_id = submission_job_id(&submission).clone();
    let benchmark_id = submission_benchmark_id(&submission).clone();

    // Same claim-binding gate as the single-submit path (renews the lease); a
    // verification failure becomes this item's per-index error (404/409 message).
    if !held && client_supplied_job_id {
        let new_expiry = state.config.lease_expiry_from(Utc::now());
        if let Err(e) = verify_claim(&*state.todo_store, &job_id, client_id, new_expiry).await {
            return reject_batch_item(index, &e);
        }
    }

    // Same retriable-failure divergence as the single-submit path: no result is
    // recorded; the job is denied and recycled. A handling failure becomes
    // this item's per-index error.
    if let Some(f) = retriable_failure(&submission, held, client_supplied_job_id) {
        return match handle_retriable_failure(&*state.todo_store, f, client_id, &job_id).await {
            Ok(()) => BatchItemResult::accepted(index, job_id),
            Err(e) => reject_batch_item(index, &e),
        };
    }

    let body_to_write = match serde_json::to_value(&submission) {
        Ok(v) => v,
        Err(e) => {
            let e = AppError::Internal(format!("failed to serialize submission: {e}"));
            return reject_batch_item(index, &e);
        }
    };

    match write_submission_record(&*state.submission_store, &submission, &body_to_write, held).await
    {
        Ok(()) => {
            tracing::info!(
                job_id = %job_id,
                benchmark_id = %benchmark_id,
                client_id = %client_id,
                batch_index = index,
                "accepted batch submission"
            );
            // Same terminal teardown as the single-submit path (success or
            // non-retriable failure; retriable returned above).
            if !held && client_supplied_job_id {
                teardown_completed_job(&*state.todo_store, &job_id, client_id).await;
            }
            BatchItemResult::accepted(index, job_id)
        }
        Err(e) => reject_batch_item(index, &e),
    }
}

/// POST /benchmarks/batch — submit multiple results in a single request.
///
/// Per-item failures are swallowed: this endpoint returns `200 OK` even
/// when some submissions fail validation or fail to write. Each element
/// of the returned `results` array reports either a `job_id` (success)
/// or an `error` string for that index. Callers must inspect every item
/// to know what actually succeeded. The whole request only fails (4xx)
/// when the request envelope itself is bad: unapproved client, missing
/// `submissions` array, empty array, or more than `BATCH_MAX_SUBMISSIONS`
/// items. See `docs/httpapi.md` for the full contract.
pub async fn submit_benchmark_batch(
    State(state): State<AppState>,
    AuthenticatedClient(client): AuthenticatedClient,
    ApiJson(body): ApiJson<serde_json::Value>,
) -> Result<impl IntoResponse, AppError> {
    // The whole batch shares one disposition (held vs pipeline) from the
    // single authenticated client — there is no per-item mixing. A
    // pending client whose archive is disabled fails the whole request
    // with `403`, same as `POST /benchmarks`.
    let held = resolve_submission_disposition(&client, &state.config)?;
    let client_id = client.client_id;

    let submissions = body["submissions"]
        .as_array()
        .ok_or_else(|| AppError::BadRequest("missing submissions array".into()))?;

    if submissions.is_empty() {
        return Err(AppError::BadRequest("submissions array is empty".into()));
    }
    if submissions.len() > BATCH_MAX_SUBMISSIONS {
        return Err(AppError::BadRequest(format!(
            "too many submissions (max {BATCH_MAX_SUBMISSIONS})"
        )));
    }

    let catalog = state
        .catalog_cache
        .get()
        .await
        .map_err(|e| AppError::Internal(format!("failed to load benchmark catalog: {e}")))?;

    let results: Vec<BatchItemResult> =
        futures::stream::iter(submissions.iter().cloned().enumerate())
            .map(|(i, item)| process_batch_item(&state, &client_id, &catalog, held, i, item))
            .buffered(BATCH_CONCURRENCY)
            .collect()
            .await;

    Ok(Json(BatchResponse { results }))
}

// GET /jobs/{job_id}
pub async fn get_job(
    State(state): State<AppState>,
    AuthenticatedClient(client): AuthenticatedClient,
    Path(job_id): Path<String>,
) -> Result<impl IntoResponse, AppError> {
    let job_id = JobId::try_new(job_id)?;
    let record = state
        .submission_store
        .find_job(&job_id)
        .await
        .map_err(|e| AppError::Internal(format!("failed to find job: {e}")))?
        .ok_or_else(|| AppError::NotFound("job not found".into()))?;

    // Parse the body via the typed Submission so we can dispatch on
    // variant cleanly. Uses `parse_stored_submission` so legacy
    // bodies that pre-date the `message_type` tag still parse as
    // success — production has ~20k of them.
    let submission = crate::submission::parse_stored_submission(&record.body)
        .map_err(|e| AppError::Internal(format!("malformed submission on disk: {e}")))?;

    // Path-level partitioning by client is gone; enforce ownership
    // against the payload's `client_id` and 404 on mismatch so we
    // don't leak job existence to other clients.
    let owner = match &submission {
        Submission::Success(s) => &s.client_id,
        Submission::Failure(f) => &f.client_id,
    };
    if owner.as_str() != client.client_id.as_str() {
        return Err(AppError::NotFound("job not found".into()));
    }

    match submission {
        // Success and failure bodies are different typed structs; erase to
        // `Response` here so both `match` arms share one return type.
        Submission::Failure(f) => Ok(Json(failure_job_response(*f)).into_response()),
        Submission::Success(s) => Ok(success_job_response(&state, &record.state, *s, &job_id)
            .await?
            .into_response()),
    }
}

/// `GET /jobs/{job_id}` body for a failed run (httpapi.md §2.9). `status` is the
/// constant `"failed"`; every other field is moved out of the stored
/// [`FailureSubmission`] (no clones).
#[derive(Serialize)]
struct FailureJobResponse {
    job_id: JobId,
    benchmark_id: BenchmarkId,
    benchmark_type: BenchmarkType,
    status: &'static str,
    submitted_at: DateTime<Utc>,
    failure_reason: NonEmptyTrimmedString,
    model_name: Option<NonEmptyTrimmedString>,
    model_quant: Option<NonEmptyTrimmedString>,
    runtime_name: Option<NonEmptyTrimmedString>,
    runtime_version: Option<NonEmptyTrimmedString>,
    /// A failure is never scored, so it has no warehouse row — this response is
    /// the only place the submitting client's version is readable.
    client_version: Option<NonEmptyTrimmedString>,
}

fn failure_job_response(f: crate::submission::FailureSubmission) -> FailureJobResponse {
    let crate::submission::FailureSubmission {
        wire,
        job_id,
        submitted_at,
        benchmark_type,
        ..
    } = f;
    let crate::submission::FailureInput {
        benchmark_id,
        failure_reason,
        model_name,
        model_quant,
        runtime_name,
        runtime_version,
        client_version,
        ..
    } = wire;
    FailureJobResponse {
        job_id,
        benchmark_id,
        benchmark_type,
        status: "failed",
        submitted_at,
        failure_reason,
        model_name,
        model_quant,
        runtime_name,
        runtime_version,
        client_version,
    }
}

/// `GET /jobs/{job_id}` body for a successful run (httpapi.md §2.9). The
/// `scored_at` / `score_runtime_version` / `metrics` fields are always present
/// (`null` until the job is scored), so they are `Option` *without*
/// `skip_serializing_if` — mirroring the prior `json!` that seeded them `null`
/// and filled them in once metrics were read.
#[derive(Serialize)]
struct SuccessJobResponse {
    job_id: JobId,
    benchmark_id: BenchmarkId,
    benchmark_type: BenchmarkType,
    status: &'static str,
    submitted_at: DateTime<Utc>,
    scored_at: Option<String>,
    score_runtime_version: Option<String>,
    metrics: Option<Vec<JobMetric>>,
}

async fn success_job_response(
    state: &AppState,
    job_state: &JobState,
    s: crate::submission::SuccessSubmission,
    job_id: &JobId,
) -> Result<Json<SuccessJobResponse>, AppError> {
    let mut scored_at = None;
    let mut score_runtime_version = None;
    let mut metrics = None;

    if *job_state == JobState::Processed {
        // Scan only the recent-day window (`warehouse_read_days`). Hard cap:
        // a job scored longer ago than the window reports as `processed`
        // with `metrics: null` rather than triggering a whole-archive scan —
        // its rows stay available for bulk queries.
        if let Some(job_metrics) = state
            .warehouse_store
            .read_job_metrics(&s.wire.benchmark_id, &s.client_id, job_id)
            .await
            .map_err(|e| AppError::Internal(format!("failed to read metrics: {e}")))?
        {
            // `JobMetric` serializes to the same `{metric, value, value_stddev,
            // unit}` shape the old `json!` built by hand, so the array moves
            // through unchanged.
            scored_at = Some(job_metrics.scored_at);
            score_runtime_version = job_metrics.score_runtime_version;
            metrics = Some(job_metrics.metrics);
        }
    }

    let crate::submission::SuccessSubmission {
        wire,
        job_id: response_job_id,
        submitted_at,
        benchmark_type,
        ..
    } = s;
    Ok(Json(SuccessJobResponse {
        job_id: response_job_id,
        benchmark_id: wire.benchmark_id,
        benchmark_type,
        status: job_state.as_str(),
        submitted_at,
        scored_at,
        score_runtime_version,
        metrics,
    }))
}

/// One element of the `GET /jobs/{job_id}/eval-sample-results` array
/// (httpapi.md §2.10). `messages` is the sample's chat transcript, parsed from
/// its stored JSON string — a serialized `Vec<ChatMessage>` written by the
/// scoring path — back into that same contract type, so it serializes as
/// structured JSON rather than an escaped string and round-trips byte-for-byte.
/// The scorer (pipette-scores) emits only `{role, content}` per message, so
/// `ChatMessage`'s `extra` flatten stays empty; it's forward-compat for keys
/// the scorer doesn't send today. The stored row also carries columns
/// (`stop_reason`, `completion_tokens`, …) this response intentionally omits.
#[derive(Serialize)]
struct EvalSampleResultResponse {
    id: String,
    messages: Vec<ChatMessage>,
    completion: String,
    is_correct: bool,
    failed: bool,
    failed_reason: Option<String>,
}

// GET /jobs/{job_id}/eval-sample-results
pub async fn get_eval_sample_results(
    State(state): State<AppState>,
    AuthenticatedClient(client): AuthenticatedClient,
    Path(job_id): Path<String>,
) -> Result<impl IntoResponse, AppError> {
    let job_id = JobId::try_new(job_id)?;
    let record = state
        .submission_store
        .find_job(&job_id)
        .await
        .map_err(|e| AppError::Internal(format!("failed to find job: {e}")))?
        .ok_or_else(|| AppError::NotFound("job not found".into()))?;

    if record.body["client_id"].as_str() != Some(client.client_id.as_str()) {
        return Err(AppError::NotFound("job not found".into()));
    }

    if record.state != JobState::Processed {
        return Err(AppError::NotFound("job not found".into()));
    }

    if record.body["benchmark_type"].as_str() != Some(BenchmarkType::Eval.as_ref()) {
        return Err(AppError::NotFound("job not found".into()));
    }

    let rows = state
        .eval_sample_result_store
        .read(&job_id)
        .await
        .map_err(|e| AppError::Internal(format!("failed to read eval sample results: {e}")))?
        .ok_or_else(|| {
            AppError::Internal(format!(
                "no eval sample results found for processed job {job_id}"
            ))
        })?;

    let result = rows
        .into_iter()
        .map(|r| {
            let messages: Vec<ChatMessage> = serde_json::from_str(&r.messages).map_err(|e| {
                AppError::Internal(format!(
                    "malformed messages JSON for sample '{}': {e}",
                    r.id
                ))
            })?;
            Ok(EvalSampleResultResponse {
                id: r.id,
                messages,
                completion: r.completion,
                is_correct: r.is_correct,
                failed: r.failed,
                failed_reason: r.failed_reason,
            })
        })
        .collect::<Result<Vec<_>, AppError>>()?;

    Ok(Json(result))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stores::{LocalFsSubmissionStore, LocalFsTodoStore};
    use anyhow::Context;
    use rstest::rstest;

    /// Build a `LocalFsTodoStore` over a fresh tempdir with the `todo/`
    /// subdirectory layout the store methods expect to already exist.
    fn todo_store(root: &std::path::Path) -> anyhow::Result<LocalFsTodoStore> {
        let todo_dir = root.join("todo");
        [
            "avail",
            "leased",
            "denied",
            "eligible/clients",
            "pending-reindex",
            "tmp",
            "suspended",
        ]
        .iter()
        .try_for_each(|sub| std::fs::create_dir_all(todo_dir.join(sub)))?;
        Ok(LocalFsTodoStore::new(todo_dir))
    }

    /// Build a `FailureSubmission` from a job body (its identity fields are
    /// copied verbatim), for driving the helpers directly.
    fn failure_from(
        job_body: &serde_json::Value,
    ) -> anyhow::Result<Box<crate::submission::FailureSubmission>> {
        match crate::submission::system_failure_from_job_body(
            job_body,
            crate::benchmark::BenchmarkType::PrefillThroughput,
            "x",
            Utc::now(),
        )? {
            Submission::Failure(f) => Ok(f),
            Submission::Success(_) => {
                anyhow::bail!("system_failure_from_job_body returned a success")
            }
        }
    }

    /// The lease-vanished guard: a retriable failure whose lease was recycled
    /// between the lease scan and the recycle rename. `verify_claim` renews the
    /// lease, so `queue-maintenance` cannot cause this; the client racing itself
    /// (a concurrent heartbeat or duplicate submission) can.
    /// `handle_retriable_failure` must still record the denial and return `Ok`
    /// (→ 202) without recycling — it must not 500, and must not synthesize an
    /// `avail/` entry. The race is hard to stage through the HTTP handler, so it
    /// is exercised by a direct call with no lease seeded.
    #[tokio::test]
    async fn retriable_failure_returns_ok_when_lease_vanished() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let todo = todo_store(dir.path())?;

        let client = ClientId::try_new("c1")?;
        let job = JobId::new_unchecked("550e8400-e29b-41d4-a716-446655440000");
        // A throwaway failure record — its fields aren't reached on this path.
        let job_body = json!({
            "job_id": job.as_str(),
            "spec": {"benchmark": "prefill_throughput_256"},
        });
        let failure = failure_from(&job_body)?;

        // No lease is seeded, so the recycle lookup finds nothing.
        handle_retriable_failure(&todo, failure.as_ref(), &client, &job).await?;

        // Denial recorded; nothing recycled into avail/.
        let denied = todo.list_denied_for_job(&job).await?;
        assert!(denied.iter().any(|c| c.as_str() == "c1"));
        assert!(todo.get_avail_by_job(&job).await?.is_none());
        Ok(())
    }

    /// `verify_claim` authorizes by *renewing* the caller's lease, not merely
    /// reading it: an expired-but-present lease is accepted (completed work is
    /// not discarded) and its expiry is pushed to `new_expiry`, so
    /// `queue-maintenance` can no longer recycle it out from under the in-flight
    /// result write. This is the atomic guard against the expire/submit TOCTOU.
    #[tokio::test]
    async fn verify_claim_renews_expired_but_present_lease() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let todo = todo_store(dir.path())?;
        let client = ClientId::try_new("ev1_me")?;
        let job = JobId::new_unchecked("550e8400-e29b-41d4-a716-446655440000");

        // Plant a lease that has already expired.
        let old_expiry = Utc::now() - chrono::Duration::hours(2);
        let leased = dir
            .path()
            .join("todo")
            .join("leased")
            .join(crate::todo_filename::leased_key(&job, &client, old_expiry));
        std::fs::create_dir_all(leased.parent().context("lease path has no parent")?)?;
        std::fs::write(&leased, b"{}")?;

        let new_expiry = Utc::now() + chrono::Duration::hours(1);
        verify_claim(&todo, &job, &client, new_expiry).await?;

        // The lease still exists and its expiry was pushed into the future — no
        // longer recyclable by the expiry pass while the result write lands.
        let expiry = todo
            .list_leased_for_client(&client)
            .await?
            .iter()
            .find_map(|k| {
                let (kj, _kc, exp) = crate::todo_filename::parse_leased_key(k).ok()?;
                (kj == job).then_some(exp)
            })
            .context("lease should still exist after renewal")?;
        assert!(
            expiry > Utc::now(),
            "lease expiry should be renewed into the future"
        );
        Ok(())
    }

    /// The relinquish's post-recycle recheck
    /// ([`delete_recycled_entry_if_recorded`]) removes a recycled `avail/`
    /// entry exactly when the job already has a submission record — the
    /// "recorded and claimable" state a result write racing the relinquish
    /// leaves behind. With no record, the recycled entry stands: the job is
    /// legitimately claimable again.
    #[rstest]
    #[case::record_landed_removes_entry(true)]
    #[case::no_record_keeps_entry(false)]
    #[tokio::test]
    async fn recycled_entry_removed_only_when_recorded(
        #[case] recorded: bool,
    ) -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let todo = todo_store(dir.path())?;
        let submissions = LocalFsSubmissionStore::new(dir.path().join("submissions"));
        let client = ClientId::try_new("ev1_me")?;
        let job = JobId::new_unchecked("job-recycled");

        // The state the recycle leaves behind: the job is back in avail/ —
        // and, in the raced case, its record has just landed.
        let avail =
            dir.path()
                .join("todo")
                .join("avail")
                .join(crate::todo_filename::avail_filename(
                    &job,
                    crate::types::ExpiresAt::Never,
                ));
        std::fs::write(
            &avail,
            serde_json::to_vec(&json!({ "job_id": "job-recycled" }))?,
        )?;
        if recorded {
            submissions
                .write_processed(
                    &job,
                    &json!({ "job_id": "job-recycled", "message_type": "success" }),
                )
                .await?;
        }

        let removed = delete_recycled_entry_if_recorded(&todo, &submissions, &client, &job).await?;

        assert_eq!(removed, recorded);
        assert_eq!(todo.get_avail_by_job(&job).await?.is_none(), recorded);
        Ok(())
    }

    /// `benchmark_mismatch` is `None` when the report names the benchmark the job
    /// body's spec declares, and carries both sides otherwise — including when the
    /// body has no readable `spec.benchmark` to compare against, which counts as a
    /// disagreement rather than silently passing.
    #[rstest]
    #[case::matches(json!({"spec": {"benchmark": "prefill_throughput_256"}}), None)]
    #[case::different_benchmark(
        json!({"spec": {"benchmark": "eval_test"}}),
        Some(Some("eval_test"))
    )]
    #[case::no_spec(json!({}), Some(None))]
    #[case::spec_without_benchmark(json!({"spec": {}}), Some(None))]
    // A non-string `benchmark` is unreadable, so it cannot match anything.
    #[case::non_string_benchmark(json!({"spec": {"benchmark": 42}}), Some(None))]
    fn benchmark_mismatch_flags_a_disagreeing_or_unreadable_benchmark(
        #[case] job_body: serde_json::Value,
        #[case] expected_side: Option<Option<&str>>,
    ) -> anyhow::Result<()> {
        // The failure always reports `prefill_throughput_256`; the case varies the
        // job body it is compared against.
        let failure = failure_from(&json!({
            "job_id": "j",
            "spec": {"benchmark": "prefill_throughput_256"},
        }))?;

        let got = benchmark_mismatch(failure.as_ref(), &job_body);
        match expected_side {
            None => assert_eq!(got, None, "expected no mismatch"),
            Some(expected) => assert_eq!(got, Some(("prefill_throughput_256", expected))),
        }
        Ok(())
    }

    /// Collect what the line-oriented formatter writes for events emitted inside
    /// `f`, so a test can assert on the rendered log line rather than on the
    /// field values behind it.
    fn capture_logs(f: impl FnOnce()) -> String {
        #[derive(Clone)]
        struct Writer(Arc<std::sync::Mutex<Vec<u8>>>);

        impl std::io::Write for Writer {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.0.lock().expect("capture lock").extend_from_slice(buf);
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Writer {
            type Writer = Self;
            fn make_writer(&'a self) -> Self::Writer {
                self.clone()
            }
        }

        let buf = Arc::new(std::sync::Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::fmt()
            .with_writer(Writer(Arc::clone(&buf)))
            .with_ansi(false)
            .with_max_level(tracing::Level::TRACE)
            .finish();
        tracing::subscriber::with_default(subscriber, f);
        String::from_utf8_lossy(&buf.lock().expect("capture lock")).into_owned()
    }

    /// One event renders as one line, whatever a client puts in a free-text
    /// field. The formatter is line-oriented, so a control character reaching
    /// the output would let a client's string be read back as a record of its
    /// own — carrying a timestamp, level, and target the client chose.
    ///
    /// Asserted on the rendered line rather than on the format specifier, so a
    /// future field added with `Display` fails here too.
    #[rstest]
    #[case::newline("boom\n2026-01-01T00:00:00.000000Z ERROR pipette_mgmt::auth: forged")]
    #[case::carriage_return("boom\rforged")]
    #[case::ansi_escape("boom\u{1b}[2K\u{1b}[31mforged")]
    fn accepted_failure_log_escapes_client_free_text(
        #[case] failure_reason: &str,
    ) -> anyhow::Result<()> {
        let submission: Submission = serde_json::from_value(json!({
            "message_type": "failure",
            "benchmark_id": "prefill_throughput_256",
            "retriable": false,
            "failure_reason": failure_reason,
            "client_id": "ev1_me",
            "job_id": "job-1",
            "submitted_at": "2026-03-10T12:00:00Z",
            "benchmark_type": "prefill_throughput",
        }))?;

        let captured = capture_logs(|| log_accepted_submission(&submission));
        let line = captured.strip_suffix('\n').unwrap_or(&captured);

        assert!(
            !line.contains(char::is_control),
            "no control character may reach the log line: {line:?}"
        );
        // Escaped, not dropped — the reason is the point of the event.
        assert!(line.contains("boom"), "failure_reason is missing: {line:?}");
        Ok(())
    }
}
