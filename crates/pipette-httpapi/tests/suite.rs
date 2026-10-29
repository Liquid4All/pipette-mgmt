//! The integration tests that share one binary, since each test binary costs
//! ~110 MB and about a second of macOS signature validation on first exec.
//!
//! A file that reads back a log line is its own `[[test]]` instead: `tracing`
//! caches a callsite's interest process-globally, so a neighbour can leave its
//! capture empty. `ci/checks/test-targets.sh` enforces the split.

#![expect(
    clippy::disallowed_macros,
    clippy::disallowed_methods,
    reason = "tests build the requests a client sends, so they keep `json!` (see clippy.toml)"
)]

mod api_schema;
mod audit;
mod benchmarks;
mod campaign_listings;
mod campaign_page;
mod campaign_readers;
mod campaigns;
mod catalog_import_runs;
mod clerk_keys;
mod client_admin;
mod client_auth;
mod client_contact;
mod client_permissions;
mod client_preauth;
mod clients;
mod compression;
mod datasets;
mod descriptor_catalog;
mod error_envelope;
mod fleet;
mod gated_documents;
mod health;
mod model_params;
mod openapi;
mod org_catalog;
mod orgs;
mod plan_job_results;
mod plan_notes;
mod plan_pages;
mod plan_reads;
mod plan_validation;
mod plans;
mod private_benchmarks;
mod queue_maintenance;
mod request_limits;
mod rerun_recalculate;
mod rfc9421;
mod route_table;
mod scoring;
mod service_accounts;
mod unverified;
mod user_admin;
mod user_auth;
mod vault;
