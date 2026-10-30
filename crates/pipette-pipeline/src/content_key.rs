//! The content key: a stable digest over a job's content, so ingestion runs a
//! cell that two plans describe only once.
//!
//! Eligibility, negatives included, is keyed: a merge must never widen who may
//! run a job.

use std::collections::BTreeSet;

use serde_json::{Map, Value};

use pipette_core::canonical_json::sha256_hex;
use pipette_core::matching::REQUIRED_FLAGS;
use pipette_plan_types::canonical_json as canonicalize;

use crate::vault_check::drop_custom_labels;

/// Hashed into the preimage, so v2 digests stay apart from v1 records that
/// share the `content-keys/v1/` namespace.
const VERSION: &str = "pipette-content-key-v2";
const LEGACY_VERSION: &str = "pipette-content-key-v1";
const CELL_VERSION: &str = "pipette-cell-key-v1";

/// Identity is what a merge collapses, and the join rule negotiates the
/// deadline instead.
const UNKEYED: [&str; 4] = ["job_id", "plan_id", "expires_at", "merge_duplicates"];
const FLAG_CLAUSES: [&str; 4] = ["requires", "clients", "excludes", "exclude_clients"];
const GROUP_CLAUSES: [&str; 2] = ["any_of", "not_all_of"];

/// The content digest of an input job body, before ingestion stamps identity.
///
/// An opted-out job still publishes its digest, so a later plan can join it.
/// A vault source's `custom_label` is unkeyed, so labelled and unlabelled
/// bodies of one cell still merge.
///
/// Limitation: `1` and `1.0` digest apart; that only misses a merge.
pub fn content_digest(body: &Value) -> String {
    let mut content = body.clone();
    if let Some(fields) = content.as_object_mut() {
        UNKEYED.into_iter().for_each(|field| {
            fields.remove(field);
        });
        FLAG_CLAUSES.into_iter().for_each(|field| {
            fields.insert(field.to_string(), flag_set(body, field));
        });
        GROUP_CLAUSES.into_iter().for_each(|field| {
            fields.insert(field.to_string(), group_sets(body, field));
        });
    }
    drop_custom_labels(&mut content);
    let preimage = serde_json::json!({"v": VERSION, "body": content});
    sha256_hex(&canonicalize(&preimage))
}

/// [`content_digest`] without the eligibility clauses: the cell a job measures,
/// whoever runs it. A revision's carry reads it to find a finished run whose
/// requirements changed, and the job-cell index keys on it. Nothing merges on it.
pub fn cell_digest(body: &Value) -> String {
    let mut content = body.clone();
    if let Some(fields) = content.as_object_mut() {
        UNKEYED
            .into_iter()
            .chain(FLAG_CLAUSES)
            .chain(GROUP_CLAUSES)
            .chain([REQUIRED_FLAGS])
            .for_each(|field| {
                fields.remove(field);
            });
    }
    drop_custom_labels(&mut content);
    let preimage = serde_json::json!({"v": CELL_VERSION, "body": content});
    sha256_hex(&canonicalize(&preimage))
}

/// The former digest, used only to recognize a manifest written under v1.
/// Do not use it for new placements: it ignores fields that can hold secrets.
pub(crate) fn legacy_content_digest(body: &Value) -> String {
    let mut preimage = Map::new();
    preimage.insert("any_of".to_string(), group_sets(body, "any_of"));
    preimage.insert("clients".to_string(), flag_set(body, "clients"));
    preimage.insert(
        "exclude_clients".to_string(),
        flag_set(body, "exclude_clients"),
    );
    preimage.insert("excludes".to_string(), flag_set(body, "excludes"));
    preimage.insert("not_all_of".to_string(), group_sets(body, "not_all_of"));
    preimage.insert("requires".to_string(), flag_set(body, "requires"));
    preimage.insert(
        "spec".to_string(),
        body.get("spec").cloned().unwrap_or(Value::Null),
    );
    preimage.insert("v".to_string(), Value::String(LEGACY_VERSION.to_string()));
    sha256_hex(&canonicalize(&Value::Object(preimage)))
}

/// The body's `merge_duplicates` field; absent or null merges. The error is the
/// malformed value, for the rejection to name.
pub fn merge_duplicates(body: &Value) -> Result<bool, &Value> {
    match body.get("merge_duplicates") {
        None | Some(Value::Null) => Ok(true),
        Some(Value::Bool(b)) => Ok(*b),
        Some(other) => Err(other),
    }
}

/// Whether this input looks for a job to join. Validation has already rejected
/// a malformed value through [`merge_duplicates`].
pub fn mergeable(body: &Value) -> bool {
    merge_duplicates(body).unwrap_or(true)
}

fn flag_set(body: &Value, field: &str) -> Value {
    let set: BTreeSet<&str> = body
        .get(field)
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    Value::Array(set.into_iter().map(|s| Value::String(s.into())).collect())
}

/// A sorted set of sorted groups. An empty group survives: `not_all_of: [[]]`
/// excludes everyone, `not_all_of: []` nobody.
fn group_sets(body: &Value, field: &str) -> Value {
    let groups: BTreeSet<BTreeSet<&str>> = body
        .get(field)
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_array)
        .map(|group| group.iter().filter_map(Value::as_str).collect())
        .collect();
    Value::Array(
        groups
            .into_iter()
            .map(|group| Value::Array(group.into_iter().map(|s| Value::String(s.into())).collect()))
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use rstest::rstest;
    use serde_json::json;

    use super::*;

    fn spec() -> Value {
        json!({"benchmark": "bench-1", "model": {"name": "m"}, "runtime": {"name": "r"}})
    }

    fn body() -> Value {
        json!({"requires": ["os:macos", "runtime:mlx"], "spec": spec()})
    }

    fn digest(body: &Value) -> String {
        content_digest(body)
    }

    #[rstest]
    #[case::a_vault_source(
        json!({"source": "vault_url", "url": "/vault/resources/vault_1/content", "principal_id": "org_1"}),
        true
    )]
    #[case::a_public_source(json!({"source": "huggingface", "org": "o", "repo_name": "r"}), false)]
    fn only_a_vault_sources_custom_label_is_outside_the_key(
        #[case] model: Value,
        #[case] same_cell: bool,
    ) {
        let cell = |model: &Value| json!({"requires": ["os:macos"], "spec": {"model": model}});
        let mut labelled = model.clone();
        labelled["custom_label"] = json!("ACMELM q4");
        assert_eq!(digest(&cell(&labelled)) == digest(&cell(&model)), same_cell);
    }

    /// Each case is a rewriting of [`body`] that must not change the key.
    #[rstest]
    #[case::requires_order(json!({"requires": ["runtime:mlx", "os:macos"], "spec": spec()}))]
    #[case::envelope_order(json!({"spec": spec(), "requires": ["os:macos", "runtime:mlx"]}))]
    #[case::duplicate_flag(
        json!({"requires": ["os:macos", "runtime:mlx", "os:macos"], "spec": spec()})
    )]
    #[case::empty_clients(
        json!({"requires": ["os:macos", "runtime:mlx"], "clients": [], "spec": spec()})
    )]
    #[case::empty_any_of(
        json!({"requires": ["os:macos", "runtime:mlx"], "any_of": [], "spec": spec()})
    )]
    #[case::empty_excludes(
        json!({"requires": ["os:macos", "runtime:mlx"], "excludes": [], "spec": spec()})
    )]
    #[case::empty_not_all_of(
        json!({"requires": ["os:macos", "runtime:mlx"], "not_all_of": [], "spec": spec()})
    )]
    #[case::empty_exclude_clients(
        json!({"requires": ["os:macos", "runtime:mlx"], "exclude_clients": [], "spec": spec()})
    )]
    #[case::explicit_opt_in(
        json!({"requires": ["os:macos", "runtime:mlx"], "merge_duplicates": true, "spec": spec()})
    )]
    #[case::expiry_excluded(
        json!({"requires": ["os:macos", "runtime:mlx"], "expires_at": "never", "spec": spec()})
    )]
    fn same_cell_digests_alike(#[case] other: Value) {
        assert_eq!(digest(&body()), digest(&other));
    }

    #[rstest]
    #[case::extra_requires(json!({"requires": ["os:macos"], "spec": spec()}))]
    #[case::different_requires(json!({"requires": ["os:ios", "runtime:mlx"], "spec": spec()}))]
    #[case::added_clients(
        json!({"requires": ["os:macos", "runtime:mlx"], "clients": ["ev1_a"], "spec": spec()})
    )]
    #[case::added_any_of(
        json!({"requires": ["os:macos", "runtime:mlx"], "any_of": [["a", "b"]], "spec": spec()})
    )]
    #[case::added_excludes(
        json!({"requires": ["os:macos", "runtime:mlx"], "excludes": ["chip:a15"], "spec": spec()})
    )]
    #[case::added_not_all_of(json!({
        "requires": ["os:macos", "runtime:mlx"],
        "not_all_of": [["chip:a15", "runtime:mlx"]],
        "spec": spec(),
    }))]
    #[case::added_exclude_clients(json!({
        "requires": ["os:macos", "runtime:mlx"],
        "exclude_clients": ["ev1_a"],
        "spec": spec(),
    }))]
    #[case::different_spec(
        json!({"requires": ["os:macos", "runtime:mlx"], "spec": json!({"benchmark": "bench-2"})})
    )]
    #[case::nested_spec_difference(json!({
        "requires": ["os:macos", "runtime:mlx"],
        "spec": json!({"benchmark": "bench-1", "model": {"name": "other"}, "runtime": {"name": "r"}}),
    }))]
    #[case::top_level_secret(json!({
        "requires": ["os:macos", "runtime:mlx"],
        "spec": spec(),
        "private_note": "owner only",
    }))]
    #[case::top_level_credential(json!({
        "requires": ["os:macos", "runtime:mlx"],
        "spec": spec(),
        "credentials": {"token": "hf_OTHER"},
    }))]
    fn different_cells_digest_apart(#[case] other: Value) {
        assert_ne!(digest(&body()), digest(&other));
    }

    #[rstest]
    #[case::any_of("any_of")]
    #[case::not_all_of("not_all_of")]
    fn a_grouped_field_normalizes_at_both_levels(#[case] field: &str) {
        let mut one = json!({"requires": ["os:macos"], "spec": spec()});
        one[field] = json!([["a", "b"], ["c", "d"]]);
        let mut other = json!({"requires": ["os:macos"], "spec": spec()});
        other[field] = json!([["d", "c"], ["b", "a"]]);
        assert_eq!(digest(&one), digest(&other));
    }

    #[test]
    fn an_empty_group_digests_apart_from_an_empty_list() {
        let empty_group = json!({
            "requires": ["os:macos"],
            "not_all_of": [[]],
            "spec": spec(),
        });
        let empty_list = json!({
            "requires": ["os:macos"],
            "not_all_of": [],
            "spec": spec(),
        });
        assert_ne!(digest(&empty_group), digest(&empty_list));
    }

    #[test]
    fn whitespace_does_not_reach_the_digest() -> anyhow::Result<()> {
        let compact: Value = serde_json::from_str(
            r#"{"requires":["os:macos"],"spec":{"benchmark":"bench-1","model":{},"runtime":{}}}"#,
        )?;
        let spaced: Value = serde_json::from_str(
            "{ \"requires\" : [ \"os:macos\" ] ,\n  \"spec\" : { \"benchmark\" : \"bench-1\" ,\
             \n \"model\" : { } , \"runtime\" : { } } }",
        )?;
        assert_eq!(digest(&compact), digest(&spaced));
        Ok(())
    }

    #[test]
    fn integer_and_float_spellings_are_not_unified() {
        let integer = json!({"requires": ["os:macos"], "spec": {"tokens": 1}});
        let float = json!({"requires": ["os:macos"], "spec": {"tokens": 1.0}});
        assert_ne!(digest(&integer), digest(&float));
    }

    #[test]
    fn opt_out_keeps_the_key_of_its_cell() {
        let mut opted_out = body();
        opted_out["merge_duplicates"] = json!(false);
        assert_eq!(digest(&opted_out), digest(&body()));
        assert!(!mergeable(&opted_out));
        assert!(mergeable(&body()));
    }

    #[rstest]
    #[case::absent(None, Ok(true))]
    #[case::null(Some(json!(null)), Ok(true))]
    #[case::opt_out(Some(json!(false)), Ok(false))]
    #[case::opt_in(Some(json!(true)), Ok(true))]
    #[case::string(Some(json!("false")), Err(()))]
    #[case::number(Some(json!(0)), Err(()))]
    fn merge_duplicates_arms(#[case] field: Option<Value>, #[case] expected: Result<bool, ()>) {
        let mut body = body();
        if let Some(field) = field {
            body["merge_duplicates"] = field;
        }
        assert_eq!(merge_duplicates(&body).map_err(|_| ()), expected);
        assert_eq!(mergeable(&body), expected.unwrap_or(true));
    }

    /// A preimage change that keeps `v2` fails here: take a new version.
    #[test]
    fn v2_digest_is_pinned() {
        let body = json!({"requires": ["os:macos"], "spec": {"benchmark": "bench-1"}});
        assert_eq!(
            digest(&body),
            "43c6050384dfe17e0237692573556cf307506cf238499711b580377051266ffe"
        );
    }

    #[test]
    fn a_cell_digest_leaves_out_eligibility_only() {
        let routed = json!({
            "requires": ["os:ios", "tag:lab-east"],
            "any_of": [["device:iphone17pro"]],
            "excludes": ["chip:a15"],
            "not_all_of": [["os:ios", "chip:a16"]],
            "clients": ["ev1_a"],
            "exclude_clients": ["ev1_b"],
            "required_flags": ["tag:lab-east"],
            "expires_at": "20290101T000000Z",
            "spec": spec(),
        });
        assert_eq!(cell_digest(&routed), cell_digest(&body()));
        let mut other = body();
        other["spec"]["benchmark"] = json!("bench-2");
        assert_ne!(cell_digest(&other), cell_digest(&body()));
        assert_ne!(cell_digest(&body()), digest(&body()));
    }

    #[test]
    fn v1_digest_is_pinned_for_migration() {
        let body = json!({"requires": ["os:macos"], "spec": {"benchmark": "bench-1"}});
        assert_eq!(
            legacy_content_digest(&body),
            "a3fac3b6b8a01163cf808bd0695063e13029610c5bd1540e77a25486b67588ef"
        );
    }
}
