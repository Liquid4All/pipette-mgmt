use std::path::PathBuf;
use std::sync::Arc;

use clap::{Parser, Subcommand};
use futures::stream::{self, StreamExt, TryStreamExt};
use pipette_mgmt::client::ClientStatus;
use pipette_mgmt::config::{Config, StorageConfig};
use pipette_mgmt::handlers::AppState;
use pipette_mgmt::model_params::ModelCatalog;
use pipette_mgmt::plan::{PlanStatus, PlanStatusView};
use pipette_mgmt::plan_cli::{self, CancelOutcome, PlanRef};
use pipette_mgmt::preauth::{self, MintParams, PreauthUsage};
use pipette_mgmt::score;
use pipette_mgmt::storage_lock::{self, StorageLock};
use pipette_mgmt::stores::{
    AuthStore, STORAGE_CONCURRENCY, Stores, build_local_fs_auth_store, build_local_fs_stores,
    build_local_fs_todo_store, build_s3_auth_store, build_s3_stores, build_s3_todo_store,
    purge_client_todo_state,
};
use pipette_mgmt::types::{BenchmarkId, ClientId, PlanId, PreauthKeyId};
use pipette_mgmt::validated::{NonEmptyTrimmedString, Tag};
use pipette_mgmt::{fix_canonical, fix_model_param, queue_maintenance, requeue_eval};

#[derive(Parser)]
#[command(
    name = "pipette-mgmt",
    about = "Pipette — edge benchmark management server",
    version = pipette_mgmt::BUILD_VERSION
)]
struct Cli {
    /// Path to TOML configuration file
    #[arg(long, env = "PIPETTE_MGMT_CONFIG")]
    config: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Start the HTTP server
    Serve,
    /// Fast pass: route eval submissions to the score-queue, score non-eval
    /// submissions, and finalize already-scored evals. Does not call the
    /// scoring service. Run frequently. Aliased as `score` for back-compat
    /// with existing crons.
    #[command(visible_alias = "score")]
    ProcessSubmissions,
    /// Slow pass: drain the score-queue, calling the scoring service for each
    /// eval job and staging the result for the next `process-submissions` run
    /// to finalize. Runs serially on its own schedule; takes no mutate lock.
    ScoreEval,
    /// Backfill warehouse `model_params_*_millions` columns from the catalog
    FixModelParam {
        /// Count matching rows without rewriting Parquet files
        #[arg(long)]
        dry_run: bool,
        /// Restrict the rewrite to rows for these model name(s). Repeatable
        /// (`--model A --model B`) and comma-separated (`--model A,B`) — both
        /// accumulate. Names are normalized like catalog lookups, so `--model
        /// LiquidAI/LFM2.5-230M` also matches the `-GGUF` and quant variants.
        /// Omit to process every row.
        #[arg(long = "model", value_name = "MODEL_NAME", value_delimiter = ',')]
        models: Vec<String>,
    },
    /// Re-canonicalize the warehouse's opaque JSON columns
    /// (`model_descriptor`, `runtime_descriptor`, `benchmark_flags`,
    /// `model_flags`, `runtime_flags`) and recompute their `_sha256` ids
    FixCanonical {
        /// Count matching rows without rewriting Parquet files
        #[arg(long)]
        dry_run: bool,
        /// Restrict the rewrite to these column(s). Repeatable (`--column A
        /// --column B`) and comma-separated (`--column A,B`) — both
        /// accumulate. An unknown name is an error. Omit to process every
        /// opaque column.
        #[arg(long = "column", value_name = "COLUMN", value_delimiter = ',')]
        columns: Vec<String>,
    },
    /// Manage clients
    Clients {
        #[command(subcommand)]
        action: ClientAction,
    },
    /// Copy already-scored submissions for an eval benchmark back into
    /// `incoming/` as fresh submissions so the next scoring passes
    /// (`process-submissions` + `score-eval`) score them again — use after a
    /// scorer fix. The benchmark is looked up in the catalog and the command
    /// errors unless it exists and is an eval; matching warehouse jobs (by
    /// `benchmark_id`) are then re-staged.
    /// Each re-stage mints a new `job_id` and `submitted_at = now`; the
    /// original `job_id`'s processed archive and warehouse rows are left
    /// untouched alongside the fresh ones. Because of that, re-running over
    /// the whole benchmark doubles the re-staged set each time — use
    /// `--submitted-before` to scope a run to the original (pre-migration)
    /// jobs and avoid that.
    RequeueEval {
        /// Benchmark id to re-score (e.g. `eval_ifbench_original`) — a
        /// catalog key, not the bare eval id. Must resolve to an eval.
        #[arg(long, value_parser = parse_benchmark_id)]
        benchmark_id: BenchmarkId,
        /// Only re-stage jobs submitted at or after this time. RFC3339
        /// (e.g. `2026-06-01T00:00:00Z`) or a bare `YYYY-MM-DD` (midnight UTC).
        #[arg(long, value_parser = parse_timestamp_micros)]
        submitted_after: Option<i64>,
        /// Only re-stage jobs submitted at or before this time (same formats
        /// as `--submitted-after`). Set it to just before the migration
        /// started to exclude already-re-staged copies (whose `submitted_at`
        /// is "now") and avoid doubling on re-runs.
        #[arg(long, value_parser = parse_timestamp_micros)]
        submitted_before: Option<i64>,
        /// Only re-stage jobs whose recorded `score_runtime_version` matches
        /// exactly. For evals this is the client's on-device runtime version,
        /// not a scoring-service version.
        #[arg(long)]
        score_runtime_version: Option<String>,
        /// List what would be re-staged without writing anything.
        #[arg(long)]
        dry_run: bool,
    },
    /// Inspect and clear the storage mutate lock — the advisory lock
    /// that serializes `process-submissions`, `fix-model-param`, and
    /// `requeue-eval`.
    /// Use this only to recover from a crashed command that left a stale
    /// lock behind.
    Unlock {
        /// Clear the lock even if its lease is still active.
        #[arg(long)]
        force: bool,
    },
    /// Manage the write-only unverified submission archive
    /// (`submissions/unverified/`). See `docs/storage.md` §4.1.
    Unverified {
        #[command(subcommand)]
        action: UnverifiedAction,
    },
    /// Maintain the `todo/` job queue: recycle expired leases, expire jobs
    /// past their deadline (writing synthetic failure records), rebuild the
    /// `eligible/` index from `avail/` jobs and registered client device
    /// profiles, GC orphaned markers, and delete stale `tmp/` files. Run on a
    /// cron; see `docs/operations.md` §3.1–3.2. Writes only to `todo/` and
    /// `submissions/processed/`, so it takes no mutate lock.
    QueueMaintenance,
    /// Rebuild the reverse tag index (`tags-index/by-tag/`) from the
    /// authoritative forward tree, repairing any drift (e.g. from a crash
    /// mid-write). Idempotent — safe to run any time. The `todo/eligible/`
    /// index is rebuilt separately by `queue-maintenance`.
    Reindex,
    /// Mint and manage pre-auth registration keys. A client presenting a valid
    /// key at registration is auto-approved (and optionally seeded with the
    /// key's tags / organization). See `docs/authentication.md` §6.
    Preauth {
        #[command(subcommand)]
        action: PreauthAction,
    },
    /// Ingest and administer plans. A plan is a set of jobs expanded by
    /// `pipette-plan` into a directory of job-body files, ingested here as a
    /// unit and tracked by a manifest. See `docs/plan-ingestion.md`.
    Plans {
        #[command(subcommand)]
        action: PlanAction,
    },
}

#[derive(Subcommand)]
enum PlanAction {
    /// Ingest a directory of job files as one plan: validate the whole set,
    /// mint the plan and job ids, and stage the jobs into the `todo/` queue.
    /// Prints the ingest report as JSON on stdout — the same shape
    /// `POST /plans` returns. Nothing is written if any job is rejected.
    Ingest {
        /// Directory of `*.json` job bodies, as written by `pipette-plan
        /// generate --out <dir>`. Every `*.json` file is a job; other files are
        /// ignored, subdirectories are not descended into, and the directory is
        /// left unmodified.
        dir: PathBuf,
        /// Optional label for human reference, carried on the plan manifest.
        /// Not an identity — two plans may share a name.
        #[arg(long)]
        plan_name: Option<String>,
    },
    /// List plans, newest first.
    List {
        /// Show only plans in this lifecycle state (`creating`, `active`,
        /// `pending_clients`, `complete`, `cancelled`). Omit for all.
        #[arg(long)]
        status: Option<PlanStatusArg>,
    },
    /// Show one plan's progress: its lifecycle status, any pending
    /// cancellation, the ingestion-time fleet-match warnings, and the progress
    /// snapshot `queue-maintenance` last computed.
    Status {
        /// The plan id (`plan-{uuid}`), as printed by `plans ingest`.
        #[arg(value_parser = parse_plan_id, required_unless_present = "plan_name")]
        plan_id: Option<PlanId>,
        /// Look the plan up by its `--plan-name` label instead. Scans every
        /// manifest, and fails if the name matches no plan or more than one.
        #[arg(long, conflicts_with = "plan_id")]
        plan_name: Option<String>,
    },
    /// Record a cancellation request for a plan. Writes a cancel marker only —
    /// automatic job teardown is not implemented yet, so the plan's jobs stay
    /// claimable until the `queue-maintenance` cancel pass ships.
    Cancel {
        /// The plan id (`plan-{uuid}`) to cancel.
        #[arg(value_parser = parse_plan_id, required_unless_present = "plan_name")]
        plan_id: Option<PlanId>,
        /// Cancel by `--plan-name` label instead. Scans every manifest, and
        /// fails if the name matches no plan or more than one — a name is a
        /// label, not an identity, so an ambiguous one is never guessed at.
        #[arg(long, conflicts_with = "plan_id")]
        plan_name: Option<String>,
    },
}

/// `--status` values for `plans list`.
///
/// `rename_all` is load-bearing: clap's derive defaults to kebab-case, which
/// would accept `pending-clients` while every other surface — `plans status`,
/// the manifest, the docs — spells it `pending_clients`. An operator filtering
/// by the status they just read should not have to translate it.
#[derive(Clone, Copy, clap::ValueEnum)]
#[value(rename_all = "snake_case")]
enum PlanStatusArg {
    Creating,
    Active,
    PendingClients,
    Complete,
    Cancelled,
}

impl From<PlanStatusArg> for PlanStatus {
    fn from(arg: PlanStatusArg) -> Self {
        match arg {
            PlanStatusArg::Creating => PlanStatus::Creating,
            PlanStatusArg::Active => PlanStatus::Active,
            PlanStatusArg::PendingClients => PlanStatus::PendingClients,
            PlanStatusArg::Complete => PlanStatus::Complete,
            PlanStatusArg::Cancelled => PlanStatus::Cancelled,
        }
    }
}

#[derive(Subcommand)]
enum PreauthAction {
    /// Mint a key and print its token **once** (the secret is never stored or
    /// shown again).
    Create {
        /// Let the key be used more than once (until it expires or is revoked).
        /// Without this flag the key is single-use.
        #[arg(long)]
        multi_use: bool,
        /// Expire the key after this long (e.g. `30d`, `24h`). Defaults to 90d
        /// when omitted; pass --no-expiry for a permanent key.
        #[arg(long, value_parser = parse_duration, conflicts_with = "no_expiry")]
        expires_in: Option<std::time::Duration>,
        /// Mint a key that never expires. Discouraged: a leaked permanent key
        /// stays valid until manually revoked.
        #[arg(long)]
        no_expiry: bool,
        /// Tag to apply to every client registering with this key. Repeatable.
        #[arg(long = "tag", value_name = "TAG")]
        tags: Vec<String>,
        /// Organization to stamp on clients registering with this key.
        #[arg(long)]
        org: Option<String>,
        /// Free-form note stored with the key (shown in `preauth list`).
        #[arg(long)]
        note: Option<String>,
    },
    /// List all pre-auth keys (metadata only — never the secret).
    List,
    /// Revoke a key so it can no longer be used.
    Revoke {
        /// Key id (the part between `preauth_` and `.` in the token).
        key_id: String,
    },
    /// Delete keys that can no longer grant — expired, revoked, or spent
    /// single-use. Spent single-use keys usually delete themselves on consume;
    /// this reaps the stragglers plus expired-but-unused and revoked keys.
    Prune {
        /// Report what would be deleted without deleting anything.
        #[arg(long)]
        dry_run: bool,
    },
}

#[derive(Subcommand)]
enum UnverifiedAction {
    /// Delete held objects whose backend modification time (S3
    /// `LastModified` / filesystem `mtime`) is older than the given age,
    /// across all clients. Bounds the size of the archive on servers
    /// where the feature is enabled. Does not take the storage mutate
    /// lock — the unverified tree is disjoint from the warehouse and the
    /// scorer, so it is safe to run while `serve` and `score` are active.
    Prune {
        /// Age threshold (e.g. `7d`, `24h`, `30m`). Objects modified more
        /// recently are kept.
        #[arg(long, value_parser = parse_duration)]
        older_than: std::time::Duration,
        /// Print what would be deleted without removing anything.
        #[arg(long)]
        dry_run: bool,
    },
    /// Delete every held submission for one client. Use after rejecting a
    /// client whose held submissions should be discarded.
    Delete {
        /// Client id whose held submissions to delete.
        #[arg(long, value_parser = parse_client_id)]
        client_id: ClientId,
        /// Print what would be deleted without removing anything.
        #[arg(long)]
        dry_run: bool,
    },
    /// Move one client's held submissions into the normal pipeline so the
    /// scorer picks them up: `success` bodies land in `incoming/`,
    /// `failure` bodies in `processed/`. Use after approving a client
    /// whose earlier submissions were held. Each promoted object is
    /// removed from the unverified tree once re-staged.
    Promote {
        /// Client id whose held submissions to promote.
        #[arg(long, value_parser = parse_client_id)]
        client_id: ClientId,
        /// List what would be promoted without writing or deleting.
        #[arg(long)]
        dry_run: bool,
    },
}

#[derive(Subcommand)]
enum ClientAction {
    /// List all registered clients, optionally filtered by tag
    List {
        /// Filter to clients carrying this tag (served from the reverse tag
        /// index). Repeatable; a client must carry every `--tag` given (AND).
        #[arg(long = "tag", value_name = "TAG")]
        tags: Vec<String>,
    },
    /// Manage a client's tags. Tags are flat labels (e.g. `team-mobile`,
    /// `us-east`) assigned manually here on the mgmt side — clients never set
    /// their own.
    Tag {
        #[command(subcommand)]
        action: TagAction,
    },
    /// Approve a pending client
    Approve {
        /// Client ID to approve
        client_id: String,
    },
    /// Reject (delete) a pending client
    Reject {
        /// Client ID to reject
        client_id: String,
    },
    /// Delete a client identity
    Delete {
        /// Client ID to delete
        client_id: String,
    },
    /// List clients currently suspended from claiming (each holds a stale
    /// lease that must be reclaimed or expire before it can claim again)
    ListSuspended,
    /// Clear a client's suspension so it may claim jobs again
    Unsuspend {
        /// Client ID to unsuspend
        client_id: String,
    },
    /// Update mutable client details (organization, details, contact email)
    Update {
        /// Client ID to update
        client_id: String,
        /// New organization name
        #[arg(long)]
        organization: Option<String>,
        /// New free-form client details
        #[arg(long = "details")]
        client_details: Option<String>,
        /// New contact email
        #[arg(long = "email")]
        contact_email: Option<String>,
    },
}

#[derive(Subcommand)]
enum TagAction {
    /// Add one or more tags to a client (no-op for tags it already has)
    Add {
        /// Client ID to tag
        client_id: String,
        /// One or more flat tags, e.g. `team-mobile us-east`
        #[arg(required = true, value_name = "TAG")]
        tags: Vec<String>,
    },
    /// Remove one or more tags from a client (no-op for tags it lacks)
    Remove {
        /// Client ID to untag
        client_id: String,
        /// One or more tags to remove
        #[arg(required = true, value_name = "TAG")]
        tags: Vec<String>,
    },
    /// List a client's tags
    List {
        /// Client ID whose tags to list
        client_id: String,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    use std::io::IsTerminal;
    tracing_subscriber::fmt()
        .with_ansi(std::io::stdout().is_terminal())
        .init();

    let cli = Cli::parse();
    let config_path = cli.config.unwrap_or_else(|| {
        let system_config = PathBuf::from("/etc/pipette-mgmt/config.toml");
        if system_config.exists() {
            return system_config;
        }
        dirs::config_dir()
            .unwrap_or_default()
            .join("pipette-mgmt/config.toml")
    });
    let config = Config::load(&config_path)?;
    let lock_ttl = std::time::Duration::from_secs(config.mutate_lock_ttl_secs.get());

    match cli.command {
        Command::Serve => {
            let stores = build_stores(&config).await?;
            serve(config, stores).await
        }
        Command::ProcessSubmissions => {
            let stores = build_stores(&config).await?;
            run_locked(
                &config.storage,
                "mutate",
                "process-submissions",
                lock_ttl,
                score::run_process_submissions(&config, stores),
            )
            .await
        }
        Command::ScoreEval => {
            // Its own `score-eval` lock — NOT the shared `mutate` lock. This
            // serializes score-eval instances so two long runs can't both
            // score the same to_do job, while leaving the warehouse writers
            // (process-submissions / fix-model-param / requeue-eval) free to
            // run during a multi-minute /score call.
            let stores = build_stores(&config).await?;
            run_locked(
                &config.storage,
                "score-eval",
                "score-eval",
                lock_ttl,
                score::run_score_eval(&config, stores),
            )
            .await
        }
        Command::FixModelParam { dry_run, models } => {
            let stores = build_stores(&config).await?;
            let catalog =
                ModelCatalog::load(&config.storage, config.model_params_mapping_path.as_deref())
                    .await?;
            let totals = if dry_run {
                fix_model_param::run(&stores, &catalog, true, &models).await?
            } else {
                run_locked(
                    &config.storage,
                    "mutate",
                    "fix-model-param",
                    lock_ttl,
                    fix_model_param::run(&stores, &catalog, false, &models),
                )
                .await?
            };
            totals.log("fix-model-param", dry_run);
            Ok(())
        }
        Command::FixCanonical { dry_run, columns } => {
            let stores = build_stores(&config).await?;
            if dry_run {
                fix_canonical::run(&stores, true, &columns).await
            } else {
                run_locked(
                    &config.storage,
                    "mutate",
                    "fix-canonical",
                    lock_ttl,
                    fix_canonical::run(&stores, false, &columns),
                )
                .await
            }
        }
        Command::Clients { action } => {
            let stores = build_stores(&config).await?;
            handle_client_action(action, stores).await
        }
        Command::RequeueEval {
            benchmark_id,
            submitted_after,
            submitted_before,
            score_runtime_version,
            dry_run,
        } => {
            let stores = build_stores(&config).await?;
            let filters = requeue_eval::Filters {
                submitted_after,
                submitted_before,
                score_runtime_version,
            };
            // A dry run only reads (catalog + warehouse metrics + processed
            // bodies), so it skips the lock. A live run writes into
            // `incoming/` that `score` consumes — take the mutate lock to
            // serialize it against `score` and the `fix-*` commands.
            if dry_run {
                requeue_eval::run(&stores, &benchmark_id, &filters, true).await
            } else {
                run_locked(
                    &config.storage,
                    "mutate",
                    "requeue-eval",
                    lock_ttl,
                    requeue_eval::run(&stores, &benchmark_id, &filters, false),
                )
                .await
            }
        }
        Command::Unlock { force } => storage_lock::unlock(&config.storage, force).await,
        Command::Unverified { action } => {
            let stores = build_stores(&config).await?;
            // No mutate lock for any of these: the unverified tree is
            // disjoint from the warehouse and submission queues that
            // `score` / `fix-*` touch (see docs/storage.md §4.1).
            handle_unverified_action(action, stores).await
        }
        Command::QueueMaintenance => {
            let stores = build_stores(&config).await?;
            // Fail fast if the `todo/` backend can't do atomic claims before
            // mutating queue state.
            stores.todo.validate_backend().await?;
            // The expiry pass resolves `benchmark_type` for its synthetic
            // failure records from the catalog.
            let catalog = stores.catalog.load_catalog().await?;
            // No mutate lock: this writes only to `todo/` and (synthetic
            // failures) `submissions/processed/`, disjoint from the warehouse
            // and submission queues `score` / `fix-*` serialize on.
            queue_maintenance::run(
                &*stores.todo,
                &*stores.auth,
                &*stores.submissions,
                &catalog,
                std::time::Duration::from_secs(config.todo_tmp_max_age_secs.get()),
            )
            .await
        }
        Command::Reindex => {
            let stores = build_stores(&config).await?;
            let report = pipette_mgmt::stores::reindex_tags(&*stores.auth).await?;
            if report.removed == 0 && report.added == 0 {
                println!("Tag index already consistent; nothing to repair.");
            } else {
                println!(
                    "Reindexed tags: added {} reverse marker(s), removed {} stale marker(s).",
                    report.added, report.removed
                );
            }
            Ok(())
        }
        Command::Preauth { action } => {
            let stores = build_stores(&config).await?;
            handle_preauth_action(action, stores.auth).await
        }
        Command::Plans { action } => {
            let stores = build_stores(&config).await?;
            // No mutate lock: this writes only `plans/` and `todo/`, disjoint
            // from the warehouse and submission queues `score` / `fix-*`
            // serialize on — the same argument as `queue-maintenance`.
            handle_plan_action(action, &stores).await
        }
    }
}

/// Row for the `preauth list` table. Metadata only — the secret is never stored.
/// Related fields are paired into one column as two stacked lines (`tabled`
/// renders an embedded newline as a second line) to keep the table narrow.
#[derive(tabled::Tabled)]
struct PreauthRow {
    /// key_id, then created-at.
    #[tabled(rename = "Key ID")]
    key_id: String,
    /// status, then expires-at.
    #[tabled(rename = "Lifecycle")]
    lifecycle: String,
    #[tabled(rename = "Usage")]
    usage: String,
    /// seeded tags, then seeded org.
    #[tabled(rename = "Seeds")]
    seeds: String,
    #[tabled(rename = "Note")]
    note: String,
}

/// Default lifetime applied to a pre-auth key when neither --expires-in nor
/// --no-expiry is given, so a forgotten flag never yields a permanent key.
const DEFAULT_PREAUTH_TTL_DAYS: i64 = 90;

async fn handle_preauth_action(
    action: PreauthAction,
    auth_store: Arc<dyn AuthStore>,
) -> anyhow::Result<()> {
    match action {
        PreauthAction::Create {
            multi_use,
            expires_in,
            no_expiry,
            tags,
            org,
            note,
        } => {
            let usage = if multi_use {
                PreauthUsage::MultiUse
            } else {
                PreauthUsage::SingleUse
            };
            let now = chrono::Utc::now();
            // Never mint a permanent key by accident: apply a bounded default
            // TTL unless the operator explicitly opts out with --no-expiry.
            let ttl = match expires_in {
                Some(d) => Some(chrono::Duration::from_std(d)?),
                None if no_expiry => None,
                None => Some(chrono::Duration::days(DEFAULT_PREAUTH_TTL_DAYS)),
            };
            let expires_at = ttl.map(|d| now + d);
            let default_tags = tags
                .into_iter()
                .map(|t| Tag::try_new(t).map_err(|e| anyhow::anyhow!("--tag: {e}")))
                .collect::<Result<std::collections::BTreeSet<_>, _>>()?;
            let default_organization = org
                .map(|o| {
                    NonEmptyTrimmedString::try_new(o).map_err(|e| anyhow::anyhow!("--org: {e}"))
                })
                .transpose()?;

            let minted = preauth::mint(
                MintParams {
                    usage,
                    expires_at,
                    default_tags,
                    default_organization,
                    note,
                },
                now,
            )?;
            auth_store.put_preauth_key(&minted.key).await?;

            println!("Created pre-auth key {}.", minted.key.key_id);
            println!(
                "  usage: {}   expires: {}",
                match usage {
                    PreauthUsage::SingleUse => "single-use",
                    PreauthUsage::MultiUse => "multi-use",
                },
                match expires_at {
                    Some(at) => at.to_rfc3339(),
                    None => "never".to_string(),
                }
            );
            println!(
                "\nToken (shown once — store it now):\n\n  {}\n",
                minted.token
            );
        }
        PreauthAction::List => {
            let mut keys = auth_store.list_preauth_keys().await?;
            if keys.is_empty() {
                println!("No pre-auth keys.");
                return Ok(());
            }
            keys.sort_by_key(|k| std::cmp::Reverse(k.created_at));
            let now = chrono::Utc::now();
            let rows: Vec<PreauthRow> = keys.iter().map(|k| preauth_row(k, now)).collect();
            let mut table = tabled::Table::new(&rows);
            table.with(tabled::settings::Style::psql());
            println!("{} pre-auth key(s):\n{table}", keys.len());
        }
        PreauthAction::Revoke { key_id } => {
            // Revoke is a delete — the record has no mutable "revoked" state.
            let key_id = PreauthKeyId::try_new(key_id)?;
            auth_store.delete_preauth_key(&key_id).await?;
            println!("Revoked (deleted) pre-auth key {key_id}.");
        }
        PreauthAction::Prune { dry_run } => {
            let now = chrono::Utc::now();
            // Records are listed before markers. A key spent in between shows up
            // in `keys` and so is treated as still held, which errs toward
            // keeping a marker — the safe direction.
            let keys = auth_store.list_preauth_keys().await?;
            // Expiry is the only reason a *record* is prunable; spent single-use
            // and revoked keys have already had theirs deleted.
            let expired: Vec<PreauthKeyId> = keys
                .iter()
                .filter(|k| k.is_expired(now))
                .map(|k| k.key_id.clone())
                .collect();

            // A spent marker is what keeps a single-use key spent, so one may
            // only be dropped once that key has no record left to consume.
            // Deleting a marker whose record survives would revive the key.
            let held: std::collections::HashSet<&PreauthKeyId> =
                keys.iter().map(|k| &k.key_id).collect();
            let orphaned: Vec<PreauthKeyId> = auth_store
                .list_spent_markers()
                .await?
                .into_iter()
                .filter(|id| !held.contains(id))
                .collect();

            if expired.is_empty() && orphaned.is_empty() {
                println!("Nothing to prune.");
                return Ok(());
            }

            let verb = if dry_run { "Would prune" } else { "Pruned" };
            if !dry_run {
                stream::iter(expired.iter().map(|id| {
                    let auth_store = auth_store.clone();
                    let id = id.clone();
                    async move { auth_store.delete_preauth_key(&id).await }
                }))
                .buffer_unordered(STORAGE_CONCURRENCY)
                .try_collect::<Vec<_>>()
                .await?;
                stream::iter(orphaned.iter().map(|id| {
                    let auth_store = auth_store.clone();
                    let id = id.clone();
                    async move { auth_store.delete_spent_marker(&id).await }
                }))
                .buffer_unordered(STORAGE_CONCURRENCY)
                .try_collect::<Vec<_>>()
                .await?;
            }
            if !expired.is_empty() {
                println!("{verb} {} expired pre-auth key(s):", expired.len());
                expired.iter().for_each(|id| println!("  {id}"));
            }
            if !orphaned.is_empty() {
                println!("{verb} {} spent-key marker(s):", orphaned.len());
                orphaned.iter().for_each(|id| println!("  {id}"));
            }
        }
    }
    Ok(())
}

/// Row for the `plans list` table. Progress figures come from the manifest's
/// cached `progress_snapshot`, which only `queue-maintenance` writes — a plan
/// ingested before that pass has run has none yet.
#[derive(tabled::Tabled)]
struct PlanRow {
    #[tabled(rename = "Plan ID")]
    plan_id: String,
    #[tabled(rename = "Name")]
    name: String,
    /// Lifecycle status, flagged when a cancel is requested but not yet latched.
    #[tabled(rename = "Status")]
    status: String,
    #[tabled(rename = "Created")]
    created: String,
    #[tabled(rename = "Jobs")]
    jobs: String,
    /// Finished-of-total from the snapshot, or a placeholder when there is none.
    #[tabled(rename = "Finished")]
    finished: String,
}

/// Dispatch the `plans` subcommand group. Renders what `plan_cli` returns: the
/// ingest report as JSON (the shape `POST /plans` reuses), everything else as
/// operator-facing text.
async fn handle_plan_action(action: PlanAction, stores: &Stores) -> anyhow::Result<()> {
    match action {
        PlanAction::Ingest { dir, plan_name } => {
            // §6.2 resolves each job's `benchmark_id` against the catalog.
            let catalog = stores.catalog.load_catalog().await?;
            let report = plan_cli::ingest_dir(stores, &catalog, &dir, plan_name).await?;
            // The report is the machine-readable artifact of an ingest (§8) —
            // printed verbatim so it can be piped or saved as the record of
            // which file became which job.
            println!("{}", serde_json::to_string_pretty(&report)?);
        }
        PlanAction::List { status } => {
            let manifests =
                plan_cli::list_plans(stores.plans.as_ref(), status.map(Into::into)).await?;
            if manifests.is_empty() {
                match status {
                    Some(_) => println!("No plans match that status."),
                    None => println!("No plans ingested."),
                }
                return Ok(());
            }
            // One listing for every pending cancel, rather than a marker check
            // per plan: a cancel is invisible in `status` until the next
            // maintenance pass, so without this the whole list would look
            // untouched right after an operator cancelled something.
            let pending: std::collections::HashSet<PlanId> = stores
                .plans
                .list_cancel_markers()
                .await?
                .into_iter()
                .collect();

            println!("{} plan(s):\n", manifests.len());
            let rows: Vec<PlanRow> = manifests
                .iter()
                .map(|p| PlanRow {
                    plan_id: p.plan_id.to_string(),
                    name: p.plan_name.clone().unwrap_or_else(|| "-".to_string()),
                    status: if pending.contains(&p.plan_id) {
                        format!("{} (cancel pending)", p.status.label())
                    } else {
                        p.status.label().to_string()
                    },
                    created: p.created_at.to_rfc3339(),
                    jobs: p.job_ids.len().to_string(),
                    finished: match &p.progress_snapshot {
                        Some(s) => format!("{}/{}", s.counts.finished, s.counts.total),
                        None => "-".to_string(),
                    },
                })
                .collect();
            let mut table = tabled::Table::new(&rows);
            table.with(tabled::settings::Style::psql());
            println!("{table}");
        }
        PlanAction::Status { plan_id, plan_name } => {
            let plan_ref = plan_ref(plan_id, plan_name)?;
            let view = plan_cli::plan_status(stores.plans.as_ref(), &plan_ref).await?;
            print_plan_status(&view);
        }
        PlanAction::Cancel { plan_id, plan_name } => {
            let plan_ref = plan_ref(plan_id, plan_name)?;
            let (plan_id, outcome) =
                plan_cli::cancel_plan(stores.plans.as_ref(), &plan_ref).await?;
            match outcome {
                CancelOutcome::Requested => println!(
                    "Cancellation recorded for {plan_id}.\n\
                     Its jobs are NOT stopped yet: the `queue-maintenance` teardown pass \
                     that retires them has not shipped, so they stay claimable for now.",
                ),
                CancelOutcome::AlreadyCancelled => {
                    println!("Plan {plan_id} is already cancelled; nothing to do.")
                }
            }
        }
    }
    Ok(())
}

/// Build a [`PlanRef`] from the mutually-exclusive `<plan_id>` / `--plan-name`
/// arguments. clap enforces exactly one is present, so the `None`/`None` arm is
/// unreachable in practice and reported rather than panicked on.
fn plan_ref(plan_id: Option<PlanId>, plan_name: Option<String>) -> anyhow::Result<PlanRef> {
    match (plan_id, plan_name) {
        (Some(id), _) => Ok(PlanRef::Id(id)),
        (None, Some(name)) => Ok(PlanRef::Name(name)),
        (None, None) => anyhow::bail!("provide a <plan_id> or --plan-name"),
    }
}

/// Render `plans status` for a human. Labelled lines rather than a table: this
/// is one record with nested groups, not a list of rows.
fn print_plan_status(view: &PlanStatusView) {
    println!("Plan     {}", view.plan_id);
    println!("Name     {}", view.plan_name.as_deref().unwrap_or("-"));
    println!("Status   {}", view.status.label());
    println!("Created  {}", view.created_at.to_rfc3339());
    if let Some(ended) = view.terminal_at {
        println!("Ended    {} ({})", ended.to_rfc3339(), view.status.label());
    }
    if view.cancel_requested {
        println!(
            "Cancel   requested — the status above latches to `cancelled` on the next \
             `queue-maintenance` run"
        );
    }

    match &view.progress_snapshot {
        // Say so explicitly rather than leaving a blank: absent means "not
        // computed yet", which is different from "computed, and all zero".
        None => {
            println!("\nProgress not yet computed — `queue-maintenance` has not run for this plan")
        }
        Some(snapshot) => {
            let c = &snapshot.counts;
            println!(
                "\nProgress (computed {})",
                snapshot.computed_at.to_rfc3339()
            );
            println!(
                "  {} job(s): {} finished, {} running, {} available, {} failed",
                c.total, c.finished, c.running, c.available, c.failed
            );
            if !snapshot.starved.is_empty() {
                println!("\nStarved — outstanding jobs matching no registered, approved client:");
                for group in &snapshot.starved {
                    println!(
                        "  {} job(s) {}",
                        group.job_ids.len(),
                        describe_requirement(&group.requires, &group.any_of, &group.clients)
                    );
                }
            }
        }
    }

    if !view.warnings.is_empty() {
        // Frozen at ingestion, so they can disagree with the live `starved` list
        // above once clients register afterwards — labelled to prevent reading a
        // stale warning as current.
        println!("\nWarnings at ingestion:");
        for warning in &view.warnings {
            println!("  {}", warning.message);
        }
    }
}

/// One-line description of an eligibility requirement, for the starved groups.
fn describe_requirement(requires: &[String], any_of: &[Vec<String>], clients: &[String]) -> String {
    let mut clauses = Vec::new();
    if !requires.is_empty() {
        clauses.push(format!("requiring {}", requires.join(", ")));
    }
    if !any_of.is_empty() {
        let groups: Vec<String> = any_of
            .iter()
            .map(|g| format!("[{}]", g.join(", ")))
            .collect();
        clauses.push(format!("with any-of {}", groups.join(" & ")));
    }
    if !clients.is_empty() {
        clauses.push(format!("pinned to {}", clients.join(", ")));
    }
    if clauses.is_empty() {
        "with no eligibility".to_string()
    } else {
        clauses.join(" ")
    }
}

/// Render a key as a `preauth list` row (never exposes the secret hash).
fn preauth_row(
    k: &pipette_mgmt::preauth::PreauthKey,
    now: chrono::DateTime<chrono::Utc>,
) -> PreauthRow {
    let usage = match k.usage {
        PreauthUsage::SingleUse => "single-use",
        PreauthUsage::MultiUse => "multi-use",
    };
    let tags = if k.default_tags.is_empty() {
        "-".to_string()
    } else {
        k.default_tags
            .iter()
            .map(|t| t.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    };
    let org = k.default_organization.as_ref().map_or("-", |o| o.as_str());
    let status = if k.is_expired(now) {
        "expired"
    } else {
        "active"
    };
    let expires = k
        .expires_at
        .map_or_else(|| "never".to_string(), |e| e.to_rfc3339());
    PreauthRow {
        key_id: format!("{}\n{}", k.key_id, k.created_at.to_rfc3339()),
        lifecycle: format!("{status}\n{expires}"),
        usage: usage.to_string(),
        seeds: format!("tags: {tags}\norg: {org}"),
        note: k.note.clone().unwrap_or_else(|| "-".to_string()),
    }
}

async fn handle_unverified_action(action: UnverifiedAction, stores: Stores) -> anyhow::Result<()> {
    let store = stores.submissions;
    match action {
        UnverifiedAction::Prune {
            older_than,
            dry_run,
        } => {
            let summary = store.prune_unverified(older_than, dry_run).await?;
            let prefix = if dry_run {
                "[dry-run] would delete"
            } else {
                "deleted"
            };
            println!(
                "{prefix} {} unverified object(s); {} kept",
                summary.deleted, summary.kept
            );
        }
        UnverifiedAction::Delete { client_id, dry_run } => {
            let deleted = store.delete_unverified_client(&client_id, dry_run).await?;
            let prefix = if dry_run {
                "[dry-run] would delete"
            } else {
                "deleted"
            };
            println!("{prefix} {deleted} held submission(s) for client {client_id}");
        }
        UnverifiedAction::Promote { client_id, dry_run } => {
            let held = store.list_unverified_client(&client_id).await?;
            if dry_run {
                println!(
                    "[dry-run] would promote {} held submission(s) for client {client_id}",
                    held.len()
                );
                return Ok(());
            }
            let mut promoted = 0usize;
            for (job_id, body) in held {
                // Route by the body's own `message_type`, reusing the
                // same incoming/processed split as live submission. The
                // body already carries the real `client_id`, `job_id`,
                // and `submitted_at`, so re-staging is a straight move.
                let submission = pipette_mgmt::submission::parse_stored_submission(&body)
                    .map_err(|e| anyhow::anyhow!("malformed held submission {job_id}: {e}"))?;
                match submission {
                    pipette_mgmt::submission::Submission::Success(_) => {
                        store.write_incoming(&job_id, &body).await?;
                    }
                    pipette_mgmt::submission::Submission::Failure(_) => {
                        store.write_processed(&job_id, &body).await?;
                    }
                }
                // Delete only after the re-stage write succeeds, so a
                // mid-promotion crash leaves the held copy intact rather
                // than losing the submission.
                store.delete_unverified(&client_id, &job_id).await?;
                promoted += 1;
            }
            println!("promoted {promoted} held submission(s) for client {client_id}");
        }
    }
    Ok(())
}

/// clap `value_parser` for `--older-than`: parse a `<number><unit>`
/// duration (`s`/`m`/`h`/`d`, e.g. `7d`, `24h`, `30m`) into a
/// `std::time::Duration`.
fn parse_duration(s: &str) -> Result<std::time::Duration, String> {
    let s = s.trim();
    let split = s
        .find(|c: char| !c.is_ascii_digit())
        .ok_or_else(|| format!("missing unit in duration '{s}' (use s/m/h/d, e.g. 7d)"))?;
    let (value, unit) = s.split_at(split);
    let n: u64 = value
        .parse()
        .map_err(|_| format!("invalid duration number in '{s}'"))?;
    let secs = match unit {
        "s" => n,
        "m" => n * 60,
        "h" => n * 3600,
        "d" => n * 86_400,
        other => {
            return Err(format!(
                "invalid duration unit '{other}' in '{s}' (use s/m/h/d)"
            ));
        }
    };
    Ok(std::time::Duration::from_secs(secs))
}

/// Run `fut` while holding the storage mutate lock, so it cannot
/// interleave its read-modify-write with another `score` / `fix-*`
/// command. The lock is released whether `fut` succeeds or fails; a
/// panic leaves the lease to expire on its own (see `storage_lock`).
/// clap `value_parser` for `--benchmark-id`: validate into the typed
/// `BenchmarkId` at the argument boundary (it has no `FromStr`).
fn parse_benchmark_id(s: &str) -> Result<BenchmarkId, String> {
    BenchmarkId::try_new(s).map_err(|e| e.to_string())
}

/// clap `value_parser` for `--client-id`: validate into the typed
/// `ClientId` at the argument boundary (it has no `FromStr`).
fn parse_client_id(s: &str) -> Result<ClientId, String> {
    ClientId::try_new(s).map_err(|e| e.to_string())
}

/// clap `value_parser` for a positional `<plan_id>`: validate into the typed
/// `PlanId` at the argument boundary, so a path-traversing or key-injecting
/// value is rejected before it reaches the store.
fn parse_plan_id(s: &str) -> Result<PlanId, String> {
    PlanId::try_new(s).map_err(|e| e.to_string())
}

/// clap `value_parser` for the `requeue-eval` `--submitted-*` flags: parse
/// a human timestamp into micros-since-epoch, matching the warehouse row's
/// `submitted_at`. Accepts RFC3339 (`2026-06-01T00:00:00Z`) or a bare
/// `YYYY-MM-DD` date, interpreted as midnight UTC.
fn parse_timestamp_micros(s: &str) -> Result<i64, String> {
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(s) {
        return Ok(dt.timestamp_micros());
    }
    if let Ok(date) = chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d") {
        return date
            .and_hms_opt(0, 0, 0)
            .map(|naive| naive.and_utc().timestamp_micros())
            .ok_or_else(|| format!("invalid date: {s}"));
    }
    Err(format!(
        "expected RFC3339 (e.g. 2026-06-01T00:00:00Z) or YYYY-MM-DD, got: {s}"
    ))
}

async fn run_locked<T>(
    storage: &StorageConfig,
    lock_name: &str,
    holder: &str,
    ttl: std::time::Duration,
    fut: impl std::future::Future<Output = anyhow::Result<T>>,
) -> anyhow::Result<T> {
    let lock = StorageLock::acquire_named(storage, lock_name, holder, ttl).await?;
    let result = fut.await;
    lock.release().await;
    result
}

async fn build_stores(config: &Config) -> anyhow::Result<Stores> {
    // The `todo/` job queue resolves its backend via `config.todo_storage()`
    // (`[todo_storage]`, defaulting to `[storage]`); it must be S3 Express One
    // Zone in production for atomic claims. It is resolved once, here, before
    // the `[storage]` builders run: the S3 builder takes it as a parameter, and
    // the local_fs arm overwrites the seed store `build_local_fs_stores` creates
    // for its direct callers. The S3 todo builder is async (aws-config
    // credential chain), which is why `build_stores` is async.
    let todo = match config.todo_storage() {
        StorageConfig::LocalFs { .. } => {
            tracing::info!("initializing local_fs todo storage");
            build_local_fs_todo_store(config.todo_storage())?
        }
        StorageConfig::S3 { bucket, .. } => {
            tracing::info!(bucket, "initializing s3 todo storage");
            build_s3_todo_store(config.todo_storage()).await?
        }
    };

    let mut stores = match &config.storage {
        StorageConfig::LocalFs { .. } => {
            tracing::info!("initializing local_fs storage");
            let mut stores = build_local_fs_stores(config)?;
            stores.todo = todo;
            stores
        }
        StorageConfig::S3 { bucket, .. } => {
            tracing::info!(bucket, "initializing s3 storage");
            build_s3_stores(config, todo)?
        }
    };

    stores.auth = match &config.auth_storage {
        StorageConfig::LocalFs { .. } => {
            tracing::info!("initializing local_fs auth storage");
            build_local_fs_auth_store(&config.auth_storage)
        }
        StorageConfig::S3 { bucket, .. } => {
            tracing::info!(bucket, "initializing s3 auth storage");
            build_s3_auth_store(&config.auth_storage)
        }
    }?;

    Ok(stores)
}

/// Resolves once the process is asked to stop, so in-flight requests finish
/// instead of being cut off mid-write.
///
/// `SIGTERM` is what a container runtime sends before it kills the process;
/// `SIGINT` is an interactive `^C`. Either way the listener stops accepting and
/// `axum::serve` returns once the requests already in progress complete.
async fn shutdown_signal() {
    let interrupt = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut signal) => {
                signal.recv().await;
            }
            // Without a handler there is no terminate to wait for, so this arm
            // simply never completes and the other one decides.
            Err(e) => {
                tracing::warn!(error = %e, "cannot listen for SIGTERM");
                std::future::pending::<()>().await;
            }
        }
    };

    tokio::select! {
        () = interrupt => tracing::info!(signal = "SIGINT", "shutdown requested"),
        () = terminate => tracing::info!(signal = "SIGTERM", "shutdown requested"),
    }
}

async fn serve(config: Config, stores: Stores) -> anyhow::Result<()> {
    // Fail fast before binding if the `todo/` backend can't do atomic claims
    // (S3 Express One Zone required in production; local_fs is a no-op).
    stores.todo.validate_backend().await?;

    let catalog = stores.catalog.load_catalog().await?;
    tracing::info!("loaded {} benchmarks", catalog.len());

    let catalog_cache = Arc::new(pipette_mgmt::catalog_cache::CatalogCache::new(
        stores.catalog,
        catalog,
        std::time::Duration::from_secs(config.catalog_ttl_secs),
    ));

    let state = AppState {
        config: Arc::new(config.clone()),
        catalog_cache,
        http_client: reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(config.http_timeout_secs))
            .build()?,
        replay_cache: Arc::new(pipette_mgmt::auth::ReplayCache::new()),
        migrated_clients: Arc::new(pipette_mgmt::auth::MigratedClients::new()),
        auth_store: stores.auth,
        submission_store: stores.submissions,
        warehouse_store: stores.warehouse,
        eval_sample_result_store: stores.eval_sample_results,
        todo_store: stores.todo,
    };

    let app = pipette_mgmt::router::app(state);

    // The per-request warning for a timestamp-only signature is silent when no
    // client sends one, which is indistinguishable from the flag being off. Say
    // so once at startup instead, so the weaker mode is never invisible.
    if config.accept_legacy_signatures {
        tracing::warn!(
            "accepting timestamp-only signatures; clear accept_legacy_signatures once the \
             \"accepted timestamp-only signature\" warnings stop"
        );
    }

    let listener = tokio::net::TcpListener::bind(&config.listen_addr).await?;
    tracing::info!("listening on {}", config.listen_addr);
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    tracing::info!("server stopped");

    Ok(())
}

/// Display-only row for `clients list`. Tags don't live on the client record,
/// so they're fetched separately and joined in here; the columns mirror the
/// `Client` table with a trailing `Tags` column.
#[derive(tabled::Tabled)]
struct ClientRow {
    #[tabled(rename = "Client ID")]
    client_id: String,
    #[tabled(rename = "Organization")]
    organization: String,
    #[tabled(rename = "Details")]
    details: String,
    #[tabled(rename = "Contact")]
    contact: String,
    #[tabled(rename = "Status")]
    status: String,
    #[tabled(rename = "Registered")]
    registered: String,
    #[tabled(rename = "Tags")]
    tags: String,
    /// When this client first signed a `v1` payload, or `—` while it has only
    /// ever used the timestamp-only fallback. Every client carrying a date is
    /// the condition for switching that fallback off
    /// (`docs/authentication.md` §2.3).
    #[tabled(rename = "Migrated")]
    migrated: String,
}

impl ClientRow {
    fn from_client(
        c: &pipette_mgmt::client::Client,
        tags: &std::collections::BTreeSet<pipette_mgmt::validated::Tag>,
        migrated: Option<chrono::DateTime<chrono::Utc>>,
    ) -> Self {
        Self {
            client_id: c.client_id.to_string(),
            organization: c.organization.to_string(),
            details: pipette_mgmt::client::truncate_details(&c.client_details),
            contact: c.contact_email.to_string(),
            status: c.status.to_string(),
            registered: c.registered_at.to_string(),
            tags: join_tags(tags),
            migrated: migrated.map_or_else(|| "—".to_string(), |at| at.to_string()),
        }
    }
}

#[derive(tabled::Tabled)]
struct SuspendedRow {
    #[tabled(rename = "Client ID")]
    client_id: String,
    #[tabled(rename = "Suspended At")]
    suspended_at: String,
    #[tabled(rename = "Conflicting Job")]
    conflicting_job_id: String,
}

/// Comma-join a client's tags for display, or `-` when untagged.
fn join_tags(tags: &std::collections::BTreeSet<pipette_mgmt::validated::Tag>) -> String {
    if tags.is_empty() {
        "-".to_string()
    } else {
        tags.iter()
            .map(|t| t.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    }
}

// Unlike `serve` and `queue-maintenance`, these commands do not call
// `stores.todo.validate_backend()`. The startup probe guards atomic renames
// against a misconfigured (non-Express) S3 bucket, and the only `todo/`-touching
// actions here — `ListSuspended`, `Unsuspend`, `Delete` — solely list and delete
// markers. They never rename, so the guarantee the probe checks is irrelevant to
// them, and validating would be pure ceremony. The rule is: validate iff you
// rename (see docs/storage.md, "todo/ requires S3 Express One Zone").
async fn handle_client_action(action: ClientAction, stores: Stores) -> anyhow::Result<()> {
    let auth_store = stores.auth;

    match action {
        ClientAction::List { tags } => {
            let filters: Vec<pipette_mgmt::validated::Tag> = tags
                .into_iter()
                .map(pipette_mgmt::validated::Tag::try_new)
                .collect::<Result<_, _>>()
                .map_err(|e| anyhow::anyhow!("--tag: {e}"))?;

            let clients: Vec<pipette_mgmt::client::Client> = if filters.is_empty() {
                auth_store.list_clients().await?
            } else {
                // Fetch each --tag's client set concurrently, then AND them
                // (intersection is commutative, so unordered results are fine).
                let sets: Vec<std::collections::BTreeSet<ClientId>> =
                    stream::iter(filters.iter().map(|tag| {
                        let auth_store = auth_store.clone();
                        let tag = tag.clone();
                        async move {
                            anyhow::Ok(
                                auth_store
                                    .list_client_ids_by_tag(&tag)
                                    .await?
                                    .into_iter()
                                    .collect::<std::collections::BTreeSet<_>>(),
                            )
                        }
                    }))
                    .buffer_unordered(STORAGE_CONCURRENCY)
                    .try_collect()
                    .await?;
                let ids = sets
                    .into_iter()
                    .reduce(|a, b| a.intersection(&b).cloned().collect())
                    .unwrap_or_default();
                // Load the surviving records concurrently (order restored below).
                let mut clients: Vec<pipette_mgmt::client::Client> =
                    stream::iter(ids.into_iter().map(|id| {
                        let auth_store = auth_store.clone();
                        async move { anyhow::Ok(auth_store.get_client(&id).await?) }
                    }))
                    .buffer_unordered(STORAGE_CONCURRENCY)
                    .try_filter_map(|c| async move { anyhow::Ok(c) })
                    .try_collect()
                    .await?;
                clients.sort_by_key(|c| std::cmp::Reverse(c.registered_at));
                clients
            };
            if clients.is_empty() {
                if filters.is_empty() {
                    println!("No clients registered.");
                } else {
                    println!("No clients match the given tag filter.");
                }
            } else {
                println!("{} client(s):\n", clients.len());
                // Migration markers aren't on the record either, but unlike tags
                // they live in one flat tree — a single listing covers every
                // client, so it is read once here rather than per row.
                let migrated: std::collections::HashMap<_, _> = auth_store
                    .list_signature_migrations()
                    .await?
                    .into_iter()
                    .map(|(id, record)| (id, record.first_seen))
                    .collect();
                // Tags aren't on the record — fetch each client's set (one
                // listing per client) concurrently. `buffered` keeps the sorted
                // order for the table.
                let rows: Vec<ClientRow> = stream::iter(clients.iter().map(|c| {
                    let auth_store = auth_store.clone();
                    let migrated = &migrated;
                    async move {
                        let tags = auth_store.get_client_tags(&c.client_id).await?;
                        anyhow::Ok(ClientRow::from_client(
                            c,
                            &tags,
                            migrated.get(&c.client_id).copied(),
                        ))
                    }
                }))
                .buffered(STORAGE_CONCURRENCY)
                .try_collect()
                .await?;
                let mut table = tabled::Table::new(&rows);
                table.with(tabled::settings::Style::psql());
                println!("{table}");
            }
        }
        ClientAction::Tag { action } => {
            handle_tag_action(action, auth_store).await?;
        }
        ClientAction::Approve { client_id } => {
            let client_id = ClientId::try_new(client_id)?;
            let mut c = auth_store
                .get_client(&client_id)
                .await?
                .ok_or_else(|| anyhow::anyhow!("client {client_id} not found"))?;
            if c.status == ClientStatus::Approved {
                eprintln!("Client {client_id} is already approved.");
                return Ok(());
            }
            let prev_status = c.status;
            c.status = ClientStatus::Approved;
            auth_store.put_client(&c).await?;
            tracing::info!(client_id = %client_id, previous_status = %prev_status,
                "client approved");
            println!("Approved client {client_id} ({prev_status} -> approved).");
        }
        ClientAction::Reject { client_id } => {
            let client_id = ClientId::try_new(client_id)?;
            let c = auth_store
                .get_client(&client_id)
                .await?
                .ok_or_else(|| anyhow::anyhow!("client {client_id} not found"))?;
            if c.status != ClientStatus::Pending {
                anyhow::bail!(
                    "can only reject pending clients, {client_id} has status \"{}\"",
                    c.status,
                );
            }
            auth_store.delete_client(&client_id).await?;
            tracing::info!(client_id = %client_id, "client rejected");
            println!("Rejected and deleted client {client_id}.");
        }
        ClientAction::Delete { client_id } => {
            let client_id = ClientId::try_new(client_id)?;
            // Idempotent so it can be re-run to converge: a prior delete that
            // partially failed may have removed the record but left queue-state
            // orphans (or vice versa). Every step below tolerates an
            // already-absent target, so re-running finishes the cleanup rather
            // than erroring on the record that is already gone. Because an
            // absent record is not an error, a mistyped id cannot be
            // distinguished from an already-deleted one — the "no record found"
            // message is the only signal.
            let existed = auth_store.get_client(&client_id).await?.is_some();
            auth_store.delete_client(&client_id).await?;
            // Clean up the client's queue state on a best-effort basis
            // (queue-maintenance reconciles any leftover).
            purge_client_todo_state(&*stores.todo, &client_id).await;
            tracing::info!(client_id = %client_id, existed, "client deleted");
            if existed {
                println!("deleted identity {client_id}");
            } else {
                println!(
                    "client {client_id}: no identity record found; removed any residual queue state"
                );
            }
        }
        ClientAction::ListSuspended => {
            let mut records = stores.todo.list_suspensions().await?;
            if records.is_empty() {
                println!("No clients are suspended.");
            } else {
                // Most recently suspended first, matching `list`'s newest-first order.
                records.sort_by_key(|(_, r)| std::cmp::Reverse(r.suspended_at));
                let rows: Vec<SuspendedRow> = records
                    .iter()
                    .map(|(client_id, record)| SuspendedRow {
                        client_id: client_id.to_string(),
                        suspended_at: record.suspended_at.to_string(),
                        conflicting_job_id: record.conflicting_job_id.to_string(),
                    })
                    .collect();
                println!("{} suspended client(s):\n", rows.len());
                let mut table = tabled::Table::new(&rows);
                table.with(tabled::settings::Style::psql());
                println!("{table}");
            }
        }
        ClientAction::Unsuspend { client_id } => {
            let client_id = ClientId::try_new(client_id)?;
            // Idempotent: a no-op if the client was not suspended.
            stores.todo.delete_suspension(&client_id).await?;
            tracing::info!(client_id = %client_id, "client unsuspended");
            println!("Cleared suspension for client {client_id}.");
        }
        ClientAction::Update {
            client_id,
            organization,
            client_details,
            contact_email,
        } => {
            if organization.is_none() && client_details.is_none() && contact_email.is_none() {
                anyhow::bail!(
                    "nothing to update: pass at least one of --organization, --details, --email"
                );
            }
            let client_id = ClientId::try_new(client_id)?;
            let mut c = auth_store
                .get_client(&client_id)
                .await?
                .ok_or_else(|| anyhow::anyhow!("client {client_id} not found"))?;
            let mut changes: Vec<String> = Vec::new();
            if let Some(v) = organization {
                let v = pipette_mgmt::validated::NonEmptyTrimmedString::try_new(v)
                    .map_err(|e| anyhow::anyhow!("--organization: {e}"))?;
                if c.organization != v {
                    changes.push(format!("organization: {:?} -> {:?}", c.organization, v));
                    c.organization = v;
                }
            }
            if let Some(v) = client_details {
                let v = pipette_mgmt::validated::NonEmptyTrimmedString::try_new(v)
                    .map_err(|e| anyhow::anyhow!("--details: {e}"))?;
                if c.client_details != v {
                    changes.push(format!("details: {:?} -> {:?}", c.client_details, v));
                    c.client_details = v;
                }
            }
            if let Some(v) = contact_email {
                let v = pipette_mgmt::validated::ContactEmail::try_new(v)
                    .map_err(|e| anyhow::anyhow!("--email: {e}"))?;
                if c.contact_email != v {
                    changes.push(format!("contact_email: {:?} -> {:?}", c.contact_email, v));
                    c.contact_email = v;
                }
            }
            if changes.is_empty() {
                println!("Client {client_id}: no changes (provided values match current).");
                return Ok(());
            }
            auth_store.put_client(&c).await?;
            tracing::info!(client_id = %client_id, "client details updated");
            println!("Updated client {client_id}:");
            for line in changes {
                println!("  {line}");
            }
        }
    }
    Ok(())
}

/// Handle `clients tag {add,remove,list}`. Tags are stored as leaf markers in
/// the auth store (two mirrored trees), not on the client record, so each
/// operation is a marker write/delete or a directory listing — no record
/// read-modify-write.
async fn handle_tag_action(
    action: TagAction,
    auth_store: Arc<dyn AuthStore>,
) -> anyhow::Result<()> {
    // Parse a batch of `--tag` args into validated `Tag`s up front, so a typo in
    // any one aborts before we touch the store. Collecting into a BTreeSet
    // de-duplicates (including args that normalize to the same tag) and yields
    // them in sorted order.
    fn parse_tags(raw: Vec<String>) -> anyhow::Result<std::collections::BTreeSet<Tag>> {
        raw.into_iter()
            .map(|t| Tag::try_new(t).map_err(|e| anyhow::anyhow!("invalid tag: {e}")))
            .collect()
    }

    // Every action names a client; confirm it exists before touching tags.
    async fn require_client(
        auth_store: &Arc<dyn AuthStore>,
        client_id: &ClientId,
    ) -> anyhow::Result<()> {
        auth_store
            .get_client(client_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("client {client_id} not found"))?;
        Ok(())
    }

    match action {
        TagAction::Add { client_id, tags } => {
            let client_id = ClientId::try_new(client_id)?;
            let parsed = parse_tags(tags)?;
            require_client(&auth_store, &client_id).await?;
            let existing = auth_store.get_client_tags(&client_id).await?;
            // Only the not-yet-present tags (parsed is sorted + de-duped, so
            // `added` is too).
            let added: Vec<Tag> = parsed
                .into_iter()
                .filter(|tag| !existing.contains(tag))
                .collect();
            if added.is_empty() {
                println!("Client {client_id}: no new tags (all already present).");
                return Ok(());
            }
            stream::iter(added.iter().map(|tag| {
                let auth_store = auth_store.clone();
                let client_id = client_id.clone();
                let tag = tag.clone();
                async move { auth_store.add_client_tag(&client_id, &tag).await }
            }))
            .buffer_unordered(STORAGE_CONCURRENCY)
            .try_collect::<Vec<_>>()
            .await?;
            tracing::info!(client_id = %client_id, count = added.len(), "client tags added");
            println!("Added {} tag(s) to {client_id}:", added.len());
            added.iter().for_each(|t| println!("  + {t}"));
        }
        TagAction::Remove { client_id, tags } => {
            let client_id = ClientId::try_new(client_id)?;
            let parsed = parse_tags(tags)?;
            require_client(&auth_store, &client_id).await?;
            let existing = auth_store.get_client_tags(&client_id).await?;
            // Only the tags the client actually has (parsed is sorted + de-duped).
            let removed: Vec<Tag> = parsed
                .into_iter()
                .filter(|tag| existing.contains(tag))
                .collect();
            if removed.is_empty() {
                println!("Client {client_id}: no matching tags to remove.");
                return Ok(());
            }
            stream::iter(removed.iter().map(|tag| {
                let auth_store = auth_store.clone();
                let client_id = client_id.clone();
                let tag = tag.clone();
                async move { auth_store.remove_client_tag(&client_id, &tag).await }
            }))
            .buffer_unordered(STORAGE_CONCURRENCY)
            .try_collect::<Vec<_>>()
            .await?;
            tracing::info!(client_id = %client_id, count = removed.len(), "client tags removed");
            println!("Removed {} tag(s) from {client_id}:", removed.len());
            removed.iter().for_each(|t| println!("  - {t}"));
        }
        TagAction::List { client_id } => {
            let client_id = ClientId::try_new(client_id)?;
            require_client(&auth_store, &client_id).await?;
            let tags = auth_store.get_client_tags(&client_id).await?;
            if tags.is_empty() {
                println!("Client {client_id} has no tags.");
            } else {
                println!("Client {client_id}: {}", join_tags(&tags));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::Context;
    use clap::{CommandFactory, ValueEnum};
    use rstest::rstest;

    /// Every `--status` value is spelled exactly as that status prints
    /// everywhere else. clap's derive defaults to kebab-case, so without the
    /// `rename_all` on `PlanStatusArg` this drifts to `pending-clients` and an
    /// operator filtering by the status `plans status` just showed them gets
    /// "invalid value" — which is precisely what happened before this test.
    #[test]
    fn plan_status_arg_spellings_match_the_status_labels() -> anyhow::Result<()> {
        // Reflection over `value_variants()` rather than a case per status: a
        // sixth status added to the enum is covered automatically, where
        // hardcoded cases would silently stop guarding the new one.
        for arg in PlanStatusArg::value_variants() {
            let flag_value = arg
                .to_possible_value()
                .context("every variant is selectable")?
                .get_name()
                .to_string();
            assert_eq!(
                flag_value,
                PlanStatus::from(*arg).label(),
                "`--status {flag_value}` must match the printed status label"
            );
        }
        Ok(())
    }

    /// A nil-UUID plan id, spelled as a literal so it can live in a `#[case]`
    /// argument (a formatted `String` would not outlive the attribute).
    const NIL_PLAN_ID: &str = "plan-00000000-0000-0000-0000-000000000000";

    /// The `plans` group parses its documented forms, and the two ways of naming
    /// a plan stay mutually exclusive.
    #[rstest]
    #[case::ingest(&["pipette-mgmt", "plans", "ingest", "/tmp/jobs"], true)]
    #[case::ingest_named(
        &["pipette-mgmt", "plans", "ingest", "/tmp/jobs", "--plan-name", "smoke"], true
    )]
    #[case::list(&["pipette-mgmt", "plans", "list"], true)]
    #[case::list_filtered(&["pipette-mgmt", "plans", "list", "--status", "pending_clients"], true)]
    // A plan is named by exactly one of <plan_id> / --plan-name.
    #[case::cancel_by_id(&["pipette-mgmt", "plans", "cancel", NIL_PLAN_ID], true)]
    #[case::cancel_by_name(&["pipette-mgmt", "plans", "cancel", "--plan-name", "smoke"], true)]
    #[case::cancel_with_neither_form(&["pipette-mgmt", "plans", "cancel"], false)]
    #[case::cancel_with_both_forms(
        &["pipette-mgmt", "plans", "cancel", NIL_PLAN_ID, "--plan-name", "smoke"], false
    )]
    // Ids are validated at the argument boundary, before reaching the store.
    #[case::status_rejects_path_traversal(
        &["pipette-mgmt", "plans", "status", "../../etc/passwd"], false
    )]
    // A status outside the enum is rejected rather than silently ignored.
    #[case::list_rejects_unknown_status(
        &["pipette-mgmt", "plans", "list", "--status", "bogus"], false
    )]
    fn plans_argument_parsing(#[case] args: &[&str], #[case] accepted: bool) {
        assert_eq!(
            Cli::try_parse_from(args).is_ok(),
            accepted,
            "parsing {args:?}"
        );
    }

    /// clap's own invariants for the whole command tree (duplicate flags,
    /// conflicting settings) — cheap insurance now that `plans` adds a group.
    #[test]
    fn cli_definition_is_valid() {
        Cli::command().debug_assert();
    }
}
