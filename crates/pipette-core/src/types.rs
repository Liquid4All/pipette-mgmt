use std::fmt;

use pipette_plan_types::{
    AuditEventId, CampaignId, ClientId, IdError, OrgId, PublicKeyHex, UserId, UserKeyId,
    VaultResourceId,
};

/// Mints a prefixed id: the type's prefix and a UUIDv7, so each keyspace lists
/// in creation order with no index of its own.
pub trait Mint: Sized {
    fn mint() -> Self;
}

impl Mint for OrgId {
    fn mint() -> Self {
        mint_prefixed(Self::PREFIX)
    }
}

impl Mint for UserId {
    fn mint() -> Self {
        mint_prefixed(Self::PREFIX)
    }
}

impl Mint for VaultResourceId {
    fn mint() -> Self {
        mint_prefixed(Self::PREFIX)
    }
}

impl Mint for CampaignId {
    fn mint() -> Self {
        mint_prefixed(Self::PREFIX)
    }
}

impl Mint for AuditEventId {
    fn mint() -> Self {
        mint_prefixed(Self::PREFIX)
    }
}

fn mint_prefixed<T>(prefix: &str) -> T
where
    T: TryFrom<String>,
    <T as TryFrom<String>>::Error: fmt::Debug,
{
    id_from_valid(
        format!("{prefix}{}", uuid::Uuid::now_v7()),
        "a hyphenated UUID under the type's prefix satisfies its charset and prefix",
    )
}

/// Derives a key holder's id: the type's prefix and the hex `sha256` of the key.
/// A [`ClientId`] and a [`UserKeyId`] of one keypair share the digest, which
/// `public-key-index/{digest}` relies on.
pub trait DeriveFromKey: Sized {
    fn derive(public_key: &PublicKeyHex) -> Self;
}

impl DeriveFromKey for ClientId {
    fn derive(public_key: &PublicKeyHex) -> Self {
        derive_prefixed(Self::PREFIX, public_key)
    }
}

impl DeriveFromKey for UserKeyId {
    fn derive(public_key: &PublicKeyHex) -> Self {
        derive_prefixed(Self::PREFIX, public_key)
    }
}

fn derive_prefixed<T>(prefix: &str, public_key: &PublicKeyHex) -> T
where
    T: TryFrom<String>,
    <T as TryFrom<String>>::Error: fmt::Debug,
{
    id_from_valid(
        format!("{prefix}{}", crate::key_index::digest_of(public_key)),
        "a hex digest under the type's prefix satisfies its charset and prefix",
    )
}

/// Parses an id read back from an object key or filename, refusing a segment
/// the parse rewrites (the plan-types ids trim): its id would name another key.
pub fn id_from_key<Id, E>(segment: &str, parse: fn(String) -> Result<Id, E>) -> Result<Id, IdError>
where
    Id: fmt::Display,
    IdError: From<E>,
{
    let id = parse(segment.to_owned())?;
    if id.to_string() != segment {
        return Err(IdError::NotVerbatim {
            segment: segment.to_owned(),
        });
    }
    Ok(id)
}

#[derive(Debug, thiserror::Error)]
pub enum MintError {
    #[error(transparent)]
    IdError(#[from] IdError),
    #[error("OS random source failed: {0}")]
    Rng(#[from] getrandom::Error),
}

/// Build an id from a string this crate has already proven valid for it, as
/// when widening one validated id into a superset charset or assembling a fixed
/// prefix and a UUID.
///
/// The single sanctioned place to assert an id's validity: panicking constructs
/// are denied crate-wide, and these conversions are total, so an error arm at
/// each caller would lie. Ids from outside input use `try_new` instead.
#[expect(
    clippy::expect_used,
    reason = "the one sanctioned home for infallible id conversions, per the doc above"
)]
pub fn id_from_valid<T>(value: String, invariant: &'static str) -> T
where
    T: TryFrom<String>,
    <T as TryFrom<String>>::Error: fmt::Debug,
{
    T::try_from(value).expect(invariant)
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    #[rstest]
    #[case::exact("ev1_a", true)]
    #[case::leading_space(" ev1_a", false)]
    #[case::trailing_space("ev1_a ", false)]
    #[case::bad_charset("ev1.a", false)]
    fn a_key_segment_names_a_client_only_verbatim(#[case] segment: &str, #[case] ok: bool) {
        assert_eq!(
            id_from_key(segment, ClientId::try_new).is_ok(),
            ok,
            "{segment:?}"
        );
    }

    /// A minted campaign id carries the prefix `try_new` requires.
    #[test]
    fn a_minted_campaign_id_round_trips() {
        let id = CampaignId::mint();
        assert!(
            CampaignId::try_new(id.as_str().to_string()).is_ok(),
            "{id:?}"
        );
    }
}
