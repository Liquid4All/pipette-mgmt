//! Pre-auth registration keys: an operator mints a key, a client presents it at
//! `POST /clients/register` to be auto-approved (and optionally seeded with the
//! key's tags/organization), skipping manual approval. A key is either
//! single-use (spent by the first registration) or multi-use (usable until it
//! expires or is revoked).
//!
//! Token: `preauth_{key_id}.{secret}` — the prefix names it on sight; only
//! `sha256(secret)` is stored, never the secret. The record is
//! `preauth/{key_id}.json` and is never rewritten: spending a single-use key
//! creates a sibling `preauth/{key_id}.spent` marker and then deletes the
//! record. The create is exclusive — `If-None-Match: *` on S3, `O_EXCL` on a
//! filesystem — so exactly one of any number of concurrent consumes wins,
//! whichever process or replica each arrives at. That needs only a
//! single-object conditional create, so plain S3 is enough and no move is
//! involved. Consuming a multi-use key does not touch storage, so it is
//! race-free by construction. The secret is verified before any status is
//! revealed, so the record is not an enumeration oracle.

use std::collections::BTreeSet;
use std::fmt;

use chrono::{DateTime, Utc};
use rand_core::{OsRng, RngCore};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::types::{InvalidId, PreauthKeyId};
use crate::validated::{NonEmptyTrimmedString, Tag};

/// Prefix on every token, so a bare string is recognizable at a glance as a
/// pre-auth key (in a shell, a log, a paste).
pub const TOKEN_PREFIX: &str = "preauth_";

/// A pre-auth token secret in the clear (the `{secret}` half of a token). Wraps
/// the raw value so it can't be confused with an ordinary string and never
/// leaks through `Debug`/`Display` — [`Secret::expose`] is the one, greppable
/// way to read the bytes back out.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    pub fn new(raw: impl Into<String>) -> Self {
        Self(raw.into())
    }

    /// The raw secret. Named `expose` so every disclosure site is easy to audit.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(redacted)")
    }
}

/// Whether a key may be consumed once or repeatedly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PreauthUsage {
    /// Consumable exactly once — the first successful registration spends it.
    SingleUse,
    /// Consumable any number of times until it expires or is revoked. Consuming
    /// it never mutates the record, so there is no counter to race on.
    MultiUse,
}

/// The persisted pre-auth key record (`preauth/{key_id}.json`). Never contains
/// the secret — only `secret_hash`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PreauthKey {
    pub key_id: PreauthKeyId,
    /// Hex `sha256(secret)`. The secret itself is shown once at mint and never
    /// stored.
    pub secret_hash: String,
    /// Whether the key is single- or multi-use.
    pub usage: PreauthUsage,
    #[serde(default)]
    pub expires_at: Option<DateTime<Utc>>,
    /// Tags applied to any client registering with this key.
    #[serde(default)]
    pub default_tags: BTreeSet<Tag>,
    /// Organization applied to any client registering with this key (overrides
    /// the client-supplied value when set).
    #[serde(default)]
    pub default_organization: Option<NonEmptyTrimmedString>,
    #[serde(default)]
    pub note: Option<String>,
    pub created_at: DateTime<Utc>,
}

impl PreauthKey {
    pub fn is_expired(&self, now: DateTime<Utc>) -> bool {
        self.expires_at.is_some_and(|at| at < now)
    }

    /// What a successful consume grants the registering client. A valid key
    /// always approves — that's the point of a pre-auth key — so approval is
    /// implicit; the grant only carries what to seed onto the client.
    pub fn grant(&self) -> PreauthGrant {
        PreauthGrant {
            default_tags: self.default_tags.clone(),
            default_organization: self.default_organization.clone(),
        }
    }
}

/// What a registering client is granted by a valid key. Approval is implied (a
/// valid pre-auth key always approves); these are the values seeded onto the
/// new client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreauthGrant {
    pub default_tags: BTreeSet<Tag>,
    pub default_organization: Option<NonEmptyTrimmedString>,
}

/// Why a key could not be consumed. Ordered so the secret is verified before any
/// status is revealed — a caller without the secret only ever sees `NotFound` or
/// `BadSecret`, never `Expired`. The `Display` message is deliberately coarse:
/// unknown key and bad secret both read as "invalid" so the response isn't an
/// enumeration oracle. A spent single-use key, a revoked key, and a pruned key
/// are all simply deleted, so re-use reads as `NotFound`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum PreauthRejection {
    #[error("invalid pre-auth key")]
    NotFound,
    #[error("invalid pre-auth key")]
    BadSecret,
    #[error("pre-auth key has expired")]
    Expired,
}

/// Outcome of a consume attempt. `Err` on the surrounding `Result` is reserved
/// for I/O; a well-formed rejection is `Rejected`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PreauthConsumeOutcome {
    Granted(PreauthGrant),
    Rejected(PreauthRejection),
}

/// A freshly minted key: the record to persist plus the one-time token to show.
pub struct MintedKey {
    pub token: String,
    pub key: PreauthKey,
}

/// Parameters for [`mint`].
pub struct MintParams {
    pub usage: PreauthUsage,
    pub expires_at: Option<DateTime<Utc>>,
    pub default_tags: BTreeSet<Tag>,
    pub default_organization: Option<NonEmptyTrimmedString>,
    pub note: Option<String>,
}

/// Hex `sha256` of a secret.
pub fn hash_secret(secret: &Secret) -> String {
    hex::encode(Sha256::digest(secret.expose().as_bytes()))
}

/// Constant-time equality of the presented secret's hash against the stored
/// hash — avoids leaking the stored hash byte-by-byte via comparison timing.
pub fn secret_matches(secret: &Secret, stored_hash: &str) -> bool {
    let computed = hash_secret(secret);
    let (a, b) = (computed.as_bytes(), stored_hash.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Parse a token into its `(key_id, secret)` parts. Returns `None` if it lacks
/// the `preauth_` prefix, has no `.` separator, carries an empty secret, or the
/// key-id segment is not charset-valid.
pub fn parse_token(token: &str) -> Option<(PreauthKeyId, Secret)> {
    let rest = token.trim().strip_prefix(TOKEN_PREFIX)?;
    let (id, secret) = rest.split_once('.')?;
    if secret.is_empty() {
        return None;
    }
    let key_id = PreauthKeyId::try_new(id).ok()?;
    Some((key_id, Secret::new(secret)))
}

/// Bytes of randomness in a `key_id` — just a collision-resistant lookup id
/// (64-bit), not a secret, so it stays short (16 hex chars).
const KEY_ID_BYTES: usize = 8;
/// Bytes of randomness in the secret — the actual credential, 256-bit.
const SECRET_BYTES: usize = 32;

/// Mint a new key: random `key_id` + high-entropy secret, hashed into the
/// record. `now` is threaded in so callers control the clock. The `InvalidId`
/// error is structurally unreachable (a hex `key_id` is always charset-valid)
/// but propagated rather than unwrapped.
pub fn mint(params: MintParams, now: DateTime<Utc>) -> Result<MintedKey, InvalidId> {
    let key_id = PreauthKeyId::try_new(random_hex(KEY_ID_BYTES))?;
    let secret = Secret::new(random_hex(SECRET_BYTES));
    let token = format!("{TOKEN_PREFIX}{key_id}.{}", secret.expose());
    let key = PreauthKey {
        key_id,
        secret_hash: hash_secret(&secret),
        usage: params.usage,
        expires_at: params.expires_at,
        default_tags: params.default_tags,
        default_organization: params.default_organization,
        note: params.note,
        created_at: now,
    };
    Ok(MintedKey { token, key })
}

/// Validate a presented secret against a loaded key at `now`. Secret is checked
/// first so status is never revealed to a caller who doesn't hold it.
///
/// Passing here means the key is well-formed, unexpired, and the secret matches
/// — not that it is still unspent. A single-use key is claimed by the caller,
/// which is where concurrent consumes are resolved into one winner, so a grant
/// from this function is a precondition for spending rather than permission to
/// proceed.
pub fn validate(
    key: &PreauthKey,
    secret: &Secret,
    now: DateTime<Utc>,
) -> Result<PreauthGrant, PreauthRejection> {
    if !secret_matches(secret, &key.secret_hash) {
        return Err(PreauthRejection::BadSecret);
    }
    if key.is_expired(now) {
        return Err(PreauthRejection::Expired);
    }
    Ok(key.grant())
}

/// `2 * n_bytes` hex chars from the OS CSPRNG (`OsRng`, the same source used for
/// Ed25519 keygen).
fn random_hex(n_bytes: usize) -> String {
    let mut buf = vec![0u8; n_bytes];
    OsRng.fill_bytes(&mut buf);
    hex::encode(buf)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    fn minted(usage: PreauthUsage) -> Result<MintedKey, InvalidId> {
        mint(
            MintParams {
                usage,
                expires_at: None,
                default_tags: BTreeSet::new(),
                default_organization: None,
                note: None,
            },
            Utc::now(),
        )
    }

    #[test]
    fn token_round_trips_and_hides_the_hash() -> anyhow::Result<()> {
        let m = minted(PreauthUsage::SingleUse)?;
        let (id, secret) = parse_token(&m.token).ok_or_else(|| anyhow::anyhow!("parse"))?;
        assert_eq!(id, m.key.key_id);
        assert!(secret_matches(&secret, &m.key.secret_hash));
        // The stored hash must never appear in the shown token.
        assert!(!m.token.contains(&m.key.secret_hash));
        Ok(())
    }

    #[test]
    fn secret_debug_is_redacted() -> anyhow::Result<()> {
        let secret = Secret::new("super-secret-value");
        let rendered = format!("{secret:?}");
        assert!(!rendered.contains("super-secret-value"), "{rendered}");
        assert_eq!(rendered, "Secret(redacted)");
        Ok(())
    }

    #[rstest]
    #[case("")]
    #[case("preauth_")]
    #[case("preauth_abc")]
    #[case("abc.def")]
    #[case("preauth_.secret")]
    #[case("preauth_bad/id.secret")]
    fn parse_rejects_malformed_tokens(#[case] bad: &str) {
        assert!(parse_token(bad).is_none(), "expected {bad:?} rejected");
    }

    #[test]
    fn validate_checks_secret_before_status() -> anyhow::Result<()> {
        // An expired key with the WRONG secret reports BadSecret, not Expired —
        // status is never revealed to someone without the secret.
        let mut m = minted(PreauthUsage::SingleUse)?;
        m.key.expires_at = Some(Utc::now() - chrono::Duration::seconds(1));
        assert_eq!(
            validate(&m.key, &Secret::new("wrong-secret"), Utc::now()),
            Err(PreauthRejection::BadSecret)
        );
        Ok(())
    }

    #[test]
    fn validate_accepts_valid_and_rejects_expired() -> anyhow::Result<()> {
        let m = minted(PreauthUsage::SingleUse)?;
        let (_id, secret) = parse_token(&m.token).ok_or_else(|| anyhow::anyhow!("parse"))?;

        // Valid path grants.
        assert!(validate(&m.key, &secret, Utc::now()).is_ok());

        let mut expired = m.key.clone();
        expired.expires_at = Some(Utc::now() - chrono::Duration::seconds(1));
        assert_eq!(
            validate(&expired, &secret, Utc::now()),
            Err(PreauthRejection::Expired)
        );
        Ok(())
    }

    #[test]
    fn is_expired_tracks_the_deadline() -> anyhow::Result<()> {
        let now = Utc::now();
        let mut key = minted(PreauthUsage::SingleUse)?.key;

        assert!(!key.is_expired(now), "no deadline never expires");

        key.expires_at = Some(now + chrono::Duration::seconds(1));
        assert!(!key.is_expired(now));

        key.expires_at = Some(now - chrono::Duration::seconds(1));
        assert!(key.is_expired(now));
        Ok(())
    }
}
