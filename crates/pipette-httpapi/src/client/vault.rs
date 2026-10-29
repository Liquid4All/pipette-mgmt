//! A vault record and its bytes, at the address a plan names as an artifact URL.
//! Each read takes a device or an operator ([`crate::vault_access`]).

use axum::Json;
use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};

use pipette_core::vault::VaultResource;
use pipette_pipeline::vault_alias::KnownOrigins;
use pipette_plan_types::{ErrorBody, Named, VaultReadQuery, VaultResourceDetail, VaultResourceId};

use crate::api_v1::vault::{entry_alias, entry_with_toml, object_state, require, require_entitled};
use crate::auth::{Caller, VaultFetcher};
use crate::error::AppError;
use crate::extract::{ApiPath, ApiQuery, Parsed};
use crate::names::owner_labels;
use crate::state::AppState;
use crate::vault_access::VaultAuthority;

// GET /vault/resources/{resource_id}
//
// A device reads the digest and size here first, so it never re-downloads a
// checkpoint it already holds.
pub async fn get_resource(
    State(state): State<AppState>,
    fetcher: VaultFetcher,
    ApiPath(resource_id): ApiPath<String>,
    ApiQuery(read): ApiQuery<VaultReadQuery>,
) -> Result<impl IntoResponse, AppError> {
    // Not `Parsed`: a device's malformed query is refused before its id, as it always was.
    let resource_id = VaultResourceId::try_new(resource_id)?;
    let resource = require(&state, &resource_id).await?;
    require_entitled(&state, &fetcher, &resource).await?;
    let user = matches!(fetcher, VaultFetcher::User { .. });
    let object = if user && read.object.unwrap_or(false) {
        Some(
            object_state(&state, &resource)
                .await
                .map_err(|e| AppError::internal(e.context("failed to stat the vault object")))?,
        )
    } else {
        None
    };
    let names = if user && read.names.unwrap_or(false) {
        Some(owner_labels(&state, std::iter::once(&resource.owner)).await)
    } else {
        None
    };
    // As the writes decide, so the app draws its controls from the answer.
    let authority = match &fetcher {
        VaultFetcher::User { user, rights } => {
            Some(VaultAuthority::resolve(&state, Caller { user, rights }).await?)
        }
        VaultFetcher::Client(_) => None,
    };
    let may_administer = authority
        .as_ref()
        .map(|authority| authority.may_administer(&resource.owner));
    let entry = entry_with_toml(&resource)?;
    let alias = entry_alias(&state, &resource, entry.as_ref(), &KnownOrigins::new()).await;
    let model_params = match &authority {
        Some(authority) => {
            crate::api_v1::model_params::of_vault_model(
                &state,
                authority,
                &resource,
                entry.as_ref(),
            )
            .await?
        }
        None => None,
    };
    Ok(Json(Named {
        body: VaultResourceDetail {
            resource: resource.into(),
            entry,
            alias,
            object,
            may_administer,
            model_params,
        },
        names,
    }))
}

// GET /vault/resources/{resource_id}/content
pub async fn get_resource_content(
    State(state): State<AppState>,
    fetcher: VaultFetcher,
    ApiPath(Parsed(resource_id)): ApiPath<Parsed<VaultResourceId>>,
    request_headers: HeaderMap,
) -> Result<Response, AppError> {
    let resource = require(&state, &resource_id).await?;
    require_entitled(&state, &fetcher, &resource).await?;

    // The ETag is the record's pin, not the object's: the client's digest check
    // catches an object replaced behind the record.
    let etag = HeaderValue::from_str(&format!("\"{}\"", resource.sha256))
        .map_err(|e| AppError::Internal(format!("a sha256 is not a header value: {e}")))?;

    // The range is judged against the object's size, so a resume past its end
    // is refused before the object is opened.
    let range = match resume_offset(&request_headers, &etag) {
        None => None,
        Some(from) => {
            let size = state
                .vault_store
                .stat_blob(&resource.object_key)
                .await
                .map_err(|e| AppError::internal(e.context("failed to stat the vault object")))?
                .ok_or_else(|| object_missing(&resource))?;
            if from >= size {
                tracing::warn!(
                    event = "vault_range_unsatisfiable",
                    %resource_id,
                    from,
                    bytes = size,
                    "refused a vault range past the end of the object"
                );
                return range_unsatisfiable(size);
            }
            Some((from, size))
        }
    };

    let body = state
        .vault_store
        .open_blob(&resource.object_key, range.map_or(0, |(from, _)| from))
        .await
        .map_err(|e| AppError::internal(e.context("failed to open the vault object")))?
        .ok_or_else(|| object_missing(&resource))?;

    let status = if range.is_some() {
        StatusCode::PARTIAL_CONTENT
    } else {
        StatusCode::OK
    };
    let mut response = (status, Body::from_stream(body.stream)).into_response();
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/octet-stream"),
    );
    headers.insert(header::CONTENT_LENGTH, HeaderValue::from(body.bytes));
    headers.insert(header::ETAG, etag);
    headers.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    // Private bytes must not sit in a shared cache on the way back.
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, no-store"),
    );
    if let Some((from, size)) = range {
        headers.insert(
            header::CONTENT_RANGE,
            content_range(&format!("{from}-{}/{size}", size - 1))?,
        );
        tracing::info!(
            event = "vault_content_resumed",
            %resource_id,
            from,
            bytes = size,
            "serving a vault object from an offset"
        );
    }
    Ok(response)
}

/// Only `Range: bytes=N-` resumes, under an `If-Range` absent or equal to
/// `etag`; RFC 9110 lets a server ignore every other form.
fn resume_offset(headers: &HeaderMap, etag: &HeaderValue) -> Option<u64> {
    if headers.get(header::IF_RANGE).is_some_and(|tag| tag != etag) {
        return None;
    }
    let range = headers.get(header::RANGE)?.to_str().ok()?;
    let first = range.strip_prefix("bytes=")?.strip_suffix('-')?;
    // `u64::from_str` takes a leading `+`, which a byte position does not.
    if !first.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    first.parse().ok()
}

fn range_unsatisfiable(size: u64) -> Result<Response, AppError> {
    let mut response = (
        StatusCode::RANGE_NOT_SATISFIABLE,
        Json(ErrorBody {
            error: "the range starts at or past the end of the vault object".to_string(),
        }),
    )
        .into_response();
    let headers = response.headers_mut();
    headers.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    headers.insert(header::CONTENT_RANGE, content_range(&format!("*/{size}"))?);
    Ok(response)
}

fn content_range(range: &str) -> Result<HeaderValue, AppError> {
    HeaderValue::from_str(&format!("bytes {range}"))
        .map_err(|e| AppError::Internal(format!("a range is not a header value: {e}")))
}

/// A record whose blob is gone: removed outside mgmt, or lost to a delete race.
fn object_missing(resource: &VaultResource) -> AppError {
    tracing::error!(
        event = "vault_object_missing",
        resource_id = %resource.resource_id,
        object_key = %resource.object_key,
        "the vault record names an object that is not there"
    );
    AppError::NotFound("vault object not found".into())
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    #[rstest]
    #[case::open_ended(Some("bytes=4-"), None, Some(4))]
    #[case::from_zero(Some("bytes=0-"), None, Some(0))]
    #[case::matching_validator(Some("bytes=4-"), Some("\"pin\""), Some(4))]
    #[case::other_validator(Some("bytes=4-"), Some("\"other\""), None)]
    #[case::weak_validator(Some("bytes=4-"), Some("W/\"pin\""), None)]
    #[case::date_validator(Some("bytes=4-"), Some("Mon, 05 Oct 2026 12:00:00 GMT"), None)]
    #[case::no_range(None, None, None)]
    #[case::bounded(Some("bytes=0-3"), None, None)]
    #[case::suffix(Some("bytes=-4"), None, None)]
    #[case::several(Some("bytes=0-1,4-"), None, None)]
    #[case::other_unit(Some("items=4-"), None, None)]
    #[case::signed(Some("bytes=+4-"), None, None)]
    #[case::no_position(Some("bytes=-"), None, None)]
    #[case::too_large(Some("bytes=18446744073709551616-"), None, None)]
    fn only_an_open_ended_range_under_the_pin_resumes(
        #[case] range: Option<&'static str>,
        #[case] if_range: Option<&'static str>,
        #[case] expected: Option<u64>,
    ) {
        let mut headers = HeaderMap::new();
        if let Some(range) = range {
            headers.insert(header::RANGE, HeaderValue::from_static(range));
        }
        if let Some(if_range) = if_range {
            headers.insert(header::IF_RANGE, HeaderValue::from_static(if_range));
        }
        assert_eq!(
            resume_offset(&headers, &HeaderValue::from_static("\"pin\"")),
            expected
        );
    }
}
