//! `fix-canonical` subcommand: re-canonicalize the warehouse's opaque JSON
//! columns and recompute their `_sha256` content ids. See `docs/cli.md` and
//! `docs/storage.md`.
//!
//! Five columns are opaque to the server but canonicalized before storage so
//! that pattern search and grouping over them are stable: `model_descriptor`,
//! `runtime_descriptor`, `benchmark_flags`, `model_flags`, `runtime_flags`.
//! Rows written before a column was canonicalized — or before its hash column
//! existed — hold whatever the client sent, which silently splits one logical
//! configuration across several grouping buckets. This command applies the
//! current [`crate::canonical_json`] rules to every row so historical data
//! groups with new data.
//!
//! It is deliberately whole-family rather than per-column: the canonicalization
//! rules are shared, so a change to them (or a new opaque column) is fixed by
//! re-running this rather than adding another `fix-*`.
//!
//! Race: the rewrite is read-modify-write on Parquet partitions, so it
//! must not run concurrently with `score` or another `fix-*`. The CLI
//! enforces this by holding the storage mutate lock (see
//! `crate::storage_lock`) for the whole run.

use crate::canonical_json::{canonicalize_flags, canonicalize_str, sha256_hex};
use crate::stores::Stores;
use crate::warehouse::MetricRow;

/// The opaque columns this command owns, as CLI-facing names.
pub const COLUMNS: [&str; 5] = [
    "model_descriptor",
    "runtime_descriptor",
    "benchmark_flags",
    "model_flags",
    "runtime_flags",
];

/// Entry point used by the binary.
///
/// `only_columns`, when non-empty, restricts the rewrite to the named columns;
/// every other column is left as stored and not counted. Names are the
/// [`COLUMNS`] entries; an unknown name is an error rather than a silent no-op,
/// since a typo would otherwise report "nothing to do" on a real backlog.
pub async fn run(stores: &Stores, dry_run: bool, only_columns: &[String]) -> anyhow::Result<()> {
    let selected = Selection::parse(only_columns)?;

    let mut totals = Totals::default();
    stores
        .warehouse
        .for_each_metric_row(&mut |row| {
            let changed = normalize_row(row, &selected, dry_run);
            totals.record(&changed);
            // In a dry run nothing is mutated, so the file is never marked
            // dirty — `normalize_row` has already counted what it would do.
            !dry_run && changed.any()
        })
        .await?;
    totals.log(dry_run);
    Ok(())
}

/// Which columns the current run is allowed to touch.
struct Selection {
    model_descriptor: bool,
    runtime_descriptor: bool,
    benchmark_flags: bool,
    model_flags: bool,
    runtime_flags: bool,
}

impl Selection {
    /// Empty input means "every column" — the whole-warehouse default.
    fn parse(only: &[String]) -> anyhow::Result<Self> {
        if only.is_empty() {
            return Ok(Self {
                model_descriptor: true,
                runtime_descriptor: true,
                benchmark_flags: true,
                model_flags: true,
                runtime_flags: true,
            });
        }
        if let Some(unknown) = only.iter().find(|n| !COLUMNS.contains(&n.as_str())) {
            anyhow::bail!(
                "unknown column {unknown:?} — expected one of: {}",
                COLUMNS.join(", ")
            );
        }
        let has = |name: &str| only.iter().any(|c| c == name);
        let selection = Self {
            model_descriptor: has("model_descriptor"),
            runtime_descriptor: has("runtime_descriptor"),
            benchmark_flags: has("benchmark_flags"),
            model_flags: has("model_flags"),
            runtime_flags: has("runtime_flags"),
        };
        tracing::info!(columns = ?only, "restricting fix-canonical to selected columns");
        Ok(selection)
    }
}

/// Per-column tally of what a single row needed. Returned rather than applied
/// directly so a dry run counts exactly what a live run would change.
#[derive(Default)]
struct RowChanges {
    model_descriptor: bool,
    runtime_descriptor: bool,
    benchmark_flags: bool,
    model_flags: bool,
    runtime_flags: bool,
}

impl RowChanges {
    fn any(&self) -> bool {
        self.model_descriptor
            || self.runtime_descriptor
            || self.benchmark_flags
            || self.model_flags
            || self.runtime_flags
    }
}

/// Bring one row's opaque columns to canonical form, recomputing each hash from
/// the canonical value. Returns which columns differed; mutates the row only
/// when `dry_run` is false.
///
/// A hash is recomputed whenever its value column is in scope, not only when
/// the value changed — that is what backfills `model_flags_sha256` /
/// `runtime_flags_sha256` onto rows written before those columns existed.
fn normalize_row(row: &mut MetricRow, selected: &Selection, dry_run: bool) -> RowChanges {
    // Unparseable input passes through trimmed — the documented behavior for an
    // opaque blob. The flags columns add the top-level-`{}`-is-NULL rule on top,
    // so a row stored before that rule joins the same bucket as one after it.
    let descriptor = |value: Option<&str>| value.map(canonicalize_str);

    RowChanges {
        model_descriptor: selected.model_descriptor
            && normalize_column(
                &mut row.model_descriptor,
                &mut row.model_descriptor_sha256,
                descriptor,
                dry_run,
            ),
        runtime_descriptor: selected.runtime_descriptor
            && normalize_column(
                &mut row.runtime_descriptor,
                &mut row.runtime_descriptor_sha256,
                descriptor,
                dry_run,
            ),
        benchmark_flags: selected.benchmark_flags
            && normalize_column(
                &mut row.benchmark_flags,
                &mut row.benchmark_flags_sha256,
                canonicalize_flags,
                dry_run,
            ),
        model_flags: selected.model_flags
            && normalize_column(
                &mut row.model_flags,
                &mut row.model_flags_sha256,
                canonicalize_flags,
                dry_run,
            ),
        runtime_flags: selected.runtime_flags
            && normalize_column(
                &mut row.runtime_flags,
                &mut row.runtime_flags_sha256,
                canonicalize_flags,
                dry_run,
            ),
    }
}

/// Bring one `(value, hash)` column pair to canonical form. Returns whether
/// either differed from what was stored; writes only when `dry_run` is false.
///
/// Taking the pair together is the point: it is the only place the two field
/// names are matched up, so a column cannot be silently paired with another
/// column's hash.
fn normalize_column(
    value: &mut Option<String>,
    hash: &mut Option<String>,
    canonicalize: impl Fn(Option<&str>) -> Option<String>,
    dry_run: bool,
) -> bool {
    let new_value = canonicalize(value.as_deref());
    let new_hash = new_value.as_deref().map(sha256_hex);
    if new_value == *value && new_hash == *hash {
        return false;
    }
    if !dry_run {
        *value = new_value;
        *hash = new_hash;
    }
    true
}

/// Aggregate counters for the per-run summary: rows touched overall, plus a
/// per-column breakdown so an operator can see which rollout the backlog is from.
#[derive(Default)]
struct Totals {
    rows: usize,
    model_descriptor: usize,
    runtime_descriptor: usize,
    benchmark_flags: usize,
    model_flags: usize,
    runtime_flags: usize,
}

impl Totals {
    fn record(&mut self, changes: &RowChanges) {
        if !changes.any() {
            return;
        }
        self.rows += 1;
        self.model_descriptor += usize::from(changes.model_descriptor);
        self.runtime_descriptor += usize::from(changes.runtime_descriptor);
        self.benchmark_flags += usize::from(changes.benchmark_flags);
        self.model_flags += usize::from(changes.model_flags);
        self.runtime_flags += usize::from(changes.runtime_flags);
    }

    fn log(&self, dry_run: bool) {
        // Both stdout and tracing: a CLI run without RUST_LOG=info
        // still shows what happened.
        if self.rows == 0 {
            println!("fix-canonical: no work needed (every row already canonical)");
            tracing::info!(dry_run, "fix-canonical: no work needed");
            return;
        }
        let verb = if dry_run { "would update" } else { "updated" };
        println!(
            "fix-canonical: {verb} {rows} rows \
             (model_descriptor {md}, runtime_descriptor {rd}, benchmark_flags {bf}, \
             model_flags {mf}, runtime_flags {rf})",
            rows = self.rows,
            md = self.model_descriptor,
            rd = self.runtime_descriptor,
            bf = self.benchmark_flags,
            mf = self.model_flags,
            rf = self.runtime_flags,
        );
        tracing::info!(
            rows = self.rows,
            model_descriptor = self.model_descriptor,
            runtime_descriptor = self.runtime_descriptor,
            benchmark_flags = self.benchmark_flags,
            model_flags = self.model_flags,
            runtime_flags = self.runtime_flags,
            dry_run,
            "fix-canonical",
        );
    }
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use crate::fix_canonical::*;
    use crate::parquet_utils::WriterOpts;
    use crate::stores::build_local_fs_stores;
    use crate::warehouse::{self, MetricRow};

    fn test_stores(dir: &std::path::Path) -> anyhow::Result<Stores> {
        let config = crate::config::Config {
            evals_server_url: "http://unused".to_string(),
            storage: crate::config::StorageConfig::local_fs(dir.to_path_buf()),
            auth_storage: crate::config::StorageConfig::local_fs(dir.to_path_buf()),
            ..crate::config::Config::default()
        };
        build_local_fs_stores(&config)
    }

    /// Seed one partition with `rows`, run the command, return the rewritten rows
    /// and the partition path (so mtime assertions can reuse it).
    async fn seed(dir: &std::path::Path, rows: &[MetricRow]) -> anyhow::Result<std::path::PathBuf> {
        let part_dir =
            dir.join("warehouse/results/benchmark_id=test/client_id=ev1_c/month=2026-03");
        std::fs::create_dir_all(&part_dir)?;
        let path = part_dir.join("part-0001.parquet");
        std::fs::write(
            &path,
            warehouse::rows_to_parquet_bytes(WriterOpts::default(), rows)?,
        )?;
        Ok(path)
    }

    /// All three flag columns are canonicalized and hashed identically: a
    /// top-level `{}` collapses to NULL, a nested one survives, and a plain
    /// (non-JSON) string is preserved trimmed rather than mangled.
    #[rstest]
    #[case::sorts_keys(Some(r#"{ "b": 1, "a": 2 }"#), Some(r#"{"a":2,"b":1}"#))]
    #[case::empty_object_is_null(Some("{}"), None)]
    #[case::plain_string_passes_through(Some("--n-gpu-layers 999"), Some("--n-gpu-layers 999"))]
    #[case::nested_empty_object_kept(Some(r#"{"doomloop":{}}"#), Some(r#"{"doomloop":{}}"#))]
    #[case::absent_stays_absent(None, None)]
    #[tokio::test]
    async fn run_canonicalizes_flag_columns(
        #[case] stored: Option<&str>,
        #[case] expected: Option<&str>,
    ) -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let stores = test_stores(dir.path())?;
        let path = seed(
            dir.path(),
            &[MetricRow {
                model_flags: stored.map(str::to_string),
                runtime_flags: stored.map(str::to_string),
                benchmark_flags: stored.map(str::to_string),
                ..Default::default()
            }],
        )
        .await?;

        run(&stores, false, &[]).await?;

        let after = warehouse::rows_from_parquet_bytes(&std::fs::read(&path)?)?;
        // The hash is present exactly when the value is, and is a hash of the
        // canonical value rather than of what was stored.
        let expected_hash = expected.map(sha256_hex);
        [
            (
                "model_flags",
                &after[0].model_flags,
                &after[0].model_flags_sha256,
            ),
            (
                "runtime_flags",
                &after[0].runtime_flags,
                &after[0].runtime_flags_sha256,
            ),
            (
                "benchmark_flags",
                &after[0].benchmark_flags,
                &after[0].benchmark_flags_sha256,
            ),
        ]
        .iter()
        .for_each(|(name, value, hash)| {
            assert_eq!(value.as_deref(), expected, "{name}");
            assert_eq!(**hash, expected_hash, "{name}_sha256");
        });
        Ok(())
    }

    /// Two rows whose flags differ only in key order and whitespace must end up
    /// on one `sha256` — the whole point of the rewrite.
    #[tokio::test]
    async fn run_collapses_formatting_variants_to_one_hash() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let stores = test_stores(dir.path())?;
        let path = seed(
            dir.path(),
            &[
                MetricRow {
                    result_id: "r1".into(),
                    runtime_flags: Some(r#"{"threads":8,"gpu":99}"#.into()),
                    ..Default::default()
                },
                MetricRow {
                    result_id: "r2".into(),
                    runtime_flags: Some(r#"{ "gpu": 99, "threads": 8 }"#.into()),
                    ..Default::default()
                },
            ],
        )
        .await?;

        run(&stores, false, &[]).await?;

        let after = warehouse::rows_from_parquet_bytes(&std::fs::read(&path)?)?;
        assert_eq!(after[0].runtime_flags, after[1].runtime_flags);
        assert_eq!(after[0].runtime_flags_sha256, after[1].runtime_flags_sha256);
        assert!(after[0].runtime_flags_sha256.is_some());
        Ok(())
    }

    /// Descriptors are canonicalized and their hashes recomputed from the
    /// canonical form, not from what was stored.
    #[tokio::test]
    async fn run_recomputes_descriptor_hashes() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let stores = test_stores(dir.path())?;
        let path = seed(
            dir.path(),
            &[MetricRow {
                model_descriptor: Some(r#"{ "type": "mlx", "org": "LiquidAI" }"#.into()),
                model_descriptor_sha256: Some("stale".into()),
                ..Default::default()
            }],
        )
        .await?;

        run(&stores, false, &[]).await?;

        let after = warehouse::rows_from_parquet_bytes(&std::fs::read(&path)?)?;
        let canonical = r#"{"org":"LiquidAI","type":"mlx"}"#;
        assert_eq!(after[0].model_descriptor.as_deref(), Some(canonical));
        assert_eq!(
            after[0].model_descriptor_sha256,
            Some(sha256_hex(canonical))
        );
        Ok(())
    }

    /// `--column` restricts the rewrite: an out-of-scope column keeps its
    /// non-canonical value.
    #[tokio::test]
    async fn run_respects_column_filter() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let stores = test_stores(dir.path())?;
        let messy = r#"{ "b": 1, "a": 2 }"#;
        let path = seed(
            dir.path(),
            &[MetricRow {
                model_flags: Some(messy.into()),
                runtime_flags: Some(messy.into()),
                ..Default::default()
            }],
        )
        .await?;

        run(&stores, false, &["model_flags".to_string()]).await?;

        let after = warehouse::rows_from_parquet_bytes(&std::fs::read(&path)?)?;
        assert_eq!(after[0].model_flags.as_deref(), Some(r#"{"a":2,"b":1}"#));
        assert_eq!(after[0].runtime_flags.as_deref(), Some(messy));
        assert!(after[0].runtime_flags_sha256.is_none());
        Ok(())
    }

    #[tokio::test]
    async fn run_errors_on_unknown_column() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let stores = test_stores(dir.path())?;
        match run(&stores, false, &["nope".to_string()]).await {
            Ok(()) => anyhow::bail!("expected an error for an unknown column"),
            Err(err) => assert!(err.to_string().contains("unknown column"), "got: {err}"),
        }
        Ok(())
    }

    /// The command is built to be re-run, so a second pass over its own output
    /// must be a no-op — not another rewrite of the same rows.
    #[tokio::test]
    async fn run_is_idempotent() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let stores = test_stores(dir.path())?;
        let path = seed(
            dir.path(),
            &[MetricRow {
                model_flags: Some(r#"{ "b": 1, "a": 2 }"#.into()),
                runtime_flags: Some("  --n-gpu-layers 999  ".into()),
                benchmark_flags: Some(r#"{ "readiness": { } }"#.into()),
                model_descriptor: Some(r#"{ "type": "mlx", "org": "LiquidAI" }"#.into()),
                ..Default::default()
            }],
        )
        .await?;

        run(&stores, false, &[]).await?;
        let after_first = warehouse::rows_from_parquet_bytes(&std::fs::read(&path)?)?;
        let mtime_after_first = std::fs::metadata(&path)?.modified()?;

        std::thread::sleep(std::time::Duration::from_millis(10));
        run(&stores, false, &[]).await?;

        assert_eq!(
            mtime_after_first,
            std::fs::metadata(&path)?.modified()?,
            "second run should find nothing to do and leave the file alone"
        );
        let after_second = warehouse::rows_from_parquet_bytes(&std::fs::read(&path)?)?;
        assert_eq!(after_first[0].model_flags, after_second[0].model_flags);
        assert_eq!(after_first[0].runtime_flags, after_second[0].runtime_flags);
        assert_eq!(
            after_first[0].benchmark_flags,
            after_second[0].benchmark_flags
        );
        assert_eq!(
            after_first[0].model_descriptor,
            after_second[0].model_descriptor
        );
        Ok(())
    }

    /// A file is rewritten wholesale once any row is dirty, so the clean rows
    /// sharing it must survive untouched — losing them would be silent.
    #[tokio::test]
    async fn run_preserves_clean_rows_sharing_a_dirty_file() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let stores = test_stores(dir.path())?;
        let canonical = r#"{"a":2,"b":1}"#;
        let path = seed(
            dir.path(),
            &[
                MetricRow {
                    result_id: "clean".into(),
                    model_flags: Some(canonical.into()),
                    model_flags_sha256: Some(sha256_hex(canonical)),
                    value: 1.5,
                    ..Default::default()
                },
                MetricRow {
                    result_id: "dirty".into(),
                    model_flags: Some(r#"{ "b": 1, "a": 2 }"#.into()),
                    value: 2.5,
                    ..Default::default()
                },
            ],
        )
        .await?;

        run(&stores, false, &[]).await?;

        let after = warehouse::rows_from_parquet_bytes(&std::fs::read(&path)?)?;
        assert_eq!(after.len(), 2, "no row may be dropped by the rewrite");
        let clean = after
            .iter()
            .find(|r| r.result_id == "clean")
            .ok_or_else(|| anyhow::anyhow!("clean row was dropped"))?;
        assert_eq!(clean.model_flags.as_deref(), Some(canonical));
        assert_eq!(clean.model_flags_sha256, Some(sha256_hex(canonical)));
        // A field the command never touches must round-trip through the rewrite.
        assert_eq!(clean.value, 1.5);
        Ok(())
    }

    /// Already-canonical rows leave the file untouched (mtime preserved), and a
    /// dry run never rewrites even when there is work to do.
    #[rstest]
    #[case::already_canonical(r#"{"a":2,"b":1}"#, false)]
    #[case::dry_run(r#"{ "b": 1, "a": 2 }"#, true)]
    #[tokio::test]
    async fn run_does_not_rewrite(
        #[case] stored: &str,
        #[case] dry_run: bool,
    ) -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let stores = test_stores(dir.path())?;
        let canonical_hash = sha256_hex(r#"{"a":2,"b":1}"#);
        let path = seed(
            dir.path(),
            &[MetricRow {
                model_flags: Some(stored.into()),
                // Aligned only in the already-canonical case; in the dry-run
                // case the row is genuinely dirty and must still not be written.
                model_flags_sha256: (!dry_run).then_some(canonical_hash),
                ..Default::default()
            }],
        )
        .await?;
        let mtime_before = std::fs::metadata(&path)?.modified()?;

        std::thread::sleep(std::time::Duration::from_millis(10));
        run(&stores, dry_run, &[]).await?;

        assert_eq!(
            mtime_before,
            std::fs::metadata(&path)?.modified()?,
            "file should not have been rewritten"
        );
        let after = warehouse::rows_from_parquet_bytes(&std::fs::read(&path)?)?;
        assert_eq!(after[0].model_flags.as_deref(), Some(stored));
        Ok(())
    }
}
