mod helpers;

use std::collections::HashMap;

use arrow::array::Array;
use axum::http::StatusCode;
use chrono::Datelike;
use rstest::rstest;
use serde_json::json;

use pipette_mgmt::score;
use pipette_mgmt::stores::{ScoreQueueStage, build_local_fs_stores};
use pipette_mgmt::types::BenchmarkId;
use pipette_mgmt::warehouse;

use helpers::{
    TEST_LIST_LIMIT, authed_get, body_json, job, make_state, make_state_with_evals_url,
    register_and_approve, run_full_score, setup_benchmarks, setup_models, start_mock_evals_server,
    start_mock_evals_server_malformed_score, submit_benchmark,
};

/// Chunk size for the chunked-scoring tests, small enough that a run spans
/// several chunks. `const` so the non-zero proof is compile-time (same pattern
/// as `TEST_LIST_LIMIT`).
const TEST_CHUNK_SIZE: std::num::NonZeroUsize = std::num::NonZeroUsize::new(2).unwrap();

/// Assert a nullable column holds `expected` on every scored row of a
/// partition, `None` standing for SQL NULL. `A` is the arrow array the column
/// downcasts to, and `value` reads one cell of it. Returns the number of rows
/// checked so callers can prove the partition wasn't empty — an assertion that
/// runs zero times passes for the wrong reason.
fn assert_column<A, T>(
    partition: &std::path::Path,
    column: &str,
    expected: Option<T>,
    value: impl Fn(&A, usize) -> T,
) -> anyhow::Result<usize>
where
    A: Array + Clone + 'static,
    T: PartialEq + std::fmt::Debug,
{
    let parquet_files = std::fs::read_dir(partition)?
        .map(|entry| Ok(entry?.path()))
        .collect::<anyhow::Result<Vec<_>>>()?
        .into_iter()
        .filter(|path| path.extension().is_some_and(|e| e == "parquet"))
        .collect::<Vec<_>>();

    parquet_files
        .into_iter()
        .try_fold(0usize, |checked, path| -> anyhow::Result<usize> {
            let file = std::fs::File::open(&path)?;
            parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder::try_new(file)?
                .build()?
                .try_fold(checked, |checked, batch| -> anyhow::Result<usize> {
                    let batch = batch?;
                    let values = batch
                        .column_by_name(column)
                        .ok_or_else(|| anyhow::anyhow!("{column} column must exist"))?
                        .as_any()
                        .downcast_ref::<A>()
                        .ok_or_else(|| anyhow::anyhow!("{column} has unexpected type"))?
                        .clone();
                    (0..batch.num_rows()).for_each(|i| {
                        assert_eq!(
                            (!values.is_null(i)).then(|| value(&values, i)),
                            expected,
                            "{column} on row {i}"
                        );
                    });
                    Ok(checked + batch.num_rows())
                })
        })
}

/// [`assert_column`] over a nullable string column. Compares owned `String`s,
/// because a borrowed cell would tie the compared value to the array it came
/// from.
fn assert_string_column(
    partition: &std::path::Path,
    column: &str,
    expected: Option<&str>,
) -> anyhow::Result<usize> {
    assert_column::<arrow::array::StringArray, _>(
        partition,
        column,
        expected.map(String::from),
        |values, i| values.value(i).to_string(),
    )
}

/// [`assert_column`] over a nullable `int64` column.
fn assert_i64_column(
    partition: &std::path::Path,
    column: &str,
    expected: Option<i64>,
) -> anyhow::Result<usize> {
    assert_column::<arrow::array::Int64Array, _>(partition, column, expected, |values, i| {
        values.value(i)
    })
}

// ---------------------------------------------------------------------------
// Throughput scoring
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_throughput_scoring_produces_metrics() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (sk, client_id) = register_and_approve(&state).await?;

    let job_id = submit_benchmark(
        &state,
        &sk,
        &client_id,
        &json!({
            "benchmark_id": "prefill_throughput_256",
            "device_name": "test-device",
            "device_form_factor": "embedded",
            "device_os_name": "Linux",
            "device_os_version": "22.04",
            "device_chip_model": "test-chip",
            "device_ram_bytes": 17179869184i64,
            "model_name": "llama-3.2-1b",
            "model_quant": "q4_0",
            "model_params_total_millions": 1000,
            "runtime_name": "llama.cpp",
            "runtime_version": "b5000",
            "prefill_time_ms": 34.7,
            "prefill_time_ms_stddev": 1.2
        }),
    )
    .await?;

    score::run_process_submissions(&state.config, build_local_fs_stores(&state.config)?).await?;

    let resp = authed_get(&state, &sk, &client_id, &format!("/jobs/{job_id}")).await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await?;
    assert_eq!(body["status"], "processed");
    assert!(body["scored_at"].as_str().is_some());
    assert!(body["score_runtime_version"].is_null());
    let metrics = body["metrics"]
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("missing metrics array"))?;
    let metric_names: Vec<&str> = metrics
        .iter()
        .map(|m| {
            m["metric"]
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("missing metric name"))
        })
        .collect::<Result<Vec<_>, _>>()?;
    assert!(metric_names.contains(&"ttft"));
    assert!(metric_names.contains(&"prefill_throughput"));
    let ttft = metrics
        .iter()
        .find(|m| m["metric"] == "ttft")
        .ok_or_else(|| anyhow::anyhow!("ttft metric not found"))?;
    let ttft_stddev = ttft["value_stddev"]
        .as_f64()
        .ok_or_else(|| anyhow::anyhow!("ttft missing value_stddev"))?;
    assert!((ttft_stddev - 1.2).abs() < 0.001);
    let throughput = metrics
        .iter()
        .find(|m| m["metric"] == "prefill_throughput")
        .ok_or_else(|| anyhow::anyhow!("prefill_throughput metric not found"))?;
    // throughput_stddev = throughput * stddev / time
    // throughput = 256 / 34.7 * 1000 ≈ 7378.1, stddev = 1.2, time = 34.7
    // → 7378.1 * 1.2 / 34.7 ≈ 255.1
    assert!(throughput["value_stddev"].as_f64().is_some());
    Ok(())
}

#[tokio::test]
async fn test_scorer_preserves_mlx_model_name_and_quant() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (_sk, client_id) = register_and_approve(&state).await?;

    let job_id = "mlx-preserve-job";
    let key = job(job_id);
    // Body intentionally omits `message_type` — exercises the
    // legacy-body fallback in `parse_stored_submission`. ~20k
    // pre-existing bodies in production lack the tag.
    let body = json!({
        "benchmark_id": "prefill_throughput_256",
        "benchmark_type": "prefill_throughput",
        "client_id": client_id.as_str(),
        "job_id": job_id,
        "submitted_at": "2026-01-01T00:00:00Z",
        "device_name": "test-device",
        "device_form_factor": "embedded",
        "device_os_name": "Linux",
        "device_os_version": "22.04",
        "device_chip_model": "test-chip",
        "device_ram_bytes": 17_179_869_184i64,
        "model_name": "mlx-community/LFM2-2.6B",
        "model_quant": "4bit",
        "model_params_total_millions": 2600,
        "runtime_name": "mlx-lm",
        "runtime_version": "0.26.0",
        "prefill_time_ms": 34.7
    });
    build_local_fs_stores(&state.config)?
        .submissions
        .write_incoming(&key, &body)
        .await?;

    score::run_process_submissions(&state.config, build_local_fs_stores(&state.config)?).await?;

    let data_dir = state
        .config
        .storage
        .data_dir()
        .ok_or_else(|| anyhow::anyhow!("expected local_fs config"))?;
    let partition = warehouse::warehouse_day_partition_dir(
        &data_dir.join("warehouse/results"),
        &BenchmarkId::try_new("prefill_throughput_256")?,
        &client_id,
        "2026-01-01",
    );
    let mut seen = 0usize;
    for entry in std::fs::read_dir(&partition)? {
        let path = entry?.path();
        if path.extension().is_some_and(|e| e == "parquet") {
            let file = std::fs::File::open(&path)?;
            let reader =
                parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder::try_new(file)?
                    .build()?;
            for batch in reader {
                let batch = batch?;
                let model_names = batch
                    .column_by_name("model_name")
                    .ok_or_else(|| anyhow::anyhow!("model_name column must exist"))?
                    .as_any()
                    .downcast_ref::<arrow::array::StringArray>()
                    .ok_or_else(|| anyhow::anyhow!("model_name column has unexpected type"))?;
                let model_quants = batch
                    .column_by_name("model_quant")
                    .ok_or_else(|| anyhow::anyhow!("model_quant column must exist"))?
                    .as_any()
                    .downcast_ref::<arrow::array::StringArray>()
                    .ok_or_else(|| anyhow::anyhow!("model_quant column has unexpected type"))?;
                for i in 0..batch.num_rows() {
                    assert_eq!(model_names.value(i), "mlx-community/LFM2-2.6B");
                    assert_eq!(model_quants.value(i), "4bit");
                    seen += 1;
                }
            }
        }
    }
    assert!(seen > 0, "expected scored mlx rows");
    Ok(())
}

/// The collector accepts padded device/model/runtime strings; the scorer is
/// responsible for trimming them before writing the warehouse row. Submit
/// a job with whitespace on every trimmable field, run the scorer, and
/// verify the resulting parquet row carries the canonical (trimmed)
/// values.
#[tokio::test]
async fn test_scorer_trims_device_model_runtime_fields() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (sk, client_id) = register_and_approve(&state).await?;

    submit_benchmark(
        &state,
        &sk,
        &client_id,
        &json!({
            "benchmark_id": "prefill_throughput_256",
            "device_name": "  test-device  ",
            "device_form_factor": "embedded\n",
            "device_os_name": "\tLinux",
            "device_os_version": " 22.04 ",
            "device_chip_model": "test-chip ",
            "device_gpu_model": " gpu-x ",
            "device_npu_model": " npu-y ",
            "device_ram_bytes": 17_179_869_184i64,
            "model_name": "  llama-3.2-1b\t",
            "model_quant": "\tq4_0 ",
            "model_flags": " --flag ",
            "runtime_name": " llama.cpp ",
            "runtime_version": "  b5000\n",
            "runtime_flags": " -n 32 ",
            "prefill_time_ms": 34.7,
        }),
    )
    .await?;

    score::run_process_submissions(&state.config, build_local_fs_stores(&state.config)?).await?;

    let data_dir = state
        .config
        .storage
        .data_dir()
        .ok_or_else(|| anyhow::anyhow!("expected local_fs config"))?;
    let now = chrono::Utc::now();
    let day_key = format!("{:04}-{:02}-{:02}", now.year(), now.month(), now.day());
    let partition = warehouse::warehouse_day_partition_dir(
        &data_dir.join("warehouse/results"),
        &BenchmarkId::try_new("prefill_throughput_256")?,
        &client_id,
        &day_key,
    );

    let expected: HashMap<&str, &str> = HashMap::from([
        ("device_name", "test-device"),
        ("device_form_factor", "embedded"),
        ("device_os_name", "Linux"),
        ("device_os_version", "22.04"),
        ("device_chip_model", "test-chip"),
        ("device_gpu_model", "gpu-x"),
        ("device_npu_model", "npu-y"),
        ("model_name", "llama-3.2-1b"),
        ("model_quant", "q4_0"),
        ("model_flags", "--flag"),
        ("runtime_name", "llama.cpp"),
        ("runtime_version", "b5000"),
        ("runtime_flags", "-n 32"),
    ]);

    let mut seen = 0usize;
    for entry in std::fs::read_dir(&partition)? {
        let path = entry?.path();
        if path.extension().is_some_and(|e| e == "parquet") {
            let file = std::fs::File::open(&path)?;
            let reader =
                parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder::try_new(file)?
                    .build()?;
            for batch in reader {
                let batch = batch?;
                for (col, expected_val) in &expected {
                    let arr = batch
                        .column_by_name(col)
                        .ok_or_else(|| anyhow::anyhow!("{col} column must exist"))?
                        .as_any()
                        .downcast_ref::<arrow::array::StringArray>()
                        .ok_or_else(|| anyhow::anyhow!("{col} column has unexpected type"))?;
                    for i in 0..batch.num_rows() {
                        assert_eq!(arr.value(i), *expected_val, "{col} row {i} not trimmed",);
                    }
                }
                seen += batch.num_rows();
            }
        }
    }
    assert!(seen > 0, "expected at least one scored row");
    Ok(())
}

#[tokio::test]
async fn test_max_memory_usage_scoring_produces_metrics() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (sk, client_id) = register_and_approve(&state).await?;

    let job_id = submit_benchmark(
        &state,
        &sk,
        &client_id,
        &json!({
            "benchmark_id": "max_memory_usage_256",
            "device_name": "test-device",
            "device_form_factor": "embedded",
            "device_os_name": "Linux",
            "device_os_version": "22.04",
            "device_chip_model": "test-chip",
            "device_ram_bytes": 17179869184i64,
            "model_name": "llama-3.2-1b",
            "model_quant": "q4_0",
            "model_params_total_millions": 1000,
            "runtime_name": "llama.cpp",
            "runtime_version": "b5000",
            "max_host_bytes": 1073741824i64,
            "max_gpu_bytes": 536870912i64,
            "max_npu_bytes": 268435456i64
        }),
    )
    .await?;

    score::run_process_submissions(&state.config, build_local_fs_stores(&state.config)?).await?;

    let resp = authed_get(&state, &sk, &client_id, &format!("/jobs/{job_id}")).await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await?;
    assert_eq!(body["status"], "processed");
    let metrics = body["metrics"]
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("missing metrics array"))?;
    let metric_names: Vec<&str> = metrics
        .iter()
        .map(|m| {
            m["metric"]
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("missing metric name"))
        })
        .collect::<Result<Vec<_>, _>>()?;
    assert!(metric_names.contains(&"max_host_usage"));
    assert!(metric_names.contains(&"max_gpu_usage"));
    assert!(metric_names.contains(&"max_npu_usage"));
    Ok(())
}

/// pipette-clients still emits `max_ram_bytes` / `max_vram_bytes`; this round-trip
/// asserts the legacy names flow through to the new metric names at score time.
#[tokio::test]
async fn test_max_memory_usage_legacy_field_names_still_score() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (sk, client_id) = register_and_approve(&state).await?;

    let job_id = submit_benchmark(
        &state,
        &sk,
        &client_id,
        &json!({
            "benchmark_id": "max_memory_usage_256",
            "device_name": "test-device",
            "device_form_factor": "embedded",
            "device_os_name": "Linux",
            "device_os_version": "22.04",
            "device_chip_model": "test-chip",
            "device_ram_bytes": 17179869184i64,
            "model_name": "llama-3.2-1b",
            "model_quant": "q4_0",
            "model_params_total_millions": 1000,
            "runtime_name": "llama.cpp",
            "runtime_version": "b5000",
            "max_ram_bytes": 1073741824i64,
            "max_vram_bytes": 536870912i64
        }),
    )
    .await?;

    score::run_process_submissions(&state.config, build_local_fs_stores(&state.config)?).await?;

    let resp = authed_get(&state, &sk, &client_id, &format!("/jobs/{job_id}")).await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await?;
    assert_eq!(body["status"], "processed");
    let metric_names: Vec<&str> = body["metrics"]
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("missing metrics array"))?
        .iter()
        .map(|m| {
            m["metric"]
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("missing metric name"))
        })
        .collect::<Result<Vec<_>, _>>()?;
    assert!(metric_names.contains(&"max_host_usage"));
    assert!(metric_names.contains(&"max_gpu_usage"));
    Ok(())
}

#[tokio::test]
async fn test_scoring_moves_submission_to_processed() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (sk, client_id) = register_and_approve(&state).await?;

    let job_id = submit_benchmark(
        &state,
        &sk,
        &client_id,
        &json!({
            "benchmark_id": "prefill_throughput_256",
            "device_name": "test-device",
            "device_form_factor": "embedded",
            "device_os_name": "Linux",
            "device_os_version": "22.04",
            "device_chip_model": "test-chip",
            "device_ram_bytes": 17179869184i64,
            "model_name": "llama-3.2-1b",
            "model_quant": "q4_0",
            "model_params_total_millions": 1000,
            "runtime_name": "llama.cpp",
            "runtime_version": "b5000",
            "prefill_time_ms": 34.7
        }),
    )
    .await?;

    let data_dir = state
        .config
        .storage
        .data_dir()
        .ok_or_else(|| anyhow::anyhow!("expected local_fs config"))?;
    let incoming_path = data_dir
        .join("submissions/incoming")
        .join(format!("{job_id}.json"));
    let expected = std::fs::read_to_string(&incoming_path)?;

    score::run_process_submissions(&state.config, build_local_fs_stores(&state.config)?).await?;

    assert!(!incoming_path.exists());

    let processed_path = data_dir
        .join("submissions/processed")
        .join(format!("{job_id}.json.gz"));
    assert!(processed_path.exists());
    let f = std::fs::File::open(&processed_path)?;
    let mut decoder = flate2::read::GzDecoder::new(f);
    let mut actual = String::new();
    std::io::Read::read_to_string(&mut decoder, &mut actual)?;
    assert_eq!(actual, expected);
    Ok(())
}

// ---------------------------------------------------------------------------
// Eval scoring (with mock evals server)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_eval_scoring_produces_accuracy_and_eval_sample_results() -> anyhow::Result<()> {
    let evals_url = start_mock_evals_server().await?;

    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state_with_evals_url(dir.path(), &evals_url).await?;
    let (sk, client_id) = register_and_approve(&state).await?;

    let job_id = submit_benchmark(
        &state,
        &sk,
        &client_id,
        &json!({
            "benchmark_id": "eval_test",
            "device_name": "test-device",
            "device_form_factor": "embedded",
            "device_os_name": "Linux",
            "device_os_version": "22.04",
            "device_chip_model": "test-chip",
            "device_ram_bytes": 17179869184i64,
            "model_name": "llama-3.2-1b",
            "model_quant": "q4_0",
            "model_params_total_millions": 1000,
            "runtime_name": "llama.cpp",
            "runtime_version": "b5000",
            "completions": [
                {"id": "s1", "completion": "4"},
                {"id": "s2", "completion": "London"},
                {"id": "s3", "completion": "blue"}
            ]
        }),
    )
    .await?;

    run_full_score(&state.config).await?;

    // Verify warehouse accuracy metric
    let resp = authed_get(&state, &sk, &client_id, &format!("/jobs/{job_id}")).await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await?;
    assert_eq!(body["status"], "processed");
    assert_eq!(body["score_runtime_version"], "mock-v1.0.0");
    let metrics = body["metrics"]
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("missing metrics array"))?;
    assert_eq!(metrics.len(), 1);
    assert_eq!(metrics[0]["metric"], "accuracy");
    assert_eq!(metrics[0]["unit"], "ratio");
    assert!(metrics[0].get("value_stddev").is_some());
    assert!(metrics[0]["value_stddev"].is_null());
    let accuracy = metrics[0]["value"]
        .as_f64()
        .ok_or_else(|| anyhow::anyhow!("missing accuracy value"))?;
    assert!((accuracy - 0.6667).abs() < 0.01);

    // Verify eval sample results
    let resp = authed_get(
        &state,
        &sk,
        &client_id,
        &format!("/jobs/{job_id}/eval-sample-results"),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await?;
    let samples = body
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("expected samples array"))?;
    assert_eq!(samples.len(), 3);

    let by_id: HashMap<&str, &serde_json::Value> = samples
        .iter()
        .map(|s| {
            Ok((
                s["id"]
                    .as_str()
                    .ok_or_else(|| anyhow::anyhow!("missing sample id"))?,
                s,
            ))
        })
        .collect::<anyhow::Result<HashMap<_, _>>>()?;

    // s1: correct, prompt "What is 2+2?", completion "4"
    let s1 = by_id["s1"];
    assert_eq!(s1["completion"], "4");
    assert_eq!(s1["is_correct"], true);
    assert_eq!(s1["messages"][0]["role"], "user");
    assert_eq!(s1["messages"][0]["content"], "What is 2+2?");

    // s2: incorrect
    let s2 = by_id["s2"];
    assert_eq!(s2["completion"], "London");
    assert_eq!(s2["is_correct"], false);

    // s3: correct
    let s3 = by_id["s3"];
    assert_eq!(s3["completion"], "blue");
    assert_eq!(s3["is_correct"], true);

    Ok(())
}

#[tokio::test]
async fn test_malformed_submission_on_disk_stays_in_incoming() -> anyhow::Result<()> {
    // Simulate schema drift: a submission on disk that the current Submission
    // deserializer cannot parse. Behavior under review: stay in incoming/ and
    // let the next cron retry. No dead-letter path.
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (_sk, client_id) = register_and_approve(&state).await?;

    let job_id = "poison-pill-job";
    let key = job(job_id);
    // Body missing required field `device_form_factor`, etc.
    let bad_body = json!({
        "benchmark_id": "prefill_throughput_256",
        "client_id": client_id.as_str(),
        "job_id": job_id,
        "submitted_at": "2026-01-01T00:00:00Z",
    });
    pipette_mgmt::stores::build_local_fs_stores(&state.config)?
        .submissions
        .write_incoming(&key, &bad_body)
        .await?;

    // Run scoring — should not crash; the bad submission is counted as failed.
    score::run_process_submissions(&state.config, build_local_fs_stores(&state.config)?).await?;

    // File should still be in incoming/ (behavior preserved: no dead-letter).
    let stores = pipette_mgmt::stores::build_local_fs_stores(&state.config)?;
    let incoming = stores.submissions.list_incoming(TEST_LIST_LIMIT).await?;
    assert!(
        incoming.iter().any(|k| k.as_str() == job_id),
        "malformed submission should remain in incoming/",
    );
    Ok(())
}

#[tokio::test]
async fn test_eval_scoring_fails_on_malformed_upstream_score_response() -> anyhow::Result<()> {
    let evals_url = start_mock_evals_server_malformed_score().await?;

    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state_with_evals_url(dir.path(), &evals_url).await?;
    let (sk, client_id) = register_and_approve(&state).await?;

    let job_id = submit_benchmark(
        &state,
        &sk,
        &client_id,
        &json!({
            "benchmark_id": "eval_test",
            "device_name": "test-device",
            "device_form_factor": "embedded",
            "device_os_name": "Linux",
            "device_os_version": "22.04",
            "device_chip_model": "test-chip",
            "device_ram_bytes": 17179869184i64,
            "model_name": "llama-3.2-1b",
            "model_quant": "q4_0",
            "model_params_total_millions": 1000,
            "runtime_name": "llama.cpp",
            "runtime_version": "b5000",
            "completions": [
                {"id": "s1", "completion": "4"},
            ]
        }),
    )
    .await?;

    // Route -> eval scoring (malformed response fails) -> finalize (nothing).
    run_full_score(&state.config).await?;

    // The eval was routed out of incoming, but scoring failed on the malformed
    // response, so it stays parked in score-queue/to_do — never finalized.
    let stores = build_local_fs_stores(&state.config)?;
    let to_do = stores
        .submissions
        .list_queue(ScoreQueueStage::ToDo, TEST_LIST_LIMIT)
        .await?;
    assert_eq!(
        to_do.iter().map(|j| j.as_str()).collect::<Vec<_>>(),
        vec![job_id.as_str()],
        "malformed eval should remain in score-queue/to_do for retry",
    );
    Ok(())
}

/// When the scoring service is unreachable, the run must not fail and must
/// leave eval submissions in `incoming/` so the next invocation retries them
/// — rather than burning a timeout per submission or marking them failed.
#[tokio::test]
async fn test_eval_scoring_skips_when_service_down() -> anyhow::Result<()> {
    // Port 1 is privileged and unbound: connection is refused immediately,
    // so the run pauses fast instead of waiting on a timeout.
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state_with_evals_url(dir.path(), "http://127.0.0.1:1").await?;
    let (sk, client_id) = register_and_approve(&state).await?;

    let body = json!({
        "benchmark_id": "eval_test",
        "device_name": "test-device",
        "device_form_factor": "embedded",
        "device_os_name": "Linux",
        "device_os_version": "22.04",
        "device_chip_model": "test-chip",
        "device_ram_bytes": 17179869184i64,
        "model_name": "llama-3.2-1b",
        "model_quant": "q4_0",
        "model_params_total_millions": 1000,
        "runtime_name": "llama.cpp",
        "runtime_version": "b5000",
        "completions": [
            {"id": "s1", "completion": "4"},
        ]
    });
    let job_ids: Vec<String> = futures::future::try_join_all(
        (0..2).map(|_| submit_benchmark(&state, &sk, &client_id, &body)),
    )
    .await?;

    // Both runs must succeed (the crons exit 0 and retry on their schedule):
    // the fast pass routes the evals into to_do, the slow pass pauses when the
    // service is unreachable, leaving them in to_do.
    score::run_process_submissions(&state.config, build_local_fs_stores(&state.config)?).await?;
    score::run_score_eval(&state.config, build_local_fs_stores(&state.config)?).await?;

    // Every eval submission stays in score-queue/to_do for the next invocation
    // — none was finalized or marked processed.
    let stores = build_local_fs_stores(&state.config)?;
    let to_do: std::collections::HashSet<String> = stores
        .submissions
        .list_queue(ScoreQueueStage::ToDo, TEST_LIST_LIMIT)
        .await?
        .iter()
        .map(|j| j.as_str().to_string())
        .collect();
    for job_id in &job_ids {
        assert!(
            to_do.contains(job_id),
            "submission {job_id} should remain in score-queue/to_do when service is down",
        );
    }
    Ok(())
}

#[tokio::test]
async fn test_eval_sample_results_404_for_non_eval_job() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (sk, client_id) = register_and_approve(&state).await?;

    let job_id = submit_benchmark(
        &state,
        &sk,
        &client_id,
        &json!({
            "benchmark_id": "prefill_throughput_256",
            "device_name": "test-device",
            "device_form_factor": "embedded",
            "device_os_name": "Linux",
            "device_os_version": "22.04",
            "device_chip_model": "test-chip",
            "device_ram_bytes": 17179869184i64,
            "model_name": "m",
            "model_quant": "q",
            "model_params_total_millions": 100,
            "runtime_name": "rt",
            "runtime_version": "v1",
            "prefill_time_ms": 34.7
        }),
    )
    .await?;

    score::run_process_submissions(&state.config, build_local_fs_stores(&state.config)?).await?;

    let resp = authed_get(
        &state,
        &sk,
        &client_id,
        &format!("/jobs/{job_id}/eval-sample-results"),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    Ok(())
}

#[tokio::test]
async fn test_eval_sample_results_404_for_incoming_job() -> anyhow::Result<()> {
    let evals_url = start_mock_evals_server().await?;

    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state_with_evals_url(dir.path(), &evals_url).await?;
    let (sk, client_id) = register_and_approve(&state).await?;

    let job_id = submit_benchmark(
        &state,
        &sk,
        &client_id,
        &json!({
            "benchmark_id": "eval_test",
            "device_name": "test-device",
            "device_form_factor": "embedded",
            "device_os_name": "Linux",
            "device_os_version": "22.04",
            "device_chip_model": "test-chip",
            "device_ram_bytes": 17179869184i64,
            "model_name": "m",
            "model_quant": "q",
            "model_params_total_millions": 100,
            "runtime_name": "rt",
            "runtime_version": "v1",
            "completions": [
                {"id": "s1", "completion": "4"},
                {"id": "s2", "completion": "Paris"},
                {"id": "s3", "completion": "blue"}
            ]
        }),
    )
    .await?;

    // Don't score — job stays incoming
    let resp = authed_get(
        &state,
        &sk,
        &client_id,
        &format!("/jobs/{job_id}/eval-sample-results"),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    Ok(())
}

/// Legacy submissions written before gateway-side duplicate-id rejection may
/// still sit in `incoming/` with duplicate completion ids. The scorer must
/// dedupe (first occurrence wins) before posting to `/score` so the job can
/// drain instead of failing forever.
#[tokio::test]
async fn test_scorer_dedupes_duplicate_completion_ids_in_legacy_submission() -> anyhow::Result<()> {
    let evals_url = start_mock_evals_server().await?;

    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state_with_evals_url(dir.path(), &evals_url).await?;
    let (sk, client_id) = register_and_approve(&state).await?;

    // Bypass the HTTP handler (which now rejects duplicate ids) and write a
    // legacy submission directly into `incoming/`.
    let job_id = "legacy-dup-job";
    let key = job(job_id);
    let body = json!({
        "message_type": "success",
        "benchmark_id": "eval_test",
        "benchmark_type": "eval",
        "client_id": client_id.as_str(),
        "job_id": job_id,
        "submitted_at": "2026-01-01T00:00:00Z",
        "device_name": "test-device",
        "device_form_factor": "embedded",
        "device_os_name": "Linux",
        "device_os_version": "22.04",
        "device_chip_model": "test-chip",
        "device_ram_bytes": 17_179_869_184i64,
        "model_name": "llama-3.2-1b",
        "model_quant": "q4_0",
        "model_params_total_millions": 1000,
        "runtime_name": "llama.cpp",
        "runtime_version": "b5000",
        "completions": [
            {"id": "s1", "completion": "first"},
            {"id": "s2", "completion": "London"},
            // duplicate of s1 — the safety net should drop it
            {"id": "s1", "completion": "second"},
            {"id": "s3", "completion": "blue"},
        ]
    });
    build_local_fs_stores(&state.config)?
        .submissions
        .write_incoming(&key, &body)
        .await?;

    run_full_score(&state.config).await?;

    let resp = authed_get(&state, &sk, &client_id, &format!("/jobs/{job_id}")).await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await?;
    assert_eq!(body["status"], "processed");

    // Per-sample results should have one row per unique id (3, not 4) and the
    // surviving s1 row should keep the *first* completion string.
    let resp = authed_get(
        &state,
        &sk,
        &client_id,
        &format!("/jobs/{job_id}/eval-sample-results"),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await?;
    let samples = body
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("expected samples array"))?;
    assert_eq!(samples.len(), 3, "duplicate id should be dropped");

    let s1 = samples
        .iter()
        .find(|s| s["id"].as_str() == Some("s1"))
        .ok_or_else(|| anyhow::anyhow!("missing s1"))?;
    assert_eq!(
        s1["completion"].as_str(),
        Some("first"),
        "first occurrence should win"
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// VL throughput scoring with a multi-artifact model_descriptor
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_vl_throughput_scoring_canonicalizes_model_descriptor() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (sk, client_id) = register_and_approve(&state).await?;

    // model_descriptor sent as a JSON *string* with keys deliberately out of order
    // and whitespace — the ingest path must parse it, canonicalize (sort keys,
    // strip whitespace), and store the canonical string.
    let model_descriptor_wire = r#"{ "type": "hf_gguf_vision", "repo_name": "LFM2.5-VL-450M-GGUF", "org": "LiquidAI", "filename": "LFM2.5-VL-450M-Q4_0.gguf", "mmproj_filename": "mmproj-f16.gguf" }"#;
    // Nested and out of order, so the assertion below proves keys are sorted at
    // every level rather than only the top one.
    let benchmark_flags_wire = r#"{ "readiness": { "skip_thermal": true, "max_wait_secs": 300 }, "http_timeout_seconds": 1800 }"#;
    let job_id = submit_benchmark(
        &state,
        &sk,
        &client_id,
        &json!({
            "benchmark_id": "vl_throughput_384x384_32_64",
            "device_name": "test-device",
            "device_form_factor": "embedded",
            "device_os_name": "Linux",
            "device_os_version": "22.04",
            "device_chip_model": "test-chip",
            "device_ram_bytes": 17179869184i64,
            "model_name": "LiquidAI/LFM2.5-VL-450M-GGUF",
            "model_quant": "Q4_0",
            "model_params_total_millions": 450,
            "model_descriptor": model_descriptor_wire,
            "benchmark_flags": benchmark_flags_wire,
            // `model_flags` is JSON here, so it canonicalizes like the fields
            // above; `runtime_flags` is the plain-string spelling the field is
            // documented to accept, which must survive untouched but trimmed.
            "model_flags": r#"{ "enable_thinking": true, "cache": false }"#,
            "runtime_flags": "  --n-gpu-layers 999  ",
            "runtime_name": "llama.cpp",
            "runtime_version": "b8683",
            "prompt_tokens": 75,
            "prompt_ms": 352.3,
            "prompt_ms_stddev": 3.8,
            "predicted_ms": 32.7,
            "predicted_ms_stddev": 1.5
        }),
    )
    .await?;

    score::run_process_submissions(&state.config, build_local_fs_stores(&state.config)?).await?;

    // Verify metrics via API
    let resp = authed_get(&state, &sk, &client_id, &format!("/jobs/{job_id}")).await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await?;
    assert_eq!(body["status"], "processed");
    let metrics = body["metrics"]
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("missing metrics array"))?;
    let metric_names: Vec<&str> = metrics
        .iter()
        .map(|m| {
            m["metric"]
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("missing metric name"))
        })
        .collect::<Result<Vec<_>, _>>()?;
    assert!(metric_names.contains(&"ttft"));
    assert!(metric_names.contains(&"prefill_throughput"));
    assert!(metric_names.contains(&"decode_throughput"));
    assert!(metric_names.contains(&"e2e_latency"));

    // Verify model_descriptor round-trips through the warehouse Parquet in canonical
    // form: keys sorted lexicographically, no whitespace.
    let expected_canonical = concat!(
        r#"{"filename":"LFM2.5-VL-450M-Q4_0.gguf","mmproj_filename":"mmproj-f16.gguf","#,
        r#""org":"LiquidAI","repo_name":"LFM2.5-VL-450M-GGUF","type":"hf_gguf_vision"}"#
    );
    // model_descriptor_sha256 is derived mgmt-side as the sha256 of the
    // *canonical* descriptor string (not the raw wire bytes).
    let expected_sha = pipette_mgmt::canonical_json::sha256_hex(expected_canonical);
    // The harness configuration takes the same treatment — canonical form and a
    // sha256 of *that*, so two runs measured the same way group together
    // however each client happened to format its payload.
    let expected_flags_canonical = concat!(
        r#"{"http_timeout_seconds":1800,"readiness":"#,
        r#"{"max_wait_secs":300,"skip_thermal":true}}"#
    );
    let expected_flags_sha = pipette_mgmt::canonical_json::sha256_hex(expected_flags_canonical);
    // `model_flags` / `runtime_flags` get the same canonical-plus-hash
    // treatment, except that the non-JSON spelling is passed through trimmed
    // rather than rejected or mangled.
    let expected_model_flags = r#"{"cache":false,"enable_thinking":true}"#;
    let expected_model_flags_sha = pipette_mgmt::canonical_json::sha256_hex(expected_model_flags);
    let expected_runtime_flags = "--n-gpu-layers 999";
    let expected_runtime_flags_sha =
        pipette_mgmt::canonical_json::sha256_hex(expected_runtime_flags);
    let data_dir = state
        .config
        .storage
        .data_dir()
        .ok_or_else(|| anyhow::anyhow!("expected local_fs config"))?;
    let warehouse_dir = data_dir.join("warehouse/results");
    let partition = warehouse::warehouse_day_partition_dir(
        &warehouse_dir,
        &BenchmarkId::try_new("vl_throughput_384x384_32_64")?,
        &client_id,
        &chrono::Utc::now().format("%Y-%m-%d").to_string(),
    );
    assert!(partition.exists());

    for entry in std::fs::read_dir(&partition)? {
        let path = entry?.path();
        if path.extension().is_some_and(|e| e == "parquet") {
            let file = std::fs::File::open(&path)?;
            let mut reader =
                parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder::try_new(file)?
                    .build()?;
            reader.try_for_each(|batch| -> anyhow::Result<()> {
                let batch = batch?;
                let str_col = |name: &str| -> anyhow::Result<arrow::array::StringArray> {
                    Ok(batch
                        .column_by_name(name)
                        .ok_or_else(|| anyhow::anyhow!("{name} column must exist"))?
                        .as_any()
                        .downcast_ref::<arrow::array::StringArray>()
                        .ok_or_else(|| anyhow::anyhow!("{name} column has unexpected type"))?
                        .clone())
                };
                let descriptor = str_col("model_descriptor")?;
                let sha = str_col("model_descriptor_sha256")?;
                let flags = str_col("benchmark_flags")?;
                let flags_sha = str_col("benchmark_flags_sha256")?;
                let model_flags = str_col("model_flags")?;
                let model_flags_sha = str_col("model_flags_sha256")?;
                let runtime_flags = str_col("runtime_flags")?;
                let runtime_flags_sha = str_col("runtime_flags_sha256")?;
                (0..batch.num_rows()).for_each(|i| {
                    assert!(!descriptor.is_null(i));
                    assert_eq!(descriptor.value(i), expected_canonical);
                    assert!(!sha.is_null(i));
                    assert_eq!(sha.value(i), expected_sha.as_str());
                    assert!(!flags.is_null(i));
                    assert_eq!(flags.value(i), expected_flags_canonical);
                    assert!(!flags_sha.is_null(i));
                    assert_eq!(flags_sha.value(i), expected_flags_sha.as_str());
                    assert!(!model_flags.is_null(i));
                    assert_eq!(model_flags.value(i), expected_model_flags);
                    assert!(!model_flags_sha.is_null(i));
                    assert_eq!(model_flags_sha.value(i), expected_model_flags_sha.as_str());
                    assert!(!runtime_flags.is_null(i));
                    assert_eq!(runtime_flags.value(i), expected_runtime_flags);
                    assert!(!runtime_flags_sha.is_null(i));
                    assert_eq!(
                        runtime_flags_sha.value(i),
                        expected_runtime_flags_sha.as_str()
                    );
                });
                Ok(())
            })?;
        }
    }

    Ok(())
}

/// Submit one `prefill_throughput` run, score it, and return the warehouse
/// partition it landed in. `extra` holds the fields to add to the standard
/// body — the ones under test.
///
/// The returned `TempDir` owns the data directory the partition lives in, so a
/// caller must hold it for as long as it reads the partition.
async fn score_one_submission(
    extra: &[(&str, serde_json::Value)],
) -> anyhow::Result<(tempfile::TempDir, std::path::PathBuf)> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (sk, client_id) = register_and_approve(&state).await?;

    let mut body = json!({
        "benchmark_id": "prefill_throughput_256",
        "device_name": "test-device",
        "device_form_factor": "embedded",
        "device_os_name": "Linux",
        "device_os_version": "22.04",
        "device_chip_model": "test-chip",
        "device_ram_bytes": 17179869184i64,
        "model_name": "m",
        "model_quant": "q",
        "runtime_name": "rt",
        "runtime_version": "v1",
        "prefill_time_ms": 10.0
    });
    body.as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("body is not a JSON object"))?
        .extend(
            extra
                .iter()
                .map(|(field, value)| ((*field).to_string(), value.clone())),
        );

    submit_benchmark(&state, &sk, &client_id, &body).await?;
    score::run_process_submissions(&state.config, build_local_fs_stores(&state.config)?).await?;

    let data_dir = state
        .config
        .storage
        .data_dir()
        .ok_or_else(|| anyhow::anyhow!("expected local_fs config"))?;
    let partition = warehouse::warehouse_day_partition_dir(
        &data_dir.join("warehouse/results"),
        &BenchmarkId::try_new("prefill_throughput_256")?,
        &client_id,
        &chrono::Utc::now().format("%Y-%m-%d").to_string(),
    );
    assert!(partition.exists(), "expected a scored partition");
    Ok((dir, partition))
}

/// What reaches the warehouse for each shape of `benchmark_flags`.
///
/// A top-level empty object carries no information an absent field does not, so
/// it collapses to NULL — otherwise "nothing reported" would occupy two
/// distinct `benchmark_flags_sha256` buckets and split every group-by over it.
/// A *nested* empty object is not the same claim ("I have a readiness block and
/// it is empty"), so it is stored. Anything populated round-trips in canonical
/// form with a hash of that form.
#[rstest]
#[case::top_level_empty("{ }", None)]
#[case::nested_empty(r#"{ "readiness": { } }"#, Some(r#"{"readiness":{}}"#))]
#[case::populated(
    r#"{ "readiness": { "skip_thermal": true, "max_wait_secs": 300 } }"#,
    Some(r#"{"readiness":{"max_wait_secs":300,"skip_thermal":true}}"#)
)]
#[tokio::test]
async fn test_benchmark_flags_reaches_the_warehouse(
    #[case] wire: &str,
    #[case] expected: Option<&str>,
) -> anyhow::Result<()> {
    let (_dir, partition) = score_one_submission(&[("benchmark_flags", json!(wire))]).await?;

    let expected_sha = expected.map(pipette_mgmt::canonical_json::sha256_hex);
    let checked = assert_string_column(&partition, "benchmark_flags", expected)?;
    assert_string_column(
        &partition,
        "benchmark_flags_sha256",
        expected_sha.as_deref(),
    )?;
    assert!(checked > 0, "expected at least one scored row");
    Ok(())
}

/// `client_version` reaches every warehouse row of a submission, and stays
/// NULL for a client that doesn't report it — so "which harness build produced
/// this number" is a column rather than something to infer from the run date.
#[rstest]
#[case::reported(Some("0.14.2"))]
#[case::not_reported(None)]
#[tokio::test]
async fn test_client_version_reaches_the_warehouse(
    #[case] wire: Option<&str>,
) -> anyhow::Result<()> {
    let extra: Vec<_> = wire
        .map(|v| ("client_version", json!(v)))
        .into_iter()
        .collect();
    let (_dir, partition) = score_one_submission(&extra).await?;

    let checked = assert_string_column(&partition, "client_version", wire)?;
    assert!(checked > 0, "expected at least one scored row");
    Ok(())
}

/// The per-run swap / host memory peaks reach every warehouse row of a
/// submission, and stay NULL for a client that doesn't measure them. A
/// `prefill_throughput` body carries them here: every benchmark type reports
/// them, so the scorer applies no per-type gate.
#[rstest]
#[case::reported(Some(6_442_450_944), Some(12_884_901_888))]
#[case::not_reported(None, None)]
#[tokio::test]
async fn test_observed_memory_reaches_the_warehouse(
    #[case] swap: Option<i64>,
    #[case] host: Option<i64>,
) -> anyhow::Result<()> {
    let extra: Vec<_> = [
        ("observation_max_swap_bytes", swap),
        ("observation_max_host_bytes", host),
    ]
    .into_iter()
    .filter_map(|(field, value)| Some((field, json!(value?))))
    .collect();
    let (_dir, partition) = score_one_submission(&extra).await?;

    let checked = assert_i64_column(&partition, "observation_max_swap_bytes", swap)?;
    assert_i64_column(&partition, "observation_max_host_bytes", host)?;
    assert!(checked > 0, "expected at least one scored row");
    Ok(())
}

// ---------------------------------------------------------------------------
// model_params backfill in the scorer
// ---------------------------------------------------------------------------

/// A submission written to disk before ``model_params_total_millions` was a required
/// field still parses (the field is `Option<i32>` on `Submission`). When the
/// scorer hits a row with no value, it falls back to the
/// `model_name → mill_params` lookup so the warehouse row is populated even
/// though the submitter never sent the field.
#[tokio::test]
async fn test_scorer_backfills_model_params_from_lookup() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    setup_models(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (_sk, client_id) = register_and_approve(&state).await?;

    // Bypass the HTTP handler (which would reject a body without
    // model_params_total_millions) and write a pre-feature submission directly.
    let job_id = "legacy-job";
    let key = job(job_id);
    let body = json!({
        "message_type": "success",
        "benchmark_id": "prefill_throughput_256",
        "benchmark_type": "prefill_throughput",
        "client_id": client_id.as_str(),
        "job_id": job_id,
        "submitted_at": "2026-01-01T00:00:00Z",
        "device_name": "test-device",
        "device_form_factor": "embedded",
        "device_os_name": "Linux",
        "device_os_version": "22.04",
        "device_chip_model": "test-chip",
        "device_ram_bytes": 17_179_869_184i64,
        "model_name": "LiquidAI/LFM2-700M",
        "model_quant": "q4_0",
        "runtime_name": "llama.cpp",
        "runtime_version": "b5000",
        "prefill_time_ms": 34.7
    });
    build_local_fs_stores(&state.config)?
        .submissions
        .write_incoming(&key, &body)
        .await?;

    score::run_process_submissions(&state.config, build_local_fs_stores(&state.config)?).await?;

    let data_dir = state
        .config
        .storage
        .data_dir()
        .ok_or_else(|| anyhow::anyhow!("expected local_fs config"))?;
    let partition = warehouse::warehouse_day_partition_dir(
        &data_dir.join("warehouse/results"),
        &BenchmarkId::try_new("prefill_throughput_256")?,
        &client_id,
        "2026-01-01",
    );
    let mut found_value = None;
    for entry in std::fs::read_dir(&partition)? {
        let path = entry?.path();
        if path.extension().is_some_and(|e| e == "parquet") {
            let file = std::fs::File::open(&path)?;
            let reader =
                parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder::try_new(file)?
                    .build()?;
            for batch in reader {
                let batch = batch?;
                let col = batch
                    .column_by_name("model_params_total_millions")
                    .ok_or_else(|| {
                        anyhow::anyhow!("model_params_total_millions column must exist")
                    })?;
                let arr = col
                    .as_any()
                    .downcast_ref::<arrow::array::Int32Array>()
                    .ok_or_else(|| {
                        anyhow::anyhow!("unexpected type for model_params_total_millions")
                    })?;
                for i in 0..batch.num_rows() {
                    assert!(
                        !arr.is_null(i),
                        "expected scorer fallback to fill the column"
                    );
                    found_value = Some(arr.value(i));
                }
            }
        }
    }
    assert_eq!(
        found_value,
        Some(700),
        "fallback should resolve LFM2-700M to 700"
    );
    Ok(())
}

/// A submission that already has a `model_params_total_millions` value but for a
/// known model where the lookup says something different — the scorer
/// should still rewrite it to the canonical value at score time. This
/// covers data written by older code paths that didn't normalize at ingress.
#[tokio::test]
async fn test_scorer_normalizes_wrong_model_params_for_known_model() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    setup_models(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (_sk, client_id) = register_and_approve(&state).await?;

    let job_id = "wrong-value-job";
    let key = job(job_id);
    let body = json!({
        "message_type": "success",
        "benchmark_id": "prefill_throughput_256",
        "benchmark_type": "prefill_throughput",
        "client_id": client_id.as_str(),
        "job_id": job_id,
        "submitted_at": "2026-01-01T00:00:00Z",
        "device_name": "test-device",
        "device_form_factor": "embedded",
        "device_os_name": "Linux",
        "device_os_version": "22.04",
        "device_chip_model": "test-chip",
        "device_ram_bytes": 17_179_869_184i64,
        "model_name": "LiquidAI/LFM2-700M",
        "model_quant": "q4_0",
        "model_params_total_millions": 999, // wrong value, should be normalized to 700
        "runtime_name": "llama.cpp",
        "runtime_version": "b5000",
        "prefill_time_ms": 34.7
    });
    build_local_fs_stores(&state.config)?
        .submissions
        .write_incoming(&key, &body)
        .await?;

    score::run_process_submissions(&state.config, build_local_fs_stores(&state.config)?).await?;

    let data_dir = state
        .config
        .storage
        .data_dir()
        .ok_or_else(|| anyhow::anyhow!("expected local_fs config"))?;
    let partition = warehouse::warehouse_day_partition_dir(
        &data_dir.join("warehouse/results"),
        &BenchmarkId::try_new("prefill_throughput_256")?,
        &client_id,
        "2026-01-01",
    );
    let mut found_value = None;
    for entry in std::fs::read_dir(&partition)? {
        let path = entry?.path();
        if path.extension().is_some_and(|e| e == "parquet") {
            let file = std::fs::File::open(&path)?;
            let reader =
                parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder::try_new(file)?
                    .build()?;
            for batch in reader {
                let batch = batch?;
                let arr = batch
                    .column_by_name("model_params_total_millions")
                    .ok_or_else(|| {
                        anyhow::anyhow!("model_params_total_millions column must exist")
                    })?
                    .as_any()
                    .downcast_ref::<arrow::array::Int32Array>()
                    .ok_or_else(|| anyhow::anyhow!("unexpected type"))?;
                for i in 0..batch.num_rows() {
                    found_value = Some(arr.value(i));
                }
            }
        }
    }
    assert_eq!(
        found_value,
        Some(700),
        "scorer should have overridden 999 → 700"
    );
    Ok(())
}

/// The scorer resolves the curated `(total, active)` for a known model —
/// overriding the client's wrong values — regardless of how the model is
/// identified: by `model_name`, by the opaque `model_descriptor` (no
/// `model_name`), or by the descriptor when `model_name` is present but not in
/// the catalog. `setup_models` maps `LFM2-8B-A1B` to total 8340 / active 1500.
#[rstest]
#[case::via_model_name(json!({"model_name": "LiquidAI/LFM2-8B-A1B", "model_quant": "q4_0"}))]
#[case::via_descriptor(
    json!({"model_descriptor": "{\"repo_name\":\"LFM2-8B-A1B-GGUF\",\"type\":\"hf_gguf_text\"}"})
)]
#[case::via_descriptor_when_name_unknown(json!({
    "model_name": "unlisted-alias",
    "model_descriptor": "{\"repo_name\":\"LFM2-8B-A1B-GGUF\",\"type\":\"hf_gguf_text\"}"
}))]
#[tokio::test]
async fn test_scorer_resolves_moe_params_from_catalog(
    #[case] identity: serde_json::Value,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    setup_models(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (sk, client_id) = register_and_approve(&state).await?;

    // Client sends wrong (9999) params to exercise the catalog override; the
    // per-case `identity` fields select how the model is recognized.
    let mut body = json!({
        "benchmark_id": "prefill_throughput_256",
        "device_name": "test-device",
        "device_form_factor": "embedded",
        "device_os_name": "Linux",
        "device_os_version": "22.04",
        "device_chip_model": "test-chip",
        "device_ram_bytes": 17179869184i64,
        "model_params_total_millions": 9999,
        "model_params_active_millions": 9999,
        "runtime_name": "llama.cpp",
        "runtime_version": "b5000",
        "prefill_time_ms": 34.7
    });
    {
        let obj = body
            .as_object_mut()
            .ok_or_else(|| anyhow::anyhow!("body is a JSON object"))?;
        let ident = identity
            .as_object()
            .ok_or_else(|| anyhow::anyhow!("identity is a JSON object"))?;
        ident.iter().for_each(|(k, v)| {
            obj.insert(k.clone(), v.clone());
        });
    }

    let _job_id = submit_benchmark(&state, &sk, &client_id, &body).await?;
    score::run_process_submissions(&state.config, build_local_fs_stores(&state.config)?).await?;

    let data_dir = state
        .config
        .storage
        .data_dir()
        .ok_or_else(|| anyhow::anyhow!("expected local_fs config"))?;
    let partition = warehouse::warehouse_day_partition_dir(
        &data_dir.join("warehouse/results"),
        &BenchmarkId::try_new("prefill_throughput_256")?,
        &client_id,
        &chrono::Utc::now().format("%Y-%m-%d").to_string(),
    );

    let mut total_seen = None;
    let mut active_seen = None;
    std::fs::read_dir(&partition)?
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "parquet"))
        .try_for_each(|path| -> anyhow::Result<()> {
            let file = std::fs::File::open(&path)?;
            let mut reader =
                parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder::try_new(file)?
                    .build()?;
            reader.try_for_each(|batch| -> anyhow::Result<()> {
                let batch = batch?;
                let int_col = |name: &str| -> anyhow::Result<arrow::array::Int32Array> {
                    Ok(batch
                        .column_by_name(name)
                        .ok_or_else(|| anyhow::anyhow!("{name} column must exist"))?
                        .as_any()
                        .downcast_ref::<arrow::array::Int32Array>()
                        .ok_or_else(|| anyhow::anyhow!("{name} column has unexpected type"))?
                        .clone())
                };
                let total = int_col("model_params_total_millions")?;
                let active = int_col("model_params_active_millions")?;
                (0..batch.num_rows()).for_each(|i| {
                    total_seen = Some(total.value(i));
                    active_seen = Some(active.value(i));
                });
                Ok(())
            })
        })?;

    assert_eq!(total_seen, Some(8340), "catalog total");
    assert_eq!(active_seen, Some(1500), "catalog active");
    Ok(())
}

/// When the client omits both `model_params_total_millions` and
/// `model_params_active_millions` and the model is not in the catalog, the
/// warehouse row carries null in both columns — the resolver leaves
/// what it can't fill rather than fabricating a value.
#[tokio::test]
async fn test_scorer_leaves_null_when_neither_catalog_nor_submission_has_values()
-> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    setup_models(dir.path())?; // catalog has LFM2-700M, llama-3.2-1b, LFM2-8B-A1B
    let state = make_state(dir.path()).await?;
    let (sk, client_id) = register_and_approve(&state).await?;

    // model_name is not in the catalog and the body omits both
    // model_params_*_millions fields.
    let _job_id = submit_benchmark(
        &state,
        &sk,
        &client_id,
        &json!({
            "benchmark_id": "prefill_throughput_256",
            "device_name": "test-device",
            "device_form_factor": "embedded",
            "device_os_name": "Linux",
            "device_os_version": "22.04",
            "device_chip_model": "test-chip",
            "device_ram_bytes": 17179869184i64,
            "model_name": "totally-not-in-the-catalog",
            "model_quant": "q4_0",
            "runtime_name": "llama.cpp",
            "runtime_version": "b5000",
            "prefill_time_ms": 34.7
        }),
    )
    .await?;

    score::run_process_submissions(&state.config, build_local_fs_stores(&state.config)?).await?;

    let data_dir = state
        .config
        .storage
        .data_dir()
        .ok_or_else(|| anyhow::anyhow!("expected local_fs config"))?;
    let partition = warehouse::warehouse_day_partition_dir(
        &data_dir.join("warehouse/results"),
        &BenchmarkId::try_new("prefill_throughput_256")?,
        &client_id,
        &chrono::Utc::now().format("%Y-%m-%d").to_string(),
    );

    let mut total_null = false;
    let mut active_null = false;
    for entry in std::fs::read_dir(&partition)? {
        let path = entry?.path();
        if path.extension().is_some_and(|e| e == "parquet") {
            let file = std::fs::File::open(&path)?;
            let reader =
                parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder::try_new(file)?
                    .build()?;
            for batch in reader {
                let batch = batch?;
                let total_arr = batch
                    .column_by_name("model_params_total_millions")
                    .expect("total column must exist");
                let active_arr = batch
                    .column_by_name("model_params_active_millions")
                    .expect("active column must exist");
                for i in 0..batch.num_rows() {
                    if total_arr.is_null(i) {
                        total_null = true;
                    }
                    if active_arr.is_null(i) {
                        active_null = true;
                    }
                }
            }
        }
    }
    assert!(
        total_null,
        "expected at least one null model_params_total_millions"
    );
    assert!(
        active_null,
        "expected at least one null model_params_active_millions"
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Chunked scoring (score_chunk_size)
// ---------------------------------------------------------------------------

/// `run_score` should drain a backlog larger than `score_chunk_size` by
/// pulling chunks in a loop within a single invocation.
#[tokio::test]
async fn test_run_score_drains_backlog_in_chunks() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let mut state = make_state(dir.path()).await?;
    // Force a small chunk size so we exercise the loop with a modest backlog.
    let mut config = (*state.config).clone();
    config.score_chunk_size = std::num::NonZeroUsize::new(2).expect("2 is non-zero");
    state.config = std::sync::Arc::new(config);

    let (sk, client_id) = register_and_approve(&state).await?;

    let backlog = 5usize;
    let mut job_ids = Vec::with_capacity(backlog);
    for _ in 0..backlog {
        let job_id = submit_benchmark(
            &state,
            &sk,
            &client_id,
            &json!({
                "benchmark_id": "prefill_throughput_256",
                "device_name": "test-device",
                "device_form_factor": "embedded",
                "device_os_name": "Linux",
                "device_os_version": "22.04",
                "device_chip_model": "test-chip",
                "device_ram_bytes": 17179869184i64,
                "model_name": "m",
                "model_quant": "q",
                "model_params_total_millions": 100,
                "runtime_name": "rt",
                "runtime_version": "v1",
                "prefill_time_ms": 34.7
            }),
        )
        .await?;
        job_ids.push(job_id);
    }

    score::run_process_submissions(&state.config, build_local_fs_stores(&state.config)?).await?;

    let stores = build_local_fs_stores(&state.config)?;
    assert!(
        stores
            .submissions
            .list_incoming(TEST_LIST_LIMIT)
            .await?
            .is_empty(),
        "all submissions should be drained across chunks",
    );
    for job_id in &job_ids {
        let resp = authed_get(&state, &sk, &client_id, &format!("/jobs/{job_id}")).await?;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await?;
        assert_eq!(
            body["status"], "processed",
            "job {job_id} should be processed"
        );
    }
    Ok(())
}

/// Mixed success/failure across chunks: failed submissions stay in
/// `incoming/` and must be filtered out in subsequent chunks of the same
/// invocation, while well-formed submissions are processed.
#[tokio::test]
async fn test_run_score_chunked_mixed_success_and_failure() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let mut state = make_state(dir.path()).await?;
    let mut config = (*state.config).clone();
    config.score_chunk_size = TEST_CHUNK_SIZE;
    state.config = std::sync::Arc::new(config);

    let (sk, client_id) = register_and_approve(&state).await?;

    // Three well-formed submissions that should score successfully.
    let mut good_job_ids = Vec::new();
    for _ in 0..3 {
        let job_id = submit_benchmark(
            &state,
            &sk,
            &client_id,
            &json!({
                "benchmark_id": "prefill_throughput_256",
                "device_name": "test-device",
                "device_form_factor": "embedded",
                "device_os_name": "Linux",
                "device_os_version": "22.04",
                "device_chip_model": "test-chip",
                "device_ram_bytes": 17179869184i64,
                "model_name": "m",
                "model_quant": "q",
                "model_params_total_millions": 100,
                "runtime_name": "rt",
                "runtime_version": "v1",
                "prefill_time_ms": 34.7
            }),
        )
        .await?;
        good_job_ids.push(job_id);
    }

    // Two poison-pill submissions written directly to the store with bodies
    // missing required fields. They will fail to deserialize on score.
    let stores = build_local_fs_stores(&state.config)?;
    let mut bad_job_ids = Vec::new();
    for i in 0..2 {
        let job_id = format!("poison-{i}");
        let key = job(&job_id);
        let bad_body = json!({
            "benchmark_id": "prefill_throughput_256",
            "client_id": client_id.as_str(),
            "job_id": job_id,
            "submitted_at": "2026-01-01T00:00:00Z",
        });
        stores.submissions.write_incoming(&key, &bad_body).await?;
        bad_job_ids.push(job_id);
    }

    score::run_process_submissions(&state.config, build_local_fs_stores(&state.config)?).await?;

    let stores = build_local_fs_stores(&state.config)?;
    let remaining = stores.submissions.list_incoming(TEST_LIST_LIMIT).await?;
    let remaining_ids: std::collections::HashSet<&str> =
        remaining.iter().map(|k| k.as_str()).collect();
    for job_id in &bad_job_ids {
        assert!(
            remaining_ids.contains(job_id.as_str()),
            "poison submission {job_id} should remain in incoming/"
        );
    }
    for job_id in &good_job_ids {
        assert!(
            !remaining_ids.contains(job_id.as_str()),
            "good submission {job_id} should be processed",
        );
        let resp = authed_get(&state, &sk, &client_id, &format!("/jobs/{job_id}")).await?;
        let body = body_json(resp).await?;
        assert_eq!(body["status"], "processed");
    }
    Ok(())
}

/// A submission that fails to score must be skipped on subsequent chunk
/// iterations within the same `run_score` call so it can't trigger an
/// infinite re-list/re-attempt loop. It stays in `incoming/` for the next
/// invocation.
#[tokio::test]
async fn test_run_score_does_not_loop_on_failed_submissions() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let mut state = make_state(dir.path()).await?;
    // Tiny chunk size — without per-invocation failure tracking, a fully
    // failing chunk would re-list the same poison pill forever.
    let mut config = (*state.config).clone();
    config.score_chunk_size = std::num::NonZeroUsize::MIN;
    state.config = std::sync::Arc::new(config);

    let (_sk, client_id) = register_and_approve(&state).await?;

    let job_id = "poison-pill";
    let key = job(job_id);
    let bad_body = json!({
        "benchmark_id": "prefill_throughput_256",
        "client_id": client_id.as_str(),
        "job_id": job_id,
        "submitted_at": "2026-01-01T00:00:00Z",
    });
    build_local_fs_stores(&state.config)?
        .submissions
        .write_incoming(&key, &bad_body)
        .await?;

    // Must terminate (without the failed-key set this would loop forever).
    score::run_process_submissions(&state.config, build_local_fs_stores(&state.config)?).await?;

    // Failed submission stays in incoming/ for the next invocation.
    let stores = build_local_fs_stores(&state.config)?;
    let incoming = stores.submissions.list_incoming(TEST_LIST_LIMIT).await?;
    assert!(
        incoming.iter().any(|k| k.as_str() == job_id),
        "failed submission should remain in incoming/",
    );
    Ok(())
}

/// End-to-end through the split pipeline, asserting each staged transition:
/// `incoming/` → `to_do` (fast route) → `to_finalize` (slow eval scoring) →
/// `processed/` (fast finalize), with the accuracy metric landing in the
/// warehouse. Exercises the mock scoring service through the slow worker.
#[tokio::test]
async fn test_eval_pipeline_stages_through_score_queue() -> anyhow::Result<()> {
    let evals_url = start_mock_evals_server().await?;

    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state_with_evals_url(dir.path(), &evals_url).await?;
    let (sk, client_id) = register_and_approve(&state).await?;

    let job_id = submit_benchmark(
        &state,
        &sk,
        &client_id,
        &json!({
            "benchmark_id": "eval_test",
            "device_name": "test-device",
            "device_form_factor": "embedded",
            "device_os_name": "Linux",
            "device_os_version": "22.04",
            "device_chip_model": "test-chip",
            "device_ram_bytes": 17179869184i64,
            "model_name": "llama-3.2-1b",
            "model_quant": "q4_0",
            "model_params_total_millions": 1000,
            "runtime_name": "llama.cpp",
            "runtime_version": "b5000",
            "completions": [
                {"id": "s1", "completion": "4"},
                {"id": "s2", "completion": "London"},
                {"id": "s3", "completion": "blue"}
            ]
        }),
    )
    .await?;

    let in_stage = |stage| {
        let config = state.config.clone();
        let job_id = job_id.clone();
        async move {
            let jobs = build_local_fs_stores(&config)?
                .submissions
                .list_queue(stage, TEST_LIST_LIMIT)
                .await?;
            anyhow::Ok(jobs.iter().any(|j| j.as_str() == job_id))
        }
    };
    let in_incoming = || {
        let config = state.config.clone();
        let job_id = job_id.clone();
        async move {
            let jobs = build_local_fs_stores(&config)?
                .submissions
                .list_incoming(TEST_LIST_LIMIT)
                .await?;
            anyhow::Ok(jobs.iter().any(|j| j.as_str() == job_id))
        }
    };

    assert!(in_incoming().await?, "starts in incoming/");

    // Fast pass routes the eval out of incoming/ into to_do.
    score::run_process_submissions(&state.config, build_local_fs_stores(&state.config)?).await?;
    assert!(!in_incoming().await?, "left incoming/ after route");
    assert!(in_stage(ScoreQueueStage::ToDo).await?, "routed to to_do");
    assert!(!in_stage(ScoreQueueStage::ToFinalize).await?);

    // Slow pass calls the scoring service and stages the result for finalize.
    score::run_score_eval(&state.config, build_local_fs_stores(&state.config)?).await?;
    assert!(
        !in_stage(ScoreQueueStage::ToDo).await?,
        "left to_do after scoring"
    );
    assert!(
        in_stage(ScoreQueueStage::ToFinalize).await?,
        "staged in to_finalize after scoring"
    );

    // Fast pass finalizes: warehouse write + archive to processed/.
    score::run_process_submissions(&state.config, build_local_fs_stores(&state.config)?).await?;
    assert!(
        !in_stage(ScoreQueueStage::ToFinalize).await?,
        "left to_finalize"
    );

    let resp = authed_get(&state, &sk, &client_id, &format!("/jobs/{job_id}")).await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await?;
    assert_eq!(body["status"], "processed");
    assert_eq!(body["score_runtime_version"], "mock-v1.0.0");
    let metrics = body["metrics"]
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("missing metrics array"))?;
    assert_eq!(metrics.len(), 1);
    assert_eq!(metrics[0]["metric"], "accuracy");
    let accuracy = metrics[0]["value"]
        .as_f64()
        .ok_or_else(|| anyhow::anyhow!("missing accuracy value"))?;
    assert!((accuracy - 0.6667).abs() < 0.01);
    Ok(())
}

/// A stored eval submission body for `eval_test` with completions the mock
/// scores (s1/s3 correct, s2 wrong). Used to stage `score-queue` entries
/// directly.
fn stored_eval_body(client_id: &str, job_id: &str) -> serde_json::Value {
    json!({
        "message_type": "success",
        "benchmark_id": "eval_test",
        "benchmark_type": "eval",
        "client_id": client_id,
        "job_id": job_id,
        "submitted_at": "2026-01-01T00:00:00Z",
        "device_name": "d",
        "device_form_factor": "embedded",
        "device_os_name": "linux",
        "device_os_version": "22.04",
        "device_chip_model": "chip",
        "device_ram_bytes": 17179869184i64,
        "model_name": "llama-3.2-1b",
        "model_quant": "q4_0",
        "model_params_total_millions": 1000,
        "runtime_name": "llama.cpp",
        "runtime_version": "b5000",
        "completions": [
            {"id": "s1", "completion": "4"},
            {"id": "s2", "completion": "London"},
            {"id": "s3", "completion": "blue"}
        ]
    })
}

/// (Gap 1) Idempotency / re-score skip-guard: a job already staged in
/// `to_finalize` must not be re-sent to the scoring service. The evals URL is
/// dead — if `score-eval` tried to call it, the job would fail and stay in
/// `to_do`; instead the skip-guard clears `to_do` without calling out.
#[tokio::test]
async fn test_score_eval_skips_job_already_in_to_finalize() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state_with_evals_url(dir.path(), "http://127.0.0.1:1").await?;
    let stores = build_local_fs_stores(&state.config)?;
    let job = job("evaljob");

    // Simulate a crash after staging to_finalize but before clearing to_do.
    stores
        .submissions
        .enqueue(
            ScoreQueueStage::ToDo,
            &job,
            &stored_eval_body("c1", "evaljob"),
        )
        .await?;
    stores
        .submissions
        .enqueue(
            ScoreQueueStage::ToFinalize,
            &job,
            &json!({"already": "scored"}),
        )
        .await?;

    score::run_score_eval(&state.config, build_local_fs_stores(&state.config)?).await?;

    // Skip path: to_do cleared without re-scoring; to_finalize left intact.
    assert!(
        stores
            .submissions
            .list_queue(ScoreQueueStage::ToDo, TEST_LIST_LIMIT)
            .await?
            .is_empty()
    );
    assert!(
        stores
            .submissions
            .read_queue(ScoreQueueStage::ToFinalize, &job)
            .await?
            .is_some()
    );
    Ok(())
}

/// (Gap 3) `score-eval` isolates a failing `to_do` job: one job whose
/// benchmark isn't in the catalog fails and stays in `to_do`, while a healthy
/// job is scored and staged for finalize.
#[tokio::test]
async fn test_score_eval_isolates_a_failing_to_do_job() -> anyhow::Result<()> {
    let evals_url = start_mock_evals_server().await?;
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state_with_evals_url(dir.path(), &evals_url).await?;
    let stores = build_local_fs_stores(&state.config)?;

    let good = job("good");
    let bad = job("bad");
    stores
        .submissions
        .enqueue(
            ScoreQueueStage::ToDo,
            &good,
            &stored_eval_body("c1", "good"),
        )
        .await?;
    let mut bad_body = stored_eval_body("c1", "bad");
    bad_body["benchmark_id"] = json!("missing_bench"); // not in the catalog
    stores
        .submissions
        .enqueue(ScoreQueueStage::ToDo, &bad, &bad_body)
        .await?;

    score::run_score_eval(&state.config, build_local_fs_stores(&state.config)?).await?;

    // good advanced to to_finalize and left to_do; bad stayed in to_do.
    assert!(
        stores
            .submissions
            .read_queue(ScoreQueueStage::ToFinalize, &good)
            .await?
            .is_some()
    );
    let to_do: Vec<String> = stores
        .submissions
        .list_queue(ScoreQueueStage::ToDo, TEST_LIST_LIMIT)
        .await?
        .iter()
        .map(|j| j.as_str().to_string())
        .collect();
    assert_eq!(to_do, vec!["bad".to_string()]);
    assert!(
        stores
            .submissions
            .read_queue(ScoreQueueStage::ToFinalize, &bad)
            .await?
            .is_none()
    );
    Ok(())
}

/// (Gap 2) The finalize stage isolates a failing job: a malformed `to_finalize`
/// entry (missing `score`) is deferred while a legitimately-scored job
/// finalizes to `processed/`.
#[tokio::test]
async fn test_finalize_isolates_a_failing_job() -> anyhow::Result<()> {
    let evals_url = start_mock_evals_server().await?;
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state_with_evals_url(dir.path(), &evals_url).await?;
    let (sk, client_id) = register_and_approve(&state).await?;

    // Produce a real to_finalize entry: submit -> route -> score-eval.
    let good_job = submit_benchmark(
        &state,
        &sk,
        &client_id,
        &json!({
            "benchmark_id": "eval_test",
            "device_name": "test-device",
            "device_form_factor": "embedded",
            "device_os_name": "Linux",
            "device_os_version": "22.04",
            "device_chip_model": "test-chip",
            "device_ram_bytes": 17179869184i64,
            "model_name": "llama-3.2-1b",
            "model_quant": "q4_0",
            "model_params_total_millions": 1000,
            "runtime_name": "llama.cpp",
            "runtime_version": "b5000",
            "completions": [
                {"id": "s1", "completion": "4"},
                {"id": "s2", "completion": "London"},
                {"id": "s3", "completion": "blue"}
            ]
        }),
    )
    .await?;
    score::run_process_submissions(&state.config, build_local_fs_stores(&state.config)?).await?;
    score::run_score_eval(&state.config, build_local_fs_stores(&state.config)?).await?;

    // Inject a malformed to_finalize entry (no `score`) that finalize can't process.
    let stores = build_local_fs_stores(&state.config)?;
    let bad = job("badfinal");
    stores
        .submissions
        .enqueue(
            ScoreQueueStage::ToFinalize,
            &bad,
            &json!({"submission": stored_eval_body("c1", "badfinal")}),
        )
        .await?;

    // Finalize pass: good lands in processed/, bad is deferred in to_finalize.
    score::run_process_submissions(&state.config, build_local_fs_stores(&state.config)?).await?;

    let resp = authed_get(&state, &sk, &client_id, &format!("/jobs/{good_job}")).await?;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(body_json(resp).await?["status"], "processed");
    assert!(
        stores
            .submissions
            .read_queue(ScoreQueueStage::ToFinalize, &bad)
            .await?
            .is_some(),
        "malformed finalize entry should be deferred, not lost"
    );
    Ok(())
}
