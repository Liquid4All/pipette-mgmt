//! The SQL-backed history of catalog importer runs.

use axum::{Json, extract::State};

use pipette_plan_types::{CatalogImportRuns, CatalogImportRunsQuery};

use crate::{
    auth::ActiveUser, error::AppError, extract::ApiQuery, list::list_limit, state::AppState,
};

/// The newest import and refresh runs, plus the last successful import time.
pub async fn list_catalog_import_runs(
    State(state): State<AppState>,
    _user: ActiveUser,
    ApiQuery(query): ApiQuery<CatalogImportRunsQuery>,
) -> Result<Json<CatalogImportRuns>, AppError> {
    let runs = state.catalog_import_runs.as_deref().ok_or_else(|| {
        AppError::NotFound("catalog import runs are kept only on a [database]".into())
    })?;
    let limit = list_limit(query.limit, &state.config)?;
    Ok(Json(
        runs.recent_runs(limit).await.map_err(AppError::internal)?,
    ))
}
