//! Request bodies for an organization's descriptor catalog writes.

use chrono::{DateTime, Utc};
use serde::Deserialize;

use crate::{CatalogName, Model, NonEmptyVec, QuantizationLabel, Runtime};

/// A new entry with exactly one kind of member: `runtimes` or `models`.
#[derive(Debug, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct CreateDescriptorEntryRequest {
    pub name: CatalogName,
    pub release_time: Option<DateTime<Utc>>,
    pub runtimes: Option<NonEmptyVec<Runtime>>,
    pub models: Option<NonEmptyVec<ModelMemberRequest>>,
}

/// One model member: the model table of a plan and the quantization label.
/// The server derives the format from the model type.
#[derive(Debug, Deserialize)]
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
