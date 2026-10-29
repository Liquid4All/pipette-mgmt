use std::time::Duration;

use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};

use pipette_plan_types::ErrorBody;

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
    /// A record outside the caller's scope. The wire answer is the `404` of an
    /// absent record, so a probe learns nothing. The audit log records it as
    /// refused.
    #[error("Not Found: {0}")]
    OutOfScope(String),
    #[error("Request Timeout: {0}")]
    RequestTimeout(String),
    #[error("Payload Too Large: {0}")]
    PayloadTooLarge(String),
    #[error("Unsupported Media Type: {0}")]
    UnsupportedMediaType(String),
    #[error("Conflict: {0}")]
    Conflict(String),
    #[error("Precondition Failed: {0}")]
    PreconditionFailed(String),
    /// Refused for capacity, not for anything the request says; retry later.
    #[error("Too Many Requests: {0}")]
    TooManyRequests(String),
    /// Refused for the caller's rate; `Retry-After` gives the wait.
    #[error("Too Many Requests: {0}")]
    RateLimited(String, Duration),
    #[error("Bad Gateway: {0}")]
    BadGateway(String),
    #[error("Internal: {0}")]
    Internal(String),
}

impl AppError {
    /// A `500` carrying the whole `anyhow` context chain, redacted on the wire
    /// by [`Self::message`].
    pub fn internal(e: anyhow::Error) -> Self {
        Self::Internal(format!("{e:#}"))
    }

    /// The message for the caller: a `4xx` payload as-is, a `5xx` collapsed to
    /// a fixed string, since it can name storage paths or upstream topology.
    pub fn message(&self) -> &str {
        // Keyed on the status class, so a `5xx` variant added later redacts by
        // default.
        if self.status().is_server_error() {
            return match self {
                Self::BadGateway(_) => "upstream request failed",
                Self::BadRequest(_)
                | Self::Unauthorized(_)
                | Self::Forbidden(_)
                | Self::NotFound(_)
                | Self::OutOfScope(_)
                | Self::RequestTimeout(_)
                | Self::PayloadTooLarge(_)
                | Self::UnsupportedMediaType(_)
                | Self::Conflict(_)
                | Self::PreconditionFailed(_)
                | Self::TooManyRequests(_)
                | Self::RateLimited(..)
                | Self::Internal(_) => "internal server error",
            };
        }
        self.payload()
    }

    /// Private: a `5xx` payload is for [`Self::message`] to withhold.
    fn payload(&self) -> &str {
        match self {
            Self::BadRequest(msg)
            | Self::Unauthorized(msg)
            | Self::Forbidden(msg)
            | Self::NotFound(msg)
            | Self::OutOfScope(msg)
            | Self::RequestTimeout(msg)
            | Self::PayloadTooLarge(msg)
            | Self::UnsupportedMediaType(msg)
            | Self::Conflict(msg)
            | Self::PreconditionFailed(msg)
            | Self::TooManyRequests(msg)
            | Self::RateLimited(msg, _)
            | Self::BadGateway(msg)
            | Self::Internal(msg) => msg,
        }
    }

    /// The log-level policy: any `5xx` but the upstream `502` is our fault, an
    /// `error!`. Here because a batch item reports failure inside a `200` too.
    pub fn is_service_fault(&self) -> bool {
        self.status().is_server_error() && !matches!(self, Self::BadGateway(_))
    }

    pub fn status(&self) -> StatusCode {
        match self {
            Self::BadRequest(_) => StatusCode::BAD_REQUEST,
            Self::Unauthorized(_) => StatusCode::UNAUTHORIZED,
            Self::Forbidden(_) => StatusCode::FORBIDDEN,
            Self::NotFound(_) | Self::OutOfScope(_) => StatusCode::NOT_FOUND,
            Self::RequestTimeout(_) => StatusCode::REQUEST_TIMEOUT,
            Self::PayloadTooLarge(_) => StatusCode::PAYLOAD_TOO_LARGE,
            Self::UnsupportedMediaType(_) => StatusCode::UNSUPPORTED_MEDIA_TYPE,
            Self::Conflict(_) => StatusCode::CONFLICT,
            Self::PreconditionFailed(_) => StatusCode::PRECONDITION_FAILED,
            Self::TooManyRequests(_) | Self::RateLimited(..) => StatusCode::TOO_MANY_REQUESTS,
            Self::BadGateway(_) => StatusCode::BAD_GATEWAY,
            Self::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    /// Log a `5xx`: the only place its redacted cause survives.
    pub fn log_if_server_error(&self) {
        let status = self.status();
        if self.is_service_fault() {
            tracing::error!(status = status.as_u16(), error = %self, "request failed");
        } else if status.is_server_error() {
            // A `502` is an unhealthy upstream, recoverable off-box.
            tracing::warn!(status = status.as_u16(), error = %self, "upstream request failed");
        }
    }
}

/// A malformed client-supplied id (e.g. a `job_id` URL path segment) is a
/// `400`, so untrusted-boundary handlers can validate with `try_new(...)?`.
impl From<pipette_plan_types::IdError> for AppError {
    fn from(e: pipette_plan_types::IdError) -> Self {
        AppError::BadRequest(e.to_string())
    }
}

/// A failed OS RNG draw is this service's fault, so a redacted `500`.
impl From<getrandom::Error> for AppError {
    fn from(e: getrandom::Error) -> Self {
        AppError::Internal(format!("OS random source failed: {e}"))
    }
}

/// Leaf construction errors from [`pipette_core::validated`] are bad client
/// input: a `400` carrying the leaf error's `Display`.
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
    pipette_pipeline::audit::AuditFilterError,
    pipette_pipeline::client_cli::FlagError,
    pipette_plan_types::EmptyStringError,
    pipette_plan_types::ContactEmailError,
    pipette_plan_types::BatteryLevelError,
    pipette_plan_types::PublicKeyHexError,
    pipette_plan_types::TagError,
    pipette_plan_types::PrincipalIdError,
);

/// An ingest failure splits by audience. A HuggingFace failure's body is the
/// fixed upstream message: its cause can hold a CDN URL with its signature.
impl From<pipette_pipeline::plan_ingestion::IngestError> for AppError {
    fn from(e: pipette_pipeline::plan_ingestion::IngestError) -> Self {
        use pipette_pipeline::plan_ingestion::IngestError;
        match e {
            IngestError::Rejected(r) => AppError::BadRequest(r.to_string()),
            IngestError::ImportBusy | IngestError::HubRateLimited { .. } => {
                AppError::TooManyRequests(e.to_string())
            }
            IngestError::Upstream { .. } => AppError::BadGateway(e.to_string()),
            // The log keeps the whole `anyhow` chain; `message()` withholds it.
            IngestError::Other(e) => AppError::internal(e),
        }
    }
}

/// `NotFound` renders the "plan not found" body every plan route shares.
impl From<pipette_pipeline::plan_cli::PlanLookupError> for AppError {
    fn from(e: pipette_pipeline::plan_cli::PlanLookupError) -> Self {
        use pipette_pipeline::plan_cli::PlanLookupError;
        match e {
            PlanLookupError::NotFound { .. } => AppError::NotFound("plan not found".into()),
            PlanLookupError::Store(e) => AppError::internal(e),
        }
    }
}

impl From<pipette_pipeline::org::OrgLookupError> for AppError {
    fn from(e: pipette_pipeline::org::OrgLookupError) -> Self {
        use pipette_pipeline::org::OrgLookupError;
        match e {
            OrgLookupError::NotFound(_) => AppError::NotFound(e.to_string()),
            OrgLookupError::Store(source) => {
                AppError::internal(source.context("failed to read the organization"))
            }
        }
    }
}

impl From<pipette_pipeline::client_admin::ClientAdminError> for AppError {
    fn from(error: pipette_pipeline::client_admin::ClientAdminError) -> Self {
        use pipette_pipeline::client_admin::ClientAdminError;
        match error {
            ClientAdminError::NotFound(client_id) => {
                AppError::NotFound(format!("no client {client_id}"))
            }
            // The record's state refuses the verb: a conflict, not a malformed
            // request.
            ClientAdminError::NotPending { .. } => AppError::Conflict(error.to_string()),
            // As a stale `PATCH /clients/me` is: nothing was written, resend.
            ClientAdminError::Contended(_) => AppError::Conflict(error.to_string()),
            ClientAdminError::Store(e) => AppError::internal(e.context("the client store failed")),
        }
    }
}

impl From<pipette_pipeline::campaign_admin::CampaignAdminError> for AppError {
    fn from(error: pipette_pipeline::campaign_admin::CampaignAdminError) -> Self {
        use pipette_pipeline::campaign_admin::CampaignAdminError;
        let message = error.to_string();
        match error {
            CampaignAdminError::NotFound(_)
            | CampaignAdminError::NoSuchPlan(_)
            | CampaignAdminError::NotInCampaign { .. } => AppError::NotFound(message),
            CampaignAdminError::NoSuchUser(_)
            | CampaignAdminError::NoSuchOrg(_)
            | CampaignAdminError::NoOwnerNamed
            | CampaignAdminError::NothingToRerun
            | CampaignAdminError::NoSuchSourceVariant { .. } => AppError::BadRequest(message),
            CampaignAdminError::Edit(e) if e.is_caller_fault() => AppError::BadRequest(message),
            CampaignAdminError::Edit(e) => AppError::internal(anyhow::Error::new(e)),
            CampaignAdminError::NoPlan(_)
            | CampaignAdminError::Archived(_)
            | CampaignAdminError::NotOnPlan { .. }
            | CampaignAdminError::NoDocument(_)
            | CampaignAdminError::NotInPlan { .. }
            | CampaignAdminError::Unanswered(_)
            | CampaignAdminError::NoneAnswered { .. }
            | CampaignAdminError::NotRescorable(_)
            | CampaignAdminError::NoneRescorable(_)
            | CampaignAdminError::Overtaken { .. }
            | CampaignAdminError::DeletedDuringFiling { .. }
            | CampaignAdminError::StillRunning { .. } => AppError::Conflict(message),
            CampaignAdminError::NotAdvanced { source, .. }
            | CampaignAdminError::NotCreated { source, .. }
            | CampaignAdminError::NotCancelled { source, .. } => {
                AppError::internal(source.context(message))
            }
            CampaignAdminError::SourceMismatch(_) => AppError::internal(anyhow::anyhow!(message)),
            CampaignAdminError::Store(e) => {
                AppError::internal(e.context("the campaign store failed"))
            }
        }
    }
}

impl From<pipette_pipeline::org_catalog::OrgCatalogError> for AppError {
    fn from(error: pipette_pipeline::org_catalog::OrgCatalogError) -> Self {
        use pipette_pipeline::org_catalog::OrgCatalogError;
        let message = error.to_string();
        match error {
            OrgCatalogError::NotFound { .. } => AppError::NotFound(message),
            OrgCatalogError::MisleadingPrefix { .. } | OrgCatalogError::UnsafeSampleName(_) => {
                AppError::BadRequest(message)
            }
            OrgCatalogError::ShadowsPublic(_)
            | OrgCatalogError::HeldElsewhere(_)
            | OrgCatalogError::DatasetHeldElsewhere(_)
            | OrgCatalogError::SamplesHeldElsewhere { .. }
            | OrgCatalogError::DatasetPinnedPublicly(_) => AppError::Conflict(message),
            OrgCatalogError::Store(e) => {
                AppError::internal(e.context("the organization catalog failed"))
            }
        }
    }
}

impl From<pipette_pipeline::vault_admin::VaultAdminError> for AppError {
    fn from(error: pipette_pipeline::vault_admin::VaultAdminError) -> Self {
        use pipette_pipeline::vault_admin::VaultAdminError;
        let message = error.to_string();
        match error {
            VaultAdminError::NotFound(_) => AppError::NotFound(message),
            VaultAdminError::NoSuchUser(_)
            | VaultAdminError::NoSuchOrg(_)
            | VaultAdminError::NoOwnerNamed
            | VaultAdminError::DigestMismatch { .. }
            | VaultAdminError::NoBlob { .. }
            | VaultAdminError::NotA { .. } => AppError::BadRequest(message),
            VaultAdminError::TooLarge { .. } => AppError::PayloadTooLarge(message),
            // Redacted by `AppError::message`: a store's own text names keys.
            VaultAdminError::Store(e) => AppError::internal(e.context("the vault store failed")),
        }
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let status = self.status();
        self.log_if_server_error();
        let body = ErrorBody {
            error: self.message().to_string(),
        };
        let mut response = (status, axum::Json(body)).into_response();
        if matches!(self, Self::OutOfScope(_)) {
            response.extensions_mut().insert(crate::audit::OutOfScope);
        }
        if let Self::RateLimited(_, wait) = self {
            response
                .headers_mut()
                .insert(header::RETRY_AFTER, HeaderValue::from(wait.as_secs()));
        }
        response
    }
}

#[cfg(test)]
mod tests {
    use anyhow::Context;
    use rstest::rstest;
    use serde_json::json;

    use pipette_pipeline::org::OrgLookupError;
    use pipette_pipeline::plan_cli::PlanLookupError;
    use pipette_plan_types::{
        BatteryLevel, ContactEmail, NonEmptyTrimmedString, PlanId, PublicKeyHex, Tag,
    };

    use super::*;

    /// Pins the exact `400` strings. Each case is a constructor because
    /// building an error means rejecting an input, and a `#[case]` has no `?`.
    #[rstest]
    #[case(
        || NonEmptyTrimmedString::try_new("   ").err().map(AppError::from),
        "must be a non-empty string (whitespace-only is not accepted)"
    )]
    #[case(
        || ContactEmail::try_new("notanemail").err().map(AppError::from),
        "contact_email is not a valid email address"
    )]
    #[case(
        || BatteryLevel::try_new(200).err().map(AppError::from),
        "device_battery_level (200) must be between 0 and 100"
    )]
    #[case(
        || PublicKeyHex::try_new("zz").err().map(AppError::from),
        "public_key must be valid hex"
    )]
    #[case(
        || Tag::try_new("").err().map(AppError::from),
        "tag must not be empty"
    )]
    #[case(
        || pipette_plan_types::VaultResourceId::try_new("vault_a.b").err().map(AppError::from),
        "vault resource id contains '.'; allowed: [A-Za-z0-9_-]"
    )]
    fn leaf_validation_error_maps_to_bad_request_verbatim(
        #[case] reject: fn() -> Option<AppError>,
        #[case] expected: &str,
    ) -> anyhow::Result<()> {
        let err = reject().context("the constructor must reject its input")?;
        assert!(
            matches!(err, AppError::BadRequest(_)),
            "expected BadRequest, got {err:?}"
        );
        assert_eq!(err.message(), expected);
        Ok(())
    }

    /// The whole message is pinned: it is the only place the index reaches the
    /// caller.
    #[test]
    fn ingest_rejection_maps_to_bad_request_naming_the_index() {
        let err: AppError = pipette_pipeline::plan_ingestion::IngestError::Rejected(
            pipette_pipeline::plan_ingestion::RejectionError::Job {
                index: 3,
                label: "3".to_string(),
                kind: pipette_pipeline::plan_ingestion::JobRejectionError::PresetJobId,
            },
        )
        .into();
        assert!(matches!(err, AppError::BadRequest(_)), "got {err:?}");
        assert_eq!(
            err.message(),
            "rejected job \"3\" (index 3): carries a job_id (server-assigned at ingestion)"
        );
    }

    #[rstest]
    #[case::busy(
        pipette_pipeline::plan_ingestion::IngestError::ImportBusy,
        StatusCode::TOO_MANY_REQUESTS,
        "too many HuggingFace imports are in progress; retry shortly"
    )]
    #[case::rate_limited(
        pipette_pipeline::plan_ingestion::IngestError::HubRateLimited {
            index: 1,
            label: "1".to_string(),
            reason: "HuggingFace rate-limited the requests for a/b. Retry after 55 seconds".to_string(),
        },
        StatusCode::TOO_MANY_REQUESTS,
        "could not import the model of job \"1\" (index 1) at this time: \
         HuggingFace rate-limited the requests for a/b. Retry after 55 seconds"
    )]
    #[case::upstream(
        pipette_pipeline::plan_ingestion::IngestError::Upstream {
            index: 1,
            label: "1".to_string(),
            reason: "HuggingFace failed for a/b: status 500".to_string(),
        },
        StatusCode::BAD_GATEWAY,
        "upstream request failed"
    )]
    fn an_import_failure_maps_to_its_status(
        #[case] error: pipette_pipeline::plan_ingestion::IngestError,
        #[case] status: StatusCode,
        #[case] message: &str,
    ) {
        let logged = error.to_string();
        let err = AppError::from(error);
        assert_eq!(err.status(), status);
        assert_eq!(err.message(), message);
        assert!(err.to_string().ends_with(&logged), "{err}");
    }

    /// Pins each `4xx` body against a rename of the source variant's `Display`.
    #[rstest]
    #[case::not_found(
        || Ok(PlanLookupError::NotFound { plan_id: PlanId::try_new("plan-absent")? }.into()),
        StatusCode::NOT_FOUND,
        "plan not found"
    )]
    #[case::org_not_found(
        || Ok(OrgLookupError::NotFound("acme".parse()?).into()),
        StatusCode::NOT_FOUND,
        "no organization acme"
    )]
    fn error_maps_by_variant(
        #[case] lookup_error: fn() -> anyhow::Result<AppError>,
        #[case] expected_status: StatusCode,
        #[case] expected_message: &str,
    ) -> anyhow::Result<()> {
        let err = lookup_error()?;
        assert_eq!(err.status(), expected_status);
        assert_eq!(err.message(), expected_message);
        Ok(())
    }

    /// Stands in for what a storage backend puts in an error, which must never
    /// reach the wire.
    const SECRET: &str = "/srv/pipette/clients/ev1_abc.json: Permission denied (os error 13)";

    #[rstest]
    #[case::internal(AppError::Internal(SECRET.into()), "internal server error")]
    #[case::bad_gateway(AppError::BadGateway(SECRET.into()), "upstream request failed")]
    // A store failure in ingestion: a redacted `500` that keeps its context
    // chain.
    #[case::ingest_store_failure(
        pipette_pipeline::plan_ingestion::IngestError::Other(
            anyhow::anyhow!(SECRET).context("writing creating manifest for plan-x")
        ).into(),
        "internal server error"
    )]
    #[case::plan_store_failure(
        PlanLookupError::Store(
            anyhow::anyhow!(SECRET).context("reading manifest for plan-x")
        ).into(),
        "internal server error"
    )]
    #[case::org_store_failure(
        OrgLookupError::Store(
            anyhow::anyhow!(SECRET).context("reading the organization acme")
        ).into(),
        "internal server error"
    )]
    fn server_error_message_is_redacted(#[case] err: AppError, #[case] expected: &str) {
        assert_eq!(err.message(), expected);
        // `Display` is the operator's view and must keep the full payload.
        assert!(
            err.to_string().contains(SECRET),
            "log view lost the payload: {err}"
        );
    }

    /// An RNG failure is this service's fault: a redacted `500`, logged at `error!`.
    #[test]
    fn an_rng_failure_is_a_redacted_service_fault() {
        let err = AppError::from(getrandom::Error::UNSUPPORTED);
        assert_eq!(err.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert!(err.is_service_fault());
        assert_eq!(err.message(), "internal server error");
        assert!(err.to_string().contains("OS random source failed"), "{err}");
    }

    /// Pinned exactly, so the `5xx` cases catch a leak and the `4xx` cases
    /// catch over-redaction.
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
    #[case::unsupported_media_type_verbatim(
        AppError::UnsupportedMediaType("send application/json".into()),
        StatusCode::UNSUPPORTED_MEDIA_TYPE,
        "send application/json"
    )]
    #[case::precondition_failed_verbatim(
        AppError::PreconditionFailed("acme already keeps a benchmark x".into()),
        StatusCode::PRECONDITION_FAILED,
        "acme already keeps a benchmark x"
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
