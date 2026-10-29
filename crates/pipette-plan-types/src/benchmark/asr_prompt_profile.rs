//! `AsrPromptProfile`: the model family whose ASR recipe the evals server asks
//! an `asr_eval`'s samples in. Loose on the wire as
//! [`super::VlmPromptProfile`] is.

use serde::{Deserialize, Serialize};
use strum::{AsRefStr, Display, EnumString, VariantArray};

/// The ASR prompt profiles the evals server builds, spelled exactly as its
/// samples endpoint's `prompt_profile` query takes them.
///
/// Each profile's recipe (its "card" in this crate) is its family's public
/// one:
///
/// - `gemma4`: Google's gemma-4-E2B-it model card at
///   `3e22461f65e89153144f8adb70e3b8c2cc9845a7`, "Best Practices" (the audio
///   ASR prompt, and sampling at temperature `1.0`, top_p `0.95`, top_k `64`),
///   with 512 tokens.
/// - `qwen25_omni`: Qwen's ASR cookbook (QwenLM/Qwen2.5-Omni at `d8a31ca5`,
///   `cookbooks/universal_audio_understanding.ipynb`): greedy, 256 tokens.
/// - `voxtral`: Mistral's transcription usage (the Voxtral-Mini-3B-2507 card at
///   `3060fe34`, and mistral-common's `TranscriptionRequest` at `b5416172`):
///   temperature `0.0`, 500 tokens.
/// - `minicpm_o`: OpenBMB's `modeling_minicpmo.py` (openbmb/MiniCPM-o-4_5 at
///   `d59117d1`, `prepare_generation_config`: three beams at repetition
///   penalty `1.2`, or greedy at `1.02`) and OpenBMB/OmniEvalKit at `1adac057`
///   (`o_e_Kit/configs/generation_configs.json`: three beams, no sampling,
///   128 tokens). The card is the 3-beam recipe; an entry's unpinned beam
///   penalties are its `1.2` and `1.0`, and a greedy entry or another beam
///   width is a deviation from it.
/// - `lfm25_audio`: the LiquidAI/LFM2.5-Audio-1.5B card at `c362a062`, which
///   points to the Liquid4All/liquid-audio README (`19e65845`, "ASR"):
///   greedy text decoding. The catalog's entries cap a transcript at 256 new tokens,
///   pipette's own: the README's 512 is its general text default, and on the
///   Open ASR Leaderboard sets it only lets a rare repetition loop run longer.
/// - `lfm3`: no public card; the recipe renders the model's native ASR chat
///   format, greedy, and the entries' 256-token cap is pipette's own.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Display, AsRefStr, EnumString, VariantArray)]
pub enum KnownAsrPromptProfile {
    #[strum(serialize = "gemma4")]
    Gemma4,
    #[strum(serialize = "qwen25_omni")]
    Qwen25Omni,
    #[strum(serialize = "voxtral")]
    Voxtral,
    #[strum(serialize = "minicpm_o")]
    MinicpmO,
    #[strum(serialize = "lfm25_audio")]
    Lfm25Audio,
    #[strum(serialize = "lfm3")]
    Lfm3,
}

impl KnownAsrPromptProfile {
    /// All variants, in declaration order.
    pub const ALL: &'static [Self] = <Self as VariantArray>::VARIANTS;
}

/// A profile in a strict [`super::BenchmarkDefinition::AsrEval`]. Parsing never
/// fails or rewrites a profile: an unknown one is kept verbatim, and the evals
/// server refuses a name it does not build.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "String", into = "String")]
pub enum AsrPromptProfile {
    Known(KnownAsrPromptProfile),
    Unknown(String),
}

impl AsrPromptProfile {
    /// The wire spelling.
    pub fn as_str(&self) -> &str {
        match self {
            Self::Known(known) => known.as_ref(),
            Self::Unknown(raw) => raw,
        }
    }
}

impl From<String> for AsrPromptProfile {
    fn from(raw: String) -> Self {
        raw.parse().map_or(Self::Unknown(raw), Self::Known)
    }
}

impl From<&str> for AsrPromptProfile {
    fn from(raw: &str) -> Self {
        raw.parse()
            .map_or_else(|_| Self::Unknown(raw.to_string()), Self::Known)
    }
}

impl From<AsrPromptProfile> for String {
    fn from(profile: AsrPromptProfile) -> Self {
        match profile {
            AsrPromptProfile::Known(known) => known.as_ref().to_string(),
            AsrPromptProfile::Unknown(raw) => raw,
        }
    }
}

impl std::fmt::Display for AsrPromptProfile {
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
    #[case(KnownAsrPromptProfile::Gemma4, "gemma4")]
    #[case(KnownAsrPromptProfile::Qwen25Omni, "qwen25_omni")]
    #[case(KnownAsrPromptProfile::Voxtral, "voxtral")]
    #[case(KnownAsrPromptProfile::MinicpmO, "minicpm_o")]
    #[case(KnownAsrPromptProfile::Lfm25Audio, "lfm25_audio")]
    #[case(KnownAsrPromptProfile::Lfm3, "lfm3")]
    fn a_known_profile_is_spelled_as_the_evals_server_spells_it(
        #[case] profile: KnownAsrPromptProfile,
        #[case] wire: &str,
    ) {
        assert_eq!(profile.to_string(), wire);
        assert_eq!(
            AsrPromptProfile::from(wire),
            AsrPromptProfile::Known(profile)
        );
    }

    #[rstest]
    #[case::known(AsrPromptProfile::Known(KnownAsrPromptProfile::MinicpmO), "minicpm_o")]
    #[case::unknown(AsrPromptProfile::Unknown("whisper".to_string()), "whisper")]
    fn a_profile_round_trips_through_string_losslessly(
        #[case] profile: AsrPromptProfile,
        #[case] wire: &str,
    ) -> anyhow::Result<()> {
        assert_eq!(profile.as_str(), wire);
        assert_eq!(serde_json::to_value(&profile)?, wire);
        assert_eq!(
            serde_json::from_value::<AsrPromptProfile>(wire.into())?,
            profile
        );
        Ok(())
    }
}
