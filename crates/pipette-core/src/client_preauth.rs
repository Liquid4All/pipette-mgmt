//! Client pre-auth registration keys: a client presents one at registration to
//! be auto-approved and seeded with the key's defaults. A single-use spend is an
//! exclusive create of a `.spent` marker, so one concurrent consume wins.

use std::collections::BTreeSet;
use std::fmt;

use chrono::{DateTime, Utc};
use getrandom::SysRng;
use getrandom::rand_core::TryRng;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use pipette_plan_types::{
    ClientOwner, ClientPermission, ClientPreauthKeyId, ClientPreauthUsage, NonEmptyTrimmedString,
    OrgId, Tag, UserId,
};

use crate::client_permission::{ClientPermissionSet, StoredClientPermission};
use crate::types::MintError;

/// Prefix on every token this build mints, so a bare string is recognizable at
/// a glance as a client pre-auth key (in a shell, a log, a paste).
pub const TOKEN_PREFIX: &str = "client_preauth_";

/// Prefix an earlier build minted, still accepted. `client_preauth_` does not
/// start with `preauth_`, so at most one of them strips.
pub const LEGACY_TOKEN_PREFIX: &str = "preauth_";

/// A client pre-auth token secret in the clear (the `{secret}` half of a
/// token); [`ClientPreauthSecret::expose`] is the one greppable way to read it.
///
/// The short derive list is the point: no `Display`, `Debug`, `Serialize` or
/// `Deref`, so it cannot leak into a log line or record by default. `Debug` is
/// hand-written, redacted, so a surrounding struct can still derive it.
#[nutype::nutype(derive(Clone, PartialEq, Eq, AsRef))]
pub struct ClientPreauthSecret(String);

impl ClientPreauthSecret {
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

impl fmt::Debug for ClientPreauthSecret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ClientPreauthSecret(redacted)")
    }
}

/// A pre-auth token in the clear: `client_preauth_{key_id}.{secret}`, as
/// [`mint`] hands it out once and a client presents it at registration.
/// Redacted like [`ClientPreauthSecret`], since it carries the secret verbatim;
/// `Deserialize` only, as nothing serializes one back out.
#[nutype::nutype(derive(Clone, PartialEq, Eq, AsRef, Deserialize))]
pub struct ClientPreauthToken(String);

impl ClientPreauthToken {
    /// The raw token. Named `expose` so every disclosure site is easy to audit,
    /// matching [`ClientPreauthSecret::expose`].
    pub fn expose(&self) -> &str {
        self.as_ref()
    }

    /// Split into the `(key_id, secret)` parts a lookup needs, or `None` when
    /// malformed. A method, so parsing is not an `expose` site and the other
    /// keyspace's token cannot be handed to it.
    pub fn parse_parts(&self) -> Option<(ClientPreauthKeyId, ClientPreauthSecret)> {
        let token = self.expose().trim();
        let rest = token
            .strip_prefix(TOKEN_PREFIX)
            .or_else(|| token.strip_prefix(LEGACY_TOKEN_PREFIX))?;
        let (id, secret) = rest.split_once('.')?;
        if secret.is_empty() {
            return None;
        }
        let key_id = ClientPreauthKeyId::try_new(id).ok()?;
        Some((key_id, ClientPreauthSecret::new(secret)))
    }
}

impl fmt::Debug for ClientPreauthToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ClientPreauthToken(redacted)")
    }
}

/// The persisted pre-auth key record (`client-preauth/{key_id}.json`). Never
/// contains the secret — only `secret_hash`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientPreauthKey {
    pub key_id: ClientPreauthKeyId,
    /// Hex `sha256(secret)`. The secret itself is shown once at mint and never
    /// stored.
    pub secret_hash: String,
    pub usage: ClientPreauthUsage,
    #[serde(default)]
    pub expires_at: Option<DateTime<Utc>>,
    /// Tags applied to any client registering with this key.
    #[serde(default)]
    pub default_tags: BTreeSet<Tag>,
    /// Organization applied to any client registering with this key (overrides
    /// the client-supplied value when set).
    #[serde(default)]
    pub default_organization: Option<NonEmptyTrimmedString>,
    /// Owner given to any client registering with this key; minting the key is
    /// the operator vouching for it. Omitted when absent, so an ownerless key
    /// keeps the legacy shape.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_owner: Option<ClientOwner>,
    /// Permissions granted to any client registering with this key. `None` is
    /// no template (the registration default applies); `Some(empty)` grants
    /// nothing. Stored members, so an older binary can consume a key whose
    /// template names a permission it does not know.
    #[serde(default)]
    pub default_permissions: Option<ClientPermissionSet>,
    #[serde(default)]
    pub note: Option<String>,
    pub created_at: DateTime<Utc>,
    /// Who minted it over HTTP, whose authority registration checks again;
    /// `None`, and not written, for a host-minted key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub minted_by: Option<UserId>,
}

impl ClientPreauthKey {
    pub fn is_expired(&self, now: DateTime<Utc>) -> bool {
        self.expires_at.is_some_and(|at| at < now)
    }

    /// The key as `org_id`'s listing shows it at `now`, without its secret:
    /// destructured, so the hash can never ride along into an answer.
    pub fn view(
        self,
        org_id: OrgId,
        now: DateTime<Utc>,
    ) -> pipette_plan_types::ClientPreauthKeyView {
        let expired = self.is_expired(now);
        let ClientPreauthKey {
            key_id,
            secret_hash: _,
            usage,
            expires_at,
            default_tags,
            default_organization,
            default_owner: _,
            default_permissions,
            note,
            created_at,
            minted_by,
        } = self;
        pipette_plan_types::ClientPreauthKeyView {
            key_id,
            org_id,
            usage,
            expires_at,
            expired,
            default_tags,
            default_organization,
            default_permissions: default_permissions.as_ref().map(Into::into),
            // An older host-minted record may hold a blank or padded note.
            note: note.and_then(|note| NonEmptyTrimmedString::try_new(note).ok()),
            created_at,
            minted_by,
        }
    }

    /// What a successful consume seeds onto the registering client; approval
    /// is implicit.
    pub fn grant(&self) -> ClientPreauthGrant {
        ClientPreauthGrant {
            default_tags: self.default_tags.clone(),
            default_organization: self.default_organization.clone(),
            default_owner: self.default_owner.clone(),
            default_permissions: self.default_permissions.clone(),
        }
    }
}

/// What a valid key seeds onto a new client; a valid key always approves.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientPreauthGrant {
    pub default_tags: BTreeSet<Tag>,
    pub default_organization: Option<NonEmptyTrimmedString>,
    /// The key's owner. Not yet checked against the rosters: the key may have
    /// outlived what it names, which registration has to refuse.
    pub default_owner: Option<ClientOwner>,
    /// The key's permission template, or `None` when it carries none. See
    /// [`ClientPreauthKey::default_permissions`].
    pub default_permissions: Option<ClientPermissionSet>,
}

/// Why a key could not be consumed. The secret is checked before any status, so
/// a caller without it sees only `NotFound` or `BadSecret`, and `Display` reads
/// both as "invalid": no enumeration oracle. Spent, revoked and pruned keys are
/// deleted, so re-use reads as `NotFound`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ClientPreauthRejectionError {
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
pub enum ClientPreauthConsumeOutcome {
    Granted(ClientPreauthGrant),
    Rejected(ClientPreauthRejectionError),
}

/// A freshly minted key: the record to persist plus the one-time token to show.
/// `Debug` is safe to derive: the token redacts itself.
#[derive(Debug)]
pub struct ClientMintedKey {
    pub token: ClientPreauthToken,
    pub key: ClientPreauthKey,
}

/// Parameters for [`mint`].
pub struct ClientPreauthParams {
    pub usage: ClientPreauthUsage,
    pub expires_at: Option<DateTime<Utc>>,
    pub default_tags: BTreeSet<Tag>,
    pub default_organization: Option<NonEmptyTrimmedString>,
    /// Resolved against the rosters by the caller, so a key names only an owner
    /// that existed when it was minted.
    pub default_owner: Option<ClientOwner>,
    /// Minting is input, so it takes only [`ClientPermission`]s; [`mint`]
    /// widens this to the stored form.
    pub default_permissions: Option<BTreeSet<ClientPermission>>,
    pub note: Option<NonEmptyTrimmedString>,
}

/// Bytes of randomness in a `key_id` — just a collision-resistant lookup id
/// (64-bit), not a secret, so it stays short (16 hex chars).
const KEY_ID_BYTES: usize = 8;
/// Bytes of randomness in the secret — the actual credential, 256-bit.
const SECRET_BYTES: usize = 32;

/// Mint a new key: random `key_id` + high-entropy secret, hashed into the
/// record. `MintError::IdError` is unreachable (a hex `key_id` is always valid)
/// but propagated rather than unwrapped.
pub fn mint(params: ClientPreauthParams, now: DateTime<Utc>) -> Result<ClientMintedKey, MintError> {
    let key_id = ClientPreauthKeyId::try_new(random_hex(KEY_ID_BYTES)?)?;
    let secret = ClientPreauthSecret::new(random_hex(SECRET_BYTES)?);
    let token = ClientPreauthToken::new(format!("{TOKEN_PREFIX}{key_id}.{}", secret.expose()));
    let key = ClientPreauthKey {
        key_id,
        secret_hash: secret.hash(),
        usage: params.usage,
        expires_at: params.expires_at,
        default_tags: params.default_tags,
        default_organization: params.default_organization,
        default_owner: params.default_owner,
        default_permissions: params
            .default_permissions
            .map(|set| set.into_iter().map(StoredClientPermission::Known).collect()),
        note: params.note.map(Into::into),
        created_at: now,
        minted_by: None,
    };
    Ok(ClientMintedKey { token, key })
}

/// Validate a presented secret against a loaded key at `now`, the secret first
/// so status is never revealed without it. Not proof the key is unspent: the
/// caller's claim of a single-use key picks the one winner.
pub fn validate(
    key: &ClientPreauthKey,
    secret: &ClientPreauthSecret,
    now: DateTime<Utc>,
) -> Result<ClientPreauthGrant, ClientPreauthRejectionError> {
    if !secret.matches(&key.secret_hash) {
        return Err(ClientPreauthRejectionError::BadSecret);
    }
    if key.is_expired(now) {
        return Err(ClientPreauthRejectionError::Expired);
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

    use pipette_plan_types::OrgId;

    use super::*;
    use crate::types::Mint;

    fn minted(usage: ClientPreauthUsage) -> Result<ClientMintedKey, MintError> {
        mint(
            ClientPreauthParams {
                usage,
                expires_at: None,
                default_tags: BTreeSet::new(),
                default_organization: None,
                default_owner: None,
                default_permissions: None,
                note: None,
            },
            Utc::now(),
        )
    }

    #[test]
    fn token_round_trips_and_hides_the_hash() -> anyhow::Result<()> {
        let m = minted(ClientPreauthUsage::SingleUse)?;
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
        let secret = ClientPreauthSecret::new("super-secret-value");
        let rendered = format!("{secret:?}");
        assert!(!rendered.contains("super-secret-value"), "{rendered}");
        assert_eq!(rendered, "ClientPreauthSecret(redacted)");
        Ok(())
    }

    #[test]
    fn token_debug_is_redacted() -> anyhow::Result<()> {
        let token = ClientPreauthToken::new("client_preauth_abc123.super-secret-value");
        let rendered = format!("{token:?}");
        assert!(!rendered.contains("super-secret-value"), "{rendered}");
        assert!(!rendered.contains("client_preauth_abc123"), "{rendered}");
        assert_eq!(rendered, "ClientPreauthToken(redacted)");
        Ok(())
    }

    /// A struct holding a token derives `Debug` and inherits the redaction,
    /// asserted against a real minted credential.
    #[test]
    fn a_surrounding_struct_inherits_the_token_redaction() -> anyhow::Result<()> {
        let m = minted(ClientPreauthUsage::SingleUse)?;
        let raw = m.token.expose().to_string();
        let (_id, secret) = m
            .token
            .parse_parts()
            .ok_or_else(|| anyhow::anyhow!("parse"))?;

        let rendered = format!("{m:?}");
        assert!(!rendered.contains(&raw), "{rendered}");
        assert!(!rendered.contains(secret.expose()), "{rendered}");
        assert!(
            rendered.contains("ClientPreauthToken(redacted)"),
            "{rendered}"
        );
        Ok(())
    }

    #[rstest]
    #[case("")]
    #[case("client_preauth_")]
    #[case("client_preauth_abc")]
    #[case("abc.def")]
    #[case("client_preauth_.secret")]
    #[case("client_preauth_bad/id.secret")]
    #[case("preauth_")]
    #[case("preauth_.secret")]
    fn parse_rejects_malformed_tokens(#[case] bad: &str) {
        assert!(
            ClientPreauthToken::new(bad).parse_parts().is_none(),
            "expected {bad:?} rejected"
        );
    }

    /// A user's token fails the grammar here rather than being looked up in a
    /// keyspace it was never written to.
    #[test]
    fn parse_rejects_a_users_token() {
        let prefix = crate::user_preauth::TOKEN_PREFIX;
        assert!(
            ClientPreauthToken::new(format!("{prefix}abc123.secret"))
                .parse_parts()
                .is_none()
        );
    }

    /// A token minted before the rename parses to the same `key_id` and secret.
    #[test]
    fn parse_accepts_the_legacy_prefix() -> anyhow::Result<()> {
        let m = minted(ClientPreauthUsage::SingleUse)?;
        let body = m
            .token
            .expose()
            .strip_prefix(TOKEN_PREFIX)
            .ok_or_else(|| anyhow::anyhow!("mint must use the current prefix"))?;

        let (id, secret) = ClientPreauthToken::new(format!("{LEGACY_TOKEN_PREFIX}{body}"))
            .parse_parts()
            .ok_or_else(|| anyhow::anyhow!("the legacy prefix must parse"))?;
        assert_eq!(id, m.key.key_id);
        assert!(secret.matches(&m.key.secret_hash));
        Ok(())
    }

    #[test]
    fn validate_checks_secret_before_status() -> anyhow::Result<()> {
        // An expired key with the WRONG secret reports BadSecret, not Expired —
        // status is never revealed to someone without the secret.
        let mut m = minted(ClientPreauthUsage::SingleUse)?;
        m.key.expires_at = Some(Utc::now() - chrono::Duration::seconds(1));
        assert_eq!(
            validate(
                &m.key,
                &ClientPreauthSecret::new("wrong-secret"),
                Utc::now()
            ),
            Err(ClientPreauthRejectionError::BadSecret)
        );
        Ok(())
    }

    #[test]
    fn validate_accepts_valid_and_rejects_expired() -> anyhow::Result<()> {
        let m = minted(ClientPreauthUsage::SingleUse)?;
        let (_id, secret) = m
            .token
            .parse_parts()
            .ok_or_else(|| anyhow::anyhow!("parse"))?;

        // Valid path grants.
        assert!(validate(&m.key, &secret, Utc::now()).is_ok());

        let mut expired = m.key.clone();
        expired.expires_at = Some(Utc::now() - chrono::Duration::seconds(1));
        assert_eq!(
            validate(&expired, &secret, Utc::now()),
            Err(ClientPreauthRejectionError::Expired)
        );
        Ok(())
    }

    /// A key minted by a newer binary, naming a permission this one does not
    /// know, still reads back; otherwise a valid registration fails with `500`.
    #[test]
    fn a_key_template_reads_back_an_unknown_name() -> anyhow::Result<()> {
        let mut key = minted(ClientPreauthUsage::SingleUse)?.key;
        key.default_permissions = Some(
            [
                StoredClientPermission::Known(ClientPermission::SubmitResults),
                StoredClientPermission::classify(pipette_plan_types::PermissionName::try_new(
                    "operate_centrifuge",
                )?),
            ]
            .into_iter()
            .collect(),
        );

        let round_tripped: ClientPreauthKey = serde_json::from_slice(&serde_json::to_vec(&key)?)?;
        let names: Vec<&str> = round_tripped
            .default_permissions
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("template must survive"))?
            .iter()
            .map(StoredClientPermission::wire_name)
            .collect();
        assert_eq!(names, ["operate_centrifuge", "submit_results"]);
        Ok(())
    }

    /// Every key minted before owners existed has no `default_owner`, so an
    /// ownerless key must write that same shape and read it back as `None`.
    /// The owner a key does carry reaches the grant.
    #[test]
    fn an_owner_is_optional_on_the_record_and_reaches_the_grant() -> anyhow::Result<()> {
        let mut key = minted(ClientPreauthUsage::SingleUse)?.key;
        let written = serde_json::to_value(&key)?;
        assert!(written.get("default_owner").is_none(), "{written}");
        assert_eq!(serde_json::from_value::<ClientPreauthKey>(written)?, key);

        let owner = ClientOwner::Org {
            org_id: OrgId::mint(),
        };
        key.default_owner = Some(owner.clone());
        let round_tripped: ClientPreauthKey = serde_json::from_slice(&serde_json::to_vec(&key)?)?;
        assert_eq!(round_tripped.grant().default_owner, Some(owner));
        Ok(())
    }

    #[rstest]
    #[case::blank("  ", None)]
    #[case::padded(" laptop ", Some("laptop"))]
    fn a_stored_note_reads_trimmed_and_blank_as_none(
        #[case] stored: &str,
        #[case] shown: Option<&str>,
    ) -> anyhow::Result<()> {
        let mut key = minted(ClientPreauthUsage::SingleUse)?.key;
        key.note = Some(stored.to_owned());
        let view = key.view(OrgId::mint(), Utc::now());
        assert_eq!(view.note.as_ref().map(NonEmptyTrimmedString::as_str), shown);
        Ok(())
    }

    #[test]
    fn is_expired_tracks_the_deadline() -> anyhow::Result<()> {
        let now = Utc::now();
        let mut key = minted(ClientPreauthUsage::SingleUse)?.key;

        assert!(!key.is_expired(now), "no deadline never expires");

        key.expires_at = Some(now + chrono::Duration::seconds(1));
        assert!(!key.is_expired(now));

        key.expires_at = Some(now - chrono::Duration::seconds(1));
        assert!(key.is_expired(now));
        Ok(())
    }
}
