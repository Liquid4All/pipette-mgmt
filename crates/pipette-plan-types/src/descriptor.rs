//! Canonical descriptor JSON and the digest that addresses it, so the id
//! survives key order and whitespace. Other clients reproduce the rule in their
//! own languages, so its fixed test vectors must not change.

use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha256};

/// Compact JSON with object keys sorted recursively. Array order is kept, and
/// numbers keep `serde_json`'s rendering, so `1` and `1.0` stay distinct.
pub fn canonical_json(value: &Value) -> String {
    let mut out = String::new();
    write_canonical(&mut out, value);
    out
}

/// Hex SHA-256 over the canonical form of `value` — the id `pipette-mgmt`
/// stores as `runtime_descriptor_sha256`.
pub fn descriptor_digest<T: Serialize>(value: &T) -> Result<String, serde_json::Error> {
    let json = serde_json::to_value(value)?;
    Ok(hex::encode(Sha256::digest(
        canonical_json(&json).as_bytes(),
    )))
}

/// Whether an object is a vault source, tagged on `source` (model dialects) or
/// on `type` (a uv runtime's own source table).
pub fn is_vault_source(fields: &serde_json::Map<String, Value>) -> bool {
    ["source", "type"].into_iter().any(|key| {
        fields
            .get(key)
            .and_then(Value::as_str)
            .is_some_and(|tag| tag.starts_with("vault_"))
    })
}

/// Whether any nested object names a vault source.
pub fn names_vault_source(value: &Value) -> bool {
    match value {
        Value::Object(fields) => is_vault_source(fields) || fields.values().any(names_vault_source),
        Value::Array(items) => items.iter().any(names_vault_source),
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => false,
    }
}

/// Remove labels from vault sources before taking a descriptor's content id.
/// A label is a record name, not part of the model or runtime identity.
pub fn drop_vault_custom_labels(value: &mut Value) -> bool {
    match value {
        Value::Object(fields) => {
            let own = is_vault_source(fields) && fields.remove("custom_label").is_some();
            let nested = fields
                .values_mut()
                .map(drop_vault_custom_labels)
                .filter(|dropped| *dropped)
                .count()
                > 0;
            own || nested
        }
        Value::Array(items) => {
            items
                .iter_mut()
                .map(drop_vault_custom_labels)
                .filter(|dropped| *dropped)
                .count()
                > 0
        }
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => false,
    }
}

/// Unambiguous over a store's entries, short enough to retype.
const DIGEST_DISPLAY_LEN: usize = 12;

/// A [`descriptor_digest`] truncated for display.
pub fn short_digest(digest: &str) -> &str {
    digest.get(..DIGEST_DISPLAY_LEN).unwrap_or(digest)
}

fn write_canonical(out: &mut String, value: &Value) {
    match value {
        Value::Object(map) => {
            out.push('{');
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort_unstable();
            keys.iter().enumerate().for_each(|(i, key)| {
                if i > 0 {
                    out.push(',');
                }
                out.push_str(&Value::String((*key).clone()).to_string());
                out.push(':');
                if let Some(v) = map.get(*key) {
                    write_canonical(out, v);
                }
            });
            out.push('}');
        }
        Value::Array(items) => {
            out.push('[');
            items.iter().enumerate().for_each(|(i, item)| {
                if i > 0 {
                    out.push(',');
                }
                write_canonical(out, item);
            });
            out.push(']');
        }
        leaf @ (Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_)) => {
            out.push_str(&leaf.to_string())
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn spelling_does_not_change_the_digest() -> anyhow::Result<()> {
        let a = json!({"type": "mlx", "version": "0.31.3", "flavor": "macos-arm64"});
        let b = json!({"flavor": "macos-arm64", "version": "0.31.3", "type": "mlx"});
        assert_eq!(canonical_json(&a), canonical_json(&b));
        assert_eq!(descriptor_digest(&a)?, descriptor_digest(&b)?);
        Ok(())
    }

    /// Arrays keep their order because position carries meaning (install
    /// flags are argv).
    #[test]
    fn canonical_form_sorts_deeply_and_preserves_arrays() {
        let value = json!({
            "b": {"z": 1, "a": 2},
            "a": ["second", "first"],
        });
        assert_eq!(
            canonical_json(&value),
            r#"{"a":["second","first"],"b":{"a":2,"z":1}}"#
        );
    }

    #[test]
    fn distinct_descriptors_differ() -> anyhow::Result<()> {
        let a = json!({"type": "mlx", "version": "0.31.3"});
        let b = json!({"type": "mlx", "version": "0.31.4"});
        assert_ne!(descriptor_digest(&a)?, descriptor_digest(&b)?);
        Ok(())
    }

    /// Fixed vectors other clients reproduce; a change here splits descriptor ids.
    #[test]
    fn canonicalization_matches_pipette_mgmt() -> anyhow::Result<()> {
        let cases = [
            (json!({}), "{}"),
            (json!({"b": 1, "a": 2}), r#"{"a":2,"b":1}"#),
            (json!({"a": [1, 2]}), r#"{"a":[1,2]}"#),
            (json!({"a": null}), r#"{"a":null}"#),
            (json!({"a": "x y"}), r#"{"a":"x y"}"#),
            // A key is escaped like a string value.
            (json!({"a\"b": 1}), r#"{"a\"b":1}"#),
            // Numbers keep `serde_json`'s rendering: `1.0` is not unified with `1`.
            (json!({"a": 1.0}), r#"{"a":1.0}"#),
        ];
        cases.iter().try_for_each(|(value, expected)| {
            assert_eq!(&canonical_json(value), expected);
            anyhow::Ok(())
        })?;
        // Catches a change to the hashing, not just the canonical form.
        assert_eq!(
            descriptor_digest(&json!({"b": 1, "a": 2}))?,
            hex::encode(Sha256::digest(r#"{"a":2,"b":1}"#.as_bytes()))
        );
        Ok(())
    }

    #[test]
    fn short_digest_truncates_without_panicking_on_short_input() {
        assert_eq!(short_digest("abcdef0123456789abcdef"), "abcdef012345");
        assert_eq!(short_digest("abc"), "abc");
    }
}

#[cfg(test)]
mod onnx_genai_descriptor_tests {
    use crate::runtime::source_repo;
    use crate::{
        OnnxGenaiIosPipette, OnnxGenaiIosPipetteFlavor, OnnxGenaiStack, PrivateApis, Runtime,
        descriptor,
    };

    /// Duplicated verbatim in the iOS suite (`PipetteTests/RuntimeTests.swift`): both
    /// clients must submit byte-identical descriptors, or one runtime gets two ids.
    const CANONICAL: &str = concat!(
        r#"{"flavor":"ios-arm64","packages":{"onnxruntime":{"repository_url":"#,
        r#""github.com/microsoft/onnxruntime","repository_version":"v1.26.0"},"#,
        r#""onnxruntime_genai":{"repository_url":"github.com/microsoft/onnxruntime-genai","#,
        r#""repository_version":"v0.15.2"}},"type":"onnx_genai_ios_pipette"}"#,
    );
    const DIGEST: &str = "529c5b33969fd906c9cae6a75bc714486be88834cc946421724df20000821653";

    fn sample() -> anyhow::Result<Runtime> {
        Ok(Runtime::OnnxGenaiIosPipette(OnnxGenaiIosPipette {
            packages: OnnxGenaiStack {
                onnxruntime: source_repo("github.com/microsoft/onnxruntime", "v1.26.0")?,
                onnxruntime_genai: source_repo(
                    "github.com/microsoft/onnxruntime-genai",
                    "v0.15.2",
                )?,
            },
            flavor: OnnxGenaiIosPipetteFlavor::IosArm64,
            private_apis: PrivateApis::default(),
        }))
    }

    #[test]
    fn onnx_genai_ios_descriptor_matches_the_swift_mirror() -> anyhow::Result<()> {
        let runtime = sample()?;
        assert_eq!(
            descriptor::canonical_json(&serde_json::to_value(&runtime)?),
            CANONICAL
        );
        assert_eq!(descriptor::descriptor_digest(&runtime)?, DIGEST);
        Ok(())
    }

    /// Each private-API combination is its own runtime identity, so none shares a digest
    /// with the stock build or with another combination.
    #[test]
    fn each_private_api_combination_is_a_different_descriptor() -> anyhow::Result<()> {
        let Runtime::OnnxGenaiIosPipette(stock) = sample()? else {
            anyhow::bail!("sample is an OnnxGenaiIosPipette");
        };
        let mut digests = std::collections::HashSet::new();
        for (thermal, memory) in [(false, false), (true, true), (true, false), (false, true)] {
            let runtime = Runtime::OnnxGenaiIosPipette(OnnxGenaiIosPipette {
                private_apis: PrivateApis { thermal, memory },
                ..stock.clone()
            });
            assert!(digests.insert(descriptor::descriptor_digest(&runtime)?));
        }
        assert!(digests.contains(DIGEST));
        Ok(())
    }
}
