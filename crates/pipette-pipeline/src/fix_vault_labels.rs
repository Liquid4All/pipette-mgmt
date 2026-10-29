//! `fix-vault-labels`: give each vault source in the warehouse's descriptors
//! the `custom_label` its records give it ([`crate::vault_label`]). It rewrites
//! Parquet in place, so the CLI holds the storage mutate lock for the run.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use futures::{StreamExt, TryStreamExt};
use serde_json::Value;

use pipette_core::vault::NoCustomLabel;
use pipette_core::warehouse::MetricRow;
use pipette_plan_types::{JobId, VaultResourceId};
use pipette_store::{STORAGE_CONCURRENCY, Stores, drain_vault_resources};

use crate::vault_check::{job_names, ran_source, reported_resources};
use crate::vault_label::{Fill, Outcome, unlabelled_resources};

/// The descriptor columns a source that takes a label can sit in.
/// `model_vault_descriptor` holds imports alone, which take none.
const COLUMNS: [fn(&mut MetricRow) -> &mut Option<String>; 2] = [
    |row| &mut row.model_descriptor,
    |row| &mut row.runtime_descriptor,
];

/// `only_resources`, when non-empty, narrows the fill to vault sources naming
/// one of them. An id with no vault record is an error.
pub async fn run(
    stores: &Stores,
    dry_run: bool,
    only_resources: &[VaultResourceId],
) -> anyhow::Result<Totals> {
    let listing = drain_vault_resources(&*stores.vault).await?;
    listing.unreadable.iter().for_each(|entry| {
        tracing::warn!(%entry, "skipping malformed vault record");
    });
    // Every record is read up front: the walk cannot await.
    let fill = Fill::narrowed(listing.items, only_resources)?;
    if !only_resources.is_empty() {
        tracing::info!(resources = ?only_resources, "restricting fix-vault-labels to selected resources");
    }

    let sources = job_sources(stores).await?;

    let mut totals = Totals::default();
    stores
        .warehouse
        .for_each_metric_row(&mut |row| {
            let source = ran(row).and_then(|job| sources.get(&job));
            let changed = fill_row(&fill, source, row, dry_run, &mut totals);
            !dry_run && changed
        })
        .await?;
    Ok(totals)
}

/// The job a row's descriptors were reported for: a copy's `rescored_from`.
fn ran(row: &MetricRow) -> Option<JobId> {
    let job = row.rescored_from.as_ref().or(row.job_id.as_ref())?;
    JobId::try_new(job).ok()
}

/// The source of each job a row with an unlabelled vault source reports, read
/// in a first, read-only walk: the filling walk cannot await.
async fn job_sources(stores: &Stores) -> anyhow::Result<HashMap<JobId, Value>> {
    let mut jobs = BTreeSet::new();
    stores
        .warehouse
        .for_each_metric_row(&mut |row| {
            let unlabelled = COLUMNS.into_iter().any(|column| {
                column(row)
                    .as_deref()
                    .is_some_and(|stored| !unlabelled_resources(stored).is_empty())
            });
            jobs.extend(ran(row).filter(|_| unlabelled));
            false
        })
        .await?;
    let sources = futures::stream::iter(jobs)
        .map(|job| async move {
            let source = ran_source(stores, &job).await?;
            anyhow::Ok(source.map(|source| (job, source)))
        })
        .buffer_unordered(STORAGE_CONCURRENCY)
        .try_collect::<Vec<_>>()
        .await?;
    Ok(sources.into_iter().flatten().collect())
}

/// `source` is the body of the job `row` reports; a column naming a vault
/// resource it does not name takes no label ([`job_names`]).
fn fill_row(
    fill: &Fill,
    source: Option<&Value>,
    row: &mut MetricRow,
    dry_run: bool,
    totals: &mut Totals,
) -> bool {
    // Counted, not `any`: every column is filled, not only the first.
    let filled = COLUMNS
        .into_iter()
        .filter(|column| {
            let value = column(row);
            let Some((stored, (filled, outcomes))) =
                value.as_deref().map(|stored| (stored, fill.value(stored)))
            else {
                return false;
            };
            if outcomes.is_empty() {
                return false;
            }
            if !reported_resources([stored]).is_some_and(|reported| job_names(source, &reported)) {
                *totals.skipped.entry("not named by its job").or_default() += 1;
                return false;
            }
            totals.record(outcomes);
            let Some(filled) = filled else {
                return false;
            };
            if !dry_run {
                *value = Some(filled);
            }
            true
        })
        .count();
    totals.rows += usize::from(filled > 0);
    filled > 0
}

/// The summary's name for why a source's records give it no label.
fn reason(why: NoCustomLabel) -> &'static str {
    match why {
        NoCustomLabel::NoRecords => "no vault path",
        NoCustomLabel::Imported => "imported file",
        NoCustomLabel::NamesDiffer => "files under different names",
    }
}

/// Counters for the per-run summary, which an operator reads after a dry run
/// and before the real one.
#[derive(Debug, Default)]
pub struct Totals {
    rows: usize,
    values: usize,
    /// Per resource, the label it gives and how many values took it.
    filled: BTreeMap<VaultResourceId, (String, usize)>,
    /// Vault sources left unlabelled, by why.
    skipped: BTreeMap<&'static str, usize>,
    /// Resources named with no record, each warned about once.
    missing: BTreeSet<VaultResourceId>,
}

impl Totals {
    fn record(&mut self, outcomes: Vec<Outcome>) {
        // A resource counts once per value, however many of its sources took a label.
        let mut filled = BTreeMap::new();
        outcomes.into_iter().for_each(|outcome| match outcome {
            Outcome::Filled(label, ids) => {
                filled.extend(ids.into_iter().map(|id| (id, label.clone())));
            }
            Outcome::Missing(ids) => {
                *self.skipped.entry("missing record").or_default() += 1;
                ids.into_iter()
                    .filter(|id| self.missing.insert(id.clone()))
                    .for_each(|id| {
                        tracing::warn!(
                            resource_id = %id,
                            "a result names a vault resource with no record"
                        );
                    });
            }
            Outcome::OtherOwner => *self.skipped.entry("another owner's record").or_default() += 1,
            Outcome::Unlabelled(why) => *self.skipped.entry(reason(why)).or_default() += 1,
        });
        self.values += usize::from(!filled.is_empty());
        filled
            .into_iter()
            .for_each(|(id, label)| self.filled.entry(id).or_insert((label, 0)).1 += 1);
    }

    pub fn log(&self, dry_run: bool) {
        // Both stdout and tracing: stdout still shows what happened when
        // RUST_LOG filters out info.
        if self.values == 0 {
            println!("fix-vault-labels: no label to fill");
        } else {
            let verb = if dry_run { "would fill" } else { "filled" };
            println!(
                "fix-vault-labels: {verb} {values} values in {rows} rows",
                values = self.values,
                rows = self.rows
            );
        }
        self.filled.iter().for_each(|(id, (label, values))| {
            println!("  {id} {label:?}: {values}");
            tracing::info!(
                resource_id = %id,
                label = %label,
                values,
                dry_run,
                "fix-vault-labels",
            );
        });
        let skipped = self
            .skipped
            .iter()
            .map(|(why, count)| format!("{why} {count}"))
            .collect::<Vec<_>>()
            .join(", ");
        if !skipped.is_empty() {
            println!("fix-vault-labels: vault sources left unlabelled: {skipped}");
        }
        tracing::info!(
            rows = self.rows,
            values = self.values,
            skipped = %skipped,
            dry_run,
            "fix-vault-labels",
        );
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use anyhow::Context;

    use pipette_core::parquet_utils::WriterOpts;
    use pipette_core::vault::VaultResource;
    use pipette_core::warehouse;
    use pipette_store_fs::build_local_fs_stores;
    use pipette_store_testkit::{
        a_huggingface_origin, a_vault_resource, a_vault_runtime, a_vision_upload,
    };

    use super::*;
    use crate::vault_check::{VaultScope, check, descriptor_sha256};
    use crate::vault_label::{ran, reported};

    fn test_stores(dir: &std::path::Path) -> anyhow::Result<Stores> {
        let config = pipette_core::config::Config {
            evals_server_url: "http://unused".to_string(),
            storage: pipette_core::config::StorageConfig::local_fs(dir.to_path_buf()),
            auth_storage: pipette_core::config::StorageConfig::local_fs(dir.to_path_buf()),
            ..pipette_core::config::Config::default()
        };
        build_local_fs_stores(&config)
    }

    /// Seed one partition with `rows` and return its path.
    fn seed(dir: &std::path::Path, rows: &[MetricRow]) -> anyhow::Result<std::path::PathBuf> {
        let part_dir =
            dir.join("warehouse/results/benchmark_id=test/client_id=ev1_c/month=2026-10");
        std::fs::create_dir_all(&part_dir)?;
        let path = part_dir.join("part-0001.parquet");
        std::fs::write(
            &path,
            warehouse::rows_to_parquet_bytes(WriterOpts::default(), rows)?,
        )?;
        Ok(path)
    }

    /// Write the object `record` names, so ingestion's check finds it backed.
    fn write_blob(root: &std::path::Path, record: &VaultResource) -> anyhow::Result<()> {
        let blob = root
            .join("vault-resources")
            .join(record.object_key.as_ref());
        std::fs::create_dir_all(blob.parent().context("a key has a parent")?)?;
        std::fs::write(blob, b"x")?;
        Ok(())
    }

    /// A row of job `result_id`.
    fn row(result_id: &str, model: &str, runtime: Option<&str>) -> MetricRow {
        MetricRow {
            result_id: result_id.into(),
            job_id: Some(result_id.to_string()),
            model_descriptor: Some(model.to_string()),
            model_descriptor_sha256: Some(descriptor_sha256(model)),
            runtime_descriptor: runtime.map(str::to_string),
            runtime_descriptor_sha256: runtime.map(descriptor_sha256),
            value: Some(1.5),
            ..Default::default()
        }
    }

    /// The file's mtime, read a moment before anything could rewrite it.
    fn modified(path: &std::path::Path) -> anyhow::Result<std::time::SystemTime> {
        let mtime = std::fs::metadata(path)?.modified()?;
        std::thread::sleep(std::time::Duration::from_millis(10));
        Ok(mtime)
    }

    /// A dry run writes nothing, the real run fills in place with every hash as
    /// stored, and a second run finds nothing to fill. A row whose job's source
    /// does not name the record, or that has none, takes no label.
    #[tokio::test]
    async fn a_run_fills_in_place_once() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let stores = test_stores(dir.path())?;
        let vision = a_vision_upload()?;
        let build = a_vault_runtime()?;
        let import = VaultResource {
            origin: Some(a_huggingface_origin()?),
            ..a_vault_resource()?
        };
        let stored: Vec<&VaultResource> = vision.iter().chain([&build, &import]).collect();
        futures::future::try_join_all(stored.iter().map(|r| stores.vault.put_resource(r))).await?;
        stored
            .iter()
            .try_for_each(|record| write_blob(dir.path(), record))?;
        let model = vision.first().context("an upload has a record")?;
        let gone = a_vault_resource()?;
        ran(&stores, "upload", &[model, &build]).await?;
        ran(&stores, "import", &[&import]).await?;
        ran(&stores, "gone", &[&gone]).await?;
        ran(&stores, "theirs", &[&build]).await?;
        let public = r#"{"org":"LiquidAI","repo_name":"LFM2-1.2B-GGUF","source":"huggingface","type":"gguf_text"}"#;
        let seeded = [
            row(
                "upload",
                &reported(model, false)?,
                Some(&reported(&build, false)?),
            ),
            row("import", &reported(&import, false)?, None),
            // Behind an alias: the column is not walked.
            MetricRow {
                model_vault_descriptor: Some(reported(&import, false)?),
                ..row("aliased", public, None)
            },
            row("gone", &reported(&gone, false)?, None),
            row("theirs", &reported(model, false)?, None),
            row("unsourced", &reported(model, false)?, None),
            row("public", public, Some("llama.cpp b9050")),
        ];
        let path = seed(dir.path(), &seeded)?;

        let before = modified(&path)?;
        let dry = run(&stores, true, &[]).await?;
        assert_eq!(before, modified(&path)?, "a dry run rewrites nothing");
        assert_eq!((dry.rows, dry.values), (1, 2));
        let label = |name: &str, values| (name.to_string(), values);
        let expected = BTreeMap::from([
            (
                vision[0].resource_id.clone(),
                label("acme LFM2-VL-3B q4", 1),
            ),
            (
                vision[1].resource_id.clone(),
                label("acme LFM2-VL-3B q4", 1),
            ),
            (build.resource_id.clone(), label("acme llama.cpp build", 1)),
        ]);
        assert_eq!(dry.filled, expected);
        let skipped = BTreeMap::from([
            ("imported file", 1),
            ("missing record", 1),
            ("not named by its job", 2),
        ]);
        assert_eq!(dry.skipped, skipped);

        let real = run(&stores, false, &[]).await?;
        assert_eq!((real.rows, real.values), (1, 2));
        assert_eq!(real.filled, expected);
        assert_eq!(real.skipped, skipped);

        let after = warehouse::rows_from_parquet_bytes(&std::fs::read(&path)?)?;
        assert_eq!(after.len(), seeded.len(), "no row may be dropped");
        seeded.iter().zip(&after).for_each(|(was, now)| {
            let id = &now.result_id;
            assert_eq!(was.result_id, *id);
            assert_eq!(
                was.model_descriptor_sha256, now.model_descriptor_sha256,
                "{id}"
            );
            assert_eq!(
                was.runtime_descriptor_sha256, now.runtime_descriptor_sha256,
                "{id}"
            );
            assert_eq!(
                was.model_vault_descriptor, now.model_vault_descriptor,
                "{id}"
            );
            assert_eq!(was.value, now.value, "{id}");
            if id != "upload" {
                assert_eq!(was.model_descriptor, now.model_descriptor, "{id}");
                assert_eq!(was.runtime_descriptor, now.runtime_descriptor, "{id}");
            }
        });
        let filled = after
            .iter()
            .find(|r| r.result_id == "upload")
            .context("the filled row was dropped")?;
        let filled_model = filled.model_descriptor.clone().context("no model")?;
        let filled_runtime = filled.runtime_descriptor.clone().context("no runtime")?;
        assert_eq!(filled_model, reported(model, true)?);
        assert_eq!(filled_runtime, reported(&build, true)?);
        assert_eq!(
            filled.model_descriptor_sha256,
            Some(descriptor_sha256(&filled_model))
        );
        assert_eq!(
            filled.runtime_descriptor_sha256,
            Some(descriptor_sha256(&filled_runtime))
        );
        // Ingestion takes the filled labels as its own.
        for descriptor in [&filled_model, &filled_runtime] {
            check(
                &*stores.vault,
                &VaultScope::Every,
                &serde_json::from_str(descriptor)?,
                &mut HashMap::new(),
            )
            .await
            .map_err(|e| anyhow::anyhow!("ingestion refused {descriptor}: {e:?}"))?;
        }

        let before = modified(&path)?;
        let again = run(&stores, false, &[]).await?;
        assert_eq!(before, modified(&path)?, "a second run rewrites nothing");
        assert_eq!((again.rows, again.values), (0, 0));
        assert!(again.filled.is_empty());
        assert_eq!(again.skipped, skipped);
        Ok(())
    }

    #[tokio::test]
    async fn run_errors_on_an_unknown_resource() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let stores = test_stores(dir.path())?;
        let unknown = a_vault_resource()?.resource_id;
        match run(&stores, true, &[unknown]).await {
            Ok(_) => anyhow::bail!("expected an error for an unknown resource"),
            Err(err) => assert!(err.to_string().contains("is not a vault record"), "{err}"),
        }
        Ok(())
    }
}
