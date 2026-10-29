//! `fix-model-param`: rewrite warehouse rows' model_params columns from the
//! table. It rewrites Parquet in place, so the CLI holds the storage mutate
//! lock for the whole run.

use std::collections::HashSet;
use std::hash::{DefaultHasher, Hash, Hasher};

use pipette_core::warehouse::MetricRow;
use pipette_store::Stores;

use crate::model_params::{self, ModelEntry, ModelParamTable};

/// `only_models`, when non-empty, keeps rows whose model key matches. The
/// match normalizes names, so `LFM2.5-230M` also matches its `-GGUF` and
/// quant variants.
pub async fn run(
    stores: &Stores,
    table: &ModelParamTable,
    dry_run: bool,
    only_models: &[String],
) -> anyhow::Result<Totals> {
    if table.is_empty() {
        anyhow::bail!(
            "model parameter table is empty (no model-params rows), so there is nothing to fix from"
        );
    }
    tracing::info!(
        entries = table.len(),
        "fix-model-param using model parameter table"
    );

    let filter: Option<HashSet<String>> = if only_models.is_empty() {
        None
    } else {
        let set: HashSet<String> = only_models
            .iter()
            .map(|m| model_params::normalize(m))
            .collect();
        tracing::info!(models = ?set, "restricting fix-model-param to selected models");
        Some(set)
    };

    let mut totals = Totals::default();
    let mut warned: HashSet<u64> = HashSet::new();
    stores
        .warehouse
        .for_each_metric_row(&mut |row| {
            let resolved = resolve(table, row);
            if let Some(filter) = &filter
                && !selected(filter, resolved.as_ref())
            {
                return false;
            }
            let Some(resolved) = resolved else {
                // No `model_descriptor` the table can place: leave it alone.
                if warned.insert(unknown_key(row)) {
                    tracing::warn!(
                        model_name = ?row.model_name,
                        model_descriptor = ?row.model_descriptor,
                        "no model-params row; row left unchanged"
                    );
                }
                totals.unknown += 1;
                return false;
            };
            let entry = resolved.entry;
            let new_total = Some(entry.total);
            let new_active = Some(entry.active);
            if row.model_params_total_millions == new_total
                && row.model_params_active_millions == new_active
            {
                return false;
            }
            totals.changed += 1;
            if dry_run {
                return false;
            }
            row.model_params_total_millions = new_total;
            row.model_params_active_millions = new_active;
            true
        })
        .await?;
    Ok(totals)
}

/// A row's table identity: the key its `model_descriptor` resolved to, and
/// the entry under it.
pub struct Resolved {
    /// The matched model key — the identity `--model` filters a row on.
    pub key: String,
    pub entry: ModelEntry,
}

/// Resolve a row by its descriptors, as the scorer does; `model_name` is
/// client-set and never consulted. Shared with [`crate::model_audit`] so the
/// audit cannot promise a repair this would not make.
pub fn resolve(table: &ModelParamTable, row: &MetricRow) -> Option<Resolved> {
    table
        .resolve_recorded(
            row.model_descriptor.as_deref(),
            row.model_vault_descriptor.as_deref(),
        )
        .map(|(key, entry)| Resolved { key, entry })
}

/// A row that resolves to nothing is never repaired, so never selected.
fn selected(filter: &HashSet<String>, resolved: Option<&Resolved>) -> bool {
    matches!(resolved, Some(Resolved { key, .. }) if filter.contains(key))
}

/// Dedup token for the once-per-run unknown warning. Hashed: descriptors embed
/// per-build revisions, so storing them would grow the set per build.
fn unknown_key(row: &MetricRow) -> u64 {
    let mut hasher = DefaultHasher::new();
    match (&row.model_name, &row.model_descriptor) {
        (Some(name), _) => (0u8, name).hash(&mut hasher),
        (None, Some(descriptor)) => (1u8, descriptor).hash(&mut hasher),
        (None, None) => 2u8.hash(&mut hasher),
    }
    hasher.finish()
}

/// Aggregate counters for the per-run summary.
#[derive(Default, Debug, PartialEq, Eq)]
pub struct Totals {
    pub changed: usize,
    pub unknown: usize,
}

impl Totals {
    pub fn log(&self, msg: &'static str, dry_run: bool) {
        // Both stdout and tracing: stdout still shows what happened when
        // RUST_LOG filters out info.
        if self.changed == 0 && self.unknown == 0 {
            println!(
                "{msg}: no work needed (every row already aligned with the model parameter table)"
            );
            tracing::info!(msg, dry_run, "no work needed");
            return;
        }
        let verb = if dry_run { "would update" } else { "updated" };
        println!(
            "{msg}: {verb} {changed} rows; {unknown} rows name no model the table can place \
             and were left unchanged",
            changed = self.changed,
            unknown = self.unknown,
        );
        tracing::info!(
            msg,
            changed = self.changed,
            unknown = self.unknown,
            dry_run,
            "fix-model-param: done"
        );
    }
}

#[cfg(test)]
mod tests {
    use anyhow::Context;
    use rstest::rstest;

    use pipette_core::parquet_utils::WriterOpts;
    use pipette_core::warehouse::{self, MetricRow};
    use pipette_plan_types::{BenchmarkId, ClientId};
    use pipette_store_fs::build_local_fs_stores;

    use crate::fix_model_param::*;

    fn make_row(
        model_name: &str,
        total: Option<i32>,
        active: Option<i32>,
    ) -> anyhow::Result<MetricRow> {
        Ok(MetricRow {
            result_id: format!("{model_name}_0"),
            benchmark_id: BenchmarkId::try_new("bench1")?,
            metric: "ttft".to_string(),
            client_id: ClientId::try_new("c1")?,
            device_name: Some("d".to_string()),
            device_chip_model: Some("chip".to_string()),
            device_ram_bytes: Some(1_000_000_000),
            model_name: Some(model_name.to_string()),
            model_params_total_millions: total,
            model_params_active_millions: active,
            value: Some(1.0),
            unit: "ms".to_string(),
            submitted_at: 1_000_000,
            scored_at: 2_000_000,
            parameter_prefill_tokens: Some(256),
            ..Default::default()
        })
    }

    /// A row with no `model_name`, carrying only the opaque descriptor — the
    /// shape a descriptor-first client produces.
    fn make_descriptor_row(
        result_id: &str,
        descriptor: &str,
        total: Option<i32>,
        active: Option<i32>,
    ) -> anyhow::Result<MetricRow> {
        Ok(MetricRow {
            model_name: None,
            model_descriptor: Some(descriptor.to_string()),
            ..make_row(result_id, total, active)?
        })
    }

    fn test_table() -> anyhow::Result<ModelParamTable> {
        ModelParamTable::from_toml(
            r#"
"LFM2-700M" = 742
"LFM2.5-2.6B" = 2697
"LFM2-8B-A1B" = { total = 8340, active = 1500 }
"#,
        )
    }

    fn test_stores(dir: &std::path::Path) -> anyhow::Result<Stores> {
        let config = pipette_core::config::Config {
            evals_server_url: "http://unused".to_string(),
            storage: pipette_core::config::StorageConfig::local_fs(dir.to_path_buf()),
            auth_storage: pipette_core::config::StorageConfig::local_fs(dir.to_path_buf()),
            ..pipette_core::config::Config::default()
        };
        build_local_fs_stores(&config)
    }

    /// Seed one partition with `rows` and run `fix-model-param` over `only_models`.
    async fn run_fix(
        rows: Vec<MetricRow>,
        only_models: &[&str],
    ) -> anyhow::Result<(Vec<MetricRow>, Totals)> {
        let dir = tempfile::tempdir()?;
        let stores = test_stores(dir.path())?;

        let part_dir = dir
            .path()
            .join("warehouse/results/benchmark_id=bench1/client_id=c1/month=2026-03");
        std::fs::create_dir_all(&part_dir)?;
        let bytes = warehouse::rows_to_parquet_bytes(WriterOpts::default(), &rows)?;
        let path = part_dir.join("part-0001.parquet");
        std::fs::write(&path, bytes)?;

        let only: Vec<String> = only_models.iter().map(|s| s.to_string()).collect();
        let totals = run(&stores, &test_table()?, false, &only).await?;

        Ok((
            warehouse::rows_from_parquet_bytes(&std::fs::read(&path)?)?,
            totals,
        ))
    }

    /// Assert the `(total, active)` params on the row named `name`.
    fn assert_params(
        rows: &[MetricRow],
        name: &str,
        total: i32,
        active: i32,
    ) -> anyhow::Result<()> {
        let row = rows
            .iter()
            .find(|r| r.model_name.as_deref() == Some(name))
            .ok_or_else(|| anyhow::anyhow!("missing row {name}"))?;
        assert_eq!(
            row.model_params_total_millions,
            Some(total),
            "total for {name}"
        );
        assert_eq!(
            row.model_params_active_millions,
            Some(active),
            "active for {name}"
        );
        Ok(())
    }

    /// A gguf descriptor for `repo`, the shape a client sends.
    fn gguf(repo: &str) -> String {
        format!(
            r#"{{"type":"gguf_text","source":"huggingface","org":"LiquidAI","repo_name":"{repo}","path":"{repo}-Q4_K_M.gguf"}}"#
        )
    }

    /// Rows are keyed by `model_name` only for the assertions. `seed` is
    /// `(name, repo, total = active)`; `expect` is `(name, total, active)`.
    #[rstest]
    // No filter: dense + MoE table hits are rewritten, the unknown row is left as-is.
    #[case::no_filter(
        vec![
            ("row-a", "LFM2-700M-GGUF", 999),
            ("row-b", "LFM2-8B-A1B-GGUF", 9999),
            ("row-c", "totally-unknown-GGUF", 42),
        ],
        vec![],
        vec![("row-a", 742, 742), ("row-b", 8340, 1500), ("row-c", 42, 42)],
    )]
    // Filter by the base name: the `-GGUF` variant matches via normalization;
    // the in-table MoE row is excluded and must stay untouched.
    #[case::filter_normalizes_variants(
        vec![("row-a", "LFM2-700M-GGUF", 999), ("row-b", "LFM2-8B-A1B-GGUF", 9999)],
        vec!["LiquidAI/LFM2-700M"],
        vec![("row-a", 742, 742), ("row-b", 9999, 9999)],
    )]
    #[tokio::test]
    async fn run_rewrites_expected_rows(
        #[case] seed: Vec<(&str, &str, i32)>,
        #[case] only: Vec<&str>,
        #[case] expect: Vec<(&str, i32, i32)>,
    ) -> anyhow::Result<()> {
        let rows = seed
            .iter()
            .map(|(name, repo, v)| {
                Ok(MetricRow {
                    model_descriptor: Some(gguf(repo)),
                    ..make_row(name, Some(*v), Some(*v))?
                })
            })
            .collect::<anyhow::Result<Vec<_>>>()?;
        let (after, _) = run_fix(rows, &only).await?;
        expect
            .iter()
            .try_for_each(|(name, total, active)| assert_params(&after, name, *total, *active))
    }

    /// The name is client-set; only the descriptor establishes a repair.
    #[tokio::test]
    async fn a_catalogued_name_without_a_descriptor_is_not_repaired() -> anyhow::Result<()> {
        let (after, totals) =
            run_fix(vec![make_row("LFM2-700M", Some(999), Some(999))?], &[]).await?;

        assert_eq!(
            totals,
            Totals {
                changed: 0,
                unknown: 1
            }
        );
        assert_params(&after, "LFM2-700M", 999, 999)
    }

    /// The descriptor a `LFM2.5-2.6B` GGUF submission carries when it sends no
    /// `model_name` at all — canonical JSON, keys sorted.
    const DESCRIPTOR_2_6B: &str = r#"{"org":"LiquidAI","path":"LFM2.5-2.6B-Q4_K_M.gguf","repo_name":"LFM2.5-2.6B-GGUF","revision":"0074c93826b862d71610b19e41f19277e8fdbaca","source":"huggingface","type":"gguf_text"}"#;

    /// `assert_params` by `result_id`, for rows with no `model_name`.
    fn assert_params_by_id(
        rows: &[MetricRow],
        result_id: &str,
        total: i32,
        active: i32,
    ) -> anyhow::Result<()> {
        let row = rows
            .iter()
            .find(|r| r.result_id == result_id)
            .ok_or_else(|| anyhow::anyhow!("missing row {result_id}"))?;
        assert_eq!(row.model_params_total_millions, Some(total), "total");
        assert_eq!(row.model_params_active_millions, Some(active), "active");
        Ok(())
    }

    /// The fixer must resolve descriptor-only rows as the scorer does.
    /// `expect_totals` tells "filtered out" from "failed to resolve", which the
    /// row values cannot.
    #[rstest]
    #[case::no_filter(vec![], 2697, 2697, Totals { changed: 1, unknown: 0 })]
    #[case::selected(vec!["LiquidAI/LFM2.5-2.6B-GGUF"], 2697, 2697, Totals { changed: 1, unknown: 0 })]
    #[case::selected_by_base_name(vec!["LFM2.5-2.6B"], 2697, 2697, Totals { changed: 1, unknown: 0 })]
    #[case::excluded(vec!["LFM2-700M"], 1, 1, Totals::default())]
    #[tokio::test]
    async fn descriptor_only_row_resolves_and_honors_filter(
        #[case] only: Vec<&str>,
        #[case] total: i32,
        #[case] active: i32,
        #[case] expect_totals: Totals,
    ) -> anyhow::Result<()> {
        let rows = vec![make_descriptor_row("d", DESCRIPTOR_2_6B, Some(1), Some(1))?];
        let (after, totals) = run_fix(rows, &only).await?;
        assert_eq!(totals, expect_totals);
        assert_params_by_id(&after, "d_0", total, active)
    }

    /// Mirrors the scorer: the descriptor decides, whatever `model_name` says.
    #[tokio::test]
    async fn an_unrecognized_model_name_does_not_stop_the_descriptor() -> anyhow::Result<()> {
        let row = MetricRow {
            model_descriptor: Some(DESCRIPTOR_2_6B.to_string()),
            ..make_row("some-vendor-alias", Some(1), Some(1))?
        };
        let (after, totals) = run_fix(vec![row], &[]).await?;
        assert_eq!(
            totals,
            Totals {
                changed: 1,
                unknown: 0
            }
        );
        assert_params(&after, "some-vendor-alias", 2697, 2697)
    }

    #[tokio::test]
    async fn row_without_any_model_identity_is_left_unchanged() -> anyhow::Result<()> {
        let row = MetricRow {
            model_name: None,
            ..make_row("anonymous", Some(7), Some(7))?
        };
        let (after, totals) = run_fix(vec![row], &[]).await?;
        assert_eq!(
            totals,
            Totals {
                changed: 0,
                unknown: 1
            }
        );
        assert_params_by_id(&after, "anonymous_0", 7, 7)
    }

    #[tokio::test]
    async fn run_skips_aligned_files() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let stores = test_stores(dir.path())?;

        let part_dir = dir
            .path()
            .join("warehouse/results/benchmark_id=bench1/client_id=c1/month=2026-03");
        std::fs::create_dir_all(&part_dir)?;
        let rows = vec![MetricRow {
            model_descriptor: Some(gguf("LFM2-700M-GGUF")),
            ..make_row("LFM2-700M", Some(742), Some(742))?
        }];
        let bytes = warehouse::rows_to_parquet_bytes(WriterOpts::default(), &rows)?;
        let path = part_dir.join("part-0001.parquet");
        std::fs::write(&path, &bytes)?;
        let mtime_before = std::fs::metadata(&path)?.modified()?;

        std::thread::sleep(std::time::Duration::from_millis(10));
        let totals = run(&stores, &test_table()?, false, &[]).await?;

        assert_eq!(totals, Totals::default());
        let mtime_after = std::fs::metadata(&path)?.modified()?;
        assert_eq!(
            mtime_before, mtime_after,
            "aligned file should not have been rewritten"
        );
        Ok(())
    }

    #[tokio::test]
    async fn run_errors_when_table_is_empty() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let stores = test_stores(dir.path())?;
        let err = run(&stores, &ModelParamTable::empty(), false, &[])
            .await
            .err()
            .context("run must reject this")?;
        assert!(
            err.to_string().contains("model parameter table is empty"),
            "got: {err}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn dry_run_does_not_rewrite() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let stores = test_stores(dir.path())?;

        let part_dir = dir
            .path()
            .join("warehouse/results/benchmark_id=bench1/client_id=c1/month=2026-03");
        std::fs::create_dir_all(&part_dir)?;
        let rows = vec![MetricRow {
            model_descriptor: Some(gguf("LFM2-700M-GGUF")),
            ..make_row("LFM2-700M", Some(999), Some(999))?
        }];
        let bytes = warehouse::rows_to_parquet_bytes(WriterOpts::default(), &rows)?;
        let path = part_dir.join("part-0001.parquet");
        std::fs::write(&path, &bytes)?;
        let mtime_before = std::fs::metadata(&path)?.modified()?;

        std::thread::sleep(std::time::Duration::from_millis(10));
        let totals = run(&stores, &test_table()?, true, &[]).await?;

        // The row is still counted as `changed` — a dry run reports what a
        // live run would do.
        assert_eq!(totals.changed, 1);
        let mtime_after = std::fs::metadata(&path)?.modified()?;
        assert_eq!(
            mtime_before, mtime_after,
            "dry run should not rewrite parquet files"
        );
        let after = warehouse::rows_from_parquet_bytes(&std::fs::read(&path)?)?;
        assert_eq!(after[0].model_params_total_millions, Some(999));
        assert_eq!(after[0].model_params_active_millions, Some(999));
        Ok(())
    }

    #[tokio::test]
    async fn run_counts_every_unknown_row() -> anyhow::Result<()> {
        // `warned` dedupes the log per name, but `unknown` counts rows: 3, not 2.
        let dir = tempfile::tempdir()?;
        let stores = test_stores(dir.path())?;

        let part_dir = dir
            .path()
            .join("warehouse/results/benchmark_id=bench1/client_id=c1/month=2026-03");
        std::fs::create_dir_all(&part_dir)?;
        let rows = vec![
            make_row("unknown-a", Some(1), Some(1))?,
            make_row("unknown-a", Some(2), Some(2))?,
            make_row("unknown-b", Some(3), Some(3))?,
        ];
        let bytes = warehouse::rows_to_parquet_bytes(WriterOpts::default(), &rows)?;
        let path = part_dir.join("part-0001.parquet");
        std::fs::write(&path, &bytes)?;
        let mtime_before = std::fs::metadata(&path)?.modified()?;

        std::thread::sleep(std::time::Duration::from_millis(10));
        let totals = run(&stores, &test_table()?, false, &[]).await?;

        assert_eq!(
            totals,
            Totals {
                changed: 0,
                unknown: 3
            }
        );
        // No table hits → file untouched.
        let mtime_after = std::fs::metadata(&path)?.modified()?;
        assert_eq!(mtime_before, mtime_after);
        Ok(())
    }
}
