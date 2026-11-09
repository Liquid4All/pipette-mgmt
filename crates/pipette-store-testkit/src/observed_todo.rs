use std::collections::HashSet;
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};

use pipette_core::denial::DenialRecord;
use pipette_plan_types::{ClientId, ExpiresAt, JobId};
use pipette_store::{
    BacklogSnapshot, CampaignBacklogSnapshot, ClaimOutcome, ClaimResult, ReclaimOutcome,
    RecycleResult, RedateResult, ReindexBatch, RenewLeaseResult, TodoStore,
};

use crate::UnexpectedCalls;

/// A wrapper around a queue store for tests. It records each read for named
/// jobs. It fails each read of the whole availability, lease, or denial listing.
/// A test queues lease results with [`Self::script_leases`] to make a race occur.
pub struct ObservedTodoStore {
    inner: std::sync::Arc<dyn TodoStore>,
    pub unexpected: UnexpectedCalls,
    reads: std::sync::Mutex<Vec<(&'static str, Vec<JobId>)>>,
    lease_script:
        std::sync::Mutex<std::collections::VecDeque<Vec<pipette_core::todo_filename::Lease>>>,
    allow_listings: bool,
    avail_barrier: Option<std::sync::Arc<tokio::sync::Barrier>>,
}

impl ObservedTodoStore {
    pub fn new(inner: std::sync::Arc<dyn TodoStore>) -> Self {
        Self {
            inner,
            unexpected: UnexpectedCalls::default(),
            reads: Default::default(),
            lease_script: Default::default(),
            avail_barrier: None,
            allow_listings: false,
        }
    }
    /// Record reads during a full maintenance run, which requires global listings.
    pub fn with_global_listings(mut self) -> Self {
        self.allow_listings = true;
        self
    }
    /// Wraps `inner` for a test that swaps the result into its store field.
    pub fn wrap(inner: &std::sync::Arc<dyn TodoStore>) -> std::sync::Arc<Self> {
        std::sync::Arc::new(Self::new(inner.clone()))
    }
    /// Hold each targeted availability read until the other participant arrives.
    pub fn with_avail_barrier(mut self, barrier: std::sync::Arc<tokio::sync::Barrier>) -> Self {
        self.avail_barrier = Some(barrier);
        self
    }
    /// Queues `leases` as the reply to the next `leases_for_jobs` read.
    pub fn script_leases(&self, leases: Vec<pipette_core::todo_filename::Lease>) {
        self.lease_script
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push_back(leases);
    }
    fn record(&self, method: &'static str, jobs: &[JobId]) {
        self.reads
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push((method, jobs.to_vec()));
    }
    pub fn take_reads(&self) -> Vec<(&'static str, Vec<JobId>)> {
        std::mem::take(
            &mut *self
                .reads
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        )
    }
}

#[async_trait]
impl TodoStore for ObservedTodoStore {
    fn queue_body_reads(&self) -> u64 {
        self.inner.queue_body_reads()
    }

    async fn list_avail(&self) -> anyhow::Result<Vec<String>> {
        if self.allow_listings {
            return self.inner.list_avail().await;
        }
        Err(self.unexpected.record("list_avail"))
    }
    async fn get_avail(
        &self,
        job_id: &JobId,
        expires_at: ExpiresAt,
    ) -> anyhow::Result<Option<serde_json::Value>> {
        self.record("get_avail", std::slice::from_ref(job_id));
        self.inner.get_avail(job_id, expires_at).await
    }
    async fn delete_avail(&self, job_id: &JobId, expires_at: ExpiresAt) -> anyhow::Result<()> {
        self.inner.delete_avail(job_id, expires_at).await
    }
    async fn delete_avail_by_job(&self, job_id: &JobId) -> anyhow::Result<()> {
        self.inner.delete_avail_by_job(job_id).await
    }
    async fn get_avail_by_job(&self, job_id: &JobId) -> anyhow::Result<Option<serde_json::Value>> {
        self.inner.get_avail_by_job(job_id).await
    }
    async fn avail_key_for_job(&self, job_id: &JobId) -> anyhow::Result<Option<String>> {
        self.inner.avail_key_for_job(job_id).await
    }
    async fn redate_avail(
        &self,
        job_id: &JobId,
        from: ExpiresAt,
        to: ExpiresAt,
    ) -> anyhow::Result<RedateResult> {
        self.inner.redate_avail(job_id, from, to).await
    }
    async fn list_leased(&self) -> anyhow::Result<Vec<String>> {
        if self.allow_listings {
            return self.inner.list_leased().await;
        }
        Err(self.unexpected.record("list_leased"))
    }
    async fn list_leased_for_client(&self, client_id: &ClientId) -> anyhow::Result<Vec<String>> {
        self.inner.list_leased_for_client(client_id).await
    }
    async fn get_leased(
        &self,
        job_id: &JobId,
        expires_at: ExpiresAt,
        client_id: &ClientId,
        lease_expiry: DateTime<Utc>,
    ) -> anyhow::Result<Option<serde_json::Value>> {
        self.inner
            .get_leased(job_id, expires_at, client_id, lease_expiry)
            .await
    }
    async fn renew_lease(
        &self,
        job_id: &JobId,
        client_id: &ClientId,
        new_expiry: DateTime<Utc>,
    ) -> anyhow::Result<RenewLeaseResult> {
        self.inner.renew_lease(job_id, client_id, new_expiry).await
    }
    async fn delete_lease(
        &self,
        job_id: &JobId,
        expires_at: ExpiresAt,
        client_id: &ClientId,
        expiry: DateTime<Utc>,
    ) -> anyhow::Result<()> {
        self.inner
            .delete_lease(job_id, expires_at, client_id, expiry)
            .await
    }
    async fn claim_job(
        &self,
        job_id: &JobId,
        expires_at: ExpiresAt,
        client_id: &ClientId,
        lease_expiry: DateTime<Utc>,
    ) -> anyhow::Result<ClaimResult> {
        self.inner
            .claim_job(job_id, expires_at, client_id, lease_expiry)
            .await
    }
    async fn claim_for_client(
        &self,
        client_id: &ClientId,
        lease_expiry: DateTime<Utc>,
        lost_lease_limit: usize,
        passed_over: &[JobId],
        checked_lease: Option<&pipette_core::todo_filename::Lease>,
        now: DateTime<Utc>,
    ) -> anyhow::Result<ClaimOutcome> {
        self.inner
            .claim_for_client(
                client_id,
                lease_expiry,
                lost_lease_limit,
                passed_over,
                checked_lease,
                now,
            )
            .await
    }
    async fn reclaim_for_client(
        &self,
        job_id: &JobId,
        client_id: &ClientId,
        lease_expiry: DateTime<Utc>,
        lost_lease_limit: usize,
        now: DateTime<Utc>,
    ) -> anyhow::Result<ReclaimOutcome> {
        self.inner
            .reclaim_for_client(job_id, client_id, lease_expiry, lost_lease_limit, now)
            .await
    }
    async fn recycle_lease(
        &self,
        job_id: &JobId,
        expires_at: ExpiresAt,
        client_id: &ClientId,
        lease_expiry: DateTime<Utc>,
    ) -> anyhow::Result<RecycleResult> {
        self.inner
            .recycle_lease(job_id, expires_at, client_id, lease_expiry)
            .await
    }
    async fn write_denied(
        &self,
        job_id: &JobId,
        client_id: &ClientId,
        record: &DenialRecord,
    ) -> anyhow::Result<()> {
        self.inner.write_denied(job_id, client_id, record).await
    }
    async fn get_denied(
        &self,
        job_id: &JobId,
        client_id: &ClientId,
    ) -> anyhow::Result<Option<DenialRecord>> {
        self.inner.get_denied(job_id, client_id).await
    }
    async fn is_denied(&self, job_id: &JobId, client_id: &ClientId) -> anyhow::Result<bool> {
        self.inner.is_denied(job_id, client_id).await
    }
    async fn delete_denied_for_job(&self, job_id: &JobId) -> anyhow::Result<()> {
        self.inner.delete_denied_for_job(job_id).await
    }
    async fn delete_denied(&self, job_id: &JobId, client_id: &ClientId) -> anyhow::Result<()> {
        self.inner.delete_denied(job_id, client_id).await
    }
    async fn list_all_denied(&self) -> anyhow::Result<Vec<(JobId, ClientId)>> {
        if self.allow_listings {
            return self.inner.list_all_denied().await;
        }
        Err(self.unexpected.record("list_all_denied"))
    }
    async fn list_eligible_for_client(
        &self,
        client_id: &ClientId,
    ) -> anyhow::Result<Vec<(JobId, ExpiresAt)>> {
        self.inner.list_eligible_for_client(client_id).await
    }
    async fn write_eligible(
        &self,
        client_id: &ClientId,
        job_id: &JobId,
        expires_at: ExpiresAt,
    ) -> anyhow::Result<()> {
        self.inner
            .write_eligible(client_id, job_id, expires_at)
            .await
    }
    async fn delete_eligible_for_client(&self, client_id: &ClientId) -> anyhow::Result<()> {
        self.inner.delete_eligible_for_client(client_id).await
    }
    async fn delete_eligible(
        &self,
        client_id: &ClientId,
        job_id: &JobId,
        expires_at: ExpiresAt,
    ) -> anyhow::Result<()> {
        self.inner
            .delete_eligible(client_id, job_id, expires_at)
            .await
    }
    async fn list_all_eligible(&self) -> anyhow::Result<Vec<(ClientId, JobId, ExpiresAt)>> {
        self.inner.list_all_eligible().await
    }
    async fn write_pending_reindex(&self, client_id: &ClientId) -> anyhow::Result<()> {
        self.inner.write_pending_reindex(client_id).await
    }
    async fn list_pending_reindex(&self) -> anyhow::Result<Vec<(ClientId, String)>> {
        self.inner.list_pending_reindex().await
    }
    async fn delete_pending_reindex(&self, key: &str) -> anyhow::Result<()> {
        self.inner.delete_pending_reindex(key).await
    }
    async fn has_pending_reindex(&self, client_id: &ClientId) -> anyhow::Result<bool> {
        self.inner.has_pending_reindex(client_id).await
    }
    async fn write_lost_lease(
        &self,
        job_id: &JobId,
        client_id: &ClientId,
        lease_expiry: DateTime<Utc>,
    ) -> anyhow::Result<()> {
        self.inner
            .write_lost_lease(job_id, client_id, lease_expiry)
            .await
    }
    async fn list_lost_leases(
        &self,
        job_id: &JobId,
    ) -> anyhow::Result<Vec<(ClientId, DateTime<Utc>)>> {
        self.inner.list_lost_leases(job_id).await
    }
    async fn delete_lost_leases_for_job(&self, job_id: &JobId) -> anyhow::Result<()> {
        self.inner.delete_lost_leases_for_job(job_id).await
    }
    async fn list_lost_lease_jobs(&self) -> anyhow::Result<Vec<JobId>> {
        self.inner.list_lost_lease_jobs().await
    }
    async fn write_tmp(&self, job_id: &JobId, body: &serde_json::Value) -> anyhow::Result<()> {
        self.inner.write_tmp(job_id, body).await
    }
    async fn promote_avail(&self, job_id: &JobId, expires_at: ExpiresAt) -> anyhow::Result<()> {
        self.inner.promote_avail(job_id, expires_at).await
    }
    async fn list_stale_tmp(&self, age: Duration) -> anyhow::Result<Vec<String>> {
        self.inner.list_stale_tmp(age).await
    }
    async fn delete_tmp_object(&self, key: &str) -> anyhow::Result<()> {
        self.inner.delete_tmp_object(key).await
    }
    async fn read_backlog(&self) -> anyhow::Result<Option<BacklogSnapshot>> {
        self.inner.read_backlog().await
    }
    async fn write_backlog(&self, snapshot: &BacklogSnapshot) -> anyhow::Result<()> {
        self.inner.write_backlog(snapshot).await
    }
    async fn read_campaign_backlog(&self) -> anyhow::Result<Option<CampaignBacklogSnapshot>> {
        self.inner.read_campaign_backlog().await
    }
    async fn write_campaign_backlog(
        &self,
        snapshot: &CampaignBacklogSnapshot,
    ) -> anyhow::Result<()> {
        self.inner.write_campaign_backlog(snapshot).await
    }
    async fn jobs_needing_reindex(&self, jobs: &[JobId]) -> anyhow::Result<ReindexBatch> {
        // Requests include failed batches and jobs whose bodies were not read.
        self.record("jobs_needing_reindex_requested", jobs);
        let flagged = self.inner.jobs_needing_reindex(jobs).await?;
        self.record(
            "jobs_needing_reindex",
            &flagged
                .jobs
                .iter()
                .map(|job| job.job_id.clone())
                .collect::<Vec<_>>(),
        );
        Ok(flagged)
    }
    async fn avail_clause_groups(
        &self,
        jobs: &[JobId],
    ) -> anyhow::Result<Vec<(serde_json::Value, Vec<JobId>)>> {
        self.record("avail_clause_groups", jobs);
        self.inner.avail_clause_groups(jobs).await
    }
    async fn write_eligible_rows(&self, client: &ClientId, jobs: &[JobId]) -> anyhow::Result<()> {
        self.inner.write_eligible_rows(client, jobs).await
    }
    async fn replace_eligibility(
        &self,
        job_id: &JobId,
        revision: i64,
        clients: &[ClientId],
    ) -> anyhow::Result<bool> {
        self.inner
            .replace_eligibility(job_id, revision, clients)
            .await
    }
    async fn flag_for_reindex(&self, jobs: &[JobId]) -> anyhow::Result<()> {
        self.inner.flag_for_reindex(jobs).await
    }
    async fn jobs_awaiting_index(&self, jobs: &[JobId]) -> anyhow::Result<HashSet<JobId>> {
        self.record("jobs_awaiting_index", jobs);
        self.inner.jobs_awaiting_index(jobs).await
    }
    async fn read_workers(&self) -> anyhow::Result<HashSet<ClientId>> {
        self.inner.read_workers().await
    }
    async fn write_workers(&self, workers: &HashSet<ClientId>) -> anyhow::Result<()> {
        self.inner.write_workers(workers).await
    }
    async fn read_gc_candidates(&self) -> anyhow::Result<HashSet<String>> {
        self.inner.read_gc_candidates().await
    }
    async fn write_gc_candidates(&self, candidates: &HashSet<String>) -> anyhow::Result<()> {
        self.inner.write_gc_candidates(candidates).await
    }
    async fn stamp_liveness(&self, client_id: &ClientId) -> anyhow::Result<()> {
        self.inner.stamp_liveness(client_id).await
    }
    async fn read_liveness(&self, client_id: &ClientId) -> anyhow::Result<Option<DateTime<Utc>>> {
        self.inner.read_liveness(client_id).await
    }
    async fn list_liveness(&self) -> anyhow::Result<Vec<(ClientId, DateTime<Utc>)>> {
        self.inner.list_liveness().await
    }
    async fn delete_liveness(&self, client_id: &ClientId) -> anyhow::Result<()> {
        self.inner.delete_liveness(client_id).await
    }
    async fn validate_backend(&self) -> anyhow::Result<()> {
        self.inner.validate_backend().await
    }
    async fn avail_for_jobs(
        &self,
        jobs: &[JobId],
    ) -> anyhow::Result<Vec<pipette_store::AvailEntry>> {
        self.record("avail_for_jobs", jobs);
        if let Some(barrier) = &self.avail_barrier {
            barrier.wait().await;
        }
        self.inner.avail_for_jobs(jobs).await
    }
    async fn leases_for_jobs(
        &self,
        jobs: &[JobId],
    ) -> anyhow::Result<Vec<pipette_core::todo_filename::Lease>> {
        self.record("leases_for_jobs", jobs);
        let scripted = self
            .lease_script
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .pop_front();
        if let Some(leases) = scripted {
            anyhow::ensure!(
                leases.iter().all(|lease| jobs.contains(&lease.job_id)),
                "scripted leases name a job this leases_for_jobs read did not ask for"
            );
            return Ok(leases);
        }
        self.inner.leases_for_jobs(jobs).await
    }
    async fn denials_for_jobs(&self, jobs: &[JobId]) -> anyhow::Result<Vec<(JobId, ClientId)>> {
        self.record("denials_for_jobs", jobs);
        self.inner.denials_for_jobs(jobs).await
    }
    async fn leases_for_client(
        &self,
        client: &ClientId,
    ) -> anyhow::Result<Vec<pipette_core::todo_filename::Lease>> {
        self.record("leases_for_client", &[]);
        self.inner.leases_for_client(client).await
    }
}
