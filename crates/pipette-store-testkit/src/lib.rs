//! Store contract assertions both backends run, and their fixtures. The
//! `*Unimplemented` fakes fill unused [`pipette_store::Stores`] fields and
//! error on any call; [`UnexpectedCalls`] records calls code may swallow.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::num::{NonZeroU64, NonZeroUsize};
use std::time::Duration;

use anyhow::Context;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use futures::TryStreamExt;
use sha2::Digest;

use pipette_core::audit::{RETENTION_DAYS, new_event};
use pipette_core::benchmark::Benchmark;
use pipette_core::campaign::{Campaign, CampaignOwner, NewCampaign};
use pipette_core::client::{Client, MigrationRecord};
use pipette_core::client_preauth::{ClientPreauthConsumeOutcome, ClientPreauthSecret};
use pipette_core::dataset::{DatasetArtifact, ProbeState, ProbeVerdict};
use pipette_core::denial::DenialRecord;
use pipette_core::key_index::{KeyClaim, KeyHolderPrincipalId};
use pipette_core::model_params::ModelParams;
use pipette_core::plan::{PlanHeader, PlanManifest, PlanSource, SettledOutcome, sample_manifest};
use pipette_core::types::Mint;
use pipette_core::user::UserKey;
use pipette_core::user_preauth::UserPreauthSecret;
use pipette_core::vault::{FoundBlob, VaultImport, VaultObjectKey, VaultOwner, VaultResource};
use pipette_plan_types::{
    AuditAction, AuditActor, AuditActorKind, AuditEvent, AuditEventId, AuditOutcome, AuditTarget,
    BatchSize, BenchmarkDefinition, BenchmarkId, CampaignId, ClientId, ClientPreauthKeyId,
    ExpiresAt, FirstClerkSignIn, JobId, ModelKey, NonEmptyTrimmedString, NonNegativeCount, OrgId,
    PlanId, PlanStatus, PositiveCount, PrefillThroughputBenchmark, PublicKeyHex, RuntimeOutput,
    Sha256, Tag, UserId, UserKeyId, VaultArtifact, VaultOrigin, VaultResourceId, VaultUploadFile,
};
use pipette_store::{
    ArtifactBody, AuditFilter, AuditPage, AuditStore, BacklogSnapshot, BlobBody, BlobWrite,
    CampaignBacklogSnapshot, CampaignPage, CampaignStore, CatalogStore, CellResult, CellScope,
    ClaimResult, ClientListing, ClientPage, ClientStore, ContentKeyCas, ContentKeyClaim,
    ContentKeyRecord, ContentKeyVersion, CreateOutcome, DatasetCacheStore, EmailClaim,
    JobPlanMarker, JobState, KeyIndexStore, ModelParamsPage, ModelParamsStore, OrgStore,
    PendingSourceMarker, PlanFilter, PlanListing, PlanPage, PlanReach, PlanStore, RecycleResult,
    RedateResult, RenewLeaseResult, ScoreQueueStage, SlugClaim, StagedWrite, SubmissionStore,
    SuspensionRecord, TagStore, TodoStore, UnreadableObjectError, UserStore, VaultResourcePage,
    VaultStore, drain_campaigns, drain_clients, drain_matching_plans, drain_model_params,
    drain_vault_resources,
};

/// A `NonZeroUsize` from a literal, without the `unwrap` the lint policy bans.
/// A zero would be a bug in the caller, so it reads as one rather than panicking.
const fn nonzero_usize(value: usize) -> NonZeroUsize {
    match NonZeroUsize::new(value) {
        Some(n) => n,
        None => NonZeroUsize::MIN,
    }
}

/// The calls a test double received that its test did not script: an error
/// alone is lost when the code under test logs it and moves on.
#[derive(Debug, Default)]
pub struct UnexpectedCalls(std::sync::Mutex<Vec<&'static str>>);

impl UnexpectedCalls {
    /// Record `method` and return the error the double answers with.
    pub fn record(&self, method: &'static str) -> anyhow::Error {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(method);
        anyhow::anyhow!("{method}: not scripted by this test double")
    }

    /// Fail if any call was recorded.
    pub fn none(&self) -> anyhow::Result<()> {
        let calls = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        anyhow::ensure!(calls.is_empty(), "unexpected calls: {calls:?}");
        Ok(())
    }
}

pub struct TodoStoreUnimplemented;

#[async_trait]
impl TodoStore for TodoStoreUnimplemented {
    async fn list_avail(&self) -> anyhow::Result<Vec<String>> {
        anyhow::bail!("list_avail: not implemented by this fake")
    }
    async fn get_avail(
        &self,
        _job_id: &JobId,
        _expires_at: ExpiresAt,
    ) -> anyhow::Result<Option<serde_json::Value>> {
        anyhow::bail!("get_avail: not implemented by this fake")
    }
    async fn delete_avail(&self, _job_id: &JobId, _expires_at: ExpiresAt) -> anyhow::Result<()> {
        anyhow::bail!("delete_avail: not implemented by this fake")
    }
    async fn delete_avail_by_job(&self, _job_id: &JobId) -> anyhow::Result<()> {
        anyhow::bail!("delete_avail_by_job: not implemented by this fake")
    }
    async fn get_avail_by_job(&self, _job_id: &JobId) -> anyhow::Result<Option<serde_json::Value>> {
        anyhow::bail!("get_avail_by_job: not implemented by this fake")
    }
    async fn avail_key_for_job(&self, _job_id: &JobId) -> anyhow::Result<Option<String>> {
        anyhow::bail!("avail_key_for_job: not implemented by this fake")
    }
    async fn redate_avail(
        &self,
        _job_id: &JobId,
        _from: ExpiresAt,
        _to: ExpiresAt,
    ) -> anyhow::Result<RedateResult> {
        anyhow::bail!("redate_avail: not implemented by this fake")
    }
    async fn list_leased(&self) -> anyhow::Result<Vec<String>> {
        anyhow::bail!("list_leased: not implemented by this fake")
    }
    async fn list_leased_for_client(&self, _client_id: &ClientId) -> anyhow::Result<Vec<String>> {
        anyhow::bail!("list_leased_for_client: not implemented by this fake")
    }
    async fn get_leased(
        &self,
        _job_id: &JobId,
        _expires_at: ExpiresAt,
        _client_id: &ClientId,
        _lease_expiry: DateTime<Utc>,
    ) -> anyhow::Result<Option<serde_json::Value>> {
        anyhow::bail!("get_leased: not implemented by this fake")
    }
    async fn renew_lease(
        &self,
        _job_id: &JobId,
        _client_id: &ClientId,
        _new_expiry: DateTime<Utc>,
    ) -> anyhow::Result<RenewLeaseResult> {
        anyhow::bail!("renew_lease: not implemented by this fake")
    }
    async fn delete_lease(
        &self,
        _job_id: &JobId,
        _expires_at: ExpiresAt,
        _client_id: &ClientId,
        _expiry: DateTime<Utc>,
    ) -> anyhow::Result<()> {
        anyhow::bail!("delete_lease: not implemented by this fake")
    }
    async fn claim_job(
        &self,
        _job_id: &JobId,
        _expires_at: ExpiresAt,
        _client_id: &ClientId,
        _lease_expiry: DateTime<Utc>,
    ) -> anyhow::Result<ClaimResult> {
        anyhow::bail!("claim_job: not implemented by this fake")
    }
    async fn recycle_lease(
        &self,
        _job_id: &JobId,
        _expires_at: ExpiresAt,
        _client_id: &ClientId,
        _lease_expiry: DateTime<Utc>,
    ) -> anyhow::Result<RecycleResult> {
        anyhow::bail!("recycle_lease: not implemented by this fake")
    }
    async fn write_denied(
        &self,
        _job_id: &JobId,
        _client_id: &ClientId,
        _record: &DenialRecord,
    ) -> anyhow::Result<()> {
        anyhow::bail!("write_denied: not implemented by this fake")
    }
    async fn get_denied(
        &self,
        _job_id: &JobId,
        _client_id: &ClientId,
    ) -> anyhow::Result<Option<DenialRecord>> {
        anyhow::bail!("get_denied: not implemented by this fake")
    }
    async fn is_denied(&self, _job_id: &JobId, _client_id: &ClientId) -> anyhow::Result<bool> {
        anyhow::bail!("is_denied: not implemented by this fake")
    }
    async fn delete_denied_for_job(&self, _job_id: &JobId) -> anyhow::Result<()> {
        anyhow::bail!("delete_denied_for_job: not implemented by this fake")
    }
    async fn delete_denied(&self, _job_id: &JobId, _client_id: &ClientId) -> anyhow::Result<()> {
        anyhow::bail!("delete_denied: not implemented by this fake")
    }
    async fn list_all_denied(&self) -> anyhow::Result<Vec<(JobId, ClientId)>> {
        anyhow::bail!("list_all_denied: not implemented by this fake")
    }
    async fn list_eligible_for_client(
        &self,
        _client_id: &ClientId,
    ) -> anyhow::Result<Vec<(JobId, ExpiresAt)>> {
        anyhow::bail!("list_eligible_for_client: not implemented by this fake")
    }
    async fn write_eligible(
        &self,
        _client_id: &ClientId,
        _job_id: &JobId,
        _expires_at: ExpiresAt,
    ) -> anyhow::Result<()> {
        anyhow::bail!("write_eligible: not implemented by this fake")
    }
    async fn delete_eligible_for_client(&self, _client_id: &ClientId) -> anyhow::Result<()> {
        anyhow::bail!("delete_eligible_for_client: not implemented by this fake")
    }
    async fn delete_eligible(
        &self,
        _client_id: &ClientId,
        _job_id: &JobId,
        _expires_at: ExpiresAt,
    ) -> anyhow::Result<()> {
        anyhow::bail!("delete_eligible: not implemented by this fake")
    }
    async fn list_all_eligible(&self) -> anyhow::Result<Vec<(ClientId, JobId, ExpiresAt)>> {
        anyhow::bail!("list_all_eligible: not implemented by this fake")
    }
    async fn write_pending_reindex(&self, _client_id: &ClientId) -> anyhow::Result<()> {
        anyhow::bail!("write_pending_reindex: not implemented by this fake")
    }
    async fn list_pending_reindex(&self) -> anyhow::Result<Vec<(ClientId, String)>> {
        anyhow::bail!("list_pending_reindex: not implemented by this fake")
    }
    async fn delete_pending_reindex(&self, _key: &str) -> anyhow::Result<()> {
        anyhow::bail!("delete_pending_reindex: not implemented by this fake")
    }
    async fn has_pending_reindex(&self, _client_id: &ClientId) -> anyhow::Result<bool> {
        anyhow::bail!("has_pending_reindex: not implemented by this fake")
    }
    async fn write_pending_reindex_job(&self, _job_id: &JobId) -> anyhow::Result<()> {
        anyhow::bail!("write_pending_reindex_job: not implemented by this fake")
    }
    async fn list_pending_reindex_jobs(&self) -> anyhow::Result<Vec<JobId>> {
        anyhow::bail!("list_pending_reindex_jobs: not implemented by this fake")
    }
    async fn delete_pending_reindex_job(&self, _job_id: &JobId) -> anyhow::Result<()> {
        anyhow::bail!("delete_pending_reindex_job: not implemented by this fake")
    }
    async fn write_lost_lease(
        &self,
        _job_id: &JobId,
        _client_id: &ClientId,
        _lease_expiry: DateTime<Utc>,
    ) -> anyhow::Result<()> {
        anyhow::bail!("write_lost_lease: not implemented by this fake")
    }
    async fn list_lost_leases(
        &self,
        _job_id: &JobId,
    ) -> anyhow::Result<Vec<(ClientId, DateTime<Utc>)>> {
        anyhow::bail!("list_lost_leases: not implemented by this fake")
    }
    async fn delete_lost_leases_for_job(&self, _job_id: &JobId) -> anyhow::Result<()> {
        anyhow::bail!("delete_lost_leases_for_job: not implemented by this fake")
    }
    async fn list_lost_lease_jobs(&self) -> anyhow::Result<Vec<JobId>> {
        anyhow::bail!("list_lost_lease_jobs: not implemented by this fake")
    }
    async fn write_tmp(&self, _job_id: &JobId, _body: &serde_json::Value) -> anyhow::Result<()> {
        anyhow::bail!("write_tmp: not implemented by this fake")
    }
    async fn promote_avail(&self, _job_id: &JobId, _expires_at: ExpiresAt) -> anyhow::Result<()> {
        anyhow::bail!("promote_avail: not implemented by this fake")
    }
    async fn list_stale_tmp(&self, _age: Duration) -> anyhow::Result<Vec<String>> {
        anyhow::bail!("list_stale_tmp: not implemented by this fake")
    }
    async fn delete_tmp_object(&self, _key: &str) -> anyhow::Result<()> {
        anyhow::bail!("delete_tmp_object: not implemented by this fake")
    }
    async fn read_eligible_indexed(&self) -> anyhow::Result<HashSet<String>> {
        anyhow::bail!("read_eligible_indexed: not implemented by this fake")
    }
    async fn write_eligible_indexed(&self, _indexed: &HashSet<String>) -> anyhow::Result<()> {
        anyhow::bail!("write_eligible_indexed: not implemented by this fake")
    }
    async fn read_eligible_workers(&self) -> anyhow::Result<HashSet<ClientId>> {
        anyhow::bail!("read_eligible_workers: not implemented by this fake")
    }
    async fn write_eligible_workers(&self, _workers: &HashSet<ClientId>) -> anyhow::Result<()> {
        anyhow::bail!("write_eligible_workers: not implemented by this fake")
    }
    async fn read_backlog(&self) -> anyhow::Result<Option<BacklogSnapshot>> {
        anyhow::bail!("read_backlog: not implemented by this fake")
    }
    async fn write_backlog(&self, _snapshot: &BacklogSnapshot) -> anyhow::Result<()> {
        anyhow::bail!("write_backlog: not implemented by this fake")
    }
    async fn read_campaign_backlog(&self) -> anyhow::Result<Option<CampaignBacklogSnapshot>> {
        anyhow::bail!("read_campaign_backlog: not implemented by this fake")
    }
    async fn write_campaign_backlog(
        &self,
        _snapshot: &CampaignBacklogSnapshot,
    ) -> anyhow::Result<()> {
        anyhow::bail!("write_campaign_backlog: not implemented by this fake")
    }
    async fn read_gc_candidates(&self) -> anyhow::Result<HashSet<String>> {
        anyhow::bail!("read_gc_candidates: not implemented by this fake")
    }
    async fn write_gc_candidates(&self, _candidates: &HashSet<String>) -> anyhow::Result<()> {
        anyhow::bail!("write_gc_candidates: not implemented by this fake")
    }
    async fn write_suspension(
        &self,
        _client_id: &ClientId,
        _suspended_at: DateTime<Utc>,
        _conflicting_job_id: &JobId,
    ) -> anyhow::Result<()> {
        anyhow::bail!("write_suspension: not implemented by this fake")
    }
    async fn read_suspension(
        &self,
        _client_id: &ClientId,
    ) -> anyhow::Result<Option<SuspensionRecord>> {
        anyhow::bail!("read_suspension: not implemented by this fake")
    }
    async fn delete_suspension(&self, _client_id: &ClientId) -> anyhow::Result<()> {
        anyhow::bail!("delete_suspension: not implemented by this fake")
    }
    async fn list_suspensions(&self) -> anyhow::Result<Vec<(ClientId, SuspensionRecord)>> {
        anyhow::bail!("list_suspensions: not implemented by this fake")
    }
    async fn list_suspended_ids(&self) -> anyhow::Result<Vec<ClientId>> {
        anyhow::bail!("list_suspended_ids: not implemented by this fake")
    }
    async fn stamp_liveness(&self, _client_id: &ClientId) -> anyhow::Result<()> {
        anyhow::bail!("stamp_liveness: not implemented by this fake")
    }
    async fn read_liveness(&self, _client_id: &ClientId) -> anyhow::Result<Option<DateTime<Utc>>> {
        anyhow::bail!("read_liveness: not implemented by this fake")
    }
    async fn list_liveness(&self) -> anyhow::Result<Vec<(ClientId, DateTime<Utc>)>> {
        anyhow::bail!("list_liveness: not implemented by this fake")
    }
    async fn delete_liveness(&self, _client_id: &ClientId) -> anyhow::Result<()> {
        anyhow::bail!("delete_liveness: not implemented by this fake")
    }
    async fn validate_backend(&self) -> anyhow::Result<()> {
        anyhow::bail!("validate_backend: not implemented by this fake")
    }
}

pub struct CampaignStoreUnimplemented;

#[async_trait]
impl CampaignStore for CampaignStoreUnimplemented {
    async fn get_campaign(&self, _campaign_id: &CampaignId) -> anyhow::Result<Option<Campaign>> {
        anyhow::bail!("get_campaign: not implemented by this fake")
    }
    async fn put_campaign(&self, _campaign: &Campaign) -> anyhow::Result<()> {
        anyhow::bail!("put_campaign: not implemented by this fake")
    }
    async fn list_campaigns(
        &self,
        _after: Option<&CampaignId>,
        _limit: NonZeroUsize,
    ) -> anyhow::Result<CampaignPage> {
        anyhow::bail!("list_campaigns: not implemented by this fake")
    }
    async fn delete_campaign(&self, _campaign_id: &CampaignId) -> anyhow::Result<()> {
        anyhow::bail!("delete_campaign: not implemented by this fake")
    }
}

pub struct ModelParamsStoreUnimplemented;

#[async_trait]
impl ModelParamsStore for ModelParamsStoreUnimplemented {
    async fn get_model_params(&self, _key: &ModelKey) -> anyhow::Result<Option<ModelParams>> {
        anyhow::bail!("get_model_params: not implemented by this fake")
    }
    async fn put_model_params(&self, _params: &ModelParams) -> anyhow::Result<()> {
        anyhow::bail!("put_model_params: not implemented by this fake")
    }
    async fn list_model_params(
        &self,
        _after: Option<&ModelKey>,
        _limit: NonZeroUsize,
    ) -> anyhow::Result<ModelParamsPage> {
        anyhow::bail!("list_model_params: not implemented by this fake")
    }
    async fn delete_model_params(&self, _key: &ModelKey) -> anyhow::Result<()> {
        anyhow::bail!("delete_model_params: not implemented by this fake")
    }
}

pub struct VaultStoreUnimplemented;

#[async_trait]
impl VaultStore for VaultStoreUnimplemented {
    async fn get_resource(
        &self,
        _resource_id: &VaultResourceId,
    ) -> anyhow::Result<Option<VaultResource>> {
        anyhow::bail!("get_resource: not implemented by this fake")
    }
    async fn put_resource(&self, _resource: &VaultResource) -> anyhow::Result<()> {
        anyhow::bail!("put_resource: not implemented by this fake")
    }
    async fn list_resources(
        &self,
        _after: Option<&VaultResourceId>,
        _limit: NonZeroUsize,
    ) -> anyhow::Result<VaultResourcePage> {
        anyhow::bail!("list_resources: not implemented by this fake")
    }
    async fn delete_resource(&self, _resource_id: &VaultResourceId) -> anyhow::Result<()> {
        anyhow::bail!("delete_resource: not implemented by this fake")
    }
    async fn stat_blob(&self, _key: &VaultObjectKey) -> anyhow::Result<Option<u64>> {
        anyhow::bail!("stat_blob: not implemented by this fake")
    }
    async fn delete_blob(&self, _key: &VaultObjectKey) -> anyhow::Result<()> {
        anyhow::bail!("delete_blob: not implemented by this fake")
    }
    async fn put_blob(
        &self,
        _key: &VaultObjectKey,
        _sha256: &Sha256,
        _body: BlobBody,
    ) -> anyhow::Result<BlobWrite> {
        anyhow::bail!("put_blob: not implemented by this fake")
    }
    async fn open_blob(
        &self,
        _key: &VaultObjectKey,
        _from: u64,
    ) -> anyhow::Result<Option<ArtifactBody>> {
        anyhow::bail!("open_blob: not implemented by this fake")
    }
    async fn put_staged_blob(
        &self,
        _owner: &VaultOwner,
        _body: BlobBody,
        _limit: NonZeroU64,
    ) -> anyhow::Result<StagedWrite> {
        anyhow::bail!("put_staged_blob: not implemented by this fake")
    }
    async fn get_import(
        &self,
        _owner: &VaultOwner,
        _import: &Sha256,
    ) -> anyhow::Result<Option<VaultImport>> {
        anyhow::bail!("get_import: not implemented by this fake")
    }
    async fn put_import(
        &self,
        _owner: &VaultOwner,
        _import: &Sha256,
        _entry: &VaultImport,
    ) -> anyhow::Result<()> {
        anyhow::bail!("put_import: not implemented by this fake")
    }
}

/// Errors on every call, so a test stands it in for a tag read that fails.
pub struct TagStoreUnimplemented;

#[async_trait]
impl TagStore for TagStoreUnimplemented {
    async fn add_client_tag(&self, _client_id: &ClientId, _tag: &Tag) -> anyhow::Result<()> {
        anyhow::bail!("add_client_tag: not implemented by this fake")
    }
    async fn remove_client_tag(&self, _client_id: &ClientId, _tag: &Tag) -> anyhow::Result<()> {
        anyhow::bail!("remove_client_tag: not implemented by this fake")
    }
    async fn delete_client_tags(&self, _client_id: &ClientId) -> anyhow::Result<()> {
        anyhow::bail!("delete_client_tags: not implemented by this fake")
    }
    async fn get_client_tags(&self, _client_id: &ClientId) -> anyhow::Result<BTreeSet<Tag>> {
        anyhow::bail!("get_client_tags: not implemented by this fake")
    }
    async fn list_client_ids_by_tag(&self, _tag: &Tag) -> anyhow::Result<Vec<ClientId>> {
        anyhow::bail!("list_client_ids_by_tag: not implemented by this fake")
    }
    async fn list_forward_tag_markers(&self) -> anyhow::Result<Vec<(ClientId, Tag)>> {
        anyhow::bail!("list_forward_tag_markers: not implemented by this fake")
    }
    async fn list_reverse_tag_markers(&self) -> anyhow::Result<Vec<(ClientId, Tag)>> {
        anyhow::bail!("list_reverse_tag_markers: not implemented by this fake")
    }
}

/// An [`AuditStore`] that keeps each appended event in memory, for a test
/// about what is recorded and when. Its reads, prunes and counts return an
/// error.
#[derive(Default)]
pub struct CapturingAuditStore(std::sync::Mutex<Vec<AuditEvent>>);

impl CapturingAuditStore {
    /// The events appended so far, oldest first.
    pub fn events(&self) -> Vec<AuditEvent> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

#[async_trait]
impl AuditStore for CapturingAuditStore {
    async fn append(&self, event: &AuditEvent) -> anyhow::Result<()> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(event.clone());
        Ok(())
    }

    async fn list_events(
        &self,
        _: &AuditFilter,
        _: Option<&AuditEventId>,
        _: NonZeroUsize,
    ) -> anyhow::Result<AuditPage> {
        anyhow::bail!("list_events: not implemented by this fake")
    }

    async fn prune_before(&self, _: DateTime<Utc>) -> anyhow::Result<u64> {
        anyhow::bail!("prune_before: not implemented by this fake")
    }

    async fn count_before(&self, _: DateTime<Utc>) -> anyhow::Result<u64> {
        anyhow::bail!("count_before: not implemented by this fake")
    }
}

pub struct PlanStoreUnimplemented;

#[async_trait]
impl PlanStore for PlanStoreUnimplemented {
    async fn put_plan(&self, _manifest: &PlanManifest) -> anyhow::Result<()> {
        anyhow::bail!("put_plan: not implemented by this fake")
    }
    async fn get_plan(&self, _plan_id: &PlanId) -> anyhow::Result<Option<PlanManifest>> {
        anyhow::bail!("get_plan: not implemented by this fake")
    }
    async fn list_matching_plans(
        &self,
        _filter: &PlanFilter,
        _after: Option<&PlanId>,
        _limit: NonZeroUsize,
    ) -> anyhow::Result<PlanPage> {
        anyhow::bail!("list_matching_plans: not implemented by this fake")
    }
    async fn write_cancel_marker(&self, _plan_id: &PlanId) -> anyhow::Result<()> {
        anyhow::bail!("write_cancel_marker: not implemented by this fake")
    }
    async fn has_cancel_marker(&self, _plan_id: &PlanId) -> anyhow::Result<bool> {
        anyhow::bail!("has_cancel_marker: not implemented by this fake")
    }
    async fn list_cancel_markers(&self) -> anyhow::Result<Vec<PlanId>> {
        anyhow::bail!("list_cancel_markers: not implemented by this fake")
    }
    async fn delete_cancel_marker(&self, _plan_id: &PlanId) -> anyhow::Result<()> {
        anyhow::bail!("delete_cancel_marker: not implemented by this fake")
    }
    async fn get_content_key(
        &self,
        _digest: &str,
    ) -> anyhow::Result<Option<(ContentKeyRecord, ContentKeyVersion)>> {
        anyhow::bail!("get_content_key: not implemented by this fake")
    }
    async fn create_content_key(
        &self,
        _digest: &str,
        _record: &ContentKeyRecord,
    ) -> anyhow::Result<ContentKeyClaim> {
        anyhow::bail!("create_content_key: not implemented by this fake")
    }
    async fn replace_content_key(
        &self,
        _digest: &str,
        _version: &ContentKeyVersion,
        _record: &ContentKeyRecord,
    ) -> anyhow::Result<ContentKeyCas> {
        anyhow::bail!("replace_content_key: not implemented by this fake")
    }
    async fn delete_content_key(
        &self,
        _digest: &str,
        _version: &ContentKeyVersion,
    ) -> anyhow::Result<ContentKeyCas> {
        anyhow::bail!("delete_content_key: not implemented by this fake")
    }
    async fn list_content_keys(
        &self,
    ) -> anyhow::Result<Vec<(String, ContentKeyRecord, ContentKeyVersion)>> {
        anyhow::bail!("list_content_keys: not implemented by this fake")
    }
    async fn write_job_plan_marker(
        &self,
        _job_id: &JobId,
        _plan_id: &PlanId,
        _expires_at: ExpiresAt,
    ) -> anyhow::Result<()> {
        anyhow::bail!("write_job_plan_marker: not implemented by this fake")
    }
    async fn list_job_plan_markers(
        &self,
        _job_id: &JobId,
    ) -> anyhow::Result<Vec<(PlanId, ExpiresAt)>> {
        anyhow::bail!("list_job_plan_markers: not implemented by this fake")
    }
    async fn list_all_job_plan_markers(&self) -> anyhow::Result<Vec<JobPlanMarker>> {
        anyhow::bail!("list_all_job_plan_markers: not implemented by this fake")
    }
    async fn delete_job_plan_marker(
        &self,
        _job_id: &JobId,
        _plan_id: &PlanId,
        _expires_at: ExpiresAt,
    ) -> anyhow::Result<()> {
        anyhow::bail!("delete_job_plan_marker: not implemented by this fake")
    }
    async fn put_plan_source(
        &self,
        _plan_id: &PlanId,
        _source: &PlanSource,
    ) -> anyhow::Result<CreateOutcome> {
        anyhow::bail!("put_plan_source: not implemented by this fake")
    }
    async fn get_plan_source(&self, _plan_id: &PlanId) -> anyhow::Result<Option<PlanSource>> {
        anyhow::bail!("get_plan_source: not implemented by this fake")
    }
    async fn delete_plan_source(&self, _plan_id: &PlanId) -> anyhow::Result<()> {
        anyhow::bail!("delete_plan_source: not implemented by this fake")
    }
    async fn write_pending_source_marker(&self, _plan_id: &PlanId) -> anyhow::Result<()> {
        anyhow::bail!("write_pending_source_marker: not implemented by this fake")
    }
    async fn list_pending_source_markers(&self) -> anyhow::Result<Vec<PendingSourceMarker>> {
        anyhow::bail!("list_pending_source_markers: not implemented by this fake")
    }
    async fn delete_pending_source_marker(&self, _plan_id: &PlanId) -> anyhow::Result<()> {
        anyhow::bail!("delete_pending_source_marker: not implemented by this fake")
    }
    async fn put_job_source(
        &self,
        _job_id: &JobId,
        _body: &serde_json::Value,
    ) -> anyhow::Result<CreateOutcome> {
        anyhow::bail!("put_job_source: not implemented by this fake")
    }
    async fn get_job_source(&self, _job_id: &JobId) -> anyhow::Result<Option<serde_json::Value>> {
        anyhow::bail!("get_job_source: not implemented by this fake")
    }
    async fn put_job_plan_link(
        &self,
        _job_id: &JobId,
        _plan_id: &PlanId,
    ) -> anyhow::Result<CreateOutcome> {
        anyhow::bail!("put_job_plan_link: not implemented by this fake")
    }
    async fn list_job_plan_links(&self, _job_id: &JobId) -> anyhow::Result<Vec<PlanId>> {
        anyhow::bail!("list_job_plan_links: not implemented by this fake")
    }
    async fn put_job_cell(
        &self,
        _job_id: &JobId,
        _cell_digest: &str,
        _scope: &CellScope,
    ) -> anyhow::Result<()> {
        anyhow::bail!("put_job_cell: not implemented by this fake")
    }
    async fn find_cell_results(
        &self,
        _cell_digests: &[String],
        _scope: &CellScope,
    ) -> anyhow::Result<Vec<CellResult>> {
        anyhow::bail!("find_cell_results: not implemented by this fake")
    }
}

/// Large enough to return any test fixture's full backlog in one page.
pub const TEST_LIST_LIMIT: std::num::NonZeroUsize = std::num::NonZeroUsize::new(100).unwrap();

/// The signature-migration marker keeps the *first* `v1` sighting, so the
/// operator view reports the migration, not the client's last request.
/// `write_under_root` writes a key relative to the auth root.
pub async fn assert_signature_migration_keeps_first_sighting<F, Fut>(
    store: &dyn ClientStore,
    write_under_root: F,
) -> anyhow::Result<()>
where
    F: Fn(String, Vec<u8>) -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<()>>,
{
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
    let (_, record) = listed.first().context("expected one migration")?;
    write_under_root(
        format!("signature-migration/ {client_id}.json"),
        serde_json::to_vec(record)?,
    )
    .await?;
    let listed = store.list_signature_migrations().await?;
    assert_eq!(listed.len(), 1, "a padded key is not a second migration");
    let (listed_id, listed_record) = listed.first().context("expected one migration")?;
    assert_eq!(*listed_id, client_id);
    assert_eq!(listed_record.first_seen, first);
    Ok(())
}

/// The `UserStore` invite surface. Takes a `ClientStore` view of the *same*
/// store: a client key and an invite minted with the same id must not see
/// each other.
pub async fn assert_user_preauth_store_contract(
    users: &dyn UserStore,
    clients: &dyn ClientStore,
) -> anyhow::Result<()> {
    use pipette_core::user_preauth::{
        UserPreauthConsumeOutcome, UserPreauthParams, UserPreauthRejectionError,
    };
    use pipette_plan_types::UserId;
    use pipette_plan_types::UserPreauthUsage;

    let account = UserId::mint();
    let minted = pipette_core::user_preauth::mint(
        UserPreauthParams {
            user_id: account.clone(),
            usage: UserPreauthUsage::SingleUse,
            expires_at: None,
            note: None,
        },
        Utc::now(),
    )?;
    let key_id = minted.key.key_id.clone();
    let (_, secret) = minted
        .token
        .parse_parts()
        .ok_or_else(|| anyhow::anyhow!("a minted token must parse"))?;

    // An invite that was never persisted is unknown, not an error.
    assert!(matches!(
        users.consume_user_preauth_key(&key_id, &secret).await?,
        UserPreauthConsumeOutcome::Rejected(UserPreauthRejectionError::NotFound)
    ));

    users.put_user_preauth_key(&minted.key).await?;
    let listed = users.list_user_preauth_keys().await?;
    assert_eq!(listed.len(), 1);
    let invite = listed.first().context("expected one invite")?;
    assert_eq!(invite.user_id, account, "the invite names its account");

    assert!(
        clients.list_client_preauth_keys().await?.is_empty(),
        "an invite must not appear in the client key listing"
    );
    // Only the listing is checked: the id types differ, so a cross-surface
    // consume does not compile; the keyspaces must be disjoint on disk too.

    // A wrong secret is `BadSecret`; the invite survives it.
    assert!(matches!(
        users
            .consume_user_preauth_key(&key_id, &UserPreauthSecret::new("wrong"))
            .await?,
        UserPreauthConsumeOutcome::Rejected(UserPreauthRejectionError::BadSecret)
    ));
    assert_eq!(users.list_user_preauth_keys().await?.len(), 1);

    let UserPreauthConsumeOutcome::Granted(grant) =
        users.consume_user_preauth_key(&key_id, &secret).await?
    else {
        anyhow::bail!("a valid single-use invite must grant");
    };
    assert_eq!(grant.user_id, account);

    // Single-use: spent, so the record is gone and a replay is unknown.
    assert!(users.list_user_preauth_keys().await?.is_empty());
    assert_eq!(
        users.list_spent_user_preauth_markers().await?,
        vec![key_id.clone()]
    );
    assert!(matches!(
        users.consume_user_preauth_key(&key_id, &secret).await?,
        UserPreauthConsumeOutcome::Rejected(UserPreauthRejectionError::NotFound)
    ));

    // The spend marker is the user keyspace's alone.
    assert!(
        clients
            .list_spent_client_preauth_markers()
            .await?
            .is_empty(),
        "spending an invite must not mark a client key spent"
    );

    users.delete_spent_user_preauth_marker(&key_id).await?;
    assert!(users.list_spent_user_preauth_markers().await?.is_empty());
    // Idempotent.
    users.delete_spent_user_preauth_marker(&key_id).await?;
    users.delete_user_preauth_key(&key_id).await?;

    // Multi-use is where the kinds diverge: an invite attaches many keys to one
    // account, so it must survive a consume and never write a spend marker.
    let multi = pipette_core::user_preauth::mint(
        UserPreauthParams {
            user_id: account.clone(),
            usage: UserPreauthUsage::MultiUse,
            expires_at: None,
            note: None,
        },
        Utc::now(),
    )?;
    let multi_id = multi.key.key_id.clone();
    let (_, multi_secret) = multi
        .token
        .parse_parts()
        .ok_or_else(|| anyhow::anyhow!("a minted token must parse"))?;
    users.put_user_preauth_key(&multi.key).await?;

    for attempt in 1..=2 {
        let UserPreauthConsumeOutcome::Granted(grant) = users
            .consume_user_preauth_key(&multi_id, &multi_secret)
            .await?
        else {
            anyhow::bail!("consume {attempt} of a multi-use invite must grant");
        };
        assert_eq!(
            grant.user_id, account,
            "every consume names the same account"
        );
    }
    assert_eq!(
        users.list_user_preauth_keys().await?.len(),
        1,
        "consuming a multi-use invite must not delete it"
    );
    assert!(
        users.list_spent_user_preauth_markers().await?.is_empty(),
        "a multi-use invite writes no spend marker, so there is nothing to race on"
    );

    users.delete_user_preauth_key(&multi_id).await?;
    assert!(users.list_user_preauth_keys().await?.is_empty());
    Ok(())
}

/// The client-permission surface, for an existing `client_id` with no
/// permissions object: absent and cleared are two answers, a no-op mutation
/// writes nothing, and deleting the client takes the object with it.
/// `write_under_root` writes a key relative to the auth root.
pub async fn assert_client_permission_store_roundtrip<F, Fut>(
    store: &dyn ClientStore,
    client_id: &ClientId,
    write_under_root: F,
) -> anyhow::Result<()>
where
    F: Fn(String, Vec<u8>) -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<()>>,
{
    use pipette_core::client_permission::ClientPermissionSet;
    use pipette_plan_types::ClientPermission;

    // No object at all grants nothing; an operator grants such a record by hand.
    assert!(store.get_client_permissions(client_id).await?.is_none());
    assert!(
        !store
            .list_client_permissions()
            .await?
            .iter()
            .any(|(id, _)| id == client_id),
        "a principal with no object is absent from the listing, not present with an empty set"
    );

    let granted = ClientPermissionSet::from_iter([
        ClientPermission::PerformWork,
        ClientPermission::SubmitResults,
    ]);
    store.put_client_permissions(client_id, &granted).await?;
    // A padded key would otherwise parse, by trimming, as a second `client_id`.
    write_under_root(
        format!("client-permissions/ {client_id}.json"),
        serde_json::to_vec(&serde_json::json!({ "permissions": granted }))?,
    )
    .await?;
    assert_eq!(
        store.get_client_permissions(client_id).await?,
        Some(granted.clone())
    );
    // The listing carries the keyed read's set, under the id it is stored at.
    let listed: Vec<_> = store
        .list_client_permissions()
        .await?
        .into_iter()
        .filter(|(id, _)| id == client_id)
        .collect();
    assert_eq!(listed.len(), 1, "one object, one entry");
    let (listed_id, listed_set) = listed.first().context("expected one entry")?;
    assert_eq!(*listed_set, granted);
    assert_eq!(listed_id, client_id);

    // A cleared set is an object holding `[]`, not an absent object.
    let cleared = ClientPermissionSet::default();
    store.put_client_permissions(client_id, &cleared).await?;
    let read = store
        .get_client_permissions(client_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("a cleared set must read as an object, not as absent"))?;
    assert!(read.is_empty());

    // A mutation that grants something reports the change and the resulting set.
    let mutation = store
        .mutate_client_permissions(client_id, &|set| {
            set.insert_all([ClientPermission::PerformWork])
        })
        .await?;
    assert!(mutation.changed);
    assert_eq!(
        mutation.resulting,
        ClientPermissionSet::from_iter([ClientPermission::PerformWork])
    );

    // Replaying it changes nothing, and the store is untouched: this is what
    // lets an idempotent `add` skip the write.
    let repeat = store
        .mutate_client_permissions(client_id, &|set| {
            set.insert_all([ClientPermission::PerformWork])
        })
        .await?;
    assert!(!repeat.changed);
    assert_eq!(repeat.resulting, mutation.resulting);
    assert_eq!(
        store.get_client_permissions(client_id).await?,
        Some(repeat.resulting)
    );

    // A mutation on an absent object starts from the empty set, not a default.
    store.delete_client_permissions(client_id).await?;
    assert!(store.get_client_permissions(client_id).await?.is_none());
    let from_absent = store
        .mutate_client_permissions(client_id, &|set| {
            set.insert_all([ClientPermission::PerformWork])
        })
        .await?;
    assert!(from_absent.changed);
    assert_eq!(
        from_absent.resulting,
        ClientPermissionSet::from_iter([ClientPermission::PerformWork])
    );

    // A no-op mutation on an absent object writes nothing, so no empty object
    // turns a record granted nothing into a cleared set.
    store.delete_client_permissions(client_id).await?;
    let noop = store
        .mutate_client_permissions(client_id, &|_| false)
        .await?;
    assert!(!noop.changed);
    assert!(store.get_client_permissions(client_id).await?.is_none());

    // Deleting the identity takes the set with it, so a re-registration of the
    // same public key does not inherit it.
    store.delete_client(client_id).await?;
    assert!(store.get_client_permissions(client_id).await?.is_none());

    // Deleting an absent object is a no-op rather than an error.
    store.delete_client_permissions(client_id).await?;
    Ok(())
}
/// [`assert_client_permission_store_roundtrip`]'s properties over the user
/// set, for a `user_id` with no permissions object: neither kind may invent a
/// grant for a principal that has none.
pub async fn assert_user_permission_store_roundtrip(
    store: &dyn UserStore,
    user_id: &UserId,
) -> anyhow::Result<()> {
    use pipette_core::user_permission::UserPermissionSet;
    use pipette_plan_types::UserPermission;

    // An account granted nothing has no object at all.
    assert!(store.get_user_permissions(user_id).await?.is_none());

    let granted =
        UserPermissionSet::from_iter([UserPermission::SubmitPlans, UserPermission::ReadAnyClients]);
    store.put_user_permissions(user_id, &granted).await?;
    assert_eq!(store.get_user_permissions(user_id).await?, Some(granted));

    // A cleared set is an object holding `[]`, not an absent object. The two
    // resolve alike for this kind today, so the store is what keeps them apart.
    let cleared = UserPermissionSet::default();
    store.put_user_permissions(user_id, &cleared).await?;
    let read = store
        .get_user_permissions(user_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("a cleared set must read as an object, not as absent"))?;
    assert!(read.is_empty());

    // A mutation that grants something reports the change and the resulting set.
    let mutation = store
        .mutate_user_permissions(user_id, &|set| {
            set.insert_all([UserPermission::CancelPlans])
        })
        .await?;
    assert!(mutation.changed);
    assert_eq!(
        mutation.resulting,
        UserPermissionSet::from_iter([UserPermission::CancelPlans])
    );

    // Replaying it changes nothing, and the store is untouched: this is what
    // lets an idempotent `add` skip the write.
    let repeat = store
        .mutate_user_permissions(user_id, &|set| {
            set.insert_all([UserPermission::CancelPlans])
        })
        .await?;
    assert!(!repeat.changed);
    assert_eq!(repeat.resulting, mutation.resulting);
    assert_eq!(
        store.get_user_permissions(user_id).await?,
        Some(repeat.resulting)
    );

    // A mutation on an absent object starts from the empty set, not a default.
    store.delete_user_permissions(user_id).await?;
    assert!(store.get_user_permissions(user_id).await?.is_none());
    let from_absent = store
        .mutate_user_permissions(user_id, &|set| {
            set.insert_all([UserPermission::CancelPlans])
        })
        .await?;
    assert!(from_absent.changed);
    assert_eq!(
        from_absent.resulting,
        UserPermissionSet::from_iter([UserPermission::CancelPlans])
    );

    // A no-op mutation on an absent object writes nothing, so no empty object
    // turns an account granted nothing into a cleared set.
    store.delete_user_permissions(user_id).await?;
    let noop = store.mutate_user_permissions(user_id, &|_| false).await?;
    assert!(!noop.changed);
    assert!(store.get_user_permissions(user_id).await?.is_none());

    // Deleting an absent object is a no-op rather than an error.
    store.delete_user_permissions(user_id).await?;
    Ok(())
}

pub async fn assert_user_key_store_roundtrip(
    store: &dyn UserStore,
    user_id: &UserId,
    other_id: &UserId,
) -> anyhow::Result<()> {
    use pipette_plan_types::NonEmptyTrimmedString;
    use pipette_plan_types::PublicKeyHex;

    let key = |byte: u8, details: &str| -> anyhow::Result<UserKey> {
        Ok(UserKey::new(
            PublicKeyHex::try_new(hex::encode([byte; PublicKeyHex::BYTE_LEN]))?,
            NonEmptyTrimmedString::try_new(details)?,
            chrono::Utc::now(),
        ))
    };

    // An account with no keys lists nothing rather than failing.
    assert!(store.list_user_keys(user_id).await?.is_empty());

    let laptop = key(0x11, "work laptop")?;
    store.put_user_key(user_id, &laptop).await?;
    assert_eq!(
        store.get_user_key(user_id, &laptop.key_id).await?,
        Some(laptop.clone())
    );

    // The binding check: the same key id under another account resolves to
    // nothing, so a forged principal fails structurally.
    assert!(
        store
            .get_user_key(other_id, &laptop.key_id)
            .await?
            .is_none()
    );

    // A second machine joins the account without disturbing the first.
    let desktop = key(0x22, "desktop")?;
    store.put_user_key(user_id, &desktop).await?;
    let listed = store.list_user_keys(user_id).await?;
    assert_eq!(listed.len(), 2);
    assert!(listed.contains(&laptop) && listed.contains(&desktop));

    // Re-registering a key already held rewrites it rather than adding a row.
    let renamed = UserKey::new(
        laptop.public_key.clone(),
        NonEmptyTrimmedString::try_new("the laptop, renamed")?,
        chrono::Utc::now(),
    );
    assert_eq!(renamed.key_id, laptop.key_id, "the id is the key's digest");
    store.put_user_key(user_id, &renamed).await?;
    assert_eq!(store.list_user_keys(user_id).await?.len(), 2);
    assert_eq!(
        store
            .get_user_key(user_id, &laptop.key_id)
            .await?
            .map(|k| k.key_details),
        Some(renamed.key_details)
    );

    // Revocation takes effect at once, and reaches only its own key.
    store.delete_user_key(user_id, &laptop.key_id).await?;
    assert!(store.get_user_key(user_id, &laptop.key_id).await?.is_none());
    assert_eq!(store.list_user_keys(user_id).await?, vec![desktop]);

    // Idempotent, so a revocation can be re-run to converge.
    store.delete_user_key(user_id, &laptop.key_id).await?;
    Ok(())
}

/// `public-key-index/`: a claim is won by exactly one caller, so a second
/// claim does not overwrite.
pub async fn assert_key_index_store_roundtrip(
    store: &dyn KeyIndexStore,
    client_id: &ClientId,
    user_id: &UserId,
) -> anyhow::Result<()> {
    use pipette_core::key_index::digest_of;
    use pipette_plan_types::PublicKeyHex;

    let digest = digest_of(&PublicKeyHex::try_new(hex::encode(
        [0x33; PublicKeyHex::BYTE_LEN],
    ))?);
    let device = KeyHolderPrincipalId::Client(client_id.clone());
    let account = KeyHolderPrincipalId::User(user_id.clone());

    // A free keypair is claimable, and the claim reports that it landed.
    assert_eq!(store.holder_of_public_key(&digest).await?, None);
    assert_eq!(
        store.claim_public_key(&digest, &device).await?,
        KeyClaim::Claimed
    );
    assert_eq!(
        store.holder_of_public_key(&digest).await?,
        Some(device.clone())
    );

    // A second principal's claim does not land, and names the holder: this
    // stops one keypair addressing a client record and a user at once.
    assert_eq!(
        store.claim_public_key(&digest, &account).await?,
        KeyClaim::Taken(device.clone())
    );

    // Re-claiming your own keypair reads as taken by yourself — the retry.
    let repeat = store.claim_public_key(&digest, &device).await?;
    assert_eq!(repeat, KeyClaim::Taken(device.clone()));
    assert!(repeat.is_held_by(&device));

    // Released, the keypair is free again: unread, and claimable by the
    // principal refused before. The release is idempotent.
    store.release_public_key(&digest).await?;
    store.release_public_key(&digest).await?;
    assert_eq!(store.holder_of_public_key(&digest).await?, None);
    assert_eq!(
        store.claim_public_key(&digest, &account).await?,
        KeyClaim::Claimed,
        "a released keypair is claimable by the principal refused before"
    );
    Ok(())
}

/// The two permission keyspaces hold one object per principal and never reach
/// into each other: a layout that dropped the keyspace parameter would read one
/// kind's grants as the other's. Assumes neither principal has an object and
/// both stores share a backend.
pub async fn assert_permission_objects_are_per_principal(
    clients: &dyn ClientStore,
    users: &dyn UserStore,
    client_id: &ClientId,
    user_id: &UserId,
) -> anyhow::Result<()> {
    use pipette_core::client_permission::ClientPermissionSet;
    use pipette_core::user_permission::UserPermissionSet;
    use pipette_plan_types::ClientPermission;
    use pipette_plan_types::UserPermission;

    let device = ClientPermissionSet::from_iter([ClientPermission::PerformWork]);
    let operator = UserPermissionSet::from_iter([UserPermission::SubmitPlans]);
    clients.put_client_permissions(client_id, &device).await?;
    users.put_user_permissions(user_id, &operator).await?;

    assert_eq!(
        clients.get_client_permissions(client_id).await?,
        Some(device.clone()),
        "a user's write must not land on the client's object"
    );
    // The fleet-wide listing reads one keyspace too: listing the shared parent
    // would hand the matcher a user's set under a client id.
    assert_eq!(
        clients.list_client_permissions().await?,
        vec![(client_id.clone(), device.clone())],
        "the fleet-wide listing must return this keyspace and nothing else"
    );
    assert_eq!(
        users.get_user_permissions(user_id).await?,
        Some(operator),
        "a client's write must not land on the user's object"
    );

    users.delete_user_permissions(user_id).await?;
    assert!(users.get_user_permissions(user_id).await?.is_none());
    assert_eq!(
        clients.get_client_permissions(client_id).await?,
        Some(device),
        "deleting one principal's object must leave the other's standing"
    );

    // The client listing reads one keyspace, so a user's object must not turn
    // up in it even though the two are the same shape one prefix apart.
    let listed = clients.list_client_permissions().await?;
    assert_eq!(
        listed.len(),
        1,
        "the client listing must return clients only, never a user's object"
    );
    assert_eq!(listed.first().context("expected one client")?.0, *client_id);
    Ok(())
}

/// The `UserStore` account surface, on a store with no accounts or address
/// claims: the address claim is exclusive and names its holder when refused.
pub async fn assert_user_account_store_roundtrip(store: &dyn UserStore) -> anyhow::Result<()> {
    use pipette_core::user::User;
    use pipette_plan_types::ContactEmail;
    use pipette_plan_types::UserAccountType;
    use pipette_plan_types::UserStatus;

    let email = ContactEmail::try_new("operator@example.com")?;
    let user = User::try_new(email.clone(), Utc::now())?;
    let user_id = user.user_id.clone();

    // An account that does not exist is `None`, not an error: the record is the
    // existence authority, and asking about a stranger is an ordinary question.
    assert!(store.get_user(&user_id).await?.is_none());
    assert!(store.list_users().await?.is_empty());
    assert!(store.lookup_user_by_email(email.as_str()).await?.is_none());

    // Claim, then write — the order every caller owes, so a record never exists
    // for an address the account does not hold.
    assert_eq!(
        store.claim_user_email(email.as_str(), &user_id).await?,
        EmailClaim::Claimed
    );
    assert_eq!(
        store.lookup_user_by_email(email.as_str()).await?,
        Some(user_id.clone())
    );
    store.put_user(&user).await?;

    assert_eq!(
        store.get_user(&user_id).await?,
        Some(user.clone()),
        "the record must survive the store whole"
    );
    assert_eq!(store.list_users().await?, vec![user.clone()]);

    // A second account cannot take the address, and is told which one has it.
    let other = User::try_new(email.clone(), Utc::now())?;
    assert_eq!(
        store
            .claim_user_email(email.as_str(), &other.user_id)
            .await?,
        EmailClaim::Taken(user_id.clone()),
        "an exclusive create must refuse the loser and name the winner"
    );
    assert_eq!(
        store.get_user(&other.user_id).await?,
        None,
        "losing the claim must leave no record behind"
    );

    // A disable is an ordinary whole-record write; the account keeps its claim.
    let disabled = User {
        status: UserStatus::Disabled,
        ..user.clone()
    };
    store.put_user(&disabled).await?;
    assert_eq!(store.get_user(&user_id).await?, Some(disabled));
    assert_eq!(
        store.lookup_user_by_email(email.as_str()).await?,
        Some(user_id.clone()),
        "a disabled account still holds its address"
    );

    // So is a retype, in both directions.
    let service = User {
        account_type: UserAccountType::Service,
        ..user.clone()
    };
    store.put_user(&service).await?;
    assert_eq!(store.get_user(&user_id).await?, Some(service.clone()));
    assert_eq!(store.list_users().await?, vec![service]);
    store.put_user(&user).await?;
    assert_eq!(store.get_user(&user_id).await?, Some(user.clone()));

    // Record first, then the claim: the order that makes every remaining key
    // inert in one write.
    store.delete_user(&user_id).await?;
    assert!(store.get_user(&user_id).await?.is_none());
    assert_eq!(
        store.lookup_user_by_email(email.as_str()).await?,
        Some(user_id),
        "the claim outlives the record until it is released"
    );
    store.release_user_email(email.as_str()).await?;
    assert!(store.lookup_user_by_email(email.as_str()).await?.is_none());
    assert!(store.list_users().await?.is_empty());

    // Both deletes are idempotent, which is what lets a partially failed
    // teardown be finished by running it again.
    store.delete_user(&other.user_id).await?;
    store.release_user_email(email.as_str()).await?;

    // The address is free again, so it can be re-claimed by a new account.
    assert_eq!(
        store
            .claim_user_email(email.as_str(), &other.user_id)
            .await?,
        EmailClaim::Claimed
    );
    Ok(())
}

/// The first sign-in mark every backend is held to: set, read back, cleared,
/// with the set and the clear idempotent and one account's mark no other's.
pub async fn assert_user_awaiting_sign_in_roundtrip(store: &dyn UserStore) -> anyhow::Result<()> {
    let (user_id, other) = (UserId::mint(), UserId::mint());
    assert!(
        !store.user_awaits_sign_in(&user_id).await?,
        "unmarked by default"
    );

    store.mark_user_awaiting_sign_in(&user_id).await?;
    store.mark_user_awaiting_sign_in(&user_id).await?;
    assert!(store.user_awaits_sign_in(&user_id).await?);
    assert!(
        !store.user_awaits_sign_in(&other).await?,
        "a mark names one account"
    );

    store.clear_user_awaiting_sign_in(&user_id).await?;
    store.clear_user_awaiting_sign_in(&user_id).await?;
    assert!(!store.user_awaits_sign_in(&user_id).await?);
    Ok(())
}

/// The first Clerk sign-in record every backend is held to: the first record
/// wins and a later one changes nothing, one account's is no other's, and the
/// delete is idempotent.
pub async fn assert_user_first_clerk_sign_in_roundtrip(
    store: &dyn UserStore,
) -> anyhow::Result<()> {
    let (user_id, other) = (UserId::mint(), UserId::mint());
    assert_eq!(store.get_user_first_clerk_sign_in(&user_id).await?, None);

    let at = DateTime::from_timestamp(1_800_000_000, 0).context("a valid instant")?;
    let first = FirstClerkSignIn {
        at,
        prior_keys: vec![UserKeyId::try_new("userkey_0195c3e2a4f37b8c")?],
    };
    let later = FirstClerkSignIn {
        at: at + chrono::TimeDelta::days(1),
        prior_keys: Vec::new(),
    };
    assert!(
        store
            .record_user_first_clerk_sign_in(&user_id, &first)
            .await?
    );
    assert!(
        !store
            .record_user_first_clerk_sign_in(&user_id, &later)
            .await?,
        "only the first sign-in records"
    );
    assert_eq!(
        store.get_user_first_clerk_sign_in(&user_id).await?,
        Some(first)
    );
    assert_eq!(
        store.get_user_first_clerk_sign_in(&other).await?,
        None,
        "a record names one account"
    );

    store.delete_user_first_clerk_sign_in(&user_id).await?;
    store.delete_user_first_clerk_sign_in(&user_id).await?;
    assert_eq!(store.get_user_first_clerk_sign_in(&user_id).await?, None);
    Ok(())
}

/// An account never appears in `list_clients`, the fleet the matcher narrows,
/// where it would be a job candidate. Assumes both stores share one backend.
pub async fn assert_users_are_not_clients(
    clients: &dyn ClientStore,
    users: &dyn UserStore,
    client: &Client,
) -> anyhow::Result<()> {
    use pipette_core::user::User;
    use pipette_plan_types::ContactEmail;

    let user = User::try_new(ContactEmail::try_new("op@example.com")?, Utc::now())?;
    users
        .claim_user_email(user.contact_email.as_str(), &user.user_id)
        .await?;
    users.put_user(&user).await?;
    clients.put_client(client).await?;

    let listed_clients = drain_clients(clients).await?.items;
    assert_eq!(
        listed_clients.len(),
        1,
        "an account must not join the fleet"
    );
    assert_eq!(
        listed_clients
            .first()
            .context("expected one client")?
            .client_id,
        client.client_id
    );
    assert_eq!(
        users.list_users().await?.len(),
        1,
        "a client must not appear in the account roster"
    );

    // Deleting one principal leaves the other standing.
    users.delete_user(&user.user_id).await?;
    assert_eq!(drain_clients(clients).await?.items.len(), 1);
    assert!(users.get_user(&user.user_id).await?.is_none());
    Ok(())
}

/// A stale `replace_client` neither revives a deleted record nor undoes an operator's edit.
pub async fn assert_client_replace_is_conditional(
    store: &dyn ClientStore,
    client: &Client,
) -> anyhow::Result<()> {
    use futures::StreamExt;

    use pipette_plan_types::NonEmptyTrimmedString;

    enum Between {
        Nothing,
        Deleted,
        Edited,
    }
    let with_details = |details: &str| -> anyhow::Result<Client> {
        Ok(Client {
            client_details: NonEmptyTrimmedString::try_new(details)?,
            ..client.clone()
        })
    };
    let updated = with_details("the device's edit")?;
    let edited = with_details("an operator's edit")?;
    let cases = [
        (Between::Nothing, true, Some(&updated)),
        (Between::Deleted, false, None),
        (Between::Edited, false, Some(&edited)),
    ];
    futures::stream::iter(cases)
        .map(anyhow::Ok)
        .try_for_each(|(between, replaced, stored)| {
            let updated = &updated;
            let edited = &edited;
            async move {
                store.put_client(client).await?;
                let read = store
                    .get_client(&client.client_id)
                    .await?
                    .context("the seeded record reads back")?;
                match between {
                    Between::Nothing => {}
                    Between::Deleted => store.delete_client(&client.client_id).await?,
                    Between::Edited => store.put_client(edited).await?,
                }
                anyhow::ensure!(store.replace_client(&read, updated).await? == replaced);
                anyhow::ensure!(store.get_client(&client.client_id).await?.as_ref() == stored);
                Ok(())
            }
        })
        .await
}

/// The `ClientStore` pre-auth surface, on a store with no keys or spent
/// markers: a rejection never spends the key, a single-use key grants exactly
/// once, and a multi-use consume mutates nothing.
pub async fn assert_client_preauth_store_roundtrip(store: &dyn ClientStore) -> anyhow::Result<()> {
    use pipette_core::client_permission::StoredClientPermission;
    use pipette_core::client_preauth::{ClientPreauthParams, ClientPreauthRejectionError, mint};
    use pipette_plan_types::ClientPermission;
    use pipette_plan_types::ClientPreauthUsage;
    use pipette_plan_types::NonEmptyTrimmedString;
    use pipette_plan_types::PermissionName;

    let now = Utc::now();
    let params = |usage, expires_at| ClientPreauthParams {
        usage,
        expires_at,
        default_tags: BTreeSet::new(),
        default_organization: None,
        default_owner: None,
        default_permissions: None,
        note: None,
    };

    // An unknown id is rejected before the spend, so it leaves nothing behind.
    let absent = ClientPreauthKeyId::try_new("deadbeefdeadbeef")?;
    assert_eq!(
        store
            .consume_client_preauth_key(&absent, &ClientPreauthSecret::new("nothing"))
            .await?,
        ClientPreauthConsumeOutcome::Rejected(ClientPreauthRejectionError::NotFound)
    );
    assert_eq!(store.get_client_preauth_key(&absent).await?, None);
    assert!(store.list_spent_client_preauth_markers().await?.is_empty());
    assert!(store.list_client_preauth_keys().await?.is_empty());

    // Every seedable field, plus a template name this binary does not define:
    // a backend that cannot read it back turns a valid registration into a 500.
    let mut minted = mint(
        ClientPreauthParams {
            default_tags: BTreeSet::from([Tag::try_new("lab-bench")?]),
            default_organization: Some(NonEmptyTrimmedString::try_new("seeded-org")?),
            default_owner: Some(pipette_plan_types::ClientOwner::Org {
                org_id: OrgId::mint(),
            }),
            default_permissions: Some(BTreeSet::from([ClientPermission::PerformWork])),
            note: Some(NonEmptyTrimmedString::try_new("shared contract")?),
            usage: ClientPreauthUsage::SingleUse,
            expires_at: Some(now + Duration::from_secs(3600)),
        },
        now,
    )?;
    minted
        .key
        .default_permissions
        .as_mut()
        .ok_or_else(|| anyhow::anyhow!("the template was minted with a set"))?
        .insert(StoredClientPermission::classify(PermissionName::try_new(
            "operate_centrifuge",
        )?));
    store.put_client_preauth_key(&minted.key).await?;

    let listed = store.list_client_preauth_keys().await?;
    let [read] = listed.as_slice() else {
        anyhow::bail!(
            "one minted key must list as one record, got {}",
            listed.len()
        );
    };
    // The whole record, so a later field and `created_at` (the `preauth list`
    // sort key) cannot go unasserted.
    assert_eq!(
        read, &minted.key,
        "a record must read back as minted, its unknown template name included"
    );

    let (key_id, secret) = minted
        .token
        .parse_parts()
        .ok_or_else(|| anyhow::anyhow!("the minted token must parse"))?;

    // A lookup reads the record back and spends nothing.
    assert_eq!(
        store.get_client_preauth_key(&key_id).await?.as_ref(),
        Some(&minted.key)
    );
    assert!(store.list_spent_client_preauth_markers().await?.is_empty());

    // A wrong secret does not spend the key: the marker is created only once
    // the secret verifies, so a non-holder cannot burn it.
    assert_eq!(
        store
            .consume_client_preauth_key(&key_id, &ClientPreauthSecret::new("wrong-secret"))
            .await?,
        ClientPreauthConsumeOutcome::Rejected(ClientPreauthRejectionError::BadSecret)
    );
    assert!(store.list_spent_client_preauth_markers().await?.is_empty());
    assert_eq!(store.list_client_preauth_keys().await?.len(), 1);

    // The grant is the record's, verbatim — this is what registration seeds.
    assert_eq!(
        store.consume_client_preauth_key(&key_id, &secret).await?,
        ClientPreauthConsumeOutcome::Granted(minted.key.grant())
    );

    // A spent key leaves the operator's listing, and the marker is what keeps
    // it spent.
    assert_eq!(
        store.list_spent_client_preauth_markers().await?,
        std::slice::from_ref(&key_id)
    );
    assert!(
        store.list_client_preauth_keys().await?.is_empty(),
        "a spent marker is not a key record"
    );
    assert_eq!(store.get_client_preauth_key(&key_id).await?, None);

    // Re-use reads as unknown: spent, revoked, and never-minted are one answer,
    // so the route is no enumeration oracle.
    assert_eq!(
        store.consume_client_preauth_key(&key_id, &secret).await?,
        ClientPreauthConsumeOutcome::Rejected(ClientPreauthRejectionError::NotFound)
    );

    // Consuming a multi-use key touches no storage — no marker, record intact —
    // which is why that path has no race to resolve.
    let multi = mint(params(ClientPreauthUsage::MultiUse, None), now)?;
    store.put_client_preauth_key(&multi.key).await?;
    let (multi_id, multi_secret) = multi
        .token
        .parse_parts()
        .ok_or_else(|| anyhow::anyhow!("the minted token must parse"))?;
    let granted = ClientPreauthConsumeOutcome::Granted(multi.key.grant());
    assert_eq!(
        store
            .consume_client_preauth_key(&multi_id, &multi_secret)
            .await?,
        granted
    );
    assert_eq!(
        store
            .consume_client_preauth_key(&multi_id, &multi_secret)
            .await?,
        granted,
        "a multi-use key must grant again"
    );
    assert_eq!(store.list_client_preauth_keys().await?.len(), 1);
    assert_eq!(
        store.list_spent_client_preauth_markers().await?,
        std::slice::from_ref(&key_id),
        "a multi-use consume must leave no marker"
    );

    // An expired key is rejected and not spent, so `prune` strands no marker
    // when it deletes the record.
    let expired = mint(
        params(
            ClientPreauthUsage::SingleUse,
            Some(now - Duration::from_secs(1)),
        ),
        now,
    )?;
    store.put_client_preauth_key(&expired.key).await?;
    let (expired_id, expired_secret) = expired
        .token
        .parse_parts()
        .ok_or_else(|| anyhow::anyhow!("the minted token must parse"))?;
    assert_eq!(
        store
            .consume_client_preauth_key(&expired_id, &expired_secret)
            .await?,
        ClientPreauthConsumeOutcome::Rejected(ClientPreauthRejectionError::Expired)
    );
    assert_eq!(
        store.list_spent_client_preauth_markers().await?,
        std::slice::from_ref(&key_id),
        "a rejected consume must leave no marker"
    );

    // Revoke is a delete and is idempotent, so `revoke` and `prune` may both
    // name one key. Only the key named goes.
    store.delete_client_preauth_key(&multi_id).await?;
    store.delete_client_preauth_key(&multi_id).await?;
    assert_eq!(
        store
            .list_client_preauth_keys()
            .await?
            .into_iter()
            .map(|key| key.key_id)
            .collect::<Vec<_>>(),
        [expired_id]
    );

    // So is dropping the marker a spend left, which `prune` does once the key
    // it spent has no record left to revive.
    store.delete_spent_client_preauth_marker(&key_id).await?;
    store.delete_spent_client_preauth_marker(&key_id).await?;
    assert!(store.list_spent_client_preauth_markers().await?.is_empty());
    Ok(())
}

/// [`TagStore`], for two distinct untagged clients with `a < b` (the reverse
/// lookup is sorted). `write_under_root` writes a key under the store root.
pub async fn assert_tag_store_roundtrip<F, Fut>(
    store: &dyn TagStore,
    a: &ClientId,
    b: &ClientId,
    write_under_root: F,
) -> anyhow::Result<()>
where
    F: Fn(String, Vec<u8>) -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<()>>,
{
    let team = Tag::try_new("team-mobile")?;
    let east = Tag::try_new("us-east")?;

    store.add_client_tag(a, &team).await?;
    store.add_client_tag(a, &east).await?;
    store.add_client_tag(b, &team).await?;
    store.add_client_tag(a, &team).await?; // idempotent re-add
    // Padded strays would trim to `a`; the forward one names a tag `a` lacks,
    // which a reindex would grant.
    write_under_root(format!("tags-index/by-tag/{team}/ {a}"), Vec::new()).await?;
    write_under_root(format!("tags-index/by-client/ {a}/us-west"), Vec::new()).await?;
    write_under_root(format!("tags-index/by-tag/us-west/ {a}"), Vec::new()).await?;
    let mut forward = store.list_forward_tag_markers().await?;
    forward.sort();
    assert_eq!(
        forward,
        vec![
            (a.clone(), team.clone()),
            (a.clone(), east.clone()),
            (b.clone(), team.clone())
        ]
    );
    let mut reverse = store.list_reverse_tag_markers().await?;
    reverse.sort();
    assert_eq!(reverse, forward);

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

    // Deleting a client's tags clears it from both trees, and again is a no-op.
    store.delete_client_tags(b).await?;
    store.delete_client_tags(b).await?;
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

/// The first write wins and a later one changes nothing: the synthetic-failure
/// path reads a refused write as proof a real submission owns the job. The
/// body is pinned too, since `AlreadyExists` after an overwrite would lose it.
pub async fn assert_exclusive_incoming_write(store: &dyn SubmissionStore) -> anyhow::Result<()> {
    let job_id = JobId::new_unchecked("job-exclusive");
    let first = serde_json::json!({ "job_id": "job-exclusive", "who": "first" });
    let second = serde_json::json!({ "job_id": "job-exclusive", "who": "second" });

    // A free key is created, and the body read back is the one written.
    assert_eq!(
        store.write_incoming_new(&job_id, &first).await?,
        CreateOutcome::Created
    );
    let record = store
        .get_submission(&job_id)
        .await?
        .context("expected the created body")?;
    assert_eq!(record.state, JobState::Incoming);
    assert_eq!(
        record.body.get("who").and_then(serde_json::Value::as_str),
        Some("first")
    );

    // A taken key is refused, twice, and the incumbent's body survives both.
    for _ in 0..2 {
        assert_eq!(
            store.write_incoming_new(&job_id, &second).await?,
            CreateOutcome::AlreadyExists
        );
        let record = store
            .get_submission(&job_id)
            .await?
            .context("expected the incumbent body")?;
        assert_eq!(
            record.body.get("who").and_then(serde_json::Value::as_str),
            Some("first")
        );
    }

    // The exclusion is per key, not global: a second job still creates. This is
    // what catches an impl that refuses whenever *any* body is in `incoming/`.
    let other = JobId::new_unchecked("job-exclusive-other");
    assert_eq!(
        store.write_incoming_new(&other, &first).await?,
        CreateOutcome::Created
    );

    // `write_incoming` keeps overwriting — the exclusion belongs to the new
    // method alone, and the client submission path depends on that.
    store.write_incoming(&job_id, &second).await?;
    let record = store
        .get_submission(&job_id)
        .await?
        .context("expected the overwritten body")?;
    assert_eq!(
        record.body.get("who").and_then(serde_json::Value::as_str),
        Some("second")
    );

    // Once the scorer archives the incumbent the key is free again, and a
    // queue-maintenance re-run must not mistake it for a live submitter.
    store.mark_processed(&job_id).await?;
    assert_eq!(
        store.write_incoming_new(&job_id, &first).await?,
        CreateOutcome::Created
    );

    Ok(())
}

/// A record in any of `find_job`'s four places is a record; a body only in
/// `unverified/` is not. The ids share a prefix, to catch prefix matching.
pub async fn assert_has_record_contract(store: &dyn SubmissionStore) -> anyhow::Result<()> {
    /// Where a case puts its job's body before the check.
    enum Place {
        Nowhere,
        Incoming,
        ToDo,
        Parked,
        ToFinalize,
        Processed,
        Unverified,
    }
    let body = |job_id: &JobId| serde_json::json!({ "job_id": job_id.as_str() });
    let cases = [
        ("job-has-record-none", Place::Nowhere, false),
        ("job-has-record-incoming", Place::Incoming, true),
        ("job-has-record-to-do", Place::ToDo, true),
        ("job-has-record-parked", Place::Parked, true),
        ("job-has-record-to-finalize", Place::ToFinalize, true),
        ("job-has-record-processed", Place::Processed, true),
        ("job-has-record-unverified", Place::Unverified, false),
    ];
    for (job_id, place, _) in &cases {
        let job_id = JobId::new_unchecked(*job_id);
        match place {
            Place::Nowhere => {}
            Place::Incoming => store.write_incoming(&job_id, &body(&job_id)).await?,
            Place::ToDo => {
                store
                    .enqueue(ScoreQueueStage::ToDo, &job_id, &body(&job_id))
                    .await?;
            }
            Place::Parked => {
                store
                    .enqueue(ScoreQueueStage::Parked, &job_id, &body(&job_id))
                    .await?;
            }
            Place::ToFinalize => {
                let payload = serde_json::json!({ "submission": body(&job_id), "score": {} });
                store
                    .enqueue(ScoreQueueStage::ToFinalize, &job_id, &payload)
                    .await?;
            }
            Place::Processed => store.write_processed(&job_id, &body(&job_id)).await?,
            Place::Unverified => {
                store
                    .write_unverified(&ClientId::try_new("ev1_held")?, &job_id, &body(&job_id))
                    .await?;
            }
        }
    }
    for (job_id, _, expected) in &cases {
        let found = store.has_record(&JobId::new_unchecked(*job_id)).await?;
        assert_eq!(found, *expected, "has_record({job_id})");
    }
    // `job-has-record` is a prefix of every key above, and has no record.
    assert!(
        !store
            .has_record(&JobId::new_unchecked("job-has-record"))
            .await?
    );
    Ok(())
}

/// One call reads every job's outcome from the top-level `message_type` (a
/// non-string reads as `Finished`; `processed/` wins over `incoming/`). Each
/// body nests a `failure` type and the ids share a prefix, to catch a backend
/// that reads past the top level or matches keys by prefix.
pub async fn assert_find_outcomes_contract(store: &dyn SubmissionStore) -> anyhow::Result<()> {
    /// Where a case puts its job's body before the read.
    enum Place {
        Nowhere,
        Incoming,
        ToDo,
        Parked,
        ToFinalize,
        Processed,
        Unverified,
    }
    let body = |job_id: &JobId, message_type: Option<serde_json::Value>| {
        let mut body = serde_json::json!({
            "job_id": job_id.as_str(),
            "transcript": [{ "message_type": "failure", "content": "a turn" }],
        });
        if let (Some(message_type), Some(fields)) = (message_type, body.as_object_mut()) {
            fields.insert("message_type".to_owned(), message_type);
        }
        body
    };
    let cases = [
        ("job-outcome-none", Place::Nowhere, None, None),
        (
            "job-outcome-incoming",
            Place::Incoming,
            Some("failure".into()),
            Some(SettledOutcome::Failed),
        ),
        (
            "job-outcome-to-do",
            Place::ToDo,
            Some("success".into()),
            Some(SettledOutcome::Finished),
        ),
        (
            "job-outcome-parked",
            Place::Parked,
            Some("success".into()),
            Some(SettledOutcome::Finished),
        ),
        (
            "job-outcome-to-finalize",
            Place::ToFinalize,
            Some("failure".into()),
            Some(SettledOutcome::Failed),
        ),
        (
            "job-outcome-processed",
            Place::Processed,
            Some("failure".into()),
            Some(SettledOutcome::Failed),
        ),
        (
            "job-outcome-processed-untagged",
            Place::Processed,
            None,
            Some(SettledOutcome::Finished),
        ),
        (
            "job-outcome-processed-numeric",
            Place::Processed,
            Some(serde_json::json!(1)),
            Some(SettledOutcome::Finished),
        ),
        (
            "job-outcome-unverified",
            Place::Unverified,
            Some("failure".into()),
            None,
        ),
    ];
    for (job_id, place, message_type, _) in &cases {
        let job_id = JobId::new_unchecked(*job_id);
        let body = body(&job_id, message_type.clone());
        match place {
            Place::Nowhere => {}
            Place::Incoming => store.write_incoming(&job_id, &body).await?,
            Place::ToDo => store.enqueue(ScoreQueueStage::ToDo, &job_id, &body).await?,
            Place::Parked => {
                store
                    .enqueue(ScoreQueueStage::Parked, &job_id, &body)
                    .await?;
            }
            Place::ToFinalize => {
                let payload = serde_json::json!({ "submission": body, "score": {} });
                store
                    .enqueue(ScoreQueueStage::ToFinalize, &job_id, &payload)
                    .await?;
            }
            Place::Processed => store.write_processed(&job_id, &body).await?,
            Place::Unverified => {
                store
                    .write_unverified(&ClientId::try_new("ev1_held")?, &job_id, &body)
                    .await?;
            }
        }
    }
    // `job-outcome` is a prefix of every key above, and has no record.
    let job_ids: Vec<JobId> = cases
        .iter()
        .map(|(job_id, ..)| JobId::new_unchecked(*job_id))
        .chain([JobId::new_unchecked("job-outcome")])
        .collect();
    let expected: HashMap<JobId, SettledOutcome> = cases
        .iter()
        .filter_map(|(job_id, _, _, outcome)| outcome.map(|o| (JobId::new_unchecked(*job_id), o)))
        .collect();
    assert_eq!(store.find_outcomes(&job_ids).await?, expected);
    assert!(store.find_outcomes(&[]).await?.is_empty());

    let twice = JobId::new_unchecked("job-outcome-twice");
    store
        .write_incoming(&twice, &body(&twice, Some("failure".into())))
        .await?;
    store
        .write_processed(&twice, &body(&twice, Some("success".into())))
        .await?;
    assert_eq!(
        store.find_outcomes(std::slice::from_ref(&twice)).await?,
        HashMap::from([(twice, SettledOutcome::Finished)]),
        "processed/ comes first"
    );

    let broken = JobId::new_unchecked("job-outcome-broken");
    store
        .enqueue(
            ScoreQueueStage::ToFinalize,
            &broken,
            &serde_json::json!({ "score": {} }),
        )
        .await?;
    let found = store.find_outcomes(&[broken]).await;
    assert!(found.is_err(), "{found:?}");
    Ok(())
}

/// One object per client per job, a rewrite replaces its own, and the ids share
/// a prefix to catch a backend listing by string prefix, not by directory.
pub async fn assert_runtime_output_roundtrip(store: &dyn SubmissionStore) -> anyhow::Result<()> {
    let job = JobId::new_unchecked("job-1");
    let other_job = JobId::new_unchecked("job-10");
    let a = ClientId::try_new("ev1_a")?;
    let b = ClientId::try_new("ev1_b")?;
    let output = |text: &str| RuntimeOutput {
        runtime_stderr: Some(text.to_owned()),
        ..RuntimeOutput::default()
    };
    let listed = |mut pairs: Vec<(ClientId, RuntimeOutput)>| {
        pairs.sort_by(|x, y| x.0.cmp(&y.0));
        pairs
    };

    assert!(store.list_runtime_output(&job).await?.is_empty());

    store
        .write_runtime_output(&job, &a, &output("a first"))
        .await?;
    store.write_runtime_output(&job, &b, &output("b")).await?;
    store
        .write_runtime_output(&other_job, &a, &output("other job"))
        .await?;
    assert_eq!(
        listed(store.list_runtime_output(&job).await?),
        vec![(a.clone(), output("a first")), (b.clone(), output("b"))]
    );

    store
        .write_runtime_output(&job, &a, &output("a again"))
        .await?;
    assert_eq!(
        listed(store.list_runtime_output(&job).await?),
        vec![(a.clone(), output("a again")), (b, output("b"))]
    );
    assert_eq!(
        store.list_runtime_output(&other_job).await?,
        vec![(a, output("other job"))]
    );
    Ok(())
}

/// `incoming/` and a queue stage page past their cursor, each id once in
/// ascending order, so a drain reaches the ids behind ones it cannot clear.
pub async fn assert_submission_listings_page(store: &dyn SubmissionStore) -> anyhow::Result<()> {
    let jobs: Vec<JobId> = (0..5)
        .map(|i| JobId::new_unchecked(format!("job-{i}")))
        .collect();
    let body = serde_json::json!({});
    futures::stream::iter(jobs.iter().map(anyhow::Ok))
        .try_for_each(|job| {
            let body = &body;
            async move {
                store.write_incoming(job, body).await?;
                store.enqueue(ScoreQueueStage::ToDo, job, body).await
            }
        })
        .await?;
    let page = NonZeroUsize::new(2).context("2 is non-zero")?;
    let drain = |stage: Option<ScoreQueueStage>| async move {
        let mut seen: Vec<JobId> = Vec::new();
        loop {
            let listed = match stage {
                Some(stage) => store.list_queue_after(stage, seen.last(), page).await?,
                None => store.list_incoming_after(seen.last(), page).await?,
            };
            let exhausted = listed.len() < page.get();
            seen.extend(listed);
            if exhausted {
                return anyhow::Ok(seen);
            }
        }
    };
    assert_eq!(drain(None).await?, jobs);
    assert_eq!(drain(Some(ScoreQueueStage::ToDo)).await?, jobs);
    Ok(())
}

/// A content key is claimed by exactly one caller, and every later write is
/// conditioned on the version read: a stale version changes nothing.
pub async fn assert_content_key_roundtrip(store: &dyn PlanStore) -> anyhow::Result<()> {
    let digest = "a".repeat(64);
    let other_digest = "b".repeat(64);
    let record = |n: u32| ContentKeyRecord {
        job_id: JobId::from_uuid(uuid::Uuid::from_u128(n.into())),
        plan_id: PlanId::from_uuid(uuid::Uuid::from_u128(n.into())),
        created_at: DateTime::from_timestamp(1_780_000_000 + i64::from(n), 0)
            .unwrap_or_else(Utc::now),
    };

    // An unclaimed digest reads as absent, and lists as nothing.
    assert!(store.get_content_key(&digest).await?.is_none());
    assert!(store.list_content_keys().await?.is_empty());

    // The first claim wins; a second claim reads the winner's record back.
    assert!(matches!(
        store.create_content_key(&digest, &record(1)).await?,
        ContentKeyClaim::Created
    ));
    let ContentKeyClaim::AlreadyExists(incumbent, version) =
        store.create_content_key(&digest, &record(2)).await?
    else {
        anyhow::bail!("a second claim on one digest must not report Created");
    };
    assert_eq!(incumbent, record(1));

    // A read returns the same record, and a version a swap accepts.
    let (read, read_version) = store
        .get_content_key(&digest)
        .await?
        .ok_or_else(|| anyhow::anyhow!("claimed digest reads back"))?;
    assert_eq!(read, record(1));

    // The stale swap runs first, so the assertion below proves the record it
    // refused never reached the store.
    assert_eq!(
        store
            .replace_content_key(&other_digest, &read_version, &record(3))
            .await?,
        ContentKeyCas::Lost,
        "a swap against an absent record is lost"
    );
    assert_eq!(
        store
            .replace_content_key(&digest, &version, &record(4))
            .await?,
        ContentKeyCas::Applied
    );
    let (replaced, replaced_version) = store
        .get_content_key(&digest)
        .await?
        .ok_or_else(|| anyhow::anyhow!("replaced digest reads back"))?;
    assert_eq!(replaced, record(4));
    assert_eq!(
        store
            .replace_content_key(&digest, &read_version, &record(5))
            .await?,
        ContentKeyCas::Lost,
        "a swap presenting the superseded version must not land"
    );
    assert_eq!(
        store
            .get_content_key(&digest)
            .await?
            .map(|(record, _)| record),
        Some(record(4))
    );

    // The listing carries the digest, the record, and a usable version.
    let listed = store.list_content_keys().await?;
    assert_eq!(listed.len(), 1);
    let entry = listed.first().context("expected one content key")?;
    assert_eq!(entry.0, digest);
    assert_eq!(entry.1, record(4));

    // A delete is conditioned too: the superseded version is refused, the
    // current one collects the record, and a repeat delete finds nothing.
    assert_eq!(
        store.delete_content_key(&digest, &read_version).await?,
        ContentKeyCas::Lost
    );
    assert!(store.get_content_key(&digest).await?.is_some());
    assert_eq!(
        store.delete_content_key(&digest, &replaced_version).await?,
        ContentKeyCas::Applied
    );
    assert!(store.get_content_key(&digest).await?.is_none());
    assert_eq!(
        store.delete_content_key(&digest, &replaced_version).await?,
        ContentKeyCas::Lost
    );
    assert!(store.list_content_keys().await?.is_empty());

    // Concurrent claims resolve to one winner and every loser reads a
    // *complete* record: publishing the key before its bytes would fail a parse.
    let contested = "c".repeat(64);
    let claimants: Vec<ContentKeyRecord> = (10..18).map(record).collect();
    let outcomes = futures::future::join_all(
        claimants
            .iter()
            .map(|r| store.create_content_key(&contested, r)),
    )
    .await;
    let mut created = 0;
    for outcome in outcomes {
        match outcome? {
            ContentKeyClaim::Created => created += 1,
            ContentKeyClaim::AlreadyExists(seen, _) => assert!(
                claimants.contains(&seen),
                "a loser reads one claimant's complete record"
            ),
        }
    }
    assert_eq!(created, 1, "one digest admits one create");
    Ok(())
}

/// Membership reads both per job (teardown's last-member rule) and whole-tree
/// (one snapshot per maintenance run), and a marker's expiry suffix survives.
pub async fn assert_job_plan_marker_roundtrip(store: &dyn PlanStore) -> anyhow::Result<()> {
    let shared = JobId::from_uuid(uuid::Uuid::from_u128(1));
    let solo = JobId::from_uuid(uuid::Uuid::from_u128(2));
    let first = PlanId::from_uuid(uuid::Uuid::from_u128(10));
    let second = PlanId::from_uuid(uuid::Uuid::from_u128(11));
    let soon = ExpiresAt::At(
        DateTime::from_timestamp(1_780_000_000, 0).ok_or_else(|| anyhow::anyhow!("timestamp"))?,
    );

    // A job with no membership reads as empty rather than as an error.
    assert!(store.list_job_plan_markers(&shared).await?.is_empty());
    assert!(store.list_all_job_plan_markers().await?.is_empty());

    store.write_job_plan_marker(&shared, &first, soon).await?;
    // Each plan records its own expiry, so two members of one job can carry
    // different horizons.
    store
        .write_job_plan_marker(&shared, &second, ExpiresAt::Never)
        .await?;
    store
        .write_job_plan_marker(&solo, &first, ExpiresAt::Never)
        .await?;
    // Idempotent: re-writing a membership adds no second marker.
    store.write_job_plan_marker(&shared, &first, soon).await?;

    let mut members = store.list_job_plan_markers(&shared).await?;
    members.sort_by(|a, b| a.0.as_str().cmp(b.0.as_str()));
    assert_eq!(
        members,
        vec![(first.clone(), soon), (second.clone(), ExpiresAt::Never)]
    );
    assert_eq!(
        store.list_job_plan_markers(&solo).await?,
        vec![(first.clone(), ExpiresAt::Never)]
    );
    assert_eq!(store.list_all_job_plan_markers().await?.len(), 3);

    // Dropping one membership leaves the job's other members alone, and is
    // idempotent.
    store.delete_job_plan_marker(&shared, &first, soon).await?;
    store.delete_job_plan_marker(&shared, &first, soon).await?;
    assert_eq!(
        store.list_job_plan_markers(&shared).await?,
        vec![(second, ExpiresAt::Never)]
    );

    // The last membership going is what licenses teardown to retire a job.
    store
        .delete_job_plan_marker(&solo, &first, ExpiresAt::Never)
        .await?;
    assert!(store.list_job_plan_markers(&solo).await?.is_empty());
    assert_eq!(store.list_all_job_plan_markers().await?.len(), 1);
    Ok(())
}

/// A source reads back byte for byte and is write-once; one that names an
/// `auth_token` is refused and writes nothing.
pub async fn assert_plan_source_roundtrip(store: &dyn PlanStore) -> anyhow::Result<()> {
    let plan_id = PlanId::from_uuid(uuid::Uuid::from_u128(20));
    let other = PlanId::from_uuid(uuid::Uuid::from_u128(21));
    // Non-ASCII text and a comment: the stored bytes are the submitted ones,
    // not a re-serialization.
    let source = PlanSource::new(
        "# Ünïcode comment kept as written\nbenchmarks = [\"decode_throughput_512_100\"]\n"
            .to_string(),
    )?;
    let other_source = PlanSource::new("benchmarks = [\"other\"]\n".to_string())?;

    assert!(store.get_plan_source(&plan_id).await?.is_none());

    let gated = PlanSource::new(
        "models = [{ repo_name = \"r\", auth_token = \"hf_SECRET\" }]\n".to_string(),
    )?;
    let refused = store.put_plan_source(&plan_id, &gated).await;
    let message = format!(
        "{:#}",
        refused.err().context("a source with a token is refused")?
    );
    assert!(!message.contains("hf_SECRET"), "{message}");
    assert!(store.get_plan_source(&plan_id).await?.is_none());

    assert_eq!(
        store.put_plan_source(&plan_id, &source).await?,
        CreateOutcome::Created
    );
    assert_eq!(
        store.put_plan_source(&other, &other_source).await?,
        CreateOutcome::Created
    );
    let read = store
        .get_plan_source(&plan_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("the source reads back"))?;
    assert_eq!(read.text(), source.text());
    assert_eq!(read.sha256(), source.sha256());

    // A second put with different text is refused, and the first source stays.
    assert_eq!(
        store.put_plan_source(&plan_id, &other_source).await?,
        CreateOutcome::AlreadyExists
    );
    let read = store
        .get_plan_source(&plan_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("the first source reads back"))?;
    assert_eq!(read.text(), source.text());
    assert_eq!(read.sha256(), source.sha256());

    // A source lives beside `plans/`, never in it.
    let listed = store.list_plans(None, None, TEST_LIST_LIMIT).await?;
    assert!(listed.items.is_empty(), "a source listed as a manifest");
    assert!(
        listed.unreadable.is_empty(),
        "a source read as a bad manifest"
    );

    // Idempotent, and the other plan's source stays.
    store.delete_plan_source(&plan_id).await?;
    store.delete_plan_source(&plan_id).await?;
    assert!(store.get_plan_source(&plan_id).await?.is_none());
    assert_eq!(
        store
            .get_plan_source(&other)
            .await?
            .map(|s| s.sha256().clone()),
        Some(other_source.sha256().clone())
    );

    // A delete frees the key, so a put creates again.
    assert_eq!(
        store.put_plan_source(&plan_id, &other_source).await?,
        CreateOutcome::Created
    );
    Ok(())
}

/// A job source is write-once, reads back as the same JSON alone or in a batch,
/// and never appears as a manifest or a membership marker.
pub async fn assert_job_source_roundtrip(store: &dyn PlanStore) -> anyhow::Result<()> {
    let job_id = JobId::from_uuid(uuid::Uuid::from_u128(40));
    let other = JobId::from_uuid(uuid::Uuid::from_u128(41));
    let first = serde_json::json!({
        "job_id": job_id.as_str(),
        "expires_at": "20260801T000000Z",
        "spec": { "benchmark": "decode_throughput_512_100" },
    });
    let second = serde_json::json!({ "job_id": job_id.as_str(), "who": "second" });
    let other_body = serde_json::json!({ "job_id": other.as_str() });

    assert!(store.get_job_source(&job_id).await?.is_none());

    assert_eq!(
        store.put_job_source(&job_id, &first).await?,
        CreateOutcome::Created
    );
    assert_eq!(
        store.put_job_source(&other, &other_body).await?,
        CreateOutcome::Created
    );
    assert_eq!(store.get_job_source(&job_id).await?, Some(first.clone()));
    assert_eq!(
        store.get_job_source(&other).await?,
        Some(other_body.clone())
    );

    // Read together, each source comes back with its job, and an absent one has no entry.
    let absent = JobId::from_uuid(uuid::Uuid::from_u128(42));
    let mut both = store
        .get_job_sources(&[job_id.clone(), absent, other.clone()])
        .await?;
    both.items.sort_by(|a, b| a.0.cmp(&b.0));
    assert_eq!(
        both.items,
        vec![(job_id.clone(), first.clone()), (other.clone(), other_body)]
    );
    assert!(both.unreadable.is_empty());
    assert!(store.get_job_sources(&[]).await?.items.is_empty());

    // A second put with a different body is refused, and the first body stays.
    assert_eq!(
        store.put_job_source(&job_id, &second).await?,
        CreateOutcome::AlreadyExists
    );
    assert_eq!(store.get_job_source(&job_id).await?, Some(first));

    // A job source lives beside `plans/` and `job-plans/`, never in them.
    let listed = store.list_plans(None, None, TEST_LIST_LIMIT).await?;
    assert!(listed.items.is_empty(), "a job source listed as a manifest");
    assert!(
        listed.unreadable.is_empty(),
        "a job source read as a bad manifest"
    );
    assert!(store.list_all_job_plan_markers().await?.is_empty());
    Ok(())
}

/// A job's link listing names every plan that links it and only those; a link
/// is write-once and never appears as a manifest or a membership marker.
pub async fn assert_job_plan_link_roundtrip(store: &dyn PlanStore) -> anyhow::Result<()> {
    let shared = JobId::from_uuid(uuid::Uuid::from_u128(50));
    // A job id that `shared` prefixes, so a listing that drops the trailing
    // `/` names this job's links too.
    let prefixed = JobId::new_unchecked(format!("{shared}0"));
    let first = PlanId::from_uuid(uuid::Uuid::from_u128(60));
    let second = PlanId::from_uuid(uuid::Uuid::from_u128(61));

    // A job with no link reads as empty rather than as an error.
    assert!(store.list_job_plan_links(&shared).await?.is_empty());

    assert_eq!(
        store.put_job_plan_link(&shared, &first).await?,
        CreateOutcome::Created
    );
    assert_eq!(
        store.put_job_plan_link(&shared, &second).await?,
        CreateOutcome::Created
    );
    assert_eq!(
        store.put_job_plan_link(&prefixed, &first).await?,
        CreateOutcome::Created
    );
    // A retry adds no second link.
    assert_eq!(
        store.put_job_plan_link(&shared, &first).await?,
        CreateOutcome::AlreadyExists
    );

    let mut linked = store.list_job_plan_links(&shared).await?;
    linked.sort_by(|a, b| a.as_str().cmp(b.as_str()));
    assert_eq!(linked, vec![first.clone(), second.clone()]);
    assert_eq!(
        store.list_job_plan_links(&prefixed).await?,
        vec![first.clone()]
    );

    // Listed together, each job comes with its own plans, a repeated one once;
    // a job no plan links has no entry.
    let unlinked = JobId::from_uuid(uuid::Uuid::from_u128(51));
    let mut together = store
        .list_job_plan_links_of(&[shared.clone(), unlinked, prefixed.clone(), shared.clone()])
        .await?;
    together.sort_by(|a, b| a.0.as_str().cmp(b.0.as_str()));
    together
        .iter_mut()
        .for_each(|(_, plans)| plans.sort_by(|a, b| a.as_str().cmp(b.as_str())));
    assert_eq!(
        together,
        vec![
            (shared.clone(), vec![first.clone(), second.clone()]),
            (prefixed.clone(), vec![first.clone()]),
        ]
    );
    assert!(store.list_job_plan_links_of(&[]).await?.is_empty());

    // A link lives beside `plans/` and `job-plans/`, never in them.
    let listed = store.list_plans(None, None, TEST_LIST_LIMIT).await?;
    assert!(listed.items.is_empty(), "a link listed as a manifest");
    assert!(
        listed.unreadable.is_empty(),
        "a link read as a bad manifest"
    );
    assert!(store.list_all_job_plan_markers().await?.is_empty());
    Ok(())
}

/// Assert that each job in `job_ids` has a job source equal to its queued
/// `avail/` body, and a link to `plan_id` alone.
pub async fn assert_each_job_has_its_source_and_link(
    plans: &dyn PlanStore,
    todo: &dyn TodoStore,
    plan_id: &PlanId,
    job_ids: &[JobId],
) -> anyhow::Result<()> {
    for job_id in job_ids {
        let queued = todo
            .get_avail_by_job(job_id)
            .await?
            .with_context(|| format!("job {job_id} is queued"))?;
        assert_eq!(
            plans.get_job_source(job_id).await?,
            Some(queued),
            "the source of job {job_id} is its queued body"
        );
        assert_eq!(
            plans.list_job_plan_links(job_id).await?,
            vec![plan_id.clone()],
            "job {job_id} links its plan"
        );
    }
    Ok(())
}

/// How many job sources and job-to-plan links a `local_fs` `[storage]` root at
/// `data_dir` holds. A missing directory counts as empty.
pub fn local_fs_job_record_count(data_dir: &std::path::Path) -> usize {
    let entries = |dir: std::path::PathBuf| {
        std::fs::read_dir(dir)
            .into_iter()
            .flatten()
            .filter_map(|entry| entry.ok())
    };
    let sources = entries(data_dir.join("job-sources")).count();
    let links: usize = entries(data_dir.join("job-plan-links"))
        .map(|job| entries(job.path()).count())
        .sum();
    sources + links
}

/// Delete `plan_id`'s manifest from a `local_fs` root at `data_dir` by hand:
/// no code path deletes a manifest.
pub fn local_fs_remove_plan(data_dir: &std::path::Path, plan_id: &PlanId) -> anyhow::Result<()> {
    let path = data_dir.join("plans").join(format!("{plan_id}.json"));
    std::fs::remove_file(&path).with_context(|| format!("removing {}", path.display()))
}

/// Splice `count` plans with no stored document and no job between `head` and
/// the plan that it supersedes. A plan filed as job bodies has no stored
/// document either. Return the ids of the spliced plans, oldest first.
pub async fn splice_without_document(
    plans: &dyn PlanStore,
    head: &PlanId,
    count: usize,
) -> anyhow::Result<Vec<PlanId>> {
    let head = plans.get_plan(head).await?.context("the head")?;
    let spliced: Vec<PlanId> = (0..count)
        .map(|_| PlanId::from_uuid(uuid::Uuid::now_v7()))
        .collect();
    let previous =
        std::iter::once(head.previous_plan_id.clone()).chain(spliced.iter().cloned().map(Some));
    for (plan_id, previous_plan_id) in spliced.iter().zip(previous) {
        plans
            .put_plan(&PlanManifest {
                plan_id: plan_id.clone(),
                previous_plan_id,
                source_sha256: None,
                submitted_sha256: None,
                job_ids: Vec::new(),
                content_keys: Default::default(),
                rerun_of: Default::default(),
                ..head.clone()
            })
            .await?;
    }
    if let Some(newest) = spliced.last() {
        plans
            .put_plan(&PlanManifest {
                previous_plan_id: Some(newest.clone()),
                ..head
            })
            .await?;
    }
    Ok(spliced)
}

/// Delete the job source of `job_id` from a `local_fs` `[storage]` root at
/// `data_dir`, as someone who deletes it from storage by hand.
pub fn local_fs_remove_job_source(
    data_dir: &std::path::Path,
    job_id: &JobId,
) -> anyhow::Result<()> {
    let path = data_dir.join("job-sources").join(format!("{job_id}.json"));
    std::fs::remove_file(&path).with_context(|| format!("removing {}", path.display()))
}

/// Delete the link from `job_id` to `plan_id` from a `local_fs` `[storage]`
/// root at `data_dir`, as someone who deletes it from storage by hand.
pub fn local_fs_remove_job_plan_link(
    data_dir: &std::path::Path,
    job_id: &JobId,
    plan_id: &PlanId,
) -> anyhow::Result<()> {
    let path = data_dir
        .join("job-plan-links")
        .join(job_id.as_str())
        .join(plan_id.as_str());
    std::fs::remove_file(&path).with_context(|| format!("removing {}", path.display()))
}

/// A pending-source marker lists with a modification time the sweep's age gate
/// can read, and its write and delete are idempotent.
pub async fn assert_pending_source_marker_roundtrip(store: &dyn PlanStore) -> anyhow::Result<()> {
    let first = PlanId::from_uuid(uuid::Uuid::from_u128(30));
    let second = PlanId::from_uuid(uuid::Uuid::from_u128(31));
    let before = chrono::Utc::now() - chrono::Duration::minutes(1);

    assert!(store.list_pending_source_markers().await?.is_empty());

    store.write_pending_source_marker(&first).await?;
    store.write_pending_source_marker(&first).await?;
    store.write_pending_source_marker(&second).await?;
    let mut markers = store.list_pending_source_markers().await?;
    markers.sort_by(|a, b| a.plan_id.as_str().cmp(b.plan_id.as_str()));
    assert_eq!(
        markers
            .iter()
            .map(|m| m.plan_id.clone())
            .collect::<Vec<_>>(),
        vec![first.clone(), second.clone()]
    );
    assert!(
        markers.iter().all(|m| m.written_at > before),
        "each marker carries the time it was written: {markers:?}"
    );

    store.delete_pending_source_marker(&first).await?;
    store.delete_pending_source_marker(&first).await?;
    assert_eq!(
        store
            .list_pending_source_markers()
            .await?
            .into_iter()
            .map(|m| m.plan_id)
            .collect::<Vec<_>>(),
        vec![second]
    );
    Ok(())
}

/// Finish `job_id` as a client does: its result recorded, and its queue
/// entry gone.
pub async fn finish_job(
    submissions: &dyn SubmissionStore,
    todo: &dyn TodoStore,
    job_id: &JobId,
) -> anyhow::Result<()> {
    finish_job_as(
        submissions,
        todo,
        job_id,
        &serde_json::json!({"job_id": job_id.as_str()}),
    )
    .await
}

async fn finish_job_as(
    submissions: &dyn SubmissionStore,
    todo: &dyn TodoStore,
    job_id: &JobId,
    record: &serde_json::Value,
) -> anyhow::Result<()> {
    submissions.write_processed(job_id, record).await?;
    todo.delete_avail_by_job(job_id).await
}

/// A recorded success of `benchmark_type` for `job_id`, as a client submits it
/// and the server stores it. An `eval` one holds completions to score again.
pub fn success_record(job_id: &JobId, benchmark_type: &str) -> serde_json::Value {
    serde_json::json!({
        "message_type": "success",
        "benchmark_type": benchmark_type,
        "benchmark_id": "eval_test",
        "client_id": "ev1_test",
        "job_id": job_id.as_str(),
        "submitted_at": "2026-05-29T14:49:43Z",
        "device_name": "MacBook Pro",
        "device_form_factor": "laptop",
        "device_os_name": "macOS",
        "device_os_version": "26",
        "device_chip_model": "Apple M5",
        "device_ram_bytes": 51_539_607_552_i64,
        "model_name": "m",
        "model_quant": "Q5_K_M",
        "runtime_name": "rt",
        "runtime_version": "v1",
        "completions": [{"id": "s1", "completion": "@@@@"}],
        "requested_model_flags": "{}",
    })
}

/// [`finish_job`] with a [`success_record`] of `benchmark_type`.
pub async fn finish_job_with_success(
    submissions: &dyn SubmissionStore,
    todo: &dyn TodoStore,
    job_id: &JobId,
    benchmark_type: &str,
) -> anyhow::Result<()> {
    finish_job_as(
        submissions,
        todo,
        job_id,
        &success_record(job_id, benchmark_type),
    )
    .await
}

/// [`finish_job_with_success`] whose record names the client `build` it ran
/// on, or none.
pub async fn finish_job_with_success_on_build(
    submissions: &dyn SubmissionStore,
    todo: &dyn TodoStore,
    job_id: &JobId,
    benchmark_type: &str,
    build: Option<&str>,
) -> anyhow::Result<()> {
    let mut record = success_record(job_id, benchmark_type);
    if let (Some(fields), Some(build)) = (record.as_object_mut(), build) {
        fields.insert("client_version".into(), build.into());
    }
    finish_job_as(submissions, todo, job_id, &record).await
}

/// [`finish_job`] with a recorded failure, which holds no completions.
pub async fn finish_job_with_failure(
    submissions: &dyn SubmissionStore,
    todo: &dyn TodoStore,
    job_id: &JobId,
) -> anyhow::Result<()> {
    finish_job_as(
        submissions,
        todo,
        job_id,
        &serde_json::json!({"job_id": job_id.as_str(), "message_type": "failure"}),
    )
    .await
}

/// The blob every vault contract test reads back. Small, because what is under
/// test is the keyspace split and not the streaming.
pub const VAULT_BLOB: &[u8] = b"private weights, not really";

/// What [`VAULT_BLOB`] hashes to.
pub fn vault_blob_sha256() -> anyhow::Result<Sha256> {
    Ok(Sha256::try_new(hex::encode(sha2::Sha256::digest(
        VAULT_BLOB,
    )))?)
}

/// `bytes` as a body arrives: a few bytes at a time, so a store is held to
/// joining chunks rather than taking the body whole.
pub fn blob_body(bytes: &'static [u8]) -> BlobBody {
    let chunks: Vec<anyhow::Result<bytes::Bytes>> = bytes
        .chunks(5)
        .map(|chunk| Ok(bytes::Bytes::from_static(chunk)))
        .collect();
    Box::pin(futures::stream::iter(chunks))
}

/// What [`cut_off_blob_body`] delivers before it fails.
pub const CUT_OFF_BYTES: &[u8] = b"private";

/// A body that delivers [`CUT_OFF_BYTES`] and then fails, as a dropped
/// connection does.
pub fn cut_off_blob_body() -> BlobBody {
    Box::pin(futures::stream::iter([
        Ok(bytes::Bytes::from_static(CUT_OFF_BYTES)),
        Err(anyhow::anyhow!("the connection dropped")),
    ]))
}

/// A blob limit of exactly the length of [`VAULT_BLOB`].
pub fn vault_blob_limit() -> anyhow::Result<NonZeroU64> {
    NonZeroU64::new(VAULT_BLOB.len() as u64).context("the test blob is not empty")
}

/// Every byte of `body`, joined.
async fn read_all(body: ArtifactBody) -> anyhow::Result<Vec<u8>> {
    body.stream
        .try_fold(Vec::new(), |mut acc, chunk| async move {
            acc.extend_from_slice(&chunk);
            Ok(acc)
        })
        .await
}

/// A record over [`VAULT_BLOB`], uploaded as a `gguf_text` model by a fresh
/// account. Its blob is not written: that is the caller's.
pub fn a_vault_resource() -> anyhow::Result<VaultResource> {
    vault_resource_for(
        VaultOwner::User {
            user_id: UserId::mint(),
        },
        vault_blob_sha256()?,
        VAULT_BLOB.len() as u64,
    )
}

/// The records of `owner`'s upload of `artifact` under `name`, each file of
/// 4096 bytes. Their blobs are not written: that is the caller's.
pub fn a_vault_upload(
    name: &str,
    owner: VaultOwner,
    artifact: serde_json::Value,
) -> anyhow::Result<Vec<VaultResource>> {
    let artifact: VaultArtifact<VaultUploadFile> = serde_json::from_value(artifact)?;
    Ok(VaultResource::from_upload(
        NonEmptyTrimmedString::try_new(name)?,
        owner,
        artifact.map(|_, file| FoundBlob { file, bytes: 4096 }),
        Utc::now(),
    )?)
}

/// The record of a llama.cpp build that a fresh account uploaded as
/// `acme llama.cpp build`.
pub fn a_vault_runtime() -> anyhow::Result<VaultResource> {
    a_vault_upload(
        "acme llama.cpp build",
        VaultOwner::User {
            user_id: UserId::mint(),
        },
        serde_json::json!({
            "type": "llamacpp_cli_stock_tools",
            "flavor": "macos-arm64",
            "archive": {
                "filename": "llama-b1234-bin-macos-arm64.tar.gz",
                "sha256": "e".repeat(64),
            },
        }),
    )?
    .pop()
    .context("an upload of one file makes one record")
}

/// The model and mmproj records of a `gguf_vision` model that a fresh account
/// uploaded as `acme LFM2-VL-3B q4`.
pub fn a_vision_upload() -> anyhow::Result<Vec<VaultResource>> {
    a_vault_upload(
        "acme LFM2-VL-3B q4",
        VaultOwner::User {
            user_id: UserId::mint(),
        },
        serde_json::json!({
            "type": "gguf_vision",
            "model": {"filename": "LFM2-VL-3B-Q4_K_M.gguf", "sha256": "a".repeat(64)},
            "mmproj": {"filename": "mmproj-LFM2-VL-3B-F16.gguf", "sha256": "b".repeat(64)},
        }),
    )
}

/// The origin of one gguf file mgmt imported from HuggingFace, displayed as
/// `LiquidAI/LFM2-700M-GGUF@aaaa…aaaa:LFM2-700M-Q4_0.gguf`.
pub fn a_huggingface_origin() -> anyhow::Result<VaultOrigin> {
    Ok(serde_json::from_value(serde_json::json!({
        "source": "huggingface",
        "org": "LiquidAI",
        "repo_name": "LFM2-700M-GGUF",
        "commit": "a".repeat(40),
        "path": "LFM2-700M-Q4_0.gguf",
    }))?)
}

/// `owner`'s record over one file of `bytes` hashing to `sha256`, uploaded as a
/// `gguf_text` model. Its blob is not written: that is the caller's.
pub fn vault_resource_for(
    owner: VaultOwner,
    sha256: Sha256,
    bytes: u64,
) -> anyhow::Result<VaultResource> {
    let artifact: VaultArtifact<VaultUploadFile> = serde_json::from_value(serde_json::json!({
        "type": "gguf_text",
        "model": {"filename": "private-checkpoint.gguf", "sha256": sha256},
    }))?;
    VaultResource::from_upload(
        NonEmptyTrimmedString::try_new("private checkpoint")?,
        owner,
        artifact.map(|_, file| FoundBlob { file, bytes }),
        Utc::now(),
    )?
    .pop()
    .context("an upload of one file makes one record")
}

/// A private entry for the catalog contract, distinct from anything the
/// shipped examples carry.
pub fn an_org_benchmark() -> anyhow::Result<Benchmark> {
    Benchmark::from_def(BenchmarkDefinition::PrefillThroughput(
        PrefillThroughputBenchmark {
            benchmark_id: BenchmarkId::try_new("prefill_throughput_acme_4096")?,
            parameter_prefill_tokens: NonNegativeCount::try_new(4096)?,
            parameter_batch_size: BatchSize::try_new(2)?,
        },
    ))
}

/// The per-organization catalog: empty rather than an error when nothing is
/// written, entries marked as the organization's and invisible to another.
pub async fn assert_org_catalog_store_roundtrip(store: &dyn CatalogStore) -> anyhow::Result<()> {
    let acme = OrgId::mint();
    let labs = OrgId::mint();
    let entry = an_org_benchmark()?;
    let id = entry.benchmark_id().clone();

    assert!(store.load_org_catalog(&acme).await?.is_empty());
    assert!(store.get_org_benchmark(&acme, &id).await?.is_none());

    store.put_org_benchmark(&acme, &entry).await?;
    let expected = entry.clone().private_to(acme.clone());
    assert_eq!(
        store.get_org_benchmark(&acme, &id).await?,
        Some(expected.clone()),
        "the entry must survive the store whole, and come back as the organization's"
    );
    let catalog = store.load_org_catalog(&acme).await?;
    assert_eq!(catalog.len(), 1);
    assert_eq!(catalog.get(&id), Some(&expected));

    // Another organization's catalog is another keyspace.
    assert!(store.load_org_catalog(&labs).await?.is_empty());
    assert!(store.get_org_benchmark(&labs, &id).await?.is_none());

    // A put under the same id replaces.
    let replaced = Benchmark::from_def(BenchmarkDefinition::PrefillThroughput(
        PrefillThroughputBenchmark {
            benchmark_id: id.clone(),
            parameter_prefill_tokens: NonNegativeCount::try_new(8192)?,
            parameter_batch_size: BatchSize::one(),
        },
    ))?;
    store.put_org_benchmark(&acme, &replaced).await?;
    assert_eq!(
        store.get_org_benchmark(&acme, &id).await?,
        Some(replaced.private_to(acme.clone()))
    );

    store.delete_org_benchmark(&acme, &id).await?;
    assert!(store.get_org_benchmark(&acme, &id).await?.is_none());
    assert!(store.load_org_catalog(&acme).await?.is_empty());
    store.delete_org_benchmark(&acme, &id).await?;
    Ok(())
}

/// One organization's listing is sorted by id, matches
/// [`CatalogStore::list_benchmark_ids_of`], and reports an unparsable entry as
/// the [`UnreadableObjectError`] a read by id fails with. `write_raw` writes
/// `{org_id}/{benchmark_id}` past [`CatalogStore::put_org_benchmark`]'s parse.
pub async fn assert_org_benchmark_listing<F, Fut>(
    store: &dyn CatalogStore,
    write_raw: F,
) -> anyhow::Result<()>
where
    F: Fn(OrgId, &'static str, &'static str) -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<()>>,
{
    let acme = OrgId::mint();
    let labs = OrgId::mint();
    assert!(store.list_org_benchmarks(&acme).await?.is_empty());

    // Written out of id order, so the order below is the listing's own.
    let late = an_org_benchmark()?;
    let early = Benchmark::from_def(BenchmarkDefinition::PrefillThroughput(
        PrefillThroughputBenchmark {
            benchmark_id: BenchmarkId::try_new("prefill_throughput_acme_1024")?,
            parameter_prefill_tokens: NonNegativeCount::try_new(1024)?,
            parameter_batch_size: BatchSize::one(),
        },
    ))?;
    store.put_org_benchmark(&acme, &late).await?;
    store.put_org_benchmark(&acme, &early).await?;
    write_raw(acme.clone(), "broken", "benchmark_type = \"nope\"").await?;
    store.put_org_benchmark(&labs, &late).await?;

    let listed = store.list_org_benchmarks(&acme).await?;
    let ids: Vec<&str> = listed.iter().map(|(id, _)| id.as_str()).collect();
    let kept = [
        "broken",
        "prefill_throughput_acme_1024",
        "prefill_throughput_acme_4096",
    ];
    assert_eq!(ids, kept);
    let by_key = store.list_benchmark_ids_of(&acme).await?;
    assert_eq!(
        by_key.iter().map(BenchmarkId::as_str).collect::<Vec<_>>(),
        kept,
        "the ids are the listing's, the unparseable one too"
    );
    for (benchmark_id, entry) in &listed {
        let read = store.get_org_benchmark(&acme, benchmark_id).await;
        match (entry, read) {
            (Ok(listed), Ok(Some(read))) => assert_eq!(listed, &read),
            (Err(listed), Err(read)) => {
                let read = read
                    .downcast::<UnreadableObjectError>()
                    .map_err(|e| e.context("a read by id fails with no UnreadableObjectError"))?;
                assert_eq!((&listed.key, &listed.reason), (&read.key, &read.reason));
            }
            (entry, read) => anyhow::bail!("{benchmark_id}: listed {entry:?}, read {read:?}"),
        }
    }
    assert_eq!(
        listed.get(1).and_then(|(_, entry)| entry.as_ref().ok()),
        Some(&early.private_to(acme.clone())),
        "a listed entry comes back as the organization's"
    );

    assert!(
        store
            .list_benchmark_ids_of(&OrgId::mint())
            .await?
            .is_empty()
    );
    let labs_listed = store.list_org_benchmarks(&labs).await?;
    assert_eq!(labs_listed.len(), 1);
    assert_eq!(
        labs_listed.first().map(|(id, _)| id),
        Some(late.benchmark_id())
    );
    Ok(())
}

/// The cross-organization listing every backend is held to: empty before
/// anything is written, then every organization's ids and nothing from the
/// public catalog. `store` must start with no private entries.
pub async fn assert_org_benchmark_ids_span_every_org(
    store: &dyn CatalogStore,
) -> anyhow::Result<()> {
    assert!(store.list_org_benchmark_ids().await?.is_empty());

    let acme = OrgId::mint();
    let labs = OrgId::mint();
    let entry = an_org_benchmark()?;
    let other = Benchmark::from_def(BenchmarkDefinition::PrefillThroughput(
        PrefillThroughputBenchmark {
            benchmark_id: BenchmarkId::try_new("prefill_throughput_labs_512")?,
            parameter_prefill_tokens: NonNegativeCount::try_new(512)?,
            parameter_batch_size: BatchSize::one(),
        },
    ))?;
    store.put_org_benchmark(&acme, &entry).await?;
    store.put_org_benchmark(&labs, &entry).await?;
    store.put_org_benchmark(&labs, &other).await?;

    let mut listed = store.list_org_benchmark_ids().await?;
    listed.sort();
    let mut expected = vec![
        (acme.clone(), entry.benchmark_id().clone()),
        (labs.clone(), entry.benchmark_id().clone()),
        (labs.clone(), other.benchmark_id().clone()),
    ];
    expected.sort();
    assert_eq!(listed, expected);

    store
        .delete_org_benchmark(&labs, entry.benchmark_id())
        .await?;
    assert_eq!(store.list_org_benchmark_ids().await?.len(), 2);
    Ok(())
}

/// A blob is written only whole and only as its digest says, and the blob half
/// answers separately from the record half, which is what the split is for.
pub async fn assert_vault_store_roundtrip(store: &dyn VaultStore) -> anyhow::Result<()> {
    let resource = a_vault_resource()?;
    let resource_id = resource.resource_id.clone();
    let key = resource.object_key.clone();
    let sha256 = vault_blob_sha256()?;
    let len = VAULT_BLOB.len() as u64;

    assert!(store.get_resource(&resource_id).await?.is_none());
    assert!(drain_vault_resources(store).await?.items.is_empty());
    assert_eq!(store.stat_blob(&key).await?, None);

    // Bytes that hash to anything else are not kept, and neither is a body
    // that fails midway: a reader never sees a blob other than its digest says.
    assert_eq!(
        store
            .put_blob(&key, &sha256, blob_body(b"other weights"))
            .await?,
        BlobWrite::Mismatch {
            actual: Sha256::try_new(hex::encode(sha2::Sha256::digest(b"other weights")))?,
            bytes: 13,
        }
    );
    assert_eq!(
        store.stat_blob(&key).await?,
        None,
        "a mismatch writes nothing"
    );
    assert!(
        store
            .put_blob(&key, &sha256, cut_off_blob_body())
            .await
            .is_err()
    );
    assert_eq!(
        store.stat_blob(&key).await?,
        None,
        "a cut-off body writes nothing"
    );

    assert_eq!(
        store.put_blob(&key, &sha256, blob_body(VAULT_BLOB)).await?,
        BlobWrite::Stored { bytes: len }
    );
    assert_eq!(
        store.stat_blob(&key).await?,
        Some(len),
        "a blob is readable before any record names it"
    );
    // It is listed with its size and a write time, under the key it was put at.
    let listed = store.list_blobs().await?;
    let [blob] = listed.as_slice() else {
        anyhow::bail!("one blob was written: {listed:?}");
    };
    assert_eq!((&blob.key, blob.bytes), (&key, len));
    assert!((Utc::now() - blob.modified).num_minutes().abs() < 5);
    // Writing the same bytes again is harmless: the key says what they are.
    assert_eq!(
        store.put_blob(&key, &sha256, blob_body(VAULT_BLOB)).await?,
        BlobWrite::Stored { bytes: len }
    );

    store.put_resource(&resource).await?;
    assert_eq!(
        store.get_resource(&resource_id).await?,
        Some(resource.clone()),
        "the record must survive the store whole"
    );

    // A second record, so the listing pages and orders by creation. It
    // carries an origin, which the store keeps whole too.
    let second = VaultResource {
        origin: Some(a_huggingface_origin()?),
        ..a_vault_resource()?
    };
    store.put_resource(&second).await?;
    assert_eq!(
        store.get_resource(&second.resource_id).await?,
        Some(second.clone()),
        "a record with an origin must survive the store whole"
    );
    // Read together, the records asked for come back whole, a repeated one
    // once, and an absent one has no entry.
    let mut together = store
        .get_resources(&[
            second.resource_id.clone(),
            VaultResourceId::mint(),
            resource_id.clone(),
            second.resource_id.clone(),
        ])
        .await?;
    together.sort_by(|a, b| a.resource_id.as_str().cmp(b.resource_id.as_str()));
    let mut both = vec![resource.clone(), second.clone()];
    both.sort_by(|a, b| a.resource_id.as_str().cmp(b.resource_id.as_str()));
    assert_eq!(together, both);
    assert!(store.get_resources(&[]).await?.is_empty());
    let page = store.list_resources(None, nonzero_usize(1)).await?;
    assert_eq!(page.items.len(), 1);
    assert_eq!(
        page.items
            .first()
            .context("expected one resource")?
            .resource_id,
        resource_id,
        "creation order"
    );
    let rest = store
        .list_resources(page.resume_after.as_ref(), nonzero_usize(10))
        .await?;
    assert_eq!(rest.items.len(), 1);
    assert_eq!(
        rest.items
            .first()
            .context("expected one resource")?
            .resource_id,
        second.resource_id
    );

    // One owner's records, in the same order, and nobody else's.
    let later = vault_resource_for(resource.owner.clone(), vault_blob_sha256()?, len)?;
    store.put_resource(&later).await?;
    let ids = |listing: pipette_store::VaultResourceListing| -> Vec<VaultResourceId> {
        listing
            .items
            .into_iter()
            .map(|resource| resource.resource_id)
            .collect()
    };
    assert_eq!(
        ids(store.list_resources_of(&resource.owner).await?),
        [resource_id.clone(), later.resource_id.clone()]
    );
    assert_eq!(
        ids(store.list_resources_of(&second.owner).await?),
        std::slice::from_ref(&second.resource_id)
    );
    store.delete_resource(&later.resource_id).await?;
    assert_eq!(
        ids(store.list_resources_of(&resource.owner).await?),
        std::slice::from_ref(&resource_id)
    );

    // The bytes, streamed whole.
    let body = store
        .open_blob(&key, 0)
        .await?
        .ok_or_else(|| anyhow::anyhow!("the blob the record names is missing"))?;
    assert_eq!(body.bytes, len);
    assert_eq!(read_all(body).await?, VAULT_BLOB);

    // The bytes from an offset to the end, with the length of that tail.
    let tail = VAULT_BLOB
        .get(4..)
        .context("the test blob is longer than the offset")?;
    let body = store
        .open_blob(&key, 4)
        .await?
        .ok_or_else(|| anyhow::anyhow!("the blob the record names is missing"))?;
    assert_eq!(body.bytes, len - 4);
    assert_eq!(read_all(body).await?, tail);

    // A blob nobody uploaded reads as absent through both halves, rather than
    // erroring.
    let missing = second.object_key.clone();
    assert_eq!(store.stat_blob(&missing).await?, None);
    assert!(store.open_blob(&missing, 0).await?.is_none());

    // The delete takes the record and leaves the bytes, which another record
    // may still name.
    store.delete_resource(&resource_id).await?;
    assert!(store.get_resource(&resource_id).await?.is_none());
    assert_eq!(
        store.stat_blob(&key).await?,
        Some(len),
        "deleting a record must not reach the blob it named"
    );
    store.delete_resource(&resource_id).await?;

    // The blob goes only by its own delete, which is idempotent.
    store.delete_blob(&key).await?;
    assert_eq!(store.stat_blob(&key).await?, None);
    assert!(store.open_blob(&key, 0).await?.is_none());
    assert!(store.list_blobs().await?.is_empty());
    store.delete_blob(&key).await?;
    Ok(())
}

/// The store digests the bytes as they stream and keys the blob by owner and
/// digest; a body over the limit, or one that fails midway, writes nothing.
pub async fn assert_vault_staged_write(store: &dyn VaultStore) -> anyhow::Result<()> {
    let owner = VaultOwner::User {
        user_id: UserId::mint(),
    };
    let sha256 = vault_blob_sha256()?;
    let len = VAULT_BLOB.len() as u64;
    let key = pipette_core::vault::blob_key(&owner, &sha256)?;
    let limit = vault_blob_limit()?;

    // One byte too many is refused, and leaves nothing at the key.
    let tight = NonZeroU64::new(len - 1).context("the test blob is longer than one byte")?;
    assert_eq!(
        store
            .put_staged_blob(&owner, blob_body(VAULT_BLOB), tight)
            .await?,
        StagedWrite::TooLarge { limit: tight }
    );
    assert_eq!(
        store.stat_blob(&key).await?,
        None,
        "an oversized body writes nothing"
    );

    // A store that kept the cut-off bytes would key them by their own digest.
    let cut_off_key = pipette_core::vault::blob_key(
        &owner,
        &Sha256::try_new(hex::encode(sha2::Sha256::digest(CUT_OFF_BYTES)))?,
    )?;
    assert!(
        store
            .put_staged_blob(&owner, cut_off_blob_body(), limit)
            .await
            .is_err()
    );
    assert_eq!(
        store.stat_blob(&cut_off_key).await?,
        None,
        "a cut-off body writes nothing"
    );

    // A body exactly at the limit is kept, at the key of its digest.
    let stored = StagedWrite::Stored {
        sha256: sha256.clone(),
        bytes: len,
        key: key.clone(),
    };
    assert_eq!(
        store
            .put_staged_blob(&owner, blob_body(VAULT_BLOB), limit)
            .await?,
        stored
    );
    assert_eq!(store.stat_blob(&key).await?, Some(len));
    let body = store
        .open_blob(&key, 0)
        .await?
        .context("the staged blob is missing")?;
    assert_eq!(read_all(body).await?, VAULT_BLOB);

    // The same bytes again find the blob already in place.
    assert_eq!(
        store
            .put_staged_blob(&owner, blob_body(VAULT_BLOB), limit)
            .await?,
        stored
    );
    assert_eq!(store.stat_blob(&key).await?, Some(len));
    Ok(())
}

/// Each staged write logs one promote event. Not `async`: [`capture_logs`]
/// runs `store` on a runtime of its own.
pub fn assert_vault_staged_write_logs_once(store: &dyn VaultStore) -> anyhow::Result<()> {
    let (written, logs) = capture_logs(|runtime| {
        runtime.block_on(async {
            let owner = a_vault_resource()?.owner;
            for _ in 0..2 {
                store
                    .put_staged_blob(&owner, blob_body(VAULT_BLOB), vault_blob_limit()?)
                    .await?;
            }
            anyhow::Ok(())
        })
    })?;
    written?;
    let already_stored: Vec<bool> = logs
        .lines()
        .filter(|line| line.contains("vault staged blob promoted"))
        .map(|line| line.contains("already_stored=true"))
        .collect();
    assert_eq!(already_stored, [false, true], "{logs}");
    Ok(())
}

/// Run `f` under an `INFO` log subscriber and return what it wrote. A copy of
/// `pipette_web_testkit::logs::capture_logs_at_level` (which says why `f` gets
/// its own runtime): that crate depends on the store backends.
pub fn capture_logs<T>(
    f: impl FnOnce(&tokio::runtime::Runtime) -> T,
) -> anyhow::Result<(T, String)> {
    #[derive(Clone)]
    struct Writer(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

    impl std::io::Write for Writer {
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

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Writer {
        type Writer = Self;
        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    // Never the only registered dispatcher: alone, tracing-core caches a callsite's
    // interest from the first thread to reach it, often a sibling test with none.
    static SECOND: std::sync::OnceLock<tracing::Dispatch> = std::sync::OnceLock::new();
    SECOND.get_or_init(|| tracing::Dispatch::new(tracing::subscriber::NoSubscriber::default()));

    let buf = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let subscriber = tracing_subscriber::fmt()
        .with_writer(Writer(std::sync::Arc::clone(&buf)))
        .with_ansi(false)
        .with_max_level(tracing::Level::INFO)
        .finish();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("test runtime")?;
    let out = tracing::subscriber::with_default(subscriber, || f(&runtime));
    let captured = buf
        .lock()
        .map_err(|_| anyhow::anyhow!("log capture buffer poisoned"))?;
    Ok((out, String::from_utf8_lossy(&captured).into_owned()))
}

/// An import-index entry is absent until written, read back whole, replaced by
/// a second write, and keyed by owner and import digest.
pub async fn assert_vault_import_roundtrip(store: &dyn VaultStore) -> anyhow::Result<()> {
    let owner = VaultOwner::Org {
        org_id: OrgId::mint(),
    };
    let other = VaultOwner::User {
        user_id: UserId::mint(),
    };
    let import = vault_blob_sha256()?;
    let another = Sha256::try_new("b".repeat(64))?;

    assert_eq!(store.get_import(&owner, &import).await?, None);

    let first = VaultImport {
        resource_ids: vec![VaultResourceId::mint(), VaultResourceId::mint()],
    };
    store.put_import(&owner, &import, &first).await?;
    assert_eq!(
        store.get_import(&owner, &import).await?,
        Some(first.clone()),
        "the entry must survive the store whole"
    );
    assert_eq!(store.get_import(&other, &import).await?, None);
    assert_eq!(store.get_import(&owner, &another).await?, None);

    // The last writer wins.
    let second = VaultImport {
        resource_ids: vec![VaultResourceId::mint()],
    };
    store.put_import(&owner, &import, &second).await?;
    assert_eq!(store.get_import(&owner, &import).await?, Some(second));
    Ok(())
}

/// An event by a fresh account, listed in `org_ids`' logs.
pub fn an_audit_event(org_ids: &[&str]) -> anyhow::Result<AuditEvent> {
    let mut event = new_event(
        AuditActor::User {
            user_id: UserId::mint(),
            key_id: pipette_plan_types::UserKeyId::try_new("userkey_0195c3e2a4f37b8c")?,
        },
        AuditAction::Http {
            method: "PATCH".into(),
            route: "/api/v1/campaigns/{campaign_id}".into(),
            path: "/api/v1/campaigns/campaign_a".into(),
            status: 200,
        },
        AuditOutcome::Succeeded,
    );
    event.org_ids = org_ids
        .iter()
        .map(|org_id| OrgId::try_new(*org_id))
        .collect::<Result<_, _>>()?;
    Ok(event)
}

/// Every event `filter` keeps, walked `per_page` at a time.
async fn audit_walk(
    store: &dyn AuditStore,
    filter: &AuditFilter,
    per_page: usize,
) -> anyhow::Result<Vec<AuditEventId>> {
    let mut seen = Vec::new();
    let mut before = None;
    loop {
        let page = store
            .list_events(filter, before.as_ref(), nonzero_usize(per_page))
            .await?;
        assert!(page.items.len() <= per_page);
        seen.extend(page.items.into_iter().map(|event| event.event_id));
        match page.resume_after {
            Some(next) => before = Some(next),
            None => return Ok(seen),
        }
    }
}

/// An audit log lists newest first, by organization, outcome, actor kind and
/// named record, and prunes by age.
pub async fn assert_audit_store_contract(store: &dyn AuditStore) -> anyhow::Result<()> {
    let every = AuditFilter::default();
    assert_eq!(
        audit_walk(store, &every, 10).await?,
        Vec::<AuditEventId>::new()
    );

    let first = an_audit_event(&["org_a"])?;
    let device = ClientId::try_new("ev1_0123456789abcdef")?;
    let second = AuditEvent {
        actor: AuditActor::Client {
            client_id: device.clone(),
        },
        ..an_audit_event(&["org_a", "org_b"])?
    };
    let campaign = CampaignId::mint();
    let third = AuditEvent {
        outcome: AuditOutcome::Refused,
        targets: [AuditTarget::Campaign {
            campaign_id: campaign.clone(),
        }]
        .into(),
        ..an_audit_event(&[])?
    };
    for event in [&first, &second, &third] {
        store.append(event).await?;
    }
    let ids = |events: &[&AuditEvent]| -> Vec<AuditEventId> {
        events.iter().map(|event| event.event_id.clone()).collect()
    };

    let page = store.list_events(&every, None, nonzero_usize(10)).await?;
    assert_eq!(
        page.items,
        vec![third.clone(), second.clone(), first.clone()]
    );
    assert!(page.unreadable.is_empty());
    for per_page in [1, 2] {
        assert_eq!(
            audit_walk(store, &every, per_page).await?,
            ids(&[&third, &second, &first]),
            "{per_page} a page"
        );
    }

    let org = |org_id: &str| -> anyhow::Result<AuditFilter> {
        Ok(AuditFilter {
            org_id: Some(OrgId::try_new(org_id)?),
            ..AuditFilter::default()
        })
    };
    let refused = AuditFilter {
        outcomes: [AuditOutcome::Refused].into(),
        ..AuditFilter::default()
    };
    let people = AuditFilter {
        actor_kinds: [AuditActorKind::User].into(),
        ..AuditFilter::default()
    };
    // Several values each walk their own branch, merged newest first.
    let mixed = AuditFilter {
        outcomes: [AuditOutcome::Refused, AuditOutcome::Succeeded].into(),
        actor_kinds: [AuditActorKind::User, AuditActorKind::Client].into(),
        ..AuditFilter::default()
    };
    let people_in_a = AuditFilter {
        org_id: Some(OrgId::try_new("org_a")?),
        ..people.clone()
    };
    // A record is named as a target or as the account or device that acted.
    let about = |record: AuditTarget| AuditFilter {
        record: Some(record),
        ..AuditFilter::default()
    };
    let campaign_history = about(AuditTarget::Campaign {
        campaign_id: campaign,
    });
    let device_history = about(AuditTarget::Client { client_id: device });
    let device_in_b = AuditFilter {
        org_id: Some(OrgId::try_new("org_b")?),
        ..device_history.clone()
    };
    let refused_device = AuditFilter {
        outcomes: [AuditOutcome::Refused].into(),
        ..device_history.clone()
    };
    let walks = [
        ("org_a", org("org_a")?, 1, ids(&[&second, &first])),
        ("org_b", org("org_b")?, 10, ids(&[&second])),
        ("org_c", org("org_c")?, 10, Vec::new()),
        ("refused", refused.clone(), 10, ids(&[&third])),
        ("people", people.clone(), 1, ids(&[&third, &first])),
        ("mixed", mixed, 1, ids(&[&third, &second, &first])),
        ("people in org_a", people_in_a.clone(), 10, ids(&[&first])),
        (
            "a campaign's history",
            campaign_history.clone(),
            10,
            ids(&[&third]),
        ),
        ("a device's history", device_history, 10, ids(&[&second])),
        (
            "a device in org_b",
            device_in_b.clone(),
            10,
            ids(&[&second]),
        ),
        ("a device refused", refused_device, 10, Vec::new()),
    ];
    for (label, filter, per_page, expected) in walks {
        assert_eq!(
            audit_walk(store, &filter, per_page).await?,
            expected,
            "{label}"
        );
    }

    // A page that ends the log carries no cursor, even when it is full.
    let full = store.list_events(&every, None, nonzero_usize(3)).await?;
    assert_eq!(full.items.len(), 3);
    assert_eq!(full.resume_after, None);
    for filter in [
        &every,
        &refused,
        &people,
        &people_in_a,
        &campaign_history,
        &device_in_b,
    ] {
        let listed = store
            .list_events(filter, None, nonzero_usize(10))
            .await?
            .items;
        assert!(
            listed.iter().all(|event| filter.admits(event)),
            "{filter:?}"
        );
    }

    let retention = chrono::Duration::days(i64::from(RETENTION_DAYS));
    let old = AuditEvent {
        created_at: Utc::now() - retention - chrono::Duration::days(70),
        ..an_audit_event(&["org_a"])?
    };
    store.append(&old).await?;
    let cutoff = Utc::now() - retention;
    assert_eq!(store.count_before(cutoff).await?, 1);
    assert_eq!(store.prune_before(cutoff).await?, 1);
    assert_eq!(store.count_before(cutoff).await?, 0);
    assert_eq!(store.prune_before(cutoff).await?, 0);
    assert_eq!(
        audit_walk(store, &org("org_a")?, 10).await?,
        ids(&[&second, &first])
    );
    Ok(())
}

/// A dense model's record under `key`, written by a fresh account.
pub fn a_model_params(key: &str, total_millions: i32) -> anyhow::Result<ModelParams> {
    Ok(ModelParams::new(
        ModelKey::try_new(key)?,
        PositiveCount::try_new(total_millions)?,
        None,
        None,
        UserId::mint(),
        Utc::now(),
    )?)
}

/// A model-params listing pages in `{key}.json` byte order.
pub async fn assert_model_params_store_roundtrip(
    store: &dyn ModelParamsStore,
) -> anyhow::Result<()> {
    assert!(drain_model_params(store).await?.items.is_empty());

    let vault = a_model_params("vault_01a123b6-1b01-7382-b7f9-c3af2100782d", 4512)?;
    let moe = ModelParams::new(
        ModelKey::try_new("lfm2-8b-a1b")?,
        PositiveCount::try_new(8340)?,
        Some(PositiveCount::try_new(1500)?),
        Some(NonEmptyTrimmedString::try_new(
            "MoE: card states 1.5B active",
        )?),
        UserId::mint(),
        Utc::now(),
    )?;
    assert!(store.get_model_params(&vault.key).await?.is_none());
    store.put_model_params(&vault).await?;
    store.put_model_params(&moe).await?;
    assert_eq!(
        store.get_model_params(&vault.key).await?,
        Some(vault.clone())
    );
    let listing = drain_model_params(store).await?;
    assert_eq!(listing.items, vec![moe.clone(), vault.clone()]);
    assert!(listing.unreadable.is_empty());

    let stem = a_model_params("lfm2-8b", 8340)?;
    store.put_model_params(&stem).await?;
    assert_eq!(
        drain_model_params(store).await?.items,
        vec![moe.clone(), stem.clone(), vault.clone()],
        "`{{key}}.json` byte order: a key lists after the ones it prefixes"
    );
    store.delete_model_params(&stem.key).await?;

    let page = store.list_model_params(None, nonzero_usize(1)).await?;
    assert_eq!(page.items, vec![moe.clone()]);
    assert_eq!(page.resume_after, Some(moe.key.clone()));
    let next = store
        .list_model_params(page.resume_after.as_ref(), nonzero_usize(1))
        .await?;
    assert_eq!(next.items, vec![vault.clone()]);
    assert_eq!(next.resume_after, None);

    let corrected = ModelParams {
        total_millions: PositiveCount::try_new(4513)?,
        active_millions: PositiveCount::try_new(4513)?,
        ..vault.clone()
    };
    store.put_model_params(&corrected).await?;
    assert_eq!(
        drain_model_params(store).await?.items,
        vec![moe.clone(), corrected],
        "a write under a held key replaces the record"
    );

    store.delete_model_params(&vault.key).await?;
    store.delete_model_params(&vault.key).await?;
    assert!(store.get_model_params(&vault.key).await?.is_none());
    assert_eq!(drain_model_params(store).await?.items, vec![moe]);
    Ok(())
}

/// The id is the stable thing: an edit leaves it and the plan it names alone,
/// and a listing pages in creation order.
pub async fn assert_campaign_store_roundtrip(store: &dyn CampaignStore) -> anyhow::Result<()> {
    use pipette_plan_types::NonEmptyTrimmedString;

    let plan_id = PlanId::from_uuid(uuid::Uuid::now_v7());
    let mut campaign = NewCampaign::new(
        NonEmptyTrimmedString::try_new("Nightly LFM2 regression")?,
        CampaignOwner::User {
            user_id: UserId::mint(),
        },
    )
    .around(plan_id.clone(), Utc::now());
    let campaign_id = campaign.campaign_id.clone();

    // A campaign that does not exist is `None`, not an error.
    assert!(store.get_campaign(&campaign_id).await?.is_none());
    assert!(drain_campaigns(store).await?.items.is_empty());

    store.put_campaign(&campaign).await?;
    assert_eq!(
        store.get_campaign(&campaign_id).await?,
        Some(campaign.clone()),
        "the record must survive the store whole"
    );

    // Rename, labels and hand-off are whole-record writes that move neither the
    // id they are addressed by nor the head the campaign is on.
    let org_id = OrgId::mint();
    campaign.rename(NonEmptyTrimmedString::try_new("Nightly LFM2")?, Utc::now());
    campaign.set_labels(
        serde_json::from_value(serde_json::json!({"study": "lfm2", "Device": "S26 Ultra"}))?,
        Utc::now(),
    );
    campaign.set_owner(
        CampaignOwner::Org {
            org_id: org_id.clone(),
        },
        Utc::now(),
    );
    store.put_campaign(&campaign).await?;

    let read = store
        .get_campaign(&campaign_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("the campaign vanished between writes"))?;
    assert_eq!(read.campaign_id, campaign_id, "the identity never moves");
    assert_eq!(read.name.as_str(), "Nightly LFM2");
    assert_eq!(read.owner.org_id(), Some(&org_id));
    assert_eq!(read.labels, campaign.labels);
    assert_eq!(
        read.current_plan_id,
        Some(plan_id),
        "an edit must leave the head the campaign is on alone"
    );

    // A record running nothing (the pre-first-plan shape) must round-trip too.
    let blank = Campaign {
        current_plan_id: None,
        ..NewCampaign::new(
            NonEmptyTrimmedString::try_new("Not started")?,
            CampaignOwner::User {
                user_id: UserId::mint(),
            },
        )
        .around(PlanId::from_uuid(uuid::Uuid::now_v7()), Utc::now())
    };
    store.put_campaign(&blank).await?;
    assert_eq!(
        store.get_campaign(&blank.campaign_id).await?,
        Some(blank.clone())
    );

    // The listing returns every campaign and nothing else, in creation order —
    // the property the UUIDv7 suffix buys, and the one paging relies on.
    let listing = drain_campaigns(store).await?;
    assert_eq!(listing.items, vec![campaign.clone(), blank.clone()]);
    assert!(listing.unreadable.is_empty());

    // A page smaller than the keyspace resumes past exactly what it returned.
    let page = store.list_campaigns(None, nonzero_usize(1)).await?;
    assert_eq!(page.items.len(), 1);
    assert_eq!(page.resume_after, Some(campaign_id.clone()));
    let next = store
        .list_campaigns(page.resume_after.as_ref(), nonzero_usize(1))
        .await?;
    assert_eq!(
        next.items
            .iter()
            .map(|c| &c.campaign_id)
            .collect::<Vec<_>>(),
        vec![&blank.campaign_id],
    );
    assert_eq!(
        next.resume_after, None,
        "the end of the keyspace is what ends a drain, not a short page"
    );

    // Idempotent, which is what lets a partially failed teardown be finished by
    // running it again.
    store.delete_campaign(&campaign_id).await?;
    store.delete_campaign(&campaign_id).await?;
    assert!(store.get_campaign(&campaign_id).await?.is_none());
    assert_eq!(
        drain_campaigns(store)
            .await?
            .items
            .iter()
            .map(|c| &c.campaign_id)
            .collect::<Vec<_>>(),
        vec![&blank.campaign_id],
        "deleting one campaign leaves the others alone"
    );
    Ok(())
}

/// `write_raw` writes into `plans/` past [`PlanStore::put_plan`], to plant what
/// the trait cannot: a manifest that does not parse, a key that names no plan.
pub async fn assert_plan_listing_pagination<F, Fut>(
    store: &dyn PlanStore,
    write_raw: F,
) -> anyhow::Result<()>
where
    F: Fn(&'static str, Vec<u8>) -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<()>>,
{
    let one = nonzero_usize(1);
    let two = nonzero_usize(2);
    let four = nonzero_usize(4);
    let plenty = nonzero_usize(50);
    let id = |name: &str| PlanId::try_new(name);
    let ids = |page: &PlanPage| {
        page.items
            .iter()
            .map(|p| p.plan_id.as_str().to_string())
            .collect::<Vec<_>>()
    };

    // An empty keyspace pages to nothing, and says the scan reached the end.
    let empty = store.list_plans(None, None, plenty).await?;
    assert!(empty.items.is_empty());
    assert!(empty.unreadable.is_empty());
    assert!(
        empty.resume_after.is_none(),
        "an empty keyspace ends the scan rather than inviting another page"
    );

    // Written out of key order; the prefix pair proves paging by the complete
    // object key, `.json` suffix included.
    for (name, status) in [
        ("plan-d", PlanStatus::Complete),
        ("plan-a", PlanStatus::Active),
        ("plan-e", PlanStatus::Active),
        ("plan-b", PlanStatus::Complete),
        ("plan-a-1", PlanStatus::Active),
        ("plan-c", PlanStatus::Active),
    ] {
        store
            .put_plan(&sample_manifest(id(name)?, status, false)?)
            .await?;
    }

    // A drain at the smallest page yields every plan once, ascending, and ends.
    let mut drained = Vec::new();
    let mut after: Option<PlanId> = None;
    loop {
        let page = store.list_plans(None, after.as_ref(), one).await?;
        drained.extend(ids(&page));
        match page.resume_after {
            Some(cursor) => after = Some(cursor),
            None => break,
        }
    }
    assert_eq!(
        drained,
        vec!["plan-a-1", "plan-a", "plan-b", "plan-c", "plan-d", "plan-e"],
        "a drain is ascending by key, and reads each plan once"
    );

    // A non-final page is exactly `limit` rows and names the key to resume from.
    let page = store.list_plans(None, None, two).await?;
    assert_eq!(ids(&page), vec!["plan-a-1", "plan-a"]);
    assert_eq!(
        page.resume_after.as_ref().map(PlanId::as_str),
        Some("plan-a")
    );

    // A page that reaches the end says so, whether or not it is full.
    let last = store.list_plans(None, None, nonzero_usize(6)).await?;
    assert_eq!(last.items.len(), 6);
    assert!(
        last.resume_after.is_none(),
        "a full page that consumed the keyspace still ends the scan"
    );

    // `after` is exclusive: the plan it names is not in the next page.
    let page = store.list_plans(None, Some(&id("plan-b")?), two).await?;
    assert_eq!(ids(&page), vec!["plan-c", "plan-d"]);

    // The same request twice is the same page.
    let again = store.list_plans(None, Some(&id("plan-b")?), two).await?;
    assert_eq!(ids(&again), ids(&page));
    assert_eq!(again.resume_after, page.resume_after);

    // A cursor is a lower bound on the key, not a row, so one naming no
    // manifest still resumes where that key would sort.
    let page = store
        .list_plans(None, Some(&id("plan-c9")?), plenty)
        .await?;
    assert_eq!(ids(&page), vec!["plan-d", "plan-e"]);

    // An unreadable object occupies no row; `plan-b2` sorts between `plan-b`
    // and `plan-c`, so a four-row page has to read past it to fill.
    write_raw("plan-b2.json", b"{not json".to_vec()).await?;
    let page = store.list_plans(None, None, four).await?;
    assert_eq!(
        ids(&page),
        vec!["plan-a-1", "plan-a", "plan-b", "plan-c"],
        "a page reads more objects than it returns rather than returning a short page"
    );
    assert_eq!(page.unreadable.len(), 1);
    let report = page.unreadable.first().context("expected one report")?;
    assert!(
        report.key.contains("plan-b2"),
        "the report names the object: {report}"
    );

    // A key that names no plan is neither a row nor a report.
    write_raw("notes.txt", b"not a manifest".to_vec()).await?;
    write_raw("plan.bad.json", b"{not json".to_vec()).await?;

    // Across several pages, the broken object is reported exactly once.
    let mut listing = PlanListing::default();
    let mut after = None;
    let mut page_count = 0;
    loop {
        let page = store.list_plans(None, after.as_ref(), two).await?;
        page_count += 1;
        listing.items.extend(page.items);
        listing.unreadable.extend(page.unreadable);
        match page.resume_after {
            Some(cursor) => after = Some(cursor),
            None => break,
        }
    }
    assert!(page_count > 1, "the assertion must cross a page boundary");
    assert_eq!(
        listing
            .items
            .iter()
            .map(|p| p.plan_id.as_str())
            .collect::<Vec<_>>(),
        vec!["plan-a-1", "plan-a", "plan-b", "plan-c", "plan-d", "plan-e"]
    );
    assert_eq!(
        listing.unreadable.len(),
        1,
        "a broken object is reported once per drain, not once per page that reads it"
    );

    // The status filter leaves reports alone (an unreadable manifest has no
    // status) and a filtered scan still crosses pages and reaches the end.
    let page = store
        .list_plans(Some(PlanStatus::Complete), None, plenty)
        .await?;
    assert_eq!(ids(&page), vec!["plan-b", "plan-d"]);
    assert_eq!(page.unreadable.len(), 1);
    assert!(page.resume_after.is_none());
    let mut filtered = PlanListing::default();
    let mut after = None;
    let mut page_count = 0;
    loop {
        let page = store
            .list_plans(Some(PlanStatus::Complete), after.as_ref(), one)
            .await?;
        page_count += 1;
        filtered.items.extend(page.items);
        filtered.unreadable.extend(page.unreadable);
        match page.resume_after {
            Some(cursor) => after = Some(cursor),
            None => break,
        }
    }
    assert!(page_count > 1, "the assertion must cross a page boundary");
    assert_eq!(
        filtered
            .items
            .iter()
            .map(|p| p.plan_id.as_str())
            .collect::<Vec<_>>(),
        vec!["plan-b", "plan-d"]
    );
    assert_eq!(filtered.unreadable.len(), 1);
    Ok(())
}

/// `write_raw` plants an object as [`assert_plan_listing_pagination`]'s does.
pub async fn assert_get_plans_contract<F, Fut>(
    store: &dyn PlanStore,
    write_raw: F,
) -> anyhow::Result<()>
where
    F: Fn(&'static str, Vec<u8>) -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<()>>,
{
    let id = |name: &str| PlanId::try_new(name);
    assert!(store.get_plans(&[]).await?.items.is_empty());
    assert!(store.get_plan_summaries(&[]).await?.items.is_empty());

    let a = sample_manifest(id("plan-a")?, PlanStatus::Active, true)?;
    let b = sample_manifest(id("plan-b")?, PlanStatus::Complete, false)?;
    let c = sample_manifest(id("plan-c")?, PlanStatus::Active, false)?;
    futures::future::try_join_all([&a, &b, &c].map(|manifest| store.put_plan(manifest))).await?;
    write_raw("plan-broken.json", b"{not json".to_vec()).await?;
    let wanted = [
        id("plan-c")?,
        id("plan-missing")?,
        id("plan-a")?,
        id("plan-broken")?,
        id("plan-a")?,
    ];

    // An absent id has no entry, a named-twice one reads once, and a broken
    // one is reported without failing the rest.
    let read = store.get_plans(&wanted).await?;
    let mut items = read.items;
    items.sort_by(|x, y| x.plan_id.cmp(&y.plan_id));
    assert_eq!(items, vec![a.clone(), c.clone()]);
    let keys: Vec<&str> = read.unreadable.iter().map(|bad| bad.key.as_str()).collect();
    assert_eq!(keys, vec!["plans/plan-broken.json"]);

    let read = store.get_plan_summaries(&wanted).await?;
    let mut items = read.items;
    items.sort_by(|x, y| x.plan_id.cmp(&y.plan_id));
    assert_eq!(items, vec![a.into(), c.into()]);
    let keys: Vec<&str> = read.unreadable.iter().map(|bad| bad.key.as_str()).collect();
    assert_eq!(keys, vec!["plans/plan-broken.json"]);
    Ok(())
}

/// Each filter field narrows the rows, the fields combine, and a filtered page
/// keeps the paging contract of [`PlanStore::list_plans`].
pub async fn assert_plan_filter_contract(store: &dyn PlanStore) -> anyhow::Result<()> {
    let id = |name: &str| PlanId::try_new(name);
    let ids = |plans: &[PlanManifest]| {
        plans
            .iter()
            .map(|p| p.plan_id.as_str().to_string())
            .collect::<Vec<_>>()
    };
    let campaign_a = CampaignId::mint();
    let campaign_b = CampaignId::mint();
    let alice = UserId::mint();
    let bob = UserId::mint();
    let rows = [
        (
            "plan-a",
            PlanStatus::Active,
            Some(&campaign_a),
            Some(&alice),
        ),
        (
            "plan-b",
            PlanStatus::Complete,
            Some(&campaign_a),
            Some(&bob),
        ),
        (
            "plan-c",
            PlanStatus::Active,
            Some(&campaign_b),
            Some(&alice),
        ),
        ("plan-d", PlanStatus::Active, None, None),
        (
            "plan-e",
            PlanStatus::Active,
            Some(&campaign_a),
            Some(&alice),
        ),
    ];
    for (name, status, campaign, submitter) in rows {
        let mut manifest = sample_manifest(id(name)?, status, false)?;
        manifest.campaign_id = campaign.cloned();
        manifest.submitted_by = submitter.cloned();
        store.put_plan(&manifest).await?;
    }

    let cases = [
        (
            PlanFilter::default(),
            vec!["plan-a", "plan-b", "plan-c", "plan-d", "plan-e"],
        ),
        (
            PlanFilter {
                campaign_id: Some(campaign_a.clone()),
                ..PlanFilter::default()
            },
            vec!["plan-a", "plan-b", "plan-e"],
        ),
        (
            PlanFilter {
                submitted_by: Some(alice.clone()),
                ..PlanFilter::default()
            },
            vec!["plan-a", "plan-c", "plan-e"],
        ),
        (
            PlanFilter {
                status: Some(PlanStatus::Active),
                campaign_id: Some(campaign_a.clone()),
                submitted_by: Some(alice.clone()),
                reach: None,
            },
            vec!["plan-a", "plan-e"],
        ),
        (
            PlanFilter {
                campaign_id: Some(campaign_b.clone()),
                submitted_by: Some(bob.clone()),
                ..PlanFilter::default()
            },
            vec![],
        ),
    ];
    for (filter, expected) in &cases {
        let listing = drain_matching_plans(store, filter).await?;
        assert_eq!(&ids(&listing.items), expected, "{filter:?}");
        assert!(listing.unreadable.is_empty(), "{filter:?}");
    }

    // A filtered page is full when it is not the last, resumes after its last
    // row, and ends where no admitted plan is left.
    let campaign = PlanFilter {
        campaign_id: Some(campaign_a),
        ..PlanFilter::default()
    };
    let first = store
        .list_matching_plans(&campaign, None, nonzero_usize(2))
        .await?;
    assert_eq!(ids(&first.items), vec!["plan-a", "plan-b"]);
    let cursor = first
        .resume_after
        .context("a full page with plans left names its cursor")?;
    let rest = store
        .list_matching_plans(&campaign, Some(&cursor), nonzero_usize(2))
        .await?;
    assert_eq!(ids(&rest.items), vec!["plan-e"]);
    assert!(rest.resume_after.is_none(), "the last page ends the scan");
    Ok(())
}

/// The store admits what [`PlanReach::admits`] does, and a page over a mostly
/// out-of-reach keyspace is still full in one call, so a scoped listing costs
/// one store read a page; [`PlanStore::list_matching_plan_headers`] too.
pub async fn assert_plan_reach_contract(store: &dyn PlanStore) -> anyhow::Result<()> {
    let mine = CampaignId::mint();
    let theirs = CampaignId::mint();
    let me = UserId::mint();
    let other = UserId::mint();
    let reach = PlanReach {
        campaigns: BTreeSet::from([mine.clone()]),
        filer: Some(me.clone()),
    };
    // Every seventh plan is in reach, alternately by campaign and by filer; the
    // rest: another campaign's filed by the caller, another filer's, no filer.
    let mut expected = Vec::new();
    for n in 0..60 {
        let plan_id = PlanId::try_new(format!("plan-{n:03}"))?;
        let (campaign, submitter) = match (n % 7, n % 3) {
            (0, 0) => (Some(&mine), Some(&other)),
            (0, _) => (None, Some(&me)),
            (_, 0) => (Some(&theirs), Some(&me)),
            (_, 1) => (None, Some(&other)),
            _ => (None, None),
        };
        // With a snapshot, and `terminal_at` on every other plan, so each
        // header field is compared populated.
        let status = if n % 2 == 0 {
            PlanStatus::Complete
        } else {
            PlanStatus::Active
        };
        let mut manifest = sample_manifest(plan_id.clone(), status, true)?;
        manifest.campaign_id = campaign.cloned();
        manifest.submitted_by = submitter.cloned();
        store.put_plan(&manifest).await?;
        assert_eq!(
            reach.admits(campaign, submitter),
            n % 7 == 0,
            "the fixture puts plan {n} on the wrong side of the reach"
        );
        if n % 7 == 0 {
            expected.push(plan_id.as_str().to_owned());
        }
    }
    let filter = PlanFilter {
        reach: Some(reach),
        ..PlanFilter::default()
    };

    let drained = drain_matching_plans(store, &filter).await?;
    let ids: Vec<&str> = drained.items.iter().map(|p| p.plan_id.as_str()).collect();
    assert_eq!(ids, expected);

    let mut paged = Vec::new();
    let mut after: Option<PlanId> = None;
    loop {
        let page = store
            .list_matching_plan_headers(&filter, after.as_ref(), nonzero_usize(3))
            .await?;
        for header in &page.items {
            let manifest = store
                .get_plan(&header.plan_id)
                .await?
                .context("a listed plan reads")?;
            assert_eq!(header, &PlanHeader::from(manifest));
        }
        let ids: Vec<String> = page.items.iter().map(|p| p.plan_id.to_string()).collect();
        match page.resume_after {
            Some(cursor) => {
                assert_eq!(ids.len(), 3, "a page that is not the last is full");
                assert_eq!(
                    ids.last(),
                    Some(&cursor.to_string()),
                    "the cursor is the last row"
                );
                paged.extend(ids);
                after = Some(cursor);
            }
            None => {
                paged.extend(ids);
                break;
            }
        }
    }
    assert_eq!(paged, expected);

    // No filer, a campaign reader's reach: the campaign's plans alone, and no
    // plan filed outside a campaign, an unattributed one included.
    let listed = PlanFilter {
        reach: Some(PlanReach {
            campaigns: BTreeSet::from([mine.clone()]),
            filer: None,
        }),
        ..PlanFilter::default()
    };
    let drained = drain_matching_plans(store, &listed).await?;
    let ids: Vec<&str> = drained.items.iter().map(|p| p.plan_id.as_str()).collect();
    let in_campaign: Vec<String> = (0..60)
        .filter(|n| n % 7 == 0 && n % 3 == 0)
        .map(|n| format!("plan-{n:03}"))
        .collect();
    assert_eq!(ids, in_campaign);
    Ok(())
}

/// `write_raw` writes into `clients/` past [`ClientStore::put_client`], as in
/// [`assert_plan_listing_pagination`].
pub async fn assert_client_listing_pagination<F, Fut>(
    store: &dyn ClientStore,
    write_raw: F,
) -> anyhow::Result<()>
where
    F: Fn(&'static str, Vec<u8>) -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<()>>,
{
    use pipette_core::client::ClientStatus;
    use pipette_plan_types::ContactEmail;
    use pipette_plan_types::NonEmptyTrimmedString;

    let one = nonzero_usize(1);
    let two = nonzero_usize(2);
    let four = nonzero_usize(4);
    let plenty = nonzero_usize(50);
    let id = |name: &str| ClientId::try_new(name);
    let ids = |page: &ClientPage| {
        page.items
            .iter()
            .map(|c| c.client_id.as_str().to_string())
            .collect::<Vec<_>>()
    };
    // `registered_at` runs *against* the key order, so a listing that still
    // sorted by it would fail every ordering assertion below.
    let record = |name: &str, nth: u8| -> anyhow::Result<Client> {
        Ok(Client {
            client_id: ClientId::try_new(name)?,
            public_key: PublicKeyHex::try_new(format!("{:02x}", nth).repeat(32))?,
            organization: NonEmptyTrimmedString::try_new("org")?,
            client_details: NonEmptyTrimmedString::try_new("details")?,
            contact_email: ContactEmail::try_new("a@b.com")?,
            status: ClientStatus::Approved,
            registered_at: DateTime::from_timestamp(1_800_000_000 - i64::from(nth), 0)
                .unwrap_or_else(Utc::now),
            device_profile: Default::default(),
            capabilities: Default::default(),
        })
    };

    // An empty keyspace pages to nothing, and says the scan reached the end.
    let empty = store.list_clients(None, plenty).await?;
    assert!(empty.items.is_empty());
    assert!(empty.unreadable.is_empty());
    assert!(
        empty.resume_after.is_none(),
        "an empty keyspace ends the scan rather than inviting another page"
    );

    // Written out of key order; the prefix pair proves paging by the complete
    // object key, `.json` suffix included.
    for (nth, name) in (0_u8..).zip(["ev1_d", "ev1_a", "ev1_e", "ev1_b", "ev1_a-1", "ev1_c"]) {
        store.put_client(&record(name, nth)?).await?;
    }

    // Read together, the records asked for come back, a repeated one once, and
    // an absent one has no entry.
    let mut together: Vec<String> = store
        .get_clients(&[id("ev1_e")?, id("ev1_nobody")?, id("ev1_a")?, id("ev1_e")?])
        .await?
        .into_iter()
        .map(|client| client.client_id.to_string())
        .collect();
    together.sort();
    assert_eq!(together, vec!["ev1_a", "ev1_e"]);
    assert!(store.get_clients(&[]).await?.is_empty());

    // A drain at the smallest page yields every record once, ascending, and ends.
    let mut drained = Vec::new();
    let mut after: Option<ClientId> = None;
    loop {
        let page = store.list_clients(after.as_ref(), one).await?;
        drained.extend(ids(&page));
        match page.resume_after {
            Some(cursor) => after = Some(cursor),
            None => break,
        }
    }
    assert_eq!(
        drained,
        vec!["ev1_a-1", "ev1_a", "ev1_b", "ev1_c", "ev1_d", "ev1_e"],
        "a drain is ascending by key, and reads each record once"
    );

    // A non-final page is exactly `limit` rows and names the key to resume from.
    let page = store.list_clients(None, two).await?;
    assert_eq!(ids(&page), vec!["ev1_a-1", "ev1_a"]);
    assert_eq!(
        page.resume_after.as_ref().map(ClientId::as_str),
        Some("ev1_a")
    );

    // A page that reaches the end says so, whether or not it is full.
    let last = store.list_clients(None, nonzero_usize(6)).await?;
    assert_eq!(last.items.len(), 6);
    assert!(
        last.resume_after.is_none(),
        "a full page that consumed the keyspace still ends the scan"
    );

    // `after` is exclusive: the record it names is not in the next page.
    let page = store.list_clients(Some(&id("ev1_b")?), two).await?;
    assert_eq!(ids(&page), vec!["ev1_c", "ev1_d"]);

    // The same request twice is the same page.
    let again = store.list_clients(Some(&id("ev1_b")?), two).await?;
    assert_eq!(ids(&again), ids(&page));
    assert_eq!(again.resume_after, page.resume_after);

    // A cursor is a lower bound on the key, not a row, so it still resumes
    // after the record it names is deleted.
    store.delete_client(&id("ev1_c")?).await?;
    let page = store.list_clients(Some(&id("ev1_c")?), plenty).await?;
    assert_eq!(ids(&page), vec!["ev1_d", "ev1_e"]);
    store.put_client(&record("ev1_c", 4)?).await?;

    // An unreadable object occupies no row; `ev1_b2` sorts between `ev1_b`
    // and `ev1_c`, so a four-row page has to read past it to fill.
    write_raw("ev1_b2.json", b"{not json".to_vec()).await?;
    let page = store.list_clients(None, four).await?;
    assert_eq!(
        ids(&page),
        vec!["ev1_a-1", "ev1_a", "ev1_b", "ev1_c"],
        "a page reads more objects than it returns rather than returning a short page"
    );
    assert_eq!(page.unreadable.len(), 1);
    let report = page.unreadable.first().context("expected one report")?;
    assert!(
        report.key.contains("ev1_b2"),
        "the report names the object: {report}"
    );

    // A key that names no client is neither a row nor a report; a padded key
    // would otherwise parse, by trimming, as `ev1_a`.
    write_raw("notes.txt", b"not a record".to_vec()).await?;
    write_raw("ev1.bad.json", b"{not json".to_vec()).await?;
    write_raw(" ev1_a.json", b"{not json".to_vec()).await?;
    let listed = store.list_client_ids().await?;
    assert_eq!(
        listed.iter().filter(|id| id.as_str() == "ev1_a").count(),
        1,
        "a padded key is not a second ev1_a: {listed:?}"
    );

    // Across several pages, the broken object is reported exactly once.
    let mut listing = ClientListing::default();
    let mut after = None;
    let mut page_count = 0;
    loop {
        let page = store.list_clients(after.as_ref(), two).await?;
        page_count += 1;
        listing.items.extend(page.items);
        listing.unreadable.extend(page.unreadable);
        match page.resume_after {
            Some(cursor) => after = Some(cursor),
            None => break,
        }
    }
    assert!(page_count > 1, "the assertion must cross a page boundary");
    assert_eq!(
        listing
            .items
            .iter()
            .map(|c| c.client_id.as_str())
            .collect::<Vec<_>>(),
        vec!["ev1_a-1", "ev1_a", "ev1_b", "ev1_c", "ev1_d", "ev1_e"]
    );
    assert_eq!(
        listing.unreadable.len(),
        1,
        "a broken object is reported once per drain, not once per page that reads it"
    );
    Ok(())
}

/// [`assert_user_account_store_roundtrip`]'s properties over organizations:
/// the handle claim is exclusive and names its holder when refused.
pub async fn assert_org_store_roundtrip(store: &dyn OrgStore) -> anyhow::Result<()> {
    use pipette_core::organization::{Membership, Organization, Ownership};
    use pipette_plan_types::ClientOwner;
    use pipette_plan_types::NonEmptyTrimmedString;
    use pipette_plan_types::OrgSlug;
    use pipette_plan_types::OrgStatus;

    let slug = OrgSlug::try_new("acme")?;
    let org = Organization::new(
        slug.clone(),
        NonEmptyTrimmedString::try_new("Acme Corp., Inc.")?,
        Utc::now(),
    );
    let org_id = org.org_id.clone();

    // An organization that does not exist is `None`, not an error.
    assert!(store.get_org(&org_id).await?.is_none());
    assert!(store.list_orgs().await?.is_empty());
    assert!(store.lookup_org_by_slug(slug.as_str()).await?.is_none());

    // Claim, then write — the order every caller owes, so a record never exists
    // for a handle the organization does not hold.
    assert_eq!(
        store.claim_org_slug(slug.as_str(), &org_id).await?,
        SlugClaim::Claimed
    );
    assert_eq!(
        store.lookup_org_by_slug(slug.as_str()).await?,
        Some(org_id.clone())
    );
    store.put_org(&org).await?;

    assert_eq!(
        store.get_org(&org_id).await?,
        Some(org.clone()),
        "the record must survive the store whole"
    );
    assert_eq!(store.list_orgs().await?, vec![org.clone()]);

    // A second organization cannot take the handle, and is told which one has
    // it.
    let other = Organization::new(
        slug.clone(),
        NonEmptyTrimmedString::try_new("Acme Impostor")?,
        Utc::now(),
    );
    assert_eq!(
        store.claim_org_slug(slug.as_str(), &other.org_id).await?,
        SlugClaim::Taken(org_id.clone()),
        "an exclusive create must refuse the loser and name the winner"
    );
    assert_eq!(
        store.get_org(&other.org_id).await?,
        None,
        "losing the claim must leave no record behind"
    );

    // A rename and a disable are whole-record writes that leave the handle,
    // which is why the two are separate fields.
    let renamed = Organization {
        display_name: NonEmptyTrimmedString::try_new("Acme Holdings")?,
        status: OrgStatus::Disabled,
        ..org.clone()
    };
    store.put_org(&renamed).await?;
    assert_eq!(store.get_org(&org_id).await?, Some(renamed));
    assert_eq!(
        store.lookup_org_by_slug(slug.as_str()).await?,
        Some(org_id.clone()),
        "a renamed, disabled organization still holds its handle"
    );

    // Record first, then the claim, so a crash between them strands the handle
    // rather than freeing one an organization still holds.
    store.delete_org(&org_id).await?;
    assert!(store.get_org(&org_id).await?.is_none());
    assert_eq!(
        store.lookup_org_by_slug(slug.as_str()).await?,
        Some(org_id.clone()),
        "the claim outlives the record until it is released"
    );
    store.release_org_slug(slug.as_str()).await?;
    assert!(store.lookup_org_by_slug(slug.as_str()).await?.is_none());
    assert!(store.list_orgs().await?.is_empty());

    // Both deletes are idempotent, which is what lets a partially failed
    // teardown be finished by running it again.
    store.delete_org(&other.org_id).await?;
    store.release_org_slug(slug.as_str()).await?;

    // The handle is free again, so a new organization can take it.
    assert_eq!(
        store.claim_org_slug(slug.as_str(), &other.org_id).await?,
        SlugClaim::Claimed
    );

    // The two relations: replaced whole, deleted idempotently.
    let (alice, bob) = (UserId::mint(), UserId::mint());
    assert!(store.get_membership(&alice).await?.is_none());
    assert!(store.list_memberships().await?.is_empty());
    let membership = |user_id: &UserId, org_id: &OrgId| Membership {
        user_id: user_id.clone(),
        org_id: org_id.clone(),
    };
    store.put_membership(&membership(&alice, &org_id)).await?;
    store
        .put_membership(&membership(&bob, &other.org_id))
        .await?;
    // A move replaces the membership rather than adding a second one.
    store
        .put_membership(&membership(&alice, &other.org_id))
        .await?;
    assert_eq!(
        store.get_membership(&alice).await?,
        Some(other.org_id.clone())
    );
    let mut expected = vec![
        membership(&alice, &other.org_id),
        membership(&bob, &other.org_id),
    ];
    expected.sort_by(|a, b| a.user_id.as_str().cmp(b.user_id.as_str()));
    assert_eq!(store.list_memberships().await?, expected);
    store.delete_membership(&alice).await?;
    store.delete_membership(&alice).await?;
    assert!(store.get_membership(&alice).await?.is_none());

    let rack = ClientId::try_new("ev1_rack")?;
    assert!(store.get_ownership(&rack).await?.is_none());
    let by_org = ClientOwner::Org {
        org_id: other.org_id.clone(),
    };
    store
        .put_ownership(&Ownership {
            client_id: rack.clone(),
            owner: by_org.clone(),
        })
        .await?;
    assert_eq!(store.get_ownership(&rack).await?, Some(by_org));
    let by_user = ClientOwner::User { user_id: bob };
    let handed_over = Ownership {
        client_id: rack.clone(),
        owner: by_user,
    };
    store.put_ownership(&handed_over).await?;
    assert_eq!(store.list_ownerships().await?, vec![handed_over]);
    store.delete_ownership(&rack).await?;
    store.delete_ownership(&rack).await?;
    assert!(store.list_ownerships().await?.is_empty());
    Ok(())
}

/// A [`DatasetCacheStore`] holds a tar and its metadata by digest: absent until
/// written, streamed back byte for byte, metadata replaced in place (the probe
/// rewrites it every minute). `scratch` is a directory for the source tar.
pub async fn assert_dataset_cache_store_roundtrip(
    store: &dyn DatasetCacheStore,
    scratch: &std::path::Path,
) -> anyhow::Result<()> {
    use futures::TryStreamExt;

    let digest = Sha256::try_new("ab".repeat(32))?;
    anyhow::ensure!(store.get_artifact(&digest).await?.is_none());
    anyhow::ensure!(!store.has_tar(&digest).await?);
    anyhow::ensure!(store.open_tar(&digest).await?.is_none());

    // Larger than one read chunk, so the stream has to join several.
    let body: Vec<u8> = (0..700_000_u32).map(|i| (i % 251) as u8).collect();
    let source = scratch.join("source.tar");
    std::fs::write(&source, &body)?;
    store.put_tar(&digest, &source).await?;
    anyhow::ensure!(store.has_tar(&digest).await?);
    let opened = store
        .open_tar(&digest)
        .await?
        .ok_or_else(|| anyhow::anyhow!("a written tar must open"))?;
    anyhow::ensure!(opened.bytes == body.len() as u64);
    let chunks: Vec<bytes::Bytes> = opened.stream.try_collect().await?;
    anyhow::ensure!(
        chunks.concat() == body,
        "the tar must stream back unchanged"
    );

    let now = Utc::now();
    let mut artifact: DatasetArtifact = serde_json::from_value(serde_json::json!({
        "digest": digest,
        "bytes": body.len(),
        "format": "vl-spec-decode-corpus/1",
        "adapter": "mmspec@1",
        "upstream": {
            "source": "git",
            "url": "github.com/acme/corpus",
            "revision": "0123456789abcdef0123456789abcdef01234567",
            "identity": "2923603292c7dd463ad01a119f57965817778fc3",
            "digest": "cd".repeat(32),
        },
        "built_at": now,
        "probe": {"state": "ok", "checked_at": now, "since": now},
    }))?;
    store.put_artifact(&artifact).await?;
    anyhow::ensure!(store.get_artifact(&digest).await?.as_ref() == Some(&artifact));

    artifact.probe = ProbeVerdict::next(&artifact.probe, ProbeState::Moved, now, None, None);
    store.put_artifact(&artifact).await?;
    let reread = store
        .get_artifact(&digest)
        .await?
        .ok_or_else(|| anyhow::anyhow!("metadata must survive a rewrite"))?;
    anyhow::ensure!(reread.probe.state == ProbeState::Moved);
    Ok(())
}

/// A public llama.cpp release member for descriptor catalog contracts.
pub fn a_member(
    version: &str,
    flavor: &str,
) -> anyhow::Result<pipette_plan_types::DescriptorMember> {
    let runtime = serde_json::from_value(serde_json::json!({
        "type": "llamacpp_cli_stock_tools", "source": "github_release",
        "repository_url": "github.com/ggml-org/llama.cpp",
        "repository_version": version, "flavor": flavor,
    }))?;
    Ok(pipette_plan_types::DescriptorMember::new(
        runtime,
        Utc::now(),
    )?)
}

/// A public model member for descriptor catalog contracts: a model of
/// `model_type` from one Hugging Face repository at a pinned commit. A
/// speculative pair takes the repository as its target and its draft.
pub fn a_model_member(
    model_type: &str,
    repo: &str,
    quantization: &str,
) -> anyhow::Result<pipette_plan_types::DescriptorMember> {
    let source = serde_json::json!({
        "source": "huggingface", "org": "acme", "repo_name": repo,
        "revision": "0123456789abcdef0123456789abcdef01234567",
    });
    let mut model = match model_type {
        "mlx_speculative" | "torch_speculative" => {
            serde_json::json!({"target": source.clone(), "draft": source.clone()})
        }
        "gguf_text" => serde_json::json!({"path": "model.gguf"}),
        "gguf_vision" | "gguf_vision_speculative" => {
            serde_json::json!({"model": "model.gguf", "mmproj": "mmproj.gguf"})
        }
        _ => serde_json::json!({}),
    };
    let fields = model.as_object_mut().context("model fields")?;
    fields.insert("type".into(), model_type.into());
    if !fields.contains_key("target") {
        fields.extend(source.as_object().cloned().unwrap_or_default());
    }
    Ok(pipette_plan_types::DescriptorMember::new_model(
        serde_json::from_value(model)?,
        pipette_plan_types::QuantizationLabel::try_new(quantization)?,
        Utc::now(),
    )?)
}

/// One active entry, with the release time in whole seconds.
pub fn a_descriptor_entry(
    name: &str,
    kind: pipette_plan_types::DescriptorEntryKind,
    release_second: i64,
    member: pipette_plan_types::DescriptorMember,
) -> anyhow::Result<pipette_plan_types::DescriptorEntry> {
    use pipette_plan_types::{
        CatalogName, DescriptorEntry, DescriptorEntryCreator, DescriptorEntryState, NonEmptyVec,
    };
    let at = DateTime::<Utc>::from_timestamp(release_second, 0).context("valid release time")?;
    Ok(DescriptorEntry::new(
        pipette_plan_types::CatalogScope::Global,
        CatalogName::try_new(name)?,
        kind,
        DescriptorEntryState::Active,
        at,
        at,
        DescriptorEntryCreator::Operator,
        NonEmptyVec::try_new(vec![member])?,
    )?)
}

/// Shared contract for model entries in SQL descriptor catalog records.
pub async fn assert_descriptor_catalog_model_contract(
    store: &dyn pipette_store::DescriptorCatalogStore,
) -> anyhow::Result<()> {
    use pipette_plan_types::{
        CatalogName, CatalogScope, DescriptorEntryKind as Kind, DescriptorKind,
        DescriptorMemberKey, ModelFormat, ModelType, NonEmptyVec, QuantizationLabel, RuntimeType,
    };
    use pipette_store::{AppendOutcome, CreateEntryOutcome, DescriptorEntryFilter};

    let global = CatalogScope::Global;
    let gguf = a_model_member("gguf_text", "AcmeLM-3B-GGUF", "q4_0")?;
    let model = a_descriptor_entry("AcmeLM-3B", Kind::HandMade, 200, gguf.clone())?;
    let runtime = a_descriptor_entry(
        "llamacpp-b1",
        Kind::Stable,
        100,
        a_member("b1", "linux-x64-rocm")?,
    )?;
    for entry in [&model, &runtime] {
        assert_eq!(
            store.create_entry(entry).await?,
            CreateEntryOutcome::Created
        );
    }
    assert_eq!(
        store.get_entry(&global, &model.name).await?,
        Some(model.clone())
    );

    let mlx = a_model_member("mlx", "AcmeLM-3B-MLX-4bit", "4bit")?;
    assert_eq!(
        store
            .append_members(
                &global,
                &model.name,
                NonEmptyVec::try_new(vec![mlx.clone()])?
            )
            .await?,
        AppendOutcome::Appended(pipette_plan_types::DescriptorEntry {
            members: NonEmptyVec::try_new(vec![gguf.clone(), mlx.clone()])?,
            ..model.clone()
        })
    );
    let q4_0 = QuantizationLabel::try_new("q4_0")?;
    let refusals = [
        (
            &model.name,
            a_model_member("gguf_text", "AcmeLM-3B-Other-GGUF", "q4_0")?,
            AppendOutcome::DuplicateModelKey {
                format: ModelFormat::Gguf,
                quantization: q4_0.clone(),
            },
        ),
        (
            &model.name,
            a_model_member("gguf_vision", "AcmeLM-3B-GGUF", "q8_0")?,
            AppendOutcome::FormatTypeClash {
                format: ModelFormat::Gguf,
                first: ModelType::GgufText,
                second: ModelType::GgufVision,
            },
        ),
        (
            &model.name,
            a_model_member("torch_speculative", "AcmeLM-3B", "bf16")?,
            AppendOutcome::SpeculativeMix,
        ),
        (
            &model.name,
            a_member("b1", "macos-arm64")?,
            AppendOutcome::DescriptorKindMismatch,
        ),
        (
            &runtime.name,
            a_model_member("torch", "AcmeLM-3B", "bf16")?,
            AppendOutcome::DescriptorKindMismatch,
        ),
    ];
    for (name, member, outcome) in refusals {
        assert_eq!(
            store
                .append_members(&global, name, NonEmptyVec::try_new(vec![member])?)
                .await?,
            outcome
        );
    }
    let current = store
        .get_entry(&global, &model.name)
        .await?
        .context("model entry after appends")?;
    assert_eq!(current.members.as_ref(), [gguf.clone(), mlx.clone()]);
    assert_eq!(current.descriptor_types(), ["gguf_text", "mlx"]);

    let mut stable = model.clone();
    stable.name = CatalogName::try_new("AcmeLM-3B-stable")?;
    stable.kind = Kind::Stable;
    assert!(store.create_entry(&stable).await.is_err());

    let global_filter = || DescriptorEntryFilter {
        scopes: [CatalogScope::Global].into(),
        ..Default::default()
    };
    let filters = [
        (
            DescriptorEntryFilter {
                descriptor_kind: Some(DescriptorKind::Runtime),
                ..global_filter()
            },
            vec!["llamacpp-b1"],
        ),
        (
            DescriptorEntryFilter {
                descriptor_kind: Some(DescriptorKind::Model),
                ..global_filter()
            },
            vec!["AcmeLM-3B"],
        ),
        (
            DescriptorEntryFilter {
                model_types: [ModelType::Mlx].into(),
                ..global_filter()
            },
            vec!["AcmeLM-3B"],
        ),
        (
            DescriptorEntryFilter {
                model_types: [ModelType::Torch].into(),
                ..global_filter()
            },
            vec![],
        ),
        (
            DescriptorEntryFilter {
                runtime_types: [RuntimeType::LlamacppCliStockTools].into(),
                ..global_filter()
            },
            vec!["llamacpp-b1"],
        ),
        (
            DescriptorEntryFilter {
                runtime_types: [RuntimeType::LlamacppCliStockTools].into(),
                model_types: [ModelType::GgufText].into(),
                ..global_filter()
            },
            vec!["AcmeLM-3B", "llamacpp-b1"],
        ),
    ];
    for (filter, names) in filters {
        let page = store.list_entries(&filter, None, nonzero_usize(10)).await?;
        assert!(page.unreadable.is_empty(), "{:?}", page.unreadable);
        let found: Vec<&str> = page.items.iter().map(|entry| entry.name.as_str()).collect();
        assert_eq!(found, names, "{filter:?}");
    }

    let hits = store.find_by_digests(&[mlx.digest().to_owned()]).await?;
    let hit = hits
        .get(mlx.digest())
        .and_then(|hits| hits.first())
        .context("model digest hit")?;
    assert_eq!(hit.name, model.name);
    assert_eq!(
        hit.member_key,
        DescriptorMemberKey::Model {
            format: ModelFormat::Mlx,
            quantization: QuantizationLabel::try_new("4bit")?,
        }
    );
    Ok(())
}

/// Shared contract for SQL descriptor catalog records.
pub async fn assert_descriptor_catalog_store_contract(
    store: &dyn pipette_store::DescriptorCatalogStore,
) -> anyhow::Result<()> {
    use pipette_plan_types::{
        CatalogName, CatalogScope, DescriptorEntryKind as Kind, DescriptorEntryState as State,
        NonEmptyVec,
    };
    use pipette_store::{
        AppendOutcome, CatalogEntryKey, CreateEntryOutcome, DescriptorEntryFilter,
        DescriptorEntryPage, SetStateOutcome, TransitionOutcome, UnknownCursorError,
    };

    fn entry_names(page: &DescriptorEntryPage) -> Vec<&str> {
        page.items.iter().map(|entry| entry.name.as_str()).collect()
    }

    let global = CatalogScope::Global;
    let global_filter = || DescriptorEntryFilter {
        scopes: [CatalogScope::Global].into(),
        ..Default::default()
    };

    let first = a_descriptor_entry(
        "Foo-1",
        Kind::Stable,
        100,
        a_member("b1", "linux-x64-rocm")?,
    )?;
    let first_member = first.members.first().context("first member")?.clone();
    assert_eq!(
        store.create_entry(&first).await?,
        CreateEntryOutcome::Created
    );
    assert_eq!(
        store
            .get_entry(&global, &CatalogName::try_new("fOo-1")?)
            .await?,
        Some(first.clone())
    );
    let lower = a_descriptor_entry(
        "foo-1",
        Kind::Stable,
        100,
        a_member("b2", "linux-x64-rocm")?,
    )?;
    assert_eq!(
        store.create_entry(&lower).await?,
        CreateEntryOutcome::NameTaken {
            name: first.name.clone(),
            state: State::Active
        }
    );

    assert_eq!(
        store
            .append_members(
                &global,
                &first.name,
                NonEmptyVec::try_new(vec![first_member.clone()])?
            )
            .await?,
        AppendOutcome::DuplicateFlavor("linux-x64-rocm".into())
    );
    let second = a_member("b1", "macos-arm64")?;
    assert_eq!(
        store
            .append_members(
                &global,
                &first.name,
                NonEmptyVec::try_new(vec![second.clone()])?
            )
            .await?,
        AppendOutcome::Appended(pipette_plan_types::DescriptorEntry {
            members: NonEmptyVec::try_new(vec![first_member.clone(), second.clone()])?,
            ..first.clone()
        })
    );
    let current = store
        .get_entry(&global, &first.name)
        .await?
        .context("entry after append")?;
    assert_eq!(
        current.members.as_ref(),
        [first_member.clone(), second.clone()]
    );
    let batch = [a_member("b1", "custom-a")?, a_member("b1", "custom-b")?];
    assert_eq!(
        store
            .append_members(
                &global,
                &first.name,
                NonEmptyVec::try_new(vec![batch[0].clone(), batch[0].clone()])?
            )
            .await?,
        AppendOutcome::DuplicateFlavor("custom-a".into())
    );
    assert_eq!(
        store
            .append_members(&global, &first.name, NonEmptyVec::try_new(batch.to_vec())?)
            .await?,
        AppendOutcome::Appended(pipette_plan_types::DescriptorEntry {
            members: NonEmptyVec::try_new(vec![
                first_member.clone(),
                second.clone(),
                batch[0].clone(),
                batch[1].clone(),
            ])?,
            ..first.clone()
        })
    );
    let current = store
        .get_entry(&global, &first.name)
        .await?
        .context("entry after batch append")?;
    assert_eq!(
        current.members.as_ref(),
        [
            first_member.clone(),
            second.clone(),
            batch[0].clone(),
            batch[1].clone()
        ]
    );
    assert_eq!(
        store
            .append_members(
                &global,
                &first.name,
                NonEmptyVec::try_new(vec![second.clone()])?
            )
            .await?,
        AppendOutcome::DuplicateFlavor("macos-arm64".into())
    );
    let missing = CatalogName::try_new("missing")?;
    assert_eq!(
        store
            .append_members(
                &global,
                &missing,
                NonEmptyVec::try_new(vec![a_member("b3", "macos-arm64")?])?
            )
            .await?,
        AppendOutcome::NoSuchEntry
    );
    let other_runtime = serde_json::from_value(serde_json::json!({"type":"apple_foundation"}))?;
    let other_member = pipette_plan_types::DescriptorMember::new(other_runtime, Utc::now())?;
    assert_eq!(
        store
            .append_members(
                &global,
                &first.name,
                NonEmptyVec::try_new(vec![other_member])?
            )
            .await?,
        AppendOutcome::RuntimeTypeMismatch
    );

    let daily = a_descriptor_entry("daily-1", Kind::Daily, 300, first_member.clone())?;
    let tie = a_descriptor_entry(
        "another-1",
        Kind::Stable,
        300,
        a_member("b4", "linux-x64-rocm")?,
    )?;
    assert_eq!(
        store.create_entry(&daily).await?,
        CreateEntryOutcome::Created
    );
    assert_eq!(store.create_entry(&tie).await?, CreateEntryOutcome::Created);
    assert!(
        store
            .list_entries(&DescriptorEntryFilter::default(), None, nonzero_usize(10))
            .await?
            .items
            .is_empty()
    );
    let expected = ["another-1", "daily-1", "Foo-1"];
    for size in [1, 2, 10] {
        let mut seen = Vec::new();
        let mut after = None;
        loop {
            let page = store
                .list_entries(&global_filter(), after.as_ref(), nonzero_usize(size))
                .await?;
            assert!(page.unreadable.is_empty(), "{:?}", page.unreadable);
            seen.extend(page.items.into_iter().map(|entry| entry.name.to_string()));
            after = page.resume_after;
            if after.is_none() {
                break;
            }
        }
        assert_eq!(seen, expected);
    }
    let unknown = CatalogEntryKey {
        scope: CatalogScope::Global,
        name: CatalogName::try_new("never-1")?,
    };
    let error = store
        .list_entries(&global_filter(), Some(&unknown), nonzero_usize(10))
        .await
        .err()
        .context("an unknown cursor must be refused")?;
    assert_eq!(
        error
            .downcast_ref::<UnknownCursorError>()
            .map(|e| &e.cursor),
        Some(&unknown),
        "{error:#}"
    );
    let filters = [
        (
            DescriptorEntryFilter {
                kinds: [Kind::Daily].into(),
                ..global_filter()
            },
            vec!["daily-1"],
        ),
        (
            DescriptorEntryFilter {
                states: [State::Active].into(),
                ..global_filter()
            },
            expected.to_vec(),
        ),
        (
            DescriptorEntryFilter {
                runtime_types: [pipette_plan_types::RuntimeType::LlamacppCliStockTools].into(),
                ..global_filter()
            },
            expected.to_vec(),
        ),
        (
            DescriptorEntryFilter {
                runtime_types: [pipette_plan_types::RuntimeType::AppleFoundation].into(),
                ..global_filter()
            },
            vec![],
        ),
        (
            DescriptorEntryFilter {
                name_prefix: Some("DAI".into()),
                ..global_filter()
            },
            vec!["daily-1"],
        ),
    ];
    for (filter, names) in filters {
        let page = store.list_entries(&filter, None, nonzero_usize(10)).await?;
        assert_eq!(entry_names(&page), names);
    }
    let hits = store
        .find_by_digests(&[first_member.digest().to_owned()])
        .await?;
    let mut names = hits
        .get(first_member.digest())
        .context("digest hits")?
        .iter()
        .map(|hit| hit.name.as_str())
        .collect::<Vec<_>>();
    names.sort_unstable();
    assert_eq!(names, ["Foo-1", "daily-1"]);

    let flavorless = pipette_plan_types::DescriptorMember::new(
        serde_json::from_value(serde_json::json!({"type":"apple_foundation"}))?,
        DateTime::<Utc>::from_timestamp(100, 0).context("valid added time")?,
    )?;
    let plain = a_descriptor_entry("plain-1", Kind::HandMade, 50, flavorless.clone())?;
    assert_eq!(
        store.create_entry(&plain).await?,
        CreateEntryOutcome::Created
    );
    assert_eq!(
        store
            .append_members(
                &global,
                &plain.name,
                NonEmptyVec::try_new(vec![flavorless])?
            )
            .await?,
        AppendOutcome::DuplicateDigest(
            plain
                .members
                .first()
                .context("plain member")?
                .digest()
                .to_owned()
        )
    );

    assert_eq!(
        store.set_state(&global, &missing, State::Hidden).await?,
        SetStateOutcome::NoSuchEntry
    );
    assert_eq!(
        store.set_state(&global, &daily.name, State::Active).await?,
        SetStateOutcome::Unchanged
    );
    assert_eq!(
        store.set_state(&global, &daily.name, State::Hidden).await?,
        SetStateOutcome::Changed {
            from: State::Active
        }
    );
    let hidden = store
        .list_entries(
            &DescriptorEntryFilter {
                states: [State::Hidden].into(),
                ..global_filter()
            },
            None,
            nonzero_usize(10),
        )
        .await?;
    assert_eq!(entry_names(&hidden), ["daily-1"]);

    assert_eq!(
        store
            .transition_state(&global, &missing, State::Active, State::Pinned)
            .await?,
        TransitionOutcome::NoSuchEntry
    );
    assert_eq!(
        store
            .transition_state(&global, &daily.name, State::Active, State::Pinned)
            .await?,
        TransitionOutcome::UnexpectedState(State::Hidden)
    );
    assert_eq!(
        store
            .get_entry(&global, &daily.name)
            .await?
            .context("daily entry")?
            .state,
        State::Hidden
    );
    assert_eq!(
        store
            .transition_state(&global, &daily.name, State::Hidden, State::Pinned)
            .await?,
        TransitionOutcome::Changed
    );
    assert_eq!(
        store
            .get_entry(&global, &daily.name)
            .await?
            .context("daily entry")?
            .state,
        State::Pinned
    );
    assert_eq!(
        store
            .transition_state(&global, &daily.name, State::Pinned, State::Hidden)
            .await?,
        TransitionOutcome::Changed
    );

    assert_eq!(
        store
            .set_state(&global, &first.name, State::Tombstoned)
            .await?,
        SetStateOutcome::Changed {
            from: State::Active
        }
    );
    assert_eq!(
        store.set_state(&global, &first.name, State::Active).await?,
        SetStateOutcome::Tombstoned
    );
    assert_eq!(
        store
            .transition_state(&global, &first.name, State::Active, State::Pinned)
            .await?,
        TransitionOutcome::Tombstoned
    );
    assert_eq!(
        store
            .transition_state(&global, &first.name, State::Tombstoned, State::Active)
            .await?,
        TransitionOutcome::Tombstoned
    );
    assert_eq!(
        store
            .append_members(
                &global,
                &first.name,
                NonEmptyVec::try_new(vec![a_member("b5", "windows-x64")?])?
            )
            .await?,
        AppendOutcome::Tombstoned
    );
    let upper = a_descriptor_entry("FOO-1", Kind::Daily, 400, a_member("b6", "linux-x64-rocm")?)?;
    assert_eq!(
        store
            .get_entry(&global, &first.name)
            .await?
            .context("tombstoned entry")?
            .state,
        State::Tombstoned
    );
    let hits = store
        .find_by_digests(&[first_member.digest().to_owned()])
        .await?;
    assert!(
        hits.get(first_member.digest())
            .context("tombstoned digest hit")?
            .iter()
            .any(|hit| hit.name == first.name && hit.state == State::Tombstoned)
    );
    assert_eq!(
        store.create_entry(&upper).await?,
        CreateEntryOutcome::NameTaken {
            name: first.name.clone(),
            state: State::Tombstoned
        }
    );

    let org_a = CatalogScope::Org(pipette_plan_types::OrgId::try_new("org_a")?);
    let org_b = CatalogScope::Org(pipette_plan_types::OrgId::try_new("org_b")?);
    let mut in_a = first.clone();
    in_a.scope = org_a.clone();
    in_a.kind = Kind::HandMade;
    in_a.state = State::Active;
    let mut in_b = in_a.clone();
    in_b.scope = org_b.clone();
    assert_eq!(
        store.create_entry(&in_a).await?,
        CreateEntryOutcome::Created
    );
    assert_eq!(
        store.create_entry(&in_b).await?,
        CreateEntryOutcome::Created
    );
    assert_eq!(
        store.get_entry(&org_a, &in_a.name).await?,
        Some(in_a.clone())
    );
    assert_eq!(
        store.get_entry(&org_b, &in_b.name).await?,
        Some(in_b.clone())
    );
    let mut duplicate = in_a.clone();
    duplicate.name = CatalogName::try_new("fOO-1")?;
    assert_eq!(
        store.create_entry(&duplicate).await?,
        CreateEntryOutcome::NameTaken {
            name: in_a.name.clone(),
            state: State::Active,
        }
    );
    let mut invalid = in_a.clone();
    invalid.name = CatalogName::try_new("invalid-kind")?;
    invalid.kind = Kind::Daily;
    assert!(store.create_entry(&invalid).await.is_err());
    invalid.kind = Kind::HandMade;
    invalid.created_by = pipette_plan_types::DescriptorEntryCreator::ImporterLlamacpp;
    assert!(store.create_entry(&invalid).await.is_err());

    let scopes: std::collections::BTreeSet<_> =
        [global.clone(), org_a.clone(), org_b.clone()].into();
    let mut after = None;
    let mut seen = Vec::new();
    loop {
        let page = store
            .list_entries(
                &DescriptorEntryFilter {
                    scopes: scopes.clone(),
                    ..Default::default()
                },
                after.as_ref(),
                nonzero_usize(1),
            )
            .await?;
        seen.extend(
            page.items
                .into_iter()
                .map(|entry| (entry.scope, entry.name.key())),
        );
        after = page.resume_after;
        if after.is_none() {
            break;
        }
    }
    [&global, &org_a, &org_b].into_iter().for_each(|scope| {
        assert!(seen.contains(&(scope.clone(), first.name.key())));
    });
    assert_eq!(seen.len(), 6);
    let hits = store
        .find_by_digests(&[first_member.digest().to_owned()])
        .await?;
    let hit_scopes = hits
        .get(first_member.digest())
        .context("scoped digest hits")?
        .iter()
        .filter(|hit| hit.name.key() == first.name.key())
        .map(|hit| hit.scope.clone())
        .collect::<Vec<_>>();
    [&global, &org_a, &org_b].into_iter().for_each(|scope| {
        assert!(hit_scopes.contains(scope));
    });

    assert_eq!(
        store
            .set_state(&org_b, &in_b.name, State::Tombstoned)
            .await?,
        SetStateOutcome::Changed {
            from: State::Active
        }
    );
    assert_eq!(
        store.create_entry(&in_b).await?,
        CreateEntryOutcome::NameTaken {
            name: in_b.name.clone(),
            state: State::Tombstoned
        }
    );
    assert_eq!(
        store
            .live_entries_in_org(org_b.org_id().context("org b")?)
            .await?,
        Vec::<CatalogName>::new()
    );
    assert_eq!(
        store
            .live_entries_in_org(org_a.org_id().context("org a")?)
            .await?,
        vec![in_a.name.clone()]
    );
    store
        .delete_tombstones_in_org(org_b.org_id().context("org b")?)
        .await?;
    assert!(store.get_entry(&org_b, &in_b.name).await?.is_none());
    assert!(store.get_entry(&org_a, &in_a.name).await?.is_some());
    Ok(())
}
