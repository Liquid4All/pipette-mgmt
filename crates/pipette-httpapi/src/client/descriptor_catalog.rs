//! The descriptor catalog short list for a device.

use axum::{Json, extract::State, http::header, response::IntoResponse};

use pipette_pipeline::descriptor_names::CatalogReadScope;

use pipette_plan_types::Sha256;

use crate::api_v1::descriptor_catalog::{catalog, family_logo, short_list};
use crate::auth::{AuthenticatedClient, client_catalog_org};
use crate::error::AppError;
use crate::extract::{ApiPath, Parsed};
use crate::state::AppState;

/// The short list for the device that signs the request. A pending device
/// receives only global entries, even if an organization owns it.
pub async fn list_own_descriptor_catalog(
    State(state): State<AppState>,
    AuthenticatedClient(client): AuthenticatedClient,
) -> Result<impl IntoResponse, AppError> {
    let store = catalog(&state)?;
    let read = CatalogReadScope::own(client_catalog_org(&state, &client).await?);
    let list = short_list(&state, store, &client, read).await?;
    Ok(([(header::CACHE_CONTROL, "private, no-store")], Json(list)))
}

/// The family logo with a digest from the short list of the device that
/// signs the request.
pub async fn get_own_descriptor_family_logo(
    State(state): State<AppState>,
    AuthenticatedClient(client): AuthenticatedClient,
    ApiPath(Parsed(sha256)): ApiPath<Parsed<Sha256>>,
) -> Result<impl IntoResponse, AppError> {
    let read = CatalogReadScope::own(client_catalog_org(&state, &client).await?);
    family_logo(catalog(&state)?, &read, sha256).await
}
