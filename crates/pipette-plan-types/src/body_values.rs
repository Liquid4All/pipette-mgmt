//! Values every operator body is written in: the fleet tier, list order, the
//! query readers and the refusal body.

use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize};

/// Whether a body carries the fleet's detail: client ids, clause sets and
/// client counts are withheld from a reader who may not read the fleet.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, strum::EnumIter, strum::IntoStaticStr,
)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum FleetDetail {
    /// Client counts, clause sets and lease holders are disclosed.
    Full,
    /// Only the caller's job counts and ids remain, with fixed reason strings.
    Withheld,
}

impl FleetDetail {
    /// The serde spelling.
    pub fn label(self) -> &'static str {
        self.into()
    }
}

/// The order a listing's rows come in: ascending key order unless the caller
/// asks `newest`. A cursor holds only under the order that issued it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum ListOrder {
    #[default]
    Oldest,
    Newest,
}

impl FromStr for ListOrder {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "oldest" => Ok(Self::Oldest),
            "newest" => Ok(Self::Newest),
            other => Err(format!(
                "unknown order `{other}`, expected `oldest` or `newest`"
            )),
        }
    }
}

/// Read a query value, an empty one as absent: a form's `<select>` sends
/// `status=` for "any status". A wrong value is still refused.
pub fn empty_as_none<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: FromStr,
    T::Err: std::fmt::Display,
{
    let raw = Option::<String>::deserialize(deserializer)?;
    match raw.as_deref().map(str::trim) {
        None | Some("") => Ok(None),
        Some(value) => T::from_str(value)
            .map(Some)
            .map_err(serde::de::Error::custom),
    }
}

/// Read a value through `T`'s `FromStr`, for an id whose `Deserialize` takes a
/// stored value unchecked.
pub fn parsed<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: FromStr,
    T::Err: std::fmt::Display,
{
    String::deserialize(deserializer)?
        .parse()
        .map_err(serde::de::Error::custom)
}

/// [`parsed`] over an optional list.
pub fn parsed_list<'de, D, T>(deserializer: D) -> Result<Option<Vec<T>>, D::Error>
where
    D: Deserializer<'de>,
    T: FromStr,
    T::Err: std::fmt::Display,
{
    Option::<Vec<String>>::deserialize(deserializer)?
        .map(|raw| raw.iter().map(|s| s.parse()).collect::<Result<Vec<T>, _>>())
        .transpose()
        .map_err(serde::de::Error::custom)
}

/// A comma-separated query list read through `T`'s `FromStr`. Present but empty
/// stays `Some`, so a route can still refuse the parameter.
pub mod comma_list {
    use std::fmt::Display;
    use std::str::FromStr;

    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S, T>(list: &Option<Vec<T>>, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
        T: Display,
    {
        match list {
            Some(list) => {
                let parts: Vec<String> = list.iter().map(ToString::to_string).collect();
                serializer.serialize_str(&parts.join(","))
            }
            None => serializer.serialize_none(),
        }
    }

    pub fn deserialize<'de, D, T>(deserializer: D) -> Result<Option<Vec<T>>, D::Error>
    where
        D: Deserializer<'de>,
        T: FromStr,
        T::Err: Display,
    {
        Option::<String>::deserialize(deserializer)?
            .map(|list| {
                list.split(',')
                    .map(str::trim)
                    .filter(|part| !part.is_empty())
                    .map(str::parse)
                    .collect::<Result<Vec<T>, _>>()
            })
            .transpose()
            .map_err(serde::de::Error::custom)
    }
}

/// `T` or `null`, for a field that must be present but may be empty: the
/// derive drops the `null` from a required `Option`.
#[cfg(feature = "schemars")]
pub(crate) fn nullable<T: schemars::JsonSchema>(
    generator: &mut schemars::SchemaGenerator,
) -> schemars::Schema {
    generator.subschema_for::<Option<T>>()
}

/// Every refusal's body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct ErrorBody {
    pub error: String,
}

#[cfg(test)]
mod tests {
    use rstest::rstest;
    use strum::IntoEnumIterator as _;

    use super::*;

    #[test]
    fn an_order_is_read_by_name() {
        assert_eq!("newest".parse(), Ok(ListOrder::Newest));
        assert_eq!("oldest".parse(), Ok(ListOrder::Oldest));
        assert!("bogus".parse::<ListOrder>().is_err());
    }

    #[derive(Debug, Deserialize, Serialize)]
    struct Plans {
        #[serde(
            default,
            with = "crate::comma_list",
            skip_serializing_if = "Option::is_none"
        )]
        plans: Option<Vec<crate::PlanId>>,
    }

    #[rstest]
    #[case::absent(serde_json::json!({}), None)]
    #[case::empty(serde_json::json!({ "plans": "" }), Some(vec![]))]
    #[case::blanks_skipped(
        serde_json::json!({ "plans": " plan-a , ,plan-b" }),
        Some(vec!["plan-a", "plan-b"])
    )]
    fn a_comma_list_reads(
        #[case] query: serde_json::Value,
        #[case] expected: Option<Vec<&str>>,
    ) -> anyhow::Result<()> {
        let Plans { plans } = serde_json::from_value(query)?;
        let plans = plans.map(|plans| plans.iter().map(ToString::to_string).collect::<Vec<_>>());
        assert_eq!(
            plans,
            expected.map(|e| e.into_iter().map(String::from).collect())
        );
        Ok(())
    }

    #[test]
    fn a_comma_list_refuses_a_bad_id() {
        assert!(
            serde_json::from_value::<Plans>(serde_json::json!({ "plans": "plan-a,bad!id" }))
                .is_err()
        );
    }

    #[test]
    fn a_comma_list_serializes_joined() -> anyhow::Result<()> {
        let plans =
            serde_json::from_value::<Plans>(serde_json::json!({ "plans": "plan-a,plan-b" }))?;
        assert_eq!(
            serde_json::to_value(plans)?,
            serde_json::json!({ "plans": "plan-a,plan-b" })
        );
        Ok(())
    }

    #[test]
    fn fleet_detail_label_matches_the_serde_spelling() {
        FleetDetail::iter().for_each(|detail| {
            assert_eq!(serde_json::json!(detail), serde_json::json!(detail.label()));
        });
    }
}
