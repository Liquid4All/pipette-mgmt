//! Canonical JSON for the opaque descriptor and flag fields: keys sorted at
//! every depth, whitespace stripped, so equal content stores as equal bytes and
//! pattern search over the stored string is stable.

use serde_json::Value;

use pipette_plan_types::canonical_json as canonicalize;

/// Canonicalize a JSON string. Input that is not valid JSON is returned
/// trimmed: the server never rejects an opaque ref, it only normalizes what it
/// parses.
pub fn canonicalize_str(s: &str) -> String {
    match serde_json::from_str::<Value>(s) {
        Ok(value) => canonicalize(&value),
        Err(_) => s.trim().to_string(),
    }
}

/// Canonicalize one of the opaque flag fields (`model_flags`, `runtime_flags`,
/// `benchmark_flags`) for storage, collapsing a top-level `{}` or blank value to
/// `None`: "nothing reported" needs one spelling and one hash. A nested `{}` is
/// a claim and stays; a plain string (`--n-gpu-layers 999`) is stored trimmed.
pub fn canonicalize_flags(value: Option<&str>) -> Option<String> {
    value
        .map(canonicalize_str)
        .filter(|flags| flags != "{}" && !flags.is_empty())
}

/// `value` with every object's keys inserted in sorted order, recursively: a
/// build with `serde_json/preserve_order` serializes a map in insertion order.
pub fn sort_keys(value: Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut entries: Vec<(String, Value)> = map.into_iter().collect();
            entries.sort_by(|(a, _), (b, _)| a.cmp(b));
            Value::Object(
                entries
                    .into_iter()
                    .map(|(k, v)| (k, sort_keys(v)))
                    .collect(),
            )
        }
        Value::Array(items) => Value::Array(items.into_iter().map(sort_keys).collect()),
        scalar @ (Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_)) => scalar,
    }
}

/// Hex `sha256` of a string — the stable content id stored for a canonical
/// `model_descriptor` / `runtime_descriptor` (identical descriptors hash alike).
pub fn sha256_hex(s: &str) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(s.as_bytes()))
}

#[cfg(test)]
mod tests {
    use rstest::rstest;
    use serde_json::json;

    use super::*;

    /// `canonicalize` on an already-parsed `Value`: sorts object keys
    /// recursively, preserves array order, compacts scalars.
    #[rstest]
    #[case(json!({ "type": "x", "org": "o", "filename": "f" }), r#"{"filename":"f","org":"o","type":"x"}"#)]
    #[case(json!({ "outer": { "z": 1, "a": 2 }, "b": 3 }), r#"{"b":3,"outer":{"a":2,"z":1}}"#)]
    #[case(
        json!({ "artifacts": [ { "role": "b", "id": 2 }, { "id": 1, "role": "a" } ] }),
        r#"{"artifacts":[{"id":2,"role":"b"},{"id":1,"role":"a"}]}"#
    )]
    fn canonicalize_sorts_keys_and_compacts(#[case] input: Value, #[case] expected: &str) {
        assert_eq!(canonicalize(&input), expected);
    }

    /// `canonicalize_str` on a raw wire string: strips whitespace and sorts keys
    /// when the input parses; returns it trimmed-but-unchanged when it doesn't.
    #[rstest]
    // whitespace and key order in the raw string are both normalized
    #[case("{ \"b\" : 2 ,\n\"a\": 1 }", r#"{"a":1,"b":2}"#)]
    #[case(
        r#"{ "type": "hf_gguf_text", "org": "meta-llama", "repo_name": "llama-3.2-1b", "filename": "Q4_K_M.gguf" }"#,
        r#"{"filename":"Q4_K_M.gguf","org":"meta-llama","repo_name":"llama-3.2-1b","type":"hf_gguf_text"}"#
    )]
    // four-artifact audio model: many keys, stably ordered
    #[case(
        r#"{"type":"hf_gguf_audio","repo_name":"LFM2.5-Audio-1.5B-GGUF","org":"LiquidAI","filename":"m.gguf","vocoder_filename":"v.gguf","mmproj_filename":"p.gguf","tokenizer_filename":"t.gguf"}"#,
        r#"{"filename":"m.gguf","mmproj_filename":"p.gguf","org":"LiquidAI","repo_name":"LFM2.5-Audio-1.5B-GGUF","tokenizer_filename":"t.gguf","type":"hf_gguf_audio","vocoder_filename":"v.gguf"}"#
    )]
    // unparseable input is returned trimmed, otherwise unchanged
    #[case("  not json  ", "not json")]
    fn canonicalize_str_normalizes_or_passes_through(#[case] input: &str, #[case] expected: &str) {
        assert_eq!(canonicalize_str(input), expected);
    }

    /// Keys sort at every depth; array order holds.
    #[test]
    fn sort_keys_orders_every_object() -> anyhow::Result<()> {
        let value =
            serde_json::from_str(r#"{ "b": {"z": 1, "a": 2}, "a": [ {"y": 1, "x": 2} ] }"#)?;
        assert_eq!(
            serde_json::to_string(&sort_keys(value))?,
            r#"{"a":[{"x":2,"y":1}],"b":{"a":2,"z":1}}"#
        );
        Ok(())
    }

    /// `canonicalize_flags` adds the top-level-empty-object rule on top of
    /// `canonicalize_str`, and keeps the plain-string spelling that
    /// `model_flags` / `runtime_flags` are documented to accept.
    #[rstest]
    #[case::absent(None, None)]
    #[case::sorts_keys(Some(r#"{ "b": 1, "a": 2 }"#), Some(r#"{"a":2,"b":1}"#))]
    // "nothing reported" must have exactly one spelling and one hash bucket
    #[case::top_level_empty(Some("{ }"), None)]
    // a nested empty object is a different claim and survives
    #[case::nested_empty(Some(r#"{ "readiness": { } }"#), Some(r#"{"readiness":{}}"#))]
    #[case::plain_string(Some("  --n-gpu-layers 999  "), Some("--n-gpu-layers 999"))]
    // whitespace-only is not a claim; it collapses like an absent field rather
    // than becoming an empty-string grouping bucket
    #[case::blank(Some("   "), None)]
    fn canonicalize_flags_collapses_empty_and_keeps_plain_strings(
        #[case] input: Option<&str>,
        #[case] expected: Option<&str>,
    ) {
        assert_eq!(canonicalize_flags(input).as_deref(), expected);
    }
}
