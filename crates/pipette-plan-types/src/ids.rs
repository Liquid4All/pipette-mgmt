//! The plan domain's ids and a job's deadline, spelled the same in stored
//! records, queue keys and HTTP bodies.

use std::fmt;
use std::str::FromStr;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::PrincipalIdError;

/// Unique identifier for a campaign: `campaign_` followed by a UUIDv7, which
/// mgmt mints.
///
/// The stable handle in the plan domain: a [`PlanId`] names one immutable
/// ingestion, while a campaign survives revisions and follows
/// `current_plan_id` to whatever is executing now.
///
/// Prefixed, unlike [`PlanId`], so a plan id pasted into a campaign route is a
/// `400` at the boundary instead of a `404` from the store.
#[nutype::nutype(
    validate(with = validate_campaign_id, error = IdError),
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
        // So a path segment and a query parameter naming a campaign parse
        // through the same validator.
        FromStr,
    ),
)]
pub struct CampaignId(String);

/// Charset plus prefix, [`OrgId`](crate::OrgId)'s rule one keyspace over.
fn validate_campaign_id(s: &str) -> Result<(), IdError> {
    check_id(s, "campaign_id", "[A-Za-z0-9_-]", &['_', '-'])?;
    if !s.starts_with(CampaignId::PREFIX) {
        return Err(IdError::MissingPrefix {
            field: "campaign_id",
            prefix: CampaignId::PREFIX,
        });
    }
    Ok(())
}

impl CampaignId {
    /// Prefix on **every** campaign id, for [`OrgId::PREFIX`](crate::OrgId::PREFIX)'s reason.
    pub const PREFIX: &'static str = "campaign_";

    pub fn as_str(&self) -> &str {
        self.as_ref()
    }
}

/// Unique identifier for an audit event: `audit_` followed by a UUIDv7, so ids
/// sort in the order the events happened.
#[nutype::nutype(
    validate(with = validate_audit_event_id, error = IdError),
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
        FromStr,
    ),
)]
pub struct AuditEventId(String);

fn validate_audit_event_id(s: &str) -> Result<(), IdError> {
    check_id(s, "audit_event_id", "[A-Za-z0-9_-]", &['_', '-'])?;
    if !s.starts_with(AuditEventId::PREFIX) {
        return Err(IdError::MissingPrefix {
            field: "audit_event_id",
            prefix: AuditEventId::PREFIX,
        });
    }
    Ok(())
}

impl AuditEventId {
    pub const PREFIX: &'static str = "audit_";

    pub fn as_str(&self) -> &str {
        self.as_ref()
    }
}

#[cfg(test)]
mod campaign_id_tests {
    use rstest::rstest;

    use super::*;

    /// A plan id does not pass for a campaign id: the prefix makes it a `400`.
    #[rstest]
    #[case::prefixed("campaign_anything".to_string(), true)]
    #[case::a_plan_id("plan-0195c3e2-a4f3-7b8c-9d1e-2f3a4b5c6d7e".to_string(), false)]
    #[case::an_org_id("org_a3f8c2".to_string(), false)]
    #[case::bare("nightly".to_string(), false)]
    #[case::empty(String::new(), false)]
    fn a_campaign_id_requires_its_prefix(#[case] raw: String, #[case] accepted: bool) {
        assert_eq!(
            CampaignId::try_new(raw.clone()).is_ok(),
            accepted,
            "{raw:?}"
        );
    }
}

/// Unique identifier for a submitted job.
///
/// Always `[A-Za-z0-9-]`: safe verbatim in paths and object keys, and, with no
/// `.` or `/`, unambiguous to split out of names like `{job_id}.{expires_at}`.
/// No unchecked constructor outside tests (`From<String>` is not derived): a
/// `/` or `..` would be a path-traversal / key-injection vector.
///
/// Hand-written, not `#[nutype]`: `Deserialize` is deliberately unvalidated so
/// a stored legacy body still reads (client ingest goes through
/// [`JobId::try_new`]), and the test-only `new_unchecked` needs the field.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct JobId(String);

impl fmt::Display for JobId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl AsRef<str> for JobId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

/// Validating, unlike `Deserialize`: what a request names goes through here.
impl FromStr for JobId {
    type Err = IdError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::try_new(s)
    }
}

impl From<JobId> for String {
    fn from(id: JobId) -> Self {
        id.0
    }
}

/// Unique identifier for an ingested plan — the plan-manifest key
/// (`plans/{plan_id}.json`) and the handle for progress and cancellation.
///
/// Like [`JobId`], always `[A-Za-z0-9-]` with no unchecked constructor;
/// `Deserialize` is unvalidated because manifests are server-written.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PlanId(String);

impl fmt::Display for PlanId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl AsRef<str> for PlanId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

/// Validating, as [`JobId`]'s is.
impl FromStr for PlanId {
    type Err = IdError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::try_new(s)
    }
}

impl From<PlanId> for String {
    fn from(id: PlanId) -> Self {
        id.0
    }
}

/// Constructor error for the validating ID newtypes. Carries the
/// field name so the message says *which* ID was bad.
#[derive(Debug, thiserror::Error)]
pub enum IdError {
    #[error("{field} must not be empty")]
    Empty { field: &'static str },
    #[error("{field} contains invalid character {ch:?}; allowed: {allowed}")]
    InvalidChar {
        field: &'static str,
        ch: char,
        allowed: &'static str,
    },
    /// A value whose charset is fine but which does not carry the prefix its
    /// kind requires.
    #[error("{field} must start with {prefix:?}")]
    MissingPrefix {
        field: &'static str,
        prefix: &'static str,
    },
    /// A value of the wrong width for a fixed-width kind
    /// (`pipette_core::key_index::PublicKeyDigest`).
    #[error("{field} must be {expected}, got {got} characters")]
    Width {
        field: &'static str,
        expected: &'static str,
        got: usize,
    },
    #[error(transparent)]
    Principal(#[from] PrincipalIdError),
    /// A key segment its id's parse rewrites, so the id would name another key.
    #[error("key segment {segment:?} does not name an id verbatim")]
    NotVerbatim { segment: String },
}

/// The one charset check for every id newtype: non-empty, and every character
/// ASCII-alphanumeric or in `extra`. No caller allows `/` or `:`, so a valid id
/// is a safe path segment; the `todo/`-tree ids also exclude `.`, which keeps
/// `.`-delimited filenames unambiguous.
fn check_id(
    s: &str,
    field: &'static str,
    allowed: &'static str,
    extra: &[char],
) -> Result<(), IdError> {
    if s.is_empty() {
        return Err(IdError::Empty { field });
    }
    if let Some(ch) = s
        .chars()
        .find(|c| !c.is_ascii_alphanumeric() && !extra.contains(c))
    {
        return Err(IdError::InvalidChar { field, ch, allowed });
    }
    Ok(())
}

/// The expiry field encoded in an `avail/` filename: `never`, or ISO 8601
/// compact (`20260101T120000Z`) so it is filename-safe without URL-encoding.
///
/// The derived `Ord` (declaration order) sorts sooner expiries first and
/// `Never` last; the claim handler relies on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ExpiresAt {
    At(DateTime<Utc>),
    Never,
}

impl Serialize for ExpiresAt {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for ExpiresAt {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        value.parse().map_err(serde::de::Error::custom)
    }
}

impl ExpiresAt {
    /// Whether this expiry lies strictly before `now`. The strict `<` matches
    /// the `queue-maintenance` sweep, so `claim` refuses exactly what it
    /// expires.
    pub fn is_expired(self, now: DateTime<Utc>) -> bool {
        match self {
            ExpiresAt::At(dt) => dt < now,
            ExpiresAt::Never => false,
        }
    }

    /// The instant, `None` for never: a deadline as an API body writes it.
    pub fn instant(self) -> Option<DateTime<Utc>> {
        match self {
            ExpiresAt::At(dt) => Some(dt),
            ExpiresAt::Never => None,
        }
    }
}

impl fmt::Display for ExpiresAt {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ExpiresAt::At(dt) => write!(f, "{}", dt.format("%Y%m%dT%H%M%SZ")),
            ExpiresAt::Never => write!(f, "never"),
        }
    }
}

#[derive(Debug, thiserror::Error)]
#[error("invalid ExpiresAt value: {0:?}")]
pub struct ExpiresAtError(String);

impl FromStr for ExpiresAt {
    type Err = ExpiresAtError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s == "never" {
            return Ok(ExpiresAt::Never);
        }
        chrono::NaiveDateTime::parse_from_str(s, "%Y%m%dT%H%M%SZ")
            .map(|ndt| ExpiresAt::At(ndt.and_utc()))
            .map_err(|_| ExpiresAtError(s.to_string()))
    }
}

/// Identifier for a pre-auth registration key. `[A-Za-z0-9_-]`, so it is safe
/// in the `client-preauth/{key_id}.json` key and, with no `.`, splits out of
/// the `client_preauth_{key_id}.{secret}` token.
#[nutype::nutype(
    validate(with = validate_client_preauth_key_id, error = IdError),
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
        FromStr,
    ),
)]
pub struct ClientPreauthKeyId(String);

fn validate_client_preauth_key_id(s: &str) -> Result<(), IdError> {
    check_id(s, "client_preauth_key_id", "[A-Za-z0-9_-]", &['_', '-'])
}

impl ClientPreauthKeyId {
    pub fn as_str(&self) -> &str {
        self.as_ref()
    }
}

/// Identifier for a user pre-auth key, under [`ClientPreauthKeyId`]'s charset
/// for the `user-preauth/{key_id}.json` key and the
/// `user_preauth_{key_id}.{secret}` token.
///
/// A separate type because the two name records in two keyspaces: mixing them
/// up is a compile error rather than a `404` that reads like a revoked key.
#[nutype::nutype(
    validate(with = validate_user_preauth_key_id, error = IdError),
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
        FromStr,
    ),
)]
pub struct UserPreauthKeyId(String);

fn validate_user_preauth_key_id(s: &str) -> Result<(), IdError> {
    check_id(s, "user_preauth_key_id", "[A-Za-z0-9_-]", &['_', '-'])
}

impl UserPreauthKeyId {
    pub fn as_str(&self) -> &str {
        self.as_ref()
    }
}

/// A model key: a model name as table lookup normalizes it, or a vault
/// resource id. `[a-z0-9._-]` with no leading `.`, so it is one safe path segment.
#[nutype::nutype(
    validate(with = validate_model_key, error = IdError),
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
        Serialize,
        Deserialize,
        FromStr,
    ),
)]
pub struct ModelKey(String);

impl ModelKey {
    pub const MAX_LEN: usize = 200;

    pub fn as_str(&self) -> &str {
        self.as_ref()
    }
}

fn validate_model_key(s: &str) -> Result<(), IdError> {
    const FIELD: &str = "model key";
    const ALLOWED: &str = "[a-z0-9._-], not starting with '.'";
    if s.is_empty() {
        return Err(IdError::Empty { field: FIELD });
    }
    if s.len() > ModelKey::MAX_LEN {
        return Err(IdError::Width {
            field: FIELD,
            expected: "at most 200 characters",
            got: s.len(),
        });
    }
    let bad = |(i, c): &(usize, char)| {
        !(c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '_' | '-'))
            && !(*c == '.' && *i > 0)
    };
    match s.char_indices().find(bad) {
        Some((_, ch)) => Err(IdError::InvalidChar {
            field: FIELD,
            ch,
            allowed: ALLOWED,
        }),
        None => Ok(()),
    }
}

#[cfg(test)]
mod model_key_tests {
    use rstest::rstest;

    use super::*;

    #[rstest]
    #[case::normalized_name("acmelm-4.5b-text", true)]
    #[case::vault_id("vault_01a123b6-1b01-7382-b7f9-c3af2100782d", true)]
    #[case::underscored("apple_foundation_text", true)]
    #[case::empty("", false)]
    #[case::upper("ACMELM", false)]
    #[case::slash("liquidai/acmelm", false)]
    #[case::leading_dot("..", false)]
    #[case::space("lfm 3", false)]
    fn model_key_try_new_charset(#[case] key: &str, #[case] valid: bool) {
        assert_eq!(ModelKey::try_new(key).is_ok(), valid, "{key:?}");
    }
}

impl JobId {
    /// Prefix on every server-minted id, so one reads as a job id in logs. A
    /// hyphen, because `_` is outside the job-id charset.
    pub const MINT_PREFIX: &'static str = "job-";

    /// Infallible: `job-{uuid}` is charset-safe by construction.
    ///
    /// Mint with `Uuid::now_v7` so `avail/` listings and logs read in arrival
    /// order; nothing derives correctness from that order. `try_new` does not
    /// enforce the prefix, so client-echoed ids and fixtures need not carry it.
    pub fn from_uuid(id: uuid::Uuid) -> Self {
        Self(format!("{}{}", Self::MINT_PREFIX, id))
    }

    /// Validating constructor for any id built from a string: client input, and values
    /// reconstructed from stored data, which fail closed. Rejects `/` and `..` (path
    /// traversal, key injection) and `.`, the delimiter in `{job_id}.{…}` key names.
    pub fn try_new(s: impl Into<String>) -> Result<Self, IdError> {
        let s = s.into();
        check_id(&s, "job_id", "[A-Za-z0-9-]", &['-'])?;
        Ok(Self(s))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl PlanId {
    /// Prefix on every server-minted plan id. A hyphen, because `_` is outside
    /// the id charset.
    pub const MINT_PREFIX: &'static str = "plan-";

    /// Infallible: `plan-{uuid}` is charset-safe by construction. Mint with
    /// `Uuid::now_v7`, as [`JobId::from_uuid`] does.
    pub fn from_uuid(id: uuid::Uuid) -> Self {
        Self(format!("{}{}", Self::MINT_PREFIX, id))
    }

    /// Validating constructor for a plan id reconstructed from a stored object
    /// key, so a corrupt key fails closed.
    pub fn try_new(s: impl Into<String>) -> Result<Self, IdError> {
        let s = s.into();
        check_id(&s, "plan_id", "[A-Za-z0-9-]", &['-'])?;
        Ok(Self(s))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Test-only permissive constructor, behind `testkit`, which only a
/// dev-dependency enables, so a release build has none.
#[cfg(any(test, feature = "testkit"))]
impl JobId {
    pub fn new_unchecked(s: impl Into<String>) -> Self {
        Self(s.into())
    }
}

#[cfg(test)]
mod tests {
    use rstest::rstest;
    use serde_json::json;

    use super::*;

    /// `try_new` rejects empty, path-significant and `.`-bearing ids.
    #[rstest]
    #[case("job-550e8400-e29b-41d4-a716-446655440000", true)] // server mint shape
    #[case("550e8400-e29b-41d4-a716-446655440000", true)] // bare UUID (client echo)
    #[case("mlx-preserve-job", true)]
    #[case("", false)] // empty
    #[case("job_1", false)] // '_' is outside the job_id charset (unlike client_id)
    #[case("a/b", false)] // path separator
    #[case("..", false)] // traversal
    #[case("a.b", false)] // '.' is the todo/ filename & marker delimiter
    #[case("has space", false)]
    fn job_id_try_new_charset(#[case] input: &str, #[case] valid: bool) {
        assert_eq!(JobId::try_new(input).is_ok(), valid, "{input:?}");
    }

    #[test]
    fn job_id_from_uuid_prefixes_and_is_charset_safe() {
        let id = JobId::from_uuid(uuid::Uuid::nil());
        assert!(id.as_str().starts_with(JobId::MINT_PREFIX));
        assert!(JobId::try_new(id.as_str()).is_ok());
    }

    #[test]
    fn plan_id_from_uuid_prefixes_and_is_charset_safe() {
        let id = PlanId::from_uuid(uuid::Uuid::nil());
        assert!(id.as_str().starts_with(PlanId::MINT_PREFIX));
        assert!(PlanId::try_new(id.as_str()).is_ok());
    }

    #[rstest]
    #[case("plan-018fce2a-7b41-7e00-9c3d-2a1b6f4e8d20", true)] // server mint shape
    #[case("018fce2a-7b41-7e00-9c3d-2a1b6f4e8d20", true)] // bare UUID
    #[case("", false)] // empty
    #[case("plan_1", false)] // '_' is outside the charset
    #[case("a/b", false)] // path separator
    #[case("a.b", false)] // '.' is a key delimiter
    fn plan_id_try_new_charset(#[case] input: &str, #[case] valid: bool) {
        assert_eq!(PlanId::try_new(input).is_ok(), valid, "{input:?}");
    }

    #[test]
    fn job_id_deserialize_is_transparent() {
        // The safety boundary is `try_new` at client-facing ingest, not serde.
        assert!(serde_json::from_value::<JobId>(json!("job-1")).is_ok());
    }

    #[test]
    fn expires_at_never_roundtrip() -> anyhow::Result<()> {
        let s = ExpiresAt::Never.to_string();
        assert_eq!(s, "never");
        assert_eq!(s.parse::<ExpiresAt>()?, ExpiresAt::Never);
        Ok(())
    }

    /// Infallible so it fits in a `#[case]`; an unparsable literal gives
    /// `Never`.
    fn at(s: &str) -> ExpiresAt {
        s.parse().map(ExpiresAt::At).unwrap_or(ExpiresAt::Never)
    }

    #[rstest]
    #[case::at(at("2026-01-01T12:00:00Z"), json!("20260101T120000Z"))]
    #[case::never(ExpiresAt::Never, json!("never"))]
    fn expires_at_serde_roundtrip(
        #[case] value: ExpiresAt,
        #[case] encoded: serde_json::Value,
    ) -> anyhow::Result<()> {
        assert_eq!(serde_json::to_value(value)?, encoded);
        assert_eq!(serde_json::from_value::<ExpiresAt>(encoded)?, value);
        Ok(())
    }

    #[test]
    fn expires_at_serde_rejects_malformed_string() -> anyhow::Result<()> {
        let Err(err) = serde_json::from_value::<ExpiresAt>(json!("not-a-date")) else {
            anyhow::bail!("expected a deserialize error");
        };
        assert!(err.to_string().contains("invalid ExpiresAt value"));
        Ok(())
    }

    #[test]
    fn expires_at_at_roundtrip() -> anyhow::Result<()> {
        use chrono::TimeZone;
        let dt = Utc.with_ymd_and_hms(2026, 1, 1, 12, 0, 0).unwrap();
        let v = ExpiresAt::At(dt);
        let s = v.to_string();
        assert_eq!(s, "20260101T120000Z");
        assert_eq!(s.parse::<ExpiresAt>()?, ExpiresAt::At(dt));
        Ok(())
    }

    #[test]
    fn expires_at_epoch_boundary() -> anyhow::Result<()> {
        use chrono::TimeZone;
        let dt = Utc.with_ymd_and_hms(1970, 1, 1, 0, 0, 0).unwrap();
        let s = ExpiresAt::At(dt).to_string();
        assert_eq!(s, "19700101T000000Z");
        assert_eq!(s.parse::<ExpiresAt>()?, ExpiresAt::At(dt));
        Ok(())
    }

    #[test]
    fn expires_at_rejects_garbage() {
        assert!("not-a-date".parse::<ExpiresAt>().is_err());
        assert!("".parse::<ExpiresAt>().is_err());
    }

    #[test]
    fn expires_at_orders_by_urgency() {
        use chrono::TimeZone;
        let earlier = ExpiresAt::At(Utc.with_ymd_and_hms(2026, 1, 1, 12, 0, 0).unwrap());
        let later = ExpiresAt::At(Utc.with_ymd_and_hms(2026, 1, 2, 12, 0, 0).unwrap());
        assert!(earlier < later);
        assert!(later < ExpiresAt::Never);
        assert!(earlier < ExpiresAt::Never);

        let mut v = vec![ExpiresAt::Never, later, earlier];
        v.sort();
        assert_eq!(v, vec![earlier, later, ExpiresAt::Never]);
    }

    #[test]
    fn expires_at_is_expired() {
        use chrono::TimeZone;
        let now = Utc.with_ymd_and_hms(2026, 1, 2, 12, 0, 0).unwrap();
        let past = ExpiresAt::At(Utc.with_ymd_and_hms(2026, 1, 1, 12, 0, 0).unwrap());
        let future = ExpiresAt::At(Utc.with_ymd_and_hms(2026, 1, 3, 12, 0, 0).unwrap());

        assert!(past.is_expired(now));
        assert!(!future.is_expired(now));
        assert!(!ExpiresAt::Never.is_expired(now));
        // Strict `<`: a job expiring at exactly `now` is still claimable.
        assert!(!ExpiresAt::At(now).is_expired(now));
    }
}
