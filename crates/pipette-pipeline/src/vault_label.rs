//! The `custom_label` a vault source lacks, from the records it names: filled
//! by the scorer on new rows and by `fix-vault-labels` on stored ones. The
//! label is no part of a descriptor's id.

use std::collections::{BTreeSet, HashMap, HashSet};

use serde_json::{Map, Value};

use pipette_core::canonical_json::canonicalize_str;
use pipette_core::vault::{NoCustomLabel, VaultResource, custom_label};
use pipette_plan_types::{VaultResourceId, is_vault_source};

use crate::vault_check::{CUSTOM_LABEL, vault_paths};

/// The vault records a fill labels sources from, and the resources the fill
/// is narrowed to.
#[derive(Default)]
pub(crate) struct Fill {
    records: HashMap<VaultResourceId, VaultResource>,
    only: Option<HashSet<VaultResourceId>>,
}

/// What became of one unlabelled vault source.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Outcome {
    /// Labelled with this name, from these records.
    Filled(String, BTreeSet<VaultResourceId>),
    /// These resources have no record.
    Missing(Vec<VaultResourceId>),
    /// A record is not the principal's that the source names.
    OtherOwner,
    Unlabelled(NoCustomLabel),
}

impl Fill {
    /// A fill of every source `records` give a label.
    pub(crate) fn every(records: impl IntoIterator<Item = VaultResource>) -> Self {
        Self {
            records: records
                .into_iter()
                .map(|record| (record.resource_id.clone(), record))
                .collect(),
            only: None,
        }
    }

    /// [`Fill::every`], narrowed to the sources naming one of `only` unless it is
    /// empty. An id with no record errors: a typo would otherwise report nothing.
    pub(crate) fn narrowed(
        records: Vec<VaultResource>,
        only: &[VaultResourceId],
    ) -> anyhow::Result<Self> {
        let fill = Self::every(records);
        if let Some(unknown) = only.iter().find(|id| !fill.records.contains_key(*id)) {
            anyhow::bail!("{unknown} is not a vault record");
        }
        Ok(Self {
            only: (!only.is_empty()).then(|| only.iter().cloned().collect()),
            ..fill
        })
    }

    /// `stored` with a label on each unlabelled vault source its records name,
    /// canonical, or `None` when none was added; and each source's outcome.
    pub(crate) fn value(&self, stored: &str) -> (Option<String>, Vec<Outcome>) {
        let Ok(mut descriptor) = serde_json::from_str::<Value>(stored) else {
            return (None, Vec::new());
        };
        let mut outcomes = Vec::new();
        each_unlabelled(&mut descriptor, &mut |fields| {
            outcomes.extend(self.source(fields));
        });
        let filled = outcomes.iter().any(|o| matches!(o, Outcome::Filled(..)));
        (
            filled.then(|| canonicalize_str(&descriptor.to_string())),
            outcomes,
        )
    }

    /// Label one unlabelled vault source, or say why not. `None` for a source
    /// naming no resource the fill is narrowed to.
    fn source(&self, fields: &mut Map<String, Value>) -> Option<Outcome> {
        let ids = resources(fields);
        if let Some(only) = &self.only
            && !ids.iter().any(|id| only.contains(id))
        {
            return None;
        }
        let records: Option<Vec<&VaultResource>> =
            ids.iter().map(|id| self.records.get(id)).collect();
        let Some(records) = records else {
            let missing = ids
                .into_iter()
                .filter(|id| !self.records.contains_key(id))
                .collect();
            return Some(Outcome::Missing(missing));
        };
        // A descriptor is a device's text, so it may name a record that
        // ingestion would refuse it: one its principal does not own.
        let principal = fields
            .get("principal_id")
            .and_then(Value::as_str)
            .map(str::trim);
        if records
            .iter()
            .any(|record| Some(record.owner.label().as_str()) != principal)
        {
            return Some(Outcome::OtherOwner);
        }
        Some(match custom_label(&records) {
            Ok(label) => {
                let label = label.to_string();
                fields.insert(CUSTOM_LABEL.to_string(), Value::String(label.clone()));
                Outcome::Filled(label, ids)
            }
            Err(why) => Outcome::Unlabelled(why),
        })
    }
}

/// The resources the unlabelled vault sources of `stored` name: the records
/// [`Fill::value`] labels it from.
pub(crate) fn unlabelled_resources(stored: &str) -> BTreeSet<VaultResourceId> {
    let mut ids = BTreeSet::new();
    if let Ok(mut descriptor) = serde_json::from_str::<Value>(stored) {
        each_unlabelled(&mut descriptor, &mut |fields| ids.extend(resources(fields)));
    }
    ids
}

/// Every unlabelled vault source at any depth, as
/// `vault_check::drop_custom_labels` walks them.
fn each_unlabelled(value: &mut Value, visit: &mut impl FnMut(&mut Map<String, Value>)) {
    match value {
        Value::Object(fields) => {
            if is_vault_source(fields) && unlabelled(fields) {
                visit(fields);
            }
            fields.values_mut().for_each(|v| each_unlabelled(v, visit));
        }
        Value::Array(items) => items.iter_mut().for_each(|v| each_unlabelled(v, visit)),
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {}
    }
}

fn resources(fields: &Map<String, Value>) -> BTreeSet<VaultResourceId> {
    vault_paths(fields)
        .map(|(_, path)| path.resource_id().clone())
        .collect()
}

/// Whether a vault source has no label: no key, or a `null` one, which
/// ingestion reads as none.
fn unlabelled(fields: &Map<String, Value>) -> bool {
    matches!(fields.get(CUSTOM_LABEL), None | Some(Value::Null))
}

/// The canonical descriptor a device reports for `record`'s plan entry, with
/// or without the vault's label.
#[cfg(test)]
pub(crate) fn reported(record: &VaultResource, labelled: bool) -> anyhow::Result<String> {
    use anyhow::Context;

    use pipette_plan_types::VaultEntry;

    let mut entry = match record.entry().context("the record has a plan entry")? {
        VaultEntry::Model(model) => serde_json::to_value(model)?,
        VaultEntry::Runtime(runtime) => serde_json::to_value(runtime)?,
    };
    if !labelled {
        crate::vault_check::drop_custom_labels(&mut entry);
    }
    Ok(canonicalize_str(&entry.to_string()))
}

/// Store the source of job `job` naming `records`, as filing a plan does.
#[cfg(test)]
pub(crate) async fn ran(
    stores: &pipette_store::Stores,
    job: &str,
    records: &[&VaultResource],
) -> anyhow::Result<()> {
    let spec = records
        .iter()
        .map(|record| Ok(serde_json::from_str::<Value>(&reported(record, true)?)?))
        .collect::<anyhow::Result<Vec<Value>>>()?;
    stores
        .plans
        .put_job_source(
            &pipette_plan_types::JobId::try_new(job)?,
            &serde_json::json!({ "spec": spec }),
        )
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use anyhow::Context;
    use serde_json::json;

    use pipette_core::vault::VaultOwner;
    use pipette_plan_types::OrgId;
    use pipette_store_testkit::{
        a_huggingface_origin, a_vault_resource, a_vault_runtime, a_vault_upload, a_vision_upload,
    };

    use super::*;
    use crate::vault_check::descriptor_sha256;

    const ORG: &str = "org_0195ab";

    fn file(filename: &str, digit: char) -> Value {
        json!({"filename": filename, "sha256": digit.to_string().repeat(64)})
    }

    /// The records of one upload of [`ORG`]'s named `name`.
    fn upload(name: &str, artifact: Value) -> anyhow::Result<Vec<VaultResource>> {
        let owner = VaultOwner::Org {
            org_id: OrgId::try_new(ORG)?,
        };
        a_vault_upload(name, owner, artifact)
    }

    fn a_gguf(name: &str, digit: char) -> anyhow::Result<VaultResource> {
        upload(
            name,
            json!({"type": "gguf_text", "model": file("ACMELM-4.5B-Q4_K_M.gguf", digit)}),
        )?
        .pop()
        .context("one file makes one record")
    }

    fn url(record: &VaultResource) -> String {
        format!("/vault/resources/{}/content", record.resource_id)
    }

    /// A `gguf_text` source of `principal`'s naming `record`, unlabelled.
    fn a_source(record: &VaultResource, principal: &str) -> Value {
        json!({"type": "gguf_text", "source": "vault_url", "url": url(record),
               "principal_id": principal})
    }

    fn fill_over(records: &[&VaultResource], only: &[VaultResourceId]) -> anyhow::Result<Fill> {
        Fill::narrowed(records.iter().map(|r| (*r).clone()).collect(), only)
    }

    fn ids(records: &[&VaultResource]) -> BTreeSet<VaultResourceId> {
        records.iter().map(|r| r.resource_id.clone()).collect()
    }

    /// An upload's unlabelled descriptor becomes the one its plan entry gives,
    /// one label per vault source, under the id it already had.
    #[test]
    fn fills_what_the_uploads_entry_carries() -> anyhow::Result<()> {
        let speculative = upload(
            "acme ACMELM mlx",
            json!({"type": "mlx_speculative",
                   "target_dir_archive": file("target.tar.gz", 'f'),
                   "draft_dir_archive": file("draft.tar.gz", '1')}),
        )?;
        let cases = [
            ("a gguf model", vec![a_gguf("acme ACMELM q4", 'a')?], 1),
            ("a llama.cpp runtime", vec![a_vault_runtime()?], 1),
            ("a vision model's two files", a_vision_upload()?, 1),
            ("a speculative pair's two sources", speculative, 2),
        ];
        cases
            .into_iter()
            .try_for_each(|(case, records, sources)| -> anyhow::Result<()> {
                let record = records.first().context(case)?;
                let stored = reported(record, false)?;
                let fill = Fill::every(records.iter().cloned());

                let (filled, outcomes) = fill.value(&stored);

                let filled = filled.context(case)?;
                assert_eq!(filled, reported(record, true)?, "{case}");
                assert_eq!(
                    filled.matches("\"custom_label\"").count(),
                    sources,
                    "{case}"
                );
                assert_eq!(
                    descriptor_sha256(&filled),
                    descriptor_sha256(&stored),
                    "{case}"
                );
                assert_eq!(outcomes.len(), sources, "{case}");
                let labelled = outcomes
                    .into_iter()
                    .map(|outcome| {
                        let Outcome::Filled(label, ids) = outcome else {
                            anyhow::bail!("{case}: {outcome:?}");
                        };
                        assert_eq!(label, record.name.to_string(), "{case}");
                        Ok(ids)
                    })
                    .collect::<anyhow::Result<Vec<_>>>()?;
                let labelled: BTreeSet<VaultResourceId> = labelled.into_iter().flatten().collect();
                assert_eq!(labelled, ids(&records.iter().collect::<Vec<_>>()), "{case}");
                Ok(())
            })
    }

    /// A source whose records give it no label is left as stored, and says why.
    #[test]
    fn leaves_a_source_its_records_give_no_label() -> anyhow::Result<()> {
        let import = VaultResource {
            origin: Some(a_huggingface_origin()?),
            ..a_vault_resource()?
        };
        let q4 = a_gguf("acme ACMELM q4", 'a')?;
        let q8 = a_gguf("acme ACMELM q8", 'b')?;
        let gone = a_gguf("a deleted upload", 'd')?;
        let fill = fill_over(&[&import, &q4, &q8], &[])?;
        let two_uploads = json!({
            "type": "gguf_vision", "source": "vault_url", "model": url(&q4), "mmproj": url(&q8),
            "principal_id": ORG,
        });
        let mut no_principal = a_source(&q4, ORG);
        no_principal
            .as_object_mut()
            .context("the source is a JSON object")?
            .remove("principal_id");
        let cases = [
            (
                "an import",
                reported(&import, false)?,
                Outcome::Unlabelled(NoCustomLabel::Imported),
            ),
            (
                "two uploads",
                two_uploads.to_string(),
                Outcome::Unlabelled(NoCustomLabel::NamesDiffer),
            ),
            (
                "a missing record",
                reported(&gone, false)?,
                Outcome::Missing(vec![gone.resource_id.clone()]),
            ),
            (
                "another owner's record",
                a_source(&q4, "org_0195bb").to_string(),
                Outcome::OtherOwner,
            ),
            (
                "no principal",
                no_principal.to_string(),
                Outcome::OtherOwner,
            ),
        ];
        cases.into_iter().for_each(|(case, stored, why)| {
            assert_eq!(fill.value(&stored), (None, vec![why]), "{case}");
        });
        Ok(())
    }

    /// An existing label stays, wrong or not, and nothing outside a vault
    /// source is labelled, though it hold the key or a vault path.
    #[test]
    fn leaves_labels_and_other_objects_alone() -> anyhow::Result<()> {
        let q4 = a_gguf("acme ACMELM q4", 'a')?;
        let fill = fill_over(&[&q4], &[])?;
        let cases = [
            json!({"source": "vault_url", "url": url(&q4), "principal_id": ORG,
                   "custom_label": "another name"}),
            json!({"source": "huggingface", "org": "LiquidAI", "repo_name": "LFM2-1.2B-GGUF",
                   "custom_label": "a public model"}),
            json!({"type": "gguf_text", "source": "url", "url": url(&q4)}),
            json!("llama.cpp b9050"),
        ];
        cases.into_iter().for_each(|stored| {
            assert_eq!(fill.value(&stored.to_string()), (None, vec![]), "{stored}");
        });
        assert_eq!(fill.value("llama.cpp b9050"), (None, vec![]));

        // A `null` label is none, as ingestion reads it.
        let null = json!({"source": "vault_url", "url": url(&q4), "principal_id": ORG,
                          "custom_label": null});
        let (filled, _) = fill.value(&null.to_string());
        let filled: Value = serde_json::from_str(&filled.context("nothing filled")?)?;
        assert_eq!(filled.get(CUSTOM_LABEL), Some(&json!("acme ACMELM q4")));
        Ok(())
    }

    /// An id with no record is refused.
    #[test]
    fn a_resource_selection_narrows_the_fill() -> anyhow::Result<()> {
        let target = a_gguf("acme ACMELM target", 'a')?;
        let draft = a_gguf("acme ACMELM draft", 'b')?;
        let fill = fill_over(
            &[&target, &draft],
            std::slice::from_ref(&target.resource_id),
        )?;
        let pair = json!({
            "type": "mlx_speculative",
            "target": {"source": "vault_archive", "url": url(&target), "principal_id": ORG},
            "draft": {"source": "vault_archive", "url": url(&draft), "principal_id": ORG},
        });

        let (filled, outcomes) = fill.value(&pair.to_string());

        let filled: Value = serde_json::from_str(&filled.context("nothing filled")?)?;
        assert_eq!(
            filled.pointer("/target/custom_label"),
            Some(&json!("acme ACMELM target"))
        );
        assert_eq!(filled.pointer("/draft/custom_label"), None);
        assert_eq!(
            outcomes,
            [Outcome::Filled(
                "acme ACMELM target".to_string(),
                ids(&[&target])
            )]
        );

        let unknown = a_gguf("never stored", 'c')?.resource_id;
        let Err(err) = fill_over(&[&target], std::slice::from_ref(&unknown)) else {
            anyhow::bail!("expected an error for {unknown}");
        };
        assert!(err.to_string().contains("is not a vault record"), "{err}");
        Ok(())
    }

    /// The records a fill reads are those of the unlabelled sources alone.
    #[test]
    fn only_an_unlabelled_source_calls_for_its_records() -> anyhow::Result<()> {
        let target = a_gguf("acme ACMELM target", 'a')?;
        let draft = a_gguf("acme ACMELM draft", 'b')?;
        let pair = json!({
            "type": "mlx_speculative",
            "target": {"source": "vault_archive", "url": url(&target), "principal_id": ORG},
            "draft": {"source": "vault_archive", "url": url(&draft), "principal_id": ORG,
                      "custom_label": "acme ACMELM draft"},
        });
        assert_eq!(unlabelled_resources(&pair.to_string()), ids(&[&target]));
        assert!(unlabelled_resources("llama.cpp b9050").is_empty());
        Ok(())
    }
}
