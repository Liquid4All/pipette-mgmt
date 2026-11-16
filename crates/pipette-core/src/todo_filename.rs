use chrono::{DateTime, Utc};

use pipette_plan_types::{ClientId, ExpiresAt, JobId, PlanId};

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

/// One parsed `leased/` entry: everything `TodoStore::delete_lease` needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lease {
    pub job_id: JobId,
    pub expires_at: ExpiresAt,
    pub client_id: ClientId,
    pub lease_expiry: DateTime<Utc>,
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;
    use rstest::rstest;

    use super::*;

    fn dt(y: i32, mo: u32, d: u32, h: u32, m: u32, s: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(y, mo, d, h, m, s).unwrap()
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
}
