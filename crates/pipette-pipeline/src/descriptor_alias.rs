//! The check that a stored descriptor names the same build as a catalog
//! member, also when the two digests differ. A client can serialize a
//! descriptor in its own way, and an author can write a field that holds its
//! default value. Each of these gives a different digest for the same build.
//!
//! An observed descriptor gets an alias to a member only if both are true:
//!
//! - It parses as the typed descriptor, and the parse ignores no key. An
//!   unknown key blocks the alias, because the two builds can differ in it.
//! - The parsed value is equal to the member's value.

use std::collections::HashMap;
use std::hash::Hash;

use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

/// One step of the path to a key in a JSON document.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Step {
    Key(String),
    Index(usize),
}

/// A descriptor parsed as `T`, and the first key that the parse ignored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Strict<T> {
    pub value: T,
    /// A dotted path, for example `source.zzz`. `None` if the parse used
    /// every key.
    pub unknown_key: Option<String>,
}

/// What the check finds for one observed descriptor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AliasVerdict<'a, M> {
    /// The descriptor has no unknown key, and its value is equal to a member.
    Alias(&'a M),
    /// The value is equal to a member, but the parse ignored this key.
    Blocked { unknown_key: String },
    /// The descriptor parses, but no member has its value.
    NoMember,
    /// The descriptor does not parse as `T`.
    Unparsed,
}

/// Values that each fail to parse as a field of most types. A known field
/// refuses at least one of them. The parse ignores both values under an
/// unknown key.
fn sentinels() -> [Value; 2] {
    [Value::Bool(true), Value::Object(serde_json::Map::new())]
}

/// Parse `observed` as `T`, and find a key that the parse ignored.
///
/// serde does not report an ignored key inside an internally tagged enum or a
/// flattened struct, and `Runtime` and `Model` use both. So this check uses a
/// probe. It serializes the parsed value again, and finds each key of
/// `observed` that the new document does not have. Such a key is one of these:
///
/// - a known field that holds its default value;
/// - a serde alias of a known field;
/// - an unknown key.
///
/// The probe puts each such key into the new document with a [`sentinels`]
/// value, and parses the result. A known field refuses a sentinel, so the
/// parse fails. An unknown key is ignored, so the parse succeeds. A field
/// that accepts every sentinel counts as unknown, so an uncertain key blocks
/// the alias. Object-valued extra keys are blocked even when the parent key
/// is known: probing the parent cannot prove that its children were all used.
pub fn parse_strict<T: DeserializeOwned + Serialize>(observed: &Value) -> Option<Strict<T>> {
    let value: T = serde_json::from_value(observed.clone()).ok()?;
    let typed = serde_json::to_value(&value).ok()?;
    let unknown_key = extra_keys(observed, &typed)
        .into_iter()
        .find(|path| {
            value_at(observed, path).is_some_and(|value| value.is_object() || value.is_array())
                || !is_known::<T>(&typed, path)
        })
        .map(|path| dotted(&path));
    Some(Strict { value, unknown_key })
}

/// Check `observed` against the members, keyed by their typed values.
///
/// A model descriptor must have its vault `custom_label`s dropped first, on
/// both sides, as the digest drops them
/// ([`pipette_plan_types::drop_vault_custom_labels`]).
pub fn alias_verdict<'a, T, M>(observed: &Value, members: &'a HashMap<T, M>) -> AliasVerdict<'a, M>
where
    T: DeserializeOwned + Serialize + Eq + Hash,
{
    let Some(parsed) = parse_strict::<T>(observed) else {
        return AliasVerdict::Unparsed;
    };
    match (members.get(&parsed.value), parsed.unknown_key) {
        (None, _) => AliasVerdict::NoMember,
        (Some(member), None) => AliasVerdict::Alias(member),
        (Some(_), Some(unknown_key)) => AliasVerdict::Blocked { unknown_key },
    }
}

/// The paths of the keys in `observed` that `typed` does not have. The walk
/// goes into an object or an array only where both documents have one.
fn extra_keys(observed: &Value, typed: &Value) -> Vec<Vec<Step>> {
    let under = |step: Step, paths: Vec<Vec<Step>>| {
        paths.into_iter().map(move |mut path| {
            path.insert(0, step.clone());
            path
        })
    };
    match (observed, typed) {
        (Value::Object(observed), Value::Object(typed)) => observed
            .iter()
            .flat_map(|(key, value)| match typed.get(key) {
                Some(typed) => under(Step::Key(key.clone()), extra_keys(value, typed)).collect(),
                None => vec![vec![Step::Key(key.clone())]],
            })
            .collect(),
        (Value::Array(observed), Value::Array(typed)) => observed
            .iter()
            .zip(typed)
            .enumerate()
            .flat_map(|(index, (value, typed))| {
                under(Step::Index(index), extra_keys(value, typed)).collect::<Vec<_>>()
            })
            .collect(),
        _ => Vec::new(),
    }
}

/// Whether `T` refuses a sentinel at `path` in `typed`.
fn is_known<T: DeserializeOwned>(typed: &Value, path: &[Step]) -> bool {
    sentinels().into_iter().any(|sentinel| {
        with_value_at(typed, path, sentinel)
            .is_some_and(|probe| serde_json::from_value::<T>(probe).is_err())
    })
}

fn value_at<'a>(document: &'a Value, path: &[Step]) -> Option<&'a Value> {
    path.iter().try_fold(document, |node, step| match step {
        Step::Key(key) => node.get(key),
        Step::Index(index) => node.get(*index),
    })
}

/// A copy of `document` with `value` at `path`. `None` if the parent of the
/// path is not an object in `document`.
fn with_value_at(document: &Value, path: &[Step], value: Value) -> Option<Value> {
    let (Step::Key(last), parents) = path.split_last()? else {
        return None;
    };
    let mut copy = document.clone();
    let parent = parents
        .iter()
        .try_fold(&mut copy, |node, step| match step {
            Step::Key(key) => node.get_mut(key.as_str()),
            Step::Index(index) => node.get_mut(*index),
        })?;
    parent.as_object_mut()?.insert(last.clone(), value);
    Some(copy)
}

fn dotted(path: &[Step]) -> String {
    path.iter()
        .map(|step| match step {
            Step::Key(key) => key.clone(),
            Step::Index(index) => index.to_string(),
        })
        .collect::<Vec<_>>()
        .join(".")
}

#[cfg(test)]
mod tests {
    use rstest::rstest;
    use serde::{Deserialize, Serialize};
    use serde_json::json;

    use pipette_plan_types::{Model, Runtime};

    use super::*;

    fn runtime(version: &str, flavor: &str) -> Value {
        json!({
            "type": "llamacpp_cli_stock_tools", "source": "github_release",
            "repository_url": "github.com/ggml-org/llama.cpp",
            "repository_version": version, "flavor": flavor,
        })
    }

    fn model(repo: &str) -> Value {
        json!({
            "type": "gguf_text", "source": "huggingface", "org": "acme",
            "repo_name": repo, "path": "model.gguf",
            "revision": "0123456789abcdef0123456789abcdef01234567",
        })
    }

    /// `base` with `key` set to `value`. A `None` value removes the key.
    fn with(mut base: Value, key: &str, value: Option<Value>) -> Value {
        if let Some(fields) = base.as_object_mut() {
            match value {
                Some(value) => fields.insert(key.to_owned(), value),
                None => fields.remove(key),
            };
        }
        base
    }

    #[derive(Debug, PartialEq, Eq)]
    enum Expect {
        Alias,
        Blocked(String),
        NoMember,
        Unparsed,
    }

    fn verdict<T>(member: Value, observed: &Value) -> anyhow::Result<Expect>
    where
        T: DeserializeOwned + Serialize + Eq + Hash,
    {
        let members = HashMap::from([(serde_json::from_value::<T>(member)?, "member")]);
        Ok(match alias_verdict(observed, &members) {
            AliasVerdict::Alias(_) => Expect::Alias,
            AliasVerdict::Blocked { unknown_key } => Expect::Blocked(unknown_key),
            AliasVerdict::NoMember => Expect::NoMember,
            AliasVerdict::Unparsed => Expect::Unparsed,
        })
    }

    #[rstest]
    #[case::minimal(runtime("b1", "macos-arm64"), Expect::Alias)]
    #[case::null_for_an_absent_option(
        with(runtime("b1", "macos-arm64"), "asset", Some(Value::Null)),
        Expect::Alias
    )]
    #[case::a_serde_alias_key(
        with(
            with(runtime("b1", "macos-arm64"), "repository_url", None),
            "source_repo",
            Some(json!("github.com/ggml-org/llama.cpp"))
        ),
        Expect::Alias
    )]
    #[case::unknown_top_level_key(
        with(runtime("b1", "macos-arm64"), "zzz", Some(json!(1))),
        Expect::Blocked("zzz".into())
    )]
    #[case::unknown_key_that_holds_an_object(
        with(runtime("b1", "macos-arm64"), "zzz", Some(json!({"a": 1}))),
        Expect::Blocked("zzz".into())
    )]
    #[case::different_flavor(runtime("b1", "linux-x64-vulkan"), Expect::NoMember)]
    #[case::different_tag(runtime("b2", "macos-arm64"), Expect::NoMember)]
    #[case::different_type(
        with(runtime("b1", "macos-arm64"), "type", Some(json!("partner_runtime"))),
        Expect::Unparsed
    )]
    #[case::not_a_descriptor(json!("llama.cpp b1"), Expect::Unparsed)]
    fn a_runtime_is_checked_against_the_member(
        #[case] observed: Value,
        #[case] expected: Expect,
    ) -> anyhow::Result<()> {
        assert_eq!(
            verdict::<Runtime>(runtime("b1", "macos-arm64"), &observed)?,
            expected
        );
        Ok(())
    }

    #[rstest]
    #[case::minimal(model("AcmeLM-GGUF"), Expect::Alias)]
    #[case::null_for_an_absent_option(
        with(model("AcmeLM-GGUF"), "sha256", Some(Value::Null)),
        Expect::Alias
    )]
    #[case::unknown_key_in_a_flattened_repo(
        with(model("AcmeLM-GGUF"), "zzz", Some(json!("x"))),
        Expect::Blocked("zzz".into())
    )]
    #[case::different_repo(model("OtherLM-GGUF"), Expect::NoMember)]
    #[case::different_revision(
        with(model("AcmeLM-GGUF"), "revision", Some(json!("main"))),
        Expect::NoMember
    )]
    fn a_model_is_checked_against_the_member(
        #[case] observed: Value,
        #[case] expected: Expect,
    ) -> anyhow::Result<()> {
        assert_eq!(verdict::<Model>(model("AcmeLM-GGUF"), &observed)?, expected);
        Ok(())
    }

    /// The probe reaches a key inside an array element.
    #[test]
    fn an_unknown_key_in_an_array_element_is_found() -> anyhow::Result<()> {
        let observed = json!({"items": [{"a": 1, "zzz": 2}]});
        let typed = json!({"items": [{"a": 1}]});
        assert_eq!(
            extra_keys(&observed, &typed)
                .iter()
                .map(|path| dotted(path))
                .collect::<Vec<_>>(),
            vec!["items.0.zzz"]
        );
        Ok(())
    }

    #[test]
    fn an_object_field_alias_cannot_hide_an_unknown_child() -> anyhow::Result<()> {
        #[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize, Serialize)]
        struct Child {
            known: u32,
        }

        #[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize, Serialize)]
        struct Parent {
            #[serde(alias = "old_child")]
            child: Child,
        }

        let member = json!({"child": {"known": 1}});
        let observed = json!({"old_child": {"known": 1, "unknown": 2}});
        assert_eq!(
            verdict::<Parent>(member, &observed)?,
            Expect::Blocked("old_child".into())
        );
        Ok(())
    }
}
