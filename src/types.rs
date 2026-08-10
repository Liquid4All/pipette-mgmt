use chrono::{DateTime, Utc};
use derive_more::{AsRef, Display, Into};
use serde::{Deserialize, Serialize};
use std::{fmt, str::FromStr};

/// Unique identifier for a registered client, derived from their public key.
/// On `Deserialize` we require a non-empty value so on-disk drift can't
/// silently produce blank client IDs that bypass path-existence checks.
/// `From<String>` is *not* derived — use [`ClientId::try_from`] or
/// [`ClientId::try_new`], both of which validate.
#[derive(
    Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, Display, AsRef, Into,
)]
#[serde(try_from = "String", into = "String")]
pub struct ClientId(String);

/// Unique identifier for a benchmark, derived from the TOML filename.
///
/// The stored value matches `[A-Za-z0-9][A-Za-z0-9_.-]*`, enforced on
/// `Deserialize` and by [`BenchmarkId::try_new`]. `.` is in the charset because
/// catalog ids carry version numbers (`eval_ifbench_2026.06.1`); the leading
/// character is held to alphanumerics so the value can never be a relative path
/// segment, which is how it stays safe to interpolate into the catalog key
/// (`benchmarks/{benchmark_id}.toml`) and the warehouse partition
/// (`benchmark_id={benchmark_id}`). See [`ClientId`] for the rationale behind
/// dropping the derived `From<String>`.
#[derive(
    Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, Display, AsRef, Into,
)]
#[serde(try_from = "String", into = "String")]
pub struct BenchmarkId(String);

/// Unique identifier for a submitted job.
///
/// The stored value is always `[A-Za-z0-9-]` — safe to interpolate verbatim
/// into filesystem paths and object keys (`avail/{job_id}.…`,
/// `incoming/{job_id}`), and, because `.` is excluded, unambiguous to split
/// back out of the `.`-delimited names that embed it (`{job_id}.{expires_at}`,
/// `denied/{job_id}.{client_id}`). Two constructors enforce that, matched to
/// the source:
///
/// - [`JobId::from_uuid`] — server mints (`Uuid::now_v7`). Infallible; a UUID is
///   charset-safe by construction.
/// - [`JobId::try_new`] — any id built from a string: client-supplied input
///   (validated at the handler boundary → `400`) and values reconstructed from
///   stored filenames/columns (validated on read → fail-closed).
///
/// There is deliberately **no** unchecked/permissive constructor outside
/// `#[cfg(test)]` (`From<String>` is *not* derived): a `job_id` with `/` or
/// `..` would be a path-traversal / key-injection vector, and no legitimate id
/// needs those characters. Deserialize is `#[serde(transparent)]`; job bodies
/// are server-controlled, and every client-facing ingest path parses the id
/// through [`JobId::try_new`].
#[derive(
    Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, Display, AsRef, Into,
)]
#[serde(transparent)]
pub struct JobId(String);

/// Unique identifier for an ingested plan — the plan-manifest key
/// (`plans/{plan_id}.json`) and the handle for progress and cancellation.
///
/// Like [`JobId`], the stored value is always `[A-Za-z0-9-]`, so it is safe to
/// interpolate verbatim into the manifest object key. Two constructors, matched
/// to the source:
///
/// - [`PlanId::from_uuid`] — server mints at ingestion (`Uuid::now_v7`),
///   yielding `plan-{uuid}`. Infallible; a UUID is charset-safe by construction.
/// - [`PlanId::try_new`] — a value reconstructed from a stored object key,
///   validated on read (fail-closed).
///
/// `From<String>` is deliberately *not* derived, for the same path-traversal /
/// key-injection reason as [`JobId`]. `Deserialize` is `#[serde(transparent)]`
/// (unvalidated), which is safe because manifests are server-written: the value
/// only ever originates from [`PlanId::from_uuid`] or a round-trip of one, never
/// from client input — mirroring [`JobId`]'s rationale.
#[derive(
    Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, Display, AsRef, Into,
)]
#[serde(transparent)]
pub struct PlanId(String);

/// Constructor error for the validating ID newtypes. Carries the
/// field name so the message says *which* ID was bad.
///
/// Defined here so the leaf `types.rs` module stays free of
/// `anyhow`. `InvalidId: std::error::Error`, so `?` lifts it into
/// any `anyhow::Result<...>` caller automatically.
#[derive(Debug, thiserror::Error)]
pub enum InvalidId {
    #[error("{field} must not be empty")]
    Empty { field: &'static str },
    #[error("{field} contains invalid character {ch:?}; allowed: {allowed}")]
    InvalidChar {
        field: &'static str,
        ch: char,
        allowed: &'static str,
    },
}

/// Shared validation body for the charset-restricted id newtypes: non-empty, and
/// every character either ASCII-alphanumeric or in `extra`. Every caller's
/// charset excludes `/` and characters illegal in filenames on some platforms
/// (`:`), so a validated id is safe to use verbatim as a filesystem path segment
/// and object-key component. The `todo/`-tree ids (`ClientId`, `JobId`,
/// `PlanId`, `PreauthKeyId`) also exclude `.`, which is what lets `.`-delimited
/// composite filenames embed them unambiguously; [`BenchmarkId`] admits `.` and
/// adds its own leading-character rule to stay path-safe. `allowed` is the
/// human-readable charset for the error message. Returns the string back so the
/// caller can wrap it in its newtype.
fn validate_id(
    s: String,
    field: &'static str,
    allowed: &'static str,
    extra: &[char],
) -> Result<String, InvalidId> {
    if s.is_empty() {
        return Err(InvalidId::Empty { field });
    }
    if let Some(ch) = s
        .chars()
        .find(|c| !c.is_ascii_alphanumeric() && !extra.contains(c))
    {
        return Err(InvalidId::InvalidChar { field, ch, allowed });
    }
    Ok(s)
}

/// The expiry field encoded in an `avail/` filename.
/// `Never` encodes as the literal string `never`; `At` uses ISO 8601 compact
/// (`20260101T120000Z`) so the value is filename-safe without URL-encoding.
///
/// The derived `Ord` orders by urgency: an earlier `At` is "less than" a later
/// one, and every `At` is less than `Never` (variants compare in declaration
/// order). The claim handler relies on this to prefer soonest-expiring jobs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ExpiresAt {
    At(DateTime<Utc>),
    Never,
}

impl ExpiresAt {
    /// Whether this expiry lies strictly before `now`. `Never` is never
    /// expired. The boundary matches the `queue-maintenance` expiry sweep
    /// (`expires_at < now`), so the set of jobs `claim`/`reclaim` refuse to
    /// hand out is exactly the set the sweep will expire — a job expiring at
    /// precisely `now` is still claimable.
    pub fn is_expired(self, now: DateTime<Utc>) -> bool {
        match self {
            ExpiresAt::At(dt) => dt < now,
            ExpiresAt::Never => false,
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
pub struct InvalidExpiresAt(String);

impl FromStr for ExpiresAt {
    type Err = InvalidExpiresAt;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s == "never" {
            return Ok(ExpiresAt::Never);
        }
        chrono::NaiveDateTime::parse_from_str(s, "%Y%m%dT%H%M%SZ")
            .map(|ndt| ExpiresAt::At(ndt.and_utc()))
            .map_err(|_| InvalidExpiresAt(s.to_string()))
    }
}

/// Identifier for a pre-auth registration key. Same charset rules as
/// [`ClientId`] (`[A-Za-z0-9_-]`), so it is safe to interpolate verbatim into
/// the `preauth/{key_id}.json` object key, and — because it excludes `.` — is
/// unambiguous to split out of the `evk_{key_id}.{secret}` token on the `.`.
#[derive(
    Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, Display, AsRef, Into,
)]
#[serde(try_from = "String", into = "String")]
pub struct PreauthKeyId(String);

impl TryFrom<String> for PreauthKeyId {
    type Error = InvalidId;
    fn try_from(s: String) -> Result<Self, Self::Error> {
        Ok(Self(validate_id(
            s,
            "preauth_key_id",
            "[A-Za-z0-9_-]",
            &['_', '-'],
        )?))
    }
}

impl PreauthKeyId {
    /// Validating constructor. Errors on the empty string or any character
    /// outside `[A-Za-z0-9_-]` (so the id is always a safe object-key segment).
    pub fn try_new(s: impl Into<String>) -> Result<Self, InvalidId> {
        Self::try_from(s.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for ClientId {
    type Error = InvalidId;
    fn try_from(s: String) -> Result<Self, Self::Error> {
        // `_` is allowed (derived ids are `ev1_<hex>`, see
        // `crate::client::derive_client_id`); the `.`-delimited names that
        // embed a client_id stay unambiguous because `.` is outside this
        // charset, so it never appears inside the id itself.
        Ok(Self(validate_id(
            s,
            "client_id",
            "[A-Za-z0-9_-]",
            &['_', '-'],
        )?))
    }
}

/// Human-readable form of the [`BenchmarkId`] grammar, for error messages.
const BENCHMARK_ID_ALLOWED: &str = "[A-Za-z0-9][A-Za-z0-9_.-]*";

impl TryFrom<String> for BenchmarkId {
    type Error = InvalidId;
    fn try_from(s: String) -> Result<Self, Self::Error> {
        // Alone among the id types, this charset admits `.`, because catalog ids
        // carry version numbers — `eval_ifbench_2026.06.1`. The leading
        // character is confined to alphanumerics to pay for that: it means no id
        // can *be* a relative path segment (`.` or `..`), so a value used as a
        // path or key component cannot climb out of its prefix, and hidden-file
        // (`.foo`) and flag-lookalike (`-foo`) names are excluded by the same
        // rule. A `.` further in is inert — every use site either prefixes the
        // id (`benchmark_id=`) or suffixes it (`.toml`).
        match s.chars().next() {
            None => {
                return Err(InvalidId::Empty {
                    field: "benchmark_id",
                });
            }
            Some(ch) if !ch.is_ascii_alphanumeric() => {
                return Err(InvalidId::InvalidChar {
                    field: "benchmark_id",
                    ch,
                    allowed: BENCHMARK_ID_ALLOWED,
                });
            }
            Some(_) => {}
        }
        Ok(Self(validate_id(
            s,
            "benchmark_id",
            BENCHMARK_ID_ALLOWED,
            &['_', '.', '-'],
        )?))
    }
}

impl ClientId {
    /// Validating constructor. Errors on the empty string or any character
    /// outside `[A-Za-z0-9_-]` (so the id is always a safe path segment).
    pub fn try_new(s: impl Into<String>) -> Result<Self, InvalidId> {
        Self::try_from(s.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl BenchmarkId {
    /// Validating constructor. Errors on the empty string, on a first character
    /// outside `[A-Za-z0-9]`, or on any later character outside
    /// `[A-Za-z0-9_.-]` — so the id is always a safe path segment, and never a
    /// relative one.
    pub fn try_new(s: impl Into<String>) -> Result<Self, InvalidId> {
        Self::try_from(s.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl JobId {
    /// Prefix on every server-minted id, so a bare `job_id` string is
    /// recognizable at a glance (in logs, filenames, warehouse rows) as a job id
    /// rather than some other id — mirroring `client_id`'s `ev1_` prefix. Uses a
    /// hyphen, not an underscore, because `_` is not in the job-id charset (it
    /// is the `todo/` filename/marker delimiter — see [`JobId::try_new`]).
    pub const MINT_PREFIX: &'static str = "job-";

    /// Infallible constructor for server-minted ids. A [`Uuid`] is
    /// `[A-Za-z0-9-]` by construction and [`MINT_PREFIX`](Self::MINT_PREFIX) is
    /// too, so the resulting `job-{uuid}` is always a safe filesystem path
    /// segment / object-key component — no validation (and no panic) needed.
    ///
    /// Mint with `Uuid::now_v7` (time-ordered), not `new_v4`: `avail/` keys are
    /// `job-{uuid}.{expires_at}` and `queue-maintenance`'s new-job cursor relies
    /// on them sorting in arrival order (see `docs/planner.md`). A random v4
    /// would break that ordering and the cursor with it.
    ///
    /// The prefix is a **minting convention**, not an invariant `try_new`
    /// enforces: client-echoed ids and readable test fixtures need not carry it
    /// (they only need the safe charset), so the prefix labels real ids without
    /// forcing every fixture to be a UUID.
    pub fn from_uuid(id: uuid::Uuid) -> Self {
        Self(format!("{}{}", Self::MINT_PREFIX, id))
    }

    /// Validating constructor for any id built from a **string** — untrusted
    /// input (a client-supplied `job_id`, e.g. a URL path segment) and values
    /// **reconstructed from stored data** (filenames, Parquet fields). Errors on
    /// the empty string or any character outside `[A-Za-z0-9-]`, so the id is
    /// always a safe filesystem path segment / object-key component — closing
    /// path-traversal (`..`, `/`) and S3 key-injection vectors in the stores that
    /// interpolate it into keys (`avail/{job_id}.…`,
    /// `leased/{client_id}/{job_id}.…`, `incoming/{job_id}`). `.` is rejected
    /// because it is the delimiter in those `{job_id}.{…}` names and in
    /// `denied/{job_id}.{client_id}` markers, and a `job_id` containing `.`
    /// would make those splits ambiguous; unlike `client_id`, `_` is rejected
    /// too (the charset is `[A-Za-z0-9-]`). On the reconstruction path a
    /// corrupt value fails closed rather than producing a dangerous id. UUIDs
    /// satisfy all of this by construction.
    pub fn try_new(s: impl Into<String>) -> Result<Self, InvalidId> {
        Ok(Self(validate_id(
            s.into(),
            "job_id",
            "[A-Za-z0-9-]",
            &['-'],
        )?))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl PlanId {
    /// Prefix on every server-minted plan id — `plan-{uuid}` — mirroring
    /// [`JobId::MINT_PREFIX`]. A hyphen, not an underscore, because `_` is
    /// outside the id charset.
    pub const MINT_PREFIX: &'static str = "plan-";

    /// Infallible constructor for server-minted ids. Mint with `Uuid::now_v7`
    /// (time-ordered), matching [`JobId::from_uuid`]; a [`Uuid`] plus the
    /// [`MINT_PREFIX`](Self::MINT_PREFIX) is `[A-Za-z0-9-]` by construction, so
    /// the resulting `plan-{uuid}` is always a safe object-key component — no
    /// validation (and no panic) needed.
    pub fn from_uuid(id: uuid::Uuid) -> Self {
        Self(format!("{}{}", Self::MINT_PREFIX, id))
    }

    /// Validating constructor for a plan id reconstructed from a stored object
    /// key. Errors on the empty string or any character outside `[A-Za-z0-9-]`,
    /// so a corrupt key fails closed rather than producing a path-traversal /
    /// key-injection id. UUIDs satisfy this by construction.
    pub fn try_new(s: impl Into<String>) -> Result<Self, InvalidId> {
        Ok(Self(validate_id(
            s.into(),
            "plan_id",
            "[A-Za-z0-9-]",
            &['-'],
        )?))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Test-only permissive constructor. Production code cannot create an
/// unvalidated `JobId` (a `/`- or `..`-bearing id would be a path-traversal /
/// key-injection vector) — that boundary is enforced by the compiler, not by
/// discipline. Integration tests (which compile the lib without `cfg(test)`)
/// can't see this; they build ids through a `job()` test helper backed by
/// [`JobId::try_new`].
#[cfg(test)]
impl JobId {
    pub fn new_unchecked(s: impl Into<String>) -> Self {
        Self(s.into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;
    use serde_json::json;

    #[test]
    fn benchmark_id_rejects_empty_on_deserialize() {
        let err = serde_json::from_value::<BenchmarkId>(json!(""))
            .expect_err("empty benchmark_id must be rejected");
        assert!(err.to_string().contains("benchmark_id must not be empty"));
    }

    #[test]
    fn client_id_rejects_empty_on_deserialize() {
        let err = serde_json::from_value::<ClientId>(json!(""))
            .expect_err("empty client_id must be rejected");
        assert!(err.to_string().contains("client_id must not be empty"));
    }

    /// Expected outcome of `ClientId::try_new` for a given input.
    enum Expect {
        Valid,
        Empty,
        BadChar,
    }

    /// `client_id` must be a safe filesystem path segment (`[A-Za-z0-9_-]`):
    /// derived `ev1_<hex>` ids and reasonable ids are accepted; empty and
    /// path-significant / non-portable characters are rejected so they can
    /// never reach a `todo/` path.
    #[rstest]
    #[case("ev1_0a1b2c", Expect::Valid)]
    #[case("client-1", Expect::Valid)]
    #[case("org_team_device", Expect::Valid)]
    #[case("C2", Expect::Valid)]
    #[case("", Expect::Empty)]
    #[case("ev1:abcd", Expect::BadChar)]
    #[case("a/b", Expect::BadChar)]
    #[case("a.b", Expect::BadChar)]
    #[case("../etc", Expect::BadChar)]
    #[case("has space", Expect::BadChar)]
    #[case("tab\tx", Expect::BadChar)]
    fn client_id_charset(#[case] input: &str, #[case] expect: Expect) {
        let result = ClientId::try_new(input);
        match expect {
            Expect::Valid => assert!(result.is_ok(), "{input:?} should be valid"),
            Expect::Empty => assert!(
                matches!(result, Err(InvalidId::Empty { .. })),
                "{input:?} should be rejected as empty"
            ),
            Expect::BadChar => assert!(
                matches!(result, Err(InvalidId::InvalidChar { .. })),
                "{input:?} should be rejected for an invalid character"
            ),
        }
    }

    /// `JobId::try_new` is the validating constructor for string-sourced ids
    /// (client input + storage reconstruction). It must reject the empty string,
    /// any path-significant / non-portable character (so a `job_id` can never
    /// traverse or inject into a `todo/`-tree path), including `.` (so
    /// `.`-delimited filenames and markers split unambiguously). UUIDs and safe
    /// fixtures pass.
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

    /// `benchmark_id` is the one id type admitting `.`, since catalog ids carry
    /// version numbers. The leading character stays alphanumeric, which is what
    /// keeps the value from ever *being* a relative path segment — the property
    /// the other types get for free by excluding `.` outright. Ids from the
    /// shipped `examples/benchmarks/` catalog are the accepting cases.
    #[rstest]
    #[case("prefill_throughput_256", Expect::Valid)]
    #[case("eval_ifbench_2026.06.1", Expect::Valid)]
    #[case("vl_max_memory_256x256_text1024_img1", Expect::Valid)]
    #[case("end_to_end_latency_4096_256", Expect::Valid)]
    #[case("2026_first_char_may_be_a_digit", Expect::Valid)]
    #[case("", Expect::Empty)]
    // A leading dot would make the id a relative path segment, or a hidden file.
    #[case(".", Expect::BadChar)]
    #[case("..", Expect::BadChar)]
    #[case(".hidden", Expect::BadChar)]
    // A leading dash reads as a flag to anything shelling out.
    #[case("-flag", Expect::BadChar)]
    #[case("_leading_underscore", Expect::BadChar)]
    // Path separators and non-portable characters stay out at any position.
    #[case("a/b", Expect::BadChar)]
    #[case("../etc/passwd", Expect::BadChar)]
    #[case("a:b", Expect::BadChar)]
    #[case("has space", Expect::BadChar)]
    #[case("tab\tx", Expect::BadChar)]
    fn benchmark_id_charset(#[case] input: &str, #[case] expect: Expect) {
        let result = BenchmarkId::try_new(input);
        match expect {
            Expect::Valid => assert!(result.is_ok(), "{input:?} should be valid"),
            Expect::Empty => assert!(
                matches!(result, Err(InvalidId::Empty { .. })),
                "{input:?} should be rejected as empty"
            ),
            Expect::BadChar => assert!(
                matches!(result, Err(InvalidId::InvalidChar { .. })),
                "{input:?} should be rejected for an invalid character"
            ),
        }
    }

    /// Every id in the shipped catalog must survive the validator, since the
    /// catalog loader reconstructs a `BenchmarkId` from each filename — a
    /// rejection there would drop a benchmark from the served catalog.
    #[test]
    fn shipped_catalog_ids_are_all_valid() -> anyhow::Result<()> {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/benchmarks");
        let mut checked = 0usize;
        for entry in std::fs::read_dir(&dir)? {
            let path = entry?.path();
            if path.extension().is_none_or(|ext| ext != "toml") {
                continue;
            }
            let stem = path
                .file_stem()
                .and_then(|s| s.to_str())
                .ok_or_else(|| anyhow::anyhow!("unreadable filename: {}", path.display()))?;
            assert!(
                BenchmarkId::try_new(stem).is_ok(),
                "shipped catalog id {stem:?} must validate"
            );
            checked += 1;
        }
        assert!(
            checked > 0,
            "no catalog fixtures found in {}",
            dir.display()
        );
        Ok(())
    }

    #[test]
    fn job_id_from_uuid_prefixes_and_is_charset_safe() {
        let id = JobId::from_uuid(uuid::Uuid::nil());
        // Minted ids carry the `job-` label...
        assert!(id.as_str().starts_with(JobId::MINT_PREFIX));
        // ...and are charset-safe by construction, so `try_new` accepts them.
        assert!(JobId::try_new(id.as_str()).is_ok());
    }

    #[test]
    fn plan_id_from_uuid_prefixes_and_is_charset_safe() {
        let id = PlanId::from_uuid(uuid::Uuid::nil());
        assert!(id.as_str().starts_with(PlanId::MINT_PREFIX));
        // Charset-safe by construction, so `try_new` accepts a minted id.
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
        // Deserialize stays transparent (job bodies are server-controlled); the
        // safety boundary is `try_new` at client-facing ingest, not serde.
        assert!(serde_json::from_value::<JobId>(json!("job-1")).is_ok());
    }

    #[test]
    fn expires_at_never_roundtrip() {
        let s = ExpiresAt::Never.to_string();
        assert_eq!(s, "never");
        assert_eq!(s.parse::<ExpiresAt>().unwrap(), ExpiresAt::Never);
    }

    #[test]
    fn expires_at_at_roundtrip() {
        use chrono::TimeZone;
        let dt = Utc.with_ymd_and_hms(2026, 1, 1, 12, 0, 0).unwrap();
        let v = ExpiresAt::At(dt);
        let s = v.to_string();
        assert_eq!(s, "20260101T120000Z");
        assert_eq!(s.parse::<ExpiresAt>().unwrap(), ExpiresAt::At(dt));
    }

    #[test]
    fn expires_at_epoch_boundary() {
        use chrono::TimeZone;
        let dt = Utc.with_ymd_and_hms(1970, 1, 1, 0, 0, 0).unwrap();
        let s = ExpiresAt::At(dt).to_string();
        assert_eq!(s, "19700101T000000Z");
        assert_eq!(s.parse::<ExpiresAt>().unwrap(), ExpiresAt::At(dt));
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
        // Soonest-expiring sorts first; Never sorts last.
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
        // Never is never expired.
        assert!(!ExpiresAt::Never.is_expired(now));
        // A job expiring at precisely `now` is still claimable (strict `<`),
        // matching the queue-maintenance sweep boundary.
        assert!(!ExpiresAt::At(now).is_expired(now));
    }
}
