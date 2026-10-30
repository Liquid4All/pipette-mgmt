use chrono::{DateTime, NaiveDateTime, Utc};

use pipette_plan_types::{ClientId, ExpiresAt, JobId, PlanId};

use crate::types::id_from_key;

const JSON: &str = ".json";

/// Relative key for an available job: `{job_id}/{expires_at}.json`, the leaf
/// being the job's official deadline. `job_id` gets its own segment because a
/// directory bucket lists only `/`-terminated prefixes, so the job-scoped reads
/// resolve in one listing.
pub fn avail_filename(job_id: &JobId, expires_at: ExpiresAt) -> String {
    format!("{job_id}/{expires_at}{JSON}")
}

/// Staging filename under `tmp/`: no `expires_at`, which `promote_avail` adds
/// when it renames the body out.
pub fn tmp_filename(job_id: &JobId) -> String {
    format!("{job_id}{JSON}")
}

/// The reason an `avail/` name does not address a job. Each variant names the
/// whole input, as the caller holds no other context for it.
#[derive(Debug, thiserror::Error)]
pub enum AvailKeyError {
    #[error("avail key {name:?} has no {sep:?} separator")]
    MissingSeparator { name: String, sep: char },
    #[error("avail key {name:?} has no .json suffix")]
    MissingSuffix { name: String },
    #[error("avail key {name:?} has an invalid job_id")]
    JobId {
        name: String,
        #[source]
        source: pipette_plan_types::IdError,
    },
    #[error("avail key {name:?} has an invalid expires_at")]
    ExpiresAt {
        name: String,
        #[source]
        source: pipette_plan_types::ExpiresAtError,
    },
}

/// Split `{job_id}{sep}{expires_at}` into its two fields. Both `avail/` forms
/// reduce to this once the `.json` suffix is off, and they differ only in `sep`.
fn split_job_and_expiry(name: &str, sep: char) -> Result<(JobId, ExpiresAt), AvailKeyError> {
    let stem = name
        .strip_suffix(JSON)
        .ok_or_else(|| AvailKeyError::MissingSuffix {
            name: name.to_string(),
        })?;
    let (job_str, expires_str) =
        stem.split_once(sep)
            .ok_or_else(|| AvailKeyError::MissingSeparator {
                name: name.to_string(),
                sep,
            })?;
    let job_id = JobId::try_new(job_str).map_err(|source| AvailKeyError::JobId {
        name: name.to_string(),
        source,
    })?;
    let expires_at = expires_str
        .parse()
        .map_err(|source| AvailKeyError::ExpiresAt {
            name: name.to_string(),
            source,
        })?;
    Ok((job_id, expires_at))
}

/// Split an `avail/` key back into its job and deadline.
/// [`avail_filename`] documents the format.
pub fn parse_avail_filename(name: &str) -> Result<(JobId, ExpiresAt), AvailKeyError> {
    split_job_and_expiry(name, '/')
}

/// Split an `avail/` name in the single-segment form `{job_id}.{expires_at}.json`
/// an earlier layout wrote, so `stores::reject_unmigrated_avail_keys` can tell a
/// real job from foreign cruft.
pub fn parse_flat_avail_filename(name: &str) -> Result<(JobId, ExpiresAt), AvailKeyError> {
    split_job_and_expiry(name, '.')
}

/// Eligible-marker filename: `{job_id}.{expires_at}`, mirroring the job's
/// `avail/` deadline so a claim can rank candidates and address the `avail/` key
/// from the per-client index alone.
///
/// Ingestion assigns a job's expiry and the `queue-maintenance` expiry pass
/// moves it (renaming within `avail/`, only once lapsed); every other path
/// carries the value across.
pub fn eligible_filename(job_id: &JobId, expires_at: ExpiresAt) -> String {
    format!("{job_id}.{expires_at}")
}

pub fn parse_eligible_filename(name: &str) -> anyhow::Result<(JobId, ExpiresAt)> {
    // job_id and expires_at both exclude `.`, so the single `.` is an
    // unambiguous delimiter — same scheme as `avail/`.
    let (job_str, expires_str) = name
        .split_once('.')
        .ok_or_else(|| anyhow::anyhow!("eligible filename missing '.' separator: {name:?}"))?;
    let job_id =
        JobId::try_new(job_str).map_err(|e| anyhow::anyhow!("invalid job_id in {name:?}: {e}"))?;
    let expires_at: ExpiresAt = expires_str
        .parse()
        .map_err(|e| anyhow::anyhow!("invalid expires_at in {name:?}: {e}"))?;
    Ok((job_id, expires_at))
}

/// Membership-marker leaf for a job's plan under `job-plans/{job_id}/`:
/// `{plan_id}.{expires_at}`, the horizon that plan holds the job under (either
/// side of the job's deadline). A plan may hold several markers on one job; its
/// effective horizon is the latest.
pub fn job_plan_marker_filename(plan_id: &PlanId, expires_at: ExpiresAt) -> String {
    format!("{plan_id}.{expires_at}")
}

/// Split a `job-plans/` marker filename back into its plan and deadline. Errors
/// are `anyhow` strings: every caller warns and skips the name as foreign cruft.
pub fn parse_job_plan_marker_filename(name: &str) -> anyhow::Result<(PlanId, ExpiresAt)> {
    let (plan_str, expires_str) = name
        .split_once('.')
        .ok_or_else(|| anyhow::anyhow!("job-plans marker missing '.' separator: {name:?}"))?;
    let plan_id = PlanId::try_new(plan_str)
        .map_err(|e| anyhow::anyhow!("invalid plan_id in {name:?}: {e}"))?;
    let expires_at: ExpiresAt = expires_str
        .parse()
        .map_err(|e| anyhow::anyhow!("invalid expires_at in {name:?}: {e}"))?;
    Ok((plan_id, expires_at))
}

/// Denied-marker filename: `{job_id}.{client_id}` (empty file). The inverse of
/// [`parse_denied_marker`], and the only place that writes the separator.
pub fn denied_marker_filename(job_id: &JobId, client_id: &ClientId) -> String {
    format!("{job_id}.{client_id}")
}

/// The prefix every denied marker for `job_id` shares, for a sweep of one job's
/// denials; `job_id` excludes `.`, so it matches that job alone.
pub fn denied_job_prefix(job_id: &JobId) -> String {
    format!("{job_id}.")
}

/// Parse a denied-marker filename `{job_id}.{client_id}`; neither id admits
/// `.`, so the single `.` is the separator.
pub fn parse_denied_marker(name: &str) -> anyhow::Result<(JobId, ClientId)> {
    let (job_str, client_str) = name
        .split_once('.')
        .ok_or_else(|| anyhow::anyhow!("denied marker missing '.' separator: {name:?}"))?;
    let job_id =
        JobId::try_new(job_str).map_err(|e| anyhow::anyhow!("invalid job_id in {name:?}: {e}"))?;
    let client_id = id_from_key(client_str, ClientId::try_new)
        .map_err(|e| anyhow::anyhow!("invalid client_id in {name:?}: {e}"))?;
    Ok((job_id, client_id))
}

/// Pending-reindex flag filename: `{client_id}.{uuid}`, a fresh key per call.
/// The reindex pass deletes exactly the keys it captured, so a flag written
/// mid-rebuild survives and re-triggers the next run.
pub fn pending_reindex_filename(client_id: &ClientId) -> String {
    format!("{}.{}", client_id, uuid::Uuid::now_v7())
}

/// Parse a pending-reindex flag filename back to its [`ClientId`].
pub fn parse_pending_reindex_filename(name: &str) -> anyhow::Result<ClientId> {
    let (client_str, _nonce) = name
        .rsplit_once('.')
        .ok_or_else(|| anyhow::anyhow!("pending-reindex flag missing '.' separator: {name:?}"))?;
    id_from_key(client_str, ClientId::try_new)
        .map_err(|e| anyhow::anyhow!("invalid client_id in {name:?}: {e}"))
}

/// Relative key for a lease,
/// `{client_id}/{job_id}.{expires_at}.{lease_expiry}.json`. The client segment
/// lets `heartbeat`/`reclaim` list one prefix; carrying the job's `expires_at`
/// lets `recycle_lease` rebuild the `avail/` key with no body read.
pub fn leased_key(
    job_id: &JobId,
    expires_at: ExpiresAt,
    client_id: &ClientId,
    lease_expiry: DateTime<Utc>,
) -> String {
    format!(
        "{}/{}.{}.{}{JSON}",
        client_id,
        job_id,
        expires_at,
        lease_expiry.format("%Y%m%dT%H%M%SZ"),
    )
}

/// One parsed `leased/` entry: everything `TodoStore::delete_lease` needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lease {
    pub job_id: JobId,
    pub expires_at: ExpiresAt,
    pub client_id: ClientId,
    pub lease_expiry: DateTime<Utc>,
}

pub fn parse_leased_key(key: &str) -> anyhow::Result<Lease> {
    // `{client_id}/{job_id}.{expires_at}.{lease_expiry}.json`. client_id is the
    // first path segment; the leaf splits on its two '.'.
    let (client_str, leaf) = key
        .split_once('/')
        .ok_or_else(|| anyhow::anyhow!("leased key missing '/' separator: {key:?}"))?;
    let client_id = id_from_key(client_str, ClientId::try_new)
        .map_err(|e| anyhow::anyhow!("invalid client_id in {key:?}: {e}"))?;

    let stem = leaf
        .strip_suffix(JSON)
        .ok_or_else(|| anyhow::anyhow!("leased key missing .json suffix: {key:?}"))?;
    let (job_str, rest) = stem
        .split_once('.')
        .ok_or_else(|| anyhow::anyhow!("leased key missing '.' separator: {key:?}"))?;
    let (expires_str, lease_expiry_str) = rest
        .split_once('.')
        .ok_or_else(|| anyhow::anyhow!("leased key missing second '.' separator: {key:?}"))?;
    let job_id =
        JobId::try_new(job_str).map_err(|e| anyhow::anyhow!("invalid job_id in {key:?}: {e}"))?;
    let expires_at: ExpiresAt = expires_str
        .parse()
        .map_err(|e| anyhow::anyhow!("invalid expires_at in {key:?}: {e}"))?;
    let lease_expiry = NaiveDateTime::parse_from_str(lease_expiry_str, "%Y%m%dT%H%M%SZ")
        .map(|ndt| ndt.and_utc())
        .map_err(|_| anyhow::anyhow!("invalid lease_expiry {lease_expiry_str:?} in {key:?}"))?;
    Ok(Lease {
        job_id,
        expires_at,
        client_id,
        lease_expiry,
    })
}

/// Lost-lease marker name inside `lost/{job_id}/`: `{client_id}.{lease_expiry}`
/// (empty file). It names one lease, so recording the same loss twice writes
/// one marker.
pub fn lost_lease_filename(client_id: &ClientId, lease_expiry: DateTime<Utc>) -> String {
    format!("{client_id}.{}", lease_expiry.format("%Y%m%dT%H%M%SZ"))
}

/// Parse a [`lost_lease_filename`] back to the client and the lease expiry.
pub fn parse_lost_lease_filename(name: &str) -> anyhow::Result<(ClientId, DateTime<Utc>)> {
    let (client_str, expiry_str) = name
        .split_once('.')
        .ok_or_else(|| anyhow::anyhow!("lost-lease marker missing '.' separator: {name:?}"))?;
    let client_id = id_from_key(client_str, ClientId::try_new)
        .map_err(|e| anyhow::anyhow!("invalid client_id in {name:?}: {e}"))?;
    let lease_expiry = NaiveDateTime::parse_from_str(expiry_str, "%Y%m%dT%H%M%SZ")
        .map(|ndt| ndt.and_utc())
        .map_err(|_| anyhow::anyhow!("invalid lease_expiry {expiry_str:?} in {name:?}"))?;
    Ok((client_id, lease_expiry))
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;
    use rstest::rstest;

    use super::*;

    fn dt(y: i32, mo: u32, d: u32, h: u32, m: u32, s: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(y, mo, d, h, m, s).unwrap()
    }

    // ── pending-reindex flag filenames ─────────────────────────────────────

    /// Minted names round-trip, including `_`-bearing client ids: the `.`
    /// nonce separator is outside the id charset, so the split is exact.
    #[rstest]
    #[case::plain("cx")]
    #[case::underscored("ev1_a3f8")]
    #[case::hyphenated("some-client-id")]
    fn pending_reindex_filename_round_trips(#[case] id: &str) -> anyhow::Result<()> {
        let client = ClientId::try_new(id)?;
        let name = pending_reindex_filename(&client);
        assert_eq!(parse_pending_reindex_filename(&name)?, client);
        Ok(())
    }

    #[test]
    fn pending_reindex_filename_rejects_foreign_names() {
        assert!(parse_pending_reindex_filename(".DS_Store").is_err());
        assert!(parse_pending_reindex_filename("").is_err());
        // No '.' separator: a bare name is not a flag.
        assert!(parse_pending_reindex_filename("cx").is_err());
        // A padded client id would trim to another client's flag.
        assert!(parse_pending_reindex_filename(" cx.0192").is_err());
    }

    // ── filename round-trips ───────────────────────────────────────────────

    #[rstest]
    #[case(JobId::new_unchecked("abc123"), ExpiresAt::Never, "abc123/never.json")]
    #[case(
        JobId::new_unchecked("some-job-id"),
        ExpiresAt::At(dt(2026, 1, 1, 12, 0, 0)),
        "some-job-id/20260101T120000Z.json"
    )]
    #[case(
        JobId::new_unchecked("x"),
        ExpiresAt::At(dt(1970, 1, 1, 0, 0, 0)),
        "x/19700101T000000Z.json"
    )]
    fn avail_roundtrip(
        #[case] job: JobId,
        #[case] exp: ExpiresAt,
        #[case] want: &str,
    ) -> anyhow::Result<()> {
        let name = avail_filename(&job, exp);
        assert_eq!(name, want);
        assert_eq!(parse_avail_filename(&name)?, (job, exp));
        Ok(())
    }

    #[test]
    fn tmp_filename_is_job_id_dot_json() {
        assert_eq!(
            tmp_filename(&JobId::new_unchecked("job-abc")),
            "job-abc.json"
        );
    }

    #[rstest]
    #[case(JobId::new_unchecked("abc123"), ExpiresAt::Never, "abc123.never")]
    #[case(
        JobId::new_unchecked("some-job-id"),
        ExpiresAt::At(dt(2026, 1, 1, 12, 0, 0)),
        "some-job-id.20260101T120000Z"
    )]
    fn eligible_roundtrip(
        #[case] job: JobId,
        #[case] exp: ExpiresAt,
        #[case] want: &str,
    ) -> anyhow::Result<()> {
        let name = eligible_filename(&job, exp);
        assert_eq!(name, want);
        assert_eq!(parse_eligible_filename(&name)?, (job, exp));
        Ok(())
    }

    // The middle case proves a `client_id` with underscores survives: it sits
    // in its own path segment, so the leaf still splits on its two `.`. The
    // last case pins `ExpiresAt::Never`, whose `never` encoding sits in the
    // same slot as a timestamp.
    #[rstest]
    #[case(
        JobId::new_unchecked("job1"),
        ExpiresAt::At(dt(2026, 9, 8, 0, 0, 0)),
        "client1",
        dt(2026, 6, 15, 8, 30, 0),
        "client1/job1.20260908T000000Z.20260615T083000Z.json"
    )]
    #[case(
        JobId::new_unchecked("job2"),
        ExpiresAt::At(dt(2026, 9, 8, 0, 0, 0)),
        "org_team_device",
        dt(2026, 6, 15, 0, 0, 0),
        "org_team_device/job2.20260908T000000Z.20260615T000000Z.json"
    )]
    #[case(
        JobId::new_unchecked("j"),
        ExpiresAt::Never,
        "c",
        dt(1970, 1, 1, 0, 0, 0),
        "c/j.never.19700101T000000Z.json"
    )]
    fn leased_roundtrip(
        #[case] job: JobId,
        #[case] expires_at: ExpiresAt,
        #[case] client: &str,
        #[case] expiry: DateTime<Utc>,
        #[case] want: &str,
    ) -> anyhow::Result<()> {
        let client = ClientId::try_new(client)?;
        let key = leased_key(&job, expires_at, &client, expiry);
        assert_eq!(key, want);
        assert_eq!(
            parse_leased_key(&key)?,
            Lease {
                job_id: job,
                expires_at,
                client_id: client,
                lease_expiry: expiry
            }
        );
        Ok(())
    }

    /// A membership marker round-trips, including the `never` encoding, which
    /// occupies the same slot as a timestamp.
    #[rstest]
    #[case("plan-abc", ExpiresAt::Never, "plan-abc.never")]
    #[case(
        "plan-0199a1c2-3d4e-7f80-9abc-def012345678",
        ExpiresAt::At(dt(2026, 9, 8, 0, 0, 0)),
        "plan-0199a1c2-3d4e-7f80-9abc-def012345678.20260908T000000Z"
    )]
    fn job_plan_marker_roundtrip(
        #[case] plan_id: &str,
        #[case] expires_at: ExpiresAt,
        #[case] want: &str,
    ) -> anyhow::Result<()> {
        let plan_id = PlanId::try_new(plan_id)?;
        let name = job_plan_marker_filename(&plan_id, expires_at);
        assert_eq!(name, want);
        assert_eq!(
            parse_job_plan_marker_filename(&name)?,
            (plan_id, expires_at)
        );
        Ok(())
    }

    #[rstest]
    #[case("nodot")] // missing '.' separator
    #[case(".never")] // empty plan_id
    #[case("plan-abc.")] // empty expires_at
    #[case("plan-abc.notadate")] // unparseable expires_at
    #[case("plan/../x.never")] // unsafe plan_id charset
    fn job_plan_marker_rejects(#[case] input: &str) {
        assert!(parse_job_plan_marker_filename(input).is_err());
    }

    // The second case proves a `client_id` with underscores survives: `job_id`
    // and `client_id` both exclude `.`, so the single `.` is the separator.
    #[rstest]
    #[case("job1.client1", "job1", "client1")]
    #[case("job2.org_team_device", "job2", "org_team_device")]
    fn denied_marker_roundtrip(
        #[case] input: &str,
        #[case] job: &str,
        #[case] client: &str,
    ) -> anyhow::Result<()> {
        let (job_id, client_id) = parse_denied_marker(input)?;
        assert_eq!(job_id, JobId::try_new(job)?);
        assert_eq!(client_id, ClientId::try_new(client)?);
        assert_eq!(denied_marker_filename(&job_id, &client_id), input);
        Ok(())
    }

    // ── parse error cases ──────────────────────────────────────────────────

    /// The last case is the single-segment form: the migration renames such a
    /// name into a job directory, and it must not read as a key of its own.
    #[rstest]
    #[case("abc/never")] // missing .json suffix
    #[case("noslash.json")] // missing '/' separator
    #[case("job/notadate.json")] // unparseable expires_at
    #[case("job.never.json")] // single-segment form
    fn avail_parse_rejects(#[case] input: &str) {
        assert!(parse_avail_filename(input).is_err());
    }

    /// The migration reads a single-segment name and rebuilds it into the
    /// per-job form, so the two parsers must agree on the fields.
    #[rstest]
    #[case("abc123.never.json", JobId::new_unchecked("abc123"), ExpiresAt::Never)]
    #[case(
        "some-job-id.20260101T120000Z.json",
        JobId::new_unchecked("some-job-id"),
        ExpiresAt::At(dt(2026, 1, 1, 12, 0, 0))
    )]
    fn flat_avail_name_rebuilds_into_the_per_job_form(
        #[case] input: &str,
        #[case] job: JobId,
        #[case] exp: ExpiresAt,
    ) -> anyhow::Result<()> {
        assert_eq!(parse_flat_avail_filename(input)?, (job.clone(), exp));
        let rebuilt = avail_filename(&job, exp);
        assert_eq!(parse_avail_filename(&rebuilt)?, (job, exp));
        Ok(())
    }

    /// A name already in the per-job form is not a migration candidate.
    #[rstest]
    #[case("abc/never.json")] // per-job form
    #[case("nodot.json")] // missing '.' separator
    #[case("job.notadate.json")] // unparseable expires_at
    fn flat_avail_parse_rejects(#[case] input: &str) {
        assert!(parse_flat_avail_filename(input).is_err());
    }

    #[rstest]
    #[case("nodot")] // missing '.' separator
    #[case("job.notadate")] // unparseable expires_at
    fn eligible_parse_rejects(#[case] input: &str) {
        assert!(parse_eligible_filename(input).is_err());
    }

    #[rstest]
    #[case("nodot")] // missing '.' separator
    #[case(".client")] // empty job_id
    #[case("job.")] // empty client_id
    #[case("job/../x.client")] // unsafe job_id charset
    #[case("job.client1 ")] // padded client_id would trim to another client's
    fn denied_marker_rejects(#[case] input: &str) {
        assert!(parse_denied_marker(input).is_err());
    }

    #[test]
    fn a_padded_marker_is_refused_as_not_verbatim() {
        let message = parse_denied_marker("job.client1 ")
            .err()
            .map(|e| format!("{e:#}"));
        assert!(
            message
                .as_deref()
                .is_some_and(|m| m.contains("does not name an id verbatim")),
            "{message:?}"
        );
    }

    #[rstest]
    #[case("job.never.20260101T000000Z.json")] // no '/' → missing client segment
    #[case("client/job.never.20260101T000000Z")] // missing .json suffix
    #[case("client/nodot.json")] // missing '.' separator
    #[case("client/job.never.json")] // only one '.' → no lease_expiry
    #[case("client/job.notadate.20260101T000000Z.json")] // unparseable expires_at
    #[case("client/job.never.badexpiry.json")] // non-numeric lease_expiry
    #[case("client/job.never.20261301T000000Z.json")] // month 13
    #[case("client/job.never.20260132T000000Z.json")] // day 32
    #[case("client/job.never.20260229T000000Z.json")] // Feb 29 in a non-leap year
    #[case("client/job.never.20260101T250000Z.json")] // hour 25
    #[case("client/job.never.20260101T006000Z.json")] // minute 60
    #[case(" client/job.never.20260101T000000Z.json")] // padded client_id
    fn leased_parse_rejects(#[case] input: &str) {
        assert!(parse_leased_key(input).is_err());
    }

    #[test]
    fn lost_lease_filename_round_trips() -> anyhow::Result<()> {
        let client = ClientId::try_new("ev1_a3f8")?;
        let expiry = dt(2026, 10, 3, 4, 50, 0);
        let name = lost_lease_filename(&client, expiry);
        assert_eq!(name, "ev1_a3f8.20261003T045000Z");
        assert_eq!(parse_lost_lease_filename(&name)?, (client, expiry));
        Ok(())
    }

    #[rstest]
    #[case(".DS_Store")]
    #[case("nodot")]
    #[case("client.notadate")]
    #[case("client.20261003T045000Z.json")]
    fn lost_lease_parse_rejects(#[case] input: &str) {
        assert!(parse_lost_lease_filename(input).is_err());
    }
}
