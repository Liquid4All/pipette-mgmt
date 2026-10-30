//! `fix-canonical`: re-canonicalize the warehouse's opaque JSON columns and
//! recompute their `_sha256` ids, so older rows group with new ones. It rewrites
//! Parquet in place, so the CLI holds the storage mutate lock for the whole run.

use pipette_store::Stores;

use pipette_core::canonical_json::{canonicalize_flags, canonicalize_str, sha256_hex};
use pipette_core::warehouse::MetricRow;

use crate::vault_check::descriptor_sha256;

/// The opaque columns this command owns, as CLI-facing names.
pub const COLUMNS: [&str; 8] = [
    "model_descriptor",
    "runtime_descriptor",
    "benchmark_flags",
    "model_flags",
    "runtime_flags",
    "requested_model_flags",
    "requested_runtime_flags",
    "requested_benchmark_flags",
];

/// How each column is reached and normalized, in [`COLUMNS`] order. The
/// accessor returns `(value, hash)` as one borrow, so a value cannot be paired
/// with another column's hash.
type Accessor = fn(&mut MetricRow) -> (&mut Option<String>, &mut Option<String>);
type Canonicalizer = fn(Option<&str>) -> Option<String>;
/// A canonical value's content id.
type Hasher = fn(&str) -> String;

const ACCESSORS: [(Accessor, Canonicalizer, Hasher); COLUMNS.len()] = [
    (
        |r| (&mut r.model_descriptor, &mut r.model_descriptor_sha256),
        canonicalize_descriptor,
        descriptor_sha256,
    ),
    (
        |r| (&mut r.runtime_descriptor, &mut r.runtime_descriptor_sha256),
        canonicalize_descriptor,
        descriptor_sha256,
    ),
    (
        |r| (&mut r.benchmark_flags, &mut r.benchmark_flags_sha256),
        canonicalize_flags,
        sha256_hex,
    ),
    (
        |r| (&mut r.model_flags, &mut r.model_flags_sha256),
        canonicalize_flags,
        sha256_hex,
    ),
    (
        |r| (&mut r.runtime_flags, &mut r.runtime_flags_sha256),
        canonicalize_flags,
        sha256_hex,
    ),
    (
        |r| {
            (
                &mut r.requested_model_flags,
                &mut r.requested_model_flags_sha256,
            )
        },
        canonicalize_flags,
        sha256_hex,
    ),
    (
        |r| {
            (
                &mut r.requested_runtime_flags,
                &mut r.requested_runtime_flags_sha256,
            )
        },
        canonicalize_flags,
        sha256_hex,
    ),
    (
        |r| {
            (
                &mut r.requested_benchmark_flags,
                &mut r.requested_benchmark_flags_sha256,
            )
        },
        canonicalize_flags,
        sha256_hex,
    ),
];

/// No empty-object rule, unlike the flag columns: a client never reports an
/// empty descriptor.
fn canonicalize_descriptor(value: Option<&str>) -> Option<String> {
    value.map(canonicalize_str)
}

/// `only_columns`, when non-empty, restricts the rewrite to those [`COLUMNS`].
/// An unknown name errors: a typo would otherwise report "nothing to do".
pub async fn run(stores: &Stores, dry_run: bool, only_columns: &[String]) -> anyhow::Result<()> {
    let selected = parse_selection(only_columns)?;

    let mut totals = Totals::default();
    stores
        .warehouse
        .for_each_metric_row(&mut |row| {
            let changed = normalize_row(row, &selected, dry_run);
            totals.record(&changed);
            !dry_run && changed.iter().any(|c| *c)
        })
        .await?;
    totals.log(dry_run);
    Ok(())
}

/// Which columns this run may touch, in [`COLUMNS`] order; empty means all.
fn parse_selection(only: &[String]) -> anyhow::Result<[bool; COLUMNS.len()]> {
    if only.is_empty() {
        return Ok([true; COLUMNS.len()]);
    }
    if let Some(unknown) = only.iter().find(|n| !COLUMNS.contains(&n.as_str())) {
        anyhow::bail!(
            "unknown column {unknown:?}; expected one of: {}",
            COLUMNS.join(", ")
        );
    }
    tracing::info!(columns = ?only, "restricting fix-canonical to selected columns");
    Ok(COLUMNS.map(|name| only.iter().any(|c| c == name)))
}

/// Returns which columns differed, in [`COLUMNS`] order. A hash is recomputed
/// whenever its column is in scope, which backfills rows that predate the hash.
fn normalize_row(
    row: &mut MetricRow,
    selected: &[bool; COLUMNS.len()],
    dry_run: bool,
) -> [bool; COLUMNS.len()] {
    let mut changed = [false; COLUMNS.len()];
    ACCESSORS
        .iter()
        .zip(selected)
        .zip(&mut changed)
        .filter(|((_, in_scope), _)| **in_scope)
        .for_each(|(((accessor, canonicalize, hasher), _), out)| {
            let (value, hash) = accessor(row);
            *out = normalize_column(value, hash, *canonicalize, *hasher, dry_run);
        });
    changed
}

/// Bring one `(value, hash)` column pair to canonical form. Returns whether
/// either differed from what was stored; writes only when `dry_run` is false.
fn normalize_column(
    value: &mut Option<String>,
    hash: &mut Option<String>,
    canonicalize: Canonicalizer,
    hasher: Hasher,
    dry_run: bool,
) -> bool {
    let new_value = canonicalize(value.as_deref());
    let new_hash = new_value.as_deref().map(hasher);
    if new_value == *value && new_hash == *hash {
        return false;
    }
    if !dry_run {
        *value = new_value;
        *hash = new_hash;
    }
    true
}

/// Per-run counters, broken down per column so an operator can see which
/// rollout the backlog is from.
#[derive(Default)]
struct Totals {
    rows: usize,
    per_column: [usize; COLUMNS.len()],
}

impl Totals {
    fn record(&mut self, changes: &[bool; COLUMNS.len()]) {
        if !changes.iter().any(|c| *c) {
            return;
        }
        self.rows += 1;
        self.per_column
            .iter_mut()
            .zip(changes)
            .for_each(|(count, changed)| *count += usize::from(*changed));
    }

    fn log(&self, dry_run: bool) {
        // Both stdout and tracing: stdout still shows what happened when
        // RUST_LOG filters out info.
        if self.rows == 0 {
            println!("fix-canonical: no work needed (every row already canonical)");
            tracing::info!(dry_run, "fix-canonical: no work needed");
            return;
        }
        let verb = if dry_run { "would update" } else { "updated" };
        let breakdown = COLUMNS
            .iter()
            .zip(self.per_column)
            .map(|(name, count)| format!("{name} {count}"))
            .collect::<Vec<_>>()
            .join(", ");
        println!(
            "fix-canonical: {verb} {rows} rows ({breakdown})",
            rows = self.rows
        );
        tracing::info!(
            rows = self.rows,
            breakdown = %breakdown,
            dry_run,
            "fix-canonical",
        );
    }
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use pipette_core::parquet_utils::WriterOpts;
    use pipette_core::warehouse::{self, MetricRow};
    use pipette_store_fs::build_local_fs_stores;

    use crate::fix_canonical::*;

    fn test_stores(dir: &std::path::Path) -> anyhow::Result<Stores> {
        let config = pipette_core::config::Config {
            evals_server_url: "http://unused".to_string(),
            storage: pipette_core::config::StorageConfig::local_fs(dir.to_path_buf()),
            auth_storage: pipette_core::config::StorageConfig::local_fs(dir.to_path_buf()),
            ..pipette_core::config::Config::default()
        };
        build_local_fs_stores(&config)
    }

    /// Seed one partition with `rows`; returns its path for mtime assertions.
    fn seed(dir: &std::path::Path, rows: &[MetricRow]) -> anyhow::Result<std::path::PathBuf> {
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

    /// All six flag columns are canonicalized and hashed identically.
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
                requested_model_flags: stored.map(str::to_string),
                requested_runtime_flags: stored.map(str::to_string),
                requested_benchmark_flags: stored.map(str::to_string),
                ..Default::default()
            }],
        )?;

        run(&stores, false, &[]).await?;

        let after = warehouse::rows_from_parquet_bytes(&std::fs::read(&path)?)?;
        // The hash is of the canonical value, not the stored one. Every hash column
        // starts NULL here, so this also pins the backfill.
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
            (
                "requested_model_flags",
                &after[0].requested_model_flags,
                &after[0].requested_model_flags_sha256,
            ),
            (
                "requested_runtime_flags",
                &after[0].requested_runtime_flags,
                &after[0].requested_runtime_flags_sha256,
            ),
            (
                "requested_benchmark_flags",
                &after[0].requested_benchmark_flags,
                &after[0].requested_benchmark_flags_sha256,
            ),
        ]
        .iter()
        .for_each(|(name, value, hash)| {
            assert_eq!(value.as_deref(), expected, "{name}");
            assert_eq!(**hash, expected_hash, "{name}_sha256");
        });
        Ok(())
    }

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
        )?;

        run(&stores, false, &[]).await?;

        let after = warehouse::rows_from_parquet_bytes(&std::fs::read(&path)?)?;
        assert_eq!(after[0].runtime_flags, after[1].runtime_flags);
        assert_eq!(after[0].runtime_flags_sha256, after[1].runtime_flags_sha256);
        assert!(after[0].runtime_flags_sha256.is_some());
        Ok(())
    }

    /// Hashes are recomputed from the canonical form, not from what was stored.
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
        )?;

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
        )?;

        run(&stores, false, &["model_flags".to_string()]).await?;

        let after = warehouse::rows_from_parquet_bytes(&std::fs::read(&path)?)?;
        assert_eq!(after[0].model_flags.as_deref(), Some(r#"{"a":2,"b":1}"#));
        assert_eq!(after[0].runtime_flags.as_deref(), Some(messy));
        assert!(after[0].runtime_flags_sha256.is_none());
        Ok(())
    }

    /// `COLUMNS` and `ACCESSORS` are parallel lists, so the reads below name each
    /// field by hand rather than inherit a misalignment through `ACCESSORS`.
    #[rstest]
    #[case::model_descriptor("model_descriptor")]
    #[case::runtime_descriptor("runtime_descriptor")]
    #[case::benchmark_flags("benchmark_flags")]
    #[case::model_flags("model_flags")]
    #[case::runtime_flags("runtime_flags")]
    #[case::requested_model_flags("requested_model_flags")]
    #[case::requested_runtime_flags("requested_runtime_flags")]
    #[case::requested_benchmark_flags("requested_benchmark_flags")]
    #[tokio::test]
    async fn run_touches_only_the_named_column(#[case] column: &str) -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let stores = test_stores(dir.path())?;
        let messy = r#"{ "b": 1, "a": 2 }"#;
        let canonical = r#"{"a":2,"b":1}"#;
        let path = seed(
            dir.path(),
            &[MetricRow {
                model_descriptor: Some(messy.into()),
                runtime_descriptor: Some(messy.into()),
                benchmark_flags: Some(messy.into()),
                model_flags: Some(messy.into()),
                runtime_flags: Some(messy.into()),
                requested_model_flags: Some(messy.into()),
                requested_runtime_flags: Some(messy.into()),
                requested_benchmark_flags: Some(messy.into()),
                ..Default::default()
            }],
        )?;

        run(&stores, false, &[column.to_string()]).await?;

        let after = warehouse::rows_from_parquet_bytes(&std::fs::read(&path)?)?;
        let row = &after[0];
        let stored: [(&str, Option<&str>, Option<&str>); 8] = [
            (
                "model_descriptor",
                row.model_descriptor.as_deref(),
                row.model_descriptor_sha256.as_deref(),
            ),
            (
                "runtime_descriptor",
                row.runtime_descriptor.as_deref(),
                row.runtime_descriptor_sha256.as_deref(),
            ),
            (
                "benchmark_flags",
                row.benchmark_flags.as_deref(),
                row.benchmark_flags_sha256.as_deref(),
            ),
            (
                "model_flags",
                row.model_flags.as_deref(),
                row.model_flags_sha256.as_deref(),
            ),
            (
                "runtime_flags",
                row.runtime_flags.as_deref(),
                row.runtime_flags_sha256.as_deref(),
            ),
            (
                "requested_model_flags",
                row.requested_model_flags.as_deref(),
                row.requested_model_flags_sha256.as_deref(),
            ),
            (
                "requested_runtime_flags",
                row.requested_runtime_flags.as_deref(),
                row.requested_runtime_flags_sha256.as_deref(),
            ),
            (
                "requested_benchmark_flags",
                row.requested_benchmark_flags.as_deref(),
                row.requested_benchmark_flags_sha256.as_deref(),
            ),
        ];
        let expected_hash = sha256_hex(canonical);
        stored.iter().for_each(|(name, value, hash)| {
            if *name == column {
                assert_eq!(*value, Some(canonical), "{name} should be canonicalized");
                assert_eq!(*hash, Some(expected_hash.as_str()), "{name}_sha256");
            } else {
                assert_eq!(*value, Some(messy), "{name} should be untouched");
                assert_eq!(*hash, None, "{name}_sha256 should stay absent");
            }
        });
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
        )?;

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
                    value: Some(1.5),
                    ..Default::default()
                },
                MetricRow {
                    result_id: "dirty".into(),
                    model_flags: Some(r#"{ "b": 1, "a": 2 }"#.into()),
                    value: Some(2.5),
                    ..Default::default()
                },
            ],
        )?;

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
        assert_eq!(clean.value, Some(1.5));
        Ok(())
    }

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
        )?;
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
