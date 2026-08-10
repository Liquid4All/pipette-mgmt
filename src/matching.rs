//! Job ↔ client eligibility matching.
//!
//! [`job_matches_client`] decides whether a client is eligible for a job, the
//! core of the `queue-maintenance` eligible index. The rules are specified in
//! `docs/planner.md` (§Client Matching Rules) and `docs/plan-ingestion.md`
//! (§Capability matching). The function is pure (no I/O), so it is fully
//! unit-testable in isolation.

use std::collections::BTreeSet;

use serde_json::Value;

use crate::client::Client;

/// True if `client` is eligible for the job described by `job_body`.
///
/// Eligibility is the **union** of two independent paths; either alone suffices:
///
/// ```text
/// eligible = client_id ∈ job_body.clients                        (explicit list)
///          OR ( effective_capabilities(client) ⊇ job_body.requires
///               AND every job_body.any_of group shares at least
///                   one flag with effective_capabilities(client) ) (capability flags)
/// ```
///
/// The capability path is a conjunction of clauses: the flat `requires` set
/// (all-of) plus zero or more `any_of` groups (each at-least-one-of). `any_of`
/// only ever *narrows* that path — a job with `any_of` but an empty or absent
/// `requires` matches nobody through it, however satisfiable its groups.
///
/// A present-but-empty `requires` array matches **no** client: requiring zero
/// capabilities would make every client eligible, which a work-dispatch system
/// must never do by accident. This is the fail-closed direction — only the
/// `clients` array can match such a job. Malformed clauses fail closed the same
/// way: a `requires` or `any_of` that is not an array, a group that is not an
/// array, or an element that is not a string all make the capability path match
/// nobody, and an empty `any_of` group is unsatisfiable (an at-least-one-of
/// over nothing). A bad requirement must never *widen* eligibility. (A job with
/// neither `clients` nor `requires` is rejected by plan ingestion and out of
/// scope here.)
pub fn job_matches_client(job_body: &Value, client: &Client) -> bool {
    if client_listed(job_body, client) {
        return true;
    }

    let capabilities = client.effective_capabilities();
    let requires_met = job_requires(job_body)
        .is_some_and(|requires| !requires.is_empty() && requires.is_subset(&capabilities));
    requires_met
        && job_any_of(job_body)
            .is_some_and(|groups| groups.iter().all(|group| !group.is_disjoint(&capabilities)))
}

/// The `clients` roster of a **`clients`-only** job — a non-empty `clients`
/// array and no non-empty `requires` — or `None` for any other job shape.
///
/// A `clients`-only job's eligible set is closed: once every listed client has
/// a `denied/` marker the job can never succeed, and it is escalated to a
/// terminal synthetic `"system"` failure. A job with `requires` is
/// open-ended (a new matching client could appear), so it never classifies
/// here and is left to the `expires_at` backstop. `any_of` plays no part in
/// the classification: it narrows only the capability path, which a
/// no-`requires` job can never match, so the closed roster is the `clients`
/// list either way. Consumed by the `queue-maintenance` all-denied
/// reconciliation pass, the sole owner of that rule.
pub fn clients_only_roster(job_body: &Value) -> Option<Vec<&str>> {
    let clients: Vec<&str> = job_body
        .get("clients")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    let has_requires = job_body
        .get("requires")
        .and_then(Value::as_array)
        .is_some_and(|a| !a.is_empty());
    (!clients.is_empty() && !has_requires).then_some(clients)
}

/// Exact membership of the client's id in the job's `clients` array. Client ids
/// are opaque identifiers, so the test is exact equality.
fn client_listed(job_body: &Value, client: &Client) -> bool {
    job_body
        .get("clients")
        .and_then(Value::as_array)
        .is_some_and(|arr| {
            arr.iter()
                .any(|v| v.as_str() == Some(client.client_id.as_str()))
        })
}

/// A JSON array of flags as a set, or `None` if any element is not a string.
///
/// A non-string element collapses the whole clause to `None` rather than being
/// silently dropped: dropping it could only ever leave a *smaller* set, which
/// is easier to satisfy — the widening direction a fail-closed matcher must
/// avoid.
fn flag_set(flags: &[Value]) -> Option<BTreeSet<String>> {
    flags
        .iter()
        .map(|v| v.as_str().map(str::to_string))
        .collect()
}

/// The job's required capability flags as a set, or `None` when `requires` is
/// absent or malformed (not an array, or containing a non-string element —
/// see [`flag_set`]).
fn job_requires(job_body: &Value) -> Option<BTreeSet<String>> {
    flag_set(job_body.get("requires")?.as_array()?)
}

/// The job's `any_of` clause groups, or `None` when the field is malformed
/// (not an array, a group that is not an array, or a member that is not a
/// string — see [`flag_set`]). An absent `any_of` is an empty group list — no
/// constraint. An **empty group** is well-formed but unsatisfiable (an
/// at-least-one-of over nothing), so it stays in the list and matches nobody.
fn job_any_of(job_body: &Value) -> Option<Vec<BTreeSet<String>>> {
    let Some(any_of) = job_body.get("any_of") else {
        return Some(Vec::new());
    };
    any_of
        .as_array()?
        .iter()
        .map(|group| group.as_array().and_then(|flags| flag_set(flags)))
        .collect()
}

/// True when the job's capability clauses are **malformed**: a `requires`
/// that is present but not a well-formed flag array, or an `any_of` that is
/// not a well-formed list of flag-array groups. Such a job fails closed in
/// [`job_matches_client`] — the sound matching decision, but one that leaves
/// the job silently unclaimable until its author corrects it — so the
/// eligible-index pass uses this classification to leave an operator-visible
/// record. An *absent* `requires` is not malformed: that is the ordinary
/// shape of a `clients`-only job.
pub fn capability_clauses_malformed(job_body: &Value) -> bool {
    (job_body.get("requires").is_some() && job_requires(job_body).is_none())
        || job_any_of(job_body).is_none()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::{Client, DeviceProfile};
    use crate::types::ClientId;
    use crate::validated::{ContactEmail, NonEmptyTrimmedString, PublicKeyHex};
    use crate::warehouse::DeviceFormFactor;
    use rstest::rstest;
    use serde_json::json;

    /// A client with the given device profile and reported capabilities.
    fn client_with(id: &str, profile: DeviceProfile, capabilities: &[&str]) -> Client {
        Client {
            client_id: ClientId::try_new(id).unwrap(),
            public_key: PublicKeyHex::try_new(
                "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789",
            )
            .unwrap(),
            organization: NonEmptyTrimmedString::try_new("org").unwrap(),
            client_details: NonEmptyTrimmedString::try_new("details").unwrap(),
            contact_email: ContactEmail::try_new("a@b.com").unwrap(),
            status: crate::client::ClientStatus::Approved,
            registered_at: "2026-01-01T00:00:00Z".parse().unwrap(),
            device_profile: profile,
            capabilities: capabilities.iter().map(|s| s.to_string()).collect(),
        }
    }

    fn nz(s: &str) -> Option<NonEmptyTrimmedString> {
        Some(NonEmptyTrimmedString::try_new(s).unwrap())
    }

    /// A representative lab device: normalizes to
    /// `{os:ios, os_version:26.1, device:iphone17pro, chip:applea19pro,
    /// form_factor:phone, ram_bytes:8000000000}`, plus a reported
    /// `runtime:llama_cpp` capability.
    fn iphone() -> Client {
        client_with(
            "ev1_a",
            DeviceProfile {
                device_name: nz("iPhone 17 Pro"),
                device_os_name: nz("iOS"),
                device_os_version: nz("26.1"),
                device_chip_model: nz("Apple A19 Pro"),
                device_form_factor: Some(DeviceFormFactor::Phone),
                device_ram_bytes: Some(8_000_000_000),
                ..Default::default()
            },
            &["runtime:llama_cpp"],
        )
    }

    // ── clients array ────────────────────────────────────────────────────────

    #[test]
    fn clients_array_exact_membership() {
        let client = client_with("ev1_a", DeviceProfile::default(), &[]);
        assert!(job_matches_client(
            &json!({"clients": ["ev1_a", "ev1_b"]}),
            &client
        ));
        assert!(!job_matches_client(&json!({"clients": ["ev1_b"]}), &client));
        // No partial / prefix matching on client ids.
        assert!(!job_matches_client(&json!({"clients": ["ev1_"]}), &client));
    }

    #[test]
    fn clients_array_matches_even_without_capabilities() {
        let client = client_with("ev1_a", DeviceProfile::default(), &[]);
        assert!(job_matches_client(&json!({"clients": ["ev1_a"]}), &client));
        // …and even when it also fails the requires path.
        assert!(job_matches_client(
            &json!({"clients": ["ev1_a"], "requires": ["os:android"]}),
            &client
        ));
    }

    // ── capability containment ────────────────────────────────────────────────

    /// Eligibility of a job body against [`iphone`] (client id `ev1_a`), which
    /// has effective capabilities `{os:ios, os_version:26.1, device:iphone17pro,
    /// chip:applea19pro, form_factor:phone, ram_bytes:8000000000,
    /// runtime:llama_cpp}`.
    #[rstest]
    // A subset drawn from both normalized device flags and a reported flag.
    #[case::subset_mixed(json!({"requires": ["os:ios", "device:iphone17pro", "runtime:llama_cpp"]}), true)]
    #[case::subset_single_normalized(json!({"requires": ["chip:applea19pro"]}), true)]
    #[case::exact_ram(json!({"requires": ["ram_bytes:8000000000"]}), true)]
    // Any unmet flag fails the whole requirement (superset containment).
    #[case::missing_flag(json!({"requires": ["os:android"]}), false)]
    #[case::partial_match(json!({"requires": ["os:ios", "runtime:mlx"]}), false)]
    #[case::exact_ram_mismatch(json!({"requires": ["ram_bytes:16000000000"]}), false)]
    // Empty requires matches nobody (zero conditions would match everyone);
    // the explicit list still matches such a job.
    #[case::empty_requires(json!({"requires": []}), false)]
    #[case::empty_requires_but_listed(json!({"clients": ["ev1_a"], "requires": []}), true)]
    // Neither clients nor requires → nobody.
    #[case::absent(json!({}), false)]
    // Malformed requires fails closed: not an array, or a non-string element
    // (which must collapse the whole requirement, not shrink it to an
    // easier-to-satisfy set).
    #[case::malformed_string(json!({"requires": "os:ios"}), false)]
    #[case::malformed_object(json!({"requires": {}}), false)]
    #[case::malformed_null(json!({"requires": null}), false)]
    #[case::malformed_non_string_element(json!({"requires": ["os:ios", 42]}), false)]
    // any_of: each group must share at least one flag with the client.
    #[case::any_of_group_satisfied(json!({"requires": ["os:ios"], "any_of": [["device:iphone17pro", "device:iphone18"]]}), true)]
    #[case::any_of_group_unmet(json!({"requires": ["os:ios"], "any_of": [["device:iphone18", "device:iphone18pro"]]}), false)]
    #[case::any_of_every_group_must_intersect(json!({"requires": ["os:ios"], "any_of": [["device:iphone17pro"], ["runtime:mlx"]]}), false)]
    #[case::any_of_multiple_groups_satisfied(json!({"requires": ["os:ios"], "any_of": [["device:iphone17pro"], ["runtime:llama_cpp"]]}), true)]
    // An empty group list is the default — no constraint; an empty *group* is
    // an at-least-one-of over nothing, unsatisfiable.
    #[case::any_of_no_groups(json!({"requires": ["os:ios"], "any_of": []}), true)]
    #[case::any_of_empty_group(json!({"requires": ["os:ios"], "any_of": [[]]}), false)]
    // any_of only narrows: without a non-empty requires, the capability path
    // matches nobody, however satisfiable the groups.
    #[case::any_of_without_requires(json!({"any_of": [["device:iphone17pro"]]}), false)]
    #[case::any_of_with_empty_requires(json!({"requires": [], "any_of": [["device:iphone17pro"]]}), false)]
    // Malformed any_of fails closed: not an array, a group that is not an
    // array, a non-string member, or null (not an array either).
    #[case::any_of_malformed_string(json!({"requires": ["os:ios"], "any_of": "device:iphone17pro"}), false)]
    #[case::any_of_malformed_flat_group(json!({"requires": ["os:ios"], "any_of": ["device:iphone17pro"]}), false)]
    #[case::any_of_malformed_null_group(json!({"requires": ["os:ios"], "any_of": [null]}), false)]
    #[case::any_of_malformed_non_string_member(json!({"requires": ["os:ios"], "any_of": [["device:iphone17pro", 42]]}), false)]
    #[case::any_of_malformed_null(json!({"requires": ["os:ios"], "any_of": null}), false)]
    // The explicit clients path is independent of any_of — an unmet or
    // malformed clause never blocks a listed client.
    #[case::any_of_unmet_but_listed(json!({"clients": ["ev1_a"], "requires": ["os:ios"], "any_of": [[]]}), true)]
    #[case::any_of_malformed_but_listed(json!({"clients": ["ev1_a"], "any_of": "bogus"}), true)]
    fn job_matches_iphone(#[case] job_body: serde_json::Value, #[case] expected: bool) {
        assert_eq!(job_matches_client(&job_body, &iphone()), expected);
    }

    #[test]
    fn runtime_flags_match_as_whole_strings() {
        // Each flag is compared as a whole, opaque string: a versioned flag and
        // its unversioned prefix are two distinct, independent flags.
        let general = json!({"requires": ["runtime:llama_cpp"]});
        let pinned = json!({"requires": ["runtime:llama_cpp:b9999"]});

        // A client advertising only the general flag does not match a build-pinned
        // job; one advertising only the versioned flag does not match a general job.
        let general_only = client_with("ev1_a", DeviceProfile::default(), &["runtime:llama_cpp"]);
        let versioned_only = client_with(
            "ev1_b",
            DeviceProfile::default(),
            &["runtime:llama_cpp:b9999"],
        );
        assert!(!job_matches_client(&pinned, &general_only));
        assert!(!job_matches_client(&general, &versioned_only));

        // The recommended pattern: a client reports *every level it supports*, so
        // a build-pinned client advertising both flags matches jobs at either
        // granularity.
        let both = client_with(
            "ev1_c",
            DeviceProfile::default(),
            &["runtime:llama_cpp", "runtime:llama_cpp:b9999"],
        );
        assert!(job_matches_client(&general, &both));
        assert!(job_matches_client(&pinned, &both));
    }

    // ── roster classification ──────────────────────────────────────────────────

    /// `clients_only_roster` returns the roster only for a `clients`-only job (a
    /// non-empty `clients` list and no non-empty `requires`); any other shape is
    /// `None`. An empty `requires` still counts as clients-only, since it matches
    /// nobody and the closed roster is then the whole eligible set.
    #[rstest]
    #[case::clients_only(json!({"clients": ["ev1_a", "ev1_b"]}), Some(vec!["ev1_a", "ev1_b"]))]
    #[case::clients_with_requires(json!({"clients": ["ev1_a"], "requires": ["os:ios"]}), None)]
    #[case::clients_with_empty_requires(json!({"clients": ["ev1_a"], "requires": []}), Some(vec!["ev1_a"]))]
    #[case::clients_with_any_of(json!({"clients": ["ev1_a"], "any_of": [["os:ios"]]}), Some(vec!["ev1_a"]))]
    #[case::requires_only(json!({"requires": ["os:ios"]}), None)]
    #[case::empty(json!({}), None)]
    fn clients_only_roster_classification(
        #[case] job_body: serde_json::Value,
        #[case] expected: Option<Vec<&str>>,
    ) {
        assert_eq!(clients_only_roster(&job_body), expected);
    }

    // ── malformed-clause classification ────────────────────────────────────

    /// [`capability_clauses_malformed`] flags shape errors only — the
    /// fail-closed-but-silent cases the eligible-index pass warns about. An
    /// absent `requires` (a `clients`-only job) and an empty `any_of` group
    /// (well-formed, unsatisfiable) are legal shapes, not malformations.
    #[rstest]
    #[case::malformed_requires(json!({"requires": "os:ios"}), true)]
    #[case::malformed_requires_element(json!({"requires": ["os:ios", 42]}), true)]
    #[case::malformed_any_of(json!({"requires": ["os:ios"], "any_of": "bogus"}), true)]
    #[case::malformed_any_of_group(json!({"requires": ["os:ios"], "any_of": [null]}), true)]
    #[case::clients_only_absent_requires(json!({"clients": ["ev1_a"]}), false)]
    #[case::well_formed(json!({"requires": ["os:ios"], "any_of": [["device:iphone17pro"]]}), false)]
    #[case::empty_group_not_malformed(json!({"requires": ["os:ios"], "any_of": [[]]}), false)]
    fn malformed_clause_classification(
        #[case] job_body: serde_json::Value,
        #[case] expected: bool,
    ) {
        assert_eq!(capability_clauses_malformed(&job_body), expected);
    }
}
