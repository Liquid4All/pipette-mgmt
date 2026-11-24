//! Resolve catalog names before the planner sees a document.
use std::collections::{BTreeSet, HashSet};
use std::ops::Range;

use pipette_core::matching::FleetTags;
use pipette_plan_types::{
    CatalogQualifier, CatalogScope, ClientId, DescriptorKind, DescriptorMemberKey, MemberSelector,
    ModelType, OrgId, QualifiedCatalogName,
};

use crate::descriptor_names::{self, CatalogReadScope};
use crate::descriptor_plan::{self, Eligibility, ResolveError};
use crate::plan_ingestion::IngestContext;
use crate::vault_check::named_resources;

#[derive(Debug)]
pub struct NamedDocument {
    pub submitted_sha256: pipette_plan_types::Sha256,
    pub text: String,
    /// Where each byte of `text` sits in the submitted text.
    pub offsets: OffsetMap,
    pub warnings: Vec<pipette_planner::PlanWarning>,
    pub resolved: Vec<ResolvedName>,
}

/// The replacements that turned a submitted text into its rewrite, in text
/// order: each name's span in the submitted text, and the length of the text
/// that replaced it.
#[derive(Debug, Clone, Default)]
pub struct OffsetMap(Vec<(Range<usize>, usize)>);

impl OffsetMap {
    /// The submitted offset of byte `at` of the rewritten text. A byte inside
    /// a replacement maps to the start of the name that it replaced.
    pub fn submitted(&self, at: usize) -> usize {
        let mut submitted_end = 0;
        let mut rewritten_end = 0;
        for (span, len) in &self.0 {
            let rewritten_start = rewritten_end + span.start.saturating_sub(submitted_end);
            if at < rewritten_start {
                break;
            }
            if at < rewritten_start + len {
                return span.start;
            }
            submitted_end = span.end;
            rewritten_end = rewritten_start + len;
        }
        submitted_end + at.saturating_sub(rewritten_end)
    }
}

#[derive(Debug, Clone)]
pub struct ResolvedName {
    pub variant: usize,
    /// The qualified name of the entry, for example `global/example`.
    pub catalog_name: String,
    /// The selector value from the submitted document.
    pub selector: String,
    /// The span of the string in the submitted text.
    pub span: Option<Range<usize>>,
    pub member_key: DescriptorMemberKey,
    pub(crate) model_type: Option<ModelType>,
}

#[derive(Debug, thiserror::Error)]
pub enum PlanNameError {
    #[error("this server has no descriptor catalog; write a table instead of a catalog name")]
    NoCatalog,
    #[error("variant {variant}: {source}")]
    Resolve {
        variant: usize,
        span: Option<Range<usize>>,
        #[source]
        source: Box<ResolveError>,
    },
    #[error("variant {variant}: invalid catalog name `{selector}`: {source}")]
    InvalidName {
        variant: usize,
        selector: String,
        span: Option<Range<usize>>,
        #[source]
        source: pipette_plan_types::QualifiedNameError,
    },
    #[error("variant {variant}: invalid client id: {source}")]
    InvalidClientId {
        variant: usize,
        #[source]
        source: pipette_plan_types::PrincipalIdError,
    },
    #[error("variant {variant}: `{field}` must be an array of strings")]
    FieldNotStrings { variant: usize, field: &'static str },
    #[error("variant {variant}: `{selector}` names a model entry; `runtimes` takes a runtime")]
    NotARuntime {
        variant: usize,
        selector: String,
        span: Option<Range<usize>>,
    },
    #[error("variant {variant}: `{selector}` names a runtime entry; `models` takes a model")]
    NotAModel {
        variant: usize,
        selector: String,
        span: Option<Range<usize>>,
    },
    #[error(
        "variant {variant}: model selector `{selector}` has no compatible runtime in this variant"
    )]
    NoCompatibleRuntime {
        variant: usize,
        selector: String,
        span: Option<Range<usize>>,
    },
    #[error("catalog organization `{0}` disappeared while the name resolved")]
    OrgMissing(pipette_plan_types::OrgId),
    #[error("the submitted document's digest is invalid")]
    Digest(#[from] pipette_plan_types::Sha256Error),
    #[error("the resolved descriptor does not serialize as TOML")]
    Serialize(#[from] toml_edit::ser::Error),
    #[error(transparent)]
    Store(#[from] anyhow::Error),
}

impl PlanNameError {
    pub fn is_caller_fault(&self) -> bool {
        match self {
            Self::NoCatalog
            | Self::Resolve { .. }
            | Self::InvalidName { .. }
            | Self::InvalidClientId { .. }
            | Self::FieldNotStrings { .. }
            | Self::NotARuntime { .. }
            | Self::NotAModel { .. }
            | Self::NoCompatibleRuntime { .. } => true,
            Self::OrgMissing(_) | Self::Digest(_) | Self::Serialize(_) | Self::Store(_) => false,
        }
    }
    /// The variant at fault, if the fault is one variant's.
    pub fn variant(&self) -> Option<usize> {
        match self {
            Self::Resolve { variant, .. }
            | Self::InvalidName { variant, .. }
            | Self::InvalidClientId { variant, .. }
            | Self::FieldNotStrings { variant, .. }
            | Self::NotARuntime { variant, .. }
            | Self::NotAModel { variant, .. }
            | Self::NoCompatibleRuntime { variant, .. } => Some(*variant),
            Self::NoCatalog
            | Self::OrgMissing(_)
            | Self::Digest(_)
            | Self::Serialize(_)
            | Self::Store(_) => None,
        }
    }
    pub fn span(&self) -> Option<Range<usize>> {
        match self {
            Self::Resolve { span, .. }
            | Self::InvalidName { span, .. }
            | Self::NotARuntime { span, .. }
            | Self::NotAModel { span, .. }
            | Self::NoCompatibleRuntime { span, .. } => span.clone(),
            Self::NoCatalog
            | Self::InvalidClientId { .. }
            | Self::FieldNotStrings { .. }
            | Self::OrgMissing(_)
            | Self::Digest(_)
            | Self::Serialize(_)
            | Self::Store(_) => None,
        }
    }
}

/// Replace each catalog name in `runtimes` with its runtime table. Replace each
/// model selector in `models` with its model table. Preserve all other bytes,
/// including array whitespace and comments. Parse and rewrite the document
/// off the executor. Resolve names in the catalog scope of `own_org`.
pub async fn resolve_names(
    ctx: &IngestContext<'_>,
    own_org: Option<&OrgId>,
    text: String,
) -> Result<NamedDocument, PlanNameError> {
    let (text, submitted_sha256, variants) = tokio::task::spawn_blocking(move || {
        let sha256 =
            pipette_plan_types::Sha256::try_new(pipette_core::canonical_json::sha256_hex(&text));
        let variants = named_variants(&text);
        (text, sha256, variants)
    })
    .await
    .map_err(anyhow::Error::from)?;
    let mut result = NamedDocument {
        submitted_sha256: submitted_sha256?,
        text,
        offsets: OffsetMap::default(),
        warnings: Vec::new(),
        resolved: Vec::new(),
    };
    let variants = variants?;
    if variants.is_empty() {
        return Ok(result);
    }
    let scope = CatalogReadScope::own(own_org.cloned());
    let mut edits = Vec::new();
    for named in variants {
        let variant = named.variant;
        // A bare name checks its offer rule against the variant's flags and
        // clients, which every bare name in the variant shares. An unknown
        // client has no verified flags, so it cannot satisfy an offer rule.
        let mut clients = Vec::new();
        for id in named.clients {
            let client_id = ClientId::try_new(id)
                .map_err(|source| PlanNameError::InvalidClientId { variant, source })?;
            let Some(client) = ctx.clients.get_client(&client_id).await? else {
                clients.push(BTreeSet::new());
                continue;
            };
            let tags = ctx.tags.get_client_tags(&client_id).await?;
            clients.push(
                FleetTags::new(tags.into_iter().map(|tag| (client_id.clone(), tag)))
                    .capabilities_of(&client),
            );
        }
        for Slot {
            selector,
            span,
            kind,
        } in named.slots
        {
            let refuse = |source| PlanNameError::Resolve {
                variant,
                span: span.clone(),
                source: Box::new(source),
            };
            let catalog = ctx.descriptor_catalog.ok_or(PlanNameError::NoCatalog)?;
            let written = selector
                .split_once("::")
                .map_or(selector.as_str(), |(name, _)| name);
            let name: QualifiedCatalogName =
                written
                    .parse()
                    .map_err(|source| PlanNameError::InvalidName {
                        variant,
                        selector: selector.clone(),
                        span: span.clone(),
                        source,
                    })?;
            let Some(found) =
                descriptor_names::resolve_for_plan(catalog, ctx.orgs, &scope, &name).await?
            else {
                return Err(refuse(ResolveError::UnknownName(selector.clone())));
            };
            let wrong_kind = || match kind {
                DescriptorKind::Runtime => PlanNameError::NotARuntime {
                    variant,
                    selector: selector.clone(),
                    span: span.clone(),
                },
                DescriptorKind::Model => PlanNameError::NotAModel {
                    variant,
                    selector: selector.clone(),
                    span: span.clone(),
                },
            };
            if found.descriptor_kind() != kind {
                return Err(wrong_kind());
            }
            // Only a bare runtime name checks an offer rule.
            let (requires, clients) = if kind == DescriptorKind::Model || selector.contains("::") {
                (&BTreeSet::new(), &[][..])
            } else {
                (&named.requires, &clients[..])
            };
            // Check only the explicit member's resources. With a bare runtime
            // name, the resolver still needs to choose the member.
            let key = selector
                .contains("::")
                .then(|| MemberSelector::parse_for_entry(&selector, &found).ok())
                .flatten()
                .map(|selected| selected.member_key);
            let check_all_members = kind == DescriptorKind::Runtime && key.is_none();
            let resources: BTreeSet<_> = found
                .members
                .iter()
                .filter(|member| {
                    check_all_members || key.as_ref().is_some_and(|key| member.key() == *key)
                })
                .flat_map(|member| named_resources(&member.descriptor()))
                .collect();
            let mut available = HashSet::new();
            for id in resources {
                if let Some(resource) = ctx.vault.get_resource(&id).await?
                    && ctx.vault_scope.admits(&resource.owner)
                {
                    available.insert(id);
                }
            }
            let selected = descriptor_plan::resolve(
                &selector,
                Some(&found),
                Eligibility {
                    requires,
                    any_of: &[],
                    clients,
                },
                ctx.descriptor_config,
                &available,
            )
            .map_err(refuse)?;
            let qualifier = match &found.scope {
                CatalogScope::Global => CatalogQualifier::Global,
                CatalogScope::Org(id) => CatalogQualifier::Org(
                    ctx.orgs
                        .get_org(id)
                        .await?
                        .ok_or_else(|| PlanNameError::OrgMissing(id.clone()))?
                        .slug,
                ),
            };
            let qualified = QualifiedCatalogName {
                qualifier: Some(qualifier),
                name: found.name.clone(),
            };
            let member = selected.member;
            let member_key = member.key();
            let label = descriptor_names::member_selector(&qualified, &member_key);
            let (value, model_type) = match kind {
                DescriptorKind::Runtime => {
                    let runtime = member.runtime().ok_or_else(wrong_kind)?;
                    (
                        serde::Serialize::serialize(
                            runtime,
                            toml_edit::ser::ValueSerializer::new(),
                        )?,
                        None,
                    )
                }
                DescriptorKind::Model => {
                    let model = member.model().ok_or_else(wrong_kind)?;
                    (
                        serde::Serialize::serialize(model, toml_edit::ser::ValueSerializer::new())?,
                        Some(ModelType::of(model)),
                    )
                }
            };
            if !selected.warnings.is_empty() {
                result.warnings.push(pipette_planner::PlanWarning {
                    variant,
                    message: format!("variant {variant}: `{qualified}` is hidden"),
                });
            }
            if let Some(span) = &span {
                edits.push((
                    span.clone(),
                    format!("\n# catalog name at filing: {label}\n{value}"),
                ));
            }
            tracing::debug!(variant, ?kind, catalog_name = %qualified, selector = %label, "catalog name resolved");
            result.resolved.push(ResolvedName {
                variant,
                catalog_name: qualified.to_string(),
                selector,
                span,
                member_key,
                model_type,
            });
        }
    }
    if edits.is_empty() {
        return Ok(result);
    }
    edits.sort_unstable_by_key(|(span, _)| span.start);
    result.offsets = OffsetMap(
        edits
            .iter()
            .map(|(span, replacement)| (span.clone(), replacement.len()))
            .collect(),
    );
    let text = std::mem::take(&mut result.text);
    result.text = tokio::task::spawn_blocking(move || {
        edits
            .into_iter()
            .rev()
            .fold(text, |mut text, (span, replacement)| {
                text.replace_range(span, &replacement);
                text
            })
    })
    .await
    .map_err(anyhow::Error::from)?;
    Ok(result)
}

/// The catalog names of one variant, read from the document.
struct NamedVariant {
    variant: usize,
    /// Catalog references in the variant.
    slots: Vec<Slot>,
    /// The variant's `requires`; empty unless a slot is a bare runtime name.
    requires: BTreeSet<String>,
    /// The variant's `clients` less its `exclude_clients`, whose offer rule a
    /// bare runtime name checks; empty unless a slot is a bare runtime name. A
    /// client in `exclude_clients` never runs the variant's jobs.
    clients: Vec<String>,
}

struct Slot {
    selector: String,
    span: Option<Range<usize>>,
    /// The descriptor kind accepted by the array.
    kind: DescriptorKind,
}

/// Each variant of `text` that names a catalog entry in `runtimes` or
/// `models`. A text that does not parse names none; expansion reports its
/// fault.
fn named_variants(text: &str) -> Result<Vec<NamedVariant>, PlanNameError> {
    let Ok(doc) = toml_edit::Document::parse(text) else {
        return Ok(Vec::new());
    };
    let mut found = Vec::new();
    let mut variant = 0;
    while let Some(entry) = doc.get("variants").and_then(|v| v.get(variant)) {
        let slots: Vec<_> = [
            ("runtimes", DescriptorKind::Runtime),
            ("models", DescriptorKind::Model),
        ]
        .into_iter()
        .flat_map(|(field, kind)| {
            entry
                .get(field)
                .and_then(toml_edit::Item::as_array)
                .into_iter()
                .flat_map(|array| array.iter())
                .filter_map(move |slot| {
                    Some(Slot {
                        selector: slot.as_str()?.to_owned(),
                        span: slot.span(),
                        kind,
                    })
                })
        })
        .collect();
        if !slots.is_empty() {
            let bare_runtime = slots
                .iter()
                .any(|slot| slot.kind == DescriptorKind::Runtime && !slot.selector.contains("::"));
            let (requires, clients) = if bare_runtime {
                let excluded: HashSet<_> = strings(entry, "exclude_clients", variant)?
                    .into_iter()
                    .collect();
                (
                    strings(entry, "requires", variant)?.into_iter().collect(),
                    strings(entry, "clients", variant)?
                        .into_iter()
                        .filter(|id| !excluded.contains(id))
                        .collect(),
                )
            } else {
                (BTreeSet::new(), Vec::new())
            };
            found.push(NamedVariant {
                variant,
                slots,
                requires,
                clients,
            });
        }
        variant += 1;
    }
    Ok(found)
}

fn strings(
    entry: &toml_edit::Item,
    field: &'static str,
    variant: usize,
) -> Result<Vec<String>, PlanNameError> {
    let Some(item) = entry.get(field) else {
        return Ok(Vec::new());
    };
    let invalid = || PlanNameError::FieldNotStrings { variant, field };
    item.as_array()
        .ok_or_else(invalid)?
        .iter()
        .map(|v| v.as_str().map(str::to_owned).ok_or_else(invalid))
        .collect()
}

#[cfg(test)]
mod tests {
    use anyhow::Context;
    use rstest::rstest;

    use pipette_plan_types::{
        CatalogName, DescriptorEntry, DescriptorEntryCreator, DescriptorEntryKind,
        DescriptorEntryState, DescriptorMember, NonEmptyVec,
    };
    use pipette_store::DescriptorCatalogStore;

    use super::*;

    /// A store set and a catalog that holds two global entries in one state:
    /// the runtime entry `example`, whose one member the catalog offers to
    /// `os:macos`, and the model entry `example-model`, with the members
    /// `gguf::q4_0` and `gguf::q8_0`.
    struct Fixture {
        _dir: tempfile::TempDir,
        stores: pipette_store::Stores,
        catalog: crate::descriptor_catalog_admin::tests::Memory,
        config: pipette_core::config::DescriptorCatalogConfig,
        benchmarks: std::collections::HashMap<
            pipette_plan_types::BenchmarkId,
            pipette_core::benchmark::Benchmark,
        >,
        /// No organization, so names resolve in the global catalog.
        own_org: Option<pipette_plan_types::OrgId>,
        runtime: pipette_plan_types::Runtime,
        /// The model of `example-model::gguf::q4_0`.
        model: pipette_plan_types::Model,
    }

    impl Fixture {
        async fn new(state: DescriptorEntryState) -> anyhow::Result<Self> {
            let dir = tempfile::tempdir()?;
            let stores = pipette_store_fs::build_local_fs_stores(
                &pipette_core::config::Config {
                    storage: pipette_core::config::StorageConfig::local_fs(
                        dir.path().to_path_buf(),
                    ),
                    auth_storage: pipette_core::config::StorageConfig::local_fs(
                        dir.path().to_path_buf(),
                    ),
                    ..Default::default()
                },
                pipette_store_testkit::no_queue(),
            )?;
            let catalog = crate::descriptor_catalog_admin::tests::Memory::default();
            let now = jiff::Timestamp::now();
            let runtime: pipette_plan_types::Runtime = serde_json::from_value(
                serde_json::json!({"type":"llamacpp_cli_stock_tools", "source":"github_release", "repository_url":"github.com/ggml-org/llama.cpp", "repository_version":"b1", "flavor":"macos-arm64"}),
            )?;
            let mut entry = DescriptorEntry::new(
                CatalogScope::Global,
                CatalogName::try_new("example")?,
                DescriptorEntryKind::HandMade,
                DescriptorEntryState::Active,
                now,
                now,
                DescriptorEntryCreator::Operator,
                NonEmptyVec::try_new(vec![DescriptorMember::new(runtime.clone(), now)?])?,
            )?;
            entry.state = state;
            catalog.create_entry(&entry).await?;
            let members = [("example-q4", "q4_0"), ("example-q8", "q8_0")]
                .iter()
                .map(|(repo, quantization)| {
                    pipette_store_testkit::a_model_member("gguf_text", repo, quantization)
                })
                .collect::<anyhow::Result<Vec<_>>>()?;
            let model = members
                .first()
                .and_then(DescriptorMember::model)
                .context("a model member")?
                .clone();
            let mut models = DescriptorEntry::new(
                CatalogScope::Global,
                CatalogName::try_new("example-model")?,
                DescriptorEntryKind::HandMade,
                DescriptorEntryState::Active,
                now,
                now,
                DescriptorEntryCreator::Operator,
                NonEmptyVec::try_new(members)?,
            )?;
            models.state = state;
            catalog.create_entry(&models).await?;
            let config = toml::from_str(
                "[[offer_rules]]\nruntime_type = 'llamacpp_cli_stock_tools'\nflavor = 'macos-arm64'\nrequires = ['os:macos']\n",
            )?;
            Ok(Self {
                _dir: dir,
                stores,
                catalog,
                config,
                benchmarks: std::collections::HashMap::new(),
                own_org: None,
                runtime,
                model,
            })
        }

        /// Store an approved Linux client, which the `os:macos` offer rule
        /// does not cover.
        async fn linux_client(&self) -> anyhow::Result<ClientId> {
            let client = pipette_core::client::Client {
                client_id: ClientId::try_new("ev1_linux")?,
                public_key: pipette_plan_types::PublicKeyHex::try_new(
                    "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789",
                )?,
                organization: pipette_plan_types::NonEmptyTrimmedString::try_new("lab")?,
                client_details: pipette_plan_types::NonEmptyTrimmedString::try_new("rig")?,
                name: None,
                contact_email: pipette_plan_types::ContactEmail::try_new("ops@example.com")?,
                status: pipette_core::client::ClientStatus::Approved,
                registered_at: "2026-01-01T00:00:00Z".parse()?,
                device_profile: pipette_plan_types::DeviceProfile {
                    device_os_name: Some(pipette_plan_types::NonEmptyTrimmedString::try_new(
                        "Linux",
                    )?),
                    ..Default::default()
                },
                capabilities: BTreeSet::new(),
            };
            self.stores.clients.put_client(&client).await?;
            Ok(client.client_id)
        }

        fn ctx(&self) -> IngestContext<'_> {
            IngestContext {
                descriptor_catalog: Some(&self.catalog),
                ..IngestContext::from_stores(
                    &self.stores,
                    &self.benchmarks,
                    &self.config,
                    crate::plan_ingestion::ModelCheck::none(),
                    std::num::NonZeroUsize::MAX,
                    std::time::Duration::from_secs(3600),
                )
            }
        }
    }

    /// One variant that names `selector` in its `runtimes`, between `prefix`
    /// and `suffix`, with `requires` as its one flag if it is not empty.
    fn named_variant(selector: &str, requires: &str, (prefix, suffix): (&str, &str)) -> String {
        let requires = if requires.is_empty() {
            String::new()
        } else {
            format!("\"{requires}\"")
        };
        format!(
            "# header\n[[variants]]\nrequires = [{requires}]\nruntimes = [{prefix}\"{selector}\"{suffix}] # end\n"
        )
    }

    #[rstest]
    #[case::bare("example", "os:macos", DescriptorEntryState::Active)]
    #[case::explicit("example::macos-arm64", "", DescriptorEntryState::Active)]
    #[case::qualified("global/example", "os:macos", DescriptorEntryState::Active)]
    #[case::hidden("example", "os:macos", DescriptorEntryState::Hidden)]
    #[tokio::test]
    async fn a_name_resolves_and_rewrites_in_place(
        #[case] selector: &str,
        #[case] requires: &str,
        #[case] state: DescriptorEntryState,
        #[values(("", ""), ("\n # keep\n ", ",\n"), (" ", ","))] layout: (&str, &str),
    ) -> anyhow::Result<()> {
        let fixture = Fixture::new(state).await?;
        let ctx = fixture.ctx();
        let text = named_variant(selector, requires, layout);
        let named = resolve_names(&ctx, fixture.own_org.as_ref(), text.clone()).await?;
        assert!(named.text.starts_with("# header\n[[variants]]\nrequires ="));
        assert!(named.text.ends_with(&format!("{}] # end\n", layout.1)));
        assert!(
            named
                .text
                .contains("# catalog name at filing: global/example::macos-arm64\n")
        );
        let parsed: toml::Value = toml::from_str(&named.text)?;
        let written: pipette_plan_types::Runtime =
            parsed["variants"][0]["runtimes"][0].clone().try_into()?;
        assert_eq!(written, fixture.runtime);
        let end = "] # end";
        assert_eq!(
            named
                .offsets
                .submitted(named.text.find(end).context("rewrite keeps the end")?),
            text.find(end).context("text has an end")?
        );
        let comment = named
            .text
            .find("\n# catalog name at filing:")
            .context("rewrite has the comment")?;
        assert_eq!(
            named.offsets.submitted(comment),
            text.find(&format!("\"{selector}\""))
                .context("text has the name")?
        );
        assert_eq!(
            named.warnings.len(),
            usize::from(state == DescriptorEntryState::Hidden)
        );
        let second = resolve_names(&ctx, fixture.own_org.as_ref(), named.text.clone()).await?;
        assert_eq!(second.text, named.text);
        assert!(second.resolved.is_empty());
        Ok(())
    }

    #[rstest]
    #[case::unknown("missing", "os:macos", DescriptorEntryState::Active)]
    #[case::tombstone("example", "os:macos", DescriptorEntryState::Tombstoned)]
    #[case::not_offered("example", "os:linux", DescriptorEntryState::Active)]
    #[case::no_candidate("example", "", DescriptorEntryState::Active)]
    #[tokio::test]
    async fn a_name_is_refused(
        #[case] selector: &str,
        #[case] requires: &str,
        #[case] state: DescriptorEntryState,
    ) -> anyhow::Result<()> {
        let fixture = Fixture::new(state).await?;
        let text = named_variant(selector, requires, ("", ""));
        let result = resolve_names(&fixture.ctx(), fixture.own_org.as_ref(), text).await;
        assert!(
            matches!(result, Err(PlanNameError::Resolve { .. })),
            "{result:?}"
        );
        Ok(())
    }

    /// The name comment survives the gated-model rewrite of the stored
    /// document, which removes the token.
    #[tokio::test]
    async fn a_name_comment_survives_the_gated_rewrite() -> anyhow::Result<()> {
        let fixture = Fixture::new(DescriptorEntryState::Active).await?;
        let text = named_variant("example", "os:macos", ("", ""));
        let named = resolve_names(&fixture.ctx(), fixture.own_org.as_ref(), text).await?;
        let gated = format!(
            "benchmarks = ['decode_throughput_512_100']\n{}\nmodels = [{{type='gguf_text', source='huggingface', org='acme', repo_name='example', path='example.gguf', auth_token='hf_test'}}]\n",
            named.text
        );
        let stored = crate::plan_document::with_vault_arms(
            pipette_core::plan::PlanSource::new(gated)?,
            &std::collections::HashMap::new(),
        )?;
        assert!(
            stored
                .text()
                .contains("# catalog name at filing: global/example::macos-arm64")
        );
        assert!(stored.text().starts_with("# Filed with"));
        assert!(!stored.text().contains("hf_test"));
        Ok(())
    }

    /// A bare name's offer check counts each listed client except those in
    /// `exclude_clients`. Unknown and Linux clients are not offered.
    #[rstest]
    #[case::listed("clients = ['ev1_linux']", false)]
    #[case::excluded("clients = ['ev1_linux']\nexclude_clients = ['ev1_linux']", true)]
    #[case::unknown("clients = ['ev1_missing']", false)]
    #[tokio::test]
    async fn the_offer_check_counts_the_listed_clients(
        #[case] clients: &str,
        #[case] accepted: bool,
    ) -> anyhow::Result<()> {
        let fixture = Fixture::new(DescriptorEntryState::Active).await?;
        fixture.linux_client().await?;
        let text =
            format!("[[variants]]\nrequires = ['os:macos']\n{clients}\nruntimes = ['example']");
        let result = resolve_names(&fixture.ctx(), fixture.own_org.as_ref(), text).await;
        assert_eq!(result.is_ok(), accepted, "{result:?}");
        Ok(())
    }

    /// A name that does not parse keeps its parse fault as the source, and its
    /// span.
    #[tokio::test]
    async fn an_unparsable_name_keeps_its_fault() -> anyhow::Result<()> {
        let fixture = Fixture::new(DescriptorEntryState::Active).await?;
        let text = "[[variants]]\nruntimes = ['Not A Slug/example']";
        let result = resolve_names(&fixture.ctx(), fixture.own_org.as_ref(), text.into()).await;
        assert!(
            matches!(
                result,
                Err(PlanNameError::InvalidName { span: Some(_), .. })
            ),
            "{result:?}"
        );
        Ok(())
    }

    /// A server without a catalog keeps a document with no name byte for
    /// byte, and refuses a name.
    #[tokio::test]
    async fn without_a_catalog_only_a_name_is_refused() -> anyhow::Result<()> {
        let fixture = Fixture::new(DescriptorEntryState::Active).await?;
        let ctx = IngestContext {
            descriptor_catalog: None,
            ..fixture.ctx()
        };
        let plain = "# intact\n[[variants]]\nruntimes = [{type='apple_foundation'}]\n";
        let kept = resolve_names(&ctx, fixture.own_org.as_ref(), plain.into()).await?;
        assert_eq!(kept.text.as_bytes(), plain.as_bytes());
        let named = "[[variants]]\nruntimes=['example']";
        assert!(matches!(
            resolve_names(&ctx, fixture.own_org.as_ref(), named.into()).await,
            Err(PlanNameError::NoCatalog)
        ));
        Ok(())
    }

    #[rstest]
    #[case::before_every_edit(3, 3)]
    #[case::inside_the_first_edit(12, 5)]
    #[case::between_the_edits(26, 15)]
    #[case::inside_the_second_edit(33, 20)]
    #[case::after_every_edit(40, 27)]
    fn a_rewritten_offset_maps_to_the_submitted_text(
        #[case] rewritten: usize,
        #[case] submitted: usize,
    ) {
        // Submitted 5..10 grew to 16 bytes and 20..24 to 6 bytes.
        let offsets = OffsetMap(vec![(5..10, 16), (20..24, 6)]);
        assert_eq!(offsets.submitted(rewritten), submitted);
    }

    /// A variant field that a bare name reads, and a hidden name's warning,
    /// are placed at their variant's header.
    #[tokio::test]
    async fn name_issues_are_placed_at_their_variant() -> anyhow::Result<()> {
        let fixture = Fixture::new(DescriptorEntryState::Hidden).await?;
        let header = "benchmarks = ['decode_throughput_512_100']\n[[variants]]\nruntimes = [{type='apple_foundation'}]\nmodels = [{type='gguf_text', source='huggingface', org='acme', repo_name='example', path='example.gguf'}]\n[[variants]]\n";
        let bad = format!("{header}requires = [1]\nruntimes = ['example']\n");
        let Err(error) = resolve_names(&fixture.ctx(), fixture.own_org.as_ref(), bad.clone()).await
        else {
            anyhow::bail!("a numeric flag resolved");
        };
        let verdict = crate::plan_validation::refusal(&bad, error.into())?;
        let [issue] = verdict.issues.as_slice() else {
            anyhow::bail!("not one issue: {:?}", verdict.issues);
        };
        assert_eq!(issue.line, Some(5), "{issue:?}");
        let hidden = format!(
            "{header}requires = ['os:macos']\nruntimes = ['example']\nmodels = [{{type='gguf_text', source='huggingface', org='acme', repo_name='example', path='example.gguf'}}]\nunknown_field = 1\n"
        );
        let named = resolve_names(&fixture.ctx(), fixture.own_org.as_ref(), hidden.clone()).await?;
        let stage = crate::plan_validation::expand(
            std::num::NonZeroUsize::MAX,
            pipette_planner::prefix_types(&hidden),
            crate::plan_validation::Submission::named(hidden.clone(), named),
        )
        .await?;
        let crate::plan_validation::Stage::Refused(verdict) = stage else {
            anyhow::bail!("the document expanded");
        };
        let warning = verdict
            .issues
            .iter()
            .find(|issue| issue.message.contains("hidden"))
            .context("a hidden-name warning")?;
        assert_eq!(warning.line, Some(5), "{warning:?}");
        Ok(())
    }

    /// A fault after a catalog name is placed on its line in the submitted
    /// text, not in the longer rewrite.
    #[tokio::test]
    async fn a_fault_after_a_name_is_placed_in_the_submitted_text() -> anyhow::Result<()> {
        let fixture = Fixture::new(DescriptorEntryState::Active).await?;
        let text = "benchmarks = ['decode_throughput_512_100']\n[[variants]]\nruntimes = ['example::macos-arm64']\nmodels = [{type='gguf_text', source='huggingface', org='acme', repo_name='example', path='example.gguf'}]\n[[variants]]\nruntimes = [{type='apple_foundation'}]\nmodels = [{type='gguf_text', source='huggingface', org='acme', repo_name='example', path='example.gguf'}]\nunknown_field = 1\n";
        let named =
            resolve_names(&fixture.ctx(), fixture.own_org.as_ref(), text.to_owned()).await?;
        assert_ne!(named.text, text);
        let stage = crate::plan_validation::expand(
            std::num::NonZeroUsize::MAX,
            pipette_planner::prefix_types(text),
            crate::plan_validation::Submission::named(text.to_owned(), named),
        )
        .await?;
        let crate::plan_validation::Stage::Refused(verdict) = stage else {
            anyhow::bail!("the document expanded");
        };
        let [issue] = verdict.issues.as_slice() else {
            anyhow::bail!("not one issue: {:?}", verdict.issues);
        };
        assert!(issue.message.contains("unknown_field"), "{issue:?}");
        assert_eq!(issue.line, Some(8));
        assert_eq!(issue.column, Some(1));
        Ok(())
    }

    /// One variant on macOS with the runtime of `example` and `model` in its
    /// `models`, which a `# end` comment follows.
    fn model_variant(model: &str) -> String {
        format!(
            "benchmarks = ['decode_throughput_512_100']\n[[variants]]\nrequires = ['os:macos']\nruntimes = ['example::macos-arm64']\nmodels = [{model}] # end\n"
        )
    }

    #[rstest]
    #[case::explicit("example-model::gguf::q4_0", DescriptorEntryState::Active)]
    #[case::qualified("global/example-model::gguf::q4_0", DescriptorEntryState::Active)]
    #[case::hidden("example-model::gguf::q4_0", DescriptorEntryState::Hidden)]
    #[tokio::test]
    async fn a_model_selector_resolves_and_rewrites_in_place(
        #[case] selector: &str,
        #[case] state: DescriptorEntryState,
    ) -> anyhow::Result<()> {
        let fixture = Fixture::new(state).await?;
        let text = model_variant(&format!("\"{selector}\""));
        let named = resolve_names(&fixture.ctx(), fixture.own_org.as_ref(), text.clone()).await?;
        assert!(named.text.ends_with("] # end\n"));
        assert!(
            named
                .text
                .contains("# catalog name at filing: global/example-model::gguf::q4_0\n"),
            "{}",
            named.text
        );
        let parsed: toml::Value = toml::from_str(&named.text)?;
        let written: pipette_plan_types::Model =
            parsed["variants"][0]["models"][0].clone().try_into()?;
        assert_eq!(written, fixture.model);
        let comment = named
            .text
            .rfind("\n# catalog name at filing:")
            .context("rewrite has the model comment")?;
        assert_eq!(
            named.offsets.submitted(comment),
            text.find(&format!("\"{selector}\""))
                .context("text has the selector")?
        );
        // The hidden runtime entry warns too.
        assert_eq!(
            named.warnings.len(),
            2 * usize::from(state == DescriptorEntryState::Hidden)
        );
        let model = named
            .resolved
            .iter()
            .find_map(|name| match &name.member_key {
                DescriptorMemberKey::Model {
                    format,
                    quantization,
                } => Some((name, format, quantization)),
                DescriptorMemberKey::Runtime { .. } => None,
            })
            .context("a resolved model")?;
        assert_eq!(model.0.catalog_name, "global/example-model");
        assert_eq!(model.0.selector, selector);
        assert_eq!(model.1.to_string(), "gguf");
        assert_eq!(model.2.as_str(), "q4_0");
        Ok(())
    }

    #[tokio::test]
    async fn a_model_selector_expands_as_its_table() -> anyhow::Result<()> {
        let fixture = Fixture::new(DescriptorEntryState::Active).await?;
        let text = "benchmarks = ['decode_throughput_512_100']\n[[variants]]\nrequires = ['os:macos']\nmodels = ['example-model::gguf::q4_0']\nruntimes = ['example::macos-arm64']\n";
        let named =
            resolve_names(&fixture.ctx(), fixture.own_org.as_ref(), text.to_owned()).await?;
        let table = model_variant(
            &serde::Serialize::serialize(&fixture.model, toml_edit::ser::ValueSerializer::new())?
                .to_string(),
        );
        let table = resolve_names(&fixture.ctx(), fixture.own_org.as_ref(), table).await?;
        let types = pipette_planner::prefix_types(text);
        let expand = |text: &str| {
            crate::plan_document::expand_text(std::num::NonZeroUsize::MAX, &types, text)
        };
        let from_selector = expand(&named.text)?.bodies;
        assert!(!from_selector.is_empty());
        assert_eq!(from_selector, expand(&table.text)?.bodies);
        Ok(())
    }

    /// The resolver reports incomplete, unknown, and tombstoned selectors at
    /// their source spans. It lists members for incomplete selectors and treats
    /// an old quantization label as an unknown member.
    #[rstest]
    #[case::bare_name(
        "example-model",
        DescriptorEntryState::Active,
        Some("gguf::q4_0, gguf::q8_0")
    )]
    #[case::format_only(
        "example-model::gguf",
        DescriptorEntryState::Active,
        Some("gguf::q4_0, gguf::q8_0")
    )]
    #[case::unknown_name("missing::gguf::q4_0", DescriptorEntryState::Active, None)]
    #[case::unknown_label("example-model::gguf::q2_k", DescriptorEntryState::Active, None)]
    #[case::tombstoned("example-model::gguf::q4_0", DescriptorEntryState::Tombstoned, None)]
    #[tokio::test]
    async fn a_model_selector_is_refused(
        #[case] selector: &str,
        #[case] state: DescriptorEntryState,
        #[case] members: Option<&str>,
    ) -> anyhow::Result<()> {
        // The runtime is a table, so a tombstoned `example` refuses nothing.
        let fixture = Fixture::new(state).await?;
        let text = format!(
            "[[variants]]\nrequires = ['os:macos']\nruntimes = [{{type='apple_foundation'}}]\nmodels = ['{selector}']\n"
        );
        let result = resolve_names(&fixture.ctx(), fixture.own_org.as_ref(), text).await;
        let Err(error @ PlanNameError::Resolve { span: Some(_), .. }) = result else {
            anyhow::bail!("expected a refusal at the selector, got {result:?}");
        };
        if let Some(members) = members {
            let message = std::error::Error::source(&error)
                .context("a resolve source")?
                .to_string();
            assert!(message.contains(members), "{message}");
        }
        Ok(())
    }

    /// The resolver reports a catalog reference in the wrong array at its
    /// source span.
    #[rstest]
    #[case::model_in_runtimes("runtimes = ['example-model::gguf::q4_0']", false)]
    #[case::runtime_in_models("models = ['example::macos-arm64']", true)]
    #[tokio::test]
    async fn a_name_in_the_wrong_array_is_refused(
        #[case] slot: &str,
        #[case] in_models: bool,
    ) -> anyhow::Result<()> {
        let fixture = Fixture::new(DescriptorEntryState::Active).await?;
        let text = format!("[[variants]]\nrequires = ['os:macos']\n{slot}\n");
        let result = resolve_names(&fixture.ctx(), fixture.own_org.as_ref(), text).await;
        let refused = if in_models {
            matches!(result, Err(PlanNameError::NotAModel { span: Some(_), .. }))
        } else {
            matches!(
                result,
                Err(PlanNameError::NotARuntime { span: Some(_), .. })
            )
        };
        assert!(refused, "{result:?}");
        Ok(())
    }

    /// The compatibility error names the selector. Validation places the error
    /// at the selector.
    #[rstest]
    #[case::selector_only("", 6)]
    #[case::inline_model_first(
        "  {type='gguf_text', source='huggingface', org='acme', repo_name='inline', path='inline.gguf'},\n",
        7
    )]
    #[tokio::test]
    async fn a_model_selector_with_no_compatible_runtime_names_the_selector(
        #[case] preceding_models: &str,
        #[case] line: usize,
    ) -> anyhow::Result<()> {
        let fixture = Fixture::new(DescriptorEntryState::Active).await?;
        let text = format!(
            "benchmarks = ['decode_throughput_512_100']\n[[variants]]\nrequires = ['os:ios']\nruntimes = [{{type='apple_foundation'}}]\nmodels = [\n{preceding_models}  'example-model::gguf::q4_0',\n]\n"
        );
        let named = resolve_names(&fixture.ctx(), fixture.own_org.as_ref(), text.clone()).await?;
        let stage = crate::plan_validation::expand(
            std::num::NonZeroUsize::MAX,
            pipette_planner::prefix_types(&text),
            crate::plan_validation::Submission::named(text, named),
        )
        .await?;
        let crate::plan_validation::Stage::Refused(verdict) = stage else {
            anyhow::bail!("the document expanded");
        };
        let [issue] = verdict.issues.as_slice() else {
            anyhow::bail!("not one issue: {:?}", verdict.issues);
        };
        assert_eq!(
            issue.message,
            "variant 0: model selector `example-model::gguf::q4_0` has no compatible runtime in this variant"
        );
        assert_eq!(
            (issue.line, issue.column),
            (Some(line), Some(3)),
            "{issue:?}"
        );
        Ok(())
    }

    /// A model comment survives the gated-model rewrite of another model in
    /// the same variant, and the selector's model reaches it unchanged.
    #[tokio::test]
    async fn a_model_comment_survives_the_gated_rewrite() -> anyhow::Result<()> {
        let fixture = Fixture::new(DescriptorEntryState::Active).await?;
        let text = model_variant(
            "'example-model::gguf::q4_0', {type='gguf_text', source='huggingface', org='acme', repo_name='gated', path='gated.gguf', auth_token='hf_test'}",
        );
        let named = resolve_names(&fixture.ctx(), fixture.own_org.as_ref(), text).await?;
        let stored = crate::plan_document::with_vault_arms(
            pipette_core::plan::PlanSource::new(named.text)?,
            &std::collections::HashMap::new(),
        )?;
        assert!(
            stored
                .text()
                .contains("# catalog name at filing: global/example-model::gguf::q4_0")
        );
        assert!(!stored.text().contains("hf_test"));
        let parsed: toml::Value = toml::from_str(stored.text())?;
        let written: pipette_plan_types::Model =
            parsed["variants"][0]["models"][0].clone().try_into()?;
        assert_eq!(written, fixture.model);
        Ok(())
    }
}
