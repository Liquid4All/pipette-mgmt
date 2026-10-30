//! `EvalId`: the eval a benchmark names, loose on the wire and strict inside,
//! since the server may list evals this client doesn't know yet.

use serde::{Deserialize, Serialize};
use strum::{AsRefStr, Display, EnumString, VariantArray};

/// The evals this client knows how to run end to end: the set `pipette-scores`'
/// `POST /score` can score, spelled exactly as it spells them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Display, AsRefStr, EnumString, VariantArray)]
#[strum(serialize_all = "snake_case")]
pub enum KnownEvalId {
    Ifbench,
    Ifstruct,
    GpqaDiamond,
    // snake_case would give `math500`; the scorer spells `math_500`.
    #[strum(serialize = "math_500")]
    Math500,
}

impl KnownEvalId {
    /// All variants, in declaration order.
    pub const ALL: &'static [Self] = <Self as VariantArray>::VARIANTS;

    /// Resolve a loose `parameter_eval_id` to a known eval.
    pub fn from_eval_id(eval_id: &str) -> Option<Self> {
        eval_id.parse().ok()
    }

    /// Client-side sampling temperature for this eval's `/completion` requests.
    /// Pass@k evals sample at `0.6` so their repeated draws differ. The match is
    /// exhaustive so a new eval forces a temperature decision.
    pub fn sampling_temperature(self) -> f64 {
        match self {
            Self::Ifbench | Self::Ifstruct | Self::GpqaDiamond | Self::Math500 => 0.6,
        }
    }
}

/// An eval identifier in a strict [`super::BenchmarkDefinition::Eval`]. Parsing
/// never fails or rewrites an id: an unknown one is kept verbatim.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "String", into = "String")]
pub enum EvalId {
    Known(KnownEvalId),
    Unknown(String),
}

impl EvalId {
    /// The wire spelling.
    pub fn as_str(&self) -> &str {
        match self {
            Self::Known(known) => known.as_ref(),
            Self::Unknown(raw) => raw,
        }
    }

    /// As [`KnownEvalId::sampling_temperature`]; an unknown eval is greedy.
    pub fn sampling_temperature(&self) -> f64 {
        match self {
            Self::Known(known) => known.sampling_temperature(),
            Self::Unknown(_) => 0.0,
        }
    }
}

impl From<String> for EvalId {
    fn from(raw: String) -> Self {
        KnownEvalId::from_eval_id(&raw).map_or(Self::Unknown(raw), Self::Known)
    }
}

impl From<&str> for EvalId {
    fn from(raw: &str) -> Self {
        KnownEvalId::from_eval_id(raw).map_or_else(|| Self::Unknown(raw.to_string()), Self::Known)
    }
}

impl From<EvalId> for String {
    fn from(id: EvalId) -> Self {
        match id {
            EvalId::Known(known) => known.as_ref().to_string(),
            EvalId::Unknown(raw) => raw,
        }
    }
}

impl std::fmt::Display for EvalId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;
    #[test]
    fn known_eval_id_round_trips_every_variant() {
        KnownEvalId::ALL.iter().for_each(|&eval| {
            assert_eq!(KnownEvalId::from_eval_id(eval.as_ref()), Some(eval));
        });
    }

    #[test]
    fn eval_id_parses_known_and_preserves_unknown() {
        assert_eq!(EvalId::from("ifbench"), EvalId::Known(KnownEvalId::Ifbench));
        assert_eq!(
            EvalId::from("math_500"),
            EvalId::Known(KnownEvalId::Math500)
        );
        assert_eq!(
            EvalId::from("some_future_eval"),
            EvalId::Unknown("some_future_eval".to_string())
        );
    }

    #[rstest]
    #[case::known(EvalId::Known(KnownEvalId::Math500), "math_500")]
    #[case::unknown(EvalId::Unknown("some_future_eval".to_string()), "some_future_eval")]
    fn eval_id_round_trips_through_string_losslessly(
        #[case] id: EvalId,
        #[case] wire: &str,
    ) -> anyhow::Result<()> {
        assert_eq!(id.as_str(), wire);
        assert_eq!(id.to_string(), wire);
        assert_eq!(serde_json::to_value(&id)?, wire);
        assert_eq!(EvalId::from(wire), id);
        assert_eq!(EvalId::from(wire.to_owned()), id);
        assert_eq!(serde_json::from_value::<EvalId>(wire.into())?, id);
        Ok(())
    }

    #[rstest]
    #[case("ifbench", 0.6)]
    #[case("ifstruct", 0.6)]
    #[case("gpqa_diamond", 0.6)]
    #[case("math_500", 0.6)]
    #[case("mmlu_pro", 0.0)]
    #[case("totally_unknown_eval", 0.0)]
    fn sampling_temperature_by_eval_id(#[case] eval_id: &str, #[case] expected: f64) {
        assert_eq!(
            EvalId::from(eval_id).sampling_temperature(),
            expected,
            "eval_id={eval_id}"
        );
    }

    /// `BenchmarkParams` takes the rule's temperature without a fallible path.
    #[test]
    fn every_eval_id_rule_is_a_valid_temperature() {
        KnownEvalId::ALL.iter().for_each(|&eval| {
            let t = eval.sampling_temperature();
            assert!(
                crate::SamplingTemperature::try_new(t).is_ok(),
                "{eval}: {t}"
            );
        });
    }

    #[test]
    fn known_eval_id_spelling_matches_scoring_backend() {
        // Must match pipette-scores' `EvalId` exactly, or `POST /score` 404s.
        assert_eq!(KnownEvalId::Ifbench.to_string(), "ifbench");
        assert_eq!(KnownEvalId::Ifstruct.to_string(), "ifstruct");
        assert_eq!(KnownEvalId::GpqaDiamond.to_string(), "gpqa_diamond");
        assert_eq!(KnownEvalId::Math500.to_string(), "math_500");
    }

    #[test]
    fn known_eval_id_unrecognized_is_none() {
        assert_eq!(KnownEvalId::from_eval_id("mmlu_pro"), None);
        assert_eq!(KnownEvalId::from_eval_id("totally_unknown_eval"), None);
    }
}
