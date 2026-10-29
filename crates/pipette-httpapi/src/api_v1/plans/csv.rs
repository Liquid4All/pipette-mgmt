//! `GET /api/v1/plans/{plan_id}/results.csv`: a plan's result rows as a CSV file.

use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, SecondsFormat, Utc};
use serde_json::Value;

use pipette_core::warehouse::SYSTEM_CLIENT_ID;
use pipette_pipeline::plan_results::{DEVICE_COLUMNS, ResultRow};
use pipette_plan_types::{DescriptorDisplay, JobId};

type CsvCell = fn(&ResultRow) -> String;

/// The columns [`results_csv`] writes first, in [`ResultRow`]'s field order.
const CSV_COLUMNS: &[(&str, CsvCell)] = &[
    ("job_id", |row| text(row.job_id.as_str())),
    ("benchmark_id", |row| text(&row.benchmark_id)),
    ("benchmark_type", |row| text(&row.benchmark_type)),
    ("metric", |row| text(&row.metric)),
    ("value", |row| number(row.value)),
    ("value_stddev", |row| number(row.value_stddev)),
    ("unit", |row| text(&row.unit)),
    ("failed", |row| {
        row.failed
            .map(|failed| failed.to_string())
            .unwrap_or_default()
    }),
    ("failed_reason", |row| {
        optional_text(row.failed_reason.as_deref())
    }),
    // The JSON columns as stored: canonical JSON, or the client's plain text.
    ("model_descriptor", |row| {
        optional_text(row.model_descriptor.as_deref())
    }),
    ("runtime_descriptor", |row| {
        optional_text(row.runtime_descriptor.as_deref())
    }),
    ("model_vault_descriptor", |row| {
        optional_text(row.model_vault_descriptor.as_deref())
    }),
    ("model_flags", |row| {
        optional_text(row.model_flags.as_deref())
    }),
    ("runtime_flags", |row| {
        optional_text(row.runtime_flags.as_deref())
    }),
    ("benchmark_flags", |row| {
        optional_text(row.benchmark_flags.as_deref())
    }),
    ("requested_model_flags", |row| {
        optional_text(row.requested_model_flags.as_deref())
    }),
    ("requested_runtime_flags", |row| {
        optional_text(row.requested_runtime_flags.as_deref())
    }),
    ("requested_benchmark_flags", |row| {
        optional_text(row.requested_benchmark_flags.as_deref())
    }),
    ("eval_metadata", |row| {
        optional_text(row.eval_metadata.as_deref())
    }),
    ("client_extras", |row| {
        optional_text(row.client_extras.as_deref())
    }),
    // The reserved id names no machine, as in the JSON rows.
    ("client_id", |row| {
        optional_text(
            row.client_id
                .as_deref()
                .filter(|id| *id != SYSTEM_CLIENT_ID),
        )
    }),
    ("device_name", |row| {
        optional_text(row.device_name.as_deref())
    }),
    ("submitted_at", |row| time(row.submitted_at)),
    ("scored_at", |row| time(row.scored_at)),
    ("device_time_ms", |row| {
        row.device_time_ms
            .map(|ms| ms.to_string())
            .unwrap_or_default()
    }),
    ("failure_source", |row| {
        optional_text(row.failure_source.as_ref().map(AsRef::as_ref))
    }),
];

type CsvMap = fn(&ResultRow) -> &BTreeMap<String, Value>;

/// The catalog names of each runtime digest and model digest that the rows
/// show, as the reader sees them.
pub(super) type CatalogNames = BTreeMap<String, DescriptorDisplay>;

type CsvDigest = fn(&ResultRow) -> Option<&String>;

/// A run contains one column, one column per map name, or the catalog name of
/// one of the row's descriptors.
enum CsvRun<'a> {
    One(&'a str, CsvCell),
    Named(&'a str, CsvMap, Vec<&'a str>),
    CatalogName(&'a str, CsvDigest, &'a CatalogNames),
}

impl<'a> CsvRun<'a> {
    /// A column for each name some row carries in `map`, alphabetical.
    fn named(rows: &'a [ResultRow], prefix: &'a str, map: CsvMap) -> Self {
        let names: BTreeSet<&str> = rows
            .iter()
            .flat_map(|row| map(row).keys().map(String::as_str))
            .collect();
        Self::Named(prefix, map, names.into_iter().collect())
    }

    fn headers(&self) -> Vec<String> {
        match self {
            Self::One(header, _) => vec![(*header).to_string()],
            Self::Named(prefix, _, names) => {
                names.iter().map(|name| format!("{prefix}{name}")).collect()
            }
            Self::CatalogName(header, ..) => vec![(*header).to_string()],
        }
    }

    fn cells(&self, row: &ResultRow) -> Vec<String> {
        match self {
            Self::One(_, cell) => vec![cell(row)],
            Self::Named(_, map, names) => names
                .iter()
                .map(|name| map(row).get(*name).map(named_value).unwrap_or_default())
                .collect(),
            Self::CatalogName(_, digest, names) => vec![optional_text(
                digest(row)
                    .and_then(|digest| names.get(digest))
                    .map(|name| name.display.as_str()),
            )],
        }
    }
}

/// `rows` as a CSV file: a header line, then one line per row, in order. Later
/// columns follow the parameters so the earlier ones keep their places. A text
/// cell led by `=`, `+`, `-`, `@`, a tab or a CR gets a leading `'`, so a
/// spreadsheet reads it as text, not as a formula.
pub(super) fn results_csv(rows: &[ResultRow], names: &CatalogNames) -> Result<Vec<u8>, csv::Error> {
    let runs = runs(rows, names);
    let mut out = csv::Writer::from_writer(Vec::new());
    out.write_record(runs.iter().flat_map(CsvRun::headers))?;
    rows.iter()
        .try_for_each(|row| out.write_record(runs.iter().flat_map(|run| run.cells(row))))?;
    out.into_inner().map_err(|e| e.into_error().into())
}

/// The file's columns as the OpenAPI document describes them, read off the
/// runs [`results_csv`] writes, so the two cannot disagree.
pub(crate) fn columns_description() -> String {
    let columns: Vec<String> = runs(&[], &CatalogNames::new())
        .iter()
        .flat_map(|run| match run {
            // With no rows, only the parameter_ and observation_ runs are empty.
            CsvRun::Named(prefix, _, names) if names.is_empty() => {
                vec![format!("`{prefix}<name>` for each name a row carries")]
            }
            CsvRun::One(..) | CsvRun::Named(..) | CsvRun::CatalogName(..) => run
                .headers()
                .iter()
                .map(|header| format!("`{header}`"))
                .collect(),
        })
        .collect();
    format!(
        "A header line, then a line per result row, as the jobs read's `ResultRow` \
         holds it. The columns, in order: {}. Names a row carries are alphabetical \
         within their run. An empty cell is a `null`, or a name the row does not \
         carry; a descriptor, flag, `eval_metadata` or `client_extras` cell holds \
         the stored JSON, or the client's plain text; times are RFC 3339 in UTC. A \
         text cell that starts with `=`, `+`, `-`, `@`, a tab or a carriage return \
         gets a leading `'`, so a spreadsheet reads it as text. \
         `runtime_catalog_name` holds the catalog name of the row's runtime as \
         the reader sees it, for example `llamacpp-v0.7.0::linux-cuda-12-8`. \
         `model_catalog_name` holds the catalog name of the row's model, for \
         example `AcmeLM-VL-3B::gguf::q4_0`. Each is empty for a descriptor with \
         no catalog name.",
        columns.join(", ")
    )
}

/// The runs of columns a file of `rows` holds, in order. `names` gives the
/// catalog names of the runtimes and the models.
fn runs<'a>(rows: &'a [ResultRow], names: &'a CatalogNames) -> Vec<CsvRun<'a>> {
    CSV_COLUMNS
        .iter()
        .map(|&(header, cell)| CsvRun::One(header, cell))
        .chain([
            CsvRun::named(rows, "parameter_", |row| &row.parameters),
            CsvRun::One("client_version", |row| {
                optional_text(row.client_version.as_deref())
            }),
            CsvRun::One("rescored_from", |row| {
                optional_text(row.rescored_from.as_ref().map(JobId::as_str))
            }),
            CsvRun::One("run_submitted_at", |row| time(row.run_submitted_at)),
            CsvRun::One("score_runtime_version", |row| {
                optional_text(row.score_runtime_version.as_deref())
            }),
            CsvRun::One("model_params_total_millions", |row| {
                number(row.model_params_total_millions)
            }),
            CsvRun::One("model_params_active_millions", |row| {
                number(row.model_params_active_millions)
            }),
            CsvRun::CatalogName(
                "runtime_catalog_name",
                |row| row.runtime_descriptor_sha256.as_ref(),
                names,
            ),
            CsvRun::CatalogName(
                "model_catalog_name",
                |row| row.model_descriptor_sha256.as_ref(),
                names,
            ),
            CsvRun::named(rows, "observation_", |row| &row.observations),
            CsvRun::Named(
                "device_",
                |row| &row.device,
                DEVICE_COLUMNS
                    .iter()
                    .filter_map(|column| column.strip_prefix("device_"))
                    .collect(),
            ),
        ])
        .collect()
}

fn text(value: &str) -> String {
    if value.starts_with(['=', '+', '-', '@', '\t', '\r']) {
        format!("'{value}")
    } else {
        value.to_string()
    }
}

fn optional_text(value: Option<&str>) -> String {
    value.map(text).unwrap_or_default()
}

fn number(value: Option<impl ToString>) -> String {
    value.map(|value| value.to_string()).unwrap_or_default()
}

fn time(at: Option<DateTime<Utc>>) -> String {
    at.map(|at| at.to_rfc3339_opts(SecondsFormat::AutoSi, true))
        .unwrap_or_default()
}

/// A string is text. A number or a flag keeps its JSON form, so a negative
/// number stays a number.
fn named_value(value: &Value) -> String {
    match value {
        Value::String(value) => text(value),
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::Array(_) | Value::Object(_) => {
            value.to_string()
        }
    }
}

#[cfg(test)]
mod tests {
    use pipette_plan_types::FailureSource;

    use super::*;

    fn csv_of(rows: &[ResultRow]) -> anyhow::Result<String> {
        Ok(String::from_utf8(results_csv(rows, &CatalogNames::new())?)?)
    }

    fn row(job_id: &str, metric: &str, value: Option<f32>) -> anyhow::Result<ResultRow> {
        Ok(ResultRow {
            job_id: JobId::try_new(job_id)?,
            benchmark_id: "prefill_throughput_256".to_string(),
            benchmark_type: "prefill_throughput".to_string(),
            metric: metric.to_string(),
            value,
            value_stddev: value.map(|_| 0.5),
            unit: "ms".to_string(),
            failed: Some(value.is_none()),
            failed_reason: value
                .is_none()
                .then(|| "out of memory on ev1_a".to_string()),
            model_descriptor: None,
            runtime_descriptor: None,
            runtime_descriptor_sha256: None,
            model_descriptor_sha256: None,
            model_vault_descriptor: None,
            model_flags: None,
            runtime_flags: None,
            benchmark_flags: None,
            requested_model_flags: None,
            requested_runtime_flags: None,
            requested_benchmark_flags: None,
            eval_metadata: None,
            client_extras: None,
            parameters: BTreeMap::new(),
            observations: BTreeMap::new(),
            client_id: Some("ev1_a".to_string()),
            device_name: Some("Pixel 9".to_string()),
            device: BTreeMap::new(),
            client_version: None,
            submitted_at: None,
            scored_at: Some("2026-09-29T10:05:00Z".parse()?),
            device_time_ms: value.map(|_| 184_250),
            failure_source: value.is_none().then_some(FailureSource::Client),
            rescored_from: None,
            run_submitted_at: None,
            score_runtime_version: None,
            model_params_total_millions: None,
            model_params_active_millions: None,
        })
    }

    const CSV_HEADER: &str = "job_id,benchmark_id,benchmark_type,metric,value,value_stddev,\
                              unit,failed,failed_reason,model_descriptor,runtime_descriptor,\
                              model_vault_descriptor,model_flags,runtime_flags,benchmark_flags,\
                              requested_model_flags,requested_runtime_flags,\
                              requested_benchmark_flags,eval_metadata,client_extras,client_id,\
                              device_name,submitted_at,scored_at,device_time_ms,failure_source";

    #[test]
    fn the_csv_has_a_column_for_each_parameter_any_row_carries() -> anyhow::Result<()> {
        let mut measured = row("job-1", "ttft", Some(41.0))?;
        measured.parameters = BTreeMap::from([("prefill_tokens".to_string(), Value::from(256))]);
        let mut failed = row("job-2", "ttft", None)?;
        failed.parameters = BTreeMap::from([("eval_id".to_string(), Value::from("mmlu"))]);
        let csv = csv_of(&[measured, failed])?;
        assert_eq!(
            csv,
            format!(
                "{CSV_HEADER},parameter_eval_id,parameter_prefill_tokens,{tail}\n\
                 job-1,prefill_throughput_256,prefill_throughput,ttft,41,0.5,ms,false,,,,,,,,,,,,,\
                 ev1_a,Pixel 9,,2026-09-29T10:05:00Z,184250,,,256,,,,,,,,,{empty}\n\
                 job-2,prefill_throughput_256,prefill_throughput,ttft,,,ms,true,\
                 out of memory on ev1_a,,,,,,,,,,,,ev1_a,Pixel 9,,2026-09-29T10:05:00Z,,client,\
                 mmlu,,,,,,,,,,{empty}\n",
                tail = csv_tail(),
                empty = device_cells(&[]),
            )
        );
        Ok(())
    }

    fn rows_with_device_and_client() -> anyhow::Result<Vec<ResultRow>> {
        let mut measured = row("job-1", "ttft", Some(41.0))?;
        measured.client_version = Some("1.4.0".to_string());
        measured.rescored_from = Some(JobId::try_new("job-0")?);
        measured.run_submitted_at = Some("2026-09-01T08:00:00Z".parse()?);
        measured.score_runtime_version = Some("1.2.3".to_string());
        measured.model_params_total_millions = Some(8340);
        measured.model_params_active_millions = Some(1500);
        measured.observations =
            BTreeMap::from([("max_swap_bytes".to_string(), Value::from(1_048_576))]);
        measured.device = BTreeMap::from([
            ("os_name".to_string(), Value::from("Android")),
            ("chip_model".to_string(), Value::from("=Tensor G4")),
        ]);
        let mut other = row("job-2", "ttft", Some(40.0))?;
        other.parameters = BTreeMap::from([("prefill_tokens".to_string(), Value::from(256))]);
        other.device = BTreeMap::from([("ram_bytes".to_string(), Value::from(12_000_000_000_u64))]);
        Ok(vec![measured, other])
    }

    /// A row's `device_*` cells, `set` filled in and the rest empty.
    fn device_cells(set: &[(&str, &str)]) -> String {
        DEVICE_COLUMNS
            .iter()
            .map(|column| {
                set.iter()
                    .find(|(name, _)| name == column)
                    .map_or("", |(_, cell)| cell)
            })
            .collect::<Vec<_>>()
            .join(",")
    }

    /// The header after the parameters and observations.
    fn csv_tail() -> String {
        format!(
            "client_version,rescored_from,run_submitted_at,score_runtime_version,\
             model_params_total_millions,model_params_active_millions,runtime_catalog_name,\
             model_catalog_name,{}",
            DEVICE_COLUMNS.join(",")
        )
    }

    #[test]
    fn the_csv_carries_the_device_and_the_client_after_the_parameters() -> anyhow::Result<()> {
        let csv = csv_of(&rows_with_device_and_client()?)?;
        // A device text led by `=` is not a formula.
        let first = device_cells(&[
            ("device_os_name", "Android"),
            ("device_chip_model", "'=Tensor G4"),
        ]);
        let second = device_cells(&[("device_ram_bytes", "12000000000")]);
        assert_eq!(
            csv,
            format!(
                "{CSV_HEADER},parameter_prefill_tokens,client_version,rescored_from,\
                 run_submitted_at,score_runtime_version,model_params_total_millions,\
                 model_params_active_millions,runtime_catalog_name,model_catalog_name,\
                 observation_max_swap_bytes,{devices}\n\
                 job-1,prefill_throughput_256,prefill_throughput,ttft,41,0.5,ms,false,,,,,,,,,,,,,\
                 ev1_a,Pixel 9,,2026-09-29T10:05:00Z,184250,,,1.4.0,job-0,2026-09-01T08:00:00Z,1.2.3,8340,1500,,,1048576,{first}\n\
                 job-2,prefill_throughput_256,prefill_throughput,ttft,40,0.5,ms,false,,,,,,,,,,,,,\
                 ev1_a,Pixel 9,,2026-09-29T10:05:00Z,184250,,256,,,,,,,,,,{second}\n",
                devices = DEVICE_COLUMNS.join(",")
            )
        );
        Ok(())
    }

    /// The document's account of the columns names each header the file has,
    /// in its order: a named run by its prefix.
    #[test]
    fn the_columns_description_follows_the_file() -> anyhow::Result<()> {
        let rows = rows_with_device_and_client()?;
        let csv = csv_of(&rows)?;
        let header = csv.lines().next().unwrap_or_default();
        let description = columns_description();
        let mut rest = description.as_str();
        let missing: Vec<&str> = header
            .split(',')
            .filter(|column| {
                let named = ["parameter_", "observation_"]
                    .iter()
                    .find(|prefix| column.starts_with(*prefix))
                    .map(|prefix| format!("`{prefix}<name>`"));
                let spelled = named.unwrap_or_else(|| format!("`{column}`"));
                match rest.find(&spelled) {
                    Some(at) => {
                        rest = rest.get(at..).unwrap_or_default();
                        false
                    }
                    None => true,
                }
            })
            .collect();
        anyhow::ensure!(
            missing.is_empty(),
            "out of order or absent: {missing:?}\n{description}"
        );
        Ok(())
    }

    #[test]
    fn the_csv_names_each_column_once() -> anyhow::Result<()> {
        let csv = csv_of(&rows_with_device_and_client()?)?;
        let header: Vec<&str> = csv.lines().next().unwrap_or_default().split(',').collect();
        let unique: BTreeSet<&str> = header.iter().copied().collect();
        assert_eq!(unique.len(), header.len(), "{header:?}");
        Ok(())
    }

    /// Each JSON column's cell is its stored text: canonical JSON, or a
    /// client's plain text, guarded where it reads as a formula.
    #[test]
    fn the_csv_carries_each_json_column_as_stored() -> anyhow::Result<()> {
        let mut measured = row("job-1", "ttft", Some(41.0))?;
        measured.model_descriptor = Some(r#"{"path":"m.gguf","type":"gguf_text"}"#.to_string());
        measured.runtime_descriptor = Some(r#"{"type":"catalog_defined"}"#.to_string());
        measured.model_flags = Some("enable_thinking=true".to_string());
        measured.runtime_flags = Some("--mmap 0 -fa 1".to_string());
        measured.requested_runtime_flags = Some(r#"{"threads":8}"#.to_string());
        measured.eval_metadata = Some(r#"{"samples_failed":3}"#.to_string());
        measured.client_extras = Some(r#"{"peak_rss_kib":10}"#.to_string());
        let csv = csv_of(&[measured])?;
        assert!(
            csv.contains(concat!(
                r#",false,,"{""path"":""m.gguf"",""type"":""gguf_text""}","{""type"":""catalog_defined""}",,"#,
                r#"enable_thinking=true,'--mmap 0 -fa 1,,,"{""threads"":8}",,"{""samples_failed"":3}","#,
                r#""{""peak_rss_kib"":10}",ev1_a,"#
            )),
            "{csv}"
        );
        Ok(())
    }

    /// The reserved id names no machine, as in the JSON rows.
    #[test]
    fn the_csv_names_no_machine_for_a_failure_the_server_declared() -> anyhow::Result<()> {
        let mut declared = row("job-1", "ttft", None)?;
        declared.client_id = Some(SYSTEM_CLIENT_ID.to_string());
        declared.device_name = None;
        let csv = csv_of(&[declared])?;
        assert!(
            csv.contains(concat!(
                ",out of memory on ev1_a,",
                // The eleven JSON columns, then the client and the device.
                ",,,,,,,,,,,",
                ",,,2026-09-29T10:05:00Z"
            )),
            "{csv}"
        );
        Ok(())
    }

    /// A row whose runtime digest or model digest has a catalog name carries
    /// the name. A row without one, and a row with no digest, leave the cell
    /// empty.
    #[rstest::rstest]
    #[case::runtime(
        "runtime_catalog_name",
        |row: &mut ResultRow, digest| row.runtime_descriptor_sha256 = digest,
        "llamacpp-v0.7.0::linux-cuda-12-8"
    )]
    #[case::model(
        "model_catalog_name",
        |row: &mut ResultRow, digest| row.model_descriptor_sha256 = digest,
        "AcmeLM-VL-3B::gguf::q4_0"
    )]
    fn the_catalog_name_is_filled_for_named_rows_only(
        #[case] header: &str,
        #[case] set_digest: fn(&mut ResultRow, Option<String>),
        #[case] display: &str,
    ) -> anyhow::Result<()> {
        let named_digest = "ab".repeat(32);
        let mut named = row("job-1", "ttft", Some(41.0))?;
        set_digest(&mut named, Some(named_digest.clone()));
        let mut unnamed = row("job-2", "ttft", Some(40.0))?;
        set_digest(&mut unnamed, Some("cd".repeat(32)));
        let bare = row("job-3", "ttft", Some(39.0))?;
        let (name, _) = display.split_once("::").unwrap_or((display, ""));
        let names = CatalogNames::from([(
            named_digest,
            DescriptorDisplay {
                display: display.to_string(),
                name: name.to_string(),
                synonyms: Vec::new(),
            },
        )]);
        let csv = String::from_utf8(results_csv(&[named, unnamed, bare], &names)?)?;
        let headers: Vec<&str> = csv.lines().next().unwrap_or_default().split(',').collect();
        let cells_of = |column: &str| -> anyhow::Result<Vec<String>> {
            let at = headers
                .iter()
                .position(|header| *header == column)
                .ok_or_else(|| anyhow::anyhow!("no {column} column in {headers:?}"))?;
            Ok(csv
                .lines()
                .skip(1)
                .map(|line| line.split(',').nth(at).unwrap_or_default().to_string())
                .collect())
        };
        assert_eq!(cells_of(header)?, [display, "", ""]);
        let other = ["runtime_catalog_name", "model_catalog_name"]
            .into_iter()
            .find(|column| *column != header)
            .ok_or_else(|| anyhow::anyhow!("two catalog name columns"))?;
        assert_eq!(cells_of(other)?, ["", "", ""]);
        Ok(())
    }

    /// The device columns are there with no row to carry one.
    #[test]
    fn a_plan_without_results_is_a_header_alone() -> anyhow::Result<()> {
        assert_eq!(csv_of(&[])?, format!("{CSV_HEADER},{}\n", csv_tail()));
        Ok(())
    }

    #[rstest::rstest]
    #[case::plain("Pixel 9", "Pixel 9")]
    #[case::comma("Pixel 9, Pro", r#""Pixel 9, Pro""#)]
    #[case::quote(r#"Pixel "9""#, r#""Pixel ""9""""#)]
    #[case::newline("Pixel\n9", "\"Pixel\n9\"")]
    #[case::equals("=HYPERLINK(\"x\")", r#""'=HYPERLINK(""x"")""#)]
    #[case::plus("+1", "'+1")]
    #[case::minus("-1", "'-1")]
    #[case::at("@SUM(A1)", "'@SUM(A1)")]
    #[case::tab("\tx", "'\tx")]
    #[case::carriage_return("\rx", "\"'\rx\"")]
    fn a_csv_text_cell_is_quoted_and_never_a_formula(
        #[case] device_name: &str,
        #[case] cell: &str,
    ) -> anyhow::Result<()> {
        let mut named = row("job-1", "ttft", Some(41.0))?;
        named.device_name = Some(device_name.to_string());
        let csv = csv_of(&[named])?;
        assert!(csv.contains(&format!(",ev1_a,{cell},")), "{csv}");
        Ok(())
    }

    #[rstest::rstest]
    #[case::negative_number(Value::from(-3), "-3")]
    #[case::fraction(Value::from(0.5), "0.5")]
    #[case::flag(Value::from(true), "true")]
    #[case::text(Value::from("mmlu"), "mmlu")]
    #[case::text_that_reads_as_a_formula(Value::from("-3"), "'-3")]
    fn a_csv_named_value_cell_keeps_a_number_a_number(#[case] value: Value, #[case] cell: &str) {
        assert_eq!(named_value(&value), cell);
    }
}
