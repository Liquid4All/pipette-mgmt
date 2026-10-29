//! The processing tier: the submission pipeline, the plan catalog and its
//! maintenance, and the administration the CLI and HTTP routes share. It works
//! through the `pipette_store` traits and links no HTTP server framework.

pub mod audit;
pub mod campaign_admin;
pub mod campaign_chain;
pub mod catalog_cache;
pub mod catalog_import;
pub mod client_admin;
pub mod client_cli;
pub mod content_key;
pub mod dataset_cache;
pub mod descriptor_catalog_admin;
pub mod descriptor_names;
pub mod descriptor_offer;
pub mod fix_canonical;
pub mod fix_model_param;
pub mod fix_vault_labels;
mod gated_import;
pub mod hf_import;
mod http_client;
pub mod model_audit;
pub mod model_params;
pub mod org;
pub mod org_catalog;
pub mod plan_cells;
pub mod plan_cli;
pub mod plan_document;
pub mod plan_ingestion;
mod plan_maintenance;
pub mod plan_results;
pub mod plan_validation;
pub mod queue_maintenance;
mod recalculation;
pub mod record_copy;
pub mod requeue_eval;
pub mod score;
pub mod scoring_service;
pub mod submission;
pub mod user_admin;
pub mod variant_edit;
pub mod vault_admin;
pub mod vault_alias;
pub mod vault_check;
mod vault_label;
