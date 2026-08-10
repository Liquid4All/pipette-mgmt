use std::path::Path;
use std::sync::Arc;

use arrow::array::{Array, BooleanArray, Int64Array, StringArray};
use arrow::datatypes::{DataType, Field, Schema};
use arrow::record_batch::RecordBatch;

use crate::parquet_utils::{
    WriterOpts, read_batches_from_bytes, read_batches_from_file, write_batch_bytes,
    write_batches_to_file,
};

#[derive(Debug, Clone)]
pub struct EvalSampleResult {
    pub id: String,
    pub messages: String,
    pub completion: String,
    pub is_correct: bool,
    /// `true` when the sample ended in `failure` — derived from
    /// `stop_reason == failure`, not the client's flag (see
    /// Liquid4All/pipette-clients#103). Marks a sample the client couldn't
    /// complete, e.g. a runtime crash mid-`/completion`. Still forwarded to
    /// `/score` (scored `is_correct = false` on its empty completion) and
    /// counted in the accuracy denominator; surfaces as a per-sample badge in
    /// the datasheet UI.
    pub failed: bool,
    /// Free-form, human-readable description of the failure, when known.
    /// `None` for `failed=false` rows.
    pub failed_reason: Option<String>,
    /// Canonical stop reason for this sample, one of `eos` | `truncated` |
    /// `doom_loop` | `failure` | `unknown` (see `docs/scoring-service.md`
    /// for the enum contract). `None` when the sample was never labelled —
    /// e.g. a pre-feature client that didn't report one. Client-`failed`
    /// samples are mapped to `failure` here.
    pub stop_reason: Option<String>,
    /// Provenance of `stop_reason`: `recorded` when captured at generation
    /// by the client (or derived from the client's `failed` flag), `derived`
    /// when reconstructed after the fact (e.g. a tokenizer-based backfill).
    /// `None` whenever `stop_reason` is `None`.
    pub stop_reason_source: Option<String>,
    /// Free-form observation behind `stop_reason` — the *why / raw signal*
    /// the client recorded (crash detail for `failure`, unclassified
    /// `stop_type` for `unknown`, the trigger for `doom_loop`); normally
    /// empty for a clean `eos` / `truncated`. Re-injected from the client
    /// submission; generalizes `failed_reason`. `None` when unreported.
    pub stop_detail: Option<String>,
    /// Completion (output) token count for this sample, re-injected from the
    /// client submission. `None` when the client didn't report one.
    pub completion_tokens: Option<i64>,
}

pub fn parquet_schema() -> Schema {
    Schema::new(vec![
        Field::new("id", DataType::Utf8, false),
        Field::new("messages", DataType::Utf8, false),
        Field::new("completion", DataType::Utf8, false),
        Field::new("is_correct", DataType::Boolean, false),
        // Nullable so historical parquet (no column) reads back as `false`/`None`
        // via the `column_by_name` lookup below.
        Field::new("failed", DataType::Boolean, true),
        Field::new("failed_reason", DataType::Utf8, true),
        // Per-sample stop_reason plumbing. All nullable so parquet written
        // before these columns existed reads back as `None`.
        Field::new("stop_reason", DataType::Utf8, true),
        Field::new("stop_reason_source", DataType::Utf8, true),
        Field::new("stop_detail", DataType::Utf8, true),
        Field::new("completion_tokens", DataType::Int64, true),
    ])
}

pub fn write_parquet(
    opts: WriterOpts,
    path: &Path,
    rows: &[EvalSampleResult],
) -> anyhow::Result<()> {
    if rows.is_empty() {
        return Ok(());
    }
    let schema = Arc::new(parquet_schema());
    let batch = rows_to_batch(&schema, rows)?;
    write_batches_to_file(opts, path, schema, &[batch])
}

pub fn read_parquet(path: &Path) -> anyhow::Result<Option<Vec<EvalSampleResult>>> {
    if !path.exists() {
        return Ok(None);
    }
    let mut rows = Vec::new();
    for batch in read_batches_from_file(path)? {
        rows.extend(batch_to_rows(&batch?)?);
    }
    Ok(Some(rows))
}

/// Serialize eval sample results to Parquet bytes in memory.
pub(crate) fn rows_to_parquet_bytes(
    opts: WriterOpts,
    rows: &[EvalSampleResult],
) -> anyhow::Result<Vec<u8>> {
    let schema = Arc::new(parquet_schema());
    let batch = rows_to_batch(&schema, rows)?;
    write_batch_bytes(opts, schema, &batch)
}

/// Deserialize eval sample results from Parquet bytes.
pub(crate) fn rows_from_parquet_bytes(data: &[u8]) -> anyhow::Result<Vec<EvalSampleResult>> {
    let mut rows = Vec::new();
    for batch in read_batches_from_bytes(data)? {
        rows.extend(batch_to_rows(&batch?)?);
    }
    Ok(rows)
}

fn rows_to_batch(schema: &Arc<Schema>, rows: &[EvalSampleResult]) -> anyhow::Result<RecordBatch> {
    let ids: Vec<&str> = rows.iter().map(|r| r.id.as_str()).collect();
    let messages: Vec<&str> = rows.iter().map(|r| r.messages.as_str()).collect();
    let completions: Vec<&str> = rows.iter().map(|r| r.completion.as_str()).collect();
    let is_corrects: Vec<bool> = rows.iter().map(|r| r.is_correct).collect();
    let faileds: Vec<Option<bool>> = rows.iter().map(|r| Some(r.failed)).collect();
    let failed_reasons: Vec<Option<&str>> =
        rows.iter().map(|r| r.failed_reason.as_deref()).collect();
    let stop_reasons: Vec<Option<&str>> = rows.iter().map(|r| r.stop_reason.as_deref()).collect();
    let stop_reason_sources: Vec<Option<&str>> = rows
        .iter()
        .map(|r| r.stop_reason_source.as_deref())
        .collect();
    let stop_details: Vec<Option<&str>> = rows.iter().map(|r| r.stop_detail.as_deref()).collect();
    let completion_tokens: Vec<Option<i64>> = rows.iter().map(|r| r.completion_tokens).collect();

    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(StringArray::from(ids)),
            Arc::new(StringArray::from(messages)),
            Arc::new(StringArray::from(completions)),
            Arc::new(BooleanArray::from(is_corrects)),
            Arc::new(BooleanArray::from(faileds)),
            Arc::new(StringArray::from(failed_reasons)),
            Arc::new(StringArray::from(stop_reasons)),
            Arc::new(StringArray::from(stop_reason_sources)),
            Arc::new(StringArray::from(stop_details)),
            Arc::new(Int64Array::from(completion_tokens)),
        ],
    )?;

    Ok(batch)
}

fn batch_to_rows(batch: &RecordBatch) -> anyhow::Result<Vec<EvalSampleResult>> {
    let ids = batch
        .column_by_name("id")
        .ok_or_else(|| anyhow::anyhow!("missing id column"))?
        .as_any()
        .downcast_ref::<StringArray>()
        .ok_or_else(|| anyhow::anyhow!("id column has unexpected type"))?;
    let messages = batch
        .column_by_name("messages")
        .ok_or_else(|| anyhow::anyhow!("missing messages column"))?
        .as_any()
        .downcast_ref::<StringArray>()
        .ok_or_else(|| anyhow::anyhow!("messages column has unexpected type"))?;
    let completions = batch
        .column_by_name("completion")
        .ok_or_else(|| anyhow::anyhow!("missing completion column"))?
        .as_any()
        .downcast_ref::<StringArray>()
        .ok_or_else(|| anyhow::anyhow!("completion column has unexpected type"))?;
    let is_corrects = batch
        .column_by_name("is_correct")
        .ok_or_else(|| anyhow::anyhow!("missing is_correct column"))?
        .as_any()
        .downcast_ref::<BooleanArray>()
        .ok_or_else(|| anyhow::anyhow!("is_correct column has unexpected type"))?;
    // failed / failed_reason: optional columns. Pre-feature parquet
    // files don't have them; treat absence as the default value so old
    // data reads back as "not failed".
    let faileds = batch
        .column_by_name("failed")
        .and_then(|c| c.as_any().downcast_ref::<BooleanArray>());
    let failed_reasons = batch
        .column_by_name("failed_reason")
        .and_then(|c| c.as_any().downcast_ref::<StringArray>());
    // stop_reason columns: also optional. Pre-feature parquet lacks them; a
    // missing column (or a null cell) reads back as `None`.
    let stop_reasons = batch
        .column_by_name("stop_reason")
        .and_then(|c| c.as_any().downcast_ref::<StringArray>());
    let stop_reason_sources = batch
        .column_by_name("stop_reason_source")
        .and_then(|c| c.as_any().downcast_ref::<StringArray>());
    let stop_details = batch
        .column_by_name("stop_detail")
        .and_then(|c| c.as_any().downcast_ref::<StringArray>());
    let completion_tokens = batch
        .column_by_name("completion_tokens")
        .and_then(|c| c.as_any().downcast_ref::<Int64Array>());

    let opt_str = |arr: Option<&StringArray>, i: usize| {
        arr.and_then(|a| (!a.is_null(i)).then(|| a.value(i).to_string()))
    };

    let mut rows = Vec::with_capacity(batch.num_rows());
    for i in 0..batch.num_rows() {
        let failed = faileds
            .map(|a| !a.is_null(i) && a.value(i))
            .unwrap_or(false);
        let failed_reason = opt_str(failed_reasons, i);
        rows.push(EvalSampleResult {
            id: ids.value(i).to_string(),
            messages: messages.value(i).to_string(),
            completion: completions.value(i).to_string(),
            is_correct: is_corrects.value(i),
            failed,
            failed_reason,
            stop_reason: opt_str(stop_reasons, i),
            stop_reason_source: opt_str(stop_reason_sources, i),
            stop_detail: opt_str(stop_details, i),
            completion_tokens: completion_tokens.and_then(|a| (!a.is_null(i)).then(|| a.value(i))),
        });
    }

    Ok(rows)
}

#[cfg(test)]
mod tests {
    use crate::eval_sample_result::*;
    use anyhow::Context;

    fn make_test_rows() -> Vec<EvalSampleResult> {
        vec![
            EvalSampleResult {
                id: "sample1".to_string(),
                messages: r#"[{"role":"user","content":"What is 2+2?"}]"#.to_string(),
                completion: "4".to_string(),
                is_correct: true,
                failed: false,
                failed_reason: None,
                stop_reason: Some("eos".to_string()),
                stop_reason_source: Some("recorded".to_string()),
                stop_detail: None,
                completion_tokens: Some(1),
            },
            EvalSampleResult {
                id: "sample2".to_string(),
                messages: r#"[{"role":"user","content":"What is the capital of France?"}]"#
                    .to_string(),
                completion: "London".to_string(),
                is_correct: false,
                failed: false,
                failed_reason: None,
                stop_reason: Some("truncated".to_string()),
                stop_reason_source: Some("recorded".to_string()),
                stop_detail: None,
                completion_tokens: Some(8192),
            },
        ]
    }

    #[test]
    fn test_write_and_read_parquet() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("test.parquet");

        let rows = make_test_rows();
        write_parquet(WriterOpts::default(), &path, &rows)?;

        let result = read_parquet(&path)?.context("expected parquet data")?;
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].id, "sample1");
        assert_eq!(
            result[0].messages,
            r#"[{"role":"user","content":"What is 2+2?"}]"#
        );
        assert_eq!(result[0].completion, "4");
        assert!(result[0].is_correct);
        assert!(!result[0].failed);
        assert!(result[0].failed_reason.is_none());
        assert_eq!(result[1].id, "sample2");
        assert!(!result[1].is_correct);
        Ok(())
    }

    #[test]
    fn test_failed_row_round_trips() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("failed.parquet");
        let rows = vec![
            EvalSampleResult {
                id: "ok".to_string(),
                messages: r#"[]"#.to_string(),
                completion: "yes".to_string(),
                is_correct: true,
                failed: false,
                failed_reason: None,
                stop_reason: Some("eos".to_string()),
                stop_reason_source: Some("recorded".to_string()),
                stop_detail: None,
                completion_tokens: Some(3),
            },
            EvalSampleResult {
                id: "bad".to_string(),
                messages: r#"[]"#.to_string(),
                completion: String::new(),
                is_correct: false,
                failed: true,
                failed_reason: Some("llama-server crashed".to_string()),
                stop_reason: Some("failure".to_string()),
                stop_reason_source: Some("recorded".to_string()),
                stop_detail: Some("llama-server crashed".to_string()),
                completion_tokens: None,
            },
        ];
        write_parquet(WriterOpts::default(), &path, &rows)?;

        let result = read_parquet(&path)?.context("expected parquet data")?;
        assert_eq!(result.len(), 2);
        assert!(!result[0].failed);
        assert_eq!(result[0].failed_reason, None);
        assert_eq!(result[0].stop_reason.as_deref(), Some("eos"));
        assert_eq!(result[0].stop_reason_source.as_deref(), Some("recorded"));
        assert_eq!(result[0].stop_detail, None);
        assert_eq!(result[0].completion_tokens, Some(3));
        assert!(result[1].failed);
        assert_eq!(
            result[1].failed_reason.as_deref(),
            Some("llama-server crashed")
        );
        assert_eq!(result[1].stop_reason.as_deref(), Some("failure"));
        assert_eq!(
            result[1].stop_detail.as_deref(),
            Some("llama-server crashed")
        );
        assert_eq!(result[1].completion_tokens, None);
        Ok(())
    }

    #[test]
    fn test_pre_feature_parquet_reads_new_columns_as_none() -> anyhow::Result<()> {
        // A parquet written before the stop_reason feature carries only the
        // original six columns. Reading it back must not error and must
        // surface the new columns as `None` (schema evolution / forward
        // compatibility).
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("legacy.parquet");

        let old_schema = Arc::new(Schema::new(vec![
            Field::new("id", DataType::Utf8, false),
            Field::new("messages", DataType::Utf8, false),
            Field::new("completion", DataType::Utf8, false),
            Field::new("is_correct", DataType::Boolean, false),
            Field::new("failed", DataType::Boolean, true),
            Field::new("failed_reason", DataType::Utf8, true),
        ]));
        let batch = RecordBatch::try_new(
            old_schema.clone(),
            vec![
                Arc::new(StringArray::from(vec!["s1"])),
                Arc::new(StringArray::from(vec!["[]"])),
                Arc::new(StringArray::from(vec!["hi"])),
                Arc::new(BooleanArray::from(vec![true])),
                Arc::new(BooleanArray::from(vec![Some(false)])),
                Arc::new(StringArray::from(vec![None::<&str>])),
            ],
        )?;
        write_batches_to_file(WriterOpts::default(), &path, old_schema, &[batch])?;

        let result = read_parquet(&path)?.context("expected parquet data")?;
        assert_eq!(result.len(), 1);
        let row = &result[0];
        assert_eq!(row.id, "s1");
        assert!(row.is_correct);
        assert!(!row.failed);
        assert_eq!(row.stop_reason, None);
        assert_eq!(row.stop_reason_source, None);
        assert_eq!(row.stop_detail, None);
        assert_eq!(row.completion_tokens, None);
        Ok(())
    }

    #[test]
    fn test_read_nonexistent_file() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("nonexistent.parquet");
        let result = read_parquet(&path)?;
        assert!(result.is_none());
        Ok(())
    }

    #[test]
    fn test_write_empty_rows() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("empty.parquet");
        write_parquet(WriterOpts::default(), &path, &[])?;
        assert!(!path.exists());
        Ok(())
    }

    #[test]
    fn test_multiline_messages() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("test.parquet");

        let rows = vec![EvalSampleResult {
            id: "s1".to_string(),
            messages: r#"[{"role":"system","content":"You are a helper."},{"role":"user","content":"Do X"}]"#.to_string(),
            completion: "Done".to_string(),
            is_correct: true,
            failed: false,
            failed_reason: None,
            stop_reason: None,
            stop_reason_source: None,
            stop_detail: None,
            completion_tokens: None,
        }];
        write_parquet(WriterOpts::default(), &path, &rows)?;

        let result = read_parquet(&path)?.context("expected parquet data")?;
        assert_eq!(result.len(), 1);
        assert!(result[0].messages.contains("system"));
        assert!(result[0].messages.contains("user"));
        Ok(())
    }
}
