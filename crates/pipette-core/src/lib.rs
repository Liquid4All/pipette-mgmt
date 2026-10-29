//! Shared domain types, validation newtypes, permissions, configuration, and
//! the warehouse row format. No storage backends, HTTP frameworks or async
//! runtimes, so a handler change does not rebuild this crate.

pub mod audit;
pub mod benchmark;
pub mod campaign;
pub mod canonical_json;
pub mod client;
pub mod client_permission;
pub mod client_preauth;
pub mod config;
pub mod dataset;
pub mod denial;
pub mod eval_sample_result;
pub mod key_index;
pub mod matching;
pub mod model_params;
pub mod organization;
pub mod parquet_utils;
pub mod plan;
pub mod preauth_prune;
pub mod tmp_file;
pub mod todo_filename;
pub mod types;
pub mod user;
pub mod user_permission;
pub mod user_preauth;
pub mod vault;
pub mod warehouse;

pub const BUILD_VERSION: &str = env!("PIPETTE_MGMT_VERSION");
