use std::collections::hash_map::Entry;
use std::collections::{HashMap, HashSet};
use std::sync::{Mutex, PoisonError, RwLock};
use std::time::{Duration, Instant};

use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use chrono::{DateTime, TimeDelta, Utc};
use ed25519_dalek::{SecretKey, Signature, SigningKey, VerifyingKey};
use getrandom::SysRng;
use getrandom::rand_core::TryRng;
use sha2::Digest;

use pipette_core::client::{Client, ClientStatus, MigrationRecord};
use pipette_core::client_permission::ClientPermissionSet;
use pipette_core::key_index::{KeyHolderPrincipalId, digest_of_key_id};
use pipette_core::user::User;
use pipette_core::user_permission::{Holder, UserPermissionSet};
use pipette_httpsig as httpsig;
use pipette_httpsig::HttpSigError;
use pipette_plan_types::{
    AuditActor, ClientId, ClientPermission, FleetDetail, OrgId, Sha256, UserAccountType, UserId,
    UserKeyId, UserPermission, UserStatus,
};

use crate::error::AppError;
use crate::gate::Gate;
use crate::state::AppState;

/// How far a request's `X-Timestamp` may sit from server time, in either
/// direction.
pub(crate) const TIMESTAMP_TOLERANCE_SECS: i64 = 300;

/// How often [`ReplayCache`] drops aged-out entries. A late sweep costs memory
/// and nothing else.
const SWEEP_INTERVAL_SECS: i64 = 60;

fn within_tolerance(ts: DateTime<Utc>, now: DateTime<Utc>) -> bool {
    (now - ts).num_seconds().abs() <= TIMESTAMP_TOLERANCE_SECS
}

/// Whether a signature had already been spent — see [`ReplayCache::claim`].
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum SignatureClaim {
    Fresh,
    Replayed,
    /// The last sweep aged the timestamp out, so the cache can no longer tell.
    Expired,
}

/// Spent signatures, keyed on the decoded bytes so a re-encoded header cannot
/// pass as new. Entries live while [`within_tolerance`] holds for their
/// timestamp, and [`Self::claim`] refuses as expired any timestamp the last
/// sweep already aged out, so a dropped entry can never be spent again.
pub struct ReplayCache {
    inner: Mutex<Spent>,
}

struct Spent {
    /// Signature bytes → the timestamp of the request that spent them.
    signatures: HashMap<[u8; 64], DateTime<Utc>>,
    /// Seeded by the first claim, so the cache reads only its caller's clock.
    last_sweep: Option<DateTime<Utc>>,
}

impl ReplayCache {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(Spent {
                signatures: HashMap::new(),
                last_sweep: None,
            }),
        }
    }

    /// Lookup and insert share one lock, so of two racing copies of a request
    /// exactly one gets [`SignatureClaim::Fresh`].
    pub(crate) fn claim(
        &self,
        signature: &[u8; 64],
        timestamp: DateTime<Utc>,
        now: DateTime<Utc>,
    ) -> SignatureClaim {
        // Poison is harmless: a panic between `HashMap` calls cannot tear it.
        let mut spent = self.inner.lock().unwrap_or_else(PoisonError::into_inner);

        // The first claim starts the interval rather than sweeping an empty map.
        let last_sweep = *spent.last_sweep.get_or_insert(now);
        let swept = if (now - last_sweep).num_seconds() >= SWEEP_INTERVAL_SECS {
            spent.signatures.retain(|_, ts| within_tolerance(*ts, now));
            spent.last_sweep = Some(now);
            now
        } else {
            last_sweep
        };
        // A sweep on a later clock than this request's freshness check may have dropped its entry.
        if (swept - timestamp).num_seconds() > TIMESTAMP_TOLERANCE_SECS {
            return SignatureClaim::Expired;
        }

        match spent.signatures.entry(*signature) {
            Entry::Occupied(_) => SignatureClaim::Replayed,
            Entry::Vacant(slot) => {
                slot.insert(timestamp);
                SignatureClaim::Fresh
            }
        }
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.inner
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .signatures
            .len()
    }
}

impl Default for ReplayCache {
    fn default() -> Self {
        Self::new()
    }
}

/// Clients known to have presented a `v1` or `v3` signature. Migration is
/// monotonic, so a hit never goes stale; a miss means "ask the store", since
/// the client may have migrated through another instance.
pub struct MigratedClients {
    inner: RwLock<HashSet<ClientId>>,
}

impl MigratedClients {
    pub fn new() -> Self {
        Self {
            inner: RwLock::new(HashSet::new()),
        }
    }

    fn contains(&self, client_id: &ClientId) -> bool {
        self.inner
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .contains(client_id)
    }

    fn insert(&self, client_id: ClientId) {
        self.inner
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(client_id);
    }
}

impl Default for MigratedClients {
    fn default() -> Self {
        Self::new()
    }
}

/// The shortest time between two liveness stamps that one process writes for
/// one client.
const LIVENESS_STAMP_INTERVAL: Duration = Duration::from_secs(60);

/// When this process last tried to stamp each client's liveness marker, and the
/// wait before the next try. The try is recorded before the write starts, so
/// concurrent requests from one client write once.
pub struct LivenessThrottle {
    inner: Mutex<HashMap<ClientId, (Instant, Duration)>>,
}

impl LivenessThrottle {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
        }
    }

    fn claim(&self, client_id: &ClientId, now: Instant) -> bool {
        let mut tries = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        let due = tries
            .get(client_id)
            .is_none_or(|(tried, wait)| now.saturating_duration_since(*tried) >= *wait);
        if due {
            tries.insert(client_id.clone(), (now, LIVENESS_STAMP_INTERVAL));
        }
        due
    }

    fn retry_after(&self, client_id: &ClientId, wait: Duration) {
        if let Some((_, current)) = self
            .inner
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get_mut(client_id)
        {
            *current = wait;
        }
    }
}

impl Default for LivenessThrottle {
    fn default() -> Self {
        Self::new()
    }
}

/// Half of [`LIVENESS_STAMP_INTERVAL`] plus a random part of the other half, so
/// many clients' retries spread out after the store recovers.
fn liveness_retry_delay(random: u64) -> Duration {
    let half = LIVENESS_STAMP_INTERVAL / 2;
    let half_ms = u64::try_from(half.as_millis()).unwrap_or(u64::MAX);
    half + Duration::from_millis(random.checked_rem(half_ms).unwrap_or(0))
}

/// A failed write is logged and does not fail the authenticated request. The
/// events name no client: the request span already carries `client_id`.
async fn stamp_liveness(state: &AppState, client_id: &ClientId) {
    if !state.liveness_throttle.claim(client_id, Instant::now()) {
        return;
    }
    match state.todo_store.stamp_liveness(client_id).await {
        Ok(()) => tracing::debug!("liveness stamped"),
        Err(e) => {
            // Without a random value the retry is not spread, and still waits.
            let retry_in = liveness_retry_delay(SysRng.try_next_u64().unwrap_or(0));
            state.liveness_throttle.retry_after(client_id, retry_in);
            tracing::warn!(
                error = %e,
                retry_in_secs = retry_in.as_secs(),
                "failed to stamp liveness; serving the request"
            );
        }
    }
}

/// Whether `client_id` has presented a `v1` or `v3` signature. A read failure
/// propagates: a swallowed error would reopen the replayable fallback.
async fn has_migrated(state: &AppState, client_id: &ClientId) -> Result<bool, AppError> {
    if state.migrated_clients.contains(client_id) {
        return Ok(true);
    }
    let migrated = state
        .client_store
        .has_signature_migration(client_id)
        .await
        .map_err(|e| {
            AppError::Internal(format!(
                "failed to read the signature migration flag: {e:#}"
            ))
        })?;
    if migrated {
        state.migrated_clients.insert(client_id.clone());
    }
    Ok(migrated)
}

/// Best-effort: the request already authenticated, and the next `v1` request
/// retries an unwritten marker.
async fn record_migration(state: &AppState, client_id: &ClientId, now: DateTime<Utc>) {
    if state.migrated_clients.contains(client_id) {
        return;
    }
    match state
        .client_store
        .record_signature_migration(client_id, now)
        .await
    {
        Ok(record) => {
            state.migrated_clients.insert(client_id.clone());
            // Only the first write logs: a restart empties the cache, so
            // `Existing` is an old migration.
            if record == MigrationRecord::First {
                tracing::info!(
                    client_id = %client_id,
                    "client migrated to v1 signatures; timestamp-only refused from here on"
                );
            }
        }
        Err(e) => tracing::warn!(
            client_id = %client_id,
            error = %e,
            "failed to record signature migration"
        ),
    }
}

/// The client a request is signed by, once its signature has been verified.
/// Extraction spends the signature, so extract it once per request: a second
/// extraction (say, a `from_extractor` layer too) is rejected as a replay.
pub struct AuthenticatedClient(pub Client);

impl FromRequestParts<AppState> for AuthenticatedClient {
    type Rejection = AppError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let verified = verify_client(parts, state).await;
        crate::audit::note_caller(
            parts,
            verified
                .as_ref()
                .ok()
                .map(|AuthenticatedClient(client)| AuditActor::Client {
                    client_id: client.client_id.clone(),
                }),
        );
        verified
    }
}

async fn verify_client(parts: &Parts, state: &AppState) -> Result<AuthenticatedClient, AppError> {
    let client_id = ClientId::try_new(
        parts
            .headers
            .get("X-Client-Id")
            .and_then(|v| v.to_str().ok())
            .ok_or_else(|| AppError::Unauthorized("missing X-Client-Id header".into()))?,
    )
    .map_err(|e| AppError::Unauthorized(format!("invalid X-Client-Id header: {e}")))?;
    // Recorded before the signature check, so the extractor's own `500`s
    // and refusals are attributed in the request span.
    tracing::Span::current().record("client_id", tracing::field::display(&client_id));

    let timestamp = parts
        .headers
        .get("X-Timestamp")
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| AppError::Unauthorized("missing X-Timestamp header".into()))?
        .to_string();

    let signature_hex = parts
        .headers
        .get("X-Signature")
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| AppError::Unauthorized("missing X-Signature header".into()))?
        .to_string();

    let client = state
        .client_store
        .get_client(&client_id)
        .await
        .map_err(|e| AppError::Internal(format!("failed to load the client: {e:#}")))?
        .ok_or_else(|| {
            tracing::warn!("refused: unknown client");
            AppError::Unauthorized("client not found".into())
        })?;

    let ts = chrono::DateTime::parse_from_rfc3339(&timestamp)
        .map_err(|_| AppError::Unauthorized("invalid timestamp format".into()))?
        .with_timezone(&Utc);
    let now = Utc::now();
    if !within_tolerance(ts, now) {
        tracing::warn!(
            skew_secs = (now - ts).num_seconds(),
            "refused: request timestamp outside the tolerance window"
        );
        return Err(AppError::Unauthorized("timestamp expired".into()));
    }

    // Registration validated the key as 32-byte hex, so a decode failure is
    // a corrupt record and a `500`.
    let pk_bytes = hex::decode(client.public_key.as_str())
        .map_err(|_| AppError::Internal("invalid stored public key".into()))?;
    let pk_array: [u8; 32] = pk_bytes
        .try_into()
        .map_err(|_| AppError::Internal("invalid public key length".into()))?;
    // Registration does not check for a curve point, so a bad key is the
    // caller's `401`; a `500` would let anyone mint 5xx responses.
    let verifying_key = VerifyingKey::from_bytes(&pk_array).map_err(|_| {
        tracing::warn!("refused: the registered public key is not a valid curve point");
        AppError::Unauthorized("invalid public key".into())
    })?;

    let sig_bytes = hex::decode(&signature_hex)
        .map_err(|_| AppError::Unauthorized("invalid signature hex".into()))?;
    let sig_array: [u8; 64] = sig_bytes
        .try_into()
        .map_err(|_| AppError::Unauthorized("invalid signature length".into()))?;
    let signature = Signature::from_bytes(&sig_array);

    // The nonce narrows the signature to one request, not one request
    // shape. `docs/authentication.md` is the contract clients sign against.
    let path_and_query = parts.uri.path_and_query().map_or("", |p| p.as_str());
    let nonce = parts
        .headers
        .get("X-Nonce")
        .and_then(|v| v.to_str().ok())
        .filter(|nonce| !nonce.is_empty());

    // Strict: a small-order public key admits a signature that verifies
    // against every message.
    let verifies = |payload: String| {
        verifying_key
            .verify_strict(payload.as_bytes(), &signature)
            .is_ok()
    };
    // `v3` is `v1` with the authority, so it cannot be replayed against
    // another deployment; `v1` stays accepted for the devices in the field.
    let authority = httpsig::authority(parts);
    let host_bound = nonce
        .zip(authority.as_deref())
        .filter(|(nonce, authority)| {
            verifies(format!(
                "v3\n{}\n{}\n{}\n{}\n{}\n{}",
                parts.method, authority, path_and_query, timestamp, client_id, nonce
            ))
        })
        .map(|(_, authority)| authority);
    if let Some(authority) = host_bound {
        own_authority(state, authority)?;
    }
    let payload_verified = host_bound.is_some()
        || nonce.is_some_and(|nonce| {
            verifies(format!(
                "v1\n{}\n{}\n{}\n{}\n{}",
                parts.method, path_and_query, timestamp, client_id, nonce
            ))
        });

    if !payload_verified {
        // The timestamp-only fallback lets clients migrate without a flag
        // day. It binds nothing but freshness.
        let legacy_verified = state.config.accept_legacy_signatures
            && verifying_key
                .verify_strict(timestamp.as_bytes(), &signature)
                .is_ok();
        if !legacy_verified {
            // Name the missing nonce: for a client that signs correctly but
            // omits it, that is the whole problem.
            let reason = match nonce {
                None => "missing X-Nonce header",
                Some(_) => "invalid signature",
            };
            tracing::warn!(reason, "refused: the signature did not verify");
            return Err(AppError::Unauthorized(reason.into()));
        }

        // A migrated client is held to `v1`, so its captured timestamp-only
        // signatures stop authenticating. Checked only once the legacy
        // signature verifies, so noise never reaches the store.
        if has_migrated(state, &client_id).await? {
            tracing::warn!(
                client_id = %client_id,
                method = %parts.method,
                path = %path_and_query,
                "refused timestamp-only signature from a migrated client"
            );
            return Err(AppError::Unauthorized("invalid signature".into()));
        }

        // Logged per request: these are the clients still to migrate.
        tracing::warn!(
            client_id = %client_id,
            method = %parts.method,
            path = %path_and_query,
            "accepted timestamp-only signature"
        );
    }

    // Only a `v1` or `v3` signature names a single request, so only it is
    // claimed, and only once verified, so invented signatures cannot fill the
    // cache.
    if payload_verified {
        match state.replay_cache.claim(&sig_array, ts, now) {
            SignatureClaim::Fresh => {}
            SignatureClaim::Replayed => {
                // The signature is a credential and stays out of the log.
                tracing::warn!(
                    client_id = %client_id,
                    method = %parts.method,
                    path = %path_and_query,
                    "rejected replayed signature"
                );
                return Err(AppError::Unauthorized("signature already used".into()));
            }
            SignatureClaim::Expired => {
                tracing::warn!(
                    skew_secs = (now - ts).num_seconds(),
                    "refused: request timestamp outside the tolerance window"
                );
                return Err(AppError::Unauthorized("timestamp expired".into()));
            }
        }
        record_migration(state, &client_id, now).await;
    }

    // Every verified signature counts as activity. This includes the
    // request of a pending client that `ApprovedClient` refuses.
    stamp_liveness(state, &client_id).await;

    Ok(AuthenticatedClient(client))
}

/// An approved [`AuthenticatedClient`]; a pending client gets `403` before any
/// body is read. Spends the signature, so take one or the other, never both.
pub struct ApprovedClient(pub Client);

impl FromRequestParts<AppState> for ApprovedClient {
    type Rejection = AppError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let AuthenticatedClient(client) =
            AuthenticatedClient::from_request_parts(parts, state).await?;
        if client.status != ClientStatus::Approved {
            tracing::warn!("refused: client is not approved");
            return Err(AppError::Forbidden("client is not approved".into()));
        }
        Ok(ApprovedClient(client))
    }
}

/// An authenticated client's permission set, read once per request and never
/// cached: a revocation must take effect on the client's next request.
pub struct ClientRights(Option<ClientPermissionSet>);

impl ClientRights {
    /// A read failure is a `500`, never an empty set, which would refuse a
    /// client that holds the right.
    pub async fn load(state: &AppState, client_id: &ClientId) -> Result<Self, AppError> {
        let stored = state
            .client_store
            .get_client_permissions(client_id)
            .await
            .map_err(|e| {
                AppError::Internal(format!("failed to read the client's permissions: {e:#}"))
            })?;
        Ok(Self(stored))
    }

    /// `Ok(())` when the client holds `permission`; a logged `403` otherwise.
    /// The request span carries `client_id`, `method` and `path`, so the event
    /// does not repeat them.
    pub fn require(&self, permission: ClientPermission) -> Result<(), AppError> {
        if pipette_core::client_permission::effective_holds(self.0.as_ref(), permission) {
            return Ok(());
        }
        tracing::warn!(
            required_permission = %permission,
            "refused: client lacks permission"
        );
        Err(AppError::Forbidden(format!(
            "client lacks the {permission} permission"
        )))
    }
}

/// An [`ApprovedClient`] that also holds `perform_work`, decided from headers
/// alone. Not for the submission routes: their permission follows the body.
pub struct WorkingClient(pub Client);

impl FromRequestParts<AppState> for WorkingClient {
    type Rejection = AppError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        // Status first: a pending client is told it is pending, not missing a
        // grant.
        let ApprovedClient(client) = ApprovedClient::from_request_parts(parts, state).await?;
        ClientRights::load(state, &client.client_id)
            .await?
            .require(ClientPermission::PerformWork)?;
        Ok(WorkingClient(client))
    }
}

fn required_header(parts: &Parts, name: &'static str) -> Result<String, AppError> {
    parts
        .headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
        .ok_or_else(|| AppError::Unauthorized(format!("missing {name} header")))
}

/// A `401`, never a `500`: the signature is the caller's input.
fn decode_signature(signature_hex: &str) -> Result<[u8; 64], AppError> {
    hex::decode(signature_hex)
        .map_err(|_| AppError::Unauthorized("invalid signature hex".into()))?
        .try_into()
        .map_err(|_| AppError::Unauthorized("invalid signature length".into()))
}

/// A hex decode failure is a corrupt record (`500`); bytes that are no curve
/// point are the caller's (`401`).
fn verifying_key(public_key: &pipette_plan_types::PublicKeyHex) -> Result<VerifyingKey, AppError> {
    let bytes: [u8; 32] = hex::decode(public_key.as_str())
        .map_err(|_| AppError::Internal("invalid stored public key".into()))?
        .try_into()
        .map_err(|_| AppError::Internal("invalid public key length".into()))?;
    VerifyingKey::from_bytes(&bytes).map_err(|_| {
        tracing::warn!("refused: the registered public key is not a valid curve point");
        AppError::Unauthorized("invalid public key".into())
    })
}

/// `Ok(())` when `authority`, the one a verified signature covers, is one of
/// the config's `authorities`, so a request signed for another deployment
/// holding the same key cannot be replayed here.
fn own_authority(state: &AppState, authority: &str) -> Result<(), AppError> {
    if state
        .config
        .authorities
        .iter()
        .any(|ours| ours.eq_ignore_ascii_case(authority))
    {
        return Ok(());
    }
    tracing::warn!(%authority, "refused: a signature over another authority");
    Err(AppError::Unauthorized(format!(
        "the signature covers authority {authority}, which this server does not answer to"
    )))
}

/// A fresh Ed25519 key. Not `SigningKey::generate`: it takes an infallible RNG,
/// so an OS failure would panic.
pub fn generate_signing_key() -> Result<SigningKey, getrandom::Error> {
    let mut secret = SecretKey::default();
    SysRng.try_fill_bytes(&mut secret)?;
    Ok(SigningKey::from_bytes(&secret))
}

/// The account a request is signed by, and the key it signed with. Spends the
/// signature ([`ReplayCache::claim`]): take one extractor per request.
pub struct AuthenticatedUser {
    /// `disabled` is refused here; `pending` is left to [`ActiveUser`].
    pub user: User,
    /// Recorded, not re-derived: a revoked key is deleted.
    pub key_id: UserKeyId,
}

impl FromRequestParts<AppState> for AuthenticatedUser {
    type Rejection = AppError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let verified = verify_user(parts, state).await;
        crate::audit::note_caller(
            parts,
            verified.as_ref().ok().map(|signed| AuditActor::User {
                user_id: signed.user.user_id.clone(),
                key_id: signed.key_id.clone(),
            }),
        );
        verified
    }
}

async fn verify_user(parts: &mut Parts, state: &AppState) -> Result<AuthenticatedUser, AppError> {
    if httpsig::present(&parts.headers) {
        return verify_rfc9421(parts, state).await;
    }
    // `X-Client-Id` carries either prefix: the header keeps its name
    // because it is deployed under it, and the value widened.
    let user_id = UserId::try_new(required_header(parts, "X-Client-Id")?)
        .map_err(|e| AppError::Unauthorized(format!("invalid X-Client-Id header: {e}")))?;
    // Recorded before the signature is checked, so the extractor's own
    // `500`s are attributed rather than orphaned.
    tracing::Span::current().record("client_id", tracing::field::display(&user_id));

    let signature = UserSignature::parse(parts)?;
    let principal = user_id.to_string();
    signature.verify(parts, state, user_id, &principal).await
}

/// A user request's signing headers, parsed and inside the freshness window.
struct UserSignature {
    key_id: UserKeyId,
    timestamp: String,
    ts: DateTime<Utc>,
    now: DateTime<Utc>,
    signature_hex: String,
    nonce: String,
}

impl UserSignature {
    fn parse(parts: &Parts) -> Result<Self, AppError> {
        let key_id = UserKeyId::try_new(required_header(parts, "X-User-Key-Id")?)
            .map_err(|e| AppError::Unauthorized(format!("invalid X-User-Key-Id header: {e}")))?;
        let timestamp = required_header(parts, "X-Timestamp")?;
        let signature_hex = required_header(parts, "X-Signature")?;
        // No fallback to fall back *to*, so the nonce is required outright.
        let nonce = required_header(parts, "X-Nonce")?;
        if nonce.is_empty() {
            return Err(AppError::Unauthorized("missing X-Nonce header".into()));
        }

        let ts = chrono::DateTime::parse_from_rfc3339(&timestamp)
            .map_err(|_| AppError::Unauthorized("invalid timestamp format".into()))?
            .with_timezone(&Utc);
        let now = Utc::now();
        if !within_tolerance(ts, now) {
            tracing::warn!(
                %key_id,
                skew_secs = (now - ts).num_seconds(),
                "refused: request timestamp outside the tolerance window"
            );
            return Err(AppError::Unauthorized("timestamp expired".into()));
        }

        Ok(Self {
            key_id,
            timestamp,
            ts,
            now,
            signature_hex,
            nonce,
        })
    }

    /// Verify the signature as `user_id`'s, with `principal` in the payload's
    /// principal field: the account id, or empty when the signer named only its
    /// key ([`KeyOnlyUser`]).
    async fn verify(
        self,
        parts: &Parts,
        state: &AppState,
        user_id: UserId,
        principal: &str,
    ) -> Result<AuthenticatedUser, AppError> {
        let Self {
            key_id,
            timestamp,
            ts,
            now,
            signature_hex,
            nonce,
        } = self;
        if !state.config.accept_v2_signatures {
            tracing::warn!(%key_id, "refused: a v2 signature while accept_v2_signatures is off");
            return Err(AppError::Unauthorized(
                "v2 signatures are not accepted; sign with RFC 9421".into(),
            ));
        }

        // Both gets at once, each checked in the order below: the key first,
        // the account only once the signature verifies.
        let (registration, user) = tokio::join!(
            state.user_store.get_user_key(&user_id, &key_id),
            state.user_store.get_user(&user_id),
        );

        // The binding check: addressed by both ids, so a request naming another
        // account reads a key that holds nothing.
        let registration = registration
            .map_err(|e| AppError::Internal(format!("failed to load the user key: {e:#}")))?
            .ok_or_else(|| {
                tracing::warn!(%key_id, "refused: unknown user key");
                AppError::Unauthorized("key not found".into())
            })?;

        let verifying_key = verifying_key(&registration.public_key)?;
        let signature_bytes = decode_signature(&signature_hex)?;
        let signature = Signature::from_bytes(&signature_bytes);

        // `v2` signs the key id, so the header is only a lookup hint. Strict
        // for the reason [`AuthenticatedClient`] gives.
        let path_and_query = parts.uri.path_and_query().map_or("", |p| p.as_str());
        let payload = format!(
            "v2\n{}\n{}\n{}\n{}\n{}\n{}",
            parts.method, path_and_query, timestamp, principal, key_id, nonce
        );
        if verifying_key
            .verify_strict(payload.as_bytes(), &signature)
            .is_err()
        {
            // No legacy branch: that fallback is a per-client migration aid, and
            // users start strict.
            tracing::warn!(%key_id, "refused: the signature did not verify");
            return Err(AppError::Unauthorized("invalid signature".into()));
        }
        state.legacy_user_keys.note(&key_id);
        admit(
            state,
            parts,
            Verified {
                user_id,
                key_id,
                registration,
                user,
                signature: signature_bytes,
                ts,
                now,
            },
        )
    }
}

/// A user signature that verified, of either scheme, and what [`admit`] reads.
struct Verified {
    user_id: UserId,
    key_id: UserKeyId,
    registration: pipette_core::user::UserKey,
    user: anyhow::Result<Option<User>>,
    signature: [u8; 64],
    ts: DateTime<Utc>,
    now: DateTime<Utc>,
}

/// The checks after a user signature verifies, shared by both schemes: the
/// key's expiry, the account's standing, and the replay record.
fn admit(
    state: &AppState,
    parts: &Parts,
    verified: Verified,
) -> Result<AuthenticatedUser, AppError> {
    let Verified {
        user_id,
        key_id,
        registration,
        user,
        signature: signature_bytes,
        ts,
        now,
    } = verified;
    let path_and_query = parts.uri.path_and_query().map_or("", |p| p.as_str());
    // After the signature, so only the key's holder learns it lapsed.
    if registration.expired(now) {
        tracing::warn!(%key_id, "refused: the key has expired");
        return Err(AppError::Unauthorized("key expired".into()));
    }

    // Signature first, then status: a caller that cannot sign learns nothing
    // about whether the account exists or is disabled.
    let user = user
        .map_err(|e| AppError::Internal(format!("failed to load the user: {e:#}")))?
        // An interrupted deletion can leave a key with no account; it
        // authenticates nobody.
        .ok_or_else(|| {
            tracing::warn!(%key_id, "refused: the key's account does not exist");
            AppError::Unauthorized("user not found".into())
        })?;
    // Disabled reaches nothing; pending reaches `GET /api/v1/users/me` only and
    // stops at [`ActiveUser`]. Matched, so a new status must pick a side.
    match user.status {
        UserStatus::Active | UserStatus::Pending => {}
        UserStatus::Disabled => {
            tracing::warn!("refused: user is disabled");
            return Err(AppError::Forbidden("user is disabled".into()));
        }
    }

    // Spent only once genuine. One cache serves both kinds: the payloads carry
    // distinct version tags.
    match state.replay_cache.claim(&signature_bytes, ts, now) {
        SignatureClaim::Fresh => Ok(AuthenticatedUser { user, key_id }),
        SignatureClaim::Replayed => {
            // The signature is a credential and stays out of the log.
            tracing::warn!(
                user_id = %user_id,
                key_id = %key_id,
                method = %parts.method,
                path = %path_and_query,
                "rejected replayed signature"
            );
            Err(AppError::Unauthorized("signature already used".into()))
        }
        SignatureClaim::Expired => {
            tracing::warn!(
                %key_id,
                skew_secs = (now - ts).num_seconds(),
                "refused: request timestamp outside the tolerance window"
            );
            Err(AppError::Unauthorized("timestamp expired".into()))
        }
    }
}

/// A user request signed with RFC 9421 ([`httpsig`]): the key names the
/// account, through `public-key-index/`, so no header claims one.
async fn verify_rfc9421(
    parts: &mut Parts,
    state: &AppState,
) -> Result<AuthenticatedUser, AppError> {
    // One scheme per request, so no header outside the signature names a
    // principal or a key the signature did not.
    if let Some(legacy) = ["X-Client-Id", "X-User-Key-Id", "X-Signature"]
        .into_iter()
        .find(|name| parts.headers.contains_key(*name))
    {
        tracing::warn!(
            header = legacy,
            "refused: an RFC 9421 request carrying a v2 header"
        );
        return Err(AppError::Unauthorized(format!(
            "a request signs one way: RFC 9421 or X-Signature, and this one carries {legacy}"
        )));
    }
    let now = Utc::now();
    let parsed = httpsig::parse(parts, now, TimeDelta::seconds(TIMESTAMP_TOLERANCE_SECS))
        .map_err(refused_rfc9421)?;
    own_authority(state, &parsed.authority)?;
    let user_id = account_of_key(state, &parsed.key_id).await?;
    tracing::Span::current().record("client_id", tracing::field::display(&user_id));
    // The key names the account; a refusal from here on is a claim on it.
    crate::audit::note_claim(parts, user_id.as_str());
    let key_id = parsed.key_id.clone();

    let (registration, user) = tokio::join!(
        state.user_store.get_user_key(&user_id, &key_id),
        state.user_store.get_user(&user_id),
    );
    let registration = registration
        .map_err(|e| AppError::Internal(format!("failed to load the user key: {e:#}")))?
        .ok_or_else(|| {
            tracing::warn!(%key_id, "refused: unknown user key");
            AppError::Unauthorized("key not found".into())
        })?;
    // `verify_strict` for the reason [`AuthenticatedClient`] gives.
    if verifying_key(&registration.public_key)?
        .verify_strict(parsed.base.as_bytes(), &parsed.signature)
        .is_err()
    {
        tracing::warn!(%key_id, "refused: the signature did not verify");
        return Err(AppError::Unauthorized("invalid signature".into()));
    }
    let authenticated = admit(
        state,
        parts,
        Verified {
            user_id,
            key_id,
            registration,
            user,
            signature: parsed.signature.to_bytes(),
            ts: parsed.created,
            now,
        },
    )?;
    // Without a covered digest the signature vouches for an empty body, so a
    // body that arrives anyway, whatever its framing said, is refused.
    let digest = match parsed.content_sha256 {
        Some(digest) => digest,
        None => Sha256::try_new(hex::encode(sha2::Sha256::digest(b"")))
            .map_err(|e| AppError::Internal(format!("the empty body's digest: {e}")))?,
    };
    parts.extensions.insert(SignedContentSha256(digest));
    Ok(authenticated)
}

fn refused_rfc9421(error: HttpSigError) -> AppError {
    match &error {
        HttpSigError::Stale { skew_secs } => tracing::warn!(
            skew_secs,
            "refused: request timestamp outside the tolerance window"
        ),
        HttpSigError::MissingHeader(_) | HttpSigError::Expired | HttpSigError::Profile(_) => {
            tracing::warn!(%error, "refused: an RFC 9421 signature outside the profile");
        }
    }
    AppError::Unauthorized(error.to_string())
}

/// The body digest an RFC 9421 signature covered (the empty body's when none),
/// for the body readers to hold the body to.
#[derive(Clone, Debug)]
pub struct SignedContentSha256(pub Sha256);

/// The keys this process has seen sign `v2`, each logged once, so the log
/// names the callers still to move to RFC 9421 without a line per request.
pub struct LegacyUserKeys(Mutex<HashSet<UserKeyId>>);

impl LegacyUserKeys {
    pub fn new() -> Self {
        Self(Mutex::new(HashSet::new()))
    }

    fn note(&self, key_id: &UserKeyId) {
        let first = self
            .0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(key_id.clone());
        if first {
            tracing::info!(%key_id, "user signs v2; RFC 9421 replaces it");
        }
    }
}

impl Default for LegacyUserKeys {
    fn default() -> Self {
        Self::new()
    }
}

/// What an authenticated user may do, read once per request like
/// [`ClientRights`]. A separate type: the absent-object default is per kind.
#[derive(Clone)]
pub struct UserRights {
    set: UserPermissionSet,
    holder: Holder,
}

impl UserRights {
    /// A read failure is a `500`, never an empty set. Private: the caller's set
    /// reaches every check as [`Caller`].
    async fn load(state: &AppState, user: &User) -> Result<Self, AppError> {
        let stored = state
            .user_store
            .get_user_permissions(&user.user_id)
            .await
            .map_err(|e| {
                AppError::Internal(format!("failed to read the user's permissions: {e:#}"))
            })?;
        let holder = Holder::of(user);
        Ok(Self {
            set: pipette_core::user_permission::effective(stored, holder),
            holder,
        })
    }

    /// Another account's set, for a check about that account rather than the
    /// caller, such as whether a campaign's new owner files plans.
    pub async fn of_account(state: &AppState, account: &User) -> Result<Self, AppError> {
        Self::load(state, account).await
    }

    /// For a caller that renders a decision rather than gating on it; a gate
    /// uses [`require`](Self::require).
    pub fn holds(&self, permission: UserPermission) -> bool {
        self.set.holds(permission)
    }

    pub fn fleet_detail(&self) -> FleetDetail {
        if self.holds(UserPermission::ReadClients) {
            FleetDetail::Full
        } else {
            FleetDetail::Withheld
        }
    }

    /// Every name in `wanted` this caller may **not** hand out: a caller grants
    /// only what it holds.
    pub fn refused_grants(
        &self,
        wanted: impl IntoIterator<Item = UserPermission>,
    ) -> Vec<UserPermission> {
        self.set.refused_grants(wanted)
    }

    /// The server-wide names `target` holds and this caller does not
    /// ([`UserPermissionSet::server_wide_beyond`]).
    pub fn server_wide_lacking(&self, target: &UserPermissionSet) -> Vec<UserPermission> {
        target.server_wide_beyond(&self.set)
    }

    /// `Ok(())` when the user holds `permission`; a logged `403` otherwise.
    pub fn require(&self, permission: UserPermission) -> Result<(), AppError> {
        if self.set.holds(permission) {
            return Ok(());
        }
        tracing::warn!(
            required_permission = %permission,
            "refused: user lacks permission"
        );
        Err(AppError::Forbidden(if self.holder.may_hold(permission) {
            format!("user lacks the {permission} permission")
        } else if self.holder.campaign_reader {
            format!("a campaign reader holds read_plans alone, not {permission}")
        } else {
            format!("a service account cannot hold the {permission} permission")
        }))
    }
}

/// An [`AuthenticatedUser`] that is `active`; `pending` gets a `403` naming the
/// decision it waits on. Spends the signature: take one extractor, never two.
pub struct ActiveUser {
    pub user: User,
    pub key_id: UserKeyId,
    /// The set that met the route's [`Gate`].
    pub rights: UserRights,
}

impl FromRequestParts<AppState> for ActiveUser {
    type Rejection = AppError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let (gate, AuthenticatedUser { user, key_id }) = signed_active(parts, state).await?;
        if user.account_type == UserAccountType::Service
            && user_org(state, &user.user_id).await?.is_none()
        {
            tracing::warn!("refused: a service account on no organization");
            return Err(AppError::Forbidden(
                "a service account must belong to an organization".into(),
            ));
        }
        let rights = gated_rights(parts, state, gate, &user).await?;
        Ok(Self {
            user,
            key_id,
            rights,
        })
    }
}

/// The account a request acts for and the set that met its route's gate,
/// borrowed from the extractor so every check reads that one set.
#[derive(Clone, Copy)]
pub struct Caller<'a> {
    pub user: &'a User,
    pub rights: &'a UserRights,
}

impl ActiveUser {
    pub fn caller(&self) -> Caller<'_> {
        Caller {
            user: &self.user,
            rights: &self.rights,
        }
    }
}

impl HumanUser {
    pub fn caller(&self) -> Caller<'_> {
        Caller {
            user: &self.user,
            rights: &self.rights,
        }
    }
}

/// An [`ActiveUser`] that is a person's: the extractor of every route that
/// administers accounts, organizations or devices, which a service account
/// never does.
pub struct HumanUser {
    pub user: User,
    pub key_id: UserKeyId,
    pub rights: UserRights,
}

impl FromRequestParts<AppState> for HumanUser {
    type Rejection = AppError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let (gate, AuthenticatedUser { user, key_id }) = signed_active(parts, state).await?;
        if user.account_type == UserAccountType::Service {
            tracing::warn!("refused: a service account on an administration route");
            return Err(AppError::Forbidden(
                "a service account cannot administer accounts, organizations or devices".into(),
            ));
        }
        let rights = gated_rights(parts, state, gate, &user).await?;
        Ok(Self {
            user,
            key_id,
            rights,
        })
    }
}

/// The route's gate and the active account that signed. The gate is read
/// first, so a route declaring none fails whoever calls it.
async fn signed_active(
    parts: &mut Parts,
    state: &AppState,
) -> Result<(Gate, AuthenticatedUser), AppError> {
    let gate = Gate::of(parts)?;
    let signed = AuthenticatedUser::from_request_parts(parts, state).await?;
    require_active(&signed.user)?;
    Ok((gate, signed))
}

async fn gated_rights(
    parts: &mut Parts,
    state: &AppState,
    gate: Gate,
    user: &User,
) -> Result<UserRights, AppError> {
    // An `Active` route scopes itself, by owner or membership, and so would
    // reach past the campaigns a reader is held to.
    if matches!(gate, Gate::Active) && user.campaign_scope.is_some() {
        tracing::warn!("refused: a campaign reader on a route its campaigns do not scope");
        return Err(AppError::Forbidden(
            "a campaign reader reads its campaigns and their plans alone".into(),
        ));
    }
    let rights = UserRights::load(state, user).await?;
    gate.admit(parts, state, &rights).await?;
    Ok(rights)
}

/// `403` unless `user` is active: the standing an [`ActiveUser`] and a Clerk
/// sign-in both need.
pub fn require_active(user: &User) -> Result<(), AppError> {
    match user.status {
        UserStatus::Active => Ok(()),
        UserStatus::Pending => {
            tracing::warn!("refused: user is pending an operator's approval");
            Err(AppError::Forbidden(
                "user is pending an operator's approval".into(),
            ))
        }
        UserStatus::Disabled => {
            tracing::warn!("refused: user is disabled");
            Err(AppError::Forbidden("user is disabled".into()))
        }
    }
}

/// An [`AuthenticatedUser`] whose request may name only its key, for
/// `GET /api/v1/users/me`: the account is the one `public-key-index/` names.
pub struct KeyOnlyUser(pub AuthenticatedUser);

impl FromRequestParts<AppState> for KeyOnlyUser {
    type Rejection = AppError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        // An RFC 9421 signature names only its key whatever the route.
        if parts.headers.contains_key("X-Client-Id") || httpsig::present(&parts.headers) {
            return Ok(Self(
                AuthenticatedUser::from_request_parts(parts, state).await?,
            ));
        }

        let signature = UserSignature::parse(parts)?;
        let user_id = account_of_key(state, &signature.key_id).await?;
        tracing::Span::current().record("client_id", tracing::field::display(&user_id));
        Ok(Self(signature.verify(parts, state, user_id, "").await?))
    }
}

/// The account `public-key-index/` names for `key_id`. A device-held key is
/// also "key not found", so an unauthenticated caller cannot probe for keys.
async fn account_of_key(state: &AppState, key_id: &UserKeyId) -> Result<UserId, AppError> {
    let refuse = || {
        tracing::warn!(%key_id, "refused: no account holds the key");
        AppError::Unauthorized("key not found".into())
    };
    let digest = digest_of_key_id(key_id).ok_or_else(refuse)?;
    let holder = state
        .key_index_store
        .holder_of_public_key(&digest)
        .await
        .map_err(|e| AppError::Internal(format!("failed to read the key's holder: {e:#}")))?;
    match holder {
        Some(KeyHolderPrincipalId::User(user_id)) => Ok(user_id),
        Some(KeyHolderPrincipalId::Client(_)) | None => Err(refuse()),
    }
}

/// The `POST /api/v1/plans/validate` caller. Headers-only, so a device (`403`)
/// is refused before `plan_body_limit_bytes` of body is buffered.
pub struct PlanUser {
    pub user: User,
    pub rights: UserRights,
}

impl FromRequestParts<AppState> for PlanUser {
    type Rejection = AppError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        // Only a user key signs RFC 9421, so such a request is an account's.
        let raw = parts
            .headers
            .get("X-Client-Id")
            .and_then(|v| v.to_str().ok());
        if raw.is_none() && !httpsig::present(&parts.headers) {
            return Err(AppError::Unauthorized("missing X-Client-Id header".into()));
        }
        if raw.is_some_and(|raw| !raw.starts_with(UserId::PREFIX)) {
            crate::audit::note_caller(parts, None);
            tracing::warn!("refused: a device cannot check a plan");
            return Err(AppError::Forbidden(
                "a device cannot check a plan; sign the request as a user account".into(),
            ));
        }
        let ActiveUser { user, rights, .. } = ActiveUser::from_request_parts(parts, state).await?;
        Ok(Self { user, rights })
    }
}

/// The `GET /vault/resources/{id}/content` principal, of either kind. The
/// resource's owner is judged in `pipette_httpapi::vault_access`.
pub enum VaultFetcher {
    /// Holds `perform_work`. Boxed: a [`Client`] is far larger than the other
    /// variant.
    Client(Box<Client>),
    /// No grant is required: an empty set still reaches the caller's own vault.
    User { user: User, rights: UserRights },
}

impl FromRequestParts<AppState> for VaultFetcher {
    type Rejection = AppError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        // Peeked, not consumed: each branch parses the header. Only a user key
        // signs RFC 9421.
        let user = httpsig::present(&parts.headers)
            || parts
                .headers
                .get("X-Client-Id")
                .and_then(|v| v.to_str().ok())
                .ok_or_else(|| AppError::Unauthorized("missing X-Client-Id header".into()))?
                .starts_with(UserId::PREFIX);

        if user {
            let ActiveUser { user, rights, .. } =
                ActiveUser::from_request_parts(parts, state).await?;
            return Ok(Self::User { user, rights });
        }
        let WorkingClient(client) = WorkingClient::from_request_parts(parts, state).await?;
        Ok(Self::Client(Box::new(client)))
    }
}

/// The `GET /benchmarks` reader: anonymous without `X-Client-Id`, else held to
/// its kind's full check, so a bad signature is a `401`, not the public view.
/// Standing is not a gate here; it decides [`Self::org_id`].
pub enum CatalogReader {
    Anonymous,
    /// A signed device, of any status. Boxed for [`VaultFetcher`]'s reason.
    Client(Box<Client>),
    /// A signed account, `active` or `pending`.
    User(User),
}

impl CatalogReader {
    /// The organization whose private entries this reader is served: only an
    /// approved client's or an active user's that is no campaign reader.
    pub async fn org_id(&self, state: &AppState) -> Result<Option<OrgId>, AppError> {
        match self {
            Self::Anonymous => Ok(None),
            Self::Client(client) => client_catalog_org(state, client).await,
            // A campaign reader reads its campaigns alone, not its
            // organization's catalog.
            Self::User(user) if user.campaign_scope.is_some() => Ok(None),
            Self::User(user) => match user.status {
                UserStatus::Active => user_org(state, &user.user_id).await,
                UserStatus::Pending | UserStatus::Disabled => Ok(None),
            },
        }
    }
}

/// The organization `user_id` belongs to, with a failed read as a `500`.
pub async fn user_org(state: &AppState, user_id: &UserId) -> Result<Option<OrgId>, AppError> {
    state
        .org_store
        .get_membership(user_id)
        .await
        .map_err(|e| AppError::Internal(format!("failed to read the account's membership: {e:#}")))
}

/// [`CatalogReader::org_id`] for a device a route already holds. Compared by
/// id, so a stranded membership matches nothing rather than failing the read.
pub async fn client_catalog_org(
    state: &AppState,
    client: &Client,
) -> Result<Option<OrgId>, AppError> {
    match client.status {
        ClientStatus::Approved => {}
        ClientStatus::Pending => return Ok(None),
    }
    pipette_pipeline::org::owning_org(state.org_store.as_ref(), &client.client_id)
        .await
        .map_err(|e| AppError::Internal(format!("failed to resolve the client's owner: {e:#}")))
}

impl FromRequestParts<AppState> for CatalogReader {
    type Rejection = AppError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        // Peeked as in [`VaultFetcher`]. A present but unreadable header is
        // refused by the kind's extractor, not read as anonymous.
        let signed_9421 = httpsig::present(&parts.headers);
        let Some(raw) = parts.headers.get("X-Client-Id") else {
            if !signed_9421 {
                return Ok(Self::Anonymous);
            }
            let AuthenticatedUser { user, .. } =
                AuthenticatedUser::from_request_parts(parts, state).await?;
            return Ok(Self::User(user));
        };

        if signed_9421 || raw.as_bytes().starts_with(UserId::PREFIX.as_bytes()) {
            let AuthenticatedUser { user, .. } =
                AuthenticatedUser::from_request_parts(parts, state).await?;
            return Ok(Self::User(user));
        }
        let AuthenticatedClient(client) =
            AuthenticatedClient::from_request_parts(parts, state).await?;
        Ok(Self::Client(Box::new(client)))
    }
}

#[cfg(test)]
mod tests {
    use ed25519_dalek::Signer;

    use pipette_core::types::DeriveFromKey;
    use pipette_plan_types::PublicKeyHex;

    use crate::auth::*;

    #[test]
    fn test_signature_verification_roundtrip() -> anyhow::Result<()> {
        let signing_key = generate_signing_key()?;
        let verifying_key = signing_key.verifying_key();
        let pk_hex = hex::encode(verifying_key.as_bytes());

        let timestamp = "2026-03-10T12:00:00Z";
        let signature = signing_key.sign(timestamp.as_bytes());
        let sig_hex = hex::encode(signature.to_bytes());

        let pk_bytes = hex::decode(&pk_hex)?;
        let pk_arr: [u8; 32] = pk_bytes
            .try_into()
            .map_err(|_| anyhow::anyhow!("bad pk length"))?;
        let vk = VerifyingKey::from_bytes(&pk_arr)?;
        let sig_bytes = hex::decode(&sig_hex)?;
        let sig_arr: [u8; 64] = sig_bytes
            .try_into()
            .map_err(|_| anyhow::anyhow!("bad sig length"))?;
        let sig = Signature::from_bytes(&sig_arr);
        assert!(vk.verify_strict(timestamp.as_bytes(), &sig).is_ok());
        Ok(())
    }

    /// Strictness is load-bearing: with the identity point as key and `R`, and
    /// `s` = 0, a signature verifies leniently against any message.
    #[test]
    fn test_a_small_order_key_forges_a_signature_for_any_message() -> anyhow::Result<()> {
        let mut identity = [0u8; 32];
        identity[0] = 1;
        let vk = VerifyingKey::from_bytes(&identity)?;

        let mut forged = [0u8; 64];
        forged[0] = 1;
        let forged = Signature::from_bytes(&forged);

        for message in ["one message", "and an entirely different one"] {
            assert!(
                ed25519_dalek::Verifier::verify(&vk, message.as_bytes(), &forged).is_ok(),
                "lenient verification admits the forgery, which is what makes \
                 strictness load-bearing"
            );
            assert!(vk.verify_strict(message.as_bytes(), &forged).is_err());
        }
        Ok(())
    }

    #[test]
    fn test_client_id_derivation_matches() -> anyhow::Result<()> {
        let signing_key = generate_signing_key()?;
        let pk = PublicKeyHex::try_new(hex::encode(signing_key.verifying_key().as_bytes()))?;
        let client_id = ClientId::derive(&pk);
        assert!(client_id.as_str().starts_with("ev1_"));
        Ok(())
    }

    /// A distinct fake signature: `claim` only compares bytes.
    fn sig(n: u8) -> [u8; 64] {
        [n; 64]
    }

    #[rstest::rstest]
    #[case::never_tried(None, None, "ev1_a", true)]
    #[case::claimed_at_the_same_instant(Some(0), None, "ev1_a", false)]
    #[case::claimed_59s_ago(Some(59), None, "ev1_a", false)]
    #[case::claimed_60s_ago(Some(60), None, "ev1_a", true)]
    #[case::failed_29s_ago_with_a_30s_wait(Some(29), Some(30), "ev1_a", false)]
    #[case::failed_30s_ago_with_a_30s_wait(Some(30), Some(30), "ev1_a", true)]
    #[case::another_client_claimed(Some(0), None, "ev1_b", true)]
    fn test_liveness_throttle_claim(
        #[case] claimed_secs_ago: Option<u64>,
        #[case] retry_after_secs: Option<u64>,
        #[case] asking: &str,
        #[case] claimed: bool,
    ) -> anyhow::Result<()> {
        let throttle = LivenessThrottle::new();
        let now = Instant::now() + LIVENESS_STAMP_INTERVAL;
        let a = ClientId::try_new("ev1_a")?;
        if let Some(ago) = claimed_secs_ago {
            assert!(throttle.claim(&a, now - Duration::from_secs(ago)));
        }
        if let Some(wait) = retry_after_secs {
            throttle.retry_after(&a, Duration::from_secs(wait));
        }
        assert_eq!(throttle.claim(&ClientId::try_new(asking)?, now), claimed);
        Ok(())
    }

    /// The retry delay after a failed stamp is at least half the interval and
    /// less than the full interval.
    #[rstest::rstest]
    #[case::smallest(0, Duration::from_secs(30))]
    #[case::wraps(30_000, Duration::from_secs(30))]
    #[case::largest(29_999, Duration::from_millis(59_999))]
    fn test_liveness_retry_delay(#[case] random: u64, #[case] delay: Duration) {
        assert_eq!(liveness_retry_delay(random), delay);
    }

    #[test]
    fn test_first_claim_is_fresh_and_the_second_is_a_replay() -> anyhow::Result<()> {
        let cache = ReplayCache::new();
        let now = Utc::now();

        assert_eq!(cache.claim(&sig(1), now, now), SignatureClaim::Fresh);
        assert_eq!(cache.claim(&sig(1), now, now), SignatureClaim::Replayed);
        // A different signature is unaffected by the spent one.
        assert_eq!(cache.claim(&sig(2), now, now), SignatureClaim::Fresh);
        Ok(())
    }

    #[test]
    fn test_concurrent_claims_admit_exactly_one() -> anyhow::Result<()> {
        let cache = ReplayCache::new();
        let now = Utc::now();

        let fresh = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..16)
                .map(|_| scope.spawn(|| cache.claim(&sig(1), now, now)))
                .collect();
            handles
                .into_iter()
                .filter_map(|h| h.join().ok())
                .filter(|claim| *claim == SignatureClaim::Fresh)
                .count()
        });

        assert_eq!(fresh, 1, "exactly one claim may win");
        Ok(())
    }

    #[test]
    fn test_sweep_drops_only_signatures_past_the_tolerance_window() -> anyhow::Result<()> {
        let cache = ReplayCache::new();
        let start = Utc::now();
        let tolerance = chrono::TimeDelta::seconds(TIMESTAMP_TOLERANCE_SECS);

        assert_eq!(cache.claim(&sig(1), start, start), SignatureClaim::Fresh);
        assert_eq!(cache.len(), 1);

        // Still inside the window: the sweep runs but must keep the entry, or a
        // signature would become replayable before it expired.
        let inside = start + tolerance;
        assert_eq!(cache.claim(&sig(2), inside, inside), SignatureClaim::Fresh);
        assert_eq!(cache.len(), 2, "an in-window signature must be retained");
        assert_eq!(
            cache.claim(&sig(1), start, inside),
            SignatureClaim::Replayed
        );

        // Past the window and due for a sweep: the first entry goes, the second
        // stays.
        let outside = inside + chrono::TimeDelta::seconds(SWEEP_INTERVAL_SECS);
        assert_eq!(
            cache.claim(&sig(3), outside, outside),
            SignatureClaim::Fresh
        );
        assert_eq!(cache.len(), 2, "aged-out signatures should be swept");
        Ok(())
    }

    /// Each step is `(signature, timestamp, now, expected)`, times in seconds
    /// from a common start. A sweep on a later clock than a request was checked
    /// on must not free that request's signature.
    #[rstest::rstest]
    #[case::checked_before_a_later_sweep(&[
        (1, 0, 0, SignatureClaim::Fresh),
        (2, 301, 301, SignatureClaim::Fresh),
        (1, 0, 300, SignatureClaim::Expired),
    ])]
    #[case::checked_long_before_a_later_sweep(&[
        (1, 0, 0, SignatureClaim::Fresh),
        (2, 400, 400, SignatureClaim::Fresh),
        (1, 0, 300, SignatureClaim::Expired),
    ])]
    #[case::unspent_and_checked_before_a_later_sweep(&[
        (1, 0, 0, SignatureClaim::Fresh),
        (2, 301, 301, SignatureClaim::Fresh),
        (3, 0, 300, SignatureClaim::Expired),
    ])]
    #[case::still_inside_the_last_sweep(&[
        (1, 0, 0, SignatureClaim::Fresh),
        (2, 300, 300, SignatureClaim::Fresh),
        (1, 0, 300, SignatureClaim::Replayed),
        (3, 0, 300, SignatureClaim::Fresh),
    ])]
    fn test_sweep_never_frees_a_signature_still_presentable(
        #[case] claims: &[(u8, i64, i64, SignatureClaim)],
    ) -> anyhow::Result<()> {
        let cache = ReplayCache::new();
        let start = Utc::now();
        let at = |secs| start + chrono::TimeDelta::seconds(secs);

        claims
            .iter()
            .enumerate()
            .for_each(|(step, (n, ts, now, expected))| {
                assert_eq!(
                    &cache.claim(&sig(*n), at(*ts), at(*now)),
                    expected,
                    "claim {step}"
                );
            });
        Ok(())
    }
}
