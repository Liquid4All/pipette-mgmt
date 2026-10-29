//! The vault reads the organization overview counts with, and the upload table
//! [`types`] serves the app.

use axum::Json;

use pipette_core::vault::VaultOwner;
use pipette_plan_types::{OrgId, VaultTypes};

use crate::api_v1::vault::types::upload_types;
use crate::auth::{ActiveUser, Caller};
use crate::error::AppError;
use crate::state::AppState;
use crate::vault_access::VaultReach;

/// How many of the organization's vault resources the caller reads.
pub async fn count_org_resources(
    state: &AppState,
    caller: Caller<'_>,
    org_id: &OrgId,
) -> Result<usize, AppError> {
    let owner = VaultOwner::Org {
        org_id: org_id.clone(),
    };
    if !VaultReach::resolve(state, caller).await?.admits(&owner) {
        return Ok(0);
    }
    let listing = state
        .vault_store
        .list_resources_of(&owner)
        .await
        .map_err(|e| AppError::internal(e.context("failed to list the vault resources")))?;
    listing.unreadable.iter().for_each(|entry| {
        tracing::warn!(%entry, "unreadable vault record in a listing");
    });
    Ok(listing.items.len())
}

// GET /api/v1/vault/types
//
// Static, so any active account reads it, as any may upload for itself.
pub async fn types(_: ActiveUser) -> Json<VaultTypes> {
    Json(upload_types())
}
