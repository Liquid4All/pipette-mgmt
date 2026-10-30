//! Principal ids (organizations, accounts, devices, keys, vault resources) and
//! the names and addresses attached to them: org slugs, tags, contact emails.

use nutype::nutype;
use serde::{Deserialize, Serialize};

use crate::text::ascii_lowercase;

/// The id the management server assigns a client at registration.
///
/// Trimmed: it rides in `X-Client-Id` and the signed payload, where a padded
/// copy signs as a client the server does not know. No prefix is required, as
/// ids registered before [`Self::PREFIX`] still parse.
#[nutype(
    sanitize(trim),
    validate(with = check_client_id, error = PrincipalIdError),
    derive(
        Debug,
        Clone,
        Display,
        FromStr,
        PartialEq,
        Eq,
        PartialOrd,
        Ord,
        Hash,
        Serialize,
        Deserialize,
        AsRef,
        Into,
        TryFrom
    )
)]
pub struct ClientId(String);

impl ClientId {
    /// The prefix a derived id carries: `ev1_` and the hex `sha256` of the
    /// client's public key.
    pub const PREFIX: &'static str = "ev1_";

    pub fn as_str(&self) -> &str {
        self.as_ref()
    }
}

/// The id of a user account: `user_` followed by a hyphenated UUIDv7, minted by
/// the management server (a user holds many keys, so none to derive from).
///
/// Here, not in `pipette-mgmt-client`: a plan manifest records its filer.
/// Not a [`ClientId`]: both ride in `X-Client-Id`, and one type would let a
/// client id compile into a user credential.
#[nutype(
    sanitize(trim),
    validate(with = check_user_id, error = PrincipalIdError),
    derive(
        Debug,
        Clone,
        Display,
        FromStr,
        PartialEq,
        Eq,
        PartialOrd,
        Ord,
        Hash,
        Serialize,
        Deserialize,
        AsRef,
        Into,
        TryFrom
    )
)]
pub struct UserId(String);

impl UserId {
    /// The prefix every user id carries.
    pub const PREFIX: &'static str = "user_";

    pub fn as_str(&self) -> &str {
        self.as_ref()
    }
}

/// The id of one registered public key: `userkey_` followed by the hex
/// `sha256` of the key bytes.
///
/// Sent in `X-User-Key-Id` and signed, so a multi-key principal verifies with
/// one store read. Not `KeyId`, which would also answer to a pre-auth key; the
/// wire keeps the server's `key_id` spelling.
#[nutype(
    sanitize(trim),
    validate(with = check_user_key_id, error = PrincipalIdError),
    derive(
        Debug,
        Clone,
        Display,
        FromStr,
        PartialEq,
        Eq,
        PartialOrd,
        Ord,
        Hash,
        Serialize,
        Deserialize,
        AsRef,
        Into,
        TryFrom
    )
)]
pub struct UserKeyId(String);

impl UserKeyId {
    /// The prefix every user key id carries: the only thing telling it from a
    /// [`ClientId`] of the same key.
    pub const PREFIX: &'static str = "userkey_";

    pub fn as_str(&self) -> &str {
        self.as_ref()
    }
}

/// The id of an organization: `org_` followed by a hyphenated UUIDv7, minted
/// by the management server. Always the minted id, never the renameable
/// handle: it reaches a model descriptor, where a rename would split one model
/// into two.
#[nutype(
    sanitize(trim),
    validate(with = check_org_id, error = PrincipalIdError),
    derive(
        Debug,
        Clone,
        Display,
        FromStr,
        PartialEq,
        Eq,
        PartialOrd,
        Ord,
        Hash,
        Serialize,
        Deserialize,
        AsRef,
        Into,
        TryFrom
    )
)]
pub struct OrgId(String);

impl OrgId {
    /// See [`UserId::PREFIX`].
    pub const PREFIX: &'static str = "org_";

    pub fn as_str(&self) -> &str {
        self.as_ref()
    }
}

/// The id of a vault resource: `vault_` followed by a hyphenated UUIDv7,
/// minted by the management server. What a [`VaultPath`](crate::VaultPath) addresses.
#[nutype(
    sanitize(trim),
    validate(with = check_vault_resource_id, error = PrincipalIdError),
    derive(
        Debug,
        Clone,
        Display,
        FromStr,
        PartialEq,
        Eq,
        PartialOrd,
        Ord,
        Hash,
        Serialize,
        Deserialize,
        AsRef,
        Into,
        TryFrom
    )
)]
pub struct VaultResourceId(String);

impl VaultResourceId {
    /// See [`UserId::PREFIX`].
    pub const PREFIX: &'static str = "vault_";

    pub fn as_str(&self) -> &str {
        self.as_ref()
    }
}

/// Who a vault fetch is for: an organization or an account, as one bare id
/// string told apart by its prefix.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(try_from = "String", into = "String")]
pub enum VaultPrincipalId {
    Org(OrgId),
    User(UserId),
}

impl std::str::FromStr for VaultPrincipalId {
    type Err = PrincipalIdError;

    /// Dispatched on the prefix, so a value of neither kind is refused as such.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let s = s.trim();
        if s.starts_with(OrgId::PREFIX) {
            return Ok(Self::Org(OrgId::try_new(s)?));
        }
        if s.starts_with(UserId::PREFIX) {
            return Ok(Self::User(UserId::try_new(s)?));
        }
        Err(PrincipalIdError::UnknownKind {
            kind: "principal id",
        })
    }
}

impl TryFrom<String> for VaultPrincipalId {
    type Error = PrincipalIdError;

    fn try_from(s: String) -> Result<Self, Self::Error> {
        s.parse()
    }
}

impl std::fmt::Display for VaultPrincipalId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Org(org_id) => org_id.fmt(f),
            Self::User(user_id) => user_id.fmt(f),
        }
    }
}

impl From<VaultPrincipalId> for String {
    fn from(principal: VaultPrincipalId) -> Self {
        principal.to_string()
    }
}

fn check_org_id(value: &str) -> Result<(), PrincipalIdError> {
    check_prefixed_id(value, "organization id", OrgId::PREFIX)
}

fn check_vault_resource_id(value: &str) -> Result<(), PrincipalIdError> {
    check_prefixed_id(value, "vault resource id", VaultResourceId::PREFIX)
}

fn check_user_id(value: &str) -> Result<(), PrincipalIdError> {
    check_prefixed_id(value, "user id", UserId::PREFIX)
}

fn check_user_key_id(value: &str) -> Result<(), PrincipalIdError> {
    check_prefixed_id(value, "user key id", UserKeyId::PREFIX)
}

fn check_client_id(value: &str) -> Result<(), PrincipalIdError> {
    check_id_charset(value, "client id")
}

/// Empty, then charset, then prefix: the most specific failure first.
fn check_prefixed_id(
    value: &str,
    kind: &'static str,
    prefix: &'static str,
) -> Result<(), PrincipalIdError> {
    check_id_charset(value, kind)?;
    if !value.starts_with(prefix) {
        return Err(PrincipalIdError::MissingPrefix { kind, prefix });
    }
    Ok(())
}

/// Non-empty and `[A-Za-z0-9_-]`: a newline in the newline-delimited signed
/// payload would forge a field boundary, and an id must be one key segment.
fn check_id_charset(value: &str, kind: &'static str) -> Result<(), PrincipalIdError> {
    if value.is_empty() {
        return Err(PrincipalIdError::Empty { kind });
    }
    match value
        .chars()
        .find(|c| !c.is_ascii_alphanumeric() && !matches!(c, '_' | '-'))
    {
        Some(ch) => Err(PrincipalIdError::Charset { kind, ch }),
        None => Ok(()),
    }
}

/// Why a value cannot name a principal or one of its keys. `kind` names which
/// id was read, since the kinds differ only by prefix.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum PrincipalIdError {
    #[error("{kind} must not be empty")]
    Empty { kind: &'static str },

    /// A character outside `[A-Za-z0-9_-]`.
    #[error("{kind} contains '{}'; allowed: [A-Za-z0-9_-]", .ch.escape_default())]
    Charset { kind: &'static str, ch: char },

    /// Most often an id of the other kind.
    #[error("{kind} must start with `{prefix}`")]
    MissingPrefix {
        kind: &'static str,
        prefix: &'static str,
    },

    /// A [`VaultPrincipalId`] that starts with neither prefix.
    #[error("{kind} must start with `org_` or `user_`")]
    UnknownKind { kind: &'static str },
}

/// An organization's handle, such as `acme` or `liquid-edge`: the stable name
/// an operator types, unlike the free-text display name.
///
/// [`Tag`](crate::Tag)'s invariant, so one handle is one key segment, and never
/// [`OrgId`]'s prefix, which keeps the id and handle namespaces disjoint.
#[nutype::nutype(
    sanitize(trim, with = ascii_lowercase),
    validate(with = validate_org_slug, error = OrgSlugError),
    derive(
        Debug,
        Clone,
        PartialEq,
        Eq,
        PartialOrd,
        Ord,
        Hash,
        AsRef,
        Deref,
        Display,
        Into,
        Serialize,
        Deserialize,
        FromStr,
    ),
)]
pub struct OrgSlug(String);

/// Construction error for [`OrgSlug`], carrying the offending value.
#[derive(Debug, thiserror::Error)]
pub enum OrgSlugError {
    #[error("organization slug must not be empty")]
    Empty,
    #[error("organization slug {slug:?} is too long ({len} chars, max {max})")]
    TooLong {
        slug: String,
        len: usize,
        max: usize,
    },
    #[error(
        "organization slug {slug:?} contains invalid character {ch:?}; allowed: [a-z0-9_-] (no '/')"
    )]
    InvalidChar { slug: String, ch: char },
    #[error(
        "organization slug {slug:?} may not start with {prefix:?}, which is how an organization \
         id is spelled"
    )]
    IdPrefixed { slug: String, prefix: &'static str },
}

/// Runs on the trimmed, lowercased value. `OrgRef` tells id from handle by the
/// prefix, so a prefixed handle would make a record reachable only by its id.
fn validate_org_slug(s: &str) -> Result<(), OrgSlugError> {
    if s.is_empty() {
        return Err(OrgSlugError::Empty);
    }
    if s.starts_with(OrgId::PREFIX) {
        return Err(OrgSlugError::IdPrefixed {
            slug: s.to_string(),
            prefix: OrgId::PREFIX,
        });
    }
    if s.len() > OrgSlug::MAX_LEN {
        return Err(OrgSlugError::TooLong {
            slug: s.to_string(),
            len: s.len(),
            max: OrgSlug::MAX_LEN,
        });
    }
    if let Some(ch) = s
        .chars()
        .find(|c| !c.is_ascii_lowercase() && !c.is_ascii_digit() && *c != '_' && *c != '-')
    {
        return Err(OrgSlugError::InvalidChar {
            ch,
            slug: s.to_string(),
        });
    }
    Ok(())
}

impl OrgSlug {
    pub const MAX_LEN: usize = 64;

    pub fn as_str(&self) -> &str {
        self.as_ref()
    }
}

/// A hex-encoded Ed25519 public key (32 bytes, 64 hex chars), validated on
/// construction and `Deserialize`.
#[nutype::nutype(
    sanitize(trim),
    validate(with = validate_public_key_hex, error = PublicKeyHexError),
    derive(
        Debug,
        Clone,
        PartialEq,
        Eq,
        Hash,
        AsRef,
        Deref,
        Display,
        Into,
        Serialize,
        Deserialize,
    ),
)]
pub struct PublicKeyHex(String);

/// Construction error for [`PublicKeyHex`].
#[derive(Debug, thiserror::Error)]
pub enum PublicKeyHexError {
    #[error("public_key must be valid hex")]
    InvalidHex,
    #[error("public_key must be 32 bytes (64 hex chars), got {got}")]
    WrongByteLength { got: usize },
}

fn validate_public_key_hex(s: &str) -> Result<(), PublicKeyHexError> {
    let bytes = hex::decode(s).map_err(|_| PublicKeyHexError::InvalidHex)?;
    if bytes.len() != PublicKeyHex::BYTE_LEN {
        return Err(PublicKeyHexError::WrongByteLength { got: bytes.len() });
    }
    Ok(())
}

impl PublicKeyHex {
    /// Raw byte length of an Ed25519 public key.
    pub const BYTE_LEN: usize = 32;

    pub fn as_str(&self) -> &str {
        self.as_ref()
    }

    /// The decoded key, always [`BYTE_LEN`](Self::BYTE_LEN) bytes.
    ///
    /// Cannot fail: the value was hex-decoded at construction. Not a fallback
    /// to an empty `Vec`: callers digest it and would mint the wrong key's id.
    #[expect(
        clippy::expect_used,
        reason = "validated as hex at construction, per the doc above"
    )]
    pub fn to_bytes(&self) -> Vec<u8> {
        hex::decode(self.as_str())
            .expect("PublicKeyHex invariant: the value validated as hex at construction")
    }
}

/// A trimmed email address, parsed by the `email_address` crate (RFC 5322) and
/// narrowed: no quoted local part, no display name, no domain literal, at least
/// two domain labels. A shape check only, not proof the address gets mail.
#[nutype::nutype(
    sanitize(trim),
    validate(with = validate_contact_email, error = ContactEmailError),
    derive(
        Debug,
        Clone,
        PartialEq,
        Eq,
        Hash,
        AsRef,
        Deref,
        Display,
        Into,
        Serialize,
        Deserialize,
    ),
)]
pub struct ContactEmail(String);

/// Construction error for [`ContactEmail`]; the failure reason is not public.
#[derive(Debug, thiserror::Error)]
#[error("contact_email is not a valid email address")]
pub struct ContactEmailError;

/// Runs on the trimmed value. A quoted local part admits nearly anything, so
/// `"../../etc/passwd"@example.com` is valid RFC 5322 and reads as a path; the
/// unquoted form forbids `..`, and `/` is refused on top of it.
fn validate_contact_email(s: &str) -> Result<(), ContactEmailError> {
    if s.len() > ContactEmail::MAX_LEN {
        return Err(ContactEmailError);
    }
    let parsed = email_address::EmailAddress::parse_with_options(s, ContactEmail::parse_options())
        .map_err(|_| ContactEmailError)?;
    let local = parsed.local_part();
    if local.starts_with('"') || local.contains('/') {
        return Err(ContactEmailError);
    }
    Ok(())
}

impl ContactEmail {
    /// RFC 5321 §4.5.3.1.3 caps a forward-path at 256 octets including
    /// the angle brackets, so 254 octets is the longest address that
    /// can legally cross the wire. The `email_address` crate bounds
    /// the local part and the domain separately but not their sum, so
    /// we cap the whole thing here.
    ///
    /// Octets, not characters — as are the crate's own bounds. A
    /// Cyrillic local part is two octets per character and so gets
    /// half the room an ASCII one does; see
    /// `contact_email_length_caps_count_bytes_not_characters`.
    pub const MAX_LEN: usize = 254;

    pub fn as_str(&self) -> &str {
        self.as_ref()
    }

    /// The crate's defaults suit a mail header: a display name would stay in
    /// the stored value, a domain literal names a host, `user@localhost` is a
    /// typo.
    fn parse_options() -> email_address::Options {
        email_address::Options::default()
            .without_display_text()
            .without_domain_literal()
            .with_required_tld()
    }
}

/// A flat client tag such as `team-mobile`, set by an operator, never a client.
///
/// Trimmed, lowercased, then `[a-z0-9_-]`: flat on purpose, so the tag index
/// stays a two-level `{tag}/{client_id}` tree.
#[nutype::nutype(
    sanitize(trim, with = ascii_lowercase),
    validate(with = validate_tag, error = TagError),
    derive(
        Debug,
        Clone,
        PartialEq,
        Eq,
        PartialOrd,
        Ord,
        Hash,
        AsRef,
        Deref,
        Display,
        Into,
        Serialize,
        Deserialize,
    ),
)]
pub struct Tag(String);

/// Construction error for [`Tag`], carrying the offending value.
#[derive(Debug, thiserror::Error)]
pub enum TagError {
    #[error("tag must not be empty")]
    Empty,
    #[error("tag {tag:?} is too long ({len} chars, max {max})")]
    TooLong { tag: String, len: usize, max: usize },
    #[error("tag {tag:?} contains invalid character {ch:?}; allowed: [a-z0-9_-] (no '/')")]
    InvalidChar { tag: String, ch: char },
}

/// Runs on the trimmed, lowercased value.
fn validate_tag(s: &str) -> Result<(), TagError> {
    if s.is_empty() {
        return Err(TagError::Empty);
    }
    if s.len() > Tag::MAX_LEN {
        return Err(TagError::TooLong {
            tag: s.to_string(),
            len: s.len(),
            max: Tag::MAX_LEN,
        });
    }
    if let Some(ch) = s
        .chars()
        .find(|c| !c.is_ascii_lowercase() && !c.is_ascii_digit() && *c != '_' && *c != '-')
    {
        return Err(TagError::InvalidChar {
            ch,
            tag: s.to_string(),
        });
    }
    Ok(())
}

impl Tag {
    pub const MAX_LEN: usize = 64;

    pub fn as_str(&self) -> &str {
        self.as_ref()
    }
}

#[cfg(test)]
mod tests {
    use anyhow::Context;
    use rstest::rstest;
    use serde_json::json;

    use super::*;

    /// A value without the prefix is refused, a client id most of all:
    /// admitting one would let a device sign as an operator.
    #[rstest::rstest]
    #[case::minted("user_0192f0c1-8b7e-7c3a-9d55-1f2e3d4c5b6a", true)]
    #[case::prefixed("user_anything", true)]
    #[case::padded("  user_a3f8  ", true)]
    #[case::a_client_id("ev1_a3f8c2", false)]
    #[case::a_user_key_id("userkey_a3f8c2", false)]
    #[case::bare("operator", false)]
    #[case::empty("", false)]
    #[case::inner_space("user_a3 f8", false)]
    #[case::inner_newline("user_a3\nf8", false)]
    fn a_user_id_requires_its_prefix_and_charset(#[case] raw: &str, #[case] accepted: bool) {
        assert_eq!(UserId::try_new(raw).is_ok(), accepted, "{raw:?}");
    }

    /// A refused char is escaped even when printable, so a look-alike cannot
    /// pass for ASCII.
    #[rstest::rstest]
    #[case::user_charset(
        UserId::try_new("user_a3 f8").err(),
        "user id contains ' '; allowed: [A-Za-z0-9_-]"
    )]
    #[case::user_blank(UserId::try_new("   ").err(), "user id must not be empty")]
    #[case::user_key_blank(UserKeyId::try_new("   ").err(), "user key id must not be empty")]
    #[case::client_charset(
        ClientId::try_new("a.b").err(),
        "client id contains '.'; allowed: [A-Za-z0-9_-]"
    )]
    #[case::client_look_alike(
        ClientId::try_new("b\u{430}").err(),
        "client id contains '\\u{430}'; allowed: [A-Za-z0-9_-]"
    )]
    #[case::client_blank(ClientId::try_new("  ").err(), "client id must not be empty")]
    // One error type serves every id, so `kind` says which field to edit.
    #[case::user_key_prefix(
        UserKeyId::try_new("user_a3f8").err(),
        "user key id must start with `userkey_`"
    )]
    #[case::user_prefix(
        UserId::try_new("userkey_a3f8").err(),
        "user id must start with `user_`"
    )]
    #[case::org_prefix(
        OrgId::try_new("user_a3f8").err(),
        "organization id must start with `org_`"
    )]
    #[case::vault_resource_prefix(
        VaultResourceId::try_new("org_a3f8").err(),
        "vault resource id must start with `vault_`"
    )]
    #[case::principal_of_neither_kind(
        "acme".parse::<VaultPrincipalId>().err(),
        "principal id must start with `org_` or `user_`"
    )]
    #[case::principal_of_a_known_kind(
        "user_has space".parse::<VaultPrincipalId>().err(),
        "user id contains ' '; allowed: [A-Za-z0-9_-]"
    )]
    fn a_refused_id_names_the_shape(
        #[case] refusal: Option<PrincipalIdError>,
        #[case] rendered: &str,
    ) {
        assert_eq!(refusal.map(|e| e.to_string()).as_deref(), Some(rendered));
    }

    /// Deserialize validates like `try_new`, trim included, so a blank value
    /// read back from disk or the wire is refused rather than kept.
    #[rstest::rstest]
    #[case::empty("\"\"")]
    #[case::blank("\"   \"")]
    fn a_blank_id_is_refused_on_deserialize(#[case] json: &str) {
        let refusal = serde_json::from_str::<ClientId>(json)
            .err()
            .map(|e| e.to_string());
        assert!(
            refusal
                .as_deref()
                .is_some_and(|m| m.contains("client id must not be empty")),
            "{json}: {refusal:?}"
        );
    }

    /// A value without the prefix is refused, a user id most of all: in the key
    /// slot the server would resolve neither.
    #[rstest::rstest]
    #[case::prefixed("userkey_a3f8c2", true)]
    #[case::padded("  userkey_a3f8c2  ", true)]
    #[case::bare_key("key_a3f8c2", false)]
    #[case::a_user_id("user_a3f8c2", false)]
    #[case::a_client_id("ev1_a3f8c2", false)]
    #[case::a_preauth_token("preauth_a3f8c2.secret", false)]
    #[case::empty("", false)]
    #[case::path_segment("userkey_a3f8/../../etc", false)]
    #[case::inner_newline("userkey_a3\nf8", false)]
    fn a_user_key_id_requires_its_prefix_and_charset(#[case] raw: &str, #[case] accepted: bool) {
        assert_eq!(UserKeyId::try_new(raw).is_ok(), accepted, "{raw:?}");
    }

    /// `.` delimits the names that embed a client id. No prefix is required:
    /// ids registered before `ev1_` are on record.
    #[rstest::rstest]
    #[case::derived("ev1_0a1b2c", true)]
    #[case::legacy("macbook-pro-2", true)]
    #[case::padded("  ev1_0a1b2c  ", true)]
    #[case::empty("", false)]
    #[case::colon("ev1:abcd", false)]
    #[case::slash("a/b", false)]
    #[case::dot("a.b", false)]
    #[case::traversal("../etc", false)]
    #[case::inner_space("has space", false)]
    fn a_client_id_requires_its_charset(#[case] raw: &str, #[case] accepted: bool) {
        assert_eq!(ClientId::try_new(raw).is_ok(), accepted, "{raw:?}");
    }

    #[test]
    fn as_str_is_the_trimmed_id() -> Result<(), Box<dyn std::error::Error>> {
        assert_eq!(UserId::try_new(" user_a3f8 ")?.as_str(), "user_a3f8");
        assert_eq!(
            UserKeyId::try_new(" userkey_a3f8 ")?.as_str(),
            "userkey_a3f8"
        );
        assert_eq!(OrgId::try_new(" org_a3f8 ")?.as_str(), "org_a3f8");
        assert_eq!(
            VaultResourceId::try_new(" vault_a3f8 ")?.as_str(),
            "vault_a3f8"
        );
        assert_eq!(ClientId::try_new(" ev1_a3f8 ")?.as_str(), "ev1_a3f8");
        Ok(())
    }

    #[test]
    fn a_user_id_serializes_as_its_bare_value() -> Result<(), Box<dyn std::error::Error>> {
        let id = UserId::try_new("user_0192f0c1")?;
        assert_eq!(serde_json::to_string(&id)?, "\"user_0192f0c1\"");
        assert_eq!(serde_json::from_str::<UserId>("\"user_0192f0c1\"")?, id);
        Ok(())
    }

    #[rstest::rstest]
    #[case::minted("org_01936d7e-0f4b-7c1e-9a2b-3c4d5e6f7a8b", true)]
    #[case::padded("  org_a3f8  ", true)]
    #[case::a_user_id("user_a3f8c2", false)]
    #[case::the_handle("acme", false)]
    #[case::empty("", false)]
    #[case::upper_prefix("ORG_upper", false)]
    #[case::path_segment("org_has/slash", false)]
    fn an_org_id_requires_its_prefix_and_charset(#[case] raw: &str, #[case] accepted: bool) {
        assert_eq!(OrgId::try_new(raw).is_ok(), accepted, "{raw:?}");
    }

    #[rstest::rstest]
    #[case::minted("vault_0195c3e2-a4f3-7b8c-9d1e-2f3a4b5c6d7e", true)]
    #[case::an_org_id("org_a3f8c2", false)]
    #[case::bare("0195c3e2", false)]
    #[case::inner_slash("vault_a/b", false)]
    fn a_vault_resource_id_requires_its_prefix_and_charset(
        #[case] raw: &str,
        #[case] accepted: bool,
    ) {
        assert_eq!(VaultResourceId::try_new(raw).is_ok(), accepted, "{raw:?}");
    }

    /// Each prefix parses to its own kind; a client id is neither.
    #[rstest::rstest]
    #[case::org("org_01936d", true)]
    #[case::user("user_01936d", true)]
    #[case::padded(" org_01936d ", true)]
    #[case::handle("acme", false)]
    #[case::client("ev1_a3f8", false)]
    #[case::empty("", false)]
    #[case::inner_space("org_has space", false)]
    fn a_principal_id_accepts_exactly_the_two_kinds(#[case] raw: &str, #[case] accepted: bool) {
        assert_eq!(raw.parse::<VaultPrincipalId>().is_ok(), accepted, "{raw:?}");
    }

    #[test]
    fn a_principal_id_routes_each_prefix_to_its_own_kind() -> Result<(), PrincipalIdError> {
        assert!(matches!(
            "org_01936d".parse::<VaultPrincipalId>()?,
            VaultPrincipalId::Org(_)
        ));
        assert!(matches!(
            "user_01936d".parse::<VaultPrincipalId>()?,
            VaultPrincipalId::User(_)
        ));
        Ok(())
    }

    /// One bare string on the wire, both directions.
    #[rstest::rstest]
    #[case("org_01936d")]
    #[case("user_01936d")]
    fn a_principal_id_round_trips_as_a_bare_string(
        #[case] raw: &str,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let principal: VaultPrincipalId = raw.parse()?;
        let json = serde_json::to_string(&principal)?;
        assert_eq!(json, format!("\"{raw}\""));
        assert_eq!(serde_json::from_str::<VaultPrincipalId>(&json)?, principal);
        Ok(())
    }

    #[test]
    fn public_key_hex_accepts_valid_64_char_hex() -> anyhow::Result<()> {
        let pk = "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789";
        let parsed: PublicKeyHex = serde_json::from_value(json!(pk))?;
        assert_eq!(parsed.as_str(), pk);
        Ok(())
    }

    #[test]
    fn public_key_hex_trims_padded_input() -> anyhow::Result<()> {
        let pk = "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789";
        let parsed: PublicKeyHex = serde_json::from_value(json!(format!("  {pk}\n")))?;
        assert_eq!(parsed.as_str(), pk);
        Ok(())
    }

    #[test]
    fn public_key_hex_rejects_invalid_hex() -> anyhow::Result<()> {
        let err = serde_json::from_value::<PublicKeyHex>(json!("zz"))
            .err()
            .context("non-hex rejected")?;
        assert!(err.to_string().contains("valid hex"));
        Ok(())
    }

    #[test]
    fn public_key_hex_rejects_wrong_byte_length() -> anyhow::Result<()> {
        let err = serde_json::from_value::<PublicKeyHex>(json!("abcdef"))
            .err()
            .context("short hex rejected")?;
        assert!(err.to_string().contains("32 bytes"));
        Ok(())
    }

    #[test]
    fn contact_email_accepts_plain_address() -> anyhow::Result<()> {
        let e: ContactEmail = serde_json::from_value(json!("user@example.com"))?;
        assert_eq!(e.as_str(), "user@example.com");
        Ok(())
    }

    #[test]
    fn contact_email_accepts_plus_addressing() -> anyhow::Result<()> {
        let e: ContactEmail = serde_json::from_value(json!("user+tag@example.com"))?;
        assert_eq!(e.as_str(), "user+tag@example.com");
        let e: ContactEmail = serde_json::from_value(json!("x+y@example.com"))?;
        assert_eq!(e.as_str(), "x+y@example.com");
        Ok(())
    }

    #[test]
    fn contact_email_accepts_dots_and_subdomains() -> anyhow::Result<()> {
        let e: ContactEmail = serde_json::from_value(json!("first.last@sub.example.co.uk"))?;
        assert_eq!(e.as_str(), "first.last@sub.example.co.uk");
        Ok(())
    }

    #[test]
    fn contact_email_trims_padded_input() -> anyhow::Result<()> {
        let e: ContactEmail = serde_json::from_value(json!("  user@example.com\n"))?;
        assert_eq!(e.as_str(), "user@example.com");
        Ok(())
    }

    /// Spot-checks only: the grammar belongs to the `email_address` crate.
    #[rstest]
    #[case("", "empty")]
    #[case("notanemail", "missing @")]
    #[case("a@b@c.com", "multiple @")]
    #[case("@example.com", "empty local")]
    #[case("user@", "empty domain")]
    #[case("us er@example.com", "internal whitespace")]
    #[case("user@localhost", "domain without a TLD")]
    #[case("user@.example.com", "empty leading domain label")]
    #[case(".user@example.com", "leading dot in local part")]
    #[case("user..name@example.com", "consecutive dots in local part")]
    fn contact_email_rejects_invalid_shapes(#[case] input: &str, #[case] desc: &str) {
        assert!(
            serde_json::from_value::<ContactEmail>(json!(input)).is_err(),
            "expected {desc:?} to be rejected (input: {input:?})"
        );
    }

    /// Each parses under the crate's defaults; locks the narrowing so widening
    /// the parser options back fails a test.
    #[rstest]
    #[case("\"../../etc/passwd\"@example.com", "quoted local part holding a path")]
    #[case("\"user\"@example.com", "quoted local part, otherwise ordinary")]
    #[case("\"us er\"@example.com", "quoted local part holding a space")]
    #[case("\"a@b\"@example.com", "quoted local part holding a second @")]
    #[case("a/../b@example.com", "unquoted local part holding a path separator")]
    #[case("Ops <ops@example.com>", "display name")]
    #[case("<ops@example.com>", "bracketed address")]
    #[case("user@[192.0.2.1]", "domain literal")]
    fn contact_email_rejects_rfc5322_shapes_that_are_not_contact_addresses(
        #[case] input: &str,
        #[case] desc: &str,
    ) {
        assert!(
            serde_json::from_value::<ContactEmail>(json!(input)).is_err(),
            "expected {desc:?} to be rejected (input: {input:?})"
        );
    }

    /// Both inputs stay inside every per-part bound the crate enforces, so only
    /// `MAX_LEN` can refuse the first; an over-long label would prove nothing.
    #[test]
    fn contact_email_rejects_overlong_address() -> anyhow::Result<()> {
        let labels = format!("{0}.{0}.{0}", "d".repeat(63));

        let over = format!("{}@{labels}.com", "l".repeat(64));
        assert_eq!(over.len(), 260);
        assert!(
            serde_json::from_value::<ContactEmail>(json!(over)).is_err(),
            "expected {} octets to be rejected by the {}-octet cap",
            over.len(),
            ContactEmail::MAX_LEN
        );

        let at_limit = format!("{}@{labels}.co", "l".repeat(59));
        assert_eq!(at_limit.len(), ContactEmail::MAX_LEN);
        let parsed: ContactEmail = serde_json::from_value(json!(at_limit))?;
        assert_eq!(
            parsed.as_str(),
            at_limit,
            "an address exactly at the cap must still parse"
        );
        Ok(())
    }

    /// Real addresses the local-part narrowing must not take out.
    #[rstest]
    #[case("o'brien@example.com", "apostrophe")]
    #[case("bounce=user=example.com@lists.example.org", "VERP bounce address")]
    #[case("user_name@example.com", "underscore")]
    #[case("user-name@example.com", "hyphen")]
    #[case("user%test@example.com", "percent")]
    fn contact_email_accepts_unusual_but_real_local_parts(
        #[case] input: &str,
        #[case] desc: &str,
    ) -> anyhow::Result<()> {
        let e: ContactEmail = serde_json::from_value(json!(input))?;
        assert_eq!(e.as_str(), input, "expected {desc:?} to survive unchanged");
        Ok(())
    }

    // Non-ASCII: characterization tests pinning where `email_address` draws
    // the line (`char::is_alphanumeric`); change them deliberately.

    /// Stored exactly as it arrived, never punycoded. A shape check, not a
    /// registry lookup, so an undelegated TLD like `.эйай` passes.
    #[rstest]
    #[case("почта@example.com", "unicode local part")]
    #[case("почта@пример.эйай", "unicode on both sides")]
    #[case("b\u{00FC}ro@пример.эйай", "precomposed latin diacritic")]
    #[case("contact@пример.эйай", "Cyrillic")]
    #[case("contact@пример.рф", "Cyrillic, .рф")]
    #[case("contact@приклад.укр", "Cyrillic, .укр")]
    #[case("contact@мысал.қаз", "Cyrillic with Kazakh letters, .қаз")]
    #[case("contact@例子.中国", "Han, .中国")]
    #[case("contact@例子.台灣", "Han, .台灣")]
    #[case("contact@例え.日本", "Japanese, .日本")]
    #[case("contact@예시.한국", "Hangul, .한국")]
    #[case("contact@παράδειγμα.ελ", "Greek, .ελ")]
    #[case("contact@օրինակ.հայ", "Armenian, .հայ")]
    #[case("contact@მაგალითი.გე", "Georgian, .გე")]
    #[case("contact@উদাহরণ.ভারত", "Bengali, .ভারত")]
    #[case("contact@مثال.السعودية", "Arabic, .السعودية (RTL)")]
    #[case("contact@نمونه.ایران", "Persian, .ایران (RTL)")]
    #[case("contact@דוגמה.ישראל", "Hebrew, .ישראל (RTL)")]
    fn contact_email_accepts_non_ascii(
        #[case] input: &str,
        #[case] desc: &str,
    ) -> anyhow::Result<()> {
        let e: ContactEmail = serde_json::from_value(json!(input))?;
        assert_eq!(e.as_str(), input, "expected {desc:?} to survive unchanged");
        Ok(())
    }

    /// The grammar admits letters and digits, so symbols and combining marks
    /// are rejected.
    #[rstest]
    #[case("🙂@пример.эйай", "emoji local part")]
    #[case("bu\u{0308}ro@пример.эйай", "combining diaeresis")]
    fn contact_email_rejects_non_letter_non_ascii(#[case] input: &str, #[case] desc: &str) {
        assert!(
            serde_json::from_value::<ContactEmail>(json!(input)).is_err(),
            "expected {desc:?} to be rejected (input: {input:?})"
        );
    }

    /// "Digits" is wider than `0-9`: any Unicode digit, on either side of `@`.
    #[rstest]
    #[case(
        "почта\u{0662}\u{0660}\u{0662}\u{0666}@пример.эйай",
        "Arabic-Indic digits, local part"
    )]
    #[case("contact@пример\u{0662}.эйай", "Arabic-Indic digit, domain label")]
    #[case(
        "почта\u{0968}\u{0966}\u{0968}\u{096C}@пример.эйай",
        "Devanagari digits, local part"
    )]
    fn contact_email_accepts_non_ascii_digits(
        #[case] input: &str,
        #[case] desc: &str,
    ) -> anyhow::Result<()> {
        let e: ContactEmail = serde_json::from_value(json!(input))?;
        assert_eq!(e.as_str(), input, "expected {desc:?} to be accepted");
        Ok(())
    }

    /// Known defect, not policy: a non-spacing mark (Thai tone mark, virama) is
    /// refused because `email_address`'s `UTF8-non-ascii` check tests codepoint
    /// bytes, not UTF-8. Pinned so a crate bump flips it visibly.
    #[rstest]
    #[case("contact@ตัวอย่าง.ไทย", "Thai with a tone mark (U+0E48)")]
    #[case("contact@हिन्दी.भारत", "Devanagari with a virama (U+094D)")]
    #[case("contact@உதாரணம்.சிங்கப்பூர்", "Tamil with a virama (U+0BCD)")]
    fn contact_email_rejects_scripts_needing_non_spacing_marks(
        #[case] input: &str,
        #[case] desc: &str,
    ) {
        assert!(
            serde_json::from_value::<ContactEmail>(json!(input)).is_err(),
            "expected {desc:?} to be rejected today (input: {input:?})"
        );
    }

    /// Words without a non-spacing mark pass, which hides the defect above.
    #[rstest]
    #[case("contact@ไทย.ไทย", "Thai without marks")]
    #[case("contact@उदाहरण.भारत", "Devanagari without a virama")]
    #[case("contact@กั.ไทย", "Thai with a *spacing* mark (U+0E31)")]
    fn contact_email_accepts_same_scripts_without_non_spacing_marks(
        #[case] input: &str,
        #[case] desc: &str,
    ) -> anyhow::Result<()> {
        let e: ContactEmail = serde_json::from_value(json!(input))?;
        assert_eq!(e.as_str(), input, "expected {desc:?} to be accepted");
        Ok(())
    }

    /// Each is unrenderable or misleading in a terminal yet makes a distinct
    /// string.
    #[rstest]
    #[case("contact@пример.эйай\u{200B}", "trailing zero-width space")]
    #[case("contact@example.co\u{200D}m", "zero-width joiner inside a label")]
    #[case("contact@\u{202E}elpmaxe.com", "right-to-left override")]
    fn contact_email_rejects_invisible_and_bidi_controls(#[case] input: &str, #[case] desc: &str) {
        assert!(
            serde_json::from_value::<ContactEmail>(json!(input)).is_err(),
            "expected {desc:?} to be rejected (input: {input:?})"
        );
    }

    /// Known sharp edge, documented not endorsed: NFC `ü` parses, NFD `u` +
    /// U+0308 does not, and macOS hands back NFD on some paths.
    #[test]
    fn contact_email_does_not_normalize_unicode() {
        let nfc = "b\u{00FC}ro@пример.эйай";
        let nfd = "bu\u{0308}ro@пример.эйай";
        assert_ne!(nfc, nfd, "the two spellings differ as bytes");

        assert!(
            serde_json::from_value::<ContactEmail>(json!(nfc)).is_ok(),
            "NFC spelling is accepted"
        );
        assert!(
            serde_json::from_value::<ContactEmail>(json!(nfd)).is_err(),
            "NFD spelling of the same address is rejected"
        );
    }

    /// Known sharp edge: nothing folds confusables, so an `auto_approve` rule
    /// fails safe, but a human skimming the client list can be misled.
    #[rstest]
    #[case(
        "contact@gmail.com",
        "contact@gm\u{0430}il.com",
        "Cyrillic homograph of gmail.com"
    )]
    #[case(
        "contact@пример.эйай",
        "contact@xn--e1afmkfd.xn--80arb1f",
        "unicode domain vs its punycode"
    )]
    #[case(
        "contact@example.com",
        "contact@ｅｘａｍｐｌｅ.com",
        "fullwidth forms of the same label"
    )]
    #[case(
        "ex@example.com",
        "\u{1D41E}\u{1D431}@example.com",
        "mathematical bold letters for `ex`"
    )]
    fn contact_email_keeps_confusable_spellings_distinct(
        #[case] a: &str,
        #[case] b: &str,
        #[case] desc: &str,
    ) -> anyhow::Result<()> {
        let a: ContactEmail = serde_json::from_value(json!(a))?;
        let b: ContactEmail = serde_json::from_value(json!(b))?;
        assert_ne!(a, b, "{desc}: must not be treated as the same address");
        Ok(())
    }

    /// Flat `[a-z0-9_-]` only; a slash above all, since tags are flat.
    #[rstest]
    #[case("team", true)]
    #[case("team-mobile", true)]
    #[case("us-east", true)]
    #[case("batch-2026q3", true)]
    #[case("a_b-c", true)]
    #[case("", false)]
    #[case("   ", false)]
    #[case("\t\n", false)]
    #[case("team/mobile", false)]
    #[case("team mobile", false)]
    #[case("team.mobile", false)]
    #[case("team:mobile", false)]
    #[case("café", false)]
    fn tag_validation(#[case] input: &str, #[case] valid: bool) {
        assert_eq!(Tag::try_new(input).is_ok(), valid, "{input:?}");
    }

    /// Input is trimmed and lowercased, via both `try_new` and `Deserialize`.
    #[rstest]
    #[case("  Team-Mobile\n", "team-mobile")]
    #[case("TEAM-MOBILE", "team-mobile")]
    #[case("us-east", "us-east")]
    fn tag_trims_and_lowercases(#[case] input: &str, #[case] expected: &str) -> anyhow::Result<()> {
        assert_eq!(Tag::try_new(input)?.as_str(), expected);
        let deserialized: Tag = serde_json::from_value(json!(input))?;
        assert_eq!(deserialized.as_str(), expected);
        Ok(())
    }

    #[test]
    fn tag_enforces_length_bound() {
        assert!(matches!(
            Tag::try_new("a".repeat(Tag::MAX_LEN + 1)),
            Err(TagError::TooLong { .. })
        ));
    }

    #[test]
    fn tag_serializes_transparently() -> anyhow::Result<()> {
        let t = Tag::try_new("team-mobile")?;
        assert_eq!(serde_json::to_value(&t)?, json!("team-mobile"));
        Ok(())
    }
}
