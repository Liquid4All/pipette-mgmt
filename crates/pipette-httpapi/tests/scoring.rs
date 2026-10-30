//! The scorer: `incoming/` → metrics in the warehouse, one section per
//! benchmark type (`docs/benchmarks.md`). Scoring has no client on the other
//! end, so a wrong number goes unnoticed: these tests assert the values, not
//! just that a row appeared.

use std::collections::{HashMap, HashSet};
use std::io::Write as _;

use anyhow::Context;
use arrow::array::Array;
use axum::http::StatusCode;
use chrono::Datelike;
use futures::TryStreamExt;
use httpmock::Method::POST;
use httpmock::{HttpMockRequest, HttpMockResponse, MockServer};
use rstest::rstest;
use serde_json::json;

use pipette_core::denial::DenialRecord;
use pipette_core::warehouse;
use pipette_httpapi::state::AppState;
use pipette_pipeline::score;
use pipette_plan_types::BenchmarkId;
use pipette_store::ScoreQueueStage;
use pipette_store_fs::build_local_fs_stores;
use pipette_store_testkit::TEST_LIST_LIMIT;
use pipette_web_testkit::{
    authed_get, authed_post, body_json, job, make_state, make_state_with_evals_url,
    register_and_approve, run_full_score, run_mocked, run_qm, seed_avail, seed_leased,
    setup_benchmarks, setup_models, submit_benchmark,
};

/// Small enough that a run spans several chunks.
const TEST_CHUNK_SIZE: std::num::NonZeroUsize = std::num::NonZeroUsize::new(2).unwrap();

/// The dataset the mock `/score` knows: each sample's prompt and its verdict.
const MOCK_SAMPLES: [(&str, &str, bool); 3] = [
    ("s1", "What is 2+2?", true),
    ("s2", "Capital of France?", false),
    ("s3", "Color of the sky?", true),
];

/// A mock evals server whose `/score` echoes each posted completion with the
/// fixture's messages and verdict, and answers `400` to a request it cannot
/// score in full, unknown or repeated ids included.
async fn mock_evals_server() -> MockServer {
    let server = MockServer::start_async().await;
    server
        .mock_async(|when, then| {
            when.method(POST).path("/score");
            then.respond_with(mock_score);
        })
        .await;
    server
}

fn mock_score(req: &HttpMockRequest) -> HttpMockResponse {
    let request: serde_json::Value = match serde_json::from_slice(req.body_ref()) {
        Ok(request) => request,
        Err(e) => return mock_bad_request(&format!("unparseable request: {e}")),
    };
    let Some(completions) = request
        .get("completions")
        .and_then(serde_json::Value::as_array)
    else {
        return mock_bad_request("request has no `completions` array");
    };
    let mut seen = HashSet::with_capacity(completions.len());
    let mut scored_samples = Vec::with_capacity(completions.len());
    for c in completions {
        let (Some(id), Some(completion)) = (
            c.get("id").and_then(serde_json::Value::as_str),
            c.get("completion").and_then(serde_json::Value::as_str),
        ) else {
            return mock_bad_request(&format!(
                "completion without a string `id` and `completion`: {c}"
            ));
        };
        let Some((_, prompt, is_correct)) = MOCK_SAMPLES.iter().find(|(s, ..)| *s == id) else {
            return mock_bad_request(&format!("unknown sample id {id:?}"));
        };
        if !seen.insert(id) {
            return mock_bad_request(&format!("duplicate sample id {id:?}"));
        }
        scored_samples.push(json!({
            "id": id,
            "messages": [{"role": "user", "content": prompt}],
            "completion": completion,
            "is_correct": is_correct,
        }));
    }
    let body = json!({
        "runtime_version": "mock-v1.0.0",
        // `accuracy` is the headline a vlm_eval is not finalized without.
        "context": {"accuracy": 0.6667, "accuracy_mock": 0.6667, "samples_seen": scored_samples.len()},
        "scored_samples": scored_samples,
    });
    HttpMockResponse::builder()
        .status(200)
        .body(body.to_string())
        .build()
}

/// A `400` whose reason also goes to stderr, since the scorer only logs it.
/// Not `eprintln!`: this runs on httpmock's pooled server thread, whose libtest
/// capture belongs to whichever test first started it.
fn mock_bad_request(reason: &str) -> HttpMockResponse {
    let _ = writeln!(std::io::stderr(), "mock /score: {reason}");
    HttpMockResponse::builder()
        .status(400)
        .body(reason.to_owned())
        .build()
}

/// The `.parquet` part files of a partition, in name order, which is write
/// order.
fn partition_parquet_files(partition: &std::path::Path) -> anyhow::Result<Vec<std::path::PathBuf>> {
    let mut paths = std::fs::read_dir(partition)?
        .map(|entry| Ok(entry?.path()))
        .collect::<anyhow::Result<Vec<_>>>()?
        .into_iter()
        .filter(|path| path.extension().is_some_and(|e| e == "parquet"))
        .collect::<Vec<_>>();
    paths.sort();
    Ok(paths)
}

/// `None` stands for SQL NULL. Returns the rows checked, so a caller can prove
/// the partition was not empty.
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
    partition_parquet_files(partition)?.into_iter().try_fold(
        0usize,
        |checked, path| -> anyhow::Result<usize> {
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
        },
    )
}

/// [`assert_column`] over a nullable string column.
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

fn assert_f64_column(
    partition: &std::path::Path,
    column: &str,
    expected: Option<f64>,
) -> anyhow::Result<usize> {
    assert_column::<arrow::array::Float64Array, _>(partition, column, expected, |values, i| {
        values.value(i)
    })
}

fn assert_i32_column(
    partition: &std::path::Path,
    column: &str,
    expected: Option<i32>,
) -> anyhow::Result<usize> {
    assert_column::<arrow::array::Int32Array, _>(partition, column, expected, |values, i| {
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
    let key = job(job_id)?;
    // No `message_type`: exercises the legacy-body fallback in
    // `parse_stored_submission`, which production's untagged bodies need.
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

/// The collector accepts padded strings; the scorer trims them before writing
/// the warehouse row.
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

    let expected = [
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
    ];

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

/// The greedy pin is the case that matters: `0.0` must reach the column as a
/// value, not as the NULL that means "the client chose". The descriptor carries
/// the same pins beside the entry's other parameters, read off the catalog even
/// when the client declares the narrow pair.
#[tokio::test]
async fn test_eval_scoring_stamps_the_pinned_sampling() -> anyhow::Result<()> {
    let evals = mock_evals_server().await;

    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    std::fs::write(
        dir.path().join("benchmarks/eval_test_pinned.toml"),
        r#"benchmark_type = "eval"
parameter_eval_id = "test_eval"
parameter_dataset_name = "test_ds"
parameter_max_tokens = 100
parameter_temperature = 0.0
parameter_top_p = 0.95
parameter_top_k = 64
parameter_min_p = 0.0
parameter_seed = 7
"#,
    )?;
    let state = make_state_with_evals_url(dir.path(), &evals.base_url()).await?;
    let (sk, client_id) = register_and_approve(&state).await?;

    submit_benchmark(
        &state,
        &sk,
        &client_id,
        &json!({
            "benchmark_id": "eval_test_pinned",
            "benchmark_type": "eval",
            "benchmark_params": {"eval_id": "test_eval", "dataset_name": "test_ds", "max_tokens": 100},
            "device_name": "test-device",
            "device_form_factor": "embedded",
            "device_os_name": "Linux",
            "device_os_version": "22.04",
            "device_chip_model": "test-chip",
            "device_ram_bytes": 17179869184i64,
            "model_name": "llama-3.2-1b",
            "model_quant": "q4_0",
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

    let data_dir = state
        .config
        .storage
        .data_dir()
        .ok_or_else(|| anyhow::anyhow!("expected local_fs config"))?;
    let partition = warehouse::warehouse_day_partition_dir(
        &data_dir.join("warehouse/results"),
        &BenchmarkId::try_new("eval_test_pinned")?,
        &client_id,
        &chrono::Utc::now().format("%Y-%m-%d").to_string(),
    );
    assert!(partition.exists(), "expected a scored partition");

    let checked = assert_f64_column(&partition, "parameter_temperature", Some(0.0))?;
    assert!(checked > 0, "the partition held no rows to check");
    assert_f64_column(&partition, "parameter_top_p", Some(0.95))?;
    assert_i32_column(&partition, "parameter_top_k", Some(64))?;
    assert_f64_column(&partition, "parameter_min_p", Some(0.0))?;
    assert_i64_column(&partition, "parameter_seed", Some(7))?;
    let params = r#"{"benchmark_type":"eval","dataset_name":"test_ds","eval_id":"test_eval","max_tokens":100,"min_p":0.0,"seed":7,"temperature":0.0,"top_k":64,"top_p":0.95}"#;
    assert_string_column(&partition, "benchmark_params", Some(params))?;
    assert_string_column(
        &partition,
        "benchmark_params_sha256",
        // The id a client computes from its own plan-types copy.
        Some(&pipette_plan_types::descriptor_digest(
            &serde_json::from_str::<serde_json::Value>(params)?,
        )?),
    )?;
    Ok(())
}

/// A pinned vlm_eval entry over the mock's three samples: its sampling, a
/// prompt profile, a corpus pin and a recipe.
fn write_vlm_eval_pinned(dir: &std::path::Path) -> anyhow::Result<()> {
    std::fs::write(
        dir.join("benchmarks/vlm_eval_test_pinned.toml"),
        r#"benchmark_type = "vlm_eval"
parameter_lmms_task = "test_task"
parameter_dataset_version = "2026.10.1"
parameter_sample_count = 3
parameter_max_tokens = 100
parameter_temperature = 0.7
parameter_top_p = 0.8
parameter_top_k = 20
parameter_min_p = 0.0
parameter_presence_penalty = 1.5
parameter_repeat_penalty = 1.0
parameter_seed = 7
parameter_prompt_profile = "internvl"

[dataset]
format  = "vlm-eval-corpus/1"
adapter = "evals-server@1"
digest  = "1111111111111111111111111111111111111111111111111111111111111111"
bytes   = 1024

[dataset.upstream]
source          = "evals_server"
eval_id         = "test_task"
dataset_version = "2026.10.1"
prompt_profile  = "internvl"
identity        = "2222222222222222222222222222222222222222222222222222222222222222"

[recipe]
family  = "Test family"
models  = [{ repo = "org/repo" }]
sources = ["https://huggingface.co/org/repo"]
"#,
    )?;
    Ok(())
}

/// A vlm_eval submission without one answer to each sample is refused when it
/// arrives, so the client keeps its checkpoint and nothing is queued that can
/// never be scored. A reply cut off at the cap is an answer.
#[rstest]
#[case::short(json!([{"id": "s1", "completion": "4"}, {"id": "s2", "completion": "London"}]), true)]
#[case::a_failed_row(json!([{"id": "s1", "completion": "4"}, {"id": "s2", "completion": "London"}, {"id": "s3", "completion": "", "stop_reason": "failure"}]), true)]
#[case::a_legacy_failed_row(json!([{"id": "s1", "completion": "4"}, {"id": "s2", "completion": "London"}, {"id": "s3", "completion": "", "failed": true}]), true)]
#[case::cut_off(json!([{"id": "s1", "completion": "4"}, {"id": "s2", "completion": "London"}, {"id": "s3", "completion": "the sky is", "stop_reason": "truncated"}]), false)]
#[tokio::test]
async fn a_vlm_eval_submission_without_an_answer_to_every_sample_is_refused(
    #[case] completions: serde_json::Value,
    #[case] refused: bool,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    write_vlm_eval_pinned(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (sk, client_id) = register_and_approve(&state).await?;
    let resp = authed_post(
        &state,
        &sk,
        &client_id,
        "/benchmarks",
        &json!({
            "benchmark_id": "vlm_eval_test_pinned",
            "benchmark_type": "vlm_eval",
            "benchmark_params": {"lmms_task": "test_task", "dataset_version": "2026.10.1", "sample_count": 3, "max_tokens": 100},
            "device_name": "test-device",
            "device_form_factor": "embedded",
            "device_os_name": "Linux",
            "device_os_version": "22.04",
            "device_chip_model": "test-chip",
            "device_ram_bytes": 17179869184i64,
            "model_name": "lfm2-vl-450m",
            "model_quant": "q4_0",
            "runtime_name": "llama.cpp",
            "runtime_version": "b5000",
            "observation_dataset_digest": "1".repeat(64),
            "completions": completions,
        }),
    )
    .await?;
    if !refused {
        assert!(resp.status().is_success(), "{}", resp.status());
        return Ok(());
    }
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let text = body_json(resp).await?.to_string();
    assert!(
        text.contains("one answer to each of its 3 samples"),
        "{text}"
    );
    Ok(())
}

/// On an entry that names a seed rule, only a result that declares the rule
/// lands: one that declares none, or a pair without it, may come from a client
/// that drew by the repeat offset, and is refused on either route.
#[rstest]
#[case::declares_the_rule(Some(json!({"lmms_task": "test_task", "dataset_version": "2026.10.1", "sample_count": 3, "max_tokens": 100, "seed_rule": "per_sample_sha256"})), None)]
#[case::declares_nothing(None, Some("names a seed_rule: a result must declare"))]
#[case::leaves_the_rule_out(
    Some(json!({"lmms_task": "test_task", "dataset_version": "2026.10.1", "sample_count": 3, "max_tokens": 100})),
    Some("benchmark_params do not match")
)]
#[tokio::test]
async fn a_result_for_a_seed_rule_entry_must_declare_the_rule(
    #[values("/benchmarks", "/benchmarks/batch")] route: &str,
    #[case] params: Option<serde_json::Value>,
    #[case] refused: Option<&str>,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    write_vlm_eval_pinned(dir.path())?;
    let pinned = std::fs::read_to_string(dir.path().join("benchmarks/vlm_eval_test_pinned.toml"))?;
    std::fs::write(
        dir.path().join("benchmarks/vlm_eval_test_seed_rule.toml"),
        pinned.replace(
            "parameter_seed = 7\n",
            "parameter_seed = 7\nparameter_seed_rule = \"per_sample_sha256\"\n",
        ),
    )?;
    let state = make_state(dir.path()).await?;
    let (sk, client_id) = register_and_approve(&state).await?;
    let mut submission = json!({
        "benchmark_id": "vlm_eval_test_seed_rule",
        "device_name": "test-device",
        "device_form_factor": "embedded",
        "device_os_name": "Linux",
        "device_os_version": "22.04",
        "device_chip_model": "test-chip",
        "device_ram_bytes": 17179869184i64,
        "model_name": "lfm2-vl-450m",
        "model_quant": "q4_0",
        "runtime_name": "llama.cpp",
        "runtime_version": "b5000",
        "observation_dataset_digest": "1".repeat(64),
        "completions": [
            {"id": "s1", "completion": "4"},
            {"id": "s2", "completion": "London"},
            {"id": "s3", "completion": "blue"}
        ],
    });
    if let Some(params) = params {
        submission["benchmark_type"] = json!("vlm_eval");
        submission["benchmark_params"] = params;
    }
    let batch = route == "/benchmarks/batch";
    let body = if batch {
        json!({ "submissions": [submission] })
    } else {
        submission
    };
    let resp = authed_post(&state, &sk, &client_id, route, &body).await?;
    let (status, reply) = (resp.status(), body_json(resp).await?);
    let error = match (batch, refused) {
        (false, None) => {
            assert!(status.is_success(), "{status} {reply}");
            return Ok(());
        }
        (true, None) => {
            assert_eq!(status, StatusCode::OK);
            assert!(reply["results"][0]["error"].is_null(), "{reply}");
            return Ok(());
        }
        (false, Some(_)) => {
            assert_eq!(status, StatusCode::BAD_REQUEST);
            &reply["error"]
        }
        (true, Some(_)) => {
            assert_eq!(status, StatusCode::OK);
            assert!(reply["results"][0]["job_id"].is_null(), "{reply}");
            &reply["results"][0]["error"]
        }
    };
    let error = error.as_str().context("the refusal names its reason")?;
    let want = refused.context("a refusal")?;
    assert!(error.contains(want), "{error}");
    Ok(())
}

/// A vlm_eval that did not run the corpus its entry pins is refused when it
/// arrives, on either route, as scoring would refuse it on every pass.
#[rstest]
#[case::none_reported(None)]
#[case::another_corpus(Some("3".repeat(64)))]
#[tokio::test]
async fn a_vlm_eval_submission_off_the_pinned_corpus_is_refused(
    #[values("/benchmarks", "/benchmarks/batch")] route: &str,
    #[case] reported: Option<String>,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    write_vlm_eval_pinned(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (sk, client_id) = register_and_approve(&state).await?;
    let submission = json!({
        "benchmark_id": "vlm_eval_test_pinned",
        "benchmark_type": "vlm_eval",
        "benchmark_params": {"lmms_task": "test_task", "dataset_version": "2026.10.1", "sample_count": 3, "max_tokens": 100},
        "device_name": "test-device",
        "device_form_factor": "embedded",
        "device_os_name": "Linux",
        "device_os_version": "22.04",
        "device_chip_model": "test-chip",
        "device_ram_bytes": 17179869184i64,
        "model_name": "lfm2-vl-450m",
        "model_quant": "q4_0",
        "runtime_name": "llama.cpp",
        "runtime_version": "b5000",
        "observation_dataset_digest": reported,
        "completions": [
            {"id": "s1", "completion": "4"},
            {"id": "s2", "completion": "London"},
            {"id": "s3", "completion": "blue"}
        ],
    });
    let batch = route == "/benchmarks/batch";
    let body = if batch {
        json!({ "submissions": [submission] })
    } else {
        submission
    };
    let resp = authed_post(&state, &sk, &client_id, route, &body).await?;
    let (status, reply) = (resp.status(), body_json(resp).await?);
    let error = if batch {
        assert_eq!(status, StatusCode::OK);
        assert!(reply["results"][0]["job_id"].is_null(), "{reply}");
        &reply["results"][0]["error"]
    } else {
        assert_eq!(status, StatusCode::BAD_REQUEST);
        &reply["error"]
    };
    let error = error.as_str().context("the refusal names its reason")?;
    assert!(error.contains("the corpus the entry pins"), "{error}");
    Ok(())
}

/// A vlm_eval pins its sampling as an eval does, the two penalties included,
/// and names its prompt profile; a declaration from a client that predates the
/// knobs and the profile still agrees. Its sample results read back with their
/// prompt, as an eval's do.
#[tokio::test]
async fn test_vlm_eval_scoring_stamps_the_pinned_sampling_and_profile() -> anyhow::Result<()> {
    let evals = mock_evals_server().await;

    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    write_vlm_eval_pinned(dir.path())?;
    let state = make_state_with_evals_url(dir.path(), &evals.base_url()).await?;
    let (sk, client_id) = register_and_approve(&state).await?;

    let job_id = submit_benchmark(
        &state,
        &sk,
        &client_id,
        &json!({
            "benchmark_id": "vlm_eval_test_pinned",
            "benchmark_type": "vlm_eval",
            "benchmark_params": {"lmms_task": "test_task", "dataset_version": "2026.10.1", "sample_count": 3, "max_tokens": 100},
            "device_name": "test-device",
            "device_form_factor": "embedded",
            "device_os_name": "Linux",
            "device_os_version": "22.04",
            "device_chip_model": "test-chip",
            "device_ram_bytes": 17179869184i64,
            "model_name": "lfm2-vl-450m",
            "model_quant": "q4_0",
            "runtime_name": "llama.cpp",
            "runtime_version": "b5000",
            // The corpus the entry pins: a vlm_eval is scored only as that.
            "observation_dataset_digest": "1111111111111111111111111111111111111111111111111111111111111111",
            "completions": [
                {"id": "s1", "completion": "4"},
                {"id": "s2", "completion": "London"},
                {"id": "s3", "completion": "blue"}
            ]
        }),
    )
    .await?;

    run_full_score(&state.config).await?;

    let data_dir = state
        .config
        .storage
        .data_dir()
        .ok_or_else(|| anyhow::anyhow!("expected local_fs config"))?;
    let partition = warehouse::warehouse_day_partition_dir(
        &data_dir.join("warehouse/results"),
        &BenchmarkId::try_new("vlm_eval_test_pinned")?,
        &client_id,
        &chrono::Utc::now().format("%Y-%m-%d").to_string(),
    );
    assert!(partition.exists(), "expected a scored partition");

    let checked = assert_f64_column(&partition, "parameter_temperature", Some(0.7))?;
    assert!(checked > 0, "the partition held no rows to check");
    assert_f64_column(&partition, "parameter_top_p", Some(0.8))?;
    assert_i32_column(&partition, "parameter_top_k", Some(20))?;
    assert_f64_column(&partition, "parameter_min_p", Some(0.0))?;
    assert_f64_column(&partition, "parameter_presence_penalty", Some(1.5))?;
    assert_f64_column(&partition, "parameter_repeat_penalty", Some(1.0))?;
    assert_i64_column(&partition, "parameter_seed", Some(7))?;
    assert_string_column(&partition, "parameter_prompt_profile", Some("internvl"))?;
    let params = r#"{"benchmark_type":"vlm_eval","dataset_version":"2026.10.1","lmms_task":"test_task","max_tokens":100,"min_p":0.0,"presence_penalty":1.5,"prompt_profile":"internvl","repeat_penalty":1.0,"sample_count":3,"seed":7,"temperature":0.7,"top_k":20,"top_p":0.8}"#;
    assert_string_column(&partition, "benchmark_params", Some(params))?;

    let resp = authed_get(
        &state,
        &sk,
        &client_id,
        &format!("/jobs/{job_id}/eval-sample-results"),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await?;
    let s1 = body
        .as_array()
        .and_then(|samples| samples.iter().find(|s| s["id"] == "s1"))
        .with_context(|| format!("no sample s1: {body}"))?;
    assert_eq!(
        s1["messages"],
        json!([{"role": "user", "content": "What is 2+2?"}])
    );
    Ok(())
}

/// asr_eval's `/score`: the entry's prompt profile, its recipe's thinking mode
/// (off), the pinned samples digest, and the scorer's word-level corpus rate
/// with each clip's alignment.
fn mock_asr_score(req: &HttpMockRequest) -> HttpMockResponse {
    let request: serde_json::Value = match serde_json::from_slice(req.body_ref()) {
        Ok(request) => request,
        Err(e) => return mock_bad_request(&format!("unparseable request: {e}")),
    };
    if request.get("prompt_profile") != Some(&json!("minicpm_o")) {
        return mock_bad_request(&format!("asr_eval without its profile: {request}"));
    }
    if request.get("enable_thinking") != Some(&json!(false)) {
        return mock_bad_request(&format!(
            "asr_eval not sent its recipe's thinking: {request}"
        ));
    }
    if request.get("samples_digest") != Some(&json!(format!("sha256:{}", "2".repeat(64)))) {
        return mock_bad_request(&format!("asr_eval not sent its pinned samples: {request}"));
    }
    let Some(completions) = request
        .get("completions")
        .and_then(serde_json::Value::as_array)
    else {
        return mock_bad_request("request has no `completions` array");
    };
    let scored_samples: Vec<serde_json::Value> = completions
        .iter()
        .map(|c| {
            let id = c.get("id").cloned().unwrap_or_default();
            let exact = id == "a1";
            json!({
                "id": id,
                // The scorer keeps each clip's part in place, emptied.
                "messages": [{"role": "user", "content": [
                    {"type": "input_audio", "input_audio": {"data": "", "format": "wav"}},
                    {"type": "text", "text": "Transcribe."},
                ]}],
                "completion": c.get("completion"),
                "is_correct": exact,
                "measures": {"unit": id, "hits": 2, "substitutions": i32::from(!exact),
                             "deletions": 0, "insertions": 0, "reference_units": 2},
            })
        })
        .collect();
    let body = json!({
        "runtime_version": "mock-v1.0.0",
        "context": {"error_unit": "word", "error_rate": 0.25, "result": 25.0,
                    "samples_total": 2, "samples_excluded": 0},
        "scored_samples": scored_samples,
    });
    HttpMockResponse::builder()
        .status(200)
        .body(body.to_string())
        .build()
}

/// A pinned asr_eval entry over two clips: MiniCPM-o's beams on LibriSpeech
/// clean, with its corpus pin and recipe.
fn write_asr_eval_minicpm_o(dir: &std::path::Path) -> anyhow::Result<()> {
    std::fs::write(
        dir.join("benchmarks/asr_eval_ls_clean_minicpm_o.toml"),
        r#"benchmark_type = "asr_eval"
parameter_asr_task = "asr_esb_librispeech_test_clean"
parameter_dataset_version = "2026.10.1"
parameter_sample_count = 2
parameter_prompt_profile = "minicpm_o"
parameter_max_tokens = 128
parameter_num_beams = 3
parameter_beam_repetition_penalty = 1.2
parameter_beam_length_penalty = 1.0
"#
        .to_owned()
            + &pipette_core::dataset::asr_eval_catalog_tables(
                "asr_esb_librispeech_test_clean",
                "2026.10.1",
                "minicpm_o",
            ),
    )?;
    Ok(())
}

/// An asr_eval submission without one answer to each clip is refused when it
/// arrives, as a vlm_eval's is: the client keeps its checkpoint, and nothing
/// is queued that can never be scored. A failed row counts by either field,
/// so a row whose `failed` flag and `stop_reason` disagree is refused too. A
/// transcript cut off at the cap is an answer.
#[rstest]
#[case::short(json!([{"id": "a1", "completion": "hello world"}]), true)]
#[case::a_failed_clip(json!([{"id": "a1", "completion": "hello world"}, {"id": "a2", "completion": "", "stop_reason": "failure"}]), true)]
#[case::a_legacy_failed_clip(json!([{"id": "a1", "completion": "hello world"}, {"id": "a2", "completion": "", "failed": true}]), true)]
#[case::a_contradicting_clip(json!([{"id": "a1", "completion": "hello world"}, {"id": "a2", "completion": "", "failed": true, "stop_reason": "eos"}]), true)]
#[case::a_failed_duplicate(json!([{"id": "a1", "completion": "hello world"}, {"id": "a1", "completion": "", "stop_reason": "failure"}, {"id": "a2", "completion": "hello word"}]), true)]
#[case::cut_off(json!([{"id": "a1", "completion": "hello world"}, {"id": "a2", "completion": "hello", "stop_reason": "truncated"}]), false)]
#[tokio::test]
async fn an_asr_eval_submission_without_an_answer_to_every_sample_is_refused(
    #[case] completions: serde_json::Value,
    #[case] refused: bool,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    write_asr_eval_minicpm_o(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (sk, client_id) = register_and_approve(&state).await?;
    let resp = authed_post(
        &state,
        &sk,
        &client_id,
        "/benchmarks",
        &json!({
            "benchmark_id": "asr_eval_ls_clean_minicpm_o",
            "benchmark_type": "asr_eval",
            "benchmark_params": {"asr_task": "asr_esb_librispeech_test_clean", "dataset_version": "2026.10.1",
                "sample_count": 2, "prompt_profile": "minicpm_o", "max_tokens": 128, "num_beams": 3,
                "beam_repetition_penalty": 1.2, "beam_length_penalty": 1.0},
            "device_name": "test-device",
            "device_form_factor": "embedded",
            "device_os_name": "Linux",
            "device_os_version": "22.04",
            "device_chip_model": "test-chip",
            "device_ram_bytes": 17179869184i64,
            "model_name": "minicpm-o-4.5",
            "model_quant": "q8_0",
            "runtime_name": "llama.cpp",
            "runtime_version": "b5000",
            "observation_dataset_digest": "1".repeat(64),
            "completions": completions,
        }),
    )
    .await?;
    if !refused {
        assert!(resp.status().is_success(), "{}", resp.status());
        return Ok(());
    }
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let text = body_json(resp).await?.to_string();
    assert!(
        text.contains("an asr_eval is a result only with one answer to each of its 2 samples"),
        "{text}"
    );
    Ok(())
}

/// An asr_eval scores as one `wer` row, a ratio, and stamps its beams; its
/// sample results keep the scorer's measures and lose the clip.
#[tokio::test]
async fn test_asr_eval_scoring_reports_wer_and_its_beams() -> anyhow::Result<()> {
    let evals = MockServer::start_async().await;
    evals
        .mock_async(|when, then| {
            when.method(POST).path("/score");
            then.respond_with(mock_asr_score);
        })
        .await;

    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    write_asr_eval_minicpm_o(dir.path())?;
    let state = make_state_with_evals_url(dir.path(), &evals.base_url()).await?;
    let (sk, client_id) = register_and_approve(&state).await?;
    // A claimed job whose requested flags ask for thinking every way a text
    // eval would: the scorer still hears the recipe's mode, off.
    let claimed = "job-6a1f8400-e29b-41d4-a716-446655440000";
    seed_leased(
        dir.path(),
        claimed,
        &client_id,
        &json!({
            "job_id": claimed,
            "spec": {
                "benchmark": "asr_eval_ls_clean_minicpm_o",
                "model": {"type": "gguf_text", "path": "m.gguf"},
                "runtime": {"type": "llamacpp_cli_stock_tools"},
                "model_flags": {"benchmark_type": "eval", "model_type": "gguf_text", "enable_thinking": true},
                "runtime_flags": {"reasoning": "on"},
            },
        }),
    )?;

    let job_id = submit_benchmark(
        &state,
        &sk,
        &client_id,
        &json!({
            "job_id": claimed,
            "benchmark_id": "asr_eval_ls_clean_minicpm_o",
            "benchmark_type": "asr_eval",
            "benchmark_params": {"asr_task": "asr_esb_librispeech_test_clean", "dataset_version": "2026.10.1",
                "sample_count": 2, "prompt_profile": "minicpm_o", "max_tokens": 128, "num_beams": 3,
                "beam_repetition_penalty": 1.2, "beam_length_penalty": 1.0},
            "device_name": "test-device",
            "device_form_factor": "embedded",
            "device_os_name": "Linux",
            "device_os_version": "22.04",
            "device_chip_model": "test-chip",
            "device_ram_bytes": 17179869184i64,
            "model_name": "minicpm-o-4.5",
            "model_quant": "q8_0",
            "runtime_name": "llama.cpp",
            "runtime_version": "b5000",
            // The corpus the entry pins: an asr_eval is scored only as that.
            "observation_dataset_digest": "1".repeat(64),
            "completions": [
                {"id": "a1", "completion": "hello world"},
                {"id": "a2", "completion": "hello word"}
            ]
        }),
    )
    .await?;

    run_full_score(&state.config).await?;

    let resp = authed_get(&state, &sk, &client_id, &format!("/jobs/{job_id}")).await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await?;
    assert_eq!(body["status"], "processed", "{body}");
    let metrics = body["metrics"]
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("missing metrics array"))?;
    assert_eq!(metrics.len(), 1, "{body}");
    assert_eq!(metrics[0]["metric"], "wer");
    assert_eq!(metrics[0]["unit"], "ratio");
    let wer = metrics[0]["value"]
        .as_f64()
        .ok_or_else(|| anyhow::anyhow!("missing wer value"))?;
    assert!((wer - 0.25).abs() < 1e-6, "{wer}");

    let data_dir = state
        .config
        .storage
        .data_dir()
        .ok_or_else(|| anyhow::anyhow!("expected local_fs config"))?;
    let partition = warehouse::warehouse_day_partition_dir(
        &data_dir.join("warehouse/results"),
        &BenchmarkId::try_new("asr_eval_ls_clean_minicpm_o")?,
        &client_id,
        &chrono::Utc::now().format("%Y-%m-%d").to_string(),
    );
    let checked = assert_i32_column(&partition, "parameter_num_beams", Some(3))?;
    assert!(checked > 0, "the partition held no rows to check");
    assert_f64_column(&partition, "parameter_beam_repetition_penalty", Some(1.2))?;
    assert_f64_column(&partition, "parameter_beam_length_penalty", Some(1.0))?;
    assert_f64_column(&partition, "parameter_temperature", None)?;
    assert_string_column(&partition, "parameter_prompt_profile", Some("minicpm_o"))?;
    // The thinking request reached the job, so the mock's insistence on
    // `false` shows the recipe's mode, not the request's, went to `/score`.
    assert_string_column(
        &partition,
        "requested_model_flags",
        Some(r#"{"benchmark_type":"eval","enable_thinking":true,"model_type":"gguf_text"}"#),
    )?;

    let resp = authed_get(
        &state,
        &sk,
        &client_id,
        &format!("/jobs/{job_id}/eval-sample-results"),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await?;
    let a2 = body
        .as_array()
        .and_then(|samples| samples.iter().find(|s| s["id"] == "a2"))
        .with_context(|| format!("no sample a2: {body}"))?;
    assert_eq!(a2["measures"]["substitutions"], 1);
    // The clip is not stored, but its part is, empty, so the turn's order
    // stays auditable.
    assert_eq!(
        a2["messages"],
        json!([{"role": "user", "content": [
            {"type": "input_audio", "input_audio": {"data": "", "format": "wav"}},
            {"type": "text", "text": "Transcribe."},
        ]}])
    );
    Ok(())
}

/// A partial asr_eval is never scored: a stored body that answers one of its
/// two clips, or fails one by either field, stays queued and logged, `/score`
/// is not asked, and no row is published. The route refuses such a body when
/// it arrives (above), so this one is stored as an answered run and then
/// rewritten, as a body that predates the route's check would be. The full
/// run above is the negative control.
#[rstest::rstest]
#[case::short(json!([{"id": "a1", "completion": "hello world"}]))]
#[case::a_failed_clip(json!([
    {"id": "a1", "completion": "hello world"},
    {"id": "a2", "completion": "", "stop_reason": "failure"},
]))]
#[case::a_legacy_failed_clip(json!([
    {"id": "a1", "completion": "hello world"},
    {"id": "a2", "completion": "", "failed": true},
]))]
#[case::a_failed_duplicate(json!([
    {"id": "a1", "completion": "hello world"},
    {"id": "a1", "completion": "", "stop_reason": "failure"},
    {"id": "a2", "completion": "hello word"},
]))]
fn a_partial_asr_eval_is_never_scored(
    #[case] completions: serde_json::Value,
) -> anyhow::Result<()> {
    pipette_web_testkit::run_mocked(partial_asr_eval(completions))
}

async fn partial_asr_eval(completions: serde_json::Value) -> anyhow::Result<()> {
    let evals = MockServer::start_async().await;
    let score = evals
        .mock_async(|when, then| {
            when.method(POST).path("/score");
            then.respond_with(mock_asr_score);
        })
        .await;
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    std::fs::write(
        dir.path().join("benchmarks/asr_eval_ls_clean_voxtral.toml"),
        "benchmark_type = \"asr_eval\"\nparameter_asr_task = \"asr_esb_librispeech_test_clean\"\n\
         parameter_dataset_version = \"2026.10.1\"\nparameter_sample_count = 2\n\
         parameter_prompt_profile = \"voxtral\"\nparameter_max_tokens = 500\n"
            .to_owned()
            + &pipette_core::dataset::asr_eval_catalog_tables(
                "asr_esb_librispeech_test_clean",
                "2026.10.1",
                "voxtral",
            ),
    )?;
    let state = make_state_with_evals_url(dir.path(), &evals.base_url()).await?;
    let (sk, client_id) = register_and_approve(&state).await?;
    let job_id = submit_benchmark(
        &state,
        &sk,
        &client_id,
        &json!({
            "benchmark_id": "asr_eval_ls_clean_voxtral",
            "benchmark_type": "asr_eval",
            "benchmark_params": {"asr_task": "asr_esb_librispeech_test_clean", "dataset_version": "2026.10.1",
                "sample_count": 2, "prompt_profile": "voxtral", "max_tokens": 500},
            "device_name": "test-device",
            "device_form_factor": "embedded",
            "device_os_name": "Linux",
            "device_os_version": "22.04",
            "device_chip_model": "test-chip",
            "device_ram_bytes": 17179869184i64,
            "model_name": "voxtral-mini",
            "model_quant": "q8_0",
            "runtime_name": "llama.cpp",
            "runtime_version": "b5000",
            "observation_dataset_digest": "1".repeat(64),
            "completions": [
                {"id": "a1", "completion": "hello world"},
                {"id": "a2", "completion": "hello word"}
            ],
        }),
    )
    .await?;
    let stores = build_local_fs_stores(&state.config)?;
    let key = job(&job_id)?;
    let mut body = stores
        .submissions
        .get_submission(&key)
        .await?
        .ok_or_else(|| anyhow::anyhow!("the submission is stored"))?
        .body;
    *body
        .get_mut("completions")
        .ok_or_else(|| anyhow::anyhow!("the stored body has completions"))? = completions;
    stores.submissions.write_incoming(&key, &body).await?;

    run_full_score(&state.config).await?;

    score.assert_calls_async(0).await;
    let stores = build_local_fs_stores(&state.config)?;
    let to_do = stores
        .submissions
        .list_queue(ScoreQueueStage::ToDo, TEST_LIST_LIMIT)
        .await?;
    assert_eq!(
        to_do.iter().map(|j| j.as_str()).collect::<Vec<_>>(),
        vec![job_id.as_str()],
        "a partial asr_eval stays queued",
    );
    let resp = authed_get(&state, &sk, &client_id, &format!("/jobs/{job_id}")).await?;
    let body = body_json(resp).await?;
    assert_ne!(body.get("status"), Some(&json!("processed")), "{body}");
    Ok(())
}

#[tokio::test]
async fn test_eval_scoring_produces_accuracy_and_eval_sample_results() -> anyhow::Result<()> {
    let evals = mock_evals_server().await;

    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state_with_evals_url(dir.path(), &evals.base_url()).await?;
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

    let s1 = by_id["s1"];
    assert_eq!(s1["completion"], "4");
    assert_eq!(s1["is_correct"], true);
    assert_eq!(s1["messages"][0]["role"], "user");
    assert_eq!(s1["messages"][0]["content"], "What is 2+2?");

    let s2 = by_id["s2"];
    assert_eq!(s2["completion"], "London");
    assert_eq!(s2["is_correct"], false);

    let s3 = by_id["s3"];
    assert_eq!(s3["completion"], "blue");
    assert_eq!(s3["is_correct"], true);

    Ok(())
}

#[tokio::test]
async fn test_malformed_submission_on_disk_stays_in_incoming() -> anyhow::Result<()> {
    // There is no dead-letter path: the next cron retries it.
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (_sk, client_id) = register_and_approve(&state).await?;

    let job_id = "poison-pill-job";
    let key = job(job_id)?;
    // Body missing required field `device_form_factor`, etc.
    let bad_body = json!({
        "benchmark_id": "prefill_throughput_256",
        "client_id": client_id.as_str(),
        "job_id": job_id,
        "submitted_at": "2026-01-01T00:00:00Z",
    });
    pipette_store_fs::build_local_fs_stores(&state.config)?
        .submissions
        .write_incoming(&key, &bad_body)
        .await?;

    score::run_process_submissions(&state.config, build_local_fs_stores(&state.config)?).await?;

    let stores = pipette_store_fs::build_local_fs_stores(&state.config)?;
    let incoming = stores.submissions.list_incoming(TEST_LIST_LIMIT).await?;
    assert!(
        incoming.iter().any(|k| k.as_str() == job_id),
        "malformed submission should remain in incoming/",
    );
    Ok(())
}

#[tokio::test]
async fn test_eval_scoring_fails_on_malformed_upstream_score_response() -> anyhow::Result<()> {
    // `200` without `scored_samples`, which the typed deserializer must reject.
    let evals = MockServer::start_async().await;
    let score = evals
        .mock_async(|when, then| {
            when.method(POST).path("/score");
            then.status(200)
                .json_body(json!({"runtime_version": "x", "not_scored_samples": []}));
        })
        .await;

    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state_with_evals_url(dir.path(), &evals.base_url()).await?;
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
    // A missed matcher answers 404, which would also leave the job in `to_do`.
    score.assert_async().await;
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
    let evals = mock_evals_server().await;

    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state_with_evals_url(dir.path(), &evals.base_url()).await?;
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

/// A failure sent with its crash text in `stop_detail` alone reads it back as
/// `failed_reason`; a sample that did not fail reads null.
#[tokio::test]
async fn test_eval_sample_results_read_failed_reason_from_stop_detail() -> anyhow::Result<()> {
    let evals = mock_evals_server().await;

    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state_with_evals_url(dir.path(), &evals.base_url()).await?;
    let (sk, client_id) = register_and_approve(&state).await?;

    let job_id = submit_benchmark(
        &state,
        &sk,
        &client_id,
        &eval_submission(json!([
            {"id": "s1", "completion": "4"},
            {
                "id": "s2",
                "completion": "",
                "stop_reason": "failure",
                "stop_detail": "llama-server crashed mid-completion",
                "completion_tokens": 0
            },
            {
                "id": "s3",
                "completion": "blue blue blue",
                "stop_reason": "doom_loop",
                "stop_detail": "the same line repeated 40 times"
            }
        ])),
    )
    .await?;
    run_full_score(&state.config).await?;

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
        .ok_or_else(|| anyhow::anyhow!("expected samples array: {body}"))?;
    assert_eq!(samples.len(), 3, "{body}");
    // `get` tells a missing `failed_reason` key from a null one.
    let failure_fields = samples
        .iter()
        .map(|s| {
            let id = s
                .get("id")
                .and_then(serde_json::Value::as_str)
                .with_context(|| format!("sample without an id: {s}"))?;
            Ok((
                id,
                (s.get("failed").cloned(), s.get("failed_reason").cloned()),
            ))
        })
        .collect::<anyhow::Result<HashMap<_, _>>>()?;
    assert_eq!(
        failure_fields,
        HashMap::from([
            ("s1", (Some(json!(false)), Some(serde_json::Value::Null))),
            (
                "s2",
                (
                    Some(json!(true)),
                    Some(json!("llama-server crashed mid-completion"))
                )
            ),
            ("s3", (Some(json!(false)), Some(serde_json::Value::Null))),
        ])
    );
    Ok(())
}

/// An `eval_test` submission carrying `completions`, whose ids the mock
/// `/score` must know.
fn eval_submission(completions: serde_json::Value) -> serde_json::Value {
    json!({
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
        "completions": completions,
    })
}

#[tokio::test]
async fn test_eval_sample_results_of_another_clients_job_read_as_not_found() -> anyhow::Result<()> {
    let evals = mock_evals_server().await;

    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state_with_evals_url(dir.path(), &evals.base_url()).await?;
    let (sk_owner, owner) = register_and_approve(&state).await?;
    let (sk_other, other) = register_and_approve(&state).await?;

    let job_id = submit_benchmark(
        &state,
        &sk_owner,
        &owner,
        &eval_submission(json!([
            {"id": "s1", "completion": "4"},
            {"id": "s2", "completion": "London"},
            {"id": "s3", "completion": "blue"}
        ])),
    )
    .await?;
    run_full_score(&state.config).await?;
    let uri = format!("/jobs/{job_id}/eval-sample-results");

    // Scored and readable, so ownership alone stands behind the other 404.
    let resp = authed_get(&state, &sk_owner, &owner, &uri).await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let rows = body_json(resp).await?;
    assert_eq!(rows.as_array().map(Vec::len), Some(3), "{rows}");

    let resp = authed_get(&state, &sk_other, &other, &uri).await?;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    assert_eq!(body_json(resp).await?, json!({"error": "job not found"}));
    Ok(())
}

/// A row's `failed` / `failed_reason` come from the sample's `stop_reason`, so
/// a legacy `failed: true` alone reads back not failed.
#[tokio::test]
async fn test_eval_sample_results_derive_failed_from_stop_reason() -> anyhow::Result<()> {
    let evals = mock_evals_server().await;

    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state_with_evals_url(dir.path(), &evals.base_url()).await?;
    let (sk, client_id) = register_and_approve(&state).await?;

    let job_id = submit_benchmark(
        &state,
        &sk,
        &client_id,
        &eval_submission(json!([
            {"id": "s1", "completion": "4"},
            {
                "id": "s2",
                "completion": "",
                "stop_reason": "failure",
                "failed_reason": "llama-server crashed mid-completion"
            },
            {"id": "s3", "completion": "blue", "failed": true, "failed_reason": "legacy flag only"}
        ])),
    )
    .await?;
    run_full_score(&state.config).await?;

    let resp = authed_get(
        &state,
        &sk,
        &client_id,
        &format!("/jobs/{job_id}/eval-sample-results"),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await?;
    // `get` tells a missing `failed_reason` key from a null one.
    let mut rows: Vec<_> = body
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("expected samples array: {body}"))?
        .iter()
        .map(|r| {
            (
                r["id"].clone(),
                r["failed"].clone(),
                r.get("failed_reason").cloned(),
            )
        })
        .collect();
    // The route promises no row order.
    rows.sort_by(|a, b| a.0.as_str().cmp(&b.0.as_str()));
    assert_eq!(
        rows,
        vec![
            (json!("s1"), json!(false), Some(serde_json::Value::Null)),
            (
                json!("s2"),
                json!(true),
                Some(json!("llama-server crashed mid-completion"))
            ),
            (json!("s3"), json!(false), Some(serde_json::Value::Null)),
        ]
    );
    Ok(())
}

/// A failed eval is processed with no sample results written, so the route
/// answers as for any job that has none, not as a server fault.
#[tokio::test]
async fn test_eval_sample_results_404_for_failed_job() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (sk, client_id) = register_and_approve(&state).await?;

    let claimed = "job-550e8400-e29b-41d4-a716-446655440000";
    seed_leased(
        dir.path(),
        claimed,
        &client_id,
        &failure_job_body(claimed, "eval_test", &[client_id.as_str()]),
    )?;
    let job_id = submit_benchmark(
        &state,
        &sk,
        &client_id,
        &json!({
            "message_type": "failure",
            "job_id": claimed,
            "benchmark_id": "eval_test",
            "retriable": false,
            "failure_reason": "runtime OOM",
        }),
    )
    .await?;
    score::run_process_submissions(&state.config, build_local_fs_stores(&state.config)?).await?;

    // Processed, so the 404 below is the failure's and not the unscored job's.
    let record = build_local_fs_stores(&state.config)?
        .submissions
        .find_job(&job(claimed)?)
        .await?
        .ok_or_else(|| anyhow::anyhow!("expected an archived failure"))?;
    assert_eq!(record.state, pipette_store::JobState::Processed);

    let resp = authed_get(
        &state,
        &sk,
        &client_id,
        &format!("/jobs/{job_id}/eval-sample-results"),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    let body = body_json(resp).await?;
    assert!(
        body["error"]
            .as_str()
            .context("a refusal carries an error string")?
            .contains("not found")
    );
    Ok(())
}

/// A failure past the warehouse lock window is archived unrecorded, and is just
/// as missing its sample results as a recorded one.
#[tokio::test]
async fn test_eval_sample_results_404_for_failure_archived_unrecorded() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (sk, client_id) = register_and_approve(&state).await?;

    // Four days back: its partition closed a day ago under the 72-hour window.
    let submitted_at = chrono::Utc::now() - chrono::TimeDelta::days(4);
    let job_id = job("job-archived-failure")?;
    let stores = build_local_fs_stores(&state.config)?;
    stores
        .submissions
        .write_incoming(
            &job_id,
            &json!({
                "message_type": "failure",
                "benchmark_id": "eval_test",
                "benchmark_type": "eval",
                "client_id": client_id.as_str(),
                "job_id": job_id.as_str(),
                "submitted_at": submitted_at.to_rfc3339(),
                "retriable": false,
                "failure_reason": "runtime OOM",
            }),
        )
        .await?;
    score::run_process_submissions(&state.config, build_local_fs_stores(&state.config)?).await?;

    let record = stores
        .submissions
        .find_job(&job_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("expected an archived failure"))?;
    assert_eq!(record.state, pipette_store::JobState::Processed);

    let resp = authed_get(
        &state,
        &sk,
        &client_id,
        &format!("/jobs/{job_id}/eval-sample-results"),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    let body = body_json(resp).await?;
    assert!(
        body["error"]
            .as_str()
            .context("a refusal carries an error string")?
            .contains("not found")
    );
    Ok(())
}

/// Submissions from before the gateway rejected duplicate ids may still sit in
/// `incoming/`; the scorer keeps the first of each so the job drains.
#[tokio::test]
async fn test_scorer_dedupes_duplicate_completion_ids_in_legacy_submission() -> anyhow::Result<()> {
    let evals = mock_evals_server().await;

    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state_with_evals_url(dir.path(), &evals.base_url()).await?;
    let (sk, client_id) = register_and_approve(&state).await?;

    // Written directly: the HTTP handler now rejects duplicate ids.
    let job_id = "legacy-dup-job";
    let key = job(job_id)?;
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

/// What score-eval posts to `/score`, byte for byte: a claimed job's samples
/// carry `stop_reason` and the body the job's `enable_thinking`; an ad-hoc
/// submission with no stop reason posts the body without either key.
#[rstest]
#[case::claimed_thinking_job(
    Some(json!({"benchmark_type": "eval", "model_type": "gguf_text", "enable_thinking": true})),
    json!([
        {"id": "s1", "completion": "4", "stop_reason": "truncated", "completion_tokens": 100},
        {"id": "s2", "completion": "", "stop_reason": "failure", "stop_detail": "crashed"},
        {"id": "s3", "completion": "blue"},
    ]),
    r#"{"eval_id":"test_eval","dataset_name":"test_ds","completions":[{"id":"s1","completion":"4","stop_reason":"truncated"},{"id":"s2","completion":"","stop_reason":"failure"},{"id":"s3","completion":"blue"}],"enable_thinking":true}"#
)]
#[case::ad_hoc_without_stop_reasons(
    None,
    json!([{"id": "s1", "completion": "4"}]),
    r#"{"eval_id":"test_eval","dataset_name":"test_ds","completions":[{"id":"s1","completion":"4"}]}"#
)]
fn test_eval_scoring_forwards_stop_reason_and_enable_thinking(
    #[case] job_model_flags: Option<serde_json::Value>,
    #[case] completions: serde_json::Value,
    #[case] expected_body: &str,
) -> anyhow::Result<()> {
    run_mocked(assert_score_request_body(
        job_model_flags,
        completions,
        expected_body,
    ))
}

async fn assert_score_request_body(
    job_model_flags: Option<serde_json::Value>,
    completions: serde_json::Value,
    expected_body: &str,
) -> anyhow::Result<()> {
    let evals = MockServer::start_async().await;
    let score = evals
        .mock_async(|when, then| {
            when.method(POST).path("/score").body(expected_body);
            then.respond_with(mock_score);
        })
        .await;

    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state_with_evals_url(dir.path(), &evals.base_url()).await?;
    let (sk, client_id) = register_and_approve(&state).await?;

    let mut body = json!({
        "benchmark_id": "eval_test",
        "device_name": "test-device",
        "device_form_factor": "embedded",
        "device_os_name": "Linux",
        "device_os_version": "22.04",
        "device_chip_model": "test-chip",
        "device_ram_bytes": 17179869184i64,
        "model_name": "llama-3.2-1b",
        "model_quant": "q4_0",
        "runtime_name": "llama.cpp",
        "runtime_version": "b5000",
        // The client's own report, which mgmt does not forward.
        "model_flags": "enable_thinking=false",
        "completions": completions,
    });
    if let Some(model_flags) = job_model_flags {
        let claimed = "job-550e8400-e29b-41d4-a716-446655440000";
        // A plan-authored group, axis keys included.
        seed_leased(
            dir.path(),
            claimed,
            &client_id,
            &json!({
                "job_id": claimed,
                "spec": {
                    "benchmark": "eval_test",
                    "model": {"type": "gguf_text", "path": "m.gguf"},
                    "runtime": {"type": "llamacpp_cli_stock_tools"},
                    "model_flags": model_flags,
                },
            }),
        )?;
        body.as_object_mut()
            .context("the submission is a JSON object")?
            .insert("job_id".to_string(), json!(claimed));
    }
    let job_id = submit_benchmark(&state, &sk, &client_id, &body).await?;

    run_full_score(&state.config).await?;

    // A body that differs by a byte misses the matcher, answers 404, and
    // leaves the job unscored.
    score.assert_async().await;
    let resp = authed_get(&state, &sk, &client_id, &format!("/jobs/{job_id}")).await?;
    assert_eq!(
        body_json(resp).await?.get("status"),
        Some(&json!("processed"))
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

    // A JSON *string*, keys out of order and spaced: ingest must canonicalize
    // it.
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

    // Canonical in the warehouse: keys sorted, no whitespace.
    let expected_canonical = concat!(
        r#"{"filename":"LFM2.5-VL-450M-Q4_0.gguf","mmproj_filename":"mmproj-f16.gguf","#,
        r#""org":"LiquidAI","repo_name":"LFM2.5-VL-450M-GGUF","type":"hf_gguf_vision"}"#
    );
    // model_descriptor_sha256 is derived mgmt-side as the sha256 of the
    // *canonical* descriptor string (not the raw wire bytes).
    let expected_sha = pipette_core::canonical_json::sha256_hex(expected_canonical);
    // The harness configuration takes the same treatment — canonical form and a
    // sha256 of *that*, so two runs measured the same way group together
    // however each client happened to format its payload.
    let expected_flags_canonical = concat!(
        r#"{"http_timeout_seconds":1800,"readiness":"#,
        r#"{"max_wait_secs":300,"skip_thermal":true}}"#
    );
    let expected_flags_sha = pipette_core::canonical_json::sha256_hex(expected_flags_canonical);
    // `model_flags` / `runtime_flags` get the same canonical-plus-hash
    // treatment, except that the non-JSON spelling is passed through trimmed
    // rather than rejected or mangled.
    let expected_model_flags = r#"{"cache":false,"enable_thinking":true}"#;
    let expected_model_flags_sha = pipette_core::canonical_json::sha256_hex(expected_model_flags);
    let expected_runtime_flags = "--n-gpu-layers 999";
    let expected_runtime_flags_sha =
        pipette_core::canonical_json::sha256_hex(expected_runtime_flags);
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

#[tokio::test]
async fn test_vl_throughput_end_to_end_latency_takes_the_client_total() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (sk, client_id) = register_and_approve(&state).await?;

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
            "runtime_name": "llama.cpp",
            "runtime_version": "b8683",
            "prompt_tokens": 75,
            "prompt_ms": 352.3,
            "prompt_ms_stddev": 3.8,
            "predicted_ms": 32.7,
            "predicted_ms_stddev": 1.5,
            "total_time_ms": 394.1,
            "total_time_ms_stddev": 4.2
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
    let metric = |name: &str| {
        metrics
            .iter()
            .find(|m| m["metric"] == name)
            .ok_or_else(|| anyhow::anyhow!("{name} metric not found"))
    };
    let number = |m: &serde_json::Value, field: &str| {
        m[field]
            .as_f64()
            .ok_or_else(|| anyhow::anyhow!("{} missing {field}", m["metric"]))
    };
    let end_to_end = metric("end_to_end_latency")?;
    assert!((number(end_to_end, "value")? - 394.1).abs() < 0.01);
    assert!((number(end_to_end, "value_stddev")? - 4.2).abs() < 0.01);
    assert!(metric("e2e_latency").is_err());
    assert!((number(metric("ttft")?, "value")? - 352.3).abs() < 0.01);
    Ok(())
}

// ---------------------------------------------------------------------------
// ASR: submit, score, and the warehouse row
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_asr_throughput_scores_and_records_its_token_counts() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    std::fs::write(
        dir.path()
            .join("benchmarks/asr_throughput_9400ms_text1024_decode128.toml"),
        "benchmark_type = \"asr_throughput\"\n\
         parameter_audio_duration_ms = 9400\n\
         parameter_audio_sample_rate_hz = 16000\n\
         parameter_audio_channels = 1\n\
         parameter_text_tokens = 1024\n\
         parameter_decode_tokens = 128\n",
    )?;
    let state = make_state(dir.path()).await?;
    let (sk, client_id) = register_and_approve(&state).await?;

    let body = |extra: serde_json::Value| -> anyhow::Result<serde_json::Value> {
        let mut base = json!({
            "benchmark_id": "asr_throughput_9400ms_text1024_decode128",
            "device_name": "test-device",
            "device_form_factor": "embedded",
            "device_os_name": "Linux",
            "device_os_version": "22.04",
            "device_chip_model": "test-chip",
            "device_ram_bytes": 17179869184i64,
            "model_name": "LiquidAI/LFM2.5-Audio-1.5B-GGUF",
            "model_quant": "Q8_0",
            "model_params_total_millions": 1500,
            "runtime_name": "llama.cpp",
            "runtime_version": "b11256",
            "asr_prompt_tokens": 1161,
            "asr_audio_tokens": 118,
            "asr_prompt_ms": 83.3,
            "asr_prompt_ms_stddev": 0.9,
            "asr_predicted_ms": 129.4,
            "asr_predicted_ms_stddev": 1.1,
            "total_time_ms": 230.0,
            "total_time_ms_stddev": 2.0,
            "client_extras": r#"{"asr_context_sizing":"exact","asr_audio_tokens_source":"calibrated","asr_beyond_model_audio_limit":false}"#
        });
        base.as_object_mut()
            .ok_or_else(|| anyhow::anyhow!("the body is an object"))?
            .extend(
                extra
                    .as_object()
                    .ok_or_else(|| anyhow::anyhow!("extras are an object"))?
                    .clone(),
            );
        Ok(base)
    };

    let job_id = submit_benchmark(&state, &sk, &client_id, &body(json!({}))?).await?;
    score::run_process_submissions(&state.config, build_local_fs_stores(&state.config)?).await?;

    let resp = authed_get(&state, &sk, &client_id, &format!("/jobs/{job_id}")).await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let job = body_json(resp).await?;
    assert_eq!(job["status"], "processed");
    let values: std::collections::BTreeMap<String, f64> = job["metrics"]
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("missing metrics array"))?
        .iter()
        .filter_map(|m| Some((m["metric"].as_str()?.to_owned(), m["value"].as_f64()?)))
        .collect();
    assert_eq!(
        values.keys().map(String::as_str).collect::<Vec<_>>(),
        [
            "decode_throughput",
            "end_to_end_latency",
            "prefill_throughput",
            "ttft"
        ]
    );
    // The same metrics as vl_throughput: no sum of the two phases.
    assert!((values["ttft"] - 83.3).abs() < 0.1);
    assert!((values["prefill_throughput"] - 1161.0 / 83.3 * 1000.0).abs() < 1.0);
    assert!((values["decode_throughput"] - 128.0 / 129.4 * 1000.0).abs() < 1.0);
    assert!((values["end_to_end_latency"] - 230.0).abs() < 0.1);

    // The token counts are observation columns on the stored rows, not metrics,
    // and the clip's length and format and the text beside it are parameter
    // columns.
    let data_dir = state
        .config
        .storage
        .data_dir()
        .ok_or_else(|| anyhow::anyhow!("expected local_fs config"))?;
    let partition = warehouse::warehouse_day_partition_dir(
        &data_dir.join("warehouse/results"),
        &BenchmarkId::try_new("asr_throughput_9400ms_text1024_decode128")?,
        &client_id,
        &chrono::Utc::now().format("%Y-%m-%d").to_string(),
    );
    let rows = [
        ("observation_asr_throughput_prefill_tokens", Some(1161)),
        ("observation_asr_throughput_audio_tokens", Some(118)),
        ("parameter_audio_duration_ms", Some(9400)),
        ("parameter_audio_sample_rate_hz", Some(16_000)),
        ("parameter_audio_channels", Some(1)),
        ("parameter_text_tokens", Some(1024)),
        ("parameter_decode_tokens", Some(128)),
    ]
    .into_iter()
    .map(|(column, expected)| assert_i32_column(&partition, column, expected))
    .collect::<anyhow::Result<Vec<_>>>()?;
    assert!(rows.iter().all(|&n| n == 4), "one row per metric: {rows:?}");
    // How the row was produced, typed out of client_extras into its own columns.
    [
        ("observation_asr_context_sizing", "exact"),
        ("observation_asr_audio_tokens_source", "calibrated"),
    ]
    .into_iter()
    .try_for_each(|(column, expected)| {
        assert_eq!(assert_string_column(&partition, column, Some(expected))?, 4);
        anyhow::Ok(())
    })?;
    assert_eq!(
        assert_bool_column(
            &partition,
            "observation_asr_beyond_model_audio_limit",
            Some(false)
        )?,
        4
    );

    // A row that does not say how its context was sized is turned away.
    let refused = body(json!({ "client_extras": r#"{"asr_audio_tokens_source":"calibrated"}"# }))?;
    let response = authed_post(&state, &sk, &client_id, "/benchmarks", &refused).await?;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let refusal = body_json(response).await?;
    assert!(
        refusal["error"]
            .as_str()
            .is_some_and(|e| e.contains("client_extras.asr_context_sizing")),
        "{refusal}"
    );
    Ok(())
}

#[tokio::test]
async fn test_asr_max_memory_scores_a_peak_and_records_the_clip() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    std::fs::write(
        dir.path()
            .join("benchmarks/asr_max_memory_9400ms_text1024_decode256.toml"),
        "benchmark_type = \"asr_max_memory\"\n\
         parameter_audio_duration_ms = 9400\n\
         parameter_audio_sample_rate_hz = 48000\n\
         parameter_audio_channels = 2\n\
         parameter_text_tokens = 1024\n\
         parameter_decode_tokens = 256\n",
    )?;
    let state = make_state(dir.path()).await?;
    let (sk, client_id) = register_and_approve(&state).await?;

    let body = json!({
        "benchmark_id": "asr_max_memory_9400ms_text1024_decode256",
        "device_name": "test-device",
        "device_form_factor": "embedded",
        "device_os_name": "Linux",
        "device_os_version": "22.04",
        "device_chip_model": "test-chip",
        "device_ram_bytes": 17179869184i64,
        "model_name": "LiquidAI/LFM2.5-Audio-1.5B-GGUF",
        "model_quant": "Q8_0",
        "model_params_total_millions": 1500,
        "runtime_name": "llama.cpp",
        "runtime_version": "b11256",
        "max_host_bytes": 2_193_148_256_i64,
        "client_extras": r#"{"asr_context_sizing":"upper_bound","asr_beyond_model_audio_limit":true}"#
    });
    let job_id = submit_benchmark(&state, &sk, &client_id, &body).await?;
    score::run_process_submissions(&state.config, build_local_fs_stores(&state.config)?).await?;

    let resp = authed_get(&state, &sk, &client_id, &format!("/jobs/{job_id}")).await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let job = body_json(resp).await?;
    assert_eq!(job["status"], "processed");
    let metrics = job["metrics"]
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("missing metrics array"))?;
    assert_eq!(metrics.len(), 1, "{metrics:?}");
    assert_eq!(metrics[0]["metric"], "max_host_usage");
    assert_eq!(metrics[0]["unit"], "bytes");
    // Metric values are f32 on the wire, so a byte count this large rounds.
    let peak = metrics[0]["value"]
        .as_f64()
        .ok_or_else(|| anyhow::anyhow!("the metric has a value"))?;
    assert!((peak - 2_193_148_256.0).abs() < 256.0, "{peak}");

    let data_dir = state
        .config
        .storage
        .data_dir()
        .ok_or_else(|| anyhow::anyhow!("expected local_fs config"))?;
    let partition = warehouse::warehouse_day_partition_dir(
        &data_dir.join("warehouse/results"),
        &BenchmarkId::try_new("asr_max_memory_9400ms_text1024_decode256")?,
        &client_id,
        &chrono::Utc::now().format("%Y-%m-%d").to_string(),
    );
    // A peak has no token observations; those belong to throughput.
    let rows = [
        ("parameter_audio_duration_ms", Some(9400)),
        ("parameter_audio_sample_rate_hz", Some(48_000)),
        ("parameter_audio_channels", Some(2)),
        ("parameter_text_tokens", Some(1024)),
        ("parameter_decode_tokens", Some(256)),
        ("observation_asr_throughput_audio_tokens", None),
    ]
    .into_iter()
    .map(|(column, expected)| assert_i32_column(&partition, column, expected))
    .collect::<anyhow::Result<Vec<_>>>()?;
    assert!(rows.iter().all(|&n| n == 1), "one row per metric: {rows:?}");
    // The sizing is stored as sent (a reader compares only `exact` peaks); a
    // peak has no audio-token count, so no source.
    assert_eq!(
        assert_string_column(
            &partition,
            "observation_asr_context_sizing",
            Some("upper_bound")
        )?,
        1
    );
    assert_eq!(
        assert_string_column(&partition, "observation_asr_audio_tokens_source", None)?,
        1
    );
    assert_eq!(
        assert_bool_column(
            &partition,
            "observation_asr_beyond_model_audio_limit",
            Some(true)
        )?,
        1
    );
    Ok(())
}

/// Score one `prefill_throughput` run with `extra` added to the standard body,
/// and return its warehouse partition. Hold the `TempDir` while reading it.
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

/// A top-level `{}` collapses to NULL, so "nothing reported" is one
/// `benchmark_flags_sha256` bucket; a nested empty object is a different claim
/// and is stored.
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

    let expected_sha = expected.map(pipette_core::canonical_json::sha256_hex);
    let checked = assert_string_column(&partition, "benchmark_flags", expected)?;
    assert_string_column(
        &partition,
        "benchmark_flags_sha256",
        expected_sha.as_deref(),
    )?;
    assert!(checked > 0, "expected at least one scored row");
    Ok(())
}

/// What reaches the warehouse for each shape of `client_configuration`: the
/// bytes the client sent, verbatim, and NULL when it sent none.
#[rstest]
#[case::absent(None, None)]
#[case::names_only(
    Some(r#"{"envs":["CUDA_VISIBLE_DEVICES"]}"#),
    Some(r#"{"envs":["CUDA_VISIBLE_DEVICES"]}"#)
)]
#[case::stored_as_sent(
    Some(r#"{ "envs": [ "CUDA_VISIBLE_DEVICES", "GGML_CUDA_ENABLE_UNIFIED_MEMORY=0" ] }"#),
    Some(r#"{ "envs": [ "CUDA_VISIBLE_DEVICES", "GGML_CUDA_ENABLE_UNIFIED_MEMORY=0" ] }"#)
)]
// Stored, where `benchmark_flags` collapses `{}` to NULL: no hash here, so no
// grouping bucket to keep to one spelling.
#[case::empty_object(Some("{}"), Some("{}"))]
#[tokio::test]
async fn test_client_configuration_reaches_the_warehouse(
    #[case] wire: Option<&str>,
    #[case] expected: Option<&str>,
) -> anyhow::Result<()> {
    let extra: Vec<(&str, serde_json::Value)> = wire
        .map(|w| vec![("client_configuration", json!(w))])
        .unwrap_or_default();
    let (_dir, partition) = score_one_submission(&extra).await?;

    let checked = assert_string_column(&partition, "client_configuration", expected)?;
    assert!(checked > 0, "expected at least one scored row");
    Ok(())
}

/// `client_extras` reaches the warehouse as the bytes the client sent, and NULL
/// when it sent none. The runtime output never does: it has no column.
#[rstest]
#[case::absent(None)]
#[case::object(Some(r#"{ "build": "nightly" }"#))]
#[case::asr_keys_on_a_text_row(Some(r#"{ "asr_context_sizing": "exact" }"#))]
#[tokio::test]
async fn test_client_extras_reaches_the_warehouse(
    #[case] wire: Option<&str>,
) -> anyhow::Result<()> {
    let mut extra = vec![("runtime_stderr", json!("warming up\n"))];
    extra.extend(wire.map(|w| ("client_extras", json!(w))));
    let (_dir, partition) = score_one_submission(&extra).await?;

    let checked = assert_string_column(&partition, "client_extras", wire)?;
    assert!(checked > 0, "expected at least one scored row");
    // Only an ASR row types its keys out of client_extras.
    assert_string_column(&partition, "observation_asr_context_sizing", None)?;
    Ok(())
}

/// `device_time_ms` reaches the warehouse as the client sent it, and NULL when
/// it sent none.
#[rstest]
#[case::absent(None)]
#[case::measured(Some(184_250))]
#[tokio::test]
async fn test_device_time_ms_reaches_the_warehouse(
    #[case] wire: Option<i64>,
) -> anyhow::Result<()> {
    let extra: Vec<(&str, serde_json::Value)> = wire
        .map(|w| ("device_time_ms", json!(w)))
        .into_iter()
        .collect();
    let (_dir, partition) = score_one_submission(&extra).await?;

    let checked = assert_i64_column(&partition, "device_time_ms", wire)?;
    assert!(checked > 0, "expected at least one scored row");
    Ok(())
}

/// Each device telemetry field reaches its own warehouse column as the bytes the
/// client sent, and NULL when it sent none.
#[rstest]
#[tokio::test]
async fn test_device_telemetry_reaches_the_warehouse(
    #[values(false, true)] sent: bool,
) -> anyhow::Result<()> {
    let telemetry = [
        (
            "device_hardware_readings",
            r#"[ {"iteration": 0, "sample_point": "before_rep", "linux_sysfs_thermal_zones": [{"zone": "thermal_zone0", "type": "cpu-thermal", "temp": 62300}]} ]"#,
        ),
        (
            "device_os_throttling_readings",
            r#"[ {"iteration": 0, "sample_point": "before_rep", "raspberry_pi_vcgencmd_get_throttled": "throttled=0x0"} ]"#,
        ),
        (
            "readiness_gate_waits",
            r#"[ {"iteration": 0, "poll_count": 1, "platform": "pi5", "phase": "proceeding", "elapsed_ms": 38} ]"#,
        ),
    ];
    let extra: Vec<(&str, serde_json::Value)> = telemetry
        .iter()
        .filter(|_| sent)
        .map(|(field, wire)| (*field, json!(wire)))
        .collect();
    let (_dir, partition) = score_one_submission(&extra).await?;

    telemetry.into_iter().try_for_each(|(field, wire)| {
        let checked = assert_string_column(&partition, field, sent.then_some(wire))?;
        assert!(checked > 0, "expected at least one scored row");
        Ok(())
    })
}

/// A row carries both the flags a run asked for and those it executed with,
/// which differ here on purpose. Each `_sha256` hashes its own canonical
/// string, so "requested" and "applied" are separate join keys.
#[tokio::test]
async fn test_requested_and_effective_flags_both_reach_the_warehouse() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (sk, client_id) = register_and_approve(&state).await?;

    let claimed = "job-550e8400-e29b-41d4-a716-446655440000";
    seed_leased(
        dir.path(),
        claimed,
        &client_id,
        &json!({
            "job_id": claimed,
            "spec": {
                "benchmark": "prefill_throughput_256",
                "model": {"type": "gguf_text", "path": "m.gguf"},
                "runtime": {"type": "llamacpp_cli_stock_tools"},
                "runtime_flags": {"runtime_type": "llamacpp_cli_stock_tools", "n_gpu_layers": 99},
            },
        }),
    )?;

    let mut body = json!({
        "job_id": claimed,
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
    // What the run applied: the discriminant is gone and a resolved thread count
    // is present.
    body["runtime_flags"] = json!(r#"{"n_gpu_layers":99,"threads":8}"#);
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

    let requested = r#"{"n_gpu_layers":99,"runtime_type":"llamacpp_cli_stock_tools"}"#;
    let effective = r#"{"n_gpu_layers":99,"threads":8}"#;
    assert_ne!(
        requested, effective,
        "the two sides must not be the same string"
    );

    let checked = assert_string_column(&partition, "requested_runtime_flags", Some(requested))?;
    assert_string_column(
        &partition,
        "requested_runtime_flags_sha256",
        Some(&pipette_core::canonical_json::sha256_hex(requested)),
    )?;
    assert_string_column(&partition, "runtime_flags", Some(effective))?;
    assert_string_column(
        &partition,
        "runtime_flags_sha256",
        Some(&pipette_core::canonical_json::sha256_hex(effective)),
    )?;
    // The axes the plan left unset record no request, while the run's own
    // model_flags stay absent too.
    assert_string_column(&partition, "requested_model_flags", None)?;
    assert_string_column(&partition, "requested_benchmark_flags", None)?;
    assert!(checked > 0, "expected at least one scored row");
    Ok(())
}

/// It stays NULL for a client that does not report it.
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

/// The per-run swap and host memory peaks reach every row, NULL when unsent, on
/// a `prefill_throughput` body: the scorer applies no per-type gate.
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

/// A stored submission without `model_params_total_millions` still parses, and
/// the scorer fills the row from the `model_name → mill_params` lookup.
#[tokio::test]
async fn test_scorer_backfills_model_params_from_lookup() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    setup_models(dir.path()).await?;
    let state = make_state(dir.path()).await?;
    let (_sk, client_id) = register_and_approve(&state).await?;

    // Written directly: the HTTP handler would reject a body without it.
    let job_id = "legacy-job";
    let key = job(job_id)?;
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
        "model_descriptor": "{\"org\":\"LiquidAI\",\"path\":\"LFM2-700M-Q4_0.gguf\",\"repo_name\":\"LFM2-700M-GGUF\",\"source\":\"huggingface\",\"type\":\"gguf_text\"}",
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

/// A run from a vault file imported from HuggingFace records the HuggingFace
/// model's descriptor, so it groups with runs from HuggingFace; the row keeps
/// the reported one too. An upload is recorded as reported, with the label the
/// device left out.
#[rstest]
#[case::imported(true)]
#[case::uploaded(false)]
#[tokio::test]
async fn test_a_vault_model_is_recorded_under_its_origin(
    #[case] imported: bool,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    setup_models(dir.path()).await?;
    let state = make_state(dir.path()).await?;
    assert_vault_model_recorded(&state, "prefill_throughput_256", imported).await
}

/// An eval run records the alias on the finalize pass.
#[tokio::test]
async fn test_an_eval_vault_model_is_recorded_under_its_origin() -> anyhow::Result<()> {
    let evals = mock_evals_server().await;
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    setup_models(dir.path()).await?;
    let state = make_state_with_evals_url(dir.path(), &evals.base_url()).await?;
    assert_vault_model_recorded(&state, "eval_test", true).await
}

/// A success body of `benchmark_id` for `job_id`, measured as its benchmark
/// type reports: an eval with completions, a prefill with its time.
fn submission_body(
    benchmark_id: &str,
    client_id: &str,
    job_id: &str,
    model_descriptor: &str,
) -> anyhow::Result<serde_json::Value> {
    let mut body = json!({
        "message_type": "success",
        "benchmark_id": benchmark_id,
        "client_id": client_id,
        "job_id": job_id,
        "submitted_at": "2026-01-01T00:00:00Z",
        "device_name": "test-device",
        "device_form_factor": "embedded",
        "device_os_name": "Linux",
        "device_os_version": "22.04",
        "device_chip_model": "test-chip",
        "device_ram_bytes": 17_179_869_184i64,
        "model_name": "m",
        "model_quant": "q4_0",
        "model_descriptor": model_descriptor,
        "runtime_name": "llama.cpp",
        "runtime_version": "b5000",
    });
    let measurement = if benchmark_id == "eval_test" {
        [
            ("benchmark_type", json!("eval")),
            (
                "completions",
                json!([
                    {"id": "s1", "completion": "4"},
                    {"id": "s2", "completion": "London"},
                    {"id": "s3", "completion": "blue"}
                ]),
            ),
        ]
    } else {
        [
            ("benchmark_type", json!("prefill_throughput")),
            ("prefill_time_ms", json!(34.7)),
        ]
    };
    body.as_object_mut()
        .context("the submission body is a JSON object")?
        .extend(measurement.map(|(key, value)| (key.to_string(), value)));
    Ok(body)
}

/// A re-run job's warehouse rows name the job it runs again, read off the
/// plan that minted it.
#[tokio::test]
async fn test_a_rerun_job_records_the_job_it_runs_again() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    setup_models(dir.path()).await?;
    let state = make_state(dir.path()).await?;
    assert_rerun_recorded(
        &state,
        "prefill_throughput_256",
        r#"{"batch_size":1,"benchmark_type":"prefill_throughput","prefill_tokens":256}"#,
    )
    .await
}

/// An eval re-run records its parent, and the entry's pair, on the finalize pass.
#[tokio::test]
async fn test_an_eval_rerun_job_records_the_job_it_runs_again() -> anyhow::Result<()> {
    let evals = mock_evals_server().await;
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    setup_models(dir.path()).await?;
    let state = make_state_with_evals_url(dir.path(), &evals.base_url()).await?;
    assert_rerun_recorded(
        &state,
        "eval_test",
        r#"{"benchmark_type":"eval","dataset_name":"test_ds","eval_id":"test_eval","max_tokens":100,"temperature":0.0}"#,
    )
    .await
}

/// Score one `benchmark_id` run of a job a plan re-ran, and check its rows name
/// the job it runs again and carry `params`, the entry's pair.
async fn assert_rerun_recorded(
    state: &AppState,
    benchmark_id: &str,
    params: &str,
) -> anyhow::Result<()> {
    let (_sk, client_id) = register_and_approve(state).await?;
    let stores = build_local_fs_stores(&state.config)?;

    let (rerun, parent) = (job("rerun-job")?, job("first-job")?);
    let manifest = pipette_core::plan::PlanManifest {
        rerun_of: std::collections::BTreeMap::from([(rerun.clone(), parent.clone())]),
        ..pipette_core::plan::sample_manifest(
            pipette_plan_types::PlanId::from_uuid(uuid::Uuid::now_v7()),
            pipette_plan_types::PlanStatus::Active,
            false,
        )?
    };
    stores.plans.put_plan(&manifest).await?;
    stores
        .plans
        .put_job_plan_link(&rerun, &manifest.plan_id)
        .await?;
    let descriptor = json!({
        "type": "gguf_text",
        "source": "huggingface",
        "org": "LiquidAI",
        "repo_name": "LFM2-700M-GGUF",
        "revision": "a".repeat(40),
        "path": "LFM2-700M-Q4_0.gguf",
    })
    .to_string();
    let body = submission_body(
        benchmark_id,
        client_id.as_str(),
        rerun.as_str(),
        &descriptor,
    )?;
    stores.submissions.write_incoming(&rerun, &body).await?;
    run_full_score(&state.config).await?;

    let data_dir = state
        .config
        .storage
        .data_dir()
        .context("expected local_fs config")?;
    let partition = warehouse::warehouse_day_partition_dir(
        &data_dir.join("warehouse/results"),
        &BenchmarkId::try_new(benchmark_id)?,
        &client_id,
        "2026-01-01",
    );
    let checked = assert_string_column(&partition, "rerun_of", Some(parent.as_str()))?;
    assert!(checked > 0, "expected at least one scored row");
    assert_string_column(&partition, "benchmark_params", Some(params))?;
    Ok(())
}

/// Score one `benchmark_id` run of a vault model reported without a label, and
/// check the columns that record it. `imported` gives the record a HuggingFace
/// origin; an upload is recorded under its record's name instead.
async fn assert_vault_model_recorded(
    state: &AppState,
    benchmark_id: &str,
    imported: bool,
) -> anyhow::Result<()> {
    let (_sk, client_id) = register_and_approve(state).await?;
    let stores = build_local_fs_stores(&state.config)?;

    let record = pipette_core::vault::VaultResource {
        origin: imported
            .then(pipette_store_testkit::a_huggingface_origin)
            .transpose()?,
        ..pipette_store_testkit::a_vault_resource()?
    };
    stores.vault.put_resource(&record).await?;
    let source = json!({
        "type": "gguf_text",
        "source": "vault_url",
        "url": format!("/vault/resources/{}/content", record.resource_id),
        "principal_id": record.owner.label(),
    });
    let reported = source.to_string();

    let job_id = "vault-job";
    stores
        .plans
        .put_job_source(&job(job_id)?, &json!({ "spec": { "model": source } }))
        .await?;
    let body = submission_body(benchmark_id, client_id.as_str(), job_id, &reported)?;
    stores
        .submissions
        .write_incoming(&job(job_id)?, &body)
        .await?;
    run_full_score(&state.config).await?;

    let data_dir = state
        .config
        .storage
        .data_dir()
        .context("expected local_fs config")?;
    let partition = warehouse::warehouse_day_partition_dir(
        &data_dir.join("warehouse/results"),
        &BenchmarkId::try_new(benchmark_id)?,
        &client_id,
        "2026-01-01",
    );
    let reported = pipette_core::canonical_json::canonicalize_str(&reported);
    let hf = pipette_core::canonical_json::canonicalize_str(
        &json!({
            "type": "gguf_text",
            "source": "huggingface",
            "org": "LiquidAI",
            "repo_name": "LFM2-700M-GGUF",
            "revision": "a".repeat(40),
            "path": "LFM2-700M-Q4_0.gguf",
        })
        .to_string(),
    );
    let mut labelled = source;
    labelled
        .as_object_mut()
        .context("the source is a JSON object")?
        .insert("custom_label".to_string(), json!(record.name.to_string()));
    let labelled = pipette_core::canonical_json::canonicalize_str(&labelled.to_string());
    // The label is no part of the id, so an upload keeps the reported text's.
    let (recorded, id, vault, params) = if imported {
        (&hf, &hf, Some(reported.as_str()), Some(700))
    } else {
        (&labelled, &reported, None, None)
    };
    let checked = assert_string_column(&partition, "model_descriptor", Some(recorded))?;
    assert!(checked > 0, "expected at least one scored row");
    assert_string_column(
        &partition,
        "model_descriptor_sha256",
        Some(&pipette_core::canonical_json::sha256_hex(id)),
    )?;
    assert_string_column(&partition, "model_vault_descriptor", vault)?;
    assert_i32_column(&partition, "model_params_total_millions", params)?;
    Ok(())
}

/// The scorer rewrites a known model's wrong `model_params_total_millions` to
/// the catalog value, for rows older code wrote without normalizing.
#[tokio::test]
async fn test_scorer_normalizes_wrong_model_params_for_known_model() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    setup_models(dir.path()).await?;
    let state = make_state(dir.path()).await?;
    let (_sk, client_id) = register_and_approve(&state).await?;

    let job_id = "wrong-value-job";
    let key = job(job_id)?;
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
        "model_descriptor": "{\"org\":\"LiquidAI\",\"path\":\"LFM2-700M-Q4_0.gguf\",\"repo_name\":\"LFM2-700M-GGUF\",\"source\":\"huggingface\",\"type\":\"gguf_text\"}",
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
/// overriding the client's wrong values — from the `model_descriptor`, with or
/// without a `model_name` and whatever that name says. `setup_models` maps
/// `LFM2-8B-A1B` to total 8340 / active 1500.
#[rstest]
#[case::via_descriptor(
    json!({"model_descriptor": "{\"org\":\"LiquidAI\",\"path\":\"LFM2-8B-A1B-Q4_0.gguf\",\"repo_name\":\"LFM2-8B-A1B-GGUF\",\"source\":\"huggingface\",\"type\":\"gguf_text\"}"})
)]
#[case::descriptor_with_a_matching_name(json!({
    "model_name": "LiquidAI/LFM2-8B-A1B",
    "model_quant": "q4_0",
    "model_descriptor": "{\"org\":\"LiquidAI\",\"path\":\"LFM2-8B-A1B-Q4_0.gguf\",\"repo_name\":\"LFM2-8B-A1B-GGUF\",\"source\":\"huggingface\",\"type\":\"gguf_text\"}"
}))]
#[case::descriptor_with_an_unlisted_name(json!({
    "model_name": "unlisted-alias",
    "model_descriptor": "{\"org\":\"LiquidAI\",\"path\":\"LFM2-8B-A1B-Q4_0.gguf\",\"repo_name\":\"LFM2-8B-A1B-GGUF\",\"source\":\"huggingface\",\"type\":\"gguf_text\"}"
}))]
#[tokio::test]
async fn test_scorer_resolves_moe_params_from_catalog(
    #[case] identity: serde_json::Value,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    setup_models(dir.path()).await?;
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

#[tokio::test]
async fn test_scorer_leaves_null_when_neither_catalog_nor_submission_has_values()
-> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    setup_models(dir.path()).await?; // catalog has LFM2-700M, llama-3.2-1b, LFM2-8B-A1B
    let state = make_state(dir.path()).await?;
    let (sk, client_id) = register_and_approve(&state).await?;

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
                    .context("total column must exist")?;
                let active_arr = batch
                    .column_by_name("model_params_active_millions")
                    .context("active column must exist")?;
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

#[tokio::test]
async fn test_run_score_drains_backlog_in_chunks() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let mut state = make_state(dir.path()).await?;
    // Force a small chunk size so we exercise the loop with a modest backlog.
    let mut config = (*state.config).clone();
    config.score_chunk_size = std::num::NonZeroUsize::new(2).context("2 is non-zero")?;
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

    // Poison pills: written directly, missing required fields.
    let stores = build_local_fs_stores(&state.config)?;
    let mut bad_job_ids = Vec::new();
    for i in 0..2 {
        let job_id = format!("poison-{i}");
        let key = job(&job_id)?;
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

/// It stays in `incoming/` for the next invocation.
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
    let key = job(job_id)?;
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

    score::run_process_submissions(&state.config, build_local_fs_stores(&state.config)?).await?;

    let stores = build_local_fs_stores(&state.config)?;
    let incoming = stores.submissions.list_incoming(TEST_LIST_LIMIT).await?;
    assert!(
        incoming.iter().any(|k| k.as_str() == job_id),
        "failed submission should remain in incoming/",
    );
    Ok(())
}

/// End-to-end through the split pipeline: `incoming/` → `to_do` (fast route) →
/// `to_finalize` (slow eval scoring) → `processed/` (fast finalize), with the
/// accuracy metric landing in the warehouse.
#[tokio::test]
async fn test_eval_pipeline_stages_through_score_queue() -> anyhow::Result<()> {
    let evals = mock_evals_server().await;

    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state_with_evals_url(dir.path(), &evals.base_url()).await?;
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

/// Evals that fail on every pass, however many and wherever they sort, never
/// hold back the scoring of the ones after them.
#[tokio::test]
async fn evals_that_keep_failing_do_not_stall_the_score_queue() -> anyhow::Result<()> {
    let evals = mock_evals_server().await;

    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let mut state = make_state_with_evals_url(dir.path(), &evals.base_url()).await?;
    let mut config = (*state.config).clone();
    config.score_chunk_size = std::num::NonZeroUsize::MIN;
    state.config = std::sync::Arc::new(config);
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
    score::run_process_submissions(&state.config, build_local_fs_stores(&state.config)?).await?;

    // Each sorts ahead of the good job.
    let stores = build_local_fs_stores(&state.config)?;
    let poison = json!({"benchmark_id": "eval_test", "client_id": client_id.as_str()});
    futures::stream::iter((0..1001).map(|i| job(format!("0-poison-{i:04}"))))
        .try_for_each(|poison_id| {
            let (stores, poison) = (&stores, &poison);
            async move {
                stores
                    .submissions
                    .enqueue(ScoreQueueStage::ToDo, &poison_id, poison)
                    .await
            }
        })
        .await?;

    score::run_score_eval(&state.config, build_local_fs_stores(&state.config)?).await?;

    let staged = build_local_fs_stores(&state.config)?
        .submissions
        .list_queue(ScoreQueueStage::ToFinalize, TEST_LIST_LIMIT)
        .await?;
    assert!(
        staged.iter().any(|j| j.as_str() == job_id),
        "the good eval is scored past the failing ones"
    );
    Ok(())
}

/// Both stages read as `scoring` with null score fields, not as a 404.
#[tokio::test]
async fn test_eval_in_the_score_queue_reads_as_scoring() -> anyhow::Result<()> {
    let evals = mock_evals_server().await;

    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state_with_evals_url(dir.path(), &evals.base_url()).await?;
    let (sk, client_id) = register_and_approve(&state).await?;

    let job_id = submit_benchmark(
        &state,
        &sk,
        &client_id,
        &eval_submission(json!([
            {"id": "s1", "completion": "4"},
            {"id": "s2", "completion": "London"},
            {"id": "s3", "completion": "blue"}
        ])),
    )
    .await?;

    // The fast pass routes the eval to `to_do`, then the slow pass stages it in
    // `to_finalize`. No finalize pass runs, so it never reaches `processed/`.
    for stage in [ScoreQueueStage::ToDo, ScoreQueueStage::ToFinalize] {
        let stores = build_local_fs_stores(&state.config)?;
        match stage {
            ScoreQueueStage::ToDo => score::run_process_submissions(&state.config, stores).await?,
            ScoreQueueStage::ToFinalize => score::run_score_eval(&state.config, stores).await?,
            ScoreQueueStage::Failures | ScoreQueueStage::Parked => {
                anyhow::bail!("{stage:?} is not a stage this test drives")
            }
        }
        assert!(
            build_local_fs_stores(&state.config)?
                .submissions
                .read_queue(stage, &job(&job_id)?)
                .await?
                .is_some(),
            "the eval must sit in {stage:?}"
        );

        let resp = authed_get(&state, &sk, &client_id, &format!("/jobs/{job_id}")).await?;
        assert_eq!(resp.status(), StatusCode::OK, "in {stage:?}");
        let body = body_json(resp).await?;
        assert_eq!(body["status"], "scoring", "in {stage:?}: {body}");
        assert_eq!(body["job_id"], job_id);
        assert_eq!(body["benchmark_id"], "eval_test");
        assert_eq!(body["benchmark_type"], "eval");
        let submitted_at = body["submitted_at"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("submitted_at must be a string: {body}"))?;
        chrono::DateTime::parse_from_rfc3339(submitted_at)
            .with_context(|| format!("submitted_at is not RFC 3339: {submitted_at}"))?;
        for field in ["scored_at", "score_runtime_version", "metrics"] {
            assert_eq!(
                body.get(field),
                Some(&serde_json::Value::Null),
                "{field} must be present and null in {stage:?}: {body}"
            );
        }
    }
    Ok(())
}

/// An `eval_test` body the mock scores (s1/s3 correct, s2 wrong), for staging
/// `score-queue` entries directly.
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

/// A job already staged in `to_finalize` is not re-sent to the scoring service:
/// the evals URL is dead, so a call would leave the job in `to_do`.
#[tokio::test]
async fn test_score_eval_skips_job_already_in_to_finalize() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state_with_evals_url(dir.path(), "http://127.0.0.1:1").await?;
    let stores = build_local_fs_stores(&state.config)?;
    let job = job("evaljob")?;

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

/// A job whose benchmark is not in the catalog stays in `to_do` while a healthy
/// one is staged for finalize.
#[tokio::test]
async fn test_score_eval_isolates_a_failing_to_do_job() -> anyhow::Result<()> {
    let evals = mock_evals_server().await;
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state_with_evals_url(dir.path(), &evals.base_url()).await?;
    let stores = build_local_fs_stores(&state.config)?;

    let good = job("good")?;
    let bad = job("bad")?;
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

/// A malformed `to_finalize` entry (no `score`) is deferred while a scored job
/// finalizes to `processed/`.
#[tokio::test]
async fn test_finalize_isolates_a_failing_job() -> anyhow::Result<()> {
    let evals = mock_evals_server().await;
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state_with_evals_url(dir.path(), &evals.base_url()).await?;
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
    let bad = job("badfinal")?;
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

async fn failed_attempts(
    submissions: &dyn pipette_store::SubmissionStore,
    job_id: &str,
) -> anyhow::Result<Option<u64>> {
    Ok(submissions
        .read_queue(ScoreQueueStage::Failures, &job(job_id)?)
        .await?
        .and_then(|f| f.get("attempts").and_then(serde_json::Value::as_u64)))
}

/// A scorer that echoes an id the job never sent passes `score-eval`, then
/// fails every finalize: those passes are counted too, and the job is parked.
#[tokio::test]
async fn an_eval_whose_score_never_finalizes_is_parked() -> anyhow::Result<()> {
    let evals = MockServer::start_async().await;
    evals
        .mock_async(|when, then| {
            when.method(POST).path("/score");
            then.status(200).json_body(json!({
                "runtime_version": "mock-v1.0.0",
                "scored_samples": [
                    {"id": "unknown", "messages": [], "completion": "4", "is_correct": true}
                ],
            }));
        })
        .await;
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state_with_evals_url(dir.path(), &evals.base_url()).await?;
    let (sk, client_id) = register_and_approve(&state).await?;
    let job_id = submit_benchmark(
        &state,
        &sk,
        &client_id,
        &eval_submission(json!([{"id": "s1", "completion": "4"}])),
    )
    .await?;
    let finalize_pass = || async {
        score::run_process_submissions(&state.config, build_local_fs_stores(&state.config)?).await
    };
    finalize_pass().await?;
    score::run_score_eval(&state.config, build_local_fs_stores(&state.config)?).await?;
    let stores = build_local_fs_stores(&state.config)?;
    let submissions = &*stores.submissions;
    let staged = submissions
        .read_queue(ScoreQueueStage::ToFinalize, &job(&job_id)?)
        .await?
        .context("the echo is not checked before finalize")?;

    finalize_pass().await?;
    let failures = submissions
        .read_queue(ScoreQueueStage::Failures, &job(&job_id)?)
        .await?
        .context("a failed finalize is counted")?;
    assert_eq!(failures["attempts"], 1);
    assert!(
        failures["error"]
            .as_str()
            .is_some_and(|e| e.contains("scorer response mismatch")),
        "{failures}"
    );

    // Skip ahead to the last attempt.
    submissions
        .enqueue(
            ScoreQueueStage::Failures,
            &job(&job_id)?,
            &json!({"attempts": score::MAX_SCORE_ATTEMPTS - 1, "error": "earlier"}),
        )
        .await?;
    finalize_pass().await?;
    assert!(
        submissions
            .read_queue(ScoreQueueStage::ToFinalize, &job(&job_id)?)
            .await?
            .is_none()
    );
    assert_eq!(
        submissions
            .read_queue(ScoreQueueStage::Parked, &job(&job_id)?)
            .await?
            .as_ref(),
        Some(&staged["submission"]),
        "the raw submission is parked, as from to_do"
    );
    assert_eq!(
        failed_attempts(submissions, &job_id).await?,
        Some(score::MAX_SCORE_ATTEMPTS)
    );
    let resp = authed_get(&state, &sk, &client_id, &format!("/jobs/{job_id}")).await?;
    assert_eq!(body_json(resp).await?["status"], "scoring");
    Ok(())
}

/// A `502` or `503` is the scorer's outage, not the job's: the pass pauses and
/// counts nothing, so an outage parks nothing. A `504`, like a timeout, may be
/// the job's own size: it is counted, then the pass pauses. Any other status
/// fails just that job.
#[rstest]
#[case::bad_gateway(502, [None, None])]
#[case::service_unavailable(503, [None, None])]
#[case::gateway_timeout(504, [Some(1), None])]
#[case::internal_error(500, [Some(1), Some(1)])]
#[tokio::test]
async fn a_scorer_error_status_counts_against_the_job_only_when_it_may_be_its_own(
    #[case] status: u16,
    #[case] want: [Option<u64>; 2],
) -> anyhow::Result<()> {
    let evals = MockServer::start_async().await;
    evals
        .mock_async(|when, then| {
            when.method(POST).path("/score");
            then.status(status).body("scorer says no");
        })
        .await;
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state_with_evals_url(dir.path(), &evals.base_url()).await?;
    let stores = build_local_fs_stores(&state.config)?;
    let submissions = &*stores.submissions;
    let jobs = ["a", "b"];
    futures::stream::iter(jobs.map(anyhow::Ok))
        .try_for_each(|id| async move {
            submissions
                .enqueue(
                    ScoreQueueStage::ToDo,
                    &job(id)?,
                    &stored_eval_body("c1", id),
                )
                .await
        })
        .await?;

    score::run_score_eval(&state.config, build_local_fs_stores(&state.config)?).await?;

    let got =
        futures::future::try_join_all(jobs.map(|id| failed_attempts(submissions, id))).await?;
    assert_eq!(got, want);
    assert_eq!(
        submissions
            .list_queue(ScoreQueueStage::ToDo, TEST_LIST_LIMIT)
            .await?
            .len(),
        2,
        "nothing is parked on a first failure"
    );
    Ok(())
}

/// A job whose call always times out is counted on each pass, though the pass
/// pauses after it, so it is parked and no longer holds back the jobs after it.
#[tokio::test]
async fn an_eval_that_always_times_out_is_parked() -> anyhow::Result<()> {
    let evals = MockServer::start_async().await;
    evals
        .mock_async(|when, then| {
            when.method(POST).path("/score");
            then.status(200).delay(std::time::Duration::from_secs(3));
        })
        .await;
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let mut state = make_state_with_evals_url(dir.path(), &evals.base_url()).await?;
    let mut config = (*state.config).clone();
    config.http_timeout_secs = 1;
    state.config = std::sync::Arc::new(config);
    let stores = build_local_fs_stores(&state.config)?;
    let submissions = &*stores.submissions;
    futures::stream::iter(["a", "b"].map(anyhow::Ok))
        .try_for_each(|id| async move {
            submissions
                .enqueue(
                    ScoreQueueStage::ToDo,
                    &job(id)?,
                    &stored_eval_body("c1", id),
                )
                .await
        })
        .await?;
    let pass = || async {
        score::run_score_eval(&state.config, build_local_fs_stores(&state.config)?).await
    };

    pass().await?;
    assert_eq!(failed_attempts(submissions, "a").await?, Some(1));
    assert_eq!(failed_attempts(submissions, "b").await?, None, "paused");

    submissions
        .enqueue(
            ScoreQueueStage::Failures,
            &job("a")?,
            &json!({"attempts": score::MAX_SCORE_ATTEMPTS - 1, "error": "earlier"}),
        )
        .await?;
    pass().await?;
    assert!(
        submissions
            .read_queue(ScoreQueueStage::Parked, &job("a")?)
            .await?
            .is_some()
    );
    pass().await?;
    assert_eq!(
        failed_attempts(submissions, "b").await?,
        Some(1),
        "the job after the parked one is reached"
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Failure rows
// ---------------------------------------------------------------------------

/// The warehouse partition a `(benchmark_id, client_id)` pair writes today.
fn today_partition(
    state: &AppState,
    benchmark_id: &str,
    client_id: &pipette_plan_types::ClientId,
) -> anyhow::Result<std::path::PathBuf> {
    let data_dir = state
        .config
        .storage
        .data_dir()
        .ok_or_else(|| anyhow::anyhow!("expected local_fs config"))?;
    Ok(warehouse::warehouse_day_partition_dir(
        &data_dir.join("warehouse/results"),
        &BenchmarkId::try_new(benchmark_id)?,
        client_id,
        &chrono::Utc::now().format("%Y-%m-%d").to_string(),
    ))
}

/// The facts a failure-row assertion reads off one warehouse row.
#[derive(Debug, Clone, PartialEq)]
struct RowFacts {
    metric: String,
    unit: String,
    /// `None` stands for SQL NULL.
    value: Option<f32>,
    failed: Option<bool>,
}

/// Read [`RowFacts`] for every row of a partition, across all its part files,
/// ordered by `metric` so a caller compares against a sorted expectation.
fn read_row_facts(partition: &std::path::Path) -> anyhow::Result<Vec<RowFacts>> {
    let mut out: Vec<RowFacts> = Vec::new();
    for path in partition_parquet_files(partition)? {
        let file = std::fs::File::open(&path)?;
        for batch in
            parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder::try_new(file)?.build()?
        {
            let batch = batch?;
            let str_col = |name: &str| -> anyhow::Result<arrow::array::StringArray> {
                Ok(batch
                    .column_by_name(name)
                    .ok_or_else(|| anyhow::anyhow!("{name} column must exist"))?
                    .as_any()
                    .downcast_ref::<arrow::array::StringArray>()
                    .ok_or_else(|| anyhow::anyhow!("{name} has unexpected type"))?
                    .clone())
            };
            let metrics = str_col("metric")?;
            let units = str_col("unit")?;
            let values = batch
                .column_by_name("value")
                .ok_or_else(|| anyhow::anyhow!("value column must exist"))?
                .as_any()
                .downcast_ref::<arrow::array::Float32Array>()
                .ok_or_else(|| anyhow::anyhow!("value has unexpected type"))?
                .clone();
            let failed = batch
                .column_by_name("failed")
                .ok_or_else(|| anyhow::anyhow!("failed column must exist"))?
                .as_any()
                .downcast_ref::<arrow::array::BooleanArray>()
                .ok_or_else(|| anyhow::anyhow!("failed has unexpected type"))?
                .clone();
            out.extend((0..batch.num_rows()).map(|i| RowFacts {
                metric: metrics.value(i).to_string(),
                unit: units.value(i).to_string(),
                value: (!values.is_null(i)).then(|| values.value(i)),
                failed: (!failed.is_null(i)).then(|| failed.value(i)),
            }));
        }
    }
    out.sort_by(|a, b| a.metric.cmp(&b.metric));
    Ok(out)
}

/// [`assert_column`] over a nullable boolean column.
fn assert_bool_column(
    partition: &std::path::Path,
    column: &str,
    expected: Option<bool>,
) -> anyhow::Result<usize> {
    assert_column::<arrow::array::BooleanArray, _>(partition, column, expected, |values, i| {
        values.value(i)
    })
}

/// A job body a plan would author: it names the benchmark, the model and
/// runtime specs, and the flag groups the job asks for.
fn failure_job_body(job_id: &str, benchmark_id: &str, clients: &[&str]) -> serde_json::Value {
    json!({
        "job_id": job_id,
        "clients": clients,
        "spec": {
            "benchmark": benchmark_id,
            "model": {"type": "gguf_text", "path": "m.gguf"},
            "runtime": {"type": "llamacpp_cli_stock_tools"},
            "model_flags": {"model_type": "gguf_text", "enable_thinking": true},
            "runtime_flags": {"runtime_type": "llamacpp_cli_stock_tools", "threads": 8},
        },
    })
}

/// A non-retriable client failure writes rows naming the metrics the job meant
/// to measure, with no measurement. The requested flags come from the claim,
/// the stamp a scored row gets, so a failure groups with its cell's successes.
#[tokio::test]
async fn test_client_failure_records_intended_metric_rows() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (sk, client_id) = register_and_approve(&state).await?;

    let claimed = "job-550e8400-e29b-41d4-a716-446655440000";
    seed_leased(
        dir.path(),
        claimed,
        &client_id,
        &failure_job_body(claimed, "prefill_throughput_256", &[client_id.as_str()]),
    )?;

    submit_benchmark(
        &state,
        &sk,
        &client_id,
        &json!({
            "message_type": "failure",
            "job_id": claimed,
            "benchmark_id": "prefill_throughput_256",
            "retriable": false,
            "failure_reason": "runtime crashed: OOM at decode step",
            "model_name": "m",
            "model_quant": "q",
            "runtime_name": "rt",
            "runtime_version": "v1",
            "client_version": "0.14.2",
        }),
    )
    .await?;
    score::run_process_submissions(&state.config, build_local_fs_stores(&state.config)?).await?;

    let partition = today_partition(&state, "prefill_throughput_256", &client_id)?;
    assert!(partition.exists(), "expected a failure partition");

    // One row per metric a prefill_throughput run intends to produce, each
    // naming the unit the definition fixes and carrying no measurement.
    assert_eq!(
        read_row_facts(&partition)?,
        vec![
            RowFacts {
                metric: "prefill_throughput".to_string(),
                unit: "tokens/sec".to_string(),
                value: None,
                failed: Some(true),
            },
            RowFacts {
                metric: "ttft".to_string(),
                unit: "ms".to_string(),
                value: None,
                failed: Some(true),
            },
        ]
    );

    let checked = assert_bool_column(&partition, "failed", Some(true))?;
    assert_eq!(checked, 2, "both intended metrics reached the warehouse");
    assert_bool_column(&partition, "failure_retriable", Some(false))?;
    assert_string_column(&partition, "failure_source", Some("client"))?;
    assert_string_column(
        &partition,
        "failed_reason",
        Some("runtime crashed: OOM at decode step"),
    )?;
    // Echoed from the submission: a failure reports which build failed.
    assert_string_column(&partition, "client_version", Some("0.14.2"))?;
    // Every row names its job, so a per-cell failure tally counts distinct
    // `job_id` rather than rows — a failure holds one row per intended metric.
    assert_string_column(&partition, "job_id", Some(claimed))?;
    // The flags the job asked for, canonicalized by the handler.
    assert_string_column(
        &partition,
        "requested_model_flags",
        Some(r#"{"enable_thinking":true,"model_type":"gguf_text"}"#),
    )?;
    assert_string_column(
        &partition,
        "requested_runtime_flags",
        Some(r#"{"runtime_type":"llamacpp_cli_stock_tools","threads":8}"#),
    )?;
    assert_string_column(&partition, "requested_benchmark_flags", None)?;
    Ok(())
}

/// A recorded failure leaves `incoming/` and lands in `processed/`, exactly as
/// a scored job does — `mark_processed` runs last, after the partition write.
#[tokio::test]
async fn test_recorded_failure_leaves_incoming_for_processed() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (sk, client_id) = register_and_approve(&state).await?;

    let claimed = "job-550e8400-e29b-41d4-a716-446655440000";
    seed_leased(
        dir.path(),
        claimed,
        &client_id,
        &failure_job_body(claimed, "prefill_throughput_256", &[client_id.as_str()]),
    )?;
    submit_benchmark(
        &state,
        &sk,
        &client_id,
        &json!({
            "message_type": "failure",
            "job_id": claimed,
            "benchmark_id": "prefill_throughput_256",
            "retriable": false,
            "failure_reason": "runtime OOM",
        }),
    )
    .await?;

    let stores = build_local_fs_stores(&state.config)?;
    let before = stores
        .submissions
        .find_job(&job(claimed)?)
        .await?
        .ok_or_else(|| anyhow::anyhow!("expected a stored failure"))?;
    assert_eq!(before.state, pipette_store::JobState::Incoming);

    score::run_process_submissions(&state.config, build_local_fs_stores(&state.config)?).await?;

    let after = stores
        .submissions
        .find_job(&job(claimed)?)
        .await?
        .ok_or_else(|| anyhow::anyhow!("expected an archived failure"))?;
    assert_eq!(after.state, pipette_store::JobState::Processed);
    // The scorer drained `incoming/`, so nothing is left for the next tick.
    assert!(
        stores
            .submissions
            .list_incoming(TEST_LIST_LIMIT)
            .await?
            .is_empty()
    );
    Ok(())
}

/// With no success-only field: the status follows the body, not the location.
#[tokio::test]
async fn test_archived_failure_still_reads_as_failed() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (sk, client_id) = register_and_approve(&state).await?;

    let claimed = "job-550e8400-e29b-41d4-a716-446655440000";
    seed_leased(
        dir.path(),
        claimed,
        &client_id,
        &failure_job_body(claimed, "prefill_throughput_256", &[client_id.as_str()]),
    )?;
    submit_benchmark(
        &state,
        &sk,
        &client_id,
        &json!({
            "message_type": "failure",
            "job_id": claimed,
            "benchmark_id": "prefill_throughput_256",
            "retriable": false,
            "failure_reason": "runtime OOM",
        }),
    )
    .await?;
    score::run_process_submissions(&state.config, build_local_fs_stores(&state.config)?).await?;

    // The route below reads the archive, not `incoming/`.
    let archived = build_local_fs_stores(&state.config)?
        .submissions
        .find_job(&job(claimed)?)
        .await?
        .ok_or_else(|| anyhow::anyhow!("expected an archived failure"))?;
    assert_eq!(archived.state, pipette_store::JobState::Processed);

    let resp = authed_get(&state, &sk, &client_id, &format!("/jobs/{claimed}")).await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await?;
    assert_eq!(body["job_id"], claimed);
    assert_eq!(body["status"], "failed", "{body}");
    assert_eq!(body["benchmark_id"], "prefill_throughput_256");
    assert_eq!(body["benchmark_type"], "prefill_throughput");
    assert_eq!(body["failure_reason"], "runtime OOM");
    for field in ["scored_at", "score_runtime_version", "metrics"] {
        assert!(
            body.get(field).is_none(),
            "{field} must be absent on a failed job: {body}"
        );
    }
    Ok(())
}

/// Which server-declared failure a test provokes.
#[derive(Debug, Clone, Copy)]
enum Retirement {
    Expired,
    AllClientsDenied,
    LeaseLost,
}

/// Each server-declared failure is told apart by `failure_source` alone, with
/// NULL telemetry and effective flags; the requested flags come off the `spec`.
#[rstest]
#[case::expired(Retirement::Expired, "expired")]
#[case::all_clients_denied(Retirement::AllClientsDenied, "all_clients_denied")]
#[case::lease_lost(Retirement::LeaseLost, "lease_lost")]
#[tokio::test]
async fn test_synthetic_failure_records_its_source(
    #[case] retirement: Retirement,
    #[case] expected_source: &str,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (_, client_id) = register_and_approve(&state).await?;

    let body = failure_job_body("job-1", "prefill_throughput_256", &[client_id.as_str()]);
    let expires_at = match retirement {
        Retirement::Expired => {
            pipette_plan_types::ExpiresAt::At(chrono::Utc::now() - chrono::Duration::days(1))
        }
        Retirement::AllClientsDenied | Retirement::LeaseLost => {
            pipette_plan_types::ExpiresAt::Never
        }
    };
    let seeded = seed_avail(dir.path(), "job-1", expires_at, &body)?;
    match retirement {
        Retirement::Expired => {}
        // The whole closed roster has denied it, so no eligible client is left.
        Retirement::AllClientsDenied => {
            state
                .todo_store
                .write_denied(&seeded, &client_id, &DenialRecord::default())
                .await?;
        }
        // Two leases already lost, and the current one has lapsed: the third.
        Retirement::LeaseLost => {
            let now = chrono::Utc::now();
            for hours in [3, 2] {
                state
                    .todo_store
                    .write_lost_lease(&seeded, &client_id, now - chrono::Duration::hours(hours))
                    .await?;
            }
            let claimed = state
                .todo_store
                .claim_job(
                    &seeded,
                    expires_at,
                    &client_id,
                    now - chrono::Duration::hours(1),
                )
                .await?;
            anyhow::ensure!(
                matches!(claimed, pipette_store::ClaimResult::Claimed(_)),
                "the job was queued"
            );
        }
    }

    run_qm(&state).await?;
    score::run_process_submissions(&state.config, build_local_fs_stores(&state.config)?).await?;

    // A synthetic failure is attributed to the reserved `system` client.
    let system = pipette_plan_types::ClientId::try_new("system")?;
    let partition = today_partition(&state, "prefill_throughput_256", &system)?;
    assert!(partition.exists(), "expected a synthetic failure partition");

    let checked = assert_string_column(&partition, "failure_source", Some(expected_source))?;
    assert_eq!(checked, 2, "both intended metrics reached the warehouse");
    assert_bool_column(&partition, "failed", Some(true))?;
    // No client declared a pair; the entry's is stamped.
    assert_string_column(
        &partition,
        "benchmark_params",
        Some(r#"{"batch_size":1,"benchmark_type":"prefill_throughput","prefill_tokens":256}"#),
    )?;
    // A synthetic failure is terminal by construction.
    assert_bool_column(&partition, "failure_retriable", Some(false))?;

    // No device ran the job.
    assert_string_column(&partition, "device_name", None)?;
    assert_string_column(&partition, "device_form_factor", None)?;
    assert_string_column(&partition, "device_os_name", None)?;
    assert_string_column(&partition, "device_os_version", None)?;
    assert_string_column(&partition, "device_chip_model", None)?;
    assert_i64_column(&partition, "device_ram_bytes", None)?;
    // No run reported a configuration, so only the requested side is populated.
    assert_string_column(&partition, "model_flags", None)?;
    assert_string_column(&partition, "runtime_flags", None)?;
    assert_string_column(&partition, "benchmark_flags", None)?;
    assert_string_column(
        &partition,
        "requested_model_flags",
        Some(r#"{"enable_thinking":true,"model_type":"gguf_text"}"#),
    )?;
    assert_string_column(
        &partition,
        "requested_runtime_flags",
        Some(r#"{"runtime_type":"llamacpp_cli_stock_tools","threads":8}"#),
    )?;
    Ok(())
}

/// A failure body stored without `failure_source` records rather than aborting:
/// the reserved `"system"` id records NULL, any other id a client report.
#[rstest]
#[case::legacy_client_body("ev1_legacy", Some("client"))]
#[case::legacy_synthetic_body("system", None)]
#[tokio::test]
async fn test_backlog_failure_body_resolves_its_source(
    #[case] client_id: &str,
    #[case] expected_source: Option<&str>,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;

    // Written straight to `incoming/`, with no `failure_source` key at all.
    let job_id = job("job-backlog")?;
    let stores = build_local_fs_stores(&state.config)?;
    stores
        .submissions
        .write_incoming(
            &job_id,
            &json!({
                "message_type": "failure",
                "benchmark_id": "prefill_throughput_256",
                "benchmark_type": "prefill_throughput",
                "client_id": client_id,
                "job_id": job_id.as_str(),
                "submitted_at": chrono::Utc::now().to_rfc3339(),
                "retriable": false,
                "failure_reason": "a body written before failure_source existed",
            }),
        )
        .await?;

    score::run_process_submissions(&state.config, build_local_fs_stores(&state.config)?).await?;

    let owner = pipette_plan_types::ClientId::try_new(client_id)?;
    let partition = today_partition(&state, "prefill_throughput_256", &owner)?;
    assert!(partition.exists(), "the backlog body records, not aborts");
    let checked = assert_string_column(&partition, "failure_source", expected_source)?;
    assert_eq!(checked, 2, "both intended metrics reached the warehouse");
    assert_bool_column(&partition, "failed", Some(true))?;
    Ok(())
}

/// A failure body past the warehouse lock window is archived unrecorded: its
/// row would target a closed partition, whose refused write aborts the chunk
/// and would wedge every later run. A body inside the window records as normal.
#[rstest]
// Four days back: its partition closed a day ago under the 72-hour window.
#[case::past_the_window(4, false)]
// Inside the window, so it records as any fresh failure does.
#[case::inside_the_window(0, true)]
#[tokio::test]
async fn test_failure_past_the_lock_window_archives_unrecorded(
    #[case] days_back: i64,
    #[case] want_recorded: bool,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;

    let submitted_at = chrono::Utc::now() - chrono::TimeDelta::days(days_back);
    let client_id = pipette_plan_types::ClientId::try_new("ev1_backlog")?;
    let job_id = job("job-lock-window")?;
    let stores = build_local_fs_stores(&state.config)?;
    stores
        .submissions
        .write_incoming(
            &job_id,
            &json!({
                "message_type": "failure",
                "benchmark_id": "prefill_throughput_256",
                "benchmark_type": "prefill_throughput",
                "client_id": client_id.as_str(),
                "job_id": job_id.as_str(),
                "submitted_at": submitted_at.to_rfc3339(),
                "retriable": false,
                "failure_reason": "runtime OOM",
            }),
        )
        .await?;

    score::run_process_submissions(&state.config, build_local_fs_stores(&state.config)?).await?;

    // Either way the body is archived, so the scorer never sees it again. This
    // is the property that keeps a closed-partition body from wedging the run.
    let after = stores
        .submissions
        .find_job(&job_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("expected the body to survive as an archive"))?;
    assert_eq!(after.state, pipette_store::JobState::Processed);
    assert!(
        stores
            .submissions
            .list_incoming(TEST_LIST_LIMIT)
            .await?
            .is_empty()
    );

    // The partition is addressed by the body's own day, not today's.
    let data_dir = state
        .config
        .storage
        .data_dir()
        .ok_or_else(|| anyhow::anyhow!("expected local_fs config"))?;
    let partition = warehouse::warehouse_day_partition_dir(
        &data_dir.join("warehouse/results"),
        &BenchmarkId::try_new("prefill_throughput_256")?,
        &client_id,
        &submitted_at.format("%Y-%m-%d").to_string(),
    );
    assert_eq!(
        partition.exists(),
        want_recorded,
        "a body past the lock window must leave its partition untouched"
    );
    if want_recorded {
        assert_bool_column(&partition, "failed", Some(true))?;
    }
    Ok(())
}

/// The intended metric set, and so the row count, is per benchmark type. A
/// failure cannot intend `max_gpu_usage` or `max_npu_usage`, as the definition
/// names no device. A failed eval is recorded here, never queued for the slow
/// scorer, which could not score it.
#[rstest]
#[case::prefill(
    "prefill_throughput_256",
    vec![("prefill_throughput", "tokens/sec"), ("ttft", "ms")],
)]
#[case::max_memory("max_memory_usage_256", vec![("max_host_usage", "bytes")])]
#[case::eval("eval_test", vec![("accuracy", "ratio")])]
#[tokio::test]
async fn test_intended_metric_rows_per_benchmark_type(
    #[case] benchmark_id: &str,
    #[case] expected: Vec<(&str, &str)>,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (sk, client_id) = register_and_approve(&state).await?;

    let claimed = "job-550e8400-e29b-41d4-a716-446655440000";
    seed_leased(
        dir.path(),
        claimed,
        &client_id,
        &failure_job_body(claimed, benchmark_id, &[client_id.as_str()]),
    )?;
    submit_benchmark(
        &state,
        &sk,
        &client_id,
        &json!({
            "message_type": "failure",
            "job_id": claimed,
            "benchmark_id": benchmark_id,
            "retriable": false,
            "failure_reason": "runtime OOM",
        }),
    )
    .await?;
    score::run_process_submissions(&state.config, build_local_fs_stores(&state.config)?).await?;

    let partition = today_partition(&state, benchmark_id, &client_id)?;
    assert_eq!(
        read_row_facts(&partition)?,
        expected
            .iter()
            .map(|(metric, unit)| RowFacts {
                metric: (*metric).to_string(),
                unit: (*unit).to_string(),
                value: None,
                failed: Some(true),
            })
            .collect::<Vec<_>>()
    );
    // The failure was recorded on the fast path, not handed to the slow scorer.
    assert!(
        build_local_fs_stores(&state.config)?
            .submissions
            .list_queue(ScoreQueueStage::ToDo, TEST_LIST_LIMIT)
            .await?
            .is_empty(),
        "a failure body must never reach the score queue"
    );
    Ok(())
}

/// `failed IS NOT TRUE`, the filter every pre-existing metric query must add,
/// returns exactly the measured rows, each with its value.
#[tokio::test]
async fn test_failed_is_not_true_selects_exactly_the_measured_rows() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (sk, client_id) = register_and_approve(&state).await?;

    // A measured run, submitted ad-hoc so it answers no job.
    submit_benchmark(
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
            "runtime_name": "rt",
            "runtime_version": "v1",
            "prefill_time_ms": 10.0,
        }),
    )
    .await?;

    // A failure of the same cell, so both land in one partition.
    let claimed = "job-550e8400-e29b-41d4-a716-446655440000";
    seed_leased(
        dir.path(),
        claimed,
        &client_id,
        &failure_job_body(claimed, "prefill_throughput_256", &[client_id.as_str()]),
    )?;
    submit_benchmark(
        &state,
        &sk,
        &client_id,
        &json!({
            "message_type": "failure",
            "job_id": claimed,
            "benchmark_id": "prefill_throughput_256",
            "retriable": false,
            "failure_reason": "runtime OOM",
        }),
    )
    .await?;
    score::run_process_submissions(&state.config, build_local_fs_stores(&state.config)?).await?;

    let partition = today_partition(&state, "prefill_throughput_256", &client_id)?;
    let rows = read_row_facts(&partition)?;
    assert_eq!(rows.len(), 4, "two measured rows and two failure rows");

    // `failed IS NOT TRUE` — the filter every pre-existing query must add.
    let measured: Vec<&RowFacts> = rows.iter().filter(|r| r.failed != Some(true)).collect();
    assert_eq!(measured.len(), 2, "the filter keeps only the measured rows");
    assert!(
        measured.iter().all(|r| r.value.is_some()),
        "a measured row still carries its value: {measured:?}"
    );
    let names: Vec<&str> = measured.iter().map(|r| r.metric.as_str()).collect();
    assert_eq!(names, vec!["prefill_throughput", "ttft"]);
    Ok(())
}
