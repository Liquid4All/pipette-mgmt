//! [`CatalogStore`] on SQL for organizations' private entries, wrapping the
//! object-store catalog of public ones. A body is the object stores' TOML
//! ([`Benchmark::to_toml`]), parsed on read so a refused entry is not lost.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;

use pipette_core::benchmark::Benchmark;
use pipette_plan_types::{BenchmarkId, OrgId};
use pipette_store::{CatalogStore, ORG_BENCHMARKS_PREFIX, OrgCatalogEntry, UnreadableObjectError};

use crate::db::SqlDatabase;

/// The object-store key of the entry. An unreadable report names this key.
fn entry_key(org_id: &str, benchmark_id: &str) -> String {
    format!("{ORG_BENCHMARKS_PREFIX}/{org_id}/{benchmark_id}.toml")
}

/// A row's body as `org_id`'s entry, or why it does not parse.
fn parse_entry(
    org_id: &OrgId,
    benchmark_id: &str,
    body: &str,
) -> Result<Benchmark, UnreadableObjectError> {
    Benchmark::from_toml(benchmark_id, body)
        .map(|entry| entry.private_to(org_id.clone()))
        .map_err(|e| UnreadableObjectError {
            key: entry_key(org_id.as_str(), benchmark_id),
            reason: format!("{e:#}"),
        })
}

pub(crate) struct SqlCatalogStore {
    db: SqlDatabase,
    public: Arc<dyn CatalogStore>,
}

impl SqlCatalogStore {
    pub(crate) fn new(db: SqlDatabase, public: Arc<dyn CatalogStore>) -> Self {
        Self { db, public }
    }
}

#[async_trait]
impl CatalogStore for SqlCatalogStore {
    async fn load_catalog(&self) -> anyhow::Result<HashMap<BenchmarkId, Benchmark>> {
        self.public.load_catalog().await
    }

    async fn get_org_benchmark(
        &self,
        org_id: &OrgId,
        benchmark_id: &BenchmarkId,
    ) -> anyhow::Result<Option<Benchmark>> {
        let (org, id) = (org_id.as_str(), benchmark_id.as_str());
        let body: Option<(String,)> = self
            .db
            .fetch_optional("get_org_benchmark", || {
                sqlx::query_as(
                    "SELECT body FROM org_benchmarks WHERE org_id = $1 AND benchmark_id = $2",
                )
                .bind(org)
                .bind(id)
            })
            .await?;
        body.map(|(body,)| parse_entry(org_id, id, &body))
            .transpose()
            .map_err(Into::into)
    }

    /// One query, rather than a read per entry. A row with an invalid id is
    /// skipped, as the listing by id skips it.
    async fn list_org_benchmarks(&self, org_id: &OrgId) -> anyhow::Result<Vec<OrgCatalogEntry>> {
        let org = org_id.as_str();
        let rows: Vec<(String, String)> = self
            .db
            .fetch_all("list_org_benchmarks", || {
                sqlx::query_as("SELECT benchmark_id, body FROM org_benchmarks WHERE org_id = $1")
                    .bind(org)
            })
            .await?;
        let mut entries: Vec<OrgCatalogEntry> = rows
            .into_iter()
            .filter_map(|(id, body)| {
                let benchmark_id = BenchmarkId::try_new(id.as_str())
                    .inspect_err(|e| {
                        tracing::warn!(key = %entry_key(org, &id), error = %e,
                            "skipping org benchmark row with an invalid id");
                    })
                    .ok()?;
                Some((benchmark_id, parse_entry(org_id, &id, &body)))
            })
            .collect();
        // In Rust, not ORDER BY: the column's collation need not be byte order.
        entries.sort_by(|(a, _), (b, _)| a.cmp(b));
        Ok(entries)
    }

    async fn put_org_benchmark(&self, org_id: &OrgId, benchmark: &Benchmark) -> anyhow::Result<()> {
        let body = benchmark.to_toml()?;
        self.db
            .execute("put_org_benchmark", || {
                sqlx::query(
                    "INSERT INTO org_benchmarks (org_id, benchmark_id, body) VALUES ($1, $2, $3)
                     ON CONFLICT (org_id, benchmark_id) DO UPDATE SET body = EXCLUDED.body",
                )
                .bind(org_id.as_str())
                .bind(benchmark.benchmark_id().as_str())
                .bind(body.as_str())
            })
            .await?;
        Ok(())
    }

    async fn delete_org_benchmark(
        &self,
        org_id: &OrgId,
        benchmark_id: &BenchmarkId,
    ) -> anyhow::Result<()> {
        self.db
            .execute("delete_org_benchmark", || {
                sqlx::query("DELETE FROM org_benchmarks WHERE org_id = $1 AND benchmark_id = $2")
                    .bind(org_id.as_str())
                    .bind(benchmark_id.as_str())
            })
            .await?;
        Ok(())
    }

    /// Reads no body, so an entry that fails to parse still counts and a
    /// delete can find it.
    async fn list_org_benchmark_ids(&self) -> anyhow::Result<Vec<(OrgId, BenchmarkId)>> {
        let rows: Vec<(String, String)> = self
            .db
            .fetch_all("list_org_benchmark_ids", || {
                sqlx::query_as(
                    "SELECT org_id, benchmark_id FROM org_benchmarks ORDER BY org_id, benchmark_id",
                )
            })
            .await?;
        Ok(rows
            .into_iter()
            .filter_map(|(org, id)| {
                OrgId::try_new(org.as_str())
                    .map_err(anyhow::Error::from)
                    .and_then(|org_id| Ok((org_id, BenchmarkId::try_new(id.as_str())?)))
                    .inspect_err(|e| {
                        tracing::warn!(key = %entry_key(&org, &id), error = %e,
                            "skipping org benchmark row with an invalid id");
                    })
                    .ok()
            })
            .collect())
    }

    async fn list_benchmark_ids_of(&self, org_id: &OrgId) -> anyhow::Result<Vec<BenchmarkId>> {
        let org = org_id.as_str();
        let rows: Vec<(String,)> = self
            .db
            .fetch_all("list_benchmark_ids_of", || {
                sqlx::query_as("SELECT benchmark_id FROM org_benchmarks WHERE org_id = $1")
                    .bind(org)
            })
            .await?;
        let mut ids: Vec<BenchmarkId> = rows
            .into_iter()
            .filter_map(|(id,)| {
                BenchmarkId::try_new(id.as_str())
                    .inspect_err(|e| {
                        tracing::warn!(key = %entry_key(org, &id), error = %e,
                            "skipping org benchmark row with an invalid id");
                    })
                    .ok()
            })
            .collect();
        // As `list_org_benchmarks` sorts.
        ids.sort();
        Ok(ids)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use pipette_core::benchmark::Benchmark;
    use pipette_core::config::{Config, StorageConfig};
    use pipette_core::types::Mint;
    use pipette_plan_types::{
        BatchSize, BenchmarkDefinition, BenchmarkId, NonNegativeCount, OrgId,
        PrefillThroughputBenchmark,
    };
    use pipette_store::{CatalogStore, UnreadableObjectError};

    use super::SqlCatalogStore;
    use crate::db::SqlDatabase;
    use crate::testing::{RACERS, race};

    const PUBLIC_ENTRY: &str =
        "benchmark_type = \"prefill_throughput\"\nparameter_prefill_tokens = 256";

    fn catalog(db: &SqlDatabase, dir: &std::path::Path) -> anyhow::Result<SqlCatalogStore> {
        let config = Config {
            storage: StorageConfig::local_fs(dir.to_path_buf()),
            ..Default::default()
        };
        let public = pipette_store_fs::build_local_fs_stores(&config)?.catalog;
        Ok(SqlCatalogStore::new(db.clone(), public))
    }

    async fn insert_raw(
        db: &SqlDatabase,
        org_id: &str,
        benchmark_id: &str,
        body: &str,
    ) -> anyhow::Result<()> {
        sqlx::query("INSERT INTO org_benchmarks (org_id, benchmark_id, body) VALUES ($1, $2, $3)")
            .bind(org_id)
            .bind(benchmark_id)
            .bind(body)
            .execute(db.pool())
            .await?;
        Ok(())
    }

    #[tokio::test]
    async fn sql_org_catalog_meets_the_shared_contract() -> anyhow::Result<()> {
        let test_db = crate::testing::test_db_or_skip!();
        let dir = tempfile::tempdir()?;
        let store = catalog(test_db.db(), dir.path())?;
        pipette_store_testkit::assert_org_catalog_store_roundtrip(&store).await
    }

    #[tokio::test]
    async fn sql_org_benchmark_ids_meet_the_shared_contract() -> anyhow::Result<()> {
        let test_db = crate::testing::test_db_or_skip!();
        let dir = tempfile::tempdir()?;
        let store = catalog(test_db.db(), dir.path())?;
        pipette_store_testkit::assert_org_benchmark_ids_span_every_org(&store).await
    }

    #[tokio::test]
    async fn sql_org_benchmark_listing_meets_the_shared_contract() -> anyhow::Result<()> {
        let test_db = crate::testing::test_db_or_skip!();
        let db = test_db.db();
        let dir = tempfile::tempdir()?;
        let store = catalog(db, dir.path())?;
        pipette_store_testkit::assert_org_benchmark_listing(&store, |org_id, id, body| async move {
            insert_raw(db, org_id.as_str(), id, body).await
        })
        .await
    }

    #[tokio::test]
    async fn sql_public_catalog_stays_on_the_inner_store() -> anyhow::Result<()> {
        let test_db = crate::testing::test_db_or_skip!();
        let dir = tempfile::tempdir()?;
        let benchmarks = dir.path().join("benchmarks");
        std::fs::create_dir_all(&benchmarks)?;
        let store = catalog(test_db.db(), dir.path())?;
        assert!(
            store.load_catalog().await.is_err(),
            "an empty public catalog is a misconfiguration, as on the inner store"
        );

        std::fs::write(benchmarks.join("prefill_throughput_256.toml"), PUBLIC_ENTRY)?;
        let acme = OrgId::mint();
        let entry = pipette_store_testkit::an_org_benchmark()?;
        store.put_org_benchmark(&acme, &entry).await?;

        let public = store.load_catalog().await?;
        assert_eq!(public.len(), 1);
        assert!(public.contains_key(&BenchmarkId::try_new("prefill_throughput_256")?));
        assert!(
            !dir.path().join("org-benchmarks").exists(),
            "a private entry is not written to the inner store"
        );
        let body: String = sqlx::query_scalar(
            "SELECT body FROM org_benchmarks WHERE org_id = $1 AND benchmark_id = $2",
        )
        .bind(acme.as_str())
        .bind(entry.benchmark_id().as_str())
        .fetch_one(test_db.db().pool())
        .await?;
        assert_eq!(body, entry.to_toml()?, "the body is the definition alone");
        Ok(())
    }

    /// The org's catalog omits an unreadable entry, and a read by id fails. A
    /// listing by id still includes the entry, so a delete can find it.
    #[tokio::test]
    async fn sql_org_catalog_unreadable_entry() -> anyhow::Result<()> {
        let test_db = crate::testing::test_db_or_skip!();
        let db = test_db.db();
        let dir = tempfile::tempdir()?;
        let store = catalog(db, dir.path())?;
        let acme = OrgId::mint();
        let entry = pipette_store_testkit::an_org_benchmark()?;
        store.put_org_benchmark(&acme, &entry).await?;
        insert_raw(db, acme.as_str(), "broken", "benchmark_type = \"nope\"").await?;
        insert_raw(db, acme.as_str(), "not_toml", "{").await?;

        let loaded = store.load_org_catalog(&acme).await?;
        assert_eq!(loaded.len(), 1);
        assert!(loaded.contains_key(entry.benchmark_id()));
        for id in ["broken", "not_toml"] {
            let read = store
                .get_org_benchmark(&acme, &BenchmarkId::try_new(id)?)
                .await;
            assert!(
                read.as_ref()
                    .is_err_and(|e| e.is::<UnreadableObjectError>()),
                "{read:?}"
            );
        }
        let mut listed = store.list_org_benchmark_ids().await?;
        listed.sort();
        assert_eq!(
            listed,
            vec![
                (acme.clone(), BenchmarkId::try_new("broken")?),
                (acme.clone(), BenchmarkId::try_new("not_toml")?),
                (acme.clone(), entry.benchmark_id().clone()),
            ]
        );
        Ok(())
    }

    #[tokio::test]
    async fn sql_org_benchmark_ids_skip_invalid_ids() -> anyhow::Result<()> {
        let test_db = crate::testing::test_db_or_skip!();
        let db = test_db.db();
        let dir = tempfile::tempdir()?;
        let store = catalog(db, dir.path())?;
        let acme = OrgId::mint();
        let entry = pipette_store_testkit::an_org_benchmark()?;
        store.put_org_benchmark(&acme, &entry).await?;
        insert_raw(db, "not-an-org", "x", PUBLIC_ENTRY).await?;
        insert_raw(db, acme.as_str(), "Not An Id", PUBLIC_ENTRY).await?;

        assert_eq!(
            store.list_org_benchmark_ids().await?,
            vec![(acme, entry.benchmark_id().clone())]
        );
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
    async fn sql_racing_puts_of_one_entry_all_land() -> anyhow::Result<()> {
        let test_db = crate::testing::test_db_or_skip!();
        let dir = tempfile::tempdir()?;
        let store = Arc::new(catalog(test_db.db(), dir.path())?);
        let acme = OrgId::mint();
        let id = pipette_store_testkit::an_org_benchmark()?
            .benchmark_id()
            .clone();
        let versions = (1..)
            .take(RACERS)
            .map(|tokens: i32| {
                Benchmark::from_def(BenchmarkDefinition::PrefillThroughput(
                    PrefillThroughputBenchmark {
                        benchmark_id: id.clone(),
                        parameter_prefill_tokens: NonNegativeCount::try_new(tokens * 128)?,
                        parameter_batch_size: BatchSize::one(),
                    },
                ))
            })
            .collect::<anyhow::Result<Vec<_>>>()?;
        race(|n| {
            let (store, acme, version) = (store.clone(), acme.clone(), versions[n].clone());
            async move { store.put_org_benchmark(&acme, &version).await }
        })
        .await?;
        let stored = store.get_org_benchmark(&acme, &id).await?;
        assert!(stored.is_some_and(|stored| {
            versions
                .iter()
                .any(|version| version.clone().private_to(acme.clone()) == stored)
        }));
        assert_eq!(store.list_org_benchmark_ids().await?.len(), 1);
        Ok(())
    }
}
