//! `GET /datasets/{benchmark_id}`: a corpus served from the cache only on a
//! fresh `ok` probe verdict (`docs/design/vl-spec-decode-dataset-delivery.md`).
//! Each test writes the artifact and its verdict straight into the store; the
//! route never reaches an upstream.

use axum::body::to_bytes;
use axum::http::{StatusCode, header};
use chrono::{DateTime, Utc};
use rstest::rstest;
use sha2::{Digest, Sha256};

use pipette_core::dataset::{DatasetArtifact, ProbeState, ProbeVerdict};
use pipette_httpapi::state::AppState;
use pipette_plan_types::{ClientPermission, OrgId};
use pipette_web_testkit::{
    authed_get, body_json, json_str, make_state, own_client_by_org, register_and_approve,
    register_approve_and_grant, seed_org, setup_benchmarks, unauthed_get,
};

const BENCHMARK: &str = "vl_spec_decode_fixture_k8";
const TAR: &[u8] = b"not really a tar, but the route never looks inside";

fn digest() -> String {
    hex::encode(Sha256::digest(TAR))
}

/// The fixture entry, pinned to [`TAR`]'s digest.
fn fixture_toml() -> String {
    format!(
        r#"benchmark_type = "vl_spec_decode"
parameter_draft_length = 8
parameter_max_tokens = 2048

[dataset]
format  = "vl-spec-decode-corpus/1"
adapter = "mmspec@1"
digest  = "{digest}"
bytes   = {bytes}
subset  = "GQA"

[dataset.upstream]
source   = "git"
url      = "github.com/acme/corpus"
revision = "0123456789abcdef0123456789abcdef01234567"
identity = "2923603292c7dd463ad01a119f57965817778fc3"
digest   = "2222222222222222222222222222222222222222222222222222222222222222"
"#,
        digest = digest(),
        bytes = TAR.len()
    )
}

/// Write the fixture entry into the catalog the state reads: the public one,
/// or `org_id`'s own when one is given.
fn pin_benchmark(dir: &std::path::Path, org_id: Option<&OrgId>) -> anyhow::Result<()> {
    let catalog_dir = match org_id {
        Some(org_id) => dir.join("org-benchmarks").join(org_id.as_str()),
        None => dir.join("benchmarks"),
    };
    std::fs::create_dir_all(&catalog_dir)?;
    std::fs::write(
        catalog_dir.join(format!("{BENCHMARK}.toml")),
        fixture_toml(),
    )?;
    Ok(())
}

/// Write the tar and a verdict of `state`, last checked at `checked_at`.
async fn seed_artifact(
    app: &AppState,
    dir: &std::path::Path,
    state: ProbeState,
    checked_at: DateTime<Utc>,
) -> anyhow::Result<()> {
    let digest = pipette_plan_types::Sha256::try_new(digest())?;
    let source = dir.join("seed.tar");
    std::fs::write(&source, TAR)?;
    app.dataset_cache_store.put_tar(&digest, &source).await?;
    seed_verdict(app, state, checked_at).await
}

/// Write the artifact metadata and a verdict of `state` alone, with no tar
/// behind it.
async fn seed_verdict(
    app: &AppState,
    state: ProbeState,
    checked_at: DateTime<Utc>,
) -> anyhow::Result<()> {
    let digest = pipette_plan_types::Sha256::try_new(digest())?;
    let fixture = pipette_core::benchmark::Benchmark::from_toml(BENCHMARK, &fixture_toml())?;
    let dataset = fixture
        .dataset()
        .ok_or_else(|| anyhow::anyhow!("the fixture benchmark pins a dataset"))?;
    app.dataset_cache_store
        .put_artifact(&DatasetArtifact {
            digest,
            bytes: TAR.len() as u64,
            format: dataset.format,
            adapter: dataset.adapter,
            upstream: dataset.upstream.clone(),
            subset: dataset.subset.clone(),
            samples: None,
            built_at: checked_at,
            probe: ProbeVerdict {
                state,
                checked_at,
                since: checked_at,
                observed_identity: None,
                detail: None,
            },
        })
        .await
}

async fn setup() -> anyhow::Result<(
    tempfile::TempDir,
    AppState,
    ed25519_dalek::SigningKey,
    pipette_plan_types::ClientId,
)> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    pin_benchmark(dir.path(), None)?;
    let state = make_state(dir.path()).await?;
    let (key, client_id) = register_and_approve(&state).await?;
    Ok((dir, state, key, client_id))
}

/// A private benchmark's corpus is private where its definition is: served to
/// a device the organization holds, and `404` to any other — the same answer
/// `GET /benchmarks/{benchmark_id}` gives, so the corpus URL discloses nothing
/// the catalog did not.
#[rstest]
#[case::member(Some("acme"), StatusCode::OK, None)]
#[case::another_org(Some("labs"), StatusCode::NOT_FOUND, Some("benchmark not found"))]
#[case::unowned(None, StatusCode::NOT_FOUND, Some("benchmark not found"))]
#[tokio::test]
async fn a_private_corpus_follows_its_definition(
    #[case] holder: Option<&str>,
    #[case] status: StatusCode,
    #[case] message: Option<&str>,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let acme = seed_org(&state, "acme").await?;
    pin_benchmark(dir.path(), Some(&acme.org_id))?;
    let (key, client_id) = register_and_approve(&state).await?;
    if let Some(slug) = holder {
        let org_id = match slug {
            "acme" => acme.org_id.clone(),
            other => seed_org(&state, other).await?.org_id,
        };
        own_client_by_org(&state, &client_id, &org_id).await?;
    }
    seed_artifact(&state, dir.path(), ProbeState::Ok, Utc::now()).await?;

    let resp = authed_get(&state, &key, &client_id, &format!("/datasets/{BENCHMARK}")).await?;
    assert_eq!(resp.status(), status);
    match message {
        Some(message) => assert_eq!(body_json(resp).await?["error"], message),
        None => assert_eq!(to_bytes(resp.into_body(), usize::MAX).await?, TAR),
    }
    Ok(())
}

/// A fresh `ok` streams the tar with the headers a client verifies and must not
/// cache beyond the job.
#[tokio::test]
async fn serves_the_tar_on_a_fresh_ok() -> anyhow::Result<()> {
    let (dir, state, key, client_id) = setup().await?;
    seed_artifact(&state, dir.path(), ProbeState::Ok, Utc::now()).await?;

    let resp = authed_get(&state, &key, &client_id, &format!("/datasets/{BENCHMARK}")).await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let headers = resp.headers().clone();
    assert_eq!(headers[header::CONTENT_TYPE], "application/x-tar");
    assert_eq!(headers[header::CONTENT_LENGTH], TAR.len().to_string());
    assert_eq!(headers[header::ETAG], format!("\"{}\"", digest()));
    assert_eq!(headers[header::CACHE_CONTROL], "no-store");
    assert_eq!(to_bytes(resp.into_body(), usize::MAX).await?, TAR);
    Ok(())
}

/// The artifact under the pinned digest must have been built from this
/// entry's pin: an entry that copied another's digest is not served that
/// corpus on the strength of that corpus's probe.
#[tokio::test]
async fn refuses_an_artifact_built_from_another_pin() -> anyhow::Result<()> {
    let (dir, state, key, client_id) = setup().await?;
    seed_artifact(&state, dir.path(), ProbeState::Ok, Utc::now()).await?;
    let digest = pipette_plan_types::Sha256::try_new(digest())?;
    let mut artifact = state
        .dataset_cache_store
        .get_artifact(&digest)
        .await?
        .ok_or_else(|| anyhow::anyhow!("seeded"))?;
    artifact.upstream = serde_json::from_value(serde_json::json!({
        "source": "git",
        "url": "github.com/acme/other",
        "revision": "0123456789abcdef0123456789abcdef01234567",
        "identity": "2923603292c7dd463ad01a119f57965817778fc3",
        "digest": "2222222222222222222222222222222222222222222222222222222222222222",
    }))?;
    state.dataset_cache_store.put_artifact(&artifact).await?;

    let resp = authed_get(&state, &key, &client_id, &format!("/datasets/{BENCHMARK}")).await?;
    assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);
    assert_eq!(
        body_json(resp).await?["error"],
        "artifact does not match the pin"
    );
    Ok(())
}

/// A subset is part of the pin: an artifact built for another subset of the
/// same upstream is not this entry's. One recorded before subsets were reads
/// as before.
#[rstest]
#[case::another_subset(Some("TextVQA"), StatusCode::BAD_GATEWAY)]
#[case::unrecorded(None, StatusCode::OK)]
#[tokio::test]
async fn an_artifact_is_this_subsets(
    #[case] recorded: Option<&str>,
    #[case] status: StatusCode,
) -> anyhow::Result<()> {
    let (dir, state, key, client_id) = setup().await?;
    seed_artifact(&state, dir.path(), ProbeState::Ok, Utc::now()).await?;
    let digest = pipette_plan_types::Sha256::try_new(digest())?;
    let mut artifact = state
        .dataset_cache_store
        .get_artifact(&digest)
        .await?
        .ok_or_else(|| anyhow::anyhow!("seeded"))?;
    artifact.subset = recorded.map(str::to_owned);
    state.dataset_cache_store.put_artifact(&artifact).await?;

    let resp = authed_get(&state, &key, &client_id, &format!("/datasets/{BENCHMARK}")).await?;
    assert_eq!(resp.status(), status);
    Ok(())
}

const ASR_BENCHMARK: &str = "asr_eval_fixture";

/// An asr_eval entry stating `samples`, pinned to [`TAR`]'s digest.
fn asr_fixture_toml(samples: u32) -> String {
    format!(
        r#"benchmark_type = "asr_eval"
parameter_asr_task = "asr_fleurs_nl_nl"
parameter_dataset_version = "2026.10.1"
parameter_sample_count = {samples}
parameter_prompt_profile = "voxtral"
parameter_max_tokens = 500

[dataset]
format  = "asr-eval-corpus/1"
adapter = "evals-server@1"
digest  = "{digest}"
bytes   = {bytes}

[dataset.upstream]
source          = "evals_server"
eval_id         = "asr_fleurs_nl_nl"
dataset_version = "2026.10.1"
prompt_profile  = "voxtral"
identity        = "3333333333333333333333333333333333333333333333333333333333333333"

[recipe]
family  = "Test family"
models  = [{{ repo = "org/repo" }}]
enable_thinking = false
sources = ["https://huggingface.co/org/repo"]
"#,
        digest = digest(),
        bytes = TAR.len()
    )
}

/// An asr_eval entry is served its corpus only when the count the build
/// recorded is the one the entry states: a corpus that disagrees would be
/// downloaded and then refused on every device.
#[rstest]
#[case::matching(Some(3), StatusCode::OK)]
#[case::disagreeing(Some(4), StatusCode::BAD_GATEWAY)]
#[case::unrecorded(None, StatusCode::BAD_GATEWAY)]
#[tokio::test]
async fn an_asr_corpus_is_served_only_at_its_entrys_count(
    #[case] held: Option<u32>,
    #[case] status: StatusCode,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let catalog_dir = dir.path().join("benchmarks");
    std::fs::create_dir_all(&catalog_dir)?;
    std::fs::write(
        catalog_dir.join(format!("{ASR_BENCHMARK}.toml")),
        asr_fixture_toml(3),
    )?;
    let state = make_state(dir.path()).await?;
    let (key, client_id) = register_and_approve(&state).await?;
    let fixture =
        pipette_core::benchmark::Benchmark::from_toml(ASR_BENCHMARK, &asr_fixture_toml(3))?;
    let dataset = fixture
        .dataset()
        .ok_or_else(|| anyhow::anyhow!("the fixture benchmark pins a dataset"))?;
    let source = dir.path().join("seed.tar");
    std::fs::write(&source, TAR)?;
    state
        .dataset_cache_store
        .put_tar(&dataset.digest, &source)
        .await?;
    let now = Utc::now();
    state
        .dataset_cache_store
        .put_artifact(&DatasetArtifact {
            digest: dataset.digest.clone(),
            bytes: dataset.bytes,
            format: dataset.format,
            adapter: dataset.adapter,
            upstream: dataset.upstream.clone(),
            subset: None,
            samples: held,
            built_at: now,
            probe: ProbeVerdict {
                state: ProbeState::Ok,
                checked_at: now,
                since: now,
                observed_identity: None,
                detail: None,
            },
        })
        .await?;

    let resp = authed_get(
        &state,
        &key,
        &client_id,
        &format!("/datasets/{ASR_BENCHMARK}"),
    )
    .await?;
    assert_eq!(resp.status(), status);
    if status == StatusCode::OK {
        assert_eq!(to_bytes(resp.into_body(), usize::MAX).await?, TAR);
    } else {
        assert_eq!(
            body_json(resp).await?["error"],
            "artifact does not hold the entry's samples"
        );
    }
    Ok(())
}

/// Every refusal is decided from the metadata, carries a fixed message, and
/// only the transient ones ask the client to retry, one probe tick later. A
/// stale verdict is stale whatever it says, never a permanent-looking `502`.
#[rstest]
#[case::unavailable(
    ProbeState::Unavailable,
    0,
    StatusCode::SERVICE_UNAVAILABLE,
    "upstream unavailable"
)]
#[case::gone(
    ProbeState::Gone,
    0,
    StatusCode::BAD_GATEWAY,
    "upstream no longer backs the pin"
)]
#[case::moved(
    ProbeState::Moved,
    0,
    StatusCode::BAD_GATEWAY,
    "upstream no longer backs the pin"
)]
#[case::stale_ok(
    ProbeState::Ok,
    600,
    StatusCode::SERVICE_UNAVAILABLE,
    "upstream check stale"
)]
#[case::stale_unavailable(
    ProbeState::Unavailable,
    600,
    StatusCode::SERVICE_UNAVAILABLE,
    "upstream check stale"
)]
#[case::stale_gone(
    ProbeState::Gone,
    600,
    StatusCode::SERVICE_UNAVAILABLE,
    "upstream check stale"
)]
#[case::stale_moved(
    ProbeState::Moved,
    600,
    StatusCode::SERVICE_UNAVAILABLE,
    "upstream check stale"
)]
#[tokio::test]
async fn refuses_anything_but_a_fresh_ok(
    #[case] probe: ProbeState,
    #[case] age_secs: i64,
    #[case] status: StatusCode,
    #[case] message: &str,
) -> anyhow::Result<()> {
    let (dir, state, key, client_id) = setup().await?;
    let checked_at = Utc::now() - chrono::Duration::seconds(age_secs);
    seed_artifact(&state, dir.path(), probe, checked_at).await?;

    let resp = authed_get(&state, &key, &client_id, &format!("/datasets/{BENCHMARK}")).await?;
    assert_eq!(resp.status(), status);
    assert_eq!(
        resp.headers()
            .get(header::RETRY_AFTER)
            .map(|v| v.to_str())
            .transpose()?,
        (status == StatusCode::SERVICE_UNAVAILABLE).then_some("60")
    );
    assert_eq!(body_json(resp).await?["error"], message);
    Ok(())
}

/// No metadata for the pinned digest is a transient "not built", retried a
/// probe tick later, never a `404` that reads as a catalog miss.
#[tokio::test]
async fn refuses_an_artifact_never_built() -> anyhow::Result<()> {
    let (_dir, state, key, client_id) = setup().await?;
    let resp = authed_get(&state, &key, &client_id, &format!("/datasets/{BENCHMARK}")).await?;
    assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(resp.headers()[header::RETRY_AFTER], "60");
    assert_eq!(body_json(resp).await?["error"], "dataset not built");
    Ok(())
}

/// Metadata with a fresh `ok` whose tar is missing from the cache is "not
/// built" and retried, never a `500` or an empty `200`.
#[tokio::test]
async fn refuses_a_fresh_ok_whose_tar_is_missing() -> anyhow::Result<()> {
    let (_dir, state, key, client_id) = setup().await?;
    seed_verdict(&state, ProbeState::Ok, Utc::now()).await?;
    let digest = pipette_plan_types::Sha256::try_new(digest())?;
    assert!(
        state
            .dataset_cache_store
            .get_artifact(&digest)
            .await?
            .is_some(),
        "the metadata is there"
    );
    assert!(
        !state.dataset_cache_store.has_tar(&digest).await?,
        "the tar is not"
    );

    let resp = authed_get(&state, &key, &client_id, &format!("/datasets/{BENCHMARK}")).await?;
    assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(resp.headers()[header::RETRY_AFTER], "60");
    assert_eq!(body_json(resp).await?["error"], "dataset not built");
    Ok(())
}

/// A benchmark with no corpus, and one that does not exist, are both `404`,
/// each with its own documented message.
#[rstest]
#[case::no_dataset("prefill_throughput_256", "benchmark has no dataset")]
#[case::unknown("vl_spec_decode_nope_k8", "benchmark not found")]
#[tokio::test]
async fn not_found_without_a_dataset(
    #[case] benchmark: &str,
    #[case] message: &str,
) -> anyhow::Result<()> {
    let (_dir, state, key, client_id) = setup().await?;
    let resp = authed_get(&state, &key, &client_id, &format!("/datasets/{benchmark}")).await?;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    assert_eq!(body_json(resp).await?["error"], message);
    Ok(())
}

/// A malformed id is refused by the path, and only for a caller that signed:
/// an anonymous one gets the `401` first, so id validation tells it nothing.
#[tokio::test]
async fn refuses_a_malformed_id_after_authentication() -> anyhow::Result<()> {
    let (_dir, state, key, client_id) = setup().await?;
    let uri = "/datasets/.hidden";

    let resp = authed_get(&state, &key, &client_id, uri).await?;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let error = body_json(resp).await?;
    let message = json_str(&error, "error")?;
    assert!(
        message.contains("benchmark id must start with [A-Za-z0-9]"),
        "the id grammar refused it, not a lookup: {message}"
    );

    let resp = unauthed_get(&state, uri).await?;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(
        body_json(resp).await?["error"],
        "missing X-Client-Id header"
    );
    Ok(())
}

/// A corpus is work input: an anonymous caller and a client without
/// `perform_work` are refused before the metadata is read.
#[tokio::test]
async fn requires_a_working_client() -> anyhow::Result<()> {
    let (dir, state, _, _) = setup().await?;
    seed_artifact(&state, dir.path(), ProbeState::Ok, Utc::now()).await?;
    let uri = format!("/datasets/{BENCHMARK}");

    assert_eq!(
        unauthed_get(&state, &uri).await?.status(),
        StatusCode::UNAUTHORIZED
    );
    let (key, client_id) =
        register_approve_and_grant(&state, &[ClientPermission::SubmitResults]).await?;
    assert_eq!(
        authed_get(&state, &key, &client_id, &uri).await?.status(),
        StatusCode::FORBIDDEN
    );
    Ok(())
}

/// The benchmark detail tells a client where its corpus is.
#[tokio::test]
async fn benchmark_detail_links_the_dataset() -> anyhow::Result<()> {
    let (_dir, state, _, _) = setup().await?;
    let resp = unauthed_get(&state, &format!("/benchmarks/{BENCHMARK}")).await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await?;
    assert_eq!(body["dataset"]["url"], format!("/datasets/{BENCHMARK}"));
    assert_eq!(body["dataset"]["digest"], digest());
    Ok(())
}
