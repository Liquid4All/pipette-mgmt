//! The recorded results of catalog imports and refreshes.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// The catalog command that a run executed.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, strum::EnumString, strum::AsRefStr,
)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum CatalogImportCommand {
    Import,
    Refresh,
}

/// The stored result of a run.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, strum::EnumString, strum::AsRefStr,
)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum CatalogImportResult {
    Running,
    Success,
    Error,
}

/// The outcome of a run in the run history. Each value matches one
/// `CatalogImportResult`; `failed` is the `error` result.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum CatalogImportOutcome {
    Running,
    Success,
    Failed,
}

impl From<CatalogImportResult> for CatalogImportOutcome {
    fn from(result: CatalogImportResult) -> Self {
        match result {
            CatalogImportResult::Running => Self::Running,
            CatalogImportResult::Success => Self::Success,
            CatalogImportResult::Error => Self::Failed,
        }
    }
}

/// One persisted catalog import or refresh run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct CatalogImportRun {
    pub id: String,
    pub started_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
    pub command: CatalogImportCommand,
    pub source: String,
    pub target_name: Option<String>,
    pub result: CatalogImportResult,
    pub outcome: CatalogImportOutcome,
    pub entries_created: i32,
    pub entries_hidden: i32,
    pub members_added: i32,
    pub unclassified_assets: Vec<String>,
    pub error_summary: Option<String>,
}

/// The recent runs and the last successful import, even if it is older than
/// every run in `items`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct CatalogImportRuns {
    pub items: Vec<CatalogImportRun>,
    pub last_successful_import_at: Option<DateTime<Utc>>,
}

/// The number of newest catalog runs to read.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct CatalogImportRunsQuery {
    /// Runs to return: 50 by default, at most 200 (the server's
    /// `list_default_limit` and `list_max_limit`). `0` or a value over the
    /// maximum is a `400`.
    #[cfg_attr(feature = "schemars", schemars(range(min = 1)))]
    pub limit: Option<usize>,
}
