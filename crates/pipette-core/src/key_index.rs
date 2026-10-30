//! `public-key-index/{digest}`: the claim that keeps one keypair to one
//! principal. Client and user key ids share the digest, so without it one key
//! could act as a device or an operator per request, bypassing the partition.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use sha2::{Digest, Sha256};

use pipette_plan_types::{ClientId, IdError, PublicKeyHex, UserId, UserKeyId};

use crate::types::id_from_valid;

/// The hex `sha256` of an Ed25519 public key's raw bytes — the key of a claim.
///
/// Bare, with no prefix, because it is the one value both kinds compute
/// identically: [`DeriveFromKey`](crate::types::DeriveFromKey) renders it under
/// `ev1_` for a [`ClientId`] and under `userkey_` for a [`UserKeyId`].
#[nutype::nutype(
    validate(with = validate_public_key_digest, error = IdError),
    derive(
        Debug,
        Clone,
        PartialEq,
        Eq,
        PartialOrd,
        Ord,
        Hash,
        AsRef,
        Display,
        Into,
        Serialize,
        Deserialize,
        TryFrom,
    ),
)]
pub struct PublicKeyDigest(String);

/// Lowercase hex of one fixed width. Narrower than the id charsets on purpose:
/// this value is a filename and is never operator-chosen.
fn validate_public_key_digest(s: &str) -> Result<(), IdError> {
    const HEX_WIDTH: usize = 64;

    if s.len() != HEX_WIDTH {
        return Err(IdError::Width {
            field: "public_key_digest",
            expected: "64 hex characters",
            got: s.len(),
        });
    }
    if let Some(ch) = s
        .chars()
        .find(|c| !c.is_ascii_hexdigit() || c.is_ascii_uppercase())
    {
        return Err(IdError::InvalidChar {
            field: "public_key_digest",
            ch,
            allowed: "[0-9a-f]",
        });
    }
    Ok(())
}

impl PublicKeyDigest {
    pub fn as_str(&self) -> &str {
        self.as_ref()
    }
}

/// The digest of `public_key` — the same value both id derivations hash.
pub fn digest_of(public_key: &PublicKeyHex) -> PublicKeyDigest {
    id_from_valid(
        hex::encode(Sha256::digest(public_key.to_bytes())),
        "a sha256 rendered as hex is always 64 lowercase hex characters",
    )
}

/// The digest a client id carries, recovered by slicing rather than hashing —
/// so a claim for a client already on record needs no public key re-read.
///
/// `None` for an id predating the derivation, which has no keypair to claim.
pub fn digest_of_client_id(client_id: &ClientId) -> Option<PublicKeyDigest> {
    PublicKeyDigest::try_new(client_id.as_str().strip_prefix(ClientId::PREFIX)?).ok()
}

/// The digest a user key id carries. The counterpart of
/// [`digest_of_client_id`], one prefix over.
pub fn digest_of_key_id(key_id: &UserKeyId) -> Option<PublicKeyDigest> {
    PublicKeyDigest::try_new(key_id.as_str().strip_prefix(UserKeyId::PREFIX)?).ok()
}

/// Who holds a claim: the body of `public-key-index/{digest}`, serialized as
/// the bare, self-describing id. A user rather than one of its keys: a key id
/// would point the claim at an object revocation deletes.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum KeyHolderPrincipalId {
    Client(ClientId),
    User(UserId),
}

impl KeyHolderPrincipalId {
    pub fn as_str(&self) -> &str {
        match self {
            Self::Client(id) => id.as_str(),
            Self::User(id) => id.as_str(),
        }
    }
}

impl fmt::Display for KeyHolderPrincipalId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Dispatch on the prefix, so the kind comes off the value with no store
/// lookup. A value carrying neither is refused rather than guessed at: reading
/// an unprefixed id as a client would hand a claim to the wrong half of the
/// partition.
impl FromStr for KeyHolderPrincipalId {
    type Err = IdError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s.starts_with(UserId::PREFIX) {
            return Ok(Self::User(UserId::try_new(s)?));
        }
        if s.starts_with(ClientId::PREFIX) {
            return Ok(Self::Client(ClientId::try_new(s)?));
        }
        Err(IdError::MissingPrefix {
            field: "principal_id",
            prefix: "ev1_ or user_",
        })
    }
}

impl Serialize for KeyHolderPrincipalId {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for KeyHolderPrincipalId {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(d)?;
        raw.parse().map_err(serde::de::Error::custom)
    }
}

/// What a claim attempt did. [`KeyClaim::Taken`] carries the holder so a
/// registration can tell its own retry from another principal's keypair.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyClaim {
    Claimed,
    /// The keypair was already claimed, and nothing was written.
    Taken(KeyHolderPrincipalId),
}

impl KeyClaim {
    /// Whether `holder` already owns this claim — the idempotent retry both
    /// registration paths allow, and the question they ask before refusing.
    pub fn is_held_by(&self, holder: &KeyHolderPrincipalId) -> bool {
        matches!(self, Self::Taken(current) if current == holder)
    }
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;
    use crate::types::DeriveFromKey;

    fn public_key(byte: u8) -> anyhow::Result<PublicKeyHex> {
        Ok(PublicKeyHex::try_new(hex::encode(
            [byte; PublicKeyHex::BYTE_LEN],
        ))?)
    }

    /// The whole keyspace rests on this: were the two derivations to drift, one
    /// keypair would address two claims and the partition would be bypassable
    /// again.
    #[test]
    fn one_keypair_yields_one_digest_under_both_prefixes() -> anyhow::Result<()> {
        let key = public_key(0x42)?;
        let direct = digest_of(&key);

        let client_id = ClientId::derive(&key);
        let key_id = UserKeyId::derive(&key);

        assert_eq!(digest_of_client_id(&client_id), Some(direct.clone()));
        assert_eq!(digest_of_key_id(&key_id), Some(direct));
        Ok(())
    }

    /// Slicing an id must agree with hashing the key: the release path slices
    /// and registration hashes, and a disagreement writes two claims for one
    /// keypair.
    #[rstest]
    #[case(0x00)]
    #[case(0x01)]
    #[case(0x7f)]
    #[case(0xfe)]
    #[case(0xff)]
    fn slicing_an_id_agrees_with_hashing_the_key(#[case] byte: u8) -> anyhow::Result<()> {
        let key = public_key(byte)?;
        assert_eq!(
            digest_of_client_id(&ClientId::derive(&key)),
            Some(digest_of(&key))
        );
        Ok(())
    }

    /// A principal id round-trips through the claim body's string form.
    #[rstest]
    #[case::client("ev1_abc123", true)]
    #[case::user("user_0195c3e2-a4f3-7b8c-9d1e-2f3a4b5c6d7e", true)]
    #[case::unprefixed("plain", false)]
    #[case::wrong_prefix("job-1", false)]
    #[case::empty("", false)]
    #[case::client_bad_charset("ev1_a/b", false)]
    #[case::user_bad_charset("user_a b", false)]
    fn a_principal_id_parses_by_prefix(
        #[case] raw: &str,
        #[case] accepted: bool,
    ) -> anyhow::Result<()> {
        let parsed = raw.parse::<KeyHolderPrincipalId>();
        assert_eq!(parsed.is_ok(), accepted, "{raw:?}");
        if let Ok(id) = parsed {
            assert_eq!(id.as_str(), raw);
            assert_eq!(serde_json::to_value(&id)?, serde_json::json!(raw));
        }
        Ok(())
    }

    /// A digest is a filename, so it is held to hex of one fixed width.
    #[rstest]
    #[case::exact(&"a".repeat(64), true)]
    #[case::uppercase(&"A".repeat(64), false)]
    #[case::too_short(&"a".repeat(63), false)]
    #[case::too_long(&"a".repeat(65), false)]
    #[case::non_hex(&"g".repeat(64), false)]
    #[case::traversal("../../etc/passwd", false)]
    fn a_digest_is_lowercase_hex_of_one_width(#[case] raw: &str, #[case] accepted: bool) {
        assert_eq!(PublicKeyDigest::try_new(raw).is_ok(), accepted, "{raw:?}");
    }

    /// The retry question a registration asks before refusing.
    #[test]
    fn a_claim_reports_its_own_holder() -> anyhow::Result<()> {
        let holder: KeyHolderPrincipalId = "ev1_abc".parse()?;
        let other: KeyHolderPrincipalId = "user_0195c3e2-a4f3-7b8c-9d1e-2f3a4b5c6d7e".parse()?;

        assert!(KeyClaim::Taken(holder.clone()).is_held_by(&holder));
        assert!(!KeyClaim::Taken(other).is_held_by(&holder));
        assert!(!KeyClaim::Claimed.is_held_by(&holder));
        Ok(())
    }
}
