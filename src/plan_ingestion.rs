//! Plan ingestion: validate a set of job bodies as a unit, mint identities, and
//! stage them into the `todo/` queue gated by a plan manifest. This is the core
//! function shared by the `plans ingest` CLI and `POST /plans`; both are pure
//! transports over [`ingest_jobs`].
//!
//! Validation (`docs/plan-ingestion.md` §6.2) is whole-set and fail-fast:
//! nothing is written if any job is rejected. Staging (§8) then mints the
//! `plan_id` and a `job_id` per job, writes a `creating` manifest, stages each
//! job (`write_tmp` → `promote_avail`), and flips the manifest to `active` /
//! `pending_clients` from a single fleet-match pass — the same pass that
//! produces the fleet-match warnings.
//!
//! The plan-manifest types are owned by [`crate::plan`]; this module consumes
//! them and adds only the [`IngestReport`] output DTO.

use std::collections::{BTreeSet, HashMap};

use anyhow::{Context, bail};
use chrono::{DateTime, Duration, Utc};
use serde::ser::SerializeMap;
use serde::{Serialize, Serializer};
use serde_json::Value;

use crate::benchmark::Benchmark;
use crate::client::{self, Client, ClientStatus};
use crate::matching::{capability_clauses_malformed, job_matches_client};
use crate::plan::{PlanManifest, PlanStatus, Warning};
use crate::stores::{AuthStore, PlanStore, TodoStore};
use crate::types::{BenchmarkId, ExpiresAt, JobId, PlanId};

/// Jobs with no explicit `expires_at` default to this far past ingestion, so an
/// operator who omits an expiry still gets a bounded queue lifetime rather than
/// a job that lingers in `avail/` forever. Applied by ingestion only; a job
/// written directly by another planner keeps `planner.md`'s never-expires
/// default.
const DEFAULT_EXPIRY_DAYS: i64 = 30;

/// The result of a successful ingest (`docs/plan-ingestion.md` §8), serialized
/// verbatim as the `plans ingest` stdout report and the `POST /plans` `201`
/// body. `jobs` maps each input label — a file name for `plans ingest`, a
/// cardinal index string (`"0"`, `"1"`, …) for `POST /plans` — to its minted
/// `job_id`, in input order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct IngestReport {
    pub plan_id: PlanId,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plan_name: Option<String>,
    pub job_count: usize,
    #[serde(serialize_with = "serialize_jobs_map")]
    pub jobs: Vec<(String, JobId)>,
    pub warnings: Vec<Warning>,
}

/// Serialize the ordered `(label, job_id)` pairs as a JSON object, preserving
/// input order — the serializer writes entries in call order, so cardinal
/// `POST /plans` keys stay `0`, `1`, … rather than sorting lexically.
fn serialize_jobs_map<S: Serializer>(jobs: &[(String, JobId)], s: S) -> Result<S::Ok, S::Error> {
    let mut map = s.serialize_map(Some(jobs.len()))?;
    jobs.iter()
        .try_for_each(|(label, job_id)| map.serialize_entry(label, job_id))?;
    map.end()
}

/// Validate, mint, and stage a set of job bodies as one plan.
///
/// `jobs` is a list of `(label, body)` pairs; the caller owns the labels (file
/// names or cardinal indices). On any §6.2 rejection this returns `Err` with
/// nothing written.
///
/// A failure *during* staging deliberately rolls nothing back: the `creating`
/// manifest and any jobs already promoted are left as they are. Re-running mints
/// fresh ids, so a retry can never collide with the abandoned attempt, and the
/// residue aids debugging. Any job left unpromoted in `tmp/` is reaped by the
/// existing stale-`tmp/` cleanup; tearing down the stuck `creating` manifest and
/// the jobs it did promote belongs to `queue-maintenance` (§8) and is **not yet
/// implemented**, so a plan that fails mid-staging leaves claimable jobs
/// behind.
pub async fn ingest_jobs(
    plans: &dyn PlanStore,
    todo: &dyn TodoStore,
    auth: &dyn AuthStore,
    catalog: &HashMap<BenchmarkId, Benchmark>,
    plan_name: Option<String>,
    jobs: Vec<(String, Value)>,
) -> anyhow::Result<IngestReport> {
    // 1. Validate the whole set before writing anything (§6.2, fail-fast). An
    //    empty set is rejected outright: it would mint a plan that can never
    //    become `active` (no job to match) nor `complete` (no job to reach a
    //    terminal state), so it would linger until retention collected it.
    if jobs.is_empty() {
        bail!("plan declares no jobs");
    }
    jobs.iter().try_for_each(|(label, body)| {
        validate_job(body, catalog).with_context(|| format!("rejected job {label:?}"))
    })?;

    let now = Utc::now();
    let default_expiry = ExpiresAt::At(now + Duration::days(DEFAULT_EXPIRY_DAYS));

    // 2. Mint plan_id + a job_id per job (input order — arrival-ordered `avail/`
    //    keys), stamp each job_id and the resolved expiry into its body.
    let plan_id = PlanId::from_uuid(uuid::Uuid::now_v7());
    let staged: Vec<Staged> = jobs
        .into_iter()
        .map(|(label, mut body)| {
            let job_id = JobId::from_uuid(uuid::Uuid::now_v7());
            let expires_at = resolve_expires_at(&body, default_expiry);
            if let Some(obj) = body.as_object_mut() {
                obj.insert(
                    "job_id".to_string(),
                    Value::String(job_id.as_str().to_string()),
                );
                // Stamp the *resolved* expiry back in canonical [`ExpiresAt`]
                // form, so the body and the `avail/` filename derive from one
                // value and cannot diverge. Load-bearing for lease recycling:
                // `recycle_lease` rebuilds the `avail/` key from the **body's**
                // `expires_at`, absent meaning `never`. Without this, a job
                // ingested with no expiry would come back from a lapsed lease as
                // `never` (losing the bounded lifetime `DEFAULT_EXPIRY_DAYS`
                // exists to guarantee), and one ingested with an RFC 3339 expiry
                // would fail that parse and strand in `leased/`.
                obj.insert(
                    "expires_at".to_string(),
                    Value::String(expires_at.to_string()),
                );
            }
            Staged {
                label,
                job_id,
                body,
                expires_at,
            }
        })
        .collect();
    let job_ids: Vec<JobId> = staged.iter().map(|s| s.job_id.clone()).collect();

    // 3. Write the `creating` manifest — the authoritative job-id list, before
    //    anything is claimable.
    let creating = PlanManifest {
        plan_id: plan_id.clone(),
        plan_name: plan_name.clone(),
        status: PlanStatus::Creating,
        created_at: now,
        job_ids: job_ids.clone(),
        warnings: Vec::new(),
        progress_snapshot: None,
        terminal_at: None,
    };
    plans
        .put_plan(&creating)
        .await
        .with_context(|| format!("writing creating manifest for {plan_id}"))?;

    // 4. Stage each job: write_tmp then atomic promote into avail/.
    for s in &staged {
        todo.write_tmp(&s.job_id, &s.body)
            .await
            .with_context(|| format!("staging job {} for {plan_id}", s.job_id))?;
        todo.promote_avail(&s.job_id, s.expires_at)
            .await
            .with_context(|| format!("promoting job {} for {plan_id}", s.job_id))?;
    }

    // 5. One fleet-match pass against the approved roster drives both the
    //    warnings and the active/pending_clients decision (§6.2, §8).
    let clients = auth
        .list_clients()
        .await
        .context("listing clients for fleet match")?;
    let approved: Vec<&Client> = clients
        .iter()
        .filter(|c| c.status == ClientStatus::Approved)
        .collect();
    let unmatched: Vec<&Staged> = staged
        .iter()
        .filter(|s| !approved.iter().any(|c| job_matches_client(&s.body, c)))
        .collect();
    let status = if unmatched.len() == staged.len() {
        PlanStatus::PendingClients
    } else {
        PlanStatus::Active
    };
    let warnings = group_warnings(&unmatched);

    // 6. Finalize: the manifest carries the frozen ingestion-time warnings and
    //    the resolved status.
    let final_manifest = PlanManifest {
        status,
        warnings: warnings.clone(),
        ..creating
    };
    plans
        .put_plan(&final_manifest)
        .await
        .with_context(|| format!("finalizing manifest for {plan_id}"))?;

    tracing::info!(
        plan_id = %plan_id,
        job_count = staged.len(),
        status = %status.label(),
        "plan created",
    );
    if !warnings.is_empty() {
        tracing::info!(
            plan_id = %plan_id,
            warning_groups = warnings.len(),
            "plan ingested with fleet-match warnings",
        );
    }

    Ok(IngestReport {
        plan_id,
        plan_name,
        job_count: staged.len(),
        jobs: staged.into_iter().map(|s| (s.label, s.job_id)).collect(),
        warnings,
    })
}

/// A job carried between minting (step 2) and the report (step 6): its input
/// label, minted id, `job_id`-stamped body, and resolved expiry.
struct Staged {
    label: String,
    job_id: JobId,
    body: Value,
    expires_at: ExpiresAt,
}

/// §6.2 validation of a single job body. Whole-set fail-fast is the caller's
/// concern; this rejects one job.
fn validate_job(body: &Value, catalog: &HashMap<BenchmarkId, Benchmark>) -> anyhow::Result<()> {
    let obj = body.as_object().context("body is not a JSON object")?;

    // Identity is server-assigned; a pre-set id is a malformed handoff.
    if obj.contains_key("job_id") {
        bail!("carries a job_id (server-assigned at ingestion)");
    }
    if obj.contains_key("plan_id") {
        bail!("carries a plan_id (server-assigned at ingestion)");
    }

    let has_requires = non_empty_str_array(obj.get("requires"));
    let has_clients = non_empty_str_array(obj.get("clients"));
    if !has_requires && !has_clients {
        bail!("declares neither a non-empty `requires` nor a non-empty `clients`");
    }

    // `requires` / `any_of` shapes (reuses the matcher's shape classifier, but
    // escalates malformed-at-ingest to a rejection rather than fail-closed).
    if capability_clauses_malformed(body) {
        bail!("`requires`/`any_of` is malformed (each must be an array of string flags)");
    }
    if let Some(clients) = obj.get("clients") {
        let arr = clients.as_array().context("`clients` must be an array")?;
        if arr.iter().any(|v| !v.is_string()) {
            bail!("`clients` must contain only strings");
        }
    }

    // Every capability flag must already be canonical (lowercase, no
    // whitespace); the server validates but never rewrites a plan's flags.
    if let Some(flag) = capability_flags(body).find(|f| *f != client::slugify(f)) {
        bail!("capability flag {flag:?} must be canonical (lowercase, no whitespace)");
    }

    // At most one flag per reserved namespace in the flat `requires` set.
    // `any_of` groups are deliberately many flags from one namespace, so they
    // are exempt.
    let mut seen = BTreeSet::new();
    for flag in requires_flags(body) {
        if let Some(ns) = reserved_namespace(flag)
            && !seen.insert(ns)
        {
            bail!(
                "more than one `{ns}:` flag in `requires` (reserved namespaces allow at most one)"
            );
        }
    }

    match obj.get("expires_at") {
        None | Some(Value::Null) => {}
        Some(Value::String(s)) => {
            DateTime::parse_from_rfc3339(s)
                .with_context(|| format!("malformed `expires_at` {s:?} (expected RFC 3339)"))?;
        }
        Some(_) => bail!("`expires_at` must be an RFC 3339 string"),
    }

    // The run specification. Opaque but for `benchmark` (below): the server
    // stores and forwards it without understanding a cell, which is what lets
    // the plan schema evolve in `pipette-clients` without a release here
    // (`docs/plan-ingestion.md` §1). Requiring it also rejects a body from a
    // writer predating the envelope split, which would otherwise ingest cleanly
    // and then be handed out as a claim no client can run.
    let spec = obj
        .get("spec")
        .context("missing required field `spec`")?
        .as_object()
        .context("`spec` is not a JSON object")?;

    // `spec.benchmark` resolvable against the catalog — the one spec field the
    // server reads, to attribute synthetic failures (§9) and to fill the claim
    // envelope's `benchmark_id`.
    let benchmark_id = spec
        .get("benchmark")
        .and_then(Value::as_str)
        .context("`spec` is missing required field `benchmark`")?;
    let benchmark_id = BenchmarkId::try_new(benchmark_id).context("invalid `spec.benchmark`")?;
    if !catalog.contains_key(&benchmark_id) {
        bail!("spec.benchmark {benchmark_id} not in the benchmark catalog");
    }

    // `model` and `runtime` are present but unread — a presence check, not a
    // schema check. They complete the three fields a run specification cannot
    // omit, so a truncated spec is refused at ingestion instead of leasing out a
    // job whose only possible outcome is a terminal client-side rejection.
    ["model", "runtime"]
        .into_iter()
        .try_for_each(|field| match spec.get(field) {
            Some(v) if !v.is_null() => Ok(()),
            _ => bail!("`spec` is missing required field `{field}`"),
        })
}

/// The reserved namespace a canonical flag belongs to (the token before the
/// first `:`), or `None` for a free-form flag. Assumes `flag` is canonical,
/// which validation checks first.
fn reserved_namespace(flag: &str) -> Option<&str> {
    if client::is_reserved_capability(flag) {
        flag.split_once(':').map(|(ns, _)| ns)
    } else {
        None
    }
}

fn non_empty_str_array(value: Option<&Value>) -> bool {
    value
        .and_then(Value::as_array)
        .is_some_and(|a| !a.is_empty())
}

/// The flat `requires` flags of a job body, empty when absent/malformed (shape
/// is validated separately).
fn requires_flags(body: &Value) -> impl Iterator<Item = &str> {
    body.get("requires")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
}

/// Every capability flag in a job body — the flat `requires` plus every
/// `any_of` group member. `clients` entries are ids, not flags, and are
/// excluded.
fn capability_flags(body: &Value) -> impl Iterator<Item = &str> {
    let any_of = body
        .get("any_of")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_array)
        .flatten()
        .filter_map(Value::as_str);
    requires_flags(body).chain(any_of)
}

/// Resolve a job's expiry: an RFC 3339 `expires_at` when present and parseable
/// (validation has already guaranteed this), else the ingestion default.
///
/// RFC 3339 is the *handoff* format `pipette-plan` writes
/// (`docs/plan-ingestion.md` §4); [`ExpiresAt`]'s compact `Display` is the form
/// job bodies and queue filenames carry internally. Ingestion is the boundary
/// that converts between them — see the stamping step in [`ingest_jobs`].
fn resolve_expires_at(body: &Value, default: ExpiresAt) -> ExpiresAt {
    body.get("expires_at")
        .and_then(Value::as_str)
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|dt| ExpiresAt::At(dt.with_timezone(&Utc)))
        .unwrap_or(default)
}

/// The requirement identity a fleet-match warning groups on: the flat
/// `requires`, the `any_of` groups, and the explicit `clients`. Normalized to
/// sets so two jobs with the same requirement in a different textual order
/// group together.
#[derive(PartialEq, Eq)]
struct RequirementKey {
    requires: BTreeSet<String>,
    any_of: BTreeSet<BTreeSet<String>>,
    clients: BTreeSet<String>,
}

fn requirement_key(body: &Value) -> RequirementKey {
    let requires = requires_flags(body).map(str::to_string).collect();
    let any_of = body
        .get("any_of")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_array)
        .map(|group| {
            group
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .collect();
    let clients = body
        .get("clients")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_string)
        .collect();
    RequirementKey {
        requires,
        any_of,
        clients,
    }
}

/// Group the unmatched jobs by identical requirement set into per-group
/// [`Warning`]s, in first-appearance order.
fn group_warnings(unmatched: &[&Staged]) -> Vec<Warning> {
    let mut groups: Vec<(RequirementKey, Vec<JobId>)> = Vec::new();
    for s in unmatched {
        let key = requirement_key(&s.body);
        match groups.iter_mut().find(|(k, _)| *k == key) {
            Some((_, ids)) => ids.push(s.job_id.clone()),
            None => groups.push((key, vec![s.job_id.clone()])),
        }
    }
    groups
        .into_iter()
        .map(|(key, job_ids)| Warning {
            message: warning_message(job_ids.len(), &key),
            job_ids,
        })
        .collect()
}

fn warning_message(count: usize, key: &RequirementKey) -> String {
    let (noun, verb) = if count == 1 {
        ("job", "matches")
    } else {
        ("jobs", "match")
    };
    let mut clauses = Vec::new();
    if !key.requires.is_empty() {
        clauses.push(format!("requiring {}", join(&key.requires)));
    }
    if !key.any_of.is_empty() {
        let groups: Vec<String> = key
            .any_of
            .iter()
            .map(|g| format!("[{}]", join(g)))
            .collect();
        clauses.push(format!("with any-of {}", groups.join(" & ")));
    }
    if !key.clients.is_empty() {
        clauses.push(format!("pinned to {}", join(&key.clients)));
    }
    let desc = if clauses.is_empty() {
        "with no eligibility".to_string()
    } else {
        clauses.join(" ")
    };
    format!("{count} {noun} {desc} {verb} no registered, approved client")
}

fn join(flags: &BTreeSet<String>) -> String {
    flags.iter().cloned().collect::<Vec<_>>().join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::{Client, ClientStatus, DeviceProfile};
    use crate::config::{Config, StorageConfig};
    use crate::stores::{Stores, build_local_fs_stores};
    use crate::types::ClientId;
    use crate::validated::{ContactEmail, NonEmptyTrimmedString, PublicKeyHex};
    use rstest::rstest;
    use serde_json::json;

    /// The minimum run specification validation accepts: `benchmark` resolvable
    /// against [`catalog`], plus `model` and `runtime` present. Their contents are
    /// opaque to the server, so the fixtures leave them empty rather than imply
    /// this layer inspects them.
    fn spec_bench_1() -> Value {
        json!({"benchmark": "bench-1", "model": {}, "runtime": {}})
    }

    /// A one-benchmark catalog keyed by `bench-1`; validation only ever asks it
    /// `contains_key`.
    fn catalog() -> anyhow::Result<HashMap<BenchmarkId, Benchmark>> {
        let def = "benchmark_type = \"prefill_throughput\"\nparameter_prefill_tokens = 100";
        let bench = Benchmark::from_toml("bench-1", def)?;
        Ok(HashMap::from([(BenchmarkId::try_new("bench-1")?, bench)]))
    }

    // ── §6.2 validation: rejections ─────────────────────────────────────────

    #[rstest]
    #[case::not_object(json!("nope"))]
    #[case::carries_job_id(json!({"requires":["os:macos"],"spec":spec_bench_1(),"job_id":"job-x"}))]
    #[case::carries_plan_id(json!({"requires":["os:macos"],"spec":spec_bench_1(),"plan_id":"plan-x"}))]
    #[case::no_eligibility(json!({"spec":spec_bench_1()}))]
    #[case::empty_requires_and_clients(json!({"requires":[],"clients":[],"spec":spec_bench_1()}))]
    #[case::malformed_requires(json!({"requires":"os:macos","spec":spec_bench_1()}))]
    #[case::non_string_requires(json!({"requires":["os:macos",42],"spec":spec_bench_1()}))]
    #[case::malformed_any_of(json!({"requires":["os:macos"],"any_of":"x","spec":spec_bench_1()}))]
    #[case::non_canonical_flag(json!({"requires":["OS:macos"],"spec":spec_bench_1()}))]
    // The canonical check covers `any_of` members too, not just flat `requires`.
    #[case::non_canonical_any_of_flag(json!({"requires":["os:ios"],"any_of":[["OS:macos"]],"spec":spec_bench_1()}))]
    #[case::whitespace_flag(json!({"requires":["os:mac os"],"spec":spec_bench_1()}))]
    #[case::reserved_namespace_dup(json!({"requires":["os:ios","os:android"],"spec":spec_bench_1()}))]
    #[case::malformed_expires_at(json!({"requires":["os:macos"],"spec":spec_bench_1(),"expires_at":"soon"}))]
    #[case::non_string_clients(json!({"clients":["ev1_a",7],"spec":spec_bench_1()}))]
    #[case::missing_spec(json!({"requires":["os:macos"]}))]
    #[case::spec_not_object(json!({"requires":["os:macos"],"spec":"eval_test"}))]
    #[case::missing_benchmark(json!({"requires":["os:macos"],"spec":{"model":{},"runtime":{}}}))]
    #[case::unknown_benchmark(json!({"requires":["os:macos"],"spec":{"benchmark":"nope","model":{},"runtime":{}}}))]
    #[case::missing_model(json!({"requires":["os:macos"],"spec":{"benchmark":"bench-1","runtime":{}}}))]
    #[case::missing_runtime(json!({"requires":["os:macos"],"spec":{"benchmark":"bench-1","model":{}}}))]
    #[case::null_model(json!({"requires":["os:macos"],"spec":{"benchmark":"bench-1","model":null,"runtime":{}}}))]
    #[case::null_runtime(json!({"requires":["os:macos"],"spec":{"benchmark":"bench-1","model":{},"runtime":null}}))]
    // A body from a writer predating the envelope split: the spec content is
    // flat, so there is no `spec` to lease out and the job would be unrunnable.
    #[case::pre_envelope_flat_body(json!({"requires":["os:macos"],"benchmark_id":"bench-1","model_descriptor":"{}"}))]
    fn validate_job_rejects(#[case] body: Value) -> anyhow::Result<()> {
        assert!(validate_job(&body, &catalog()?).is_err());
        Ok(())
    }

    // ── §6.2 validation: acceptances ────────────────────────────────────────

    #[rstest]
    #[case::requires_only(json!({"requires":["os:macos"],"spec":spec_bench_1()}))]
    #[case::clients_only(json!({"clients":["ev1_a"],"spec":spec_bench_1()}))]
    // Reserved-namespace uniqueness is scoped to the flat `requires`; an `any_of`
    // group is deliberately many flags from one namespace.
    #[case::any_of_same_namespace(json!({"requires":["os:ios"],"any_of":[["os:ios","os:android"]],"spec":spec_bench_1()}))]
    // Free-form (non-reserved) flags may share a prefix.
    #[case::free_form_shared_prefix(json!({"requires":["runtime:llama_cpp","runtime:llama_cpp:b1"],"spec":spec_bench_1()}))]
    #[case::valid_expires_at(json!({"requires":["os:macos"],"spec":spec_bench_1(),"expires_at":"2026-08-01T00:00:00Z"}))]
    fn validate_job_accepts(#[case] body: Value) -> anyhow::Result<()> {
        validate_job(&body, &catalog()?)?;
        Ok(())
    }

    // ── warning grouping & message text ─────────────────────────────────────

    fn staged(job_id: &str, body: Value) -> Staged {
        Staged {
            label: format!("{job_id}.json"),
            job_id: JobId::new_unchecked(job_id),
            body,
            expires_at: ExpiresAt::Never,
        }
    }

    /// Unmatched jobs group by identical requirement set — regardless of the
    /// order the flags were written in — keep first-appearance order, name their
    /// `job_ids`, and agree verb with count.
    #[test]
    fn warnings_group_by_requirement_set() -> anyhow::Result<()> {
        let a = staged("job-a", json!({"requires": ["os:ios", "runtime:mlx"]}));
        // Same requirement *set*, different textual order — must join a's group.
        let b = staged("job-b", json!({"requires": ["runtime:mlx", "os:ios"]}));
        let c = staged("job-c", json!({"clients": ["ev1_x"]}));
        let d = staged(
            "job-d",
            json!({"requires": ["os:ios"], "any_of": [["device:a", "device:b"]]}),
        );
        let warnings = group_warnings(&[&a, &b, &c, &d]);

        assert_eq!(warnings.len(), 3, "a+b share a group; c and d are distinct");
        assert_eq!(
            warnings[0].job_ids,
            vec![a.job_id.clone(), b.job_id.clone()]
        );
        assert_eq!(
            warnings[0].message,
            "2 jobs requiring os:ios, runtime:mlx match no registered, approved client"
        );
        // Singular agrees in number ("1 job … matches", not "1 job … match").
        assert_eq!(warnings[1].job_ids, vec![c.job_id.clone()]);
        assert_eq!(
            warnings[1].message,
            "1 job pinned to ev1_x matches no registered, approved client"
        );
        assert_eq!(
            warnings[2].message,
            "1 job requiring os:ios with any-of [device:a, device:b] \
             matches no registered, approved client"
        );
        Ok(())
    }

    // ── report serialization ────────────────────────────────────────────────

    #[test]
    fn report_serializes_jobs_as_ordered_object_and_omits_absent_name() -> anyhow::Result<()> {
        let report = IngestReport {
            plan_id: PlanId::from_uuid(uuid::Uuid::nil()),
            plan_name: None,
            job_count: 2,
            // Labels chosen so input order and lexical order *disagree*
            // (`"10" < "2"` as strings): a regression to a key-sorting map would
            // emit `job-b` first and fail the assertion below.
            jobs: vec![
                ("2".to_string(), JobId::new_unchecked("job-a")),
                ("10".to_string(), JobId::new_unchecked("job-b")),
            ],
            warnings: Vec::new(),
        };
        let text = serde_json::to_string(&report)?;
        assert!(!text.contains("plan_name"), "None plan_name is omitted");
        let a = text.find("job-a").context("job-a present")?;
        let b = text.find("job-b").context("job-b present")?;
        assert!(
            a < b,
            "cardinal keys preserve input order, not lexical order"
        );
        Ok(())
    }

    // ── end-to-end on the local-fs backend ──────────────────────────────────

    fn stores_in(dir: &std::path::Path) -> anyhow::Result<Stores> {
        let config = Config {
            storage: StorageConfig::local_fs(dir.to_path_buf()),
            auth_storage: StorageConfig::local_fs(dir.to_path_buf()),
            ..Config::default()
        };
        build_local_fs_stores(&config)
    }

    fn client(id: &str, os: &str, status: ClientStatus) -> anyhow::Result<Client> {
        Ok(Client {
            client_id: ClientId::try_new(id)?,
            public_key: PublicKeyHex::try_new(
                "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789",
            )?,
            organization: NonEmptyTrimmedString::try_new("org")?,
            client_details: NonEmptyTrimmedString::try_new("d")?,
            contact_email: ContactEmail::try_new("a@b.com")?,
            status,
            registered_at: Utc::now(),
            device_profile: DeviceProfile {
                device_os_name: Some(NonEmptyTrimmedString::try_new(os)?),
                ..Default::default()
            },
            capabilities: Default::default(),
        })
    }

    async fn ingest(
        stores: &Stores,
        catalog: &HashMap<BenchmarkId, Benchmark>,
        plan_name: Option<String>,
        jobs: Vec<(String, Value)>,
    ) -> anyhow::Result<IngestReport> {
        ingest_jobs(
            stores.plans.as_ref(),
            stores.todo.as_ref(),
            stores.auth.as_ref(),
            catalog,
            plan_name,
            jobs,
        )
        .await
    }

    #[tokio::test]
    async fn ingest_stages_jobs_active_with_grouped_warning() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let stores = stores_in(dir.path())?;
        stores
            .auth
            .put_client(&client("ev1_a", "macOS", ClientStatus::Approved)?)
            .await?;

        let jobs = vec![
            (
                "match.json".to_string(),
                json!({"requires":["os:macos"],"spec":spec_bench_1()}),
            ),
            (
                "starve.json".to_string(),
                json!({"requires":["os:ios"],"spec":spec_bench_1()}),
            ),
        ];
        let report = ingest(&stores, &catalog()?, Some("smoke".to_string()), jobs).await?;

        assert_eq!(report.job_count, 2);
        assert!(report.plan_id.as_str().starts_with("plan-"));
        assert_eq!(report.jobs[0].0, "match.json");
        // Only the os:ios job is starved, and the warning names exactly its id.
        assert_eq!(report.warnings.len(), 1);
        assert_eq!(report.warnings[0].job_ids, vec![report.jobs[1].1.clone()]);

        // Both jobs are in avail/, each stamped with its minted job_id.
        for (_, job_id) in &report.jobs {
            let body = stores
                .todo
                .get_avail_by_job(job_id)
                .await?
                .context("job promoted to avail/")?;
            assert_eq!(
                body.get("job_id").and_then(Value::as_str),
                Some(job_id.as_str())
            );
        }

        // Manifest finalized to active, carrying the full id list and the frozen
        // warning.
        let manifest = stores
            .plans
            .get_plan(&report.plan_id)
            .await?
            .context("manifest written")?;
        assert_eq!(manifest.status, PlanStatus::Active);
        assert_eq!(manifest.job_ids.len(), 2);
        assert_eq!(manifest.warnings.len(), 1);
        assert_eq!(manifest.plan_name.as_deref(), Some("smoke"));
        Ok(())
    }

    /// Ingestion stamps the resolved expiry into the body, so a lapsed lease
    /// recycles back to the *same* `avail/` key. `recycle_lease` rebuilds that
    /// key from the body's `expires_at` (absent → `never`, and parsed in
    /// [`ExpiresAt`]'s compact form), so an unstamped body would either lose the
    /// 30-day default or fail the parse and strand the job in `leased/`.
    #[rstest]
    // No expires_at in the handoff: the 30-day default must survive the round trip.
    #[case::default_expiry(None)]
    // An explicit RFC 3339 expiry (the handoff format) must survive it too.
    #[case::explicit_expiry(Some("2026-08-01T00:00:00Z"))]
    #[tokio::test]
    async fn ingest_stamps_expiry_so_lease_recycle_preserves_it(
        #[case] expires_at: Option<&str>,
    ) -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let stores = stores_in(dir.path())?;
        let mut body = json!({"requires":["os:macos"],"spec":spec_bench_1()});
        if let Some(ts) = expires_at {
            body["expires_at"] = json!(ts);
        }
        let report = ingest(
            &stores,
            &catalog()?,
            None,
            vec![("j.json".to_string(), body)],
        )
        .await?;
        let job_id = report.jobs[0].1.clone();

        // The staged expiry, read off the avail/ key.
        let staged_key = stores
            .todo
            .list_avail(None, crate::TEST_LIST_LIMIT)
            .await?
            .pop()
            .context("job in avail/")?;
        let (_, staged_expiry) = crate::todo_filename::parse_avail_filename(&staged_key)?;
        if let Some(ts) = expires_at {
            let want = ExpiresAt::At(DateTime::parse_from_rfc3339(ts)?.with_timezone(&Utc));
            assert_eq!(staged_expiry, want, "explicit expiry is honored");
        }
        assert_ne!(
            staged_expiry,
            ExpiresAt::Never,
            "ingestion never stages an unbounded job"
        );

        // Claim it, then recycle the lease as queue-maintenance would.
        let client_id = ClientId::try_new("ev1_a")?;
        let lease_expiry = Utc::now();
        assert!(matches!(
            stores
                .todo
                .claim_job(&job_id, staged_expiry, &client_id, lease_expiry)
                .await?,
            crate::stores::ClaimResult::Claimed(_)
        ));
        assert_eq!(
            stores
                .todo
                .recycle_lease(&job_id, &client_id, lease_expiry)
                .await?,
            crate::stores::RecycleResult::Recycled
        );

        // Back in avail/ under the identical key — expiry neither lost nor
        // downgraded to `never`.
        let recycled_key = stores
            .todo
            .list_avail(None, crate::TEST_LIST_LIMIT)
            .await?
            .pop()
            .context("job recycled to avail/")?;
        assert_eq!(recycled_key, staged_key);
        Ok(())
    }

    /// A plan whose every job is unmatched finalizes as `pending_clients` with a
    /// warning — whether nothing in the fleet has the capability, or the only
    /// capable client isn't **approved** (§6.2 "registered, approved client").
    #[rstest]
    #[case::capability_mismatch(ClientStatus::Approved, "macOS", "os:ios")]
    #[case::client_not_approved(ClientStatus::Pending, "macOS", "os:macos")]
    #[tokio::test]
    async fn ingest_pending_clients_when_nothing_matches(
        #[case] status: ClientStatus,
        #[case] client_os: &str,
        #[case] requires: &str,
    ) -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let stores = stores_in(dir.path())?;
        stores
            .auth
            .put_client(&client("ev1_a", client_os, status)?)
            .await?;

        let report = ingest(
            &stores,
            &catalog()?,
            None,
            vec![(
                "only.json".to_string(),
                json!({"requires":[requires],"spec":spec_bench_1()}),
            )],
        )
        .await?;

        let manifest = stores
            .plans
            .get_plan(&report.plan_id)
            .await?
            .context("manifest")?;
        assert_eq!(manifest.status, PlanStatus::PendingClients);
        assert_eq!(report.warnings.len(), 1);
        Ok(())
    }

    /// An empty job set is rejected rather than minting a plan that could never
    /// activate or complete.
    #[tokio::test]
    async fn empty_job_set_is_rejected() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let stores = stores_in(dir.path())?;
        assert!(ingest(&stores, &catalog()?, None, vec![]).await.is_err());
        assert!(stores.plans.list_plans(None).await?.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn rejected_set_writes_nothing() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let stores = stores_in(dir.path())?;
        // The first job is valid; the second violates reserved-namespace
        // uniqueness — the whole set must be rejected with nothing written.
        let jobs = vec![
            (
                "ok.json".to_string(),
                json!({"requires":["os:macos"],"spec":spec_bench_1()}),
            ),
            (
                "bad.json".to_string(),
                json!({"requires":["os:ios","os:android"],"spec":spec_bench_1()}),
            ),
        ];
        assert!(ingest(&stores, &catalog()?, None, jobs).await.is_err());

        assert!(stores.plans.list_plans(None).await?.is_empty());
        assert!(
            stores
                .todo
                .list_avail(None, crate::TEST_LIST_LIMIT)
                .await?
                .is_empty()
        );
        Ok(())
    }
}
