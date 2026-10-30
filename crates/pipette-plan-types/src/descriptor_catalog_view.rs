//! HTTP views and filters for the descriptor catalog.

use std::collections::BTreeMap;
use std::fmt;
use std::str::FromStr;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{
    CatalogName, CatalogScope, DescriptorEntry, DescriptorEntryCreator, DescriptorEntryKind,
    DescriptorEntryState, DescriptorKind, DescriptorMemberDescriptor, DescriptorMemberKey,
    ListPage, ModelFormat, ModelKey, PositiveCount, QualifiedCatalogName, QualifiedNameError,
    QuantizationLabel, QuantizationLabelError, empty_as_none,
};

/// The complete name of one catalog member: the entry's name, then the
/// member key.
///
/// - `<name>` selects a runtime member without a flavor.
/// - `<name>::<flavor>` selects a runtime member.
/// - `<name>::<format>::<quantization>` selects a model member.
///
/// The name can be qualified, for example `global/<name>::<flavor>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemberSelector {
    pub name: QualifiedCatalogName,
    pub member_key: DescriptorMemberKey,
}

#[derive(Debug, thiserror::Error)]
pub enum SelectorError {
    #[error(transparent)]
    Name(#[from] QualifiedNameError),
    #[error("a member selector part must not be empty")]
    EmptyPart,
    #[error("a member selector has at most three parts: name::format::quantization")]
    TooManyParts,
    #[error("a model selector requires name::format::quantization")]
    IncompleteModel,
    #[error("a runtime selector has at most two parts: name::flavor")]
    RuntimeKeyShape,
    #[error("unknown model format {0:?}: expected gguf, mlx, torch, openvino or onnx")]
    UnknownFormat(String),
    #[error("a quantization label matches [a-z0-9_]+")]
    Quantization(#[from] QuantizationLabelError),
}

impl MemberSelector {
    /// Parse a selector after the caller has found its entry. The entry decides
    /// whether the rest of the selector is a runtime or model key.
    pub fn parse_for_entry(s: &str, entry: &DescriptorEntry) -> Result<Self, SelectorError> {
        let (name, key) = match s.split_once("::") {
            None => (s, None),
            Some((name, key)) => (name, Some(key)),
        };
        let name = name.parse()?;
        let member_key = selector_member_key(key, entry.descriptor_kind())?;
        Ok(Self { name, member_key })
    }
}

/// Read a member key using the entry's descriptor kind.
fn selector_member_key(
    key: Option<&str>,
    kind: DescriptorKind,
) -> Result<DescriptorMemberKey, SelectorError> {
    let part = |part: &str| match part {
        "" => Err(SelectorError::EmptyPart),
        part => Ok(part.to_owned()),
    };
    match (kind, key) {
        (DescriptorKind::Runtime, None) => Ok(DescriptorMemberKey::Runtime { flavor: None }),
        (DescriptorKind::Runtime, Some(key)) if key.contains("::") => {
            Err(SelectorError::RuntimeKeyShape)
        }
        (DescriptorKind::Runtime, Some(flavor)) => Ok(DescriptorMemberKey::Runtime {
            flavor: Some(part(flavor)?),
        }),
        (DescriptorKind::Model, None) => Err(SelectorError::IncompleteModel),
        (DescriptorKind::Model, Some(key)) => {
            let Some((format, quantization)) = key.split_once("::") else {
                part(key)?;
                return Err(SelectorError::IncompleteModel);
            };
            if quantization.contains("::") {
                return Err(SelectorError::TooManyParts);
            }
            Ok(DescriptorMemberKey::Model {
                format: part(format)?
                    .parse::<ModelFormat>()
                    .map_err(|_| SelectorError::UnknownFormat(format.to_owned()))?,
                quantization: QuantizationLabel::try_new(part(quantization)?)?,
            })
        }
    }
}

impl fmt::Display for MemberSelector {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.name.fmt(f)?;
        match &self.member_key {
            DescriptorMemberKey::Runtime { flavor: None } => Ok(()),
            DescriptorMemberKey::Runtime {
                flavor: Some(flavor),
            } => write!(f, "::{flavor}"),
            DescriptorMemberKey::Model {
                format,
                quantization,
            } => write!(f, "::{format}::{quantization}"),
        }
    }
}

/// Whether a member has a configured offer rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum DescriptorMemberCoverage {
    Covered,
    NoRule,
    Unsupported,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct DescriptorMemberSummary {
    pub selector: String,
    pub member_key: DescriptorMemberKey,
    /// The offer-rule coverage of a runtime member. The field is absent for a
    /// model member, because a model has no offer rules.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub coverage: Option<DescriptorMemberCoverage>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct DescriptorMemberView {
    pub selector: String,
    pub member_key: DescriptorMemberKey,
    /// The offer-rule coverage of a runtime member. The field is absent for a
    /// model member, because a model has no offer rules.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub coverage: Option<DescriptorMemberCoverage>,
    /// The number of approved devices offered this member. The server includes
    /// this count only on request after a successful read of the whole fleet.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub offered_device_count: Option<usize>,
    pub digest: String,
    pub added_at: DateTime<Utc>,
    pub descriptor: DescriptorMemberDescriptor,
    /// The descriptor as an inline table in a plan's runtimes or models array.
    pub plan_toml: String,
    /// The key of a model member's row in the model parameter table. The
    /// field is absent for a runtime member. It is also absent for a model
    /// descriptor that has no name from which to make a valid key.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_key: Option<ModelKey>,
    /// The parameter counts in the row of `model_key`. The field is absent
    /// when the table has no such row.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parameters: Option<DescriptorMemberParameters>,
}

/// A model member's parameter counts, in millions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct DescriptorMemberParameters {
    pub total_millions: PositiveCount,
    /// Equal to `total_millions` for a dense model.
    pub active_millions: PositiveCount,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct DescriptorEntryListRow {
    #[cfg_attr(feature = "schemars", schemars(with = "String"))]
    pub name: CatalogName,
    /// `global`, or the id of the organization that owns the entry.
    #[cfg_attr(feature = "schemars", schemars(with = "String"))]
    pub scope: CatalogScope,
    /// The name as this caller sees it: bare, `global/<name>`, or
    /// `<org-slug>/<name>`.
    pub display_name: String,
    pub kind: DescriptorEntryKind,
    pub state: DescriptorEntryState,
    pub release_time: DateTime<Utc>,
    pub created_at: DateTime<Utc>,
    pub created_by: DescriptorEntryCreator,
    pub descriptor_kind: DescriptorKind,
    /// The descriptor types of the entry's members, each listed once. A runtime
    /// entry has one type. A model entry has one type for each format.
    pub descriptor_types: Vec<String>,
    pub member_count: usize,
    pub member_summaries: Vec<DescriptorMemberSummary>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub members: Option<Vec<DescriptorMemberView>>,
}

/// One catalog entry offered to a named device. Run evidence can be added to
/// this item without changing the full catalog's entry view.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct DescriptorCatalogShortListItem {
    #[cfg_attr(feature = "schemars", schemars(with = "String"))]
    pub name: CatalogName,
    pub release_time: DateTime<Utc>,
    pub descriptor_kind: DescriptorKind,
    /// The members offered to the device. A runtime entry gives the one
    /// member that the device resolves. A model entry has no precedence, so it
    /// gives every model member that an offered runtime can load.
    pub members: Vec<DescriptorMemberView>,
}

/// Why one member of a pinned or active entry was not offered to the device.
/// `no_runtime` applies to a model member: no runtime member offered to the
/// device can load it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum DescriptorWithheldReasonCode {
    NoRule,
    Unsupported,
    NotMet,
    NoRuntime,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct DescriptorWithheldMember {
    #[cfg_attr(feature = "schemars", schemars(with = "String"))]
    pub name: CatalogName,
    pub selector: String,
    pub member_key: DescriptorMemberKey,
    pub reason_code: DescriptorWithheldReasonCode,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct DescriptorWithheldReport {
    /// Number of members the device cannot use, including those in entries
    /// that still resolve to another member.
    pub member_count: usize,
    /// Number of pinned or active entries with no offered member.
    pub entry_count: usize,
    pub members: Vec<DescriptorWithheldMember>,
}

/// The whole short list and its withheld report for one registered device.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct DescriptorCatalogShortList {
    #[serde(flatten)]
    pub page: ListPage<DescriptorCatalogShortListItem>,
    pub withheld: DescriptorWithheldReport,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct DescriptorEntryLookup {
    pub entries: Vec<DescriptorEntryListRow>,
    pub unknown: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct DescriptorNameHit {
    pub selector: String,
    #[cfg_attr(feature = "schemars", schemars(with = "String"))]
    pub name: CatalogName,
    /// `global`, or the id of the organization that owns the entry.
    #[cfg_attr(feature = "schemars", schemars(with = "String"))]
    pub scope: CatalogScope,
    /// The name as this caller sees it: bare, `global/<name>`, or
    /// `<org-slug>/<name>`.
    pub display_name: String,
    pub member_key: DescriptorMemberKey,
    pub descriptor_kind: DescriptorKind,
    pub kind: DescriptorEntryKind,
    pub state: DescriptorEntryState,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct DescriptorDigestNames {
    pub names: BTreeMap<String, Vec<DescriptorNameHit>>,
}

/// Filters for the paged descriptor catalog.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct DescriptorCatalogQuery {
    /// One scope to keep: `global`, or an organization id or slug. The
    /// default is every scope that the caller can read.
    #[serde(
        default,
        deserialize_with = "empty_as_none",
        skip_serializing_if = "Option::is_none"
    )]
    pub scope: Option<String>,
    /// Kinds to keep, comma-separated.
    #[cfg_attr(feature = "schemars", schemars(with = "Option<String>"))]
    #[serde(
        default,
        with = "crate::comma_list",
        skip_serializing_if = "Option::is_none"
    )]
    pub kind: Option<Vec<DescriptorEntryKind>>,
    /// States to keep, comma-separated. Tombstoned is not allowed.
    #[cfg_attr(feature = "schemars", schemars(with = "Option<String>"))]
    #[serde(
        default,
        with = "crate::comma_list",
        skip_serializing_if = "Option::is_none"
    )]
    pub state: Option<Vec<DescriptorEntryState>>,
    /// Descriptor family to keep: runtime or model.
    #[serde(
        default,
        deserialize_with = "empty_as_none",
        skip_serializing_if = "Option::is_none"
    )]
    pub descriptor_kind: Option<DescriptorKind>,
    /// Runtime or model types to keep, comma-separated.
    #[cfg_attr(feature = "schemars", schemars(with = "Option<String>"))]
    #[serde(
        default,
        with = "crate::comma_list",
        rename = "type",
        skip_serializing_if = "Option::is_none"
    )]
    pub descriptor_type: Option<Vec<String>>,
    /// A case-insensitive name prefix.
    #[serde(
        default,
        deserialize_with = "empty_as_none",
        skip_serializing_if = "Option::is_none"
    )]
    pub prefix: Option<String>,
    /// Include each member's full descriptor.
    #[serde(
        default,
        deserialize_with = "empty_as_none",
        skip_serializing_if = "Option::is_none"
    )]
    pub members: Option<bool>,
    /// Rows a page holds: 50 by default, at most 200.
    #[serde(
        default,
        deserialize_with = "empty_as_none",
        skip_serializing_if = "Option::is_none"
    )]
    pub limit: Option<usize>,
    /// The preceding page's `next_cursor`.
    #[serde(
        default,
        deserialize_with = "empty_as_none",
        skip_serializing_if = "Option::is_none"
    )]
    pub cursor: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct DescriptorNameQuery {
    /// Up to 100 comma-separated names. Each name is bare, `global/<name>`,
    /// or `<org-slug>/<name>`.
    #[cfg_attr(feature = "schemars", schemars(with = "Option<String>"))]
    #[serde(
        default,
        with = "crate::comma_list",
        skip_serializing_if = "Option::is_none"
    )]
    pub names: Option<Vec<RequestedCatalogName>>,
    /// Count approved devices offered each member. The server reads the fleet
    /// only when this option is true.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub offer_counts: Option<bool>,
}

/// One name of a name lookup, with the text that the caller wrote.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestedCatalogName {
    /// The name as the caller wrote it. The lookup gives this text back for
    /// an unknown name.
    pub text: String,
    pub name: QualifiedCatalogName,
}

impl FromStr for RequestedCatalogName {
    type Err = QualifiedNameError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(Self {
            text: s.to_owned(),
            name: s.parse()?,
        })
    }
}

impl fmt::Display for RequestedCatalogName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.text)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct DescriptorDigestQuery {
    /// Up to 100 comma-separated lowercase SHA-256 digests.
    #[cfg_attr(feature = "schemars", schemars(with = "Option<String>"))]
    #[serde(
        default,
        with = "crate::comma_list",
        skip_serializing_if = "Option::is_none"
    )]
    pub digests: Option<Vec<String>>,
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use rstest::rstest;
    use serde_json::json;
    use strum::IntoEnumIterator;

    use super::*;
    use crate::{
        AppleFoundation, CatalogQualifier, DescriptorMember, Model, ModelType, NonEmptyVec,
        OrgSlug, Runtime, RuntimeType,
    };

    fn an_entry(kind: DescriptorKind) -> anyhow::Result<DescriptorEntry> {
        let at = Utc::now();
        let member = match kind {
            DescriptorKind::Runtime => {
                DescriptorMember::new(Runtime::AppleFoundation(AppleFoundation::default()), at)?
            }
            DescriptorKind::Model => DescriptorMember::new_model(
                serde_json::from_value::<Model>(json!({
                    "type": "mlx", "source": "huggingface", "org": "acme",
                    "repo_name": "AcmeLM-3B",
                    "revision": "0123456789abcdef0123456789abcdef01234567"
                }))?,
                QuantizationLabel::try_new("4bit")?,
                at,
            )?,
        };
        Ok(DescriptorEntry::new(
            CatalogScope::Global,
            CatalogName::try_new("AcmeLM-3B")?,
            DescriptorEntryKind::HandMade,
            DescriptorEntryState::Hidden,
            at,
            at,
            DescriptorEntryCreator::Operator,
            NonEmptyVec::try_new(vec![member])?,
        )?)
    }

    fn runtime_key(flavor: Option<&str>) -> serde_json::Value {
        json!({"kind": "runtime", "flavor": flavor})
    }

    fn model_key(format: &str, quantization: &str) -> serde_json::Value {
        json!({"kind": "model", "format": format, "quantization": quantization})
    }

    #[rstest]
    #[case::bare_runtime("Foo-1", None, runtime_key(None))]
    #[case::runtime("Foo-1::linux-x64", None, runtime_key(Some("linux-x64")))]
    #[case::model("AcmeLM-3B::gguf::q4_0", None, model_key("gguf", "q4_0"))]
    #[case::global_runtime(
        "global/x::macos-arm64",
        Some("global"),
        runtime_key(Some("macos-arm64"))
    )]
    #[case::org_model("acme/x::mlx::4bit", Some("acme"), model_key("mlx", "4bit"))]
    #[case::spaced_name("acme/Acme LM 3B::gguf::q4_0", Some("acme"), model_key("gguf", "q4_0"))]
    fn member_selector_round_trips(
        #[case] input: &str,
        #[case] qualifier: Option<&str>,
        #[case] member_key: serde_json::Value,
    ) -> anyhow::Result<()> {
        let member_key: DescriptorMemberKey = serde_json::from_value(member_key)?;
        let qualifier = match qualifier {
            None => None,
            Some(QualifiedCatalogName::GLOBAL) => Some(CatalogQualifier::Global),
            Some(slug) => Some(CatalogQualifier::Org(OrgSlug::try_new(slug)?)),
        };
        let kind = match member_key {
            DescriptorMemberKey::Runtime { .. } => DescriptorKind::Runtime,
            DescriptorMemberKey::Model { .. } => DescriptorKind::Model,
        };
        let selector = MemberSelector::parse_for_entry(input, &an_entry(kind)?)?;
        assert_eq!(selector.name.qualifier, qualifier);
        assert_eq!(selector.member_key, member_key);
        assert_eq!(selector.to_string(), input);
        Ok(())
    }

    #[rstest]
    #[case::empty_flavor("Foo-1::", DescriptorKind::Runtime)]
    #[case::empty_name("::linux", DescriptorKind::Runtime)]
    #[case::bad_name("foo:bar", DescriptorKind::Runtime)]
    #[case::trailing_space("Foo-1 ::linux", DescriptorKind::Runtime)]
    #[case::empty_format("x::::q4_0", DescriptorKind::Model)]
    #[case::empty_quantization("x::gguf::", DescriptorKind::Model)]
    #[case::four_parts("x::gguf::q4_0::extra", DescriptorKind::Model)]
    #[case::bad_qualifier("a b/x::linux", DescriptorKind::Runtime)]
    #[case::unknown_format("x::safetensors::bf16", DescriptorKind::Model)]
    #[case::uppercase_quantization("x::gguf::Q4_0", DescriptorKind::Model)]
    #[case::bare_model("AcmeLM-3B", DescriptorKind::Model)]
    #[case::format_only("AcmeLM-3B::gguf", DescriptorKind::Model)]
    #[case::model_key_on_runtime("Foo-1::gguf::q4_0", DescriptorKind::Runtime)]
    fn member_selector_rejects(
        #[case] input: &str,
        #[case] kind: DescriptorKind,
    ) -> anyhow::Result<()> {
        assert!(
            MemberSelector::parse_for_entry(input, &an_entry(kind)?).is_err(),
            "{input}"
        );
        Ok(())
    }

    /// `DescriptorMemberDescriptor` is untagged, and the `type` filter tries a
    /// runtime type first. Both rely on each type name in one family alone.
    #[test]
    fn runtime_and_model_type_names_are_disjoint() {
        let runtimes: BTreeSet<String> = RuntimeType::iter().map(|t| t.to_string()).collect();
        let shared: Vec<String> = ModelType::iter()
            .map(|t| t.to_string())
            .filter(|name| runtimes.contains(name))
            .collect();
        assert!(shared.is_empty(), "type names in both families: {shared:?}");
    }

    #[test]
    fn model_member_key_and_selector_have_a_wire_shape() -> anyhow::Result<()> {
        let summary = DescriptorMemberSummary {
            selector: "Example-1::gguf::q4_0".into(),
            member_key: serde_json::from_value(model_key("gguf", "q4_0"))?,
            coverage: None,
        };
        assert_eq!(
            serde_json::to_value(&summary)?,
            json!({
                "selector": "Example-1::gguf::q4_0",
                "member_key": {"kind": "model", "format": "gguf", "quantization": "q4_0"}
            })
        );
        let hit = DescriptorNameHit {
            selector: "global/Example-1::gguf::q4_0".into(),
            name: CatalogName::try_new("Example-1")?,
            scope: CatalogScope::Global,
            display_name: "global/Example-1".into(),
            member_key: serde_json::from_value(model_key("gguf", "q4_0"))?,
            descriptor_kind: DescriptorKind::Model,
            kind: DescriptorEntryKind::HandMade,
            state: DescriptorEntryState::Active,
        };
        assert_eq!(
            serde_json::to_value(&hit)?,
            json!({
                "selector": "global/Example-1::gguf::q4_0",
                "name": "Example-1",
                "scope": "global",
                "display_name": "global/Example-1",
                "member_key": {"kind": "model", "format": "gguf", "quantization": "q4_0"},
                "descriptor_kind": "model",
                "kind": "hand-made",
                "state": "active"
            })
        );
        let decoded: DescriptorNameHit = serde_json::from_value(serde_json::to_value(&hit)?)?;
        assert_eq!(decoded, hit);
        Ok(())
    }
}
