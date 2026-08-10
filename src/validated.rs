//! Validated input types. Newtypes that establish their invariant at
//! `Deserialize` time so on-disk bodies, HTTP request payloads, and
//! every other parse boundary share one source of truth for what a
//! valid string looks like.

use std::fmt;
use std::ops::Deref;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

// ---------------------------------------------------------------------------
// NonEmptyTrimmedString
// ---------------------------------------------------------------------------

/// A `String` that is both **trimmed** *and* **non-empty**. Used for
/// required wire fields where `""` or `"   "` would be a client
/// error. The invariant is enforced at `Deserialize` time, so a body
/// that arrives with `"model_name": "   "` is rejected as a 400 with
/// a clear message rather than persisted and surfacing downstream as
/// a blank warehouse row.
///
/// Behaves like `&str` via `Deref` so existing call sites that pass
/// the field to `parse::<T>()`, format strings, `==`, etc. need no
/// change. Serializes transparently as a plain JSON string.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
#[serde(transparent)]
pub struct NonEmptyTrimmedString(String);

/// Construction error for [`NonEmptyTrimmedString`]. Empty or
/// whitespace-only input is rejected.
#[derive(Debug, thiserror::Error)]
#[error("must be a non-empty string (whitespace-only is not accepted)")]
pub struct EmptyStringError;

impl NonEmptyTrimmedString {
    /// Validating constructor for internal callers (test fixtures,
    /// migrations) that have a string in hand without going through
    /// `Deserialize`. Errors on empty / whitespace-only input —
    /// same rules as `Deserialize`.
    pub fn try_new(s: impl Into<String>) -> Result<Self, EmptyStringError> {
        let raw = s.into();
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Err(EmptyStringError);
        }
        if trimmed.len() == raw.len() {
            Ok(Self(raw))
        } else {
            Ok(Self(trimmed.to_string()))
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn into_inner(self) -> String {
        self.0
    }
}

impl<'de> Deserialize<'de> for NonEmptyTrimmedString {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        Self::try_new(s).map_err(serde::de::Error::custom)
    }
}

impl Deref for NonEmptyTrimmedString {
    type Target = str;
    fn deref(&self) -> &str {
        &self.0
    }
}

impl AsRef<str> for NonEmptyTrimmedString {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for NonEmptyTrimmedString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<NonEmptyTrimmedString> for String {
    fn from(t: NonEmptyTrimmedString) -> String {
        t.0
    }
}

// ---------------------------------------------------------------------------
// PublicKeyHex
// ---------------------------------------------------------------------------

/// A hex-encoded Ed25519 public key (32 bytes → 64 hex chars).
/// Validates on construction and `Deserialize`, so call sites that
/// hold a `PublicKeyHex` are statically guaranteed to have a parseable
/// key — no per-handler hex decode + length check.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PublicKeyHex(String);

/// Construction error for [`PublicKeyHex`].
#[derive(Debug, thiserror::Error)]
pub enum PublicKeyHexError {
    #[error("public_key must be valid hex")]
    InvalidHex,
    #[error("public_key must be 32 bytes (64 hex chars), got {got}")]
    WrongByteLength { got: usize },
}

impl PublicKeyHex {
    /// Raw byte length of an Ed25519 public key.
    pub const BYTE_LEN: usize = 32;

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn into_inner(self) -> String {
        self.0
    }

    fn validate(s: &str) -> Result<(), PublicKeyHexError> {
        let bytes = hex::decode(s).map_err(|_| PublicKeyHexError::InvalidHex)?;
        if bytes.len() != Self::BYTE_LEN {
            return Err(PublicKeyHexError::WrongByteLength { got: bytes.len() });
        }
        Ok(())
    }

    /// Construct from an already-validated string. Trims first and
    /// returns an error if the result doesn't parse as a 32-byte hex
    /// string.
    pub fn try_new(s: impl Into<String>) -> Result<Self, PublicKeyHexError> {
        let raw = s.into();
        let trimmed = raw.trim();
        Self::validate(trimmed)?;
        if trimmed.len() == raw.len() {
            Ok(Self(raw))
        } else {
            Ok(Self(trimmed.to_string()))
        }
    }
}

impl<'de> Deserialize<'de> for PublicKeyHex {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        Self::try_new(s).map_err(serde::de::Error::custom)
    }
}

impl Serialize for PublicKeyHex {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.0)
    }
}

impl Deref for PublicKeyHex {
    type Target = str;
    fn deref(&self) -> &str {
        &self.0
    }
}

impl AsRef<str> for PublicKeyHex {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for PublicKeyHex {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<PublicKeyHex> for String {
    fn from(p: PublicKeyHex) -> String {
        p.0
    }
}

// ---------------------------------------------------------------------------
// ContactEmail
// ---------------------------------------------------------------------------

/// A free-text email address. Validated via the `email_address`
/// crate's RFC 5322 parser at `Deserialize` and `try_new` time —
/// we don't hand-roll the rules. Trimmed leading/trailing
/// whitespace on construction so a `" user@example.com "` from a
/// shell-pipe client is accepted.
///
/// This is **not** a guarantee that the address can receive mail —
/// only that its shape parses. It catches the obvious typos
/// (`"Joe"`, `"user@localhost"`, `"a@b@c.com"`) at the wire boundary.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ContactEmail(String);

/// Construction error for [`ContactEmail`]. Single unit error —
/// callers only need to know "valid or not"; the specific failure
/// reason (`missing @`, `no . in domain`, …) is not part of the
/// public API. Matches the shape of [`EmptyStringError`].
#[derive(Debug, thiserror::Error)]
#[error("contact_email is not a valid email address")]
pub struct ContactEmailError;

impl ContactEmail {
    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn into_inner(self) -> String {
        self.0
    }

    /// Validating constructor for internal callers (test fixtures,
    /// CLI argument parsing) that don't go through `Deserialize`.
    /// Trims and then delegates to the `email_address` crate's
    /// RFC 5322 parser — we don't hand-roll the rules.
    pub fn try_new(s: impl Into<String>) -> Result<Self, ContactEmailError> {
        let raw = s.into();
        let trimmed = raw.trim();
        if !email_address::EmailAddress::is_valid(trimmed) {
            return Err(ContactEmailError);
        }
        if trimmed.len() == raw.len() {
            Ok(Self(raw))
        } else {
            Ok(Self(trimmed.to_string()))
        }
    }
}

impl<'de> Deserialize<'de> for ContactEmail {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        Self::try_new(s).map_err(serde::de::Error::custom)
    }
}

impl Serialize for ContactEmail {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.0)
    }
}

impl Deref for ContactEmail {
    type Target = str;
    fn deref(&self) -> &str {
        &self.0
    }
}

impl AsRef<str> for ContactEmail {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ContactEmail {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<ContactEmail> for String {
    fn from(e: ContactEmail) -> String {
        e.0
    }
}

// ---------------------------------------------------------------------------
// Tag
// ---------------------------------------------------------------------------

/// A flat client tag such as `team-mobile`, `us-east`, or `batch-2026q3`.
///
/// Tags are assigned manually on the mgmt side (`clients tag add`) to organize
/// the fleet; a client never sets its own. The invariant, established at
/// `Deserialize` / `try_new` time: a non-empty run of `[a-z0-9_-]` — no slash,
/// dot, whitespace, or other path-significant characters. Input is trimmed and
/// lowercased first, so `" Team-Mobile "` and `team-mobile` are the same tag —
/// case never produces a near-duplicate.
///
/// Deliberately **flat** (no `/` hierarchy): a tag is a single path segment, so
/// the tag-index marker trees stay a clean two-level `{tag}/{client_id}` shape
/// with no separator ambiguity. `Ord`/`Hash` so a client can hold a sorted,
/// de-duplicated `BTreeSet<Tag>`. Serializes transparently as a string.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Tag(String);

/// Construction error for [`Tag`]. Carries the offending value so the CLI / 400
/// message says which tag was rejected and why.
#[derive(Debug, thiserror::Error)]
pub enum TagError {
    #[error("tag must not be empty")]
    Empty,
    #[error("tag {tag:?} is too long ({len} chars, max {max})")]
    TooLong { tag: String, len: usize, max: usize },
    #[error("tag {tag:?} contains invalid character {ch:?}; allowed: [a-z0-9_-] (no '/')")]
    InvalidChar { tag: String, ch: char },
}

impl Tag {
    pub const MAX_LEN: usize = 64;

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn into_inner(self) -> String {
        self.0
    }

    /// Validating constructor. Trims and lowercases, then enforces the flat
    /// `[a-z0-9_-]` charset. Same rules as `Deserialize`.
    pub fn try_new(s: impl Into<String>) -> Result<Self, TagError> {
        let normalized = s.into().trim().to_ascii_lowercase();
        if normalized.is_empty() {
            return Err(TagError::Empty);
        }
        if normalized.len() > Self::MAX_LEN {
            return Err(TagError::TooLong {
                len: normalized.len(),
                max: Self::MAX_LEN,
                tag: normalized,
            });
        }
        if let Some(ch) = normalized
            .chars()
            .find(|c| !c.is_ascii_lowercase() && !c.is_ascii_digit() && *c != '_' && *c != '-')
        {
            return Err(TagError::InvalidChar {
                ch,
                tag: normalized,
            });
        }
        Ok(Self(normalized))
    }
}

impl<'de> Deserialize<'de> for Tag {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        Self::try_new(s).map_err(serde::de::Error::custom)
    }
}

impl Serialize for Tag {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.0)
    }
}

impl Deref for Tag {
    type Target = str;
    fn deref(&self) -> &str {
        &self.0
    }
}

impl AsRef<str> for Tag {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Tag {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<Tag> for String {
    fn from(t: Tag) -> String {
        t.0
    }
}

// ---------------------------------------------------------------------------
// BatteryLevel
// ---------------------------------------------------------------------------

/// Battery charge as an integer percentage in `0..=100`, validated at
/// `Deserialize` time (same pattern as [`NonEmptyTrimmedString`]) so an
/// out-of-range reading is rejected at the wire boundary rather than
/// persisted. Optional on the submission.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct BatteryLevel(i32);

/// Construction error for [`BatteryLevel`]. Values outside `0..=100`
/// are rejected.
#[derive(Debug, thiserror::Error)]
#[error("device_battery_level ({0}) must be between 0 and 100")]
pub struct BatteryLevelError(pub i32);

impl BatteryLevel {
    /// Validating constructor for internal callers (test fixtures)
    /// that hold an integer without going through `Deserialize`.
    /// Errors on values outside `0..=100` — same rule as `Deserialize`.
    pub fn try_new(level: i32) -> Result<Self, BatteryLevelError> {
        if (0..=100).contains(&level) {
            Ok(Self(level))
        } else {
            Err(BatteryLevelError(level))
        }
    }

    /// The percentage as a plain `i32` (always `0..=100`).
    pub fn get(self) -> i32 {
        self.0
    }
}

impl<'de> Deserialize<'de> for BatteryLevel {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let level = i32::deserialize(d)?;
        Self::try_new(level).map_err(serde::de::Error::custom)
    }
}

impl Serialize for BatteryLevel {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_i32(self.0)
    }
}

impl fmt::Display for BatteryLevel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl From<BatteryLevel> for i32 {
    fn from(b: BatteryLevel) -> i32 {
        b.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;
    use serde_json::json;

    #[test]
    fn non_empty_trimmed_string_trims_padded_input() -> anyhow::Result<()> {
        let v: NonEmptyTrimmedString = serde_json::from_value(json!("  llama-3.2-1b\t"))?;
        assert_eq!(v.as_str(), "llama-3.2-1b");
        Ok(())
    }

    #[test]
    fn non_empty_trimmed_string_rejects_empty_string() {
        let err = serde_json::from_value::<NonEmptyTrimmedString>(json!(""))
            .expect_err("empty string must be rejected");
        assert!(err.to_string().contains("non-empty"));
    }

    #[test]
    fn non_empty_trimmed_string_rejects_whitespace_only() {
        let err = serde_json::from_value::<NonEmptyTrimmedString>(json!("   \t\n  "))
            .expect_err("whitespace-only string must be rejected");
        assert!(err.to_string().contains("non-empty"));
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
    fn public_key_hex_rejects_invalid_hex() {
        let err =
            serde_json::from_value::<PublicKeyHex>(json!("zz")).expect_err("non-hex rejected");
        assert!(err.to_string().contains("valid hex"));
    }

    #[test]
    fn public_key_hex_rejects_wrong_byte_length() {
        let err = serde_json::from_value::<PublicKeyHex>(json!("abcdef"))
            .expect_err("short hex rejected");
        assert!(err.to_string().contains("32 bytes"));
    }

    #[test]
    fn contact_email_accepts_plain_address() -> anyhow::Result<()> {
        let e: ContactEmail = serde_json::from_value(json!("user@example.com"))?;
        assert_eq!(e.as_str(), "user@example.com");
        Ok(())
    }

    #[test]
    fn contact_email_accepts_plus_addressing() -> anyhow::Result<()> {
        // Gmail-style sub-addressing (`local+tag@domain`) must
        // pass — common for routing rules / form aliases.
        let e: ContactEmail = serde_json::from_value(json!("user+tag@example.com"))?;
        assert_eq!(e.as_str(), "user+tag@example.com");
        // Also the common `firstname+lastname` shape.
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

    /// Reject the obvious bad shapes. We don't assert on the
    /// specific error message — the public error is a single
    /// `ContactEmailError`. The exact set of rules is owned by
    /// the `email_address` crate (RFC 5322); we just spot-check
    /// the cases most likely to slip past a careless client.
    /// Note: `user@localhost` is *accepted* — RFC 5322 allows
    /// domains without a dot, and we trust the crate.
    #[rstest]
    #[case("", "empty")]
    #[case("notanemail", "missing @")]
    #[case("a@b@c.com", "multiple @")]
    #[case("@example.com", "empty local")]
    #[case("user@", "empty domain")]
    #[case("us er@example.com", "internal whitespace")]
    fn contact_email_rejects_invalid_shapes(#[case] input: &str, #[case] desc: &str) {
        assert!(
            serde_json::from_value::<ContactEmail>(json!(input)).is_err(),
            "expected {desc:?} to be rejected (input: {input:?})"
        );
    }

    #[rstest]
    #[case(0)]
    #[case(1)]
    #[case(42)]
    #[case(99)]
    #[case(100)]
    fn battery_level_accepts_in_range(#[case] v: i32) -> anyhow::Result<()> {
        let parsed: BatteryLevel = serde_json::from_value(json!(v))?;
        assert_eq!(parsed.get(), v);
        Ok(())
    }

    #[rstest]
    #[case(-1)]
    #[case(101)]
    #[case(255)]
    #[case(i32::MIN)]
    #[case(i32::MAX)]
    fn battery_level_rejects_out_of_range(#[case] v: i32) {
        let err = serde_json::from_value::<BatteryLevel>(json!(v))
            .expect_err("out-of-range battery level must be rejected");
        assert!(err.to_string().contains("between 0 and 100"));
    }

    #[test]
    fn battery_level_serializes_as_plain_integer() -> anyhow::Result<()> {
        let parsed: BatteryLevel = serde_json::from_value(json!(73))?;
        assert_eq!(serde_json::to_value(parsed)?, json!(73));
        Ok(())
    }

    /// Accepts flat `[a-z0-9_-]` tokens; rejects empty/whitespace and any
    /// path-significant or non-charset character (slash especially — tags are
    /// flat so the reverse index stays a clean two-level tree).
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
