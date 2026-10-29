//! Labels: the free `key: value` pairs a record carries so its readers can
//! group and filter it, on the rules of an AWS resource tag.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// A record's labels, one value a key. Keys and values keep their case and
/// are compared as written.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    try_from = "BTreeMap<String, String>",
    into = "BTreeMap<String, String>"
)]
pub struct Labels(BTreeMap<String, String>);

/// Why a set of labels is refused. Carries the key, so a `400` names it.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum LabelError {
    #[error("a record takes at most {max} labels, not {count}")]
    TooMany { count: usize, max: usize },
    #[error("a label key must not be empty")]
    EmptyKey,
    #[error("label key {key:?} is too long ({len} chars, max {max})")]
    KeyTooLong { key: String, len: usize, max: usize },
    #[error("the value of label {key:?} is too long ({len} chars, max {max})")]
    ValueTooLong { key: String, len: usize, max: usize },
    #[error("label {key:?} contains {ch:?}; allowed: letters, digits, spaces and _ . : / = + - @")]
    InvalidChar { key: String, ch: char },
    #[error("label {key:?} must not start or end with a space")]
    Padded { key: String },
}

impl Labels {
    pub const MAX: usize = 50;
    pub const KEY_MAX_CHARS: usize = 128;
    pub const VALUE_MAX_CHARS: usize = 256;

    pub fn try_new(pairs: BTreeMap<String, String>) -> Result<Self, LabelError> {
        if pairs.len() > Self::MAX {
            return Err(LabelError::TooMany {
                count: pairs.len(),
                max: Self::MAX,
            });
        }
        pairs
            .iter()
            .try_for_each(|(key, value)| check_pair(key, value))?;
        Ok(Self(pairs))
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Each pair, in key order.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.0
            .iter()
            .map(|(key, value)| (key.as_str(), value.as_str()))
    }
}

fn check_pair(key: &str, value: &str) -> Result<(), LabelError> {
    let named = || key.to_owned();
    if key.is_empty() {
        return Err(LabelError::EmptyKey);
    }
    let (key_len, value_len) = (key.chars().count(), value.chars().count());
    if key_len > Labels::KEY_MAX_CHARS {
        return Err(LabelError::KeyTooLong {
            key: named(),
            len: key_len,
            max: Labels::KEY_MAX_CHARS,
        });
    }
    if value_len > Labels::VALUE_MAX_CHARS {
        return Err(LabelError::ValueTooLong {
            key: named(),
            len: value_len,
            max: Labels::VALUE_MAX_CHARS,
        });
    }
    if let Some(ch) = key.chars().chain(value.chars()).find(|c| !allowed(*c)) {
        return Err(LabelError::InvalidChar { key: named(), ch });
    }
    if key.trim() != key || value.trim() != value {
        return Err(LabelError::Padded { key: named() });
    }
    Ok(())
}

/// The characters AWS admits in a tag.
fn allowed(c: char) -> bool {
    c.is_alphanumeric() || c == ' ' || "_.:/=+-@".contains(c)
}

impl TryFrom<BTreeMap<String, String>> for Labels {
    type Error = LabelError;

    fn try_from(pairs: BTreeMap<String, String>) -> Result<Self, Self::Error> {
        Self::try_new(pairs)
    }
}

impl From<Labels> for BTreeMap<String, String> {
    fn from(labels: Labels) -> Self {
        labels.0
    }
}

#[cfg(test)]
mod tests {
    use rstest::rstest;
    use serde_json::json;

    use super::*;

    fn pairs(of: &[(&str, &str)]) -> BTreeMap<String, String> {
        of.iter()
            .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
            .collect()
    }

    #[test]
    fn labels_read_as_an_object_and_write_back_in_key_order() -> anyhow::Result<()> {
        let labels: Labels = serde_json::from_value(
            json!({"study": "acmelm-text", "Device": "S26 Ultra", "kind": ""}),
        )?;
        assert_eq!(
            labels.iter().collect::<Vec<_>>(),
            [
                ("Device", "S26 Ultra"),
                ("kind", ""),
                ("study", "acmelm-text")
            ]
        );
        assert_eq!(
            serde_json::to_value(&labels)?,
            json!({"Device": "S26 Ultra", "kind": "", "study": "acmelm-text"})
        );
        assert!(Labels::default().is_empty());
        Ok(())
    }

    #[rstest]
    #[case::empty_key(&[("", "x")], LabelError::EmptyKey)]
    #[case::padded_key(&[(" study", "x")], LabelError::Padded { key: " study".into() })]
    #[case::padded_value(&[("study", "x ")], LabelError::Padded { key: "study".into() })]
    #[case::key_char(&[("a,b", "x")], LabelError::InvalidChar { key: "a,b".into(), ch: ',' })]
    #[case::value_char(&[("a", "x\ny")], LabelError::InvalidChar { key: "a".into(), ch: '\n' })]
    fn a_pair_outside_the_rules_is_refused(
        #[case] of: &[(&str, &str)],
        #[case] expected: LabelError,
    ) {
        assert_eq!(Labels::try_new(pairs(of)), Err(expected));
    }

    #[test]
    fn the_bounds_are_counted_in_characters_and_pairs() {
        let key = "к".repeat(Labels::KEY_MAX_CHARS);
        let value = "v".repeat(Labels::VALUE_MAX_CHARS);
        assert!(Labels::try_new(pairs(&[(&key, &value)])).is_ok());
        assert!(matches!(
            Labels::try_new(pairs(&[(&format!("{key}k"), "")])),
            Err(LabelError::KeyTooLong { len: 129, .. })
        ));
        assert!(matches!(
            Labels::try_new(pairs(&[("k", &format!("{value}v"))])),
            Err(LabelError::ValueTooLong { len: 257, .. })
        ));
        let many = |n: usize| (0..n).map(|i| (format!("k{i}"), String::new())).collect();
        assert!(Labels::try_new(many(Labels::MAX)).is_ok());
        assert_eq!(
            Labels::try_new(many(Labels::MAX + 1)),
            Err(LabelError::TooMany { count: 51, max: 50 })
        );
    }

    #[test]
    fn a_refused_set_does_not_deserialize() {
        let refused = serde_json::from_value::<Labels>(json!({"a,b": "x"}));
        assert!(refused.is_err_and(|e| e.to_string().contains("a,b")));
    }
}
