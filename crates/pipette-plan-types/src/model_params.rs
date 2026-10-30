//! `/api/v1/model-params`: the model parameter table, one editable row of parameter
//! counts per model.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{ModelKey, NonEmptyTrimmedString, PositiveCount, UserId};

/// One row: a model's parameter counts in millions, under its model key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct ModelParams {
    /// The model name in normalized form, such as `lfm2-700m` or a vault
    /// resource id.
    pub model_key: ModelKey,
    pub total_millions: PositiveCount,
    /// Equal to `total_millions` for a dense model.
    pub active_millions: PositiveCount,
    /// Where the counts come from, as whoever wrote the row put it.
    pub note: Option<NonEmptyTrimmedString>,
    pub updated_at: DateTime<Utc>,
    /// `null` on a row no account wrote: a seeded or imported one.
    #[cfg_attr(feature = "schemars", schemars(with = "Option<String>"))]
    pub updated_by: Option<UserId>,
}

/// `POST /api/v1/model-params` request body. `model_key` is any name the table
/// lookup accepts (`LiquidAI/LFM2-700M-GGUF`, a vault id); the row is keyed by
/// its normalized form.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct CreateModelParamsRequest {
    /// Read under its former name `model` too.
    #[serde(alias = "model")]
    pub model_key: String,
    pub total_millions: PositiveCount,
    /// Defaults to `total_millions`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_millions: Option<PositiveCount>,
    /// Where the counts come from. Trimmed; a blank one is none.
    #[serde(
        default,
        deserialize_with = "crate::empty_as_none",
        skip_serializing_if = "Option::is_none"
    )]
    pub note: Option<NonEmptyTrimmedString>,
}

/// `PUT /api/v1/model-params/{model_key}` request body: the row's new counts and
/// note. The row is replaced whole, so a note left out is dropped.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct UpdateModelParamsRequest {
    pub total_millions: PositiveCount,
    /// Defaults to `total_millions`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_millions: Option<PositiveCount>,
    #[serde(
        default,
        deserialize_with = "crate::empty_as_none",
        skip_serializing_if = "Option::is_none"
    )]
    pub note: Option<NonEmptyTrimmedString>,
}
