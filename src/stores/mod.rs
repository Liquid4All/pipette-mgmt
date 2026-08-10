mod local_fs;
mod s3;

use std::collections::{BTreeSet, HashMap, HashSet};
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use futures::stream::{StreamExt, TryStreamExt};
use serde::{Deserialize, Serialize};

use crate::benchmark::Benchmark;
use crate::client::{Client, MigrationRecord, SignatureMigration};
use crate::eval_sample_result::EvalSampleResult;
use crate::plan::{PlanManifest, PlanStatus};
use crate::preauth::{PreauthConsumeOutcome, PreauthKey, Secret};
use crate::types::{BenchmarkId, ClientId, ExpiresAt, JobId, PlanId, PreauthKeyId};
use crate::validated::{PublicKeyHex, Tag};
use crate::warehouse::{JobMetrics, MetricRow};

/// Parse a persisted GC orphan-candidate set (a JSON array of storage keys),
/// treating corrupt state as empty. Empty is the safe direction: it only
/// delays marker GC by one run, and the set is rewritten wholesale every run,
/// so corruption self-heals rather than wedging every subsequent run.
pub(crate) fn parse_gc_candidates(bytes: &[u8]) -> HashSet<String> {
    serde_json::from_slice(bytes).unwrap_or_else(|e| {
        tracing::warn!(error = %e, "corrupt gc-candidates state; treating as empty");
        HashSet::new()
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobState {
    Incoming,
    /// In the `score-queue/` pipeline (either stage): the slow `score-eval`
    /// pass has the job in flight. Reported to clients so a `GET /jobs/{id}`
    /// mid-pipeline shows progress instead of a 404.
    Scoring,
    Processed,
}

impl JobState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Incoming => "incoming",
            Self::Scoring => "scoring",
            Self::Processed => "processed",
        }
    }
}

/// A stage of the `score-queue/` pipeline. Each stage is a flat prefix of
/// per-job JSON payloads, drained by a worker that advances jobs to the next
/// stage. `ToDo` holds the raw submission awaiting the (slow) scoring-service
/// call; `ToFinalize` holds the submission plus its score response, awaiting
/// the (fast) warehouse write.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScoreQueueStage {
    ToDo,
    ToFinalize,
}

impl ScoreQueueStage {
    /// Prefix under `submissions/` that groups the queue stages, so the full
    /// layout is `submissions/score-queue/<leaf>/`.
    pub const ROOT: &'static str = "score-queue";

    /// This stage's leaf directory name (e.g. `to_do`).
    pub fn leaf(self) -> &'static str {
        match self {
            Self::ToDo => "to_do",
            Self::ToFinalize => "to_finalize",
        }
    }
}

#[derive(Debug, Clone)]
pub struct SubmissionRecord {
    pub job_id: JobId,
    pub state: JobState,
    pub body: serde_json::Value,
}

/// Outcome of [`SubmissionStore::prune_unverified`]. `deleted` counts
/// objects removed (or, on a dry run, the objects that *would* be
/// removed); `kept` counts objects retained because they were modified
/// more recently than the age threshold.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PruneSummary {
    pub deleted: usize,
    pub kept: usize,
}

#[async_trait]
pub trait CatalogStore: Send + Sync {
    async fn load_catalog(&self) -> anyhow::Result<HashMap<BenchmarkId, Benchmark>>;
}

#[async_trait]
pub trait AuthStore: Send + Sync {
    async fn get_client(&self, client_id: &ClientId) -> anyhow::Result<Option<Client>>;
    async fn put_client(&self, client: &Client) -> anyhow::Result<()>;
    /// Delete a client record **and** all of its tag markers (both the forward
    /// `tags-index/by-client/{id}/` tree and the reverse `tags-index/by-tag/`
    /// entries).
    async fn delete_client(&self, client_id: &ClientId) -> anyhow::Result<()>;
    async fn list_clients(&self) -> anyhow::Result<Vec<Client>>;
    async fn has_public_key(&self, public_key: &PublicKeyHex) -> anyhow::Result<bool>;

    // ── Tags ──────────────────────────────────────────────────────────────
    //
    // Tags are empty leaf markers in two trees, not a record field — each
    // direction is one listing, nothing to (de)serialize:
    //   forward:  tags-index/by-client/{client_id}/{tag}   ← source of truth
    //   reverse:  tags-index/by-tag/{tag}/{client_id}      ← derived index
    //
    // An object store has no multi-key atomic write, so the two can't be updated
    // atomically. The forward tree is authoritative; the reverse is a derived
    // accelerator for `tag -> clients`. Mutations commit the forward marker
    // first, so a crash between the two writes can only leave the *reverse*
    // stale (never the truth), and `reindex_tags` reconciles it back.

    /// Add a `(client, tag)` membership — forward marker first, then reverse.
    /// Idempotent. Does not check that the client exists — callers that need
    /// that call `get_client` first.
    async fn add_client_tag(&self, client_id: &ClientId, tag: &Tag) -> anyhow::Result<()>;

    /// Remove a `(client, tag)` membership — forward marker first, then reverse.
    /// Idempotent.
    async fn remove_client_tag(&self, client_id: &ClientId, tag: &Tag) -> anyhow::Result<()>;

    /// A client's tags, from the authoritative forward tree. Empty when the
    /// client is untagged *or* unknown (existence is a separate `get_client`).
    async fn get_client_tags(&self, client_id: &ClientId) -> anyhow::Result<BTreeSet<Tag>>;

    /// The client ids currently carrying `tag`, ascending, from the reverse
    /// index. A single listing, not a client scan.
    async fn list_client_ids_by_tag(&self, tag: &Tag) -> anyhow::Result<Vec<ClientId>>;

    /// Every `(client_id, tag)` in the authoritative forward tree. Used by
    /// [`reindex_tags`] to reconcile the derived reverse index.
    async fn list_forward_tag_markers(&self) -> anyhow::Result<Vec<(ClientId, Tag)>>;

    /// Every `(client_id, tag)` in the derived reverse tree. Used by
    /// [`reindex_tags`].
    async fn list_reverse_tag_markers(&self) -> anyhow::Result<Vec<(ClientId, Tag)>>;

    // ── Signature migration ─────────────────────────────────────────────────
    //
    // One marker per client at `signature-migration/{client_id}.json`, written
    // the first time that client presents a verified `v1` signature and never
    // rewritten. A client holding one is refused the timestamp-only fallback,
    // which is what makes a signature captured from it before it migrated
    // worthless afterwards (`docs/authentication.md` §2.3).
    //
    // Deliberately a marker tree rather than a field on the client record. The
    // migration is finite: when every client has one, the fallback is switched
    // off and this tree and the code reading it are deleted together, where a
    // record field would outlive the thing it describes. It also keeps the write
    // off the client record entirely — [`Self::put_client`] is a blind
    // whole-record write, so a read-modify-write here could clobber a concurrent
    // status change.

    /// Whether `client_id` has ever presented a `v1` signature.
    ///
    /// A read failure propagates rather than reporting `false`. The caller uses
    /// this to decide whether the replayable fallback is still available, so a
    /// swallowed error would open the path this marker exists to close.
    async fn has_signature_migration(&self, client_id: &ClientId) -> anyhow::Result<bool>;

    /// Record that `client_id` has presented a verified `v1` signature,
    /// reporting whether this call was the one that wrote the marker.
    ///
    /// Create-if-absent, so the first write wins and `first_seen` is the first
    /// sighting rather than the most recent. A client that already has a marker
    /// is [`MigrationRecord::Existing`], not an error.
    async fn record_signature_migration(
        &self,
        client_id: &ClientId,
        at: DateTime<Utc>,
    ) -> anyhow::Result<MigrationRecord>;

    /// Every recorded migration, for the `clients list` operator view. A single
    /// listing, joined against the client roster in memory.
    ///
    /// Entries that cannot be read are skipped rather than propagated. This
    /// backs a display column, and a skipped entry shows a migrated client as
    /// not yet migrated — which only delays clearing the compatibility flag.
    /// [`Self::has_signature_migration`] decides whether that flag still applies
    /// to a given client and propagates instead.
    async fn list_signature_migrations(
        &self,
    ) -> anyhow::Result<Vec<(ClientId, SignatureMigration)>>;

    // ── Pre-auth keys ───────────────────────────────────────────────────────
    //
    // Minted by an operator, presented once by a client at registration. The
    // record lives at `preauth/{key_id}.json` and is never rewritten — the only
    // post-creation change to it is deletion. `expires_at` on the record gates
    // reuse by time; a spend is gated by a sibling `preauth/{key_id}.spent`
    // marker, created exclusively. Both gates need only a single-object
    // conditional create, so a plain S3 `auth_storage` bucket suffices and no
    // multi-object move is involved. A multi-use key is not mutated on consume.

    /// Persist a freshly minted key.
    async fn put_preauth_key(&self, key: &PreauthKey) -> anyhow::Result<()>;

    /// Validate + consume the key identified by `key_id`, verifying `secret`.
    ///
    /// A single-use key is spent by creating `preauth/{key_id}.spent`
    /// exclusively, so exactly one of any number of concurrent consumes wins and
    /// the rest are rejected as unknown; the winner then deletes the record.
    /// Should that delete not land, the marker still stands and the key stays
    /// spent. A multi-use key is only read. Returns
    /// [`PreauthConsumeOutcome::Granted`] or
    /// [`PreauthConsumeOutcome::Rejected`]; `Err` is reserved for I/O.
    async fn consume_preauth_key(
        &self,
        key_id: &PreauthKeyId,
        secret: &Secret,
    ) -> anyhow::Result<PreauthConsumeOutcome>;

    /// All keys (records include only `secret_hash`, never the secret). For the
    /// `preauth list` operator view.
    async fn list_preauth_keys(&self) -> anyhow::Result<Vec<PreauthKey>>;

    /// Delete a key record outright. Idempotent — a missing key is a no-op. This
    /// is the only post-creation mutation: it backs operator `revoke`, `prune`
    /// of expired keys, and the self-delete of a spent single-use key.
    async fn delete_preauth_key(&self, key_id: &PreauthKeyId) -> anyhow::Result<()>;

    /// Key ids carrying a spent marker, whether or not their record survives.
    /// `preauth prune` pairs this with [`Self::list_preauth_keys`] to find the
    /// markers left behind by keys whose record is already gone.
    async fn list_spent_markers(&self) -> anyhow::Result<Vec<PreauthKeyId>>;

    /// Delete the spent marker for `key_id`. Idempotent.
    ///
    /// Sound only once that key has no record left: while a record survives, its
    /// marker is the one thing keeping the key spent, and removing it would make
    /// the key consumable again.
    async fn delete_spent_marker(&self, key_id: &PreauthKeyId) -> anyhow::Result<()>;
}

#[async_trait]
pub trait SubmissionStore: Send + Sync {
    async fn write_incoming(&self, job_id: &JobId, body: &serde_json::Value) -> anyhow::Result<()>;
    /// Write a submission directly to its terminal `processed/` state,
    /// bypassing the `incoming/` scoring queue. Used by the HTTP
    /// handler for `message_type: "failure"` bodies — the scorer has
    /// nothing to do with them, so routing them through `incoming/`
    /// would just leave them in a state that lies about whether
    /// they're still pending.
    async fn write_processed(&self, job_id: &JobId, body: &serde_json::Value)
    -> anyhow::Result<()>;
    /// Look up a submission by `job_id`. Searches `incoming/` first, then
    /// `processed/`. Returns `Ok(None)` if nothing is found.
    async fn get_submission(&self, job_id: &JobId) -> anyhow::Result<Option<SubmissionRecord>>;
    /// List up to `limit` incoming submission `job_id`s. Used by the
    /// scoring loop to bound per-tick LIST cost.
    ///
    /// - If the store has fewer than `limit.get()` items, all are returned;
    ///   callers can use `returned.len() < limit.get()` as the
    ///   listing-exhausted signal.
    /// - Iteration order is implementation-defined (S3: lexicographic;
    ///   local_fs: `read_dir` order). Callers must not rely on time order.
    async fn list_incoming(&self, limit: NonZeroUsize) -> anyhow::Result<Vec<JobId>>;
    /// Transition a submission from `incoming/{job_id}.json` to
    /// `processed/{job_id}.json.gz`, gzipping along the way.
    async fn mark_processed(&self, job_id: &JobId) -> anyhow::Result<()>;
    /// Delete `incoming/{job_id}.json` without archiving it. Used when a job
    /// is routed out of `incoming/` into the score-queue. Idempotent — a
    /// missing object is not an error.
    async fn delete_incoming(&self, job_id: &JobId) -> anyhow::Result<()>;
    /// Find a submission by `job_id` across `incoming/`, `processed/`, and the
    /// `score-queue/` stages. The caller is responsible for any ownership check
    /// against the payload's `client_id`; path-level partitioning by client no
    /// longer exists. Backends implement the `incoming`/`processed` lookup and
    /// fall back to [`find_in_score_queue`](Self::find_in_score_queue) on a
    /// miss, so an eval being scored reports `Scoring` rather than 404.
    async fn find_job(&self, job_id: &JobId) -> anyhow::Result<Option<SubmissionRecord>>;

    /// Look up a job sitting in the `score-queue/`, checking `to_do` then
    /// `to_finalize`. Returns a [`JobState::Scoring`] record with a bare
    /// submission body: `to_do` already holds one, and the `to_finalize`
    /// payload's `{ submission, score }` is unwrapped to its `submission` so
    /// callers see the same shape regardless of stage. Returns `Ok(None)` when
    /// the job is in neither stage.
    async fn find_in_score_queue(
        &self,
        job_id: &JobId,
    ) -> anyhow::Result<Option<SubmissionRecord>> {
        if let Some(body) = self.read_queue(ScoreQueueStage::ToDo, job_id).await? {
            return Ok(Some(SubmissionRecord {
                job_id: job_id.clone(),
                state: JobState::Scoring,
                body,
            }));
        }
        if let Some(payload) = self.read_queue(ScoreQueueStage::ToFinalize, job_id).await? {
            let body = payload
                .get("submission")
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("to_finalize payload missing `submission`"))?;
            return Ok(Some(SubmissionRecord {
                job_id: job_id.clone(),
                state: JobState::Scoring,
                body,
            }));
        }
        Ok(None)
    }

    /// Write a job's payload into a [`ScoreQueueStage`] of the `score-queue/`.
    /// The payload shape is the worker's contract: `ToDo` carries the raw
    /// submission, `ToFinalize` carries `{ submission, score }`.
    async fn enqueue(
        &self,
        stage: ScoreQueueStage,
        job_id: &JobId,
        body: &serde_json::Value,
    ) -> anyhow::Result<()>;
    /// List up to `limit` `job_id`s waiting in a stage. Same ordering and
    /// exhaustion semantics as [`list_incoming`](Self::list_incoming).
    async fn list_queue(
        &self,
        stage: ScoreQueueStage,
        limit: NonZeroUsize,
    ) -> anyhow::Result<Vec<JobId>>;
    /// Read a queued job's payload, or `None` if it isn't in that stage.
    async fn read_queue(
        &self,
        stage: ScoreQueueStage,
        job_id: &JobId,
    ) -> anyhow::Result<Option<serde_json::Value>>;
    /// Remove a job from a stage once it has advanced. Idempotent — a missing
    /// object is not an error.
    async fn dequeue(&self, stage: ScoreQueueStage, job_id: &JobId) -> anyhow::Result<()>;

    /// Hold a submission from an unapproved client at
    /// `submissions/unverified/{client_id}/{job_id}.json`.
    ///
    /// The unverified tree is partitioned by `client_id` so an operator
    /// can promote or delete a whole client's held submissions at once
    /// (see [`list_unverified_client`](Self::list_unverified_client),
    /// [`delete_unverified_client`](Self::delete_unverified_client)).
    /// It is intentionally not surfaced through `get_submission`,
    /// `list_incoming`, `find_job`, or `mark_processed`, so the scorer
    /// and the `fix-*` family never pick it up until promotion. See
    /// `docs/storage.md` §4.1.
    async fn write_unverified(
        &self,
        client_id: &ClientId,
        job_id: &JobId,
        body: &serde_json::Value,
    ) -> anyhow::Result<()>;

    /// List a single client's held submissions as `(job_id, body)`
    /// pairs. Used by the `promote` operation, which routes each body
    /// back into the normal pipeline. Returns an empty vec when the
    /// client has no held submissions.
    async fn list_unverified_client(
        &self,
        client_id: &ClientId,
    ) -> anyhow::Result<Vec<(JobId, serde_json::Value)>>;

    /// Delete a single held submission object
    /// (`unverified/{client_id}/{job_id}.json`). Used by `promote` once
    /// the body has been re-staged into the pipeline.
    async fn delete_unverified(&self, client_id: &ClientId, job_id: &JobId) -> anyhow::Result<()>;

    /// Delete every held submission for one client, returning the number
    /// deleted. When `dry_run`, nothing is deleted but the count still
    /// reflects what would be.
    async fn delete_unverified_client(
        &self,
        client_id: &ClientId,
        dry_run: bool,
    ) -> anyhow::Result<usize>;

    /// Delete held objects whose backend modification time (S3
    /// `LastModified` / filesystem `mtime`, **not** the payload's
    /// `submitted_at`) is older than `older_than`, across all clients.
    /// When `dry_run`, nothing is deleted but the returned
    /// [`PruneSummary`] still reports what would be. Operator-only; does
    /// not touch any other storage domain.
    async fn prune_unverified(
        &self,
        older_than: std::time::Duration,
        dry_run: bool,
    ) -> anyhow::Result<PruneSummary>;
}

#[async_trait]
pub trait WarehouseStore: Send + Sync {
    /// Append metric rows to a `day=` partition. Rows may span multiple jobs.
    /// The implementation appends to the tail size-capped part file and rolls
    /// to a new one at the cap — it does **not** read or dedup the rest of the
    /// partition, so a re-scored job leaves a duplicate row set that reads
    /// resolve via `JobMetrics::from_latest_rows`. Legacy `month=` partitions
    /// are never written.
    async fn write_partition_metrics(
        &self,
        benchmark_id: &BenchmarkId,
        client_id: &ClientId,
        day_key: &str,
        rows: &[MetricRow],
    ) -> anyhow::Result<()>;
    /// Read a job's metrics, scanning only partitions within the
    /// configured recent-day window (`warehouse_read_days`). Returns
    /// `None` for a job older than that window — callers report it
    /// without metrics rather than scanning the whole archive.
    async fn read_job_metrics(
        &self,
        benchmark_id: &BenchmarkId,
        client_id: &ClientId,
        job_id: &JobId,
    ) -> anyhow::Result<Option<JobMetrics>>;

    /// Visit every metric row in the warehouse, in implementation-defined
    /// order. The callback may mutate each row in place; if it returns
    /// `true` for at least one row in a file, the file is rewritten
    /// atomically (tmp+rename on local-fs, single PUT on S3). Files
    /// where no row is marked dirty are not touched.
    ///
    /// Used by the warehouse-rewrite subcommands (the `fix-*` family) so
    /// the file-walking, read, decide-to-rewrite, and atomic-replace
    /// logic lives once per backend rather than being duplicated by
    /// every caller. This must not run in parallel with `score` — both
    /// do read-modify-write on the same Parquet partitions — so the
    /// `fix-*` CLI commands hold the storage mutate lock (see
    /// `crate::storage_lock`) for their whole run.
    async fn for_each_metric_row(
        &self,
        f: &mut (dyn for<'a> FnMut(&'a mut MetricRow) -> bool + Send),
    ) -> anyhow::Result<()>;
}

#[async_trait]
pub trait EvalSampleResultStore: Send + Sync {
    /// Per-submission eval sample results, written once per scored eval
    /// job. Stored at `warehouse/eval_sample_results/{job_id}.parquet`.
    async fn write(&self, job_id: &JobId, rows: &[EvalSampleResult]) -> anyhow::Result<()>;
    async fn read(&self, job_id: &JobId) -> anyhow::Result<Option<Vec<EvalSampleResult>>>;

    /// List the `job_id` of every eval-sample-results file in the store. The
    /// keyspace is flat (`warehouse/eval_sample_results/{job_id}.parquet`), so
    /// each entry maps 1:1 to a `.parquet` file's stem. Order is
    /// implementation-defined. Intended for bulk maintenance that enumerates
    /// every file, then reads/writes each via the atomic
    /// [`read`](Self::read) / [`write`](Self::write) above.
    async fn list_job_ids(&self) -> anyhow::Result<Vec<JobId>>;
}

/// Durable store for plan manifests (`plans/{plan_id}.json` in the `[storage]`
/// backend). Mirrors the existing durable-record pattern (client / preauth JSON
/// records); manifests must outlive their jobs so a completed or cancelled plan
/// stays queryable after its jobs have left the `todo/` queue. See
/// `docs/plan-ingestion.md` §9.
#[async_trait]
pub trait PlanStore: Send + Sync {
    /// Write (create or overwrite) a plan manifest. A whole-object PUT with **no
    /// compare-and-swap** — safe not because the key is unique but because the
    /// creation writer (ingestion) and the maintenance writer never write
    /// concurrently: ingestion writes `creating` then the first `active` /
    /// `pending_clients` and hands off; from `active` onward `queue-maintenance`
    /// is the sole writer (`docs/plan-ingestion.md` §9). The one overlap the
    /// design admits — `queue-maintenance` tearing down a manifest stuck in
    /// `creating` while a slow-but-alive ingestion is still staging — is
    /// resolved out of band by the create-only cancel marker, **not** by this
    /// store: `put_plan` is last-writer-wins and preserves no terminal latch.
    async fn put_plan(&self, manifest: &PlanManifest) -> anyhow::Result<()>;

    /// Read a manifest by id. `Ok(None)` if absent — never ingested, or already
    /// retention-GC'd.
    async fn get_plan(&self, plan_id: &PlanId) -> anyhow::Result<Option<PlanManifest>>;

    /// Every manifest whose status matches `status` (`None` = all), in
    /// implementation-defined order. Lists `plans/` and reads each object — the
    /// same N+1 fan-out as [`AuthStore::list_preauth_keys`], acceptable at plan
    /// cardinality (plans are coarse-grained, far fewer than jobs).
    async fn list_plans(&self, status: Option<PlanStatus>) -> anyhow::Result<Vec<PlanManifest>>;

    /// Delete a manifest. Idempotent — a missing manifest is a no-op (mirrors
    /// [`AuthStore::delete_preauth_key`]).
    async fn delete_plan(&self, plan_id: &PlanId) -> anyhow::Result<()>;

    /// Write the cancel marker for a plan — an empty object at
    /// `cancelled_plans/{plan_id}`, a **sibling** keyspace of `plans/` so
    /// [`list_plans`](Self::list_plans) never has to filter marker objects out
    /// of its listing.
    ///
    /// The marker *requests* teardown rather than performing it: `plans cancel`
    /// writes it and stops, and `queue-maintenance` is the sole writer of both
    /// the `cancelled` latch and the `todo/` deletes
    /// (`docs/plan-ingestion.md` §9). Signaling out of band this way is what
    /// keeps a cancel from being lost to a concurrent status refresh — the
    /// overlap [`put_plan`](Self::put_plan) explicitly does not resolve.
    ///
    /// "Create-only" describes that request-not-mutation role, **not** a
    /// compare-and-swap: this is a plain idempotent PUT, so re-cancelling
    /// rewrites the same empty object and is a no-op.
    ///
    /// A caller's "does this plan exist / is it cancellable" check is therefore
    /// **best effort only**, and must not be treated as an invariant this
    /// keyspace upholds. The check and this write are not atomic, and the
    /// maintenance pass may latch the plan terminal (or the retention GC may
    /// remove its manifest) in between — an operator cancelling a plan just as
    /// it finishes is the ordinary case, not an exotic one. Nothing is corrupted
    /// when that happens, because this never touches the manifest; the residue
    /// is a marker naming a plan that is terminal or gone. Collection is
    /// therefore **convergent, not precondition-based** — see
    /// [`delete_cancel_marker`](Self::delete_cancel_marker).
    async fn write_cancel_marker(&self, plan_id: &PlanId) -> anyhow::Result<()>;

    /// Whether a plan has a pending cancel marker. Read by `plans status`, to
    /// show a cancel that has been requested but not yet latched by the
    /// maintenance pass (a window of up to one cron interval).
    async fn has_cancel_marker(&self, plan_id: &PlanId) -> anyhow::Result<bool>;

    /// Every plan id with a pending cancel marker, in implementation-defined
    /// order. The maintenance pass's entry point into teardown: markers are far
    /// sparser than manifests, so listing them is cheaper than reading every
    /// manifest to find the cancelled ones. A key that does not parse as a
    /// [`PlanId`] is foreign cruft — warned and skipped, not an error.
    async fn list_cancel_markers(&self) -> anyhow::Result<Vec<PlanId>>;

    /// Delete a cancel marker. Idempotent — a missing marker is a no-op.
    ///
    /// `queue-maintenance` owns collection, and must delete a marker in **every**
    /// case where it can no longer lead to work, not just the happy one:
    ///
    /// - the plan was torn down and has no live jobs left — the marker has done
    ///   its job, and deleting it stops the pass re-running teardown forever;
    /// - the plan is already **terminal** (`complete`, or `cancelled` from an
    ///   earlier pass) — delete the marker and leave `status` alone. A terminal
    ///   latch is never revisited (`docs/plan-ingestion.md` §9), so a marker that
    ///   arrived after the latch must not resurrect the plan as `cancelled`;
    /// - the plan has **no manifest** — never ingested, or retention-GC'd.
    ///   There is nothing to tear down and nothing to latch.
    ///
    /// Without the last two cases a marker that lost the race with the latch
    /// would be collected by nobody, leaving `plans status` reporting
    /// `cancel_requested` on a finished plan indefinitely — and, once the
    /// manifest is GC'd, with no surface that even shows the leak.
    async fn delete_cancel_marker(&self, plan_id: &PlanId) -> anyhow::Result<()>;
}

/// Returned by [`TodoStore::claim_job`].
#[derive(Debug)]
pub enum ClaimResult {
    Claimed(serde_json::Value),
    /// Source key was absent — another client won the race. Caller should
    /// skip to the next candidate.
    Gone,
}

/// Returned by [`TodoStore::recycle_lease`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecycleResult {
    /// The lease was moved back to `avail/`.
    Recycled,
    /// The lease was already gone — the holder's terminal submission tore it
    /// down, or another actor recycled it first. Nothing left to do; the
    /// distinction lets callers skip a benign race instead of treating it as
    /// a storage failure.
    Gone,
}

/// Returned by [`TodoStore::renew_lease`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RenewLeaseResult {
    Renewed,
    /// Lease file not found — job was recycled or completed.
    NotFound,
    /// Lease exists but belongs to a different client.
    WrongClient,
}

#[async_trait]
pub trait TodoStore: Send + Sync {
    // ── avail/ ──────────────────────────────────────────────────────────────
    /// List entries in key order, starting after `start_after` (exclusive).
    /// Returns at most `limit` entries. Keys are `{job_id}.{expires_at}.json`;
    /// callers parse via [`crate::todo_filename::parse_avail_filename`].
    async fn list_avail(
        &self,
        start_after: Option<&str>,
        limit: NonZeroUsize,
    ) -> anyhow::Result<Vec<String>>;

    /// Read the JSON body of an avail entry. Returns `None` if absent.
    async fn get_avail(
        &self,
        job_id: &JobId,
        expires_at: ExpiresAt,
    ) -> anyhow::Result<Option<serde_json::Value>>;

    /// Delete an avail entry. No-op if already absent.
    async fn delete_avail(&self, job_id: &JobId, expires_at: ExpiresAt) -> anyhow::Result<()>;

    /// Delete every `avail/` entry for `job_id`, regardless of the `expires_at`
    /// encoded in its filename. Used by the submission handler's job-completion
    /// teardown, which knows the `job_id` but not the (immutable) expiry — the
    /// result body carries no `expires_at` and the job body isn't in hand. A job
    /// has at most one `avail/` entry, present only in the recycle race (lease
    /// expired → `queue-maintenance` recycled it → the original client completes
    /// late); the normal claim→complete flow leaves nothing here. No-op if
    /// absent. See `planner.md`.
    async fn delete_avail_by_job(&self, job_id: &JobId) -> anyhow::Result<()>;

    /// Read the body of the `avail/` entry for `job_id`, regardless of the
    /// (immutable) `expires_at` in its filename — the caller knows the `job_id`
    /// but not the expiry. Counterpart to `delete_avail_by_job`. Used on the
    /// retriable-failure path: after `recycle_lease` returns the job to `avail/`,
    /// the handler reads its body to validate the client's report and to decide
    /// the `clients`-only all-denied escalation (see `planner.md`,
    /// "Consequences of Failure"). A job has at most one `avail/` entry; returns
    /// `None` if absent (completed, expired, or already re-claimed by another
    /// client — all of which correctly skip the escalation).
    async fn get_avail_by_job(&self, job_id: &JobId) -> anyhow::Result<Option<serde_json::Value>>;

    // ── leased/ ─────────────────────────────────────────────────────────────
    /// List every lease as a relative key `{client_id}/{job_id}.{lease_expiry}.json`
    /// across all clients. Callers parse via
    /// [`crate::todo_filename::parse_leased_key`]. Used by `claim` to build its
    /// `taken` set; callers that know the client should prefer
    /// `list_leased_for_client`.
    async fn list_leased(&self) -> anyhow::Result<Vec<String>>;

    /// List one client's leases (the `leased/{client_id}/` partition) as relative
    /// keys, same format as `list_leased`. A targeted prefix scan for callers
    /// that already know the client (`reclaim`), avoiding a sweep of the whole
    /// `leased/` tree. (`renew_lease` does this scan internally for heartbeat.)
    async fn list_leased_for_client(&self, client_id: &ClientId) -> anyhow::Result<Vec<String>>;

    /// Read the JSON body of the lease at
    /// `leased/{client_id}/{job_id}.{lease_expiry}.json`. Returns `None` if the
    /// entry is absent — the lease vanished (recycled or completed) between the
    /// caller's listing and this read. Used by the idempotent `claim` path to
    /// hand a re-polling client back the job it already holds without renewing
    /// the lease (see `planner.md`, "Existing lease check").
    async fn get_leased(
        &self,
        job_id: &JobId,
        client_id: &ClientId,
        lease_expiry: DateTime<Utc>,
    ) -> anyhow::Result<Option<serde_json::Value>>;

    /// Renew this client's lease on `job_id` to `new_expiry` (heartbeat).
    /// The store locates the client's current lease itself — the caller need
    /// not know its expiry — and atomically renames
    /// `leased/{client_id}/{job_id}.{old_expiry}.json` →
    /// `leased/{client_id}/{job_id}.{new_expiry}.json`.
    ///
    /// Returns [`RenewLeaseResult::Renewed`] on success;
    /// [`RenewLeaseResult::NotFound`] if this client holds no lease for the job
    /// and no other client does either (it was recycled or completed); and
    /// [`RenewLeaseResult::WrongClient`] if a different client holds it.
    async fn renew_lease(
        &self,
        job_id: &JobId,
        client_id: &ClientId,
        new_expiry: DateTime<Utc>,
    ) -> anyhow::Result<RenewLeaseResult>;

    /// Delete a specific lease entry. No-op if absent.
    async fn delete_lease(
        &self,
        job_id: &JobId,
        client_id: &ClientId,
        expiry: DateTime<Utc>,
    ) -> anyhow::Result<()>;

    // ── avail/ → leased/ (claim) ─────────────────────────────────────────
    /// Atomically rename `avail/{job_id}.{expires_at}.json` →
    /// `leased/{client_id}/{job_id}.{lease_expiry}.json`.
    async fn claim_job(
        &self,
        job_id: &JobId,
        expires_at: ExpiresAt,
        client_id: &ClientId,
        lease_expiry: DateTime<Utc>,
    ) -> anyhow::Result<ClaimResult>;

    // ── leased/ → avail/ (lease recycle) ────────────────────────────────────
    /// Read the body of a leased entry, extract `expires_at`, then atomically
    /// rename `leased/…` → `avail/{job_id}.{expires_at}.json`. An absent or null
    /// `expires_at` is treated as `never` (the field is optional in a job body);
    /// only a present-but-unparseable value is an error.
    ///
    /// Returns [`RecycleResult::Gone`] when the lease no longer exists at
    /// either step — the read and the rename race the holder's own terminal
    /// submission (whose teardown deletes the lease) and other recyclers, and
    /// a vanished source means someone else already resolved the job.
    async fn recycle_lease(
        &self,
        job_id: &JobId,
        client_id: &ClientId,
        lease_expiry: DateTime<Utc>,
    ) -> anyhow::Result<RecycleResult>;

    // ── denied/ ─────────────────────────────────────────────────────────────
    async fn write_denied(&self, job_id: &JobId, client_id: &ClientId) -> anyhow::Result<()>;
    async fn list_denied_for_job(&self, job_id: &JobId) -> anyhow::Result<Vec<ClientId>>;
    async fn delete_denied_for_job(&self, job_id: &JobId) -> anyhow::Result<()>;
    /// Delete a single `denied/{job_id}.{client_id}` marker. No-op if absent.
    /// The per-marker counterpart to `delete_denied_for_job`: the reconciliation
    /// sweep uses it to drop one orphan (a still-live job's marker for a deleted
    /// client) without touching that job's denials from clients that still
    /// exist — mirroring `delete_eligible` vs `delete_eligible_for_client`.
    async fn delete_denied(&self, job_id: &JobId, client_id: &ClientId) -> anyhow::Result<()>;
    /// List every `(job_id, client_id)` denial marker across all jobs (GC sweep).
    /// Parses `denied/{job_id}.{client_id}` on the `.` — both `job_id` and
    /// `client_id` exclude `.` (their charsets reject it — see
    /// [`crate::types::JobId::try_new`]), so the split is unambiguous even when
    /// `client_id` contains `_`.
    /// Mirrors `list_all_eligible`.
    async fn list_all_denied(&self) -> anyhow::Result<Vec<(JobId, ClientId)>>;

    // ── eligible/clients/ ────────────────────────────────────────────────────
    /// List the jobs this client is eligible for, each with the `expires_at`
    /// encoded in its marker filename. Returned in no guaranteed order — backends
    /// list by their own native ordering (filesystem-arbitrary for local_fs,
    /// lexicographic for S3). Callers that need a selection order (e.g. the claim
    /// handler's soonest-expiry-first preference) must impose it themselves.
    /// Carrying `expires_at` here lets the claim handler rank and claim without a
    /// secondary `avail/` scan.
    async fn list_eligible_for_client(
        &self,
        client_id: &ClientId,
    ) -> anyhow::Result<Vec<(JobId, ExpiresAt)>>;
    /// Write an eligible marker. `expires_at` must match the job's `avail/`
    /// entry; the sole caller (`queue-maintenance`) sources it from the `avail/`
    /// filename it is scanning.
    async fn write_eligible(
        &self,
        client_id: &ClientId,
        job_id: &JobId,
        expires_at: ExpiresAt,
    ) -> anyhow::Result<()>;
    /// Delete all eligible markers for a client (used by `clients delete`).
    async fn delete_eligible_for_client(&self, client_id: &ClientId) -> anyhow::Result<()>;
    /// Delete a single eligible marker. No-op if absent. Used by the
    /// `queue-maintenance` GC sweep to drop one orphaned marker (whose job left
    /// `avail/`) while preserving the client's other, still-valid markers —
    /// distinct from `delete_eligible_for_client`, which drops the whole
    /// partition.
    async fn delete_eligible(
        &self,
        client_id: &ClientId,
        job_id: &JobId,
        expires_at: ExpiresAt,
    ) -> anyhow::Result<()>;
    /// List every `(client_id, job_id, expires_at)` triple across all clients
    /// (GC sweep). The `expires_at` lets the sweep address a marker's filename
    /// to delete an orphan.
    async fn list_all_eligible(&self) -> anyhow::Result<Vec<(ClientId, JobId, ExpiresAt)>>;

    // ── pending-reindex/ ────────────────────────────────────────────────────
    // Client-scoped reindex flags, one `{client_id}.{uuid}` key per request
    // (`todo_filename::pending_reindex_filename`). A write never overwrites:
    // the reindex pass consumes flags by deleting exactly the keys it captured
    // before rebuilding, so a flag written while a rebuild runs — a racing
    // profile change — has a different key, survives the run, and re-triggers
    // reindexing on the next one.
    /// Flag this client for an eligible-index rebuild (a new distinct key).
    async fn write_pending_reindex(&self, client_id: &ClientId) -> anyhow::Result<()>;
    /// Every outstanding flag as `(client_id, key)` — a client appears once
    /// per key. `key` addresses [`Self::delete_pending_reindex`].
    /// Unparseable names are foreign cruft: warned and skipped, never an
    /// error (every system-written flag parses by construction).
    async fn list_pending_reindex(&self) -> anyhow::Result<Vec<(ClientId, String)>>;
    /// Delete one flag by the exact key `list_pending_reindex` returned.
    /// Idempotent — the key may already be gone.
    async fn delete_pending_reindex(&self, key: &str) -> anyhow::Result<()>;
    /// Whether this client has any outstanding flag — the claim/reclaim/
    /// heartbeat/submission gate. An unreadable listing is an error, not
    /// `false`: the gate exists to stop claims against stale markers, so
    /// "couldn't check" must not read as "not flagged".
    async fn has_pending_reindex(&self, client_id: &ClientId) -> anyhow::Result<bool>;

    // ── pending-reindex-jobs/ ───────────────────────────────────────────────
    // Job-scoped deferred-reindex flags: a job that was leased while a client
    // reindex ran could not be re-evaluated (the rebuild reads `avail/`), so
    // its markers may be stale for *any* client. The flag records that debt;
    // the queue-maintenance settle pass re-matches the job against every
    // client once it is back in `avail/`, then clears it.
    async fn write_pending_reindex_job(&self, job_id: &JobId) -> anyhow::Result<()>;
    async fn list_pending_reindex_jobs(&self) -> anyhow::Result<Vec<JobId>>;
    async fn delete_pending_reindex_job(&self, job_id: &JobId) -> anyhow::Result<()>;

    // ── tmp/ ─────────────────────────────────────────────────────────────────
    /// Stage a complete job body at `tmp/{job_id}.json`
    /// ([`crate::todo_filename::tmp_filename`]) — the pre-promotion holding
    /// slot. A plain write, keyed by `job_id` alone: the body is not claimable
    /// until [`promote_avail`](Self::promote_avail) moves it into `avail/`, and
    /// the `expires_at` that shapes the `avail/` name is applied only then. This
    /// is the first server-side writer of the `todo/` queue (ingestion staging,
    /// `docs/plan-ingestion.md` §8); acting in the planner role, it stages every
    /// job body before promoting any. A failure *during staging* therefore
    /// leaves only orphaned `tmp/` files (reaped by the stale-`tmp/` cleanup)
    /// and nothing claimable. A failure *during the promotion loop* is a
    /// different case: already-promoted jobs sit in `avail/` under a
    /// still-`creating` manifest and are retired by the stuck-`creating`
    /// teardown (§8/§9), not by this staging step.
    async fn write_tmp(&self, job_id: &JobId, body: &serde_json::Value) -> anyhow::Result<()>;

    /// Atomically promote a staged job from `tmp/{job_id}.json` to
    /// `avail/{job_id}.{expires_at}.json` — the **same atomic rename mechanism**
    /// `claim_job` relies on (S3 Express `RenameObject`; local-fs `rename`), so a
    /// partial (tmp-only) write can never surface in `avail/`. `expires_at` is
    /// supplied by the caller (ingestion reads it from the body once) and encoded
    /// into the immutable `avail/` filename here. A missing `tmp/` source is an
    /// error (the caller staged it immediately before), not a silent no-op.
    async fn promote_avail(&self, job_id: &JobId, expires_at: ExpiresAt) -> anyhow::Result<()>;

    /// List `tmp/` object keys whose last-modified time is older than `age`.
    async fn list_stale_tmp(&self, age: Duration) -> anyhow::Result<Vec<String>>;
    async fn delete_tmp_object(&self, key: &str) -> anyhow::Result<()>;

    // ── eligible index cursor ────────────────────────────────────────────────
    /// Read the persisted avail/ scan cursor (a raw filename key, or `None` on
    /// first run). Stored at `todo/.eligible-cursor`.
    async fn read_eligible_cursor(&self) -> anyhow::Result<Option<String>>;
    async fn write_eligible_cursor(&self, key: &str) -> anyhow::Result<()>;

    // ── GC orphan candidates ─────────────────────────────────────────────────
    /// Read the persisted orphan-candidate set: the storage keys the previous
    /// `queue-maintenance` run saw orphaned but did not yet delete (the
    /// reconciliation sweep acts only on the second consecutive sighting).
    /// Keys span every reconciled subtree, so a single set covers all of them.
    /// Missing state reads as empty (first run); corrupt state also reads as
    /// empty — the safe direction, since an empty set only delays marker GC
    /// by one run and the set is rewritten wholesale every run. Stored at
    /// `todo/.gc-candidates`.
    async fn read_gc_candidates(&self) -> anyhow::Result<HashSet<String>>;
    /// Overwrite the persisted orphan-candidate set.
    async fn write_gc_candidates(&self, candidates: &HashSet<String>) -> anyhow::Result<()>;

    // ── suspended/ ──────────────────────────────────────────────────────────
    /// Write `todo/suspended/{client_id}.json`.
    async fn write_suspension(
        &self,
        client_id: &ClientId,
        suspended_at: DateTime<Utc>,
        conflicting_job_id: &JobId,
    ) -> anyhow::Result<()>;
    async fn read_suspension(
        &self,
        client_id: &ClientId,
    ) -> anyhow::Result<Option<SuspensionRecord>>;
    async fn delete_suspension(&self, client_id: &ClientId) -> anyhow::Result<()>;
    async fn list_suspensions(&self) -> anyhow::Result<Vec<(ClientId, SuspensionRecord)>>;

    // ── startup validation ────────────────────────────────────────────────────
    /// Fail fast at startup if this backend can't perform the atomic renames the
    /// queue's claim/renew safety depends on. Called by every command that
    /// touches `todo/`. `local_fs` is trivially atomic (POSIX `rename`); the S3
    /// impl probes the bucket — see it for the mechanics, and
    /// `docs/operations.md` for the S3 Express One Zone requirement.
    async fn validate_backend(&self) -> anyhow::Result<()>;
}

/// A client's suspension record, stored at
/// `todo/suspended/{client_id}.json` in `[todo_storage]`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SuspensionRecord {
    pub suspended_at: DateTime<Utc>,
    pub conflicting_job_id: JobId,
}

#[derive(Clone)]
pub struct Stores {
    pub catalog: Arc<dyn CatalogStore>,
    pub auth: Arc<dyn AuthStore>,
    pub submissions: Arc<dyn SubmissionStore>,
    pub warehouse: Arc<dyn WarehouseStore>,
    pub eval_sample_results: Arc<dyn EvalSampleResultStore>,
    pub todo: Arc<dyn TodoStore>,
    pub plans: Arc<dyn PlanStore>,
}

/// Best-effort removal of a client's `todo/` queue state when its identity is
/// deleted (`clients delete`): the suspension record, every eligible-index
/// marker, and every pending-reindex flag.
///
/// Each failure is logged and skipped rather than propagated: by the time this
/// runs the client identity is already gone, so the command has done its
/// essential work, and aborting would only mislead the operator into thinking
/// the delete itself failed. A leftover marker is inert regardless — the
/// deleted client can no longer authenticate, so nothing can be claimed against
/// it. This purge is only the fast path; `queue-maintenance`'s orphan
/// reconciliation is the authoritative backstop, and it converges regardless
/// of how a marker was orphaned: its sweep drives off client liveness (the
/// auth roster) as well as job liveness, so within at most two runs every
/// marker of a client absent from the roster is collected — whether this purge
/// failed partway, the command never ran, or a marker was orphaned some other
/// way. `clients delete` is also idempotent, so re-running it retries the whole
/// teardown immediately rather than waiting for the sweep.
///
/// Pending-reindex flags are flat `{client_id}.{uuid}` keys and a client may
/// hold several (one per profile change), so they are enumerated and filtered
/// by client — mirroring the deleted-client path in `queue_maintenance`.
pub async fn purge_client_todo_state(todo: &dyn TodoStore, client_id: &ClientId) {
    if let Err(e) = todo.delete_suspension(client_id).await {
        tracing::warn!(client_id = %client_id, error = %e,
            "failed to delete suspension record while deleting client");
    }
    if let Err(e) = todo.delete_eligible_for_client(client_id).await {
        tracing::warn!(client_id = %client_id, error = %e,
            "failed to delete eligible markers while deleting client");
    }
    match todo.list_pending_reindex().await {
        Ok(flags) => {
            for (_, key) in flags
                .into_iter()
                .filter(|(flagged, _)| flagged == client_id)
            {
                if let Err(e) = todo.delete_pending_reindex(&key).await {
                    tracing::warn!(client_id = %client_id, key = %key, error = %e,
                        "failed to delete pending-reindex flag while deleting client");
                }
            }
        }
        Err(e) => tracing::warn!(client_id = %client_id, error = %e,
            "failed to list pending-reindex flags while deleting client"),
    }
}

/// Test-only no-op `TodoStore` for `Stores` fixtures that never touch the
/// `todo/` queue. Panics on every method call, so a test that unexpectedly
/// exercises the queue fails loudly rather than passing against dead state.
#[cfg(test)]
pub struct TodoStoreUnimplemented;

#[cfg(test)]
#[async_trait]
#[allow(unused_variables)]
impl TodoStore for TodoStoreUnimplemented {
    async fn list_avail(
        &self,
        start_after: Option<&str>,
        limit: NonZeroUsize,
    ) -> anyhow::Result<Vec<String>> {
        unimplemented!()
    }
    async fn get_avail(
        &self,
        job_id: &JobId,
        expires_at: ExpiresAt,
    ) -> anyhow::Result<Option<serde_json::Value>> {
        unimplemented!()
    }
    async fn delete_avail(&self, job_id: &JobId, expires_at: ExpiresAt) -> anyhow::Result<()> {
        unimplemented!()
    }
    async fn delete_avail_by_job(&self, job_id: &JobId) -> anyhow::Result<()> {
        unimplemented!()
    }
    async fn get_avail_by_job(&self, job_id: &JobId) -> anyhow::Result<Option<serde_json::Value>> {
        unimplemented!()
    }
    async fn list_leased(&self) -> anyhow::Result<Vec<String>> {
        unimplemented!()
    }
    async fn list_leased_for_client(&self, client_id: &ClientId) -> anyhow::Result<Vec<String>> {
        unimplemented!()
    }
    async fn get_leased(
        &self,
        job_id: &JobId,
        client_id: &ClientId,
        lease_expiry: DateTime<Utc>,
    ) -> anyhow::Result<Option<serde_json::Value>> {
        unimplemented!()
    }
    async fn renew_lease(
        &self,
        job_id: &JobId,
        client_id: &ClientId,
        new_expiry: DateTime<Utc>,
    ) -> anyhow::Result<RenewLeaseResult> {
        unimplemented!()
    }
    async fn delete_lease(
        &self,
        job_id: &JobId,
        client_id: &ClientId,
        expiry: DateTime<Utc>,
    ) -> anyhow::Result<()> {
        unimplemented!()
    }
    async fn claim_job(
        &self,
        job_id: &JobId,
        expires_at: ExpiresAt,
        client_id: &ClientId,
        lease_expiry: DateTime<Utc>,
    ) -> anyhow::Result<ClaimResult> {
        unimplemented!()
    }
    async fn recycle_lease(
        &self,
        job_id: &JobId,
        client_id: &ClientId,
        lease_expiry: DateTime<Utc>,
    ) -> anyhow::Result<RecycleResult> {
        unimplemented!()
    }
    async fn write_denied(&self, job_id: &JobId, client_id: &ClientId) -> anyhow::Result<()> {
        unimplemented!()
    }
    async fn list_denied_for_job(&self, job_id: &JobId) -> anyhow::Result<Vec<ClientId>> {
        unimplemented!()
    }
    async fn delete_denied_for_job(&self, job_id: &JobId) -> anyhow::Result<()> {
        unimplemented!()
    }
    async fn delete_denied(&self, job_id: &JobId, client_id: &ClientId) -> anyhow::Result<()> {
        unimplemented!()
    }
    async fn list_all_denied(&self) -> anyhow::Result<Vec<(JobId, ClientId)>> {
        unimplemented!()
    }
    async fn list_eligible_for_client(
        &self,
        client_id: &ClientId,
    ) -> anyhow::Result<Vec<(JobId, ExpiresAt)>> {
        unimplemented!()
    }
    async fn write_eligible(
        &self,
        client_id: &ClientId,
        job_id: &JobId,
        expires_at: ExpiresAt,
    ) -> anyhow::Result<()> {
        unimplemented!()
    }
    async fn delete_eligible_for_client(&self, client_id: &ClientId) -> anyhow::Result<()> {
        unimplemented!()
    }
    async fn delete_eligible(
        &self,
        client_id: &ClientId,
        job_id: &JobId,
        expires_at: ExpiresAt,
    ) -> anyhow::Result<()> {
        unimplemented!()
    }
    async fn list_all_eligible(&self) -> anyhow::Result<Vec<(ClientId, JobId, ExpiresAt)>> {
        unimplemented!()
    }
    async fn write_pending_reindex(&self, client_id: &ClientId) -> anyhow::Result<()> {
        unimplemented!()
    }
    async fn list_pending_reindex(&self) -> anyhow::Result<Vec<(ClientId, String)>> {
        unimplemented!()
    }
    async fn delete_pending_reindex(&self, key: &str) -> anyhow::Result<()> {
        unimplemented!()
    }
    async fn has_pending_reindex(&self, client_id: &ClientId) -> anyhow::Result<bool> {
        unimplemented!()
    }
    async fn write_pending_reindex_job(&self, job_id: &JobId) -> anyhow::Result<()> {
        unimplemented!()
    }
    async fn list_pending_reindex_jobs(&self) -> anyhow::Result<Vec<JobId>> {
        unimplemented!()
    }
    async fn delete_pending_reindex_job(&self, job_id: &JobId) -> anyhow::Result<()> {
        unimplemented!()
    }
    async fn write_tmp(&self, job_id: &JobId, body: &serde_json::Value) -> anyhow::Result<()> {
        unimplemented!()
    }
    async fn promote_avail(&self, job_id: &JobId, expires_at: ExpiresAt) -> anyhow::Result<()> {
        unimplemented!()
    }
    async fn list_stale_tmp(&self, age: Duration) -> anyhow::Result<Vec<String>> {
        unimplemented!()
    }
    async fn delete_tmp_object(&self, key: &str) -> anyhow::Result<()> {
        unimplemented!()
    }
    async fn read_eligible_cursor(&self) -> anyhow::Result<Option<String>> {
        unimplemented!()
    }
    async fn write_eligible_cursor(&self, key: &str) -> anyhow::Result<()> {
        unimplemented!()
    }
    async fn read_gc_candidates(&self) -> anyhow::Result<HashSet<String>> {
        unimplemented!()
    }
    async fn write_gc_candidates(&self, _candidates: &HashSet<String>) -> anyhow::Result<()> {
        unimplemented!()
    }
    async fn write_suspension(
        &self,
        client_id: &ClientId,
        suspended_at: DateTime<Utc>,
        conflicting_job_id: &JobId,
    ) -> anyhow::Result<()> {
        unimplemented!()
    }
    async fn read_suspension(
        &self,
        client_id: &ClientId,
    ) -> anyhow::Result<Option<SuspensionRecord>> {
        unimplemented!()
    }
    async fn delete_suspension(&self, client_id: &ClientId) -> anyhow::Result<()> {
        unimplemented!()
    }
    async fn list_suspensions(&self) -> anyhow::Result<Vec<(ClientId, SuspensionRecord)>> {
        unimplemented!()
    }
    async fn validate_backend(&self) -> anyhow::Result<()> {
        unimplemented!()
    }
}

/// Test-only no-op `PlanStore` for `Stores` fixtures that never touch the
/// `plans/` domain (mirrors [`TodoStoreUnimplemented`]). Panics on every call,
/// so a test that unexpectedly exercises the plan store fails loudly. The
/// round-trip is covered against the real `local_fs` / S3 impls instead.
#[cfg(test)]
pub struct PlanStoreUnimplemented;

#[cfg(test)]
#[async_trait]
#[allow(unused_variables)]
impl PlanStore for PlanStoreUnimplemented {
    async fn put_plan(&self, manifest: &PlanManifest) -> anyhow::Result<()> {
        unimplemented!()
    }
    async fn get_plan(&self, plan_id: &PlanId) -> anyhow::Result<Option<PlanManifest>> {
        unimplemented!()
    }
    async fn list_plans(&self, status: Option<PlanStatus>) -> anyhow::Result<Vec<PlanManifest>> {
        unimplemented!()
    }
    async fn delete_plan(&self, plan_id: &PlanId) -> anyhow::Result<()> {
        unimplemented!()
    }
    async fn write_cancel_marker(&self, plan_id: &PlanId) -> anyhow::Result<()> {
        unimplemented!()
    }
    async fn has_cancel_marker(&self, plan_id: &PlanId) -> anyhow::Result<bool> {
        unimplemented!()
    }
    async fn list_cancel_markers(&self) -> anyhow::Result<Vec<PlanId>> {
        unimplemented!()
    }
    async fn delete_cancel_marker(&self, plan_id: &PlanId) -> anyhow::Result<()> {
        unimplemented!()
    }
}

/// Outcome of [`reindex_tags`]: how many derived markers were repaired.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ReindexReport {
    /// Markers deleted — reverse entries with no backing forward marker, plus
    /// forward markers belonging to a since-deleted client (both trees).
    pub removed: usize,
    /// Reverse markers created for a valid forward membership that was missing
    /// its reverse counterpart.
    pub added: usize,
}

/// Reconcile the derived reverse tag index against the authoritative forward
/// tree, repairing any drift from a crash mid-write (the forward tree commits
/// first, so only the reverse can lag — see [`AuthStore`]'s Tags section).
///
/// The forward tree is truth *for existing clients*; `clients/{id}.json` is the
/// existence authority, so a forward marker whose client record is gone is an
/// orphan and is dropped. Idempotent and convergent: safe to run on a cron or
/// after a suspected crash, and a no-op when the trees already agree.
pub async fn reindex_tags(store: &dyn AuthStore) -> anyhow::Result<ReindexReport> {
    let records: BTreeSet<ClientId> = store
        .list_clients()
        .await?
        .into_iter()
        .map(|c| c.client_id)
        .collect();
    let forward: BTreeSet<(ClientId, Tag)> = store
        .list_forward_tag_markers()
        .await?
        .into_iter()
        .collect();
    let reverse: BTreeSet<(ClientId, Tag)> = store
        .list_reverse_tag_markers()
        .await?
        .into_iter()
        .collect();

    // Valid memberships = forward markers of clients that still exist.
    let valid: BTreeSet<(ClientId, Tag)> = forward
        .iter()
        .filter(|(id, _)| records.contains(id))
        .cloned()
        .collect();

    // Anything present but not valid must go from both trees; any valid
    // membership missing its reverse marker must be created.
    let to_remove: BTreeSet<(ClientId, Tag)> = forward
        .difference(&valid)
        .chain(reverse.difference(&valid))
        .cloned()
        .collect();
    let to_add: Vec<(ClientId, Tag)> = valid.difference(&reverse).cloned().collect();

    let report = ReindexReport {
        removed: to_remove.len(),
        added: to_add.len(),
    };
    // Repairs are independent per-(client, tag) writes over disjoint marker
    // keys, so apply them with bounded concurrency rather than one serial
    // round-trip each. `to_remove` and `to_add` touch disjoint pairs, so the
    // two phases could overlap; kept sequential for a simpler failure story.
    futures::stream::iter(
        to_remove
            .iter()
            .map(|(id, tag)| store.remove_client_tag(id, tag)),
    )
    .buffer_unordered(STORAGE_CONCURRENCY)
    .try_collect::<Vec<_>>()
    .await?;
    futures::stream::iter(to_add.iter().map(|(id, tag)| store.add_client_tag(id, tag)))
        .buffer_unordered(STORAGE_CONCURRENCY)
        .try_collect::<Vec<_>>()
        .await?;
    Ok(report)
}

/// Max in-flight object-store requests for a fan-out over many keys (tag
/// reindex/cleanup, `clients list` fetches, and similar). Bounds concurrency so
/// a large fleet can't fire thousands of simultaneous S3 requests at once.
pub const STORAGE_CONCURRENCY: usize = 16;

/// Shared assertion for the `AuthStore` signature-migration surface, run
/// against every backend so the impls are held to one contract.
///
/// The property that matters is that the marker records the *first* `v1`
/// signature and is not moved by later ones: a client migrates once, and if a
/// repeat write reset `first_seen` the operator view would report the client's
/// last request instead of its migration. Each backend reaches that through a
/// different primitive, so both are held to it here.
#[cfg(test)]
pub(crate) async fn assert_signature_migration_keeps_first_sighting(
    store: &dyn AuthStore,
) -> anyhow::Result<()> {
    let client_id = ClientId::try_new("ev1_migrating")?;
    let first = Utc::now();
    let later = first + chrono::TimeDelta::seconds(60);

    assert!(!store.has_signature_migration(&client_id).await?);
    assert_eq!(
        store.record_signature_migration(&client_id, first).await?,
        MigrationRecord::First
    );
    assert_eq!(
        store.record_signature_migration(&client_id, later).await?,
        MigrationRecord::Existing,
        "a second record must not present as a fresh migration"
    );
    assert!(store.has_signature_migration(&client_id).await?);

    let listed = store.list_signature_migrations().await?;
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].0, client_id);
    assert_eq!(listed[0].1.first_seen, first);
    Ok(())
}

/// Shared assertion for the `AuthStore` tag surface, run against every backend
/// so the impls are held to one contract. Assumes `a` and `b` are two existing,
/// distinct, initially-untagged clients with `a < b` (for the sorted
/// reverse-lookup assertion).
#[cfg(test)]
pub(crate) async fn assert_tag_store_roundtrip(
    store: &dyn AuthStore,
    a: &ClientId,
    b: &ClientId,
) -> anyhow::Result<()> {
    let team = Tag::try_new("team-mobile")?;
    let east = Tag::try_new("us-east")?;

    store.add_client_tag(a, &team).await?;
    store.add_client_tag(a, &east).await?;
    store.add_client_tag(b, &team).await?;
    store.add_client_tag(a, &team).await?; // idempotent re-add

    // Reverse: tag -> clients (ascending by client_id).
    assert_eq!(
        store.list_client_ids_by_tag(&team).await?,
        vec![a.clone(), b.clone()]
    );
    assert_eq!(store.list_client_ids_by_tag(&east).await?, vec![a.clone()]);
    // Forward: client -> tags.
    assert_eq!(
        store.get_client_tags(a).await?,
        BTreeSet::from([team.clone(), east.clone()])
    );

    // Removing one membership drops it from both directions.
    store.remove_client_tag(a, &east).await?;
    assert!(store.list_client_ids_by_tag(&east).await?.is_empty());
    assert_eq!(
        store.get_client_tags(a).await?,
        BTreeSet::from([team.clone()])
    );

    // delete_client clears the client from both trees.
    store.delete_client(b).await?;
    assert_eq!(store.list_client_ids_by_tag(&team).await?, vec![a.clone()]);
    assert!(store.get_client_tags(b).await?.is_empty());

    // Unknown tag -> empty.
    assert!(
        store
            .list_client_ids_by_tag(&Tag::try_new("nope")?)
            .await?
            .is_empty()
    );
    Ok(())
}

pub use local_fs::{build_local_fs_auth_store, build_local_fs_stores, build_local_fs_todo_store};
pub use s3::{build_s3_auth_store, build_s3_object_store, build_s3_stores, build_s3_todo_store};

// Concrete local_fs stores, exposed crate-internally for unit tests that drive
// store-backed handler helpers directly (the module is otherwise private).
#[cfg(test)]
pub(crate) use local_fs::{LocalFsSubmissionStore, LocalFsTodoStore};
