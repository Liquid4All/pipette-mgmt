//! The catalog import history route and its SQL-only gate.

use std::num::NonZeroUsize;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use axum::http::StatusCode;
use chrono::{DateTime, Duration, SubsecRound, Utc};
use rstest::rstest;

use pipette_plan_types::{CatalogImportRuns, CatalogName, UserPermission};
use pipette_store::{CatalogImportRunFinish, CatalogImportRunStore};
use pipette_web_testkit::{
    body_json, registered_user, seeded_sql_state, seeded_state, signed_empty, unauthed_get,
};

const ROUTE: &str = "/api/v1/descriptor-catalog/import-runs";

#[derive(Default)]
struct EmptyRuns(Mutex<Vec<usize>>);

impl EmptyRuns {
    fn limits(&self) -> anyhow::Result<std::sync::MutexGuard<'_, Vec<usize>>> {
        self.0
            .lock()
            .map_err(|_| anyhow::anyhow!("the recorded limits lock is poisoned"))
    }
}

#[async_trait]
impl CatalogImportRunStore for EmptyRuns {
    async fn recent_runs(&self, limit: NonZeroUsize) -> anyhow::Result<CatalogImportRuns> {
        self.limits()?.push(limit.get());
        Ok(CatalogImportRuns {
            items: Vec::new(),
            last_successful_import_at: None,
        })
    }

    async fn start_run(
        &self,
        _id: &str,
        _command: &str,
        _target: Option<&CatalogName>,
        _at: DateTime<Utc>,
    ) -> anyhow::Result<()> {
        Ok(())
    }

    async fn finish_run(
        &self,
        _id: &str,
        _at: DateTime<Utc>,
        _finish: &CatalogImportRunFinish,
    ) -> anyhow::Result<()> {
        Ok(())
    }
}

#[tokio::test]
async fn the_route_requires_a_fleet_reader_and_a_database() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let mut state = seeded_state(dir.path()).await?;
    assert_eq!(
        unauthed_get(&state, ROUTE).await?.status(),
        StatusCode::UNAUTHORIZED
    );
    let active = registered_user(&state, &[]).await?;
    assert_eq!(
        signed_empty(&state, &active, "GET", ROUTE).await?.status(),
        StatusCode::FORBIDDEN
    );
    let reader = registered_user(&state, &[UserPermission::ReadClients]).await?;
    assert_eq!(
        signed_empty(&state, &reader, "GET", ROUTE).await?.status(),
        StatusCode::NOT_FOUND
    );
    let runs = Arc::new(EmptyRuns::default());
    state.catalog_import_runs = Some(runs.clone());
    let response = signed_empty(&state, &reader, "GET", ROUTE).await?;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        body_json(response).await?,
        serde_json::json!({
            "items": [], "last_successful_import_at": null,
        })
    );
    assert_eq!(*runs.limits()?, vec![state.config.list_default_limit.get()]);
    Ok(())
}

#[rstest]
#[case::zero("limit=0")]
#[case::over_the_maximum("limit=201")]
#[case::not_a_number("limit=nope")]
#[tokio::test]
async fn a_bad_limit_is_refused_before_the_store(#[case] query: &str) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let mut state = seeded_state(dir.path()).await?;
    let runs = Arc::new(EmptyRuns::default());
    state.catalog_import_runs = Some(runs.clone());
    let reader = registered_user(&state, &[UserPermission::ReadClients]).await?;
    assert_eq!(
        signed_empty(&state, &reader, "GET", &format!("{ROUTE}?{query}"))
            .await?
            .status(),
        StatusCode::BAD_REQUEST
    );
    assert!(runs.limits()?.is_empty());
    Ok(())
}

#[tokio::test]
async fn recent_runs_include_refreshes_but_freshness_uses_only_import_successes()
-> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let Some(sql) = seeded_sql_state(dir.path()).await? else {
        return Ok(());
    };
    let state = &sql.state;
    let reader = registered_user(state, &[UserPermission::ReadClients]).await?;
    let store = state
        .catalog_import_runs
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("the SQL state has no catalog import run store"))?;
    let now = Utc::now().trunc_subsecs(6);
    let old = now - Duration::days(3);
    store.start_run("old-success", "import", None, old).await?;
    store
        .finish_run(
            "old-success",
            old + Duration::minutes(1),
            &CatalogImportRunFinish::default(),
        )
        .await?;
    for id in ["error-a", "error-b"] {
        store.start_run(id, "import", None, now).await?;
        store
            .finish_run(
                id,
                now + Duration::minutes(1),
                &CatalogImportRunFinish {
                    entries_hidden: 2,
                    unclassified_assets: vec!["b1/unknown.tar.gz".into()],
                    error_summary: Some("1 unclassified assets".into()),
                    ..Default::default()
                },
            )
            .await?;
    }
    store
        .start_run("refresh", "refresh", None, now + Duration::minutes(2))
        .await?;
    store
        .finish_run(
            "refresh",
            now + Duration::minutes(3),
            &CatalogImportRunFinish::default(),
        )
        .await?;

    let response = signed_empty(state, &reader, "GET", &format!("{ROUTE}?limit=2")).await?;
    assert_eq!(response.status(), StatusCode::OK);
    let body = body_json(response).await?;
    assert_eq!(body["items"][0]["id"], "refresh");
    assert_eq!(body["items"][1]["id"], "error-b");
    assert_eq!(body["items"][1]["entries_hidden"], 2);
    assert_eq!(body["items"][1]["outcome"], "unclassified");
    assert_eq!(
        body["items"][1]["unclassified_assets"],
        serde_json::json!(["b1/unknown.tar.gz"])
    );
    assert_eq!(
        body["last_successful_import_at"],
        serde_json::json!(old + Duration::minutes(1))
    );
    store
        .start_run("running", "import", None, now + Duration::minutes(4))
        .await?;
    let running =
        body_json(signed_empty(state, &reader, "GET", &format!("{ROUTE}?limit=1")).await?).await?;
    assert_eq!(running["items"][0]["result"], "running");
    assert!(running["items"][0]["finished_at"].is_null());
    Ok(())
}

#[tokio::test]
async fn an_empty_sql_history_has_no_successful_import() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let Some(sql) = seeded_sql_state(dir.path()).await? else {
        return Ok(());
    };
    let reader = registered_user(&sql.state, &[UserPermission::ReadClients]).await?;
    let response = signed_empty(&sql.state, &reader, "GET", ROUTE).await?;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        body_json(response).await?,
        serde_json::json!({
            "items": [], "last_successful_import_at": null,
        })
    );
    Ok(())
}
