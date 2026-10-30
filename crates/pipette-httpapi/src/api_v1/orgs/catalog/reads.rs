//! The organization benchmark reads the organization overview counts with.
//! Reach goes through [`crate::api_v1::orgs::catalog`], as the JSON routes'
//! does.

use pipette_plan_types::OrgId;

use crate::api_v1::orgs::catalog::read_reach;
use crate::auth::Caller;
use crate::error::AppError;
use crate::state::AppState;

/// How many private benchmarks the organization keeps, as the caller reads
/// them: none outside its read reach. An entry that does not read still counts.
pub async fn count_org_benchmarks(
    state: &AppState,
    caller: Caller<'_>,
    org_id: &OrgId,
) -> Result<usize, AppError> {
    if !read_reach(state, caller).await?.covers(Some(org_id)) {
        return Ok(0);
    }
    let kept = state
        .catalog_store
        .list_benchmark_ids_of(org_id)
        .await
        .map_err(|e| AppError::internal(e.context("failed to list the private benchmarks")))?;
    Ok(kept.len())
}
