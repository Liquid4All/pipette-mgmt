//! User pre-auth keys: minted against an existing account, presented to
//! register a machine's public key to it. [`crate::client_preauth`]'s design
//! over its own keyspace; the key, not the caller, names the account.

use std::fmt;

use chrono::{DateTime, Utc};
use getrandom::SysRng;
use getrandom::rand_core::TryRng;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use pipette_plan_types::{NonEmptyTrimmedString, UserId, UserPreauthKeyId, UserPreauthUsage};

use crate::types::MintError;

/// Prefix on every token this build mints, so a bare string is recognizable at
/// a glance. No legacy spelling: this keyspace never minted under another.
pub const TOKEN_PREFIX: &str = "user_preauth_";

/// A user pre-auth token secret in the clear (the `{secret}` half of a token);
/// [`UserPreauthSecret::expose`] is the one greppable way to read it.
///
/// The short derive list is the point: no `Display`, `Debug`, `Serialize` or
/// `Deref`, so it cannot leak into a log line or record by default. `Debug` is
/// hand-written, redacted, so a surrounding struct can still derive it.
#[nutype::nutype(derive(Clone, PartialEq, Eq, AsRef))]
pub struct UserPreauthSecret(String);

impl UserPreauthSecret {
    /// The raw secret. Named `expose` so every disclosure site is easy to audit.
    pub fn expose(&self) -> &str {
        self.as_ref()
    }

    /// Hex `sha256` of the secret — what the record holds in its place.
    pub fn hash(&self) -> String {
        hex::encode(Sha256::digest(self.expose().as_bytes()))
    }

    /// Constant-time equality of this secret's hash against a stored one —
    /// avoids leaking the stored hash byte-by-byte via comparison timing.
    pub fn matches(&self, stored_hash: &str) -> bool {
        let computed = self.hash();
        let (a, b) = (computed.as_bytes(), stored_hash.as_bytes());
        if a.len() != b.len() {
            return false;
        }
        a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
    }
}

impl fmt::Debug for UserPreauthSecret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("UserPreauthSecret(redacted)")
    }
}

/// A pre-auth token in the clear: `user_preauth_{key_id}.{secret}`, as
/// [`mint`] hands it out once and a user's machine presents it at registration.
/// Redacted like [`UserPreauthSecret`], since it carries the secret verbatim;
/// `Deserialize` only, as nothing serializes one back out.
#[nutype::nutype(derive(Clone, PartialEq, Eq, AsRef, Deserialize))]
pub struct UserPreauthToken(String);

impl UserPreauthToken {
    /// The raw token. Named `expose` so every disclosure site is easy to audit,
    /// matching [`UserPreauthSecret::expose`].
    pub fn expose(&self) -> &str {
        self.as_ref()
    }

    /// Split into the `(key_id, secret)` parts a lookup needs, or `None` when
    /// malformed, a client's token included. A method, so parsing is not an
    /// `expose` site and the other keyspace's token cannot be handed to it.
    pub fn parse_parts(&self) -> Option<(UserPreauthKeyId, UserPreauthSecret)> {
        let rest = self.expose().trim().strip_prefix(TOKEN_PREFIX)?;
        let (id, secret) = rest.split_once('.')?;
        if secret.is_empty() {
            return None;
        }
        let key_id = UserPreauthKeyId::try_new(id).ok()?;
        Some((key_id, UserPreauthSecret::new(secret)))
    }
}

impl fmt::Debug for UserPreauthToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("UserPreauthToken(redacted)")
    }
}

/// The persisted key record (`user-preauth/{key_id}.json`), holding only the
/// secret's hash. No permission template: the account is granted directly, and
/// an older record's `default_permissions` is ignored.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserPreauthKey {
    pub key_id: UserPreauthKeyId,
    /// The account this key admits a machine to: the key, not the register
    /// body, names it, so a holder cannot redirect it. The key can outlive the
    /// account, so a consume does not promise `users/{user_id}.json` exists.
    pub user_id: UserId,
    /// Hex `sha256(secret)`. The secret itself is shown once at mint and never
    /// stored.
    pub secret_hash: String,
    pub usage: UserPreauthUsage,
    #[serde(default)]
    pub expires_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub note: Option<String>,
    pub created_at: DateTime<Utc>,
    /// Who minted it over HTTP, whose authority registration checks again;
    /// `None`, and not written, for a host-minted key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub minted_by: Option<UserId>,
}

impl UserPreauthKey {
    pub fn is_expired(&self, now: DateTime<Utc>) -> bool {
        self.expires_at.is_some_and(|at| at < now)
    }

    /// The key as the API lists it at `now`, without its secret: destructured,
    /// so the hash can never ride along into an answer.
    pub fn view(self, now: DateTime<Utc>) -> pipette_plan_types::UserPreauthKeyView {
        let expired = self.is_expired(now);
        let UserPreauthKey {
            key_id,
            user_id,
            secret_hash: _,
            usage,
            expires_at,
            note,
            created_at,
            minted_by,
        } = self;
        pipette_plan_types::UserPreauthKeyView {
            key_id,
            user_id,
            usage,
            expires_at,
            expired,
            // An older host-minted record may hold a blank or padded note.
            note: note.and_then(|note| NonEmptyTrimmedString::try_new(note).ok()),
            created_at,
            minted_by,
        }
    }

    /// What a successful consume grants; admission is implicit.
    pub fn grant(&self) -> UserPreauthGrant {
        UserPreauthGrant {
            user_id: self.user_id.clone(),
            minted_by: self.minted_by.clone(),
        }
    }
}

/// What a valid key grants a registering machine: the account and the minter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserPreauthGrant {
    pub user_id: UserId,
    /// See [`UserPreauthKey::minted_by`].
    pub minted_by: Option<UserId>,
}

/// Why a key could not be consumed. The secret is checked before any status, so
/// a caller without it sees only `NotFound` or `BadSecret`, and `Display` reads
/// both as "invalid": no enumeration oracle. Spent, revoked and pruned keys are
/// deleted, so re-use reads as `NotFound`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum UserPreauthRejectionError {
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
pub enum UserPreauthConsumeOutcome {
    Granted(UserPreauthGrant),
    Rejected(UserPreauthRejectionError),
}

/// A freshly minted key: the record to persist plus the one-time token to show.
/// `Debug` is safe to derive: the token redacts itself.
#[derive(Debug)]
pub struct UserMintedKey {
    pub token: UserPreauthToken,
    pub key: UserPreauthKey,
}

/// Parameters for [`mint`].
pub struct UserPreauthParams {
    /// The account the key admits a machine to.
    pub user_id: UserId,
    pub usage: UserPreauthUsage,
    pub expires_at: Option<DateTime<Utc>>,
    pub note: Option<NonEmptyTrimmedString>,
}

/// Bytes of randomness in a `key_id` — just a collision-resistant lookup id
/// (64-bit), not a secret, so it stays short (16 hex chars).
const KEY_ID_BYTES: usize = 8;
/// Bytes of randomness in the secret — the actual credential, 256-bit.
const SECRET_BYTES: usize = 32;

/// Mint a new key against `params.user_id`: random `key_id` + high-entropy
/// secret, hashed into the record. `MintError::IdError` is unreachable (a hex
/// `key_id` is always valid) but propagated rather than unwrapped.
pub fn mint(params: UserPreauthParams, now: DateTime<Utc>) -> Result<UserMintedKey, MintError> {
    let key_id = UserPreauthKeyId::try_new(random_hex(KEY_ID_BYTES)?)?;
    let secret = UserPreauthSecret::new(random_hex(SECRET_BYTES)?);
    let token = UserPreauthToken::new(format!("{TOKEN_PREFIX}{key_id}.{}", secret.expose()));
    let key = UserPreauthKey {
        key_id,
        user_id: params.user_id,
        secret_hash: secret.hash(),
        usage: params.usage,
        expires_at: params.expires_at,
        note: params.note.map(Into::into),
        created_at: now,
        minted_by: None,
    };
    Ok(UserMintedKey { token, key })
}

/// Validate a presented secret against a loaded key at `now`, the secret first
/// so status is never revealed without it. Not proof the key is unspent: the
/// caller's claim of a single-use key picks the one winner.
pub fn validate(
    key: &UserPreauthKey,
    secret: &UserPreauthSecret,
    now: DateTime<Utc>,
) -> Result<UserPreauthGrant, UserPreauthRejectionError> {
    if !secret.matches(&key.secret_hash) {
        return Err(UserPreauthRejectionError::BadSecret);
    }
    if key.is_expired(now) {
        return Err(UserPreauthRejectionError::Expired);
    }
    Ok(key.grant())
}

/// `2 * n_bytes` hex chars from the OS CSPRNG (`SysRng`, the same source used for
/// Ed25519 keygen).
fn random_hex(n_bytes: usize) -> Result<String, getrandom::Error> {
    let mut buf = vec![0u8; n_bytes];
    SysRng.try_fill_bytes(&mut buf)?;
    Ok(hex::encode(buf))
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;
    use crate::types::Mint;

    fn minted(usage: UserPreauthUsage) -> Result<UserMintedKey, MintError> {
        mint(
            UserPreauthParams {
                user_id: UserId::mint(),
                usage,
                expires_at: None,
                note: None,
            },
            Utc::now(),
        )
    }

    #[test]
    fn token_round_trips_and_hides_the_hash() -> anyhow::Result<()> {
        let m = minted(UserPreauthUsage::SingleUse)?;
        let (id, secret) = m
            .token
            .parse_parts()
            .ok_or_else(|| anyhow::anyhow!("parse"))?;
        assert_eq!(id, m.key.key_id);
        assert!(secret.matches(&m.key.secret_hash));
        // The stored hash must never appear in the shown token.
        assert!(!m.token.expose().contains(&m.key.secret_hash));
        Ok(())
    }

    #[test]
    fn secret_debug_is_redacted() -> anyhow::Result<()> {
        let secret = UserPreauthSecret::new("super-secret-value");
        let rendered = format!("{secret:?}");
        assert!(!rendered.contains("super-secret-value"), "{rendered}");
        assert_eq!(rendered, "UserPreauthSecret(redacted)");
        Ok(())
    }

    #[test]
    fn token_debug_is_redacted() -> anyhow::Result<()> {
        let token = UserPreauthToken::new("user_preauth_abc123.super-secret-value");
        let rendered = format!("{token:?}");
        assert!(!rendered.contains("super-secret-value"), "{rendered}");
        assert!(!rendered.contains("user_preauth_abc123"), "{rendered}");
        assert_eq!(rendered, "UserPreauthToken(redacted)");
        Ok(())
    }

    /// A struct holding a token derives `Debug` and inherits the redaction,
    /// asserted against a real minted credential.
    #[test]
    fn a_surrounding_struct_inherits_the_token_redaction() -> anyhow::Result<()> {
        let m = minted(UserPreauthUsage::SingleUse)?;
        let raw = m.token.expose().to_string();
        let (_id, secret) = m
            .token
            .parse_parts()
            .ok_or_else(|| anyhow::anyhow!("parse"))?;

        let rendered = format!("{m:?}");
        assert!(!rendered.contains(&raw), "{rendered}");
        assert!(!rendered.contains(secret.expose()), "{rendered}");
        assert!(
            rendered.contains("UserPreauthToken(redacted)"),
            "{rendered}"
        );
        Ok(())
    }

    #[rstest]
    #[case("")]
    #[case("user_preauth_")]
    #[case("user_preauth_abc")]
    #[case("abc.def")]
    #[case("user_preauth_.secret")]
    #[case("user_preauth_bad/id.secret")]
    fn parse_rejects_malformed_tokens(#[case] bad: &str) {
        assert!(
            UserPreauthToken::new(bad).parse_parts().is_none(),
            "expected {bad:?} rejected"
        );
    }

    /// A client's token, current or legacy spelling, is malformed here, so it
    /// can never name a record in `user-preauth/`.
    #[rstest]
    #[case(crate::client_preauth::TOKEN_PREFIX)]
    #[case(crate::client_preauth::LEGACY_TOKEN_PREFIX)]
    fn parse_rejects_a_clients_token(#[case] prefix: &str) {
        assert!(
            UserPreauthToken::new(format!("{prefix}abc123.secret"))
                .parse_parts()
                .is_none()
        );
    }

    #[test]
    fn validate_checks_secret_before_status() -> anyhow::Result<()> {
        // An expired key with the WRONG secret reports BadSecret, not Expired —
        // status is never revealed to someone without the secret.
        let mut m = minted(UserPreauthUsage::SingleUse)?;
        m.key.expires_at = Some(Utc::now() - chrono::Duration::seconds(1));
        assert_eq!(
            validate(&m.key, &UserPreauthSecret::new("wrong-secret"), Utc::now()),
            Err(UserPreauthRejectionError::BadSecret)
        );
        Ok(())
    }

    #[test]
    fn validate_accepts_valid_and_rejects_expired() -> anyhow::Result<()> {
        let m = minted(UserPreauthUsage::SingleUse)?;
        let (_id, secret) = m
            .token
            .parse_parts()
            .ok_or_else(|| anyhow::anyhow!("parse"))?;

        // Valid path grants, and names the account the key was minted against.
        let grant = validate(&m.key, &secret, Utc::now())?;
        assert_eq!(grant.user_id, m.key.user_id);

        let mut expired = m.key.clone();
        expired.expires_at = Some(Utc::now() - chrono::Duration::seconds(1));
        assert_eq!(
            validate(&expired, &secret, Utc::now()),
            Err(UserPreauthRejectionError::Expired)
        );
        Ok(())
    }

    #[rstest]
    #[case::blank("  ", None)]
    #[case::padded(" laptop ", Some("laptop"))]
    fn a_stored_note_reads_trimmed_and_blank_as_none(
        #[case] stored: &str,
        #[case] shown: Option<&str>,
    ) -> anyhow::Result<()> {
        let mut key = minted(UserPreauthUsage::SingleUse)?.key;
        key.note = Some(stored.to_owned());
        let view = key.view(Utc::now());
        assert_eq!(view.note.as_ref().map(NonEmptyTrimmedString::as_str), shown);
        Ok(())
    }

    /// A record an older binary minted with a permission template still reads,
    /// so registering with it, and listing it, does not fail.
    #[test]
    fn a_key_record_with_a_template_still_reads() -> anyhow::Result<()> {
        let key = minted(UserPreauthUsage::SingleUse)?.key;
        let mut record = serde_json::to_value(&key)?;
        record["default_permissions"] = serde_json::json!(["submit_plans", "operate_centrifuge"]);

        let read: UserPreauthKey = serde_json::from_value(record)?;
        assert_eq!(read.grant(), key.grant());
        Ok(())
    }

    /// A host-minted record is written without `minted_by`, byte for byte what
    /// an older binary wrote, and one an administrator minted keeps its minter.
    #[test]
    fn the_minter_is_written_only_when_there_is_one() -> anyhow::Result<()> {
        let mut key = minted(UserPreauthUsage::SingleUse)?.key;
        assert!(!serde_json::to_string(&key)?.contains("minted_by"));

        let minter = UserId::mint();
        key.minted_by = Some(minter.clone());
        let round_tripped: UserPreauthKey = serde_json::from_slice(&serde_json::to_vec(&key)?)?;
        assert_eq!(round_tripped.grant().minted_by, Some(minter));
        Ok(())
    }

    /// The account is part of the record, so it survives a round trip through
    /// storage — a consume reads the account off the key, never off the caller.
    #[test]
    fn the_record_round_trips_through_json() -> anyhow::Result<()> {
        let key = minted(UserPreauthUsage::MultiUse)?.key;
        let round_tripped: UserPreauthKey = serde_json::from_slice(&serde_json::to_vec(&key)?)?;
        assert_eq!(round_tripped.user_id, key.user_id);
        assert_eq!(round_tripped.key_id, key.key_id);
        assert_eq!(round_tripped.usage, UserPreauthUsage::MultiUse);
        assert_eq!(round_tripped.secret_hash, key.secret_hash);
        Ok(())
    }

    #[test]
    fn is_expired_tracks_the_deadline() -> anyhow::Result<()> {
        let now = Utc::now();
        let mut key = minted(UserPreauthUsage::SingleUse)?.key;

        assert!(!key.is_expired(now), "no deadline never expires");

        key.expires_at = Some(now + chrono::Duration::seconds(1));
        assert!(!key.is_expired(now));

        key.expires_at = Some(now - chrono::Duration::seconds(1));
        assert!(key.is_expired(now));
        Ok(())
    }
}
