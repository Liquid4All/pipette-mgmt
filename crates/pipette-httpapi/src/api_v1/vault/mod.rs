//! The vault surface: uploading a private artifact's files and describing them,
//! reading the index, deleting a record. No answer names where bytes are stored,
//! and a record is immutable, as a plan carries its id in a URL.

pub mod reads;
pub mod types;

use axum::body::Body;
use axum::body::HttpBody as _;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::{Extension, Json};
use chrono::Utc;
use futures::{StreamExt, TryStreamExt, stream};

use pipette_core::vault::{VaultOwner, VaultResource};
use pipette_pipeline::vault_admin::{StoredBlob, VaultAdminError};
use pipette_pipeline::vault_alias::KnownOrigins;
use pipette_pipeline::{vault_admin, vault_alias};
use pipette_plan_types::{
    AuditTarget, ListPage, Model, Named, NonEmptyTrimmedString, ObjectState, OwnedListQuery,
    Sha256, UploadVaultResourcesRequest, VaultBlobStored, VaultEntry, VaultEntryWithToml,
    VaultPrincipalId, VaultReadQuery, VaultResourceDetail, VaultResourceId, VaultResourcesUploaded,
};
use pipette_store::{BlobBody, STORAGE_CONCURRENCY};

use crate::api_v1::campaigns::NAME_MAX_CHARS;
use crate::audit::Audit;
use crate::auth::{ActiveUser, Caller, SignedContentSha256, VaultFetcher};
use crate::error::AppError;
use crate::extract::{ApiJson, ApiPath, ApiQuery, Parsed, timed_out};
use crate::names::owner_labels;
use crate::owner::{page_within_reach, require_destination};
use crate::state::AppState;
use crate::vault_access::{VaultAuthority, VaultReach, client_may_fetch};

/// The refusal for an owner outside the caller's scope, on the files and the
/// records alike.
const OUT_OF_SCOPE: &str = "you may only upload to the vault for yourself, your organization, \
     or a member of an organization you administer";

/// Whether `resource`'s file is still stored: a stat, never a read of it,
/// which would log a missing file as the alarm-raising `vault_object_missing`.
pub(crate) async fn object_state(
    state: &AppState,
    resource: &VaultResource,
) -> anyhow::Result<ObjectState> {
    let found = state.vault_store.stat_blob(&resource.object_key).await?;
    Ok(ObjectState::of(resource.bytes, found))
}

/// [`object_state`] for one row of a listing, [`ObjectState::Unknown`] where
/// the store would not answer: one file is one cell, not the page.
pub async fn listed_object_state(state: &AppState, resource: &VaultResource) -> ObjectState {
    object_state(state, resource)
        .await
        .inspect_err(|e| {
            tracing::warn!(resource_id = %resource.resource_id, error = %format!("{e:#}"),
                "failed to stat a vault object for the listing");
        })
        .unwrap_or(ObjectState::Unknown)
}

/// The plan entry `resource` belongs to, `None` for one from before uploads.
pub(crate) fn entry_with_toml(
    resource: &VaultResource,
) -> Result<Option<VaultEntryWithToml>, AppError> {
    resource
        .entry()
        .map(VaultEntryWithToml::try_from)
        .transpose()
        .map_err(|e| AppError::Internal(format!("an entry did not render as TOML: {e}")))
}

/// [`entry_with_toml`] for one row of a listing, `None` where the entry would
/// not render: one record is one row, not the page.
fn listed_entry_with_toml(resource: &VaultResource) -> Option<VaultEntryWithToml> {
    entry_with_toml(resource)
        .inspect_err(|e| {
            tracing::warn!(resource_id = %resource.resource_id, error = %e,
                "failed to render a vault entry for the listing");
        })
        .ok()
        .flatten()
}

/// The alias of the model `entry` names ([`vault_alias`]), `None` where it
/// has none or a record read failed: the alias only labels the resource.
/// `known` spares the read of a record the caller holds.
pub(crate) async fn entry_alias(
    state: &AppState,
    resource: &VaultResource,
    entry: Option<&VaultEntryWithToml>,
    known: &KnownOrigins,
) -> Option<Model> {
    // Every file needs a HuggingFace origin, so a file without one spares the reads.
    let (Some(_), Some(VaultEntry::Model(model))) = (&resource.origin, entry.map(|e| &e.entry))
    else {
        return None;
    };
    vault_alias::alias_of(state.vault_store.as_ref(), model.clone(), known)
        .await
        .inspect_err(|e| {
            tracing::warn!(resource_id = %resource.resource_id, error = %format!("{e:#}"),
                "failed to read the vault alias");
        })
        .ok()
        .flatten()
}

/// The one bound on a name the type does not carry.
fn check_name(name: &NonEmptyTrimmedString) -> Result<(), AppError> {
    if name.chars().count() > NAME_MAX_CHARS {
        return Err(AppError::BadRequest(format!(
            "name must not exceed {NAME_MAX_CHARS} characters"
        )));
    }
    Ok(())
}

/// The record `resource_id` names, or a `404`.
pub(crate) async fn require(
    state: &AppState,
    resource_id: &VaultResourceId,
) -> Result<VaultResource, AppError> {
    Ok(vault_admin::require(state.vault_store.as_ref(), resource_id).await?)
}

/// Load a record for a **write**, refusing one this caller may not administer.
async fn for_write(
    state: &AppState,
    caller: Caller<'_>,
    resource_id: &VaultResourceId,
) -> Result<VaultResource, AppError> {
    let authority = VaultAuthority::resolve(state, caller).await?;
    let resource = require(state, resource_id).await?;
    authority.require(&resource)?;
    Ok(resource)
}

/// Refuse unless `fetcher` is entitled to `resource`: the one check the record
/// and its bytes share, so the two never answer differently.
pub(crate) async fn require_entitled(
    state: &AppState,
    fetcher: &VaultFetcher,
    resource: &VaultResource,
) -> Result<(), AppError> {
    let entitled = match fetcher {
        VaultFetcher::Client(client) => {
            tracing::Span::current()
                .record("client_id", tracing::field::display(&client.client_id));
            client_may_fetch(state, &client.client_id, &resource.owner).await?
        }
        VaultFetcher::User { user, rights } => VaultReach::from_rights(state, rights, user)
            .await?
            .admits(&resource.owner),
    };
    if entitled {
        return Ok(());
    }
    tracing::warn!(
        resource_id = %resource.resource_id,
        owner = %resource.owner.label(),
        "refused: the vault resource belongs to another principal"
    );
    Err(AppError::Forbidden(
        "this vault resource belongs to another owner".into(),
    ))
}

/// The owner and digest a blob route names, once the caller may upload for that
/// owner. Nothing is looked up first, so a `403` hides whether a digest exists.
async fn blob_target(
    state: &AppState,
    caller: Caller<'_>,
    principal_id: &str,
    sha256: &str,
) -> Result<(VaultOwner, Sha256), AppError> {
    let principal: VaultPrincipalId = principal_id
        .parse()
        .map_err(|e| AppError::BadRequest(format!("principal_id: {e}")))?;
    let sha256 = Sha256::try_new(sha256.to_string())
        .map_err(|_| AppError::BadRequest("sha256: must be 64 lowercase hex characters".into()))?;
    let owner = VaultOwner::from(principal);
    let authority = VaultAuthority::resolve(state, caller).await?;
    require_destination(state, &authority, &owner, OUT_OF_SCOPE).await?;
    Ok((owner, sha256))
}

/// Delete the record `resource_id` names, and the rest of its upload, as
/// `user`, refusing one they may not administer.
async fn delete_for_user(
    state: &AppState,
    caller: Caller<'_>,
    resource_id: &VaultResourceId,
) -> Result<(), AppError> {
    let resource = for_write(state, caller, resource_id).await?;
    let deleted = vault_admin::delete(state.vault_store.as_ref(), &resource).await?;
    deleted
        .iter()
        .for_each(|id| tracing::info!(resource_id = %id, "vault resource deleted"));
    Ok(())
}

// PUT /api/v1/vault/blobs/{principal_id}/{sha256}
//
// Streams the body, never buffered whole, and keeps it only if it hashes to
// `{sha256}`; an RFC 9421 signature's `Content-Digest` must be that same digest.
pub async fn put_blob(
    State(state): State<AppState>,
    active: ActiveUser,
    audit: Audit,
    signed: Option<Extension<SignedContentSha256>>,
    ApiPath((principal_id, sha256)): ApiPath<(String, String)>,
    body: Body,
) -> Result<impl IntoResponse, AppError> {
    let caller = active.caller();
    let (owner, sha256) = blob_target(&state, caller, &principal_id, &sha256).await?;
    // Before the already-uploaded answer too, which never reads the body.
    if let Some(Extension(SignedContentSha256(signed))) = signed
        && signed != sha256
    {
        tracing::warn!(
            signed = %signed,
            path_digest = %sha256,
            "refused: the signed Content-Digest is not the path's digest"
        );
        return Err(AppError::BadRequest(format!(
            "the signed Content-Digest sha-256 {signed} is not the path's digest {sha256}"
        )));
    }
    // A declared length past the ceiling is refused unread; `store_blob` stops
    // a body that runs past it.
    let limit = state.config.vault_blob_limit_bytes.get();
    if body.size_hint().lower() > limit {
        return Err(VaultAdminError::TooLarge { limit }.into());
    }
    let _permit = state.vault_uploads.try_acquire().map_err(|_| {
        AppError::TooManyRequests("too many vault uploads in progress; retry shortly".into())
    })?;
    let body: BlobBody = body
        .into_data_stream()
        .map_err(|e| anyhow::Error::new(UnreadBodyError(e)))
        .boxed();
    let stored = vault_admin::store_blob(state.vault_store.as_ref(), &owner, &sha256, limit, body)
        .await
        .map_err(unread_body_is_the_callers)?;
    let (status, bytes) = match stored {
        StoredBlob::Stored { bytes } => (StatusCode::CREATED, bytes),
        StoredBlob::AlreadyThere { bytes } => {
            audit.unchanged();
            (StatusCode::OK, bytes)
        }
    };
    Ok((status, Json(VaultBlobStored { sha256, bytes })))
}

/// A request body that failed to arrive, told apart from a store fault.
#[derive(Debug, thiserror::Error)]
#[error("the request body did not arrive whole: {0}")]
struct UnreadBodyError(axum::Error);

/// A body cut short, as by a client that closed its connection, is a `400`,
/// and one that went idle a `408` as on the buffered routes: the store passes
/// the body's error through, and it is not the store's fault.
fn unread_body_is_the_callers(error: VaultAdminError) -> AppError {
    let VaultAdminError::Store(e) = &error else {
        return error.into();
    };
    let Some(unread) = e.downcast_ref::<UnreadBodyError>() else {
        return error.into();
    };
    tracing::warn!(error = %unread, "refused: the upload body did not arrive whole");
    if timed_out(&unread.0) {
        AppError::RequestTimeout(unread.to_string())
    } else {
        AppError::BadRequest(unread.to_string())
    }
}

// HEAD /api/v1/vault/blobs/{principal_id}/{sha256}
pub async fn head_blob(
    State(state): State<AppState>,
    active: ActiveUser,
    ApiPath((principal_id, sha256)): ApiPath<(String, String)>,
) -> Result<StatusCode, AppError> {
    let caller = active.caller();
    let (owner, sha256) = blob_target(&state, caller, &principal_id, &sha256).await?;
    Ok(
        match vault_admin::blob_size(state.vault_store.as_ref(), &owner, &sha256).await? {
            Some(_) => StatusCode::OK,
            None => StatusCode::NOT_FOUND,
        },
    )
}

// POST /api/v1/vault/resources
pub async fn upload_resources(
    State(state): State<AppState>,
    active: ActiveUser,
    audit: Audit,
    ApiJson(upload): ApiJson<UploadVaultResourcesRequest>,
) -> Result<impl IntoResponse, AppError> {
    let authority = VaultAuthority::resolve(&state, active.caller()).await?;
    check_name(&upload.name)?;
    let owner = upload
        .owner
        .map_or_else(|| authority.own_owner(), VaultOwner::from);
    // The owner decides who may fetch the bytes, so it is settled before
    // anything is read or written.
    require_destination(&state, &authority, &owner, OUT_OF_SCOPE).await?;
    let records = vault_admin::upload(
        state.vault_store.as_ref(),
        upload.name,
        owner.clone(),
        upload.artifact,
        Utc::now(),
    )
    .await
    .inspect_err(
        |e| tracing::warn!(owner = %owner.label(), reason = %e, "refused a vault upload"),
    )?;
    records.iter().for_each(|record| {
        tracing::info!(resource_id = %record.resource_id, "vault resource uploaded");
    });
    let made = records.iter().map(|record| AuditTarget::VaultResource {
        resource_id: record.resource_id.clone(),
    });
    audit.involve(&state, made).await;
    let entry = records
        .first()
        .map(entry_with_toml)
        .transpose()?
        .flatten()
        .ok_or_else(|| AppError::Internal("an upload wrote no record".into()))?;
    Ok((
        StatusCode::CREATED,
        Json(VaultResourcesUploaded {
            entry,
            resources: records.into_iter().map(Into::into).collect(),
        }),
    ))
}

// GET /api/v1/vault/resources
pub async fn list_resources(
    State(state): State<AppState>,
    active: ActiveUser,
    ApiQuery(query): ApiQuery<OwnedListQuery>,
    ApiQuery(read): ApiQuery<VaultReadQuery>,
) -> Result<impl IntoResponse, AppError> {
    let caller = active.caller();
    let user = caller.user;
    let reach = VaultReach::resolve(&state, caller).await?;
    let authority = VaultAuthority::resolve(&state, caller).await?;
    let page = page_within_reach(
        &state.config,
        query,
        &user.user_id,
        |resource: &VaultResource| reach.admits(&resource.owner),
        "vault resources",
        |after, limit| {
            let store = state.vault_store.clone();
            async move { store.list_resources(after.as_ref(), limit).await }
        },
    )
    .await?
    .page;
    let names = if read.names.unwrap_or(false) {
        Some(owner_labels(&state, page.items.iter().map(|r| &r.owner)).await)
    } else {
        None
    };
    // One stat per row served: the page, not the reach, is what it costs.
    let stat = read.object.unwrap_or(false);
    // An upload's files are listed together, so the page answers most alias reads.
    let known: KnownOrigins = page
        .items
        .iter()
        .map(|r| (r.resource_id.clone(), r.origin.clone()))
        .collect();
    let items: Vec<VaultResourceDetail> = stream::iter(page.items)
        .map(|resource| {
            let (state, known, authority) = (&state, &known, &authority);
            let may_administer = authority.may_administer(&resource.owner);
            async move {
                let object = if stat {
                    Some(listed_object_state(state, &resource).await)
                } else {
                    None
                };
                let entry = listed_entry_with_toml(&resource);
                let alias = entry_alias(state, &resource, entry.as_ref(), known).await;
                let model_params = crate::api_v1::model_params::listed_of_vault_model(
                    state,
                    authority,
                    &resource,
                    entry.as_ref(),
                )
                .await;
                VaultResourceDetail {
                    entry,
                    alias,
                    resource: resource.into(),
                    object,
                    may_administer: Some(may_administer),
                    model_params,
                }
            }
        })
        .buffered(STORAGE_CONCURRENCY)
        .collect()
        .await;
    Ok(Json(Named {
        body: ListPage {
            items,
            next_cursor: page.next_cursor,
            unreadable: page.unreadable,
        },
        names,
    }))
}

// DELETE /api/v1/vault/resources/{resource_id}
pub async fn delete_resource(
    State(state): State<AppState>,
    active: ActiveUser,
    ApiPath(Parsed(resource_id)): ApiPath<Parsed<VaultResourceId>>,
) -> Result<impl IntoResponse, AppError> {
    let caller = active.caller();
    delete_for_user(&state, caller, &resource_id).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    /// The three answers, and that a size the record did not pin is not read
    /// as present.
    #[rstest]
    #[case(4096, Some(4096), ObjectState::Present)]
    #[case(4096, Some(4097), ObjectState::Resized { bytes: 4097 })]
    #[case(4096, None, ObjectState::Missing)]
    fn the_object_state_follows_the_store(
        #[case] recorded: u64,
        #[case] found: Option<u64>,
        #[case] expected: ObjectState,
    ) {
        assert_eq!(ObjectState::of(recorded, found), expected);
    }
}
