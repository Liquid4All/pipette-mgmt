//! `VlmEvalTask`: the lmms-eval task a `vlm_eval` names, loose on the wire and
//! strict inside as [`super::EvalId`] is, since the server may list tasks this
//! client doesn't know yet.

use serde::{Deserialize, Serialize};
use strum::{AsRefStr, Display, EnumString, VariantArray};

/// The tasks the evals server holds a pinned VLM dataset for, spelled exactly
/// as its `POST /score` spells them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Display, AsRefStr, EnumString, VariantArray)]
#[strum(serialize_all = "snake_case")]
pub enum KnownVlmEvalTask {
    Mme,
    Mmstar,
    Ocrbench,
    MmmuVal,
    Realworldqa,
    MmbenchEnDev,
    Pope,
}

impl KnownVlmEvalTask {
    /// All variants, in declaration order.
    pub const ALL: &'static [Self] = <Self as VariantArray>::VARIANTS;
}

/// A task in a strict [`super::BenchmarkDefinition::VlmEval`]. Parsing never
/// fails or rewrites a task: an unknown one is kept verbatim.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "String", into = "String")]
pub enum VlmEvalTask {
    Known(KnownVlmEvalTask),
    Unknown(String),
}

impl VlmEvalTask {
    /// The wire spelling.
    pub fn as_str(&self) -> &str {
        match self {
            Self::Known(known) => known.as_ref(),
            Self::Unknown(raw) => raw,
        }
    }
}

impl From<String> for VlmEvalTask {
    fn from(raw: String) -> Self {
        raw.parse().map_or(Self::Unknown(raw), Self::Known)
    }
}

impl From<&str> for VlmEvalTask {
    fn from(raw: &str) -> Self {
        raw.parse()
            .map_or_else(|_| Self::Unknown(raw.to_string()), Self::Known)
    }
}

impl From<VlmEvalTask> for String {
    fn from(task: VlmEvalTask) -> Self {
        match task {
            VlmEvalTask::Known(known) => known.as_ref().to_string(),
            VlmEvalTask::Unknown(raw) => raw,
        }
    }
}

impl std::fmt::Display for VlmEvalTask {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    /// Must match the evals server's task names exactly, or `POST /score` 404s.
    #[rstest]
    #[case(KnownVlmEvalTask::Mme, "mme")]
    #[case(KnownVlmEvalTask::Mmstar, "mmstar")]
    #[case(KnownVlmEvalTask::Ocrbench, "ocrbench")]
    #[case(KnownVlmEvalTask::MmmuVal, "mmmu_val")]
    #[case(KnownVlmEvalTask::Realworldqa, "realworldqa")]
    #[case(KnownVlmEvalTask::MmbenchEnDev, "mmbench_en_dev")]
    #[case(KnownVlmEvalTask::Pope, "pope")]
    fn a_known_task_is_spelled_as_the_scorer_spells_it(
        #[case] task: KnownVlmEvalTask,
        #[case] wire: &str,
    ) {
        assert_eq!(task.to_string(), wire);
        assert_eq!(VlmEvalTask::from(wire), VlmEvalTask::Known(task));
    }

    #[test]
    fn every_known_task_round_trips() {
        KnownVlmEvalTask::ALL.iter().for_each(|&task| {
            assert_eq!(
                VlmEvalTask::from(task.to_string()),
                VlmEvalTask::Known(task)
            );
        });
    }

    #[rstest]
    #[case::known(VlmEvalTask::Known(KnownVlmEvalTask::MmmuVal), "mmmu_val")]
    #[case::unknown(VlmEvalTask::Unknown("mathvista".to_string()), "mathvista")]
    fn a_task_round_trips_through_string_losslessly(
        #[case] task: VlmEvalTask,
        #[case] wire: &str,
    ) -> anyhow::Result<()> {
        assert_eq!(task.as_str(), wire);
        assert_eq!(task.to_string(), wire);
        assert_eq!(serde_json::to_value(&task)?, wire);
        assert_eq!(VlmEvalTask::from(wire), task);
        assert_eq!(VlmEvalTask::from(wire.to_owned()), task);
        assert_eq!(serde_json::from_value::<VlmEvalTask>(wire.into())?, task);
        Ok(())
    }
}
