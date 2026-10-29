//! Read-only reports on coverage of the model parameter table. A gap is otherwise invisible:
//! the scorer falls back to the client's parameter counts and says nothing.

use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;

use pipette_core::warehouse::MetricRow;
use pipette_store::Stores;

use crate::fix_model_param;
use crate::model_params::{self, ModelEntry, ModelParamTable};

/// The `models unresolved --json` document. The table size tells "nothing
/// resolved" against an empty table from a real gap.
#[derive(Debug, PartialEq, Eq, Serialize)]
pub struct UnresolvedReport<'a> {
    /// The number of rows in the model parameter table. `catalog_entries` is
    /// the `--json` key that `docs/cli.md` documents, so it keeps that name.
    pub catalog_entries: usize,
    pub report: &'a UnresolvedRows,
}

/// Every model the table cannot resolve, with how many rows wait on each.
///
/// `Serialize` for the cdk job that proposes table entries from it.
#[derive(Debug, Default, PartialEq, Eq, Serialize)]
pub struct UnresolvedRows {
    /// Keyed by the normalized name the descriptor gives the weights, so every
    /// spelling of a model lands on one line.
    pub named: BTreeMap<String, ModelMiss>,
    /// Rows whose `model_descriptor` this server could not parse: a partner
    /// format, or a client newer than this build. Teach it the shape.
    pub unparsed: ModelMiss,
    /// Rows with no `model_descriptor`, which only the client can fix. Counted
    /// apart so they do not drown the gaps above.
    pub no_descriptor: ModelMiss,
    /// Rows counted, resolved or not: the report's denominator.
    pub scanned: usize,
}

/// What one unresolved model looked like in the rows that named it.
#[derive(Debug, Default, PartialEq, Eq, Serialize)]
pub struct ModelMiss {
    pub rows: usize,
    /// A `model_descriptor` off one missed row, to give the operator an org
    /// and repo to look up. Any one will do; they differ only by build.
    pub sample_descriptor: Option<String>,
}

impl ModelMiss {
    fn record(&mut self, rows: usize, descriptor: Option<&str>) {
        self.rows += rows;
        if self.sample_descriptor.is_none() {
            self.sample_descriptor = descriptor.map(str::to_owned);
        }
    }
}

impl UnresolvedRows {
    /// Whether every row resolved. An empty warehouse is vacuously clean.
    pub fn is_clean(&self) -> bool {
        self.named.is_empty() && self.unparsed.rows == 0 && self.no_descriptor.rows == 0
    }

    /// Rows that did not resolve, across both buckets.
    pub fn unresolved_rows(&self) -> usize {
        self.named.values().map(|m| m.rows).sum::<usize>()
            + self.unparsed.rows
            + self.no_descriptor.rows
    }

    /// Count `rows` rows whose model the table resolved.
    pub fn record_resolved(&mut self, rows: usize) {
        self.scanned += rows;
    }

    /// Count `rows` rows naming one unresolved model (`key` absent if unparsed).
    /// A count, not a call: the scorer resolves once per submission, not per row.
    pub fn record_miss(&mut self, rows: usize, key: Option<&str>, descriptor: Option<&str>) {
        if rows == 0 {
            return;
        }
        self.scanned += rows;
        match (key, descriptor) {
            (Some(key), _) => self
                .named
                .entry(key.to_owned())
                .or_default()
                .record(rows, descriptor),
            (None, Some(_)) => self.unparsed.record(rows, descriptor),
            (None, None) => self.no_descriptor.record(rows, None),
        }
    }

    fn record_row(&mut self, table: &ModelParamTable, row: &MetricRow) {
        let descriptor = row.model_descriptor.as_deref();
        match fix_model_param::resolve(table, row) {
            Some(_) => self.record_resolved(1),
            None => self.record_miss(
                1,
                descriptor.and_then(model_params::descriptor_key).as_deref(),
                descriptor,
            ),
        }
    }
}

/// Scan the warehouse for rows the table cannot resolve, through
/// [`fix_model_param::resolve`]. No mutate lock: the visitor marks no row dirty.
pub async fn unresolved_in_warehouse(
    stores: &Stores,
    table: &ModelParamTable,
) -> anyhow::Result<UnresolvedRows> {
    let mut report = UnresolvedRows::default();
    stores
        .warehouse
        .for_each_metric_row(&mut |row| {
            report.record_row(table, row);
            false
        })
        .await?;
    Ok(report)
}

/// One name checked against the table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NameResolution {
    /// The name as given, so a report can echo what the caller passed.
    pub input: String,
    /// The key `input` normalizes to. Reported on a miss too: it is the key
    /// of the row to add, and it shows when two spellings are one model.
    pub key: String,
    pub entry: Option<ModelEntry>,
}

impl NameResolution {
    pub fn is_resolved(&self) -> bool {
        self.entry.is_some()
    }
}

/// Resolve model names against a table, in input order with duplicates kept.
/// Name-only: a plan has no `model_descriptor` yet.
pub fn resolve_names<I, S>(table: &ModelParamTable, names: I) -> Vec<NameResolution>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    names
        .into_iter()
        .map(|name| {
            let input = name.as_ref();
            NameResolution {
                input: input.to_owned(),
                key: model_params::normalize(input),
                entry: table.lookup(input),
            }
        })
        .collect()
}

/// The distinct model keys a [`resolve_names`] run missed: several spellings
/// fold to one entry to add.
pub fn missing_keys(resolved: &[NameResolution]) -> BTreeSet<&str> {
    resolved
        .iter()
        .filter(|n| !n.is_resolved())
        .map(|n| n.key.as_str())
        .collect()
}

#[cfg(test)]
mod tests {
    use anyhow::Context;
    use rstest::rstest;

    use pipette_core::parquet_utils::WriterOpts;
    use pipette_core::warehouse;
    use pipette_plan_types::{BenchmarkId, ClientId};
    use pipette_store_fs::build_local_fs_stores;

    use crate::model_audit::*;

    const TABLE: &str = r#"
"LFM2-700M" = 742
"granite-4.2-3b" = 3660
"#;

    fn table() -> anyhow::Result<ModelParamTable> {
        ModelParamTable::from_toml(TABLE).context("the test table must parse")
    }

    /// A row valid enough to survive a Parquet round trip.
    fn warehouse_row(result_id: &str, repo: &str) -> anyhow::Result<MetricRow> {
        Ok(MetricRow {
            result_id: result_id.to_string(),
            benchmark_id: BenchmarkId::try_new("bench1")?,
            metric: "ttft".to_string(),
            client_id: ClientId::try_new("c1")?,
            model_descriptor: Some(gguf(repo, &format!("{repo}-Q4_K_M.gguf"))),
            value: Some(1.0),
            unit: "ms".to_string(),
            submitted_at: 1_000_000,
            scored_at: 2_000_000,
            ..MetricRow::default()
        })
    }

    fn row(model_name: Option<&str>, model_descriptor: Option<&str>) -> MetricRow {
        MetricRow {
            model_name: model_name.map(str::to_owned),
            model_descriptor: model_descriptor.map(str::to_owned),
            ..MetricRow::default()
        }
    }

    /// A gguf descriptor naming `repo` and `file`, the shape a client sends.
    fn gguf(repo: &str, file: &str) -> String {
        format!(
            r#"{{"type":"gguf_text","source":"huggingface","org":"someorg","repo_name":"{repo}","path":"{file}"}}"#
        )
    }

    #[test]
    fn misses_collapse_to_one_normalized_key() -> anyhow::Result<()> {
        let table = table()?;
        let mut report = UnresolvedRows::default();
        [
            gguf("K2-Horizon-3.7B-GGUF", "K2-Horizon-3.7B-Q4_K_M.gguf"),
            gguf("K2-Horizon-3.7B-GGUF", "K2-Horizon-3.7B-Q8_0.gguf"),
            gguf("K2-Horizon-3.7B", "K2-Horizon-3.7B-F16.gguf"),
        ]
        .iter()
        .for_each(|d| report.record_row(&table, &row(None, Some(d))));

        assert_eq!(report.scanned, 3);
        assert_eq!(report.unresolved_rows(), 3);
        assert_eq!(
            report.named.keys().collect::<Vec<_>>(),
            vec!["k2-horizon-3.7b"]
        );
        Ok(())
    }

    /// Only the descriptor resolves a row; `model_name` is client-set.
    #[rstest]
    #[case::descriptor_resolves(None, Some(gguf("LFM2-700M-GGUF", "LFM2-700M-Q4_K_M.gguf")), true)]
    #[case::descriptor_wins_over_a_misleading_name(
        Some("nobody-catalogued-this-alias"),
        Some(gguf("LFM2-700M-GGUF", "LFM2-700M-Q4_K_M.gguf")),
        true
    )]
    #[case::a_catalogued_name_alone_does_not(Some("LiquidAI/LFM2-700M-GGUF"), None, false)]
    fn only_the_descriptor_resolves_a_row(
        #[case] model_name: Option<&str>,
        #[case] model_descriptor: Option<String>,
        #[case] expected_clean: bool,
    ) -> anyhow::Result<()> {
        let table = table()?;
        let mut report = UnresolvedRows::default();
        report.record_row(&table, &row(model_name, model_descriptor.as_deref()));

        assert_eq!(report.is_clean(), expected_clean);
        assert_eq!(report.scanned, 1);
        Ok(())
    }

    /// An unparseable descriptor counts in its own bucket, with a sample so the
    /// shape can be taught to the server.
    #[test]
    fn unparseable_rows_carry_a_sample_descriptor() -> anyhow::Result<()> {
        let table = table()?;
        let mut report = UnresolvedRows::default();
        report.record_row(&table, &row(None, Some(r#"{"partner":"Unknown"}"#)));
        report.record_row(&table, &row(None, Some(r#"{"partner":"Another"}"#)));

        assert!(report.named.is_empty());
        assert_eq!(report.unparsed.rows, 2);
        assert_eq!(
            report.unparsed.sample_descriptor.as_deref(),
            Some(r#"{"partner":"Unknown"}"#)
        );
        Ok(())
    }

    /// Each failure sends the operator somewhere different: add a table entry,
    /// teach a descriptor shape, or nothing.
    #[test]
    fn the_three_ways_a_row_fails_are_counted_apart() -> anyhow::Result<()> {
        let table = table()?;
        let mut report = UnresolvedRows::default();
        let uncatalogued = gguf("K2-Horizon-3.7B-GGUF", "K2-Horizon-3.7B-Q4_K_M.gguf");
        report.record_row(&table, &row(None, Some(&uncatalogued)));
        report.record_row(&table, &row(None, Some(r#"{"partner":"own-format"}"#)));
        report.record_row(&table, &row(Some("legacy-name-only"), None));

        assert_eq!(report.named.len(), 1);
        assert_eq!(report.unparsed.rows, 1);
        assert_eq!(report.no_descriptor.rows, 1);
        assert_eq!(report.unresolved_rows(), 3);
        Ok(())
    }

    /// Apple Foundation names no repo or file, so it resolves under the wire tag.
    #[test]
    fn apple_foundation_resolves_under_its_wire_tag() -> anyhow::Result<()> {
        let table = ModelParamTable::from_toml(r#""apple_foundation_text" = 3000"#)?;
        let mut report = UnresolvedRows::default();
        report.record_row(
            &table,
            &row(None, Some(r#"{"type":"apple_foundation_text"}"#)),
        );

        assert!(report.is_clean());
        Ok(())
    }

    /// Pinned because a row's `model_name` still looks authoritative.
    #[test]
    fn a_catalogued_name_does_not_rescue_an_uncatalogued_descriptor() -> anyhow::Result<()> {
        let table = table()?;
        let mut report = UnresolvedRows::default();
        let descriptor = gguf("K2-Horizon-3.7B-GGUF", "K2-Horizon-3.7B-Q4_K_M.gguf");
        report.record_row(&table, &row(Some("LiquidAI/LFM2-700M"), Some(&descriptor)));

        assert_eq!(report.unresolved_rows(), 1);
        assert_eq!(
            report.named.keys().collect::<Vec<_>>(),
            vec!["k2-horizon-3.7b"]
        );
        Ok(())
    }

    #[rstest]
    #[case::bare("LFM2-700M", true, "lfm2-700m")]
    #[case::with_org("LiquidAI/LFM2-700M", true, "lfm2-700m")]
    #[case::gguf_repo("LiquidAI/LFM2-700M-GGUF", true, "lfm2-700m")]
    #[case::quant_file("LFM2-700M-Q4_K_M.gguf", true, "lfm2-700m")]
    #[case::unsloth_dynamic("unsloth/LFM2-700M-UD-Q4_K_XL.gguf", true, "lfm2-700m")]
    #[case::mradermacher_imatrix("LFM2-700M.i1-Q4_K_M.gguf", true, "lfm2-700m")]
    #[case::unknown("IFM/K2-Horizon-3.7B-GGUF", false, "k2-horizon-3.7b")]
    fn names_resolve_through_normalization(
        #[case] name: &str,
        #[case] expected: bool,
        #[case] key: &str,
    ) -> anyhow::Result<()> {
        let table = table()?;
        let resolved = resolve_names(&table, [name]);

        let first = resolved.first().context("one name in, one result out")?;
        assert_eq!(first.is_resolved(), expected);
        assert_eq!(first.input, name);
        assert_eq!(first.key, key);
        Ok(())
    }

    /// The partition is read-only, so an attempted rewrite fails the test.
    #[tokio::test]
    async fn scanning_the_warehouse_rewrites_nothing() -> anyhow::Result<()> {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir()?;
        let config = pipette_core::config::Config {
            storage: pipette_core::config::StorageConfig::local_fs(dir.path().to_path_buf()),
            auth_storage: pipette_core::config::StorageConfig::local_fs(dir.path().to_path_buf()),
            ..pipette_core::config::Config::default()
        };
        let stores = build_local_fs_stores(&config)?;

        let partition = dir
            .path()
            .join("warehouse/results/benchmark_id=bench1/client_id=c1/month=2026-03");
        std::fs::create_dir_all(&partition)?;
        let rows = [
            warehouse_row("known", "LFM2-700M-GGUF")?,
            warehouse_row("unknown_a", "K2-Horizon-3.7B-GGUF")?,
            warehouse_row("unknown_b", "K2-Horizon-3.7B")?,
        ];
        let part = partition.join("part-0001.parquet");
        std::fs::write(
            &part,
            warehouse::rows_to_parquet_bytes(WriterOpts::default(), &rows)?,
        )?;

        let original = std::fs::metadata(&partition)?.permissions();
        std::fs::set_permissions(&partition, std::fs::Permissions::from_mode(0o555))?;
        let report = unresolved_in_warehouse(&stores, &table()?).await;
        std::fs::set_permissions(&partition, original)?;
        let report = report?;

        assert_eq!(report.scanned, 3);
        assert_eq!(report.unresolved_rows(), 2);
        assert_eq!(
            report.named.keys().collect::<Vec<_>>(),
            vec!["k2-horizon-3.7b"],
            "both spellings are one model to add"
        );
        Ok(())
    }

    /// Three spellings of two uncatalogued models are two entries.
    #[test]
    fn missing_keys_are_distinct_entries_to_add() -> anyhow::Result<()> {
        let table = table()?;
        let resolved = resolve_names(
            &table,
            [
                "IFM/K2-Horizon-3.7B-GGUF",
                "K2-Horizon-3.7B-Q4_K_M.gguf",
                "openbmb/MiniCPM5-2B",
                "LiquidAI/LFM2-700M",
            ],
        );

        assert_eq!(
            missing_keys(&resolved).into_iter().collect::<Vec<_>>(),
            vec!["k2-horizon-3.7b", "minicpm5-2b"]
        );
        Ok(())
    }
}
