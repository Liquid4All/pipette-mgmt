//! `fix-model-param` subcommand: rewrite warehouse rows' model_params
//! columns from the catalog. See `docs/cli.md` and `docs/storage.md`.
//!
//! Race: the rewrite is read-modify-write on Parquet partitions, so it
//! must not run concurrently with `score` or another `fix-*`. The CLI
//! enforces this by holding the storage mutate lock (see
//! `crate::storage_lock`) for the whole run.

use std::collections::HashSet;
use std::hash::{DefaultHasher, Hash, Hasher};

use crate::model_params::{self, ModelCatalog, ModelEntry};
use crate::stores::Stores;
use crate::warehouse::MetricRow;

/// Entry point used by the binary. Returns the run's counters; the caller
/// renders the summary via [`Totals::log`].
///
/// `only_models`, when non-empty, restricts the rewrite to rows whose
/// catalog identity matches one of the given names — every other row is
/// left untouched and not counted. The filter is normalized with the same
/// rules as catalog lookup, so a single `LFM2.5-230M` matches the base
/// repo, the `-GGUF` repo, and any quant/distribution variant. An empty
/// slice means "every row", the original whole-warehouse behavior.
pub async fn run(
    stores: &Stores,
    catalog: &ModelCatalog,
    dry_run: bool,
    only_models: &[String],
) -> anyhow::Result<Totals> {
    if catalog.is_empty() {
        anyhow::bail!(
            "model catalog is empty (no model_params_mapping.toml or no entries) — nothing to fix from"
        );
    }
    tracing::info!(entries = catalog.len(), "loaded model catalog");

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
            let resolved = resolve(catalog, row);
            if let Some(filter) = &filter
                && !selected(filter, row, resolved.as_ref())
            {
                return false;
            }
            let Some(resolved) = resolved else {
                // Neither `model_name` nor `model_descriptor` gives the row a
                // catalog identity — leave it untouched.
                if warned.insert(unknown_key(row)) {
                    tracing::warn!(
                        model_name = ?row.model_name,
                        model_descriptor = ?row.model_descriptor,
                        "no model_params_mapping.toml entry; row left unchanged"
                    );
                }
                totals.unknown += 1;
                return false;
            };
            let entry = resolved.entry();
            let new_total = Some(entry.total);
            let new_active = Some(entry.active);
            if row.model_params_total_millions == new_total
                && row.model_params_active_millions == new_active
            {
                return false;
            }
            totals.changed += 1;
            totals.via_descriptor += usize::from(matches!(resolved, Resolved::ByDescriptor { .. }));
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

/// A row's catalog identity, and how it was established.
enum Resolved {
    /// Matched by `model_name`. `--model` filters such a row on the name
    /// itself, so there's no key to carry.
    ByName(ModelEntry),
    /// Matched by substring against `model_descriptor`. `key` is the matched
    /// catalog key — the only identity `--model` has to filter a row that
    /// carries no usable `model_name`.
    ByDescriptor { key: String, entry: ModelEntry },
}

impl Resolved {
    fn entry(&self) -> ModelEntry {
        match self {
            Self::ByName(entry) | Self::ByDescriptor { entry, .. } => *entry,
        }
    }
}

/// Resolve a row against the catalog exactly as the scorer does (see
/// `score::resolve_mill_params`): exact `model_name` lookup first, then a
/// substring match against the opaque `model_descriptor` when the name is
/// absent or unrecognized. Without the descriptor fallback, rows from
/// descriptor-only submissions could never be repaired here even though the
/// scorer would have resolved them.
fn resolve(catalog: &ModelCatalog, row: &MetricRow) -> Option<Resolved> {
    if let Some(name) = row.model_name.as_deref()
        && let Some(entry) = catalog.lookup(name)
    {
        return Some(Resolved::ByName(entry));
    }
    let (key, entry) = catalog.resolve_key_from_descriptor(row.model_descriptor.as_deref()?)?;
    Some(Resolved::ByDescriptor {
        key: key.to_string(),
        entry,
    })
}

/// Whether `--model` selects this row. A row matches on either identity: its
/// normalized `model_name`, or — for descriptor-only rows, which have no name
/// to compare — the catalog key its `model_descriptor` resolved to.
fn selected(filter: &HashSet<String>, row: &MetricRow, resolved: Option<&Resolved>) -> bool {
    row.model_name
        .as_deref()
        .is_some_and(|n| filter.contains(&model_params::normalize(n)))
        || matches!(resolved, Some(Resolved::ByDescriptor { key, .. }) if filter.contains(key))
}

/// Dedup token for the once-per-run unknown warning: the `model_name` when
/// there is one, else the descriptor, else a single shared token so rows with
/// no identity at all collapse to one warning.
///
/// Hashed rather than stored: descriptors embed per-build revisions and paths,
/// so keying the set on the string would grow it with every distinct build
/// rather than every distinct model.
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
    /// Subset of `changed` resolved through `model_descriptor` rather than
    /// `model_name`.
    pub via_descriptor: usize,
    pub unknown: usize,
}

impl Totals {
    pub fn log(&self, msg: &'static str, dry_run: bool) {
        // Both stdout and tracing: a CLI run without RUST_LOG=info
        // still shows what happened.
        if self.changed == 0 && self.unknown == 0 {
            println!("{msg}: no work needed (every row already aligned with the catalog)");
            tracing::info!(dry_run, "{msg}: no work needed");
            return;
        }
        let verb = if dry_run { "would update" } else { "updated" };
        println!(
            "{msg}: {verb} {changed} rows ({via_descriptor} resolved via model_descriptor); \
             {unknown} rows reference models not in the catalog and were left unchanged",
            changed = self.changed,
            via_descriptor = self.via_descriptor,
            unknown = self.unknown,
        );
        tracing::info!(
            changed = self.changed,
            via_descriptor = self.via_descriptor,
            unknown = self.unknown,
            dry_run,
            msg,
        );
    }
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use crate::fix_model_param::*;
    use crate::parquet_utils::WriterOpts;
    use crate::stores::build_local_fs_stores;
    use crate::types::{BenchmarkId, ClientId};
    use crate::warehouse::{self, MetricRow};

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
            device_name: "d".to_string(),
            device_chip_model: "chip".to_string(),
            device_ram_bytes: 1_000_000_000,
            model_name: Some(model_name.to_string()),
            model_params_total_millions: total,
            model_params_active_millions: active,
            value: 1.0,
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

    fn test_catalog() -> ModelCatalog {
        ModelCatalog::from_toml(
            r#"
"LFM2-700M" = 742
"LFM2.5-2.6B" = 2697
"LFM2-8B-A1B" = { total = 8340, active = 1500 }
"#,
        )
        .unwrap()
    }

    fn test_stores(dir: &std::path::Path) -> Stores {
        let config = crate::config::Config {
            evals_server_url: "http://unused".to_string(),
            storage: crate::config::StorageConfig::local_fs(dir.to_path_buf()),
            auth_storage: crate::config::StorageConfig::local_fs(dir.to_path_buf()),
            ..crate::config::Config::default()
        };
        build_local_fs_stores(&config).unwrap()
    }

    /// Seed a single warehouse partition with `rows`, run `fix-model-param`
    /// restricted to `only_models`, and return the rewritten rows alongside
    /// the run's counters. Used by the value-asserting tests, which differ
    /// only in their inputs and expected `(total, active)`.
    async fn run_fix(
        rows: Vec<MetricRow>,
        only_models: &[&str],
    ) -> anyhow::Result<(Vec<MetricRow>, Totals)> {
        let dir = tempfile::tempdir()?;
        let stores = test_stores(dir.path());

        let part_dir = dir
            .path()
            .join("warehouse/results/benchmark_id=bench1/client_id=c1/month=2026-03");
        std::fs::create_dir_all(&part_dir)?;
        let bytes = warehouse::rows_to_parquet_bytes(WriterOpts::default(), &rows)?;
        let path = part_dir.join("part-0001.parquet");
        std::fs::write(&path, bytes)?;

        let only: Vec<String> = only_models.iter().map(|s| s.to_string()).collect();
        let totals = run(&stores, &test_catalog(), false, &only).await?;

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

    /// `run` rewrites the right rows to the right `(total, active)`.
    ///
    /// - `seed`: `(model_name, seeded total = active)` rows written before the run.
    /// - `only`: `--model` filter (empty = whole warehouse).
    /// - `expect`: `(model_name, expected total, expected active)` after the run.
    #[rstest]
    // No filter: dense + MoE catalog hits are rewritten, the unknown row is left as-is.
    #[case::no_filter(
        vec![("LFM2-700M", 999), ("LFM2-8B-A1B", 9999), ("totally-unknown", 42)],
        vec![],
        vec![("LFM2-700M", 742, 742), ("LFM2-8B-A1B", 8340, 1500), ("totally-unknown", 42, 42)],
    )]
    // Filter by the base name: the `-GGUF` variant matches via normalization;
    // the in-catalog MoE row is excluded and must stay untouched.
    #[case::filter_normalizes_variants(
        vec![("LiquidAI/LFM2-700M-GGUF", 999), ("LFM2-8B-A1B", 9999)],
        vec!["LiquidAI/LFM2-700M"],
        vec![("LiquidAI/LFM2-700M-GGUF", 742, 742), ("LFM2-8B-A1B", 9999, 9999)],
    )]
    #[tokio::test]
    async fn run_rewrites_expected_rows(
        #[case] seed: Vec<(&str, i32)>,
        #[case] only: Vec<&str>,
        #[case] expect: Vec<(&str, i32, i32)>,
    ) -> anyhow::Result<()> {
        let rows = seed
            .iter()
            .map(|(name, v)| make_row(name, Some(*v), Some(*v)))
            .collect::<anyhow::Result<Vec<_>>>()?;
        let (after, _) = run_fix(rows, &only).await?;
        expect
            .iter()
            .try_for_each(|(name, total, active)| assert_params(&after, name, *total, *active))
    }

    /// The descriptor a `LFM2.5-2.6B` GGUF submission carries when it sends no
    /// `model_name` at all — canonical JSON, keys sorted.
    const DESCRIPTOR_2_6B: &str = r#"{"org":"LiquidAI","path":"LFM2.5-2.6B-Q4_K_M.gguf","repo_name":"LFM2.5-2.6B-GGUF","revision":"0074c93826b862d71610b19e41f19277e8fdbaca","source":"huggingface","type":"gguf_text"}"#;

    /// Assert the `(total, active)` params on the row with `result_id` — the
    /// descriptor-only rows have no `model_name` to look them up by.
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

    /// A row carrying only `model_descriptor` gets repaired, and `--model`
    /// selects it through the catalog key that descriptor resolves to. The
    /// scorer resolves such submissions through the descriptor, so the fixer
    /// has to as well or those rows are unreachable forever.
    ///
    /// `expect_totals` pins which path did the work: a selected row is one
    /// `changed` counted under `via_descriptor`, an excluded row is no work at
    /// all — distinguishing "filtered out" from "failed to resolve", which the
    /// row values alone can't.
    #[rstest]
    #[case::no_filter(vec![], 2697, 2697, Totals { changed: 1, via_descriptor: 1, unknown: 0 })]
    #[case::selected(vec!["LiquidAI/LFM2.5-2.6B-GGUF"], 2697, 2697, Totals { changed: 1, via_descriptor: 1, unknown: 0 })]
    #[case::selected_by_base_name(vec!["LFM2.5-2.6B"], 2697, 2697, Totals { changed: 1, via_descriptor: 1, unknown: 0 })]
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

    /// Mirrors the scorer's resolution order: an unrecognized `model_name`
    /// falls through to the descriptor rather than counting as unknown.
    #[tokio::test]
    async fn unknown_model_name_falls_back_to_descriptor() -> anyhow::Result<()> {
        let row = MetricRow {
            model_descriptor: Some(DESCRIPTOR_2_6B.to_string()),
            ..make_row("some-vendor-alias", Some(1), Some(1))?
        };
        let (after, totals) = run_fix(vec![row], &[]).await?;
        assert_eq!(
            totals,
            Totals {
                changed: 1,
                via_descriptor: 1,
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
                via_descriptor: 0,
                unknown: 1
            }
        );
        assert_params_by_id(&after, "anonymous_0", 7, 7)
    }

    #[tokio::test]
    async fn run_skips_aligned_files() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let stores = test_stores(dir.path());

        let part_dir = dir
            .path()
            .join("warehouse/results/benchmark_id=bench1/client_id=c1/month=2026-03");
        std::fs::create_dir_all(&part_dir)?;
        let rows = vec![make_row("LFM2-700M", Some(742), Some(742))?];
        let bytes = warehouse::rows_to_parquet_bytes(WriterOpts::default(), &rows)?;
        let path = part_dir.join("part-0001.parquet");
        std::fs::write(&path, &bytes)?;
        let mtime_before = std::fs::metadata(&path)?.modified()?;

        std::thread::sleep(std::time::Duration::from_millis(10));
        let totals = run(&stores, &test_catalog(), false, &[]).await?;

        assert_eq!(totals, Totals::default());
        let mtime_after = std::fs::metadata(&path)?.modified()?;
        assert_eq!(
            mtime_before, mtime_after,
            "aligned file should not have been rewritten"
        );
        Ok(())
    }

    #[tokio::test]
    async fn run_errors_when_catalog_is_empty() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let stores = test_stores(dir.path());
        let err = run(&stores, &ModelCatalog::empty(), false, &[])
            .await
            .unwrap_err();
        assert!(err.to_string().contains("catalog is empty"), "got: {err}");
        Ok(())
    }

    #[tokio::test]
    async fn dry_run_does_not_rewrite() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let stores = test_stores(dir.path());

        let part_dir = dir
            .path()
            .join("warehouse/results/benchmark_id=bench1/client_id=c1/month=2026-03");
        std::fs::create_dir_all(&part_dir)?;
        let rows = vec![make_row("LFM2-700M", Some(999), Some(999))?];
        let bytes = warehouse::rows_to_parquet_bytes(WriterOpts::default(), &rows)?;
        let path = part_dir.join("part-0001.parquet");
        std::fs::write(&path, &bytes)?;
        let mtime_before = std::fs::metadata(&path)?.modified()?;

        std::thread::sleep(std::time::Duration::from_millis(10));
        let totals = run(&stores, &test_catalog(), true, &[]).await?;

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
        // Two distinct unknown models across three rows. `warned` dedupes the
        // log line per distinct name, but `unknown` counts rows — so the count
        // is 3, not 2. Log content itself isn't assertable here.
        let dir = tempfile::tempdir()?;
        let stores = test_stores(dir.path());

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
        let totals = run(&stores, &test_catalog(), false, &[]).await?;

        assert_eq!(
            totals,
            Totals {
                changed: 0,
                via_descriptor: 0,
                unknown: 3
            }
        );
        // No catalog hits → file untouched.
        let mtime_after = std::fs::metadata(&path)?.modified()?;
        assert_eq!(mtime_before, mtime_after);
        Ok(())
    }
}
