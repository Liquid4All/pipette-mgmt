//! A sequence that is never empty.

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// A [`NonEmptyVec`] was given no items.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
#[error("must not be empty")]
pub struct NonEmptyVecError;

/// `Vec<T>` guaranteed non-empty: built only by `try_new` or by deserializing a
/// non-empty sequence.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct NonEmptyVec<T>(Vec<T>);

impl<T> NonEmptyVec<T> {
    pub fn try_new(items: Vec<T>) -> Result<Self, NonEmptyVecError> {
        if items.is_empty() {
            Err(NonEmptyVecError)
        } else {
            Ok(Self(items))
        }
    }

    pub fn into_inner(self) -> Vec<T> {
        self.0
    }

    /// The first item, which every `NonEmptyVec` has.
    #[allow(
        clippy::indexing_slicing,
        reason = "`try_new` and `Deserialize` refuse an empty sequence"
    )]
    pub fn head(&self) -> &T {
        &self.0[0]
    }

    /// The last item, which every `NonEmptyVec` has.
    #[allow(
        clippy::indexing_slicing,
        reason = "`try_new` and `Deserialize` refuse an empty sequence"
    )]
    pub fn last(&self) -> &T {
        &self.0[self.0.len() - 1]
    }
}

impl<T> AsRef<[T]> for NonEmptyVec<T> {
    fn as_ref(&self) -> &[T] {
        &self.0
    }
}

impl<T> std::ops::Deref for NonEmptyVec<T> {
    type Target = [T];

    fn deref(&self) -> &[T] {
        &self.0
    }
}

impl<T: Serialize> Serialize for NonEmptyVec<T> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        self.0.serialize(serializer)
    }
}

impl<'de, T: Deserialize<'de>> Deserialize<'de> for NonEmptyVec<T> {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let v = Vec::<T>::deserialize(deserializer)?;
        Self::try_new(v).map_err(serde::de::Error::custom)
    }
}

#[cfg(feature = "schemars")]
impl<T: schemars::JsonSchema> schemars::JsonSchema for NonEmptyVec<T> {
    fn inline_schema() -> bool {
        true
    }

    fn schema_name() -> std::borrow::Cow<'static, str> {
        format!("NonEmptyVec_{}", T::schema_name()).into()
    }

    fn json_schema(generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({
            "type": "array",
            "items": generator.subschema_for::<T>(),
            "minItems": 1,
        })
    }
}

#[cfg(test)]
mod tests {

    use super::*;

    #[test]
    fn non_empty_vec_try_new_rejects_empty() {
        let empty: Vec<u32> = vec![];
        assert_eq!(
            NonEmptyVec::try_new(empty).map(NonEmptyVec::into_inner),
            Err(NonEmptyVecError)
        );
        assert_eq!(NonEmptyVecError.to_string(), "must not be empty");
        assert_eq!(
            NonEmptyVec::try_new(vec![1]).map(NonEmptyVec::into_inner),
            Ok(vec![1])
        );
    }

    /// The wire form is the bare sequence, and the non-empty rule holds on read.
    #[test]
    fn non_empty_vec_serializes_as_its_sequence() -> anyhow::Result<()> {
        let items = NonEmptyVec::try_new(vec![3_u32, 1])?;
        let json = serde_json::to_string(&items)?;
        assert_eq!(json, "[3,1]");
        assert_eq!(serde_json::from_str::<NonEmptyVec<u32>>(&json)?, items);
        assert_eq!(*items, [3, 1]);
        assert_eq!(items.as_ref(), [3, 1]);

        let refusal = serde_json::from_str::<NonEmptyVec<u32>>("[]")
            .err()
            .map(|e| e.to_string());
        assert!(
            refusal
                .as_deref()
                .is_some_and(|m| m.starts_with("must not be empty")),
            "{refusal:?}"
        );
        Ok(())
    }
}
