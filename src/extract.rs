use axum::Json;
use axum::extract::rejection::JsonRejection;
use axum::extract::{FromRequest, Request};
use axum::http::StatusCode;
use serde::de::DeserializeOwned;

use crate::error::AppError;

/// `axum::Json` for **request** bodies, but a deserialization failure surfaces
/// as [`AppError::BadRequest`] — a `400` carrying the uniform `{"error": ...}`
/// envelope — instead of axum's default rejection (a `422`/`415` with a
/// plain-text body). This keeps malformed-body responses consistent with every
/// other client error in `docs/httpapi.md`, which documents these as `400`.
///
/// Use only for extraction; responses still build with plain [`axum::Json`].
pub struct ApiJson<T>(pub T);

impl<S, T> FromRequest<S> for ApiJson<T>
where
    T: DeserializeOwned,
    S: Send + Sync,
{
    type Rejection = AppError;

    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        Json::<T>::from_request(req, state)
            .await
            .map(|Json(value)| ApiJson(value))
            .map_err(|rejection: JsonRejection| match rejection.status() {
                // A body stopped short of the route's ceiling is a size
                // problem, not a syntax one, and the caller's fix is to send
                // less rather than to correct their JSON. Reporting it as a
                // `400` would send them looking for a malformed field.
                StatusCode::PAYLOAD_TOO_LARGE => {
                    AppError::PayloadTooLarge("request body is too large".into())
                }
                _ => AppError::BadRequest(rejection.body_text()),
            })
    }
}
