//! `VlmPromptProfile`: the model family whose VLMEvalKit prompts a `vlm_eval`
//! asks the evals server for, loose on the wire as [`super::VlmEvalTask`] is,
//! since the server may build profiles this client doesn't know yet.

use serde::{Deserialize, Serialize};
use strum::{AsRefStr, Display, EnumString, VariantArray};

/// The prompt profiles the evals server builds, spelled exactly as its samples
/// endpoint's `prompt_profile` query takes them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Display, AsRefStr, EnumString, VariantArray)]
#[strum(serialize_all = "snake_case")]
pub enum KnownVlmPromptProfile {
    Internvl,
    // InternVL3.5's prompts after its R1 system prompt, the only way its
    // thinking mode turns on.
    InternvlR1,
    GraniteVision,
    MinicpmV,
    MinicpmVCot,
    LfmVl,
    // snake_case would give `lfm2_5vl3b`; the evals server spells `lfm2_5_vl_3b`.
    #[strum(serialize = "lfm2_5_vl_3b")]
    Lfm2_5Vl3b,
    // Qwen3.5's model-card prompt (its answer-field format on multiple choice).
    #[strum(serialize = "qwen3_5")]
    Qwen3_5,
}

impl KnownVlmPromptProfile {
    /// All variants, in declaration order.
    pub const ALL: &'static [Self] = <Self as VariantArray>::VARIANTS;
}

/// A profile in a strict [`super::BenchmarkDefinition::VlmEval`]. Parsing never
/// fails or rewrites a profile: an unknown one is kept verbatim, and the evals
/// server refuses a name it does not build.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "String", into = "String")]
pub enum VlmPromptProfile {
    Known(KnownVlmPromptProfile),
    Unknown(String),
}

impl VlmPromptProfile {
    /// The wire spelling.
    pub fn as_str(&self) -> &str {
        match self {
            Self::Known(known) => known.as_ref(),
            Self::Unknown(raw) => raw,
        }
    }
}

impl From<String> for VlmPromptProfile {
    fn from(raw: String) -> Self {
        raw.parse().map_or(Self::Unknown(raw), Self::Known)
    }
}

impl From<&str> for VlmPromptProfile {
    fn from(raw: &str) -> Self {
        raw.parse()
            .map_or_else(|_| Self::Unknown(raw.to_string()), Self::Known)
    }
}

impl From<VlmPromptProfile> for String {
    fn from(profile: VlmPromptProfile) -> Self {
        match profile {
            VlmPromptProfile::Known(known) => known.as_ref().to_string(),
            VlmPromptProfile::Unknown(raw) => raw,
        }
    }
}

impl std::fmt::Display for VlmPromptProfile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    /// Must match the evals server's profile names exactly, or the samples
    /// fetch is a `400`.
    #[rstest]
    #[case(KnownVlmPromptProfile::Internvl, "internvl")]
    #[case(KnownVlmPromptProfile::InternvlR1, "internvl_r1")]
    #[case(KnownVlmPromptProfile::GraniteVision, "granite_vision")]
    #[case(KnownVlmPromptProfile::MinicpmV, "minicpm_v")]
    #[case(KnownVlmPromptProfile::MinicpmVCot, "minicpm_v_cot")]
    #[case(KnownVlmPromptProfile::LfmVl, "lfm_vl")]
    #[case(KnownVlmPromptProfile::Lfm2_5Vl3b, "lfm2_5_vl_3b")]
    #[case(KnownVlmPromptProfile::Qwen3_5, "qwen3_5")]
    fn a_known_profile_is_spelled_as_the_evals_server_spells_it(
        #[case] profile: KnownVlmPromptProfile,
        #[case] wire: &str,
    ) {
        assert_eq!(profile.to_string(), wire);
        assert_eq!(
            VlmPromptProfile::from(wire),
            VlmPromptProfile::Known(profile)
        );
    }

    #[test]
    fn every_known_profile_round_trips() {
        KnownVlmPromptProfile::ALL.iter().for_each(|&profile| {
            assert_eq!(
                VlmPromptProfile::from(profile.to_string()),
                VlmPromptProfile::Known(profile)
            );
        });
    }

    #[rstest]
    #[case::known(
        VlmPromptProfile::Known(KnownVlmPromptProfile::MinicpmVCot),
        "minicpm_v_cot"
    )]
    #[case::unknown(VlmPromptProfile::Unknown("qwen2_vl".to_string()), "qwen2_vl")]
    fn a_profile_round_trips_through_string_losslessly(
        #[case] profile: VlmPromptProfile,
        #[case] wire: &str,
    ) -> anyhow::Result<()> {
        assert_eq!(profile.as_str(), wire);
        assert_eq!(profile.to_string(), wire);
        assert_eq!(serde_json::to_value(&profile)?, wire);
        assert_eq!(VlmPromptProfile::from(wire), profile);
        assert_eq!(VlmPromptProfile::from(wire.to_owned()), profile);
        assert_eq!(
            serde_json::from_value::<VlmPromptProfile>(wire.into())?,
            profile
        );
        Ok(())
    }
}
