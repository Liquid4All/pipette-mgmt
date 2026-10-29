//! The import against [`MockHub`]: each model arm and the alias of its vault
//! arm, the Hub refusals, the refusals of the import itself, the reuse of an
//! earlier import and of a shared source, the request permits, and the pinned
//! digests.

use std::collections::{BTreeMap, HashMap};
use std::num::{NonZeroU64, NonZeroUsize};
use std::pin::pin;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use bytes::Bytes;
use rstest::rstest;
use serde_json::{Value, json};
use sha2::Digest as _;

use pipette_core::types::Mint;
use pipette_hub_testkit::{
    BAD_TOKEN, GOOD_TOKEN, MockFile, MockGate, MockHub, MockRepo, PR_REF, RATE_LIMIT_RESET,
};
use pipette_plan_types::{AuthToken, OrgId, VaultFileFormat};
use pipette_store::drain_vault_resources;
use pipette_store_testkit::capture_logs;

use super::*;

fn commit(c: char) -> String {
    c.to_string().repeat(40)
}

/// The commit of each mock repository.
fn commit_of(repo: &str) -> String {
    match repo {
        "vl-gguf" => commit('a'),
        "dir" => commit('b'),
        "draft" => commit('c'),
        _ => commit('d'),
    }
}

fn sha(bytes: &[u8]) -> String {
    hex::encode(sha2::Sha256::digest(bytes))
}

const WEIGHTS: &[u8] = b"GGUF weights";

fn repos() -> HashMap<String, MockRepo> {
    let repo = |name: &str, files| {
        (
            format!("LiquidAI/{name}"),
            MockRepo {
                commit: commit_of(name),
                public: false,
                rate_limited: false,
                gate: MockGate::Open,
                files,
            },
        )
    };
    HashMap::from([
        repo(
            "vl-gguf",
            vec![
                MockFile::lfs("m.gguf", WEIGHTS),
                MockFile::lfs("mmproj/p.gguf", b"GGUF projector"),
                MockFile::lfs("d.gguf", b"GGUF drafter"),
                MockFile::lfs("q4/-lfm2.gguf", b"GGUF dashed"),
                MockFile::lfs("not.gguf", b"PK\x03\x04 a zip"),
                MockFile::lfs("sizeless.gguf", b"GGUF sizeless").sizeless(),
                MockFile::lfs("refused.gguf", b"GGUF refused").refused(),
                MockFile::git("small.gguf", b"GGUF in git"),
                MockFile::git("README.md", b"# vl"),
            ],
        ),
        repo(
            "dir",
            vec![
                MockFile::git("config.json", b"{}"),
                MockFile::lfs("model.safetensors", &[1; 700]),
                MockFile::git("4bit/config.json", b"{\"bits\":4}"),
                MockFile::lfs("4bit/model.safetensors", &[4; 300]),
            ],
        ),
        repo(
            "draft",
            vec![
                MockFile::git("config.json", b"{\"draft\":true}"),
                MockFile::lfs("model.safetensors", &[2; 100]),
            ],
        ),
        repo("long", vec![MockFile::git(&"a".repeat(300), b"x")]),
        repo("held", vec![MockFile::lfs("h.gguf", b"GGUF held").held()]),
        repo(
            "limited",
            vec![MockFile::lfs("l.gguf", b"GGUF limited").rate_limited()],
        ),
        repo(
            "broken",
            vec![
                MockFile::lfs("refused/model.safetensors", &[5; 10]).refused(),
                MockFile::lfs("digest/model.safetensors", &[6; 10]).serving(&[7; 10]),
                MockFile::git("length/config.json", b"{}").serving(b"{ }"),
            ],
        ),
    ])
}

/// A HuggingFace source in `LiquidAI/{repo}` with the good token, and the
/// fields of `extra`.
fn hf(repo: &str, extra: Value) -> Value {
    let mut source = json!({
        "source": "huggingface", "org": "LiquidAI", "repo_name": repo, "auth_token": GOOD_TOKEN,
    });
    if let (Some(source), Value::Object(extra)) = (source.as_object_mut(), extra) {
        source.extend(extra);
    }
    source
}

/// The files of `LiquidAI/{repo}` under `prefix`, with the prefix removed.
fn tree(repo: &str, prefix: Option<&str>) -> BTreeMap<String, Vec<u8>> {
    let under = prefix.map(|prefix| format!("{prefix}/"));
    repos()
        .remove(&format!("LiquidAI/{repo}"))
        .map(|repo| repo.files)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|file| {
            let name = match &under {
                Some(under) => file.path.strip_prefix(under.as_str())?.to_owned(),
                None => file.path,
            };
            Some((name, file.bytes))
        })
        .collect()
}

/// The files of the archive that `record` names.
async fn unpacked(
    vault: &dyn VaultStore,
    record: &VaultResource,
) -> anyhow::Result<BTreeMap<String, Vec<u8>>> {
    let body = vault
        .open_blob(&record.object_key, 0)
        .await?
        .context("no blob")?;
    let chunks: Vec<Bytes> = body.stream.try_collect().await?;
    let bytes = chunks.concat();
    let mut tar = tar::Archive::new(bytes.as_slice());
    tar.entries()?
        .map(|entry| {
            let mut entry = entry?;
            let name = entry.path()?.to_string_lossy().into_owned();
            let mut bytes = Vec::new();
            std::io::Read::read_to_end(&mut entry, &mut bytes)?;
            anyhow::Ok((name, bytes))
        })
        .collect()
}

fn typed(model_type: &str, mut fields: Value) -> Value {
    fields["type"] = json!(model_type);
    fields
}

/// A model of `model_type` with one HuggingFace source, as [`hf`] gives it.
fn hf_model(model_type: &str, repo: &str, extra: Value) -> anyhow::Result<Model> {
    Ok(serde_json::from_value(typed(model_type, hf(repo, extra)))?)
}

struct Fixture {
    _dir: tempfile::TempDir,
    vault: Arc<dyn VaultStore>,
    hub: MockHub,
    client: HubClient,
    owner: VaultOwner,
}

impl Fixture {
    async fn start() -> anyhow::Result<Self> {
        Self::with_requests(Config::default().hf_max_concurrent_requests).await
    }

    async fn with_requests(max_requests: NonZeroUsize) -> anyhow::Result<Self> {
        let dir = tempfile::tempdir()?;
        let config = pipette_core::config::Config {
            storage: pipette_core::config::StorageConfig::local_fs(dir.path().to_path_buf()),
            auth_storage: pipette_core::config::StorageConfig::local_fs(dir.path().to_path_buf()),
            ..Default::default()
        };
        let vault = pipette_store_fs::build_local_fs_stores(&config)?.vault;
        let hub = MockHub::start(repos()).await?;
        let client = HubClient::new(hub.endpoint.clone(), max_requests)?;
        let owner = VaultOwner::Org {
            org_id: OrgId::mint(),
        };
        Ok(Self {
            _dir: dir,
            vault,
            hub,
            client,
            owner,
        })
    }

    fn importer(&self, limit: u64) -> anyhow::Result<Importer<'_>> {
        Ok(Importer {
            hub: &self.client,
            vault: self.vault.as_ref(),
            blob_limit: NonZeroU64::new(limit).context("a zero limit")?,
        })
    }

    async fn import_with(
        &self,
        limit: u64,
        progress: &Progress,
        models: &[Model],
    ) -> anyhow::Result<Result<HashMap<Model, Model>, HfImportError>> {
        let importer = self.importer(limit)?;
        let plan = match ImportPlan::resolve(&importer, &self.owner, models).await {
            Ok(plan) => plan,
            Err(failure) => return Ok(Err(failure.error)),
        };
        Ok(plan
            .fetch(&importer, progress, Utc::now())
            .await
            .map_err(|failure| failure.error))
    }

    async fn import(&self, models: &[Model]) -> anyhow::Result<HashMap<Model, Model>> {
        Ok(self
            .import_with(u64::MAX, &Progress::Silent, models)
            .await??)
    }

    /// The bytes of each entry of the dry run of `models`, as [`fetch_bytes`].
    async fn planned_bytes(&self, models: &[Model]) -> anyhow::Result<Vec<Option<u64>>> {
        let importer = self.importer(u64::MAX)?;
        let plan = ImportPlan::resolve(&importer, &self.owner, models).await?;
        Ok(fetch_bytes(&plan))
    }

    async fn records(&self) -> anyhow::Result<Vec<VaultResource>> {
        Ok(drain_vault_resources(self.vault.as_ref()).await?.items)
    }
}

/// Every value under `key`, at any depth of `json`.
fn values_of<'a>(json: &'a Value, key: &str) -> Vec<&'a Value> {
    match json {
        Value::Object(map) => map
            .iter()
            .flat_map(|(k, v)| {
                let own = (k == key).then_some(v);
                own.into_iter().chain(values_of(v, key))
            })
            .collect(),
        Value::Array(items) => items.iter().flat_map(|v| values_of(v, key)).collect(),
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => Vec::new(),
    }
}

/// `raw` as a device that fetched it from HuggingFace reports it: each
/// repository pinned to its commit, with no token and no digest pins.
fn public_form(raw: &Value) -> Value {
    match raw {
        Value::Object(map) => {
            let mut map: serde_json::Map<String, Value> = map
                .iter()
                .filter(|(k, _)| *k != "auth_token" && !k.ends_with("sha256"))
                .map(|(k, v)| (k.clone(), public_form(v)))
                .collect();
            if let Some(repo) = map.get("repo_name").and_then(Value::as_str) {
                let pinned = json!(commit_of(repo));
                map.insert("revision".to_owned(), pinned);
            }
            Value::Object(map)
        }
        Value::Array(items) => Value::Array(items.iter().map(public_form).collect()),
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => raw.clone(),
    }
}

/// The arm names every record, pins every digest, names the owner and holds no
/// token; the scorer's alias of it is the HuggingFace form at the commit.
#[rstest]
#[case::gguf_text(
    typed("gguf_text", hf("vl-gguf", json!({"path": "m.gguf"}))),
    &[("vl-gguf", Some("m.gguf"))]
)]
#[case::gguf_vision(
    typed("gguf_vision", hf("vl-gguf", json!({"model": "m.gguf", "model_sha256": sha(WEIGHTS), "mmproj": "mmproj/p.gguf"}))),
    &[("vl-gguf", Some("m.gguf")), ("vl-gguf", Some("mmproj/p.gguf"))]
)]
#[case::gguf_vision_speculative(
    typed("gguf_vision_speculative", hf("vl-gguf", json!({"model": "m.gguf", "mmproj": "mmproj/p.gguf", "draft": "d.gguf"}))),
    &[("vl-gguf", Some("m.gguf")), ("vl-gguf", Some("mmproj/p.gguf")), ("vl-gguf", Some("d.gguf"))]
)]
#[case::gguf_vision_speculative_no_draft(
    typed("gguf_vision_speculative", hf("vl-gguf", json!({"model": "m.gguf", "mmproj": "mmproj/p.gguf"}))),
    &[("vl-gguf", Some("m.gguf")), ("vl-gguf", Some("mmproj/p.gguf"))]
)]
#[case::mlx(typed("mlx", hf("dir", json!({"prefix": "4bit"}))), &[("dir", Some("4bit"))])]
#[case::torch(typed("torch", hf("dir", json!({}))), &[("dir", None)])]
#[case::openvino(typed("openvino", hf("dir", json!({"prefix": "4bit"}))), &[("dir", Some("4bit"))])]
#[case::onnx_genai(typed("onnx_genai", hf("dir", json!({}))), &[("dir", None)])]
#[case::mlx_speculative(
    typed("mlx_speculative", json!({"target": hf("dir", json!({"prefix": "4bit"})), "draft": hf("draft", json!({}))})),
    &[("dir", Some("4bit")), ("draft", None)]
)]
#[case::torch_speculative(
    typed("torch_speculative", json!({"target": hf("dir", json!({})), "draft": hf("draft", json!({}))})),
    &[("dir", None), ("draft", None)]
)]
#[tokio::test]
async fn each_arm_imports_as_its_vault_arm(
    #[case] raw: Value,
    #[case] origins: &[(&str, Option<&str>)],
) -> anyhow::Result<()> {
    let fx = Fixture::start().await?;
    let model: Model = serde_json::from_value(raw.clone())?;
    let rewrites = fx.import(std::slice::from_ref(&model)).await?;
    let rewritten = rewrites.get(&model).context("no rewrite")?;
    let vault_arm = serde_json::to_value(rewritten)?;
    let text = vault_arm.to_string();

    assert_eq!(vault_arm["type"], raw["type"]);
    assert!(
        !text.contains("auth_token") && !text.contains("SECRET"),
        "{text}"
    );
    let principals = values_of(&vault_arm, "principal_id");
    assert!(!principals.is_empty(), "{text}");
    assert!(
        principals.iter().all(|p| **p == json!(fx.owner.label())),
        "{text}"
    );
    let sources = values_of(&vault_arm, "source");
    assert!(!sources.is_empty(), "{text}");
    assert!(
        sources
            .iter()
            .all(|s| **s == json!("vault_url") || **s == json!("vault_archive")),
        "{text}"
    );

    let mut records = fx.records().await?;
    records.sort_by_key(|record| record.own_file().map(|(role, _)| role));
    assert_eq!(records.len(), origins.len(), "{records:?}");
    for (record, (repo, path)) in records.iter().zip(origins) {
        let url = format!("/vault/resources/{}/content", record.resource_id);
        assert!(text.contains(&url), "{url} is missing from {text}");
        assert!(text.contains(record.sha256.as_ref()), "no pin in {text}");
        let origin = VaultOrigin::HuggingFace {
            org: HfOrg::try_new("LiquidAI")?,
            repo_name: HfRepoName::try_new(*repo)?,
            commit: GitOid::try_new(commit_of(repo))?,
            path: path.map(RepoSubpath::try_new).transpose()?,
        };
        assert_eq!(record.origin.as_ref(), Some(&origin));
        assert_eq!(record.name.as_ref(), origin.to_string());
        let (role, file) = record.own_file().context("no own file")?;
        match role.format() {
            VaultFileFormat::Gguf => {
                let path = path.context("a gguf record with no path")?;
                let last = path.rsplit('/').next().unwrap_or(path);
                assert_eq!(file.filename.as_ref(), last);
            }
            VaultFileFormat::Archive => {
                assert_eq!(file.filename.as_ref(), format!("{repo}.tar"));
                let files = unpacked(fx.vault.as_ref(), record).await?;
                assert_eq!(files, tree(repo, *path));
            }
        }
    }

    let none_known = crate::vault_alias::KnownOrigins::new();
    let alias = crate::vault_alias::model_alias(fx.vault.as_ref(), Some(&text), &none_known)
        .await?
        .context("no alias")?;
    let alias: Value = serde_json::from_str(&alias)?;
    assert_eq!(alias, public_form(&raw));
    Ok(())
}

/// No error text, `Debug` output or log line holds the token.
#[rstest]
#[case::no_token(None, "dir", HubRefusal::Unauthorized)]
#[case::bad_token(Some(BAD_TOKEN), "dir", HubRefusal::Forbidden)]
#[case::no_such_repo(Some(GOOD_TOKEN), "missing", HubRefusal::NotFound)]
fn a_hub_refusal_is_typed_and_names_no_token(
    #[case] token: Option<&str>,
    #[case] repo: &str,
    #[case] expected: HubRefusal,
) -> anyhow::Result<()> {
    let (outcome, logs) = capture_logs(|runtime| {
        runtime.block_on(async {
            let fx = Fixture::start().await?;
            let mut raw = typed("torch", hf(repo, json!({})));
            raw["auth_token"] = json!(token);
            let model: Model = serde_json::from_value(raw)?;
            let refused = fx
                .import_with(u64::MAX, &Progress::Silent, &[model])
                .await?;
            anyhow::Ok((refused.err(), fx.hub.downloads()))
        })
    })?;
    let (error, downloads) = outcome?;
    let error = error.context("the import ran")?;
    assert!(
        matches!(&error, HfImportError::Hub(HubError::Refused { status, .. }) if *status == expected),
        "{error:?}"
    );
    assert_eq!(downloads, 0);
    [error.to_string(), format!("{error:?}"), logs.clone()]
        .iter()
        .for_each(|text| assert!(!text.contains("SECRET"), "{text}"));
    assert!(logs.contains("hf import refused by hub"), "{logs}");
    assert!(
        logs.contains(&format!("status={}", expected.code())),
        "{logs}"
    );
    Ok(())
}

/// A bad token is refused even beside the same model with a good token.
#[tokio::test]
async fn each_token_is_checked() -> anyhow::Result<()> {
    let fx = Fixture::start().await?;
    let good: Model = hf_model("torch", "dir", json!({}))?;
    let mut bad = good.clone();
    if let Model::Torch(m) = &mut bad
        && let ModelSource::HuggingFace { repo, .. } = &mut m.source
    {
        repo.auth_token = Some(AuthToken::try_new(BAD_TOKEN)?);
    }
    let refused = fx
        .import_with(u64::MAX, &Progress::Silent, &[good, bad])
        .await?;
    assert!(
        matches!(
            refused,
            Err(HfImportError::Hub(HubError::Refused {
                status: HubRefusal::Forbidden,
                ..
            }))
        ),
        "{refused:?}"
    );
    Ok(())
}

/// The import refuses each of these before any download, and stores nothing.
#[rstest]
#[case::not_all_huggingface(
    typed("torch_speculative", json!({"target": hf("dir", json!({})), "draft": {"source": "absolute_dir", "dir": "/models/draft"}})),
    u64::MAX,
    |e: &HfImportError| matches!(e, HfImportError::NotAllHuggingFace { .. })
)]
#[case::pin_mismatch(
    typed("gguf_text", hf("vl-gguf", json!({"path": "m.gguf", "sha256": "0".repeat(64)}))),
    u64::MAX,
    |e: &HfImportError| matches!(e, HfImportError::PinMismatch { .. })
)]
#[case::no_lfs_digest(
    typed("gguf_text", hf("vl-gguf", json!({"path": "small.gguf"}))),
    u64::MAX,
    |e: &HfImportError| matches!(e, HfImportError::NoLfsDigest { .. })
)]
#[case::no_such_file(
    typed("gguf_text", hf("vl-gguf", json!({"path": "absent.gguf"}))),
    u64::MAX,
    |e: &HfImportError| matches!(e, HfImportError::NoSuchFile { .. })
)]
#[case::bad_file_name(
    typed("gguf_text", hf("vl-gguf", json!({"path": "q4/-lfm2.gguf"}))),
    u64::MAX,
    |e: &HfImportError| matches!(e, HfImportError::BadFileName { .. })
)]
#[case::missing_size(
    typed("gguf_text", hf("vl-gguf", json!({"path": "sizeless.gguf"}))),
    u64::MAX,
    |e: &HfImportError| matches!(e, HfImportError::MissingSize { .. })
)]
#[case::empty_snapshot(
    typed("mlx", hf("dir", json!({"prefix": "8bit"}))),
    u64::MAX,
    |e: &HfImportError| matches!(e, HfImportError::EmptySnapshot { .. })
)]
#[case::unsupported_path(
    typed("torch", hf("long", json!({}))),
    u64::MAX,
    |e: &HfImportError| matches!(e, HfImportError::UnsupportedPathError(_))
)]
#[case::gguf_too_large(
    typed("gguf_text", hf("vl-gguf", json!({"path": "m.gguf"}))),
    4,
    |e: &HfImportError| matches!(e, HfImportError::TooLarge { .. })
)]
#[case::archive_too_large(
    typed("torch", hf("dir", json!({}))),
    2048,
    |e: &HfImportError| matches!(e, HfImportError::TooLarge { .. })
)]
#[tokio::test]
async fn the_import_refuses_before_any_download(
    #[case] raw: Value,
    #[case] limit: u64,
    #[case] expected: fn(&HfImportError) -> bool,
) -> anyhow::Result<()> {
    let fx = Fixture::start().await?;
    let model: Model = serde_json::from_value(raw)?;
    let refused = fx.import_with(limit, &Progress::Silent, &[model]).await?;
    assert!(refused.as_ref().is_err_and(expected), "{refused:?}");
    assert_eq!(fx.hub.downloads(), 0);
    assert!(fx.records().await?.is_empty());
    Ok(())
}

/// A download that the Hub refuses, or whose bytes are not what the listing
/// gives, fails the import with a typed error and stores no record.
#[rstest]
#[case::gguf_refused(
    typed("gguf_text", hf("vl-gguf", json!({"path": "refused.gguf"}))),
    |e: &HfImportError| matches!(e, HfImportError::Hub(HubError::Refused { status: HubRefusal::Forbidden, .. }))
)]
#[case::archive_refused(
    typed("torch", hf("broken", json!({"prefix": "refused"}))),
    |e: &HfImportError| matches!(e, HfImportError::Hub(HubError::Refused { status: HubRefusal::Forbidden, .. }))
)]
#[case::archive_wrong_digest(
    typed("torch", hf("broken", json!({"prefix": "digest"}))),
    |e: &HfImportError| matches!(e, HfImportError::Body(BodyMismatchError::Digest { .. }))
)]
#[case::archive_wrong_length(
    typed("torch", hf("broken", json!({"prefix": "length"}))),
    |e: &HfImportError| matches!(e, HfImportError::Body(BodyMismatchError::Length { .. }))
)]
#[tokio::test]
async fn a_failed_download_stores_no_record(
    #[case] raw: Value,
    #[case] expected: fn(&HfImportError) -> bool,
) -> anyhow::Result<()> {
    let fx = Fixture::start().await?;
    let model: Model = serde_json::from_value(raw)?;
    let failed = fx
        .import_with(u64::MAX, &Progress::Silent, &[model])
        .await?;
    assert!(failed.as_ref().is_err_and(expected), "{failed:?}");
    assert!(fx.records().await?.is_empty());
    Ok(())
}

#[tokio::test]
async fn a_revision_with_a_slash_resolves() -> anyhow::Result<()> {
    let fx = Fixture::start().await?;
    let model: Model = hf_model("torch", "dir", json!({"revision": PR_REF}))?;
    fx.import(&[model]).await?;
    let records = fx.records().await?;
    let commits: Vec<_> = records
        .iter()
        .map(|record| match &record.origin {
            Some(VaultOrigin::HuggingFace { commit, .. }) => Some(commit.to_string()),
            _ => None,
        })
        .collect();
    assert_eq!(commits, [Some(commit_of("dir"))]);
    Ok(())
}

/// A gguf blob the owner holds with no record reports zero bytes; a file over
/// the limit and a path ustar cannot hold are refused while the plan resolves.
#[tokio::test]
async fn a_plan_reports_what_the_import_does() -> anyhow::Result<()> {
    let fx = Fixture::start().await?;
    let importer = fx.importer(u64::MAX)?;
    let model: Model = hf_model("gguf_text", "vl-gguf", json!({"path": "m.gguf"}))?;
    let models = [model.clone()];
    let entry = |action| PlannedImport {
        hf_ref: model.to_string(),
        action,
    };

    let plan = ImportPlan::resolve(&importer, &fx.owner, &models).await?;
    let first = ImportAction::Fetch {
        bytes: WEIGHTS.len() as u64,
    };
    assert_eq!(plan.entries(), [entry(first)]);
    plan.fetch(&importer, &Progress::Silent, Utc::now()).await?;

    let records = fx.records().await?;
    let resource_ids = records.iter().map(|r| r.resource_id.clone()).collect();
    let plan = ImportPlan::resolve(&importer, &fx.owner, &models).await?;
    assert_eq!(
        plan.entries(),
        [entry(ImportAction::Reuse { resource_ids })]
    );

    for record in &records {
        fx.vault.delete_resource(&record.resource_id).await?;
    }
    let plan = ImportPlan::resolve(&importer, &fx.owner, &models).await?;
    assert_eq!(plan.entries(), [entry(ImportAction::Fetch { bytes: 0 })]);

    let too_large = ImportPlan::resolve(&fx.importer(4)?, &fx.owner, &models).await;
    assert!(
        matches!(
            too_large,
            Err(ModelImportError {
                error: HfImportError::TooLarge { .. },
                ..
            })
        ),
        "{too_large:?}"
    );
    let long: Model = hf_model("torch", "long", json!({}))?;
    let unsupported = ImportPlan::resolve(&importer, &fx.owner, &[long]).await;
    assert!(
        matches!(
            unsupported,
            Err(ModelImportError {
                error: HfImportError::UnsupportedPathError(_),
                ..
            })
        ),
        "{unsupported:?}"
    );
    Ok(())
}

/// A record that goes between the two phases is fetched again, not reused.
#[tokio::test]
async fn a_reuse_checks_its_records_again_when_it_fetches() -> anyhow::Result<()> {
    let fx = Fixture::start().await?;
    let importer = fx.importer(u64::MAX)?;
    let model: Model = hf_model("gguf_text", "vl-gguf", json!({"path": "m.gguf"}))?;
    fx.import(std::slice::from_ref(&model)).await?;
    let downloads = fx.hub.downloads();

    let plan = ImportPlan::resolve(&importer, &fx.owner, std::slice::from_ref(&model)).await?;
    assert!(matches!(
        plan.entries().as_slice(),
        [PlannedImport {
            action: ImportAction::Reuse { .. },
            ..
        }]
    ));
    let gone = fx.records().await?;
    for record in &gone {
        fx.vault.delete_resource(&record.resource_id).await?;
    }
    let rewrites = plan.fetch(&importer, &Progress::Silent, Utc::now()).await?;

    let records = fx.records().await?;
    let [record] = records.as_slice() else {
        anyhow::bail!("not one record: {records:?}");
    };
    assert!(gone.iter().all(|g| g.resource_id != record.resource_id));
    let text = serde_json::to_string(rewrites.get(&model).context("no rewrite")?)?;
    assert!(text.contains(record.resource_id.as_ref()), "{text}");
    // The blob is still there, so the import writes a record and downloads nothing.
    assert_eq!(fx.hub.downloads(), downloads);
    Ok(())
}

/// A gguf file whose bytes are not a gguf is refused after its download, and
/// no record names it.
#[tokio::test]
async fn an_import_refuses_a_gguf_that_is_not_one() -> anyhow::Result<()> {
    let fx = Fixture::start().await?;
    let model: Model = hf_model("gguf_text", "vl-gguf", json!({"path": "not.gguf"}))?;
    let refused = fx
        .import_with(u64::MAX, &Progress::Silent, &[model])
        .await?;
    assert!(
        matches!(
            refused,
            Err(HfImportError::Vault(VaultAdminError::NotA { .. }))
        ),
        "{refused:?}"
    );
    assert!(fx.records().await?.is_empty());
    Ok(())
}

/// A deleted record is downloaded again; the CDN never sees the token, and the
/// log records each import without it.
#[test]
fn a_second_import_reuses_the_first() -> anyhow::Result<()> {
    let (outcome, logs) = capture_logs(|runtime| {
        runtime.block_on(async {
            let fx = Fixture::start().await?;
            let gguf: Model = hf_model("gguf_text", "vl-gguf", json!({"path": "m.gguf"}))?;
            let mlx: Model = hf_model("mlx", "dir", json!({"prefix": "4bit"}))?;
            let models = [gguf.clone(), mlx.clone()];

            let first = fx.import(&models).await?;
            let after_first = fx.hub.downloads();
            assert_eq!(after_first, 3, "one gguf file and two files under `4bit`");

            let second = fx.import(&models).await?;
            assert_eq!(fx.hub.downloads(), after_first);
            assert_eq!(second, first);

            let archive = fx
                .records()
                .await?
                .into_iter()
                .find(|r| {
                    r.own_file()
                        .is_some_and(|(role, _)| role == VaultFileRole::DirArchive)
                })
                .context("no archive record")?;
            fx.vault.delete_resource(&archive.resource_id).await?;
            let third = fx.import(&models).await?;
            assert_eq!(fx.hub.downloads(), after_first + 2);
            assert_eq!(third.get(&gguf), first.get(&gguf));
            assert_ne!(third.get(&mlx), first.get(&mlx));
            assert_eq!(fx.hub.cdn_authorized(), 0);
            anyhow::Ok(())
        })
    })?;
    outcome?;
    [
        "hf import started",
        "hf import stored record",
        "hf import reused record",
    ]
    .iter()
    .for_each(|event| assert!(logs.contains(event), "{event} is missing from {logs}"));
    assert!(!logs.contains("SECRET"), "{logs}");
    Ok(())
}

/// The bytes of each entry of `plan`, in plan order. A reuse has `None`.
fn fetch_bytes(plan: &ImportPlan) -> Vec<Option<u64>> {
    plan.entries()
        .iter()
        .map(|entry| match &entry.action {
            ImportAction::Fetch { bytes } => Some(*bytes),
            ImportAction::Reuse { .. } => None,
        })
        .collect()
}

/// Two models that share a source download it one time, in one plan or in two
/// imports. The dry run counts the source for the first model only. The records
/// of both models name one blob for the source.
#[rstest]
#[case::dir(
    typed("mlx", hf("dir", json!({"prefix": "4bit"}))),
    typed("mlx_speculative", json!({"target": hf("dir", json!({"prefix": "4bit"})), "draft": hf("draft", json!({}))})),
    &["4bit/config.json", "4bit/model.safetensors"]
)]
#[case::gguf(
    typed("gguf_text", hf("vl-gguf", json!({"path": "m.gguf"}))),
    typed("gguf_vision", hf("vl-gguf", json!({"model": "m.gguf", "mmproj": "mmproj/p.gguf"}))),
    &["m.gguf"]
)]
#[tokio::test]
async fn a_shared_source_is_handled_once(
    #[case] first: Value,
    #[case] second: Value,
    #[case] shared: &[&str],
    #[values(false, true)] separate_imports: bool,
) -> anyhow::Result<()> {
    let fx = Fixture::start().await?;
    let importer = fx.importer(u64::MAX)?;
    let first: Model = serde_json::from_value(first)?;
    let second: Model = serde_json::from_value(second)?;
    let first_alone = fx.planned_bytes(std::slice::from_ref(&first)).await?;
    let second_alone = fx.planned_bytes(std::slice::from_ref(&second)).await?;
    let (&[Some(first_bytes)], &[Some(second_bytes)]) =
        (first_alone.as_slice(), second_alone.as_slice())
    else {
        anyhow::bail!("not one fetch each: {first_alone:?} {second_alone:?}");
    };
    let unshared = second_bytes
        .checked_sub(first_bytes)
        .context("the second model has fewer bytes than the shared source")?;

    if separate_imports {
        fx.import(std::slice::from_ref(&first)).await?;
        assert_eq!(
            fx.planned_bytes(std::slice::from_ref(&second)).await?,
            [Some(unshared)]
        );
        assert_eq!(
            fx.planned_bytes(&[first.clone(), second.clone()]).await?,
            [None, Some(unshared)]
        );
        fx.import(std::slice::from_ref(&second)).await?;
    } else {
        let models = [first.clone(), second.clone()];
        let plan = ImportPlan::resolve(&importer, &fx.owner, &models).await?;
        assert_eq!(fetch_bytes(&plan), [Some(first_bytes), Some(unshared)]);
        plan.fetch(&importer, &Progress::Silent, Utc::now()).await?;
    }

    shared
        .iter()
        .for_each(|path| assert_eq!(fx.hub.downloads_of(path), 1, "{path}"));
    let records = fx.records().await?;
    let blobs = records
        .iter()
        .fold(HashMap::<_, HashSet<_>>::new(), |mut blobs, record| {
            blobs
                .entry(record.name.clone())
                .or_default()
                .insert(record.sha256.clone());
            blobs
        });
    assert!(blobs.values().all(|blobs| blobs.len() == 1), "{blobs:?}");
    assert!(
        blobs.len() < records.len(),
        "no source is shared: {records:?}"
    );
    Ok(())
}

/// A model with one directory source in two roles downloads the source one
/// time. The dry run counts the source one time.
#[tokio::test]
async fn a_source_in_two_roles_of_one_model_downloads_once() -> anyhow::Result<()> {
    let fx = Fixture::start().await?;
    let dir = hf("dir", json!({"prefix": "4bit"}));
    let mlx: Model = serde_json::from_value(typed("mlx", dir.clone()))?;
    let pair: Model = serde_json::from_value(typed(
        "mlx_speculative",
        json!({"target": dir.clone(), "draft": dir}),
    ))?;
    let source_bytes = fx.planned_bytes(&[mlx]).await?;
    assert_eq!(
        fx.planned_bytes(std::slice::from_ref(&pair)).await?,
        source_bytes
    );
    fx.import(&[pair]).await?;
    assert_eq!(fx.hub.downloads_of("4bit/config.json"), 1);
    assert_eq!(fx.hub.downloads_of("4bit/model.safetensors"), 1);
    Ok(())
}

/// The source entry names the newest archive record of the source. If the vault
/// has no such record, the next import downloads the source again, even if an
/// older record of the source exists.
#[rstest]
#[case::first_record(true, false, 1)]
#[case::second_record(false, true, 2)]
#[case::both_records(true, true, 2)]
fn a_deleted_archive_is_downloaded_again(
    #[case] delete_first: bool,
    #[case] delete_second: bool,
    #[case] downloads: usize,
) -> anyhow::Result<()> {
    let (outcome, logs) = capture_logs(|runtime| {
        runtime.block_on(async {
            let fx = Fixture::start().await?;
            let target = hf("dir", json!({"prefix": "4bit"}));
            let pair = |model_type| {
                let pair = json!({"target": target, "draft": hf("draft", json!({}))});
                serde_json::from_value::<Model>(typed(model_type, pair))
            };
            let under_4bit = |record: &VaultResource| {
                matches!(
                    &record.origin,
                    Some(VaultOrigin::HuggingFace { path: Some(path), .. })
                        if path.as_ref() == "4bit"
                )
            };
            let archive_records = || async {
                anyhow::Ok(
                    fx.records()
                        .await?
                        .into_iter()
                        .filter(under_4bit)
                        .map(|record| record.resource_id)
                        .collect::<HashSet<_>>(),
                )
            };
            let mlx: Model = serde_json::from_value(typed("mlx", target.clone()))?;
            fx.import(&[mlx]).await?;
            let first = archive_records().await?;
            fx.import(&[pair("mlx_speculative")?]).await?;
            let second = &archive_records().await? - &first;
            assert_eq!(fx.hub.downloads_of("4bit/config.json"), 1);

            let to_delete = [(delete_first, &first), (delete_second, &second)]
                .into_iter()
                .filter(|(delete, _)| *delete)
                .flat_map(|(_, ids)| ids);
            for resource_id in to_delete {
                fx.vault.delete_resource(resource_id).await?;
            }
            fx.import(&[pair("torch_speculative")?]).await?;
            assert_eq!(fx.hub.downloads_of("4bit/config.json"), downloads);
            assert_eq!(fx.hub.downloads_of("4bit/model.safetensors"), downloads);
            anyhow::Ok(())
        })
    })?;
    outcome?;
    assert!(logs.contains("hf import reused archive"), "{logs}");
    Ok(())
}

/// The progress sink gets the bytes of each file, up to its size. Each event
/// names the source of its file as `org/repo_name@commit`.
#[rstest]
#[case::torch(
    typed("torch", hf("draft", json!({}))),
    &[("draft", "config.json", 14), ("draft", "model.safetensors", 100)]
)]
#[case::gguf_vision(
    typed("gguf_vision", hf("vl-gguf", json!({"model": "m.gguf", "mmproj": "mmproj/p.gguf"}))),
    &[("vl-gguf", "m.gguf", 12), ("vl-gguf", "mmproj/p.gguf", 14)]
)]
#[case::mlx_speculative(
    typed("mlx_speculative", json!({"target": hf("dir", json!({"prefix": "4bit"})), "draft": hf("draft", json!({}))})),
    &[
        ("dir", "4bit/config.json", 10),
        ("dir", "4bit/model.safetensors", 300),
        ("draft", "config.json", 14),
        ("draft", "model.safetensors", 100),
    ]
)]
#[tokio::test]
async fn an_import_reports_its_progress(
    #[case] model: Value,
    #[case] files: &[(&str, &str, u64)],
) -> anyhow::Result<()> {
    let fx = Fixture::start().await?;
    let model: Model = serde_json::from_value(model)?;
    let (sender, mut events) = mpsc::channel(64);
    fx.import_with(u64::MAX, &Progress::Channel(sender), &[model])
        .await??;
    let mut last = HashMap::new();
    while let Ok(event) = events.try_recv() {
        last.insert(
            (event.hf_ref.clone(), event.file.clone()),
            (event.bytes_done, event.bytes_total),
        );
    }
    let expected: HashMap<_, _> = files
        .iter()
        .map(|&(repo, file, size)| {
            let hf_ref = format!("LiquidAI/{repo}@{}", commit_of(repo));
            ((hf_ref, file.to_owned()), (size, size))
        })
        .collect();
    assert_eq!(last, expected);
    Ok(())
}

#[tokio::test]
async fn the_completing_event_waits_for_room() -> anyhow::Result<()> {
    let (sender, mut events) = mpsc::channel(1);
    let chunks = ["ab", "cd", "ef"].map(|chunk| anyhow::Ok(Bytes::from_static(chunk.as_bytes())));
    let body = Progress::Channel(sender).track(
        futures::stream::iter(chunks).boxed(),
        "a/b@0123",
        "m.gguf",
        6,
    );
    let read = tokio::spawn(body.try_collect::<Vec<_>>());
    let first = events.recv().await.context("no event")?;
    let last = events.recv().await.context("no completing event")?;
    read.await??;
    assert_eq!((first.bytes_done, last.bytes_done), (2, 6));
    Ok(())
}

fn a_source(files: Vec<TarEntry>) -> anyhow::Result<Source> {
    Ok(Source {
        repo: HfRepo::parse_org_repo("LiquidAI/dir")?,
        commit: GitOid::try_new(commit('b'))?,
        filename: FileName::try_new("dir.tar")?,
        files: SourceFiles::Dir {
            prefix: None,
            entries: files,
        },
    })
}

/// The two files of the pinned-digest tests, in one order and in the reverse
/// order.
fn listed_entries() -> anyhow::Result<(Vec<TarEntry>, Vec<TarEntry>)> {
    let entries = vec![
        TarEntry {
            name: "model.safetensors".to_owned(),
            repo_path: "model.safetensors".to_owned(),
            size: 700,
            sha256: Some(Sha256::try_new("a".repeat(64))?),
        },
        TarEntry {
            name: "config.json".to_owned(),
            repo_path: "config.json".to_owned(),
            size: 2,
            sha256: None,
        },
    ];
    let reversed = entries.iter().rev().cloned().collect();
    Ok((entries, reversed))
}

/// The import digest is a stored format, so it is pinned: listing order does
/// not change it, the model type does. It is the SHA-256 of (`b…` and `a…` as
/// 40 and 64 characters):
///
/// ```text
/// {"v":1,"type":"mlx","sources":[{"role":"dir_archive","org":"LiquidAI",
/// "repo_name":"dir","commit":"b…","path":null,"files":[{"path":"config.json",
/// "size":2,"lfs_sha256":null},{"path":"model.safetensors","size":700,
/// "lfs_sha256":"a…"}]}]}
/// ```
#[test]
fn the_import_digest_is_pinned() -> anyhow::Result<()> {
    let (entries, reversed) = listed_entries()?;
    let mlx = import_digest(&VaultModel::Mlx {
        dir_archive: a_source(entries.clone())?,
    })?;
    let listed_in_reverse = import_digest(&VaultModel::Mlx {
        dir_archive: a_source(reversed)?,
    })?;
    let torch = import_digest(&VaultModel::Torch {
        dir_archive: a_source(entries)?,
    })?;
    assert_eq!(mlx, listed_in_reverse);
    assert_ne!(mlx, torch);
    assert_eq!(
        mlx.as_ref(),
        "47db4187240db4f8850162ce4d7439e2168dca524085fb46d8654e1c32a3eeb6"
    );
    Ok(())
}

/// The source digest is a stored format, so a test pins it. The listing order
/// of the files does not change the digest. The digest of a gguf source is the
/// digest of its blob. The digest of a directory source is the SHA-256 of this
/// JSON, where `b…` is 40 characters and `a…` is 64 characters:
///
/// ```text
/// {"v":1,"kind":"dir_source","org":"LiquidAI","repo_name":"dir","commit":"b…",
/// "path":null,"files":[{"path":"config.json","size":2,"lfs_sha256":null},
/// {"path":"model.safetensors","size":700,"lfs_sha256":"a…"}]}
/// ```
#[test]
fn the_source_digest_is_pinned() -> anyhow::Result<()> {
    let (entries, reversed) = listed_entries()?;
    let dir = source_id(&a_source(entries)?)?;
    assert_eq!(dir, source_id(&a_source(reversed)?)?);
    assert_eq!(
        dir.as_ref(),
        "ce7e965fe0f83d4dc8186e4d78e93bb39b24e80a1abc1a9738dbc038b48ff54e"
    );
    let blob = Sha256::try_new("c".repeat(64))?;
    let gguf = Source {
        files: SourceFiles::Gguf {
            path: RepoSubpath::try_new("m.gguf")?,
            size: 9,
            sha256: blob.clone(),
        },
        ..a_source(Vec::new())?
    };
    assert_eq!(source_id(&gguf)?, blob);
    Ok(())
}

/// One Hub request of the permit tests.
#[derive(Debug, Clone, Copy)]
enum Request {
    /// The resolve of `LiquidAI/vl-gguf`, which [`MockHub::hold`] holds.
    Resolve,
    /// The download of `h.gguf`, which [`MockHub::hold_files`] holds.
    HeldDownload,
    /// The download of `m.gguf`, read to its end.
    Download,
}

/// `LiquidAI/{name}` with the good token.
fn hf_repo(name: &str) -> anyhow::Result<HfRepo> {
    Ok(serde_json::from_value(hf(name, json!({})))?)
}

impl Fixture {
    /// Make `request`, and read its whole body.
    async fn request(&self, request: Request) -> anyhow::Result<()> {
        let (repo, path) = match request {
            Request::Resolve => {
                let model = hf_model("gguf_text", "vl-gguf", json!({"path": "m.gguf"}))?;
                let importer = self.importer(u64::MAX)?;
                ImportPlan::resolve(&importer, &self.owner, &[model])
                    .await
                    .map_err(|failure| failure.error)?;
                return Ok(());
            }
            Request::HeldDownload => ("held", "h.gguf"),
            Request::Download => ("vl-gguf", "m.gguf"),
        };
        let commit = GitOid::try_new(commit_of(repo))?;
        let body = self.client.download(&hf_repo(repo)?, &commit, path).await?;
        body.try_collect::<Vec<_>>().await?;
        Ok(())
    }

    /// The requests that reached the Hub.
    fn arrivals(&self) -> usize {
        self.hub.revision_requests() + self.hub.downloads()
    }
}

/// A resolve waits as well as a download, and the logged wait holds no token.
#[rstest]
#[case::resolve_then_resolve(Request::Resolve, Request::Resolve)]
#[case::resolve_then_download(Request::Resolve, Request::Download)]
#[case::download_then_resolve(Request::HeldDownload, Request::Resolve)]
#[case::download_then_download(Request::HeldDownload, Request::Download)]
fn a_request_waits_for_the_permit(
    #[case] first: Request,
    #[case] second: Request,
) -> anyhow::Result<()> {
    let (outcome, logs) = capture_logs(|runtime| {
        runtime.block_on(async {
            let fx = Fixture::with_requests(NonZeroUsize::MIN).await?;
            let (held, files) = (fx.hub.hold().await, fx.hub.hold_files().await);
            let mut first_done = pin!(fx.request(first));
            tokio::select! {
                done = &mut first_done => anyhow::bail!("the held request ended: {done:?}"),
                () = fx.hub.arrived(), if matches!(first, Request::Resolve) => {}
                () = fx.hub.file_arrived(), if matches!(first, Request::HeldDownload) => {}
            }
            let arrivals = fx.arrivals();
            let mut second_done = pin!(fx.request(second));
            let waited = tokio::time::timeout(Duration::from_millis(200), &mut second_done).await;
            assert!(waited.is_err(), "the second request ended: {waited:?}");
            assert_eq!(
                fx.arrivals(),
                arrivals,
                "the second request reached the Hub"
            );

            drop((held, files));
            let (first_done, second_done) = tokio::join!(first_done, second_done);
            first_done?;
            second_done?;
            anyhow::Ok(())
        })
    })?;
    outcome?;
    assert!(logs.contains("hf request waited for a permit"), "{logs}");
    assert!(logs.contains("waited_ms="), "{logs}");
    assert!(!logs.contains("SECRET"), "{logs}");
    Ok(())
}

/// How a test ends the body of a download.
#[derive(Debug, Clone, Copy)]
enum BodyEnd {
    ReadToEnd,
    Dropped,
}

#[rstest]
#[case::read_to_end(BodyEnd::ReadToEnd)]
#[case::dropped(BodyEnd::Dropped)]
#[tokio::test]
async fn a_download_holds_its_permit_until_its_body_ends(
    #[case] ends: BodyEnd,
) -> anyhow::Result<()> {
    let fx = Fixture::with_requests(NonZeroUsize::MIN).await?;
    let (repo, commit) = (hf_repo("vl-gguf")?, GitOid::try_new(commit_of("vl-gguf"))?);

    let body = fx.client.download(&repo, &commit, "m.gguf").await?;
    assert_eq!(fx.client.available_permits(), 0);
    match ends {
        BodyEnd::ReadToEnd => {
            let chunks: Vec<Bytes> = body.try_collect().await?;
            assert_eq!(chunks.concat(), WEIGHTS);
        }
        BodyEnd::Dropped => drop(body),
    }
    assert_eq!(fx.client.available_permits(), 1);
    Ok(())
}

/// Each file releases its permit before the next one opens.
#[tokio::test]
async fn an_import_of_several_files_needs_one_permit() -> anyhow::Result<()> {
    let fx = Fixture::with_requests(NonZeroUsize::MIN).await?;
    let mlx = hf_model("mlx", "dir", json!({}))?;
    let vision = serde_json::from_value(json!({
        "type": "gguf_vision", "source": "huggingface", "org": "LiquidAI",
        "repo_name": "vl-gguf", "model": "m.gguf", "mmproj": "mmproj/p.gguf",
        "auth_token": GOOD_TOKEN,
    }))?;
    let imported = tokio::time::timeout(Duration::from_secs(30), fx.import(&[mlx, vision]))
        .await
        .context("the import waits for its own permit")??;
    assert_eq!(imported.len(), 2);
    assert_eq!(
        fx.hub.downloads(),
        6,
        "four files of `dir` and two gguf files"
    );
    assert_eq!(fx.client.available_permits(), 1);
    Ok(())
}

/// The wait comes from the `RateLimit` header, and no text holds the token.
#[test]
fn a_rate_limited_hub_is_retryable() -> anyhow::Result<()> {
    let (outcome, logs) = capture_logs(|runtime| {
        runtime.block_on(async {
            let fx = Fixture::start().await?;
            let model = hf_model("gguf_text", "limited", json!({"path": "l.gguf"}))?;
            let limited = fx
                .import_with(u64::MAX, &Progress::Silent, &[model])
                .await?;
            anyhow::Ok((limited.err(), fx.records().await?.len()))
        })
    })?;
    let (error, records) = outcome?;
    let error = error.context("the import ran")?;
    assert!(
        matches!(
            &error,
            HfImportError::Hub(HubError::RateLimited { retry_after_secs, .. })
                if *retry_after_secs == Some(RATE_LIMIT_RESET)
        ),
        "{error:?}"
    );
    assert_eq!(error.fault(), ImportFault::RateLimited);
    assert!(
        error.to_string().contains("Retry after 55 seconds"),
        "{error}"
    );
    assert_eq!(records, 0);
    assert!(logs.contains("hf request rate-limited by hub"), "{logs}");
    assert!(logs.contains("retry_after_secs=55"), "{logs}");
    [error.to_string(), format!("{error:?}"), logs]
        .iter()
        .for_each(|text| assert!(!text.contains("SECRET"), "{text}"));
    Ok(())
}
