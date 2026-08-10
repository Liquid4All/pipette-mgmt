use axum::Router;
use axum::extract::DefaultBodyLimit;
use axum::routing::{get, post, put};

use crate::handlers::{self, AppState};
use crate::plan_handlers;

/// Ceiling on a request body for every route that does not raise it.
///
/// A body is buffered whole before it is parsed, so this is the memory one
/// in-flight request can pin, and nothing bounds how many arrive at once. The
/// routes it covers carry typed structs of identity and device fields —
/// kilobytes — and `POST /clients/register` among them serves unauthenticated
/// callers. Applying the small ceiling router-wide and letting the two
/// submission routes opt up means a route added later is bounded before anyone
/// considers the question.
pub const DEFAULT_BODY_LIMIT: usize = 64 * 1024;

/// Ceiling on a request body for `POST /benchmarks` and `POST /benchmarks/batch`.
///
/// A submission carries one `completions` entry per eval sample, and a batch
/// carries up to `BATCH_MAX_SUBMISSIONS` of them, so these are the only bodies
/// whose size follows the workload rather than a fixed set of fields. This
/// ceiling, not the per-item count, is what bounds a batch body. Both routes
/// require an approved client.
pub const SUBMISSION_BODY_LIMIT: usize = 128 * 1024 * 1024;

/// The served route table.
///
/// Both `serve` and the integration tests build their app from here, so a route
/// or a limit reaches the tests by existing rather than by being copied into a
/// second list.
pub fn app(state: AppState) -> Router {
    Router::new()
        .route("/", get(handlers::index))
        .route("/health", get(handlers::health))
        .route("/clients/register", post(handlers::register_client))
        .route(
            "/clients/me",
            get(handlers::get_me).patch(handlers::update_me),
        )
        .route(
            "/benchmarks",
            get(handlers::list_benchmarks)
                .post(handlers::submit_benchmark)
                .layer(DefaultBodyLimit::max(SUBMISSION_BODY_LIMIT)),
        )
        .route(
            "/benchmarks/batch",
            post(handlers::submit_benchmark_batch)
                .layer(DefaultBodyLimit::max(SUBMISSION_BODY_LIMIT)),
        )
        .route("/benchmarks/{benchmark_id}", get(handlers::get_benchmark))
        .route("/jobs/{job_id}", get(handlers::get_job))
        .route(
            "/jobs/{job_id}/eval-sample-results",
            get(handlers::get_eval_sample_results),
        )
        .route("/plans/claim", post(plan_handlers::claim))
        .route("/plans/{job_id}/heartbeat", put(plan_handlers::heartbeat))
        .route("/plans/{job_id}/reclaim", post(plan_handlers::reclaim))
        .layer(DefaultBodyLimit::max(DEFAULT_BODY_LIMIT))
        .with_state(state)
}
