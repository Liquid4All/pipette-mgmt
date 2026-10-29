//! The vault references a job body carries, checked against their records
//! before the job is queued: a `vault_*` source is the one arm the server is
//! authoritative for. A walk over tagged objects, so other arms stay unparsed.

use std::collections::{BTreeSet, HashMap, HashSet};

use serde::Serialize;
use serde_json::Value;

use pipette_core::canonical_json::{canonicalize_str, sha256_hex};
use pipette_core::vault::VaultOwner;
use pipette_plan_types::{JobId, OrgId, UserId, VaultPath, VaultResourceId, is_vault_source};
use pipette_store::{Stores, UnreadableObjectError, VaultStore};

use crate::plan_ingestion::JobRejectionError;

/// Why a body did not pass: a rejection names the job and answers `400`; a
/// store fault answers `500` with its detail redacted.
#[derive(Debug, thiserror::Error)]
pub enum VaultCheckError {
    #[error("{0}")]
    Rejected(JobRejectionError),
    #[error(transparent)]
    Store(anyhow::Error),
}

/// Whose vault resources a plan may name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VaultScope {
    /// The filer's own, and its organization's when it belongs to one.
    Own { user: UserId, org: Option<OrgId> },
    /// Only resources owned by this organization, for an org catalog member.
    Org(OrgId),
    /// Every principal's: the host CLI, and a user holding `submit_any_plans`.
    Every,
}

impl VaultScope {
    pub(crate) fn admits(&self, owner: &VaultOwner) -> bool {
        match (self, owner) {
            (Self::Every, _) => true,
            (Self::Own { user, .. }, VaultOwner::User { user_id }) => user_id == user,
            (Self::Own { org, .. }, VaultOwner::Org { org_id }) => org.as_ref() == Some(org_id),
            (Self::Org(own), VaultOwner::Org { org_id }) => own == org_id,
            (Self::Org(_), VaultOwner::User { .. }) => false,
        }
    }
}

/// One reference: the resource a vault path addresses, the principal the plan
/// says the fetch is for, and the digest the plan pins beside it, if any.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VaultRef {
    pub resource_id: VaultResourceId,
    pub principal: String,
    /// The arm's own pin: `sha256` beside `url`, `{name}_sha256` beside a named
    /// file. Optional in every dialect.
    pub sha256: Option<String>,
    /// The source's `custom_label`: what the plan says the record is named.
    pub custom_label: Option<String>,
}

/// Every vault reference in `body`, or the first malformed one: a source with
/// no `principal_id` or no URL this server serves cannot be checked.
pub fn vault_refs(body: &Value) -> Result<Vec<VaultRef>, JobRejectionError> {
    let mut refs = Vec::new();
    collect(body, &mut refs)?;
    Ok(refs)
}

fn collect(value: &Value, refs: &mut Vec<VaultRef>) -> Result<(), JobRejectionError> {
    match value {
        Value::Object(fields) => {
            if is_vault_source(fields) {
                refs.extend(refs_of(fields)?);
            }
            fields.values().try_for_each(|v| collect(v, refs))
        }
        Value::Array(items) => items.iter().try_for_each(|v| collect(v, refs)),
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => Ok(()),
    }
}

/// The field of a vault source that says what its record is named.
pub(crate) const CUSTOM_LABEL: &str = "custom_label";

/// Each vault path one `vault_*` object holds, with its field: any string field
/// that is one, whatever the arm calls its file.
pub(crate) fn vault_paths(
    fields: &serde_json::Map<String, Value>,
) -> impl Iterator<Item = (&str, VaultPath)> {
    fields
        .iter()
        .filter(|(key, _)| *key != "principal_id" && *key != CUSTOM_LABEL)
        .filter_map(|(key, v)| Some((key.as_str(), v.as_str()?.parse::<VaultPath>().ok()?)))
}

/// The references one `vault_*` object carries, one per [`vault_paths`] entry.
fn refs_of(fields: &serde_json::Map<String, Value>) -> Result<Vec<VaultRef>, JobRejectionError> {
    // Trimmed, because `pipette_plan_types::VaultPrincipalId` trims on parse and a
    // plan every client accepts must not be refused here over a space.
    let principal = fields
        .get("principal_id")
        .and_then(Value::as_str)
        .ok_or_else(|| JobRejectionError::VaultRefMalformed {
            reason: "a vault source carries no `principal_id`".to_string(),
        })?
        .trim()
        .to_string();
    let custom_label = match fields.get(CUSTOM_LABEL) {
        None | Some(Value::Null) => None,
        // Trimmed, as `pipette_plan_types::NonEmptyTrimmedString` trims on parse.
        Some(Value::String(label)) => Some(label.trim().to_string()),
        Some(_) => {
            return Err(JobRejectionError::VaultRefMalformed {
                reason: "a vault source's `custom_label` is not a string".to_string(),
            });
        }
    };
    let refs: Vec<VaultRef> = vault_paths(fields)
        .map(|(key, path)| VaultRef {
            resource_id: path.resource_id().clone(),
            principal: principal.clone(),
            sha256: fields
                .get(&digest_field_for(key))
                .and_then(Value::as_str)
                .map(|s| s.trim().to_ascii_lowercase()),
            custom_label: custom_label.clone(),
        })
        .collect();
    if refs.is_empty() {
        return Err(JobRejectionError::VaultRefMalformed {
            reason: "a vault source names no `/vault/resources/{id}/content` path".to_string(),
        });
    }
    Ok(refs)
}

/// The field that pins the digest for the path under `key`: `sha256` beside
/// `url`, `{key}_sha256` beside a named file.
fn digest_field_for(key: &str) -> String {
    if key == "url" {
        "sha256".to_string()
    } else {
        format!("{key}_sha256")
    }
}

/// Every vault resource `descriptor` names: [`vault_refs`]'s, or none for a
/// descriptor with a malformed vault source.
pub fn named_resources(descriptor: &impl Serialize) -> HashSet<VaultResourceId> {
    serde_json::to_value(descriptor)
        .ok()
        .and_then(|descriptor| vault_refs(&descriptor).ok())
        .into_iter()
        .flatten()
        .map(|vault_ref| vault_ref.resource_id)
        .collect()
}

/// Every vault resource the `descriptors` name, `None` when one holds a
/// malformed vault source. Not [`named_resources`]: the labeller still reads a
/// malformed source's ids, so it must not count as naming none.
pub(crate) fn reported_resources<'a>(
    descriptors: impl IntoIterator<Item = &'a str>,
) -> Option<BTreeSet<VaultResourceId>> {
    let each = descriptors
        .into_iter()
        .map(|text| match serde_json::from_str::<Value>(text) {
            Ok(descriptor) => job_resources(&descriptor),
            Err(_) => Some(BTreeSet::new()),
        })
        .collect::<Option<Vec<_>>>()?;
    Some(each.into_iter().flatten().collect())
}

/// Whether the body of the job a row reports, `source`, names every one of
/// `reported`. A descriptor is a device's text, so it may name another
/// principal's record, whose name and origin a row would then disclose.
pub(crate) fn job_names(source: Option<&Value>, reported: &BTreeSet<VaultResourceId>) -> bool {
    reported.is_empty()
        || source
            .and_then(job_resources)
            .is_some_and(|named| reported.is_subset(&named))
}

/// The stored source of job `ran`, or of the first job up its `rescored_from`
/// chain that has one: `requeue-eval` mints its copies without a source. `None`
/// when the chain ends or loops first, or reaches a source that is not JSON.
pub(crate) async fn ran_source(stores: &Stores, ran: &JobId) -> anyhow::Result<Option<Value>> {
    let mut job = ran.clone();
    let mut seen = HashSet::new();
    while seen.insert(job.clone()) {
        match stores.plans.get_job_source(&job).await {
            Ok(Some(source)) => return Ok(Some(source)),
            Ok(None) => {}
            Err(e)
                if e.root_cause()
                    .downcast_ref::<UnreadableObjectError>()
                    .is_some() =>
            {
                tracing::warn!(%job, error = %e, "the job source is not JSON; it names no vault resource");
                return Ok(None);
            }
            Err(e) => return Err(e.context(format!("reading the source of job {job}"))),
        }
        let Some(record) = stores.submissions.find_job(&job).await? else {
            return Ok(None);
        };
        let Some(next) = record
            .body
            .get("rescored_from")
            .and_then(Value::as_str)
            .and_then(|next| JobId::try_new(next).ok())
        else {
            return Ok(None);
        };
        job = next;
    }
    Ok(None)
}

fn job_resources(body: &Value) -> Option<BTreeSet<VaultResourceId>> {
    let refs = vault_refs(body).ok()?;
    Some(refs.into_iter().map(|r| r.resource_id).collect())
}

/// Remove each vault source's `custom_label`, and say whether any was there. It
/// repeats the record's name, so it tells no two cells apart.
pub(crate) fn drop_custom_labels(value: &mut Value) -> bool {
    pipette_plan_types::drop_vault_custom_labels(value)
}

/// The content id of a canonical descriptor: its hash, taken without any
/// `custom_label`, so a model or runtime has one id labelled or not.
pub fn descriptor_sha256(canonical: &str) -> String {
    let unlabelled = serde_json::from_str::<Value>(canonical)
        .ok()
        .and_then(|mut descriptor| {
            drop_custom_labels(&mut descriptor).then(|| canonicalize_str(&descriptor.to_string()))
        });
    // Unlabelled, or opaque text: the hash of the string as it stands.
    sha256_hex(unlabelled.as_deref().unwrap_or(canonical))
}

/// [`descriptor_sha256`] of a typed descriptor, such as a planned job's
/// runtime. It equals the digest that the scorer stores for a row that reports
/// the same descriptor.
pub fn typed_descriptor_sha256<T: Serialize>(descriptor: &T) -> Result<String, serde_json::Error> {
    Ok(descriptor_sha256(&canonicalize_str(
        &serde_json::to_string(descriptor)?,
    )))
}

/// What ingestion learns about one resource, once per set.
#[derive(Debug, Clone)]
pub struct Seen {
    owner: String,
    name: String,
    sha256: String,
    /// Whether the object the record names is still there. A producer can
    /// remove one without telling mgmt.
    backed: bool,
}

/// Refuse `body` unless every vault reference names a record within `scope`,
/// owned by the claimed principal, backed by an object, and matching any pinned
/// digest and label: each would otherwise fail on every device that claims it.
///
/// One read per distinct resource across the set, so check one set under one
/// `scope`: `seen` holds `None` for an absent or out-of-scope record.
pub async fn check(
    vault: &dyn VaultStore,
    scope: &VaultScope,
    body: &Value,
    seen: &mut HashMap<VaultResourceId, Option<Seen>>,
) -> Result<(), VaultCheckError> {
    for vault_ref in vault_refs(body).map_err(VaultCheckError::Rejected)? {
        let record = match seen.get(&vault_ref.resource_id) {
            Some(record) => record.clone(),
            None => {
                let record = match vault
                    .get_resource(&vault_ref.resource_id)
                    .await
                    .map_err(VaultCheckError::Store)?
                {
                    Some(resource) if scope.admits(&resource.owner) => Some(Seen {
                        owner: resource.owner.label(),
                        name: resource.name.to_string(),
                        sha256: resource.sha256.to_string(),
                        backed: vault
                            .stat_blob(&resource.object_key)
                            .await
                            .map_err(VaultCheckError::Store)?
                            .is_some(),
                    }),
                    // Out of reach reads as absent, so the refusal names no owner.
                    Some(resource) => {
                        tracing::warn!(
                            resource_id = %resource.resource_id,
                            owner = %resource.owner.label(),
                            "refused: the vault resource is outside the filer's reach"
                        );
                        None
                    }
                    None => None,
                };
                seen.insert(vault_ref.resource_id.clone(), record.clone());
                record
            }
        };
        let rejected = match record {
            None => JobRejectionError::UnknownVaultResource {
                resource_id: vault_ref.resource_id,
            },
            Some(Seen { owner, .. }) if owner != vault_ref.principal => {
                JobRejectionError::VaultPrincipalMismatch {
                    resource_id: vault_ref.resource_id,
                    claimed: vault_ref.principal,
                    owner,
                }
            }
            Some(Seen { backed: false, .. }) => JobRejectionError::UnbackedVaultResource {
                resource_id: vault_ref.resource_id,
            },
            Some(Seen { sha256: pinned, .. })
                if vault_ref
                    .sha256
                    .as_ref()
                    .is_some_and(|claimed| *claimed != pinned) =>
            {
                JobRejectionError::VaultDigestMismatch {
                    resource_id: vault_ref.resource_id,
                    claimed: vault_ref.sha256.unwrap_or_default(),
                    pinned,
                }
            }
            Some(Seen { name, .. })
                if vault_ref
                    .custom_label
                    .as_ref()
                    .is_some_and(|claimed| *claimed != name) =>
            {
                JobRejectionError::VaultCustomLabelMismatch {
                    resource_id: vault_ref.resource_id,
                    claimed: vault_ref.custom_label.unwrap_or_default(),
                    name,
                }
            }
            Some(_) => continue,
        };
        return Err(VaultCheckError::Rejected(rejected));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use anyhow::Context;
    use rstest::rstest;
    use serde_json::json;

    use pipette_core::vault::{VaultObjectKey, VaultResource};
    use pipette_plan_types::{NonEmptyTrimmedString, Sha256};
    use pipette_store::Stores;

    use super::*;

    const ID: &str = "vault_0195c3e2-a4f3-7b8c-9d1e-2f3a4b5c6d7e";

    #[rstest]
    #[case::llama_release(json!({"type":"llamacpp_cli_stock_tools","source":"github_release","repository_url":"github.com/ggml-org/llama.cpp","repository_version":"b11399","flavor":"linux-x64-rocm"}))]
    #[case::llama_asset(json!({"type":"llamacpp_cli_stock_tools","source":"github_release","repository_url":"github.com/ggml-org/llama.cpp","repository_version":"b11399","flavor":"linux-x64-rocm","asset":"llama.tgz"}))]
    #[case::ovms(json!({"type":"ovms","source":"github_release","repository_url":"github.com/openvinotoolkit/model_server","repository_version":"v2026.4.0","flavor":"windows-x64"}))]
    #[case::uv_pinned(json!({"type":"uv_vllm","server_version":"0.31.0","build":"cu128","python_version":"3.12","source":{"type":"pip_requirements_text","contents":"vllm==0.31.0"}}))]
    fn catalog_member_digest_matches_the_pipeline(#[case] value: Value) -> anyhow::Result<()> {
        let runtime: pipette_plan_types::Runtime = serde_json::from_value(value)?;
        let member =
            pipette_plan_types::DescriptorMember::new(runtime.clone(), chrono::Utc::now())?;
        assert_eq!(member.digest(), typed_descriptor_sha256(&runtime)?);
        Ok(())
    }

    /// A planned runtime's digest leaves out a vault `custom_label`, as the
    /// scorer's digest of the stored row does.
    #[test]
    fn a_planned_runtimes_digest_matches_the_stored_row() -> anyhow::Result<()> {
        let source = |label: Option<&str>| {
            let mut source = json!({"source": "vault_remote_archive", "url": url(ID), "principal_id": "org_1", "flavor": "windows-x64", "type": "ovms"});
            if let (Some(label), Some(fields)) = (label, source.as_object_mut()) {
                fields.insert(CUSTOM_LABEL.to_string(), Value::from(label));
            }
            source
        };
        let planned: pipette_plan_types::Runtime =
            serde_json::from_value(source(Some("ovms build")))?;
        let stored = canonicalize_str(&source(Some("ovms build")).to_string());
        let unlabelled = canonicalize_str(&source(None).to_string());
        assert_eq!(
            typed_descriptor_sha256(&planned)?,
            descriptor_sha256(&stored)
        );
        assert_eq!(typed_descriptor_sha256(&planned)?, sha256_hex(&unlabelled));
        Ok(())
    }

    /// A model member's digest is the `model_descriptor_sha256` that the
    /// scorer stores for a job that reports the same model descriptor.
    #[rstest]
    #[case::gguf_text(pipette_store_testkit::a_model_member(
        "gguf_text",
        "AcmeLM-3B-GGUF",
        "q4_0"
    ))]
    #[case::mlx(pipette_store_testkit::a_model_member("mlx", "AcmeLM-3B-MLX-4bit", "4bit"))]
    #[case::torch_speculative(pipette_store_testkit::a_model_member(
        "torch_speculative",
        "AcmeLM-3B",
        "bf16"
    ))]
    fn catalog_model_member_digest_matches_the_pipeline(
        #[case] member: anyhow::Result<pipette_plan_types::DescriptorMember>,
    ) -> anyhow::Result<()> {
        let member = member?;
        let model = member.model().context("a model member")?;
        let reported = serde_json::to_string(model)?;
        assert_eq!(
            member.digest(),
            descriptor_sha256(&canonicalize_str(&reported))
        );
        // A planned model hashes to the same digest.
        assert_eq!(
            member.digest(),
            typed_descriptor_sha256(&model.without_auth_token())?
        );
        Ok(())
    }

    /// Each file of a descriptor's vault sources counts once; a label that
    /// spells a path and a public source do not.
    #[test]
    fn a_descriptor_names_each_vault_file_once() {
        let source = |id: &str| json!({"source": "vault_archive", "url": url(id), "principal_id": "org_1", "custom_label": url("vault_0195c3e5")});
        let descriptor = json!({
            "type": "mlx_speculative",
            "target": source("vault_0195c3e2"),
            "draft": source("vault_0195c3e3"),
            "again": source("vault_0195c3e2"),
            "elsewhere": {"source": "url", "url": "https://mgmt.example/vault/resources/vault_0195c3e4/content"},
        });
        let mut ids: Vec<String> = named_resources(&descriptor)
            .iter()
            .map(ToString::to_string)
            .collect();
        ids.sort_unstable();
        assert_eq!(ids, ["vault_0195c3e2", "vault_0195c3e3"]);
    }

    fn url(id: &str) -> String {
        format!("/vault/resources/{id}/content")
    }

    /// A public source beside a vault arm contributes nothing.
    #[rstest]
    #[case::gguf_text(json!({"spec": {"model": {"type": "gguf_text", "source": "vault_url", "url": url(ID), "sha256": "AB".repeat(32), "principal_id": "org_1"}}}), 1)]
    #[case::gguf_vision(json!({"spec": {"model": {"type": "gguf_vision", "source": "vault_url", "model": url(ID), "model_sha256": "ab".repeat(32), "mmproj": url(ID), "principal_id": "org_1"}}}), 2)]
    #[case::runtime_wheelhouse(json!({"spec": {"runtime": {"type": "uv_vllm", "source": {"type": "vault_wheelhouse", "url": url(ID), "contents": "x", "principal_id": "user_1"}}}}), 1)]
    #[case::public_beside_it(json!({"spec": {"model": {"source": "huggingface", "org": "a", "repo_name": "b"}, "runtime": {"source": "vault_remote_archive", "url": url(ID), "principal_id": "org_1"}}}), 1)]
    #[case::ovms_archive(json!({"spec": {"runtime": {"type": "ovms", "source": "vault_remote_archive", "url": url(ID), "principal_id": "org_1", "flavor": "windows-x64"}}}), 1)]
    #[case::trt_edgellm_archive(json!({"spec": {"runtime": {"type": "trt_edgellm", "source": "vault_remote_archive", "url": url(ID), "principal_id": "org_1", "flavor": "jetson-orin-cuda-13"}}}), 1)]
    #[case::padded_principal(json!({"spec": {"model": {"source": "vault_url", "url": url(ID), "principal_id": "  org_1 "}}}), 1)]
    #[case::none(json!({"spec": {"model": {"source": "url", "url": "https://x.example/m.gguf"}}}), 0)]
    fn every_vault_arm_is_found(#[case] body: Value, #[case] count: usize) -> anyhow::Result<()> {
        let refs = vault_refs(&body)?;
        assert_eq!(refs.len(), count);
        refs.iter().for_each(|r| {
            assert_eq!(r.resource_id.as_str(), ID);
            assert_eq!(r.principal.trim(), r.principal, "the principal is trimmed");
            // A digest travels with its own path and nobody else's, folded to
            // the record's case.
            if let Some(sha) = &r.sha256 {
                assert_eq!(sha, &"ab".repeat(32));
            }
        });
        // The vision arm pins only its main weights, so exactly one of its two
        // references carries a digest.
        let pinned = refs.iter().filter(|r| r.sha256.is_some()).count();
        assert!(pinned <= 1, "{refs:?}");
        Ok(())
    }

    /// The owner `label` names: an organization by its `org_` prefix, else an
    /// account.
    fn owner_of(label: &str) -> anyhow::Result<VaultOwner> {
        Ok(match label.split_once('_') {
            Some(("org", _)) => VaultOwner::Org {
                org_id: OrgId::try_new(label)?,
            },
            _ => VaultOwner::User {
                user_id: UserId::try_new(label)?,
            },
        })
    }

    /// A store holding one resource owned by `owner`, uploaded as a `gguf_text`
    /// whose digest is all `a`, and the id it holds. Its blob is not written.
    async fn store_with(
        owner: &str,
    ) -> anyhow::Result<(tempfile::TempDir, Stores, VaultResourceId)> {
        let dir = tempfile::tempdir()?;
        let config = pipette_core::config::Config {
            storage: pipette_core::config::StorageConfig::local_fs(dir.path().to_path_buf()),
            auth_storage: pipette_core::config::StorageConfig::local_fs(dir.path().to_path_buf()),
            ..Default::default()
        };
        let stores = pipette_store_fs::build_local_fs_stores(&config)?;
        let resource = pipette_store_testkit::vault_resource_for(
            owner_of(owner)?,
            Sha256::try_new("a".repeat(64))?,
            1,
        )?;
        stores.vault.put_resource(&resource).await?;
        Ok((dir, stores, resource.resource_id))
    }

    /// [`store_with`], plus the object the record names, so the record is backed.
    async fn backed_store_with(
        owner: &str,
    ) -> anyhow::Result<(tempfile::TempDir, Stores, VaultResourceId)> {
        let (dir, stores, id) = store_with(owner).await?;
        let resource = stores
            .vault
            .get_resource(&id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("the record was just written"))?;
        write_blob(dir.path(), &resource)?;
        Ok((dir, stores, id))
    }

    /// Write the object `resource` names under `root`.
    fn write_blob(root: &std::path::Path, resource: &VaultResource) -> anyhow::Result<()> {
        let blob = root
            .join("vault-resources")
            .join(resource.object_key.as_ref());
        std::fs::create_dir_all(
            blob.parent()
                .ok_or_else(|| anyhow::anyhow!("a key has a parent"))?,
        )?;
        std::fs::write(blob, b"x")?;
        Ok(())
    }

    fn body(id: &str, principal: &str) -> Value {
        json!({"spec": {"model": {"source": "vault_url", "url": url(id), "principal_id": principal}}})
    }

    /// A wrong pin is refused naming both values; the right one passes in either
    /// case.
    #[rstest]
    #[case::wrong("b".repeat(64), Some("is pinned to"))]
    #[case::right("a".repeat(64), None)]
    #[case::right_uppercase("A".repeat(64), None)]
    #[tokio::test]
    async fn a_pinned_digest_must_agree_with_the_record(
        #[case] claimed: String,
        #[case] refused: Option<&str>,
    ) -> anyhow::Result<()> {
        let (_dir, stores, id) = backed_store_with("org_0195ab").await?;
        let mut body = body(id.as_str(), "org_0195ab");
        body["spec"]["model"]["sha256"] = json!(claimed);
        let mut seen = HashMap::new();
        let verdict = check(&*stores.vault, &VaultScope::Every, &body, &mut seen).await;
        match (refused, verdict) {
            (None, Ok(())) => {}
            (Some(fragment), Err(VaultCheckError::Rejected(kind))) => {
                assert!(kind.to_string().contains(fragment), "{kind}");
            }
            (_, other) => anyhow::bail!("unexpected verdict: {other:?}"),
        }
        Ok(())
    }

    /// A wrong label is refused naming both values; the record's own name passes,
    /// padded or not, as does no label.
    #[rstest]
    #[case::another(
        Some("ACMELM q8"),
        Some("is named \"private checkpoint\", not the custom_label \"ACMELM q8\"")
    )]
    #[case::a_path(Some("/vault/resources/vault_9/content"), Some("is named"))]
    #[case::the_records(Some("private checkpoint"), None)]
    #[case::padded(Some(" private checkpoint "), None)]
    #[case::none(None, None)]
    #[tokio::test]
    async fn a_custom_label_must_be_the_records_name(
        #[case] claimed: Option<&str>,
        #[case] refused: Option<&str>,
    ) -> anyhow::Result<()> {
        let (_dir, stores, id) = backed_store_with("org_0195ab").await?;
        let mut body = body(id.as_str(), "org_0195ab");
        if let Some(claimed) = claimed {
            body["spec"]["model"]["custom_label"] = json!(claimed);
        }
        // A label that spells a path is still no reference.
        assert_eq!(vault_refs(&body)?.len(), 1);
        let mut seen = HashMap::new();
        let verdict = check(&*stores.vault, &VaultScope::Every, &body, &mut seen).await;
        match (refused, verdict) {
            (None, Ok(())) => {}
            (Some(fragment), Err(VaultCheckError::Rejected(kind))) => {
                assert!(kind.to_string().contains(fragment), "{kind}");
            }
            (_, other) => anyhow::bail!("unexpected verdict: {other:?}"),
        }
        Ok(())
    }

    /// One label over two files is refused unless both records have that
    /// name: an import's files, or two uploads', share none.
    #[tokio::test]
    async fn a_custom_label_must_name_every_file_of_its_source() -> anyhow::Result<()> {
        let (dir, stores, model) = backed_store_with("org_0195ab").await?;
        let mmproj = VaultResource {
            name: NonEmptyTrimmedString::try_new("another upload")?,
            ..pipette_store_testkit::vault_resource_for(
                owner_of("org_0195ab")?,
                Sha256::try_new("b".repeat(64))?,
                1,
            )?
        };
        stores.vault.put_resource(&mmproj).await?;
        write_blob(dir.path(), &mmproj)?;
        let body = json!({"spec": {"model": {
            "type": "gguf_vision",
            "source": "vault_url",
            "model": url(model.as_str()),
            "mmproj": url(mmproj.resource_id.as_str()),
            "principal_id": "org_0195ab",
            "custom_label": "private checkpoint",
        }}});
        let mut seen = HashMap::new();
        let verdict = check(&*stores.vault, &VaultScope::Every, &body, &mut seen).await;
        let Err(VaultCheckError::Rejected(JobRejectionError::VaultCustomLabelMismatch {
            resource_id,
            ..
        })) = verdict
        else {
            anyhow::bail!("expected a label mismatch, got {verdict:?}");
        };
        assert_eq!(resource_id, mmproj.resource_id);
        Ok(())
    }

    #[rstest]
    #[case::labelled(
        r#"{"custom_label":"ACMELM q4","principal_id":"org_1","source":"vault_url","url":"/vault/resources/vault_1/content"}"#,
        r#"{"principal_id":"org_1","source":"vault_url","url":"/vault/resources/vault_1/content"}"#
    )]
    #[case::a_labelled_pair(
        r#"{"draft":{"custom_label":"p","source":"vault_archive","url":"/vault/resources/vault_2/content"},"target":{"custom_label":"p","source":"vault_archive","url":"/vault/resources/vault_1/content"},"type":"mlx_speculative"}"#,
        r#"{"draft":{"source":"vault_archive","url":"/vault/resources/vault_2/content"},"target":{"source":"vault_archive","url":"/vault/resources/vault_1/content"},"type":"mlx_speculative"}"#
    )]
    #[case::unlabelled(
        r#"{"principal_id":"org_1","source":"vault_url","url":"/vault/resources/vault_1/content"}"#,
        r#"{"principal_id":"org_1","source":"vault_url","url":"/vault/resources/vault_1/content"}"#
    )]
    #[case::a_public_source(
        r#"{"custom_label":"x","org":"o","repo_name":"r","source":"huggingface"}"#,
        r#"{"custom_label":"x","org":"o","repo_name":"r","source":"huggingface"}"#
    )]
    #[case::opaque_text("llama.cpp b9050", "llama.cpp b9050")]
    fn a_descriptors_content_id_leaves_its_label_out(
        #[case] descriptor: &str,
        #[case] hashed: &str,
    ) {
        assert_eq!(descriptor_sha256(descriptor), sha256_hex(hashed));
    }

    /// Refused at ingestion rather than found at fetch time on every device.
    #[tokio::test]
    async fn a_record_without_its_object_is_refused() -> anyhow::Result<()> {
        let (_dir, stores, id) = store_with("org_0195ab").await?;
        let mut seen = HashMap::new();
        let verdict = check(
            &*stores.vault,
            &VaultScope::Every,
            &body(id.as_str(), "org_0195ab"),
            &mut seen,
        )
        .await;
        let Err(VaultCheckError::Rejected(JobRejectionError::UnbackedVaultResource {
            resource_id,
        })) = verdict
        else {
            anyhow::bail!("expected an unbacked-resource rejection, got {verdict:?}");
        };
        assert_eq!(resource_id, id);
        Ok(())
    }

    /// The three verdicts against a real record: the owner the plan names, a
    /// different one, and no record at all.
    #[rstest]
    #[case::matches("org_0195ab", "org_0195ab", None)]
    #[case::another_principal("org_0195ab", "user_0195cd", Some("belongs to"))]
    #[tokio::test]
    async fn a_reference_is_judged_against_the_record(
        #[case] owner: &str,
        #[case] principal: &str,
        #[case] refused: Option<&str>,
    ) -> anyhow::Result<()> {
        let (_dir, stores, id) = backed_store_with(owner).await?;
        let mut seen = HashMap::new();
        let verdict = check(
            &*stores.vault,
            &VaultScope::Every,
            &body(id.as_str(), principal),
            &mut seen,
        )
        .await;
        match (refused, verdict) {
            (None, Ok(())) => {}
            (Some(fragment), Err(VaultCheckError::Rejected(kind))) => {
                assert!(kind.to_string().contains(fragment), "{kind}");
            }
            (_, other) => anyhow::bail!("unexpected verdict: {other:?}"),
        }
        Ok(())
    }

    #[tokio::test]
    async fn an_unknown_resource_is_rejected_by_name() -> anyhow::Result<()> {
        let (_dir, stores, _) = store_with("org_0195ab").await?;
        let mut seen = HashMap::new();
        let verdict = check(
            &*stores.vault,
            &VaultScope::Every,
            &body(ID, "org_0195ab"),
            &mut seen,
        )
        .await;
        let Err(VaultCheckError::Rejected(JobRejectionError::UnknownVaultResource { resource_id })) =
            verdict
        else {
            anyhow::bail!("expected an unknown-resource rejection, got {verdict:?}");
        };
        assert_eq!(resource_id.as_str(), ID);
        Ok(())
    }

    /// `Every` admits every owner. `Own` admits the filer and its organization,
    /// and neither a colleague nor another organization.
    #[rstest]
    #[case::every(None, [true, true, true, true])]
    #[case::member_of_acme(Some(Some("org_0195aa")), [true, false, true, false])]
    #[case::in_no_org(Some(None), [true, false, false, false])]
    fn a_scope_admits_its_own_and_its_organization_s(
        #[case] own: Option<Option<&str>>,
        #[case] admits: [bool; 4],
    ) -> anyhow::Result<()> {
        let scope = match own {
            None => VaultScope::Every,
            Some(org) => VaultScope::Own {
                user: UserId::try_new("user_0195aa")?,
                org: org.map(OrgId::try_new).transpose()?,
            },
        };
        let owners = ["user_0195aa", "user_0195bb", "org_0195aa", "org_0195bb"];
        for (label, want) in owners.into_iter().zip(admits) {
            assert_eq!(
                scope.admits(&owner_of(label)?),
                want,
                "{scope:?} on {label}"
            );
        }
        Ok(())
    }

    /// It is refused before anything else is judged, so the refusal never names
    /// its owner.
    #[rstest]
    #[case::names_the_owner(true, "org_0195ab", None)]
    #[case::names_another_principal(true, "user_0195cd", None)]
    #[case::unbacked(false, "org_0195ab", None)]
    #[case::pins_another_digest(true, "org_0195ab", Some("b".repeat(64)))]
    #[tokio::test]
    async fn a_record_outside_the_scope_reads_as_unknown(
        #[case] backed: bool,
        #[case] principal: &str,
        #[case] pin: Option<String>,
    ) -> anyhow::Result<()> {
        let (_dir, stores, id) = if backed {
            backed_store_with("org_0195ab").await?
        } else {
            store_with("org_0195ab").await?
        };
        let scope = VaultScope::Own {
            user: UserId::try_new("user_0195cd")?,
            org: Some(OrgId::try_new("org_0195ef")?),
        };
        let mut body = body(id.as_str(), principal);
        if let Some(pin) = pin {
            body["spec"]["model"]["sha256"] = json!(pin);
        }
        let mut seen = HashMap::new();
        let verdict = check(&*stores.vault, &scope, &body, &mut seen).await;
        let Err(VaultCheckError::Rejected(kind)) = verdict else {
            anyhow::bail!("expected a rejection, got {verdict:?}");
        };
        let JobRejectionError::UnknownVaultResource { resource_id } = &kind else {
            anyhow::bail!("expected an unknown-resource rejection, got {kind:?}");
        };
        assert_eq!(resource_id, &id);
        assert!(!kind.to_string().contains("org_0195ab"), "{kind}");
        assert!(
            matches!(seen.get(&id), Some(None)),
            "cached as absent: {seen:?}"
        );
        Ok(())
    }

    /// A store that fails on every read proves `seen` is consulted first.
    #[tokio::test]
    async fn a_resource_seen_once_is_not_read_again() -> anyhow::Result<()> {
        struct Down;
        #[async_trait::async_trait]
        impl VaultStore for Down {
            async fn get_resource(
                &self,
                _resource_id: &VaultResourceId,
            ) -> anyhow::Result<Option<VaultResource>> {
                anyhow::bail!("down")
            }
            async fn put_resource(&self, _resource: &VaultResource) -> anyhow::Result<()> {
                anyhow::bail!("down")
            }
            async fn list_resources(
                &self,
                _after: Option<&VaultResourceId>,
                _limit: std::num::NonZeroUsize,
            ) -> anyhow::Result<pipette_store::VaultResourcePage> {
                anyhow::bail!("down")
            }
            async fn delete_resource(&self, _resource_id: &VaultResourceId) -> anyhow::Result<()> {
                anyhow::bail!("down")
            }
            async fn stat_blob(&self, _key: &VaultObjectKey) -> anyhow::Result<Option<u64>> {
                anyhow::bail!("down")
            }
            async fn delete_blob(&self, _key: &VaultObjectKey) -> anyhow::Result<()> {
                anyhow::bail!("down")
            }
            async fn put_blob(
                &self,
                _key: &VaultObjectKey,
                _sha256: &Sha256,
                _body: pipette_store::BlobBody,
            ) -> anyhow::Result<pipette_store::BlobWrite> {
                anyhow::bail!("down")
            }
            async fn open_blob(
                &self,
                _key: &VaultObjectKey,
                _from: u64,
            ) -> anyhow::Result<Option<pipette_store::ArtifactBody>> {
                anyhow::bail!("down")
            }
            async fn put_staged_blob(
                &self,
                _owner: &pipette_core::vault::VaultOwner,
                _body: pipette_store::BlobBody,
                _limit: std::num::NonZeroU64,
            ) -> anyhow::Result<pipette_store::StagedWrite> {
                anyhow::bail!("down")
            }
            async fn get_import(
                &self,
                _owner: &pipette_core::vault::VaultOwner,
                _import: &Sha256,
            ) -> anyhow::Result<Option<pipette_core::vault::VaultImport>> {
                anyhow::bail!("down")
            }
            async fn put_import(
                &self,
                _owner: &pipette_core::vault::VaultOwner,
                _import: &Sha256,
                _entry: &pipette_core::vault::VaultImport,
            ) -> anyhow::Result<()> {
                anyhow::bail!("down")
            }
        }

        // A cold cache reaches the store, and the fault comes back typed.
        let mut seen = HashMap::new();
        assert!(matches!(
            check(
                &Down,
                &VaultScope::Every,
                &body(ID, "org_0195ab"),
                &mut seen
            )
            .await,
            Err(VaultCheckError::Store(_))
        ));

        // A warm cache never does.
        seen.insert(
            VaultResourceId::try_new(ID)?,
            Some(Seen {
                owner: "org_0195ab".to_string(),
                name: "private checkpoint".to_string(),
                sha256: "a".repeat(64),
                backed: true,
            }),
        );
        check(
            &Down,
            &VaultScope::Every,
            &body(ID, "org_0195ab"),
            &mut seen,
        )
        .await
        .map_err(|e| anyhow::anyhow!("a cached verdict must not touch the store: {e:?}"))?;
        Ok(())
    }

    /// A vault source that cannot be checked is refused, not skipped.
    #[rstest]
    #[case::no_principal(json!({"source": "vault_url", "url": url(ID)}))]
    #[case::absolute_url(json!({"source": "vault_url", "url": "https://elsewhere.example/vault/resources/vault_1/content", "principal_id": "org_1"}))]
    #[case::another_route(json!({"source": "vault_url", "url": "/datasets/x", "principal_id": "org_1"}))]
    #[case::local_path(json!({"source": "vault_archive", "url": "file:///tmp/x", "principal_id": "org_1"}))]
    #[case::a_label_that_is_no_text(json!({"source": "vault_url", "url": url(ID), "principal_id": "org_1", "custom_label": 7}))]
    fn an_uncheckable_vault_source_is_malformed(#[case] body: Value) {
        assert!(matches!(
            vault_refs(&body),
            Err(JobRejectionError::VaultRefMalformed { .. })
        ));
    }
}
