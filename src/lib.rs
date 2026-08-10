pub mod auth;
pub mod benchmark;
pub mod canonical_json;
pub mod catalog_cache;
pub mod client;
pub mod config;
pub mod error;
pub mod eval_sample_result;
pub mod extract;
pub mod fix_canonical;
pub mod fix_model_param;
pub mod handlers;
pub mod matching;
pub mod model_params;
pub mod parquet_utils;
pub mod plan;
pub mod plan_cli;
pub mod plan_handlers;
pub mod plan_ingestion;
pub mod preauth;
pub mod queue_maintenance;
pub mod requeue_eval;
pub mod router;
pub mod score;
pub mod scoring_service;
pub mod storage;
pub mod storage_lock;
pub mod stores;
pub mod submission;
pub mod todo_filename;
pub mod types;
pub mod validated;
pub mod warehouse;

pub const BUILD_VERSION: &str = env!("PIPETTE_MGMT_VERSION");

/// Shared `NonZeroUsize` for `list_incoming` test calls — large enough to
/// return any test fixture's full backlog without paging.
#[cfg(test)]
pub(crate) const TEST_LIST_LIMIT: std::num::NonZeroUsize =
    std::num::NonZeroUsize::new(100).unwrap();
