use std::collections::hash_map::Entry;
use std::collections::{HashMap, HashSet};
use std::sync::{Mutex, PoisonError, RwLock};

use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use chrono::{DateTime, Utc};
use ed25519_dalek::{Signature, VerifyingKey};

use crate::client::{Client, MigrationRecord};
use crate::error::AppError;
use crate::handlers::AppState;
use crate::types::ClientId;

/// How far a request's `X-Timestamp` may sit from server time, in either
/// direction.
const TIMESTAMP_TOLERANCE_SECS: i64 = 300;

/// How often [`ReplayCache`] drops entries whose timestamps have aged out. A
/// sweep costs one pass over the map, so it runs on this interval rather than
/// per request; an entry that outlives its window until the next sweep costs
/// memory and nothing else.
const SWEEP_INTERVAL_SECS: i64 = 60;

/// Whether `ts` is close enough to `now` for the request carrying it to be
/// accepted.
fn within_tolerance(ts: DateTime<Utc>, now: DateTime<Utc>) -> bool {
    (now - ts).num_seconds().abs() <= TIMESTAMP_TOLERANCE_SECS
}

/// Whether a signature had already been spent — see [`ReplayCache::claim`].
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum SignatureClaim {
    /// First presentation of this signature; the request may proceed.
    Fresh,
    /// Presented before. The request is a replay.
    Replayed,
}

/// The signatures spent so far, so that each one authenticates one request.
///
/// Ed25519 signing is deterministic, so a signature is a pure function of the
/// payload — and the payload's nonce is what makes it name one request rather
/// than one request *shape*. Seeing the same bytes twice therefore means the
/// request was replayed rather than reissued. Keying on the decoded 64 bytes
/// rather than the header text means re-encoding the same signature cannot
/// present as a new one.
///
/// Entries are held until the timestamp they carry falls outside
/// [`TIMESTAMP_TOLERANCE_SECS`], measured by the same [`within_tolerance`] the
/// freshness check uses. Retention therefore never has to be argued about
/// separately: a request that can still pass the freshness check always finds
/// its entry present, and one whose entry has aged out is rejected as stale
/// before it reaches [`Self::claim`] — so a hit there is always a genuine
/// replay.
///
/// Sweeps drop aged-out entries on an interval rather than the instant they
/// expire, so size is bounded by the authenticated request rate across
/// [`TIMESTAMP_TOLERANCE_SECS`] plus [`SWEEP_INTERVAL_SECS`].
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

    /// Spend `signature`, reporting whether it had been spent already.
    ///
    /// The lookup and the insert happen under one lock, so two copies of the
    /// same request racing each other cannot both see it as unspent — exactly
    /// one gets [`SignatureClaim::Fresh`].
    pub(crate) fn claim(
        &self,
        signature: &[u8; 64],
        timestamp: DateTime<Utc>,
        now: DateTime<Utc>,
    ) -> SignatureClaim {
        // A poisoned lock means some other caller panicked mid-claim. The
        // guarded data is a map and a timestamp, which a panic between two
        // `HashMap` operations cannot leave torn, so the contents stay usable.
        let mut spent = self.inner.lock().unwrap_or_else(PoisonError::into_inner);

        // The first claim starts the interval rather than sweeping an empty map.
        let last_sweep = *spent.last_sweep.get_or_insert(now);
        if (now - last_sweep).num_seconds() >= SWEEP_INTERVAL_SECS {
            spent.signatures.retain(|_, ts| within_tolerance(*ts, now));
            spent.last_sweep = Some(now);
        }

        match spent.signatures.entry(*signature) {
            Entry::Occupied(_) => SignatureClaim::Replayed,
            Entry::Vacant(slot) => {
                slot.insert(timestamp);
                SignatureClaim::Fresh
            }
        }
    }

    /// How many spent signatures are currently held.
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

/// The clients known to have presented a `v1` signature.
///
/// Only the migrated direction is remembered. Migration is monotonic — a client
/// that has presented a `v1` signature never un-presents it — so a remembered
/// entry can never go stale, and the marker read it saves is the one on the hot
/// path.
///
/// A *missing* entry deliberately means "ask the store", not "has not
/// migrated". A client migrates against whichever instance its request reaches,
/// so an instance that cached the negative would go on honoring the
/// timestamp-only fallback for a client that had already migrated elsewhere —
/// which is precisely the signature the fallback makes replayable. Paying a
/// store read per miss is what keeps the guarantee fleet-wide rather than
/// per-process.
/// An `RwLock` rather than a `Mutex`: entries are inserted once per client and
/// read on every authenticated request thereafter, so readers should not
/// exclude each other.
pub struct MigratedClients {
    inner: RwLock<HashSet<ClientId>>,
}

impl MigratedClients {
    pub fn new() -> Self {
        Self {
            inner: RwLock::new(HashSet::new()),
        }
    }

    /// Whether this client is already known to have migrated. A `false` here
    /// means unknown, so the caller consults the store.
    fn contains(&self, client_id: &ClientId) -> bool {
        // A poisoned lock means another caller panicked mid-operation. The
        // guarded data is a set of ids, which a panic between two `HashSet`
        // operations cannot leave torn, so the contents stay usable.
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

/// Whether `client_id` has already presented a `v1` signature, consulting the
/// store on a cache miss.
///
/// A read failure propagates rather than being treated as "has not migrated".
/// The conclusion here is asymmetric: a positive is sound under leniency, but a
/// negative decides that the replayable fallback is still available, so a
/// swallowed error would hand an attacker the very path the migration exists to
/// close — the same reasoning as the soundness note on `renew_lease`
/// (`src/stores/local_fs.rs`).
async fn has_migrated(state: &AppState, client_id: &ClientId) -> Result<bool, AppError> {
    if state.migrated_clients.contains(client_id) {
        return Ok(true);
    }
    let migrated = state
        .auth_store
        .has_signature_migration(client_id)
        .await
        .map_err(|e| AppError::Internal(format!("failed to read signature migration: {e}")))?;
    if migrated {
        state.migrated_clients.insert(client_id.clone());
    }
    Ok(migrated)
}

/// Record that `client_id` has presented a `v1` signature, so the timestamp-only
/// fallback is refused for it from here on.
///
/// Best-effort: the request that triggers this has already authenticated, and
/// failing it over a marker would deny a client that did everything right. An
/// unwritten marker costs nothing permanent — the next `v1` request tries
/// again — so a failure is logged and the request proceeds.
async fn record_migration(state: &AppState, client_id: &ClientId, now: DateTime<Utc>) {
    if state.migrated_clients.contains(client_id) {
        return;
    }
    match state
        .auth_store
        .record_signature_migration(client_id, now)
        .await
    {
        Ok(record) => {
            state.migrated_clients.insert(client_id.clone());
            // Only the call that actually wrote the marker reports the
            // transition. A restart empties the cache, so every migrated client
            // passes through here again on its next request; logging on
            // `Existing` too would announce a migration that happened long ago.
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
///
/// Extraction spends that signature ([`ReplayCache::claim`]), so it must happen
/// exactly once per request: a second extraction of the same request presents
/// the signature the first one recorded and is rejected as a replay. Reach the
/// client from a handler by taking this extractor once, not by re-running it —
/// an `axum::middleware::from_extractor` layer over routes that already take it
/// would reject every request it guards.
pub struct AuthenticatedClient(pub Client);

impl FromRequestParts<AppState> for AuthenticatedClient {
    type Rejection = AppError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let client_id = ClientId::try_new(
            parts
                .headers
                .get("X-Client-Id")
                .and_then(|v| v.to_str().ok())
                .ok_or_else(|| AppError::Unauthorized("missing X-Client-Id header".into()))?,
        )
        .map_err(|e| AppError::Unauthorized(format!("invalid X-Client-Id header: {e}")))?;

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
            .auth_store
            .get_client(&client_id)
            .await
            .map_err(|e| AppError::Internal(format!("failed to load client: {e}")))?
            .ok_or_else(|| AppError::Unauthorized("client not found".into()))?;

        let ts = chrono::DateTime::parse_from_rfc3339(&timestamp)
            .map_err(|_| AppError::Unauthorized("invalid timestamp format".into()))?
            .with_timezone(&Utc);
        let now = Utc::now();
        if !within_tolerance(ts, now) {
            return Err(AppError::Unauthorized("timestamp expired".into()));
        }

        // Verify signature. `client.public_key` is `PublicKeyHex`,
        // already validated as 32-byte hex on registration — the
        // hex decode here is just to materialize the bytes, so hex that no
        // longer decodes to 32 bytes means the stored record is corrupt and
        // the fault is the service's.
        let pk_bytes = hex::decode(client.public_key.as_str())
            .map_err(|_| AppError::Internal("invalid stored public key".into()))?;
        let pk_array: [u8; 32] = pk_bytes
            .try_into()
            .map_err(|_| AppError::Internal("invalid public key length".into()))?;
        // Whether those bytes encode a curve point is a separate question,
        // which registration does not settle: the caller supplies the key, and
        // about half of all 32-byte strings decode to no point at all. A key
        // that cannot authenticate anyone is the caller's, so it draws a `401`
        // — reporting a service fault would let anyone mint 5xx responses by
        // registering one.
        let verifying_key = VerifyingKey::from_bytes(&pk_array)
            .map_err(|_| AppError::Unauthorized("invalid public key".into()))?;

        let sig_bytes = hex::decode(&signature_hex)
            .map_err(|_| AppError::Unauthorized("invalid signature hex".into()))?;
        let sig_array: [u8; 64] = sig_bytes
            .try_into()
            .map_err(|_| AppError::Unauthorized("invalid signature length".into()))?;
        let signature = Signature::from_bytes(&sig_array);

        // Binding the method, path, and query makes a captured signature valid
        // only for the exact request it was minted for, and the nonce narrows
        // that to one *request* rather than one request shape — without it, two
        // different bodies sent to the same endpoint in the same second sign
        // identically, and a replay would be indistinguishable from either.
        // Newline delimiters keep the field boundaries unambiguous whatever a
        // field holds, and the `v1` tag leaves room for a successor scheme to
        // run alongside this one during a rollout. `docs/authentication.md` is
        // the contract clients sign against.
        //
        // A request with no usable nonce cannot present this payload at all;
        // its only remaining route is the timestamp-only fallback below.
        let path_and_query = parts.uri.path_and_query().map_or("", |p| p.as_str());
        let nonce = parts
            .headers
            .get("X-Nonce")
            .and_then(|v| v.to_str().ok())
            .filter(|nonce| !nonce.is_empty());

        // Both payloads are checked with strict verification, which requires
        // the signature's `R` and the client's public key to be points of full
        // order. A small-order public key admits a signature — `R` and `s` both
        // zero-like — that verifies against every message, so a client
        // registered with one could be signed for by anyone. Strictness is what
        // ties a client's identity to possession of its private key.
        let payload_verified = nonce.is_some_and(|nonce| {
            let signed_payload = format!(
                "v1\n{}\n{}\n{}\n{}\n{}",
                parts.method, path_and_query, timestamp, client_id, nonce
            );
            verifying_key
                .verify_strict(signed_payload.as_bytes(), &signature)
                .is_ok()
        });

        if !payload_verified {
            // The timestamp-only payload is available while
            // `accept_legacy_signatures` is set, so clients can migrate without
            // a flag day. It binds nothing but freshness.
            let legacy_verified = state.config.accept_legacy_signatures
                && verifying_key
                    .verify_strict(timestamp.as_bytes(), &signature)
                    .is_ok();
            if !legacy_verified {
                // Name the absent nonce rather than reporting a bare
                // verification failure — for a client that signs correctly but
                // omits the header, that is the whole of the problem.
                return Err(AppError::Unauthorized(match nonce {
                    None => "missing X-Nonce header".into(),
                    Some(_) => "invalid signature".into(),
                }));
            }

            // A client that has presented a `v1` signature is held to it. The
            // fallback is what makes a captured signature replayable, so
            // withdrawing it the moment a client proves it no longer needs it is
            // what turns every signature captured from that client before this
            // point into something that authenticates nothing.
            //
            // This runs only once a timestamp-only signature has verified, so a
            // caller sending noise never reaches the store, and a migrated
            // client's own traffic never reaches it either — the check costs a
            // read only for a genuine legacy request.
            if has_migrated(state, &client_id).await? {
                tracing::warn!(
                    client_id = %client_id,
                    method = %parts.method,
                    path = %path_and_query,
                    "refused timestamp-only signature from a migrated client"
                );
                return Err(AppError::Unauthorized("invalid signature".into()));
            }

            // Logged per request: the client ids appearing here are exactly the
            // ones still to migrate, and silence means the flag can be cleared.
            tracing::warn!(
                client_id = %client_id,
                method = %parts.method,
                path = %path_and_query,
                "accepted timestamp-only signature"
            );
        }

        // Only a verified `v1` signature covers a nonce, so only it names a
        // single request and can be told apart from its own replay. A
        // timestamp-only signature is shared by every request that client makes
        // in the same second; rejecting repeats of one would reject legitimate
        // traffic, so it stays unprotected until the flag is cleared.
        //
        // Spend the signature only once it has proven genuine. Claiming any
        // earlier would let an unauthenticated caller fill the cache with
        // invented signatures, turning replay protection into a memory sink.
        if payload_verified {
            let claim = state.replay_cache.claim(&sig_array, ts, now);
            if claim == SignatureClaim::Replayed {
                // The signature itself is a credential and stays out of the
                // log; the client and target identify the event well enough to
                // act on.
                tracing::warn!(
                    client_id = %client_id,
                    method = %parts.method,
                    path = %path_and_query,
                    "rejected replayed signature"
                );
                return Err(AppError::Unauthorized("signature already used".into()));
            }
            record_migration(state, &client_id, now).await;
        }

        Ok(AuthenticatedClient(client))
    }
}

#[cfg(test)]
mod tests {
    use crate::auth::*;
    use crate::client::derive_client_id;
    use crate::validated::PublicKeyHex;
    use ed25519_dalek::{Signer, SigningKey};

    #[test]
    fn test_signature_verification_roundtrip() -> anyhow::Result<()> {
        let mut csprng = rand_core::OsRng;
        let signing_key = SigningKey::generate(&mut csprng);
        let verifying_key = signing_key.verifying_key();
        let pk_hex = hex::encode(verifying_key.as_bytes());

        let timestamp = "2026-03-10T12:00:00Z";
        let signature = signing_key.sign(timestamp.as_bytes());
        let sig_hex = hex::encode(signature.to_bytes());

        // Verify
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

    /// The premise behind the extractor's use of strict verification, and the
    /// reason `test_a_small_order_public_key_cannot_be_signed_for` in
    /// `tests/auth.rs` asserts a rejection: these bytes are a working forgery
    /// under lenient verification, not merely malformed. The public key is the
    /// identity point and the signature is `R` = identity, `s` = 0, so the
    /// recomputed `R` is `[0]B - [k]A` = identity for any message at all — no
    /// private key involved.
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
        let mut csprng = rand_core::OsRng;
        let signing_key = SigningKey::generate(&mut csprng);
        let pk = PublicKeyHex::try_new(hex::encode(signing_key.verifying_key().as_bytes()))?;
        let client_id = derive_client_id(&pk)?;
        assert!(client_id.as_str().starts_with("ev1_"));
        Ok(())
    }

    /// A distinct signature per index, so a test can fill the cache without
    /// minting real keys — `claim` only ever compares the bytes.
    fn sig(n: u8) -> [u8; 64] {
        [n; 64]
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

    /// The check and the insert share one lock, so concurrent copies of a
    /// request cannot both be admitted. Without that, replay protection would
    /// hold only for requests that happen not to overlap.
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

    /// A sweep keeps every entry still inside its tolerance window and drops
    /// those past it.
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

        // Far enough past the window for the next sweep to be due: the first
        // signature can no longer authenticate anything, so holding it buys
        // nothing, while the second is still within its own window.
        let outside = inside + chrono::TimeDelta::seconds(SWEEP_INTERVAL_SECS);
        assert_eq!(
            cache.claim(&sig(3), outside, outside),
            SignatureClaim::Fresh
        );
        assert_eq!(cache.len(), 2, "aged-out signatures should be swept");
        Ok(())
    }
}
