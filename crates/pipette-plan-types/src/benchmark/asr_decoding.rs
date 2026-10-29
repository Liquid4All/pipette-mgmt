//! An asr_eval entry's decoding: which knobs its prompt profile lets it pin,
//! and the decoding it runs once every knob it leaves unset takes the profile
//! card's value. A profile's card is its family's public recipe, spelled for
//! llama-server; [`KnownAsrPromptProfile`] lists each profile's source.
//!
//! The one table both sides read. mgmt refuses at catalog intake what
//! [`AsrEvalBenchmark::check_decoding`] refuses, a client refuses the same
//! before it downloads the corpus, and the warehouse records
//! [`AsrEvalBenchmark::effective_decoding`], the decoding the client ran.
//! Changing a card value here changes what an existing entry runs, so it is a
//! methodology change: mint new entries rather than edit one in place.
//!
//! Reasoning is never on for an ASR model, so no knob here can turn it on.

use strum::{AsRefStr, Display};
use thiserror::Error;

use crate::benchmark::{
    AsrEvalBenchmark, AsrPromptProfile, BeamLengthPenalty, BeamRepetitionPenalty,
    KnownAsrPromptProfile, MinP, NumBeams, RepeatPenalty, SamplingSeed, SamplingTemperature, TopK,
    TopP,
};

/// A decoding knob an asr_eval entry may pin, spelled as its field.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Display, AsRefStr)]
pub enum AsrDecodingKnob {
    #[strum(serialize = "parameter_temperature")]
    Temperature,
    #[strum(serialize = "parameter_top_p")]
    TopP,
    #[strum(serialize = "parameter_top_k")]
    TopK,
    #[strum(serialize = "parameter_min_p")]
    MinP,
    #[strum(serialize = "parameter_repeat_penalty")]
    RepeatPenalty,
    #[strum(serialize = "parameter_seed")]
    Seed,
    #[strum(serialize = "parameter_num_beams")]
    NumBeams,
    #[strum(serialize = "parameter_beam_repetition_penalty")]
    BeamRepetitionPenalty,
    #[strum(serialize = "parameter_beam_length_penalty")]
    BeamLengthPenalty,
}

/// Why [`AsrEvalBenchmark::check_decoding`] or
/// [`AsrEvalBenchmark::effective_decoding`] refuses an entry.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum AsrDecodingError {
    #[error(
        "parameter_num_beams = 1 is no beam search; drop it, as an entry without it decodes \
         without beams"
    )]
    SingleBeam,
    #[error("{knob} needs parameter_num_beams above 1")]
    BeamPenaltyWithoutBeams { knob: AsrDecodingKnob },
    #[error(
        "{knob} beside parameter_num_beams: a beam search samples nothing; drop it (the beams' \
         own penalty is parameter_beam_repetition_penalty)"
    )]
    SamplingWithBeams { knob: AsrDecodingKnob },
    #[error(
        "parameter_num_beams on the `{profile}` prompt profile: beam search is `minicpm_o`'s \
         recipe, the only profile whose end-of-beam tokens are known"
    )]
    BeamsOffProfile { profile: String },
    #[error(
        "{knob} on the `{profile}` prompt profile, whose card's sampler chain is temperature \
         then top_p; the knob would be ignored, so drop it"
    )]
    KnobOffProfile {
        knob: AsrDecodingKnob,
        profile: String,
    },
    #[error(
        "prompt profile `{profile}` has no card in this build (it knows {known}), so its \
         decoding cannot be resolved"
    )]
    UnknownProfile { profile: String, known: String },
    #[error(
        "parameter_num_beams on vLLM: its beam search takes no repetition penalty, so it cannot \
         run `minicpm_o`'s beam recipe; run this entry on llama.cpp"
    )]
    BeamsOnVllm,
    #[error(
        "prompt profile `{profile}` on vLLM: vLLM has no implementation of its model, so the \
         entry runs on llama.cpp alone"
    )]
    ProfileOffVllm { profile: String },
}

/// The decoding an asr_eval entry runs.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum AsrDecoding {
    /// A beam search: nothing is sampled.
    Beams(AsrBeamSearch),
    Sampling(AsrSampling),
}

/// A beam search, each penalty the entry's or else the card's.
///
/// MiniCPM-o's card, the only one that takes beams, is OpenBMB's beam recipe:
/// three beams (OmniEvalKit's `generation_configs.json`) at repetition penalty
/// `1.2` and length penalty `1.0` (`prepare_generation_config` in
/// `modeling_minicpmo.py`). Any other beam count is the entry's own choice, a
/// deviation from that recipe, and so is a MiniCPM-o entry without beams,
/// which runs the greedy branch of the same `chat()` instead.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AsrBeamSearch {
    pub num_beams: NumBeams,
    /// The card's `1.2` unless pinned.
    pub repetition_penalty: BeamRepetitionPenalty,
    /// The card's `1.0` (the mean log-probability) unless pinned.
    pub length_penalty: BeamLengthPenalty,
}

/// Sampling, every knob the entry's or else the card's, else off.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AsrSampling {
    /// The card's, unless pinned: Gemma 4 samples at `1.0`, every other card
    /// transcribes greedily at `0.0`.
    pub temperature: SamplingTemperature,
    /// The card's, unless pinned: Gemma 4's `0.95`, else `1.0` (off).
    pub top_p: TopP,
    /// The card's, unless pinned: Gemma 4's `64`; `1` for the cards greedy
    /// through top-k (Qwen2.5-Omni, MiniCPM-o and the LFM profiles) while the
    /// temperature is `0.0`, and `0` (off) once a pinned temperature above it
    /// asks them to sample; `0` for Voxtral.
    pub top_k: TopK,
    /// `0.0` (off) unless pinned.
    pub min_p: MinP,
    /// MiniCPM-o's `chat()` greedy branch: `1.02` (`prepare_generation_config`
    /// in OpenBMB's `modeling_minicpmo.py`, openbmb/MiniCPM-o-4_5 at
    /// `d59117d1`); else `1.0` (off), unless pinned.
    pub repeat_penalty: RepeatPenalty,
    pub repeat_window: AsrRepeatWindow,
    /// The base seed, from which [`AsrEvalBenchmark::sample_seed`] derives
    /// each sample's; `None` is unseeded.
    pub seed: Option<SamplingSeed>,
    pub sampler_chain: AsrSamplerChain,
}

/// The tokens a repeat penalty looks back over.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AsrRepeatWindow {
    /// The runtime's own window.
    RuntimeDefault,
    /// The whole sequence, as Transformers' repetition penalty does
    /// (MiniCPM-o); llama-server spells it as the slot's context length.
    WholeSequence,
}

/// The order the samplers run in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AsrSamplerChain {
    /// The runtime's own chain.
    RuntimeDefault,
    /// Temperature, then top_p, and nothing else (Voxtral: pipette's own
    /// chain, in the order vLLM, the server Mistral's card recommends, applies
    /// them). A `top_p` of `1.0` is a no-op, so an entry that pins none runs
    /// temperature alone, as Mistral's transcription usage sets it.
    TemperatureThenTopP,
}

impl KnownAsrPromptProfile {
    /// Whether vLLM runs the profile's model with audio: Gemma 4, Qwen2.5-Omni
    /// (its thinker), Voxtral and MiniCPM-o 4.5 are vLLM 0.32 models; the LFM
    /// audio models are not.
    pub fn runs_on_vllm(self) -> bool {
        match self {
            Self::Gemma4 | Self::Qwen25Omni | Self::Voxtral | Self::MinicpmO => true,
            Self::Lfm25Audio | Self::Lfm3 => false,
        }
    }

    /// Whether the card can decode by beam search: MiniCPM-o's alone, the only
    /// family whose end-of-beam tokens are known.
    pub fn takes_beams(self) -> bool {
        match self {
            Self::MinicpmO => true,
            Self::Gemma4 | Self::Qwen25Omni | Self::Voxtral | Self::Lfm25Audio | Self::Lfm3 => {
                false
            }
        }
    }

    /// The sampling knobs the card's sampler chain would ignore, refused
    /// rather than silently dropped.
    pub fn ignored_knobs(self) -> &'static [AsrDecodingKnob] {
        match self {
            Self::Voxtral => &[
                AsrDecodingKnob::TopK,
                AsrDecodingKnob::MinP,
                AsrDecodingKnob::RepeatPenalty,
            ],
            Self::Gemma4 | Self::Qwen25Omni | Self::MinicpmO | Self::Lfm25Audio | Self::Lfm3 => &[],
        }
    }

    /// The temperature the card samples at when the entry pins none.
    fn card_temperature(self) -> SamplingTemperature {
        match self {
            Self::Gemma4 => SamplingTemperature::try_new(1.0).unwrap_or_default(),
            Self::Qwen25Omni | Self::Voxtral | Self::MinicpmO | Self::Lfm25Audio | Self::Lfm3 => {
                SamplingTemperature::greedy()
            }
        }
    }

    /// The nucleus the card samples with when the entry pins none.
    fn card_top_p(self) -> TopP {
        match self {
            Self::Gemma4 => TopP::try_new(0.95).unwrap_or_default(),
            Self::Qwen25Omni | Self::Voxtral | Self::MinicpmO | Self::Lfm25Audio | Self::Lfm3 => {
                TopP::default()
            }
        }
    }

    /// The top-k cut the card samples with when the entry pins none, at
    /// `temperature`. A card greedy through `top_k 1` keeps that cut only while
    /// it decodes greedily: a pinned temperature above `0` asks it to sample,
    /// which a cut of one token would undo.
    fn card_top_k(self, temperature: SamplingTemperature) -> TopK {
        let greedy = temperature == SamplingTemperature::greedy();
        let k = match self {
            Self::Gemma4 => 64,
            Self::Qwen25Omni | Self::MinicpmO | Self::Lfm25Audio | Self::Lfm3 if greedy => 1,
            Self::Qwen25Omni | Self::MinicpmO | Self::Lfm25Audio | Self::Lfm3 | Self::Voxtral => 0,
        };
        TopK::try_new(k).unwrap_or_default()
    }

    /// The repeat penalty the card samples with when the entry pins none, and
    /// the window it looks back over.
    fn card_repeat_penalty(self) -> (RepeatPenalty, AsrRepeatWindow) {
        match self {
            Self::MinicpmO => (
                RepeatPenalty::try_new(1.02).unwrap_or_default(),
                AsrRepeatWindow::WholeSequence,
            ),
            Self::Gemma4 | Self::Qwen25Omni | Self::Voxtral | Self::Lfm25Audio | Self::Lfm3 => {
                (RepeatPenalty::default(), AsrRepeatWindow::RuntimeDefault)
            }
        }
    }

    /// The repetition and length penalties the card's beam search runs at
    /// when the entry pins none: MiniCPM-o's `1.2` and `1.0`. No other card
    /// takes beams ([`Self::takes_beams`]), so theirs are never read and are
    /// `1.0` (off).
    fn card_beam_penalties(self) -> (BeamRepetitionPenalty, BeamLengthPenalty) {
        match self {
            Self::MinicpmO => (
                BeamRepetitionPenalty::try_new(1.2).unwrap_or_default(),
                BeamLengthPenalty::default(),
            ),
            Self::Gemma4 | Self::Qwen25Omni | Self::Voxtral | Self::Lfm25Audio | Self::Lfm3 => (
                BeamRepetitionPenalty::default(),
                BeamLengthPenalty::default(),
            ),
        }
    }

    fn sampler_chain(self) -> AsrSamplerChain {
        match self {
            Self::Voxtral => AsrSamplerChain::TemperatureThenTopP,
            Self::Gemma4 | Self::Qwen25Omni | Self::MinicpmO | Self::Lfm25Audio | Self::Lfm3 => {
                AsrSamplerChain::RuntimeDefault
            }
        }
    }
}

impl AsrEvalBenchmark {
    /// The temperature the entry runs at: its own, else its profile card's
    /// (Gemma 4 samples at `1.0`), else greedy for a profile this build has
    /// no card for and for a beam search, which samples nothing.
    pub fn sampling_temperature(&self) -> SamplingTemperature {
        if let Some(temperature) = self.parameter_temperature {
            return temperature;
        }
        match &self.parameter_prompt_profile {
            AsrPromptProfile::Known(profile) if !self.beam_search() => profile.card_temperature(),
            AsrPromptProfile::Known(_) | AsrPromptProfile::Unknown(_) => {
                SamplingTemperature::greedy()
            }
        }
    }

    /// Whether the entry decodes by beam search: more than one beam.
    pub fn beam_search(&self) -> bool {
        self.parameter_num_beams
            .is_some_and(|beams| beams.into_inner() > 1)
    }

    /// The sampling knobs the entry pins, in field order.
    fn pinned_sampling(&self) -> impl Iterator<Item = AsrDecodingKnob> {
        [
            (
                AsrDecodingKnob::Temperature,
                self.parameter_temperature.is_some(),
            ),
            (AsrDecodingKnob::TopP, self.parameter_top_p.is_some()),
            (AsrDecodingKnob::TopK, self.parameter_top_k.is_some()),
            (AsrDecodingKnob::MinP, self.parameter_min_p.is_some()),
            (
                AsrDecodingKnob::RepeatPenalty,
                self.parameter_repeat_penalty.is_some(),
            ),
            (AsrDecodingKnob::Seed, self.parameter_seed.is_some()),
        ]
        .into_iter()
        .filter_map(|(knob, pinned)| pinned.then_some(knob))
    }

    /// Whether the entry's decoding is one its prompt profile's card can run:
    ///
    /// - `parameter_num_beams` is above `1` when pinned;
    /// - a beam penalty only beside beams;
    /// - no sampling knob beside beams (any pinned temperature, `0.0`
    ///   included, top_p, top_k, min_p, repeat_penalty or seed), as a beam
    ///   search samples nothing;
    /// - beams only on `minicpm_o`;
    /// - no knob the profile's sampler chain ignores (Voxtral's top_k, min_p
    ///   and repeat_penalty).
    ///
    /// The task's language is not checked: every profile runs every task, and
    /// a cell outside the languages its card declares is marked, not refused
    /// ([`super::AsrEvalBenchmark::within_declared_languages`]).
    ///
    /// A profile this build does not know is held to the profile-free rules
    /// and may not take beams, so a catalog naming one still loads;
    /// [`Self::effective_decoding`] refuses it.
    pub fn check_decoding(&self) -> Result<(), AsrDecodingError> {
        if self
            .parameter_num_beams
            .is_some_and(|beams| beams.into_inner() == 1)
        {
            return Err(AsrDecodingError::SingleBeam);
        }
        let penalties = [
            (
                AsrDecodingKnob::BeamRepetitionPenalty,
                self.parameter_beam_repetition_penalty.is_some(),
            ),
            (
                AsrDecodingKnob::BeamLengthPenalty,
                self.parameter_beam_length_penalty.is_some(),
            ),
        ];
        if !self.beam_search()
            && let Some((knob, _)) = penalties.iter().find(|(_, pinned)| *pinned)
        {
            return Err(AsrDecodingError::BeamPenaltyWithoutBeams { knob: *knob });
        }
        if self.beam_search() {
            if let Some(knob) = self.pinned_sampling().next() {
                return Err(AsrDecodingError::SamplingWithBeams { knob });
            }
            let takes_beams = match &self.parameter_prompt_profile {
                AsrPromptProfile::Known(profile) => profile.takes_beams(),
                AsrPromptProfile::Unknown(_) => false,
            };
            if !takes_beams {
                return Err(AsrDecodingError::BeamsOffProfile {
                    profile: self.parameter_prompt_profile.to_string(),
                });
            }
        }
        if let AsrPromptProfile::Known(profile) = &self.parameter_prompt_profile
            && let Some(knob) = self
                .pinned_sampling()
                .find(|knob| profile.ignored_knobs().contains(knob))
        {
            return Err(AsrDecodingError::KnobOffProfile {
                knob,
                profile: profile.to_string(),
            });
        }
        Ok(())
    }

    /// Whether the entry can run on `runtime`, beyond [`Self::check_decoding`]:
    /// on vLLM, only a profile whose model vLLM implements, and no beams,
    /// since vLLM's beam search has no repetition penalty to run MiniCPM-o's
    /// beam recipe with. llama.cpp runs every entry that check admits.
    pub fn check_runtime(&self, runtime: crate::RuntimeType) -> Result<(), AsrDecodingError> {
        if runtime != crate::RuntimeType::UvVllm {
            return Ok(());
        }
        let on_vllm = match &self.parameter_prompt_profile {
            AsrPromptProfile::Known(profile) => profile.runs_on_vllm(),
            AsrPromptProfile::Unknown(_) => false,
        };
        if !on_vllm {
            return Err(AsrDecodingError::ProfileOffVllm {
                profile: self.parameter_prompt_profile.to_string(),
            });
        }
        if self.beam_search() {
            return Err(AsrDecodingError::BeamsOnVllm);
        }
        Ok(())
    }

    /// The decoding the entry runs: its pinned knobs, with every knob it
    /// leaves unset at its profile card's value, else off. Refuses what
    /// [`Self::check_decoding`] refuses, and a profile this build has no card
    /// for.
    pub fn effective_decoding(&self) -> Result<AsrDecoding, AsrDecodingError> {
        self.check_decoding()?;
        let profile = match &self.parameter_prompt_profile {
            AsrPromptProfile::Known(profile) => *profile,
            AsrPromptProfile::Unknown(profile) => {
                return Err(AsrDecodingError::UnknownProfile {
                    profile: profile.clone(),
                    known: KnownAsrPromptProfile::ALL
                        .iter()
                        .map(ToString::to_string)
                        .collect::<Vec<_>>()
                        .join(", "),
                });
            }
        };
        if let Some(num_beams) = self.parameter_num_beams.filter(|_| self.beam_search()) {
            let (card_repetition, card_length) = profile.card_beam_penalties();
            return Ok(AsrDecoding::Beams(AsrBeamSearch {
                num_beams,
                repetition_penalty: self
                    .parameter_beam_repetition_penalty
                    .unwrap_or(card_repetition),
                length_penalty: self.parameter_beam_length_penalty.unwrap_or(card_length),
            }));
        }
        let (card_repeat_penalty, repeat_window) = profile.card_repeat_penalty();
        let temperature = self.sampling_temperature();
        Ok(AsrDecoding::Sampling(AsrSampling {
            temperature,
            top_p: self.parameter_top_p.unwrap_or_else(|| profile.card_top_p()),
            top_k: self
                .parameter_top_k
                .unwrap_or_else(|| profile.card_top_k(temperature)),
            min_p: self.parameter_min_p.unwrap_or_default(),
            repeat_penalty: self.parameter_repeat_penalty.unwrap_or(card_repeat_penalty),
            repeat_window,
            seed: self.parameter_seed,
            sampler_chain: profile.sampler_chain(),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::PositiveCount;
    use crate::benchmark::BenchmarkId;

    fn entry(profile: &str) -> anyhow::Result<AsrEvalBenchmark> {
        Ok(AsrEvalBenchmark {
            benchmark_id: BenchmarkId::try_new("asr_eval_x")?,
            parameter_asr_task: "asr_esb_librispeech_test_clean".into(),
            parameter_dataset_version: "2026.10.1".to_string(),
            parameter_sample_count: PositiveCount::try_new(2620)?,
            parameter_prompt_profile: profile.into(),
            parameter_max_tokens: PositiveCount::try_new(256)?,
            parameter_temperature: None,
            parameter_top_p: None,
            parameter_top_k: None,
            parameter_min_p: None,
            parameter_repeat_penalty: None,
            parameter_seed: None,
            parameter_seed_rule: None,
            parameter_num_beams: None,
            parameter_beam_repetition_penalty: None,
            parameter_beam_length_penalty: None,
            dataset: None,
            recipe: None,
        })
    }

    fn beams(profile: &str, n: i32) -> anyhow::Result<AsrEvalBenchmark> {
        Ok(AsrEvalBenchmark {
            parameter_num_beams: Some(NumBeams::try_new(n)?),
            ..entry(profile)?
        })
    }

    /// What every known profile lets an entry pin, and what it refuses.
    #[rstest::rstest]
    #[case::greedy_everywhere("gemma4", None, Ok(()))]
    #[case::beams_on_minicpm(
        "minicpm_o",
        Some(beams("minicpm_o", 3)),
        Ok(())
    )]
    #[case::one_beam(
        "minicpm_o",
        Some(beams("minicpm_o", 1)),
        Err(AsrDecodingError::SingleBeam)
    )]
    #[case::beams_on_gemma(
        "gemma4",
        Some(beams("gemma4", 3)),
        Err(AsrDecodingError::BeamsOffProfile { profile: "gemma4".into() })
    )]
    #[case::beams_on_qwen(
        "qwen25_omni",
        Some(beams("qwen25_omni", 2)),
        Err(AsrDecodingError::BeamsOffProfile { profile: "qwen25_omni".into() })
    )]
    #[case::beams_on_voxtral(
        "voxtral",
        Some(beams("voxtral", 2)),
        Err(AsrDecodingError::BeamsOffProfile { profile: "voxtral".into() })
    )]
    #[case::beams_on_lfm25(
        "lfm25_audio",
        Some(beams("lfm25_audio", 2)),
        Err(AsrDecodingError::BeamsOffProfile { profile: "lfm25_audio".into() })
    )]
    #[case::beams_on_lfm3(
        "lfm3",
        Some(beams("lfm3", 2)),
        Err(AsrDecodingError::BeamsOffProfile { profile: "lfm3".into() })
    )]
    #[case::beams_on_an_unknown_profile(
        "phi4",
        Some(beams("phi4", 2)),
        Err(AsrDecodingError::BeamsOffProfile { profile: "phi4".into() })
    )]
    #[case::zero_temperature_beside_beams(
        "minicpm_o",
        Some(Ok(AsrEvalBenchmark {
            parameter_temperature: Some(SamplingTemperature::greedy()),
            ..beams("minicpm_o", 3)?
        })),
        Err(AsrDecodingError::SamplingWithBeams { knob: AsrDecodingKnob::Temperature })
    )]
    #[case::repeat_penalty_beside_beams(
        "minicpm_o",
        Some(Ok(AsrEvalBenchmark {
            parameter_repeat_penalty: Some(RepeatPenalty::try_new(1.0)?),
            ..beams("minicpm_o", 3)?
        })),
        Err(AsrDecodingError::SamplingWithBeams { knob: AsrDecodingKnob::RepeatPenalty })
    )]
    #[case::length_penalty_without_beams(
        "minicpm_o",
        Some(Ok(AsrEvalBenchmark {
            parameter_beam_length_penalty: Some(BeamLengthPenalty::try_new(1.0)?),
            ..entry("minicpm_o")?
        })),
        Err(AsrDecodingError::BeamPenaltyWithoutBeams { knob: AsrDecodingKnob::BeamLengthPenalty })
    )]
    #[case::voxtral_top_k(
        "voxtral",
        Some(Ok(AsrEvalBenchmark { parameter_top_k: Some(TopK::try_new(5)?), ..entry("voxtral")? })),
        Err(AsrDecodingError::KnobOffProfile { knob: AsrDecodingKnob::TopK, profile: "voxtral".into() })
    )]
    #[case::voxtral_min_p(
        "voxtral",
        Some(Ok(AsrEvalBenchmark { parameter_min_p: Some(MinP::try_new(0.05)?), ..entry("voxtral")? })),
        Err(AsrDecodingError::KnobOffProfile { knob: AsrDecodingKnob::MinP, profile: "voxtral".into() })
    )]
    #[case::voxtral_repeat_penalty(
        "voxtral",
        Some(Ok(AsrEvalBenchmark { parameter_repeat_penalty: Some(RepeatPenalty::try_new(1.1)?), ..entry("voxtral")? })),
        Err(AsrDecodingError::KnobOffProfile { knob: AsrDecodingKnob::RepeatPenalty, profile: "voxtral".into() })
    )]
    #[case::voxtral_top_p(
        "voxtral",
        Some(Ok(AsrEvalBenchmark { parameter_top_p: Some(TopP::try_new(0.95)?), ..entry("voxtral")? })),
        Ok(())
    )]
    #[case::gemma_recipe(
        "gemma4",
        Some(Ok(AsrEvalBenchmark {
            parameter_temperature: Some(SamplingTemperature::try_new(1.0)?),
            parameter_top_p: Some(TopP::try_new(0.95)?),
            parameter_top_k: Some(TopK::try_new(64)?),
            parameter_seed: Some(SamplingSeed::try_new(0)?),
            ..entry("gemma4")?
        })),
        Ok(())
    )]
    #[case::an_unknown_profile_loads("phi4", None, Ok(()))]
    fn a_profile_refuses_what_its_card_cannot_run(
        #[case] profile: &str,
        #[case] benchmark: Option<anyhow::Result<AsrEvalBenchmark>>,
        #[case] expected: Result<(), AsrDecodingError>,
    ) -> anyhow::Result<()> {
        let benchmark = match benchmark {
            Some(benchmark) => benchmark?,
            None => entry(profile)?,
        };
        assert_eq!(benchmark.check_decoding(), expected);
        Ok(())
    }

    /// Every error reads as one sentence: no run of spaces from a broken
    /// string continuation.
    #[test]
    fn decoding_errors_read_cleanly() {
        let errors = [
            AsrDecodingError::SingleBeam,
            AsrDecodingError::BeamPenaltyWithoutBeams {
                knob: AsrDecodingKnob::BeamRepetitionPenalty,
            },
            AsrDecodingError::SamplingWithBeams {
                knob: AsrDecodingKnob::Seed,
            },
            AsrDecodingError::BeamsOffProfile {
                profile: "gemma4".into(),
            },
            AsrDecodingError::KnobOffProfile {
                knob: AsrDecodingKnob::TopK,
                profile: "voxtral".into(),
            },
            AsrDecodingError::UnknownProfile {
                profile: "phi4".into(),
                known: "gemma4, voxtral".into(),
            },
        ];
        for error in errors {
            let text = error.to_string();
            assert!(!text.contains("  "), "{text:?}");
            assert!(!text.contains('\n'), "{text:?}");
        }
        assert_eq!(
            AsrDecodingError::SamplingWithBeams {
                knob: AsrDecodingKnob::Seed
            }
            .to_string(),
            "parameter_seed beside parameter_num_beams: a beam search samples nothing; drop it \
             (the beams' own penalty is parameter_beam_repetition_penalty)"
        );
    }

    fn sampling(
        temperature: f64,
        top_p: f64,
        top_k: i32,
        repeat_penalty: f64,
        repeat_window: AsrRepeatWindow,
        sampler_chain: AsrSamplerChain,
    ) -> anyhow::Result<AsrDecoding> {
        Ok(AsrDecoding::Sampling(AsrSampling {
            temperature: SamplingTemperature::try_new(temperature)?,
            top_p: TopP::try_new(top_p)?,
            top_k: TopK::try_new(top_k)?,
            min_p: MinP::try_new(0.0)?,
            repeat_penalty: RepeatPenalty::try_new(repeat_penalty)?,
            repeat_window,
            seed: None,
            sampler_chain,
        }))
    }

    /// An unpinned knob runs at the profile card's value, else off: what each
    /// profile's entry runs when it pins nothing. Gemma 4's card samples;
    /// the others transcribe greedily.
    #[rstest::rstest]
    #[case::gemma4(
        "gemma4",
        sampling(
            1.0,
            0.95,
            64,
            1.0,
            AsrRepeatWindow::RuntimeDefault,
            AsrSamplerChain::RuntimeDefault
        )
    )]
    #[case::qwen25_omni(
        "qwen25_omni",
        sampling(
            0.0,
            1.0,
            1,
            1.0,
            AsrRepeatWindow::RuntimeDefault,
            AsrSamplerChain::RuntimeDefault
        )
    )]
    #[case::voxtral(
        "voxtral",
        sampling(
            0.0,
            1.0,
            0,
            1.0,
            AsrRepeatWindow::RuntimeDefault,
            AsrSamplerChain::TemperatureThenTopP
        )
    )]
    #[case::minicpm_o(
        "minicpm_o",
        sampling(
            0.0,
            1.0,
            1,
            1.02,
            AsrRepeatWindow::WholeSequence,
            AsrSamplerChain::RuntimeDefault
        )
    )]
    #[case::lfm25_audio(
        "lfm25_audio",
        sampling(
            0.0,
            1.0,
            1,
            1.0,
            AsrRepeatWindow::RuntimeDefault,
            AsrSamplerChain::RuntimeDefault
        )
    )]
    #[case::lfm3(
        "lfm3",
        sampling(
            0.0,
            1.0,
            1,
            1.0,
            AsrRepeatWindow::RuntimeDefault,
            AsrSamplerChain::RuntimeDefault
        )
    )]
    fn an_unpinned_knob_runs_at_its_cards_value(
        #[case] profile: &str,
        #[case] expected: anyhow::Result<AsrDecoding>,
    ) -> anyhow::Result<()> {
        assert_eq!(entry(profile)?.effective_decoding()?, expected?);
        Ok(())
    }

    /// A card greedy through `top_k 1` samples when a temperature above `0` is
    /// pinned: its unpinned top-k is then off. A pinned top-k still wins, and
    /// a pinned `0.0` stays greedy.
    #[rstest::rstest]
    #[case::sampling_qwen("qwen25_omni", 0.7, None, 0)]
    #[case::sampling_minicpm("minicpm_o", 0.7, None, 0)]
    #[case::sampling_lfm25("lfm25_audio", 0.7, None, 0)]
    #[case::sampling_lfm3("lfm3", 0.7, None, 0)]
    #[case::pinned_top_k("qwen25_omni", 0.7, Some(5), 5)]
    #[case::pinned_greedy("qwen25_omni", 0.0, None, 1)]
    #[case::gemma_keeps_its_cut("gemma4", 0.0, None, 64)]
    fn a_pinned_temperature_samples_on_a_greedy_card(
        #[case] profile: &str,
        #[case] temperature: f64,
        #[case] top_k: Option<i32>,
        #[case] expected: i32,
    ) -> anyhow::Result<()> {
        let entry = AsrEvalBenchmark {
            parameter_temperature: Some(SamplingTemperature::try_new(temperature)?),
            parameter_top_k: top_k.map(TopK::try_new).transpose()?,
            ..entry(profile)?
        };
        let AsrDecoding::Sampling(sampling) = entry.effective_decoding()? else {
            anyhow::bail!("expected sampling");
        };
        assert_eq!(sampling.temperature.as_f64(), temperature);
        assert_eq!(sampling.top_k.into_inner(), expected);
        Ok(())
    }

    /// The temperature an entry runs at is its card's when it pins none, and
    /// greedy for a beam search and a profile without a card.
    #[rstest::rstest]
    #[case::gemma_card(entry("gemma4"), 1.0)]
    #[case::greedy_card(entry("voxtral"), 0.0)]
    #[case::pinned(Ok(AsrEvalBenchmark { parameter_temperature: Some(SamplingTemperature::greedy()), ..entry("gemma4")? }), 0.0)]
    #[case::beams(beams("minicpm_o", 3), 0.0)]
    #[case::unknown(entry("phi4"), 0.0)]
    fn the_sampling_temperature_is_the_cards(
        #[case] benchmark: anyhow::Result<AsrEvalBenchmark>,
        #[case] expected: f64,
    ) -> anyhow::Result<()> {
        assert_eq!(benchmark?.sampling_temperature().as_f64(), expected);
        Ok(())
    }

    /// Every known profile has a card: the test above covers each.
    #[test]
    fn every_known_profile_resolves() -> anyhow::Result<()> {
        for profile in KnownAsrPromptProfile::ALL {
            entry(profile.as_ref())?.effective_decoding()?;
        }
        Ok(())
    }

    /// A pinned knob wins over the card's.
    #[test]
    fn a_pinned_knob_wins() -> anyhow::Result<()> {
        let pinned = AsrEvalBenchmark {
            parameter_temperature: Some(SamplingTemperature::try_new(0.7)?),
            parameter_top_k: Some(TopK::try_new(20)?),
            parameter_repeat_penalty: Some(RepeatPenalty::try_new(1.1)?),
            parameter_seed: Some(SamplingSeed::try_new(7)?),
            ..entry("minicpm_o")?
        };
        let AsrDecoding::Sampling(sampling) = pinned.effective_decoding()? else {
            anyhow::bail!("expected sampling");
        };
        assert_eq!(sampling.temperature.as_f64(), 0.7);
        assert_eq!(sampling.top_k.into_inner(), 20);
        assert_eq!(sampling.repeat_penalty.as_f64(), 1.1);
        assert_eq!(sampling.repeat_window, AsrRepeatWindow::WholeSequence);
        assert_eq!(sampling.seed, Some(SamplingSeed::try_new(7)?));
        Ok(())
    }

    /// A beam search's unpinned penalties are the card's (MiniCPM-o's
    /// repetition `1.2`, length `1.0`), at any beam count; a pinned one wins.
    #[rstest::rstest]
    #[case::the_card(3, None, None, 1.2, 1.0)]
    #[case::another_width(5, None, None, 1.2, 1.0)]
    #[case::pinned_repetition(3, Some(1.0), None, 1.0, 1.0)]
    #[case::pinned_length(3, None, Some(0.5), 1.2, 0.5)]
    fn a_beam_search_resolves_its_penalties(
        #[case] num_beams: i32,
        #[case] repetition: Option<f64>,
        #[case] length: Option<f64>,
        #[case] expected_repetition: f64,
        #[case] expected_length: f64,
    ) -> anyhow::Result<()> {
        let beam = AsrEvalBenchmark {
            parameter_beam_repetition_penalty: repetition
                .map(BeamRepetitionPenalty::try_new)
                .transpose()?,
            parameter_beam_length_penalty: length.map(BeamLengthPenalty::try_new).transpose()?,
            ..beams("minicpm_o", num_beams)?
        };
        assert_eq!(
            beam.effective_decoding()?,
            AsrDecoding::Beams(AsrBeamSearch {
                num_beams: NumBeams::try_new(num_beams)?,
                repetition_penalty: BeamRepetitionPenalty::try_new(expected_repetition)?,
                length_penalty: BeamLengthPenalty::try_new(expected_length)?,
            })
        );
        Ok(())
    }

    /// A profile without a card loads but resolves to nothing a client could run.
    #[test]
    fn an_unknown_profile_does_not_resolve() -> anyhow::Result<()> {
        let unknown = entry("phi4")?;
        unknown.check_decoding()?;
        assert!(matches!(
            unknown.effective_decoding(),
            Err(AsrDecodingError::UnknownProfile { profile, .. }) if profile == "phi4"
        ));
        Ok(())
    }

    /// A cell is within its card's declared languages when the recipe lists
    /// the task's language; outside when it lists others; unknown when the
    /// recipe lists none, or the task is one this build does not know.
    #[rstest::rstest]
    #[case::english_set("asr_esb_ami_cleaned", Some(&["en"][..]), Some(true))]
    #[case::fleurs_english("asr_fleurs_en_us", Some(&["en"][..]), Some(true))]
    #[case::fleurs_german("asr_fleurs_de_de", Some(&["en"][..]), Some(false))]
    #[case::fleurs_listed("asr_fleurs_pt_br", Some(&["en", "pt"][..]), Some(true))]
    #[case::none_listed("asr_fleurs_de_de", None, None)]
    #[case::unknown_task("asr_fleurs_ja_jp", Some(&["en"][..]), None)]
    fn a_cell_is_marked_against_its_cards_languages(
        #[case] task: &str,
        #[case] languages: Option<&[&str]>,
        #[case] within: Option<bool>,
    ) -> anyhow::Result<()> {
        let mut recipe = serde_json::json!({
            "family": "F", "models": [{"repo": "o/r"}], "sources": ["https://a"],
            "enable_thinking": false,
        });
        if let Some(languages) = languages {
            recipe["languages"] = serde_json::json!(languages);
        }
        let asr = AsrEvalBenchmark {
            parameter_asr_task: task.into(),
            recipe: Some(serde_json::from_value(recipe)?),
            ..entry("lfm25_audio")?
        };
        assert_eq!(asr.within_declared_languages(), within);
        let no_recipe = AsrEvalBenchmark {
            recipe: None,
            ..asr
        };
        assert_eq!(no_recipe.within_declared_languages(), None);
        Ok(())
    }

    /// Every profile runs every task, a FLEURS locale outside its card's
    /// languages included: LFM2.5-Audio's card lists English alone, and its
    /// entry on FLEURS, English or not, and on a locale this build does not
    /// know, loads and resolves as on an English set. Such a cell is marked
    /// (`within_declared_languages`), never refused.
    #[rstest::rstest]
    #[case::fleurs_german("lfm25_audio", "asr_fleurs_de_de")]
    #[case::fleurs_english("lfm25_audio", "asr_fleurs_en_us")]
    #[case::fleurs_unknown_locale("lfm25_audio", "asr_fleurs_ja_jp")]
    #[case::english_set("lfm25_audio", "asr_esb_librispeech_test_clean")]
    #[case::minicpm_o_on_fleurs("minicpm_o", "asr_fleurs_pt_br")]
    #[case::qwen_on_fleurs("qwen25_omni", "asr_fleurs_nl_nl")]
    fn every_profile_runs_every_fleurs_locale(
        #[case] profile: &str,
        #[case] task: &str,
    ) -> anyhow::Result<()> {
        let asr = AsrEvalBenchmark {
            parameter_asr_task: task.into(),
            ..entry(profile)?
        };
        asr.check_decoding()?;
        asr.effective_decoding()?;
        Ok(())
    }

    /// vLLM runs the four families it implements, each without beams; llama.cpp
    /// runs every entry `check_decoding` admits. MiniCPM-o's greedy branch, its
    /// repetition penalty over the whole sequence, runs on both.
    #[rstest::rstest]
    #[case::gemma(entry("gemma4"), true)]
    #[case::qwen(entry("qwen25_omni"), true)]
    #[case::voxtral(entry("voxtral"), true)]
    #[case::minicpm_greedy(entry("minicpm_o"), true)]
    #[case::minicpm_beams(beams("minicpm_o", 3), false)]
    #[case::lfm25(entry("lfm25_audio"), false)]
    #[case::lfm3(entry("lfm3"), false)]
    #[case::unknown(entry("a_profile_from_a_newer_build"), false)]
    fn an_entry_runs_on_vllm_only_where_vllm_can_run_it(
        #[case] asr: anyhow::Result<AsrEvalBenchmark>,
        #[case] on_vllm: bool,
    ) -> anyhow::Result<()> {
        let asr = asr?;
        assert_eq!(
            asr.check_runtime(crate::RuntimeType::UvVllm).is_ok(),
            on_vllm,
            "{:?}",
            asr.check_runtime(crate::RuntimeType::UvVllm)
        );
        asr.check_runtime(crate::RuntimeType::LlamacppCliStockTools)?;
        Ok(())
    }
}
