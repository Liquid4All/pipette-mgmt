use std::collections::{BTreeSet, HashMap, HashSet};
use std::io::{IsTerminal, Write};
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;

use clap::{Args, CommandFactory, FromArgMatches, Parser, Subcommand};
use futures::stream;
use futures::stream::{StreamExt, TryStreamExt};
use strum::VariantNames;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::filter::LevelFilter;

use pipette_core::campaign::{Campaign, CampaignOwner, NewCampaign};
use pipette_core::client::{ClientStatus, normalized_flags};
use pipette_core::client_permission::ClientPermissionSet;
use pipette_core::client_preauth;
use pipette_core::client_preauth::{ClientPreauthKey, ClientPreauthParams};
use pipette_core::config::{Config, DescriptorCatalogConfig, StorageConfig};
use pipette_core::matching::describe_clauses;
use pipette_core::organization::Organization;
use pipette_core::plan::{JobExpiry, PlanExplanation, PlanLeases, PlanManifest, ProgressSnapshot};
use pipette_core::preauth_prune;
use pipette_core::todo_filename::{Lease, parse_leased_key};
use pipette_core::user::UserKey;
use pipette_core::user_permission;
use pipette_core::user_permission::{
    Holder, StoredUserPermission, UserPermissionGroup, UserPermissionSet,
};
use pipette_core::user_preauth;
use pipette_core::user_preauth::{UserPreauthKey, UserPreauthParams};
use pipette_core::vault::{VaultOwner, VaultResource};
use pipette_httpapi::api_schema::openapi::OpenApi;
use pipette_httpapi::state::AppState;
use pipette_pipeline::audit::CommandAudit;
use pipette_pipeline::catalog_import::{self, GitHubClient, GitHubToken};
use pipette_pipeline::client_cli;
use pipette_pipeline::client_cli::{CapabilityQuery, CapabilitySource};
use pipette_pipeline::descriptor_catalog_admin::{self, CatalogTransition};
use pipette_pipeline::hf_import::{HfImports, HubClient, Progress};
use pipette_pipeline::model_params::{self, ModelParamTable};
use pipette_pipeline::org;
use pipette_pipeline::org::OrgRef;
use pipette_pipeline::plan_cli;
use pipette_pipeline::plan_cli::{CancelOutcome, PlanLookupError};
use pipette_pipeline::plan_ingestion::{IngestReport, IngestSettings, ModelCheck};
use pipette_pipeline::plan_results::{ResultListView, ResultsDb};
use pipette_pipeline::variant_edit::VariantEdit;
use pipette_pipeline::{
    campaign_admin, campaign_chain, client_admin, dataset_cache, fix_canonical, fix_model_param,
    fix_vault_labels, model_audit, org_catalog, queue_maintenance, record_copy, requeue_eval,
    score, user_admin, vault_admin,
};
use pipette_plan_types::{
    AuditOutcome, BenchmarkId, CampaignId, CatalogName, CatalogScope, ClientId, ClientOwner,
    ClientPermission, ClientPreauthKeyId, ClientPreauthUsage, ContactEmail, Counts,
    DescriptorEntryKind, DescriptorEntryState, ExpiresAt, FleetDetail, JobCensus, JobId, ModelKey,
    NonEmptyTrimmedString, OrgId, OrgSlug, OrgStatus, PlanId, PlanStatus, PrincipalOwner,
    RestoredVariant, RuntimeType, SchedulingState, Tag, UserAccountType, UserId, UserKeyId,
    UserPermission, UserPreauthKeyId, UserPreauthUsage, UserStatus, VaultResourceId, Warning,
};
use pipette_store::{
    AuditStores, CatalogEntryKey, CatalogImportRunStore, ClientStore, CreateOutcome,
    DescriptorCatalogStore, DescriptorEntryFilter, EmailClaim, OrgStore, STORAGE_CONCURRENCY,
    SlugClaim, Stores, SubmissionStore, TagStore, TodoStore, UserStore, drain_campaigns,
    drain_clients, drain_vault_resources, warn_unreadable_clients,
};
use pipette_store_init::storage_lock;
use pipette_store_init::storage_lock::StorageLock;
use pipette_store_init::{build_object_stores, build_stores, conditional_put, database};

#[derive(Parser)]
#[command(
    name = "pipette-mgmt",
    about = "Pipette: edge benchmark management server",
    version = pipette_core::BUILD_VERSION
)]
struct Cli {
    /// Path to TOML configuration file
    #[arg(long, env = "PIPETTE_MGMT_CONFIG", global = true)]
    config: Option<PathBuf>,

    /// Shape of the log lines written to stderr: `text` for a person reading a
    /// terminal, `json` (one object per line) for a log collector
    #[arg(long, env = "PIPETTE_MGMT_LOG_FORMAT", value_enum, default_value_t = LogFormat::Text)]
    log_format: LogFormat,

    #[command(subcommand)]
    command: Command,
}

#[derive(Clone, Copy, clap::ValueEnum)]
enum LogFormat {
    Text,
    Json,
}

#[derive(Subcommand)]
enum Command {
    /// Parse and validate the server config file, and start no services
    Config {
        #[command(subcommand)]
        action: ConfigAction,
    },
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
    /// to finalize. Runs serially on its own schedule. Takes its own
    /// `score-eval` lock rather than the mutate lock, so an overlapping run
    /// exits and the fast pass is never blocked.
    ScoreEval,
    /// Backfill warehouse `model_params_*_millions` columns from the table.
    /// A row resolves by its `model_descriptor`, as the scorer resolves it; a
    /// row nothing resolves is left as stored and counted. The repair step
    /// after `models unresolved` and a new model-params row.
    FixModelParam {
        /// Count matching rows without rewriting Parquet files
        #[arg(long)]
        dry_run: bool,
        /// Restrict the rewrite to rows for these model name(s). Repeatable
        /// (`--model A --model B`) and comma-separated (`--model A,B`) — both
        /// accumulate. Names are normalized like table lookups, so `--model
        /// LiquidAI/LFM2.5-230M` also matches the `-GGUF` and quant variants.
        /// A row matches by its normalized `model_name` or, carrying none, by
        /// the key its descriptor resolves to. Omit to process every row.
        #[arg(long = "model", value_name = "MODEL_NAME", value_delimiter = ',')]
        models: Vec<String>,
    },
    /// Re-canonicalize the warehouse's opaque JSON columns
    /// (`model_descriptor`, `runtime_descriptor`, `benchmark_flags`,
    /// `model_flags`, `runtime_flags`, and the three `requested_*_flags`) and
    /// recompute their `_sha256` ids. Use after a canonicalization change, or
    /// to backfill a hash column a row predates.
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
    /// Give each vault source without a `custom_label` in the warehouse's
    /// `model_descriptor` and `runtime_descriptor` the name its uploaded
    /// records share, changing no `_sha256` id
    FixVaultLabels {
        /// Count what would be filled without rewriting Parquet files
        #[arg(long)]
        dry_run: bool,
        /// Restrict the fill to vault sources naming these resource(s).
        /// Repeatable (`--resource A --resource B`) and comma-separated
        /// (`--resource A,B`); both accumulate. An id with no vault record is
        /// an error. Omit to fill every source.
        #[arg(
            long = "resource",
            value_name = "RESOURCE_ID",
            value_delimiter = ',',
            value_parser = parse_vault_resource_id
        )]
        resources: Vec<VaultResourceId>,
    },
    /// Manage clients
    Clients {
        #[command(subcommand)]
        action: ClientAction,
    },
    /// Manage user accounts — the operator principal, which authors work where
    /// a client does it. Accounts are separate from clients: a user never
    /// appears in the fleet a plan can reach
    Users {
        #[command(subcommand)]
        action: UserAction,
    },
    /// Manage organizations, and the two relations hung off them: which
    /// organization an account belongs to (`users set-org`) and who owns a
    /// device (`clients owner`). An organization holds no permission, but its
    /// memberships and ownerships decide access (`docs/cli.md`)
    Orgs {
        #[command(subcommand)]
        action: OrgAction,
    },
    /// Copy already-scored submissions for an eval, vlm_eval or asr_eval benchmark back
    /// into `incoming/` as fresh submissions so the next scoring passes
    /// (`process-submissions` + `score-eval`) score them again — use after a
    /// scorer fix. The benchmark is looked up in the catalog and the command
    /// errors unless it exists and is one of those; matching warehouse
    /// jobs (by `benchmark_id`) are then re-staged.
    /// Each re-stage mints a new `job_id` and `submitted_at = now`; the
    /// original `job_id`'s processed archive and warehouse rows are left
    /// untouched alongside the fresh ones. Because of that, re-running over
    /// the whole benchmark doubles the re-staged set each time — use
    /// `--submitted-before` to scope a run to the original (pre-migration)
    /// jobs and avoid that.
    RequeueEval {
        /// Benchmark id to re-score (e.g. `eval_ifbench_original`) — a
        /// catalog key, not the bare eval id. Must resolve to an eval, a
        /// vlm_eval or an asr_eval. Re-run per dataset to cover an eval's other benchmarks.
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
        /// exactly. This is the `runtime_version` that the scoring service
        /// returned when it scored the job. Use it to select the jobs that an
        /// earlier scorer version scored.
        #[arg(long)]
        score_runtime_version: Option<String>,
        /// List what would be re-staged without writing anything.
        #[arg(long)]
        dry_run: bool,
    },
    /// Inspect and clear the storage mutate lock — the advisory lock
    /// that serializes `process-submissions`, `fix-model-param`,
    /// `fix-canonical`, `fix-vault-labels`, and `requeue-eval`.
    /// Use this only to recover from a crashed command that left a stale
    /// lock behind.
    Unlock {
        /// Clear the lease even if it is still active (a command may be
        /// running). `s3` only: a `local_fs` lock is an `flock(2)`, never stale.
        #[arg(long)]
        force: bool,
    },
    /// Manage the write-only unverified submission archive
    /// (`submissions/unverified/`).
    Unverified {
        #[command(subcommand)]
        action: UnverifiedAction,
    },
    /// Print what the runtime wrote to stdout and stderr for a job, from every
    /// client that reported on it. Read-only.
    RuntimeOutput {
        #[arg(value_name = "JOB_ID", value_parser = parse_job_id)]
        job_id: JobId,
    },
    /// Maintain the `todo/` job queue: recycle expired leases, expire jobs past
    /// their deadline (writing synthetic failure records), rebuild the
    /// `eligible/` index from `avail/` jobs and registered client device
    /// profiles, GC orphaned markers, and delete stale `tmp/` files. Run on a
    /// cron. Writes only to `todo/` and the synthetic failure records in
    /// `submissions/incoming/`, so it takes no mutate lock.
    QueueMaintenance,
    /// Rebuild the reverse tag index (`tags-index/by-tag/`) from the
    /// authoritative forward tree, repairing any drift (e.g. from a crash
    /// mid-write). Idempotent — safe to run any time. The `todo/eligible/`
    /// index is rebuilt separately by `queue-maintenance`.
    Reindex,
    /// Flag clients so the next `queue-maintenance` run rebuilds their
    /// `todo/eligible/` markers from scratch. Use it to repair stale markers.
    /// A flagged client cannot claim, reclaim, or submit until that run clears
    /// the flag. `--all` pauses the whole fleet until then. The command writes
    /// only to `todo/`, so it takes no mutate lock. See `docs/cli.md`.
    FlagReindex {
        /// Client to flag. Omit it when you use `--all`.
        #[arg(value_parser = parse_client_id, required_unless_present = "all")]
        client_id: Option<ClientId>,
        /// Flag every registered client.
        #[arg(long, conflicts_with = "client_id")]
        all: bool,
    },
    /// Read and cancel plans. A plan is a set of jobs filed as a unit into a
    /// campaign and tracked by a manifest; filing one is `campaigns`. See
    /// `docs/plan-ingestion.md`.
    Plans {
        #[command(subcommand)]
        action: PlanAction,
    },
    /// Administer campaigns: the stable id that points at whichever plan is
    /// being executed, so a dashboard or a report can name one thing while the
    /// plan underneath it is revised. A campaign holds no jobs — filing work is
    /// `campaigns ingest` and stopping it is `plans cancel`.
    Campaigns {
        #[command(subcommand)]
        action: CampaignAction,
    },
    /// Administer the vault: the private artifacts a plan can name, served to
    /// the principal that owns them.
    ///
    /// **Nothing here creates a resource.** Files are uploaded and described
    /// over the API (`PUT /api/v1/vault/blobs/…`, then `POST /api/v1/vault/resources`), from
    /// the machine that holds them (`docs/api/vault.md`). This lists, shows and
    /// deletes records; `delete` also takes the files only its records named,
    /// and `prune` the files no record names.
    Vault {
        #[command(subcommand)]
        action: VaultAction,
    },
    /// Read and manage server-wide names for runtime descriptors. Needs `[database]`.
    Catalog {
        #[command(subcommand)]
        action: CatalogAction,
    },
    /// Ask the model parameter table (the model-params rows) what it knows, or add
    /// rows to it from a file. No action takes the mutate lock.
    Models {
        #[command(subcommand)]
        action: ModelAction,
    },
    /// Keep upstream dataset corpora servable: probe their upstreams, build
    /// missing artifacts, or compute a new catalog entry's pins. See
    /// `docs/design/vl-spec-decode-dataset-delivery.md`.
    DatasetCache {
        #[command(subcommand)]
        action: DatasetCacheAction,
    },
    /// Manage the SQL database the records live on when `[database]` is set
    /// (`docs/design/sql-record-store.md`).
    Db {
        #[command(subcommand)]
        action: DbAction,
    },
    /// Read the audit log, and prune it past its retention. Needs `[database]`.
    Audit {
        #[command(subcommand)]
        action: AuditCommand,
    },
    /// Print the OpenAPI document `GET /api/v1/openapi.json` serves. Reads no
    /// config: the document is this binary's own.
    Openapi {
        /// Write the API reference into this directory instead: a static page
        /// and its stylesheet, as mgmt serves them at `/docs/`.
        #[arg(long)]
        site: Option<PathBuf>,
    },
}

#[derive(Subcommand)]
enum ConfigAction {
    /// Check the config file and report known flavors without offer rules
    Check {
        /// Fail when a known flavor has no offer rule
        #[arg(long)]
        strict: bool,
    },
}

#[derive(Subcommand)]
enum AuditCommand {
    /// Print the newest events, one a line, newest first
    List {
        /// Only the events this organization's log lists, by id or handle
        #[arg(long, value_name = "ORG", value_parser = parse_org_ref)]
        org: Option<OrgRef>,
        /// Only these outcomes, comma-separated: succeeded, refused, failed
        #[arg(long)]
        outcome: Option<String>,
        /// Only these actor kinds, comma-separated: user, client, unverified, host
        #[arg(long)]
        actor: Option<String>,
        /// Only the events naming this record id, as a target or as the actor
        #[arg(long)]
        target: Option<String>,
        #[arg(long, default_value_t = 50)]
        limit: usize,
    },
    /// Delete the events older than the retention. Run it on a schedule
    Prune {
        /// Keep this many days of events
        #[arg(long, default_value_t = pipette_core::audit::RETENTION_DAYS)]
        older_than_days: u32,
        /// Count what would be deleted, and delete nothing
        #[arg(long)]
        dry_run: bool,
    },
}

#[derive(Subcommand)]
enum DbAction {
    /// Apply each migration that this binary has and the database lacks, or
    /// move the schema up or down to `--to`.
    ///
    /// Run this command before you deploy a binary that needs a newer schema.
    /// The `serve` command refuses an older schema. To move down, run it from
    /// the newer binary before you deploy the older one. A down migration can
    /// drop data.
    Migrate {
        /// The migration to leave as the newest applied. Without it, the
        /// command only applies.
        #[arg(long)]
        to: Option<i64>,
        /// Report the move, and change nothing.
        #[arg(long)]
        dry_run: bool,
    },
    /// Exit non-zero unless the database answers with the schema version that
    /// this binary expects.
    Check,
    /// Copy every record from the object-store sections into the database.
    ///
    /// A rerun writes only what differs. The command exits non-zero while a
    /// conflict or an unreadable record remains. The command takes no storage
    /// lock, because no record writer takes one. Before a cutover, stop `serve`
    /// and the commands that write records.
    Import {
        /// Report what would be written, and write nothing.
        #[arg(long)]
        dry_run: bool,
    },
    /// Copy the records of the database to the object-store sections.
    ///
    /// The command writes each record that differs. It gives each claim to the
    /// holder that the database names. It deletes each record that the database
    /// does not hold. It exits non-zero while a conflict or an unreadable record
    /// remains. The command takes no storage lock. Stop `serve` and the commands
    /// that write records before you run it.
    Export {
        /// Report what would be written and deleted, and change nothing.
        #[arg(long)]
        dry_run: bool,
    },
    /// Index the cell of each job that has no index row, and the flags of each
    /// finished run that has none.
    ///
    /// The command reads the job sources and the submission records. Run it
    /// once after migration `0007`, and again after `db import`. A second run
    /// writes only the rows that are still missing. The command skips a job
    /// source or a record that does not parse, and reports it.
    BackfillJobCells {
        /// Report the rows that the command would write. Write nothing.
        #[arg(long)]
        dry_run: bool,
    },
}

/// `campaigns` actions: the record's CRUD, plus the ingest that moves it
/// forward.
///
/// **No `retarget`.** A campaign's pointer is set only by filing work: `create`
/// files the first plan, and `ingest`, `variants`, `revise` and `rerun` mint a
/// plan that belongs to the campaign and supersedes what it was running.
/// Nothing points a campaign at a plan somebody chose, which is what keeps a
/// plan from ever having two campaigns.
///
/// **No permission checks here**, as everywhere else in this binary: shell
/// access to the storage root is the authority, and the ownership rules the
/// `/api/v1/campaigns` routes enforce bound a *signing key*, not an operator at a
/// terminal (`docs/authentication.md`). What ownership does here is decide the
/// record's `owner` field, which those routes then read.
#[derive(Subcommand)]
enum CampaignAction {
    /// Create a campaign with its first plan, filed from a plan document. A
    /// plan belongs to the campaign that minted it, so a campaign cannot be
    /// created around a plan that already exists.
    ///
    /// Prints its minted id, which is the handle everything else names it by —
    /// there is no slug index, because a campaign is reached from a listing or a
    /// link rather than typed.
    Create {
        /// What to print for a human, e.g. `"Nightly LFM2 regression"`.
        /// Free-text: nothing keys on it, and two campaigns may share one.
        name: String,
        /// The account that owns it, and is therefore the only one that may
        /// repoint it over HTTP. Exactly one of `--user` / `--org`.
        #[arg(long, value_parser = parse_user_id, conflicts_with = "org")]
        user: Option<UserId>,
        /// The organization that owns it: its `manage_orgs` holders repoint it,
        /// and it outlives any one of them. Either spelling of the handle.
        #[arg(long, value_name = "ORG", value_parser = parse_org_ref)]
        org: Option<OrgRef>,
        /// The TOML document for the first plan, as `campaigns ingest --plan`
        /// reads it.
        #[arg(long, value_name = "FILE")]
        plan: PathBuf,
        /// The reason for the first plan, for example `nightly LFM2 sweep`.
        /// An empty value is an error. An HTTP route treats an empty `note` as
        /// no note.
        #[arg(long)]
        note: Option<String>,
        /// Refuse the plan if any job names a model the table does not know,
        /// rather than warning.
        #[arg(long)]
        strict_models: bool,
        /// Expand and validate the plan document, print the job bodies, and
        /// write nothing. No campaign is created.
        #[arg(long)]
        dry_run: bool,
    },
    /// File a plan document into a campaign, and move the campaign onto it. The
    /// new plan belongs to the campaign and supersedes the campaign's current
    /// plan.
    /// To edit the plan it runs, keeping finished jobs, use `campaigns revise`.
    ///
    /// The plan is written first and the campaign advanced after, so a failure
    /// between them leaves a filed plan that never became current rather than a
    /// campaign naming a plan that does not exist. Run it again to repair.
    Ingest {
        #[arg(value_name = "CAMPAIGN_ID", value_parser = parse_campaign_id)]
        campaign_id: CampaignId,
        /// A TOML document for a scheduler plan. The server expands it into job
        /// bodies, and stores the document beside the plan.
        #[arg(long, value_name = "FILE")]
        plan: PathBuf,
        /// The reason for the plan, for example `nightly LFM2 sweep`.
        /// An empty value is an error. An HTTP route treats an empty `note` as
        /// no note.
        #[arg(long)]
        note: Option<String>,
        /// Refuse the ingest if any job names a model the table does not
        /// know, rather than warning. Fail-fast, so a refused plan writes
        /// nothing. Without this a plan still ingests and runs; what its rows
        /// lose is the `model_params_*_millions` columns, repairable with
        /// `fix-model-param` once the table has the model.
        #[arg(long)]
        strict_models: bool,
        /// Expand and validate the plan document, print the job bodies, and
        /// write nothing. The campaign does not move.
        #[arg(long)]
        dry_run: bool,
    },
    /// Revise the plan a campaign runs by its variants: drop some, append some,
    /// and file the result into the campaign. The revision carries over every
    /// job of the replaced plan that it still describes, finished ones
    /// included, and the replaced plan is cancelled, so only the jobs the edit
    /// dropped stop.
    ///
    /// If you give none of `--add`, `--add-from`, `--restore`, or
    /// `--remove`, the command prints the current plan's variants. It
    /// numbers them for `--remove` and writes nothing. An edit names that
    /// plan with `--base`.
    #[command(group(
        clap::ArgGroup::new("edit")
            .args(["add", "add_from", "restore", "remove"])
            .multiple(true)
            .requires("base")
    ))]
    Variants {
        #[arg(value_name = "CAMPAIGN_ID", value_parser = parse_campaign_id)]
        campaign_id: CampaignId,
        /// A TOML file of `[[variants]]` blocks to append, written as in a plan
        /// document. It may also hold `remove = [...]`, as the HTTP route's body
        /// does.
        #[arg(long, value_name = "FILE")]
        add: Option<PathBuf>,
        /// A plan of any campaign whose variants to append, each with that
        /// plan's `benchmarks` where it names none, after `--add`'s. Its
        /// finished runs answer them, as `--inherit-from` offers them.
        /// Repeatable.
        #[arg(long, value_name = "PLAN_ID", value_parser = parse_plan_id)]
        add_from: Vec<PlanId>,
        /// Append one variant of a plan from any campaign after the
        /// `--add-from` variants. Use `<PLAN_ID>:<INDEX>`. The index counts the plan's
        /// variants from 0, as `GET /api/v1/plans/{plan_id}/variants` lists
        /// them. The plan's finished runs answer the variant, as
        /// `--inherit-from` offers them, and the revision records the variant.
        /// Repeat this option to restore more variants.
        #[arg(long, value_name = "PLAN_ID:INDEX")]
        restore: Vec<RestoredVariant>,
        /// A variant to drop, by its number in the listing. Repeat for more.
        #[arg(long, value_name = "INDEX")]
        remove: Vec<usize>,
        /// The plan that the numbers came from, as the listing names it. The
        /// command refuses the edit if the campaign runs another plan. A
        /// repeat of an edit that succeeded gets this refusal.
        #[arg(long, value_name = "PLAN_ID", value_parser = parse_plan_id, requires = "edit")]
        base: Option<PlanId>,
        /// The note for the revision. The command also records the change that the
        /// revision makes.
        /// An empty value is an error. An HTTP route treats an empty `note` as
        /// no note.
        #[arg(long, requires = "edit")]
        note: Option<String>,
        /// Refuse the revision if any job names a model the table does not
        /// know, rather than warning. `campaigns ingest --strict-models`'s rule.
        #[arg(long, requires = "edit")]
        strict_models: bool,
        /// Apply the edit, print the revised document and its job bodies, and
        /// write nothing. The campaign and its plan stay as they are.
        #[arg(long, requires = "edit")]
        dry_run: bool,
        /// Let a cell the replaced plan has not answered take a finished run
        /// of the campaign's earlier plans, as it takes one of that plan's.
        /// Files the plan's own document too.
        #[arg(long, requires = "edit")]
        carry_earlier: bool,
        /// A plan of any campaign whose finished runs a cell the replaced
        /// plan has not answered may take, as it takes one of that plan's.
        /// Repeatable. Files the plan's own document too.
        #[arg(long, value_name = "PLAN_ID", value_parser = parse_plan_id, requires = "edit")]
        inherit_from: Vec<PlanId>,
    },
    /// Revise the plan a campaign runs by its whole document: file `--plan` in
    /// its place. The revision carries over every job of the replaced plan
    /// that it still describes, finished ones included, and the replaced plan
    /// is cancelled, so only the jobs the new document drops stop.
    ///
    /// Without `--plan`, prints the document of the plan the campaign runs and
    /// writes nothing, with that plan's id on stderr for `--base`.
    Revise {
        #[arg(value_name = "CAMPAIGN_ID", value_parser = parse_campaign_id)]
        campaign_id: CampaignId,
        /// The revised TOML plan document. It is filed as written.
        #[arg(long, value_name = "FILE", requires = "base")]
        plan: Option<PathBuf>,
        /// The plan that the document came from. The command refuses the
        /// revision if the campaign runs another plan. A repeat of a revision
        /// that succeeded gets this refusal.
        #[arg(long, value_name = "PLAN_ID", value_parser = parse_plan_id, requires = "plan")]
        base: Option<PlanId>,
        /// The note for the revision. The command also records the change that the
        /// revision makes.
        /// An empty value is an error. An HTTP route treats an empty `note` as
        /// no note.
        #[arg(long, requires = "plan")]
        note: Option<String>,
        /// Refuse the revision if any job names a model the table does not
        /// know, rather than warning. `campaigns ingest --strict-models`'s rule.
        #[arg(long, requires = "plan")]
        strict_models: bool,
        /// Expand the document. Print the document and its job bodies. Write
        /// nothing. The campaign and its plan do not change.
        #[arg(long, requires = "plan")]
        dry_run: bool,
        /// Let a cell the replaced plan has not answered take a finished run
        /// of the campaign's earlier plans, as it takes one of that plan's.
        /// Files the plan's own document too.
        #[arg(long, requires = "plan")]
        carry_earlier: bool,
        /// A plan of any campaign whose finished runs a cell the replaced
        /// plan has not answered may take, as it takes one of that plan's.
        /// Repeatable. Files the plan's own document too.
        #[arg(long, value_name = "PLAN_ID", value_parser = parse_plan_id, requires = "plan")]
        inherit_from: Vec<PlanId>,
    },
    /// Run finished or failed jobs of the plan a campaign runs again. Files
    /// that plan's document again as a revision that carries over every other
    /// job, runs each named one afresh as a new job, and cancels the plan it
    /// replaced. A job still queued or running is refused: re-running it would
    /// duplicate work in flight.
    Rerun {
        #[arg(value_name = "CAMPAIGN_ID", value_parser = parse_campaign_id)]
        campaign_id: CampaignId,
        /// A job of the plan the campaign runs, as the console's plan page or
        /// `GET /api/v1/plans/{plan_id}/jobs` lists it. Repeat for more.
        #[arg(long = "job", value_name = "JOB_ID", value_parser = parse_job_id, required = true)]
        jobs: Vec<JobId>,
        /// The note for the revision. The command also records the change that the
        /// revision makes.
        /// An empty value is an error. An HTTP route treats an empty `note` as
        /// no note.
        #[arg(long)]
        note: Option<String>,
        /// Refuse the revision if any job names a model the table does not
        /// know, rather than warning. `campaigns ingest --strict-models`'s rule.
        #[arg(long)]
        strict_models: bool,
        /// Check the jobs, print the plan's document and its job bodies, and
        /// write nothing. The campaign and its plan stay as they are.
        #[arg(long)]
        dry_run: bool,
    },
    /// Move a campaign back to an earlier plan of its history. The command
    /// files a new plan with a copy of that plan's document. It carries over
    /// each job of the replaced plan that the copy describes. It cancels the
    /// replaced plan. The new plan always takes the finished runs of the
    /// campaign's earlier plans. The results of the copied plan then come back
    /// with no new runs.
    Revert {
        #[arg(value_name = "CAMPAIGN_ID", value_parser = parse_campaign_id)]
        campaign_id: CampaignId,
        /// The earlier plan to copy, as `campaigns show` lists it. It must be in
        /// the campaign's history.
        #[arg(long, value_name = "PLAN_ID", value_parser = parse_plan_id)]
        to: PlanId,
        /// The plan the campaign runs. The command refuses the revert if the
        /// campaign runs another plan. A repeat of a revert that succeeded gets
        /// this refusal.
        #[arg(long, value_name = "PLAN_ID", value_parser = parse_plan_id)]
        base: PlanId,
        /// The note for the revision. The command also records the change that the
        /// revision makes.
        /// An empty value is an error. An HTTP route treats an empty `note` as
        /// no note.
        #[arg(long)]
        note: Option<String>,
        /// Refuse the revision if any job names a model the table does not
        /// know, rather than warning. `campaigns ingest --strict-models`'s rule.
        #[arg(long)]
        strict_models: bool,
        /// Expand the earlier plan's document. Print the document and its job
        /// bodies. Write nothing. The campaign and its plan do not change.
        #[arg(long)]
        dry_run: bool,
    },
    /// List campaigns, newest first, with what each one currently points at.
    List {
        /// Only the campaigns this account owns.
        #[arg(long, value_parser = parse_user_id, conflicts_with = "org")]
        user: Option<UserId>,
        /// Only this organization's.
        #[arg(long, value_name = "ORG", value_parser = parse_org_ref)]
        org: Option<OrgRef>,
    },
    /// Show one campaign: its owner, the plan it is running, and the plans that
    /// one supersedes, walked back through the chain.
    Show {
        #[arg(value_name = "CAMPAIGN_ID", value_parser = parse_campaign_id)]
        campaign_id: CampaignId,
    },
    /// Rename a campaign. The id is not renameable — being stable is the whole
    /// point of it.
    Rename {
        #[arg(value_name = "CAMPAIGN_ID", value_parser = parse_campaign_id)]
        campaign_id: CampaignId,
        /// The new display name
        #[arg(long)]
        name: String,
    },
    /// Hand a campaign to another owner, keeping the plan it names: a team
    /// taking over somebody's work inherits it mid-flight.
    Owner {
        #[arg(value_name = "CAMPAIGN_ID", value_parser = parse_campaign_id)]
        campaign_id: CampaignId,
        /// The account to hand it to. Exactly one of `--user` / `--org`.
        #[arg(long, value_parser = parse_user_id, conflicts_with = "org")]
        user: Option<UserId>,
        /// The organization to hand it to.
        #[arg(long, value_name = "ORG", value_parser = parse_org_ref)]
        org: Option<OrgRef>,
    },
    /// Delete a campaign. **The pointer only** — every plan that it filed keeps
    /// its manifest, its jobs and its lifecycle. The command refuses while one of
    /// those plans runs with no cancel requested. Stop those plans with `plans
    /// cancel` first.
    Delete {
        #[arg(value_name = "CAMPAIGN_ID", value_parser = parse_campaign_id)]
        campaign_id: CampaignId,
    },
}

/// `vault` actions. Each takes an owner the way `campaigns` does, because the
/// owner is what decides which devices may fetch the bytes.
#[derive(Subcommand)]
enum VaultAction {
    /// List vault resources, newest first.
    List {
        /// Only the resources this account owns.
        #[arg(long, value_parser = parse_user_id, conflicts_with = "org")]
        user: Option<UserId>,
        /// Only this organization's.
        #[arg(long, value_name = "ORG", value_parser = parse_org_ref)]
        org: Option<OrgRef>,
    },
    /// Show one resource, the plan entry it belongs to, and whether its bytes
    /// are still stored.
    Show {
        #[arg(value_name = "RESOURCE_ID", value_parser = parse_vault_resource_id)]
        resource_id: VaultResourceId,
    },
    /// Delete a resource and the rest of its upload, whose plan entry needs
    /// every file, and each file no other resource names. To change anything,
    /// delete and upload again.
    Delete {
        #[arg(value_name = "RESOURCE_ID", value_parser = parse_vault_resource_id)]
        resource_id: VaultResourceId,
    },
    /// Delete the stored files no resource names: what a delete from before
    /// files followed their resources left, or one that failed midway. Run by
    /// hand; nothing schedules it.
    Prune {
        /// Age threshold (e.g. `24h`, `7d`). A file written more recently is
        /// kept: an upload stores its file before it describes it.
        #[arg(long, value_parser = parse_duration)]
        older_than: std::time::Duration,
        /// Print what would be deleted without removing anything.
        #[arg(long)]
        dry_run: bool,
    },
}

#[derive(Subcommand)]
enum CatalogAction {
    /// Import llama.cpp stable releases and complete daily builds.
    Import {
        /// First UTC day to check for a daily build.
        #[arg(long)]
        since: Option<chrono::NaiveDate>,
    },
    /// Add new flavors to one imported entry.
    Refresh {
        #[arg(value_parser = parse_catalog_name)]
        name: CatalogName,
        #[arg(long, value_parser = parse_org_ref)]
        org: Option<OrgRef>,
    },
    /// Create a hand-made entry from name, release_time and runtimes or models in TOML.
    Create {
        file: PathBuf,
        #[arg(long, value_parser = parse_org_ref)]
        org: Option<OrgRef>,
        #[arg(long)]
        dry_run: bool,
    },
    /// Append one runtime or model from a TOML file to a hand-made entry.
    AddMember {
        #[arg(value_parser = parse_catalog_name)]
        name: CatalogName,
        file: PathBuf,
        #[arg(long, value_parser = parse_org_ref)]
        org: Option<OrgRef>,
        #[arg(long)]
        dry_run: bool,
    },
    Pin {
        #[arg(value_parser = parse_catalog_name)]
        name: CatalogName,
        #[arg(long, value_parser = parse_org_ref)]
        org: Option<OrgRef>,
    },
    Unpin {
        #[arg(value_parser = parse_catalog_name)]
        name: CatalogName,
        #[arg(long, value_parser = parse_org_ref)]
        org: Option<OrgRef>,
    },
    Hide {
        #[arg(value_parser = parse_catalog_name)]
        name: CatalogName,
        #[arg(long, value_parser = parse_org_ref)]
        org: Option<OrgRef>,
    },
    Unhide {
        #[arg(value_parser = parse_catalog_name)]
        name: CatalogName,
        #[arg(long, value_parser = parse_org_ref)]
        org: Option<OrgRef>,
    },
    /// Permanently tombstone a name. Without --yes, type the exact name again.
    Tombstone {
        #[arg(value_parser = parse_catalog_name)]
        name: CatalogName,
        #[arg(long, value_parser = parse_org_ref)]
        org: Option<OrgRef>,
        #[arg(long)]
        yes: bool,
    },
    /// List live entries, newest release first. Hidden entries are included.
    List {
        #[arg(long, value_parser = parse_org_ref)]
        org: Option<OrgRef>,
        #[arg(long = "kind")]
        kinds: Vec<DescriptorEntryKind>,
        #[arg(long = "state")]
        states: Vec<DescriptorEntryState>,
        #[arg(long = "runtime-type", value_parser = parse_catalog_runtime_type)]
        runtime_types: Vec<RuntimeType>,
        #[arg(long)]
        prefix: Option<String>,
        #[arg(long, value_parser = parse_catalog_name)]
        cursor: Option<CatalogName>,
        #[arg(long, default_value = "50")]
        limit: NonZeroUsize,
    },
    /// Show one live entry and its members in order.
    Show {
        #[arg(value_parser = parse_catalog_name)]
        name: CatalogName,
        #[arg(long, value_parser = parse_org_ref)]
        org: Option<OrgRef>,
    },
}

/// `dataset-cache` actions.
#[derive(Subcommand)]
enum DatasetCacheAction {
    /// Probe every built artifact's upstream (a blobless `git fetch`, or the
    /// evals server's samples digest) and record the verdict on its metadata,
    /// which is all `GET /datasets/{id}` reads. Run every minute. Exits non-zero
    /// while any upstream is not `ok`.
    Probe,
    /// Build every pinned artifact that is missing: fetch, verify the pinned
    /// hashes, repack, store. Never rebuilds or deletes an existing artifact.
    /// Exits non-zero if any build failed.
    Build,
    /// Fetch and pack a benchmark's upstream without a store, and print the
    /// hashes its catalog entry pins. Read-only.
    Pins {
        #[arg(value_parser = parse_benchmark_id)]
        benchmark_id: BenchmarkId,
    },
}

/// `models` actions: coverage before ingestion, or after scoring. See
/// `docs/cli.md` for the reports and the repair loop they feed.
#[derive(Subcommand)]
enum ModelAction {
    /// Resolve model name(s) against the table. Exits non-zero if any name
    /// has no entry, so CI can gate a plan on table coverage.
    Resolve {
        /// Model name(s) to resolve. Repeatable and comma-separated, like
        /// `fix-model-param --model`.
        #[arg(required = true, value_name = "MODEL_NAME", value_delimiter = ',')]
        names: Vec<String>,
    },
    /// Report every model the warehouse holds rows for that the table cannot
    /// resolve, with the row count waiting on each. Read-only, so it is safe to
    /// run while `process-submissions` is scoring.
    Unresolved {
        /// Emit the report as JSON, for a caller that acts on it rather than
        /// reads it.
        #[arg(long)]
        json: bool,
    },
    /// Add a model-params row for each entry of a TOML file that has none
    /// (`"LFM2-700M" = 742`, or `{ total = 8340, active = 1500 }`). A key
    /// that has a row keeps it, so an edit made in the app is never undone.
    Import {
        /// The file to read.
        #[arg(value_name = "FILE")]
        file: PathBuf,
        /// Report what would be added, and write nothing.
        #[arg(long)]
        dry_run: bool,
    },
}

#[derive(Subcommand)]
enum PlanAction {
    /// List plans, newest first.
    List {
        /// Show only plans in this lifecycle state. Omit for all.
        #[arg(long)]
        status: Option<PlanStatusArg>,
        /// Show only plans attributed to this submitter: a user account
        /// (`user_…`). Matched as a whole, opaque string. Omit for all. A plan
        /// that names no submitter is selected by no value here, so a plan the
        /// `campaigns` commands filed is reachable only through an unfiltered
        /// listing or its campaign.
        #[arg(long, value_parser = parse_submitter_id)]
        submitter: Option<UserId>,
        /// Show only plans filed into this campaign. Composes with the other
        /// filters. A plan filed outside every campaign, which an older build
        /// allowed, is selected by no value here.
        #[arg(long = "campaign", value_name = "CAMPAIGN_ID", value_parser = parse_campaign_id)]
        campaign_id: Option<CampaignId>,
        /// Columns to render, in the order given. Omit for the default set:
        /// `plan_id`, `note`, `submitter`, `status`, `created`, `jobs_expire`,
        /// `jobs`, `finished`, `starved`.
        #[arg(long = "columns", value_name = "COLUMN", value_delimiter = ',')]
        columns: Vec<PlanColumn>,
    },
    /// Show one plan's progress: its lifecycle status, any pending
    /// cancellation, the ingestion-time fleet-match warnings, and the progress
    /// snapshot `queue-maintenance` last computed.
    Status {
        /// The plan id (`plan-{uuid}`), as `plans list` prints it.
        #[arg(value_parser = parse_plan_id)]
        plan_id: PlanId,
        /// Explain why the plan's jobs are not scheduled. Reads the fleet, the
        /// queue and the eligible index, then names the gate that blocks the
        /// jobs and the clause that empties each starved group. Costs several
        /// extra store listings. Without this flag the command stays two
        /// storage operations.
        #[arg(long)]
        explain: bool,
        /// List the clients that hold the plan's jobs: the live lease
        /// holders, and the jobs stranded behind a lapsed lease. The live
        /// holders are the plan's work in flight. Names each holder by client
        /// id. Costs one extra `leased/` listing. Without this flag the command
        /// stays two storage operations.
        #[arg(long)]
        leases: bool,
    },
    /// Show a plan's results: one row per metric of each job's latest scoring
    /// run, carried-over jobs included. Read from pipette-duckdb (`[duckdb]`),
    /// which republishes every few minutes, so a result or a plan filed since
    /// may be missing.
    Results {
        /// The plan id (`plan-{uuid}`), as `plans list` prints it.
        #[arg(value_parser = parse_plan_id)]
        plan_id: PlanId,
        /// Rows to print.
        #[arg(long, default_value_t = 1000)]
        limit: usize,
        /// Rows to skip, for the next page.
        #[arg(long, default_value_t = 0)]
        offset: usize,
        /// Emit the page as JSON, in the shape `GET /api/v1/plans/{plan_id}/results`
        /// answers.
        #[arg(long)]
        json: bool,
    },
    /// Record a cancellation request for a plan. The command writes a cancel
    /// marker. The jobs remain claimable until the next `queue-maintenance`
    /// run retires them. A client can start a job during this interval.
    Cancel {
        /// The plan id (`plan-{uuid}`) to cancel.
        #[arg(value_parser = parse_plan_id)]
        plan_id: PlanId,
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
    Cancelling,
    Complete,
    Cancelled,
}

impl From<PlanStatusArg> for PlanStatus {
    fn from(arg: PlanStatusArg) -> Self {
        match arg {
            PlanStatusArg::Creating => PlanStatus::Creating,
            PlanStatusArg::Active => PlanStatus::Active,
            PlanStatusArg::PendingClients => PlanStatus::PendingClients,
            PlanStatusArg::Cancelling => PlanStatus::Cancelling,
            PlanStatusArg::Complete => PlanStatus::Complete,
            PlanStatusArg::Cancelled => PlanStatus::Cancelled,
        }
    }
}

/// A `--requires`/`--any-of` value, split on whitespace: `slugify` removes it
/// from every flag, while a comma can be part of one (`device:iphone14,2`).
#[derive(Debug, Clone)]
struct FlagList(Vec<String>);

/// A column of a table whose set and order `--columns` chooses.
trait TableColumn: Copy {
    /// The display row the column reads its cell from.
    type Row<'a>;
    fn header(self) -> &'static str;
    fn cell(self, row: &Self::Row<'_>) -> String;
}

fn column_table<C: TableColumn>(columns: &[C], rows: &[C::Row<'_>]) -> tabled::Table {
    let header: Vec<String> = columns
        .iter()
        .map(|&column| column.header().to_string())
        .collect();
    let body = rows
        .iter()
        .map(|row| columns.iter().map(|&column| column.cell(row)).collect());
    let mut table = std::iter::once(header)
        .chain(body)
        .collect::<tabled::builder::Builder>()
        .build();
    table.with(tabled::settings::Style::psql());
    table
}

/// Columns available to `clients list`.
///
/// `IntoStaticStr` derives [`header`](ClientColumn::header) from the variant names
/// in title case, so adding a column adds its heading too. `ClientId` overrides
/// the derivation because the acronym is `ID`, not the `Id` a case conversion
/// produces.
#[derive(Clone, Copy, PartialEq, Eq, clap::ValueEnum, strum::IntoStaticStr)]
#[value(rename_all = "snake_case")]
#[strum(serialize_all = "title_case")]
enum ClientColumn {
    #[strum(serialize = "Client ID")]
    ClientId,
    Organization,
    /// Who owns the device, as a record rather than as the free text
    /// `Organization` carries (`pipette_plan_types::ClientOwner`). Opt-in like
    /// every other column beyond the defaults.
    Owner,
    Details,
    Contact,
    Status,
    Registered,
    Tags,
    Permissions,
    Migrated,
    Derived,
    Declared,
}

impl TableColumn for ClientColumn {
    type Row<'a> = ClientRow;

    fn header(self) -> &'static str {
        self.into()
    }

    fn cell(self, row: &ClientRow) -> String {
        match self {
            Self::ClientId => row.client_id.clone(),
            Self::Organization => row.organization.clone(),
            Self::Owner => row.owner.clone(),
            Self::Details => row.details.clone(),
            Self::Contact => row.contact.clone(),
            Self::Status => row.status.clone(),
            Self::Registered => row.registered.clone(),
            Self::Tags => row.tags.clone(),
            Self::Permissions => row.permissions.clone(),
            Self::Migrated => row.migrated.clone(),
            Self::Derived => row.derived.clone(),
            Self::Declared => row.declared.clone(),
        }
    }
}

/// Columns available to `plans list`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
#[value(rename_all = "snake_case")]
enum PlanColumn {
    PlanId,
    Note,
    Submitter,
    Status,
    Created,
    JobsExpire,
    Jobs,
    Finished,
    Starved,
    Running,
    Available,
    Failed,
    Cancelled,
    Expired,
    Warnings,
    Snapshot,
}

/// `plans list`'s columns when `--columns` names none; the rest are opt-in.
const DEFAULT_PLAN_COLUMNS: [PlanColumn; 9] = [
    PlanColumn::PlanId,
    PlanColumn::Note,
    PlanColumn::Submitter,
    PlanColumn::Status,
    PlanColumn::Created,
    PlanColumn::JobsExpire,
    PlanColumn::Jobs,
    PlanColumn::Finished,
    PlanColumn::Starved,
];

impl TableColumn for PlanColumn {
    type Row<'a> = PlanRow<'a>;

    fn header(self) -> &'static str {
        match self {
            Self::PlanId => "Plan ID",
            Self::Note => "Note",
            Self::Submitter => "Submitter",
            Self::Status => "Status",
            Self::Created => "Created",
            Self::JobsExpire => "Jobs Expire",
            Self::Jobs => "Jobs",
            Self::Finished => "Finished",
            Self::Starved => "Starved",
            Self::Running => "Running",
            Self::Available => "Available",
            Self::Failed => "Failed",
            Self::Cancelled => "Cancelled",
            Self::Expired => "Expired",
            Self::Warnings => "Warnings",
            Self::Snapshot => "Snapshot",
        }
    }

    fn cell(self, row: &PlanRow<'_>) -> String {
        let plan = row.manifest;
        let snapshot = plan.progress_snapshot.as_ref();
        match self {
            Self::PlanId => plan.plan_id.to_string(),
            Self::Note => row.note.unwrap_or("-").to_string(),
            Self::Submitter => submitter_label(plan.submitted_by.as_ref()),
            Self::Status if row.cancel_pending => {
                format!("{} (cancel pending)", plan.status.label())
            }
            Self::Status => plan.status.label().to_string(),
            Self::Created => plan.created_at.to_rfc3339(),
            Self::JobsExpire => job_expiry_cell(plan.job_expiry),
            Self::Jobs => plan.job_ids.len().to_string(),
            Self::Finished => snapshot.map_or_else(
                || "-".to_string(),
                |s| format!("{}/{}", s.counts.finished, s.counts.total),
            ),
            Self::Starved => starved_cell(snapshot),
            Self::Running => count_cell(snapshot, |c| c.running),
            Self::Available => count_cell(snapshot, |c| c.available),
            Self::Failed => count_cell(snapshot, |c| c.failed),
            Self::Cancelled => count_cell(snapshot, |c| c.cancelled),
            Self::Expired => expired_cell(snapshot, plan.expired_jobs.len()),
            Self::Warnings => warnings_cell(&plan.warnings),
            Self::Snapshot => {
                snapshot.map_or_else(|| "-".to_string(), |s| s.computed_at.to_rfc3339())
            }
        }
    }
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
    /// scorer picks them up: a success or a terminal failure lands in
    /// `incoming/`, and a retriable failure is archived to `processed/`
    /// unrecorded. Use after approving a client whose earlier submissions
    /// were held. Each object is removed from the unverified tree once
    /// re-staged.
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
    /// List all registered clients, optionally filtered by tag or capability
    List {
        /// Filter to clients carrying this tag (served from the reverse tag
        /// index). Repeatable; a client must carry every `--tag` given (AND).
        #[arg(long = "tag", value_name = "TAG")]
        tags: Vec<String>,
        /// Flag a client must carry (the job body's `requires`). A client
        /// must carry every flag given. Repeat the option to add flags, or
        /// quote one whitespace-separated list.
        #[arg(
            long = "requires",
            value_name = "FLAG[ FLAG...]",
            value_parser = parse_flag_list
        )]
        requires: Vec<FlagList>,
        /// A group of flags, at least one of which a client must carry (one
        /// of the job body's `any_of` groups). Whitespace separates the flags
        /// in a group; quote the group. Repeat the option to add groups.
        #[arg(
            long = "any-of",
            value_name = "FLAG[ FLAG...]",
            value_parser = parse_flag_list
        )]
        any_of: Vec<FlagList>,
        /// Flag that rules a client out (the job body's `excludes`). A client
        /// that carries any given flag is excluded. Repeat the option to add
        /// flags, or quote one whitespace-separated list.
        #[arg(
            long = "excludes",
            value_name = "FLAG[ FLAG...]",
            value_parser = parse_flag_list
        )]
        excludes: Vec<FlagList>,
        /// A group of flags that rules a client out only in combination (one
        /// of the job body's `not_all_of` groups). A client that carries every
        /// flag of the group is excluded. Repeat the option to add groups.
        #[arg(
            long = "not-all-of",
            value_name = "FLAG[ FLAG...]",
            value_parser = parse_flag_list
        )]
        not_all_of: Vec<FlagList>,
        /// Columns to render, in the order given. Omit for the default set:
        /// every column but `owner`, `permissions`, `derived` and `declared`,
        /// plus the last two under a capability filter. `permissions` costs one
        /// read per client; the default columns cost one listing.
        #[arg(long = "columns", value_name = "COLUMN", value_delimiter = ',')]
        columns: Vec<ClientColumn>,
    },
    /// Report the capability flags the fleet reports, grouped by namespace
    /// and counted. Use the reported flags to write a job's `requires` set.
    Capabilities {
        /// Report over clients of this status alone (`approved` or `pending`).
        /// Omit it for every registered client.
        #[arg(long)]
        status: Option<ClientStatus>,
    },
    /// Manage a client's tags. Tags are flat labels (e.g. `team-mobile`,
    /// `us-east`) assigned manually here on the mgmt side — clients never set
    /// their own.
    Tag {
        #[command(subcommand)]
        action: TagAction,
    },
    /// Manage a client's permissions. A permission names one operation at one
    /// scope, and a client holds a set of them. An operator assigns them. A
    /// client never sets its own.
    Permission {
        #[command(subcommand)]
        action: PermissionAction,
    },
    /// Mint and manage pre-auth registration keys. A client presenting a valid
    /// key at registration is auto-approved, and takes the key's tags,
    /// organization label, owner and permission template.
    ///
    /// Under `clients` rather than at the top level: a key admits a *client*,
    /// and a user is admitted from its own keyspace by its own verb.
    Preauth {
        #[command(subcommand)]
        action: ClientPreauthAction,
    },
    /// Approve a pending client
    Approve {
        /// Client ID to approve
        #[arg(value_parser = parse_client_id)]
        client_id: ClientId,
        /// Permission to grant alongside the approval. Repeatable, and
        /// additive: it never removes a permission the client already holds.
        #[arg(long = "permission", value_name = "PERMISSION", value_parser = parse_client_permission)]
        permissions: Vec<ClientPermission>,
    },
    /// Reject (delete) a pending client
    Reject {
        /// Client ID to reject
        #[arg(value_parser = parse_client_id)]
        client_id: ClientId,
    },
    /// Delete a client identity, pending or approved, with its queue markers,
    /// ownership and tags (best effort; a re-run finishes the cleanup). Its
    /// submissions and warehouse rows stay.
    Delete {
        /// Client ID to delete
        #[arg(value_parser = parse_client_id)]
        client_id: ClientId,
    },
    /// List clients currently suspended from claiming (each holds a stale
    /// lease that must be reclaimed or expire before it can claim again)
    ListSuspended,
    /// Clear a client's suspension so it may claim jobs again
    Unsuspend {
        /// Client ID to unsuspend
        #[arg(value_parser = parse_client_id)]
        client_id: ClientId,
    },
    /// Show or set who owns a device: an organization directly, or a user
    /// account whose own membership then says which organization
    /// (`pipette-mgmt orgs`).
    ///
    /// Distinct from `--organization` on `clients update`, which is free text
    /// the device sent about itself with no record behind it. This points at a
    /// record.
    Owner {
        #[command(subcommand)]
        action: ClientOwnerAction,
    },
    /// Update mutable client details (organization, details, contact email).
    /// Pass at least one flag; the command prints each field it changed.
    Update {
        /// Client ID to update
        #[arg(value_parser = parse_client_id)]
        client_id: ClientId,
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
enum ClientOwnerAction {
    /// Show a device's owner, and the organization it rolls up to
    Show {
        #[arg(value_parser = parse_client_id)]
        client_id: ClientId,
    },
    /// Give a device an owner. Exactly one of `--org` / `--user`: a device
    /// belongs to one or the other, and naming both would be two answers to one
    /// question.
    Set {
        #[arg(value_parser = parse_client_id)]
        client_id: ClientId,
        /// The organization that owns this device directly — a rack machine, a
        /// lab phone, anything that outlives whoever set it up.
        #[arg(long, value_name = "ORG", value_parser = parse_org_ref, required_unless_present = "user")]
        org: Option<OrgRef>,
        /// The account that owns this device. Its organization follows that
        /// account's membership, so moving the account moves the device.
        #[arg(long, value_name = "USER_ID", value_parser = parse_user_id, conflicts_with = "org")]
        user: Option<UserId>,
    },
    /// Clear a device's owner, leaving it in no organization
    Clear {
        #[arg(value_parser = parse_client_id)]
        client_id: ClientId,
    },
}

#[derive(Subcommand)]
enum TagAction {
    /// Add one or more tags to a client (no-op for tags it already has)
    Add {
        /// Client ID to tag
        #[arg(value_parser = parse_client_id)]
        client_id: ClientId,
        /// One or more flat tags, e.g. `team-mobile us-east`: `[a-z0-9_-]`, at
        /// most 64 characters, trimmed and lowercased on input
        #[arg(required = true, value_name = "TAG")]
        tags: Vec<String>,
    },
    /// Remove one or more tags from a client (no-op for tags it lacks)
    Remove {
        /// Client ID to untag
        #[arg(value_parser = parse_client_id)]
        client_id: ClientId,
        /// One or more tags to remove
        #[arg(required = true, value_name = "TAG")]
        tags: Vec<String>,
    },
    /// List a client's tags
    List {
        /// Client ID whose tags to list
        #[arg(value_parser = parse_client_id)]
        client_id: ClientId,
    },
}

#[derive(Subcommand)]
enum PermissionAction {
    /// Grant one or more permissions (idempotent — an already-held permission
    /// is skipped). Name one client, or pass `--tag` to grant every client
    /// that carries a tag.
    Add {
        /// Grant every client that carries this tag rather than one named client.
        #[arg(long, value_name = "TAG")]
        tag: Option<String>,
        /// `<client_id> <permission>...`, or `<permission>...` with `--tag`.
        ///
        /// One variadic positional rather than a client id followed by a list,
        /// because clap fills positionals left to right: with two, `--tag lab
        /// perform_work` would read `perform_work` as the client id.
        #[arg(required = true, value_name = "ARG")]
        args: Vec<String>,
    },
    /// Revoke one or more permissions (no-op for permissions the client lacks)
    Remove {
        /// Client ID to revoke from
        #[arg(value_parser = parse_client_id)]
        client_id: ClientId,
        /// One or more permissions, e.g. `perform_work submit_plans`. A user
        /// permission is accepted here and nowhere else: clearing one a client
        /// was granted before the partition is what removal is for.
        #[arg(required = true, value_name = "PERMISSION", value_parser = parse_removable_permission)]
        permissions: Vec<String>,
    },
    /// Replace a client's whole set. With no permission given, clears it.
    Set {
        /// Client ID to set
        #[arg(value_parser = parse_client_id)]
        client_id: ClientId,
        /// The permissions the client is to hold. Replaces whatever the client
        /// holds now. Omit them all to clear the set.
        #[arg(value_name = "PERMISSION", value_parser = parse_client_permission)]
        permissions: Vec<ClientPermission>,
    },
    /// List a client's stored permissions, sorted
    List {
        /// Client ID whose permissions to list
        #[arg(value_parser = parse_client_id)]
        client_id: ClientId,
    },
}

#[derive(Subcommand)]
enum OrgAction {
    /// Create an organization under a handle. The handle is folded to lower
    /// case and must be unique — a second organization for one handle is
    /// refused, naming the one that already holds it.
    Create {
        /// The handle the organization is addressed by: `[a-z0-9_-]`, at most
        /// 64 characters, folded to lower case, never beginning `org_`. Stable — `orgs update` never changes it, because it
        /// is the index key every command can name the organization by.
        #[arg(value_parser = parse_org_slug)]
        slug: OrgSlug,
        /// What to print for a human, e.g. `"Acme Corp., Inc."`. Defaults to
        /// the handle, which is truthful and overwritable rather than blank.
        #[arg(long)]
        name: Option<String>,
        /// Take over a handle claim whose organization record is missing — the
        /// residue of an earlier create that failed between its two writes.
        /// Deliberate rather than automatic, for the reason
        /// `users create --adopt-stranded-claim` gives: a stranded claim and a
        /// create still in flight look identical from here.
        #[arg(long)]
        adopt_stranded_claim: bool,
    },
    /// List every organization, in creation order, with its member and device
    /// counts
    List,
    /// Show one organization, its members and the devices it owns
    Show {
        /// The organization's handle or its id — either spelling resolves
        #[arg(value_name = "ORG", value_parser = parse_org_ref)]
        org: OrgRef,
    },
    /// Rename an organization. The handle is not renameable: it is the index
    /// key, so changing it is a claim, a record write and a release — three
    /// writes for a value whose purpose is to be stable. An organization that
    /// needs a different handle is a new organization and a reassignment.
    Update {
        #[arg(value_name = "ORG", value_parser = parse_org_ref)]
        org: OrgRef,
        /// The new display name
        #[arg(long)]
        name: String,
    },
    /// Mark an organization as wound down, leaving the record and its
    /// memberships in place.
    ///
    /// A disabled organization refuses only Clerk provisioning into it: its
    /// members keep everything their membership scopes. Stopping the accounts
    /// is `users disable`, one account at a time.
    Disable {
        #[arg(value_name = "ORG", value_parser = parse_org_ref)]
        org: OrgRef,
    },
    /// Return a disabled organization to the active roster
    Enable {
        #[arg(value_name = "ORG", value_parser = parse_org_ref)]
        org: OrgRef,
    },
    /// Delete an organization and release its handle. Refused while any account
    /// still names it, any device is owned by it directly, or it keeps a
    /// private benchmark, a campaign, a vault resource or an unexpired client
    /// pre-auth key. An org id is minted, so a stranded reference never
    /// resolves again and would go unnoticed. Clear those one by one, then use
    /// `--detach` to clear the accounts and devices.
    Delete {
        #[arg(value_name = "ORG", value_parser = parse_org_ref)]
        org: OrgRef,
        /// Clear every membership and direct ownership naming this
        /// organization, then delete it. A device owned by a *member* is
        /// released by clearing that member's organization, so it keeps its
        /// owner and simply stops rolling up here. Clears nothing else: any
        /// other dependent refuses the command before anything is detached.
        #[arg(long)]
        detach: bool,
    },
    /// Release a handle claim whose organization record is gone — the residue
    /// of a create that failed between its two writes, or of a `delete` that
    /// stopped after the record. Refuses a handle a live organization still
    /// holds, so it cannot take one out from under another.
    ReleaseClaim {
        /// The handle whose claim to release
        #[arg(value_parser = parse_org_slug)]
        slug: OrgSlug,
    },
    /// List the accounts belonging to an organization
    Members {
        #[arg(value_name = "ORG", value_parser = parse_org_ref)]
        org: OrgRef,
    },
    /// List the devices an organization owns: the ones granted to it directly,
    /// plus the ones owned by its members
    Clients {
        #[arg(value_name = "ORG", value_parser = parse_org_ref)]
        org: OrgRef,
    },
    /// The benchmarks an organization keeps to itself: entries under
    /// `org-benchmarks/{org_id}/` that `GET /benchmarks` serves to the
    /// organization's own devices and members beside the public catalog, and to
    /// nobody else
    ///
    /// `list`, `show` and `delete` also take the `org_…` id of a deleted
    /// organization that left entries behind, to remove them; `put` needs the
    /// record.
    Benchmarks {
        #[command(subcommand)]
        action: OrgBenchmarkAction,
    },
}

#[derive(Subcommand)]
enum OrgBenchmarkAction {
    /// List an organization's private benchmarks, or every organization's,
    /// with why an unreadable entry does not read
    List {
        /// The organization, or every one when omitted
        #[arg(value_name = "ORG", value_parser = parse_org_ref)]
        org: Option<OrgRef>,
    },
    /// Print one private benchmark as the TOML the catalog holds
    Show {
        #[arg(value_name = "ORG", value_parser = parse_org_ref)]
        org: OrgRef,
        #[arg(value_parser = parse_benchmark_id)]
        benchmark_id: BenchmarkId,
    },
    /// Write a private benchmark from a TOML file, creating or replacing the
    /// entry. The file is the same shape as one under `benchmarks/`, and its
    /// stem is the benchmark id, as it is there. Refused when the public
    /// catalog or another organization already holds that id, when another
    /// organization's entry pins its dataset or names its evals-server samples
    /// and no public one does, or when a samples name is not one plain path
    /// segment: every id resolves to one definition, an artifact is served by
    /// its digest, and samples by name.
    Put {
        #[arg(value_name = "ORG", value_parser = parse_org_ref)]
        org: OrgRef,
        /// `{benchmark_id}.toml`
        file: PathBuf,
    },
    /// Delete a private benchmark. Refused when the organization keeps none
    /// under that id.
    Delete {
        #[arg(value_name = "ORG", value_parser = parse_org_ref)]
        org: OrgRef,
        #[arg(value_parser = parse_benchmark_id)]
        benchmark_id: BenchmarkId,
    },
}

#[derive(Subcommand)]
enum UserAction {
    /// Create an account for an email address. The address is the account's
    /// handle and must be unique — a second account for one address is refused,
    /// naming the account that already holds it.
    Create {
        /// Contact email. Folded to lower case, which is what makes one mailbox
        /// one account; `operator+ci@` and `operator@` stay distinct.
        #[arg(value_parser = parse_contact_email)]
        email: ContactEmail,
        /// Take over an address claim whose account record is missing — the
        /// residue of an earlier create that failed between its two writes.
        /// Deliberate rather than automatic: a stranded claim and a create
        /// still in flight look identical from here, so adopting one blind can
        /// take an address another operator is mid-way through claiming.
        #[arg(long)]
        adopt_stranded_claim: bool,
        /// Permission to grant the new account. Repeatable. Without this flag
        /// the account holds nothing until `users permission add` grants it —
        /// an empty set and no set at all mean the same thing for a user, so
        /// there is no flag for the second.
        #[arg(long = "permission", value_name = "PERMISSION", value_parser = parse_user_permission)]
        permissions: Vec<UserPermission>,
        #[command(flatten)]
        group: PermissionGroupFlags,
        /// `human`, or `service` for a job's account: it signs with the keys
        /// its pre-auth keys register, never through Clerk, needs `--org`, and
        /// holds no admin name.
        #[arg(long = "type", value_name = "TYPE", default_value_t = UserAccountType::Human)]
        account_type: UserAccountType,
        /// The organization the account joins, by handle or id.
        #[arg(long, value_name = "ORG", value_parser = parse_org_ref)]
        org: Option<OrgRef>,
        /// Make the service account a campaign reader of this campaign.
        /// Repeatable. A campaign reader reads these campaigns and their plans
        /// alone, and holds `read_plans` alone.
        #[arg(long = "campaign", value_name = "CAMPAIGN_ID", value_parser = parse_campaign_id)]
        campaigns: Vec<CampaignId>,
    },
    /// List every account, in creation order
    List,
    /// Show one account
    Show {
        /// User ID whose account to show
        #[arg(value_parser = parse_user_id)]
        user_id: UserId,
    },
    /// Admit an account that asked for itself. A keyless `POST /api/v1/users/register`
    /// creates the account `pending` and registers the key that asked, so this
    /// is the whole of the decision: the machine re-runs nothing once it lands.
    ///
    /// Its own verb rather than a second spelling of `enable`, because it names
    /// a different transition. `enable` returns an offboarded account to
    /// service, and confusing "never admitted" with "admitted and withdrawn" is
    /// exactly the mistake a status column exists to prevent.
    Approve {
        /// User ID to approve
        #[arg(value_parser = parse_user_id)]
        user_id: UserId,
        /// Permission to grant alongside the approval. Repeatable, and
        /// additive: it never removes one the account already holds. Approving
        /// is when an operator decides what the account is for, which is why
        /// the grant is on this command rather than a second one.
        #[arg(long = "permission", value_name = "PERMISSION", value_parser = parse_user_permission)]
        permissions: Vec<UserPermission>,
        #[command(flatten)]
        group: PermissionGroupFlags,
    },
    /// Mark an account as a person's (`human`) or a job's (`service`).
    SetType {
        /// User ID to retype
        #[arg(value_parser = parse_user_id)]
        user_id: UserId,
        /// `human` or `service`
        #[arg(value_name = "TYPE")]
        account_type: UserAccountType,
    },
    /// Make a service account a campaign reader of these campaigns, or replace
    /// a reader's campaigns. A reader stays one: a wider account is a new one.
    SetCampaigns {
        /// User ID of the service account
        #[arg(value_parser = parse_user_id)]
        user_id: UserId,
        /// The campaigns it reads, all of them
        #[arg(value_name = "CAMPAIGN_ID", required = true, value_parser = parse_campaign_id)]
        campaigns: Vec<CampaignId>,
    },
    /// Refuse every request from an account, leaving the record in place.
    /// Prefer this to deletion — the plans it filed stay attributed to a
    /// principal that still resolves.
    ///
    /// Its keys stay too, and `enable` restores every one of them, including
    /// any a thief registered with a stolen key. So revoke the stolen key, and
    /// every key added since it was stolen (`users key list` lists the newest
    /// last), with `users key revoke` before enabling.
    Disable {
        /// User ID to disable
        #[arg(value_parser = parse_user_id)]
        user_id: UserId,
    },
    /// Return a disabled account to service. An account that has never been
    /// admitted is `users approve`, not this.
    Enable {
        /// User ID to enable
        #[arg(value_parser = parse_user_id)]
        user_id: UserId,
    },
    /// Delete an account, its address claim and its permissions. For an account
    /// created in error — offboarding is a `disable`.
    ///
    /// Refused while the account still owns a device, a campaign or a vault
    /// resource: each would be left naming a principal that no longer exists,
    /// and because a user id is minted that reference never resolves again.
    /// `--detach` clears the devices; hand over or delete the rest first.
    Delete {
        /// User ID to delete
        #[arg(value_parser = parse_user_id)]
        user_id: UserId,
        /// Clear the ownership of every device this account owns, then delete
        /// it. The devices stay registered and become unowned, which is where a
        /// fresh self-registration sits until an operator vouches for it.
        #[arg(long)]
        detach: bool,
    },
    /// Release an address claim whose account record is gone — the residue of a
    /// create that failed between its two writes, or of a `delete` that stopped
    /// after the record. Refuses an address a live account still holds, so it
    /// cannot be used to take one account's address out from under it.
    ///
    /// Its own command rather than a second spelling of `delete`, because it
    /// names a different key: `delete` takes the id and reads the address off
    /// the record, which is exactly what a stranded claim no longer has.
    ///
    /// The refusal is a read and then a delete, not one step, so a
    /// `create --adopt-stranded-claim` that lands in between still loses its
    /// address. Conditioning the delete on the object's version would close it;
    /// no backend exposes that here. Do not adopt and release concurrently.
    ReleaseClaim {
        /// The address whose claim to release
        #[arg(value_parser = parse_contact_email)]
        email: ContactEmail,
    },
    /// Assign an account to an organization, or clear its membership.
    ///
    /// An operator's assignment, never the account's own: a registering machine
    /// names no organization, for the same reason it does not approve itself.
    /// The membership bounds the account's bare roster grants and
    /// `manage_clients`, and decides which organization's campaigns, vault
    /// resources and private benchmarks it reaches, so moving an org admin makes
    /// it the new organization's admin.
    SetOrg {
        /// The account to assign
        #[arg(value_parser = parse_user_id)]
        user_id: UserId,
        /// The organization's handle or id. Omit it and pass `--none` to clear
        /// the membership.
        #[arg(value_name = "ORG", value_parser = parse_org_ref, required_unless_present = "none")]
        org: Option<OrgRef>,
        /// Clear the membership, leaving the account in no organization.
        #[arg(long, conflicts_with = "org")]
        none: bool,
    },
    /// Manage an account's permissions. The user half of the vocabulary: plan
    /// and fleet verbs an operator holds, never the work verbs a device does
    Permission {
        #[command(subcommand)]
        action: UserPermissionAction,
    },
    /// List and revoke the keys an account signs in with, one per machine
    Key {
        #[command(subcommand)]
        action: UserKeyAction,
    },
    /// Mint and manage pre-auth keys that register a machine against an account
    Preauth {
        #[command(subcommand)]
        action: UserPreauthAction,
    },
}

/// The permission groups, as flags.
///
/// Flattened into every command that assigns a user permission rather than
/// repeated in each, so `users create`, `users approve` and
/// `users permission add|set` carry one spelling of a group and one set of
/// refusals. The conflicts name `permissions`, the id each of those commands
/// gives its list of names, and each other.
#[derive(Args, Clone, Copy, Default)]
struct PermissionGroupFlags {
    /// Assign the organization-member group: submit and cancel the plans of
    /// the account's own campaigns, read its organization's too, and read the
    /// fleet.
    #[arg(long, conflicts_with_all = ["permissions", "org_admin", "server_admin"])]
    org_member: bool,
    /// Assign the organization-administrator group: everything inside the
    /// account's own organization, a member's plans and its people, devices,
    /// campaigns, benchmarks and vault. Scoped by membership, so an account
    /// holding it in no organization administers nothing.
    #[arg(long, conflicts_with_all = ["permissions", "org_member", "server_admin"])]
    org_admin: bool,
    /// Assign the whole user vocabulary, through the `_any_` verbs. Every
    /// plan, every client and the whole organization roster on this server,
    /// not just the account's own. The work verbs stay a client's and are not
    /// grantable here either way.
    #[arg(long, conflicts_with_all = ["permissions", "org_member", "org_admin"])]
    server_admin: bool,
}

impl PermissionGroupFlags {
    /// The group given, or `None` for neither. Clap refuses both.
    fn group(self) -> Option<UserPermissionGroup> {
        match (self.org_member, self.org_admin, self.server_admin) {
            (true, ..) => Some(UserPermissionGroup::OrgMember),
            (_, true, _) => Some(UserPermissionGroup::OrgAdmin),
            (.., true) => Some(UserPermissionGroup::ServerAdmin),
            _ => None,
        }
    }

    /// Expands a group to its names here, so nothing downstream or stored sees
    /// which spelling the operator used.
    fn resolve(self, permissions: Vec<UserPermission>) -> Vec<UserPermission> {
        self.group()
            .map_or(permissions, |group| group.permissions().to_vec())
    }
}

#[derive(Subcommand)]
enum UserPermissionAction {
    /// Grant one or more permissions (idempotent — an already-held permission
    /// is skipped)
    Add {
        /// User ID to grant to
        #[arg(value_parser = parse_user_id)]
        user_id: UserId,
        /// One or more permissions, e.g. `submit_plans read_clients`
        #[arg(
            required_unless_present_any = ["org_member", "org_admin", "server_admin"],
            value_name = "PERMISSION",
            value_parser = parse_user_permission
        )]
        permissions: Vec<UserPermission>,
        #[command(flatten)]
        group: PermissionGroupFlags,
    },
    /// Revoke one or more permissions (no-op for permissions the account lacks)
    Remove {
        /// User ID to revoke from
        #[arg(value_parser = parse_user_id)]
        user_id: UserId,
        /// One or more permissions to revoke
        #[arg(required = true, value_name = "PERMISSION", value_parser = parse_user_permission)]
        permissions: Vec<UserPermission>,
    },
    /// Replace an account's whole set. With no permission given, clears it.
    Set {
        /// User ID to set
        #[arg(value_parser = parse_user_id)]
        user_id: UserId,
        /// The permissions the account is to hold. Omit them all to clear.
        #[arg(value_name = "PERMISSION", value_parser = parse_user_permission)]
        permissions: Vec<UserPermission>,
        #[command(flatten)]
        group: PermissionGroupFlags,
    },
    /// List an account's permissions, with implied ones expanded
    List {
        /// User ID whose permissions to list
        #[arg(value_parser = parse_user_id)]
        user_id: UserId,
    },
}

#[derive(Subcommand)]
enum UserKeyAction {
    /// List an account's keys, oldest first
    List {
        /// User ID whose keys to list
        #[arg(value_parser = parse_user_id)]
        user_id: UserId,
    },
    /// Revoke one of an account's keys. It stops signing at once and its
    /// keypair is free to register again; the account's other keys keep
    /// signing.
    Revoke {
        /// User ID holding the key
        #[arg(value_parser = parse_user_id)]
        user_id: UserId,
        /// Key ID, as `users key list` prints it
        #[arg(value_parser = parse_user_key_id)]
        key_id: UserKeyId,
    },
}

#[derive(Subcommand)]
enum UserPreauthAction {
    /// Mint a key and print its token **once** (the secret is never stored or
    /// shown again). The key names the account it admits a machine to, so a
    /// holder cannot redirect it at another one.
    Create {
        /// The account a machine registering with this key joins
        #[arg(value_parser = parse_user_id)]
        user_id: UserId,
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
        /// Free-form note stored with the key (shown in `users preauth list`).
        #[arg(long)]
        note: Option<NonEmptyTrimmedString>,
    },
    /// List all user pre-auth keys (metadata only — never the secret)
    List,
    /// Revoke a key so it can no longer be used
    Revoke {
        /// Key id (the part after the token's prefix and before the `.`)
        #[arg(value_parser = parse_user_preauth_key_id)]
        key_id: UserPreauthKeyId,
    },
    /// Delete keys that can no longer grant: expired records, plus the markers
    /// left behind by spent single-use keys. A spent key deletes its own record
    /// on consume and `revoke` deletes on demand, so neither leaves a record
    /// here to reap.
    Prune {
        /// Report what would be deleted without deleting anything.
        #[arg(long)]
        dry_run: bool,
    },
}

#[derive(Subcommand)]
enum ClientPreauthAction {
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
        /// Permission to grant every client that registers with this key.
        /// Repeatable. Without this flag the key stores no template and the
        /// registration default applies (`submit_results` alone).
        #[arg(long = "permission", value_name = "PERMISSION", value_parser = parse_client_permission)]
        permissions: Vec<ClientPermission>,
        /// Mint a key whose clients hold no permission at all. Stores an empty
        /// template, which differs from no template at all.
        #[arg(long, conflicts_with = "permissions")]
        no_permissions: bool,
        /// Organization label to stamp on clients registering with this key.
        /// Free text, not ownership: `--owner-org` puts the device in an
        /// organization.
        // Its own id, so the audit does not read the label as an organization.
        #[arg(long = "org", id = "org_label", value_name = "LABEL")]
        org: Option<String>,
        /// The organization that owns every client registering with this key,
        /// as `clients owner set --org` would set it.
        #[arg(long, value_name = "ORG", value_parser = parse_org_ref)]
        owner_org: Option<OrgRef>,
        /// The account that owns every client registering with this key, as
        /// `clients owner set --user` would set it.
        #[arg(long, value_name = "USER_ID", value_parser = parse_user_id, conflicts_with = "owner_org")]
        owner_user: Option<UserId>,
        /// Free-form note stored with the key (shown in `clients preauth
        /// list`).
        #[arg(long)]
        note: Option<NonEmptyTrimmedString>,
    },
    /// List all pre-auth keys (metadata only — never the secret).
    List,
    /// Revoke a key so it can no longer be used.
    Revoke {
        /// Key id (the part after the token's prefix and before the `.`).
        #[arg(value_parser = parse_client_preauth_key_id)]
        key_id: ClientPreauthKeyId,
    },
    /// Delete keys that can no longer grant: expired records, plus the markers
    /// left behind by spent single-use keys. A spent key deletes its own record
    /// on consume and `revoke` deletes on demand, so neither leaves a record
    /// here to reap.
    Prune {
        /// Report what would be deleted without deleting anything.
        #[arg(long)]
        dry_run: bool,
    },
}

fn read_model_param_table(path: &std::path::Path) -> anyhow::Result<ModelParamTable> {
    let body = std::fs::read_to_string(path)
        .map_err(|e| anyhow::anyhow!("failed to read {}: {e}", path.display()))?;
    ModelParamTable::from_toml(&body)
}

fn report_import(report: &model_params::ImportReport, dry_run: bool) {
    let keys = |keys: &[ModelKey]| {
        keys.iter()
            .map(ModelKey::as_str)
            .collect::<Vec<_>>()
            .join(", ")
    };
    println!(
        "models import: {verb} {added} row(s); {unchanged} already held the file's counts",
        verb = if dry_run { "would add" } else { "added" },
        added = report.added.len(),
        unchanged = report.unchanged,
    );
    if !report.added.is_empty() {
        println!("  {}", keys(&report.added));
    }
    if !report.differing.is_empty() {
        println!(
            "{count} row(s) hold other counts than the file and were kept: {list}",
            count = report.differing.len(),
            list = keys(&report.differing),
        );
    }
}

/// Prints each name's folded key, which on a miss is what gets added. CI
/// branches on the exit code.
fn resolve_models(table: &ModelParamTable, names: &[String]) -> anyhow::Result<()> {
    let resolved = model_audit::resolve_names(table, names);
    resolved.iter().for_each(|name| match name.entry {
        Some(entry) => println!(
            "ok       {input} -> {key} (total {total}, active {active})",
            input = name.input,
            key = name.key,
            total = entry.total,
            active = entry.active,
        ),
        None => println!(
            "MISSING  {input} -> {key}",
            input = name.input,
            key = name.key,
        ),
    });

    let missing = model_audit::missing_keys(&resolved);
    if missing.is_empty() {
        println!(
            "All {count} name(s) resolve against the model parameter table ({entries} entries).",
            count = resolved.len(),
            entries = table.len(),
        );
        return Ok(());
    }
    anyhow::bail!(
        "{count} model(s) have no entry in the model parameter table: {list}. Add a model-params row for \
         each, or every row they produce keeps whatever parameter counts the client reported.",
        count = missing.len(),
        list = missing.into_iter().collect::<Vec<_>>().join(", "),
    )
}

/// Exits zero either way: `models resolve` is the gate.
async fn report_unresolved_models(
    stores: &Stores,
    table: &ModelParamTable,
    json: bool,
) -> anyhow::Result<()> {
    let report = model_audit::unresolved_in_warehouse(stores, table).await?;

    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&model_audit::UnresolvedReport {
                catalog_entries: table.len(),
                report: &report,
            })?
        );
        return Ok(());
    }

    if report.is_clean() {
        println!(
            "Every one of the {scanned} warehouse row(s) resolves against the model parameter table ({entries} \
             entries).",
            scanned = report.scanned,
            entries = table.len(),
        );
        return Ok(());
    }

    println!(
        "{gaps} of {scanned} warehouse row(s) name a model that the model parameter table ({entries} \
         entries) cannot resolve:",
        // Gaps in the model parameter table only: no new entry fixes the buckets below.
        gaps = report.named.values().map(|m| m.rows).sum::<usize>(),
        scanned = report.scanned,
        entries = table.len(),
    );
    report.named.iter().for_each(|(key, miss)| {
        println!("  {key}  ({rows} row(s))", rows = miss.rows);
        if let Some(descriptor) = &miss.sample_descriptor {
            println!("      sample model_descriptor: {descriptor}");
        }
    });
    if !report.named.is_empty() {
        println!("Add a model-params row for each, then run `fix-model-param` to repair the rows.");
    }
    if report.unparsed.rows > 0 {
        println!(
            "{rows} further row(s) carry a model_descriptor this server cannot parse. An entry in the model \
             parameter table will not fix those: this build does not know the shape.",
            rows = report.unparsed.rows,
        );
        if let Some(descriptor) = &report.unparsed.sample_descriptor {
            println!("      sample model_descriptor: {descriptor}");
        }
    }
    if report.no_descriptor.rows > 0 {
        println!(
            "{rows} further row(s) carry no model_descriptor at all, which on a warehouse with \
             pre-descriptor history is expected. Nothing here can place them: the client has to \
             send one.",
            rows = report.no_descriptor.rows,
        );
    }
    Ok(())
}

async fn run_models(action: ModelAction, config: Config) -> anyhow::Result<()> {
    let stores = build_stores(&config).await?;
    let store = stores.model_params.as_ref();
    match action {
        ModelAction::Resolve { names } => {
            resolve_models(&ModelParamTable::load(store).await?, &names)
        }
        ModelAction::Unresolved { json } => {
            let table = ModelParamTable::load(store).await?;
            report_unresolved_models(&stores, &table, json).await
        }
        ModelAction::Import { file, dry_run } => {
            let file = read_model_param_table(&file)?;
            let report = model_params::import(store, &file, dry_run).await?;
            report_import(&report, dry_run);
            Ok(())
        }
    }
}

fn print_openapi(site: Option<&Path>) -> anyhow::Result<()> {
    let openapi = OpenApi::build()?;
    let Some(dir) = site else {
        println!("{}", openapi.document());
        return Ok(());
    };
    std::fs::create_dir_all(dir)
        .map_err(|e| anyhow::anyhow!("failed to create {}: {e}", dir.display()))?;
    openapi.site()?.into_iter().try_for_each(|(name, text)| {
        let path = dir.join(name);
        std::fs::write(&path, text)
            .map_err(|e| anyhow::anyhow!("failed to write {}: {e}", path.display()))
    })
}

/// Install the log subscriber on stderr, so a command's stdout stays its output
/// alone. A malformed `RUST_LOG` fails the command rather than being dropped.
fn init_logging(format: LogFormat) -> anyhow::Result<()> {
    use std::io::IsTerminal;

    use tracing_subscriber::util::SubscriberInitExt;

    let filter = log_filter()
        .from_env()
        .map_err(|e| anyhow::anyhow!("RUST_LOG is not a valid log filter: {e}"))?;
    let ansi = std::io::stderr().is_terminal();
    log_dispatch(format, filter, std::io::stderr, ansi)
        .try_init()
        .map_err(|e| anyhow::anyhow!("installing the log subscriber: {e}"))
}

/// `info` unless `RUST_LOG` names directives: the request span is opened at
/// `info`, and a quieter filter drops it along with the identifiers it carries.
fn log_filter() -> tracing_subscriber::filter::Builder {
    EnvFilter::builder().with_default_directive(LevelFilter::INFO.into())
}

/// Separate from [`init_logging`] so a test can read what production writes.
fn log_dispatch<W>(format: LogFormat, filter: EnvFilter, writer: W, ansi: bool) -> tracing::Dispatch
where
    W: for<'a> tracing_subscriber::fmt::MakeWriter<'a> + Send + Sync + 'static,
{
    let builder = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(writer);
    match format {
        LogFormat::Text => builder.with_ansi(ansi).finish().into(),
        // Every enclosing span, not just the innermost: a nested span would
        // hide the request span.
        LogFormat::Json => builder
            .json()
            .flatten_event(true)
            .with_current_span(false)
            .with_span_list(true)
            .finish()
            .into(),
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let matches = Cli::command().get_matches();
    let cli = Cli::from_arg_matches(&matches)?;
    init_logging(cli.log_format)?;
    if let Command::Openapi { site } = &cli.command {
        return print_openapi(site.as_deref());
    }

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
    let audit = if writes(&cli.command) {
        start_audit(&config, &matches).await
    } else {
        None
    };
    let Some((stores, audit)) = audit else {
        return Box::pin(run(cli.command, config, &config_path)).await;
    };
    let ended = run_to_end(
        Box::pin(run(cli.command, config, &config_path)),
        shutdown_signal(),
    )
    .await;
    let outcome = match &ended {
        Ended::Returned(Ok(())) => AuditOutcome::Succeeded,
        Ended::Returned(Err(_)) | Ended::Interrupted | Ended::Panicked => AuditOutcome::Failed,
    };
    let write = async {
        if let Err(e) = audit.finish(&stores, outcome).await {
            tracing::error!(error = %format!("{e:#}"), "failed to write the audit event");
        }
    };
    match ended {
        Ended::Returned(result) => {
            write.await;
            result
        }
        Ended::Panicked => {
            write.await;
            anyhow::bail!("the command panicked; the audit event records a failure")
        }
        Ended::Interrupted => {
            // The operator asked to stop, so the write is bounded and a second
            // signal ends it.
            tokio::select! {
                () = write => {}
                () = shutdown_signal() => {
                    tracing::warn!("second signal: the interrupted command's audit event is not written");
                }
                () = tokio::time::sleep(AUDIT_DRAIN) => {
                    tracing::warn!(
                        wait_secs = AUDIT_DRAIN.as_secs(),
                        "the interrupted command's audit event was not written in time"
                    );
                }
            }
            eprintln!("Error: a signal stopped the command; the audit event records a failure");
            // Exit now: dropping the runtime would wait for a blocking task
            // that the stopped command started.
            std::process::exit(INTERRUPTED_EXIT)
        }
    }
}

/// The exit status of a command that a `SIGINT` or a `SIGTERM` stopped, as a
/// shell reports a `SIGINT`.
const INTERRUPTED_EXIT: i32 = 130;

/// How a command that changes records ended.
enum Ended {
    Returned(anyhow::Result<()>),
    /// A `SIGINT` or a `SIGTERM` arrived first.
    Interrupted,
    Panicked,
}

/// Run `command` until it returns, panics, or `stop` (a signal) ends first, so
/// that its audit event is written in each case. Only `SIGKILL` leaves no event.
async fn run_to_end(
    command: impl std::future::Future<Output = anyhow::Result<()>>,
    stop: impl std::future::Future<Output = ()>,
) -> Ended {
    use futures::FutureExt;
    tokio::select! {
        ran = std::panic::AssertUnwindSafe(command).catch_unwind() => match ran {
            Ok(result) => Ended::Returned(result),
            Err(_) => Ended::Panicked,
        },
        () = stop => Ended::Interrupted,
    }
}

/// The audit event of a command that changes records, opened before it runs.
/// Without `[database]` there is no audit log to write to. It reads and writes
/// the database alone, so it opens no object storage.
async fn start_audit(
    config: &Config,
    matches: &clap::ArgMatches,
) -> Option<(AuditStores, CommandAudit)> {
    let Some(stores) = pipette_store_init::build_audit_stores(config) else {
        tracing::debug!("not audited: the audit log is kept only on a [database]");
        return None;
    };
    let mut names = Vec::new();
    let mut org_refs = Vec::new();
    let mut leaf = matches;
    while let Some((name, sub)) = leaf.subcommand() {
        names.push(name);
        leaf = sub;
    }
    // `slug` is the handle `orgs create` and `orgs release-claim` take.
    org_refs.extend(
        leaf.ids()
            .filter(|id| matches!(id.as_str(), "org" | "owner_org" | "slug"))
            .flat_map(|id| leaf.get_raw(id.as_str()).into_iter().flatten())
            .filter_map(|value| value.to_str())
            .map(str::to_owned),
    );
    // Lossy, so an argument that is not UTF-8, such as a path, cannot stop the
    // command.
    let args: Vec<String> = std::env::args_os()
        .skip(1)
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect();
    let audit = CommandAudit::start(&stores, names.join(" "), args, org_refs).await;
    Some((stores, audit))
}

/// Whether `command` changes records. Exhaustive, so a new command is sorted
/// into one or the other; a dry run changes nothing.
fn writes(command: &Command) -> bool {
    match command {
        // Its writes are audited per request.
        Command::Serve | Command::RuntimeOutput { .. } => false,
        // A storage lock, the schema, an export, the document and the config check
        // change no records.
        Command::Config { .. } | Command::Unlock { .. } | Command::Openapi { .. } => false,
        Command::Db { action } => match action {
            DbAction::Import { dry_run } | DbAction::BackfillJobCells { dry_run } => !dry_run,
            DbAction::Check | DbAction::Migrate { .. } | DbAction::Export { .. } => false,
        },
        Command::ProcessSubmissions
        | Command::ScoreEval
        | Command::QueueMaintenance
        | Command::Reindex
        | Command::FlagReindex { .. } => true,
        Command::FixModelParam { dry_run, .. }
        | Command::FixCanonical { dry_run, .. }
        | Command::FixVaultLabels { dry_run, .. }
        | Command::RequeueEval { dry_run, .. } => !dry_run,
        Command::Clients { action } => match action {
            ClientAction::List { .. }
            | ClientAction::Capabilities { .. }
            | ClientAction::ListSuspended => false,
            ClientAction::Tag { action } => !matches!(action, TagAction::List { .. }),
            ClientAction::Permission { action } => !matches!(action, PermissionAction::List { .. }),
            ClientAction::Preauth { action } => match action {
                ClientPreauthAction::List => false,
                ClientPreauthAction::Prune { dry_run, .. } => !dry_run,
                ClientPreauthAction::Create { .. } | ClientPreauthAction::Revoke { .. } => true,
            },
            ClientAction::Owner { action } => !matches!(action, ClientOwnerAction::Show { .. }),
            ClientAction::Approve { .. }
            | ClientAction::Reject { .. }
            | ClientAction::Delete { .. }
            | ClientAction::Unsuspend { .. }
            | ClientAction::Update { .. } => true,
        },
        Command::Users { action } => match action {
            UserAction::List | UserAction::Show { .. } => false,
            UserAction::Permission { action } => {
                !matches!(action, UserPermissionAction::List { .. })
            }
            UserAction::Key { action } => !matches!(action, UserKeyAction::List { .. }),
            UserAction::Preauth { action } => match action {
                UserPreauthAction::List => false,
                UserPreauthAction::Prune { dry_run, .. } => !dry_run,
                UserPreauthAction::Create { .. } | UserPreauthAction::Revoke { .. } => true,
            },
            UserAction::Create { .. }
            | UserAction::Approve { .. }
            | UserAction::SetType { .. }
            | UserAction::SetCampaigns { .. }
            | UserAction::Disable { .. }
            | UserAction::Enable { .. }
            | UserAction::Delete { .. }
            | UserAction::ReleaseClaim { .. }
            | UserAction::SetOrg { .. } => true,
        },
        Command::Orgs { action } => match action {
            OrgAction::List
            | OrgAction::Show { .. }
            | OrgAction::Members { .. }
            | OrgAction::Clients { .. } => false,
            OrgAction::Benchmarks { action } => matches!(
                action,
                OrgBenchmarkAction::Put { .. } | OrgBenchmarkAction::Delete { .. }
            ),
            OrgAction::Create { .. }
            | OrgAction::Update { .. }
            | OrgAction::Disable { .. }
            | OrgAction::Enable { .. }
            | OrgAction::Delete { .. }
            | OrgAction::ReleaseClaim { .. } => true,
        },
        Command::Unverified { action } => match action {
            UnverifiedAction::Prune { dry_run, .. }
            | UnverifiedAction::Delete { dry_run, .. }
            | UnverifiedAction::Promote { dry_run, .. } => !dry_run,
        },
        Command::Plans { action } => matches!(action, PlanAction::Cancel { .. }),
        Command::Campaigns { action } => match action {
            CampaignAction::List { .. } | CampaignAction::Show { .. } => false,
            CampaignAction::Create { dry_run, .. }
            | CampaignAction::Ingest { dry_run, .. }
            | CampaignAction::Rerun { dry_run, .. }
            | CampaignAction::Revert { dry_run, .. } => !dry_run,
            // Without an edit each prints what it would revise.
            CampaignAction::Variants {
                add,
                add_from,
                restore,
                remove,
                dry_run,
                ..
            } => {
                !dry_run
                    && (add.is_some()
                        || !add_from.is_empty()
                        || !restore.is_empty()
                        || !remove.is_empty())
            }
            CampaignAction::Revise { plan, dry_run, .. } => !dry_run && plan.is_some(),
            CampaignAction::Rename { .. }
            | CampaignAction::Owner { .. }
            | CampaignAction::Delete { .. } => true,
        },
        Command::Vault { action } => match action {
            VaultAction::List { .. } | VaultAction::Show { .. } => false,
            VaultAction::Prune { dry_run, .. } => !dry_run,
            VaultAction::Delete { .. } => true,
        },
        Command::Catalog { action } => match action {
            CatalogAction::List { .. } | CatalogAction::Show { .. } => false,
            CatalogAction::Import { .. } | CatalogAction::Refresh { .. } => true,
            CatalogAction::Create { dry_run, .. } | CatalogAction::AddMember { dry_run, .. } => {
                !dry_run
            }
            CatalogAction::Pin { .. }
            | CatalogAction::Unpin { .. }
            | CatalogAction::Hide { .. }
            | CatalogAction::Unhide { .. }
            | CatalogAction::Tombstone { .. } => true,
        },
        Command::Models { action } => match action {
            ModelAction::Resolve { .. } | ModelAction::Unresolved { .. } => false,
            ModelAction::Import { dry_run, .. } => !dry_run,
        },
        Command::DatasetCache { action } => !matches!(action, DatasetCacheAction::Pins { .. }),
        Command::Audit { action } => match action {
            AuditCommand::List { .. } => false,
            AuditCommand::Prune { dry_run, .. } => !dry_run,
        },
    }
}

/// Warns once for each known flavor without an offer rule, and returns the
/// number of those flavors.
fn warn_unruled_flavors(catalog: &DescriptorCatalogConfig) -> usize {
    catalog
        .unruled_flavors()
        .inspect(|(runtime_type, flavor)| {
            tracing::warn!(
                runtime_type = %runtime_type,
                flavor = %flavor,
                "offer rule missing: flavor never offered; add a [[descriptor_catalog.offer_rules]] entry to offer it"
            );
        })
        .count()
}

fn check_config(config: &Config, config_path: &Path, strict: bool) -> anyhow::Result<()> {
    let unknown = Config::unknown_keys(config_path)?;
    unknown.iter().for_each(|key| {
        tracing::warn!(key = %key, "config key unknown: ignored, so its default applies");
    });
    let missing = warn_unruled_flavors(&config.descriptor_catalog);
    if strict && (missing > 0 || !unknown.is_empty()) {
        anyhow::bail!(
            "config check failed: {missing} known flavors have no offer rule, and {} keys are unknown. Add a rule for each flavor to offer, and correct or remove each unknown key. Or run without --strict to only warn.",
            unknown.len()
        );
    }
    tracing::info!(
        offer_rules = config.descriptor_catalog.offer_rules.len(),
        missing_rules = missing,
        unknown_keys = unknown.len(),
        strict,
        "config check passed"
    );
    Ok(())
}

async fn run(command: Command, config: Config, config_path: &Path) -> anyhow::Result<()> {
    let lock_ttl = std::time::Duration::from_secs(config.mutate_lock_ttl_secs.get());
    match command {
        Command::Config {
            action: ConfigAction::Check { strict },
        } => check_config(&config, config_path, strict),
        Command::Serve => {
            warn_unruled_flavors(&config.descriptor_catalog);
            tracing::info!(
                offer_rules = config.descriptor_catalog.offer_rules.len(),
                "descriptor catalog offer rules loaded"
            );
            let stores = build_stores(&config).await?;
            serve(config, stores).await
        }
        Command::ProcessSubmissions => {
            let stores = build_stores(&config).await?;
            run_locked(
                &config.storage,
                config.probe_backend_capabilities,
                "mutate",
                "process-submissions",
                lock_ttl,
                Box::pin(score::run_process_submissions(&config, stores)),
            )
            .await
        }
        Command::ScoreEval => {
            // Not `mutate`: two score-evals must not score one job, but the
            // warehouse writers stay free during a multi-minute /score call.
            let stores = build_stores(&config).await?;
            run_locked(
                &config.storage,
                config.probe_backend_capabilities,
                "score-eval",
                "score-eval",
                lock_ttl,
                score::run_score_eval(&config, stores),
            )
            .await
        }
        Command::FixModelParam { dry_run, models } => {
            let stores = build_stores(&config).await?;
            let table = ModelParamTable::load(stores.model_params.as_ref()).await?;
            let totals = if dry_run {
                fix_model_param::run(&stores, &table, true, &models).await?
            } else {
                run_locked(
                    &config.storage,
                    config.probe_backend_capabilities,
                    "mutate",
                    "fix-model-param",
                    lock_ttl,
                    fix_model_param::run(&stores, &table, false, &models),
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
                    config.probe_backend_capabilities,
                    "mutate",
                    "fix-canonical",
                    lock_ttl,
                    fix_canonical::run(&stores, false, &columns),
                )
                .await
            }
        }
        Command::FixVaultLabels { dry_run, resources } => {
            let stores = build_stores(&config).await?;
            let totals = if dry_run {
                fix_vault_labels::run(&stores, true, &resources).await?
            } else {
                run_locked(
                    &config.storage,
                    config.probe_backend_capabilities,
                    "mutate",
                    "fix-vault-labels",
                    lock_ttl,
                    fix_vault_labels::run(&stores, false, &resources),
                )
                .await?
            };
            totals.log(dry_run);
            Ok(())
        }
        Command::Clients { action } => {
            let stores = build_stores(&config).await?;
            handle_client_action(action, stores).await
        }
        Command::Users { action } => {
            // No mutate lock here or in the arms below that say so: their
            // keyspaces are disjoint from the paths `score` / `fix-*` lock.
            let stores = build_stores(&config).await?;
            handle_user_action(action, stores).await
        }
        Command::Orgs { action } => {
            // No mutate lock, as for `users`.
            let stores = build_stores(&config).await?;
            handle_org_action(action, stores).await
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
            // A live run writes the `incoming/` that `score` consumes.
            if dry_run {
                requeue_eval::run(&stores, &benchmark_id, &filters, true).await
            } else {
                run_locked(
                    &config.storage,
                    config.probe_backend_capabilities,
                    "mutate",
                    "requeue-eval",
                    lock_ttl,
                    requeue_eval::run(&stores, &benchmark_id, &filters, false),
                )
                .await
            }
        }
        Command::Unlock { force } => storage_lock::unlock(&config.storage, force).await,
        // `main` prints it before loading a config.
        Command::Openapi { site } => print_openapi(site.as_deref()),
        Command::Db { action } => run_db(action, &config).await,
        Command::Unverified { action } => {
            let stores = build_stores(&config).await?;
            // No mutate lock, as for `users`.
            handle_unverified_action(action, stores).await
        }
        Command::RuntimeOutput { job_id } => {
            let stores = build_stores(&config).await?;
            print!(
                "{}",
                runtime_output_text(&*stores.submissions, &job_id).await?
            );
            Ok(())
        }
        Command::DatasetCache { action } => {
            let stores = build_stores(&config).await?;
            // Worst case: every artifact at its deadline, plus a minute. Each
            // lock is its own, so neither waits on `mutate`. A crashed holder
            // keeps its lease to expiry: minutes for a probe, but up to the
            // whole build run for a build (hours with an unbuilt asr-eval
            // corpus), which operations.md says how to clear.
            let artifacts = dataset_cache::pinned_count(&stores).await?.max(1) as u64;
            let lease = |per_artifact: std::num::NonZeroU64| {
                std::time::Duration::from_secs(artifacts * per_artifact.get() + 60)
            };
            let probe_ttl = lease(config.dataset_probe_timeout_secs);
            match action {
                DatasetCacheAction::Probe => {
                    run_locked(
                        &config.storage,
                        config.probe_backend_capabilities,
                        "dataset-cache-probe",
                        "dataset-cache probe",
                        probe_ttl,
                        dataset_cache::run_probe(&config, &stores),
                    )
                    .await
                }
                DatasetCacheAction::Build => {
                    // Each artifact at its own format's deadline, an asr-eval
                    // corpus only while unbuilt: see `build_run_deadline`.
                    let build_ttl = dataset_cache::build_run_deadline(&config, &stores)
                        .await?
                        .saturating_add(std::time::Duration::from_secs(60));
                    run_locked(
                        &config.storage,
                        config.probe_backend_capabilities,
                        "dataset-cache-build",
                        "dataset-cache build",
                        build_ttl,
                        dataset_cache::run_build(&config, &stores),
                    )
                    .await
                }
                DatasetCacheAction::Pins { benchmark_id } => {
                    let pins = dataset_cache::pins(&config, &stores, &benchmark_id).await?;
                    println!("{pins}");
                    Ok(())
                }
            }
        }
        Command::QueueMaintenance => {
            // Refuse a schema behind this binary before any pass writes.
            database::check(&config).await?;
            let config = config.for_queue_maintenance();
            let stores = build_stores(&config).await?;
            stores.todo.validate_backend().await?;
            let catalog = org_catalog::load_server_catalog(&*stores.catalog).await?;
            // No mutate lock: its only write to `incoming/` is a create-if-absent
            // body under a key exclusive to one job.
            queue_maintenance::run(
                &*stores.todo,
                &*stores.clients,
                &*stores.tags,
                &*stores.submissions,
                &*stores.plans,
                &catalog,
                queue_maintenance::MaintenanceSettings {
                    tmp_max: std::time::Duration::from_secs(config.todo_tmp_max_age_secs.get()),
                    plan_creating_max: std::time::Duration::from_secs(
                        config.plan_creating_max_age_secs.get(),
                    ),
                    lost_lease_limit: config.plan_lost_lease_limit,
                },
            )
            .await
        }
        Command::Reindex => {
            let stores = build_stores(&config).await?;
            let report = pipette_store::reindex_tags(&*stores.clients, &*stores.tags).await?;
            tracing::info!(
                added = report.added,
                removed = report.removed,
                "tag index reindexed"
            );
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
        Command::FlagReindex { client_id, all } => {
            let stores = build_stores(&config).await?;
            // No mutate lock, as for `users`.
            handle_flag_reindex(client_id, all, &stores).await
        }
        Command::Models { action } => run_models(action, config).await,
        Command::Campaigns { action } => {
            // No mutate lock, as for `users`. Only the job-creating actions
            // need the backend probe and the model parameter table.
            let strict_models = match &action {
                CampaignAction::Create { strict_models, .. }
                | CampaignAction::Ingest { strict_models, .. }
                | CampaignAction::Variants { strict_models, .. }
                | CampaignAction::Revise { strict_models, .. }
                | CampaignAction::Rerun { strict_models, .. }
                | CampaignAction::Revert { strict_models, .. } => Some(*strict_models),
                CampaignAction::List { .. }
                | CampaignAction::Show { .. }
                | CampaignAction::Rename { .. }
                | CampaignAction::Owner { .. }
                | CampaignAction::Delete { .. } => None,
            };
            if strict_models.is_some() {
                validate_plan_index_backend(&config).await?;
            }
            let stores = build_stores(&config).await?;
            let model_table = if strict_models.is_some() {
                Some(ModelParamTable::load(stores.model_params.as_ref()).await?)
            } else {
                None
            };
            let models = match (&model_table, strict_models) {
                (Some(table), Some(true)) => ModelCheck::strict(table),
                (Some(table), _) => ModelCheck::warn(table),
                (None, _) => ModelCheck::none(),
            };
            let creating_max_age =
                std::time::Duration::from_secs(config.plan_creating_max_age_secs.get());
            // Only an action filing a new document or job bodies can carry a
            // token; a re-run or a revert files a stored document, which
            // carries none.
            let imports = match &action {
                CampaignAction::Create { .. } | CampaignAction::Ingest { .. } => true,
                CampaignAction::Variants { add, remove, .. } => add.is_some() || !remove.is_empty(),
                CampaignAction::Revise { plan, .. } => plan.is_some(),
                CampaignAction::Rerun { .. }
                | CampaignAction::Revert { .. }
                | CampaignAction::List { .. }
                | CampaignAction::Show { .. }
                | CampaignAction::Rename { .. }
                | CampaignAction::Owner { .. }
                | CampaignAction::Delete { .. } => false,
            }
            .then(|| {
                HfImports::from_config(&config)
                    .map(|imports| imports.with_progress(Progress::logged()))
            })
            .transpose()
            .map_err(|e| anyhow::anyhow!("failed to build the HuggingFace import client: {e}"))?;
            let settings = IngestSettings {
                models,
                max_jobs: config.plan_max_jobs,
                creating_max_age,
                import: imports.as_ref(),
            };
            handle_campaign_action(action, &stores, settings).await
        }
        // No mutate lock, as for `users`.
        Command::Vault { action } => {
            let stores = build_stores(&config).await?;
            handle_vault_action(action, stores).await
        }
        Command::Catalog { action } => {
            let stores = build_stores(&config).await?;
            handle_catalog_action(action, &stores, &config).await
        }
        Command::Plans { action } => {
            let stores = build_stores(&config).await?;
            // No mutate lock, as for `users`.
            handle_plan_action(action, &stores, &config).await
        }
        Command::Audit { action } => {
            let stores = build_stores(&config).await?;
            run_audit(action, &stores).await
        }
    }
}

/// The run store and the GitHub client for `catalog import` and `catalog refresh`.
fn catalog_import_context<'a>(
    stores: &'a Stores,
    command: &str,
) -> anyhow::Result<(&'a dyn CatalogImportRunStore, GitHubClient)> {
    let runs = stores
        .catalog_import_runs
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("catalog {command} needs [database]"))?;
    let token = std::env::var("GITHUB_TOKEN")
        .map_err(|_| anyhow::anyhow!("catalog {command} needs GITHUB_TOKEN"))?;
    Ok((runs, GitHubClient::new(GitHubToken::new(token))?))
}

async fn catalog_scope(
    stores: &Stores,
    reference: Option<&OrgRef>,
) -> anyhow::Result<CatalogScope> {
    match reference {
        None => Ok(CatalogScope::Global),
        Some(reference) => Ok(CatalogScope::Org(
            org::require(&*stores.orgs, reference).await?.org_id,
        )),
    }
}

async fn handle_catalog_action(
    action: CatalogAction,
    stores: &Stores,
    config: &Config,
) -> anyhow::Result<()> {
    let catalog = stores
        .descriptor_catalog
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("catalog commands need [database]"))?;
    match action {
        CatalogAction::Import { since } => {
            let (runs, api) = catalog_import_context(stores, "import")?;
            let result = catalog_import::import(
                &api,
                catalog,
                runs,
                &config.descriptor_catalog,
                since,
                chrono::Utc::now(),
            )
            .await?;
            println!(
                "Created {} entries; added {} members; hid {} entries.",
                result.entries_created, result.members_added, result.entries_hidden
            );
        }
        CatalogAction::Refresh { name, org } => {
            anyhow::ensure!(
                org.is_none(),
                "catalog refresh only accepts global importer entries"
            );
            let (runs, api) = catalog_import_context(stores, "refresh")?;
            let result = catalog_import::refresh(
                &api,
                catalog,
                runs,
                &config.descriptor_catalog,
                &name,
                chrono::Utc::now(),
            )
            .await?;
            println!("Added {} members to {name}.", result.members_added);
        }
        CatalogAction::Create { file, org, dry_run } => {
            let scope = catalog_scope(stores, org.as_ref()).await?;
            let document = std::fs::read_to_string(&file)?;
            let preview = descriptor_catalog_admin::create_with_vault(
                catalog,
                &catalog_hub(config)?,
                Some(stores.vault.as_ref()),
                &scope,
                &document,
                chrono::Utc::now(),
                dry_run,
            )
            .await?;
            print_catalog_preview(&preview, dry_run, &mut std::io::stdout())?;
        }
        CatalogAction::AddMember {
            name,
            file,
            org,
            dry_run,
        } => {
            let scope = catalog_scope(stores, org.as_ref()).await?;
            let document = std::fs::read_to_string(&file)?;
            let preview = descriptor_catalog_admin::add_member_with_vault(
                catalog,
                descriptor_catalog_admin::CatalogSources {
                    hub: &catalog_hub(config)?,
                    vault: Some(stores.vault.as_ref()),
                },
                &scope,
                &name,
                &document,
                chrono::Utc::now(),
                dry_run,
            )
            .await?;
            print_catalog_preview(&preview, dry_run, &mut std::io::stdout())?;
            print_model_member_repos(&preview);
        }
        CatalogAction::Pin { name, org } => {
            let scope = catalog_scope(stores, org.as_ref()).await?;
            change_catalog_state(catalog, &scope, &name, CatalogTransition::Pin).await?;
        }
        CatalogAction::Unpin { name, org } => {
            let scope = catalog_scope(stores, org.as_ref()).await?;
            change_catalog_state(catalog, &scope, &name, CatalogTransition::Unpin).await?;
        }
        CatalogAction::Hide { name, org } => {
            let scope = catalog_scope(stores, org.as_ref()).await?;
            change_catalog_state(catalog, &scope, &name, CatalogTransition::Hide).await?;
        }
        CatalogAction::Unhide { name, org } => {
            let scope = catalog_scope(stores, org.as_ref()).await?;
            change_catalog_state(catalog, &scope, &name, CatalogTransition::Unhide).await?;
        }
        CatalogAction::Tombstone { name, org, yes } => {
            let scope = catalog_scope(stores, org.as_ref()).await?;
            let entry = descriptor_catalog_admin::live_entry(catalog, &scope, &name).await?;
            if !yes {
                anyhow::ensure!(
                    std::io::stdin().is_terminal(),
                    "tombstone requires a terminal or --yes"
                );
                eprint!("Type {} to permanently tombstone it: ", entry.name);
                std::io::stderr().flush()?;
                // A blocking thread keeps the shutdown branch of `run_to_end`
                // pollable, so Ctrl-C at the prompt stops the command.
                let confirmation = tokio::task::spawn_blocking(|| {
                    let mut line = String::new();
                    std::io::stdin().read_line(&mut line).map(|_| line)
                })
                .await??;
                confirm_catalog_tombstone(&entry.name, &confirmation)?;
            }
            descriptor_catalog_admin::tombstone(catalog, &scope, &entry.name).await?;
            println!("Tombstoned {}", entry.name);
        }
        CatalogAction::List {
            org,
            kinds,
            states,
            runtime_types,
            prefix,
            cursor,
            limit,
        } => {
            let scope = catalog_scope(stores, org.as_ref()).await?;
            let filter = DescriptorEntryFilter {
                scopes: [scope.clone()].into(),
                kinds: kinds.into_iter().collect(),
                states: states.into_iter().collect(),
                runtime_types: runtime_types.into_iter().collect(),
                name_prefix: prefix,
                ..Default::default()
            };
            let after = cursor.map(|name| CatalogEntryKey {
                scope: scope.clone(),
                name,
            });
            let page =
                descriptor_catalog_admin::list(catalog, filter, after.as_ref(), limit).await?;
            for entry in page.items {
                let keys = descriptor_catalog_admin::member_keys(&entry.members);
                println!(
                    "{}\t{}\t{}\t{}\t{}\t{}\t{}",
                    entry.scope,
                    entry.name,
                    entry.kind,
                    entry.state,
                    entry.descriptor_types().join(","),
                    entry.release_time.to_rfc3339(),
                    keys
                );
            }
            for unreadable in page.unreadable {
                eprintln!("Unreadable catalog entry: {unreadable}");
            }
            if let Some(cursor) = page.resume_after {
                println!("next_cursor: {}", cursor.name);
            }
        }
        CatalogAction::Show { name, org } => {
            let scope = catalog_scope(stores, org.as_ref()).await?;
            let entry = descriptor_catalog_admin::show(catalog, &scope, &name)
                .await?
                .ok_or_else(|| anyhow::anyhow!("no catalog entry {name}"))?;
            println!("{}", serde_json::to_string_pretty(&entry)?);
        }
    }
    Ok(())
}

async fn change_catalog_state(
    catalog: &dyn DescriptorCatalogStore,
    scope: &CatalogScope,
    name: &CatalogName,
    change: CatalogTransition,
) -> anyhow::Result<()> {
    descriptor_catalog_admin::transition(catalog, scope, name, change).await?;
    let done = match change {
        CatalogTransition::Pin => "Pinned",
        CatalogTransition::Unpin => "Unpinned",
        CatalogTransition::Hide => "Hid",
        CatalogTransition::Unhide => "Unhid",
    };
    println!("{done} {name}");
    Ok(())
}

fn confirm_catalog_tombstone(name: &CatalogName, confirmation: &str) -> anyhow::Result<()> {
    anyhow::ensure!(
        confirmation.trim_end_matches(['\r', '\n']) == name.as_str(),
        "confirmation did not match {name}; nothing changed"
    );
    Ok(())
}

/// The Hugging Face client for `catalog create` and `catalog add-member`. It
/// sends no token, and a runtime file makes no call.
fn catalog_hub(config: &Config) -> anyhow::Result<HubClient> {
    let endpoint = reqwest::Url::parse(&config.hf_import_endpoint).map_err(|error| {
        anyhow::anyhow!(
            "hf_import_endpoint `{}`: {error}",
            config.hf_import_endpoint
        )
    })?;
    Ok(HubClient::new(endpoint, config.hf_max_concurrent_requests)?)
}

fn print_catalog_preview(
    preview: &descriptor_catalog_admin::Preview,
    dry_run: bool,
    output: &mut impl Write,
) -> anyhow::Result<()> {
    if dry_run {
        writeln!(output, "Dry run; nothing written.")?;
    }
    writeln!(output, "{}", serde_json::to_string_pretty(&preview.entry)?)?;
    for pin in &preview.pins {
        writeln!(
            output,
            "Pinned {}@{} to {}",
            pin.repo, pin.requested, pin.commit
        )?;
    }
    for name in &preview.synonyms {
        writeln!(
            output,
            "Note: the live entry {name} also holds a member of this entry"
        )?;
    }
    Ok(())
}

/// For a model add-member, the repositories of each existing member and of the
/// new member. The operator claims that they hold one model; no check can.
fn print_model_member_repos(preview: &descriptor_catalog_admin::Preview) {
    let members = &preview.entry.members;
    if members.head().model().is_none() {
        return;
    }
    let last = members.len() - 1;
    for (position, member) in members.iter().enumerate() {
        let label = if position == last {
            "New member"
        } else {
            "Existing member"
        };
        println!(
            "{label} {}: {}",
            descriptor_catalog_admin::member_key(member),
            descriptor_catalog_admin::member_refs(member)
        );
    }
}

async fn run_audit(action: AuditCommand, stores: &Stores) -> anyhow::Result<()> {
    let audit = stores
        .audit
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!(pipette_pipeline::audit::NEEDS_DATABASE))?;
    match action {
        AuditCommand::List {
            org,
            outcome,
            actor,
            target,
            limit,
        } => {
            let org_id = match &org {
                Some(reference) => Some(org::require(&*stores.orgs, reference).await?.org_id),
                None => None,
            };
            let filter = pipette_pipeline::audit::audit_filter(
                org_id,
                outcome.as_deref(),
                actor.as_deref(),
                target.as_deref(),
            )?;
            let limit = std::num::NonZeroUsize::new(limit)
                .ok_or_else(|| anyhow::anyhow!("--limit must be greater than zero"))?;
            let page = audit.list_events(&filter, None, limit).await?;
            warn_unreadable_audit(&page.unreadable);
            for event in &page.items {
                println!("{}", serde_json::to_string(event)?);
            }
            Ok(())
        }
        AuditCommand::Prune {
            older_than_days,
            dry_run,
        } => {
            let cutoff = prune_cutoff(older_than_days)?;
            if dry_run {
                let count = audit.count_before(cutoff).await?;
                println!("Would delete {count} audit event(s) from before {cutoff}.");
                return Ok(());
            }
            let pruned = audit.prune_before(cutoff).await?;
            tracing::info!(pruned, %cutoff, "audit log pruned");
            println!("Deleted {pruned} audit event(s) from before {cutoff}.");
            Ok(())
        }
    }
}

/// The time before which `audit prune --older-than-days` deletes an event.
fn prune_cutoff(older_than_days: u32) -> anyhow::Result<chrono::DateTime<chrono::Utc>> {
    chrono::TimeDelta::try_days(i64::from(older_than_days))
        .and_then(|age| chrono::Utc::now().checked_sub_signed(age))
        .ok_or_else(|| anyhow::anyhow!("--older-than-days {older_than_days} is out of range"))
}

fn warn_unreadable_audit(unreadable: &[pipette_store::UnreadableObjectError]) {
    unreadable.iter().for_each(|bad| {
        tracing::warn!(key = %bad.key, reason = %bad.reason, "skipping an unreadable audit event");
    });
}

/// Refuses a client permission by name, rather than as unknown.
fn parse_user_permission(s: &str) -> Result<UserPermission, String> {
    if let Ok(permission) = UserPermission::from_str(s) {
        return Ok(permission);
    }
    let user_names = UserPermission::VARIANTS.join(", ");
    if ClientPermission::from_str(s).is_ok() {
        return Err(format!(
            "{s:?} is a client permission; a user cannot hold it. One of: {user_names}"
        ));
    }
    Err(format!("unknown permission {s:?}; one of: {user_names}"))
}

/// The account a command names, refusing one that does not exist.
///
/// Every write below goes through this first, so a mistyped id is refused
/// rather than seeding a permission object or a pre-auth key for a principal
/// that was never created — the same guard `require_client` gives clients.
async fn require_user(
    user_store: &Arc<dyn UserStore>,
    user_id: &UserId,
) -> anyhow::Result<pipette_core::user::User> {
    user_store
        .get_user(user_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("user {user_id} not found"))
}

/// Set an organization's status, printing the transition. `disable` and
/// `enable` are the same write over the two values, so they share the body —
/// two verbs because they name two decisions, one implementation because the
/// record does not care.
async fn set_org_status(
    org_store: &Arc<dyn OrgStore>,
    reference: &OrgRef,
    status: OrgStatus,
) -> anyhow::Result<()> {
    let org = org::require(&**org_store, reference).await?;
    if org.status == status {
        println!("Organization {} is already {status}.", org.slug);
        return Ok(());
    }
    let was = org.status;
    let slug = org.slug.clone();
    org_store.put_org(&Organization { status, ..org }).await?;
    tracing::info!(slug = %slug, %status, "organization status set");
    println!("Organization {slug}: {was} -> {status}.");
    Ok(())
}

/// One line per organization, the shape `orgs list` and `orgs show` share.
fn print_org_line(org: &Organization, members: usize, clients: usize) {
    println!(
        " {:<44} | {:<8} | {:>3} member(s) | {:>4} device(s) | {} ({})",
        org.slug.as_str(),
        org.status.as_ref(),
        members,
        clients,
        org.display_name.as_str(),
        org.org_id,
    );
}

/// [`campaign_admin::owner`] decides; this adds only the CLI's words for naming
/// neither `--user` nor `--org`, flags the route does not have.
async fn campaign_owner(
    stores: &Stores,
    user: Option<UserId>,
    org: Option<OrgRef>,
) -> anyhow::Result<CampaignOwner> {
    campaign_admin::owner(stores, user, org)
        .await
        .map_err(|e| match e {
            campaign_admin::CampaignAdminError::NoOwnerNamed => {
                anyhow::anyhow!("name an owner: --user <USER_ID> or --org <ORG>")
            }
            other @ (campaign_admin::CampaignAdminError::NotFound(_)
            | campaign_admin::CampaignAdminError::NoSuchUser(_)
            | campaign_admin::CampaignAdminError::NoSuchOrg(_)
            | campaign_admin::CampaignAdminError::NoSuchPlan(_)
            | campaign_admin::CampaignAdminError::NotAdvanced { .. }
            | campaign_admin::CampaignAdminError::NotCreated { .. }
            | campaign_admin::CampaignAdminError::NoPlan(_)
            | campaign_admin::CampaignAdminError::Archived(_)
            | campaign_admin::CampaignAdminError::NotOnPlan { .. }
            | campaign_admin::CampaignAdminError::NoDocument(_)
            | campaign_admin::CampaignAdminError::SourceMismatch(_)
            | campaign_admin::CampaignAdminError::NotInCampaign { .. }
            | campaign_admin::CampaignAdminError::NoSuchSourceVariant { .. }
            | campaign_admin::CampaignAdminError::Edit(_)
            | campaign_admin::CampaignAdminError::NothingToRerun
            | campaign_admin::CampaignAdminError::NotInPlan { .. }
            | campaign_admin::CampaignAdminError::Unanswered(_)
            | campaign_admin::CampaignAdminError::NoneAnswered { .. }
            | campaign_admin::CampaignAdminError::NotRescorable(_)
            | campaign_admin::CampaignAdminError::NoneRescorable(_)
            | campaign_admin::CampaignAdminError::Overtaken { .. }
            | campaign_admin::CampaignAdminError::NotCancelled { .. }
            | campaign_admin::CampaignAdminError::StillRunning { .. }
            | campaign_admin::CampaignAdminError::DeletedDuringFiling { .. }
            | campaign_admin::CampaignAdminError::Store(_)) => anyhow::Error::new(other),
        })
}

fn print_campaign_line(campaign: &Campaign) {
    println!(
        "  {:<48} | {:<44} | {:<48} | {}",
        campaign.campaign_id.as_str(),
        campaign.owner.label(),
        campaign
            .current_plan_id
            .as_ref()
            .map_or("-", PlanId::as_str),
        campaign.name.as_str(),
    );
}

async fn ingest_into_campaign(
    stores: &Stores,
    settings: IngestSettings<'_>,
    campaign_id: CampaignId,
    input: plan_cli::PlanInput<'_>,
    note: Option<String>,
) -> anyhow::Result<()> {
    let (report, advanced) =
        campaign_admin::ingest_into(stores, settings, &campaign_id, input, note)
            .await
            .map_err(|e| match e {
                // Name the staged plan: a second ingest mints another and
                // leaves this one claimable.
                campaign_admin::CampaignAdminError::NotAdvanced {
                    ref plan_id,
                    ref source,
                    ..
                } => anyhow::anyhow!(
                    "{e}; stop it with `plans cancel {plan_id}` before filing again: {source:#}"
                ),
                other @ (campaign_admin::CampaignAdminError::NotFound(_)
                | campaign_admin::CampaignAdminError::NoSuchUser(_)
                | campaign_admin::CampaignAdminError::NoSuchOrg(_)
                | campaign_admin::CampaignAdminError::NoSuchPlan(_)
                | campaign_admin::CampaignAdminError::NoOwnerNamed
                | campaign_admin::CampaignAdminError::NotCreated { .. }
                | campaign_admin::CampaignAdminError::NoPlan(_)
                | campaign_admin::CampaignAdminError::Archived(_)
                | campaign_admin::CampaignAdminError::NotOnPlan { .. }
                | campaign_admin::CampaignAdminError::NoDocument(_)
                | campaign_admin::CampaignAdminError::SourceMismatch(_)
                | campaign_admin::CampaignAdminError::NotInCampaign { .. }
                | campaign_admin::CampaignAdminError::NoSuchSourceVariant { .. }
                | campaign_admin::CampaignAdminError::Edit(_)
                | campaign_admin::CampaignAdminError::NothingToRerun
                | campaign_admin::CampaignAdminError::NotInPlan { .. }
                | campaign_admin::CampaignAdminError::Unanswered(_)
                | campaign_admin::CampaignAdminError::NoneAnswered { .. }
                | campaign_admin::CampaignAdminError::NotRescorable(_)
                | campaign_admin::CampaignAdminError::NoneRescorable(_)
                | campaign_admin::CampaignAdminError::Overtaken { .. }
                | campaign_admin::CampaignAdminError::NotCancelled { .. }
                | campaign_admin::CampaignAdminError::StillRunning { .. }
                | campaign_admin::CampaignAdminError::DeletedDuringFiling { .. }
                | campaign_admin::CampaignAdminError::Store(_)) => anyhow::Error::new(other),
            })?;

    println!("Campaign {campaign_id} is now running {}.", report.plan_id);
    if let Some(superseded) = advanced.superseded {
        println!("It supersedes {superseded}, which is untouched: stop it with `plans cancel`.");
    }
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}

async fn edit_campaign_variants(
    stores: &Stores,
    settings: IngestSettings<'_>,
    campaign_id: CampaignId,
    base: PlanId,
    edit: VariantEdit,
    note: Option<String>,
    offer: campaign_admin::Offer,
) -> anyhow::Result<()> {
    let (report, revised) =
        campaign_admin::edit_variants(stores, settings, &campaign_id, &base, &edit, note, offer)
            .await
            .map_err(|e| revision_error(e, "list the variants and edit again"))?;
    print_revision(&campaign_id, &report, &revised)
}

async fn revise_campaign_document(
    stores: &Stores,
    settings: IngestSettings<'_>,
    campaign_id: CampaignId,
    base: PlanId,
    document: String,
    note: Option<String>,
    offer: campaign_admin::Offer,
) -> anyhow::Result<()> {
    let Some((report, revised)) = campaign_admin::revise_document(
        stores,
        settings,
        &campaign_id,
        &base,
        document,
        note,
        offer,
    )
    .await
    .map_err(|e| revision_error(e, "read the document and revise it again"))?
    else {
        println!(
            "Campaign {campaign_id} already runs this document, as plan {base}. The command filed nothing."
        );
        return Ok(());
    };
    print_revision(&campaign_id, &report, &revised)
}

async fn rerun_campaign_jobs(
    stores: &Stores,
    settings: IngestSettings<'_>,
    campaign_id: CampaignId,
    jobs: std::collections::BTreeSet<JobId>,
    note: Option<String>,
) -> anyhow::Result<()> {
    let (report, revised) = campaign_admin::rerun_jobs(stores, settings, &campaign_id, jobs, note)
        .await
        .map_err(|e| revision_error(e, "re-run the jobs again"))?;
    print_revision(&campaign_id, &report, &revised)
}

async fn revert_campaign(
    stores: &Stores,
    settings: IngestSettings<'_>,
    campaign_id: CampaignId,
    base: PlanId,
    to: PlanId,
    note: Option<String>,
) -> anyhow::Result<()> {
    let Some((report, revised)) =
        campaign_admin::revert_to(stores, settings, &campaign_id, &base, &to, note)
            .await
            .map_err(|e| revision_error(e, "read what the campaign runs and revert again"))?
    else {
        println!(
            "Campaign {campaign_id} already runs the document of {to}, as plan {base}. The command filed nothing."
        );
        return Ok(());
    };
    print_revision(&campaign_id, &report, &revised)
}

/// A revision's error, with the operator's next step; `retry` says how to run
/// the command again.
fn revision_error(e: campaign_admin::CampaignAdminError, retry: &str) -> anyhow::Error {
    match e {
        // The revision is staged and the campaign never took it up.
        campaign_admin::CampaignAdminError::NotAdvanced {
            ref plan_id,
            ref source,
            ..
        } => {
            anyhow::anyhow!("{e}; stop it with `plans cancel {plan_id}`, then {retry}: {source:#}")
        }
        campaign_admin::CampaignAdminError::Overtaken {
            cancelled: true, ..
        } => anyhow::anyhow!("{e}; it is cancelled, so {retry}"),
        campaign_admin::CampaignAdminError::Overtaken {
            cancelled: false,
            ref plan_id,
            ..
        } => anyhow::anyhow!("{e}; stop it with `plans cancel {plan_id}`"),
        campaign_admin::CampaignAdminError::NotCancelled {
            ref superseded,
            ref source,
            ..
        } => anyhow::anyhow!("{e}; stop it with `plans cancel {superseded}`: {source:#}"),
        other @ (campaign_admin::CampaignAdminError::NotFound(_)
        | campaign_admin::CampaignAdminError::NoSuchUser(_)
        | campaign_admin::CampaignAdminError::NoSuchOrg(_)
        | campaign_admin::CampaignAdminError::NoSuchPlan(_)
        | campaign_admin::CampaignAdminError::NoOwnerNamed
        | campaign_admin::CampaignAdminError::NotCreated { .. }
        | campaign_admin::CampaignAdminError::NoPlan(_)
        | campaign_admin::CampaignAdminError::Archived(_)
        | campaign_admin::CampaignAdminError::NotOnPlan { .. }
        | campaign_admin::CampaignAdminError::NoDocument(_)
        | campaign_admin::CampaignAdminError::SourceMismatch(_)
        | campaign_admin::CampaignAdminError::NotInCampaign { .. }
        | campaign_admin::CampaignAdminError::NoSuchSourceVariant { .. }
        | campaign_admin::CampaignAdminError::Edit(_)
        | campaign_admin::CampaignAdminError::NothingToRerun
        | campaign_admin::CampaignAdminError::NotInPlan { .. }
        | campaign_admin::CampaignAdminError::Unanswered(_)
        | campaign_admin::CampaignAdminError::NoneAnswered { .. }
        | campaign_admin::CampaignAdminError::NotRescorable(_)
        | campaign_admin::CampaignAdminError::NoneRescorable(_)
        | campaign_admin::CampaignAdminError::StillRunning { .. }
        | campaign_admin::CampaignAdminError::DeletedDuringFiling { .. }
        | campaign_admin::CampaignAdminError::Store(_)) => anyhow::Error::new(other),
    }
}

fn print_revision(
    campaign_id: &CampaignId,
    report: &IngestReport,
    revised: &pipette_pipeline::campaign_admin::Revised,
) -> anyhow::Result<()> {
    println!("Campaign {campaign_id} is now running {}.", report.plan_id);
    let rerun = match revised.rerun_job_count {
        0 => String::new(),
        n => format!(", {n} re-run as new jobs"),
    };
    println!(
        "It replaces {}: {} job(s) carried over{rerun}, {} dropped.",
        revised.superseded, revised.carried_job_count, revised.dropped_job_count
    );
    if revised.cancel_requested {
        println!(
            "{} is cancelled; the next `queue-maintenance` run stops the jobs the revision dropped.",
            revised.superseded
        );
    }
    println!("{}", serde_json::to_string_pretty(report)?);
    Ok(())
}

async fn create_campaign(
    stores: &Stores,
    settings: IngestSettings<'_>,
    new: NewCampaign,
    plan: &Path,
    note: Option<String>,
) -> anyhow::Result<()> {
    let (campaign, report) = campaign_admin::create(stores, settings, new, plan, note)
        .await
        .map_err(|e| match e {
            // Only an uncancelled plan needs the operator: it stays claimable under
            // a campaign id nothing holds, and a second run files a second plan.
            campaign_admin::CampaignAdminError::NotCreated {
                cancelled: true,
                ref source,
                ..
            } => anyhow::anyhow!("{e}; the plan is cancelled, so create it again: {source:#}"),
            campaign_admin::CampaignAdminError::NotCreated {
                cancelled: false,
                ref plan_id,
                ref source,
                ..
            } => anyhow::anyhow!(
                "{e}; stop it with `plans cancel {plan_id}` before creating it again: {source:#}"
            ),
            other @ (campaign_admin::CampaignAdminError::NotFound(_)
            | campaign_admin::CampaignAdminError::NoSuchUser(_)
            | campaign_admin::CampaignAdminError::NoSuchOrg(_)
            | campaign_admin::CampaignAdminError::NoSuchPlan(_)
            | campaign_admin::CampaignAdminError::NoOwnerNamed
            | campaign_admin::CampaignAdminError::NotAdvanced { .. }
            | campaign_admin::CampaignAdminError::NoPlan(_)
            | campaign_admin::CampaignAdminError::Archived(_)
            | campaign_admin::CampaignAdminError::NotOnPlan { .. }
            | campaign_admin::CampaignAdminError::NoDocument(_)
            | campaign_admin::CampaignAdminError::SourceMismatch(_)
            | campaign_admin::CampaignAdminError::NotInCampaign { .. }
            | campaign_admin::CampaignAdminError::NoSuchSourceVariant { .. }
            | campaign_admin::CampaignAdminError::Edit(_)
            | campaign_admin::CampaignAdminError::NothingToRerun
            | campaign_admin::CampaignAdminError::NotInPlan { .. }
            | campaign_admin::CampaignAdminError::Unanswered(_)
            | campaign_admin::CampaignAdminError::NoneAnswered { .. }
            | campaign_admin::CampaignAdminError::NotRescorable(_)
            | campaign_admin::CampaignAdminError::NoneRescorable(_)
            | campaign_admin::CampaignAdminError::Overtaken { .. }
            | campaign_admin::CampaignAdminError::NotCancelled { .. }
            | campaign_admin::CampaignAdminError::StillRunning { .. }
            | campaign_admin::CampaignAdminError::DeletedDuringFiling { .. }
            | campaign_admin::CampaignAdminError::Store(_)) => anyhow::Error::new(other),
        })?;

    println!("Created campaign {}", campaign.campaign_id);
    println!("Name         {}", campaign.name);
    println!("Owner        {}", campaign.owner.label());
    println!("Plan         {}", report.plan_id);
    println!(
        "File revisions into it with `campaigns ingest {}`.",
        campaign.campaign_id
    );
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}

/// `settings.models` is `ModelCheck::none()`, and `settings.import` `None`, for
/// actions that file no new document or jobs.
async fn handle_campaign_action(
    action: CampaignAction,
    stores: &Stores,
    settings: IngestSettings<'_>,
) -> anyhow::Result<()> {
    let campaigns = stores.campaigns.clone();
    match action {
        CampaignAction::Create {
            name,
            user,
            org,
            plan,
            note,
            strict_models: _,
            dry_run,
        } => {
            let name =
                NonEmptyTrimmedString::try_new(name).map_err(|e| anyhow::anyhow!("name: {e}"))?;
            // Before the dry run too, so it refuses an owner the create would.
            let owner = campaign_owner(stores, user, org).await?;
            if dry_run {
                let report =
                    campaign_admin::dry_run_document(stores, settings, &owner, &plan, note).await?;
                println!("{}", serde_json::to_string_pretty(&report)?);
                return Ok(());
            }
            create_campaign(stores, settings, NewCampaign::new(name, owner), &plan, note).await
        }
        CampaignAction::Ingest {
            campaign_id,
            plan,
            note,
            strict_models: _,
            dry_run,
        } => {
            if dry_run {
                let report =
                    campaign_admin::dry_run_into(stores, settings, &campaign_id, &plan, note)
                        .await?;
                println!("{}", serde_json::to_string_pretty(&report)?);
                return Ok(());
            }
            let input = plan_cli::PlanInput::Document(&plan);
            ingest_into_campaign(stores, settings, campaign_id, input, note).await
        }
        CampaignAction::Variants {
            campaign_id,
            add,
            remove,
            base,
            note,
            strict_models: _,
            dry_run,
            carry_earlier,
            inherit_from,
            add_from,
            restore,
        } => {
            if add.is_none() && add_from.is_empty() && restore.is_empty() && remove.is_empty() {
                let (plan_id, variants) =
                    campaign_admin::current_variants(stores, &campaign_id).await?;
                println!("# Campaign {campaign_id} runs {plan_id}.\n");
                println!("{variants}");
                return Ok(());
            }
            let mut edit = match &add {
                Some(path) => VariantEdit::parse(
                    &std::fs::read_to_string(path)
                        .map_err(|e| anyhow::anyhow!("reading {}: {e}", path.display()))?,
                )?,
                None => VariantEdit::default(),
            };
            edit.remove.extend(remove);
            // Clap's `edit` group requires it; restated rather than assumed.
            let base = base.ok_or_else(|| anyhow::anyhow!("an edit names its --base plan"))?;
            let offer = campaign_admin::Offer::read(
                &*stores.plans,
                &*stores.campaigns,
                carry_earlier,
                &campaign_admin::inheriting(&inherit_from, &add_from, &restore),
            )
            .await?;
            let added = offer.variants_of(&*stores.plans, &add_from).await?;
            let restored = offer.restored_variants(&*stores.plans, &restore).await?;
            let edit = edit.adding(added).restoring(restored, restore);
            if dry_run {
                let report = campaign_admin::dry_run_variants(
                    stores,
                    settings,
                    &campaign_id,
                    &base,
                    &edit,
                    note,
                    offer,
                )
                .await?;
                println!("{}", serde_json::to_string_pretty(&report)?);
                return Ok(());
            }
            edit_campaign_variants(stores, settings, campaign_id, base, edit, note, offer).await
        }
        CampaignAction::Revise {
            campaign_id,
            plan,
            base,
            note,
            strict_models: _,
            dry_run,
            carry_earlier,
            inherit_from,
        } => {
            let Some(plan) = plan else {
                let (plan_id, document) =
                    campaign_admin::head_document(stores, &campaign_id).await?;
                // The id on stderr, so the document redirects to a file as is.
                eprintln!(
                    "# Campaign {campaign_id} runs {plan_id}; revise it with --base {plan_id}."
                );
                print!("{document}");
                return Ok(());
            };
            let document = std::fs::read_to_string(&plan)
                .map_err(|e| anyhow::anyhow!("reading {}: {e}", plan.display()))?;
            // Clap requires it with `--plan`; restated rather than assumed.
            let base = base.ok_or_else(|| anyhow::anyhow!("a revision names its --base plan"))?;
            let offer = campaign_admin::Offer::read(
                &*stores.plans,
                &*stores.campaigns,
                carry_earlier,
                &inherit_from,
            )
            .await?;
            if dry_run {
                let report = campaign_admin::dry_run_revise_document(
                    stores,
                    settings,
                    &campaign_id,
                    &base,
                    document,
                    note,
                    offer,
                )
                .await?;
                println!("{}", serde_json::to_string_pretty(&report)?);
                return Ok(());
            }
            revise_campaign_document(stores, settings, campaign_id, base, document, note, offer)
                .await
        }
        CampaignAction::Rerun {
            campaign_id,
            jobs,
            note,
            strict_models: _,
            dry_run,
        } => {
            if dry_run {
                let report = campaign_admin::dry_run_rerun(
                    stores,
                    settings,
                    &campaign_id,
                    jobs.into_iter().collect(),
                    note,
                )
                .await?;
                println!("{}", serde_json::to_string_pretty(&report)?);
                return Ok(());
            }
            rerun_campaign_jobs(
                stores,
                settings,
                campaign_id,
                jobs.into_iter().collect(),
                note,
            )
            .await
        }
        CampaignAction::Revert {
            campaign_id,
            to,
            base,
            note,
            strict_models: _,
            dry_run,
        } => {
            if dry_run {
                let report = campaign_admin::dry_run_revert(
                    stores,
                    settings,
                    &campaign_id,
                    &base,
                    &to,
                    note,
                )
                .await?;
                println!("{}", serde_json::to_string_pretty(&report)?);
                return Ok(());
            }
            revert_campaign(stores, settings, campaign_id, base, to, note).await
        }
        CampaignAction::List { user, org } => {
            // An unknown owner errors, rather than listing nothing.
            let filter = match (user, org) {
                (None, None) => None,
                (user, org) => Some(campaign_owner(stores, user, org).await?),
            };
            let listing = drain_campaigns(&*campaigns).await?;
            listing.unreadable.iter().for_each(|entry| {
                tracing::warn!(%entry, "skipping malformed campaign record");
            });
            let mut rows: Vec<Campaign> = listing
                .items
                .into_iter()
                .filter(|campaign| filter.as_ref().is_none_or(|want| campaign.owner == *want))
                .collect();
            if rows.is_empty() {
                println!("No campaigns.");
                return Ok(());
            }
            // Newest first, as `plans list`; the store returns creation order.
            rows.reverse();
            println!("{} campaign(s):", rows.len());
            rows.iter().for_each(print_campaign_line);
            Ok(())
        }
        CampaignAction::Show { campaign_id } => {
            let campaign = campaign_admin::require(&*stores.campaigns, &campaign_id).await?;
            println!("Campaign     {}", campaign.campaign_id);
            println!("Name         {}", campaign.name);
            println!("Owner        {}", campaign.owner.label());
            println!(
                "Plan         {}",
                campaign
                    .current_plan_id
                    .as_ref()
                    .map_or("-", PlanId::as_str)
            );
            println!("Created      {}", campaign.created_at);
            println!("Updated      {}", campaign.updated_at);

            // One read per revision (up to `campaign_chain::CHAIN_MAX`), which
            // is why this is `show` rather than a column on `list`.
            let chain = campaign_chain::walk(stores.plans.as_ref(), &campaign).await?;
            let superseded = chain.superseded();
            if !superseded.is_empty() {
                println!("Superseded   {}", superseded.len());
                superseded
                    .iter()
                    .for_each(|plan_id| println!("  {plan_id}"));
                if chain.truncated {
                    println!("  (older revisions than these are not walked)");
                }
            }
            Ok(())
        }
        CampaignAction::Rename { campaign_id, name } => {
            let name =
                NonEmptyTrimmedString::try_new(name).map_err(|e| anyhow::anyhow!("--name: {e}"))?;
            let mut campaign = campaign_admin::require(&*stores.campaigns, &campaign_id).await?;
            let before = campaign.name.clone();
            if !campaign.rename(name.clone(), chrono::Utc::now()) {
                println!("Campaign {campaign_id}: no change.");
                return Ok(());
            }
            campaigns.put_campaign(&campaign).await?;
            tracing::info!(%campaign_id, "campaign renamed");
            // `as_str`: `{:?}` on the newtype prints `NonEmptyTrimmedString("…")`.
            println!(
                "Campaign {campaign_id}: {:?} -> {:?}.",
                before.as_str(),
                name.as_str()
            );
            Ok(())
        }
        CampaignAction::Owner {
            campaign_id,
            user,
            org,
        } => {
            let owner = campaign_owner(stores, user, org).await?;
            let mut campaign = campaign_admin::require(&*stores.campaigns, &campaign_id).await?;
            let before = campaign.owner.label();
            if !campaign.set_owner(owner, chrono::Utc::now()) {
                println!("Campaign {campaign_id}: no change.");
                return Ok(());
            }
            campaigns.put_campaign(&campaign).await?;
            tracing::info!(%campaign_id, owner = %campaign.owner.label(), "campaign owner set");
            println!(
                "Campaign {campaign_id}: {before} -> {}.",
                campaign.owner.label()
            );
            Ok(())
        }
        CampaignAction::Delete { campaign_id } => {
            // Read first: the store's delete is idempotent, so a missing id
            // would pass silently.
            let campaign = campaign_admin::require(&*stores.campaigns, &campaign_id).await?;
            campaign_admin::delete(&*campaigns, &*stores.plans, &campaign).await?;
            println!(
                "Deleted campaign {campaign_id} ({}). Its plans are kept.",
                campaign.name
            );
            Ok(())
        }
    }
}

/// `campaign_owner`'s rule for vault owners.
async fn vault_owner(
    stores: &Stores,
    user: Option<UserId>,
    org: Option<OrgRef>,
) -> anyhow::Result<VaultOwner> {
    vault_admin::owner(stores, user, org)
        .await
        .map_err(|e| match e {
            vault_admin::VaultAdminError::NoOwnerNamed => {
                anyhow::anyhow!("name an owner: --user <USER_ID> or --org <ORG>")
            }
            other @ (vault_admin::VaultAdminError::NotFound(_)
            | vault_admin::VaultAdminError::NoSuchUser(_)
            | vault_admin::VaultAdminError::NoSuchOrg(_)
            | vault_admin::VaultAdminError::DigestMismatch { .. }
            | vault_admin::VaultAdminError::NoBlob { .. }
            | vault_admin::VaultAdminError::NotA { .. }
            | vault_admin::VaultAdminError::TooLarge { .. }
            | vault_admin::VaultAdminError::Store(_)) => anyhow::Error::new(other),
        })
}

fn print_vault_line(resource: &VaultResource) {
    println!(
        "  {:<48} | {:<48} | {:>12} | {}",
        resource.resource_id.as_str(),
        resource.owner.label(),
        resource.bytes,
        resource.name.as_str(),
    );
}

/// `stored` is the size the store reports for the blob.
fn vault_show_lines(resource: &VaultResource, stored: Option<u64>) -> anyhow::Result<Vec<String>> {
    let mut lines = vec![
        format!("Resource     {}", resource.resource_id),
        format!("Name         {}", resource.name),
        format!("Owner        {}", resource.owner.label()),
        format!("Object       {}", resource.object_key),
        format!("Bytes        {}", resource.bytes),
        format!("Sha256       {}", resource.sha256),
        format!("Created      {}", resource.created_at),
    ];
    if let Some(artifact) = &resource.artifact {
        lines.push(format!("Variant      {}", artifact.type_name()));
    }
    if let Some((role, file)) = resource.own_file() {
        lines.push(format!("File         {}, its {role}", file.filename));
    }
    lines.push(match &resource.origin {
        Some(origin) => format!("Origin       {origin}"),
        None => "Origin       none: a user uploaded it".to_string(),
    });
    lines.push(match stored {
        Some(bytes) if bytes == resource.bytes => "Object state present".to_string(),
        Some(bytes) => format!(
            "Object state present, but {bytes} bytes where the record says {}. \
             Remove the object from the store, then upload the file again: \
             an upload keeps an object already there.",
            resource.bytes
        ),
        None => "Object state MISSING - clients get 404".to_string(),
    });
    if let Some(entry) = resource.entry() {
        lines.push("Plans name it as:".to_string());
        lines.push(format!("  {}", entry.to_toml()?));
    }
    Ok(lines)
}

async fn handle_vault_action(action: VaultAction, stores: Stores) -> anyhow::Result<()> {
    let vault = stores.vault.clone();
    match action {
        VaultAction::List { user, org } => {
            // An unknown owner errors, rather than listing nothing.
            let filter = match (user, org) {
                (None, None) => None,
                (user, org) => Some(vault_owner(&stores, user, org).await?),
            };
            let listing = drain_vault_resources(&*vault).await?;
            listing.unreadable.iter().for_each(|entry| {
                tracing::warn!(%entry, "skipping malformed vault record");
            });
            let mut rows: Vec<VaultResource> = listing
                .items
                .into_iter()
                .filter(|resource| filter.as_ref().is_none_or(|want| resource.owner == *want))
                .collect();
            if rows.is_empty() {
                println!("No vault resources.");
                return Ok(());
            }
            // Newest first; the store returns creation order.
            rows.reverse();
            println!("{} vault resource(s):", rows.len());
            rows.iter().for_each(print_vault_line);
            Ok(())
        }
        VaultAction::Show { resource_id } => {
            let resource = vault_admin::require(&*vault, &resource_id).await?;
            // Bytes can vanish from the store unseen; otherwise a client's 404
            // is the first sign.
            let stored = vault.stat_blob(&resource.object_key).await?;
            vault_show_lines(&resource, stored)?
                .iter()
                .for_each(|line| println!("{line}"));
            Ok(())
        }
        VaultAction::Delete { resource_id } => {
            // Read first, as for `campaigns delete`.
            let resource = vault_admin::require(&*vault, &resource_id).await?;
            let deleted = vault_admin::delete(&*vault, &resource).await?;
            deleted.iter().for_each(|id| {
                tracing::info!(resource_id = %id, "vault resource deleted");
                println!("Deleted vault resource {id}.");
            });
            println!(
                "Upload {:?} is gone, with each file no other resource names.",
                resource.name.as_str()
            );
            Ok(())
        }
        VaultAction::Prune {
            older_than,
            dry_run,
        } => {
            let pruned =
                vault_admin::prune(&*vault, older_than, chrono::Utc::now(), dry_run).await?;
            let verb = if dry_run {
                "[dry-run] would delete"
            } else {
                "deleted"
            };
            pruned.orphans.iter().for_each(|blob| {
                println!(
                    "{verb} {} ({} bytes, written {})",
                    blob.key,
                    blob.bytes,
                    blob.modified.to_rfc3339()
                );
            });
            let bytes: u64 = pruned.orphans.iter().map(|blob| blob.bytes).sum();
            if !dry_run {
                tracing::info!(
                    deleted = pruned.orphans.len(),
                    bytes,
                    kept_recent = pruned.recent,
                    older_than_secs = older_than.as_secs(),
                    "vault files pruned"
                );
            }
            println!(
                "{verb} {} vault file(s) no resource names, {bytes} bytes; {} more were \
                 written too recently",
                pruned.orphans.len(),
                pruned.recent
            );
            Ok(())
        }
    }
}

async fn handle_org_action(action: OrgAction, stores: Stores) -> anyhow::Result<()> {
    let org_store = stores.orgs.clone();
    match action {
        OrgAction::Create {
            slug,
            name,
            adopt_stranded_claim,
        } => {
            let display_name = match name {
                Some(name) => NonEmptyTrimmedString::try_new(name)
                    .map_err(|e| anyhow::anyhow!("--name: {e}"))?,
                None => NonEmptyTrimmedString::try_new(slug.as_str())?,
            };
            let org = Organization::new(slug, display_name, chrono::Utc::now());

            // Claim first: the exclusive create makes the handle unique, and a
            // record written first would be one the race's loser must clean up.
            let org = match org_store
                .claim_org_slug(org.slug.as_str(), &org.org_id)
                .await?
            {
                SlugClaim::Claimed => org,
                SlugClaim::Taken(holder) => {
                    let stranded = org_store.get_org(&holder).await?.is_none();
                    match (stranded, adopt_stranded_claim) {
                        // A create that failed between its two writes: build
                        // the record under the id the claim already names.
                        (true, true) => {
                            println!("Adopting the stranded claim on {}.", org.slug);
                            Organization {
                                org_id: holder,
                                ..org
                            }
                        }
                        // Refused without the flag: a create still in flight
                        // looks the same, and adopting it steals its handle.
                        (true, false) => anyhow::bail!(
                            "{} is claimed by {holder}, which has no organization record. Either \
                             an earlier create failed part-way, or one is in flight now. Re-run \
                             with --adopt-stranded-claim to take the claim over.",
                            org.slug
                        ),
                        (false, _) => anyhow::bail!(
                            "{} is already held by {holder}; a handle names one organization",
                            org.slug
                        ),
                    }
                }
            };

            if let Err(write) = org_store.put_org(&org).await {
                // Release only while no record answers to this id: a concurrent
                // adopter may have written one, and its handle must stay held.
                let holds_record = match org_store.get_org(&org.org_id).await {
                    Ok(record) => record.is_some(),
                    Err(probe) => {
                        tracing::warn!(
                            org_id = %org.org_id,
                            slug = %org.slug,
                            error = %format_args!("{probe:#}"),
                            "failed to check for the organization record; releasing the handle \
                             claim anyway"
                        );
                        false
                    }
                };
                if !holds_record
                    && let Err(release) = org_store.release_org_slug(org.slug.as_str()).await
                {
                    tracing::warn!(
                        org_id = %org.org_id,
                        slug = %org.slug,
                        error = %release,
                        "failed to release the handle claim after the record write failed; \
                         `orgs release-claim <slug>` will clear it"
                    );
                }
                return Err(write);
            }
            tracing::info!(org_id = %org.org_id, slug = %org.slug, "organization created");
            println!(
                "Created organization {} ({}) as {}.",
                org.slug, org.display_name, org.org_id
            );
            Ok(())
        }
        OrgAction::List => {
            let orgs = org_store.list_orgs().await?;
            if orgs.is_empty() {
                println!("No organizations.");
                return Ok(());
            }
            let memberships = org_store.list_memberships().await?;
            let ownerships = org_store.list_ownerships().await?;

            let counts = org::tally(&orgs, &memberships, &ownerships);
            println!("{} organization(s):", orgs.len());
            orgs.iter().for_each(|org| {
                let tallied = counts.get(&org.org_id).copied().unwrap_or_default();
                print_org_line(org, tallied.members, tallied.clients);
            });
            Ok(())
        }
        OrgAction::Show { org } => {
            let org = org::require(&*org_store, &org).await?;
            let members = org::members(&*org_store, &*stores.users, &org.org_id).await?;
            let clients = org::owned_clients(&*stores.clients, &*org_store, &org.org_id).await?;

            println!("Organization {}", org.org_id);
            println!("Handle       {}", org.slug);
            println!("Name         {}", org.display_name);
            println!("Status       {}", org.status);
            println!("Created      {}", org.created_at);
            println!("Members      {}", members.len());
            members.iter().for_each(|member| {
                println!(
                    "  {:<44} | {:<8} | {}",
                    member.user_id.as_str(),
                    render_user_status(&member.status),
                    member.contact_email.as_str()
                );
            });
            println!("Devices      {}", clients.len());
            clients.iter().for_each(|(client, owner)| {
                println!(
                    "  {:<44} | {:<24} | {}",
                    client.client_id.as_str(),
                    owner.label(),
                    client.client_details.as_str()
                );
            });
            Ok(())
        }
        OrgAction::Update { org, name } => {
            let display_name =
                NonEmptyTrimmedString::try_new(name).map_err(|e| anyhow::anyhow!("--name: {e}"))?;
            let org = org::require(&*org_store, &org).await?;
            if org.display_name == display_name {
                println!("Organization {}: no change.", org.slug);
                return Ok(());
            }
            let before = org.display_name.clone();
            let slug = org.slug.clone();
            org_store
                .put_org(&Organization {
                    display_name: display_name.clone(),
                    ..org
                })
                .await?;
            tracing::info!(slug = %slug, "organization renamed");
            println!(
                "Organization {slug}: {:?} -> {:?}.",
                before.as_str(),
                display_name.as_str()
            );
            Ok(())
        }
        OrgAction::Disable { org } => set_org_status(&org_store, &org, OrgStatus::Disabled).await,
        OrgAction::Enable { org } => set_org_status(&org_store, &org, OrgStatus::Active).await,
        OrgAction::Delete { org, detach } => {
            let org = org::require(&*org_store, &org).await?;
            let dependents = || async {
                let mut found = org::dependents(
                    &*org_store,
                    &*stores.catalog,
                    &*stores.clients,
                    &*stores.campaigns,
                    &*stores.vault,
                    &org.org_id,
                )
                .await?;
                if let Some(catalog) = stores.descriptor_catalog.as_deref() {
                    found.catalog_entries = catalog.live_entries_in_org(&org.org_id).await?;
                }
                anyhow::Ok(found)
            };
            if detach {
                // Refuse what `--detach` cannot clear before detaching anything.
                let found = dependents().await?;
                if !found.only_detachable() {
                    return Err(org_delete_refusal(&org, &found));
                }
                // A service account is never left in no organization.
                let services = service_accounts(&*stores.users, &found.members).await?;
                anyhow::ensure!(
                    services.is_empty(),
                    "organization {} still holds {} service account(s), which `--detach` does \
                     not release: {}. Move each with `users set-org <user_id> <org>`, or delete \
                     it with `users delete <user_id>`",
                    org.slug,
                    services.len(),
                    joined(&services, UserId::as_str)
                );
                let cleared = org::detach_all(&*org_store, &org.org_id).await?;
                if !cleared.is_empty() {
                    println!(
                        "Detached {} member(s) and {} directly-owned device(s).",
                        cleared.members.len(),
                        cleared.clients.len()
                    );
                }
            }
            // Re-read after any detach: a membership written in between would
            // be stranded silently.
            let found = dependents().await?;
            if !found.is_empty() {
                return Err(org_delete_refusal(&org, &found));
            }

            if let Some(catalog) = stores.descriptor_catalog.as_deref() {
                catalog.delete_tombstones_in_org(&org.org_id).await?;
            }

            // Record first, then the handle: a failure leaves a stranded handle,
            // never a free one an organization still holds.
            org_store.delete_org(&org.org_id).await?;
            tracing::info!(org_id = %org.org_id, slug = %org.slug, "organization deleted");
            org_store.release_org_slug(org.slug.as_str()).await?;
            println!("Deleted organization {} ({}).", org.slug, org.org_id);
            Ok(())
        }
        OrgAction::ReleaseClaim { slug } => {
            // Releasing a live organization's handle would let a second one
            // claim it.
            match org_store.lookup_org_by_slug(slug.as_str()).await? {
                None => println!("No claim holds {slug}; nothing to release."),
                Some(holder) if org_store.get_org(&holder).await?.is_some() => anyhow::bail!(
                    "{slug} is held by {holder}, which still has a record. Delete the \
                     organization with `orgs delete {slug}` instead of releasing its handle"
                ),
                Some(holder) => {
                    org_store.release_org_slug(slug.as_str()).await?;
                    tracing::info!(%slug, org_id = %holder, "stranded handle claim released");
                    println!("Released the claim on {slug}, stranded by {holder}.");
                }
            }
            Ok(())
        }
        OrgAction::Members { org } => {
            let org = org::require(&*org_store, &org).await?;
            let members = org::members(&*org_store, &*stores.users, &org.org_id).await?;
            if members.is_empty() {
                println!("No accounts belong to {}.", org.slug);
                return Ok(());
            }
            println!("{} account(s) in {}:", members.len(), org.slug);
            members.iter().for_each(|member| {
                println!(
                    " {:<44} | {:<8} | {}",
                    member.user_id.as_str(),
                    render_user_status(&member.status),
                    member.contact_email.as_str()
                );
            });
            Ok(())
        }
        OrgAction::Clients { org } => {
            let org = org::require(&*org_store, &org).await?;
            let clients = org::owned_clients(&*stores.clients, &*org_store, &org.org_id).await?;
            if clients.is_empty() {
                println!("No devices belong to {}.", org.slug);
                return Ok(());
            }
            println!("{} device(s) in {}:", clients.len(), org.slug);
            clients.iter().for_each(|(client, owner)| {
                println!(
                    " {:<44} | {:<8} | owner {:<44} | {}",
                    client.client_id.as_str(),
                    client.status.as_ref(),
                    owner.label(),
                    client.client_details.as_str()
                );
            });
            Ok(())
        }
        OrgAction::Benchmarks { action } => handle_org_benchmark_action(action, &stores).await,
    }
}

async fn service_accounts(
    users: &dyn UserStore,
    members: &[UserId],
) -> anyhow::Result<Vec<UserId>> {
    stream::iter(members)
        .then(|user_id| users.get_user(user_id))
        .try_filter_map(|user| async move {
            Ok(user
                .filter(|user| user.account_type == UserAccountType::Service)
                .map(|user| user.user_id))
        })
        .try_collect()
        .await
}

/// Steps listed in the order that works: `--detach` refuses while anything else
/// is left.
fn org_delete_refusal(org: &Organization, found: &org::Dependents) -> anyhow::Error {
    let mut steps = Vec::new();
    if !found.benchmarks.is_empty() {
        steps.push(format!(
            "Delete each private benchmark with `orgs benchmarks delete {} <id>`: {}",
            org.slug,
            joined(&found.benchmarks, BenchmarkId::as_str)
        ));
    }
    if !found.catalog_entries.is_empty() {
        steps.push(format!(
            "Tombstone each catalog entry with `catalog tombstone <name> --org {}`: {}",
            org.slug,
            joined(&found.catalog_entries, CatalogName::as_str)
        ));
    }
    steps.extend(holdings_steps(&found.held));
    if !found.preauth_keys.is_empty() {
        steps.push(format!(
            "Revoke each client pre-auth key with `clients preauth revoke <key_id>`: {}",
            joined(&found.preauth_keys, ClientPreauthKeyId::as_str)
        ));
    }
    if !found.members.is_empty() || !found.clients.is_empty() {
        let verb = if steps.is_empty() {
            "Clear"
        } else {
            "Then clear"
        };
        steps.push(format!(
            "{verb} the members and devices with `orgs delete {} --detach`, or reassign them",
            org.slug
        ));
    }
    anyhow::anyhow!(
        "organization {} cannot be deleted: {}. {}",
        org.slug,
        found.describe(),
        steps.join(". ")
    )
}

/// What clears `held` before its owner is deleted: a campaign can change hands,
/// a vault record cannot.
fn holdings_steps(held: &org::Holdings) -> Vec<String> {
    let mut steps = Vec::new();
    if !held.campaigns.is_empty() {
        steps.push(format!(
            "Hand each campaign over with `campaigns owner <campaign_id> --org <org>` (or \
             `--user`), or delete it: {}",
            joined(&held.campaigns, CampaignId::as_str)
        ));
    }
    if !held.vault_resources.is_empty() {
        steps.push(format!(
            "Delete each vault resource with `vault delete <resource_id>`: {}",
            joined(&held.vault_resources, VaultResourceId::as_str)
        ));
    }
    steps
}

/// Refuse `users delete` while the account owns a campaign or vault record.
async fn refuse_holdings(stores: &Stores, user_id: &UserId) -> anyhow::Result<()> {
    let owner = PrincipalOwner::User {
        user_id: user_id.clone(),
    };
    let held = org::holdings_of(&*stores.campaigns, &*stores.vault, &owner).await?;
    anyhow::ensure!(
        held.is_empty(),
        "user {user_id} still owns campaigns or vault resources. {}",
        holdings_steps(&held).join(". ")
    );
    Ok(())
}

fn joined<T>(ids: &[T], as_str: fn(&T) -> &str) -> String {
    ids.iter().map(as_str).collect::<Vec<_>>().join(", ")
}

/// The shadowing rule is `org_catalog::put`'s, shared with the route.
async fn handle_org_benchmark_action(
    action: OrgBenchmarkAction,
    stores: &Stores,
) -> anyhow::Result<()> {
    let catalog = &*stores.catalog;
    match action {
        OrgBenchmarkAction::List { org } => {
            org_benchmark_listing(stores, org.as_ref())
                .await?
                .iter()
                .for_each(|line| println!("{line}"));
            Ok(())
        }
        OrgBenchmarkAction::Show { org, benchmark_id } => {
            let (org_id, _) = catalog_holder(stores, &org).await?;
            let entry = org_catalog::require(catalog, &org_id, &benchmark_id).await?;
            println!("# org-benchmarks/{org_id}/{}.toml", entry.benchmark_id());
            print!("{}", entry.to_toml()?);
            Ok(())
        }
        OrgBenchmarkAction::Put { org, file } => {
            let org = org::require(&*stores.orgs, &org).await?;
            let (_, entry) = pipette_core::benchmark::load_entry(&file)?;
            let public = catalog.load_catalog().await?;
            let written = org_catalog::put(catalog, &public, &org.org_id, entry).await?;
            tracing::info!(
                org_id = %org.org_id,
                benchmark_id = %written.benchmark_id(),
                "organization benchmark written"
            );
            println!(
                "Wrote {} ({}) to {}'s catalog.",
                written.benchmark_id(),
                written.benchmark_type(),
                org.slug
            );
            Ok(())
        }
        OrgBenchmarkAction::Delete { org, benchmark_id } => {
            let (org_id, name) = catalog_holder(stores, &org).await?;
            org_catalog::delete(catalog, &org_id, &benchmark_id).await?;
            tracing::info!(%org_id, %benchmark_id, "organization benchmark deleted");
            println!("Deleted {benchmark_id} from {name}'s catalog.");
            Ok(())
        }
    }
}

async fn org_benchmark_listing(
    stores: &Stores,
    org: Option<&OrgRef>,
) -> anyhow::Result<Vec<String>> {
    let catalog = &*stores.catalog;
    let Some(org) = org else {
        let handles: HashMap<OrgId, String> = stores
            .orgs
            .list_orgs()
            .await?
            .into_iter()
            .map(|org| (org.org_id, org.slug.to_string()))
            .collect();
        let mut keys = catalog.list_org_benchmark_ids().await?;
        keys.sort_by(|(a_org, a_id), (b_org, b_id)| {
            (org_label(&handles, a_org), a_id).cmp(&(org_label(&handles, b_org), b_id))
        });
        let entries = org_catalog::read_kept(catalog, keys).await?;
        if entries.is_empty() {
            return Ok(vec!["No organization keeps a private benchmark.".into()]);
        }
        let orgs: HashSet<&OrgId> = entries.iter().map(|kept| &kept.org_id).collect();
        let header = format!(
            "{} private benchmark(s) across {} organization(s):",
            entries.len(),
            orgs.len()
        );
        return Ok(std::iter::once(header)
            .chain(org_benchmark_lines(&entries, Some(&handles)))
            .collect());
    };
    let (org_id, name) = catalog_holder(stores, org).await?;
    let entries = org_catalog::list(catalog, &org_id).await?;
    if entries.is_empty() {
        return Ok(vec![format!("No private benchmarks in {name}.")]);
    }
    let header = format!("{} private benchmark(s) in {name}:", entries.len());
    Ok(std::iter::once(header)
        .chain(org_benchmark_lines(&entries, None))
        .collect())
}

/// An organization's handle, or its id when the record is gone and entries
/// were left under it.
fn org_label<'a>(handles: &'a HashMap<OrgId, String>, org_id: &'a OrgId) -> &'a str {
    handles.get(org_id).map_or(org_id.as_str(), String::as_str)
}

fn org_benchmark_lines(
    entries: &[org_catalog::Kept],
    handles: Option<&HashMap<OrgId, String>>,
) -> Vec<String> {
    entries
        .iter()
        .map(|kept| {
            let org = handles.map_or_else(String::new, |handles| {
                format!(" {:<40} |", org_label(handles, &kept.org_id))
            });
            let what = match &kept.entry {
                Ok(entry) => entry.benchmark_type().to_string(),
                Err(error) => format!("unreadable: {error}"),
            };
            format!("{org} {:<48} | {what}", kept.benchmark_id.as_str())
        })
        .collect()
}

/// For a read or a delete; a write needs the record. An `org_…` id whose record
/// is gone still reaches entries left under it, so they can be removed; with
/// none left it is refused, so a typo does not read as an empty catalog.
async fn catalog_holder(stores: &Stores, reference: &OrgRef) -> anyhow::Result<(OrgId, String)> {
    if let Some(org) = org::resolve(&*stores.orgs, reference).await? {
        return Ok((org.org_id, org.slug.to_string()));
    }
    if let OrgRef::Id(org_id) = reference
        && !stores
            .catalog
            .list_benchmark_ids_of(org_id)
            .await?
            .is_empty()
    {
        return Ok((org_id.clone(), org_id.to_string()));
    }
    Err(org::OrgLookupError::NotFound(reference.clone()).into())
}

/// Owners are resolved before the write, so `org::owning_org` can read `None`
/// as "no organization" rather than "points at something missing".
async fn handle_client_owner_action(
    action: ClientOwnerAction,
    stores: &Stores,
) -> anyhow::Result<()> {
    let client_id = match &action {
        ClientOwnerAction::Show { client_id }
        | ClientOwnerAction::Set { client_id, .. }
        | ClientOwnerAction::Clear { client_id } => client_id.clone(),
    };
    stores
        .clients
        .get_client(&client_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("client {client_id} not found"))?;
    let current = stores.orgs.get_ownership(&client_id).await?;

    let owner = match action {
        ClientOwnerAction::Show { .. } => {
            let rolls_up_to = match org::owning_org(&*stores.orgs, &client_id).await? {
                Some(org_id) => stores
                    .orgs
                    .get_org(&org_id)
                    .await?
                    // Shown, not hidden: a dangling id is what the operator
                    // has to fix.
                    .map_or_else(
                        || format!("{org_id} (no record)"),
                        |org| org.slug.to_string(),
                    ),
                None => "-".to_string(),
            };
            println!("Client       {client_id}");
            println!(
                "Owner        {}",
                current
                    .as_ref()
                    .map_or_else(|| "-".to_string(), ClientOwner::label)
            );
            println!("Organization {rolls_up_to}");
            return Ok(());
        }
        ClientOwnerAction::Clear { .. } => None,
        ClientOwnerAction::Set { org, user, .. } => {
            // Clap refuses neither; restated rather than assumed.
            let owner = require_client_owner(stores, org, user).await?;
            Some(owner.ok_or_else(|| anyhow::anyhow!("pass exactly one of --org or --user"))?)
        }
    };

    if current == owner {
        println!(
            "Client {client_id}: no change (owner is already {}).",
            owner
                .as_ref()
                .map_or_else(|| "-".to_string(), ClientOwner::label)
        );
        return Ok(());
    }
    let label = |owner: &Option<ClientOwner>| {
        owner
            .as_ref()
            .map_or_else(|| "-".to_string(), ClientOwner::label)
    };
    let (before, after) = (label(&current), label(&owner));
    stores.orgs.set_ownership(&client_id, owner).await?;
    tracing::info!(client_id = %client_id, owner = %after, "client owner set");
    println!("Client {client_id}: owner {before} -> {after}.");
    Ok(())
}

/// Resolved against its roster, so no stored owner names a missing record.
async fn require_client_owner(
    stores: &Stores,
    org: Option<OrgRef>,
    user: Option<UserId>,
) -> anyhow::Result<Option<ClientOwner>> {
    Ok(match (org, user) {
        (None, None) => None,
        (Some(reference), None) => Some(ClientOwner::Org {
            org_id: org::require(&*stores.orgs, &reference).await?.org_id,
        }),
        (None, Some(user_id)) => Some(ClientOwner::User {
            user_id: require_user(&stores.users, &user_id).await?.user_id,
        }),
        // Clap refuses both; restated rather than assumed.
        (Some(_), Some(_)) => anyhow::bail!("an owner is an organization or an account, not both"),
    })
}

async fn handle_user_action(action: UserAction, stores: Stores) -> anyhow::Result<()> {
    let user_store = stores.users.clone();
    match action {
        UserAction::Create {
            email,
            adopt_stranded_claim,
            permissions,
            group,
            account_type,
            org,
            campaigns,
        } => {
            let permissions = group.resolve(permissions);
            let campaign_scope = (!campaigns.is_empty())
                .then(|| campaigns.into_iter().collect::<BTreeSet<CampaignId>>());
            anyhow::ensure!(
                campaign_scope.is_none() || account_type == UserAccountType::Service,
                "a campaign reader is a service account: pass --type service"
            );
            ensure_holdable(
                Holder {
                    account_type,
                    campaign_reader: campaign_scope.is_some(),
                },
                &permissions,
            )?;
            // Before the claim, so an unknown or disabled org refuses before anything is written.
            let org = match org {
                Some(reference) => Some(refuse_disabled(
                    org::require(&*stores.orgs, &reference).await?,
                )?),
                None if account_type == UserAccountType::Service => {
                    anyhow::bail!("a service account belongs to an organization: pass --org")
                }
                None => None,
            };
            if let (Some(scope), Some(org)) = (&campaign_scope, &org) {
                require_campaigns(&*stores.campaigns, scope, &org.org_id).await?;
            }
            let user = pipette_core::user::User {
                account_type,
                campaign_scope,
                ..pipette_core::user::User::try_new(email, chrono::Utc::now())?
            };
            // Claim first, as `orgs create` does.
            let (user, adopted) = match user_store
                .claim_user_email(user.contact_email.as_str(), &user.user_id)
                .await?
            {
                EmailClaim::Claimed => (user, false),
                EmailClaim::Taken(holder) => {
                    let stranded = user_store.get_user(&holder).await?.is_none();
                    match (stranded, adopt_stranded_claim) {
                        // A create that failed between its two writes: build
                        // the record under the id the claim already names.
                        (true, true) => {
                            println!(
                                "Adopting the stranded claim on {}.",
                                user.contact_email.as_str()
                            );
                            (
                                pipette_core::user::User {
                                    user_id: holder,
                                    ..user
                                },
                                true,
                            )
                        }
                        // Refused without the flag: a create still in flight
                        // looks the same, and adopting it steals its address.
                        (true, false) => anyhow::bail!(
                            "{} is claimed by {holder}, which has no account record. Either an \
                             earlier create failed part-way, or one is in flight now. Re-run \
                             with --adopt-stranded-claim to take the claim over.",
                            user.contact_email.as_str()
                        ),
                        (false, _) => anyhow::bail!(
                            "{} is already held by {holder}; an address names one account",
                            user.contact_email.as_str()
                        ),
                    }
                }
            };
            if let Err(write) = user_store.put_user(&user).await {
                // Release only while no record answers to this id: a concurrent
                // adopter may have written one. A failed probe still releases and
                // must not replace the write error the operator needs to see.
                let holds_record = match user_store.get_user(&user.user_id).await {
                    Ok(record) => record.is_some(),
                    Err(probe) => {
                        tracing::warn!(
                            user_id = %user.user_id,
                            error = %format_args!("{probe:#}"),
                            "failed to check for the account record; releasing the address claim \
                             anyway"
                        );
                        false
                    }
                };
                if !holds_record
                    && let Err(release) = user_store
                        .release_user_email(user.contact_email.as_str())
                        .await
                {
                    tracing::warn!(
                        user_id = %user.user_id,
                        error = %release,
                        "failed to release the address claim after the record write failed; \
                         `users release-claim <email>` will clear it"
                    );
                }
                return Err(write);
            }
            tracing::info!(user_id = %user.user_id, %account_type, adopted, "user created");
            println!(
                "Created user {} for {} ({account_type}, active).",
                user.user_id,
                user.contact_email.as_str()
            );
            // Only once the record exists, so no membership names a missing id.
            if let Some(org) = &org {
                if let Err(write) = stores
                    .orgs
                    .set_membership(&user.user_id, Some(org.org_id.clone()))
                    .await
                {
                    return Err(write.context(format!(
                        "user {} was created but belongs to no organization and holds nothing; \
                         place it with `users set-org {} {}`, then grant it with `users \
                         permission set`",
                        user.user_id, user.user_id, org.slug
                    )));
                }
                println!("User {} belongs to {}.", user.user_id, org.slug);
            }
            // Only once the record exists: a grant for a missing id is one
            // `delete` would never reach.
            if !permissions.is_empty() {
                let set = UserPermissionSet::from_iter(user_permission::normalize(&permissions));
                if let Err(grant) = user_store.put_user_permissions(&user.user_id, &set).await {
                    return Err(grant.context(format!(
                        "user {} was created but holds nothing; grant it with `users permission \
                         set {} {}`",
                        user.user_id,
                        user.user_id,
                        set.iter()
                            .map(|p| p.wire_name())
                            .collect::<Vec<_>>()
                            .join(" ")
                    )));
                }
                tracing::info!(
                    user_id = %user.user_id,
                    added = ?log_names(permissions.iter()),
                    resulting = ?set.names(),
                    "user permissions granted"
                );
                println!(
                    "User {} now holds: {}",
                    user.user_id,
                    render_permissions(set.iter())
                );
            } else if adopted {
                // An adopted id may still carry a dead account's grant (`delete`
                // removes it last); clear it rather than hand those rights on.
                if let Err(clear) = user_store.delete_user_permissions(&user.user_id).await {
                    return Err(clear.context(format!(
                        "user {} was created on an adopted claim but may still hold the grant of \
                         the account that stranded it; clear it with `users permission set {}`",
                        user.user_id, user.user_id
                    )));
                }
            }
            // Last: nobody has signed in as it yet, so its administrator may mint
            // the pre-auth key it registers with.
            user_store
                .mark_user_awaiting_sign_in(&user.user_id)
                .await
                .map_err(|e| {
                    e.context(
                        "the account was created, but its administrator cannot mint its \
                         pre-auth key over HTTP; `users preauth create` still can",
                    )
                })?;
            Ok(())
        }
        UserAction::List => {
            let users = user_store.list_users().await?;
            if users.is_empty() {
                println!("No user accounts.");
                return Ok(());
            }
            println!("{} user account(s):", users.len());
            users.iter().for_each(|user| {
                println!(
                    " {:<44} | {:<7} | {:<8} | {} | {}",
                    user.user_id.as_str(),
                    user.account_type,
                    render_user_status(&user.status),
                    user.created_at,
                    user.contact_email.as_str()
                );
            });
            Ok(())
        }
        UserAction::Show { user_id } => {
            let user = require_user(&user_store, &user_id).await?;
            let permissions = render_optional_permissions(
                user_store
                    .get_user_permissions(&user.user_id)
                    .await?
                    .map(|set| {
                        pipette_core::user_permission::effective(Some(set), Holder::of(&user))
                    }),
            );
            // The handle, which an operator types back; a dangling id is shown.
            let organization = match &stores.orgs.get_membership(&user.user_id).await? {
                None => "-".to_string(),
                Some(org_id) => stores.orgs.get_org(org_id).await?.map_or_else(
                    || format!("{org_id} (no record)"),
                    |org| org.slug.to_string(),
                ),
            };
            println!("User         {}", user.user_id);
            println!("Email        {}", user.contact_email.as_str());
            println!("Type         {}", user.account_type);
            println!("Status       {}", render_user_status(&user.status));
            println!("Organization {organization}");
            println!("Created      {}", user.created_at);
            println!("Permissions  {permissions}");
            if let Some(scope) = &user.campaign_scope {
                let scope: Vec<&str> = scope.iter().map(CampaignId::as_str).collect();
                println!("Campaigns    {}", scope.join(", "));
            }
            Ok(())
        }
        UserAction::Approve {
            user_id,
            permissions,
            group,
        } => {
            let permissions = group.resolve(permissions);
            let user = require_user(&user_store, &user_id).await?;
            ensure_holdable(Holder::of(&user), &permissions)?;
            match user.status {
                UserStatus::Pending => {
                    user_store
                        .put_user(&pipette_core::user::User {
                            status: UserStatus::Active,
                            ..user
                        })
                        .await?;
                    tracing::info!(user_id = %user_id, "user approved");
                    println!("Approved user {user_id} (pending -> active).");
                }
                // Not an error: the `--permission` grant below still applies.
                UserStatus::Active => eprintln!("User {user_id} is already active."),
                UserStatus::Disabled => anyhow::bail!(
                    "user {user_id} is disabled, not pending: it was admitted and then \
                     offboarded. Return it to service with `users enable {user_id}`"
                ),
            }
            if !permissions.is_empty() {
                let given = permissions.clone();
                let mutation = user_store
                    .mutate_user_permissions(&user_id, &move |set| {
                        let before = set.clone();
                        set.insert_all(permissions.iter().copied());
                        *set = set.normalized();
                        *set != before
                    })
                    .await?;
                if mutation.changed {
                    tracing::info!(
                        %user_id,
                        added = ?log_names(given.iter()),
                        resulting = ?mutation.resulting.names(),
                        "user permissions granted"
                    );
                }
                report_user_permissions(&user_id, mutation.changed, &mutation.resulting);
                report_implied_names(&given, &mutation.resulting);
            }
            Ok(())
        }
        UserAction::SetType {
            user_id,
            account_type,
        } => {
            let user = require_user(&user_store, &user_id).await?;
            if user.account_type == account_type {
                println!("User {user_id} is already {account_type}.");
                return Ok(());
            }
            anyhow::ensure!(
                user.campaign_scope.is_none(),
                "user {user_id} is a campaign reader, which is a service account; a person's \
                 account is a new account"
            );
            anyhow::ensure!(
                account_type != UserAccountType::Service
                    || stores.orgs.get_membership(&user_id).await?.is_some(),
                "a service account belongs to an organization: `users set-org` first"
            );
            let stored = user_store
                .get_user_permissions(&user_id)
                .await?
                .unwrap_or_default();
            let refused =
                user_permission::refused_for(Holder::unscoped(account_type), stored.known());
            anyhow::ensure!(
                refused.is_empty(),
                "user {user_id} holds {}, which a service account cannot; remove it with \
                 `users permission remove` first",
                log_names(refused.iter()).join(", ")
            );
            user_store
                .put_user(&pipette_core::user::User {
                    account_type,
                    ..user
                })
                .await?;
            tracing::info!(%user_id, %account_type, "user type set");
            println!("User {user_id} is now {account_type}.");
            Ok(())
        }
        UserAction::SetCampaigns { user_id, campaigns } => {
            let user = require_user(&user_store, &user_id).await?;
            anyhow::ensure!(
                user.account_type == UserAccountType::Service,
                "user {user_id} is a person's account; a campaign reader is a service account"
            );
            let stored = user_store
                .get_user_permissions(&user_id)
                .await?
                .unwrap_or_default();
            let refused = user_permission::refused_for(Holder::campaign_reader(), stored.known());
            anyhow::ensure!(
                refused.is_empty(),
                "user {user_id} holds {}, which a campaign reader cannot; remove it with \
                 `users permission remove` first",
                log_names(refused.iter()).join(", ")
            );
            let org_id = stores
                .orgs
                .get_membership(&user_id)
                .await?
                .ok_or_else(|| anyhow::anyhow!("user {user_id} belongs to no organization"))?;
            require_campaigns(&*stores.campaigns, &campaigns, &org_id).await?;
            let scope: BTreeSet<CampaignId> = campaigns.into_iter().collect();
            if user.campaign_scope.as_ref() == Some(&scope) {
                println!("User {user_id} already reads exactly these campaigns.");
                return Ok(());
            }
            let count = scope.len();
            user_store
                .put_user(&pipette_core::user::User {
                    campaign_scope: Some(scope),
                    ..user
                })
                .await?;
            tracing::info!(%user_id, campaigns = count, "user campaign scope set");
            println!("User {user_id} now reads {count} campaign(s) alone.");
            Ok(())
        }
        UserAction::Disable { user_id } => {
            set_user_status(&user_store, &user_id, UserStatus::Disabled).await
        }
        UserAction::Enable { user_id } => {
            // Admitting a pending account is `approve`'s decision, not this one.
            let user = require_user(&user_store, &user_id).await?;
            if user.status == UserStatus::Pending {
                anyhow::bail!(
                    "user {user_id} has never been admitted, so there is nothing to return \
                     it to. Admit it with `users approve {user_id}`"
                );
            }
            set_user_status(&user_store, &user_id, UserStatus::Active).await
        }
        UserAction::Delete { user_id, detach } => {
            // Before any detach: a campaign or vault record naming a gone
            // account reaches only `_any_` holders.
            refuse_holdings(&stores, &user_id).await?;

            // A device left pointing at a deleted account never resolves again,
            // and drops out of every organization listing silently.
            if detach {
                let cleared = org::detach_devices_of(&*stores.orgs, &user_id).await?;
                if !cleared.is_empty() {
                    println!("Detached {} device(s) from {user_id}.", cleared.len());
                }
            } else {
                let owned = org::devices_owned_by(&*stores.orgs, &user_id).await?;
                if !owned.is_empty() {
                    anyhow::bail!(
                        "user {user_id} still owns {} device(s): {}. Clear them with \
                         `users delete {user_id} --detach`, or reassign each with \
                         `clients owner set <client_id> --org <org>`",
                        owned.len(),
                        joined(&owned, ClientId::as_str)
                    );
                }
            }
            // Read again: the account is still active, and may have filed or
            // uploaded since the first read.
            refuse_holdings(&stores, &user_id).await?;

            // Idempotent so a re-run converges; the "no record" line is the only
            // sign of a mistyped id.
            let deleted =
                user_admin::delete(&*user_store, &*stores.key_index, &*stores.orgs, &user_id)
                    .await?;
            match (&deleted.email, deleted.released_claim) {
                (Some(email), true) => println!(
                    "Deleted user {user_id} ({}) and its {} registered key(s).",
                    email.as_str(),
                    deleted.keys
                ),
                (Some(email), false) => println!(
                    "Deleted user {user_id}, but left the claim on {} alone: it names another \
                     account.",
                    email.as_str()
                ),
                (None, _) => println!(
                    "No record for {user_id}; removed its permissions. Any address claim it \
                     still holds must be cleared with `users release-claim <email>`."
                ),
            }
            Ok(())
        }
        UserAction::ReleaseClaim { email } => {
            // Releasing a live account's address would let a second one claim it.
            match user_store.lookup_user_by_email(email.as_str()).await? {
                None => println!("No claim holds {}; nothing to release.", email.as_str()),
                Some(holder) if user_store.get_user(&holder).await?.is_some() => anyhow::bail!(
                    "{} is held by {holder}, which still has an account record. Delete the \
                     account with `users delete {holder}` instead of releasing its address",
                    email.as_str()
                ),
                Some(holder) => {
                    user_store.release_user_email(email.as_str()).await?;
                    tracing::info!(user_id = %holder, "stranded address claim released");
                    println!(
                        "Released the claim on {}, stranded by {holder}.",
                        email.as_str()
                    );
                }
            }
            Ok(())
        }
        UserAction::SetOrg { user_id, org, none } => {
            let user = require_user(&user_store, &user_id).await?;
            if none && user.account_type == UserAccountType::Service {
                anyhow::bail!(
                    "{user_id} is a service account, which belongs to an organization; move it to another instead"
                );
            }
            // Resolved before the write, so a membership names an existing org.
            let org_id = match (org, none) {
                (Some(reference), _) => Some(org::require(&*stores.orgs, &reference).await?),
                (None, true) => None,
                // Clap refuses this; restated rather than assumed.
                (None, false) => anyhow::bail!("pass an organization, or --none to clear"),
            };
            let before = stores.orgs.get_membership(&user_id).await?;
            if before == org_id.as_ref().map(|org| org.org_id.clone()) {
                println!("User {user_id}: no change.");
                return Ok(());
            }
            let org_id = org_id.map(refuse_disabled).transpose()?;
            stores
                .orgs
                .set_membership(&user_id, org_id.as_ref().map(|org| org.org_id.clone()))
                .await?;
            tracing::info!(
                %user_id,
                org_id = org_id.as_ref().map(|org| tracing::field::display(&org.org_id)),
                "user organization set"
            );
            match (before, org_id) {
                (_, Some(org)) => println!("User {user_id} now belongs to {}.", org.slug),
                (Some(before), None) => {
                    println!("User {user_id} no longer belongs to {before}.")
                }
                (None, None) => println!("User {user_id} belongs to no organization."),
            }
            Ok(())
        }
        UserAction::Permission { action } => {
            handle_user_permission_action(action, user_store).await
        }
        UserAction::Key { action } => handle_user_key_action(action, &stores).await,
        UserAction::Preauth { action } => handle_user_preauth_action(action, user_store).await,
    }
}

/// `(none)` for an empty set, so an operator sees it is empty, not missing.
fn render_permissions<S: AsRef<str>>(names: impl IntoIterator<Item = S>) -> String {
    let rendered = names
        .into_iter()
        .map(|name| name.as_ref().to_string())
        .collect::<Vec<_>>()
        .join(", ");
    if rendered.is_empty() {
        "(none)".to_string()
    } else {
        rendered
    }
}

/// `-` for no set at all, apart from `(none)`: a key with no template gets the
/// registration default, while an empty template grants nothing.
fn render_optional_permissions<S: AsRef<str>>(set: Option<impl IntoIterator<Item = S>>) -> String {
    set.map_or_else(|| "-".to_string(), render_permissions)
}

fn render_user_status(status: &UserStatus) -> &'static str {
    match status {
        UserStatus::Active => "active",
        UserStatus::Pending => "pending",
        UserStatus::Disabled => "disabled",
    }
}

/// A whole-record write, sound because every writer of an existing record sets
/// `status` alone: this, `users approve` and `PATCH /api/v1/users/{user_id}`.
async fn set_user_status(
    user_store: &Arc<dyn UserStore>,
    user_id: &UserId,
    status: UserStatus,
) -> anyhow::Result<()> {
    let user = require_user(user_store, user_id).await?;
    let rendered = render_user_status(&status);
    if user.status == status {
        println!("User {user_id} is already {rendered}.");
        return Ok(());
    }
    user_store
        .put_user(&pipette_core::user::User { status, ..user })
        .await?;
    tracing::info!(%user_id, %status, "user status set");
    println!("User {user_id} is now {rendered}.");
    Ok(())
}

async fn handle_user_permission_action(
    action: UserPermissionAction,
    user_store: Arc<dyn UserStore>,
) -> anyhow::Result<()> {
    match action {
        UserPermissionAction::Add {
            user_id,
            permissions,
            group,
        } => {
            let user = require_user(&user_store, &user_id).await?;
            let permissions = group.resolve(permissions);
            ensure_holdable(Holder::of(&user), &permissions)?;
            let given = permissions.clone();
            let mutation = user_store
                .mutate_user_permissions(&user_id, &move |set| {
                    let before = set.clone();
                    set.insert_all(permissions.iter().copied());
                    // The whole object: a kept implied name would survive a
                    // later `remove` of the wider one.
                    *set = set.normalized();
                    *set != before
                })
                .await?;
            if mutation.changed {
                tracing::info!(
                    %user_id,
                    added = ?log_names(given.iter()),
                    resulting = ?mutation.resulting.names(),
                    "user permissions granted"
                );
            }
            report_user_permissions(&user_id, mutation.changed, &mutation.resulting);
            report_implied_names(&given, &mutation.resulting);
            Ok(())
        }
        UserPermissionAction::Remove {
            user_id,
            permissions,
        } => {
            let user = require_user(&user_store, &user_id).await?;
            let given = permissions.clone();
            let mutation = user_store
                .mutate_user_permissions(&user_id, &move |set| {
                    set.remove_all(permissions.iter().map(|p| p.as_ref()))
                })
                .await?;
            if mutation.changed {
                tracing::info!(
                    %user_id,
                    removed = ?log_names(given.iter()),
                    resulting = ?mutation.resulting.names(),
                    "user permissions revoked"
                );
            }
            report_user_permissions(&user_id, mutation.changed, &mutation.resulting);
            report_still_granted_names(
                &given,
                &user_permission::effective(Some(mutation.resulting.clone()), Holder::of(&user)),
            );
            Ok(())
        }
        UserPermissionAction::Set {
            user_id,
            permissions,
            group,
        } => {
            let user = require_user(&user_store, &user_id).await?;
            let permissions = group.resolve(permissions);
            ensure_holdable(Holder::of(&user), &permissions)?;
            let set = UserPermissionSet::from_iter(user_permission::normalize(&permissions));
            user_store.put_user_permissions(&user_id, &set).await?;
            tracing::info!(
                %user_id,
                requested = ?log_names(permissions.iter()),
                resulting = ?set.names(),
                "user permissions replaced"
            );
            report_user_permissions(&user_id, true, &set);
            report_implied_names(&permissions, &set);
            Ok(())
        }
        UserPermissionAction::List { user_id } => {
            let user = require_user(&user_store, &user_id).await?;
            // Resolved, not stored: implied names widen what was granted.
            let stored = user_store.get_user_permissions(&user_id).await?;
            let resolved = pipette_core::user_permission::effective(stored, Holder::of(&user));
            println!("User {user_id}: {}", render_permissions(resolved.iter()));
            Ok(())
        }
    }
}

/// `org`, unless it is disabled: it keeps its members and takes no new one.
fn refuse_disabled(org: Organization) -> anyhow::Result<Organization> {
    anyhow::ensure!(
        org.status != OrgStatus::Disabled,
        "organization {} is disabled and takes no new members; `orgs enable` it first",
        org.slug
    );
    Ok(org)
}

fn ensure_holdable(holder: Holder, permissions: &[UserPermission]) -> anyhow::Result<()> {
    let refused = user_permission::refused_for(holder, permissions.iter().copied());
    anyhow::ensure!(
        refused.is_empty(),
        "{} cannot hold {}",
        holder.noun(),
        log_names(refused.iter()).join(", ")
    );
    Ok(())
}

/// Refuse a campaign scope naming a campaign that does not exist, or that
/// `org`, the reader's organization, does not own.
async fn require_campaigns<'a>(
    campaigns: &dyn pipette_store::CampaignStore,
    ids: impl IntoIterator<Item = &'a CampaignId>,
    org: &OrgId,
) -> anyhow::Result<()> {
    let found = futures::future::try_join_all(
        ids.into_iter()
            .map(|id| campaign_admin::require(campaigns, id)),
    )
    .await?;
    found.iter().try_for_each(|campaign| {
        anyhow::ensure!(
            matches!(&campaign.owner, CampaignOwner::Org { org_id } if org_id == org),
            "campaign {} is not the account's organization's: a campaign reader reads its own \
             organization's campaigns alone",
            campaign.campaign_id
        );
        Ok(())
    })
}

/// So `permission list` showing less than was typed is no surprise: the grant
/// is the same, the object smaller.
fn report_implied_names(given: &[UserPermission], stored: &UserPermissionSet) {
    let dropped: Vec<&str> = given
        .iter()
        .filter(|permission| !stored.contains(&StoredUserPermission::from(**permission)))
        .map(AsRef::as_ref)
        .collect();
    if !dropped.is_empty() {
        println!(
            "Not stored, already implied by the rest: {}",
            dropped.join(", ")
        );
    }
}

/// Removing a name a wider one implies reaches nothing in a normalized set;
/// without this line the account looks stripped of a right it still holds.
fn report_still_granted_names(removed: &[UserPermission], resolved: &UserPermissionSet) {
    let still: Vec<&str> = removed
        .iter()
        .filter(|permission| resolved.contains(&StoredUserPermission::from(**permission)))
        .map(AsRef::as_ref)
        .collect();
    if !still.is_empty() {
        println!(
            "Still granted, by a wider name the account holds: {}. Remove that name instead.",
            still.join(", ")
        );
    }
}

fn report_user_permissions(user_id: &UserId, changed: bool, set: &UserPermissionSet) {
    let names = render_permissions(set.iter());
    if changed {
        println!("User {user_id} now holds: {names}");
    } else {
        println!("User {user_id} unchanged; holds: {names}");
    }
}

async fn handle_user_key_action(action: UserKeyAction, stores: &Stores) -> anyhow::Result<()> {
    match action {
        UserKeyAction::List { user_id } => {
            require_user(&stores.users, &user_id).await?;
            let keys = stores.users.list_user_keys(&user_id).await?;
            if keys.is_empty() {
                println!("User {user_id} holds no key.");
                return Ok(());
            }
            let now = chrono::Utc::now();
            let rows: Vec<UserKeyRow> = keys.iter().map(|key| user_key_row(key, now)).collect();
            let mut table = tabled::Table::new(&rows);
            table.with(tabled::settings::Style::psql());
            println!("User {user_id} holds {} key(s):\n{table}", keys.len());
            Ok(())
        }
        UserKeyAction::Revoke { user_id, key_id } => {
            require_user(&stores.users, &user_id).await?;
            // Refused, not reported done: a mistyped id would leave a stolen
            // key signing.
            if !user_admin::revoke_key(&*stores.users, &*stores.key_index, &user_id, &key_id)
                .await?
            {
                anyhow::bail!(
                    "user {user_id} holds no key {key_id}; `users key list {user_id}` lists the \
                     ones it does"
                );
            }
            tracing::info!(%user_id, %key_id, "user key revoked");
            println!("Revoked key {key_id} of user {user_id}.");
            Ok(())
        }
    }
}

#[derive(tabled::Tabled)]
struct UserKeyRow {
    #[tabled(rename = "Key ID")]
    key_id: String,
    #[tabled(rename = "Details")]
    key_details: String,
    #[tabled(rename = "Added")]
    added_at: String,
    /// `never`, the date, or `expired` above the date.
    #[tabled(rename = "Expires")]
    expires: String,
}

fn user_key_row(key: &UserKey, now: chrono::DateTime<chrono::Utc>) -> UserKeyRow {
    let expires = match key.expires_at {
        None => "never".to_string(),
        Some(at) if key.expired(now) => format!("expired\n{}", at.to_rfc3339()),
        Some(at) => at.to_rfc3339(),
    };
    UserKeyRow {
        key_id: key.key_id.to_string(),
        key_details: key.key_details.to_string(),
        added_at: key.added_at.to_rfc3339(),
        expires,
    }
}

async fn handle_user_preauth_action(
    action: UserPreauthAction,
    user_store: Arc<dyn UserStore>,
) -> anyhow::Result<()> {
    match action {
        UserPreauthAction::Create {
            user_id,
            multi_use,
            expires_in,
            no_expiry,
            note,
        } => {
            require_user(&user_store, &user_id).await?;
            let now = chrono::Utc::now();
            let ttl = match expires_in {
                Some(d) => Some(chrono::Duration::from_std(d)?),
                None if no_expiry => None,
                None => Some(chrono::Duration::days(DEFAULT_CLIENT_PREAUTH_TTL_DAYS)),
            };
            let expires_at = ttl.map(|d| now + d);
            let minted = user_preauth::mint(
                UserPreauthParams {
                    user_id: user_id.clone(),
                    usage: if multi_use {
                        UserPreauthUsage::MultiUse
                    } else {
                        UserPreauthUsage::SingleUse
                    },
                    expires_at,
                    note,
                },
                now,
            )?;
            user_store.put_user_preauth_key(&minted.key).await?;
            tracing::info!(key_id = %minted.key.key_id, %user_id, "user pre-auth key minted");
            println!("Minted user pre-auth key {}", minted.key.key_id);
            println!(
                "Token (shown once, store it now):

  {}
",
                // The one place a token is shown, which is what `expose` marks.
                minted.token.expose()
            );
            Ok(())
        }
        UserPreauthAction::List => {
            let mut keys = user_store.list_user_preauth_keys().await?;
            if keys.is_empty() {
                println!("No user pre-auth keys.");
                return Ok(());
            }
            keys.sort_by_key(|k| std::cmp::Reverse(k.created_at));
            let now = chrono::Utc::now();
            let rows: Vec<UserPreauthRow> = keys.iter().map(|k| user_preauth_row(k, now)).collect();
            let mut table = tabled::Table::new(&rows);
            table.with(tabled::settings::Style::psql());
            println!("{} user pre-auth key(s):\n{table}", keys.len());
            Ok(())
        }
        UserPreauthAction::Revoke { key_id } => {
            user_store.delete_user_preauth_key(&key_id).await?;
            tracing::info!(%key_id, "user pre-auth key revoked");
            println!("Revoked user pre-auth key {key_id}.");
            Ok(())
        }
        UserPreauthAction::Prune { dry_run } => {
            let now = chrono::Utc::now();
            // Records are listed before markers, which is what makes the race
            // err toward keeping a marker (see [`preauth_prune::targets`]).
            let keys = user_store.list_user_preauth_keys().await?;
            let markers = user_store.list_spent_user_preauth_markers().await?;
            let targets = preauth_prune::targets(
                keys.into_iter().map(|k| {
                    let expired = k.is_expired(now);
                    (k.key_id, expired)
                }),
                markers,
            );

            if targets.is_empty() {
                println!("Nothing to prune.");
                return Ok(());
            }

            if !dry_run {
                stream::iter(targets.expired.iter().map(|id| {
                    let user_store = user_store.clone();
                    let id = id.clone();
                    async move { user_store.delete_user_preauth_key(&id).await }
                }))
                .buffer_unordered(STORAGE_CONCURRENCY)
                .try_collect::<Vec<_>>()
                .await?;
                stream::iter(targets.orphaned.iter().map(|id| {
                    let user_store = user_store.clone();
                    let id = id.clone();
                    async move { user_store.delete_spent_user_preauth_marker(&id).await }
                }))
                .buffer_unordered(STORAGE_CONCURRENCY)
                .try_collect::<Vec<_>>()
                .await?;
                tracing::info!(
                    expired = targets.expired.len(),
                    orphaned = targets.orphaned.len(),
                    "user pre-auth keys pruned"
                );
            }
            report_preauth_prune(&targets, dry_run, "user pre-auth");
            Ok(())
        }
    }
}

/// Paired fields stack in one column, as in [`ClientPreauthRow`].
#[derive(tabled::Tabled)]
struct UserPreauthRow {
    /// key_id, then created-at.
    #[tabled(rename = "Key ID")]
    key_id: String,
    #[tabled(rename = "Account")]
    user_id: String,
    /// status, then expires-at.
    #[tabled(rename = "Lifecycle")]
    lifecycle: String,
    #[tabled(rename = "Usage")]
    usage: String,
    #[tabled(rename = "Note")]
    note: String,
}

fn user_preauth_row(k: &UserPreauthKey, now: chrono::DateTime<chrono::Utc>) -> UserPreauthRow {
    let usage = match k.usage {
        UserPreauthUsage::SingleUse => "single-use",
        UserPreauthUsage::MultiUse => "multi-use",
    };
    let status = if k.is_expired(now) {
        "expired"
    } else {
        "active"
    };
    let expires = k
        .expires_at
        .map_or_else(|| "never".to_string(), |e| e.to_rfc3339());
    UserPreauthRow {
        key_id: format!("{}\n{}", k.key_id, k.created_at.to_rfc3339()),
        user_id: k.user_id.to_string(),
        lifecycle: format!("{status}\n{expires}"),
        usage: usage.to_string(),
        note: k.note.clone().unwrap_or_else(|| "-".to_string()),
    }
}

/// `--all` cannot revalidate at write time: a client deleted mid-run leaves a
/// flag that the next run's deleted-client cleanup drops.
async fn handle_flag_reindex(
    client_id: Option<ClientId>,
    all: bool,
    stores: &Stores,
) -> anyhow::Result<()> {
    let client_ids: Vec<ClientId> = match (client_id, all) {
        (Some(client_id), _) => {
            stores
                .clients
                .get_client(&client_id)
                .await?
                .ok_or_else(|| anyhow::anyhow!("client {client_id} not found"))?;
            vec![client_id]
        }
        (None, true) => {
            let fleet = drain_clients(stores.clients.as_ref()).await?;
            warn_unreadable_clients(&fleet.unreadable);
            fleet.items.into_iter().map(|c| c.client_id).collect()
        }
        (None, false) => anyhow::bail!("provide a client id or --all"),
    };

    if client_ids.is_empty() {
        println!("No registered clients; nothing to flag.");
        return Ok(());
    }

    client_admin::flag_reindex(stores.todo.as_ref(), &client_ids).await?;

    println!(
        "Flagged {} client(s) for reindex. The server refuses claim, reclaim, \
         and submit for each one until the next `queue-maintenance` run \
         rebuilds its markers, or clears them for a client that is not a \
         worker.",
        client_ids.len()
    );
    Ok(())
}

/// Related fields stack in one column (`tabled` renders a newline as a second
/// line) to keep the table narrow.
#[derive(tabled::Tabled)]
struct ClientPreauthRow {
    /// key_id, then created-at.
    #[tabled(rename = "Key ID")]
    key_id: String,
    /// status, then expires-at.
    #[tabled(rename = "Lifecycle")]
    lifecycle: String,
    #[tabled(rename = "Usage")]
    usage: String,
    /// seeded tags, org label, owner, then the permission template.
    #[tabled(rename = "Seeds")]
    seeds: String,
    #[tabled(rename = "Note")]
    note: String,
}

/// Applied without --expires-in or --no-expiry, so a forgotten flag never yields
/// a permanent key.
const DEFAULT_CLIENT_PREAUTH_TTL_DAYS: i64 = 90;

fn report_preauth_prune<Id: std::fmt::Display>(
    targets: &preauth_prune::Targets<Id>,
    dry_run: bool,
    kind: &str,
) {
    let verb = if dry_run { "Would prune" } else { "Pruned" };
    if !targets.expired.is_empty() {
        println!("{verb} {} expired {kind} key(s):", targets.expired.len());
        targets.expired.iter().for_each(|id| println!("  {id}"));
    }
    if !targets.orphaned.is_empty() {
        println!(
            "{verb} {} spent {kind} key marker(s):",
            targets.orphaned.len()
        );
        targets.orphaned.iter().for_each(|id| println!("  {id}"));
    }
}

async fn handle_client_preauth_action(
    action: ClientPreauthAction,
    stores: &Stores,
) -> anyhow::Result<()> {
    let client_store = &stores.clients;
    match action {
        ClientPreauthAction::Create {
            multi_use,
            expires_in,
            no_expiry,
            tags,
            permissions,
            no_permissions,
            org,
            owner_org,
            owner_user,
            note,
        } => {
            let default_owner = require_client_owner(stores, owner_org, owner_user).await?;
            let usage = if multi_use {
                ClientPreauthUsage::MultiUse
            } else {
                ClientPreauthUsage::SingleUse
            };
            let now = chrono::Utc::now();
            let ttl = match expires_in {
                Some(d) => Some(chrono::Duration::from_std(d)?),
                None if no_expiry => None,
                None => Some(chrono::Duration::days(DEFAULT_CLIENT_PREAUTH_TTL_DAYS)),
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
            // No template, an empty one, or a set: a repeatable flag alone
            // cannot say "empty", hence `--no-permissions`.
            let default_permissions = if no_permissions {
                Some(std::collections::BTreeSet::new())
            } else if permissions.is_empty() {
                None
            } else {
                Some(permissions.into_iter().collect())
            };

            let minted = client_preauth::mint(
                ClientPreauthParams {
                    usage,
                    expires_at,
                    default_tags,
                    default_organization,
                    default_owner,
                    default_permissions,
                    note,
                },
                now,
            )?;
            client_store.put_client_preauth_key(&minted.key).await?;
            tracing::info!(key_id = %minted.key.key_id, "client pre-auth key minted");

            println!("Created pre-auth key {}.", minted.key.key_id);
            println!(
                "  usage: {}   expires: {}",
                match usage {
                    ClientPreauthUsage::SingleUse => "single-use",
                    ClientPreauthUsage::MultiUse => "multi-use",
                },
                match expires_at {
                    Some(at) => at.to_rfc3339(),
                    None => "never".to_string(),
                }
            );
            if let Some(owner) = &minted.key.default_owner {
                println!("  owner: {}", owner.label());
            }
            println!(
                "\nToken (shown once, store it now):\n\n  {}\n",
                // The one place a token is shown, which is what `expose` marks.
                minted.token.expose()
            );
        }
        ClientPreauthAction::List => {
            let mut keys = client_store.list_client_preauth_keys().await?;
            if keys.is_empty() {
                println!("No pre-auth keys.");
                return Ok(());
            }
            keys.sort_by_key(|k| std::cmp::Reverse(k.created_at));
            let now = chrono::Utc::now();
            let rows: Vec<ClientPreauthRow> =
                keys.iter().map(|k| client_preauth_row(k, now)).collect();
            let mut table = tabled::Table::new(&rows);
            table.with(tabled::settings::Style::psql());
            println!("{} pre-auth key(s):\n{table}", keys.len());
        }
        ClientPreauthAction::Revoke { key_id } => {
            client_store.delete_client_preauth_key(&key_id).await?;
            tracing::info!(%key_id, "client pre-auth key revoked");
            println!("Revoked (deleted) pre-auth key {key_id}.");
        }
        ClientPreauthAction::Prune { dry_run } => {
            let now = chrono::Utc::now();
            // Records are listed before markers, which is what makes the race
            // err toward keeping a marker (see [`preauth_prune::targets`]).
            let keys = client_store.list_client_preauth_keys().await?;
            let markers = client_store.list_spent_client_preauth_markers().await?;
            let targets = preauth_prune::targets(
                keys.into_iter().map(|k| {
                    let expired = k.is_expired(now);
                    (k.key_id, expired)
                }),
                markers,
            );

            if targets.is_empty() {
                println!("Nothing to prune.");
                return Ok(());
            }

            if !dry_run {
                stream::iter(targets.expired.iter().map(|id| {
                    let client_store = client_store.clone();
                    let id = id.clone();
                    async move { client_store.delete_client_preauth_key(&id).await }
                }))
                .buffer_unordered(STORAGE_CONCURRENCY)
                .try_collect::<Vec<_>>()
                .await?;
                stream::iter(targets.orphaned.iter().map(|id| {
                    let client_store = client_store.clone();
                    let id = id.clone();
                    async move { client_store.delete_spent_client_preauth_marker(&id).await }
                }))
                .buffer_unordered(STORAGE_CONCURRENCY)
                .try_collect::<Vec<_>>()
                .await?;
                tracing::info!(
                    expired = targets.expired.len(),
                    orphaned = targets.orphaned.len(),
                    "client pre-auth keys pruned"
                );
            }
            report_preauth_prune(&targets, dry_run, "pre-auth");
        }
    }
    Ok(())
}

/// Cells `queue-maintenance` feeds (`Expired` too) render `-` before its first
/// snapshot: unknown, not zero. Ingestion writes `Warnings`, so `0` is real.
struct PlanRow<'a> {
    manifest: &'a PlanManifest,
    note: Option<&'a str>,
    /// A requested cancel not yet latched: only the cancel marker can say.
    cancel_pending: bool,
}

/// Work no approved client can take; `plans status --explain` names the clause.
fn starved_cell(snapshot: Option<&ProgressSnapshot>) -> String {
    let Some(snapshot) = snapshot else {
        return "-".to_string();
    };
    group_cell(
        snapshot.starved.iter().map(|g| g.job_ids.len()).sum(),
        snapshot.starved.len(),
    )
}

/// Jobs that matched no approved client at ingestion.
fn warnings_cell(warnings: &[Warning]) -> String {
    group_cell(
        warnings.iter().map(|w| w.job_ids.len()).sum(),
        warnings.len(),
    )
}

/// One unit for `Starved` and `Warnings`, the same failure at two points in
/// time, so an operator can compare them.
fn group_cell(jobs: usize, groups: usize) -> String {
    if groups == 0 {
        return "0".to_string();
    }
    format!("{jobs} in {groups} group(s)")
}

fn count_cell(snapshot: Option<&ProgressSnapshot>, pick: fn(&Counts) -> u64) -> String {
    snapshot.map_or_else(|| "-".to_string(), |s| pick(&s.counts).to_string())
}

/// The tally sits on the manifest, but only `queue-maintenance` records a lapse,
/// so the cell keys on that pass's snapshot.
fn expired_cell(snapshot: Option<&ProgressSnapshot>, expired_jobs: usize) -> String {
    match snapshot {
        Some(_) => expired_jobs.to_string(),
        None => "-".to_string(),
    }
}

/// Names the stranded share: a stalled `queue-maintenance` cron strands leases,
/// and most verdicts blame the stale snapshot instead.
fn leased_cell(census: &JobCensus) -> String {
    match census.lapsed_leases {
        0 => census.leased.to_string(),
        n => format!("{} ({n} behind a lapsed lease)", census.leased),
    }
}

fn expires_label(expires_at: ExpiresAt) -> String {
    match expires_at {
        ExpiresAt::At(at) => at.to_rfc3339(),
        ExpiresAt::Never => "never".to_string(),
    }
}

fn expiry_range(job_expiry: JobExpiry, sep: &str) -> String {
    let JobExpiry { earliest, latest } = job_expiry;
    if earliest == latest {
        expires_label(earliest)
    } else {
        format!("{}{sep}{}", expires_label(earliest), expires_label(latest))
    }
}

fn job_expiry_cell(job_expiry: Option<JobExpiry>) -> String {
    job_expiry.map_or_else(|| "-".to_string(), |e| expiry_range(e, "\n→ "))
}

/// `operator`, not `-`: no user filed the plan, so the `campaigns` commands or
/// (on an old manifest) a client did.
fn submitter_label(submitted_by: Option<&UserId>) -> String {
    submitted_by.map_or_else(|| "operator".to_string(), ToString::to_string)
}

async fn handle_plan_action(
    action: PlanAction,
    stores: &Stores,
    config: &Config,
) -> anyhow::Result<()> {
    match action {
        PlanAction::List {
            status,
            submitter,
            campaign_id,
            columns,
        } => {
            let manifests = plan_cli::list_plans(
                stores.plans.as_ref(),
                status.map(Into::into),
                submitter.as_ref(),
                campaign_id.as_ref(),
            )
            .await?;
            if manifests.is_empty() {
                match (status, &submitter, &campaign_id) {
                    (_, _, Some(c)) => println!("No plans in {c} match those filters."),
                    (Some(_), Some(s), None) => println!("No plans from {s} match that status."),
                    (Some(_), None, None) => println!("No plans match that status."),
                    (None, Some(s), None) => println!("No plans from {s}."),
                    (None, None, None) => println!("No plans ingested."),
                }
                return Ok(());
            }
            let columns = if columns.is_empty() {
                DEFAULT_PLAN_COLUMNS.to_vec()
            } else {
                columns
            };
            // A cancel is invisible in `status` until the next maintenance pass.
            let pending: std::collections::HashSet<PlanId> = stores
                .plans
                .list_cancel_markers()
                .await?
                .into_iter()
                .collect();

            // The notes are kept apart from the manifests, so they cost one
            // batch read; every other cell reads the listing.
            let ids: Vec<PlanId> = manifests.iter().map(|p| p.plan_id.clone()).collect();
            let notes = stores.plans.get_plan_notes_of(&ids).await?;

            println!("{} plan(s):\n", manifests.len());
            let rows: Vec<PlanRow> = manifests
                .iter()
                .map(|p| PlanRow {
                    manifest: p,
                    note: notes.get(&p.plan_id).and_then(|n| n.note.as_deref()),
                    cancel_pending: pending.contains(&p.plan_id),
                })
                .collect();
            println!("{}", column_table(&columns, &rows));
        }
        PlanAction::Status {
            plan_id,
            explain,
            leases,
        } => {
            let now = chrono::Utc::now();
            // `--explain` prints the starved groups with their ladders, so the
            // status block drops its own copy.
            match (leases, explain) {
                (false, false) => {
                    let record = plan_cli::plan_status(stores.plans.as_ref(), &plan_id).await?;
                    print_plan_status(record, true);
                }
                (true, false) => {
                    let (record, held) = plan_cli::plan_leases(stores, &plan_id, now).await?;
                    print_plan_status(record, true);
                    print_plan_leases(&held);
                }
                // The explanation carries the lease lists, so both blocks date
                // from one read.
                (_, true) => {
                    let (record, report) = plan_cli::explain_plan(stores, &plan_id, now).await?;
                    print_plan_status(record, false);
                    if leases {
                        print_plan_leases(&report.leases);
                    }
                    print_plan_explanation(&report);
                }
            }
        }
        PlanAction::Results {
            plan_id,
            limit,
            offset,
            json,
        } => {
            let duckdb = config.duckdb.clone().ok_or_else(|| {
                anyhow::anyhow!(
                    "`plans results` reads pipette-duckdb; add a [duckdb] section to the config"
                )
            })?;
            let manifest = stores
                .plans
                .get_plan(&plan_id)
                .await?
                .ok_or_else(|| anyhow::anyhow!("no plan {plan_id}"))?;
            let http = reqwest::Client::builder()
                .connect_timeout(std::time::Duration::from_secs(
                    config.http_connect_timeout_secs.get(),
                ))
                .build()?;
            let page = ResultsDb::new(http, duckdb)?
                .plan_results(&plan_id, offset, limit)
                .await?;
            let jobs = manifest.job_ids.len();
            let body = pipette_plan_types::ResultListView::from(ResultListView::new(
                FleetDetail::Full,
                plan_id,
                jobs,
                page,
                offset,
                limit,
            ));
            if json {
                println!("{}", serde_json::to_string_pretty(&body)?);
            } else {
                print_plan_results(&body);
            }
        }
        PlanAction::Cancel { plan_id } => {
            let outcome = match plan_cli::cancel_plan(stores.plans.as_ref(), &plan_id).await {
                Err(PlanLookupError::NotFound { plan_id }) => {
                    anyhow::bail!("no plan {plan_id}, so there is nothing to cancel")
                }
                result => result?,
            };
            match outcome {
                CancelOutcome::Requested => println!(
                    "Cancellation recorded for {plan_id}.\n\
                     Its jobs stay claimable until the next `queue-maintenance` run \
                     retires them, so a client can still start one in that window.",
                ),
                CancelOutcome::AlreadyCancelling => println!(
                    "Plan {plan_id} is already cancelling; `queue-maintenance` still \
                     retires its jobs."
                ),
                CancelOutcome::AlreadyCancelled => {
                    println!("Plan {plan_id} is already cancelled; nothing to do.")
                }
                CancelOutcome::AlreadyComplete => anyhow::bail!(
                    "plan {plan_id} is already complete: every job reached a terminal state, \
                     so there is nothing to cancel"
                ),
            }
        }
    }
    Ok(())
}

fn print_plan_results(page: &pipette_plan_types::ResultListView) {
    println!(
        "{} of {} job(s) reported, {} row(s).",
        page.jobs_reported, page.jobs, page.total
    );
    if page.results.is_empty() {
        return;
    }
    println!(
        "Rows {}–{}:\n",
        page.offset + 1,
        page.offset + page.results.len()
    );
    let or_dash = |cell: &Option<String>| cell.clone().unwrap_or_else(|| "-".to_string());
    let json = |cell: Option<serde_json::Value>| {
        cell.map_or_else(
            || "-".to_string(),
            |value| pipette_plan_types::canonical_json(&value),
        )
    };
    let header = [
        "Job ID",
        "Benchmark",
        "Model Descriptor",
        "Runtime Descriptor",
        "Parameters",
        "Metric",
        "Value",
        "Client ID",
        "Scored",
    ]
    .map(String::from)
    .to_vec();
    let body = page.results.iter().map(|row| {
        let value = match (row.failed, row.value) {
            (Some(true), _) => {
                let reason = row.failed_reason.as_deref().unwrap_or("no reason");
                match row.failure_source {
                    Some(source) => format!("failed ({source}): {reason}"),
                    None => format!("failed: {reason}"),
                }
            }
            (_, Some(value)) => match row.value_stddev {
                Some(stddev) => format!("{value} ± {stddev} {}", row.unit),
                None => format!("{value} {}", row.unit),
            },
            (_, None) => "-".to_string(),
        };
        let parameters: Vec<String> = row
            .parameters
            .iter()
            .map(|(name, value)| format!("{name}={value}"))
            .collect();
        vec![
            row.job_id.to_string(),
            row.benchmark_id.clone(),
            json(
                row.model_descriptor
                    .as_ref()
                    .and_then(|d| serde_json::to_value(d).ok()),
            ),
            json(
                row.runtime_descriptor
                    .as_ref()
                    .and_then(|d| serde_json::to_value(d).ok()),
            ),
            parameters.join(", "),
            row.metric.clone(),
            value,
            or_dash(&row.client_id),
            row.scored_at
                .map_or_else(|| "-".to_string(), |at| at.to_rfc3339()),
        ]
    });
    let mut table = std::iter::once(header)
        .chain(body)
        .collect::<tabled::builder::Builder>()
        .build();
    table.with(tabled::settings::Style::psql());
    println!("{table}");
}

/// `starved_block` is false under `--explain`, which prints the same groups.
fn print_plan_status(record: plan_cli::PlanRecord, starved_block: bool) {
    let plan_cli::PlanRecord {
        manifest: view,
        notes,
        cancel_requested,
    } = record;
    println!("Plan      {}", view.plan_id);
    println!("Note      {}", notes.note.as_deref().unwrap_or("-"));
    if let Some(change) = &notes.change {
        println!("Change    {change}");
    }
    println!("Submitter {}", submitter_label(view.submitted_by.as_ref()));
    println!("Status    {}", view.status.label());
    println!("Created   {}", view.created_at.to_rfc3339());
    if let Some(job_expiry) = view.job_expiry {
        println!("Expires   {}", expiry_range(job_expiry, " → "));
    }
    if let Some(ended) = view.terminal_at {
        println!("Ended     {} ({})", ended.to_rfc3339(), view.status.label());
    }
    if cancel_requested {
        println!(
            "Cancel    requested. The status above latches to `cancelled` on the next \
             `queue-maintenance` run"
        );
    }

    match &view.progress_snapshot {
        None => {
            println!("\nProgress not yet computed: `queue-maintenance` has not run for this plan")
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
            if c.cancelled > 0 {
                println!("  {} job(s) retired by cancellation", c.cancelled);
            }
            if starved_block && !snapshot.starved.is_empty() {
                println!("\nStarved: outstanding jobs matching no working client:");
                for group in &snapshot.starved {
                    println!(
                        "  {} job(s) {}",
                        group.job_ids.len(),
                        describe_clauses(&group.clauses)
                    );
                }
            }
        }
    }

    if !view.warnings.is_empty() {
        // Frozen at ingestion, so labelled: they can disagree with `starved`.
        println!("\nWarnings at ingestion:");
        for warning in &view.warnings {
            println!("  {}", warning.message);
        }
    }

    if !view.plan_warnings.is_empty() {
        println!("\nPlanner warnings:");
        for warning in &view.plan_warnings {
            println!("  {warning}");
        }
    }
}

/// Two lists: a lapsed lease is a stranded job, not work in flight. No lease at
/// all is said outright, since silence would read as a missing flag.
fn print_plan_leases(leases: &PlanLeases) {
    if leases.is_empty() {
        if leases.unparseable_keys == 0 {
            println!("\nNo client holds a lease on this plan's jobs.");
        } else {
            println!("\nNo client holds a lease under a `leased/` key the report can read.");
        }
    }
    print_lease_list(
        "Running now. Clients that hold a live lease on one of the plan's jobs:",
        "lease expires",
        &leases.live,
        leases.read_at,
    );
    print_lease_list(
        "Stranded. Jobs behind a lapsed lease, until `queue-maintenance` recycles them:",
        "lease lapsed",
        &leases.lapsed,
        leases.read_at,
    );
    if leases.unparseable_keys > 0 {
        println!(
            "\n{} `leased/` key(s) name no job, so a lease on this plan's jobs may sit under one.",
            leases.unparseable_keys
        );
    }
}

/// Rows carry the deadline's age: the recycle cron is unreadable from here, so
/// the reader judges whether `56m ago` is a stalled sweep.
fn print_lease_list(
    headline: &str,
    deadline: &str,
    held: &[Lease],
    read_at: chrono::DateTime<chrono::Utc>,
) {
    if held.is_empty() {
        return;
    }
    println!("\n{headline}");
    let (client_width, job_width) = held.iter().fold((0, 0), |(client, job), lease| {
        (
            client.max(lease.client_id.as_ref().len()),
            job.max(lease.job_id.as_ref().len()),
        )
    });
    held.iter().for_each(|lease| {
        println!(
            "  {:<client_width$}  {:<job_width$}  {deadline} {} ({})",
            lease.client_id.as_ref(),
            lease.job_id.as_ref(),
            lease.lease_expiry.to_rfc3339(),
            lease_offset_label((lease.lease_expiry - read_at).num_seconds()),
        );
    });
}

/// The two largest units that carry a value, for example `21h 4m`.
fn duration_label(secs: i64) -> String {
    let (days, hours, minutes, seconds) = (
        secs / 86_400,
        (secs % 86_400) / 3_600,
        (secs % 3_600) / 60,
        secs % 60,
    );
    match (days, hours, minutes) {
        (0, 0, 0) => format!("{seconds}s"),
        (0, 0, _) => format!("{minutes}m {seconds}s"),
        (0, _, _) => format!("{hours}h {minutes}m"),
        _ => format!("{days}d {hours}h"),
    }
}

/// A deadline exactly at the instant reads backward, as [`PlanLeases`] lists it.
fn lease_offset_label(secs: i64) -> String {
    match secs <= 0 {
        true => format!("{} ago", duration_label(-secs)),
        false => format!("in {}", duration_label(secs)),
    }
}

/// A future stamp says so, rather than passing a bad clock off as fresh.
fn snapshot_age_label(secs: i64) -> String {
    match secs < 0 {
        true => format!("stamped {} in the future", duration_label(-secs)),
        false => format!("{} old", duration_label(secs)),
    }
}

/// Each ladder rung applies its clause and every rung above, so the first zero
/// names the blocking clause. `exclude_clients` leads: it pre-filters the fleet.
fn print_plan_explanation(report: &PlanExplanation) {
    println!("\nWhy these jobs are not scheduled");
    match report.snapshot_computed_at {
        // Aged against the queue read, not the moment of printing.
        Some(computed_at) => println!(
            "  Snapshot  {} ({})",
            computed_at.to_rfc3339(),
            snapshot_age_label(
                report
                    .read_at
                    .signed_duration_since(computed_at)
                    .num_seconds()
            )
        ),
        None => println!("  Snapshot  no `queue-maintenance` pass ran for this plan"),
    }
    println!("  SchedulingState   {}", report.headline(FleetDetail::Full));

    let c = &report.census;
    println!(
        "  Jobs      {} in the plan: {} available, {} leased, {} finished or retired",
        c.total,
        c.available,
        leased_cell(c),
        c.gone
    );
    println!(
        "            {} claimable ({} past their deadline); {} indexed; {} denied",
        c.claimable, c.expired, c.indexed, c.denied
    );
    if c.unparseable_avail_keys > 0 {
        println!(
            "            {} `avail/` key(s) in the queue are unparseable and count in no \
             bucket above",
            c.unparseable_avail_keys
        );
    }
    if c.unparseable_leased_keys > 0 {
        println!(
            "            {} `leased/` key(s) in the queue are unparseable and count in no \
             bucket above",
            c.unparseable_leased_keys
        );
    }
    if c.unparseable_avail_keys > 0 || c.unparseable_leased_keys > 0 {
        println!(
            "            A plan job under such a key counts in `finished or retired` above. \
             Treat that {} as an upper bound.",
            c.gone
        );
    }

    let f = &report.fleet;
    // The counts nest, so the first drop names the gate to act on.
    println!(
        "  Fleet     {} registered, {} approved, {} holding `perform_work`, {} suspended, \
         {} pending an eligible-index rebuild, {} unblocked",
        f.registered, f.approved, f.working, f.suspended, f.pending_reindex, f.unblocked
    );

    if report.groups.is_empty() {
        if report.verdict == SchedulingState::Starved {
            println!(
                "\nStarved groups: none cached. The verdict reads the warnings ingestion \
                 froze. The clause detail arrives with the first `queue-maintenance` pass."
            );
        }
        return;
    }
    println!("\nStarved groups: clause sets from the snapshot, client counts read live:");
    for group in &report.groups {
        println!("  {} job(s) {}", group.job_count, group.description);
        if let Some(rule) = &group.never_matches {
            println!("    No client can ever match this job: {rule}");
        }
        print_narrowing_ladder(&group.ladder, report.fleet.working);
        if let Some(roster) = &group.roster {
            println!(
                "    Roster: {} listed, {} registered, {} approved, {} holding \
                 `perform_work`, {} not excluded",
                roster.listed, roster.registered, roster.approved, roster.working, roster.eligible
            );
        }
        if group.live_working_client_count > 0 {
            println!(
                "    {} working client(s) match now. The next `queue-maintenance` run \
                 clears this group.",
                group.live_working_client_count
            );
        } else if group.live_approved_client_count > 0 {
            println!(
                "    Matches {} approved client(s), none holding `perform_work`, so the \
                 claim path refuses them. Grant it rather than edit the clauses: \
                 `pipette-mgmt clients permission add <client_id> perform_work`.",
                group.live_approved_client_count
            );
        } else if group.live_registered_client_count > 0 {
            println!(
                "    Matches {} registered client(s), none approved. Approve one rather \
                 than edit the clauses.",
                group.live_registered_client_count
            );
        }
    }
}

/// The first zero after a non-zero rung; row 0 is the working fleet. A fleet
/// empty before any clause blames nothing: the fix is an approval or a grant.
fn blamed_rung(rows: &[(&str, usize)]) -> Option<usize> {
    rows.iter()
        .position(|(_, surviving)| *surviving == 0)
        .filter(|index| {
            index
                .checked_sub(1)
                .and_then(|above| rows.get(above))
                .is_some_and(|(_, prior)| *prior > 0)
        })
}

fn print_narrowing_ladder(ladder: &[pipette_plan_types::NarrowingRung], working: usize) {
    const HEAD: &str = "working clients";
    if ladder.is_empty() {
        return;
    }
    let rows: Vec<(&str, usize)> = std::iter::once((HEAD, working))
        .chain(
            ladder
                .iter()
                .map(|rung| (rung.clause.as_str(), rung.surviving_client_count)),
        )
        .collect();
    let width = rows
        .iter()
        .map(|(clause, _)| clause.len())
        .max()
        .unwrap_or(HEAD.len());
    let blamed = blamed_rung(&rows);
    rows.iter()
        .enumerate()
        .for_each(|(index, (clause, surviving))| {
            let marker = match blamed == Some(index) {
                true => "  <- no client survives this clause",
                false => "",
            };
            println!("      {clause:<width$}  {surviving:>5}{marker}");
        });
}

fn client_preauth_row(
    k: &ClientPreauthKey,
    now: chrono::DateTime<chrono::Utc>,
) -> ClientPreauthRow {
    let usage = match k.usage {
        ClientPreauthUsage::SingleUse => "single-use",
        ClientPreauthUsage::MultiUse => "multi-use",
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
    let owner = k
        .default_owner
        .as_ref()
        .map_or_else(|| "-".to_string(), ClientOwner::label);
    let permissions = render_optional_permissions(k.default_permissions.as_ref());
    let status = if k.is_expired(now) {
        "expired"
    } else {
        "active"
    };
    let expires = k
        .expires_at
        .map_or_else(|| "never".to_string(), |e| e.to_rfc3339());
    ClientPreauthRow {
        key_id: format!("{}\n{}", k.key_id, k.created_at.to_rfc3339()),
        lifecycle: format!("{status}\n{expires}"),
        usage: usage.to_string(),
        seeds: format!("tags: {tags}\norg: {org}\nowner: {owner}\npermissions: {permissions}"),
        note: k.note.clone().unwrap_or_else(|| "-".to_string()),
    }
}

async fn handle_unverified_action(action: UnverifiedAction, stores: Stores) -> anyhow::Result<()> {
    let store = &*stores.submissions;
    match action {
        UnverifiedAction::Prune {
            older_than,
            dry_run,
        } => {
            let summary = store.prune_unverified(older_than, dry_run).await?;
            if !dry_run {
                tracing::info!(
                    deleted = summary.deleted,
                    kept = summary.kept,
                    older_than_secs = older_than.as_secs(),
                    "unverified archive pruned"
                );
            }
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
            if !dry_run {
                tracing::info!(%client_id, deleted, "held submissions deleted");
            }
            let prefix = if dry_run {
                "[dry-run] would delete"
            } else {
                "deleted"
            };
            println!("{prefix} {deleted} held submission(s) for client {client_id}");
        }
        UnverifiedAction::Promote { client_id, dry_run } => {
            let held = store.list_unverified_client(&client_id).await?;
            let stores = &stores;
            // Read avail -> leased -> record, the order a job moves, so a claim cannot hide it.
            let held = stream::iter(held)
                .then(|(job_id, body)| async move {
                    let avail = stores.todo.avail_key_for_job(&job_id).await?.is_some();
                    anyhow::Ok((avail, job_id, body))
                })
                .try_collect::<Vec<_>>()
                .await?;
            let leased = &stores
                .todo
                .list_leased()
                .await?
                .iter()
                .filter_map(|key| parse_leased_key(key).ok())
                .map(|lease| lease.job_id)
                .collect::<HashSet<_>>();
            let (kept, free): (Vec<_>, Vec<_>) = stream::iter(held)
                .then(|(avail, job_id, body)| async move {
                    let exists = avail || held_job_exists(stores, leased, &job_id).await?;
                    anyhow::Ok((exists, job_id, body))
                })
                .try_collect::<Vec<_>>()
                .await?
                .into_iter()
                .partition(|(exists, _, _)| *exists);
            if dry_run {
                println!(
                    "[dry-run] would promote {} held submission(s) for client {client_id}; \
                     would keep {} whose job already exists",
                    free.len(),
                    kept.len()
                );
                return Ok(());
            }
            kept.iter().for_each(|(_, job_id, _)| {
                tracing::warn!(%client_id, %job_id, "promote kept a held submission whose job exists");
            });
            let client_id = &client_id;
            let promotions = stream::iter(free)
                .then(|(_, job_id, body)| async move {
                    let promotion = promote_held(store, client_id, &job_id, body).await?;
                    if promotion != HeldPromotion::Refused {
                        // Delete only after the write succeeds, so a crash keeps the copy.
                        store.delete_unverified(client_id, &job_id).await?;
                    }
                    anyhow::Ok(promotion)
                })
                .try_collect::<Vec<_>>()
                .await?;
            let count = |want| promotions.iter().filter(|p| **p == want).count();
            let promoted = count(HeldPromotion::Recorded);
            let archived = count(HeldPromotion::Archived);
            let refused = kept.len() + count(HeldPromotion::Refused);
            tracing::info!(%client_id, promoted, archived, refused, "held submissions promoted");
            println!(
                "promoted {promoted} held submission(s) for client {client_id}; archived \
                 {archived} retriable failure(s) unrecorded; kept {refused} whose job already \
                 exists"
            );
        }
    }
    Ok(())
}

#[derive(Debug, PartialEq, Eq)]
enum HeldPromotion {
    Recorded,
    Archived,
    /// The job already exists; the held copy stays for the operator.
    Refused,
}

/// A held body's job_id was never checked against a lease, so promote must not
/// let it stand in for a job the server queued or already recorded.
async fn held_job_exists(
    stores: &Stores,
    leased: &HashSet<JobId>,
    job_id: &JobId,
) -> anyhow::Result<bool> {
    // A job queued before job sources existed has none, so todo/ is read too.
    Ok(leased.contains(job_id)
        || stores.submissions.has_record(job_id).await?
        || stores.plans.get_job_source(job_id).await?.is_some())
}

/// The caller has checked [`held_job_exists`].
async fn promote_held(
    store: &dyn SubmissionStore,
    client_id: &ClientId,
    job_id: &JobId,
    mut body: serde_json::Value,
) -> anyhow::Result<HeldPromotion> {
    let submission = pipette_pipeline::submission::parse_stored_submission(&body)
        .map_err(|e| anyhow::anyhow!("malformed held submission {job_id}: {e}"))?;
    // A retriable failure is the device's fault, never recorded as a result
    // live; re-staging it now would record a terminal failure for a cell that
    // may have succeeded elsewhere, so archive it.
    let retriable_failure = matches!(
        &submission,
        pipette_pipeline::submission::Submission::Failure(f) if f.wire.retriable
    );
    pipette_pipeline::submission::move_runtime_output(store, job_id, client_id, &mut body).await?;
    if retriable_failure {
        store.write_processed(job_id, &body).await?;
        tracing::info!(%client_id, %job_id, "promote archived a held retriable failure unrecorded");
        return Ok(HeldPromotion::Archived);
    }
    match store.write_incoming_new(job_id, &body).await? {
        CreateOutcome::Created => {
            tracing::info!(%client_id, %job_id, "promote wrote a held submission to incoming");
            Ok(HeldPromotion::Recorded)
        }
        // shortcut: the moved output stays listed for this job; delete it once a store can.
        CreateOutcome::AlreadyExists => {
            tracing::warn!(%client_id, %job_id, "promote kept a held submission whose job has a record");
            Ok(HeldPromotion::Refused)
        }
    }
}

/// `<number><unit>` with `s`/`m`/`h`/`d`, e.g. `7d`.
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

fn parse_catalog_name(s: &str) -> Result<CatalogName, String> {
    CatalogName::try_new(s.to_owned()).map_err(|error| error.to_string())
}

fn parse_catalog_runtime_type(s: &str) -> Result<RuntimeType, String> {
    serde_json::from_value(serde_json::Value::String(s.to_owned()))
        .map_err(|error| error.to_string())
}

fn parse_benchmark_id(s: &str) -> Result<BenchmarkId, String> {
    BenchmarkId::try_new(s).map_err(|e| e.to_string())
}

fn parse_job_id(s: &str) -> Result<JobId, String> {
    JobId::try_new(s).map_err(|e| e.to_string())
}

/// No output at all is an error, so a mistyped id does not read as a silent
/// run. An uncaptured stream has no header; an empty one, a header alone.
async fn runtime_output_text(
    store: &dyn SubmissionStore,
    job_id: &JobId,
) -> anyhow::Result<String> {
    let mut outputs = store.list_runtime_output(job_id).await?;
    anyhow::ensure!(!outputs.is_empty(), "no runtime output for job {job_id}");
    outputs.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(outputs
        .into_iter()
        .flat_map(|(client_id, output)| {
            [
                ("runtime_stdout", output.runtime_stdout),
                ("runtime_stderr", output.runtime_stderr),
            ]
            .into_iter()
            .filter_map(move |(name, stream)| {
                let stream = stream?;
                let end = if stream.is_empty() || stream.ends_with('\n') {
                    ""
                } else {
                    "\n"
                };
                Some(format!("== {client_id} {name} ==\n{stream}{end}"))
            })
        })
        .collect())
}

fn parse_client_id(s: &str) -> Result<ClientId, String> {
    ClientId::try_new(s).map_err(|e| e.to_string())
}

fn parse_user_id(s: &str) -> Result<UserId, String> {
    UserId::try_new(s).map_err(|e| e.to_string())
}

/// Folds case: the address keys the account's index entry, and `release-claim`
/// reaches that claim without building a `User` (whose `try_new` folds too).
fn parse_contact_email(s: &str) -> Result<ContactEmail, String> {
    ContactEmail::try_new(s.to_lowercase()).map_err(|e| e.to_string())
}

/// [`OrgSlug`] folds case and trims: the handle keys the organization's index
/// entry, and one handle must be one key.
fn parse_org_slug(s: &str) -> Result<OrgSlug, String> {
    OrgSlug::try_new(s).map_err(|e| e.to_string())
}

/// A handle or a minted id; [`OrgRef`] tells them apart by the `org_` prefix.
fn parse_org_ref(s: &str) -> Result<OrgRef, String> {
    s.parse::<OrgRef>().map_err(|e| e.to_string())
}

fn parse_client_preauth_key_id(s: &str) -> Result<ClientPreauthKeyId, String> {
    ClientPreauthKeyId::try_new(s).map_err(|e| e.to_string())
}

fn parse_user_preauth_key_id(s: &str) -> Result<UserPreauthKeyId, String> {
    UserPreauthKeyId::try_new(s).map_err(|e| e.to_string())
}

fn parse_user_key_id(s: &str) -> Result<UserKeyId, String> {
    UserKeyId::try_new(s).map_err(|e| e.to_string())
}

/// A user permission is refused as the wrong kind, not as unknown; plain
/// `FromStr` would give strum's "Matching variant not found".
fn parse_client_permission(s: &str) -> Result<ClientPermission, String> {
    if let Ok(permission) = ClientPermission::from_str(s) {
        return Ok(permission);
    }
    let client_names = ClientPermission::VARIANTS.join(", ");
    if UserPermission::from_str(s).is_ok() {
        return Err(format!(
            "{s:?} is a user permission; a client cannot hold it. One of: {client_names}"
        ));
    }
    Err(format!("unknown permission {s:?}; one of: {client_names}"))
}

/// Either vocabulary, as a name: an object written before the partition may hold
/// a user name, and removal is how it is cleared. A typo is still refused.
fn parse_removable_permission(s: &str) -> Result<String, String> {
    if ClientPermission::from_str(s).is_ok() || UserPermission::from_str(s).is_ok() {
        return Ok(s.to_string());
    }
    Err(format!(
        "unknown permission {s:?}; one of: {}, {}",
        ClientPermission::VARIANTS.join(", "),
        UserPermission::VARIANTS.join(", ")
    ))
}

fn parse_flag_list(s: &str) -> Result<FlagList, String> {
    pipette_pipeline::client_cli::flag_list(s)
        .map(FlagList)
        .map_err(|e| e.to_string())
}

/// Typed at the argument boundary, so a path-traversing value never reaches the store.
fn parse_plan_id(s: &str) -> Result<PlanId, String> {
    PlanId::try_new(s).map_err(|e| e.to_string())
}

fn parse_campaign_id(s: &str) -> Result<CampaignId, String> {
    CampaignId::try_new(s).map_err(|e| e.to_string())
}

fn parse_vault_resource_id(s: &str) -> Result<VaultResourceId, String> {
    VaultResourceId::try_new(s).map_err(|e| e.to_string())
}

fn parse_submitter_id(s: &str) -> Result<UserId, String> {
    UserId::try_new(s).map_err(|e| e.to_string())
}

/// Micros since the epoch, as the warehouse's `submitted_at`; a bare date is
/// midnight UTC.
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

/// A panic skips `release`; the lease then expires on its own (`storage_lock`).
async fn run_locked<T>(
    storage: &StorageConfig,
    probe_capabilities: bool,
    lock_name: &str,
    holder: &str,
    ttl: std::time::Duration,
    fut: impl std::future::Future<Output = anyhow::Result<T>>,
) -> anyhow::Result<T> {
    // Verified here, before the lock it would silently fail to make exclusive.
    conditional_put::validate(
        probe_capabilities,
        storage,
        "[storage]",
        "the storage mutate lock cannot be exclusive, so two commands can both believe they hold \
         it and silently drop a row",
        conditional_put::Requires::CreateAndSwap,
    )
    .await?;
    let lock = StorageLock::acquire_named(storage, lock_name, holder, ttl).await?;
    let result = fut.await;
    lock.release().await;
    result
}

/// Ingestion's create-if-absent and compare-and-swap: without them a cell gets a
/// duplicate job, or a live job is orphaned from the index.
async fn validate_plan_index_backend(config: &Config) -> anyhow::Result<()> {
    conditional_put::validate(
        config.probe_backend_capabilities,
        &config.storage,
        "[storage]",
        "the duplicate-job merge is best effort, so two plans wanting one benchmark cell can \
         each queue their own job",
        conditional_put::Requires::CreateAndSwap,
    )
    .await
}

/// The longest time `serve` waits at shutdown for the audit writes that
/// outlived their answers.
const AUDIT_DRAIN: std::time::Duration = std::time::Duration::from_secs(10);

/// The longest time `serve` waits at shutdown for the requests in flight.
const REQUEST_DRAIN: std::time::Duration = std::time::Duration::from_secs(20);

/// `SIGTERM` (a container runtime's stop) or `SIGINT`, so in-flight requests
/// finish instead of being cut off mid-write.
async fn shutdown_signal() {
    let interrupt = async {
        if let Err(e) = tokio::signal::ctrl_c().await {
            tracing::warn!(error = %e, "cannot listen for SIGINT");
            std::future::pending::<()>().await;
        }
    };
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut signal) => {
                signal.recv().await;
            }
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

async fn run_db(action: DbAction, config: &Config) -> anyhow::Result<()> {
    let Some(database_config) = &config.database else {
        anyhow::bail!("the config has no [database] section");
    };
    match action {
        DbAction::Migrate { to, dry_run } => {
            let moved = database::migrate(database_config, to, dry_run).await?;
            let name = |v: Option<i64>| v.map_or_else(|| "none".into(), |v| v.to_string());
            // Named so a stale binary's no-op reads as wrong.
            let latest = name(database::latest_migration());
            let mut steps = Vec::new();
            if !moved.reverted.is_empty() {
                steps.push(format!("revert {:?}", moved.reverted));
            }
            if !moved.applied.is_empty() {
                steps.push(format!("apply {:?}", moved.applied));
            }
            let (from, to) = (name(moved.from), name(moved.to));
            let verb = if dry_run { "would move" } else { "moved" };
            if steps.is_empty() {
                println!(
                    "the schema stays at migration {from} (this binary's latest migration: {latest})"
                );
            } else {
                println!(
                    "{verb} the schema from migration {from} to {to}: {} (this binary's latest migration: {latest})",
                    steps.join(", then ")
                );
            }
        }
        DbAction::Check => {
            database::check(config).await?;
            println!("the database answers and its schema is current");
        }
        DbAction::Import { dry_run } => {
            copy_db(config, record_copy::CopyMode::Merge, dry_run).await?;
        }
        DbAction::Export { dry_run } => {
            copy_db(config, record_copy::CopyMode::Mirror, dry_run).await?;
        }
        DbAction::BackfillJobCells { dry_run } => {
            let report = database::backfill_job_cells(
                config,
                &pipette_pipeline::content_key::cell_digest,
                dry_run,
            )
            .await?;
            let verb = if dry_run { "would write" } else { "wrote" };
            println!(
                "{verb} {} job cell(s) and {} submission outcome row(s)",
                report.cells_written, report.outcomes_written
            );
            println!("{} job(s) have no submission record", report.without_record);
            println!(
                "{} job source(s) and {} record(s) do not parse",
                report.unreadable_sources, report.unreadable_records
            );
        }
    }
    Ok(())
}

/// A merge copies the object stores into the database (`db import`). A mirror
/// copies the database to the object stores (`db export`).
async fn copy_db(
    config: &Config,
    mode: record_copy::CopyMode,
    dry_run: bool,
) -> anyhow::Result<()> {
    database::check(config).await?;
    let database = build_stores(config).await?;
    let objects = build_object_stores(config).await?;
    let (from, to) = match mode {
        record_copy::CopyMode::Merge => (&objects, &database),
        record_copy::CopyMode::Mirror => (&database, &objects),
    };
    let report = record_copy::copy_records(from, to, mode, dry_run).await?;
    finish_copy(&report)
}

fn finish_copy(report: &record_copy::CopyReport) -> anyhow::Result<()> {
    print_copy_report(report);
    tracing::info!(
        mode = ?report.mode,
        dry_run = report.dry_run,
        written = report.written(),
        deleted = report.deleted(),
        conflicts = report.conflicts(),
        unreadable = report.unreadable(),
        "record copy finished"
    );
    let (conflicts, unreadable) = (report.conflicts(), report.unreadable());
    anyhow::ensure!(
        conflicts == 0 && unreadable == 0,
        "{conflicts} conflict(s) and {unreadable} unreadable record(s) were not copied; \
         resolve them and run again"
    );
    Ok(())
}

#[derive(tabled::Tabled)]
struct ImportRow {
    #[tabled(rename = "Keyspace")]
    keyspace: &'static str,
    #[tabled(rename = "Read")]
    read: usize,
    #[tabled(rename = "Written")]
    written: usize,
    #[tabled(rename = "Unchanged")]
    unchanged: usize,
    #[tabled(rename = "Deleted")]
    deleted: usize,
    #[tabled(rename = "Skipped")]
    skipped: usize,
    #[tabled(rename = "Unreadable")]
    unreadable: usize,
    #[tabled(rename = "Conflicts")]
    conflicts: usize,
}

fn print_copy_report(report: &record_copy::CopyReport) {
    let rows: Vec<ImportRow> = report
        .keyspaces
        .iter()
        .map(|k| ImportRow {
            keyspace: k.keyspace,
            read: k.read,
            written: k.written,
            unchanged: k.unchanged,
            deleted: k.deleted,
            skipped: k.skipped.len(),
            unreadable: k.unreadable.len(),
            conflicts: k.conflicts.len(),
        })
        .collect();
    let mut table = tabled::Table::new(&rows);
    table.with(tabled::settings::Style::psql());
    if report.dry_run {
        println!("Dry run: nothing changed; Written and Deleted count what would be.");
    }
    println!("{table}");
    report.keyspaces.iter().for_each(|k| {
        let reasons = k.skipped.iter().fold(
            std::collections::BTreeMap::<&str, usize>::new(),
            |mut reasons, note| {
                *reasons.entry(note.reason.as_str()).or_default() += 1;
                reasons
            },
        );
        reasons
            .iter()
            .for_each(|(reason, count)| println!("{}: {count} skipped: {reason}", k.keyspace));
        k.unreadable
            .iter()
            .for_each(|note| println!("{}: unreadable {}: {}", k.keyspace, note.id, note.reason));
        k.conflicts
            .iter()
            .for_each(|note| println!("{}: conflict {}: {}", k.keyspace, note.id, note.reason));
    });
}

async fn serve(config: Config, stores: Stores) -> anyhow::Result<()> {
    stores.todo.validate_backend().await?;

    // An unmigrated `avail/` reads as empty, and the merge pass would replace
    // its jobs as retired.
    pipette_store::ensure_avail_layout_migrated(&*stores.todo).await?;

    // Create only: the auth store's compare-and-swap repair is best-effort, at
    // the cost that a repair can overwrite a concurrent update.
    conditional_put::validate(
        config.probe_backend_capabilities,
        &config.auth_storage,
        "[auth_storage]",
        "a single-use pre-auth key can register more than one client",
        conditional_put::Requires::Create,
    )
    .await?;

    validate_plan_index_backend(&config).await?;

    database::check(&config).await?;
    if stores.audit.is_none() {
        tracing::warn!("no write is audited: the audit log is kept only on a [database]");
    }

    let http_client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(config.http_timeout_secs))
        .connect_timeout(std::time::Duration::from_secs(
            config.http_connect_timeout_secs.get(),
        ))
        .build()?;
    let state = AppState::new(config.clone(), stores, http_client).await?;

    let audit_writes = state.audit_writes.clone();
    let app = pipette_httpapi::app(state);

    // Once at startup, so the weaker mode is visible even when no client uses it.
    if config.accept_legacy_signatures {
        tracing::warn!(
            "accepting timestamp-only signatures; clear accept_legacy_signatures once the \
             \"accepted timestamp-only signature\" warnings stop"
        );
    }
    if config.authorities.is_empty() {
        tracing::warn!("authorities is empty, so every RFC 9421 and v3 signature is refused");
    }

    let listener = tokio::net::TcpListener::bind(&config.listen_addr).await?;
    tracing::info!(listen_addr = %config.listen_addr, "listening");
    pipette_httpapi::serve(
        listener,
        app,
        shutdown_signal(),
        pipette_httpapi::Limits {
            read: std::time::Duration::from_secs(config.read_timeout_secs.get()),
            drain: REQUEST_DRAIN,
            connections: config.max_connections,
        },
    )
    .await;
    // Requests past the drain cap still run, and die with the runtime; audit
    // writes that outlived their answers get AUDIT_DRAIN.
    audit_writes.close();
    if tokio::time::timeout(AUDIT_DRAIN, audit_writes.wait())
        .await
        .is_err()
    {
        tracing::warn!(
            abandoned = audit_writes.len(),
            "audit writes still running at shutdown; their events are lost"
        );
    }
    tracing::info!("server stopped");

    Ok(())
}

/// Tags, permissions, owner and migration markers are off the record, fetched
/// only when `--columns` names them.
struct ClientRow {
    client_id: String,
    organization: String,
    owner: String,
    details: String,
    contact: String,
    status: String,
    registered: String,
    tags: String,
    permissions: String,
    migrated: String,
    derived: String,
    declared: String,
}

impl ClientRow {
    fn from_client(
        c: &pipette_core::client::Client,
        owner: Option<&ClientOwner>,
        tags: Option<&std::collections::BTreeSet<pipette_plan_types::Tag>>,
        permissions: Option<&ClientPermissionSet>,
        migrated: Option<chrono::DateTime<chrono::Utc>>,
    ) -> Self {
        let flags = |flags: &std::collections::BTreeSet<String>| {
            if flags.is_empty() {
                "-".to_string()
            } else {
                flags.iter().cloned().collect::<Vec<_>>().join("\n")
            }
        };
        Self {
            client_id: c.client_id.to_string(),
            organization: c.organization.to_string(),
            owner: owner.map_or_else(|| "-".to_string(), ClientOwner::label),
            details: pipette_core::client::truncate_details(&c.client_details),
            contact: c.contact_email.to_string(),
            status: c.status.to_string(),
            registered: c.registered_at.to_string(),
            tags: tags.map_or_else(|| "-".to_string(), join_tags),
            // `-` is no object, `(none)` a cleared set: both grant nothing, but
            // only the first still needs granting by hand.
            permissions: permissions.map_or_else(
                || "-".to_string(),
                |held| {
                    if held.is_empty() {
                        "(none)".to_string()
                    } else {
                        held.iter()
                            .map(|p| p.to_string())
                            .collect::<Vec<_>>()
                            .join("\n")
                    }
                },
            ),
            migrated: migrated.map_or_else(|| "—".to_string(), |at| at.to_string()),
            derived: flags(&normalized_flags(&c.device_profile)),
            declared: flags(&c.capabilities),
        }
    }
}

#[derive(tabled::Tabled)]
struct CapabilityRow {
    #[tabled(rename = "Namespace")]
    namespace: String,
    #[tabled(rename = "Source")]
    source: &'static str,
    #[tabled(rename = "Flag")]
    flag: String,
    #[tabled(rename = "Clients")]
    clients: usize,
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

fn join_tags(tags: &std::collections::BTreeSet<pipette_plan_types::Tag>) -> String {
    if tags.is_empty() {
        "-".to_string()
    } else {
        tags.iter()
            .map(|t| t.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    }
}

fn capability_query(
    requires: Vec<FlagList>,
    any_of: Vec<FlagList>,
    excludes: Vec<FlagList>,
    not_all_of: Vec<FlagList>,
) -> CapabilityQuery {
    let flat = |lists: Vec<FlagList>| lists.into_iter().flat_map(|list| list.0).collect();
    let grouped = |lists: Vec<FlagList>| lists.into_iter().map(|list| list.0).collect();
    CapabilityQuery {
        requires: flat(requires),
        any_of: grouped(any_of),
        excludes: flat(excludes),
        not_all_of: grouped(not_all_of),
    }
}

/// A capability filter adds the two flag columns.
fn default_client_columns(has_capability_filter: bool) -> Vec<ClientColumn> {
    let mut columns = vec![
        ClientColumn::ClientId,
        ClientColumn::Organization,
        ClientColumn::Details,
        ClientColumn::Contact,
        ClientColumn::Status,
        ClientColumn::Registered,
        ClientColumn::Tags,
        ClientColumn::Migrated,
    ];
    if has_capability_filter {
        columns.extend([ClientColumn::Derived, ClientColumn::Declared]);
    }
    columns
}

async fn handle_client_action(action: ClientAction, stores: Stores) -> anyhow::Result<()> {
    let client_store = stores.clients.clone();
    let tag_store = stores.tags.clone();

    match action {
        ClientAction::List {
            tags,
            requires,
            any_of,
            excludes,
            not_all_of,
            columns,
        } => {
            let query = capability_query(requires, any_of, excludes, not_all_of);
            let has_capability_filter = query.is_present();
            if query.matches_nobody() {
                // A warning, not a parse error: the matcher rule, not the syntax,
                // makes this query unsatisfiable.
                eprintln!(
                    "Capability filter without --requires matches nobody: every other clause only narrows, so --requires needs at least one flag."
                );
            }
            let tags: Vec<Tag> = tags
                .iter()
                .cloned()
                .map(Tag::try_new)
                .collect::<Result<_, _>>()
                .map_err(|e| anyhow::anyhow!("--tag: {e}"))?;
            let clients = client_cli::select_clients(
                client_store.as_ref(),
                tag_store.as_ref(),
                &tags,
                &query,
            )
            .await?;
            let columns = if columns.is_empty() {
                default_client_columns(has_capability_filter)
            } else {
                columns
            };

            if clients.is_empty() {
                match (tags.is_empty(), has_capability_filter) {
                    (true, false) => println!("No clients registered."),
                    (false, false) => println!("No clients match the given tag filter."),
                    (true, true) => {
                        println!("No clients match the given capability filter.")
                    }
                    (false, true) => println!("No clients match the given filters."),
                }
            } else {
                println!("{} client(s):\n", clients.len());
                let migrated: std::collections::HashMap<_, _> =
                    if columns.contains(&ClientColumn::Migrated) {
                        client_store
                            .list_signature_migrations()
                            .await?
                            .into_iter()
                            .map(|(id, record)| (id, record.first_seen))
                            .collect()
                    } else {
                        std::collections::HashMap::new()
                    };
                let tags_selected = columns.contains(&ClientColumn::Tags);
                let permissions_selected = columns.contains(&ClientColumn::Permissions);
                // `buffered` keeps sorted order. A client absent from the map
                // grants nothing; no default fills it in.
                let granted: std::collections::HashMap<_, _> = if permissions_selected {
                    client_store
                        .list_client_permissions()
                        .await?
                        .into_iter()
                        .collect()
                } else {
                    std::collections::HashMap::new()
                };
                let owners: std::collections::HashMap<_, _> =
                    if columns.contains(&ClientColumn::Owner) {
                        stores
                            .orgs
                            .list_ownerships()
                            .await?
                            .into_iter()
                            .map(|ownership| (ownership.client_id, ownership.owner))
                            .collect()
                    } else {
                        std::collections::HashMap::new()
                    };
                let rows: Vec<ClientRow> = stream::iter(clients.iter().map(|c| {
                    let tag_store = tag_store.clone();
                    let migrated = &migrated;
                    let granted = &granted;
                    let owners = &owners;
                    async move {
                        let tags = if tags_selected {
                            Some(tag_store.get_client_tags(&c.client_id).await?)
                        } else {
                            None
                        };
                        let permissions = permissions_selected.then(|| {
                            pipette_core::client_permission::effective(
                                granted.get(&c.client_id).cloned(),
                            )
                        });
                        anyhow::Ok(ClientRow::from_client(
                            c,
                            owners.get(&c.client_id),
                            tags.as_ref(),
                            permissions.as_ref(),
                            migrated.get(&c.client_id).copied(),
                        ))
                    }
                }))
                .buffered(STORAGE_CONCURRENCY)
                .try_collect()
                .await?;
                println!("{}", column_table(&columns, &rows));
            }
        }
        ClientAction::Capabilities { status } => {
            let vocabulary = client_cli::capability_vocabulary(
                client_store.as_ref(),
                tag_store.as_ref(),
                status,
            )
            .await?;
            let rows: Vec<CapabilityRow> = vocabulary
                .namespaces
                .iter()
                .flat_map(|namespace| {
                    namespace.flags.iter().map(|flag| CapabilityRow {
                        namespace: namespace.namespace.clone(),
                        source: namespace.source.into(),
                        flag: flag.flag.clone(),
                        clients: flag.clients,
                    })
                })
                .chain(vocabulary.free_form.iter().map(|flag| CapabilityRow {
                    // Outside every namespace, so outside every reserved one too.
                    namespace: "-".to_string(),
                    source: CapabilitySource::Declared.into(),
                    flag: flag.flag.clone(),
                    clients: flag.clients,
                }))
                .collect();

            if rows.is_empty() {
                println!(
                    "No capability flags across {} client(s).",
                    vocabulary.clients
                );
            } else {
                println!(
                    "{} flag(s) across {} client(s):\n",
                    rows.len(),
                    vocabulary.clients
                );
                let mut table = tabled::Table::new(&rows);
                table.with(tabled::settings::Style::psql());
                println!("{table}");
            }
            if vocabulary.unreadable > 0 {
                println!(
                    "\nThe scan could not read {} client record(s). This vocabulary excludes them.",
                    vocabulary.unreadable
                );
            }
        }
        ClientAction::Tag { action } => {
            handle_tag_action(action, client_store, tag_store, stores.todo.clone()).await?;
        }
        ClientAction::Permission { action } => {
            handle_permission_action(action, client_store, tag_store).await?;
        }
        ClientAction::Owner { action } => {
            handle_client_owner_action(action, &stores).await?;
        }
        ClientAction::Preauth { action } => {
            handle_client_preauth_action(action, &stores).await?;
        }
        ClientAction::Approve {
            client_id,
            permissions,
        } => {
            let granted: std::collections::BTreeSet<ClientPermission> =
                permissions.into_iter().collect();
            match client_admin::set_status(&*client_store, &client_id, ClientStatus::Approved)
                .await?
            {
                client_admin::Applied::Unchanged => {
                    eprintln!("Client {client_id} is already approved.")
                }
                client_admin::Applied::Changed((_, change)) => println!(
                    "Approved client {client_id} ({} -> {}).",
                    change.from, change.to
                ),
            }
            // An already-approved client still takes the grant: confirming an
            // approval is when an operator most often reaches for the flag.
            if !granted.is_empty() {
                grant_permissions(&client_store, &client_id, &granted).await?;
            }
        }
        ClientAction::Reject { client_id } => {
            client_admin::reject(
                &*client_store,
                &*stores.key_index,
                &*stores.orgs,
                &*stores.tags,
                &client_id,
            )
            .await?;
            println!("Rejected and deleted client {client_id}.");
        }
        ClientAction::Delete { client_id } => {
            // Idempotent so a re-run finishes a partial delete; the "no record
            // found" line is the only sign of a mistyped id.
            let existed = client_admin::delete(
                &*client_store,
                &*stores.key_index,
                &*stores.orgs,
                &*stores.tags,
                &*stores.todo,
                &client_id,
            )
            .await?;
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
            let update = client_admin::ClientUpdate {
                organization: organization
                    .map(pipette_plan_types::NonEmptyTrimmedString::try_new)
                    .transpose()
                    .map_err(|e| anyhow::anyhow!("--organization: {e}"))?,
                client_details: client_details
                    .map(pipette_plan_types::NonEmptyTrimmedString::try_new)
                    .transpose()
                    .map_err(|e| anyhow::anyhow!("--details: {e}"))?,
                contact_email: contact_email
                    .map(pipette_plan_types::ContactEmail::try_new)
                    .transpose()
                    .map_err(|e| anyhow::anyhow!("--email: {e}"))?,
            };
            if update.is_empty() {
                anyhow::bail!(
                    "nothing to update: pass at least one of --organization, --details, --email"
                );
            }
            match client_admin::update(&*client_store, &client_id, update).await? {
                client_admin::Applied::Unchanged => {
                    println!("Client {client_id}: no changes (provided values match current).");
                }
                client_admin::Applied::Changed((_, changes)) => {
                    println!("Updated client {client_id}:");
                    changes.iter().for_each(|line| println!("  {line}"));
                }
            }
        }
    }
    Ok(())
}

/// For `permission add`, whose mixed positionals cannot carry a clap
/// `value_parser`; every other path parses through [`parse_client_permission`].
fn parse_permissions(
    raw: Vec<String>,
    flag: &str,
) -> anyhow::Result<std::collections::BTreeSet<ClientPermission>> {
    let (parsed, refused): (Vec<_>, Vec<_>) = raw
        .into_iter()
        .map(|name| parse_client_permission(&name))
        .partition(Result::is_ok);
    // Every offending name, not just the first, so one command is corrected once.
    if !refused.is_empty() {
        anyhow::bail!(
            "{flag}: {}",
            refused
                .into_iter()
                .filter_map(Result::err)
                .collect::<Vec<_>>()
                .join("; ")
        );
    }
    Ok(parsed.into_iter().filter_map(Result::ok).collect())
}

/// Permission sets and tag markers live off the record, so a write against an
/// unknown id would create state no client resolves.
async fn require_client(
    client_store: &Arc<dyn ClientStore>,
    client_id: &ClientId,
) -> anyhow::Result<()> {
    client_store
        .get_client(client_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("client {client_id} not found"))?;
    Ok(())
}

/// A list for a structured log field, not `render_permissions`' line: `(none)`
/// there could be a permission's name.
fn log_names<'a, S: AsRef<str> + 'a>(names: impl IntoIterator<Item = &'a S>) -> Vec<&'a str> {
    names.into_iter().map(AsRef::as_ref).collect()
}

async fn grant_permissions(
    client_store: &Arc<dyn ClientStore>,
    client_id: &ClientId,
    permissions: &std::collections::BTreeSet<ClientPermission>,
) -> anyhow::Result<()> {
    match client_admin::grant_permissions(client_store.as_ref(), client_id, permissions).await? {
        client_admin::Applied::Changed(resulting) => println!(
            "Client {client_id}: granted {}; now holds {}.",
            render_permissions(permissions.iter()),
            render_permissions(resulting.iter())
        ),
        client_admin::Applied::Unchanged => {
            println!("Client {client_id}: no new permissions (all already held).")
        }
    }
    Ok(())
}

async fn handle_permission_action(
    action: PermissionAction,
    client_store: Arc<dyn ClientStore>,
    tag_store: Arc<dyn TagStore>,
) -> anyhow::Result<()> {
    enum GrantTarget {
        Client(ClientId),
        Tag(Tag),
    }

    match action {
        PermissionAction::Add { tag, args } => {
            let (target, names) = match tag {
                Some(tag) => (
                    GrantTarget::Tag(Tag::try_new(tag).map_err(|e| anyhow::anyhow!("--tag: {e}"))?),
                    args,
                ),
                None => {
                    let mut args = args.into_iter();
                    let id = args
                        .next()
                        .ok_or_else(|| anyhow::anyhow!("name a client id, or pass --tag"))?;
                    (GrantTarget::Client(ClientId::try_new(id)?), args.collect())
                }
            };
            let permissions = parse_permissions(names, "permission add")?;
            if permissions.is_empty() {
                anyhow::bail!("name at least one permission to grant");
            }
            match target {
                GrantTarget::Client(client_id) => {
                    require_client(&client_store, &client_id).await?;
                    grant_permissions(&client_store, &client_id, &permissions).await?;
                }
                GrantTarget::Tag(tag) => {
                    let client_ids = tag_store.list_client_ids_by_tag(&tag).await?;
                    if client_ids.is_empty() {
                        println!("No clients carry the tag {tag}.");
                        return Ok(());
                    }
                    // A failure does not abort the rest: the operator learns
                    // which clients missed, and a re-run converges.
                    let failures: Vec<(ClientId, anyhow::Error)> =
                        stream::iter(client_ids.iter().map(|client_id| {
                            let client_store = client_store.clone();
                            let permissions = permissions.clone();
                            async move {
                                grant_permissions(&client_store, client_id, &permissions)
                                    .await
                                    .err()
                                    .map(|e| (client_id.clone(), e))
                            }
                        }))
                        .buffer_unordered(STORAGE_CONCURRENCY)
                        .filter_map(|failure| async move { failure })
                        .collect()
                        .await;
                    failures.iter().for_each(|(client_id, error)| {
                        tracing::error!(
                            client_id = %client_id,
                            error = %error,
                            "failed to grant permissions to a tagged client"
                        );
                        eprintln!("  ! {client_id}: {error}");
                    });
                    if !failures.is_empty() {
                        anyhow::bail!(
                            "{} of {} client(s) carrying {tag} did not take the grant; re-run to converge",
                            failures.len(),
                            client_ids.len()
                        );
                    }
                }
            }
        }
        PermissionAction::Remove {
            client_id,
            permissions,
        } => {
            require_client(&client_store, &client_id).await?;
            let revoked = permissions.clone();
            let mutation = client_store
                .mutate_client_permissions(&client_id, &move |set| set.remove_all(revoked.iter()))
                .await?;
            if mutation.changed {
                tracing::info!(
                    client_id = %client_id,
                    removed = ?log_names(permissions.iter()),
                    resulting = ?mutation.resulting.names(),
                    "client permissions revoked"
                );
                println!(
                    "Client {client_id}: revoked {}; now holds {}.",
                    render_permissions(permissions.iter()),
                    render_permissions(mutation.resulting.iter())
                );
            } else {
                println!("Client {client_id}: no matching permissions to revoke.");
            }
        }
        PermissionAction::Set {
            client_id,
            permissions,
        } => {
            let permissions: std::collections::BTreeSet<ClientPermission> =
                permissions.into_iter().collect();
            require_client(&client_store, &client_id).await?;
            let mutation =
                client_admin::set_permissions(client_store.as_ref(), &client_id, &permissions)
                    .await?;
            println!(
                "Client {client_id} holds {}.",
                render_permissions(mutation.resulting.iter())
            );
        }
        PermissionAction::List { client_id } => {
            require_client(&client_store, &client_id).await?;
            // Unknown names included: one an operator cannot see they cannot
            // remove.
            let resolved =
                client_admin::resolved_permissions(client_store.as_ref(), &client_id).await?;
            println!(
                "Client {client_id}: {}",
                render_permissions(resolved.iter())
            );
        }
    }
    Ok(())
}

/// A job matches a tag as a `tag:<tag>` flag, so a change flags the client for
/// an eligible-index rebuild, as a device-profile change does.
async fn handle_tag_action(
    action: TagAction,
    client_store: Arc<dyn ClientStore>,
    tag_store: Arc<dyn TagStore>,
    todo: Arc<dyn TodoStore>,
) -> anyhow::Result<()> {
    // Up front, so a typo aborts before the store is touched.
    fn parse_tags(raw: Vec<String>) -> anyhow::Result<std::collections::BTreeSet<Tag>> {
        raw.into_iter()
            .map(|t| Tag::try_new(t).map_err(|e| anyhow::anyhow!("invalid tag: {e}")))
            .collect()
    }

    match action {
        TagAction::Add { client_id, tags } => {
            let parsed = parse_tags(tags)?;
            require_client(&client_store, &client_id).await?;
            let existing = tag_store.get_client_tags(&client_id).await?;
            let added: Vec<Tag> = parsed
                .into_iter()
                .filter(|tag| !existing.contains(tag))
                .collect();
            if added.is_empty() {
                println!("Client {client_id}: no new tags (all already present).");
                return Ok(());
            }
            let writes = stream::iter(added.iter().map(|tag| {
                let tag_store = tag_store.clone();
                let client_id = client_id.clone();
                let tag = tag.clone();
                async move { tag_store.add_client_tag(&client_id, &tag).await }
            }))
            .buffer_unordered(STORAGE_CONCURRENCY)
            .try_collect::<Vec<_>>();
            between_reindex_flags(todo.as_ref(), &client_id, writes).await?;
            tracing::info!(client_id = %client_id, count = added.len(), "client tags added");
            println!("Added {} tag(s) to {client_id}:", added.len());
            added.iter().for_each(|t| println!("  + {t}"));
            print_reindex_notice(&client_id);
        }
        TagAction::Remove { client_id, tags } => {
            let parsed = parse_tags(tags)?;
            require_client(&client_store, &client_id).await?;
            let existing = tag_store.get_client_tags(&client_id).await?;
            let removed: Vec<Tag> = parsed
                .into_iter()
                .filter(|tag| existing.contains(tag))
                .collect();
            if removed.is_empty() {
                println!("Client {client_id}: no matching tags to remove.");
                return Ok(());
            }
            let writes = stream::iter(removed.iter().map(|tag| {
                let tag_store = tag_store.clone();
                let client_id = client_id.clone();
                let tag = tag.clone();
                async move { tag_store.remove_client_tag(&client_id, &tag).await }
            }))
            .buffer_unordered(STORAGE_CONCURRENCY)
            .try_collect::<Vec<_>>();
            between_reindex_flags(todo.as_ref(), &client_id, writes).await?;
            tracing::info!(client_id = %client_id, count = removed.len(), "client tags removed");
            println!("Removed {} tag(s) from {client_id}:", removed.len());
            removed.iter().for_each(|t| println!("  - {t}"));
            print_reindex_notice(&client_id);
        }
        TagAction::List { client_id } => {
            require_client(&client_store, &client_id).await?;
            let tags = tag_store.get_client_tags(&client_id).await?;
            if tags.is_empty() {
                println!("Client {client_id} has no tags.");
            } else {
                println!("Client {client_id}: {}", join_tags(&tags));
            }
        }
    }
    Ok(())
}

/// The first flag closes the claim gate before a marker goes stale; the second,
/// written even if `write` fails, outlives a rebuild that read the old tags.
async fn between_reindex_flags<T>(
    todo: &dyn TodoStore,
    client_id: &ClientId,
    write: impl std::future::Future<Output = anyhow::Result<T>>,
) -> anyhow::Result<T> {
    let ids = std::slice::from_ref(client_id);
    client_admin::flag_reindex(todo, ids).await?;
    let written = write.await;
    client_admin::flag_reindex(todo, ids).await.map_err(|e| {
        anyhow::Error::from(e).context(format!(
            "{client_id}'s tags may have changed unflagged: run `pipette-mgmt flag-reindex {client_id}`"
        ))
    })?;
    written
}

fn print_reindex_notice(client_id: &ClientId) {
    println!(
        "Flagged {client_id} for an eligible-index rebuild: it takes no work until the next \
         `queue-maintenance` run, and drops any job it holds."
    );
}

#[cfg(test)]
mod tests {
    use anyhow::Context;
    use clap::{CommandFactory, ValueEnum};
    use rstest::rstest;

    use pipette_core::types::{DeriveFromKey, Mint};
    use pipette_plan_types::{BenchmarkType, JobId, RuntimeOutput, WarningKind};
    use pipette_store::{ClaimResult, JobState};
    use pipette_store_fs::build_local_fs_stores;
    use pipette_web_testkit::{
        own_client_by_org, register_pending, registered_user, seed_org, seeded_state, set_user_org,
        signed_empty, signed_json,
    };

    use super::*;

    /// A read and a dry run change nothing, so only the rest are audited.
    #[rstest]
    #[case::serve(&["serve"], false)]
    #[case::config_check(&["config", "check"], false)]
    #[case::a_listing(&["users", "list"], false)]
    #[case::a_create(&["users", "create", "a@example.test"], true)]
    #[case::a_cron_pass(&["queue-maintenance"], true)]
    #[case::an_unlock(&["unlock"], false)]
    #[case::the_openapi_document(&["openapi"], false)]
    #[case::a_migration(&["db", "migrate"], false)]
    #[case::an_import(&["db", "import"], true)]
    #[case::an_import_dry_run(&["db", "import", "--dry-run"], false)]
    #[case::a_job_cells_backfill(&["db", "backfill-job-cells"], true)]
    #[case::a_dry_run(&["fix-canonical", "--dry-run"], false)]
    #[case::a_cancel(&["plans", "cancel", "plan-0195c3e2-a4f3-7b8c-9d1e-2f3a4b5c6d7e"], true)]
    #[case::a_prune(&["audit", "prune"], true)]
    #[case::a_prune_dry_run(&["audit", "prune", "--dry-run"], false)]
    #[case::the_log(&["audit", "list"], false)]
    #[case::a_variants_listing(&["campaigns", "variants", "campaign_0195c3e2"], false)]
    #[case::a_document_read(&["campaigns", "revise", "campaign_0195c3e2"], false)]
    #[case::a_variant_edit(
        &["campaigns", "variants", "campaign_0195c3e2", "--remove", "1", "--base", "plan-1"],
        true
    )]
    #[case::a_document_revision(
        &["campaigns", "revise", "campaign_0195c3e2", "--plan", "p.toml", "--base", "plan-1"],
        true
    )]
    #[case::catalog_create(&["catalog", "create", "entry.toml"], true)]
    #[case::catalog_create_dry_run(&["catalog", "create", "entry.toml", "--dry-run"], false)]
    #[case::catalog_add(&["catalog", "add-member", "Fork", "member.toml"], true)]
    #[case::catalog_add_dry_run(&["catalog", "add-member", "Fork", "member.toml", "--dry-run"], false)]
    #[case::catalog_import(&["catalog", "import"], true)]
    #[case::catalog_import_since(&["catalog", "import", "--since", "2026-10-15"], true)]
    #[case::catalog_refresh(&["catalog", "refresh", "llamacpp-v0.7.0"], true)]
    #[case::catalog_pin(&["catalog", "pin", "Fork"], true)]
    #[case::catalog_unpin(&["catalog", "unpin", "Fork"], true)]
    #[case::catalog_hide(&["catalog", "hide", "Fork"], true)]
    #[case::catalog_unhide(&["catalog", "unhide", "Fork"], true)]
    #[case::catalog_tombstone(&["catalog", "tombstone", "Fork", "--yes"], true)]
    #[case::catalog_list(&["catalog", "list"], false)]
    #[case::catalog_show(&["catalog", "show", "Fork"], false)]
    #[case::org_catalog_create(&["catalog", "create", "entry.toml", "--org", "acme"], true)]
    #[case::org_catalog_create_dry_run(&["catalog", "create", "entry.toml", "--org", "acme", "--dry-run"], false)]
    #[case::org_catalog_add(&["catalog", "add-member", "Fork", "member.toml", "--org", "acme"], true)]
    #[case::org_catalog_pin(&["catalog", "pin", "Fork", "--org", "acme"], true)]
    #[case::org_catalog_tombstone(&["catalog", "tombstone", "Fork", "--org", "acme", "--yes"], true)]
    #[case::org_catalog_list(&["catalog", "list", "--org", "acme"], false)]
    #[case::org_catalog_show(&["catalog", "show", "Fork", "--org", "acme"], false)]
    fn a_command_is_audited_when_it_writes(#[case] args: &[&str], #[case] audited: bool) {
        let argv = std::iter::once("pipette-mgmt").chain(args.iter().copied());
        let cli = Cli::try_parse_from(argv);
        assert!(
            cli.as_ref()
                .is_ok_and(|cli| writes(&cli.command) == audited),
            "{args:?}"
        );
    }

    #[test]
    fn catalog_import_refuses_an_invalid_since_date() {
        assert!(
            Cli::try_parse_from(["pipette-mgmt", "catalog", "import", "--since", "2026-13-45"])
                .is_err()
        );
    }

    #[rstest]
    #[case::slug("acme")]
    #[case::id("org_0195c3e2-a4f3-7b8c-9d1e-2f3a4b5c6d7e")]
    fn catalog_org_accepts_slug_or_id(#[case] org: &str) -> anyhow::Result<()> {
        let cli = Cli::try_parse_from(["pipette-mgmt", "catalog", "show", "Fork", "--org", org])?;
        let Command::Catalog {
            action: CatalogAction::Show {
                org: Some(parsed), ..
            },
        } = cli.command
        else {
            anyhow::bail!("catalog show did not keep --org");
        };
        assert_eq!(parsed.to_string(), org);
        Ok(())
    }

    #[test]
    fn catalog_import_has_no_org() {
        assert!(
            Cli::try_parse_from(["pipette-mgmt", "catalog", "import", "--org", "acme"]).is_err()
        );
    }

    #[rstest]
    #[case::exact("Fork\n", true)]
    #[case::wrong_case("fork\n", false)]
    #[case::trailing_space("Fork ", false)]
    fn catalog_tombstone_needs_exact_stored_name(
        #[case] input: &str,
        #[case] ok: bool,
    ) -> anyhow::Result<()> {
        let name = CatalogName::try_new("Fork")?;
        assert_eq!(confirm_catalog_tombstone(&name, input).is_ok(), ok);
        Ok(())
    }

    struct DryRunCatalog;

    #[async_trait::async_trait]
    impl DescriptorCatalogStore for DryRunCatalog {
        async fn lock_importer_entries(
            &self,
        ) -> anyhow::Result<Box<dyn pipette_store::CatalogEntryLock>> {
            anyhow::bail!("dry run attempted to lock importer entries")
        }

        async fn create_entry(
            &self,
            _entry: &pipette_plan_types::DescriptorEntry,
        ) -> anyhow::Result<pipette_store::CreateEntryOutcome> {
            anyhow::bail!("dry run attempted to create an entry")
        }

        async fn append_members(
            &self,
            _scope: &CatalogScope,
            _name: &CatalogName,
            _members: pipette_plan_types::NonEmptyVec<pipette_plan_types::DescriptorMember>,
        ) -> anyhow::Result<pipette_store::AppendOutcome> {
            anyhow::bail!("dry run attempted to append members")
        }

        async fn set_state(
            &self,
            _scope: &CatalogScope,
            _name: &CatalogName,
            _state: DescriptorEntryState,
        ) -> anyhow::Result<pipette_store::SetStateOutcome> {
            anyhow::bail!("dry run attempted to change entry state")
        }

        async fn transition_state(
            &self,
            _scope: &CatalogScope,
            _name: &CatalogName,
            _expected: DescriptorEntryState,
            _state: DescriptorEntryState,
        ) -> anyhow::Result<pipette_store::TransitionOutcome> {
            anyhow::bail!("dry run attempted to transition entry state")
        }

        async fn transition_state_under_importer_lock(
            &self,
            _scope: &CatalogScope,
            _name: &CatalogName,
            _expected: DescriptorEntryState,
            _state: DescriptorEntryState,
        ) -> anyhow::Result<pipette_store::TransitionOutcome> {
            anyhow::bail!("dry run attempted to transition entry state")
        }

        async fn get_entry(
            &self,
            _scope: &CatalogScope,
            _name: &CatalogName,
        ) -> anyhow::Result<Option<pipette_plan_types::DescriptorEntry>> {
            Ok(None)
        }

        async fn list_entries(
            &self,
            _filter: &DescriptorEntryFilter,
            _after: Option<&CatalogEntryKey>,
            _limit: NonZeroUsize,
        ) -> anyhow::Result<pipette_store::DescriptorEntryPage> {
            Ok(pipette_store::DescriptorEntryPage::default())
        }

        async fn find_by_digests(
            &self,
            _digests: &[String],
        ) -> anyhow::Result<HashMap<String, Vec<pipette_store::DigestHit>>> {
            Ok(HashMap::new())
        }

        async fn live_entries_in_org(&self, _org_id: &OrgId) -> anyhow::Result<Vec<CatalogName>> {
            Ok(Vec::new())
        }

        async fn delete_tombstones_in_org(&self, _org_id: &OrgId) -> anyhow::Result<()> {
            anyhow::bail!("dry run attempted to delete tombstones")
        }
    }

    #[tokio::test]
    async fn catalog_model_dry_run_prints_the_resolved_commit_without_writing() -> anyhow::Result<()>
    {
        use pipette_hub_testkit::{MockFile, MockGate, MockHub, MockRepo};

        let commit = "a".repeat(40);
        let hub = MockHub::start(HashMap::from([(
            "acme/AcmeLM-VL-3B-GGUF".to_owned(),
            MockRepo {
                commit: commit.clone(),
                files: vec![MockFile::lfs("AcmeLM-Q4_0.gguf", b"GGUF q4_0")],
                public: true,
                rate_limited: false,
                gate: MockGate::Open,
            },
        )]))
        .await?;
        let document = "name = 'AcmeLM-VL-3B'\nmodels = [{ quantization = 'q4_0', model = { type = 'gguf_text', source = 'huggingface', org = 'acme', repo_name = 'AcmeLM-VL-3B-GGUF', path = 'AcmeLM-Q4_0.gguf' } }]";
        let store = DryRunCatalog;
        let scope = CatalogScope::Global;
        let preview = descriptor_catalog_admin::create(
            &store,
            &HubClient::new(hub.endpoint.clone(), NonZeroUsize::MIN)?,
            &scope,
            document,
            chrono::Utc::now(),
            true,
        )
        .await?;
        let mut output = Vec::new();
        print_catalog_preview(&preview, true, &mut output)?;
        assert_eq!(
            String::from_utf8(output)?,
            format!(
                "Dry run; nothing written.\n{}\nPinned acme/AcmeLM-VL-3B-GGUF@main to {commit}\n",
                serde_json::to_string_pretty(&preview.entry)?
            )
        );
        assert_eq!(hub.revision_requests(), 1);
        assert!(
            store
                .get_entry(&scope, &preview.entry.name)
                .await?
                .is_none()
        );
        Ok(())
    }

    /// A command ends as it returned, as a panic, or as stopped, and each ending
    /// reaches the audit write.
    #[tokio::test]
    async fn a_command_ends_in_one_of_three_ways() {
        let pending = std::future::pending::<()>;
        let returned = run_to_end(async { Ok(()) }, pending()).await;
        assert!(matches!(returned, Ended::Returned(Ok(()))));
        let failed = run_to_end(async { Err(anyhow::anyhow!("refused")) }, pending()).await;
        assert!(matches!(failed, Ended::Returned(Err(_))));
        let panicked = run_to_end(
            async { std::panic::resume_unwind(Box::new("the command broke")) },
            pending(),
        )
        .await;
        assert!(matches!(panicked, Ended::Panicked));
        let stopped = run_to_end(std::future::pending(), async {}).await;
        assert!(matches!(stopped, Ended::Interrupted));
    }

    /// A retention past what a date can hold is refused, not a panic.
    #[rstest]
    #[case::the_default(pipette_core::audit::RETENTION_DAYS, true)]
    #[case::none(0, true)]
    #[case::past_any_date(u32::MAX, false)]
    fn a_prune_cutoff_stays_in_range(#[case] days: u32, #[case] in_range: bool) {
        assert_eq!(prune_cutoff(days).is_ok(), in_range);
    }

    #[rstest]
    #[case::absent(None, "-")]
    #[case::empty(Some(&[] as &[&str]), "(none)")]
    #[case::one_name(Some(&["perform_work"] as &[&str]), "perform_work")]
    #[case::several_names(
        Some(&["perform_work", "submit_results"] as &[&str]),
        "perform_work, submit_results"
    )]
    fn a_permission_set_renders_comma_joined_and_an_absent_one_apart(
        #[case] set: Option<&[&str]>,
        #[case] rendered: &str,
    ) {
        assert_eq!(render_optional_permissions(set), rendered);
    }

    /// Headers derive from variant names, so a rename or a `heck` bump would
    /// quietly reword the `clients list` table.
    #[rstest]
    #[case(ClientColumn::ClientId, "Client ID")]
    #[case(ClientColumn::Organization, "Organization")]
    #[case(ClientColumn::Details, "Details")]
    #[case(ClientColumn::Contact, "Contact")]
    #[case(ClientColumn::Status, "Status")]
    #[case(ClientColumn::Registered, "Registered")]
    #[case(ClientColumn::Tags, "Tags")]
    #[case(ClientColumn::Permissions, "Permissions")]
    #[case(ClientColumn::Migrated, "Migrated")]
    #[case(ClientColumn::Derived, "Derived")]
    #[case(ClientColumn::Declared, "Declared")]
    fn client_column_headers_are_pinned(#[case] column: ClientColumn, #[case] expected: &str) {
        assert_eq!(column.header(), expected);
    }

    #[rstest]
    #[case::without_capability_filter(false, &[
        ClientColumn::ClientId,
        ClientColumn::Organization,
        ClientColumn::Details,
        ClientColumn::Contact,
        ClientColumn::Status,
        ClientColumn::Registered,
        ClientColumn::Tags,
        ClientColumn::Migrated,
    ])]
    #[case::with_capability_filter(true, &[
        ClientColumn::ClientId,
        ClientColumn::Organization,
        ClientColumn::Details,
        ClientColumn::Contact,
        ClientColumn::Status,
        ClientColumn::Registered,
        ClientColumn::Tags,
        ClientColumn::Migrated,
        ClientColumn::Derived,
        ClientColumn::Declared,
    ])]
    fn default_client_columns_follow_capability_filter(
        #[case] has_capability_filter: bool,
        #[case] expected: &[ClientColumn],
    ) {
        assert!(
            default_client_columns(has_capability_filter) == expected,
            "unexpected default columns for has_capability_filter={has_capability_filter}"
        );
    }

    /// Infallible for `#[case]`: a typo falls back to `Never` and fails its own
    /// case on the rendered string.
    fn at(s: &str) -> ExpiresAt {
        s.parse().map(ExpiresAt::At).unwrap_or(ExpiresAt::Never)
    }

    /// The `status_*` range cases pin the single arrow (`E → → L` otherwise).
    #[rstest]
    #[case::uniform(
        "\n→ ",
        at("2026-08-19T17:55:00Z"),
        at("2026-08-19T17:55:00Z"),
        "2026-08-19T17:55:00+00:00"
    )]
    #[case::list_range(
        "\n→ ",
        at("2026-08-19T17:55:00Z"),
        at("2026-09-18T17:55:00Z"),
        "2026-08-19T17:55:00+00:00\n→ 2026-09-18T17:55:00+00:00"
    )]
    #[case::list_never(
        "\n→ ",
        at("2026-08-19T17:55:00Z"),
        ExpiresAt::Never,
        "2026-08-19T17:55:00+00:00\n→ never"
    )]
    #[case::status_uniform(
        " → ",
        at("2026-08-19T17:55:00Z"),
        at("2026-08-19T17:55:00Z"),
        "2026-08-19T17:55:00+00:00"
    )]
    #[case::status_range(
        " → ",
        at("2026-08-19T17:55:00Z"),
        at("2026-09-18T17:55:00Z"),
        "2026-08-19T17:55:00+00:00 → 2026-09-18T17:55:00+00:00"
    )]
    #[case::status_never(
        " → ",
        at("2026-08-19T17:55:00Z"),
        ExpiresAt::Never,
        "2026-08-19T17:55:00+00:00 → never"
    )]
    fn expiry_ranges(
        #[case] sep: &str,
        #[case] earliest: ExpiresAt,
        #[case] latest: ExpiresAt,
        #[case] expected: &str,
    ) {
        assert_eq!(expiry_range(JobExpiry { earliest, latest }, sep), expected);
    }

    #[test]
    fn job_expiry_cell_renders_an_absent_range_as_a_dash() {
        assert_eq!(job_expiry_cell(None), "-");
    }

    fn snapshot_with(groups: &[usize]) -> ProgressSnapshot {
        ProgressSnapshot {
            computed_at: chrono::DateTime::UNIX_EPOCH,
            counts: pipette_plan_types::Counts {
                total: 0,
                finished: 0,
                running: 0,
                available: 0,
                failed: 0,
                cancelled: 0,
            },
            starved: groups
                .iter()
                .map(|jobs| pipette_core::plan::StarvedGroup {
                    clauses: pipette_plan_types::StarvedClauses {
                        requires: vec!["os:ios".to_string()],
                        any_of: Vec::new(),
                        clients: Vec::new(),
                        exclude_clients: Vec::new(),
                        excludes: Vec::new(),
                        not_all_of: Vec::new(),
                    },
                    job_ids: (0..*jobs)
                        .map(|i| JobId::from_uuid(uuid::Uuid::from_u128(i as u128)))
                        .collect(),
                })
                .collect(),
        }
    }

    fn rung(clause: &str, surviving: usize) -> pipette_plan_types::NarrowingRung {
        pipette_plan_types::NarrowingRung {
            clause: clause.to_string(),
            surviving_client_count: surviving,
        }
    }

    /// Blames the rung that empties a non-empty pool, and nothing when the fleet
    /// was empty: the fix then is an approval, not a clause edit.
    #[rstest]
    // Row 0 is the working fleet, so a clause rung's index is one higher.
    #[case::later_clause_closes_it(12, &[("requires", 12), ("excludes", 9), ("any_of", 0)], Some(3))]
    #[case::first_clause_closes_it(12, &[("requires", 0), ("excludes", 0)], Some(1))]
    #[case::empty_fleet_blames_nothing(0, &[("requires", 0), ("excludes", 0)], None)]
    #[case::exclusion_closes_it(5, &[("exclude_clients", 0), ("requires", 0)], Some(1))]
    #[case::survives_every_clause(12, &[("requires", 9), ("excludes", 3)], None)]
    #[case::empty_ladder(12, &[], None)]
    fn blamed_rungs(
        #[case] working: usize,
        #[case] rungs: &[(&str, usize)],
        #[case] expected: Option<usize>,
    ) {
        let ladder: Vec<_> = rungs
            .iter()
            .map(|(clause, surviving)| rung(clause, *surviving))
            .collect();
        let rows: Vec<(&str, usize)> = std::iter::once(("working clients", working))
            .chain(
                ladder
                    .iter()
                    .map(|rung| (rung.clause.as_str(), rung.surviving_client_count)),
            )
            .collect();
        assert_eq!(blamed_rung(&rows), expected);
    }

    /// `-` (no snapshot, unknown), `0` (none found) and a count stay apart.
    #[rstest]
    #[case::no_snapshot(None, "-")]
    #[case::none_starved(Some(vec![]), "0")]
    #[case::one_group(Some(vec![4]), "4 in 1 group(s)")]
    #[case::two_groups(Some(vec![4, 7]), "11 in 2 group(s)")]
    fn starved_cells(#[case] groups: Option<Vec<usize>>, #[case] expected: &str) {
        let snapshot = groups.map(|g| snapshot_with(&g));
        assert_eq!(starved_cell(snapshot.as_ref()), expected);
    }

    /// Same unit as `Starved`; the manifest always carries the list, so no `-`.
    #[rstest]
    #[case::none(vec![], "0")]
    #[case::one_group(vec![4], "4 in 1 group(s)")]
    #[case::two_groups(vec![4, 7], "11 in 2 group(s)")]
    fn warnings_cells(#[case] groups: Vec<usize>, #[case] expected: &str) {
        let warnings: Vec<Warning> = groups
            .iter()
            .map(|jobs| Warning {
                kind: WarningKind::Fleet,
                message: "no approved client matches os:ios".to_string(),
                job_ids: (0..*jobs)
                    .map(|i| JobId::from_uuid(uuid::Uuid::from_u128(i as u128)))
                    .collect(),
            })
            .collect();
        assert_eq!(warnings_cell(&warnings), expected);
    }

    /// `leased` alone would read a stranded job as work in flight.
    #[rstest]
    #[case::none_lapsed(7, 0, "7")]
    #[case::some_lapsed(7, 4, "7 (4 behind a lapsed lease)")]
    #[case::all_lapsed(3, 3, "3 (3 behind a lapsed lease)")]
    fn leased_cells(#[case] leased: usize, #[case] lapsed: usize, #[case] expected: &str) {
        let census = JobCensus {
            total: 12,
            available: 3,
            expired: 0,
            claimable: 3,
            indexed: 3,
            leased,
            lapsed_leases: lapsed,
            gone: 2,
            denied: 0,
            unparseable_avail_keys: 0,
            unparseable_leased_keys: 0,
        };
        assert_eq!(leased_cell(&census), expected);
    }

    /// A future stamp is reported as such, not passed off as fresh.
    #[rstest]
    #[case::seconds(45, "45s old")]
    #[case::minutes(3_725, "1h 2m old")]
    #[case::days(180_000, "2d 2h old")]
    #[case::future(-90, "stamped 1m 30s in the future")]
    fn snapshot_age_labels(#[case] secs: i64, #[case] expected: &str) {
        assert_eq!(snapshot_age_label(secs), expected);
    }

    /// Zero reads backward: `PlanLeases` splits on a strict `>`, so a lease due
    /// at the report's instant is stranded.
    #[rstest]
    #[case::live_seconds(45, "in 45s")]
    #[case::live_minutes(250, "in 4m 10s")]
    #[case::due_now(0, "0s ago")]
    #[case::lapsed_seconds(-4, "4s ago")]
    #[case::lapsed_stalled_cron(-3_360, "56m 0s ago")]
    #[case::lapsed_days(-180_000, "2d 2h ago")]
    fn lease_offset_labels(#[case] secs: i64, #[case] expected: &str) {
        assert_eq!(lease_offset_label(secs), expected);
    }

    #[rstest]
    #[case::user(
        Some("user_0195c3e2-a4f3-7b8c-9d1e-2f3a4b5c6d7e"),
        "user_0195c3e2-a4f3-7b8c-9d1e-2f3a4b5c6d7e"
    )]
    #[case::no_user(None, "operator")]
    fn submitter_labels(
        #[case] submitted_by: Option<&str>,
        #[case] expected: &str,
    ) -> anyhow::Result<()> {
        let submitted_by = submitted_by.map(UserId::try_new).transpose()?;
        assert_eq!(submitter_label(submitted_by.as_ref()), expected);
        Ok(())
    }

    /// Clap defaults to kebab-case; without `rename_all` this drifts to
    /// `pending-clients`.
    #[test]
    fn plan_status_arg_spellings_match_the_status_labels() -> anyhow::Result<()> {
        // `value_variants()`, so a new status is covered automatically.
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

    fn header_line(columns: &[PlanColumn]) -> String {
        column_table(columns, &[])
            .to_string()
            .lines()
            .next()
            .unwrap_or_default()
            .trim()
            .to_string()
    }

    #[test]
    fn the_default_plan_columns_omit_the_opt_in_ones() {
        assert_eq!(
            DEFAULT_PLAN_COLUMNS,
            [
                PlanColumn::PlanId,
                PlanColumn::Note,
                PlanColumn::Submitter,
                PlanColumn::Status,
                PlanColumn::Created,
                PlanColumn::JobsExpire,
                PlanColumn::Jobs,
                PlanColumn::Finished,
                PlanColumn::Starved,
            ]
        );
    }

    #[test]
    fn plans_list_refuses_the_manifest_gc_column() {
        let argv = ["pipette-mgmt", "plans", "list", "--columns", "manifest_gc"];
        assert!(Cli::try_parse_from(argv).is_err(), "{argv:?}");
    }

    /// The defaults follow declaration order, so only the reversed pair tells a
    /// caller's order from the enum's.
    #[test]
    fn a_column_table_follows_the_given_order() {
        assert_eq!(
            header_line(&DEFAULT_PLAN_COLUMNS),
            "Plan ID | Note | Submitter | Status | Created | Jobs Expire | \
             Jobs | Finished | Starved"
        );
        assert_eq!(
            header_line(&[PlanColumn::Starved, PlanColumn::PlanId]),
            "Starved | Plan ID"
        );
    }

    /// `value_variants()`, so a new column is covered automatically.
    #[test]
    fn every_plan_column_has_its_own_header() {
        let headers: Vec<&str> = PlanColumn::value_variants()
            .iter()
            .map(|column| column.header())
            .collect();
        let distinct: std::collections::HashSet<&str> = headers.iter().copied().collect();
        assert_eq!(
            distinct.len(),
            headers.len(),
            "two columns share a header: {headers:?}"
        );
    }

    #[rstest]
    #[case::no_snapshot(None, "-")]
    #[case::zero(Some(0), "0")]
    #[case::counted(Some(7), "7")]
    fn count_cells_distinguish_an_absent_snapshot_from_zero(
        #[case] running: Option<u64>,
        #[case] expected: &str,
    ) {
        let snapshot = running.map(|running| ProgressSnapshot {
            computed_at: chrono::Utc::now(),
            counts: Counts {
                total: running,
                finished: 0,
                running,
                available: 0,
                failed: 0,
                cancelled: 0,
            },
            starved: Vec::new(),
        });
        assert_eq!(count_cell(snapshot.as_ref(), |c| c.running), expected);
    }

    /// A plan no pass has reached must not report `0` lost jobs beside cells
    /// that read unknown.
    #[rstest]
    #[case::no_snapshot(None, 3, "-")]
    #[case::snapshot_none_lost(Some(()), 0, "0")]
    #[case::snapshot_some_lost(Some(()), 3, "3")]
    fn expired_cells(#[case] snapshot: Option<()>, #[case] lost: usize, #[case] expected: &str) {
        let snapshot = snapshot.map(|()| snapshot_with(&[]));
        assert_eq!(expired_cell(snapshot.as_ref(), lost), expected);
    }

    /// Literals, so they can live in a `#[case]` argument.
    const NIL_PLAN_ID: &str = "plan-00000000-0000-0000-0000-000000000000";
    const NIL_CAMPAIGN_ID: &str = "campaign_00000000-0000-0000-0000-000000000000";
    const NIL_JOB_ID: &str = "job-00000000-0000-0000-0000-000000000000";

    const NIL_USER_ID: &str = "user_00000000-0000-0000-0000-000000000000";
    const NIL_ORG_ID: &str = "org_00000000-0000-0000-0000-000000000000";

    /// The CLI parses its documented forms and rejects the malformed ones.
    #[rstest]
    // Every plan is filed into a campaign, so `plans` has no ingest.
    #[case::plans_ingest_is_gone(&["pipette-mgmt", "plans", "ingest", "/tmp/jobs"], false)]
    #[case::plans_ingest_document_is_gone(
        &["pipette-mgmt", "plans", "ingest", "--plan", "p.toml"], false
    )]
    #[case::campaign_rerun_dry_run(
        &["pipette-mgmt", "campaigns", "rerun", NIL_CAMPAIGN_ID, "--job", NIL_JOB_ID, "--dry-run"],
        true
    )]
    // A plan is ingested from a document only.
    #[case::campaign_ingest_dir(
        &["pipette-mgmt", "campaigns", "ingest", NIL_CAMPAIGN_ID, "/tmp/jobs"], false
    )]
    #[case::campaign_ingest_document(
        &["pipette-mgmt", "campaigns", "ingest", NIL_CAMPAIGN_ID, "--plan", "p.toml"], true
    )]
    #[case::campaign_ingest_document_dry_run(
        &["pipette-mgmt", "campaigns", "ingest", NIL_CAMPAIGN_ID, "--plan", "p.toml", "--dry-run"],
        true
    )]
    #[case::campaign_ingest_without_a_plan(
        &["pipette-mgmt", "campaigns", "ingest", NIL_CAMPAIGN_ID], false
    )]
    // A campaign is created with its first plan, from a document only.
    #[case::campaign_create(
        &["pipette-mgmt", "campaigns", "create", "Nightly", "--user", NIL_USER_ID, "--plan", "p.toml"],
        true
    )]
    #[case::campaign_create_dry_run(
        &["pipette-mgmt", "campaigns", "create", "Nightly", "--org", "acme", "--plan", "p.toml",
          "--note", "nightly-1", "--dry-run"],
        true
    )]
    #[case::campaign_create_without_a_plan(
        &["pipette-mgmt", "campaigns", "create", "Nightly", "--user", NIL_USER_ID], false
    )]
    #[case::campaign_create_with_both_owners(
        &["pipette-mgmt", "campaigns", "create", "Nightly", "--user", NIL_USER_ID,
          "--org", NIL_ORG_ID, "--plan", "p.toml"],
        false
    )]
    // `campaigns variants` lists with no edit, and an edit names its base.
    #[case::campaign_variants_list(
        &["pipette-mgmt", "campaigns", "variants", NIL_CAMPAIGN_ID], true
    )]
    #[case::campaign_variants_edit(
        &["pipette-mgmt", "campaigns", "variants", NIL_CAMPAIGN_ID, "--remove", "0",
          "--add", "more.toml", "--base", NIL_PLAN_ID, "--dry-run"],
        true
    )]
    #[case::campaign_variants_edit_without_a_base(
        &["pipette-mgmt", "campaigns", "variants", NIL_CAMPAIGN_ID, "--remove", "0"], false
    )]
    #[case::campaign_variants_list_dry_run(
        &["pipette-mgmt", "campaigns", "variants", NIL_CAMPAIGN_ID, "--dry-run"], false
    )]
    #[case::campaign_variants_list_with_a_base(
        &["pipette-mgmt", "campaigns", "variants", NIL_CAMPAIGN_ID, "--base", NIL_PLAN_ID], false
    )]
    // `campaigns revise` prints the document with no --plan, and --plan names its base.
    #[case::campaign_revise_read(
        &["pipette-mgmt", "campaigns", "revise", NIL_CAMPAIGN_ID], true
    )]
    #[case::campaign_revise(
        &["pipette-mgmt", "campaigns", "revise", NIL_CAMPAIGN_ID, "--plan", "p.toml",
          "--base", NIL_PLAN_ID, "--dry-run"],
        true
    )]
    #[case::campaign_revise_without_a_base(
        &["pipette-mgmt", "campaigns", "revise", NIL_CAMPAIGN_ID, "--plan", "p.toml"], false
    )]
    #[case::campaign_revise_read_dry_run(
        &["pipette-mgmt", "campaigns", "revise", NIL_CAMPAIGN_ID, "--dry-run"], false
    )]
    #[case::campaign_revise_read_with_a_base(
        &["pipette-mgmt", "campaigns", "revise", NIL_CAMPAIGN_ID, "--base", NIL_PLAN_ID], false
    )]
    #[case::list(&["pipette-mgmt", "plans", "list"], true)]
    #[case::list_filtered(&["pipette-mgmt", "plans", "list", "--status", "pending_clients"], true)]
    #[case::list_columns(
        &["pipette-mgmt", "plans", "list", "--columns", "plan_id,starved,snapshot"], true
    )]
    #[case::list_rejects_unknown_column(
        &["pipette-mgmt", "plans", "list", "--columns", "plan_id,nope"], false
    )]
    // A plan is named by its id alone.
    #[case::cancel_by_id(&["pipette-mgmt", "plans", "cancel", NIL_PLAN_ID], true)]
    #[case::cancel_by_note(&["pipette-mgmt", "plans", "cancel", "--note", "smoke"], false)]
    #[case::cancel_without_an_id(&["pipette-mgmt", "plans", "cancel"], false)]
    #[case::results_by_id(&["pipette-mgmt", "plans", "results", NIL_PLAN_ID], true)]
    #[case::results_paged(
        &["pipette-mgmt", "plans", "results", NIL_PLAN_ID, "--limit", "50", "--offset", "100"], true
    )]
    #[case::results_as_json(&["pipette-mgmt", "plans", "results", NIL_PLAN_ID, "--json"], true)]
    #[case::results_without_an_id(&["pipette-mgmt", "plans", "results"], false)]
    // Ids are validated at the argument boundary, before reaching the store.
    #[case::status_rejects_path_traversal(
        &["pipette-mgmt", "plans", "status", "../../etc/passwd"], false
    )]
    // A status outside the enum is rejected rather than silently ignored.
    #[case::list_rejects_unknown_status(
        &["pipette-mgmt", "plans", "list", "--status", "bogus"], false
    )]
    #[case::all_clients(&["pipette-mgmt", "clients", "list"], true)]
    #[case::requires_repeated_and_space_separated(
        &[
            "pipette-mgmt",
            "clients",
            "list",
            "--requires",
            "os:ios runtime:llama_cpp",
            "--requires",
            "chip:applea19pro",
        ],
        true
    )]
    #[case::comma_stays_inside_the_flag(
        &["pipette-mgmt", "clients", "list", "--requires", "device:iphone14,2"],
        true
    )]
    #[case::any_of_repeated(&[
        "pipette-mgmt",
        "clients",
        "list",
        "--any-of",
        "gpu:a17pro gpu:a18pro",
        "--any-of",
        "os:ios os:android",
    ], true)]
    #[case::negatives(&[
        "pipette-mgmt",
        "clients",
        "list",
        "--requires",
        "os:ios",
        "--excludes",
        "chip:a15 chip:a16",
        "--not-all-of",
        "runtime:llama_cpp chip:a17",
        "--not-all-of",
        "os:ios form_factor:tablet",
    ], true)]
    #[case::rejects_non_canonical_exclusion(
        &["pipette-mgmt", "clients", "list", "--excludes", "CHIP:A15"],
        false
    )]
    #[case::columns(&[
        "pipette-mgmt",
        "clients",
        "list",
        "--columns",
        "client_id,derived,declared",
    ], true)]
    #[case::rejects_non_canonical_flag(
        &["pipette-mgmt", "clients", "list", "--requires", "OS:iOS"],
        false
    )]
    #[case::rejects_empty_flag(
        &["pipette-mgmt", "clients", "list", "--requires", ""],
        false
    )]
    #[case::rejects_unknown_column(
        &["pipette-mgmt", "clients", "list", "--columns", "client_id,nope"],
        false
    )]
    #[case::one_client(&["pipette-mgmt", "flag-reindex", "client-abc"], true)]
    #[case::all(&["pipette-mgmt", "flag-reindex", "--all"], true)]
    #[case::neither_form(&["pipette-mgmt", "flag-reindex"], false)]
    #[case::both_forms(&["pipette-mgmt", "flag-reindex", "client-abc", "--all"], false)]
    #[case::rejects_path_traversal(&["pipette-mgmt", "flag-reindex", "../../etc/passwd"], false)]
    #[case::users_create(&["pipette-mgmt", "users", "create", "op@example.com"], true)]
    #[case::users_create_adopt(
        &["pipette-mgmt", "users", "create", "op@example.com", "--adopt-stranded-claim"],
        true
    )]
    #[case::users_create_grants(
        &["pipette-mgmt", "users", "create", "op@example.com",
          "--permission", "submit_any_plans", "--permission", "read_any_clients"],
        true
    )]
    // A work verb is refused at parse.
    #[case::users_create_rejects_client_verb(
        &["pipette-mgmt", "users", "create", "op@example.com", "--permission", "perform_work"],
        false
    )]
    #[case::users_delete_by_id(&["pipette-mgmt", "users", "delete", "user_abc"], true)]
    // `delete` takes an id, `release-claim` an address; neither guesses.
    #[case::users_delete_rejects_an_email(
        &["pipette-mgmt", "users", "delete", "op@example.com"],
        false
    )]
    #[case::users_release_claim(
        &["pipette-mgmt", "users", "release-claim", "op@example.com"],
        true
    )]
    #[case::users_release_claim_rejects_an_id(
        &["pipette-mgmt", "users", "release-claim", "user_abc"],
        false
    )]
    #[case::users_release_claim_needs_an_address(
        &["pipette-mgmt", "users", "release-claim"],
        false
    )]
    #[case::users_create_needs_email(&["pipette-mgmt", "users", "create"], false)]
    #[case::users_list(&["pipette-mgmt", "users", "list"], true)]
    #[case::users_show(&["pipette-mgmt", "users", "show", "user_abc"], true)]
    #[case::users_show_rejects_an_email(
        &["pipette-mgmt", "users", "show", "op@example.com"],
        false
    )]
    #[case::users_disable(&["pipette-mgmt", "users", "disable", "user_abc"], true)]
    #[case::users_permission_add(
        &["pipette-mgmt", "users", "permission", "add", "user_abc", "submit_plans"],
        true
    )]
    #[case::users_permission_rejects_client_verb(
        &["pipette-mgmt", "users", "permission", "add", "user_abc", "perform_work"],
        false
    )]
    #[case::users_permission_add_needs_one(
        &["pipette-mgmt", "users", "permission", "add", "user_abc"],
        false
    )]
    // A prune names its age, so no default decides what is old enough to delete.
    #[case::vault_prune(
        &["pipette-mgmt", "vault", "prune", "--older-than", "24h", "--dry-run"],
        true
    )]
    #[case::vault_prune_without_an_age(&["pipette-mgmt", "vault", "prune"], false)]
    #[case::users_permission_add_a_group(
        &["pipette-mgmt", "users", "permission", "add", "user_abc", "--org-member"],
        true
    )]
    #[case::users_permission_set_a_group(
        &["pipette-mgmt", "users", "permission", "set", "user_abc", "--server-admin"],
        true
    )]
    #[case::users_create_a_group(
        &["pipette-mgmt", "users", "create", "op@example.com", "--server-admin"],
        true
    )]
    // A group beside a name, or two groups, state two sets; neither wins silently.
    #[case::users_permission_set_group_and_name_conflict(
        &["pipette-mgmt", "users", "permission", "set", "user_abc", "submit_plans", "--server-admin"],
        false
    )]
    #[case::users_permission_add_groups_conflict(
        &["pipette-mgmt", "users", "permission", "add", "user_abc", "--org-member", "--server-admin"],
        false
    )]
    #[case::users_create_group_and_name_conflict(
        &["pipette-mgmt", "users", "create", "op@example.com", "--server-admin",
          "--permission", "submit_plans"],
        false
    )]
    #[case::users_permission_set_can_clear(
        &["pipette-mgmt", "users", "permission", "set", "user_abc"],
        true
    )]
    #[case::users_preauth_create(
        &["pipette-mgmt", "users", "preauth", "create", "user_abc", "--multi-use"],
        true
    )]
    #[case::users_preauth_create_needs_account(
        &["pipette-mgmt", "users", "preauth", "create"],
        false
    )]
    #[case::users_preauth_expiry_flags_conflict(
        &["pipette-mgmt", "users", "preauth", "create", "user_abc", "--no-expiry", "--expires-in", "30d"],
        false
    )]
    // An account is granted directly, so a key carries no template.
    #[case::users_preauth_takes_no_template(
        &["pipette-mgmt", "users", "preauth", "create", "user_abc", "--org-member"],
        false
    )]
    #[case::users_preauth_prune(&["pipette-mgmt", "users", "preauth", "prune"], true)]
    #[case::users_preauth_prune_dry_run(
        &["pipette-mgmt", "users", "preauth", "prune", "--dry-run"],
        true
    )]
    #[case::users_preauth_prune_takes_no_key(
        &["pipette-mgmt", "users", "preauth", "prune", "user_preauth_abc"],
        false
    )]
    #[case::clients_capabilities(&["pipette-mgmt", "clients", "capabilities"], true)]
    #[case::clients_capabilities_status(
        &["pipette-mgmt", "clients", "capabilities", "--status", "approved"],
        true
    )]
    #[case::clients_capabilities_unknown_status(
        &["pipette-mgmt", "clients", "capabilities", "--status", "decommissioned"],
        false
    )]
    #[case::orgs_create(&["pipette-mgmt", "orgs", "create", "acme"], true)]
    #[case::orgs_create_named(
        &["pipette-mgmt", "orgs", "create", "acme", "--name", "Acme Corp., Inc."], true
    )]
    // The handle becomes an index key, so it is a single path segment.
    #[case::orgs_create_rejects_a_path_traversal(
        &["pipette-mgmt", "orgs", "create", "../../etc/passwd"], false
    )]
    #[case::orgs_create_rejects_a_dot(&["pipette-mgmt", "orgs", "create", "ac.me"], false)]
    // A handle spelled like an id would parse back as one and never resolve.
    #[case::orgs_create_rejects_an_id_prefixed_handle(
        &["pipette-mgmt", "orgs", "create", "org_foo"], false
    )]
    #[case::orgs_show_by_handle(&["pipette-mgmt", "orgs", "show", "acme"], true)]
    #[case::orgs_show_by_id(&["pipette-mgmt", "orgs", "show", NIL_ORG_ID], true)]
    #[case::orgs_delete_detach(&["pipette-mgmt", "orgs", "delete", "acme", "--detach"], true)]
    #[case::orgs_benchmarks_list_every(&["pipette-mgmt", "orgs", "benchmarks", "list"], true)]
    #[case::orgs_benchmarks_list_one(&["pipette-mgmt", "orgs", "benchmarks", "list", "acme"], true)]
    #[case::users_set_org(&["pipette-mgmt", "users", "set-org", NIL_USER_ID, "acme"], true)]
    #[case::users_set_org_none(&["pipette-mgmt", "users", "set-org", NIL_USER_ID, "--none"], true)]
    #[case::users_set_org_needs_a_target(
        &["pipette-mgmt", "users", "set-org", NIL_USER_ID], false
    )]
    #[case::users_set_org_refuses_both(
        &["pipette-mgmt", "users", "set-org", NIL_USER_ID, "acme", "--none"], false
    )]
    #[case::users_key_list(&["pipette-mgmt", "users", "key", "list", NIL_USER_ID], true)]
    #[case::users_key_revoke(
        &["pipette-mgmt", "users", "key", "revoke", NIL_USER_ID, "userkey_abc"], true
    )]
    #[case::users_key_revoke_refuses_a_client_id(
        &["pipette-mgmt", "users", "key", "revoke", NIL_USER_ID, "ev1_abc"], false
    )]
    #[case::users_key_revoke_needs_a_key(
        &["pipette-mgmt", "users", "key", "revoke", NIL_USER_ID], false
    )]
    #[case::users_key_list_refuses_a_client_id(
        &["pipette-mgmt", "users", "key", "list", "ev1_abc"], false
    )]
    #[case::clients_owner_org(
        &["pipette-mgmt", "clients", "owner", "set", "c-one", "--org", "acme"], true
    )]
    #[case::clients_owner_user(
        &["pipette-mgmt", "clients", "owner", "set", "c-one", "--user", NIL_USER_ID], true
    )]
    #[case::clients_owner_refuses_both(
        &[
            "pipette-mgmt", "clients", "owner", "set", "c-one",
            "--org", "acme", "--user", NIL_USER_ID,
        ],
        false
    )]
    #[case::clients_owner_needs_one(
        &["pipette-mgmt", "clients", "owner", "set", "c-one"], false
    )]
    #[case::clients_owner_user_refuses_an_org_id(
        &["pipette-mgmt", "clients", "owner", "set", "c-one", "--user", NIL_ORG_ID], false
    )]
    #[case::clients_preauth_owner_org(
        &["pipette-mgmt", "clients", "preauth", "create", "--owner-org", "acme"], true
    )]
    #[case::clients_preauth_owner_user(
        &["pipette-mgmt", "clients", "preauth", "create", "--owner-user", NIL_USER_ID], true
    )]
    #[case::clients_preauth_owner_refuses_both(
        &[
            "pipette-mgmt", "clients", "preauth", "create",
            "--owner-org", "acme", "--owner-user", NIL_USER_ID,
        ],
        false
    )]
    #[case::clients_preauth_note(
        &["pipette-mgmt", "clients", "preauth", "create", "--note", " laptop "], true
    )]
    #[case::clients_preauth_refuses_a_blank_note(
        &["pipette-mgmt", "clients", "preauth", "create", "--note", "  "], false
    )]
    #[case::users_preauth_note(
        &["pipette-mgmt", "users", "preauth", "create", NIL_USER_ID, "--note", " laptop "], true
    )]
    #[case::users_preauth_refuses_a_blank_note(
        &["pipette-mgmt", "users", "preauth", "create", NIL_USER_ID, "--note", "  "], false
    )]
    #[case::users_create_org_admin(
        &["pipette-mgmt", "users", "create", "op@example.com", "--org-admin"], true
    )]
    #[case::users_permission_set_org_admin(
        &["pipette-mgmt", "users", "permission", "set", "user_abc", "--org-admin"], true
    )]
    #[case::org_admin_conflicts_with_server_admin(
        &["pipette-mgmt", "users", "create", "op@example.com", "--org-admin", "--server-admin"],
        false
    )]
    #[case::org_admin_conflicts_with_org_member(
        &["pipette-mgmt", "users", "create", "op@example.com", "--org-admin", "--org-member"], false
    )]
    #[case::org_admin_conflicts_with_names(
        &["pipette-mgmt", "users", "permission", "set", "user_abc", "read_orgs", "--org-admin"],
        false
    )]
    #[case::clients_list_owner_column(
        &["pipette-mgmt", "clients", "list", "--columns", "client_id,owner"], true
    )]
    #[case::log_format_json(&["pipette-mgmt", "--log-format", "json", "serve"], true)]
    #[case::log_format_unknown(&["pipette-mgmt", "--log-format", "logfmt", "serve"], false)]
    fn cli_argument_parsing(#[case] args: &[&str], #[case] accepted: bool) {
        assert_eq!(
            Cli::try_parse_from(args).is_ok(),
            accepted,
            "parsing {args:?}"
        );
    }

    #[derive(Clone, Default)]
    struct LogCapture(Arc<std::sync::Mutex<Vec<u8>>>);

    impl LogCapture {
        fn text(&self) -> anyhow::Result<String> {
            let bytes = self
                .0
                .lock()
                .map_err(|_| anyhow::anyhow!("log capture buffer poisoned"))?;
            Ok(String::from_utf8(bytes.clone())?)
        }
    }

    impl std::io::Write for LogCapture {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0
                .lock()
                .map_err(|_| std::io::Error::other("log capture buffer poisoned"))?
                .extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// A nested span, like a batch item's, must not hide the request's identifiers.
    #[test]
    fn a_json_log_line_carries_the_event_and_every_enclosing_span() -> anyhow::Result<()> {
        let capture = LogCapture::default();
        let writer = capture.clone();
        let dispatch = log_dispatch(
            LogFormat::Json,
            EnvFilter::new("info"),
            move || writer.clone(),
            false,
        );
        pipette_web_testkit::register_second_dispatcher();
        tracing::dispatcher::with_default(&dispatch, || {
            let request = tracing::info_span!(
                "request",
                method = "POST",
                client_id = tracing::field::Empty
            );
            let _request = request.enter();
            request.record("client_id", "ev1_abc");
            let _item = tracing::info_span!("item", batch_index = 2).entered();
            tracing::warn!(job_id = "job-1", "item refused");
        });

        let text = capture.text()?;
        let mut lines = text.lines();
        let line: serde_json::Value =
            serde_json::from_str(lines.next().context("nothing was logged")?)?;
        assert!(lines.next().is_none(), "one event, one line:\n{text}");
        assert_eq!(line["level"], "WARN");
        assert_eq!(line["message"], "item refused");
        assert_eq!(line["job_id"], "job-1");
        assert_eq!(
            line["spans"],
            serde_json::json!([
                { "name": "request", "method": "POST", "client_id": "ev1_abc" },
                { "name": "item", "batch_index": 2 },
            ])
        );
        assert!(line.get("span").is_none(), "{line}");
        Ok(())
    }

    /// A route acting on a device, as an administrator signs it.
    #[derive(Clone, Copy, Debug)]
    enum DeviceAdmin {
        /// A fleet administrator hands the device to an organization.
        OwnerSet,
        /// An organization's administrator approves another's device.
        OutOfScope,
    }

    /// JSON keeps one value per span field, so the device a route acts on must
    /// not land in the `client_id` that names who signed. Text keeps both.
    #[rstest]
    #[case::owner_set(DeviceAdmin::OwnerSet)]
    #[case::out_of_scope(DeviceAdmin::OutOfScope)]
    #[test]
    fn a_json_device_admin_line_names_the_signer_and_the_device(
        #[case] route: DeviceAdmin,
    ) -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let capture = LogCapture::default();
        let writer = capture.clone();
        let dispatch = log_dispatch(
            LogFormat::Json,
            EnvFilter::new("info"),
            move || writer.clone(),
            false,
        );
        pipette_web_testkit::register_second_dispatcher();
        let (signer, device, status, message) =
            tracing::dispatcher::with_default(&dispatch, || {
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()?
                    .block_on(async {
                        let state = seeded_state(dir.path()).await?;
                        let (_, device) = register_pending(&state).await?;
                        let (signer, resp, message) = match route {
                            DeviceAdmin::OwnerSet => {
                                let acme = seed_org(&state, "acme").await?;
                                let admin =
                                    registered_user(&state, &[UserPermission::ManageAnyClients])
                                        .await?;
                                let resp = signed_json(
                                    &state,
                                    &admin,
                                    "PUT",
                                    &format!("/api/v1/clients/{device}/owner"),
                                    &serde_json::json!({
                                        "owner": { "type": "org", "org_id": acme.org_id }
                                    }),
                                )
                                .await?;
                                (admin.1, resp, "client owner set")
                            }
                            DeviceAdmin::OutOfScope => {
                                let ours = seed_org(&state, "ours").await?;
                                let theirs = seed_org(&state, "theirs").await?;
                                own_client_by_org(&state, &device, &theirs.org_id).await?;
                                let admin =
                                    registered_user(&state, &[UserPermission::ManageClients])
                                        .await?;
                                set_user_org(&state, &admin.1, Some(ours.org_id)).await?;
                                let resp = signed_empty(
                                    &state,
                                    &admin,
                                    "POST",
                                    &format!("/api/v1/clients/{device}/approve"),
                                )
                                .await?;
                                (
                                    admin.1,
                                    resp,
                                    "refused: the device is outside the caller's scope",
                                )
                            }
                        };
                        anyhow::Ok((signer, device, resp.status(), message))
                    })
            })?;
        let expected = match route {
            DeviceAdmin::OwnerSet => axum::http::StatusCode::OK,
            DeviceAdmin::OutOfScope => axum::http::StatusCode::FORBIDDEN,
        };
        let text = capture.text()?;
        assert_eq!(status, expected, "{text}");

        let lines = text
            .lines()
            .map(serde_json::from_str)
            .collect::<Result<Vec<serde_json::Value>, _>>()?;
        let line = lines
            .iter()
            .find(|line| line["message"] == message)
            .with_context(|| format!("no {message:?} line in:\n{text}"))?;
        let request = line["spans"]
            .as_array()
            .and_then(|spans| spans.iter().find(|span| span["name"] == "request"))
            .with_context(|| format!("no request span on {line}"))?;
        assert_eq!(request["client_id"], signer.to_string(), "{line}");
        assert_eq!(request["target_client_id"], device.to_string(), "{line}");
        Ok(())
    }

    #[test]
    fn the_log_filter_defaults_to_info_and_refuses_a_malformed_directive() -> anyhow::Result<()> {
        assert_eq!(
            log_filter().parse("")?.max_level_hint(),
            Some(LevelFilter::INFO)
        );
        assert_eq!(
            log_filter().parse("debug")?.max_level_hint(),
            Some(LevelFilter::DEBUG)
        );
        assert!(log_filter().parse("pipette_mgmt=loud").is_err());
        Ok(())
    }

    fn flag_reindex_stores() -> anyhow::Result<(tempfile::TempDir, Stores)> {
        let dir = tempfile::tempdir()?;
        let config = Config {
            storage: StorageConfig::LocalFs {
                data_dir: dir.path().to_path_buf(),
            },
            ..Default::default()
        };
        let stores = build_local_fs_stores(&config)?;
        Ok((dir, stores))
    }

    fn user_stores() -> anyhow::Result<(tempfile::TempDir, Stores)> {
        let dir = tempfile::tempdir()?;
        let config = Config {
            storage: StorageConfig::local_fs(dir.path().to_path_buf()),
            auth_storage: StorageConfig::local_fs(dir.path().to_path_buf()),
            ..Default::default()
        };
        let stores = build_local_fs_stores(&config)?;
        Ok((dir, stores))
    }

    fn email(s: &str) -> anyhow::Result<ContactEmail> {
        Ok(ContactEmail::try_new(s)?)
    }

    /// An account owning one resource, uploaded as `POST /api/v1/vault/resources`
    /// does.
    async fn vault_stores() -> anyhow::Result<(tempfile::TempDir, Stores, UserId, VaultResourceId)>
    {
        let (dir, stores) = user_stores()?;
        handle_user_action(
            UserAction::Create {
                adopt_stranded_claim: false,
                email: email("operator@example.test")?,
                permissions: Vec::new(),
                group: PermissionGroupFlags::default(),
                account_type: UserAccountType::Human,
                org: None,
                campaigns: Vec::new(),
            },
            stores.clone(),
        )
        .await?;
        let user_id = stores
            .users
            .list_users()
            .await?
            .pop()
            .context("the account must exist")?
            .user_id;
        let owner = VaultOwner::User {
            user_id: user_id.clone(),
        };
        let resource_id = uploaded_for(&stores, owner).await?;
        Ok((dir, stores, user_id, resource_id))
    }

    async fn uploaded_for(stores: &Stores, owner: VaultOwner) -> anyhow::Result<VaultResourceId> {
        const BLOB: &[u8] = b"GGUF private weights";
        let mut digest = pipette_core::vault::BlobDigest::default();
        digest.update(BLOB);
        let (sha256, _) = digest.finish()?;
        let body: pipette_store::BlobBody = Box::pin(futures::stream::iter([Ok(
            axum::body::Bytes::from_static(BLOB),
        )]));
        vault_admin::store_blob(&*stores.vault, &owner, &sha256, u64::MAX, body).await?;
        let artifact = serde_json::from_value(serde_json::json!({
            "type": "gguf_text",
            "model": {"filename": "acme-checkpoint.gguf", "sha256": sha256},
        }))?;
        let resource = vault_admin::upload(
            &*stores.vault,
            NonEmptyTrimmedString::try_new("acme checkpoint")?,
            owner,
            artifact,
            chrono::Utc::now(),
        )
        .await?
        .pop()
        .context("the record must exist")?;
        Ok(resource.resource_id)
    }

    /// `list --user` narrows to one owner, and an unknown owner is an error.
    #[tokio::test]
    async fn vault_list_filters_by_owner() -> anyhow::Result<()> {
        let (_dir, stores, user_id, _) = vault_stores().await?;

        let refused = handle_vault_action(
            VaultAction::List {
                user: Some(UserId::mint()),
                org: None,
            },
            stores.clone(),
        )
        .await;
        assert!(
            refused.is_err(),
            "an unknown owner is an error, not an empty list"
        );

        handle_vault_action(
            VaultAction::List {
                user: Some(user_id),
                org: None,
            },
            stores.clone(),
        )
        .await?;
        Ok(())
    }

    /// `show` and `delete` on an id that names nothing are errors.
    #[tokio::test]
    async fn vault_show_and_delete_refuse_an_unknown_id() -> anyhow::Result<()> {
        let (_dir, stores, _, _) = vault_stores().await?;
        let absent = VaultResourceId::mint();
        for action in [
            VaultAction::Show {
                resource_id: absent.clone(),
            },
            VaultAction::Delete {
                resource_id: absent.clone(),
            },
        ] {
            let refused = handle_vault_action(action, stores.clone()).await;
            let message = format!("{:#}", refused.err().context("must be refused")?);
            assert!(message.contains(absent.as_str()), "{message}");
        }
        Ok(())
    }

    /// `show` prints a resource's variant, its file and the plan entry, and
    /// says whether its bytes are still stored.
    #[tokio::test]
    async fn vault_show_prints_the_variant_the_file_and_the_entry() -> anyhow::Result<()> {
        let (_dir, stores, _, resource_id) = vault_stores().await?;
        let resource = vault_admin::require(&*stores.vault, &resource_id).await?;
        let shown = vault_show_lines(&resource, Some(resource.bytes))?.join("\n");
        let expected = [
            "Variant      gguf_text".to_string(),
            "File         acme-checkpoint.gguf, its model".to_string(),
            "Origin       none: a user uploaded it".to_string(),
            "Object state present".to_string(),
            format!("url = \"/vault/resources/{resource_id}/content\""),
        ];
        let missing: Vec<&String> = expected
            .iter()
            .filter(|line| !shown.contains(line.as_str()))
            .collect();
        assert!(missing.is_empty(), "{missing:?} missing from:\n{shown}");

        let imported = VaultResource {
            origin: Some(serde_json::from_value(serde_json::json!({
                "source": "github_release",
                "owner": "acme",
                "repo": "llama.cpp",
                "commit": "c".repeat(40),
                "asset": "llama-cli.tar.gz",
            }))?),
            ..resource.clone()
        };
        let shown = vault_show_lines(&imported, Some(imported.bytes))?.join("\n");
        let origin = format!(
            "Origin       github.com/acme/llama.cpp@{}:llama-cli.tar.gz",
            "c".repeat(40)
        );
        assert!(shown.contains(&origin), "{origin:?} missing from:\n{shown}");

        let gone = vault_show_lines(&resource, None)?.join("\n");
        assert!(gone.contains("Object state MISSING"), "{gone}");
        handle_vault_action(VaultAction::Show { resource_id }, stores.clone()).await
    }

    /// A campaign or vault resource each refuse `users delete`, before `--detach`
    /// clears anything.
    #[rstest]
    #[case::the_campaign_last(true)]
    #[case::the_vault_resource_last(false)]
    #[tokio::test]
    async fn deleting_an_account_refuses_its_campaigns_and_vault_resources(
        #[case] campaign_last: bool,
    ) -> anyhow::Result<()> {
        let (_dir, stores, user_id, resource_id) = vault_stores().await?;
        let campaign = NewCampaign::new(
            NonEmptyTrimmedString::try_new("nightly")?,
            CampaignOwner::User {
                user_id: user_id.clone(),
            },
        )
        .around(PlanId::from_uuid(uuid::Uuid::now_v7()), chrono::Utc::now());
        stores.campaigns.put_campaign(&campaign).await?;
        let device = ClientId::try_new("ev1_laptop")?;
        stores
            .orgs
            .put_ownership(&pipette_core::organization::Ownership {
                client_id: device.clone(),
                owner: ClientOwner::User {
                    user_id: user_id.clone(),
                },
            })
            .await?;
        let delete = || {
            handle_user_action(
                UserAction::Delete {
                    user_id: user_id.clone(),
                    detach: true,
                },
                stores.clone(),
            )
        };

        let refused = delete()
            .await
            .err()
            .context("a campaign and a vault resource must refuse the delete")?
            .to_string();
        [
            campaign.campaign_id.as_str(),
            resource_id.as_str(),
            "campaigns owner",
            "vault delete",
        ]
        .into_iter()
        .for_each(|named| assert!(refused.contains(named), "{named} missing from: {refused}"));

        let drop_campaign = || stores.campaigns.delete_campaign(&campaign.campaign_id);
        let drop_resource = || {
            let resource_id = resource_id.clone();
            handle_vault_action(VaultAction::Delete { resource_id }, stores.clone())
        };
        let (left, gone) = if campaign_last {
            drop_resource().await?;
            ("campaigns owner", "vault delete")
        } else {
            drop_campaign().await?;
            ("vault delete", "campaigns owner")
        };
        let refused = delete()
            .await
            .err()
            .context("either alone must refuse the delete")?
            .to_string();
        assert!(
            refused.contains(left) && !refused.contains(gone),
            "{refused}"
        );
        assert!(stores.users.get_user(&user_id).await?.is_some());
        assert!(
            stores.orgs.get_ownership(&device).await?.is_some(),
            "refused before anything was detached"
        );

        if campaign_last {
            drop_campaign().await?;
        } else {
            drop_resource().await?;
        }
        delete().await?;
        assert!(stores.users.get_user(&user_id).await?.is_none());
        assert!(stores.orgs.get_ownership(&device).await?.is_none());
        Ok(())
    }

    #[tokio::test]
    async fn vault_delete_takes_an_unshared_object() -> anyhow::Result<()> {
        let (_dir, stores, _, resource_id) = vault_stores().await?;
        let key = vault_admin::require(&*stores.vault, &resource_id)
            .await?
            .object_key;

        handle_vault_action(VaultAction::Delete { resource_id }, stores.clone()).await?;
        assert!(
            drain_vault_resources(&*stores.vault)
                .await?
                .items
                .is_empty()
        );
        assert_eq!(
            stores.vault.stat_blob(&key).await?,
            None,
            "no record names the bytes"
        );
        Ok(())
    }

    /// A dry run and a file written too recently delete nothing.
    #[tokio::test]
    async fn vault_prune_takes_only_files_no_resource_names() -> anyhow::Result<()> {
        let (dir, stores, _, resource_id) = vault_stores().await?;
        let named = vault_admin::require(&*stores.vault, &resource_id)
            .await?
            .object_key;
        let left = dir
            .path()
            .join("vault-resources")
            .join(named.as_ref())
            .with_file_name("b".repeat(64));
        std::fs::write(&left, b"GGUF left behind")?;
        let prune = |older_than: u64, dry_run| VaultAction::Prune {
            older_than: std::time::Duration::from_secs(older_than),
            dry_run,
        };

        handle_vault_action(prune(3600, false), stores.clone()).await?;
        assert!(left.exists(), "written too recently");
        handle_vault_action(prune(0, true), stores.clone()).await?;
        assert!(left.exists(), "a dry run");
        handle_vault_action(prune(0, false), stores.clone()).await?;
        assert!(!left.exists());
        assert!(stores.vault.stat_blob(&named).await?.is_some());
        Ok(())
    }

    fn slug(s: &str) -> anyhow::Result<OrgSlug> {
        Ok(OrgSlug::try_new(s)?)
    }

    /// `handle_org_action` consumes `Stores`, so each command re-opens them.
    fn reopen(dir: &tempfile::TempDir) -> anyhow::Result<Stores> {
        build_local_fs_stores(&Config {
            storage: StorageConfig::local_fs(dir.path().to_path_buf()),
            auth_storage: StorageConfig::local_fs(dir.path().to_path_buf()),
            ..Default::default()
        })
    }

    async fn create_org(dir: &tempfile::TempDir, handle: &str) -> anyhow::Result<Organization> {
        handle_org_action(
            OrgAction::Create {
                slug: slug(handle)?,
                name: None,
                adopt_stranded_claim: false,
            },
            reopen(dir)?,
        )
        .await?;
        let stores = reopen(dir)?;
        let org_id = stores
            .orgs
            .lookup_org_by_slug(handle)
            .await?
            .context("the handle must be claimed")?;
        stores
            .orgs
            .get_org(&org_id)
            .await?
            .context("the record must exist")
    }

    #[tokio::test]
    async fn org_delete_refuses_live_catalog_entries_and_clears_tombstones() -> anyhow::Result<()> {
        let Some(test_db) = pipette_store_sql::scratch::TestDb::new().await? else {
            return Ok(());
        };
        let dir = tempfile::tempdir()?;
        let stores = pipette_store_sql::overlay(test_db.db(), reopen(&dir)?);
        handle_org_action(
            OrgAction::Create {
                slug: slug("acme")?,
                name: None,
                adopt_stranded_claim: false,
            },
            stores.clone(),
        )
        .await?;
        let org = org::require(&*stores.orgs, &OrgRef::Slug(slug("acme")?)).await?;
        let scope = CatalogScope::Org(org.org_id.clone());
        assert_eq!(
            catalog_scope(&stores, Some(&OrgRef::Slug(slug("acme")?))).await?,
            scope
        );
        assert_eq!(
            catalog_scope(&stores, Some(&OrgRef::Id(org.org_id.clone()))).await?,
            scope
        );
        assert!(
            catalog_scope(&stores, Some(&OrgRef::Slug(slug("missing")?)))
                .await
                .is_err()
        );
        let catalog = stores
            .descriptor_catalog
            .as_deref()
            .context("SQL catalog")?;
        let document = "name = 'Fork'\nruntimes = [{ type = 'llamacpp_cli_stock_tools', source = 'github_release', repository_url = 'github.com/ggml-org/llama.cpp', version = 'b1', flavor = 'macos-arm64' }]";
        descriptor_catalog_admin::create(
            catalog,
            &catalog_hub(&Config::default())?,
            &scope,
            document,
            chrono::Utc::now(),
            false,
        )
        .await?;
        for detach in [false, true] {
            let refused = handle_org_action(
                OrgAction::Delete {
                    org: OrgRef::Id(org.org_id.clone()),
                    detach,
                },
                stores.clone(),
            )
            .await
            .err()
            .context("live catalog entry must refuse org delete")?;
            assert!(format!("{refused:#}").contains("live catalog entry"));
            assert!(stores.orgs.get_org(&org.org_id).await?.is_some());
        }
        let name = CatalogName::try_new("Fork")?;
        catalog
            .set_state(&scope, &name, DescriptorEntryState::Tombstoned)
            .await?;
        handle_org_action(
            OrgAction::Delete {
                org: OrgRef::Id(org.org_id.clone()),
                detach: false,
            },
            stores.clone(),
        )
        .await?;
        assert!(stores.orgs.get_org(&org.org_id).await?.is_none());
        assert!(catalog.get_entry(&scope, &name).await?.is_none());
        Ok(())
    }

    async fn member_of(
        stores: &Stores,
        org: &Organization,
    ) -> anyhow::Result<pipette_core::user::User> {
        let member =
            pipette_core::user::User::try_new(email("op@example.com")?, chrono::Utc::now())?;
        stores.users.put_user(&member).await?;
        stores
            .orgs
            .set_membership(&member.user_id, Some(org.org_id.clone()))
            .await?;
        Ok(member)
    }

    /// The second create of a handle is refused and names its holder.
    #[tokio::test]
    async fn a_second_organization_cannot_take_a_handle() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let first = create_org(&dir, "acme").await?;

        let refused = handle_org_action(
            OrgAction::Create {
                slug: slug("acme")?,
                name: None,
                adopt_stranded_claim: false,
            },
            reopen(&dir)?,
        )
        .await;
        let message = refused
            .err()
            .context("a duplicate handle must be refused")?
            .to_string();
        assert!(
            message.contains(first.org_id.as_str()),
            "the refusal must name the holder: {message}"
        );
        assert_eq!(reopen(&dir)?.orgs.list_orgs().await?.len(), 1);
        Ok(())
    }

    /// The path names no file, so the refusal comes before the document is read.
    #[tokio::test]
    async fn a_dry_run_create_refuses_an_unknown_owner() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let ghost = UserId::try_new(NIL_USER_ID)?;

        let refused = handle_campaign_action(
            CampaignAction::Create {
                name: "Nightly".into(),
                user: Some(ghost.clone()),
                org: None,
                plan: dir.path().join("absent.toml"),
                note: None,
                strict_models: false,
                dry_run: true,
            },
            &reopen(&dir)?,
            IngestSettings {
                models: ModelCheck::none(),
                max_jobs: std::num::NonZeroUsize::MIN,
                creating_max_age: std::time::Duration::from_secs(60),
                import: None,
            },
        )
        .await;
        let message = refused
            .err()
            .context("an owner that does not exist must be refused")?
            .to_string();
        assert!(message.contains(ghost.as_str()), "{message}");
        Ok(())
    }

    /// Adopting a stranded claim needs the flag, and builds the record under the
    /// id the claim already names.
    #[tokio::test]
    async fn a_stranded_handle_claim_is_adopted_only_on_demand() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let stranded = create_org(&dir, "acme").await?;
        reopen(&dir)?.orgs.delete_org(&stranded.org_id).await?;

        assert!(
            handle_org_action(
                OrgAction::Create {
                    slug: slug("acme")?,
                    name: None,
                    adopt_stranded_claim: false,
                },
                reopen(&dir)?,
            )
            .await
            .is_err(),
            "a stranded claim is not adopted silently"
        );

        handle_org_action(
            OrgAction::Create {
                slug: slug("acme")?,
                name: None,
                adopt_stranded_claim: true,
            },
            reopen(&dir)?,
        )
        .await?;
        let adopted = reopen(&dir)?
            .orgs
            .get_org(&stranded.org_id)
            .await?
            .context("the adopted record must exist")?;
        assert_eq!(
            adopted.org_id, stranded.org_id,
            "adopting builds the record under the id the claim names"
        );
        Ok(())
    }

    /// A delete is refused while anything names the organization; `--detach`
    /// clears those references and then deletes.
    #[tokio::test]
    async fn deleting_an_organization_refuses_live_references() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let org = create_org(&dir, "acme").await?;

        let stores = reopen(&dir)?;
        let member = member_of(&stores, &org).await?;

        let refused = handle_org_action(
            OrgAction::Delete {
                org: OrgRef::Slug(slug("acme")?),
                detach: false,
            },
            reopen(&dir)?,
        )
        .await;
        assert!(
            refused
                .err()
                .context("a live reference must refuse the delete")?
                .to_string()
                .contains("1 member(s)"),
        );
        assert!(reopen(&dir)?.orgs.get_org(&org.org_id).await?.is_some());

        handle_org_action(
            OrgAction::Delete {
                org: OrgRef::Slug(slug("acme")?),
                detach: true,
            },
            reopen(&dir)?,
        )
        .await?;
        let stores = reopen(&dir)?;
        assert!(stores.orgs.get_org(&org.org_id).await?.is_none());
        assert!(
            stores.orgs.lookup_org_by_slug("acme").await?.is_none(),
            "the handle is released, so it can be taken again"
        );
        assert_eq!(
            stores.orgs.get_membership(&member.user_id).await?,
            None,
            "the detached member belongs to nothing rather than to a ghost"
        );
        Ok(())
    }

    /// The `put` refusal reads a public catalog; nothing else here needs one.
    fn seed_public_catalog(dir: &tempfile::TempDir) -> anyhow::Result<()> {
        let benchmarks = dir.path().join("benchmarks");
        std::fs::create_dir_all(&benchmarks)?;
        std::fs::write(
            benchmarks.join("prefill_throughput_256.toml"),
            "benchmark_type = \"prefill_throughput\"\nparameter_prefill_tokens = 256\n",
        )?;
        Ok(())
    }

    fn an_entry_file(dir: &tempfile::TempDir, benchmark_id: &str) -> anyhow::Result<PathBuf> {
        let path = dir.path().join(format!("{benchmark_id}.toml"));
        std::fs::write(
            &path,
            "benchmark_type = \"eval\"\nparameter_eval_id = \"acme_internal\"\n\
             parameter_dataset_name = \"v3\"\nparameter_max_tokens = 4096\n",
        )?;
        Ok(path)
    }

    async fn org_benchmarks(
        dir: &tempfile::TempDir,
        action: OrgBenchmarkAction,
    ) -> anyhow::Result<()> {
        handle_org_action(OrgAction::Benchmarks { action }, reopen(dir)?).await
    }

    /// Put, replace and delete, each checked against the store directly.
    #[tokio::test]
    async fn org_benchmarks_put_show_and_delete() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        seed_public_catalog(&dir)?;
        let acme = create_org(&dir, "acme").await?;
        let acme_ref = OrgRef::Slug(slug("acme")?);
        let org = || acme_ref.clone();
        // Unprefixed, so the entry may change type when replaced.
        let file = an_entry_file(&dir, "acme_internal_v3")?;
        let id = BenchmarkId::try_new("acme_internal_v3")?;

        org_benchmarks(&dir, OrgBenchmarkAction::List { org: Some(org()) }).await?;
        org_benchmarks(
            &dir,
            OrgBenchmarkAction::Put {
                org: org(),
                file: file.clone(),
            },
        )
        .await?;
        let stores = reopen(&dir)?;
        let written = org_catalog::require(&*stores.catalog, &acme.org_id, &id).await?;
        assert_eq!(written.org_id, Some(acme.org_id.clone()));
        assert_eq!(written.benchmark_type(), BenchmarkType::Eval);
        org_benchmarks(
            &dir,
            OrgBenchmarkAction::Show {
                org: org(),
                benchmark_id: id.clone(),
            },
        )
        .await?;

        std::fs::write(
            &file,
            "benchmark_type = \"prefill_throughput\"\nparameter_prefill_tokens = 64\n",
        )?;
        org_benchmarks(&dir, OrgBenchmarkAction::Put { org: org(), file }).await?;
        let stores = reopen(&dir)?;
        let replaced = org_catalog::require(&*stores.catalog, &acme.org_id, &id).await?;
        assert_eq!(replaced.benchmark_type(), BenchmarkType::PrefillThroughput);

        org_benchmarks(
            &dir,
            OrgBenchmarkAction::Delete {
                org: org(),
                benchmark_id: id.clone(),
            },
        )
        .await?;
        let stores = reopen(&dir)?;
        assert!(
            org_catalog::list(&*stores.catalog, &acme.org_id)
                .await?
                .is_empty()
        );
        Ok(())
    }

    /// The host runs the route's rule: no public id, no other organization's,
    /// no uncreated organization.
    #[tokio::test]
    async fn org_benchmarks_refusals() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        seed_public_catalog(&dir)?;
        create_org(&dir, "acme").await?;
        let acme_ref = OrgRef::Slug(slug("acme")?);
        let org = || acme_ref.clone();

        let shadowing = an_entry_file(&dir, "prefill_throughput_256")?;
        let refused = org_benchmarks(
            &dir,
            OrgBenchmarkAction::Put {
                org: org(),
                file: shadowing,
            },
        )
        .await
        .err()
        .context("a public id must be refused")?
        .to_string();
        assert!(refused.contains("prefill_throughput_256"), "{refused}");

        let private = an_entry_file(&dir, "eval_acme_internal_v3")?;
        org_benchmarks(
            &dir,
            OrgBenchmarkAction::Put {
                org: org(),
                file: private.clone(),
            },
        )
        .await?;
        create_org(&dir, "labs").await?;
        let refused = org_benchmarks(
            &dir,
            OrgBenchmarkAction::Put {
                org: OrgRef::Slug(slug("labs")?),
                file: private,
            },
        )
        .await
        .err()
        .context("another organization's id must be refused")?
        .to_string();
        assert!(refused.contains("eval_acme_internal_v3"), "{refused}");

        let refused = org_benchmarks(
            &dir,
            OrgBenchmarkAction::List {
                org: Some(OrgRef::Slug(slug("nobody")?)),
            },
        )
        .await
        .err()
        .context("an unknown organization must be refused")?
        .to_string();
        assert!(refused.contains("nobody"), "{refused}");
        Ok(())
    }

    /// The host runs past every gate, so a job may name any organization's
    /// private benchmark.
    #[tokio::test]
    async fn campaigns_ingest_names_a_private_benchmark() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        seed_public_catalog(&dir)?;
        let acme = create_org(&dir, "acme").await?;
        let stores = reopen(&dir)?;
        let id = "prefill_throughput_acme_64";
        let entry = pipette_core::benchmark::Benchmark::from_toml(
            id,
            "benchmark_type = \"prefill_throughput\"\nparameter_prefill_tokens = 64\n",
        )?;
        stores
            .catalog
            .put_org_benchmark(&acme.org_id, &entry)
            .await?;
        let plan = dir.path().join("plan.toml");
        std::fs::write(
            &plan,
            format!(
                r#"benchmarks = ["{id}"]

[[variants]]
requires = ["os:macos"]
models   = [{{ type = "gguf_text", source = "huggingface", org = "o", repo_name = "r", path = "a.gguf" }}]
runtimes = [{{ type = "llamacpp_cli_stock_tools", source = "github_release", repository_url = "github.com/ggml-org/llama.cpp", version = "b9050", flavor = "macos-arm64" }}]
"#
            ),
        )?;

        let now = chrono::Utc::now();
        let campaign = Campaign {
            campaign_id: CampaignId::mint(),
            name: NonEmptyTrimmedString::try_new("Nightly")?,
            owner: CampaignOwner::Org {
                org_id: acme.org_id.clone(),
            },
            current_plan_id: None,
            labels: Default::default(),
            state: Default::default(),
            created_at: now,
            updated_at: now,
        };
        stores.campaigns.put_campaign(&campaign).await?;

        handle_campaign_action(
            CampaignAction::Ingest {
                campaign_id: campaign.campaign_id,
                plan,
                note: None,
                strict_models: false,
                dry_run: false,
            },
            &stores,
            IngestSettings {
                models: ModelCheck::none(),
                max_jobs: Config::default().plan_max_jobs,
                creating_max_age: std::time::Duration::from_secs(21_600),
                import: None,
            },
        )
        .await?;
        assert_eq!(stores.todo.list_avail().await?.len(), 1);
        Ok(())
    }

    /// A typo'd entry is refused by name rather than silently a no-op.
    #[rstest]
    #[case::show(true)]
    #[case::delete(false)]
    #[tokio::test]
    async fn org_benchmarks_refuse_an_unknown_entry(#[case] show: bool) -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        seed_public_catalog(&dir)?;
        create_org(&dir, "acme").await?;
        let org = OrgRef::Slug(slug("acme")?);
        let benchmark_id = BenchmarkId::try_new("eval_nope")?;
        let action = if show {
            OrgBenchmarkAction::Show { org, benchmark_id }
        } else {
            OrgBenchmarkAction::Delete { org, benchmark_id }
        };

        let refused = org_benchmarks(&dir, action)
            .await
            .err()
            .context("an unknown entry must be refused")?
            .to_string();
        assert!(refused.contains("eval_nope"), "{refused}");
        Ok(())
    }

    /// A private benchmark refuses the delete, readable or not (it counts by
    /// key), and `--detach` refuses before detaching anything.
    #[rstest]
    #[case::written(false)]
    #[case::unreadable(true)]
    #[tokio::test]
    async fn deleting_an_organization_refuses_its_private_benchmarks(
        #[case] unreadable: bool,
    ) -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        seed_public_catalog(&dir)?;
        let org = create_org(&dir, "acme").await?;
        let acme = || -> anyhow::Result<OrgRef> { Ok(OrgRef::Slug(slug("acme")?)) };
        let delete_org = || async {
            handle_org_action(
                OrgAction::Delete {
                    org: acme()?,
                    detach: true,
                },
                reopen(&dir)?,
            )
            .await
        };
        let member = member_of(&reopen(&dir)?, &org).await?;
        let id = BenchmarkId::try_new("eval_acme_internal_v3")?;
        if unreadable {
            // A field the current definition no longer takes.
            let entries = dir.path().join("org-benchmarks").join(org.org_id.as_str());
            std::fs::create_dir_all(&entries)?;
            std::fs::write(
                entries.join(format!("{id}.toml")),
                "benchmark_type = \"eval\"\nparameter_eval_id = \"acme_internal\"\n\
                 parameter_dataset_name = \"v3\"\nparameter_max_tokens = 4096\n\
                 parameter_retired = 1\n",
            )?;
        } else {
            org_benchmarks(
                &dir,
                OrgBenchmarkAction::Put {
                    org: acme()?,
                    file: an_entry_file(&dir, id.as_str())?,
                },
            )
            .await?;
        }

        let refused = delete_org()
            .await
            .err()
            .context("a private benchmark must refuse the delete")?
            .to_string();
        assert!(refused.contains("1 private benchmark(s)"), "{refused}");
        assert!(refused.contains(id.as_str()), "{refused}");
        let order = ["orgs benchmarks delete", "--detach"].map(|step| refused.find(step));
        assert!(
            matches!(order, [Some(first), Some(then)] if first < then),
            "{refused}"
        );
        let stores = reopen(&dir)?;
        assert!(stores.orgs.get_org(&org.org_id).await?.is_some());
        assert_eq!(
            stores.orgs.get_membership(&member.user_id).await?,
            Some(org.org_id.clone()),
            "refused before anything was detached"
        );

        org_benchmarks(
            &dir,
            OrgBenchmarkAction::Delete {
                org: acme()?,
                benchmark_id: id,
            },
        )
        .await?;
        delete_org().await?;
        assert!(reopen(&dir)?.orgs.get_org(&org.org_id).await?.is_none());
        Ok(())
    }

    /// What an organization owns that `--detach` does not clear.
    #[derive(Debug, Clone, Copy)]
    enum Kept {
        Campaign,
        VaultResource,
        PreauthKey,
    }

    /// Each refuses `orgs delete --detach` before anything is detached.
    #[rstest]
    #[case::campaign(Kept::Campaign, "campaigns owner")]
    #[case::vault_resource(Kept::VaultResource, "vault delete")]
    #[case::preauth_key(Kept::PreauthKey, "clients preauth revoke")]
    #[tokio::test]
    async fn deleting_an_organization_refuses_what_detach_cannot_clear(
        #[case] kept: Kept,
        #[case] step: &str,
    ) -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let org = create_org(&dir, "acme").await?;
        let stores = reopen(&dir)?;
        let member = member_of(&stores, &org).await?;
        let org_id = org.org_id.clone();
        let id = match kept {
            Kept::Campaign => {
                let campaign = NewCampaign::new(
                    NonEmptyTrimmedString::try_new("nightly")?,
                    CampaignOwner::Org { org_id },
                )
                .around(PlanId::from_uuid(uuid::Uuid::now_v7()), chrono::Utc::now());
                stores.campaigns.put_campaign(&campaign).await?;
                campaign.campaign_id.to_string()
            }
            Kept::VaultResource => uploaded_for(&stores, VaultOwner::Org { org_id })
                .await?
                .to_string(),
            Kept::PreauthKey => {
                let minted = preauth_create(Some(OrgRef::Slug(slug("acme")?)), None);
                handle_client_preauth_action(minted, &stores).await?;
                let keys = stores.clients.list_client_preauth_keys().await?;
                keys.first().context("the key")?.key_id.to_string()
            }
        };

        let delete = OrgAction::Delete {
            org: OrgRef::Slug(slug("acme")?),
            detach: true,
        };
        let refused = handle_org_action(delete, stores.clone())
            .await
            .err()
            .with_context(|| format!("{kept:?} must refuse the delete"))?
            .to_string();
        assert!(refused.contains(&id), "{refused}");
        let order = [step, "--detach"].map(|each| refused.find(each));
        assert!(
            matches!(order, [Some(first), Some(then)] if first < then),
            "{refused}"
        );
        assert_eq!(
            stores.orgs.get_membership(&member.user_id).await?,
            Some(org.org_id),
            "refused before anything was detached"
        );
        Ok(())
    }

    /// A deleted organization's id still reaches entries left under it, to read
    /// and delete but not write; once empty it is refused like a typo.
    #[tokio::test]
    async fn org_benchmarks_reach_a_deleted_organizations_entries() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        seed_public_catalog(&dir)?;
        let org = create_org(&dir, "acme").await?;
        let id = BenchmarkId::try_new("eval_acme_internal_v3")?;
        let file = an_entry_file(&dir, id.as_str())?;
        org_benchmarks(
            &dir,
            OrgBenchmarkAction::Put {
                org: OrgRef::Slug(slug("acme")?),
                file: file.clone(),
            },
        )
        .await?;
        // A stranded keyspace: the record and handle go, the entry stays.
        let stores = reopen(&dir)?;
        stores.orgs.delete_org(&org.org_id).await?;
        stores.orgs.release_org_slug("acme").await?;
        let stranded = || OrgRef::Id(org.org_id.clone());

        org_benchmarks(
            &dir,
            OrgBenchmarkAction::List {
                org: Some(stranded()),
            },
        )
        .await?;
        org_benchmarks(
            &dir,
            OrgBenchmarkAction::Show {
                org: stranded(),
                benchmark_id: id.clone(),
            },
        )
        .await?;
        let refused = org_benchmarks(
            &dir,
            OrgBenchmarkAction::Put {
                org: stranded(),
                file,
            },
        )
        .await
        .err()
        .context("a write needs the record")?
        .to_string();
        assert!(refused.contains("no organization"), "{refused}");

        org_benchmarks(
            &dir,
            OrgBenchmarkAction::Delete {
                org: stranded(),
                benchmark_id: id,
            },
        )
        .await?;
        assert!(
            reopen(&dir)?
                .catalog
                .list_org_benchmark_ids()
                .await?
                .is_empty()
        );
        let refused = org_benchmarks(
            &dir,
            OrgBenchmarkAction::List {
                org: Some(stranded()),
            },
        )
        .await
        .err()
        .context("an id with no record and no entries names nothing")?
        .to_string();
        assert!(refused.contains("no organization"), "{refused}");
        Ok(())
    }

    /// Entries are labelled by handle, or by id once the record is gone.
    #[tokio::test]
    async fn org_benchmarks_list_every_organization_and_what_does_not_read() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        seed_public_catalog(&dir)?;
        let empty = org_benchmark_listing(&reopen(&dir)?, None).await?;
        assert_eq!(empty, ["No organization keeps a private benchmark."]);

        create_org(&dir, "acme").await?;
        let labs = create_org(&dir, "labs").await?;
        org_benchmarks(
            &dir,
            OrgBenchmarkAction::Put {
                org: OrgRef::Slug(slug("acme")?),
                file: an_entry_file(&dir, "eval_acme_internal_v3")?,
            },
        )
        .await?;
        let entries = dir.path().join("org-benchmarks").join(labs.org_id.as_str());
        std::fs::create_dir_all(&entries)?;
        std::fs::write(
            entries.join("eval_labs_retired.toml"),
            "benchmark_type = \"eval\"\nparameter_retired = 1\n",
        )?;
        let gone = OrgId::mint();
        let stores = reopen(&dir)?;
        let stranded = pipette_core::benchmark::Benchmark::from_toml(
            "prefill_throughput_gone",
            "benchmark_type = \"prefill_throughput\"\nparameter_prefill_tokens = 64\n",
        )?;
        stores.catalog.put_org_benchmark(&gone, &stranded).await?;

        let every = org_benchmark_listing(&stores, None).await?;
        let [header, acme_row, labs_row, gone_row] = every.as_slice() else {
            anyhow::bail!("a header and three rows: {every:?}");
        };
        assert_eq!(header, "3 private benchmark(s) across 3 organization(s):");
        let cells = |row: &str| {
            row.split('|')
                .map(str::trim)
                .map(String::from)
                .collect::<Vec<_>>()
        };
        assert_eq!(cells(acme_row), ["acme", "eval_acme_internal_v3", "eval"]);
        let labs_cells = cells(labs_row);
        assert_eq!(labs_cells[..2], ["labs", "eval_labs_retired"]);
        assert!(labs_cells[2].starts_with("unreadable: "), "{labs_row}");
        assert!(labs_row.contains("parameter_retired"), "{labs_row}");
        assert_eq!(
            cells(gone_row),
            [
                gone.as_str(),
                "prefill_throughput_gone",
                "prefill_throughput"
            ]
        );

        let one = org_benchmark_listing(&stores, Some(&OrgRef::Slug(slug("labs")?))).await?;
        let [header, row] = one.as_slice() else {
            anyhow::bail!("a header and one row: {one:?}");
        };
        assert_eq!(header, "1 private benchmark(s) in labs:");
        assert!(
            row.contains("eval_labs_retired") && row.contains("unreadable: "),
            "{row}"
        );
        Ok(())
    }

    /// Clearing takes `--none`, not an absent argument.
    #[tokio::test]
    async fn a_membership_names_an_organization_that_exists() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let org = create_org(&dir, "acme").await?;
        let stores = reopen(&dir)?;
        let user = pipette_core::user::User::try_new(email("op@example.com")?, chrono::Utc::now())?;
        stores.users.put_user(&user).await?;

        assert!(
            handle_user_action(
                UserAction::SetOrg {
                    user_id: user.user_id.clone(),
                    org: Some(OrgRef::Slug(slug("nobody")?)),
                    none: false,
                },
                reopen(&dir)?,
            )
            .await
            .is_err(),
            "a membership must not name an organization that does not exist"
        );

        handle_user_action(
            UserAction::SetOrg {
                user_id: user.user_id.clone(),
                org: Some(OrgRef::Slug(slug("acme")?)),
                none: false,
            },
            reopen(&dir)?,
        )
        .await?;
        assert_eq!(
            reopen(&dir)?.orgs.get_membership(&user.user_id).await?,
            Some(org.org_id)
        );

        handle_user_action(
            UserAction::SetOrg {
                user_id: user.user_id.clone(),
                org: None,
                none: true,
            },
            reopen(&dir)?,
        )
        .await?;
        assert_eq!(
            reopen(&dir)?.orgs.get_membership(&user.user_id).await?,
            None
        );
        Ok(())
    }

    /// An ownership names a record that exists, and clearing leaves the device
    /// owned by nobody: what `org::owning_org` relies on.
    #[rstest]
    #[case::missing_org("org")]
    #[case::missing_user("user")]
    #[tokio::test]
    async fn an_ownership_names_a_record_that_exists(#[case] kind: &str) -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let org = create_org(&dir, "acme").await?;
        let stores = reopen(&dir)?;
        let client = flag_reindex_client("ev1_rack", "aa")?;
        stores.clients.put_client(&client).await?;

        let missing = match kind {
            "org" => ClientOwnerAction::Set {
                client_id: client.client_id.clone(),
                org: Some(OrgRef::Slug(slug("nobody")?)),
                user: None,
            },
            _ => ClientOwnerAction::Set {
                client_id: client.client_id.clone(),
                org: None,
                user: Some(UserId::mint()),
            },
        };
        assert!(
            handle_client_owner_action(missing, &reopen(&dir)?)
                .await
                .is_err(),
            "an ownership must not name a record that does not exist"
        );

        handle_client_owner_action(
            ClientOwnerAction::Set {
                client_id: client.client_id.clone(),
                org: Some(OrgRef::Slug(slug("acme")?)),
                user: None,
            },
            &reopen(&dir)?,
        )
        .await?;
        assert_eq!(
            reopen(&dir)?.orgs.get_ownership(&client.client_id).await?,
            Some(ClientOwner::Org {
                org_id: org.org_id.clone()
            })
        );

        handle_client_owner_action(
            ClientOwnerAction::Clear {
                client_id: client.client_id.clone(),
            },
            &reopen(&dir)?,
        )
        .await?;
        assert_eq!(
            reopen(&dir)?.orgs.get_ownership(&client.client_id).await?,
            None
        );
        Ok(())
    }

    fn preauth_create(
        owner_org: Option<OrgRef>,
        owner_user: Option<UserId>,
    ) -> ClientPreauthAction {
        ClientPreauthAction::Create {
            multi_use: false,
            expires_in: None,
            no_expiry: false,
            tags: Vec::new(),
            permissions: Vec::new(),
            no_permissions: false,
            org: None,
            owner_org,
            owner_user,
            note: None,
        }
    }

    #[rstest]
    #[case::missing_org("org")]
    #[case::missing_user("user")]
    #[tokio::test]
    async fn a_preauth_key_refuses_an_owner_that_does_not_exist(
        #[case] kind: &str,
    ) -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        create_org(&dir, "acme").await?;
        let missing = match kind {
            "org" => preauth_create(Some(OrgRef::Slug(slug("nobody")?)), None),
            _ => preauth_create(None, Some(UserId::mint())),
        };
        assert!(
            handle_client_preauth_action(missing, &reopen(&dir)?)
                .await
                .is_err(),
            "a key must not name an owner that does not exist"
        );
        assert!(
            reopen(&dir)?
                .clients
                .list_client_preauth_keys()
                .await?
                .is_empty()
        );
        Ok(())
    }

    /// Every spelling of an owner stores the id it resolves to, in both commands.
    #[rstest]
    #[case::org_handle("org_handle")]
    #[case::org_id("org_id")]
    #[case::user("user")]
    #[tokio::test]
    async fn an_owner_flag_stores_the_record_it_names(
        #[case] spelling: &str,
    ) -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let org = create_org(&dir, "acme").await?;
        let stores = reopen(&dir)?;
        handle_user_action(
            UserAction::Create {
                adopt_stranded_claim: false,
                email: email("owner@example.test")?,
                permissions: Vec::new(),
                group: PermissionGroupFlags::default(),
                account_type: UserAccountType::Human,
                org: None,
                campaigns: Vec::new(),
            },
            stores.clone(),
        )
        .await?;
        let user_id = stores
            .users
            .list_users()
            .await?
            .pop()
            .context("the account must exist")?
            .user_id;

        let org_owner = ClientOwner::Org {
            org_id: org.org_id.clone(),
        };
        let (owner_org, owner_user, expected) = match spelling {
            "org_handle" => (Some(OrgRef::Slug(slug("acme")?)), None, org_owner),
            "org_id" => (Some(OrgRef::Id(org.org_id.clone())), None, org_owner),
            _ => (None, Some(user_id.clone()), ClientOwner::User { user_id }),
        };

        handle_client_preauth_action(
            preauth_create(owner_org.clone(), owner_user.clone()),
            &stores,
        )
        .await?;
        let keys = stores.clients.list_client_preauth_keys().await?;
        let [key] = keys.as_slice() else {
            anyhow::bail!("one minted key must list as one record, got {}", keys.len());
        };
        assert_eq!(key.default_owner.as_ref(), Some(&expected));
        let seeds = client_preauth_row(key, chrono::Utc::now()).seeds;
        assert!(
            seeds.contains(&format!("owner: {}", expected.label())),
            "{seeds}"
        );

        let client = flag_reindex_client("ev1_rack", "aa")?;
        stores.clients.put_client(&client).await?;
        handle_client_owner_action(
            ClientOwnerAction::Set {
                client_id: client.client_id.clone(),
                org: owner_org,
                user: owner_user,
            },
            &stores,
        )
        .await?;
        assert_eq!(
            stores.orgs.get_ownership(&client.client_id).await?,
            Some(expected)
        );
        Ok(())
    }

    /// The names are stored unexpanded (implication is read-time), and no flag
    /// writes no object at all rather than an empty one.
    #[rstest]
    #[case::grants_what_it_was_given(
        vec![UserPermission::SubmitAnyPlans, UserPermission::ReadAnyClients],
        Some(serde_json::json!(["read_any_clients", "submit_any_plans"]))
    )]
    #[case::no_flag_writes_no_object(vec![], None)]
    #[case::stores_the_grant_not_the_typing(
        vec![
            UserPermission::SubmitAnyPlans,
            UserPermission::SubmitPlans,
            UserPermission::ReadPlans,
        ],
        Some(serde_json::json!(["submit_any_plans"]))
    )]
    #[tokio::test]
    async fn create_writes_the_grant_it_was_given(
        #[case] permissions: Vec<UserPermission>,
        #[case] expected: Option<serde_json::Value>,
    ) -> anyhow::Result<()> {
        let (_dir, stores) = user_stores()?;
        let auth = stores.users.clone();

        handle_user_action(
            UserAction::Create {
                adopt_stranded_claim: false,
                email: email("op@example.com")?,
                permissions,
                group: PermissionGroupFlags::default(),
                account_type: UserAccountType::Human,
                org: None,
                campaigns: Vec::new(),
            },
            stores,
        )
        .await?;

        let user = auth
            .list_users()
            .await?
            .pop()
            .context("the account must exist")?;
        let stored = auth.get_user_permissions(&user.user_id).await?;
        assert_eq!(
            stored.map(|set| serde_json::to_value(&set)).transpose()?,
            expected
        );
        Ok(())
    }

    #[tokio::test]
    async fn approve_admits_a_requested_account_and_grants_with_it() -> anyhow::Result<()> {
        let (_dir, stores) = user_stores()?;
        let auth = stores.users.clone();

        // What the keyless `POST /api/v1/users/register` leaves.
        let requested =
            pipette_core::user::User::requested(email("asker@example.com")?, chrono::Utc::now())?;
        auth.claim_user_email(requested.contact_email.as_str(), &requested.user_id)
            .await?;
        auth.put_user(&requested).await?;

        handle_user_action(
            UserAction::Approve {
                user_id: requested.user_id.clone(),
                permissions: vec![UserPermission::SubmitPlans],
                group: PermissionGroupFlags::default(),
            },
            stores,
        )
        .await?;

        let admitted = auth
            .get_user(&requested.user_id)
            .await?
            .context("the account must still exist")?;
        assert_eq!(admitted.status, UserStatus::Active);
        assert_eq!(
            auth.get_user_permissions(&requested.user_id).await?,
            Some(UserPermissionSet::from_iter([UserPermission::SubmitPlans]))
        );
        Ok(())
    }

    /// `set-type` changes the type and nothing else; a service account needs
    /// `--org`.
    #[tokio::test]
    async fn an_account_is_created_and_retyped_as_a_service() -> anyhow::Result<()> {
        let (dir, stores) = user_stores()?;
        let acme = create_org(&dir, "acme").await?;
        let auth = stores.users.clone();
        let address = email("ops+ci@example.test")?;
        let create = |org| UserAction::Create {
            adopt_stranded_claim: false,
            email: address.clone(),
            permissions: vec![UserPermission::ReadPlans],
            group: PermissionGroupFlags::default(),
            account_type: UserAccountType::Service,
            org,
            campaigns: Vec::new(),
        };
        let refused = handle_user_action(create(None), stores.clone())
            .await
            .err()
            .context("a service account with no organization is refused")?;
        assert!(refused.to_string().contains("pass --org"), "{refused}");
        assert!(auth.list_users().await?.is_empty(), "nothing was created");

        handle_user_action(create(Some(OrgRef::Slug(slug("acme")?))), stores.clone()).await?;
        let robot = auth
            .list_users()
            .await?
            .pop()
            .context("the account was created")?;
        assert_eq!(robot.account_type, UserAccountType::Service);
        assert_eq!(
            stores.orgs.get_membership(&robot.user_id).await?,
            Some(acme.org_id)
        );

        let retype = |account_type| UserAction::SetType {
            user_id: robot.user_id.clone(),
            account_type,
        };
        handle_user_action(retype(UserAccountType::Human), stores.clone()).await?;
        let retyped = auth
            .get_user(&robot.user_id)
            .await?
            .context("the account still exists")?;
        assert_eq!(
            retyped,
            pipette_core::user::User {
                account_type: UserAccountType::Human,
                ..robot.clone()
            }
        );
        handle_user_action(retype(UserAccountType::Human), stores.clone()).await?;
        assert_eq!(auth.get_user(&robot.user_id).await?, Some(retyped));
        Ok(())
    }

    /// A membership write the host refuses.
    #[derive(Debug, Clone, Copy)]
    enum RefusedMembership {
        CreateIntoDisabled,
        MoveIntoDisabled,
        ReleaseService,
        RetypeUnaffiliated,
        DetachService,
    }

    /// No one joins a disabled organization, no service account leaves one, and
    /// each refusal writes nothing.
    #[rstest]
    #[case::create_into_disabled(RefusedMembership::CreateIntoDisabled, "is disabled")]
    #[case::move_into_disabled(RefusedMembership::MoveIntoDisabled, "is disabled")]
    #[case::release_a_service(RefusedMembership::ReleaseService, "move it to another")]
    #[case::retype_the_unaffiliated(RefusedMembership::RetypeUnaffiliated, "`users set-org` first")]
    #[case::detach_a_service(RefusedMembership::DetachService, "does not release")]
    #[tokio::test]
    async fn the_host_refuses_a_membership_it_would_not_keep(
        #[case] write: RefusedMembership,
        #[case] says: &str,
    ) -> anyhow::Result<()> {
        let (dir, stores) = user_stores()?;
        let acme = create_org(&dir, "acme").await?;
        create_org(&dir, "closed").await?;
        let closed = || -> anyhow::Result<OrgRef> { Ok(OrgRef::Slug(slug("closed")?)) };
        handle_org_action(OrgAction::Disable { org: closed()? }, stores.clone()).await?;
        let auth = stores.users.clone();
        let create = |address: &str, account_type, org| -> anyhow::Result<UserAction> {
            Ok(UserAction::Create {
                adopt_stranded_claim: false,
                email: email(address)?,
                permissions: Vec::new(),
                group: PermissionGroupFlags::default(),
                account_type,
                org,
                campaigns: Vec::new(),
            })
        };
        let service = UserAccountType::Service;
        let in_acme = Some(OrgRef::Slug(slug("acme")?));
        handle_user_action(create("ci@example.test", service, in_acme)?, stores.clone()).await?;
        let robot = auth.list_users().await?.pop().context("the service")?;
        let loner =
            pipette_core::user::User::try_new(email("op@example.test")?, chrono::Utc::now())?;
        auth.put_user(&loner).await?;

        let refused = match write {
            RefusedMembership::CreateIntoDisabled => {
                let new = create("new@example.test", UserAccountType::Human, Some(closed()?))?;
                handle_user_action(new, stores.clone()).await
            }
            RefusedMembership::MoveIntoDisabled => {
                let moved = UserAction::SetOrg {
                    user_id: robot.user_id.clone(),
                    org: Some(closed()?),
                    none: false,
                };
                handle_user_action(moved, stores.clone()).await
            }
            RefusedMembership::ReleaseService => {
                let released = UserAction::SetOrg {
                    user_id: robot.user_id.clone(),
                    org: None,
                    none: true,
                };
                handle_user_action(released, stores.clone()).await
            }
            RefusedMembership::RetypeUnaffiliated => {
                let retyped = UserAction::SetType {
                    user_id: loner.user_id.clone(),
                    account_type: service,
                };
                handle_user_action(retyped, stores.clone()).await
            }
            RefusedMembership::DetachService => {
                let deleted = OrgAction::Delete {
                    org: OrgRef::Slug(slug("acme")?),
                    detach: true,
                };
                handle_org_action(deleted, stores.clone()).await
            }
        }
        .err()
        .with_context(|| format!("{write:?} must be refused"))?
        .to_string();
        assert!(refused.contains(says), "{refused}");

        assert_eq!(auth.list_users().await?.len(), 2, "nobody was created");
        assert_eq!(
            stores.orgs.get_membership(&robot.user_id).await?,
            Some(acme.org_id.clone()),
            "the service account stays in its organization"
        );
        assert_eq!(stores.orgs.get_membership(&loner.user_id).await?, None);
        assert_eq!(
            auth.get_user(&loner.user_id)
                .await?
                .map(|user| user.account_type),
            Some(UserAccountType::Human)
        );
        assert!(stores.orgs.get_org(&acme.org_id).await?.is_some());
        Ok(())
    }

    /// The campaign a case names: `acme`'s, `labs`'s, or none that exists.
    #[derive(Debug)]
    enum Named {
        Acme,
        Labs,
        Unknown,
    }

    /// `users create --campaign` and `users set-campaigns` make a campaign
    /// reader only of a service account holding nothing else, over campaigns
    /// that exist and that its organization owns.
    #[rstest::rstest]
    #[case::person_created(Some(UserAccountType::Human), vec![], Named::Acme, "a campaign reader is a service account")]
    #[case::wider_created(Some(UserAccountType::Service), vec![UserPermission::SubmitPlans], Named::Acme, "a campaign reader cannot hold submit_plans")]
    #[case::foreign_created(Some(UserAccountType::Service), vec![], Named::Labs, "its own organization's campaigns alone")]
    #[case::unknown_set(None, vec![], Named::Unknown, "no campaign campaign_")]
    #[case::wider_set(None, vec![UserPermission::SubmitPlans], Named::Acme, "which a campaign reader cannot")]
    #[case::foreign_set(None, vec![], Named::Labs, "its own organization's campaigns alone")]
    #[tokio::test]
    async fn a_campaign_reader_is_a_service_reading_existing_campaigns(
        #[case] created_as: Option<UserAccountType>,
        #[case] held: Vec<UserPermission>,
        #[case] named: Named,
        #[case] says: &str,
    ) -> anyhow::Result<()> {
        let (dir, stores) = user_stores()?;
        let acme = create_org(&dir, "acme").await?;
        let labs = create_org(&dir, "labs").await?;
        let team_campaign = |org_id: &OrgId| -> anyhow::Result<Campaign> {
            Ok(NewCampaign::new(
                NonEmptyTrimmedString::try_new("nightly")?,
                CampaignOwner::Org {
                    org_id: org_id.clone(),
                },
            )
            .around(PlanId::from_uuid(uuid::Uuid::now_v7()), chrono::Utc::now()))
        };
        let campaign = team_campaign(&acme.org_id)?;
        let theirs = team_campaign(&labs.org_id)?;
        stores.campaigns.put_campaign(&campaign).await?;
        stores.campaigns.put_campaign(&theirs).await?;
        let named = match named {
            Named::Acme => campaign.campaign_id.clone(),
            Named::Labs => theirs.campaign_id.clone(),
            Named::Unknown => CampaignId::mint(),
        };
        let create =
            |address: &str, account_type, permissions, campaigns| -> anyhow::Result<UserAction> {
                Ok(UserAction::Create {
                    adopt_stranded_claim: false,
                    email: email(address)?,
                    permissions,
                    group: PermissionGroupFlags::default(),
                    account_type,
                    org: Some(OrgRef::Slug(slug("acme")?)),
                    campaigns,
                })
            };

        let refused = match created_as {
            Some(account_type) => {
                let action = create("publish@example.test", account_type, held, vec![named])?;
                handle_user_action(action, stores.clone()).await
            }
            None => {
                let service = create(
                    "publish@example.test",
                    UserAccountType::Service,
                    held.clone(),
                    vec![],
                )?;
                handle_user_action(service, stores.clone()).await?;
                let robot = stores.users.list_users().await?.pop().context("created")?;
                let set = UserAction::SetCampaigns {
                    user_id: robot.user_id,
                    campaigns: vec![named],
                };
                handle_user_action(set, stores.clone()).await
            }
        }
        .err()
        .context("refused")?
        .to_string();
        assert!(refused.contains(says), "{refused}");
        assert!(
            stores
                .users
                .list_users()
                .await?
                .iter()
                .all(|user| user.campaign_scope.is_none()),
            "no reader was made"
        );

        let reader = create(
            "reader@example.test",
            UserAccountType::Service,
            vec![UserPermission::ReadPlans],
            vec![campaign.campaign_id.clone()],
        )?;
        handle_user_action(reader, stores.clone()).await?;
        let scopes: Vec<_> = stores
            .users
            .list_users()
            .await?
            .into_iter()
            .filter_map(|user| user.campaign_scope)
            .collect();
        assert_eq!(scopes, vec![BTreeSet::from([campaign.campaign_id])]);
        Ok(())
    }

    /// No path grants a service account an admin name, nor retypes an admin.
    #[tokio::test]
    async fn a_service_account_is_granted_no_admin_name() -> anyhow::Result<()> {
        let (dir, stores) = user_stores()?;
        create_org(&dir, "acme").await?;
        let auth = stores.users.clone();
        let address = email("ops+ci@example.test")?;
        let acme = OrgRef::Slug(slug("acme")?);
        let create = |permissions| UserAction::Create {
            adopt_stranded_claim: false,
            email: address.clone(),
            permissions,
            group: PermissionGroupFlags::default(),
            account_type: UserAccountType::Service,
            org: Some(acme.clone()),
            campaigns: Vec::new(),
        };
        let refused = handle_user_action(create(vec![UserPermission::ManageOrgs]), stores.clone())
            .await
            .err()
            .context("an admin name refused at creation")?;
        assert!(refused.to_string().contains("cannot hold manage_orgs"));
        assert!(auth.list_users().await?.is_empty(), "nothing was created");

        handle_user_action(create(vec![]), stores.clone()).await?;
        let robot = auth.list_users().await?.pop().context("created")?;
        let add = handle_user_permission_action(
            UserPermissionAction::Add {
                user_id: robot.user_id.clone(),
                permissions: vec![UserPermission::ManageClients],
                group: PermissionGroupFlags::default(),
            },
            auth.clone(),
        )
        .await;
        assert!(add.is_err());
        let set = handle_user_permission_action(
            UserPermissionAction::Set {
                user_id: robot.user_id.clone(),
                permissions: vec![UserPermission::ManageClients],
                group: PermissionGroupFlags::default(),
            },
            auth.clone(),
        )
        .await;
        assert!(set.is_err());
        let approve = handle_user_action(
            UserAction::Approve {
                user_id: robot.user_id.clone(),
                permissions: vec![UserPermission::ManageClients],
                group: PermissionGroupFlags::default(),
            },
            stores.clone(),
        )
        .await;
        assert!(approve.is_err());
        assert_eq!(auth.get_user_permissions(&robot.user_id).await?, None);

        handle_user_action(
            UserAction::SetType {
                user_id: robot.user_id.clone(),
                account_type: UserAccountType::Human,
            },
            stores.clone(),
        )
        .await?;
        auth.put_user_permissions(
            &robot.user_id,
            &UserPermissionSet::from_iter([UserPermission::ManageOrgs]),
        )
        .await?;
        let retype = handle_user_action(
            UserAction::SetType {
                user_id: robot.user_id.clone(),
                account_type: UserAccountType::Service,
            },
            stores.clone(),
        )
        .await;
        assert!(
            retype
                .err()
                .context("an admin is not retyped")?
                .to_string()
                .contains("holds manage_orgs")
        );
        assert_eq!(
            auth.get_user(&robot.user_id).await?.map(|u| u.account_type),
            Some(UserAccountType::Human)
        );
        Ok(())
    }

    /// `approve` and `enable` each refuse the other's account by name.
    #[rstest]
    #[case::approve_refuses_a_disabled_account(
        UserStatus::Disabled,
        true,
        "admitted and then offboarded. Return it to service with `users enable"
    )]
    #[case::enable_refuses_a_pending_account(
        UserStatus::Pending,
        false,
        "nothing to return it to. Admit it with `users approve"
    )]
    #[tokio::test]
    async fn the_admission_verbs_do_not_stand_in_for_each_other(
        #[case] status: UserStatus,
        #[case] approve: bool,
        #[case] expected: &str,
    ) -> anyhow::Result<()> {
        let (_dir, stores) = user_stores()?;
        let auth = stores.users.clone();

        let user = pipette_core::user::User {
            status,
            ..pipette_core::user::User::try_new(email("op@example.com")?, chrono::Utc::now())?
        };
        auth.claim_user_email(user.contact_email.as_str(), &user.user_id)
            .await?;
        auth.put_user(&user).await?;

        let action = if approve {
            UserAction::Approve {
                user_id: user.user_id.clone(),
                permissions: Vec::new(),
                group: PermissionGroupFlags::default(),
            }
        } else {
            UserAction::Enable {
                user_id: user.user_id.clone(),
            }
        };
        let refused = handle_user_action(action, stores)
            .await
            .err()
            .context("the wrong verb must be refused")?;

        assert!(
            refused.to_string().contains(expected),
            "the refusal must name the verb that makes this decision: {refused}"
        );
        assert_eq!(
            auth.get_user(&user.user_id)
                .await?
                .context("the account must still exist")?
                .status,
            status,
            "a refused verb must not write the status anyway"
        );
        Ok(())
    }

    /// Adopting a claim a stopped `delete` left must not hand the new account
    /// the dead one's grant.
    #[tokio::test]
    async fn adopting_a_claim_does_not_inherit_the_stranded_grant() -> anyhow::Result<()> {
        let (_dir, stores) = user_stores()?;
        let auth = stores.users.clone();

        let stranded = UserId::try_new("user_0195c3e2-a4f3-7b8c-9d1e-2f3a4b5c6d7e")?;
        auth.claim_user_email("op@example.com", &stranded).await?;
        auth.put_user_permissions(
            &stranded,
            &UserPermissionSet::from_iter([UserPermission::SubmitAnyPlans]),
        )
        .await?;

        handle_user_action(
            UserAction::Create {
                adopt_stranded_claim: true,
                email: email("op@example.com")?,
                permissions: Vec::new(),
                group: PermissionGroupFlags::default(),
                account_type: UserAccountType::Human,
                org: None,
                campaigns: Vec::new(),
            },
            stores,
        )
        .await?;

        assert!(
            auth.get_user_permissions(&stranded).await?.is_none(),
            "the adopted account must hold nothing the operator did not grant it"
        );
        Ok(())
    }

    #[tokio::test]
    async fn create_refuses_a_second_account_for_one_address() -> anyhow::Result<()> {
        let (_dir, stores) = user_stores()?;
        let auth = stores.users.clone();

        handle_user_action(
            UserAction::Create {
                adopt_stranded_claim: false,
                email: email("op@example.com")?,
                permissions: Vec::new(),
                group: PermissionGroupFlags::default(),
                account_type: UserAccountType::Human,
                org: None,
                campaigns: Vec::new(),
            },
            stores.clone(),
        )
        .await?;
        let first = auth.list_users().await?;
        assert_eq!(first.len(), 1);

        let refused = handle_user_action(
            UserAction::Create {
                adopt_stranded_claim: false,
                email: email("op@example.com")?,
                permissions: Vec::new(),
                group: PermissionGroupFlags::default(),
                account_type: UserAccountType::Human,
                org: None,
                campaigns: Vec::new(),
            },
            stores,
        )
        .await;
        assert!(refused.is_err(), "a duplicate address must be refused");
        assert_eq!(
            auth.list_users().await?,
            first,
            "the refusal must not disturb the account that holds the address"
        );
        Ok(())
    }

    /// A stranded claim must be adoptable: `delete` by id cannot reach it, so the
    /// address would be burned.
    #[tokio::test]
    async fn create_adopts_a_claim_left_by_a_failed_create() -> anyhow::Result<()> {
        let (_dir, stores) = user_stores()?;
        let auth = stores.users.clone();

        let stranded = UserId::try_new("user_0195c3e2-a4f3-7b8c-9d1e-2f3a4b5c6d7e")?;
        auth.claim_user_email("op@example.com", &stranded).await?;
        assert!(auth.get_user(&stranded).await?.is_none());

        // Refused without the flag: it looks like a create still in flight.
        let refused = handle_user_action(
            UserAction::Create {
                adopt_stranded_claim: false,
                email: email("op@example.com")?,
                permissions: Vec::new(),
                group: PermissionGroupFlags::default(),
                account_type: UserAccountType::Human,
                org: None,
                campaigns: Vec::new(),
            },
            stores.clone(),
        )
        .await;
        assert!(
            refused.is_err(),
            "adopting a stranded claim must be deliberate"
        );
        assert!(auth.list_users().await?.is_empty());

        handle_user_action(
            UserAction::Create {
                adopt_stranded_claim: true,
                email: email("op@example.com")?,
                permissions: Vec::new(),
                group: PermissionGroupFlags::default(),
                account_type: UserAccountType::Human,
                org: None,
                campaigns: Vec::new(),
            },
            stores,
        )
        .await?;

        let users = auth.list_users().await?;
        assert_eq!(users.len(), 1);
        assert_eq!(
            users[0].user_id, stranded,
            "the adopted account must take the id the claim already named"
        );
        assert_eq!(
            auth.lookup_user_by_email("op@example.com").await?,
            Some(stranded),
            "the claim must still point at the account that now exists"
        );
        Ok(())
    }

    /// Delete reaps all three keys together.
    #[tokio::test]
    async fn delete_removes_the_record_claim_and_permissions() -> anyhow::Result<()> {
        let (_dir, stores) = user_stores()?;
        let auth = stores.users.clone();

        handle_user_action(
            UserAction::Create {
                adopt_stranded_claim: false,
                email: email("op@example.com")?,
                permissions: Vec::new(),
                group: PermissionGroupFlags::default(),
                account_type: UserAccountType::Human,
                org: None,
                campaigns: Vec::new(),
            },
            stores.clone(),
        )
        .await?;
        let user_id = auth.list_users().await?[0].user_id.clone();
        auth.put_user_permissions(
            &user_id,
            &UserPermissionSet::from_iter([UserPermission::SubmitPlans]),
        )
        .await?;

        handle_user_action(
            UserAction::Delete {
                detach: false,
                user_id: user_id.clone(),
            },
            stores,
        )
        .await?;

        assert!(auth.get_user(&user_id).await?.is_none());
        assert!(auth.lookup_user_by_email("op@example.com").await?.is_none());
        assert!(auth.get_user_permissions(&user_id).await?.is_none());
        Ok(())
    }

    /// `delete` reads the address off the record, so after a stopped teardown
    /// only `release-claim` can reach the claim.
    #[tokio::test]
    async fn release_claim_clears_a_claim_left_by_a_partial_teardown() -> anyhow::Result<()> {
        let (_dir, stores) = user_stores()?;
        let auth = stores.users.clone();

        handle_user_action(
            UserAction::Create {
                adopt_stranded_claim: false,
                email: email("op@example.com")?,
                permissions: Vec::new(),
                group: PermissionGroupFlags::default(),
                account_type: UserAccountType::Human,
                org: None,
                campaigns: Vec::new(),
            },
            stores.clone(),
        )
        .await?;
        let user_id = auth.list_users().await?[0].user_id.clone();
        auth.delete_user(&user_id).await?;
        assert_eq!(
            auth.lookup_user_by_email("op@example.com").await?,
            Some(user_id)
        );

        handle_user_action(
            UserAction::ReleaseClaim {
                email: email("op@example.com")?,
            },
            stores,
        )
        .await?;

        assert!(
            auth.lookup_user_by_email("op@example.com").await?.is_none(),
            "the stranded claim must be released"
        );
        Ok(())
    }

    /// Nothing else frees a keypair claim, so this is what makes a machine
    /// re-registrable after its account is gone.
    #[tokio::test]
    async fn deleting_an_account_clears_its_keys_and_frees_their_keypairs() -> anyhow::Result<()> {
        use pipette_core::key_index::KeyHolderPrincipalId;

        let (_dir, stores) = user_stores()?;
        let users = stores.users.clone();
        let index = stores.key_index.clone();

        handle_user_action(
            UserAction::Create {
                adopt_stranded_claim: false,
                email: email("op@example.com")?,
                permissions: Vec::new(),
                group: PermissionGroupFlags::default(),
                account_type: UserAccountType::Human,
                org: None,
                campaigns: Vec::new(),
            },
            stores.clone(),
        )
        .await?;
        let user_id = users.list_users().await?[0].user_id.clone();

        let digests: Vec<_> = claimed_keys(&stores, &user_id, &[0x11, 0x22])
            .await?
            .into_iter()
            .map(|(_, digest)| digest)
            .collect();

        handle_user_action(
            UserAction::Delete {
                detach: false,
                user_id: user_id.clone(),
            },
            stores.clone(),
        )
        .await?;

        assert!(users.get_user(&user_id).await?.is_none());
        assert!(
            users.list_user_keys(&user_id).await?.is_empty(),
            "the key tree goes with the account"
        );
        // The keyspace has no reader, so a fresh claim is the assertion.
        let successor = pipette_core::user::User::try_new(
            ContactEmail::try_new("successor@example.com")?,
            chrono::Utc::now(),
        )?
        .user_id;
        for digest in &digests {
            assert_eq!(
                index
                    .claim_public_key(digest, &KeyHolderPrincipalId::User(successor.clone()))
                    .await?,
                pipette_core::key_index::KeyClaim::Claimed,
                "every keypair the account held must be free again"
            );
        }
        Ok(())
    }

    async fn claimed_keys(
        stores: &Stores,
        user_id: &UserId,
        bytes: &[u8],
    ) -> anyhow::Result<Vec<(UserKeyId, pipette_core::key_index::PublicKeyDigest)>> {
        use pipette_core::key_index::{KeyHolderPrincipalId, digest_of};
        use pipette_plan_types::NonEmptyTrimmedString;
        use pipette_plan_types::PublicKeyHex;

        let mut held = Vec::new();
        for byte in bytes {
            let public_key = PublicKeyHex::try_new(hex::encode([*byte; PublicKeyHex::BYTE_LEN]))?;
            let key = UserKey::new(
                public_key.clone(),
                NonEmptyTrimmedString::try_new(format!("machine {byte}"))?,
                chrono::Utc::now(),
            );
            stores.users.put_user_key(user_id, &key).await?;
            let digest = digest_of(&public_key);
            stores
                .key_index
                .claim_public_key(&digest, &KeyHolderPrincipalId::User(user_id.clone()))
                .await?;
            held.push((key.key_id, digest));
        }
        Ok(held)
    }

    async fn account_with_keys(
        stores: &Stores,
        address: &str,
        bytes: &[u8],
    ) -> anyhow::Result<(
        UserId,
        Vec<(UserKeyId, pipette_core::key_index::PublicKeyDigest)>,
    )> {
        let user = pipette_core::user::User::try_new(email(address)?, chrono::Utc::now())?;
        stores.users.put_user(&user).await?;
        let held = claimed_keys(stores, &user.user_id, bytes).await?;
        Ok((user.user_id, held))
    }

    /// The stolen-key recovery: revoke while disabled, and `enable` brings back
    /// only the other key, registration and claim intact.
    #[rstest]
    #[case::active(false)]
    #[case::while_disabled(true)]
    #[tokio::test]
    async fn revoking_a_users_key_retires_it_and_frees_its_keypair(
        #[case] while_disabled: bool,
    ) -> anyhow::Result<()> {
        use pipette_core::key_index::{KeyClaim, KeyHolderPrincipalId};

        let (_dir, stores) = user_stores()?;
        let (user_id, held) = account_with_keys(&stores, "op@example.com", &[0x11, 0x22]).await?;
        let [(revoked, revoked_digest), (kept, kept_digest)] = held.as_slice() else {
            anyhow::bail!("two keys were registered");
        };

        handle_user_action(
            UserAction::Key {
                action: UserKeyAction::List {
                    user_id: user_id.clone(),
                },
            },
            stores.clone(),
        )
        .await?;
        if while_disabled {
            handle_user_action(
                UserAction::Disable {
                    user_id: user_id.clone(),
                },
                stores.clone(),
            )
            .await?;
        }
        handle_user_action(
            UserAction::Key {
                action: UserKeyAction::Revoke {
                    user_id: user_id.clone(),
                    key_id: revoked.clone(),
                },
            },
            stores.clone(),
        )
        .await?;
        if while_disabled {
            handle_user_action(
                UserAction::Enable {
                    user_id: user_id.clone(),
                },
                stores.clone(),
            )
            .await?;
        }

        assert_eq!(
            stores
                .users
                .get_user(&user_id)
                .await?
                .map(|user| user.status),
            Some(UserStatus::Active)
        );
        assert!(
            stores
                .users
                .get_user_key(&user_id, revoked)
                .await?
                .is_none()
        );
        assert!(stores.users.get_user_key(&user_id, kept).await?.is_some());
        let successor = KeyHolderPrincipalId::User(UserId::mint());
        assert_eq!(
            stores
                .key_index
                .claim_public_key(revoked_digest, &successor)
                .await?,
            KeyClaim::Claimed,
            "the revoked keypair is free again"
        );
        assert_eq!(
            stores.key_index.holder_of_public_key(kept_digest).await?,
            Some(KeyHolderPrincipalId::User(user_id.clone())),
            "the other keypair stays claimed"
        );
        Ok(())
    }

    #[tokio::test]
    async fn revoking_a_key_the_account_does_not_hold_is_refused() -> anyhow::Result<()> {
        use pipette_core::key_index::KeyHolderPrincipalId;

        let (_dir, stores) = user_stores()?;
        let (a, _) = account_with_keys(&stores, "a@example.com", &[]).await?;
        let (b, held) = account_with_keys(&stores, "b@example.com", &[0x33]).await?;
        let [(key_id, digest)] = held.as_slice() else {
            anyhow::bail!("one key was registered");
        };

        let refused = handle_user_action(
            UserAction::Key {
                action: UserKeyAction::Revoke {
                    user_id: a.clone(),
                    key_id: key_id.clone(),
                },
            },
            stores.clone(),
        )
        .await
        .err()
        .context("revoking b's key through a must fail")?;
        let message = format!("{refused:#}");
        assert!(
            message.contains("holds no key") && message.contains("`users key list"),
            "{message}"
        );
        assert!(stores.users.get_user_key(&b, key_id).await?.is_some());
        assert_eq!(
            stores.key_index.holder_of_public_key(digest).await?,
            Some(KeyHolderPrincipalId::User(b.clone()))
        );
        Ok(())
    }

    /// The device can register again, even as the other kind.
    #[tokio::test]
    async fn deleting_a_client_frees_its_keypair() -> anyhow::Result<()> {
        use pipette_core::key_index::{KeyHolderPrincipalId, digest_of};
        use pipette_plan_types::PublicKeyHex;

        let (_dir, stores) = user_stores()?;
        let index = stores.key_index.clone();

        let public_key = PublicKeyHex::try_new(hex::encode([0x33; PublicKeyHex::BYTE_LEN]))?;
        let client_id = ClientId::derive(&public_key);
        let digest = digest_of(&public_key);
        stores
            .clients
            .put_client(&pipette_core::client::Client {
                client_id: client_id.clone(),
                public_key,
                organization: pipette_plan_types::NonEmptyTrimmedString::try_new("org")?,
                client_details: pipette_plan_types::NonEmptyTrimmedString::try_new("d")?,
                contact_email: ContactEmail::try_new("d@example.com")?,
                status: pipette_core::client::ClientStatus::Approved,
                registered_at: chrono::Utc::now(),
                device_profile: Default::default(),
                capabilities: Default::default(),
            })
            .await?;
        index
            .claim_public_key(&digest, &KeyHolderPrincipalId::Client(client_id.clone()))
            .await?;

        handle_client_action(
            ClientAction::Delete {
                client_id: client_id.clone(),
            },
            stores.clone(),
        )
        .await?;

        assert!(stores.clients.get_client(&client_id).await?.is_none());
        let account = pipette_core::user::User::try_new(
            ContactEmail::try_new("rehomed@example.com")?,
            chrono::Utc::now(),
        )?
        .user_id;
        assert_eq!(
            index
                .claim_public_key(&digest, &KeyHolderPrincipalId::User(account))
                .await?,
            pipette_core::key_index::KeyClaim::Claimed,
            "the keypair must be free once the client that held it is gone"
        );
        Ok(())
    }

    /// Releasing a live account's address would put it on two accounts.
    #[tokio::test]
    async fn release_claim_refuses_an_address_a_live_account_holds() -> anyhow::Result<()> {
        let (_dir, stores) = user_stores()?;
        let auth = stores.users.clone();

        handle_user_action(
            UserAction::Create {
                adopt_stranded_claim: false,
                email: email("op@example.com")?,
                permissions: Vec::new(),
                group: PermissionGroupFlags::default(),
                account_type: UserAccountType::Human,
                org: None,
                campaigns: Vec::new(),
            },
            stores.clone(),
        )
        .await?;
        let user_id = auth.list_users().await?[0].user_id.clone();

        let refused = handle_user_action(
            UserAction::ReleaseClaim {
                email: email("op@example.com")?,
            },
            stores,
        )
        .await;

        assert!(
            refused.is_err(),
            "a live account's address must not be released"
        );
        assert_eq!(
            auth.lookup_user_by_email("op@example.com").await?,
            Some(user_id),
            "the refusal must leave the claim in place"
        );
        Ok(())
    }

    #[tokio::test]
    async fn delete_is_idempotent() -> anyhow::Result<()> {
        let (_dir, stores) = user_stores()?;

        handle_user_action(
            UserAction::Delete {
                detach: false,
                user_id: UserId::try_new("user_0195c3e2-a4f3-7b8c-9d1e-2f3a4b5c6d7e")?,
            },
            stores.clone(),
        )
        .await?;
        handle_user_action(
            UserAction::ReleaseClaim {
                email: email("nobody@example.com")?,
            },
            stores,
        )
        .await?;
        Ok(())
    }

    /// A file where `users/` goes fails the record write (`save_user` calls
    /// `create_dir_all` first).
    #[tokio::test]
    async fn create_releases_its_claim_when_the_record_cannot_be_written() -> anyhow::Result<()> {
        let (dir, stores) = user_stores()?;
        let auth = stores.users.clone();
        std::fs::write(dir.path().join("users"), b"not a directory")?;

        let failed = handle_user_action(
            UserAction::Create {
                adopt_stranded_claim: false,
                email: email("op@example.com")?,
                permissions: Vec::new(),
                group: PermissionGroupFlags::default(),
                account_type: UserAccountType::Human,
                org: None,
                campaigns: Vec::new(),
            },
            stores,
        )
        .await;
        assert!(failed.is_err(), "the record write must fail");
        assert!(
            auth.lookup_user_by_email("op@example.com").await?.is_none(),
            "the claim must be released, leaving the address usable"
        );
        Ok(())
    }

    /// Reachable when a claim and a record disagree, after a hand-edited store or
    /// an adopt that raced a create.
    #[tokio::test]
    async fn delete_leaves_a_claim_that_names_another_account() -> anyhow::Result<()> {
        let (_dir, stores) = user_stores()?;
        let auth = stores.users.clone();

        handle_user_action(
            UserAction::Create {
                adopt_stranded_claim: false,
                email: email("op@example.com")?,
                permissions: Vec::new(),
                group: PermissionGroupFlags::default(),
                account_type: UserAccountType::Human,
                org: None,
                campaigns: Vec::new(),
            },
            stores.clone(),
        )
        .await?;
        let user_id = auth.list_users().await?[0].user_id.clone();

        let other = UserId::try_new("user_0195c3e2-a4f3-7b8c-9d1e-2f3a4b5c6d7e")?;
        auth.release_user_email("op@example.com").await?;
        auth.claim_user_email("op@example.com", &other).await?;

        handle_user_action(
            UserAction::Delete {
                detach: false,
                user_id: user_id.clone(),
            },
            stores,
        )
        .await?;

        assert!(
            auth.get_user(&user_id).await?.is_none(),
            "the named account goes"
        );
        assert_eq!(
            auth.lookup_user_by_email("op@example.com").await?,
            Some(other),
            "a claim naming another account must survive this delete"
        );
        Ok(())
    }

    /// An address may carry the `user_` prefix, so one argument cannot take both.
    #[test]
    fn a_user_prefixed_address_is_an_address_and_not_an_id() -> anyhow::Result<()> {
        let raw = "user_ops@example.com";
        assert!(raw.starts_with(UserId::PREFIX));
        assert!(
            parse_contact_email(raw).is_ok(),
            "a `user_`-prefixed local part is a legal address"
        );
        assert!(
            parse_user_id(raw).is_err(),
            "and is not a legal id, so prefix dispatch would refuse it outright"
        );
        Ok(())
    }

    /// The address becomes a path and an object key, so a traversal never
    /// reaches the store.
    #[rstest]
    #[case::parent_dir("../../etc/passwd@example.com")]
    #[case::bare_slash("a/b@example.com")]
    #[case::dot_dot_local("..@example.com")]
    #[case::quoted_traversal("\"../..\"@example.com")]
    #[case::domain_traversal("op@../example.com")]
    fn parse_contact_email_refuses_a_path_traversal(#[case] raw: &str) {
        assert!(
            parse_contact_email(raw).is_err(),
            "{raw:?} must not become a key segment"
        );
    }

    #[test]
    fn parse_contact_email_folds_case() -> anyhow::Result<()> {
        assert_eq!(
            parse_contact_email("Operator@Example.COM")
                .map_err(anyhow::Error::msg)?
                .as_str(),
            "operator@example.com"
        );
        Ok(())
    }

    /// `pk_suffix` keeps seeded clients' public keys apart.
    fn flag_reindex_client(
        id: &str,
        pk_suffix: &str,
    ) -> anyhow::Result<pipette_core::client::Client> {
        Ok(pipette_core::client::Client {
            client_id: ClientId::try_new(id)?,
            public_key: pipette_plan_types::PublicKeyHex::try_new(format!(
                "abcdef0123456789abcdef0123456789abcdef0123456789abcdef01234567{pk_suffix}"
            ))?,
            organization: NonEmptyTrimmedString::try_new("org")?,
            client_details: NonEmptyTrimmedString::try_new("details")?,
            contact_email: pipette_plan_types::ContactEmail::try_new("a@b.com")?,
            status: ClientStatus::Approved,
            registered_at: chrono::Utc::now(),
            device_profile: Default::default(),
            capabilities: Default::default(),
        })
    }

    /// The flag count is the command's whole blast radius.
    #[rstest]
    #[case::named_client(Some("c-one"), false, &["c-one"])]
    #[case::all_clients(None, true, &["c-one", "c-two"])]
    #[tokio::test]
    async fn flag_reindex_flags_its_targets(
        #[case] client_id: Option<&str>,
        #[case] all: bool,
        #[case] expected: &[&str],
    ) -> anyhow::Result<()> {
        let (_dir, stores) = flag_reindex_stores()?;
        stores
            .clients
            .put_client(&flag_reindex_client("c-one", "01")?)
            .await?;
        stores
            .clients
            .put_client(&flag_reindex_client("c-two", "02")?)
            .await?;

        let target = client_id.map(ClientId::try_new).transpose()?;
        handle_flag_reindex(target, all, &stores).await?;

        let mut flagged: Vec<String> = stores
            .todo
            .list_pending_reindex()
            .await?
            .into_iter()
            .map(|(client_id, _key)| client_id.to_string())
            .collect();
        flagged.sort();
        assert_eq!(flagged, expected);
        Ok(())
    }

    #[tokio::test]
    async fn flag_reindex_rejects_unknown_client() -> anyhow::Result<()> {
        let (_dir, stores) = flag_reindex_stores()?;
        stores
            .clients
            .put_client(&flag_reindex_client("c-one", "01")?)
            .await?;

        let target = ClientId::try_new("c-absent")?;
        assert!(
            handle_flag_reindex(Some(target), false, &stores)
                .await
                .is_err()
        );
        assert!(
            stores.todo.list_pending_reindex().await?.is_empty(),
            "a refused flag-reindex must write nothing"
        );
        Ok(())
    }

    #[tokio::test]
    async fn flag_reindex_all_on_empty_roster_writes_nothing() -> anyhow::Result<()> {
        let (_dir, stores) = flag_reindex_stores()?;

        handle_flag_reindex(None, true, &stores).await?;

        assert!(stores.todo.list_pending_reindex().await?.is_empty());
        Ok(())
    }

    /// A command that changes nothing flags nobody, so it pauses no client.
    #[rstest]
    #[case::add(true, None, 2, &["lab"])]
    #[case::add_present(true, Some("lab"), 0, &["lab"])]
    #[case::remove(false, Some("lab"), 2, &[])]
    #[case::remove_absent(false, None, 0, &[])]
    #[tokio::test]
    async fn tag_changes_flag_the_client(
        #[case] add: bool,
        #[case] existing: Option<&str>,
        #[case] flags: usize,
        #[case] expected: &[&str],
    ) -> anyhow::Result<()> {
        let (_dir, stores) = flag_reindex_stores()?;
        let client = flag_reindex_client("c-one", "01")?;
        stores.clients.put_client(&client).await?;
        if let Some(tag) = existing {
            stores
                .tags
                .add_client_tag(&client.client_id, &Tag::try_new(tag)?)
                .await?;
        }
        let client_id = client.client_id.clone();
        let tags = vec!["lab".to_string()];
        let action = if add {
            TagAction::Add { client_id, tags }
        } else {
            TagAction::Remove { client_id, tags }
        };

        handle_tag_action(
            action,
            stores.clients.clone(),
            stores.tags.clone(),
            stores.todo.clone(),
        )
        .await?;

        assert_eq!(stores.todo.list_pending_reindex().await?.len(), flags);
        let carried: Vec<String> = stores
            .tags
            .get_client_tags(&client.client_id)
            .await?
            .iter()
            .map(ToString::to_string)
            .collect();
        assert_eq!(carried, expected);
        Ok(())
    }

    /// The forward marker landed, so the index is stale whatever the command
    /// returns.
    #[tokio::test]
    async fn tag_add_flags_the_client_when_a_write_fails() -> anyhow::Result<()> {
        let (dir, stores) = flag_reindex_stores()?;
        let client = flag_reindex_client("c-one", "01")?;
        stores.clients.put_client(&client).await?;
        // The forward marker lands and the reverse one fails.
        let by_tag = dir.path().join("tags-index").join("by-tag");
        std::fs::create_dir_all(&by_tag)?;
        std::fs::write(by_tag.join("lab"), b"")?;

        let added = handle_tag_action(
            TagAction::Add {
                client_id: client.client_id.clone(),
                tags: vec!["lab".to_string()],
            },
            stores.clients.clone(),
            stores.tags.clone(),
            stores.todo.clone(),
        )
        .await;

        assert!(added.is_err(), "the reverse marker write must fail");
        assert!(
            stores
                .tags
                .get_client_tags(&client.client_id)
                .await?
                .contains(&Tag::try_new("lab")?)
        );
        assert_eq!(stores.todo.list_pending_reindex().await?.len(), 2);
        Ok(())
    }

    fn held_failure_body(job_id: &str, retriable: bool) -> serde_json::Value {
        serde_json::json!({
            "message_type": "failure",
            "job_id": job_id,
            "client_id": "ev1_held",
            "benchmark_id": "prefill_throughput_256",
            "benchmark_type": "prefill_throughput",
            "submitted_at": chrono::Utc::now().to_rfc3339(),
            "retriable": retriable,
            "failure_reason": "thermal throttle",
            "runtime_stderr": "throttled at 94C\n",
        })
    }

    /// A retriable failure is archived unrecorded, a terminal one re-staged; both
    /// leave the unverified tree. `tests/unverified.rs` covers a success.
    #[rstest]
    #[case::terminal_failure(false, JobState::Incoming)]
    #[case::retriable_failure(true, JobState::Processed)]
    #[tokio::test]
    async fn promote_archives_a_held_retriable_failure_instead_of_recording_it(
        #[case] retriable: bool,
        #[case] want_state: JobState,
    ) -> anyhow::Result<()> {
        let (_dir, stores) = flag_reindex_stores()?;
        let client_id = ClientId::try_new("ev1_held")?;
        let job_id = JobId::try_new("job-held")?;
        stores
            .submissions
            .write_unverified(
                &client_id,
                &job_id,
                &held_failure_body(job_id.as_str(), retriable),
            )
            .await?;

        handle_unverified_action(
            UnverifiedAction::Promote {
                client_id: client_id.clone(),
                dry_run: false,
            },
            stores,
        )
        .await?;

        let config = Config {
            storage: StorageConfig::LocalFs {
                data_dir: _dir.path().to_path_buf(),
            },
            ..Default::default()
        };
        let stores = build_local_fs_stores(&config)?;
        let record = stores
            .submissions
            .find_job(&job_id)
            .await?
            .context("expected the promoted body to be findable")?;
        assert_eq!(record.state, want_state);
        assert!(
            stores
                .submissions
                .list_unverified_client(&client_id)
                .await?
                .is_empty(),
            "the held copy must be gone either way"
        );
        assert!(
            record.body.get("runtime_stderr").is_none(),
            "{}",
            record.body
        );
        assert_eq!(
            runtime_output_text(&*stores.submissions, &job_id).await?,
            "== ev1_held runtime_stderr ==\nthrottled at 94C\n"
        );
        Ok(())
    }

    #[derive(Clone, Copy)]
    enum Victim {
        Incoming,
        Processed,
        Avail,
        Leased,
        Source,
    }

    fn victim_body(job_id: &JobId) -> anyhow::Result<serde_json::Value> {
        let mut victim = held_failure_body(job_id.as_str(), false);
        victim["client_id"] = serde_json::json!("ev1_victim");
        victim
            .as_object_mut()
            .context("victim body is an object")?
            .remove("runtime_stderr");
        Ok(victim)
    }

    #[rstest]
    #[case::terminal_over_incoming(false, Victim::Incoming)]
    #[case::terminal_over_processed(false, Victim::Processed)]
    #[case::terminal_over_avail(false, Victim::Avail)]
    #[case::terminal_over_leased(false, Victim::Leased)]
    #[case::terminal_over_job_source(false, Victim::Source)]
    #[case::retriable_over_incoming(true, Victim::Incoming)]
    #[case::retriable_over_processed(true, Victim::Processed)]
    #[case::retriable_over_avail(true, Victim::Avail)]
    #[tokio::test]
    async fn promote_keeps_a_held_submission_whose_job_exists(
        #[case] retriable: bool,
        #[case] victim_at: Victim,
    ) -> anyhow::Result<()> {
        let (_dir, stores) = flag_reindex_stores()?;
        let client_id = ClientId::try_new("ev1_held")?;
        let job_id = JobId::try_new("job-held")?;
        let victim = victim_body(&job_id)?;
        let want = match victim_at {
            Victim::Incoming => {
                stores.submissions.write_incoming(&job_id, &victim).await?;
                Some(JobState::Incoming)
            }
            Victim::Processed => {
                stores.submissions.write_processed(&job_id, &victim).await?;
                Some(JobState::Processed)
            }
            Victim::Source => {
                stores.plans.put_job_source(&job_id, &victim).await?;
                None
            }
            Victim::Avail | Victim::Leased => {
                stores.todo.write_tmp(&job_id, &victim).await?;
                stores.todo.promote_avail(&job_id, ExpiresAt::Never).await?;
                if matches!(victim_at, Victim::Leased) {
                    let claim = stores
                        .todo
                        .claim_job(
                            &job_id,
                            ExpiresAt::Never,
                            &ClientId::try_new("ev1_victim")?,
                            chrono::Utc::now() + chrono::Duration::hours(1),
                        )
                        .await?;
                    anyhow::ensure!(matches!(claim, ClaimResult::Claimed(_)));
                }
                None
            }
        };
        stores
            .submissions
            .write_unverified(
                &client_id,
                &job_id,
                &held_failure_body(job_id.as_str(), retriable),
            )
            .await?;

        handle_unverified_action(
            UnverifiedAction::Promote {
                client_id: client_id.clone(),
                dry_run: false,
            },
            stores.clone(),
        )
        .await?;

        let record = stores.submissions.find_job(&job_id).await?;
        assert_eq!(
            record.map(|r| (r.state, r.body)),
            want.map(|state| (state, victim))
        );
        assert_eq!(
            stores
                .submissions
                .list_unverified_client(&client_id)
                .await?
                .len(),
            1,
            "the refused held copy must stay for the operator"
        );
        assert_eq!(
            stores.submissions.list_runtime_output(&job_id).await?,
            vec![]
        );
        Ok(())
    }

    /// The create-only write catches a record that lands after the existence check.
    #[tokio::test]
    async fn promote_held_refuses_a_job_with_an_incoming_record() -> anyhow::Result<()> {
        let (_dir, stores) = flag_reindex_stores()?;
        let store = &*stores.submissions;
        let job_id = JobId::try_new("job-held")?;
        let victim = victim_body(&job_id)?;
        store.write_incoming(&job_id, &victim).await?;

        let promotion = promote_held(
            store,
            &ClientId::try_new("ev1_held")?,
            &job_id,
            held_failure_body(job_id.as_str(), false),
        )
        .await?;

        assert_eq!(promotion, HeldPromotion::Refused);
        let record = store.find_job(&job_id).await?;
        assert_eq!(
            record.map(|r| (r.state, r.body)),
            Some((JobState::Incoming, victim))
        );
        Ok(())
    }

    #[tokio::test]
    async fn runtime_output_text_prints_each_clients_streams() -> anyhow::Result<()> {
        let (_dir, stores) = flag_reindex_stores()?;
        let job_id = JobId::try_new("job-out")?;
        let store = &*stores.submissions;
        store
            .write_runtime_output(
                &job_id,
                &ClientId::try_new("ev1_b")?,
                &RuntimeOutput {
                    runtime_stderr: Some("OOM".to_owned()),
                    ..RuntimeOutput::default()
                },
            )
            .await?;
        store
            .write_runtime_output(
                &job_id,
                &ClientId::try_new("ev1_a")?,
                &RuntimeOutput {
                    runtime_stdout: Some("ready\n".to_owned()),
                    runtime_stderr: Some(String::new()),
                },
            )
            .await?;

        assert_eq!(
            runtime_output_text(store, &job_id).await?,
            "== ev1_a runtime_stdout ==\nready\n\
             == ev1_a runtime_stderr ==\n\
             == ev1_b runtime_stderr ==\nOOM\n"
        );
        let missing = runtime_output_text(store, &JobId::try_new("job-none")?).await;
        assert!(
            missing.is_err_and(|e| e.to_string() == "no runtime output for job job-none"),
            "a job with no output must be an error"
        );
        Ok(())
    }

    /// `set` with no names clears; `remove` takes a user name, to clear one
    /// written before the partition.
    #[rstest]
    #[case::add_one(&["pipette-mgmt", "clients", "permission", "add", "c-one", "perform_work"])]
    #[case::add_many(
        &["pipette-mgmt", "clients", "permission", "add", "c-one", "perform_work", "submit_results"]
    )]
    #[case::add_by_tag(&["pipette-mgmt", "clients", "permission", "add", "--tag", "lab", "perform_work"])]
    #[case::remove(&["pipette-mgmt", "clients", "permission", "remove", "c-one", "perform_work"])]
    #[case::remove_a_user_name(
        &["pipette-mgmt", "clients", "permission", "remove", "c-one", "submit_plans"]
    )]
    #[case::set(&["pipette-mgmt", "clients", "permission", "set", "c-one", "perform_work"])]
    #[case::set_clears(&["pipette-mgmt", "clients", "permission", "set", "c-one"])]
    #[case::list(&["pipette-mgmt", "clients", "permission", "list", "c-one"])]
    #[case::approve_with_grant(
        &["pipette-mgmt", "clients", "approve", "c-one", "--permission", "perform_work"]
    )]
    #[case::columns(&["pipette-mgmt", "clients", "list", "--columns", "client_id,permissions"])]
    #[case::client_preauth_template(
        &["pipette-mgmt", "clients", "preauth", "create", "--permission", "perform_work"]
    )]
    #[case::client_preauth_empty_template(
        &["pipette-mgmt", "clients", "preauth", "create", "--no-permissions"]
    )]
    fn cli_accepts_the_permission_spellings(#[case] argv: &[&str]) {
        assert!(Cli::try_parse_from(argv).is_ok(), "{argv:?}");
    }

    #[rstest]
    #[case::add_nothing(&["pipette-mgmt", "clients", "permission", "add"])]
    #[case::remove_no_names(&["pipette-mgmt", "clients", "permission", "remove", "c-one"])]
    #[case::unknown_column(&["pipette-mgmt", "clients", "list", "--columns", "permission"])]
    #[case::unknown_permission_flag(
        &["pipette-mgmt", "clients", "approve", "c-one", "--permission", "perform_wrok"]
    )]
    #[case::unknown_permission_positional(
        &["pipette-mgmt", "clients", "permission", "remove", "c-one", "perform_wrok"]
    )]
    #[case::user_permission_on_approve(
        &["pipette-mgmt", "clients", "approve", "c-one", "--permission", "submit_plans"]
    )]
    #[case::user_permission_on_set(
        &["pipette-mgmt", "clients", "permission", "set", "c-one", "submit_plans"]
    )]
    #[case::user_permission_on_a_preauth_template(
        &["pipette-mgmt", "clients", "preauth", "create", "--permission", "read_clients"]
    )]
    #[case::template_and_no_template(
        &[
            "pipette-mgmt",
            "clients",
            "preauth",
            "create",
            "--permission",
            "perform_work",
            "--no-permissions"
        ]
    )]
    fn cli_refuses_the_contradictory_permission_spellings(#[case] argv: &[&str]) {
        assert!(Cli::try_parse_from(argv).is_err(), "{argv:?}");
    }

    #[test]
    fn cli_names_the_typo_and_the_vocabulary() -> anyhow::Result<()> {
        let message = Cli::try_parse_from([
            "pipette-mgmt",
            "clients",
            "approve",
            "c-one",
            "--permission",
            "perform_wrok",
        ])
        .err()
        .context("a typo must fail at parse")?
        .to_string();
        assert!(
            message.contains("perform_wrok") && message.contains("perform_work"),
            "the message must name both the typo and the vocabulary: {message}"
        );
        Ok(())
    }

    /// A name of the wrong kind says which kind holds it, not that it is unknown.
    #[test]
    fn cli_names_the_kind_that_holds_a_permission_a_client_cannot() -> anyhow::Result<()> {
        let message = Cli::try_parse_from([
            "pipette-mgmt",
            "clients",
            "approve",
            "c-one",
            "--permission",
            "submit_plans",
        ])
        .err()
        .context("a user permission must fail at parse")?
        .to_string();
        assert!(
            message.contains("submit_plans")
                && message.contains("user permission")
                && message.contains("cannot hold"),
            "the message must name the permission and the kind that holds it: {message}"
        );
        assert!(
            !message.contains("unknown permission"),
            "a correctly spelled permission must not read as a typo: {message}"
        );
        Ok(())
    }

    /// Deliberately not aliased; the error kind is asserted, since a bare
    /// `is_err` would also hold for an unrelated parse failure.
    #[test]
    fn cli_refuses_the_old_top_level_preauth_spelling() {
        let kind = Cli::try_parse_from(["pipette-mgmt", "preauth", "create"])
            .err()
            .map(|e| e.kind());
        assert_eq!(kind, Some(clap::error::ErrorKind::InvalidSubcommand));
    }

    /// The stored object, which the server reads, not the printed lines.
    async fn stored_names(
        client_store: &Arc<dyn ClientStore>,
        client_id: &ClientId,
    ) -> anyhow::Result<Vec<String>> {
        Ok(client_store
            .get_client_permissions(client_id)
            .await?
            .map(|set| set.iter().map(|p| p.to_string()).collect())
            .unwrap_or_default())
    }

    #[tokio::test]
    async fn client_permission_actions_round_trip() -> anyhow::Result<()> {
        let (_dir, stores) = flag_reindex_stores()?;
        let client = flag_reindex_client("c-one", "01")?;
        stores.clients.put_client(&client).await?;
        let id = client.client_id.clone();

        // No object grants nothing; no default fills it in.
        assert!(stores.clients.get_client_permissions(&id).await?.is_none());
        assert!(
            client_admin::resolved_permissions(stores.clients.as_ref(), &id)
                .await?
                .is_empty(),
            "an absent object must grant nothing"
        );

        handle_permission_action(
            PermissionAction::Add {
                tag: None,
                args: vec!["c-one".into(), "perform_work".into()],
            },
            stores.clients.clone(),
            stores.tags.clone(),
        )
        .await?;
        assert_eq!(stored_names(&stores.clients, &id).await?, ["perform_work"]);

        handle_permission_action(
            PermissionAction::Add {
                tag: None,
                args: vec!["c-one".into(), "submit_results".into()],
            },
            stores.clients.clone(),
            stores.tags.clone(),
        )
        .await?;
        assert_eq!(
            stored_names(&stores.clients, &id).await?,
            ["perform_work", "submit_results"]
        );

        handle_permission_action(
            PermissionAction::Remove {
                client_id: ClientId::try_new("c-one")?,
                permissions: vec!["submit_results".into()],
            },
            stores.clients.clone(),
            stores.tags.clone(),
        )
        .await?;
        assert_eq!(stored_names(&stores.clients, &id).await?, ["perform_work"]);

        handle_permission_action(
            PermissionAction::Set {
                client_id: ClientId::try_new("c-one")?,
                permissions: vec![ClientPermission::SubmitResults],
            },
            stores.clients.clone(),
            stores.tags.clone(),
        )
        .await?;
        assert_eq!(
            stored_names(&stores.clients, &id).await?,
            ["submit_results"]
        );

        // An object holding `[]`, a different state from the absent object.
        handle_permission_action(
            PermissionAction::Set {
                client_id: ClientId::try_new("c-one")?,
                permissions: Vec::new(),
            },
            stores.clients.clone(),
            stores.tags.clone(),
        )
        .await?;
        assert!(stores.clients.get_client_permissions(&id).await?.is_some());
        assert!(stored_names(&stores.clients, &id).await?.is_empty());
        Ok(())
    }

    /// `add` is the one granting verb that parses names at run time.
    #[tokio::test]
    async fn a_client_grant_refuses_a_user_only_permission() -> anyhow::Result<()> {
        let (_dir, stores) = flag_reindex_stores()?;
        let client = flag_reindex_client("c-one", "01")?;
        stores.clients.put_client(&client).await?;
        let id = client.client_id.clone();

        let refusal = handle_permission_action(
            PermissionAction::Add {
                tag: None,
                args: vec!["c-one".into(), "submit_plans".into()],
            },
            stores.clients.clone(),
            stores.tags.clone(),
        )
        .await
        .err()
        .context("a client cannot hold a plan or fleet right")?;

        assert!(
            refusal.to_string().contains("cannot hold"),
            "unexpected refusal: {refusal}"
        );
        assert!(
            stores.clients.get_client_permissions(&id).await?.is_none(),
            "a refused grant must write nothing"
        );
        Ok(())
    }

    /// `add` normalizes the whole object, so a set that predates a widening grant
    /// is repaired by the write that widens it.
    #[tokio::test]
    async fn assigning_a_user_permission_stores_the_normalized_set() -> anyhow::Result<()> {
        let (_dir, stores) = user_stores()?;
        let auth = stores.users.clone();
        handle_user_action(
            UserAction::Create {
                adopt_stranded_claim: false,
                email: email("op@example.com")?,
                permissions: Vec::new(),
                group: PermissionGroupFlags::default(),
                account_type: UserAccountType::Human,
                org: None,
                campaigns: Vec::new(),
            },
            stores,
        )
        .await?;
        let user_id = auth
            .list_users()
            .await?
            .pop()
            .context("the account must exist")?
            .user_id;

        handle_user_permission_action(
            UserPermissionAction::Set {
                user_id: user_id.clone(),
                permissions: vec![UserPermission::SubmitPlans, UserPermission::ReadPlans],
                group: PermissionGroupFlags::default(),
            },
            auth.clone(),
        )
        .await?;
        assert_eq!(
            auth.get_user_permissions(&user_id)
                .await?
                .context("the grant must have been written")?
                .names(),
            ["submit_plans"],
            "`read_plans` is granted by `submit_plans`, so it is not stored beside it"
        );

        handle_user_permission_action(
            UserPermissionAction::Add {
                user_id: user_id.clone(),
                permissions: vec![UserPermission::SubmitAnyPlans],
                group: PermissionGroupFlags::default(),
            },
            auth.clone(),
        )
        .await?;
        let stored = auth
            .get_user_permissions(&user_id)
            .await?
            .context("the grant must survive the add")?;
        assert_eq!(
            stored.names(),
            ["submit_any_plans"],
            "the widening name subsumes the one already stored"
        );
        assert!(
            stored
                .resolved()
                .contains(&StoredUserPermission::from(UserPermission::SubmitPlans)),
            "normalizing must not narrow what the account may act on"
        );

        // The object holds the wide name, so removing the narrow one changes
        // nothing.
        handle_user_permission_action(
            UserPermissionAction::Remove {
                user_id: user_id.clone(),
                permissions: vec![UserPermission::SubmitPlans],
            },
            auth.clone(),
        )
        .await?;
        assert_eq!(
            auth.get_user_permissions(&user_id)
                .await?
                .context("the grant must survive the remove")?
                .names(),
            ["submit_any_plans"],
            "removing an implied name leaves the name that implies it"
        );
        Ok(())
    }

    /// A group's names are normalized on the way to storage.
    #[rstest]
    #[case::org_member(
        PermissionGroupFlags { org_member: true, org_admin: false, server_admin: false },
        &["cancel_plans", "edit_catalog", "read_clients", "submit_plans"]
    )]
    #[case::org_admin(
        PermissionGroupFlags { org_member: false, org_admin: true, server_admin: false },
        &["cancel_plans", "edit_catalog", "manage_clients", "manage_orgs", "submit_plans"]
    )]
    #[case::server_admin(
        PermissionGroupFlags { org_member: false, org_admin: false, server_admin: true },
        &["cancel_any_plans", "edit_any_catalog", "manage_any_clients", "manage_any_orgs",
          "read_any_clients", "read_any_plans", "submit_any_plans"]
    )]
    #[tokio::test]
    async fn a_permission_group_assigns_the_names_it_promises(
        #[case] group: PermissionGroupFlags,
        #[case] expected: &[&str],
    ) -> anyhow::Result<()> {
        let (_dir, stores) = user_stores()?;
        let auth = stores.users.clone();
        handle_user_action(
            UserAction::Create {
                adopt_stranded_claim: false,
                email: email("op@example.com")?,
                permissions: Vec::new(),
                group: PermissionGroupFlags::default(),
                account_type: UserAccountType::Human,
                org: None,
                campaigns: Vec::new(),
            },
            stores,
        )
        .await?;
        let user_id = auth
            .list_users()
            .await?
            .pop()
            .context("the account must exist")?
            .user_id;

        handle_user_permission_action(
            UserPermissionAction::Set {
                user_id: user_id.clone(),
                permissions: Vec::new(),
                group,
            },
            auth.clone(),
        )
        .await?;

        assert_eq!(
            auth.get_user_permissions(&user_id)
                .await?
                .context("the group must have been written")?
                .names(),
            expected
        );
        Ok(())
    }

    /// Not closed on the kind: an object written before the partition may hold a
    /// user name, and removal is how it is cleared.
    #[tokio::test]
    async fn removing_a_user_only_permission_from_a_client_is_allowed() -> anyhow::Result<()> {
        let (_dir, stores) = flag_reindex_stores()?;
        let client = flag_reindex_client("c-one", "01")?;
        stores.clients.put_client(&client).await?;

        handle_permission_action(
            PermissionAction::Remove {
                client_id: ClientId::try_new("c-one")?,
                permissions: vec!["submit_plans".into()],
            },
            stores.clients.clone(),
            stores.tags.clone(),
        )
        .await?;
        Ok(())
    }

    /// Every other fixture shares one directory, so only this one tells the
    /// `[storage]` and `[auth_storage]` roots apart.
    #[tokio::test]
    async fn tags_and_ownerships_land_in_storage_not_auth_storage() -> anyhow::Result<()> {
        let (data, auth) = (tempfile::tempdir()?, tempfile::tempdir()?);
        let stores = build_stores(&Config {
            storage: StorageConfig::local_fs(data.path().to_path_buf()),
            auth_storage: StorageConfig::local_fs(auth.path().to_path_buf()),
            ..Default::default()
        })
        .await?;

        let id = ClientId::try_new("ev1_rack")?;
        stores
            .tags
            .add_client_tag(&id, &Tag::try_new("lab")?)
            .await?;
        let owner = ClientOwner::Org {
            org_id: OrgId::mint(),
        };
        stores.orgs.set_ownership(&id, Some(owner)).await?;

        assert!(
            data.path()
                .join("tags-index/by-client/ev1_rack/lab")
                .exists()
        );
        assert!(data.path().join("client-owners/ev1_rack.json").exists());
        assert!(!auth.path().join("tags-index").exists());
        assert!(!auth.path().join("client-owners").exists());
        Ok(())
    }

    #[tokio::test]
    async fn client_permission_add_by_tag_grants_the_whole_fleet() -> anyhow::Result<()> {
        let (_dir, stores) = flag_reindex_stores()?;
        let lab = Tag::try_new("lab")?;
        for (id, suffix) in [("c-one", "01"), ("c-two", "02")] {
            let client = flag_reindex_client(id, suffix)?;
            stores.clients.put_client(&client).await?;
            stores.tags.add_client_tag(&client.client_id, &lab).await?;
        }
        let untagged = flag_reindex_client("c-three", "03")?;
        stores.clients.put_client(&untagged).await?;

        handle_permission_action(
            PermissionAction::Add {
                tag: Some("lab".into()),
                args: vec!["perform_work".into()],
            },
            stores.clients.clone(),
            stores.tags.clone(),
        )
        .await?;

        for id in ["c-one", "c-two"] {
            assert_eq!(
                stored_names(&stores.clients, &ClientId::try_new(id)?).await?,
                ["perform_work"],
                "{id} carries the tag and takes the grant"
            );
        }
        assert!(
            stores
                .clients
                .get_client_permissions(&untagged.client_id)
                .await?
                .is_none()
        );
        Ok(())
    }

    /// The status branch must not swallow the grant for an approved client.
    #[tokio::test]
    async fn approve_grants_permissions_to_an_already_approved_client() -> anyhow::Result<()> {
        let (_dir, stores) = flag_reindex_stores()?;
        let client = flag_reindex_client("c-one", "01")?;
        assert_eq!(client.status, ClientStatus::Approved);
        stores.clients.put_client(&client).await?;
        let auth = stores.clients.clone();

        handle_client_action(
            ClientAction::Approve {
                client_id: ClientId::try_new("c-one")?,
                permissions: vec![ClientPermission::PerformWork],
            },
            stores,
        )
        .await?;

        // Read back: an early return that skipped the grant would still succeed.
        let client_id = ClientId::try_new("c-one")?;
        assert_eq!(stored_names(&auth, &client_id).await?, ["perform_work"]);
        Ok(())
    }

    /// A typo is refused before the store, so a fleet is never half granted.
    #[tokio::test]
    async fn an_unknown_permission_name_aborts_before_the_write() -> anyhow::Result<()> {
        let (_dir, stores) = flag_reindex_stores()?;
        let client = flag_reindex_client("c-one", "01")?;
        stores.clients.put_client(&client).await?;

        let refused = handle_permission_action(
            PermissionAction::Add {
                tag: None,
                args: vec!["c-one".into(), "perform_wrok".into()],
            },
            stores.clients.clone(),
            stores.tags.clone(),
        )
        .await;
        let message = refused
            .err()
            .map(|e| e.to_string())
            .context("a typo must be refused")?;
        assert!(
            message.contains("perform_wrok") && message.contains("perform_work"),
            "the message must name both the typo and the vocabulary: {message}"
        );
        assert!(
            stores
                .clients
                .get_client_permissions(&client.client_id)
                .await?
                .is_none()
        );
        Ok(())
    }

    #[tokio::test]
    async fn add_without_a_permission_name_is_refused() -> anyhow::Result<()> {
        let (_dir, stores) = flag_reindex_stores()?;
        let client = flag_reindex_client("c-one", "01")?;
        stores.clients.put_client(&client).await?;

        assert!(
            handle_permission_action(
                PermissionAction::Add {
                    tag: None,
                    args: vec!["c-one".into()],
                },
                stores.clients.clone(),
                stores.tags.clone(),
            )
            .await
            .is_err()
        );
        Ok(())
    }

    /// A passed deadline reads `expired`, not a bare date.
    #[rstest]
    #[case::no_deadline(None, "active")]
    #[case::past_deadline(Some(-1), "expired")]
    #[case::future_deadline(Some(1), "active")]
    fn user_preauth_row_renders_the_stored_states(
        #[case] expires_in_hours: Option<i64>,
        #[case] status: &str,
    ) -> anyhow::Result<()> {
        let now = chrono::Utc::now();
        let minted = user_preauth::mint(
            UserPreauthParams {
                user_id: UserId::try_new("user_abc")?,
                usage: UserPreauthUsage::SingleUse,
                expires_at: expires_in_hours.map(|h| now + chrono::Duration::hours(h)),
                note: None,
            },
            now,
        )?;

        let row = user_preauth_row(&minted.key, now);

        assert!(
            row.lifecycle.starts_with(status),
            "the lifecycle cell must open with the status: {}",
            row.lifecycle
        );
        assert_eq!(row.note, "-", "an absent note must read as the empty glyph");
        Ok(())
    }

    fn user_key(
        expires_at: Option<chrono::DateTime<chrono::Utc>>,
    ) -> anyhow::Result<pipette_core::user_preauth::UserMintedKey> {
        Ok(user_preauth::mint(
            UserPreauthParams {
                user_id: UserId::try_new("user_abc")?,
                usage: UserPreauthUsage::SingleUse,
                expires_at,
                note: None,
            },
            chrono::Utc::now(),
        )?)
    }

    /// Expired records and spent markers go, the live key stays, and `--dry-run`
    /// touches nothing.
    #[tokio::test]
    async fn user_preauth_prune_sweeps_expired_keys_and_spent_markers() -> anyhow::Result<()> {
        let (_dir, stores) = flag_reindex_stores()?;
        let auth = stores.users.clone();
        let now = chrono::Utc::now();

        let expired = user_key(Some(now - chrono::Duration::hours(1)))?;
        auth.put_user_preauth_key(&expired.key).await?;
        let live = user_key(Some(now + chrono::Duration::days(30)))?;
        auth.put_user_preauth_key(&live.key).await?;

        // Spending leaves the marker without its record: the orphan to sweep.
        let spent = user_key(None)?;
        auth.put_user_preauth_key(&spent.key).await?;
        let (spent_id, secret) = spent
            .token
            .parse_parts()
            .context("the minted token must parse")?;
        assert!(matches!(
            auth.consume_user_preauth_key(&spent_id, &secret).await?,
            pipette_core::user_preauth::UserPreauthConsumeOutcome::Granted(_)
        ));

        handle_user_preauth_action(UserPreauthAction::Prune { dry_run: true }, auth.clone())
            .await?;
        assert_eq!(
            auth.list_user_preauth_keys().await?.len(),
            2,
            "a preview must delete nothing"
        );
        assert_eq!(auth.list_spent_user_preauth_markers().await?, [spent_id]);

        handle_user_preauth_action(UserPreauthAction::Prune { dry_run: false }, auth.clone())
            .await?;
        let surviving: Vec<_> = auth
            .list_user_preauth_keys()
            .await?
            .into_iter()
            .map(|k| k.key_id)
            .collect();
        assert_eq!(
            surviving,
            [live.key.key_id],
            "only the unexpired key may survive"
        );
        assert!(
            auth.list_spent_user_preauth_markers().await?.is_empty(),
            "the marker of a key with no record left must be swept"
        );
        Ok(())
    }

    /// Clap's own invariants (duplicate flags, conflicting settings).
    #[test]
    fn cli_definition_is_valid() {
        Cli::command().debug_assert();
    }
}
