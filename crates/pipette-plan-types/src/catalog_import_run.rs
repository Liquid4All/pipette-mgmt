//! The recorded results of catalog imports and refreshes.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// The `error_summary` of a run whose only error is its unclassified assets.
pub fn unclassified_summary(count: usize) -> String {
    format!("{count} unclassified assets")
}

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

/// What a run means to an operator. `unclassified` is an `error` run whose only
/// error is its unclassified assets: the run finished its catalog work.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum CatalogImportOutcome {
    Running,
    Success,
    Unclassified,
    Failed,
}

impl CatalogImportOutcome {
    pub fn of(
        result: CatalogImportResult,
        unclassified_assets: &[String],
        error_summary: Option<&str>,
    ) -> Self {
        match result {
            CatalogImportResult::Running => Self::Running,
            CatalogImportResult::Success => Self::Success,
            CatalogImportResult::Error
                if !unclassified_assets.is_empty()
                    && error_summary
                        == Some(unclassified_summary(unclassified_assets.len()).as_str()) =>
            {
                Self::Unclassified
            }
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

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    #[rstest]
    #[case::running(CatalogImportResult::Running, &[], None, CatalogImportOutcome::Running)]
    #[case::success(CatalogImportResult::Success, &[], None, CatalogImportOutcome::Success)]
    #[case::unclassified_only(
        CatalogImportResult::Error,
        &["b1/new.tar.gz"],
        Some("1 unclassified assets"),
        CatalogImportOutcome::Unclassified
    )]
    #[case::unclassified_with_another_error(
        CatalogImportResult::Error,
        &["b1/new.tar.gz"],
        Some("GitHub unreachable"),
        CatalogImportOutcome::Failed
    )]
    #[case::failed(
        CatalogImportResult::Error,
        &[],
        Some("GitHub unreachable"),
        CatalogImportOutcome::Failed
    )]
    fn an_outcome_separates_unclassified_assets_from_a_failure(
        #[case] result: CatalogImportResult,
        #[case] assets: &[&str],
        #[case] error_summary: Option<&str>,
        #[case] expected: CatalogImportOutcome,
    ) {
        let assets: Vec<String> = assets.iter().map(|asset| asset.to_string()).collect();
        assert_eq!(
            CatalogImportOutcome::of(result, &assets, error_summary),
            expected
        );
    }
}
