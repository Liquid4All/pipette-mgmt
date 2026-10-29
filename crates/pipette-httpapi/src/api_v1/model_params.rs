//! `/api/v1/model-params`: the model parameter table, one row of parameter counts per
//! model, and the fill that puts them on result rows the warehouse left empty.

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use chrono::Utc;

use pipette_core::model_params::ModelParams;
use pipette_core::vault::VaultResource;
use pipette_pipeline::model_params::{ModelParamTable, model_key, normalize};
use pipette_pipeline::plan_results::ResultRow;
use pipette_plan_types::{
    AuditTarget, Changed, CreateModelParamsRequest, ListPage, ModelKey, ModelParamsDetail,
    NonEmptyTrimmedString, PositiveCount, UpdateModelParamsRequest, VaultEntry, VaultEntryWithToml,
    VaultResourceId,
};
use pipette_store::{drain_model_params, warn_unreadable_model_params};

use crate::audit::Audit;
use crate::auth::{ActiveUser, Caller, HumanUser};
use crate::error::AppError;
use crate::extract::{ApiJson, ApiPath, Parsed};
use crate::owner::HandOffScope;
use crate::state::AppState;
use crate::vault_access::VaultAuthority;

// GET /api/v1/model-params
pub async fn list_model_params(
    State(state): State<AppState>,
    _: ActiveUser,
) -> Result<Json<ListPage<pipette_plan_types::ModelParams>>, AppError> {
    let listing = drain_model_params(&*state.model_params_store)
        .await
        .map_err(AppError::internal)?;
    warn_unreadable_model_params(&listing.unreadable);
    Ok(Json(ListPage::whole(
        listing.items.into_iter().map(Into::into).collect(),
        listing.unreadable.len(),
    )))
}

// POST /api/v1/model-params
pub async fn create_model_params(
    State(state): State<AppState>,
    caller: HumanUser,
    audit: Audit,
    ApiJson(body): ApiJson<CreateModelParamsRequest>,
) -> Result<impl IntoResponse, AppError> {
    let key = ModelKey::try_new(normalize(body.model_key.trim()))
        .map_err(|e| AppError::BadRequest(format!("model_key {:?}: {e}", body.model_key)))?;
    involve_resource(&state, &audit, &key).await;
    require_writer(&state, &caller, &key).await?;
    if existing(&state, &key).await?.is_some() {
        return Err(AppError::Conflict(format!(
            "model {key} already has parameter counts; edit its row"
        )));
    }
    let params = row(
        &caller,
        key,
        body.total_millions,
        body.active_millions,
        body.note,
    )?;
    let created = write(&state, &caller, params).await?;
    Ok((StatusCode::CREATED, Json(created)))
}

// PUT /api/v1/model-params/{model_key}
pub async fn update_model_params(
    State(state): State<AppState>,
    caller: HumanUser,
    audit: Audit,
    ApiPath(Parsed(key)): ApiPath<Parsed<ModelKey>>,
    ApiJson(body): ApiJson<UpdateModelParamsRequest>,
) -> Result<Json<Changed<pipette_plan_types::ModelParams>>, AppError> {
    require_writer(&state, &caller, &key).await?;
    let Some(stored) = existing(&state, &key).await? else {
        return Err(AppError::NotFound(format!(
            "model {key} has no parameter counts"
        )));
    };
    let params = row(
        &caller,
        key,
        body.total_millions,
        body.active_millions,
        body.note,
    )?;
    // Nothing is written for the values already stored, so the row keeps the
    // author and time of the edit that set them.
    let changed = !same_values(&params, &stored);
    audit.changed(changed);
    let record = if changed {
        write(&state, &caller, params).await?
    } else {
        stored.into()
    };
    Ok(Json(Changed { record, changed }))
}

// DELETE /api/v1/model-params/{model_key}
pub async fn delete_model_params(
    State(state): State<AppState>,
    caller: HumanUser,
    audit: Audit,
    ApiPath(Parsed(key)): ApiPath<Parsed<ModelKey>>,
) -> Result<StatusCode, AppError> {
    require_writer(&state, &caller, &key).await?;
    // A row that fails to read still deletes, and its event is recorded.
    let present = match existing(&state, &key).await {
        Ok(row) => row.is_some(),
        Err(e) => {
            tracing::warn!(key = %key, error = %e,
                "model params row unreadable before its delete; the delete is recorded");
            true
        }
    };
    audit.changed(present);
    state
        .model_params_store
        .delete_model_params(&key)
        .await
        .map_err(AppError::internal)?;
    tracing::info!(key = %key, by = %caller.user.user_id, "model params deleted");
    Ok(StatusCode::NO_CONTENT)
}

/// The vault resource a new row is keyed by, so its owner's log lists it; an
/// edit's path names it.
async fn involve_resource(state: &AppState, audit: &Audit, key: &ModelKey) {
    if let Ok(resource_id) = VaultResourceId::try_new(key.as_str()) {
        audit
            .involve(state, [AuditTarget::VaultResource { resource_id }])
            .await;
    }
}

/// Refuse unless `caller` may write the row under `key`: a server
/// administrator any, an organization's administrator the one keyed by a
/// vault file the organization owns.
async fn require_writer(
    state: &AppState,
    caller: &HumanUser,
    key: &ModelKey,
) -> Result<(), AppError> {
    let authority = VaultAuthority::resolve(
        state,
        Caller {
            user: &caller.user,
            rights: &caller.rights,
        },
    )
    .await?;
    if authority.is_server() {
        return Ok(());
    }
    let owned = match VaultResourceId::try_new(key.as_str()) {
        Ok(resource_id) => state
            .vault_store
            .get_resource(&resource_id)
            .await
            .map_err(AppError::internal)?
            .is_some_and(|resource| authority.may_set_model_params(&resource.owner)),
        Err(_) => false,
    };
    if owned {
        return Ok(());
    }
    // One answer for a name, another owner's file and no file: it tells a
    // caller nothing about a vault it cannot read.
    tracing::warn!(key = %key, by = %caller.user.user_id,
        "refused: the model-params row is outside the caller's scope");
    Err(AppError::Forbidden(
        "writing this row needs manage_any_orgs: its key is not a vault resource your \
         organization owns"
            .into(),
    ))
}

/// The counts a result of the vault model `resource` belongs to takes, and
/// whether `authority` may set them. `None` for a runtime.
pub(crate) async fn of_vault_model(
    state: &AppState,
    authority: &VaultAuthority,
    resource: &VaultResource,
    entry: Option<&VaultEntryWithToml>,
) -> Result<Option<ModelParamsDetail>, AppError> {
    let name = match entry.map(|e| &e.entry) {
        Some(VaultEntry::Runtime(_)) => return Ok(None),
        Some(VaultEntry::Model(model)) => model_key(model),
        // A record from before uploads is named by a plan on its own.
        None => Some(normalize(resource.resource_id.as_ref())),
    };
    let Some(key) = name.and_then(|name| ModelKey::try_new(name).ok()) else {
        return Ok(None);
    };
    let row = existing(state, &key).await?;
    Ok(Some(ModelParamsDetail {
        model_key: key,
        row: row.map(Into::into),
        may_set: authority.may_set_model_params(&resource.owner),
    }))
}

/// [`of_vault_model`] for one row of a listing, `None` where the store would
/// not answer: one row is one cell, not the page.
pub(crate) async fn listed_of_vault_model(
    state: &AppState,
    authority: &VaultAuthority,
    resource: &VaultResource,
    entry: Option<&VaultEntryWithToml>,
) -> Option<ModelParamsDetail> {
    of_vault_model(state, authority, resource, entry)
        .await
        .inspect_err(|e| {
            tracing::warn!(resource_id = %resource.resource_id, error = %e,
                "failed to read a vault model's params for the listing");
        })
        .ok()
        .flatten()
}

async fn existing(state: &AppState, key: &ModelKey) -> Result<Option<ModelParams>, AppError> {
    state
        .model_params_store
        .get_model_params(key)
        .await
        .map_err(AppError::internal)
}

/// The row `caller` asks for, checked and not yet written.
fn row(
    caller: &HumanUser,
    key: ModelKey,
    total_millions: PositiveCount,
    active_millions: Option<PositiveCount>,
    note: Option<NonEmptyTrimmedString>,
) -> Result<ModelParams, AppError> {
    ModelParams::new(
        key,
        total_millions,
        active_millions,
        note,
        caller.user.user_id.clone(),
        Utc::now(),
    )
    .map_err(|e| AppError::BadRequest(e.to_string()))
}

/// Whether `asked` holds `stored`'s key, counts and note, whoever wrote either
/// and when. Destructured, so a new field has to be weighed here.
fn same_values(asked: &ModelParams, stored: &ModelParams) -> bool {
    let ModelParams {
        key,
        total_millions,
        active_millions,
        note,
        updated_at: _,
        updated_by: _,
    } = asked;
    (key, *total_millions, *active_millions, note)
        == (
            &stored.key,
            stored.total_millions,
            stored.active_millions,
            &stored.note,
        )
}

async fn write(
    state: &AppState,
    caller: &HumanUser,
    params: ModelParams,
) -> Result<pipette_plan_types::ModelParams, AppError> {
    state
        .model_params_store
        .put_model_params(&params)
        .await
        .map_err(AppError::internal)?;
    tracing::info!(
        key = %params.key,
        total_millions = params.total_millions.into_inner(),
        active_millions = params.active_millions.into_inner(),
        by = %caller.user.user_id,
        "model params written"
    );
    Ok(params.into())
}

/// Fill the rows' empty parameter counts from the table. A failed read is
/// logged and leaves the rows as they are, so the results still answer.
pub(crate) async fn fill_model_params<'a>(
    state: &AppState,
    rows: impl Iterator<Item = &'a mut ResultRow>,
) {
    let empty: Vec<_> = rows
        .filter(|row| row.model_params_total_millions.is_none())
        .collect();
    if empty.is_empty() {
        return;
    }
    let table = match ModelParamTable::load(state.model_params_store.as_ref()).await {
        Ok(table) => table,
        Err(e) => {
            tracing::warn!(error = %format!("{e:#}"),
                "could not read model params; result rows keep their stored counts");
            return;
        }
    };
    empty
        .into_iter()
        .for_each(|row| row.fill_model_params(&table));
}
