//! The checks of the campaign group policies, declared before `plans` so a
//! revision's change can name them.

use serde::{Deserialize, Serialize};

/// The check a campaign group policy acts on. A cell the check flags is re-run
/// up to the policy's `max_reruns` times; what becomes of a cell still flagged
/// after that is fixed per check. `ModelParams` and `SmallerFailed` only flag. Ordered as listed,
/// which is kind order.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Serialize,
    Deserialize,
    strum::AsRefStr,
    strum::Display,
    strum::EnumIter,
)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum PolicyKind {
    /// A run that swapped; a cell still flagged is marked failed.
    Swap,
    /// A result whose standard deviation is more than `percent` of its value;
    /// a cell still flagged is marked failed.
    Spread,
    /// A run its device reported failed with one of the policy's `codes`; a
    /// cell still flagged keeps its result.
    Failed,
    /// A time to first token over the end-to-end latency; a cell still flagged
    /// keeps its result.
    TtftOverE2e,
    /// A result row whose model resolves to no entry of the model params
    /// table, so the pages show it without a total or active parameter count.
    /// It never re-runs or marks a job, and files nothing.
    ModelParams,
    /// A failed job whose family, the campaign's cells that differ only in
    /// their sizes, holds a larger job with rows and no failure. Only the
    /// preview reads it; it never re-runs or marks a job, and files nothing.
    SmallerFailed,
}
