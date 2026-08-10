use crate::types::{ClientId, ExpiresAt, JobId};
use chrono::{DateTime, NaiveDateTime, Utc};

const JSON: &str = ".json";

pub fn avail_filename(job_id: &JobId, expires_at: ExpiresAt) -> String {
    format!("{}.{}{}", job_id, expires_at, JSON)
}

/// Staging filename for a job body awaiting promotion: `{job_id}.json` under
/// `tmp/`. Keyed by `job_id` alone — unlike [`avail_filename`], it carries no
/// `expires_at`, because the expiry that shapes the `avail/` name is applied
/// only when `promote_avail` renames the body out of `tmp/`.
pub fn tmp_filename(job_id: &JobId) -> String {
    format!("{job_id}{JSON}")
}

pub fn parse_avail_filename(name: &str) -> anyhow::Result<(JobId, ExpiresAt)> {
    let stem = name
        .strip_suffix(JSON)
        .ok_or_else(|| anyhow::anyhow!("avail filename missing .json suffix: {name:?}"))?;
    // job_id and expires_at both exclude `.` (their charsets/encodings reject
    // it), so the single `.` is an unambiguous delimiter.
    let (job_str, expires_str) = stem
        .split_once('.')
        .ok_or_else(|| anyhow::anyhow!("avail filename missing '.' separator: {name:?}"))?;
    let job_id =
        JobId::try_new(job_str).map_err(|e| anyhow::anyhow!("invalid job_id in {name:?}: {e}"))?;
    let expires_at: ExpiresAt = expires_str
        .parse()
        .map_err(|e| anyhow::anyhow!("invalid expires_at in {name:?}: {e}"))?;
    Ok((job_id, expires_at))
}

/// Eligible-marker filename: `{job_id}.{expires_at}` (no `.json` suffix; the
/// markers are empty files). The encoded `expires_at` mirrors the job's
/// `avail/` entry so the claim handler can rank candidates and address the
/// `avail/` rename target straight from the per-client index, with no secondary
/// `avail/` scan. `queue-maintenance` (the sole writer of `eligible/`) derives
/// this expiry from the `avail/` filename, and a job's `expires_at` is
/// immutable, so the two encodings never diverge in normal operation.
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

/// Parse a denied-marker filename: `{job_id}.{client_id}` (no `.json` suffix;
/// the markers are empty files). `job_id` and `client_id` both exclude `.`
/// (their charsets reject it), so the single `.` is an unambiguous separator —
/// a `client_id` containing `_` is no parsing hazard.
pub fn parse_denied_marker(name: &str) -> anyhow::Result<(JobId, ClientId)> {
    let (job_str, client_str) = name
        .split_once('.')
        .ok_or_else(|| anyhow::anyhow!("denied marker missing '.' separator: {name:?}"))?;
    let job_id =
        JobId::try_new(job_str).map_err(|e| anyhow::anyhow!("invalid job_id in {name:?}: {e}"))?;
    let client_id = ClientId::try_new(client_str)
        .map_err(|e| anyhow::anyhow!("invalid client_id in {name:?}: {e}"))?;
    Ok((job_id, client_id))
}

/// Pending-reindex flag filename: `{client_id}.{uuid}` (empty file). Every
/// call mints a fresh v7 uuid, so each write creates a *distinct* key rather
/// than overwriting — the reindex pass consumes flags by deleting exactly the
/// keys it captured before rebuilding, and a distinct key per request is what
/// keeps a flag written mid-rebuild (a racing profile change) out of that
/// capture, so it survives the run and re-triggers on the next one.
pub fn pending_reindex_filename(client_id: &ClientId) -> String {
    format!("{}.{}", client_id, uuid::Uuid::now_v7())
}

/// Parse a pending-reindex flag filename back to its [`ClientId`]. The name is
/// `{client_id}.{uuid}`; `client_id` excludes `.` and the minted v7 uuid
/// contains none, so the single `.` splits the client id from its nonce.
pub fn parse_pending_reindex_filename(name: &str) -> anyhow::Result<ClientId> {
    let (client_str, _nonce) = name
        .rsplit_once('.')
        .ok_or_else(|| anyhow::anyhow!("pending-reindex flag missing '.' separator: {name:?}"))?;
    ClientId::try_new(client_str).map_err(|e| anyhow::anyhow!("invalid client_id in {name:?}: {e}"))
}

/// Relative key for a lease, partitioned by client:
/// `{client_id}/{job_id}.{lease_expiry}.json`. Putting `client_id` in its own
/// path segment lets `heartbeat`/`reclaim` (which know their own client) list a
/// single `leased/{client_id}/` prefix instead of scanning the whole tree, and
/// — because the leaf holds only `job_id` (no `.`) and the compact timestamp (no
/// `.`) — the leaf parses on a single `.`, so a `client_id` containing `_` is
/// not a parsing hazard.
pub fn leased_key(job_id: &JobId, client_id: &ClientId, lease_expiry: DateTime<Utc>) -> String {
    format!(
        "{}/{}.{}{JSON}",
        client_id,
        job_id,
        lease_expiry.format("%Y%m%dT%H%M%SZ"),
    )
}

pub fn parse_leased_key(key: &str) -> anyhow::Result<(JobId, ClientId, DateTime<Utc>)> {
    // `{client_id}/{job_id}.{lease_expiry}.json`. client_id is the first path
    // segment; the leaf splits on its single '.'.
    let (client_str, leaf) = key
        .split_once('/')
        .ok_or_else(|| anyhow::anyhow!("leased key missing '/' separator: {key:?}"))?;
    let client_id = ClientId::try_new(client_str)
        .map_err(|e| anyhow::anyhow!("invalid client_id in {key:?}: {e}"))?;

    let stem = leaf
        .strip_suffix(JSON)
        .ok_or_else(|| anyhow::anyhow!("leased key missing .json suffix: {key:?}"))?;
    let (job_str, expiry_str) = stem
        .split_once('.')
        .ok_or_else(|| anyhow::anyhow!("leased key missing '.' separator: {key:?}"))?;
    let job_id =
        JobId::try_new(job_str).map_err(|e| anyhow::anyhow!("invalid job_id in {key:?}: {e}"))?;
    let lease_expiry = NaiveDateTime::parse_from_str(expiry_str, "%Y%m%dT%H%M%SZ")
        .map(|ndt| ndt.and_utc())
        .map_err(|_| anyhow::anyhow!("invalid lease_expiry {expiry_str:?} in {key:?}"))?;
    Ok((job_id, client_id, lease_expiry))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use rstest::rstest;

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
    }

    // ── filename round-trips ───────────────────────────────────────────────

    #[rstest]
    #[case(JobId::new_unchecked("abc123"), ExpiresAt::Never, "abc123.never.json")]
    #[case(
        JobId::new_unchecked("some-job-id"),
        ExpiresAt::At(dt(2026, 1, 1, 12, 0, 0)),
        "some-job-id.20260101T120000Z.json"
    )]
    #[case(
        JobId::new_unchecked("x"),
        ExpiresAt::At(dt(1970, 1, 1, 0, 0, 0)),
        "x.19700101T000000Z.json"
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
    // in its own path segment, so the leaf still splits on a single `.`.
    #[rstest]
    #[case(
        JobId::new_unchecked("job1"),
        "client1",
        dt(2026, 6, 15, 8, 30, 0),
        "client1/job1.20260615T083000Z.json"
    )]
    #[case(
        JobId::new_unchecked("job2"),
        "org_team_device",
        dt(2026, 6, 15, 0, 0, 0),
        "org_team_device/job2.20260615T000000Z.json"
    )]
    #[case(
        JobId::new_unchecked("j"),
        "c",
        dt(1970, 1, 1, 0, 0, 0),
        "c/j.19700101T000000Z.json"
    )]
    fn leased_roundtrip(
        #[case] job: JobId,
        #[case] client: &str,
        #[case] expiry: DateTime<Utc>,
        #[case] want: &str,
    ) -> anyhow::Result<()> {
        let client = ClientId::try_new(client)?;
        let key = leased_key(&job, &client, expiry);
        assert_eq!(key, want);
        assert_eq!(parse_leased_key(&key)?, (job, client, expiry));
        Ok(())
    }

    // The second case proves a `client_id` with underscores survives: `job_id`
    // and `client_id` both exclude `.`, so the single `.` is the separator.
    #[rstest]
    #[case("job1.client1", "job1", "client1")]
    #[case("job2.org_team_device", "job2", "org_team_device")]
    fn denied_marker_parses(
        #[case] input: &str,
        #[case] job: &str,
        #[case] client: &str,
    ) -> anyhow::Result<()> {
        let (job_id, client_id) = parse_denied_marker(input)?;
        assert_eq!(job_id, JobId::try_new(job)?);
        assert_eq!(client_id, ClientId::try_new(client)?);
        Ok(())
    }

    // ── parse error cases ──────────────────────────────────────────────────

    #[rstest]
    #[case("abc.never")] // missing .json suffix
    #[case("nodot.json")] // missing '.' separator
    #[case("job.notadate.json")] // unparseable expires_at
    fn avail_parse_rejects(#[case] input: &str) {
        assert!(parse_avail_filename(input).is_err());
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
    fn denied_marker_rejects(#[case] input: &str) {
        assert!(parse_denied_marker(input).is_err());
    }

    #[rstest]
    #[case("job.20260101T000000Z.json")] // no '/' → missing client segment
    #[case("client/job.20260101T000000Z")] // missing .json suffix
    #[case("client/nodot.json")] // missing '.' separator
    #[case("client/job.badexpiry.json")] // non-numeric expiry
    #[case("client/job.20261301T000000Z.json")] // month 13
    #[case("client/job.20260132T000000Z.json")] // day 32
    #[case("client/job.20260229T000000Z.json")] // Feb 29 in a non-leap year
    #[case("client/job.20260101T250000Z.json")] // hour 25
    #[case("client/job.20260101T006000Z.json")] // minute 60
    fn leased_parse_rejects(#[case] input: &str) {
        assert!(parse_leased_key(input).is_err());
    }
}
