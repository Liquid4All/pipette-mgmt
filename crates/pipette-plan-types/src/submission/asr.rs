//! What an ASR row records about how it was produced. The client writes these
//! into `client_extras` (the keys below), and mgmt types them at intake: whether
//! a peak or a prefill is the clip's own depends on them, and whether the clip
//! is one the model is documented to take.

use serde::{Deserialize, Serialize};

/// How an ASR cell's context was sized. Only an `exact` peak is the clip's own:
/// an upper-bound context holds KV for audio the clip does not have, and a
/// pinned one is the plan's choice.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, strum::Display)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum AsrContextSizing {
    /// From the audio tokens the runtime counted for this clip.
    Exact,
    /// From the upper bound, when the runtime could not count them.
    UpperBound,
    /// The cell set `ctx_size`.
    Pinned,
}

impl AsrContextSizing {
    /// The key in `client_extras`.
    pub const KEY: &'static str = "asr_context_sizing";
}

/// Where an `asr_throughput` row's `asr_audio_tokens` came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, strum::Display)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum AsrAudioTokensSource {
    /// The runtime's own count, marker-free.
    Calibrated,
    /// The prompt with the clip less the same prompt without it, markers included.
    Measured,
}

impl AsrAudioTokensSource {
    /// The key in `client_extras`.
    pub const KEY: &'static str = "asr_audio_tokens_source";
}

/// The ASR keys of a `client_extras` object, typed. `None` where a key is
/// absent; an unknown value is an error naming the key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct AsrClientExtras {
    pub context_sizing: Option<AsrContextSizing>,
    pub audio_tokens_source: Option<AsrAudioTokensSource>,
    /// `true` when the clip is longer than the model's own documentation says
    /// it takes (Gemma 4's card: 30 s of audio): the speed and memory of such a
    /// row are a hypothetical workload, its transcription quality is not
    /// established there, and the reference may give no count past the limit
    /// (Gemma 4 E2B and E4B cut the clip at 30 s). `false` on a clip within
    /// it, or for a model that documents none. Intake requires it on every
    /// ASR success.
    pub beyond_model_audio_limit: Option<bool>,
}

/// A `client_extras` ASR key holding a value its enum does not know.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("client_extras.{key} is {value}, not one of {known}")]
pub struct AsrClientExtrasError {
    pub key: &'static str,
    pub value: String,
    pub known: &'static str,
}

impl AsrClientExtras {
    /// The key of [`Self::beyond_model_audio_limit`] in `client_extras`.
    pub const BEYOND_MODEL_AUDIO_LIMIT_KEY: &'static str = "asr_beyond_model_audio_limit";

    /// Reads the ASR keys of a `client_extras` JSON object (shape-checked
    /// elsewhere; anything but an object reads as having neither key).
    pub fn parse(client_extras: Option<&str>) -> Result<Self, AsrClientExtrasError> {
        fn typed<T: serde::de::DeserializeOwned>(
            map: &serde_json::Map<String, serde_json::Value>,
            key: &'static str,
            known: &'static str,
        ) -> Result<Option<T>, AsrClientExtrasError> {
            map.get(key)
                .map(|value| {
                    serde_json::from_value(value.clone()).map_err(|_| AsrClientExtrasError {
                        key,
                        value: value.to_string(),
                        known,
                    })
                })
                .transpose()
        }
        let Some(map) = client_extras.and_then(|s| {
            serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(s).ok()
        }) else {
            return Ok(Self::default());
        };
        Ok(Self {
            context_sizing: typed(&map, AsrContextSizing::KEY, "exact, upper_bound, pinned")?,
            audio_tokens_source: typed(&map, AsrAudioTokensSource::KEY, "calibrated, measured")?,
            beyond_model_audio_limit: typed(
                &map,
                Self::BEYOND_MODEL_AUDIO_LIMIT_KEY,
                "true, false",
            )?,
        })
    }
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    /// The client's spelling of each value (clients `asr::ContextSizing` /
    /// `AudioTokensSource`) reads as its variant; absent keys read as `None`.
    #[rstest]
    #[case::exact_calibrated(
        Some(r#"{"asr_context_sizing":"exact","asr_audio_tokens_source":"calibrated"}"#),
        Some(AsrContextSizing::Exact),
        Some(AsrAudioTokensSource::Calibrated)
    )]
    #[case::upper_bound_measured(
        Some(r#"{"asr_context_sizing":"upper_bound","asr_audio_tokens_source":"measured"}"#),
        Some(AsrContextSizing::UpperBound),
        Some(AsrAudioTokensSource::Measured)
    )]
    #[case::pinned_alone(
        Some(r#"{"asr_context_sizing":"pinned","build":"x"}"#),
        Some(AsrContextSizing::Pinned),
        None
    )]
    #[case::neither(Some(r#"{"build":"nightly"}"#), None, None)]
    #[case::no_extras(None, None, None)]
    fn the_client_values_read_as_their_variants(
        #[case] extras: Option<&str>,
        #[case] sizing: Option<AsrContextSizing>,
        #[case] source: Option<AsrAudioTokensSource>,
    ) -> anyhow::Result<()> {
        let got = AsrClientExtras::parse(extras)?;
        assert_eq!(
            (got.context_sizing, got.audio_tokens_source),
            (sizing, source)
        );
        Ok(())
    }

    /// The limit flag reads as the boolean the client writes; absent is `None`.
    #[rstest]
    #[case::beyond(
        Some(r#"{"asr_context_sizing":"exact","asr_beyond_model_audio_limit":true}"#),
        Some(true)
    )]
    #[case::within(Some(r#"{"asr_beyond_model_audio_limit":false}"#), Some(false))]
    #[case::absent(Some(r#"{"asr_context_sizing":"exact"}"#), None)]
    #[case::no_extras(None, None)]
    fn the_limit_flag_reads_as_a_boolean(
        #[case] extras: Option<&str>,
        #[case] beyond: Option<bool>,
    ) -> anyhow::Result<()> {
        assert_eq!(
            AsrClientExtras::parse(extras)?.beyond_model_audio_limit,
            beyond
        );
        Ok(())
    }

    /// An unknown value, or one of the wrong JSON type, is refused by key.
    #[rstest]
    #[case::unknown_sizing(r#"{"asr_context_sizing":"guessed"}"#, "asr_context_sizing")]
    #[case::number_sizing(r#"{"asr_context_sizing":3}"#, "asr_context_sizing")]
    #[case::unknown_source(
        r#"{"asr_context_sizing":"exact","asr_audio_tokens_source":"estimated"}"#,
        "asr_audio_tokens_source"
    )]
    #[case::string_limit(
        r#"{"asr_beyond_model_audio_limit":"true"}"#,
        "asr_beyond_model_audio_limit"
    )]
    #[case::number_limit(
        r#"{"asr_beyond_model_audio_limit":1}"#,
        "asr_beyond_model_audio_limit"
    )]
    fn an_unknown_value_is_refused_by_key(#[case] extras: &str, #[case] key: &str) {
        let got = AsrClientExtras::parse(Some(extras));
        assert!(matches!(&got, Err(e) if e.key == key), "{got:?}");
    }

    /// The display form is the wire form, which the warehouse column stores.
    #[test]
    fn display_is_the_wire_spelling() -> anyhow::Result<()> {
        [
            AsrContextSizing::Exact,
            AsrContextSizing::UpperBound,
            AsrContextSizing::Pinned,
        ]
        .into_iter()
        .try_for_each(|sizing| {
            assert_eq!(
                serde_json::to_value(sizing)?,
                serde_json::Value::from(sizing.to_string())
            );
            anyhow::Ok(())
        })?;
        [
            AsrAudioTokensSource::Calibrated,
            AsrAudioTokensSource::Measured,
        ]
        .into_iter()
        .try_for_each(|source| {
            assert_eq!(
                serde_json::to_value(source)?,
                serde_json::Value::from(source.to_string())
            );
            anyhow::Ok(())
        })
    }
}
