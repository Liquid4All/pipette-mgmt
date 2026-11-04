//! The slow Jobs/Results timing line has its own test target because tracing
//! caches callsite interest process-wide.

#![expect(
    clippy::disallowed_macros,
    clippy::disallowed_methods,
    reason = "the mock pipette-duckdb response is JSON (see clippy.toml)"
)]

use std::time::Duration;

use anyhow::Context;
use axum::http::StatusCode;
use httpmock::{Method::POST, MockServer};
use serde_json::json;

use pipette_plan_types::PlanStatus;
use pipette_web_testkit::{
    approved_client, authed_user_get, body_json, capture_logs_at_level, created_campaign,
    registered_user_with_detail, with_duckdb,
};

/// A delayed warehouse read crosses the Info threshold; the same complete
/// plan's next read uses the cache, while an active plan reads the warehouse.
#[test]
fn slow_and_cached_plan_jobs_have_distinct_timing_lines() -> anyhow::Result<()> {
    let (outcome, logs) = capture_logs_at_level(tracing::Level::DEBUG, |runtime| {
        runtime.block_on(async {
            let dir = tempfile::tempdir()?;
            let server = MockServer::start_async().await;
            let (state, _, _) = approved_client(dir.path()).await?;
            let state = with_duckdb(state, &server.base_url())?;
            let signer = registered_user_with_detail(&state, true).await?;
            let (_, plan_id) = created_campaign(&state, &signer, &[("name", "Timed")]).await?;
            let uri = format!("/api/v1/plans/{plan_id}/jobs?results=true&all=true");
            let query = server
                .mock_async(|when, then| {
                    when.method(POST).path("/api/query");
                    then.delay(Duration::from_millis(1100))
                        .status(200)
                        .json_body(json!({
                            "columns": [], "rows": []
                        }));
                })
                .await;
            let mut manifest = state.plan_store.get_plan(&plan_id).await?.context("plan")?;
            manifest.status = PlanStatus::Complete;
            state.plan_store.put_plan(&manifest).await?;

            let get = || async {
                let response =
                    authed_user_get(&state, &signer.0, &signer.1, &signer.2, &uri).await?;
                anyhow::ensure!(response.status() == StatusCode::OK, "{}", response.status());
                body_json(response).await
            };
            let first = get().await?;
            let second = get().await?;
            anyhow::ensure!(first == second, "cache changed the response");
            anyhow::ensure!(
                query.calls_async().await == 1,
                "cache did not avoid a query"
            );

            manifest.status = PlanStatus::Active;
            state.plan_store.put_plan(&manifest).await?;
            let third = get().await?;
            anyhow::ensure!(third["results_read"] == "ok", "{third}");
            anyhow::ensure!(
                query.calls_async().await == 2,
                "active plan reused the cache"
            );
            anyhow::Ok(())
        })
    })?;
    outcome?;
    let lines: Vec<_> = logs
        .lines()
        .filter(|line| line.contains("plan jobs results read timing"))
        .collect();
    assert_eq!(lines.len(), 3, "{logs}");
    assert!(
        lines[0].contains(" INFO ") && lines[0].contains("cache_hit=false"),
        "{logs}"
    );
    assert!(
        lines[1].contains("DEBUG") && lines[1].contains("cache_hit=true"),
        "{logs}"
    );
    assert!(
        lines[2].contains(" INFO ") && lines[2].contains("cache_hit=false"),
        "{logs}"
    );
    for line in &lines {
        for field in [
            "total_ms=",
            "plan_access_ms=",
            "page_build_ms=",
            "controls_ms=",
            "document_ms=",
            "rerun_grant_ms=",
            "cells_ms=",
            "results_ms=",
            "cache_ms=",
            "duckdb_ms=",
            "history_ms=",
            "queue_ms=",
            "assembly_ms=",
            "names_ms=",
            "serialization_ms=",
            "jobs=",
            "rows=",
            "results_read=",
        ] {
            assert!(line.contains(field), "missing {field}: {line}");
        }
    }
    Ok(())
}
