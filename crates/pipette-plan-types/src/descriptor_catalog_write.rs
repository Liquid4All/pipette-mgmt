//! Request bodies for an organization's descriptor catalog writes.

use jiff::Timestamp;
use serde::Deserialize;

use crate::{CatalogName, LogoUri, Model, ModelFormat, NonEmptyVec, QuantizationLabel, Runtime};

/// A new entry with exactly one kind of member: `runtimes` or `models`.
#[derive(Debug, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct CreateDescriptorEntryRequest {
    pub name: CatalogName,
    #[cfg_attr(feature = "schemars", schemars(with = "Option<String>", extend("format" = "date-time")))]
    pub release_time: Option<Timestamp>,
    pub runtimes: Option<NonEmptyVec<Runtime>>,
    pub models: Option<NonEmptyVec<ModelMemberRequest>>,
    /// The family of a model entry. Without it, readers see the text of the
    /// name before its first hyphen. A runtime entry refuses a family.
    #[serde(default)]
    pub family: Option<CatalogName>,
}

/// One model member: the model table of a plan and the quantization label.
/// The server derives the format from the model type.
#[derive(Debug, Clone, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct ModelMemberRequest {
    #[cfg_attr(feature = "schemars", schemars(with = "String"))]
    pub quantization: QuantizationLabel,
    pub model: Model,
}

/// One member to append: a `runtime`, or a `model` with its `quantization`.
#[derive(Debug, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct AddDescriptorMemberRequest {
    pub runtime: Option<Runtime>,
    pub model: Option<Model>,
    #[cfg_attr(feature = "schemars", schemars(with = "Option<String>"))]
    pub quantization: Option<QuantizationLabel>,
}

#[derive(Debug, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct ChangeDescriptorEntryStateRequest {
    pub action: DescriptorEntryAction,
    pub confirmation: Option<String>,
}

/// A new family override for a model entry. Omit `family` to leave it as stored.
/// Use `null` to clear the override and use the default family.
#[derive(Debug, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct SetDescriptorEntryFamilyRequest {
    #[serde(default, deserialize_with = "deserialize_family")]
    #[cfg_attr(feature = "schemars", schemars(with = "Option<CatalogName>"))]
    pub family: Option<Option<CatalogName>>,
}

fn deserialize_family<'de, D>(deserializer: D) -> Result<Option<Option<CatalogName>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Option::<CatalogName>::deserialize(deserializer).map(Some)
}

/// A new logo for a model family. Use `null` to remove the logo.
#[derive(Debug, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct SetDescriptorFamilyLogoRequest {
    // A field with `deserialize_with` and no default is required, so a body
    // must give `null` to remove a logo.
    #[serde(deserialize_with = "Option::deserialize")]
    #[cfg_attr(
        feature = "schemars",
        schemars(required, schema_with = "crate::body_values::nullable::<LogoUri>")
    )]
    pub logo_uri: Option<LogoUri>,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum DescriptorEntryAction {
    Pin,
    Unpin,
    Hide,
    Unhide,
    Tombstone,
}

/// A new name for a hand-made entry.
#[derive(Debug, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct RenameDescriptorEntryRequest {
    pub name: CatalogName,
}

/// A new quantization label for the model member with `format` and the label
/// `from`.
#[derive(Debug, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct RelabelDescriptorMemberRequest {
    pub format: ModelFormat,
    #[cfg_attr(feature = "schemars", schemars(with = "String"))]
    pub from: QuantizationLabel,
    #[cfg_attr(feature = "schemars", schemars(with = "String"))]
    pub to: QuantizationLabel,
}

/// The member to remove from a hand-made entry.
#[derive(Debug, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct RemoveDescriptorMemberRequest {
    /// The member key: a runtime member's flavor, `-` for a runtime member
    /// without a flavor, or a model member's `format::quantization`.
    pub member: String,
}
