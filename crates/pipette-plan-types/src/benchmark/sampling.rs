//! The sampling an entry may pin. `pipette-core` validates a catalog entry
//! through these same types, so a bound is declared once for both sides.

use nutype::nutype;

/// `-0.0` passes a `>= 0.0` bound but prints, and so hashes, apart from `0.0`.
fn positive_zero(value: f64) -> f64 {
    if value == 0.0 { 0.0 } else { value }
}

/// Sampling temperature for a `/completion` request, bounded to `[0.0, 2.0]`:
/// `0.0` is greedy, `2.0` the most the OpenAI-compatible servers accept. A
/// value out of range or non-finite is an error, not clamped. `Default` is
/// greedy.
#[nutype(
    sanitize(with = positive_zero),
    validate(finite, greater_or_equal = 0.0, less_or_equal = 2.0),
    default = 0.0,
    derive(
        Debug,
        Clone,
        Copy,
        PartialEq,
        Eq,
        PartialOrd,
        Default,
        Display,
        Into,
        Serialize,
        Deserialize
    )
)]
pub struct SamplingTemperature(f64);

impl SamplingTemperature {
    /// Greedy decoding, temperature `0.0`.
    pub fn greedy() -> Self {
        Self::default()
    }

    /// The bare `f64`, for inclusion in `/completion` request bodies.
    pub fn as_f64(self) -> f64 {
        self.into_inner()
    }
}

/// Nucleus-sampling mass for eval `/completion` requests, bounded to the
/// `[0.0, 1.0]` every runtime here accepts. `Default` is `1.0`, off.
#[nutype(
    sanitize(with = positive_zero),
    validate(finite, greater_or_equal = 0.0, less_or_equal = 1.0),
    default = 1.0,
    derive(
        Debug,
        Clone,
        Copy,
        PartialEq,
        Eq,
        PartialOrd,
        Default,
        Display,
        Into,
        Serialize,
        Deserialize
    )
)]
pub struct TopP(f64);

impl TopP {
    /// The bare `f64`, for inclusion in `/completion` request bodies.
    pub fn as_f64(self) -> f64 {
        self.into_inner()
    }
}

/// Minimum-probability cutoff for eval `/completion` requests, relative to the
/// most likely token; `0.0` disables it, and is `Default`.
#[nutype(
    sanitize(with = positive_zero),
    validate(finite, greater_or_equal = 0.0, less_or_equal = 1.0),
    default = 0.0,
    derive(
        Debug,
        Clone,
        Copy,
        PartialEq,
        Eq,
        PartialOrd,
        Default,
        Display,
        Into,
        Serialize,
        Deserialize
    )
)]
pub struct MinP(f64);

impl MinP {
    /// The bare `f64`, for inclusion in `/completion` request bodies.
    pub fn as_f64(self) -> f64 {
        self.into_inner()
    }
}

/// Presence penalty, bounded to the `[-2.0, 2.0]` the OpenAI-compatible servers
/// accept; `0.0` disables it.
#[nutype(
    sanitize(with = positive_zero),
    validate(finite, greater_or_equal = -2.0, less_or_equal = 2.0),
    derive(
        Debug,
        Clone,
        Copy,
        PartialEq,
        Eq,
        PartialOrd,
        Display,
        Into,
        Serialize,
        Deserialize
    )
)]
pub struct PresencePenalty(f64);

impl PresencePenalty {
    /// The bare `f64`, for inclusion in request bodies.
    pub fn as_f64(self) -> f64 {
        self.into_inner()
    }
}

/// llama.cpp's repeat penalty, which divides a repeated token's logit, so `0.0`
/// is refused; `1.0` disables it, and is `Default`.
#[nutype(
    sanitize(with = positive_zero),
    validate(finite, greater = 0.0, less_or_equal = 2.0),
    default = 1.0,
    derive(
        Debug,
        Clone,
        Copy,
        PartialEq,
        Eq,
        PartialOrd,
        Default,
        Display,
        Into,
        Serialize,
        Deserialize
    )
)]
pub struct RepeatPenalty(f64);

impl RepeatPenalty {
    /// The bare `f64`, for inclusion in request bodies.
    pub fn as_f64(self) -> f64 {
        self.into_inner()
    }
}

/// Tokens the top-k cut keeps; `0` disables it, which is how an entry turns off
/// a runtime's own default (llama.cpp ships `40`), and is `Default`. Signed to
/// match the wire and the warehouse column.
#[nutype(
    validate(greater_or_equal = 0),
    default = 0,
    derive(
        Debug,
        Clone,
        Copy,
        PartialEq,
        Eq,
        PartialOrd,
        Default,
        Hash,
        Display,
        Into,
        Serialize,
        Deserialize
    )
)]
pub struct TopK(i32);

/// Base RNG seed an entry pins, from which the client derives a per-attempt
/// seed (see [`EvalBenchmark::sample_seed`](crate::benchmark::EvalBenchmark::sample_seed)).
/// Capped at `u32::MAX`, llama.cpp's seed width: no device could honor a wider
/// one.
#[nutype(
    validate(greater_or_equal = 0, less_or_equal = 4_294_967_295),
    derive(
        Debug,
        Clone,
        Copy,
        PartialEq,
        Eq,
        PartialOrd,
        Hash,
        Display,
        Into,
        Serialize,
        Deserialize
    )
)]
pub struct SamplingSeed(i64);

/// How a vlm_eval or asr_eval entry that pins `parameter_seed` derives each
/// sample's seed from it. Absent, the base seed offset by the repeat index, the
/// rule every eval began with; an entry opts in to [`SeedRule::PerSampleSha256`].
///
/// Part of the entry's params, so a result drawn under one rule never shares
/// an identity with one drawn under the other, and a client that predates the
/// field has its result refused rather than recorded as drawn under it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SeedRule {
    /// Each sample's seed is the first four bytes, read big-endian, of the
    /// SHA-256 of `<seed>|<sample_id>` in UTF-8: the base seed in decimal, a
    /// `|`, then the sample id as the evals server serves it (`<id>#k` for a
    /// repeat). Every sample, and every repeat, draws on its own.
    PerSampleSha256,
}

/// Beams a beam search keeps (`n_beams` in pipette's ASR build of llama.cpp);
/// `1` is greedy. Capped where a single slot's KV cache stays affordable.
/// Signed to match the warehouse column.
#[nutype(
    validate(greater_or_equal = 1, less_or_equal = 8),
    derive(
        Debug,
        Clone,
        Copy,
        PartialEq,
        Eq,
        PartialOrd,
        Hash,
        Display,
        Into,
        Serialize,
        Deserialize
    )
)]
pub struct NumBeams(i32);

/// A beam search's repetition penalty as Transformers applies it after
/// `log_softmax` (a generated token's log-probability is multiplied by it when
/// negative, divided when positive); `1.0` disables it, and is `Default`; `0.0`
/// is refused.
#[nutype(
    sanitize(with = positive_zero),
    validate(finite, greater = 0.0, less_or_equal = 2.0),
    default = 1.0,
    derive(
        Debug,
        Clone,
        Copy,
        PartialEq,
        Eq,
        PartialOrd,
        Default,
        Display,
        Into,
        Serialize,
        Deserialize
    )
)]
pub struct BeamRepetitionPenalty(f64);

impl BeamRepetitionPenalty {
    /// The bare `f64`, for inclusion in request bodies.
    pub fn as_f64(self) -> f64 {
        self.into_inner()
    }
}

/// A beam search's length penalty: a finished beam scores its summed
/// log-probability over its length to this power; `1.0` is the mean, and is
/// `Default`.
#[nutype(
    sanitize(with = positive_zero),
    validate(finite, greater_or_equal = -2.0, less_or_equal = 2.0),
    default = 1.0,
    derive(
        Debug,
        Clone,
        Copy,
        PartialEq,
        Eq,
        PartialOrd,
        Default,
        Display,
        Into,
        Serialize,
        Deserialize
    )
)]
pub struct BeamLengthPenalty(f64);

impl BeamLengthPenalty {
    /// The bare `f64`, for inclusion in request bodies.
    pub fn as_f64(self) -> f64 {
        self.into_inner()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[rstest::rstest]
    #[case(0.0)]
    #[case(0.6)]
    #[case(2.0)]
    fn temperature_accepts_in_range(#[case] value: f64) -> Result<(), Box<dyn std::error::Error>> {
        assert_eq!(SamplingTemperature::try_new(value)?.as_f64(), value);
        Ok(())
    }

    #[rstest::rstest]
    #[case(0.0)]
    #[case(0.05)]
    #[case(0.95)]
    #[case(1.0)]
    fn sampling_cutoffs_read_back_what_was_set(
        #[case] value: f64,
    ) -> Result<(), Box<dyn std::error::Error>> {
        assert_eq!(TopP::try_new(value)?.as_f64(), value);
        assert_eq!(MinP::try_new(value)?.as_f64(), value);
        Ok(())
    }

    /// Parsed from the wire too, so a catalog's `-0.0` serializes as `0.0`.
    #[test]
    fn negative_zero_reads_as_zero() -> Result<(), Box<dyn std::error::Error>> {
        let wire = serde_json::json!(-0.0);
        let t: SamplingTemperature = serde_json::from_value(wire.clone())?;
        let p: TopP = serde_json::from_value(wire.clone())?;
        let m: MinP = serde_json::from_value(wire.clone())?;
        let pp: PresencePenalty = serde_json::from_value(wire)?;
        for v in [t.as_f64(), p.as_f64(), m.as_f64(), pp.as_f64()] {
            assert!(v == 0.0 && v.is_sign_positive(), "{v}");
        }
        assert_eq!(serde_json::to_string(&t)?, "0.0");
        Ok(())
    }

    #[rstest::rstest]
    #[case(-0.1)]
    #[case(1.1)]
    #[case(f64::NAN)]
    #[case(f64::INFINITY)]
    fn sampling_cutoffs_reject_outside_the_unit_interval(#[case] value: f64) {
        assert!(TopP::try_new(value).is_err(), "top_p {value}");
        assert!(MinP::try_new(value).is_err(), "min_p {value}");
    }

    #[rstest::rstest]
    #[case::most_negative(-2.0, true)]
    #[case::disabled(0.0, true)]
    #[case::qwen_card(1.5, true)]
    #[case::most_positive(2.0, true)]
    #[case::below(-2.1, false)]
    #[case::above(2.1, false)]
    #[case::nan(f64::NAN, false)]
    #[case::infinite(f64::INFINITY, false)]
    fn presence_penalty_is_the_openai_range(#[case] value: f64, #[case] accepted: bool) {
        assert_eq!(
            PresencePenalty::try_new(value).map(f64::from).ok(),
            accepted.then_some(value)
        );
    }

    /// A zero penalty would divide by zero, so the open end is refused.
    #[rstest::rstest]
    #[case::smallest(0.05, true)]
    #[case::disabled(1.0, true)]
    #[case::mild(1.05, true)]
    #[case::largest(2.0, true)]
    #[case::zero(0.0, false)]
    #[case::negative(-0.1, false)]
    #[case::above(2.1, false)]
    #[case::nan(f64::NAN, false)]
    fn repeat_penalty_is_positive_up_to_two(#[case] value: f64, #[case] accepted: bool) {
        assert_eq!(
            RepeatPenalty::try_new(value).map(f64::from).ok(),
            accepted.then_some(value)
        );
    }

    #[rstest::rstest]
    #[case::disabled(0, true)]
    #[case::llamacpp_default(40, true)]
    #[case::widest(i32::MAX, true)]
    #[case::negative(-1, false)]
    fn top_k_accepts_zero_and_up(#[case] value: i32, #[case] accepted: bool) {
        assert_eq!(
            TopK::try_new(value).map(i32::from).ok(),
            accepted.then_some(value)
        );
    }

    #[rstest::rstest]
    #[case::zero(0, true)]
    #[case::u32_max(4_294_967_295, true)]
    #[case::negative(-1, false)]
    #[case::past_u32(4_294_967_296, false)]
    fn seed_fits_a_u32(#[case] value: i64, #[case] accepted: bool) {
        assert_eq!(
            SamplingSeed::try_new(value).map(i64::from).ok(),
            accepted.then_some(value)
        );
    }

    // Out-of-range and non-finite values are rejected, not clamped.
    #[rstest::rstest]
    #[case(-0.1)]
    #[case(2.1)]
    #[case(f64::NAN)]
    #[case(f64::INFINITY)]
    #[case(f64::NEG_INFINITY)]
    fn temperature_rejects_out_of_range(#[case] value: f64) {
        assert!(
            SamplingTemperature::try_new(value).is_err(),
            "temperature {value} must be rejected"
        );
    }

    #[test]
    fn temperature_greedy_is_zero() -> anyhow::Result<()> {
        assert_eq!(SamplingTemperature::greedy().as_f64(), 0.0);
        Ok(())
    }
}
