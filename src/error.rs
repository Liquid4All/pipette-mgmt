use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::json;

#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error("Bad Request: {0}")]
    BadRequest(String),
    #[error("Unauthorized: {0}")]
    Unauthorized(String),
    #[error("Forbidden: {0}")]
    Forbidden(String),
    #[error("Not Found: {0}")]
    NotFound(String),
    #[error("Payload Too Large: {0}")]
    PayloadTooLarge(String),
    #[error("Conflict: {0}")]
    Conflict(String),
    #[error("Bad Gateway: {0}")]
    BadGateway(String),
    #[error("Internal: {0}")]
    Internal(String),
}

impl AppError {
    /// The message to hand back to the caller, without the variant prefix.
    /// (`Display` formats with the variant name and the full payload; the
    /// wire body carries just this message.)
    ///
    /// Client-error payloads (`4xx`) are written for the caller and are
    /// returned as-is. Server-error payloads (`5xx`) describe machinery the
    /// caller has no business seeing — storage paths, bucket and object keys,
    /// upstream topology, backend error text — so they collapse to a fixed
    /// string. Callers that hand this to a client are safe by construction,
    /// and the payload itself reaches operators through the log instead.
    pub fn message(&self) -> &str {
        // Keyed on the status class rather than a list of variants, so a `5xx`
        // variant added later redacts by default — reaching the wire requires
        // someone to give it a `4xx` status, not merely to file it under the
        // wrong arm of a match.
        if self.status().is_server_error() {
            return match self {
                Self::BadGateway(_) => "upstream request failed",
                _ => "internal server error",
            };
        }
        self.payload()
    }

    /// The variant's own string, whatever its status class. Private, because
    /// a server-error payload is for [`Self::message`] to withhold and the
    /// `Display` impl to record — nothing outside this module chooses.
    fn payload(&self) -> &str {
        match self {
            Self::BadRequest(msg)
            | Self::Unauthorized(msg)
            | Self::Forbidden(msg)
            | Self::NotFound(msg)
            | Self::PayloadTooLarge(msg)
            | Self::Conflict(msg)
            | Self::BadGateway(msg)
            | Self::Internal(msg) => msg,
        }
    }

    /// Whether this service is itself at fault, as opposed to the caller
    /// (`4xx`) or an upstream (`502`). This is the log-level policy: a fault of
    /// ours is an `error!`, anything else at most a `warn!`. It lives here
    /// because an error reaches operators from more than one exit — a batch
    /// item reports its failure inside a `200` body and never passes through
    /// [`IntoResponse`] — and both must classify it the same way.
    ///
    /// Phrased as "`5xx` except the upstream one" so a `5xx` variant added
    /// later is treated as our fault until someone decides otherwise — the
    /// same default-safe direction [`Self::message`] takes on redaction.
    pub(crate) fn is_service_fault(&self) -> bool {
        self.status().is_server_error() && !matches!(self, Self::BadGateway(_))
    }

    fn status(&self) -> StatusCode {
        match self {
            Self::BadRequest(_) => StatusCode::BAD_REQUEST,
            Self::Unauthorized(_) => StatusCode::UNAUTHORIZED,
            Self::Forbidden(_) => StatusCode::FORBIDDEN,
            Self::NotFound(_) => StatusCode::NOT_FOUND,
            Self::PayloadTooLarge(_) => StatusCode::PAYLOAD_TOO_LARGE,
            Self::Conflict(_) => StatusCode::CONFLICT,
            Self::BadGateway(_) => StatusCode::BAD_GATEWAY,
            Self::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }
}

/// A malformed client-supplied id (e.g. a `job_id` URL path segment) is a
/// `400`, so untrusted-boundary handlers can validate with `try_new(...)?`.
impl From<crate::types::InvalidId> for AppError {
    fn from(e: crate::types::InvalidId) -> Self {
        AppError::BadRequest(e.to_string())
    }
}

/// The leaf construction errors from [`crate::validated`] are all bad
/// client input at an untrusted boundary → `400`, carrying the leaf
/// error's own `Display` as the message. This lets a future
/// untrusted-boundary handler validate with `SomeType::try_new(...)?`
/// instead of a hand-rolled `.map_err(|e| AppError::BadRequest(e.to_string()))`.
macro_rules! bad_request_from {
    ($($err:ty),+ $(,)?) => {
        $(
            impl From<$err> for AppError {
                fn from(e: $err) -> Self {
                    AppError::BadRequest(e.to_string())
                }
            }
        )+
    };
}

bad_request_from!(
    crate::validated::EmptyStringError,
    crate::validated::ContactEmailError,
    crate::validated::BatteryLevelError,
    crate::validated::PublicKeyHexError,
    crate::validated::TagError,
);

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let status = self.status();
        // The `5xx` payload is redacted out of the body by `message()`, so this
        // log is the only place it survives — emit it before building the
        // response. A `4xx` describes what the caller did wrong and reaches
        // them in the body, so it needs no operator record.
        if status.is_server_error() {
            if self.is_service_fault() {
                tracing::error!(status = status.as_u16(), error = %self, "request failed");
            } else {
                // A `502` reports an unhealthy upstream. This service handled
                // the request correctly by declining to invent a result for it,
                // so the condition is recoverable and resolves off-box.
                tracing::warn!(status = status.as_u16(), error = %self, "upstream request failed");
            }
        }
        let body = json!({ "error": self.message() });
        (status, axum::Json(body)).into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::validated::{BatteryLevel, ContactEmail, NonEmptyTrimmedString, PublicKeyHex, Tag};
    use rstest::rstest;

    /// Each leaf validation error converts into a `400` whose message is the
    /// leaf error's own `Display`, unchanged. Locks both the routing (always
    /// `BadRequest`) and the exact wire strings so a future rename of the
    /// error types can't silently alter the 400 body.
    #[rstest]
    #[case(
        NonEmptyTrimmedString::try_new("   ").unwrap_err().into(),
        "must be a non-empty string (whitespace-only is not accepted)"
    )]
    #[case(
        ContactEmail::try_new("notanemail").unwrap_err().into(),
        "contact_email is not a valid email address"
    )]
    #[case(
        BatteryLevel::try_new(200).unwrap_err().into(),
        "device_battery_level (200) must be between 0 and 100"
    )]
    #[case(
        PublicKeyHex::try_new("zz").unwrap_err().into(),
        "public_key must be valid hex"
    )]
    #[case(Tag::try_new("").unwrap_err().into(), "tag must not be empty")]
    fn leaf_validation_error_maps_to_bad_request_verbatim(
        #[case] err: AppError,
        #[case] expected: &str,
    ) {
        assert!(
            matches!(err, AppError::BadRequest(_)),
            "expected BadRequest, got {err:?}"
        );
        assert_eq!(err.message(), expected);
    }

    /// A `5xx` payload never reaches the wire. The sentinel below stands in for
    /// the kind of thing storage backends put in an error — an absolute path, a
    /// bucket key, an OS error — so a regression that reinstates the payload
    /// fails here rather than in production.
    const SECRET: &str = "/srv/pipette/clients/ev1_abc.json: Permission denied (os error 13)";

    #[rstest]
    #[case::internal(AppError::Internal(SECRET.into()), "internal server error")]
    #[case::bad_gateway(AppError::BadGateway(SECRET.into()), "upstream request failed")]
    fn server_error_message_is_redacted(#[case] err: AppError, #[case] expected: &str) {
        assert_eq!(err.message(), expected);
        // `Display` is the operator's view and must keep the full payload —
        // it's what the `into_response` log records.
        assert!(
            err.to_string().contains(SECRET),
            "log view lost the payload: {err}"
        );
    }

    /// The wire body is built from `message()`, so redaction holds end-to-end.
    /// Pinning the body to an exact value covers both directions at once: the
    /// `5xx` case fails if any of the payload survives, the `4xx` case fails if
    /// redaction reaches a message meant for the caller.
    #[rstest]
    #[case::server_error_redacted(
        AppError::Internal(SECRET.into()),
        StatusCode::INTERNAL_SERVER_ERROR,
        "internal server error"
    )]
    #[case::upstream_error_redacted(
        AppError::BadGateway(SECRET.into()),
        StatusCode::BAD_GATEWAY,
        "upstream request failed"
    )]
    #[case::client_error_verbatim(
        AppError::NotFound("job not found".into()),
        StatusCode::NOT_FOUND,
        "job not found"
    )]
    #[case::payload_too_large_verbatim(
        AppError::PayloadTooLarge("request body is too large".into()),
        StatusCode::PAYLOAD_TOO_LARGE,
        "request body is too large"
    )]
    #[tokio::test]
    async fn response_body_carries_exactly_the_message(
        #[case] err: AppError,
        #[case] expected_status: StatusCode,
        #[case] expected_message: &str,
    ) -> anyhow::Result<()> {
        let response = err.into_response();
        assert_eq!(response.status(), expected_status);

        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX).await?;
        let body: serde_json::Value = serde_json::from_slice(&bytes)?;
        assert_eq!(body, json!({ "error": expected_message }));
        Ok(())
    }
}
