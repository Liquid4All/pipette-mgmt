//! Operator-facing plan administration: the logic behind the `pipette-mgmt
//! plans` subcommand group (`docs/plan-ingestion.md` §11).
//!
//! Lives in the library rather than the binary because `POST /plans` and
//! `GET /plans/{plan_id}` need the same logic and cannot import from a binary
//! target, and because the store-backed tests belong beside the other library
//! tests. The clap layer is left a thin shim that parses arguments and renders
//! what these functions return: presentation (tables) is the binary's job, and
//! this module returns data.
//!
//! Ingestion itself belongs to [`crate::plan_ingestion`], which is transport
//! neutral so `POST /plans` can reuse it. What is CLI-specific, and lives here,
//! is the *directory* half of the handoff contract (§7) and the plan-lookup
//! conveniences an operator needs at a terminal.

use std::cmp::Reverse;
use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, anyhow, bail};
use serde_json::Value;

use crate::benchmark::Benchmark;
use crate::plan::{PlanManifest, PlanStatus, PlanStatusView};
use crate::plan_ingestion::{IngestReport, ingest_jobs};
use crate::stores::{PlanStore, Stores};
use crate::types::{BenchmarkId, PlanId};

/// How an operator named a plan on the command line.
///
/// Two variants rather than one string that gets sniffed: a plan name is
/// free-form, so guessing whether `foo` is an id or a name would be ambiguous in
/// exactly the cases that matter. The flag the operator typed says which they
/// meant.
#[derive(Debug, Clone)]
pub enum PlanRef {
    /// The `plan_id` — preferred, and a single addressed read.
    Id(PlanId),
    /// A `--plan-name` label, resolved by scanning manifests.
    Name(String),
}

/// Resolve a [`PlanRef`] to a concrete `plan_id`.
///
/// An id passes straight through — resolving it would cost a read the callers
/// make anyway. A name is resolved by listing every manifest and matching
/// exactly, which is why `plan_id` is the preferred form.
///
/// Ambiguity is an **error, never a guess**: a plan name is explicitly not an
/// identity (`docs/plan-ingestion.md` §2), so nothing entitles this to pick the
/// newest match. The error names every candidate so the operator can re-run with
/// the id.
pub async fn resolve_plan_ref(plans: &dyn PlanStore, plan_ref: &PlanRef) -> anyhow::Result<PlanId> {
    let name = match plan_ref {
        PlanRef::Id(id) => return Ok(id.clone()),
        PlanRef::Name(name) => name.trim(),
    };
    if name.is_empty() {
        bail!("--plan-name must not be empty");
    }

    let mut matches: Vec<PlanManifest> = plans
        .list_plans(None)
        .await
        .context("listing plans to resolve --plan-name")?
        .into_iter()
        .filter(|p| p.plan_name.as_deref().map(str::trim) == Some(name))
        .collect();

    match matches.len() {
        0 => bail!("no plan named {name:?}"),
        1 => Ok(matches.remove(0).plan_id),
        n => {
            // Newest first, so the list reads in the order an operator thinks
            // about their plans even though the choice stays theirs.
            matches.sort_by_key(|p| Reverse(p.created_at));
            let candidates = matches
                .iter()
                .map(|p| {
                    format!(
                        "\n  {}  {}  created {}",
                        p.plan_id,
                        p.status.label(),
                        p.created_at.to_rfc3339()
                    )
                })
                .collect::<String>();
            bail!(
                "{n} plans are named {name:?} — a plan name is a label, not an identity. \
                 Re-run with one of these plan ids:{candidates}"
            )
        }
    }
}

/// Read a job-file directory per the handoff contract (`docs/plan-ingestion.md`
/// §7), returning `(file name, body)` pairs ready for [`ingest_jobs`].
///
/// Every `*.json` file is a job body and other files are ignored, so a directory
/// carrying a README or a generator log ingests cleanly. The scan is **flat** —
/// §7 says "every `*.json` file in the directory", and recursing would silently
/// pull in whatever an operator happened to nest there.
///
/// Ordering is by file name, not `read_dir` order, which is arbitrary. Ingestion
/// mints `job_id`s in input order and the `eligible/` cursor depends on `avail/`
/// keys being arrival-ordered (§8), so leaving the order to the filesystem would
/// make the same directory ingest differently run to run.
///
/// The directory is treated as read-only: nothing here writes, renames, or
/// deletes. Synchronous because this is a one-shot CLI read, not server-path IO.
pub fn read_job_dir(dir: &Path) -> anyhow::Result<Vec<(String, Value)>> {
    let entries = std::fs::read_dir(dir)
        .with_context(|| format!("reading job directory {}", dir.display()))?;

    let mut files: Vec<std::path::PathBuf> = Vec::new();
    for entry in entries {
        let path = entry
            .with_context(|| format!("reading an entry of {}", dir.display()))?
            .path();
        // `is_file()` also skips subdirectories and, on a symlink, follows it —
        // a symlinked job file is a legitimate way to assemble a directory.
        // Extension match is exact and lowercase: `.JSON` is not the contract.
        if path.is_file() && path.extension().is_some_and(|ext| ext == "json") {
            files.push(path);
        }
    }
    files.sort();

    if files.is_empty() {
        bail!(
            "no *.json job files in {} — `pipette-plan generate --out <dir>` writes them",
            dir.display()
        );
    }

    files
        .iter()
        .map(|path| {
            // File names are the report's keys (§7), so a non-UTF-8 name has no
            // usable label; reject rather than lossily rename it.
            let label = path
                .file_name()
                .and_then(|n| n.to_str())
                .with_context(|| format!("job file name is not valid UTF-8: {}", path.display()))?
                .to_string();
            let bytes = std::fs::read(path)
                .with_context(|| format!("reading job file {}", path.display()))?;
            // A malformed file fails the whole ingest, matching §6.2's
            // whole-set fail-fast: a partially-ingested plan is worse than none.
            let body = serde_json::from_slice(&bytes)
                .with_context(|| format!("job file {label} is not valid JSON"))?;
            Ok((label, body))
        })
        .collect()
}

/// `plans ingest` — read a job-file directory and run it through the ingestion
/// pipeline as one plan.
///
/// Validates the `todo/` backend up front, as `queue-maintenance` does: staging
/// depends on atomic `tmp/` → `avail/` renames, and finding that out midway
/// would leave a half-staged plan behind. Takes no mutate lock — this writes only
/// `plans/` and `todo/`, disjoint from the warehouse paths that lock serializes.
pub async fn ingest_dir(
    stores: &Stores,
    catalog: &HashMap<BenchmarkId, Benchmark>,
    dir: &Path,
    plan_name: Option<String>,
) -> anyhow::Result<IngestReport> {
    let plan_name = plan_name
        .map(|name| {
            let trimmed = name.trim().to_string();
            if trimmed.is_empty() {
                bail!("--plan-name must not be empty");
            }
            Ok(trimmed)
        })
        .transpose()?;

    let jobs = read_job_dir(dir)?;
    stores.todo.validate_backend().await?;

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

/// `plans list` — every plan, newest first.
///
/// [`PlanStore::list_plans`] returns implementation-defined order, so the sort
/// happens here rather than leaving the display at the mercy of the backend's
/// listing.
pub async fn list_plans(
    plans: &dyn PlanStore,
    status: Option<PlanStatus>,
) -> anyhow::Result<Vec<PlanManifest>> {
    let mut manifests = plans.list_plans(status).await.context("listing plans")?;
    manifests.sort_by_key(|p| Reverse(p.created_at));
    Ok(manifests)
}

/// `plans status` — the client-facing projection of one plan's manifest.
///
/// A single manifest read plus a marker check; the progress numbers are whatever
/// `queue-maintenance` last computed, never recomputed here (§9 makes that pass
/// their sole writer).
pub async fn plan_status(
    plans: &dyn PlanStore,
    plan_ref: &PlanRef,
) -> anyhow::Result<PlanStatusView> {
    let plan_id = resolve_plan_ref(plans, plan_ref).await?;
    let manifest = plans
        .get_plan(&plan_id)
        .await
        .with_context(|| format!("reading manifest for {plan_id}"))?
        // `ok_or_else`, not another `context`: the line above converts a storage
        // failure, this one an absent manifest. Both are `anyhow::Error` today,
        // but they are different answers ("the store broke" vs "no such plan")
        // and reading them as one operation obscures that — the distinction
        // `GET /plans/{plan_id}` will need to map to 500 vs 404.
        .ok_or_else(|| anyhow!("no plan {plan_id}"))?;
    let cancel_requested = plans
        .has_cancel_marker(&plan_id)
        .await
        .with_context(|| format!("checking cancel marker for {plan_id}"))?;
    Ok(PlanStatusView::new(manifest, cancel_requested))
}

/// What [`cancel_plan`] did, so the caller can report it honestly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CancelOutcome {
    /// The marker was written; `queue-maintenance` will tear the plan down on
    /// its next pass.
    Requested,
    /// The plan was already `cancelled`, so nothing was written.
    AlreadyCancelled,
}

/// `plans cancel` — record a cancellation request for a plan.
///
/// Writes only the out-of-band marker. The `cancelled` latch and the actual job
/// teardown belong to `queue-maintenance` (§9): a `status` write from here would
/// race that pass's status refresh, which is the loss the marker exists to
/// prevent. So a cancelled plan keeps reporting its old status until the next
/// pass — `plans status` shows `cancel_requested` meanwhile.
///
/// Refuses a plan that has no manifest, and a `complete` one whose jobs are all
/// terminal — accepting either would imply there was something left to stop.
///
/// Both refusals are **operator feedback, not invariants**: this read and the
/// marker write are not atomic, so the maintenance pass can latch the plan
/// terminal in between (cancelling a plan just as it finishes is the ordinary
/// case). Nothing is corrupted when that happens — the manifest is never written
/// here — and the stray marker is collected by that pass, which deletes markers
/// for terminal and absent plans without touching `status`
/// ([`PlanStore::delete_cancel_marker`]).
pub async fn cancel_plan(
    plans: &dyn PlanStore,
    plan_ref: &PlanRef,
) -> anyhow::Result<(PlanId, CancelOutcome)> {
    let plan_id = resolve_plan_ref(plans, plan_ref).await?;
    let manifest = plans
        .get_plan(&plan_id)
        .await
        .with_context(|| format!("reading manifest for {plan_id}"))?
        .ok_or_else(|| anyhow!("no plan {plan_id} — nothing to cancel"))?;

    match manifest.status {
        PlanStatus::Complete => bail!(
            "plan {plan_id} is already complete — every job reached a terminal state, \
             so there is nothing to cancel"
        ),
        // Already latched: teardown has run or is running, and re-marking would
        // only make the pass redo work it has already finished.
        PlanStatus::Cancelled => Ok((plan_id, CancelOutcome::AlreadyCancelled)),
        // `creating` is cancellable on purpose — it shares the same teardown
        // routine as a manifest stranded mid-ingest (§8).
        PlanStatus::Creating | PlanStatus::Active | PlanStatus::PendingClients => {
            plans
                .write_cancel_marker(&plan_id)
                .await
                .with_context(|| format!("writing cancel marker for {plan_id}"))?;
            tracing::info!(plan_id = %plan_id, "plan cancellation requested");
            Ok((plan_id, CancelOutcome::Requested))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, StorageConfig};
    use crate::plan::{PlanManifest, Warning};
    use crate::stores::build_local_fs_stores;
    use crate::types::JobId;
    use chrono::{DateTime, Duration, Utc};
    use rstest::rstest;
    use serde_json::json;
    use std::collections::BTreeSet;

    fn stores_in(dir: &std::path::Path) -> anyhow::Result<Stores> {
        let config = Config {
            storage: StorageConfig::local_fs(dir.to_path_buf()),
            auth_storage: StorageConfig::local_fs(dir.to_path_buf()),
            ..Config::default()
        };
        build_local_fs_stores(&config)
    }

    fn manifest(id: u128, name: Option<&str>, status: PlanStatus) -> anyhow::Result<PlanManifest> {
        let created_at: DateTime<Utc> = "2026-07-20T17:55:00Z".parse()?;
        Ok(PlanManifest {
            plan_id: PlanId::from_uuid(uuid::Uuid::from_u128(id)),
            plan_name: name.map(str::to_string),
            status,
            // Stagger by id so "newest first" has a deterministic expectation.
            created_at: created_at + Duration::seconds(id as i64),
            job_ids: vec![JobId::new_unchecked("job-1")],
            warnings: Vec::new(),
            progress_snapshot: None,
            terminal_at: None,
        })
    }

    // ── read_job_dir ────────────────────────────────────────────────────────

    /// Only `*.json` files are jobs, and they arrive sorted by file name — not in
    /// `read_dir` order, which is arbitrary and would make ingestion mint ids
    /// (and therefore order `avail/` keys) differently run to run.
    #[test]
    fn read_job_dir_ignores_non_json_and_sorts_by_name() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        // Written in an order that is neither sorted nor reverse-sorted.
        for name in ["c.json", "a.json", "b.json"] {
            std::fs::write(dir.path().join(name), json!({"n": name}).to_string())?;
        }
        // Ignored: wrong extension, no extension, uppercase extension, and a
        // subdirectory that itself contains a .json file.
        std::fs::write(dir.path().join("README.md"), b"notes")?;
        std::fs::write(dir.path().join("generator.log"), b"log")?;
        std::fs::write(dir.path().join("Makefile"), b"all:")?;
        std::fs::write(dir.path().join("LOUD.JSON"), b"{}")?;
        std::fs::create_dir(dir.path().join("nested"))?;
        std::fs::write(dir.path().join("nested/deep.json"), b"{}")?;

        let jobs = read_job_dir(dir.path())?;
        let labels: Vec<&str> = jobs.iter().map(|(l, _)| l.as_str()).collect();
        assert_eq!(labels, vec!["a.json", "b.json", "c.json"]);
        assert_eq!(jobs[0].1, json!({"n": "a.json"}), "bodies parsed, not raw");
        Ok(())
    }

    /// The input directory is never modified — §7 makes it read-only, so an
    /// operator can re-run or archive it.
    #[test]
    fn read_job_dir_leaves_inputs_untouched() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        std::fs::write(dir.path().join("a.json"), b"{\"k\":1}")?;
        std::fs::write(dir.path().join("keep.txt"), b"x")?;

        read_job_dir(dir.path())?;

        let mut left: Vec<String> = std::fs::read_dir(dir.path())?
            .map(|e| Ok(e?.file_name().to_string_lossy().into_owned()))
            .collect::<anyhow::Result<_>>()?;
        left.sort();
        assert_eq!(left, vec!["a.json", "keep.txt"]);
        assert_eq!(std::fs::read(dir.path().join("a.json"))?, b"{\"k\":1}");
        Ok(())
    }

    /// A directory with no job files is an error rather than an empty plan, and
    /// the message points at the tool that produces them. A directory holding
    /// only non-JSON files is the same case.
    #[rstest]
    #[case::empty(&[])]
    #[case::only_ignored_files(&["README.md", "notes.txt"])]
    fn read_job_dir_rejects_no_job_files(#[case] files: &[&str]) -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        for name in files {
            std::fs::write(dir.path().join(name), b"x")?;
        }
        let err = read_job_dir(dir.path()).unwrap_err().to_string();
        assert!(err.contains("no *.json job files"), "{err}");
        Ok(())
    }

    /// A malformed file fails the whole read, naming the offender — the same
    /// whole-set fail-fast as §6.2 validation, since a partially-ingested plan is
    /// worse than none.
    #[test]
    fn read_job_dir_rejects_malformed_json_naming_the_file() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        std::fs::write(dir.path().join("good.json"), b"{}")?;
        std::fs::write(dir.path().join("truncated.json"), b"{\"a\":")?;

        let err = format!("{:#}", read_job_dir(dir.path()).unwrap_err());
        assert!(err.contains("truncated.json"), "names the file: {err}");
        Ok(())
    }

    #[test]
    fn read_job_dir_rejects_missing_directory() -> anyhow::Result<()> {
        let err = read_job_dir(std::path::Path::new("/nonexistent/plan/dir")).unwrap_err();
        assert!(err.to_string().contains("reading job directory"));
        Ok(())
    }

    // ── ingest_dir ──────────────────────────────────────────────────────────

    /// The minimum run specification §6.2 validation accepts: `benchmark`
    /// resolvable against [`catalog`], plus `model` and `runtime` present. Their
    /// contents are opaque to the server, so these fixtures leave them empty.
    fn spec_bench_1() -> Value {
        json!({"benchmark": "bench-1", "model": {}, "runtime": {}})
    }

    /// A one-benchmark catalog keyed by `bench-1`; §6.2 validation only asks it
    /// `contains_key`.
    fn catalog() -> anyhow::Result<HashMap<BenchmarkId, Benchmark>> {
        let def = "benchmark_type = \"prefill_throughput\"\nparameter_prefill_tokens = 100";
        let bench = Benchmark::from_toml("bench-1", def)?;
        Ok(HashMap::from([(BenchmarkId::try_new("bench-1")?, bench)]))
    }

    fn job_dir(names: &[&str]) -> anyhow::Result<tempfile::TempDir> {
        let dir = tempfile::tempdir()?;
        for name in names {
            let body = json!({"requires": ["os:macos"], "spec": spec_bench_1()});
            std::fs::write(dir.path().join(name), body.to_string())?;
        }
        Ok(dir)
    }

    /// End-to-end: a directory of job bodies becomes a manifest plus staged jobs,
    /// the report keys on file names, and non-`.json` files are ignored while the
    /// inputs are left exactly as they were.
    #[tokio::test]
    async fn ingest_dir_stages_a_plan_and_leaves_inputs_alone() -> anyhow::Result<()> {
        let store_dir = tempfile::tempdir()?;
        let stores = stores_in(store_dir.path())?;
        let jobs = job_dir(&["b.json", "a.json"])?;
        std::fs::write(jobs.path().join("README.md"), b"notes")?;

        let report = ingest_dir(
            &stores,
            &catalog()?,
            jobs.path(),
            Some("  smoke  ".to_string()),
        )
        .await?;

        // Two jobs, keyed by file name in sorted order; the README is not one.
        assert_eq!(report.job_count, 2);
        assert_eq!(
            report
                .jobs
                .iter()
                .map(|(l, _)| l.as_str())
                .collect::<Vec<_>>(),
            vec!["a.json", "b.json"]
        );
        // `--plan-name` is trimmed, not stored with the operator's whitespace.
        assert_eq!(report.plan_name.as_deref(), Some("smoke"));

        // The manifest exists and lists both minted ids.
        let manifest = stores
            .plans
            .get_plan(&report.plan_id)
            .await?
            .context("manifest written")?;
        assert_eq!(manifest.job_ids.len(), 2);

        // Both jobs are claimable, each stamped with its own minted id.
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

        // The handoff directory is untouched (§7).
        let mut left: Vec<String> = std::fs::read_dir(jobs.path())?
            .map(|e| Ok(e?.file_name().to_string_lossy().into_owned()))
            .collect::<anyhow::Result<_>>()?;
        left.sort();
        assert_eq!(left, vec!["README.md", "a.json", "b.json"]);
        Ok(())
    }

    /// Ingesting the same directory twice creates a **second** plan with its own
    /// ids — the accepted operator-error window (§7), not an error. Closing it
    /// needs the content-keyed duplicate check §7 leaves to future work.
    #[tokio::test]
    async fn ingest_dir_twice_creates_two_independent_plans() -> anyhow::Result<()> {
        let store_dir = tempfile::tempdir()?;
        let stores = stores_in(store_dir.path())?;
        let jobs = job_dir(&["a.json"])?;

        let first = ingest_dir(&stores, &catalog()?, jobs.path(), None).await?;
        let second = ingest_dir(&stores, &catalog()?, jobs.path(), None).await?;

        assert_ne!(first.plan_id, second.plan_id);
        assert_ne!(first.jobs[0].1, second.jobs[0].1, "job ids are re-minted");
        assert_eq!(stores.plans.list_plans(None).await?.len(), 2);
        assert_eq!(
            stores
                .todo
                .list_avail(None, crate::TEST_LIST_LIMIT)
                .await?
                .len(),
            2,
            "both copies are queued — the duplicate work §7 accepts"
        );
        Ok(())
    }

    /// A rejected job set stages nothing, and the error names the offending file
    /// so an operator can fix it without bisecting the directory.
    #[tokio::test]
    async fn ingest_dir_rejected_set_writes_nothing() -> anyhow::Result<()> {
        let store_dir = tempfile::tempdir()?;
        let stores = stores_in(store_dir.path())?;
        let jobs = job_dir(&["ok.json"])?;
        // Two flags from one reserved namespace — a §6.2 rejection.
        std::fs::write(
            jobs.path().join("bad.json"),
            json!({"requires": ["os:ios", "os:android"], "spec": spec_bench_1()}).to_string(),
        )?;

        let err = format!(
            "{:#}",
            ingest_dir(&stores, &catalog()?, jobs.path(), None)
                .await
                .unwrap_err()
        );
        assert!(err.contains("bad.json"), "names the offending file: {err}");
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

    #[tokio::test]
    async fn ingest_dir_rejects_empty_plan_name() -> anyhow::Result<()> {
        let store_dir = tempfile::tempdir()?;
        let stores = stores_in(store_dir.path())?;
        let jobs = job_dir(&["a.json"])?;

        let err = ingest_dir(&stores, &catalog()?, jobs.path(), Some("   ".to_string()))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("must not be empty"), "{err}");
        assert!(stores.plans.list_plans(None).await?.is_empty());
        Ok(())
    }

    // ── resolve_plan_ref ────────────────────────────────────────────────────

    /// An id passes through without a listing; a unique name resolves to its id.
    #[rstest]
    // `None` = address the plan by its id rather than a name.
    #[case::by_id(None)]
    #[case::exact_name(Some("smoke"))]
    // Surrounding whitespace is trimmed on both sides of the comparison.
    #[case::whitespace_padded_name(Some("  smoke  "))]
    #[tokio::test]
    async fn resolve_plan_ref_by_id_and_unique_name(
        #[case] name: Option<&str>,
    ) -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let stores = stores_in(dir.path())?;
        let m = manifest(1, Some("smoke"), PlanStatus::Active)?;
        stores.plans.put_plan(&m).await?;

        let plan_ref = match name {
            None => PlanRef::Id(m.plan_id.clone()),
            Some(name) => PlanRef::Name(name.to_string()),
        };
        assert_eq!(
            resolve_plan_ref(stores.plans.as_ref(), &plan_ref).await?,
            m.plan_id
        );
        Ok(())
    }

    /// An id that was never ingested resolves fine — `resolve_plan_ref` does not
    /// check existence, so the caller owns the "no such plan" message.
    #[tokio::test]
    async fn resolve_plan_ref_id_does_not_verify_existence() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let stores = stores_in(dir.path())?;
        let absent = PlanId::from_uuid(uuid::Uuid::from_u128(9));
        assert_eq!(
            resolve_plan_ref(stores.plans.as_ref(), &PlanRef::Id(absent.clone())).await?,
            absent
        );
        Ok(())
    }

    /// An ambiguous name is an error naming every candidate, never a guess: a
    /// plan name is a label, not an identity (§2).
    #[tokio::test]
    async fn resolve_plan_ref_ambiguous_name_errors_with_candidates() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let stores = stores_in(dir.path())?;
        let a = manifest(1, Some("dupe"), PlanStatus::Active)?;
        let b = manifest(2, Some("dupe"), PlanStatus::Complete)?;
        stores.plans.put_plan(&a).await?;
        stores.plans.put_plan(&b).await?;

        let err = resolve_plan_ref(stores.plans.as_ref(), &PlanRef::Name("dupe".to_string()))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("2 plans are named"), "{err}");
        assert!(err.contains(a.plan_id.as_str()), "names candidate a: {err}");
        assert!(err.contains(b.plan_id.as_str()), "names candidate b: {err}");
        // A terminal plan is still listed — narrowing silently would be a guess.
        assert!(err.contains("complete"), "shows each status: {err}");
        Ok(())
    }

    #[rstest]
    // No plan carries this name at all.
    #[case("ghost", "no plan named")]
    // An empty/whitespace name can never be a legitimate match.
    #[case("   ", "must not be empty")]
    #[tokio::test]
    async fn resolve_plan_ref_name_errors(
        #[case] name: &str,
        #[case] expected: &str,
    ) -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let stores = stores_in(dir.path())?;
        stores
            .plans
            .put_plan(&manifest(1, Some("real"), PlanStatus::Active)?)
            .await?;

        let err = resolve_plan_ref(stores.plans.as_ref(), &PlanRef::Name(name.to_string()))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains(expected), "{err}");
        Ok(())
    }

    /// An unnamed plan never matches a name lookup — `None` must not collide with
    /// an empty-string query.
    #[tokio::test]
    async fn resolve_plan_ref_skips_unnamed_plans() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let stores = stores_in(dir.path())?;
        stores
            .plans
            .put_plan(&manifest(1, None, PlanStatus::Active)?)
            .await?;

        assert!(
            resolve_plan_ref(
                stores.plans.as_ref(),
                &PlanRef::Name("anything".to_string())
            )
            .await
            .is_err()
        );
        Ok(())
    }

    // ── list_plans ──────────────────────────────────────────────────────────

    /// Newest first, regardless of the backend's listing order, and the status
    /// filter passes through.
    #[tokio::test]
    async fn list_plans_sorts_newest_first_and_filters() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let stores = stores_in(dir.path())?;
        let old = manifest(1, Some("old"), PlanStatus::Active)?;
        let new = manifest(3, Some("new"), PlanStatus::Complete)?;
        let mid = manifest(2, Some("mid"), PlanStatus::Active)?;
        for m in [&old, &new, &mid] {
            stores.plans.put_plan(m).await?;
        }

        let listed = list_plans(stores.plans.as_ref(), None).await?;
        assert_eq!(
            listed.iter().map(|p| p.created_at).collect::<Vec<_>>(),
            vec![new.created_at, mid.created_at, old.created_at]
        );
        assert_eq!(
            list_plans(stores.plans.as_ref(), Some(PlanStatus::Complete)).await?,
            vec![new]
        );
        Ok(())
    }

    // ── plan_status ─────────────────────────────────────────────────────────

    /// The projection drops `job_ids`, keeps the ingestion-time warnings, and
    /// states an uncomputed `progress_snapshot` explicitly as `null` rather than
    /// omitting the key — so a caller can tell "not computed yet" from
    /// "computed, and empty".
    #[tokio::test]
    async fn plan_status_projects_without_job_ids_and_is_explicit_about_absent_snapshot()
    -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let stores = stores_in(dir.path())?;
        let mut m = manifest(1, Some("smoke"), PlanStatus::Active)?;
        m.warnings = vec![Warning {
            message: "2 jobs requiring os:macos match no registered, approved client".to_string(),
            job_ids: vec![JobId::new_unchecked("job-2")],
        }];
        stores.plans.put_plan(&m).await?;

        let view = plan_status(stores.plans.as_ref(), &PlanRef::Id(m.plan_id.clone())).await?;
        assert_eq!(view.plan_id, m.plan_id);
        assert_eq!(view.plan_name.as_deref(), Some("smoke"));
        assert_eq!(view.status, PlanStatus::Active);
        assert!(!view.cancel_requested);
        assert_eq!(view.warnings.len(), 1, "ingestion-time warnings surface");
        assert!(view.progress_snapshot.is_none());
        assert!(view.terminal_at.is_none(), "a live plan has not ended");

        let value = serde_json::to_value(&view)?;
        assert!(value.get("job_ids").is_none(), "job_ids stays internal");
        // Both optional fields state their absence rather than vanishing, so a
        // caller can tell "not computed / still live" from "key not in schema".
        for field in ["progress_snapshot", "terminal_at"] {
            assert_eq!(
                value.get(field),
                Some(&Value::Null),
                "absent {field} is an explicit null, not an omitted key"
            );
        }
        Ok(())
    }

    /// `job_ids` is the *only* manifest field the projection withholds — the
    /// claim its doc comment makes. Pins it against a manifest with every field
    /// populated, so a field added to `PlanManifest` and forgotten in
    /// `PlanStatusView` fails here rather than silently disappearing from
    /// `plans status` and `GET /plans/{plan_id}`.
    #[tokio::test]
    async fn plan_status_withholds_only_job_ids() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let stores = stores_in(dir.path())?;
        // `sample_manifest` carries a snapshot, warnings, a name, and — being
        // terminal — a `terminal_at`.
        let m = crate::plan::sample_manifest(
            PlanId::from_uuid(uuid::Uuid::from_u128(1)),
            PlanStatus::Complete,
            true,
        );
        stores.plans.put_plan(&m).await?;

        let view = plan_status(stores.plans.as_ref(), &PlanRef::Id(m.plan_id.clone())).await?;
        assert_eq!(view.terminal_at, m.terminal_at);
        assert!(view.terminal_at.is_some(), "fixture is terminal");

        let manifest_keys: BTreeSet<String> = as_object_keys(&serde_json::to_value(&m)?)?;
        let view_keys: BTreeSet<String> = as_object_keys(&serde_json::to_value(&view)?)?;
        assert_eq!(
            manifest_keys.difference(&view_keys).collect::<Vec<_>>(),
            vec!["job_ids"],
            "job_ids is the only manifest field withheld"
        );
        assert_eq!(
            view_keys.difference(&manifest_keys).collect::<Vec<_>>(),
            vec!["cancel_requested"],
            "cancel_requested is the only field not read off the manifest"
        );
        Ok(())
    }

    fn as_object_keys(value: &Value) -> anyhow::Result<BTreeSet<String>> {
        Ok(value
            .as_object()
            .context("serializes as a JSON object")?
            .keys()
            .cloned()
            .collect())
    }

    #[tokio::test]
    async fn plan_status_absent_plan_errors() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let stores = stores_in(dir.path())?;
        let absent = PlanId::from_uuid(uuid::Uuid::from_u128(9));
        let err = plan_status(stores.plans.as_ref(), &PlanRef::Id(absent))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("no plan"), "{err}");
        Ok(())
    }

    // ── cancel_plan ─────────────────────────────────────────────────────────

    /// Cancel writes the marker and **nothing else**: the manifest's `status` is
    /// untouched, because latching it here would race `queue-maintenance`'s
    /// status refresh — the loss the out-of-band marker exists to prevent (§9).
    /// `plans status` reports the pending cancel meanwhile.
    #[rstest]
    #[case::active(PlanStatus::Active)]
    #[case::pending_clients(PlanStatus::PendingClients)]
    // `creating` is cancellable on purpose: it shares the stranded-mid-ingest
    // teardown routine (§8).
    #[case::creating(PlanStatus::Creating)]
    #[tokio::test]
    async fn cancel_writes_marker_without_touching_status(
        #[case] status: PlanStatus,
    ) -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let stores = stores_in(dir.path())?;
        let m = manifest(1, Some("doomed"), PlanStatus::Active)?;
        let m = PlanManifest { status, ..m };
        stores.plans.put_plan(&m).await?;

        let (id, outcome) =
            cancel_plan(stores.plans.as_ref(), &PlanRef::Id(m.plan_id.clone())).await?;
        assert_eq!(id, m.plan_id);
        assert_eq!(outcome, CancelOutcome::Requested);

        assert!(stores.plans.has_cancel_marker(&m.plan_id).await?);
        let after = stores
            .plans
            .get_plan(&m.plan_id)
            .await?
            .context("manifest still present")?;
        assert_eq!(after.status, status, "cancel does not latch the status");
        assert_eq!(after, m, "the manifest is not rewritten at all");

        // The pending cancel is visible to an operator before the pass runs.
        let view = plan_status(stores.plans.as_ref(), &PlanRef::Id(m.plan_id)).await?;
        assert!(view.cancel_requested);
        assert_eq!(view.status, status);
        Ok(())
    }

    /// Cancelling by name resolves to the id and behaves identically.
    #[tokio::test]
    async fn cancel_by_plan_name() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let stores = stores_in(dir.path())?;
        let m = manifest(1, Some("by-name"), PlanStatus::Active)?;
        stores.plans.put_plan(&m).await?;

        let (id, outcome) =
            cancel_plan(stores.plans.as_ref(), &PlanRef::Name("by-name".to_string())).await?;
        assert_eq!(id, m.plan_id);
        assert_eq!(outcome, CancelOutcome::Requested);
        assert!(stores.plans.has_cancel_marker(&m.plan_id).await?);
        Ok(())
    }

    /// Re-cancelling a plan the pass has already latched reports the no-op
    /// instead of re-marking work that is already done.
    #[tokio::test]
    async fn cancel_already_cancelled_is_a_reported_no_op() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let stores = stores_in(dir.path())?;
        let m = manifest(1, Some("gone"), PlanStatus::Cancelled)?;
        stores.plans.put_plan(&m).await?;

        let (_, outcome) =
            cancel_plan(stores.plans.as_ref(), &PlanRef::Id(m.plan_id.clone())).await?;
        assert_eq!(outcome, CancelOutcome::AlreadyCancelled);
        assert!(
            !stores.plans.has_cancel_marker(&m.plan_id).await?,
            "no marker written for an already-latched plan"
        );
        Ok(())
    }

    /// Cancelling twice before the pass runs is idempotent — the second call
    /// rewrites the same marker rather than erroring.
    #[tokio::test]
    async fn cancel_twice_before_teardown_is_idempotent() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let stores = stores_in(dir.path())?;
        let m = manifest(1, Some("twice"), PlanStatus::Active)?;
        stores.plans.put_plan(&m).await?;

        for _ in 0..2 {
            let (_, outcome) =
                cancel_plan(stores.plans.as_ref(), &PlanRef::Id(m.plan_id.clone())).await?;
            assert_eq!(outcome, CancelOutcome::Requested);
        }
        assert_eq!(
            stores.plans.list_cancel_markers().await?,
            vec![m.plan_id.clone()]
        );
        Ok(())
    }

    /// An uncancellable plan is refused and no marker is written. Both refusals
    /// are operator feedback rather than invariants (the check is not atomic with
    /// the write), but a cancel the command *does* refuse must leave nothing
    /// behind for the maintenance pass to find.
    #[rstest]
    // No manifest at all — never ingested, or already retention-GC'd.
    #[case::absent(None, "nothing to cancel")]
    // Finished: every job is terminal, so accepting would imply otherwise.
    #[case::complete(Some(PlanStatus::Complete), "already complete")]
    #[tokio::test]
    async fn cancel_refuses_uncancellable_plan_and_writes_no_marker(
        #[case] seeded: Option<PlanStatus>,
        #[case] expected: &str,
    ) -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let stores = stores_in(dir.path())?;
        let m = manifest(1, Some("doomed"), PlanStatus::Active)?;
        if let Some(status) = seeded {
            stores
                .plans
                .put_plan(&PlanManifest {
                    status,
                    ..m.clone()
                })
                .await?;
        }

        let err = cancel_plan(stores.plans.as_ref(), &PlanRef::Id(m.plan_id.clone()))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains(expected), "{err}");
        assert!(!stores.plans.has_cancel_marker(&m.plan_id).await?);
        assert!(stores.plans.list_cancel_markers().await?.is_empty());
        Ok(())
    }
}
