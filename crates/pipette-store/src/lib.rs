//! Storage traits for the server's persistent keyspaces, and the types they
//! take and return. Opens no file or socket; [`http_stats`] and
//! [`decode_stats`] are counters the S3 backend fills.

pub mod decode_stats;
pub mod http_stats;

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::num::{NonZeroU64, NonZeroUsize};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use futures::stream::{StreamExt, TryStreamExt};
use serde::{Deserialize, Serialize};

use pipette_core::benchmark::{Benchmark, CatalogLoad};
use pipette_core::campaign::Campaign;
use pipette_core::client::{Client, MigrationRecord, SignatureMigration};
use pipette_core::client_permission::ClientPermissionSet;
use pipette_core::client_preauth::{
    ClientPreauthConsumeOutcome, ClientPreauthKey, ClientPreauthSecret,
};
use pipette_core::dataset::DatasetArtifact;
use pipette_core::denial::DenialRecord;
use pipette_core::eval_sample_result::EvalSampleResult;
use pipette_core::key_index::{KeyClaim, KeyHolderPrincipalId, PublicKeyDigest};
use pipette_core::model_params::ModelParams;
use pipette_core::organization::{Membership, Organization, Ownership};
use pipette_core::plan::{
    PlanHeader, PlanManifest, PlanNotes, PlanSource, PlanSummary, SettledOutcome,
};
use pipette_core::types::id_from_key;
use pipette_core::user::{User, UserKey};
use pipette_core::user_permission::UserPermissionSet;
use pipette_core::user_preauth::{UserPreauthConsumeOutcome, UserPreauthKey, UserPreauthSecret};
use pipette_core::vault::{VaultImport, VaultObjectKey, VaultOwner, VaultResource};
use pipette_core::warehouse::{JobMetrics, MetricRow};
use pipette_plan_types::{
    AuditActorKind, AuditEventId, AuditOutcome, AuditTarget, BenchmarkId, CampaignId, ClientId,
    ClientOwner, ClientPreauthKeyId, ExpiresAt, FirstClerkSignIn, IdError, JobId, ModelKey, OrgId,
    PlanId, PlanStatus, PublicKeyHex, RuntimeOutput, Sha256, Tag, UserId, UserKeyId,
    UserPreauthKeyId, VaultResourceId,
};
use pipette_plan_types::{
    AuditEvent, CatalogName, CatalogScope, DescriptorEntry, DescriptorEntryKind,
    DescriptorEntryState, DescriptorKind, DescriptorMember, DescriptorMemberKey, EntryError,
    ModelFormat, ModelType, NonEmptyVec, QuantizationLabel, RuntimeType,
};

/// Parse a persisted `queue-maintenance` key set; corrupt state reads as empty.
/// Empty is safe: each run rewrites the set wholesale, so corruption self-heals.
pub fn parse_key_set<T>(bytes: &[u8], what: &str) -> HashSet<T>
where
    T: serde::de::DeserializeOwned + Eq + std::hash::Hash,
{
    serde_json::from_slice(bytes).unwrap_or_else(|e| {
        tracing::warn!(error = %e, what = %what, "corrupt key-set state; treating as empty");
        HashSet::new()
    })
}

/// Parse a stored `queue-maintenance` snapshot, such as [`BacklogSnapshot`]
/// or [`CampaignBacklogSnapshot`]. Corrupt data reads as `None` until the next
/// run overwrites it.
pub fn parse_snapshot<T: serde::de::DeserializeOwned>(bytes: &[u8], what: &str) -> Option<T> {
    serde_json::from_slice(bytes)
        .inspect_err(|e| {
            tracing::warn!(error = %e, what = %what, "corrupt snapshot; treating as absent");
        })
        .ok()
}

/// Where a job sits in the pipeline. The variant names are the wire strings
/// clients read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, strum::IntoStaticStr)]
#[strum(serialize_all = "snake_case")]
pub enum JobState {
    Incoming,
    /// In a `score-queue/` stage, `parked` included; reported so a
    /// mid-pipeline read shows progress instead of a 404.
    Scoring,
    Processed,
}

impl JobState {
    pub fn as_str(self) -> &'static str {
        self.into()
    }
}

/// A stage of the `score-queue/` pipeline: `ToDo` awaits the scoring call,
/// `ToFinalize` holds the submission plus its score, awaiting the warehouse write.
/// `Failures` counts a `ToDo` or `ToFinalize` job's failed passes,
/// `{ attempts, error }`, and `Parked` holds, as `ToDo` does, the submission of
/// a job that failed too many to retry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, strum::IntoStaticStr)]
#[strum(serialize_all = "snake_case")]
pub enum ScoreQueueStage {
    ToDo,
    ToFinalize,
    Failures,
    Parked,
}

impl ScoreQueueStage {
    /// The layout is `submissions/score-queue/<leaf>/`.
    pub const ROOT: &'static str = "score-queue";

    pub fn leaf(self) -> &'static str {
        self.into()
    }
}

/// The leaf names are storage prefixes and the job states wire strings: a
/// variant rename would strand every queued job, silently.
#[cfg(test)]
#[test]
fn queue_stage_and_job_state_spellings_are_pinned() {
    assert_eq!(ScoreQueueStage::ToDo.leaf(), "to_do");
    assert_eq!(ScoreQueueStage::ToFinalize.leaf(), "to_finalize");
    assert_eq!(ScoreQueueStage::Failures.leaf(), "failures");
    assert_eq!(ScoreQueueStage::Parked.leaf(), "parked");
    assert_eq!(ScoreQueueStage::ROOT, "score-queue");
    assert_eq!(JobState::Incoming.as_str(), "incoming");
    assert_eq!(JobState::Scoring.as_str(), "scoring");
    assert_eq!(JobState::Processed.as_str(), "processed");
}

#[derive(Debug, Clone)]
pub struct SubmissionRecord {
    pub job_id: JobId,
    pub state: JobState,
    pub body: serde_json::Value,
}

/// The one field of a (possibly tens-of-MB) submission record its outcome needs.
/// Any JSON value is accepted, so one odd body cannot stop a plan's progress count.
#[derive(Debug, Deserialize)]
pub struct OutcomeProbe {
    message_type: Option<serde_json::Value>,
}

impl OutcomeProbe {
    pub fn outcome(&self) -> SettledOutcome {
        SettledOutcome::from_message_type(
            self.message_type
                .as_ref()
                .and_then(serde_json::Value::as_str),
        )
    }
}

/// The `to_finalize` payload, `{ submission, score }`, read for its outcome.
#[derive(Debug, Deserialize)]
pub struct FinalizeProbe {
    pub submission: OutcomeProbe,
}

/// [`SubmissionStore::find_outcomes`] for an object-store backend: one probe
/// per job, a job with no record left out, the first error stops the probes.
pub async fn find_outcomes_per_job<F, Fut>(
    job_ids: &[JobId],
    find_outcome: F,
) -> anyhow::Result<HashMap<JobId, SettledOutcome>>
where
    F: Fn(JobId) -> Fut + Send + Sync,
    Fut: Future<Output = anyhow::Result<Option<SettledOutcome>>> + Send,
{
    futures::stream::iter(job_ids.iter().cloned().map(|job_id| {
        let found = find_outcome(job_id.clone());
        async move { anyhow::Ok(found.await?.map(|outcome| (job_id, outcome))) }
    }))
    .buffer_unordered(STORAGE_CONCURRENCY)
    .try_filter_map(|found| async move { Ok(found) })
    .try_collect()
    .await
}

/// Outcome of [`SubmissionStore::prune_unverified`]. On a dry run `deleted`
/// counts what would be removed; `kept` counts objects newer than the threshold.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PruneSummary {
    pub deleted: usize,
    pub kept: usize,
}

/// The benchmark catalog: public entries under `benchmarks/` (cached), and
/// per-organization ones under `org-benchmarks/{org_id}/` (read per request).
/// Not a subdirectory of `benchmarks/`: the public loader lists recursively on
/// S3 and would serve every organization's entries to everyone.
#[async_trait]
pub trait CatalogStore: Send + Sync {
    async fn load_catalog(&self) -> anyhow::Result<HashMap<BenchmarkId, Benchmark>>;

    /// Every entry private to `org_id`; an unreadable entry is logged and
    /// skipped, as the public loader does.
    async fn load_org_catalog(
        &self,
        org_id: &OrgId,
    ) -> anyhow::Result<HashMap<BenchmarkId, Benchmark>> {
        let load = self.list_org_benchmarks(org_id).await?.into_iter().fold(
            CatalogLoad::default(),
            |mut load, (benchmark_id, entry)| {
                match entry {
                    Ok(benchmark) => {
                        load.catalog.insert(benchmark_id, benchmark);
                    }
                    Err(unreadable) => load.errors.push(unreadable.to_string()),
                }
                load
            },
        );
        load.log_errors();
        Ok(load.catalog)
    }

    /// One private entry. One that does not parse fails with an
    /// [`UnreadableObjectError`] root, so a caller can tell it from I/O.
    async fn get_org_benchmark(
        &self,
        org_id: &OrgId,
        benchmark_id: &BenchmarkId,
    ) -> anyhow::Result<Option<Benchmark>>;

    /// The body is [`Benchmark::to_toml`]; the path carries id and organization.
    async fn put_org_benchmark(&self, org_id: &OrgId, benchmark: &Benchmark) -> anyhow::Result<()>;

    /// Idempotent.
    async fn delete_org_benchmark(
        &self,
        org_id: &OrgId,
        benchmark_id: &BenchmarkId,
    ) -> anyhow::Result<()>;

    /// Which organization keeps which id: one listing, no entry read.
    async fn list_org_benchmark_ids(&self) -> anyhow::Result<Vec<(OrgId, BenchmarkId)>>;

    /// The ids `org_id` keeps, sorted; an entry too unreadable to serve still
    /// counts.
    async fn list_benchmark_ids_of(&self, org_id: &OrgId) -> anyhow::Result<Vec<BenchmarkId>> {
        let mut ids: Vec<BenchmarkId> = self
            .list_org_benchmark_ids()
            .await?
            .into_iter()
            .filter(|(holder, _)| holder == org_id)
            .map(|(_, benchmark_id)| benchmark_id)
            .collect();
        ids.sort();
        Ok(ids)
    }

    /// Every entry `org_id` keeps, sorted by id; unlike
    /// [`load_org_catalog`](Self::load_org_catalog) an unreadable one is reported.
    async fn list_org_benchmarks(&self, org_id: &OrgId) -> anyhow::Result<Vec<OrgCatalogEntry>> {
        let ids = self.list_benchmark_ids_of(org_id).await?;
        let reads: Vec<Option<OrgCatalogEntry>> = futures::stream::iter(ids)
            .map(|benchmark_id| async move {
                let read = read_org_benchmark(self, org_id, &benchmark_id).await?;
                Ok::<_, anyhow::Error>(read.map(|entry| (benchmark_id, entry)))
            })
            .buffered(STORAGE_CONCURRENCY)
            .try_collect()
            .await?;
        Ok(reads.into_iter().flatten().collect())
    }
}

pub type OrgCatalogEntry = (BenchmarkId, Result<Benchmark, UnreadableObjectError>);

/// One entry as a listing reads it: an unparseable one as its
/// [`UnreadableObjectError`], any other failed read fails the call.
pub async fn read_org_benchmark<C: CatalogStore + ?Sized>(
    catalog: &C,
    org_id: &OrgId,
    benchmark_id: &BenchmarkId,
) -> anyhow::Result<Option<Result<Benchmark, UnreadableObjectError>>> {
    match catalog.get_org_benchmark(org_id, benchmark_id).await {
        Ok(read) => Ok(read.map(Ok)),
        Err(e) => {
            let unreadable = e
                .downcast::<UnreadableObjectError>()
                .map_err(|e| e.context(format!("reading {org_id}'s benchmark {benchmark_id}")))?;
            Ok(Some(Err(unreadable)))
        }
    }
}

pub const ORG_BENCHMARKS_PREFIX: &str = "org-benchmarks";

/// `plans/{plan_id}.json`. A SQL store names a broken row by the key it would have.
pub const PLANS_PREFIX: &str = "plans/";

/// Outcome of [`ClientStore::mutate_client_permissions`]: whether the closure
/// changed the set, and the set as it now stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientPermissionMutation {
    pub changed: bool,
    pub resulting: ClientPermissionSet,
}

/// Outcome of [`UserStore::claim_user_email`]; `Taken` names the holder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EmailClaim {
    Claimed,
    Taken(UserId),
}

/// Outcome of [`UserStore::mutate_user_permissions`]. Not generic with
/// [`ClientPermissionMutation`], so a user's mutation never yields a client's set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserPermissionMutation {
    pub changed: bool,
    pub resulting: UserPermissionSet,
}

/// The client half of the auth store: device records, their permissions and
/// the pre-auth keys that register them.
#[async_trait]
pub trait ClientStore: Send + Sync {
    async fn get_client(&self, client_id: &ClientId) -> anyhow::Result<Option<Client>>;

    /// The records of `client_ids`, unordered, deduplicated. Best effort: an
    /// unread record is logged and left out, so a caller must not act on absence.
    async fn get_clients(&self, client_ids: &[ClientId]) -> anyhow::Result<Vec<Client>> {
        // Owned ids: a borrowed stream fails the future's `Send` bound.
        let unique: HashSet<ClientId> = client_ids.iter().cloned().collect();
        Ok(futures::stream::iter(unique)
            .map(|client_id| async move {
                self.get_client(&client_id)
                    .await
                    .inspect_err(|e| {
                        tracing::warn!(%client_id, error = %format!("{e:#}"), "a client record did not read");
                    })
                    .ok()
                    .flatten()
            })
            .buffer_unordered(STORAGE_CONCURRENCY)
            .filter_map(std::future::ready)
            .collect()
            .await)
    }

    async fn put_client(&self, client: &Client) -> anyhow::Result<()>;
    /// Write `updated` only if the stored record still equals `read`, else `false`.
    /// Not atomic by default: a backend with a conditional write overrides it.
    async fn replace_client(&self, read: &Client, updated: &Client) -> anyhow::Result<bool> {
        if self.get_client(&read.client_id).await?.as_ref() != Some(read) {
            return Ok(false);
        }
        self.put_client(updated).await?;
        Ok(true)
    }
    /// Delete a client record and its permissions object; its tags are the
    /// caller's to drop. Every set read also needs the record, and a
    /// re-registration rewrites the set before the record, so this reaps
    /// unreachable state rather than closing a grant.
    async fn delete_client(&self, client_id: &ClientId) -> anyhow::Result<()>;
    /// One page of `clients/` in key order, `after` exclusive. A record deleted
    /// mid-page is skipped; an invalid one is reported and takes no row.
    async fn list_clients(
        &self,
        after: Option<&ClientId>,
        limit: NonZeroUsize,
    ) -> anyhow::Result<ClientPage>;

    /// Every client id, ascending, from names alone (no record read).
    async fn list_client_ids(&self) -> anyhow::Result<Vec<ClientId>>;
    async fn has_public_key(&self, public_key: &PublicKeyHex) -> anyhow::Result<bool>;

    // ── Signature migration ─────────────────────────────────────────────────
    //
    // A write-once marker per client that has presented a verified `v1`
    // signature; it refuses that client the timestamp-only fallback. Not a
    // record field: `put_client` is a blind whole-record write.

    /// Whether `client_id` has ever presented a `v1` signature. A read failure
    /// propagates: a swallowed one would reopen the replayable fallback.
    async fn has_signature_migration(&self, client_id: &ClientId) -> anyhow::Result<bool>;

    /// Create-if-absent, so `first_seen` is the first sighting; an existing
    /// marker is [`MigrationRecord::Existing`], not an error.
    async fn record_signature_migration(
        &self,
        client_id: &ClientId,
        at: DateTime<Utc>,
    ) -> anyhow::Result<MigrationRecord>;

    /// Every recorded migration, for a display column, so unreadable entries
    /// are skipped (unlike [`Self::has_signature_migration`], which gates auth).
    async fn list_signature_migrations(
        &self,
    ) -> anyhow::Result<Vec<(ClientId, SignatureMigration)>>;

    // ── Permissions ─────────────────────────────────────────────────────────
    //
    // One object per client holding the whole set. Not a field on the client
    // record: the device also writes that record, blindly, and could restore a
    // permission an operator revoked between its read and its write.

    /// A principal's stored set. `None` grants nothing, like `Some(empty)`, but
    /// stays distinct so an operator can tell "never granted" from "cleared".
    async fn get_client_permissions(
        &self,
        client_id: &ClientId,
    ) -> anyhow::Result<Option<ClientPermissionSet>>;

    /// Every stored set, unordered, for [`pipette_core::matching::FleetRights`].
    /// An unreadable set is left out, never defaulted to a grant.
    async fn list_client_permissions(&self)
    -> anyhow::Result<Vec<(ClientId, ClientPermissionSet)>>;

    /// Blind overwrite, for registration, where there is nothing to race with.
    async fn put_client_permissions(
        &self,
        client_id: &ClientId,
        set: &ClientPermissionSet,
    ) -> anyhow::Result<()>;

    /// Idempotent.
    async fn delete_client_permissions(&self, client_id: &ClientId) -> anyhow::Result<()>;

    /// Read, apply `f` (which may be replayed), write back if changed. A
    /// conditional-write backend retries and errors on an exhausted budget;
    /// one without accepts last-writer-wins, tolerable as only operators write.
    async fn mutate_client_permissions(
        &self,
        client_id: &ClientId,
        f: &(dyn for<'s> Fn(&'s mut ClientPermissionSet) -> bool + Send + Sync),
    ) -> anyhow::Result<ClientPermissionMutation>;

    // ── Pre-auth keys ───────────────────────────────────────────────────────
    //
    // The record is never rewritten, only deleted. A spend is an exclusively
    // created `.spent` marker beside it: a single-object conditional create.

    async fn put_client_preauth_key(&self, key: &ClientPreauthKey) -> anyhow::Result<()>;

    /// A read that spends nothing, so registration can refuse a key before it
    /// claims the keypair.
    async fn get_client_preauth_key(
        &self,
        key_id: &ClientPreauthKeyId,
    ) -> anyhow::Result<Option<ClientPreauthKey>>;

    /// Verify `secret` and consume. A single-use key is spent by the exclusive
    /// marker create, so one concurrent consume wins; then the record is
    /// deleted, and if that fails the marker keeps it spent. `Err` is I/O only.
    async fn consume_client_preauth_key(
        &self,
        key_id: &ClientPreauthKeyId,
        secret: &ClientPreauthSecret,
    ) -> anyhow::Result<ClientPreauthConsumeOutcome>;

    async fn list_client_preauth_keys(&self) -> anyhow::Result<Vec<ClientPreauthKey>>;

    /// Idempotent.
    async fn delete_client_preauth_key(&self, key_id: &ClientPreauthKeyId) -> anyhow::Result<()>;

    /// Key ids carrying a spent marker, whether or not their record survives.
    async fn list_spent_client_preauth_markers(&self) -> anyhow::Result<Vec<ClientPreauthKeyId>>;

    /// Idempotent. Sound only once the key has no record: while one survives,
    /// the marker is all that keeps it spent.
    async fn delete_spent_client_preauth_marker(
        &self,
        key_id: &ClientPreauthKeyId,
    ) -> anyhow::Result<()>;
}

/// The user half of the auth store: operator accounts, their address claims,
/// permissions and pre-auth keys.
#[async_trait]
pub trait UserStore: Send + Sync {
    // ── User accounts ───────────────────────────────────────────────────────
    //
    // Not a flag on the client record: `list_clients` is the fleet the matcher
    // narrows. The caller claims the address before writing the record, so no
    // record exists for an address its account does not hold.

    /// The existence authority: every other key a user owns is inert once this
    /// returns `None`.
    async fn get_user(&self, user_id: &UserId) -> anyhow::Result<Option<User>>;

    /// Unconditional: after creation only an administrator writes this record,
    /// so there is no device-versus-operator lost update as with clients.
    async fn put_user(&self, user: &User) -> anyhow::Result<()>;

    /// Idempotent. Only the record; the caller reaps the address claim and
    /// permissions after it, so every remaining key is inert in one write.
    async fn delete_user(&self, user_id: &UserId) -> anyhow::Result<()>;

    /// Sorted by id, UUIDv7, so in creation order.
    async fn list_users(&self) -> anyhow::Result<Vec<User>>;

    /// An exclusive create, so exactly one of two racing creations wins. Call
    /// before [`Self::put_user`], or the race's loser leaves a record to clean up.
    async fn claim_user_email(&self, email: &str, user_id: &UserId) -> anyhow::Result<EmailClaim>;

    async fn lookup_user_by_email(&self, email: &str) -> anyhow::Result<Option<UserId>>;

    /// Idempotent. Runs after [`Self::delete_user`], so a crash strands the
    /// claim rather than freeing an address whose account still authenticates.
    async fn release_user_email(&self, email: &str) -> anyhow::Result<()>;

    // ── Registered keys ─────────────────────────────────────────────────────
    //
    // Registering claims the key in [`KeyIndexStore`] first; revoking deletes
    // the registration first, as authentication reads only the registration.

    /// Replaces any registration of the same key: the id derives from the key,
    /// and `key_details` is not identity.
    async fn put_user_key(&self, user_id: &UserId, key: &UserKey) -> anyhow::Result<()>;

    /// On the authentication path, and the binding check: the object exists
    /// under exactly one account, so a request naming another finds nothing.
    async fn get_user_key(
        &self,
        user_id: &UserId,
        key_id: &UserKeyId,
    ) -> anyhow::Result<Option<UserKey>>;

    /// Oldest first. Never on the authentication path: a cached listing would
    /// be a revocation that does not happen.
    async fn list_user_keys(&self, user_id: &UserId) -> anyhow::Result<Vec<UserKey>>;

    /// Idempotent. Revoking the last key, or the signing one, is permitted: a
    /// holder whose only key is compromised must be able to retire it at once.
    async fn delete_user_key(&self, user_id: &UserId, key_id: &UserKeyId) -> anyhow::Result<()>;

    // ── User permissions ────────────────────────────────────────────────────
    //
    // The client permission object, one keyspace over, so neither kind's delete
    // can reach the other's. Only administrators write it.

    /// `None` grants nothing, like `Some(empty)`, but stays distinct so an
    /// operator can tell "never granted" from "cleared".
    async fn get_user_permissions(
        &self,
        user_id: &UserId,
    ) -> anyhow::Result<Option<UserPermissionSet>>;

    /// Blind overwrite, for a caller that states the whole set.
    async fn put_user_permissions(
        &self,
        user_id: &UserId,
        set: &UserPermissionSet,
    ) -> anyhow::Result<()>;

    /// Idempotent. Account deletion removes the record first, so every
    /// remaining key is already inert.
    async fn delete_user_permissions(&self, user_id: &UserId) -> anyhow::Result<()>;

    /// Same contract and race guarantee as
    /// [`ClientStore::mutate_client_permissions`].
    async fn mutate_user_permissions(
        &self,
        user_id: &UserId,
        f: &(dyn for<'s> Fn(&'s mut UserPermissionSet) -> bool + Send + Sync),
    ) -> anyhow::Result<UserPermissionMutation>;

    // ── User pre-auth keys ──────────────────────────────────────────────────
    //
    // The client's pre-auth layout and spend race, over `user-preauth/`.

    async fn put_user_preauth_key(&self, key: &UserPreauthKey) -> anyhow::Result<()>;

    /// As [`ClientStore::consume_client_preauth_key`]. The grant names the
    /// account the key admits, not one the request chose.
    async fn consume_user_preauth_key(
        &self,
        key_id: &UserPreauthKeyId,
        secret: &UserPreauthSecret,
    ) -> anyhow::Result<UserPreauthConsumeOutcome>;

    async fn list_user_preauth_keys(&self) -> anyhow::Result<Vec<UserPreauthKey>>;

    /// Idempotent.
    async fn delete_user_preauth_key(&self, key_id: &UserPreauthKeyId) -> anyhow::Result<()>;

    /// Key ids carrying a spent marker, whether or not their record survives.
    async fn list_spent_user_preauth_markers(&self) -> anyhow::Result<Vec<UserPreauthKeyId>>;

    /// Sound only once the key has no record, as
    /// [`ClientStore::delete_spent_client_preauth_marker`].
    async fn delete_spent_user_preauth_marker(
        &self,
        key_id: &UserPreauthKeyId,
    ) -> anyhow::Result<()>;

    // ── First sign-in ───────────────────────────────────────────────────────
    //
    // Set at account creation, cleared by the first sign-in. Only a marked
    // account is handed a pre-auth key over HTTP.

    /// Idempotent.
    async fn mark_user_awaiting_sign_in(&self, user_id: &UserId) -> anyhow::Result<()>;

    async fn user_awaits_sign_in(&self, user_id: &UserId) -> anyhow::Result<bool>;

    /// Idempotent.
    async fn clear_user_awaiting_sign_in(&self, user_id: &UserId) -> anyhow::Result<()>;

    // ── First Clerk sign-in ─────────────────────────────────────────────────
    //
    // Written once, by the first sign-in through Clerk, so its owner is shown
    // the keys registered before it.

    /// An exclusive create, so of racing first sign-ins exactly one gets
    /// `true`; any other call leaves the record as it was.
    async fn record_user_first_clerk_sign_in(
        &self,
        user_id: &UserId,
        first: &FirstClerkSignIn,
    ) -> anyhow::Result<bool>;

    async fn get_user_first_clerk_sign_in(
        &self,
        user_id: &UserId,
    ) -> anyhow::Result<Option<FirstClerkSignIn>>;

    /// Idempotent.
    async fn delete_user_first_clerk_sign_in(&self, user_id: &UserId) -> anyhow::Result<()>;
}

/// Outcome of [`OrgStore::claim_org_slug`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SlugClaim {
    Claimed,
    Taken(OrgId),
}

/// The organization roster, its slug index and its two relations, in
/// `[storage]`: they hold ids only, so a data-bucket reader can join them
/// without reaching `[auth_storage]`.
#[async_trait]
pub trait OrgStore: Send + Sync {
    async fn get_org(&self, org_id: &OrgId) -> anyhow::Result<Option<Organization>>;

    /// Unconditional; the caller claims the slug first, so a record only
    /// exists for a handle its organization holds.
    async fn put_org(&self, org: &Organization) -> anyhow::Result<()>;

    /// Idempotent. Does not clear the memberships and ownerships naming it;
    /// the caller detaches those first.
    async fn delete_org(&self, org_id: &OrgId) -> anyhow::Result<()>;

    /// In creation order (UUIDv7 ids); an unparseable record is logged and skipped.
    async fn list_orgs(&self) -> anyhow::Result<Vec<Organization>>;

    /// An exclusive create, so exactly one of two racing creations wins.
    async fn claim_org_slug(&self, slug: &str, org_id: &OrgId) -> anyhow::Result<SlugClaim>;

    async fn lookup_org_by_slug(&self, slug: &str) -> anyhow::Result<Option<OrgId>>;

    /// Idempotent. Runs after the record is gone, so a crash strands the
    /// handle rather than freeing it while the organization exists.
    async fn release_org_slug(&self, slug: &str) -> anyhow::Result<()>;

    async fn get_membership(&self, user_id: &UserId) -> anyhow::Result<Option<OrgId>>;

    /// Unconditional, replacing the account's previous membership.
    async fn put_membership(&self, membership: &Membership) -> anyhow::Result<()>;

    /// Idempotent.
    async fn delete_membership(&self, user_id: &UserId) -> anyhow::Result<()>;

    async fn set_membership(&self, user_id: &UserId, org_id: Option<OrgId>) -> anyhow::Result<()> {
        match org_id {
            Some(org_id) => {
                let membership = Membership {
                    user_id: user_id.clone(),
                    org_id,
                };
                self.put_membership(&membership).await
            }
            None => self.delete_membership(user_id).await,
        }
    }

    /// Sorted by account id; an unparseable record is logged and skipped.
    async fn list_memberships(&self) -> anyhow::Result<Vec<Membership>>;

    async fn get_ownership(&self, client_id: &ClientId) -> anyhow::Result<Option<ClientOwner>>;

    /// Unconditional, replacing the device's previous ownership.
    async fn put_ownership(&self, ownership: &Ownership) -> anyhow::Result<()>;

    /// Idempotent.
    async fn delete_ownership(&self, client_id: &ClientId) -> anyhow::Result<()>;

    async fn set_ownership(
        &self,
        client_id: &ClientId,
        owner: Option<ClientOwner>,
    ) -> anyhow::Result<()> {
        match owner {
            Some(owner) => {
                let ownership = Ownership {
                    client_id: client_id.clone(),
                    owner,
                };
                self.put_ownership(&ownership).await
            }
            None => self.delete_ownership(client_id).await,
        }
    }

    /// Sorted by device id; an unparseable record is logged and skipped.
    async fn list_ownerships(&self) -> anyhow::Result<Vec<Ownership>>;
}

/// The client tag index: a forward tree (`by-client/`, the truth) and a
/// reverse one (`by-tag/`, derived). Mutations write forward first, so a crash
/// leaves only the reverse stale, and [`reindex_tags`] reconciles it.
#[async_trait]
pub trait TagStore: Send + Sync {
    /// Idempotent; does not check that the client exists.
    async fn add_client_tag(&self, client_id: &ClientId, tag: &Tag) -> anyhow::Result<()>;

    /// Idempotent.
    async fn remove_client_tag(&self, client_id: &ClientId, tag: &Tag) -> anyhow::Result<()>;

    /// Idempotent.
    async fn delete_client_tags(&self, client_id: &ClientId) -> anyhow::Result<()>;

    /// From the forward tree; empty for an untagged *or* unknown client.
    async fn get_client_tags(&self, client_id: &ClientId) -> anyhow::Result<BTreeSet<Tag>>;

    /// Ascending, from the reverse index.
    async fn list_client_ids_by_tag(&self, tag: &Tag) -> anyhow::Result<Vec<ClientId>>;

    async fn list_forward_tag_markers(&self) -> anyhow::Result<Vec<(ClientId, Tag)>>;

    async fn list_reverse_tag_markers(&self) -> anyhow::Result<Vec<(ClientId, Tag)>>;
}

/// `public-key-index/{digest}`, the one keyspace both principal kinds share:
/// a lock whose body names the holder. See [`pipette_core::key_index`].
#[async_trait]
pub trait KeyIndexStore: Send + Sync {
    /// An exclusive create: one concurrent registration wins. Call before
    /// writing the record it protects; a claim with no record authenticates nothing.
    async fn claim_public_key(
        &self,
        digest: &PublicKeyDigest,
        holder: &KeyHolderPrincipalId,
    ) -> anyhow::Result<KeyClaim>;

    /// For the key-only account read, which knows the key but not the account.
    async fn holder_of_public_key(
        &self,
        digest: &PublicKeyDigest,
    ) -> anyhow::Result<Option<KeyHolderPrincipalId>>;

    /// Idempotent. Runs after the record is gone, so a crash strands the claim
    /// rather than freeing a key that authenticates; nothing else frees one.
    async fn release_public_key(&self, digest: &PublicKeyDigest) -> anyhow::Result<()>;
}

/// The outcome of a create-if-absent write.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CreateOutcome {
    Created,
    /// The store wrote nothing; the first object stays unchanged.
    AlreadyExists,
}

#[async_trait]
pub trait SubmissionStore: Send + Sync {
    async fn write_incoming(&self, job_id: &JobId, body: &serde_json::Value) -> anyhow::Result<()>;
    /// Write `incoming/{job_id}.json` only while the key is free, for a lease-less
    /// server-synthetic failure that must not clobber a real submission
    /// ([`Self::write_incoming`] overwrites: a lease holder supersedes its own body).
    async fn write_incoming_new(
        &self,
        job_id: &JobId,
        body: &serde_json::Value,
    ) -> anyhow::Result<CreateOutcome>;
    /// Write straight to `processed/`, bypassing the scoring queue.
    async fn write_processed(&self, job_id: &JobId, body: &serde_json::Value)
    -> anyhow::Result<()>;
    /// Searches `incoming/`, then `processed/`.
    async fn get_submission(&self, job_id: &JobId) -> anyhow::Result<Option<SubmissionRecord>>;
    /// Up to `limit` ids past `after`, in an ascending order of the backend's
    /// own (not time order); fewer than `limit` means exhausted. A drain pages
    /// with it, so entries that keep failing cannot hide the ones behind them.
    async fn list_incoming_after(
        &self,
        after: Option<&JobId>,
        limit: NonZeroUsize,
    ) -> anyhow::Result<Vec<JobId>>;
    /// The first page of [`list_incoming_after`](Self::list_incoming_after).
    async fn list_incoming(&self, limit: NonZeroUsize) -> anyhow::Result<Vec<JobId>> {
        self.list_incoming_after(None, limit).await
    }
    async fn mark_processed(&self, job_id: &JobId) -> anyhow::Result<()>;
    /// Idempotent. Call only after the record is in its next place:
    /// [`has_record`](Self::has_record) relies on that order.
    async fn delete_incoming(&self, job_id: &JobId) -> anyhow::Result<()>;
    /// Across `incoming/`, `processed/` and the `score-queue/` stages. The
    /// caller checks ownership against the payload's `client_id`.
    async fn find_job(&self, job_id: &JobId) -> anyhow::Result<Option<SubmissionRecord>>;

    /// Metadata only, as a record can be tens of MB. Checks places in the order
    /// a record moves (`incoming/`, `to_do`, `parked`, `to_finalize`,
    /// `processed/`; each move writes before it deletes), so a moving record is
    /// still found.
    /// Every error but "not found" propagates: `false` gets a job recycled.
    async fn has_record(&self, job_id: &JobId) -> anyhow::Result<bool>;

    /// Outcomes of the jobs that have a record. Checks `processed/` first, where
    /// most records are (one request each), then the
    /// [`has_record`](Self::has_record) order. Every error but "not found"
    /// propagates, as callers keep each outcome as final.
    async fn find_outcomes(
        &self,
        job_ids: &[JobId],
    ) -> anyhow::Result<HashMap<JobId, SettledOutcome>>;

    /// A [`JobState::Scoring`] record with the bare submission body, whichever
    /// stage holds it.
    async fn find_in_score_queue(
        &self,
        job_id: &JobId,
    ) -> anyhow::Result<Option<SubmissionRecord>> {
        // `to_do` first: parking writes `parked` before it deletes `to_do`.
        let body = match self.read_queue(ScoreQueueStage::ToDo, job_id).await? {
            Some(body) => Some(body),
            None => self.read_queue(ScoreQueueStage::Parked, job_id).await?,
        };
        if let Some(body) = body {
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

    /// `ToDo` and `Parked` carry the raw submission, `ToFinalize`
    /// `{ submission, score }`.
    async fn enqueue(
        &self,
        stage: ScoreQueueStage,
        job_id: &JobId,
        body: &serde_json::Value,
    ) -> anyhow::Result<()>;
    /// As [`list_incoming_after`](Self::list_incoming_after).
    async fn list_queue_after(
        &self,
        stage: ScoreQueueStage,
        after: Option<&JobId>,
        limit: NonZeroUsize,
    ) -> anyhow::Result<Vec<JobId>>;
    /// The first page of [`list_queue_after`](Self::list_queue_after).
    async fn list_queue(
        &self,
        stage: ScoreQueueStage,
        limit: NonZeroUsize,
    ) -> anyhow::Result<Vec<JobId>> {
        self.list_queue_after(stage, None, limit).await
    }
    async fn read_queue(
        &self,
        stage: ScoreQueueStage,
        job_id: &JobId,
    ) -> anyhow::Result<Option<serde_json::Value>>;
    /// Idempotent. Call only after the record is in its next place:
    /// [`has_record`](Self::has_record) relies on that order.
    async fn dequeue(&self, stage: ScoreQueueStage, job_id: &JobId) -> anyhow::Result<()>;

    /// Hold a submission from an unapproved client, per client. Kept out of
    /// every other read so the scorer never picks it up until promotion.
    async fn write_unverified(
        &self,
        client_id: &ClientId,
        job_id: &JobId,
        body: &serde_json::Value,
    ) -> anyhow::Result<()>;

    async fn list_unverified_client(
        &self,
        client_id: &ClientId,
    ) -> anyhow::Result<Vec<(JobId, serde_json::Value)>>;

    async fn delete_unverified(&self, client_id: &ClientId, job_id: &JobId) -> anyhow::Result<()>;

    /// The number deleted, or on `dry_run` the number that would be.
    async fn delete_unverified_client(
        &self,
        client_id: &ClientId,
        dry_run: bool,
    ) -> anyhow::Result<usize>;

    /// Age is the backend's modification time, not the payload's `submitted_at`.
    async fn prune_unverified(
        &self,
        older_than: std::time::Duration,
        dry_run: bool,
    ) -> anyhow::Result<PruneSummary>;

    /// Keyed by job and client, as several clients can report on one job.
    async fn write_runtime_output(
        &self,
        job_id: &JobId,
        client_id: &ClientId,
        output: &RuntimeOutput,
    ) -> anyhow::Result<()>;

    async fn list_runtime_output(
        &self,
        job_id: &JobId,
    ) -> anyhow::Result<Vec<(ClientId, RuntimeOutput)>>;
}

#[async_trait]
pub trait WarehouseStore: Send + Sync {
    /// Append to a `day=` partition without dedup: a re-scored job leaves a
    /// duplicate row set that `JobMetrics::from_latest_rows` resolves.
    async fn write_partition_metrics(
        &self,
        benchmark_id: &BenchmarkId,
        client_id: &ClientId,
        day_key: &str,
        rows: &[MetricRow],
    ) -> anyhow::Result<()>;
    /// Scans only the `warehouse_read_days` window; an older job reads `None`.
    async fn read_job_metrics(
        &self,
        benchmark_id: &BenchmarkId,
        client_id: &ClientId,
        job_id: &JobId,
    ) -> anyhow::Result<Option<JobMetrics>>;

    /// Visit every metric row; a file with a row `f` marks dirty is rewritten
    /// atomically. A dirtying visitor must hold the storage mutate lock (it
    /// races `score`); one that never dirties must never write, as
    /// `pipette_pipeline::model_audit` runs lock-free.
    async fn for_each_metric_row(
        &self,
        f: &mut (dyn for<'a> FnMut(&'a mut MetricRow) -> bool + Send),
    ) -> anyhow::Result<()>;
}

#[async_trait]
pub trait EvalSampleResultStore: Send + Sync {
    /// Written once per scored eval job.
    async fn write(&self, job_id: &JobId, rows: &[EvalSampleResult]) -> anyhow::Result<()>;
    async fn read(&self, job_id: &JobId) -> anyhow::Result<Option<Vec<EvalSampleResult>>>;

    /// Unordered; for bulk maintenance.
    async fn list_job_ids(&self) -> anyhow::Result<Vec<JobId>>;
}

/// A readable corpus tar.
pub struct ArtifactBody {
    pub bytes: u64,
    pub stream: futures::stream::BoxStream<'static, anyhow::Result<bytes::Bytes>>,
}

/// `dataset-cache/artifacts/`: repacked corpora keyed by sha256, with build
/// records. Nothing deletes: an unbacked artifact is refused at serve time.
#[async_trait]
pub trait DatasetCacheStore: Send + Sync {
    async fn get_artifact(&self, digest: &Sha256) -> anyhow::Result<Option<DatasetArtifact>>;

    /// The build writes the tar first, so metadata always describes a tar that
    /// exists.
    async fn put_artifact(&self, artifact: &DatasetArtifact) -> anyhow::Result<()>;

    async fn has_tar(&self, digest: &Sha256) -> anyhow::Result<bool>;

    /// Atomic: a reader never sees a partial tar. The caller has verified that
    /// `file` hashes to `digest`.
    async fn put_tar(&self, digest: &Sha256, file: &std::path::Path) -> anyhow::Result<()>;

    async fn open_tar(&self, digest: &Sha256) -> anyhow::Result<Option<ArtifactBody>>;
}

/// One page of a keyspace in key order; an invalid object is reported, not fatal.
#[derive(Debug)]
pub struct Page<T, Id> {
    /// A page that is not final contains `limit` rows.
    pub items: Vec<T>,
    pub unreadable: Vec<UnreadableObjectError>,
    /// The last key examined, not always the last row, so the next page does
    /// not report the same invalid object again. `None` at the end.
    pub resume_after: Option<Id>,
}

/// Every row and report of a whole keyspace.
#[derive(Debug)]
pub struct Listing<T> {
    pub items: Vec<T>,
    pub unreadable: Vec<UnreadableObjectError>,
}

// Not derived: that would demand `T: Default` and `Id: Default`.
impl<T, Id> Default for Page<T, Id> {
    fn default() -> Self {
        Self {
            items: Vec::new(),
            unreadable: Vec::new(),
            resume_after: None,
        }
    }
}

impl<T> Listing<T> {
    pub fn map<U>(self, f: impl FnMut(T) -> U) -> Listing<U> {
        Listing {
            items: self.items.into_iter().map(f).collect(),
            unreadable: self.unreadable,
        }
    }
}

impl<T> Default for Listing<T> {
    fn default() -> Self {
        Self {
            items: Vec::new(),
            unreadable: Vec::new(),
        }
    }
}

pub type PlanPage = Page<PlanManifest, PlanId>;
pub type PlanHeaderPage = Page<PlanHeader, PlanId>;
pub type VaultResourcePage = Page<VaultResource, VaultResourceId>;

/// One stored vault blob, as [`VaultStore::list_blobs`] reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VaultBlob {
    pub key: VaultObjectKey,
    pub bytes: u64,
    /// The backend's modification time.
    pub modified: DateTime<Utc>,
}
pub type CampaignPage = Page<Campaign, CampaignId>;
pub type ModelParamsPage = Page<ModelParams, ModelKey>;
/// Newest first: `resume_after` is the next page's exclusive upper bound.
pub type AuditPage = Page<AuditEvent, AuditEventId>;
pub type ClientPage = Page<Client, ClientId>;
pub type PlanListing = Listing<PlanManifest>;
pub type PlanSummaryListing = Listing<PlanSummary>;
pub type VaultResourceListing = Listing<VaultResource>;
pub type CampaignListing = Listing<Campaign>;
pub type ModelParamsListing = Listing<ModelParams>;
pub type ClientListing = Listing<Client>;

/// Filter for [`PlanStore::list_matching_plans`]; a `None` field admits every value.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PlanFilter {
    pub status: Option<PlanStatus>,
    pub campaign_id: Option<CampaignId>,
    pub submitted_by: Option<UserId>,
    pub reach: Option<PlanReach>,
}

/// The plans a caller's own scope reads: those filed into `campaigns`, and
/// those `filer` filed outside every campaign. No `filer` reads none of those,
/// as a campaign reader reads none.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanReach {
    pub campaigns: BTreeSet<CampaignId>,
    pub filer: Option<UserId>,
}

impl PlanReach {
    pub fn admits(&self, campaign_id: Option<&CampaignId>, submitted_by: Option<&UserId>) -> bool {
        match campaign_id {
            Some(campaign_id) => self.campaigns.contains(campaign_id),
            None => self.filer.is_some() && submitted_by == self.filer.as_ref(),
        }
    }
}

impl PlanFilter {
    pub fn admits(&self, manifest: &PlanManifest) -> bool {
        self.status.is_none_or(|status| manifest.status == status)
            && self
                .campaign_id
                .as_ref()
                .is_none_or(|campaign| manifest.campaign_id.as_ref() == Some(campaign))
            && self
                .submitted_by
                .as_ref()
                .is_none_or(|user| manifest.submitted_by.as_ref() == Some(user))
            && self.reach.as_ref().is_none_or(|reach| {
                reach.admits(
                    manifest.campaign_id.as_ref(),
                    manifest.submitted_by.as_ref(),
                )
            })
    }
}

/// An object at a valid record key that contains invalid data.
#[derive(Debug, thiserror::Error)]
#[error("{key}: {reason}")]
pub struct UnreadableObjectError {
    pub key: String,
    pub reason: String,
}

/// The record id a flat keyspace's leaf names, `{id}.json` → `id`. Anything
/// else is not a record at all, so neither a row nor an [`UnreadableObjectError`].
pub fn flat_keyspace_id<Id, E>(leaf: &str, parse: fn(String) -> Result<Id, E>) -> Option<Id>
where
    Id: std::fmt::Display,
    IdError: From<E>,
{
    id_from_key(leaf.strip_suffix(".json")?, parse).ok()
}

#[cfg(test)]
#[test]
fn a_flat_keyspace_leaf_names_an_id_only_verbatim() {
    use pipette_plan_types::VaultResourceId;
    [
        ("vault_a.json", true),
        (" vault_a.json", false),
        ("vault_a .json", false),
        ("vault_a.tmp", false),
    ]
    .iter()
    .for_each(|(leaf, named)| {
        let parsed = flat_keyspace_id(leaf, VaultResourceId::try_new);
        assert_eq!(parsed.is_some(), *named, "{leaf:?}");
    });
}

/// The campaign catalog: stable handles that point into [`PlanStore`]'s
/// manifests. Its own trait as no maintenance pass touches it; on `[storage]`
/// beside `plans/`, so a campaign cannot name a plan in another bucket.
#[async_trait]
pub trait CampaignStore: Send + Sync {
    async fn get_campaign(&self, campaign_id: &CampaignId) -> anyhow::Result<Option<Campaign>>;

    /// No compare-and-swap: operators are the only writers, so a race is
    /// last-writer-wins, never a corrupt record.
    async fn put_campaign(&self, campaign: &Campaign) -> anyhow::Result<()>;

    /// In creation order (UUIDv7 ids), with [`PlanStore::list_plans`]'s paging
    /// and invalid-object contract.
    async fn list_campaigns(
        &self,
        after: Option<&CampaignId>,
        limit: NonZeroUsize,
    ) -> anyhow::Result<CampaignPage>;

    /// Idempotent. Deletes the pointer only: tearing down the plan too would
    /// let a bookkeeping change destroy running benchmarks.
    async fn delete_campaign(&self, campaign_id: &CampaignId) -> anyhow::Result<()>;
}

/// The model parameter table: parameter counts, one record per model key.
#[async_trait]
pub trait ModelParamsStore: Send + Sync {
    async fn get_model_params(&self, key: &ModelKey) -> anyhow::Result<Option<ModelParams>>;

    /// Last writer wins, as [`CampaignStore::put_campaign`].
    async fn put_model_params(&self, params: &ModelParams) -> anyhow::Result<()>;

    /// In `{key}.json` byte order, with [`PlanStore::list_plans`]'s paging and
    /// invalid-object contract.
    async fn list_model_params(
        &self,
        after: Option<&ModelKey>,
        limit: NonZeroUsize,
    ) -> anyhow::Result<ModelParamsPage>;

    /// Idempotent.
    async fn delete_model_params(&self, key: &ModelKey) -> anyhow::Result<()>;
}

/// The audit events that a listing returns. An empty or absent field matches
/// every value.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AuditFilter {
    /// The events this organization's log lists.
    pub org_id: Option<OrgId>,
    pub outcomes: BTreeSet<AuditOutcome>,
    pub actor_kinds: BTreeSet<AuditActorKind>,
    /// The events naming this record as a target, or as the account or device
    /// that acted.
    pub record: Option<AuditTarget>,
}

impl AuditFilter {
    pub fn admits(&self, event: &AuditEvent) -> bool {
        let names = |record: &AuditTarget| {
            event.targets.contains(record) || event.actor.record().as_ref() == Some(record)
        };
        self.org_id
            .as_ref()
            .is_none_or(|org_id| event.org_ids.contains(org_id))
            && (self.outcomes.is_empty() || self.outcomes.contains(&event.outcome))
            && (self.actor_kinds.is_empty() || self.actor_kinds.contains(&event.actor.kind()))
            && self.record.as_ref().is_none_or(names)
    }
}

/// The audit log: append-only, read newest first, pruned by age. SQL keeps it;
/// no object store does.
#[async_trait]
pub trait AuditStore: Send + Sync {
    async fn append(&self, event: &AuditEvent) -> anyhow::Result<()>;

    /// The events `filter` keeps, newest first, older than `before`.
    async fn list_events(
        &self,
        filter: &AuditFilter,
        before: Option<&AuditEventId>,
        limit: NonZeroUsize,
    ) -> anyhow::Result<AuditPage>;

    /// Delete every event from before `cutoff`, returning how many went.
    async fn prune_before(&self, cutoff: DateTime<Utc>) -> anyhow::Result<u64>;

    /// How many events are from before `cutoff`: a prune's dry run.
    async fn count_before(&self, cutoff: DateTime<Utc>) -> anyhow::Result<u64>;
}

/// The outcome of inserting a catalog entry. The catalog never reuses a name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CreateEntryOutcome {
    Created,
    NameTaken {
        name: CatalogName,
        state: DescriptorEntryState,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AppendOutcome {
    /// The entry as it stood at the successful append's serialization point.
    Appended(DescriptorEntry),
    NoSuchEntry,
    Tombstoned,
    /// A runtime member for a model entry, or a model member for a runtime
    /// entry.
    DescriptorKindMismatch,
    RuntimeTypeMismatch,
    DuplicateFlavor(String),
    DuplicateModelKey {
        format: ModelFormat,
        quantization: QuantizationLabel,
    },
    /// Two model types with one format.
    FormatTypeClash {
        format: ModelFormat,
        first: ModelType,
        second: ModelType,
    },
    /// Speculative and plain model types in one entry.
    SpeculativeMix,
    DuplicateDigest(String),
}

impl AppendOutcome {
    /// The outcome that refuses members which cannot share one entry. An error
    /// about the entry itself is not an outcome, and comes back unchanged.
    pub fn refusing(error: EntryError) -> Result<Self, EntryError> {
        match error {
            EntryError::DescriptorKindMismatch => Ok(Self::DescriptorKindMismatch),
            EntryError::RuntimeTypeMismatch => Ok(Self::RuntimeTypeMismatch),
            EntryError::DuplicateFlavor(flavor) => Ok(Self::DuplicateFlavor(flavor)),
            EntryError::DuplicateModelKey {
                format,
                quantization,
            } => Ok(Self::DuplicateModelKey {
                format,
                quantization,
            }),
            EntryError::FormatTypeClash {
                format,
                first,
                second,
            } => Ok(Self::FormatTypeClash {
                format,
                first,
                second,
            }),
            EntryError::SpeculativeMix => Ok(Self::SpeculativeMix),
            EntryError::DuplicateDigest(digest) => Ok(Self::DuplicateDigest(digest)),
            error @ (EntryError::Tombstoned
            | EntryError::OrgKind
            | EntryError::ModelKind
            | EntryError::CreatorScope) => Err(error),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SetStateOutcome {
    Changed { from: DescriptorEntryState },
    Unchanged,
    NoSuchEntry,
    Tombstoned,
}

/// The result of a state change that requires a given old state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransitionOutcome {
    Changed,
    NoSuchEntry,
    Tombstoned,
    /// The entry has this state, not the expected one, and nothing changed.
    UnexpectedState(DescriptorEntryState),
}

/// Empty kind and state sets admit every value. The prefix ignores case;
/// callers must select at least one scope.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DescriptorEntryFilter {
    /// No scopes means no rows; callers must choose their visible scopes.
    pub scopes: BTreeSet<CatalogScope>,
    pub kinds: BTreeSet<DescriptorEntryKind>,
    pub states: BTreeSet<DescriptorEntryState>,
    /// Runtime entries or model entries only. `None` admits both.
    pub descriptor_kind: Option<DescriptorKind>,
    /// With `model_types`, the descriptor types to keep. If both sets are
    /// empty, every type matches. Otherwise a runtime entry matches on its
    /// runtime type, and a model entry matches if any member has one of the
    /// model types.
    pub runtime_types: HashSet<RuntimeType>,
    pub model_types: HashSet<ModelType>,
    pub name_prefix: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogEntryKey {
    pub scope: CatalogScope,
    pub name: CatalogName,
}

impl std::fmt::Display for CatalogEntryKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.scope {
            CatalogScope::Global => self.name.fmt(f),
            CatalogScope::Org(id) => write!(f, "{id}/{}", self.name),
        }
    }
}

/// Why a list cursor is not a [`CatalogEntryKey`].
#[derive(Debug, thiserror::Error)]
pub enum CatalogEntryKeyError {
    #[error(transparent)]
    Scope(#[from] pipette_plan_types::PrincipalIdError),
    #[error(transparent)]
    Name(#[from] IdError),
}

/// The inverse of [`Display`](std::fmt::Display): `name` for a global entry,
/// `{org_id}/name` for an org entry.
impl std::str::FromStr for CatalogEntryKey {
    type Err = CatalogEntryKeyError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (scope, name) = match s.split_once('/') {
            Some((org, name)) => (CatalogScope::Org(OrgId::try_new(org)?), name),
            None => (CatalogScope::Global, s),
        };
        Ok(Self {
            scope,
            name: CatalogName::try_new(name)?,
        })
    }
}

#[cfg(test)]
#[test]
fn a_catalog_entry_key_round_trips_through_its_cursor() -> anyhow::Result<()> {
    let name = CatalogName::try_new("my-fork")?;
    let org = OrgId::try_new("org_0195c3e2-a4f3-7b8c-9d1e-2f3a4b5c6d7e")?;
    [CatalogScope::Global, CatalogScope::Org(org)]
        .into_iter()
        .map(|scope| CatalogEntryKey {
            scope,
            name: name.clone(),
        })
        .try_for_each(|key| -> anyhow::Result<()> {
            assert_eq!(key.to_string().parse::<CatalogEntryKey>()?, key);
            Ok(())
        })?;
    ["", "acme/my-fork", "org_a/", "org_a/my/fork"]
        .iter()
        .for_each(|cursor| assert!(cursor.parse::<CatalogEntryKey>().is_err(), "{cursor}"));
    Ok(())
}

pub type DescriptorEntryPage = Page<DescriptorEntry, CatalogEntryKey>;

/// Holds the importer state lock until it is released or dropped.
#[async_trait]
pub trait CatalogEntryLock: Send {
    async fn release(self: Box<Self>) -> anyhow::Result<()>;
}

pub struct NoopCatalogEntryLock;

#[async_trait]
impl CatalogEntryLock for NoopCatalogEntryLock {
    async fn release(self: Box<Self>) -> anyhow::Result<()> {
        Ok(())
    }
}

/// A list cursor that names no entry. A cursor from a page always names one,
/// because the store never deletes an entry.
#[derive(Debug, thiserror::Error)]
#[error("no descriptor catalog entry {cursor} to resume after")]
pub struct UnknownCursorError {
    pub cursor: CatalogEntryKey,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DigestHit {
    pub scope: CatalogScope,
    pub name: CatalogName,
    pub kind: DescriptorEntryKind,
    pub state: DescriptorEntryState,
    pub member_key: DescriptorMemberKey,
}

/// Only `[database]` keeps catalog records. A caller can add members only at the
/// end of an entry.
#[async_trait]
pub trait DescriptorCatalogStore: Send + Sync {
    /// Serialize an importer's stable ranking and writes with operator state changes.
    async fn lock_importer_entries(&self) -> anyhow::Result<Box<dyn CatalogEntryLock>>;
    async fn create_entry(&self, entry: &DescriptorEntry) -> anyhow::Result<CreateEntryOutcome>;
    async fn append_members(
        &self,
        scope: &CatalogScope,
        name: &CatalogName,
        members: NonEmptyVec<DescriptorMember>,
    ) -> anyhow::Result<AppendOutcome>;
    async fn set_state(
        &self,
        scope: &CatalogScope,
        name: &CatalogName,
        state: DescriptorEntryState,
    ) -> anyhow::Result<SetStateOutcome>;
    /// Change the state only if it equals `expected`. The check and the write
    /// are one atomic step. A tombstoned entry never changes.
    async fn transition_state(
        &self,
        scope: &CatalogScope,
        name: &CatalogName,
        expected: DescriptorEntryState,
        state: DescriptorEntryState,
    ) -> anyhow::Result<TransitionOutcome>;
    /// Transition an entry while the caller holds [`Self::lock_importer_entries`].
    async fn transition_state_under_importer_lock(
        &self,
        scope: &CatalogScope,
        name: &CatalogName,
        expected: DescriptorEntryState,
        state: DescriptorEntryState,
    ) -> anyhow::Result<TransitionOutcome>;
    async fn get_entry(
        &self,
        scope: &CatalogScope,
        name: &CatalogName,
    ) -> anyhow::Result<Option<DescriptorEntry>>;
    async fn list_entries(
        &self,
        filter: &DescriptorEntryFilter,
        after: Option<&CatalogEntryKey>,
        limit: NonZeroUsize,
    ) -> anyhow::Result<DescriptorEntryPage>;
    async fn find_by_digests(
        &self,
        digests: &[String],
    ) -> anyhow::Result<HashMap<String, Vec<DigestHit>>>;

    /// Names of live entries that prevent deletion of this organization.
    async fn live_entries_in_org(&self, org_id: &OrgId) -> anyhow::Result<Vec<CatalogName>>;

    /// Remove reserved names after the organization's live dependents are gone.
    async fn delete_tombstones_in_org(&self, org_id: &OrgId) -> anyhow::Result<()>;
}

/// Result of one catalog import or refresh run.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CatalogImportRunFinish {
    pub entries_created: i32,
    pub members_added: i32,
    /// Entries that the run created hidden, and active entries that its
    /// expiry moved to hidden.
    pub entries_hidden: i32,
    /// Each value is `tag/asset`. The tag names the release that holds the asset.
    pub unclassified_assets: Vec<String>,
    pub error_summary: Option<String>,
}

#[async_trait]
pub trait CatalogImportRunStore: Send + Sync {
    /// The newest runs and the last successful import, regardless of limit.
    async fn recent_runs(
        &self,
        limit: NonZeroUsize,
    ) -> anyhow::Result<pipette_plan_types::CatalogImportRuns>;

    async fn start_run(
        &self,
        id: &str,
        command: &str,
        target_name: Option<&CatalogName>,
        at: DateTime<Utc>,
    ) -> anyhow::Result<()>;

    async fn finish_run(
        &self,
        id: &str,
        at: DateTime<Utc>,
        finish: &CatalogImportRunFinish,
    ) -> anyhow::Result<()>;
}

/// The vault's records alone: what the audit log reads to find the owner of a
/// resource. Every shared [`VaultStore`] is one.
#[async_trait]
pub trait VaultRecords: Send + Sync {
    async fn get_resource(
        &self,
        resource_id: &VaultResourceId,
    ) -> anyhow::Result<Option<VaultResource>>;
}

#[async_trait]
impl<T: VaultStore + ?Sized> VaultRecords for Arc<T> {
    async fn get_resource(
        &self,
        resource_id: &VaultResourceId,
    ) -> anyhow::Result<Option<VaultResource>> {
        (**self).get_resource(resource_id).await
    }
}

/// The vault: private artifacts mgmt serves to one principal. Blobs
/// (`vault-resources/`) and records (`vault/`) are apart because a record
/// decides who may fetch a blob, and an uploader must not decide that.
#[async_trait]
pub trait VaultStore: Send + Sync {
    async fn get_resource(
        &self,
        resource_id: &VaultResourceId,
    ) -> anyhow::Result<Option<VaultResource>>;

    /// Unordered, deduplicated. Best effort: an unread record is logged and
    /// left out, so a caller must not act on absence.
    async fn get_resources(
        &self,
        resource_ids: &[VaultResourceId],
    ) -> anyhow::Result<Vec<VaultResource>> {
        // Owned ids: a borrowed stream fails the future's `Send` bound.
        let unique: HashSet<VaultResourceId> = resource_ids.iter().cloned().collect();
        Ok(futures::stream::iter(unique)
            .map(|resource_id| async move {
                self.get_resource(&resource_id)
                    .await
                    .inspect_err(|e| {
                        tracing::warn!(%resource_id, error = %format!("{e:#}"), "a vault record did not read");
                    })
                    .ok()
                    .flatten()
            })
            .buffer_unordered(STORAGE_CONCURRENCY)
            .filter_map(std::future::ready)
            .collect()
            .await)
    }

    /// No compare-and-swap: a record is only ever created, never edited.
    async fn put_resource(&self, resource: &VaultResource) -> anyhow::Result<()>;

    /// In creation order (UUIDv7 ids), with [`PlanStore::list_plans`]'s paging
    /// and invalid-object contract.
    async fn list_resources(
        &self,
        after: Option<&VaultResourceId>,
        limit: NonZeroUsize,
    ) -> anyhow::Result<VaultResourcePage>;

    /// Every record `owner` owns, plus every unreadable record, as one that
    /// cannot be read names no owner to rule it out.
    async fn list_resources_of(&self, owner: &VaultOwner) -> anyhow::Result<VaultResourceListing> {
        let mut listing = VaultResourceListing::default();
        let mut after: Option<VaultResourceId> = None;
        loop {
            let page = self.list_resources(after.as_ref(), DRAIN_PAGE_SIZE).await?;
            listing.items.extend(
                page.items
                    .into_iter()
                    .filter(|resource| &resource.owner == owner),
            );
            listing.unreadable.extend(page.unreadable);
            match page.resume_after {
                Some(cursor) => after = Some(cursor),
                None => return Ok(listing),
            }
        }
    }

    /// Idempotent. The blob it named is untouched.
    async fn delete_resource(&self, resource_id: &VaultResourceId) -> anyhow::Result<()>;

    /// Idempotent. The caller checks first that no record names it.
    async fn delete_blob(&self, key: &VaultObjectKey) -> anyhow::Result<()>;

    /// Every blob, unordered, for a prune. A store with no blobs of its own refuses.
    async fn list_blobs(&self) -> anyhow::Result<Vec<VaultBlob>> {
        anyhow::bail!("this vault store lists no blobs")
    }

    async fn stat_blob(&self, key: &VaultObjectKey) -> anyhow::Result<Option<u64>>;

    /// Completes only if the body hashes to `sha256`: a reader never sees a
    /// partial blob, or one other than its key says.
    async fn put_blob(
        &self,
        key: &VaultObjectKey,
        sha256: &Sha256,
        body: BlobBody,
    ) -> anyhow::Result<BlobWrite>;

    /// Stream from byte `from` (`0`, or below the blob's size) to the end; the length
    /// is the size minus `from`.
    async fn open_blob(
        &self,
        key: &VaultObjectKey,
        from: u64,
    ) -> anyhow::Result<Option<ArtifactBody>>;

    /// Write a blob whose digest is known only after the last byte, via a
    /// staging key no record can name. An existing blob at the final key is
    /// kept; the key says what the bytes are. A failed body writes nothing.
    async fn put_staged_blob(
        &self,
        owner: &VaultOwner,
        body: BlobBody,
        limit: NonZeroU64,
    ) -> anyhow::Result<StagedWrite>;

    /// An entry can name a deleted record; reuse it only if every record it
    /// names still reads.
    async fn get_import(
        &self,
        owner: &VaultOwner,
        import: &Sha256,
    ) -> anyhow::Result<Option<VaultImport>>;

    /// Last writer wins; either entry is correct, and nothing deletes the
    /// duplicate records a replaced entry names.
    async fn put_import(
        &self,
        owner: &VaultOwner,
        import: &Sha256,
        entry: &VaultImport,
    ) -> anyhow::Result<()>;
}

pub type BlobBody = futures::stream::BoxStream<'static, anyhow::Result<bytes::Bytes>>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StagedWrite {
    Stored {
        sha256: Sha256,
        bytes: u64,
        key: VaultObjectKey,
    },
    /// Nothing written.
    TooLarge { limit: NonZeroU64 },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BlobWrite {
    Stored {
        bytes: u64,
    },
    /// Nothing written: the body hashed to `actual`.
    Mismatch {
        actual: Sha256,
        bytes: u64,
    },
}

/// Plan manifests, which outlive their jobs so a finished plan stays queryable.
#[async_trait]
pub trait PlanStore: Send + Sync {
    /// No compare-and-swap: ingestion hands off to `queue-maintenance` at
    /// `active`, and the one overlap (a stuck `creating` torn down) is settled
    /// by the create-only cancel marker, not here. No terminal latch.
    /// The manifest holds no notes, so this rewrite cannot change them.
    async fn put_plan(&self, manifest: &PlanManifest) -> anyhow::Result<()>;

    /// Read the note and the revision change of a plan through
    /// [`get_plan_notes_of`](Self::get_plan_notes_of). Return empty
    /// [`PlanNotes`] for a plan without notes.
    async fn get_plan_notes(&self, plan_id: &PlanId) -> anyhow::Result<PlanNotes> {
        Ok(self
            .get_plan_notes_of(std::slice::from_ref(plan_id))
            .await?
            .remove(plan_id)
            .unwrap_or_default())
    }

    /// Read the notes of each plan in `plan_ids` in one call. The result has
    /// no entry for a plan without notes. Only the SQL store keeps notes. This
    /// default returns an empty map.
    async fn get_plan_notes_of(
        &self,
        _plan_ids: &[PlanId],
    ) -> anyhow::Result<HashMap<PlanId, PlanNotes>> {
        Ok(HashMap::new())
    }

    /// Replace the notes of a plan, as filing writes them. Return `false` if
    /// the store wrote nothing. This default drops the notes and returns
    /// `false`, so filing works without a database.
    async fn put_plan_notes(&self, _plan_id: &PlanId, _notes: &PlanNotes) -> anyhow::Result<bool> {
        Ok(false)
    }

    /// Replace only the note. Return `false` if the plan does not exist. This
    /// default returns an error, because it cannot keep the edit.
    async fn set_plan_note(&self, _plan_id: &PlanId, _note: Option<&str>) -> anyhow::Result<bool> {
        anyhow::bail!("plan notes are kept in the [database] store alone")
    }

    /// No code path deletes a manifest.
    async fn get_plan(&self, plan_id: &PlanId) -> anyhow::Result<Option<PlanManifest>>;

    /// Every readable manifest filed into `campaign_id`, unordered, or `None`
    /// where the backend cannot read them in one request (the caller then walks
    /// the chain plan by plan).
    async fn get_campaign_plans(
        &self,
        _campaign_id: &CampaignId,
    ) -> anyhow::Result<Option<Vec<PlanManifest>>> {
        Ok(None)
    }

    /// Unordered; one that fails to read is reported in `unreadable` without
    /// failing the rest.
    async fn get_plans(&self, plan_ids: &[PlanId]) -> anyhow::Result<PlanListing> {
        // Owned ids: a borrowed stream fails the future's `Send` bound.
        let unique: HashSet<PlanId> = plan_ids.iter().cloned().collect();
        let reads: Vec<(PlanId, anyhow::Result<Option<PlanManifest>>)> =
            futures::stream::iter(unique)
                .map(|plan_id| async move {
                    let read = self.get_plan(&plan_id).await;
                    (plan_id, read)
                })
                .buffer_unordered(STORAGE_CONCURRENCY)
                .collect()
                .await;
        Ok(reads
            .into_iter()
            .fold(PlanListing::default(), |mut listing, (plan_id, read)| {
                match read {
                    Ok(Some(manifest)) => listing.items.push(manifest),
                    Ok(None) => {}
                    Err(e) => listing.unreadable.push(UnreadableObjectError {
                        key: format!("{PLANS_PREFIX}{plan_id}.json"),
                        reason: format!("{e:#}"),
                    }),
                }
                listing
            }))
    }

    /// [`get_plans`](Self::get_plans) without the jobs; a backend that keeps
    /// them apart leaves them unread.
    async fn get_plan_summaries(&self, plan_ids: &[PlanId]) -> anyhow::Result<PlanSummaryListing> {
        Ok(self.get_plans(plan_ids).await?.map(PlanSummary::from))
    }

    /// One page of the manifests `filter` admits, in key order, `after`
    /// exclusive. An invalid manifest that could match is reported and takes
    /// no row. `limit` bounds rows, not reads: a selective filter reads many.
    async fn list_matching_plans(
        &self,
        filter: &PlanFilter,
        after: Option<&PlanId>,
        limit: NonZeroUsize,
    ) -> anyhow::Result<PlanPage>;

    /// [`list_matching_plans`](Self::list_matching_plans) without per-job fields.
    async fn list_matching_plan_headers(
        &self,
        filter: &PlanFilter,
        after: Option<&PlanId>,
        limit: NonZeroUsize,
    ) -> anyhow::Result<PlanHeaderPage> {
        let page = self.list_matching_plans(filter, after, limit).await?;
        Ok(Page {
            items: page.items.into_iter().map(PlanHeader::from).collect(),
            unreadable: page.unreadable,
            resume_after: page.resume_after,
        })
    }

    /// [`list_matching_plans`](Self::list_matching_plans) on `status` alone.
    async fn list_plans(
        &self,
        status: Option<PlanStatus>,
        after: Option<&PlanId>,
        limit: NonZeroUsize,
    ) -> anyhow::Result<PlanPage> {
        let filter = PlanFilter {
            status,
            ..PlanFilter::default()
        };
        self.list_matching_plans(&filter, after, limit).await
    }

    /// A sibling keyspace of `plans/`. The marker requests teardown, which
    /// `queue-maintenance` alone performs, so a cancel is not lost to a
    /// concurrent status refresh. A plain idempotent PUT, not a CAS; a caller's
    /// "is it cancellable" check is best effort, and collection is convergent.
    async fn write_cancel_marker(&self, plan_id: &PlanId) -> anyhow::Result<()>;

    async fn has_cancel_marker(&self, plan_id: &PlanId) -> anyhow::Result<bool>;

    /// Unordered; an unparseable key is warned and skipped.
    async fn list_cancel_markers(&self) -> anyhow::Result<Vec<PlanId>>;

    /// Idempotent. Maintenance must also collect a marker whose plan is already
    /// terminal (never resurrecting it as `cancelled`) or has no manifest, or
    /// the marker lingers forever.
    async fn delete_cancel_marker(&self, plan_id: &PlanId) -> anyhow::Result<()>;

    // ── content-keys/v1/{digest} ────────────────────────────────────────────
    //
    // The duplicate-job index, claimed by an exclusive create so two concurrent
    // ingests of one cell make one job. On `[storage]`, where conditional PUT is
    // required. Every later write is a CAS: an overwrite would orphan a rival's
    // live job from the index.

    async fn get_content_key(
        &self,
        digest: &str,
    ) -> anyhow::Result<Option<(ContentKeyRecord, ContentKeyVersion)>>;

    /// Create-if-absent; a loser gets the incumbent and its version, to judge a
    /// join without a second read.
    async fn create_content_key(
        &self,
        digest: &str,
        record: &ContentKeyRecord,
    ) -> anyhow::Result<ContentKeyClaim>;

    /// CAS on the version read, to take over a digest whose job is provably
    /// dead. [`ContentKeyCas::Lost`]: re-read before acting.
    async fn replace_content_key(
        &self,
        digest: &str,
        version: &ContentKeyVersion,
        record: &ContentKeyRecord,
    ) -> anyhow::Result<ContentKeyCas>;

    /// For the GC only. The condition is a fresh re-read, not an atomic
    /// compare-and-delete, so a racing replace can still be lost: bounded (the
    /// next plan mints a fresh job), and each implementation states its window.
    async fn delete_content_key(
        &self,
        digest: &str,
        version: &ContentKeyVersion,
    ) -> anyhow::Result<ContentKeyCas>;

    /// Unordered; the version lets the GC condition its delete on what it judged.
    async fn list_content_keys(
        &self,
    ) -> anyhow::Result<Vec<(String, ContentKeyRecord, ContentKeyVersion)>>;

    // ── job-plans/{job_id}/{plan_id}.{expires_at} ───────────────────────────
    //
    // One marker per job membership, written for every job at mint, so absence
    // is unambiguous. Teardown reads them to decide whether a job has a member left.

    /// Idempotent.
    async fn write_job_plan_marker(
        &self,
        job_id: &JobId,
        plan_id: &PlanId,
        expires_at: ExpiresAt,
    ) -> anyhow::Result<()>;

    /// Empty licenses teardown to retire the job, so a read failure propagates
    /// rather than reading as empty.
    async fn list_job_plan_markers(
        &self,
        job_id: &JobId,
    ) -> anyhow::Result<Vec<(PlanId, ExpiresAt)>>;

    /// Propagates a read failure, as
    /// [`list_job_plan_markers`](Self::list_job_plan_markers) does.
    async fn list_all_job_plan_markers(&self) -> anyhow::Result<Vec<JobPlanMarker>>;

    /// Idempotent.
    async fn delete_job_plan_marker(
        &self,
        job_id: &JobId,
        plan_id: &PlanId,
        expires_at: ExpiresAt,
    ) -> anyhow::Result<()>;

    // ── plan-sources/{plan_id}.toml ─────────────────────────────────────────
    //
    // The document a plan was filed from, write-once, written before the
    // `creating` manifest. A `plan-sources-pending/` marker brackets that write,
    // so `queue-maintenance` can collect a source a crashed ingest left orphaned.

    /// Create-if-absent; a reader sees the whole document or none.
    async fn put_plan_source(
        &self,
        plan_id: &PlanId,
        source: &PlanSource,
    ) -> anyhow::Result<CreateOutcome>;

    /// Non-UTF-8 bytes are an error. Compare the returned digest to the
    /// manifest's `source_sha256` to check it names this document.
    async fn get_plan_source(&self, plan_id: &PlanId) -> anyhow::Result<Option<PlanSource>>;

    /// Idempotent.
    async fn delete_plan_source(&self, plan_id: &PlanId) -> anyhow::Result<()>;

    /// A plain idempotent PUT.
    async fn write_pending_source_marker(&self, plan_id: &PlanId) -> anyhow::Result<()>;

    /// Unordered; an unparseable key is warned and skipped.
    async fn list_pending_source_markers(&self) -> anyhow::Result<Vec<PendingSourceMarker>>;

    /// Idempotent.
    async fn delete_pending_source_marker(&self, plan_id: &PlanId) -> anyhow::Result<()>;

    // ── job-sources/{job_id}.json ───────────────────────────────────────────
    //
    // The body the job ran, write-once by the first plan that asks for it. An
    // old source can still hold an `auth_token`, so a reader drops every token
    // before serving one.

    /// Create-if-absent; the bytes are `serde_json::to_vec(body)`, as `write_tmp`
    /// stages them.
    async fn put_job_source(
        &self,
        job_id: &JobId,
        body: &serde_json::Value,
    ) -> anyhow::Result<CreateOutcome>;

    /// A source that is not JSON fails with an [`UnreadableObjectError`] root,
    /// so a caller can tell it from I/O.
    async fn get_job_source(&self, job_id: &JobId) -> anyhow::Result<Option<serde_json::Value>>;

    /// Unordered; one that is not JSON is reported in `unreadable`, any other
    /// failure fails the read.
    async fn get_job_sources(
        &self,
        job_ids: &[JobId],
    ) -> anyhow::Result<Listing<(JobId, serde_json::Value)>> {
        // Owned ids: a borrowed stream fails the future's `Send` bound.
        let reads: Vec<(JobId, anyhow::Result<Option<serde_json::Value>>)> =
            futures::stream::iter(job_ids.to_vec())
                .map(|job_id| async move {
                    let read = self.get_job_source(&job_id).await;
                    (job_id, read)
                })
                .buffer_unordered(STORAGE_CONCURRENCY)
                .collect()
                .await;
        let mut listing = Listing::default();
        for (job_id, read) in reads {
            match read {
                Ok(Some(body)) => listing.items.push((job_id, body)),
                Ok(None) => {}
                Err(e) => match e.root_cause().downcast_ref::<UnreadableObjectError>() {
                    Some(unreadable) => listing.unreadable.push(UnreadableObjectError {
                        key: unreadable.key.clone(),
                        reason: unreadable.reason.clone(),
                    }),
                    None => return Err(e.context(format!("reading the source of job {job_id}"))),
                },
            }
        }
        Ok(listing)
    }

    // ── job-plan-links/{job_id}/{plan_id} ───────────────────────────────────
    //
    // One write-once link per plan that ever asked for a job; nothing deletes
    // one. Not the `job-plans/` markers, which go when a plan stops holding the job.

    /// Create-if-absent.
    async fn put_job_plan_link(
        &self,
        job_id: &JobId,
        plan_id: &PlanId,
    ) -> anyhow::Result<CreateOutcome>;

    /// Unordered; an unparseable key is warned and skipped. A read failure
    /// propagates rather than reporting no plan holds the job.
    async fn list_job_plan_links(&self, job_id: &JobId) -> anyhow::Result<Vec<PlanId>>;

    /// Unordered, deduplicated, unlinked jobs left out; a read failure propagates.
    async fn list_job_plan_links_of(
        &self,
        job_ids: &[JobId],
    ) -> anyhow::Result<Vec<(JobId, Vec<PlanId>)>> {
        // Owned ids: a borrowed stream fails the future's `Send` bound.
        let unique: HashSet<JobId> = job_ids.iter().cloned().collect();
        futures::stream::iter(unique)
            .map(|job_id| async move {
                let plan_ids = self
                    .list_job_plan_links(&job_id)
                    .await
                    .map_err(|e| e.context(format!("listing the plans of job {job_id}")))?;
                anyhow::Ok((job_id, plan_ids))
            })
            .buffer_unordered(STORAGE_CONCURRENCY)
            .try_filter(|(_, plan_ids)| std::future::ready(!plan_ids.is_empty()))
            .try_collect()
            .await
    }

    // ── job cells ───────────────────────────────────────────────────────────
    //
    // The cell digest of each minted job, which leaves out the eligibility
    // clauses, and the scope that may reuse its run. Only the SQL store keeps
    // this index; the object stores keep none, so they find no earlier run of
    // a cell.

    /// Record the cell that `job_id` measures, and the scope that may reuse
    /// its run. Create-if-absent: a job keeps the cell and the scope it was
    /// minted with.
    async fn put_job_cell(
        &self,
        _job_id: &JobId,
        _cell_digest: &str,
        _scope: &CellScope,
    ) -> anyhow::Result<()> {
        Ok(())
    }

    /// The finished runs of each of `cell_digests` that `scope` may reuse, in
    /// descending `job_id` order. A minted id holds a UUIDv7, so this order
    /// follows the mint time to the millisecond. The order of other ids is
    /// their text order. The method leaves out a run if its record holds no
    /// run flags, if a plan reran its job, or if its job has another scope or
    /// none. It gives at most [`CELL_RESULTS_PER_CELL`] runs of each cell, the
    /// newest. A cell whose only answering run is older runs again.
    async fn find_cell_results(
        &self,
        _cell_digests: &[String],
        _scope: &CellScope,
    ) -> anyhow::Result<Vec<CellResult>> {
        Ok(Vec::new())
    }
}

/// The most runs of one cell that [`PlanStore::find_cell_results`] gives. A
/// cell gets a job for each device that ran it, so its runs grow with the
/// history of the fleet.
pub const CELL_RESULTS_PER_CELL: usize = 32;

/// Who may reuse the run of a minted job ([`PlanStore::put_job_cell`]): the
/// organization of the owner of the campaign that minted it, or that owner
/// alone if the owner is a user in no organization.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CellScope {
    Org(OrgId),
    User(UserId),
}

impl CellScope {
    /// The text that the index stores. The prefix keeps the two kinds of id
    /// apart.
    pub fn key(&self) -> String {
        match self {
            Self::Org(org_id) => format!("org:{}", org_id.as_str()),
            Self::User(user_id) => format!("user:{}", user_id.as_str()),
        }
    }
}

impl std::fmt::Display for CellScope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.key())
    }
}

/// A finished run of a cell, from [`PlanStore::find_cell_results`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CellResult {
    pub cell_digest: String,
    pub job_id: JobId,
    /// The job source, which is the body that the job ran. An old source can
    /// still hold an `auth_token`. A reader must drop every token before it
    /// serves the body.
    pub body: serde_json::Value,
    /// The flags that the submission record proves the run held.
    pub run_flags: BTreeSet<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingSourceMarker {
    pub plan_id: PlanId,
    /// Backend modification time, which the GC's age gate reads.
    pub written_at: DateTime<Utc>,
}

/// Which job owns a content digest. Past `plan_creating_max_age_secs` from
/// `created_at`, a record whose job is not live is provably orphaned.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContentKeyRecord {
    pub job_id: JobId,
    pub plan_id: PlanId,
    pub created_at: DateTime<Utc>,
}

/// A handle on the exact bytes of a content-key record, for a CAS. A version
/// from another backend reads as a lost swap, never a match.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContentKeyVersion {
    /// `local_fs`: the digest of the bytes read.
    Digest(String),
    Object {
        e_tag: Option<String>,
        version: Option<String>,
    },
    /// SQL: bumped by every write of the record.
    Revision(i64),
}

#[derive(Debug)]
pub enum ContentKeyClaim {
    Created,
    AlreadyExists(ContentKeyRecord, ContentKeyVersion),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContentKeyCas {
    Applied,
    /// Another writer changed or removed the record first; nothing written.
    Lost,
}

#[derive(Debug, thiserror::Error)]
pub enum ContentKeyError {
    /// The create refused twice and the read found nothing twice: a GC racing
    /// the claim, which [`ContentKeyClaim`] cannot describe.
    #[error("content key at {path} was created and removed concurrently")]
    ConcurrentChurn { path: String },
}

/// 64 lowercase hex characters. Anything else under `content-keys/v1/` is
/// foreign, and a listing skips it.
pub fn is_content_digest(name: &str) -> bool {
    name.len() == 64
        && name
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobPlanMarker {
    pub job_id: JobId,
    pub plan_id: PlanId,
    pub expires_at: ExpiresAt,
    /// Backend modification time, which the GC's age gate reads.
    pub written_at: DateTime<Utc>,
}

#[derive(Debug)]
pub enum ClaimResult {
    Claimed(serde_json::Value),
    /// Another client won the race; skip to the next candidate.
    Gone,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RedateResult {
    Redated,
    /// A claim moved the entry first; skip the job.
    Gone,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecycleResult {
    Recycled,
    /// A benign race (the holder's submission or another recycler), not a
    /// storage failure.
    Gone,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RenewLeaseResult {
    /// Carries the job's `expires_at`, the other half of the renewed lease's key.
    Renewed(ExpiresAt),
    NotFound,
    WrongClient,
}

#[async_trait]
pub trait TodoStore: Send + Sync {
    // ── avail/ ──────────────────────────────────────────────────────────────
    /// Every `{job_id}/{expires_at}.json` key, in key order.
    async fn list_avail(&self) -> anyhow::Result<Vec<String>>;

    async fn get_avail(
        &self,
        job_id: &JobId,
        expires_at: ExpiresAt,
    ) -> anyhow::Result<Option<serde_json::Value>>;

    /// Idempotent.
    async fn delete_avail(&self, job_id: &JobId, expires_at: ExpiresAt) -> anyhow::Result<()>;

    /// Whatever `expires_at` the entry carries, for completion teardown, which
    /// has no expiry in hand. An entry exists only after a recycle race. Idempotent.
    async fn delete_avail_by_job(&self, job_id: &JobId) -> anyhow::Result<()>;

    /// Whatever `expires_at` the entry carries; a list then a read, so prefer
    /// [`Self::get_avail`] when the expiry is known.
    async fn get_avail_by_job(&self, job_id: &JobId) -> anyhow::Result<Option<serde_json::Value>>;

    // ── leased/ ─────────────────────────────────────────────────────────────
    /// Every `{client_id}/{job_id}.{expires_at}.{lease_expiry}.json` key.
    /// Prefer `list_leased_for_client` when the client is known.
    async fn list_leased(&self) -> anyhow::Result<Vec<String>>;

    async fn list_leased_for_client(&self, client_id: &ClientId) -> anyhow::Result<Vec<String>>;

    /// For the idempotent `claim` path: hands a re-polling client back its job
    /// without renewing the lease.
    async fn get_leased(
        &self,
        job_id: &JobId,
        expires_at: ExpiresAt,
        client_id: &ClientId,
        lease_expiry: DateTime<Utc>,
    ) -> anyhow::Result<Option<serde_json::Value>>;

    /// Heartbeat: atomically rename the client's lease to `new_expiry`. The
    /// store finds the lease itself; the job's `expires_at` is carried across.
    async fn renew_lease(
        &self,
        job_id: &JobId,
        client_id: &ClientId,
        new_expiry: DateTime<Utc>,
    ) -> anyhow::Result<RenewLeaseResult>;

    /// Idempotent.
    async fn delete_lease(
        &self,
        job_id: &JobId,
        expires_at: ExpiresAt,
        client_id: &ClientId,
        expiry: DateTime<Utc>,
    ) -> anyhow::Result<()>;

    // ── avail/ → leased/ (claim) ─────────────────────────────────────────
    /// Atomic rename. The lease name carries `expires_at`, so a recycle
    /// restores the exact `avail/` key.
    async fn claim_job(
        &self,
        job_id: &JobId,
        expires_at: ExpiresAt,
        client_id: &ClientId,
        lease_expiry: DateTime<Utc>,
    ) -> anyhow::Result<ClaimResult>;

    // ── avail/ → avail/ (re-date) ───────────────────────────────────────────
    /// The job's `avail/` key, which carries its live deadline. A read failure
    /// propagates: a swallowed one would read a live job as gone.
    async fn avail_key_for_job(&self, job_id: &JobId) -> anyhow::Result<Option<String>>;

    /// Atomic rename to a new deadline; the body is untouched, so the key is
    /// authoritative. Only the serialized expiry pass calls it, and a claim
    /// that moved the entry first makes it [`RedateResult::Gone`].
    async fn redate_avail(
        &self,
        job_id: &JobId,
        from: ExpiresAt,
        to: ExpiresAt,
    ) -> anyhow::Result<RedateResult>;

    // ── leased/ → avail/ (lease recycle) ────────────────────────────────────
    /// Atomic rename back to the exact key the job was claimed from; a pure
    /// rename, so it cannot fail on a malformed body.
    async fn recycle_lease(
        &self,
        job_id: &JobId,
        expires_at: ExpiresAt,
        client_id: &ClientId,
        lease_expiry: DateTime<Utc>,
    ) -> anyhow::Result<RecycleResult>;

    // ── denied/ ─────────────────────────────────────────────────────────────
    /// A second write for the same pair replaces the body.
    async fn write_denied(
        &self,
        job_id: &JobId,
        client_id: &ClientId,
        record: &DenialRecord,
    ) -> anyhow::Result<()>;
    /// An empty body reads as [`DenialRecord::default`]; an unparseable one errors.
    async fn get_denied(
        &self,
        job_id: &JobId,
        client_id: &ClientId,
    ) -> anyhow::Result<Option<DenialRecord>>;
    /// A single point read, as `claim` asks once per candidate. Every error
    /// but absence propagates: a swallowed one hands a failed job straight back.
    async fn is_denied(&self, job_id: &JobId, client_id: &ClientId) -> anyhow::Result<bool>;
    async fn delete_denied_for_job(&self, job_id: &JobId) -> anyhow::Result<()>;
    /// Idempotent. Drops one orphan without touching the job's other denials.
    async fn delete_denied(&self, job_id: &JobId, client_id: &ClientId) -> anyhow::Result<()>;
    /// Splits `{job_id}.{client_id}` on the `.`, which neither charset allows.
    async fn list_all_denied(&self) -> anyhow::Result<Vec<(JobId, ClientId)>>;

    // ── eligible/clients/ ────────────────────────────────────────────────────
    /// Unordered: a caller that needs a selection order imposes it.
    async fn list_eligible_for_client(
        &self,
        client_id: &ClientId,
    ) -> anyhow::Result<Vec<(JobId, ExpiresAt)>>;
    /// `expires_at` must match the job's `avail/` entry.
    async fn write_eligible(
        &self,
        client_id: &ClientId,
        job_id: &JobId,
        expires_at: ExpiresAt,
    ) -> anyhow::Result<()>;
    async fn delete_eligible_for_client(&self, client_id: &ClientId) -> anyhow::Result<()>;
    /// Idempotent. Drops one orphan, keeping the client's other markers.
    async fn delete_eligible(
        &self,
        client_id: &ClientId,
        job_id: &JobId,
        expires_at: ExpiresAt,
    ) -> anyhow::Result<()>;
    async fn list_all_eligible(&self) -> anyhow::Result<Vec<(ClientId, JobId, ExpiresAt)>>;

    // ── pending-reindex/ ────────────────────────────────────────────────────
    // One fresh `{client_id}.{uuid}` key per request, never overwritten: the
    // pass deletes only the keys it captured, so a flag raised mid-rebuild survives.
    async fn write_pending_reindex(&self, client_id: &ClientId) -> anyhow::Result<()>;
    /// `(client_id, key)` per flag; unparseable names are warned and skipped.
    async fn list_pending_reindex(&self) -> anyhow::Result<Vec<(ClientId, String)>>;
    /// Idempotent.
    async fn delete_pending_reindex(&self, key: &str) -> anyhow::Result<()>;
    /// The claim gate. An unreadable listing is an error: "couldn't check" must
    /// not read as "not flagged".
    async fn has_pending_reindex(&self, client_id: &ClientId) -> anyhow::Result<bool>;

    // ── pending-reindex-jobs/ ───────────────────────────────────────────────
    // A job whose `eligible/` markers may be stale for any client (leased
    // during a client reindex, or re-dated); the settle pass re-matches it.
    async fn write_pending_reindex_job(&self, job_id: &JobId) -> anyhow::Result<()>;
    async fn list_pending_reindex_jobs(&self) -> anyhow::Result<Vec<JobId>>;
    async fn delete_pending_reindex_job(&self, job_id: &JobId) -> anyhow::Result<()>;

    // ── lost/{job_id}/{client_id}.{lease_expiry} ────────────────────────────
    // One marker per lease that ended without a result; queue-maintenance
    // retires the job at `plan_lost_lease_limit`.
    /// Idempotent: the key names the lease.
    async fn write_lost_lease(
        &self,
        job_id: &JobId,
        client_id: &ClientId,
        lease_expiry: DateTime<Utc>,
    ) -> anyhow::Result<()>;
    /// Oldest first; foreign names are skipped.
    async fn list_lost_leases(
        &self,
        job_id: &JobId,
    ) -> anyhow::Result<Vec<(ClientId, DateTime<Utc>)>>;
    /// Idempotent.
    async fn delete_lost_leases_for_job(&self, job_id: &JobId) -> anyhow::Result<()>;
    async fn list_lost_lease_jobs(&self) -> anyhow::Result<Vec<JobId>>;

    // ── tmp/ ─────────────────────────────────────────────────────────────────
    /// Stage a job body, unclaimable until [`promote_avail`](Self::promote_avail).
    /// Ingestion stages every job before promoting any, so a staging failure
    /// leaves only stale `tmp/` files and nothing claimable.
    async fn write_tmp(&self, job_id: &JobId, body: &serde_json::Value) -> anyhow::Result<()>;

    /// Atomic rename into `avail/`, as `claim_job`'s, so a partial write never
    /// surfaces. A missing `tmp/` source is an error, not a no-op.
    async fn promote_avail(&self, job_id: &JobId, expires_at: ExpiresAt) -> anyhow::Result<()>;

    async fn list_stale_tmp(&self, age: Duration) -> anyhow::Result<Vec<String>>;
    async fn delete_tmp_object(&self, key: &str) -> anyhow::Result<()>;

    // ── eligible index state ─────────────────────────────────────────────────
    // Missing or corrupt state reads as empty: the next run then re-indexes
    // everything, and indexing is idempotent.
    /// The `avail/` keys the last `queue-maintenance` run indexed.
    async fn read_eligible_indexed(&self) -> anyhow::Result<HashSet<String>>;
    async fn write_eligible_indexed(&self, indexed: &HashSet<String>) -> anyhow::Result<()>;
    /// The workers the last run indexed, to find who joined or left.
    async fn read_eligible_workers(&self) -> anyhow::Result<HashSet<ClientId>>;
    async fn write_eligible_workers(&self, workers: &HashSet<ClientId>) -> anyhow::Result<()>;
    /// `None` before the first run, or when corrupt ([`parse_snapshot`]).
    async fn read_backlog(&self) -> anyhow::Result<Option<BacklogSnapshot>>;
    async fn write_backlog(&self, snapshot: &BacklogSnapshot) -> anyhow::Result<()>;
    /// `None` before the first run, or when corrupt ([`parse_snapshot`]).
    async fn read_campaign_backlog(&self) -> anyhow::Result<Option<CampaignBacklogSnapshot>>;
    async fn write_campaign_backlog(
        &self,
        snapshot: &CampaignBacklogSnapshot,
    ) -> anyhow::Result<()>;

    // ── GC orphan candidates ─────────────────────────────────────────────────
    /// Keys the last run saw orphaned; the sweep deletes only on the second
    /// sighting. Missing or corrupt reads as empty, delaying GC by one run.
    async fn read_gc_candidates(&self) -> anyhow::Result<HashSet<String>>;
    async fn write_gc_candidates(&self, candidates: &HashSet<String>) -> anyhow::Result<()>;

    // ── suspended/ ──────────────────────────────────────────────────────────
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

    /// From the listing alone: the fleet page reports the flag, never the record.
    async fn list_suspended_ids(&self) -> anyhow::Result<Vec<ClientId>>;

    // ── liveness/ ───────────────────────────────────────────────────────────
    /// A blind write of an empty marker whose modification time is the
    /// last-seen time; on S3 the server's clock sets it, so replicas agree.
    async fn stamp_liveness(&self, client_id: &ClientId) -> anyhow::Result<()>;
    async fn read_liveness(&self, client_id: &ClientId) -> anyhow::Result<Option<DateTime<Utc>>>;
    /// One listing, no body read; a name that is not a client id is warned and skipped.
    async fn list_liveness(&self) -> anyhow::Result<Vec<(ClientId, DateTime<Utc>)>>;
    /// Idempotent.
    async fn delete_liveness(&self, client_id: &ClientId) -> anyhow::Result<()>;

    // ── startup validation ────────────────────────────────────────────────────
    /// Fail fast if this backend cannot do the atomic renames claim and renew
    /// depend on (S3 needs Express One Zone).
    async fn validate_backend(&self) -> anyhow::Result<()>;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SuspensionRecord {
    pub suspended_at: DateTime<Utc>,
    pub conflicting_job_id: JobId,
}

/// The jobs each client could claim, as one `queue-maintenance` run counted them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BacklogSnapshot {
    /// The run's start; every listing counted is from then or later.
    pub counted_at: DateTime<Utc>,
    /// A client with no claimable job has no entry.
    pub jobs: BTreeMap<ClientId, u64>,
}

/// The jobs each client could claim, by campaign, as one `queue-maintenance`
/// run counted them. Only the device page reads this snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CampaignBacklogSnapshot {
    /// The run's start, the same as the [`BacklogSnapshot`] of the run.
    pub counted_at: DateTime<Utc>,
    /// A client with no claimable job has no entry.
    pub devices: BTreeMap<ClientId, CampaignBacklog>,
}

/// One client's claimable jobs, by campaign. A job counts once in each
/// campaign that holds it, so the counts can add up to more than the total.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CampaignBacklog {
    /// The distinct jobs that the plans of each campaign hold.
    pub by_campaign: BTreeMap<CampaignId, u64>,
    /// The jobs that no plan of a campaign holds.
    pub outside_campaigns: u64,
}

#[derive(Clone)]
pub struct Stores {
    pub catalog: Arc<dyn CatalogStore>,
    pub clients: Arc<dyn ClientStore>,
    pub users: Arc<dyn UserStore>,
    pub orgs: Arc<dyn OrgStore>,
    pub tags: Arc<dyn TagStore>,
    pub key_index: Arc<dyn KeyIndexStore>,
    pub submissions: Arc<dyn SubmissionStore>,
    pub warehouse: Arc<dyn WarehouseStore>,
    pub eval_sample_results: Arc<dyn EvalSampleResultStore>,
    pub todo: Arc<dyn TodoStore>,
    pub plans: Arc<dyn PlanStore>,
    pub campaigns: Arc<dyn CampaignStore>,
    pub model_params: Arc<dyn ModelParamsStore>,
    /// `None` without a `[database]`: no object store keeps the audit log.
    pub audit: Option<Arc<dyn AuditStore>>,
    pub descriptor_catalog: Option<Arc<dyn DescriptorCatalogStore>>,
    pub catalog_import_runs: Option<Arc<dyn CatalogImportRunStore>>,
    pub dataset_cache: Arc<dyn DatasetCacheStore>,
    pub vault: Arc<dyn VaultStore>,
}

/// What the audit event of a host command reads and writes: the records that
/// name its organizations, and the audit log.
#[derive(Clone)]
pub struct AuditStores {
    pub orgs: Arc<dyn OrgStore>,
    pub campaigns: Arc<dyn CampaignStore>,
    pub plans: Arc<dyn PlanStore>,
    pub vault: Arc<dyn VaultRecords>,
    pub log: Arc<dyn AuditStore>,
}

/// Best-effort removal of a deleted client's `todo/` state. Failures are
/// logged, not propagated: the identity is already gone, leftovers are inert,
/// and `queue-maintenance`'s orphan sweep collects them within two runs.
pub async fn purge_client_todo_state(todo: &dyn TodoStore, client_id: &ClientId) {
    if let Err(e) = todo.delete_suspension(client_id).await {
        tracing::warn!(client_id = %client_id, error = %e,
            "failed to delete suspension record while deleting client");
    }
    if let Err(e) = todo.delete_liveness(client_id).await {
        tracing::warn!(client_id = %client_id, error = %e,
            "failed to delete liveness marker while deleting client");
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

/// Outcome of [`reindex_tags`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ReindexReport {
    /// Unbacked reverse markers, plus both trees' markers of deleted clients.
    pub removed: usize,
    /// Missing reverse markers recreated.
    pub added: usize,
}

/// Rows a drain asks for per page. Not a config key: a drain returns the same
/// listing whatever this is.
pub const DRAIN_PAGE_SIZE: NonZeroUsize = nonzero_usize(500);

const fn nonzero_usize(value: usize) -> NonZeroUsize {
    match NonZeroUsize::new(value) {
        Some(n) => n,
        None => NonZeroUsize::MIN,
    }
}

/// [`PlanStore::list_matching_plans`] for a backend that can filter only on
/// status before it reads: pages `list_status_page` and filters the rest here.
pub async fn page_matching_plans<F, Fut>(
    filter: &PlanFilter,
    after: Option<&PlanId>,
    limit: NonZeroUsize,
    mut list_status_page: F,
) -> anyhow::Result<PlanPage>
where
    F: FnMut(Option<PlanId>, NonZeroUsize) -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<PlanPage>>,
{
    let mut page = PlanPage::default();
    let mut cursor = after.cloned();
    loop {
        let remaining = limit.get().saturating_sub(page.items.len());
        let Some(remaining) = NonZeroUsize::new(remaining) else {
            return Ok(page);
        };
        // At most `remaining` rows come back, so no admitted row is cut and no
        // report is repeated on the next page.
        let inner = list_status_page(cursor.take(), remaining).await?;
        page.items
            .extend(inner.items.into_iter().filter(|plan| filter.admits(plan)));
        page.unreadable.extend(inner.unreadable);
        page.resume_after.clone_from(&inner.resume_after);
        match inner.resume_after {
            Some(next) => cursor = Some(next),
            None => return Ok(page),
        }
    }
}

/// Every manifest matching `status`, in key order.
pub async fn drain_plans(
    plans: &dyn PlanStore,
    status: Option<PlanStatus>,
) -> anyhow::Result<PlanListing> {
    drain_matching_plans(
        plans,
        &PlanFilter {
            status,
            ..PlanFilter::default()
        },
    )
    .await
}

pub async fn drain_matching_plans(
    plans: &dyn PlanStore,
    filter: &PlanFilter,
) -> anyhow::Result<PlanListing> {
    let mut listing = PlanListing::default();
    let mut after: Option<PlanId> = None;
    loop {
        let page = plans
            .list_matching_plans(filter, after.as_ref(), DRAIN_PAGE_SIZE)
            .await?;
        listing.items.extend(page.items);
        listing.unreadable.extend(page.unreadable);
        // Only `None` ends the loop: a full page can be the last one.
        match page.resume_after {
            Some(cursor) => after = Some(cursor),
            None => return Ok(listing),
        }
    }
}

pub async fn drain_campaigns(campaigns: &dyn CampaignStore) -> anyhow::Result<CampaignListing> {
    let mut listing = CampaignListing::default();
    let mut after: Option<CampaignId> = None;
    loop {
        let page = campaigns
            .list_campaigns(after.as_ref(), DRAIN_PAGE_SIZE)
            .await?;
        listing.items.extend(page.items);
        listing.unreadable.extend(page.unreadable);
        match page.resume_after {
            Some(cursor) => after = Some(cursor),
            None => return Ok(listing),
        }
    }
}

pub async fn drain_model_params(
    store: &dyn ModelParamsStore,
) -> anyhow::Result<ModelParamsListing> {
    let mut listing = ModelParamsListing::default();
    let mut after: Option<ModelKey> = None;
    loop {
        let page = store
            .list_model_params(after.as_ref(), DRAIN_PAGE_SIZE)
            .await?;
        listing.items.extend(page.items);
        listing.unreadable.extend(page.unreadable);
        match page.resume_after {
            Some(cursor) => after = Some(cursor),
            None => return Ok(listing),
        }
    }
}

pub fn warn_unreadable_model_params(unreadable: &[UnreadableObjectError]) {
    unreadable.iter().for_each(|bad| {
        tracing::warn!(key = %bad.key, reason = %bad.reason,
            "skipping an unreadable model-params row");
    });
}

pub async fn drain_vault_resources(vault: &dyn VaultStore) -> anyhow::Result<VaultResourceListing> {
    let mut listing = VaultResourceListing::default();
    let mut after: Option<VaultResourceId> = None;
    loop {
        let page = vault
            .list_resources(after.as_ref(), DRAIN_PAGE_SIZE)
            .await?;
        listing.items.extend(page.items);
        listing.unreadable.extend(page.unreadable);
        match page.resume_after {
            Some(cursor) => after = Some(cursor),
            None => return Ok(listing),
        }
    }
}

pub fn warn_unreadable_clients(unreadable: &[UnreadableObjectError]) {
    unreadable.iter().for_each(|bad| {
        tracing::warn!(path = %bad.key, error = %bad.reason, "skipping malformed client record");
    });
}

/// In key order, not registration order.
pub async fn drain_clients(clients: &dyn ClientStore) -> anyhow::Result<ClientListing> {
    let mut listing = ClientListing::default();
    let mut after: Option<ClientId> = None;
    loop {
        let page = clients
            .list_clients(after.as_ref(), DRAIN_PAGE_SIZE)
            .await?;
        listing.items.extend(page.items);
        listing.unreadable.extend(page.unreadable);
        match page.resume_after {
            Some(cursor) => after = Some(cursor),
            None => return Ok(listing),
        }
    }
}

/// Reconcile the reverse tag index against the forward tree, dropping markers
/// of deleted clients. Idempotent. Refused when no client reads back but tags
/// exist: more likely a misconfigured `[auth_storage]` than a vanished fleet.
pub async fn reindex_tags(
    clients: &dyn ClientStore,
    tags: &dyn TagStore,
) -> anyhow::Result<ReindexReport> {
    let listing = drain_clients(clients).await?;
    warn_unreadable_clients(&listing.unreadable);
    let records: BTreeSet<ClientId> = listing.items.into_iter().map(|c| c.client_id).collect();
    let forward: BTreeSet<(ClientId, Tag)> =
        tags.list_forward_tag_markers().await?.into_iter().collect();
    if records.is_empty() && !forward.is_empty() {
        anyhow::bail!(
            "[auth_storage] holds no readable client record, but [storage] holds {} tag \
             marker(s). Reindexing would drop every tag, so it stops here: check that both \
             sections point at the same deployment.",
            forward.len()
        );
    }
    let reverse: BTreeSet<(ClientId, Tag)> =
        tags.list_reverse_tag_markers().await?.into_iter().collect();

    let valid: BTreeSet<(ClientId, Tag)> = forward
        .iter()
        .filter(|(id, _)| records.contains(id))
        .cloned()
        .collect();

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
    // The phases touch disjoint pairs and could overlap; sequential for a
    // simpler failure story.
    futures::stream::iter(
        to_remove
            .iter()
            .map(|(id, tag)| tags.remove_client_tag(id, tag)),
    )
    .buffer_unordered(STORAGE_CONCURRENCY)
    .try_collect::<Vec<_>>()
    .await?;
    futures::stream::iter(to_add.iter().map(|(id, tag)| tags.add_client_tag(id, tag)))
        .buffer_unordered(STORAGE_CONCURRENCY)
        .try_collect::<Vec<_>>()
        .await?;
    Ok(report)
}

/// Refuse a flat `{job_id}.{expires_at}.json` key from an earlier layout. This
/// build would read such a job as absent and silently sweep its markers,
/// losing its plan membership for good.
pub fn reject_unmigrated_avail_keys<'a>(
    keys: impl IntoIterator<Item = &'a str>,
) -> Result<(), UnmigratedAvailKeyError> {
    match keys
        .into_iter()
        .find(|key| pipette_core::todo_filename::parse_flat_avail_filename(key).is_ok())
    {
        Some(key) => Err(UnmigratedAvailKeyError(key.to_string())),
        None => Ok(()),
    }
}

#[derive(Debug, thiserror::Error)]
#[error(
    "avail/ holds the single-segment key {0:?} from an earlier layout; drain the \
     queue with the earlier build (cancel every plan and let its queue-maintenance \
     empty avail/), then deploy this build and re-ingest the plans"
)]
pub struct UnmigratedAvailKeyError(String);

/// For `serve` at startup. `queue-maintenance` checks the listing it already
/// holds instead, to avoid a second listing of the whole prefix.
pub async fn ensure_avail_layout_migrated(todo: &dyn TodoStore) -> anyhow::Result<()> {
    let keys = todo.list_avail().await?;
    reject_unmigrated_avail_keys(keys.iter().map(String::as_str))?;
    Ok(())
}

/// Max in-flight object-store requests for a fan-out over many keys.
pub const STORAGE_CONCURRENCY: usize = 16;

/// Max in-flight requests across one plan's jobs while it is filed or revised;
/// this width sets how long filing takes.
pub const PLAN_JOB_CONCURRENCY: usize = 64;
