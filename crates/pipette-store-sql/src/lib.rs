//! The record stores on PostgreSQL (`docs/design/sql-record-store.md`). Each
//! write is a `REPEATABLE READ` transaction made atomic by a constraint or a
//! conditional statement, never a lock; [`retry`] reruns it on a conflict.

mod audit;
mod campaigns;
mod catalog;
mod catalog_import_runs;
mod clients;
mod db;
mod descriptor_catalog;
mod job_cells;
mod key_index;
mod listing;
mod migrate;
mod model_params;
mod orgs;
mod permissions;
mod plans;
mod retry;
#[cfg(any(test, feature = "testkit"))]
pub mod scratch;
mod submissions;
mod tags;
#[cfg(test)]
mod testing;
#[cfg(test)]
mod tests;
mod users;
mod vault;

use std::sync::Arc;

pub use db::SqlDatabase;
pub use job_cells::{JobCellBackfill, backfill_job_cells};

use pipette_store::{AuditStores, Stores};

/// A claim whose insert found a conflicting row that the next read did not
/// find. `claim` names the kind of claim, `handle` for example.
#[derive(Debug, thiserror::Error)]
#[error("{claim} claim for {name} vanished mid-create")]
pub(crate) struct ClaimVanishedError {
    pub(crate) claim: &'static str,
    pub(crate) name: String,
}

/// Replace the record stores in `stores` with ones on `db`. The catalog, vault
/// and submission store wrap the ones they replace, which keep the public half,
/// the blobs and the submission records.
pub fn overlay(db: &SqlDatabase, stores: Stores) -> Stores {
    Stores {
        catalog: Arc::new(catalog::SqlCatalogStore::new(db.clone(), stores.catalog)),
        clients: Arc::new(clients::SqlClientStore::new(db.clone())),
        users: Arc::new(users::SqlUserStore::new(db.clone())),
        orgs: Arc::new(orgs::SqlOrgStore::new(db.clone())),
        tags: Arc::new(tags::SqlTagStore::new(db.clone())),
        key_index: Arc::new(key_index::SqlKeyIndexStore::new(db.clone())),
        plans: Arc::new(plans::SqlPlanStore::new(db.clone())),
        campaigns: Arc::new(campaigns::SqlCampaignStore::new(db.clone())),
        model_params: Arc::new(model_params::SqlModelParamsStore::new(db.clone())),
        audit: Some(Arc::new(audit::SqlAuditStore::new(db.clone()))),
        descriptor_catalog: Some(Arc::new(
            descriptor_catalog::SqlDescriptorCatalogStore::new(db.clone()),
        )),
        catalog_import_runs: Some(Arc::new(
            catalog_import_runs::SqlCatalogImportRunStore::new(db.clone()),
        )),
        vault: Arc::new(vault::SqlVaultStore::new(db.clone(), stores.vault)),
        submissions: Arc::new(submissions::SqlSubmissionStore::new(
            db.clone(),
            stores.submissions,
        )),
        ..stores
    }
}

/// The stores that the audit event of a host command reads and writes, on the
/// database alone: the records that name its organizations, and the audit log.
/// It opens no object storage.
pub fn audit_stores(db: &SqlDatabase) -> AuditStores {
    AuditStores {
        orgs: Arc::new(orgs::SqlOrgStore::new(db.clone())),
        campaigns: Arc::new(campaigns::SqlCampaignStore::new(db.clone())),
        plans: Arc::new(plans::SqlPlanStore::new(db.clone())),
        vault: Arc::new(vault::SqlVaultRecords::new(db.clone())),
        log: Arc::new(audit::SqlAuditStore::new(db.clone())),
    }
}

/// The migrations that `db migrate` moved the schema across. `from` and `to`
/// are the newest applied versions before and after.
#[derive(Debug, PartialEq, Eq)]
pub struct SchemaMove {
    pub from: Option<i64>,
    pub to: Option<i64>,
    pub applied: Vec<i64>,
    /// Newest first, the order of the reverts.
    pub reverted: Vec<i64>,
}

/// Move the schema to migration `to`, up or down, or apply every pending one
/// without `to`. A rerun moves nothing. `dry_run` reports and moves nothing.
pub async fn migrate(
    db: &SqlDatabase,
    to: Option<i64>,
    dry_run: bool,
) -> anyhow::Result<SchemaMove> {
    migrate::migrate(db, to, dry_run).await
}

/// The newest migration this binary embeds, so `db migrate` can name it.
pub fn latest_migration() -> Option<i64> {
    migrate::latest()
}

/// Fail unless the database answers and its schema is the version that this
/// binary expects.
pub async fn check(db: &SqlDatabase) -> anyhow::Result<()> {
    db::ping(db).await?;
    migrate::check(db).await
}
