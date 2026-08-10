//! Canonicalization for the optional `model_descriptor` / `runtime_descriptor` fields.
//!
//! `model_descriptor` and `runtime_descriptor` are the full, lossless specifications of the
//! model and runtime that produced a submission — the client's own typed
//! descriptors, serialized as nested JSON objects. A single model may
//! be one artifact (an MLX bundle) or several (a llama.cpp VL backbone + its
//! projector, or an audio model with a backbone, encoder-projector, vocoder, and
//! tokenizer — three or four separate pieces); a runtime likewise carries its
//! version/build coordinates inline. Rather than model that open-ended,
//! per-partner structure in the warehouse schema, `pipette-mgmt` treats each as
//! an opaque blob: it does **not** deserialize them into a known type (it
//! cannot — partners define their own runtimes and model formats), it only
//! stores them. The scalar convenience fields (`model_name`, `runtime_name`,
//! `runtime_version`) stay separate for cheap grouping/display.
//!
//! What it *does* do is **canonicalize** the JSON before storing, so that
//! substring/pattern search over the stored string is stable regardless of how a
//! client formatted its payload:
//!
//! - object keys are sorted lexicographically, recursively, at every level;
//! - all insignificant whitespace is stripped (compact form).
//!
//! Two clients that send the same logical `model_descriptor` with different key order or
//! spacing therefore produce byte-identical stored strings, so a query like
//! `model_descriptor LIKE '%"type":"hf_gguf_vision"%'` is reliable. Key ordering is done
//! explicitly here rather than relying on `serde_json::Value`'s map type, whose
//! ordering depends on whether the `preserve_order` feature is enabled anywhere
//! in the build graph.

use serde_json::Value;

/// Canonical, compact JSON string for `value`: object keys sorted recursively,
/// no insignificant whitespace. Array element order is preserved (it is
/// semantically significant); scalars serialize via `serde_json`'s compact form.
///
/// Key ordering and whitespace are the stability guarantees. Numbers are emitted
/// in `serde_json`'s representation, so `1` and `1.0` are *not* unified — refs
/// are almost entirely strings, so this is a cosmetic edge, not a search hazard.
pub fn canonicalize(value: &Value) -> String {
    let mut out = String::new();
    write_canonical(&mut out, value);
    out
}

/// Canonicalize a JSON string. `model_descriptor` / `runtime_descriptor` arrive on the wire
/// as JSON *strings* (the client serializes its typed descriptor); this parses
/// and re-emits them in canonical form (keys sorted, whitespace stripped) for
/// storage. If the input is not valid JSON, it is returned trimmed and
/// otherwise unchanged — the server never rejects an opaque ref, it only
/// normalizes what it can parse so pattern search stays stable.
pub fn canonicalize_str(s: &str) -> String {
    match serde_json::from_str::<Value>(s) {
        Ok(value) => canonicalize(&value),
        Err(_) => s.trim().to_string(),
    }
}

/// Canonicalize one of the three opaque flag fields — `model_flags`,
/// `runtime_flags`, `benchmark_flags` — for storage.
///
/// [`canonicalize_str`] plus one extra rule: a value carrying no claim collapses
/// to `None` — a top-level empty object, or anything empty once trimmed. Storing
/// those would give "nothing reported" several spellings and several hashes,
/// which is the one thing a grouping key must not have. A *nested* empty object
/// is a different claim ("this block exists and is empty") and is preserved.
///
/// `model_flags` / `runtime_flags` are documented to accept a plain string
/// (`--n-gpu-layers 999`) as well as JSON; those fall through
/// `canonicalize_str`'s unparseable path and are stored trimmed.
pub fn canonicalize_flags(value: Option<&str>) -> Option<String> {
    value
        .map(canonicalize_str)
        .filter(|flags| flags != "{}" && !flags.is_empty())
}

/// Hex `sha256` of a string — the stable content id stored for a canonical
/// `model_descriptor` / `runtime_descriptor` (identical descriptors hash alike).
pub fn sha256_hex(s: &str) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(s.as_bytes()))
}

fn write_canonical(out: &mut String, value: &Value) {
    match value {
        Value::Object(map) => {
            out.push('{');
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort_unstable();
            for (i, key) in keys.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                // Encode the key as a JSON string (handles escaping); a lone
                // `Value::String` serializes to a quoted, escaped token.
                out.push_str(&Value::String((*key).to_string()).to_string());
                out.push(':');
                write_canonical(out, &map[*key]);
            }
            out.push('}');
        }
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_canonical(out, item);
            }
            out.push(']');
        }
        // Scalars: `to_string` on a non-container `Value` is already compact.
        scalar => out.push_str(&scalar.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;
    use serde_json::json;

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
