//! Counts and durations, each refusing the values its fields cannot hold.

use nutype::nutype;
use thiserror::Error;

/// A count that is never zero: tokens a prompt held or a run generates, turns a
/// traversal covered, parameters a model has, GPUs or slots a runtime is given.
#[nutype(
    validate(greater = 0),
    derive(
        Debug,
        Clone,
        Copy,
        PartialEq,
        Eq,
        PartialOrd,
        Ord,
        Hash,
        Display,
        Into,
        Serialize,
        Deserialize
    )
)]
pub struct PositiveCount(i32);

/// A `u32` no [`PositiveCount`] holds: zero, or past `i32::MAX`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
#[error("count must be between 1 and {max}, got {0}", max = i32::MAX)]
pub struct PositiveCountRangeError(pub u32);

impl TryFrom<u32> for PositiveCount {
    type Error = PositiveCountRangeError;

    fn try_from(count: u32) -> Result<Self, Self::Error> {
        i32::try_from(count)
            .ok()
            .and_then(|count| Self::try_new(count).ok())
            .ok_or(PositiveCountRangeError(count))
    }
}

/// A count that may be zero, such as the tokens of an empty prompt; capped at
/// `i32::MAX` like [`PositiveCount`].
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
        Ord,
        Hash,
        Default,
        Display,
        Into,
        Serialize,
        Deserialize
    )
)]
pub struct NonNegativeCount(i32);

/// A deadline in whole seconds; zero would kill the run before it does any work.
#[nutype(
    validate(greater = 0),
    derive(
        Debug,
        Clone,
        Copy,
        PartialEq,
        Eq,
        PartialOrd,
        Ord,
        Hash,
        Display,
        Into,
        Serialize,
        Deserialize
    )
)]
pub struct PositiveSeconds(u64);

/// A whole-degree Android skin-temperature threshold, from 1 to 110 °C.
#[nutype(
    validate(greater_or_equal = 1, less_or_equal = 110),
    derive(
        Debug,
        Clone,
        Copy,
        PartialEq,
        Eq,
        PartialOrd,
        Ord,
        Hash,
        Display,
        Into,
        Serialize,
        Deserialize
    )
)]
pub struct SkinCelsius(u32);

/// Whole milliseconds of wall time that clients counted on a monotonic clock,
/// or a sum of them. Zero is a real reading.
#[nutype(
    validate(greater_or_equal = 0),
    derive(
        Debug,
        Clone,
        Copy,
        PartialEq,
        Eq,
        PartialOrd,
        Ord,
        Hash,
        Display,
        Into,
        Serialize,
        Deserialize
    )
)]
pub struct ElapsedMillis(i64);

#[cfg(test)]
mod tests {

    use super::*;

    fn parses<T: serde::de::DeserializeOwned>(json: &str) -> bool {
        serde_json::from_str::<T>(json).is_ok()
    }

    /// Past the max a value is refused rather than wrapped: a count lands in an
    /// `i32` warehouse column.
    #[rstest::rstest]
    #[case::count_one(parses::<PositiveCount>, "1", true)]
    #[case::count_max(parses::<PositiveCount>, "2147483647", true)]
    #[case::count_zero(parses::<PositiveCount>, "0", false)]
    #[case::count_negative(parses::<PositiveCount>, "-1", false)]
    #[case::count_past_max(parses::<PositiveCount>, "2147483648", false)]
    #[case::non_negative_zero(parses::<NonNegativeCount>, "0", true)]
    #[case::non_negative_max(parses::<NonNegativeCount>, "2147483647", true)]
    #[case::non_negative_negative(parses::<NonNegativeCount>, "-1", false)]
    #[case::non_negative_past_max(parses::<NonNegativeCount>, "2147483648", false)]
    #[case::seconds_one(parses::<PositiveSeconds>, "1", true)]
    #[case::seconds_max(parses::<PositiveSeconds>, "18446744073709551615", true)]
    #[case::seconds_zero(parses::<PositiveSeconds>, "0", false)]
    #[case::seconds_negative(parses::<PositiveSeconds>, "-1", false)]
    #[case::seconds_past_max(parses::<PositiveSeconds>, "18446744073709551616", false)]
    #[case::elapsed_zero(parses::<ElapsedMillis>, "0", true)]
    #[case::elapsed_max(parses::<ElapsedMillis>, "9223372036854775807", true)]
    #[case::elapsed_negative(parses::<ElapsedMillis>, "-1", false)]
    #[case::elapsed_past_max(parses::<ElapsedMillis>, "9223372036854775808", false)]
    fn positive_bounds(#[case] parses: fn(&str) -> bool, #[case] json: &str, #[case] ok: bool) {
        assert_eq!(parses(json), ok, "{json}");
    }

    #[rstest::rstest]
    #[case::one(1, Ok(1))]
    #[case::max(2_147_483_647, Ok(2_147_483_647))]
    #[case::zero(0, Err("count must be between 1 and 2147483647, got 0"))]
    #[case::past_max(
        2_147_483_648,
        Err("count must be between 1 and 2147483647, got 2147483648")
    )]
    fn positive_count_from_a_u32(#[case] count: u32, #[case] expected: Result<i32, &str>) {
        let converted = PositiveCount::try_from(count)
            .map(PositiveCount::into_inner)
            .map_err(|e| e.to_string());
        assert_eq!(converted, expected.map_err(str::to_owned));
    }

    #[test]
    fn positive_newtypes_render_as_the_bare_number() -> anyhow::Result<()> {
        let count = PositiveCount::try_new(128)?;
        assert_eq!(count.to_string(), "128");
        assert_eq!(serde_json::to_string(&count)?, "128");
        let seconds = PositiveSeconds::try_new(600)?;
        assert_eq!(seconds.to_string(), "600");
        assert_eq!(serde_json::to_string(&seconds)?, "600");
        Ok(())
    }
}
