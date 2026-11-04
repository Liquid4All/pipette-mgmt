//! Copies the job queue from one [`TodoStore`] to another, so that the target
//! equals the source. `queue import` copies the object-store queue into
//! `[database]`, and `queue export` copies it back. `docs/cli.md` lists what
//! each keyspace copies.

use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::time::{Duration, Instant};

use chrono::{DateTime, SubsecRound, Utc};
use futures::stream::{self, StreamExt, TryStreamExt};
use serde_json::Value;

use pipette_core::denial::DenialRecord;
use pipette_core::todo_filename::{
    avail_filename, leased_key, lost_lease_filename, parse_avail_filename, parse_leased_key,
};
use pipette_plan_types::{ClientId, ExpiresAt, JobId};
use pipette_store::{
    ClaimResult, LivenessImport, STORAGE_CONCURRENCY, SuspensionRecord, TodoStore,
    reject_unmigrated_avail_keys,
};

use crate::record_copy::{KeyspaceReport, LISTING_SKIPPED, Outcome, unreadable};

const DUPLICATE: &str = "the source holds the entry more than once; the copy keeps the latest";
const VANISHED: &str = "the source entry vanished during the copy; the target's copy stays";
const STAGED: &str = "a staged body that ingestion has not promoted; the copy leaves it";
const NO_SNAPSHOT_DELETE: &str =
    "the source has no snapshot, and no store method deletes the target's snapshot";

/// The result of [`copy_queue`]. It holds one entry for each keyspace, in the
/// order that the copy ran.
#[derive(Debug)]
pub struct QueueCopyReport {
    pub dry_run: bool,
    pub keyspaces: Vec<TimedKeyspace>,
    pub elapsed: Duration,
}

#[derive(Debug)]
pub struct TimedKeyspace {
    pub report: KeyspaceReport,
    pub elapsed: Duration,
}

impl QueueCopyReport {
    pub fn written(&self) -> usize {
        self.keyspaces.iter().map(|k| k.report.written).sum()
    }

    pub fn deleted(&self) -> usize {
        self.keyspaces.iter().map(|k| k.report.deleted).sum()
    }

    pub fn unreadable(&self) -> usize {
        self.keyspaces
            .iter()
            .map(|k| k.report.unreadable.len())
            .sum()
    }
}

/// Makes `to` equal to `from`: it writes each entry that differs and deletes
/// each entry that `from` does not hold. It takes no lock, so stop the queue
/// writers for an exact copy. It writes one entry at a time through the trait,
/// [`STORAGE_CONCURRENCY`] entries at once.
///
/// With `liveness`, each copied last-seen time keeps the source's instant.
/// Without it, the target stamps its own clock on each device that it lacks.
#[tracing::instrument(skip_all, fields(dry_run))]
pub async fn copy_queue(
    from: &dyn TodoStore,
    to: &dyn TodoStore,
    liveness: Option<&dyn LivenessImport>,
    dry_run: bool,
) -> anyhow::Result<QueueCopyReport> {
    let started = Instant::now();
    let copy = QueueCopy { from, to, dry_run };
    let mut keyspaces = copy.jobs().await?;
    keyspaces.push(timed(copy.denied()).await?);
    keyspaces.push(timed(copy.eligible()).await?);
    keyspaces.push(timed(copy.lost()).await?);
    keyspaces.push(timed(copy.pending_reindex()).await?);
    keyspaces.push(timed(copy.pending_reindex_jobs()).await?);
    keyspaces.push(timed(copy.suspended()).await?);
    keyspaces.push(timed(copy.liveness(liveness)).await?);
    keyspaces.push(timed(copy.tmp()).await?);
    keyspaces.push(timed(copy.run_state()).await?);
    let report = QueueCopyReport {
        dry_run,
        keyspaces,
        elapsed: started.elapsed(),
    };
    tracing::info!(
        written = report.written(),
        deleted = report.deleted(),
        unreadable = report.unreadable(),
        elapsed_ms = millis(report.elapsed),
        "queue copy finished"
    );
    Ok(report)
}

#[derive(Clone, Copy)]
struct QueueCopy<'a> {
    from: &'a dyn TodoStore,
    to: &'a dyn TodoStore,
    dry_run: bool,
}

impl QueueCopy<'_> {
    /// `avail/` and `leased/` share one row of a job in SQL. So the copy reads
    /// both listings first, keeps one entry of each job, and runs every delete
    /// before the first write.
    async fn jobs(self) -> anyhow::Result<Vec<TimedKeyspace>> {
        let mut avail = Side::new("avail");
        let mut leased = Side::new("leased");

        let (source_avail, target_avail) = avail
            .time(async {
                let source = self.from.list_avail().await?;
                reject_unmigrated_avail_keys(source.iter().map(String::as_str))?;
                anyhow::Ok((source, self.to.list_avail().await?))
            })
            .await?;
        let (source_leased, target_leased) = leased
            .time(async {
                anyhow::Ok((self.from.list_leased().await?, self.to.list_leased().await?))
            })
            .await?;

        let (source, unparsed) = parse_entries(source_avail, source_leased);
        unparsed.into_iter().for_each(|bad| {
            let side = pick(&mut avail, &mut leased, bad.kind);
            side.report.record(bad.key, Outcome::Skipped(bad.reason));
        });
        let (target, unparsed) = parse_entries(target_avail, target_leased);
        unparsed.iter().for_each(|bad| {
            tracing::warn!(key = %bad.key, reason = %bad.reason, "queue copy cannot address a target key");
        });

        let mut kept = BTreeMap::new();
        source.into_iter().for_each(|(job, mut entries)| {
            entries.sort();
            let Some(winner) = entries.pop() else {
                return;
            };
            entries.into_iter().for_each(|dropped| {
                tracing::warn!(
                    job_id = %job,
                    kept = %winner.key(&job),
                    dropped = %dropped.key(&job),
                    "queue copy dropped a duplicate entry for one job"
                );
                pick(&mut avail, &mut leased, dropped.kind())
                    .report
                    .record(dropped.key(&job), Outcome::Skipped(DUPLICATE.to_owned()));
            });
            kept.insert(job, winner);
        });

        let mut writes = Vec::new();
        let mut deletes = Vec::new();
        let mut pinned = BTreeSet::new();
        for kind in [Kind::Avail, Kind::Leased] {
            let side = pick(&mut avail, &mut leased, kind);
            let items = kept.iter().filter(|(_, entry)| entry.kind() == kind);
            let reads = side
                .time(each(items, |(job, entry)| {
                    let target = &target;
                    async move {
                        let held = target.get(job).is_some_and(|held| held.contains(entry));
                        Read {
                            key: (job.clone(), entry.clone()),
                            source: entry.read(self.from, job).await,
                            target: if held {
                                Some(entry.read(self.to, job).await)
                            } else {
                                None
                            },
                        }
                    }
                }))
                .await;
            let (written, stuck) = decide(
                &mut side.report,
                reads,
                |(job, entry)| entry.key(job),
                |a, b| a == b,
            );
            written.into_iter().for_each(|write| {
                if write.replaces {
                    deletes.push(write.key.clone());
                }
                writes.push((write.key, write.value));
            });
            pinned.extend(stuck.into_iter().map(|(job, _)| job));
        }

        // A target entry of a job that the source keeps differently goes. One
        // of a job that the source's listing lacks goes once a read confirms it.
        let mut unlisted = Vec::new();
        target
            .into_iter()
            .filter(|(job, _)| !pinned.contains(job))
            .flat_map(|(job, entries)| entries.into_iter().map(move |e| (job.clone(), e)))
            .for_each(|(job, entry)| match kept.get(&job) {
                Some(winner) if *winner == entry => {}
                Some(_) => {
                    pick(&mut avail, &mut leased, entry.kind())
                        .report
                        .record(entry.key(&job), Outcome::Deleted);
                    deletes.push((job, entry));
                }
                None => unlisted.push((job, entry)),
            });
        for kind in [Kind::Avail, Kind::Leased] {
            let side = pick(&mut avail, &mut leased, kind);
            let items = unlisted.iter().filter(|(_, entry)| entry.kind() == kind);
            let reads = side
                .time(each(items, |(job, entry)| async move {
                    let held = entry.read(self.from, job).await.map(|body| body.is_some());
                    ((job.clone(), entry.clone()), held)
                }))
                .await;
            deletes.extend(gone(&mut side.report, reads, |(job, entry)| entry.key(job)));
        }

        for kind in [Kind::Avail, Kind::Leased] {
            let side = pick(&mut avail, &mut leased, kind);
            let items = deletes.iter().filter(|(_, entry)| entry.kind() == kind);
            side.time(apply(self.dry_run, items, |(job, entry)| {
                entry.delete(self.to, job)
            }))
            .await?;
        }
        for kind in [Kind::Avail, Kind::Leased] {
            let side = pick(&mut avail, &mut leased, kind);
            let items = writes.iter().filter(|((_, entry), _)| entry.kind() == kind);
            side.time(apply(self.dry_run, items, |((job, entry), body)| {
                entry.write(self.to, job, body)
            }))
            .await?;
        }
        Ok(vec![avail.finish(), leased.finish()])
    }

    async fn denied(self) -> anyhow::Result<KeyspaceReport> {
        let mut report = KeyspaceReport::new("denied");
        let source = self.from.list_all_denied().await?;
        let target: BTreeSet<(JobId, ClientId)> =
            self.to.list_all_denied().await?.into_iter().collect();
        let listed: BTreeSet<(JobId, ClientId)> = source.iter().cloned().collect();
        let id = |(job, client): &(JobId, ClientId)| format!("{job}.{client}");

        let reads = each(source, |(job, client)| {
            let target = &target;
            async move {
                let held = target.contains(&(job.clone(), client.clone()));
                Read {
                    source: self.from.get_denied(&job, &client).await,
                    target: if held {
                        Some(self.to.get_denied(&job, &client).await)
                    } else {
                        None
                    },
                    key: (job, client),
                }
            }
        })
        .await;
        let (writes, _) = decide(&mut report, reads, id, same_denial);

        let unlisted = target.difference(&listed);
        let reads = each(unlisted, |(job, client)| async move {
            let held = self.from.is_denied(job, client).await;
            ((job.clone(), client.clone()), held)
        })
        .await;
        let deletes = gone(&mut report, reads, id);

        apply(self.dry_run, deletes, |(job, client)| async move {
            self.to.delete_denied(&job, &client).await
        })
        .await?;
        apply(self.dry_run, writes, |write| async move {
            let (job, client) = &write.key;
            self.to.write_denied(job, client, &write.value).await
        })
        .await?;
        Ok(report)
    }

    /// The SQL index holds one marker for each client and job. So, as for a
    /// job, the copy keeps the latest deadline of a client and job.
    async fn eligible(self) -> anyhow::Result<KeyspaceReport> {
        let mut report = KeyspaceReport::new("eligible");
        let source = self.from.list_all_eligible().await?;
        let target: BTreeSet<(ClientId, JobId, ExpiresAt)> =
            self.to.list_all_eligible().await?.into_iter().collect();
        let id =
            |(client, job, exp): &(ClientId, JobId, ExpiresAt)| format!("{client}/{job}.{exp}");

        let grouped = source.into_iter().fold(
            BTreeMap::<(ClientId, JobId), Vec<ExpiresAt>>::new(),
            |mut grouped, (client, job, exp)| {
                grouped.entry((client, job)).or_default().push(exp);
                grouped
            },
        );
        let mut kept = BTreeSet::new();
        let writes: Vec<_> = grouped
            .into_iter()
            .filter_map(|((client, job), mut exps)| {
                exps.sort();
                let exp = exps.pop()?;
                exps.into_iter().for_each(|dropped| {
                    tracing::warn!(
                        client_id = %client,
                        job_id = %job,
                        kept = %exp,
                        dropped = %dropped,
                        "queue copy dropped a duplicate eligibility marker"
                    );
                    report.record(
                        id(&(client.clone(), job.clone(), dropped)),
                        Outcome::Skipped(DUPLICATE.to_owned()),
                    );
                });
                let marker = (client, job, exp);
                let present = target.contains(&marker);
                report.record(id(&marker), unchanged_or_written(present));
                kept.insert(marker.clone());
                (!present).then_some(marker)
            })
            .collect();
        let deletes: Vec<_> = target.difference(&kept).cloned().collect();
        deletes
            .iter()
            .for_each(|marker| report.record(id(marker), Outcome::Deleted));

        apply(self.dry_run, deletes, |(client, job, exp)| async move {
            self.to.delete_eligible(&client, &job, exp).await
        })
        .await?;
        apply(self.dry_run, writes, |(client, job, exp)| async move {
            self.to.write_eligible(&client, &job, exp).await
        })
        .await?;
        Ok(report)
    }

    /// The trait deletes lost leases only by job. So a job whose markers differ
    /// loses its target markers, and the copy writes the source's again.
    async fn lost(self) -> anyhow::Result<KeyspaceReport> {
        let mut report = KeyspaceReport::new("lost");
        let target_jobs: BTreeSet<JobId> =
            self.to.list_lost_lease_jobs().await?.into_iter().collect();
        let jobs: BTreeSet<JobId> = self
            .from
            .list_lost_lease_jobs()
            .await?
            .into_iter()
            .chain(target_jobs.iter().cloned())
            .collect();

        let reads = each(jobs, |job| {
            let target_jobs = &target_jobs;
            async move {
                let source = self.from.list_lost_leases(&job).await;
                let target = if target_jobs.contains(&job) {
                    self.to.list_lost_leases(&job).await
                } else {
                    Ok(Vec::new())
                };
                (job, source, target)
            }
        })
        .await;

        let mut deletes = Vec::new();
        let mut writes = Vec::new();
        for (job, source, target) in reads {
            let source = match source {
                Ok(source) => to_lost_set(source),
                Err(e) => {
                    report.record(format!("{job}/"), unreadable(&e));
                    continue;
                }
            };
            let target = to_lost_set(target?);
            let id = |(client, at): &(ClientId, DateTime<Utc>)| {
                format!("{job}/{}", lost_lease_filename(client, *at))
            };
            if source == target {
                source
                    .iter()
                    .for_each(|lost| report.record(id(lost), Outcome::Unchanged));
                continue;
            }
            target
                .difference(&source)
                .for_each(|lost| report.record(id(lost), Outcome::Deleted));
            source
                .iter()
                .for_each(|lost| report.record(id(lost), Outcome::Written));
            if !target.is_empty() {
                deletes.push(job.clone());
            }
            writes.extend(
                source
                    .into_iter()
                    .map(|(client, at)| (job.clone(), client, at)),
            );
        }

        apply(self.dry_run, deletes, |job| async move {
            self.to.delete_lost_leases_for_job(&job).await
        })
        .await?;
        apply(self.dry_run, writes, |(job, client, at)| async move {
            self.to.write_lost_lease(&job, &client, at).await
        })
        .await?;
        Ok(report)
    }

    /// Each backend mints its own request id, so the copy compares the flags
    /// by client: one flag of a client stands for all of them.
    async fn pending_reindex(self) -> anyhow::Result<KeyspaceReport> {
        let mut report = KeyspaceReport::new("pending-reindex");
        let source: BTreeSet<ClientId> = self
            .from
            .list_pending_reindex()
            .await?
            .into_iter()
            .map(|(client, _)| client)
            .collect();
        let target = self.to.list_pending_reindex().await?.into_iter().fold(
            BTreeMap::<ClientId, Vec<String>>::new(),
            |mut target, (client, key)| {
                target.entry(client).or_default().push(key);
                target
            },
        );

        let writes: Vec<_> = source
            .iter()
            .filter(|client| {
                let present = target.contains_key(*client);
                report.record(client.to_string(), unchanged_or_written(present));
                !present
            })
            .cloned()
            .collect();
        let deletes: Vec<String> = target
            .into_iter()
            .filter(|(client, _)| !source.contains(client))
            .flat_map(|(_, keys)| keys)
            .collect();
        deletes
            .iter()
            .for_each(|key| report.record(key.clone(), Outcome::Deleted));

        apply(self.dry_run, deletes, |key| async move {
            self.to.delete_pending_reindex(&key).await
        })
        .await?;
        apply(self.dry_run, writes, |client| async move {
            self.to.write_pending_reindex(&client).await
        })
        .await?;
        Ok(report)
    }

    async fn pending_reindex_jobs(self) -> anyhow::Result<KeyspaceReport> {
        let mut report = KeyspaceReport::new("pending-reindex-jobs");
        let source: BTreeSet<JobId> = self
            .from
            .list_pending_reindex_jobs()
            .await?
            .into_iter()
            .collect();
        let target: BTreeSet<JobId> = self
            .to
            .list_pending_reindex_jobs()
            .await?
            .into_iter()
            .collect();
        source.iter().for_each(|job| {
            report.record(job.to_string(), unchanged_or_written(target.contains(job)));
        });
        let deletes: Vec<_> = target.difference(&source).cloned().collect();
        deletes
            .iter()
            .for_each(|job| report.record(job.to_string(), Outcome::Deleted));

        apply(self.dry_run, deletes, |job| async move {
            self.to.delete_pending_reindex_job(&job).await
        })
        .await?;
        apply(self.dry_run, source.difference(&target), |job| {
            self.to.write_pending_reindex_job(job)
        })
        .await?;
        Ok(report)
    }

    async fn suspended(self) -> anyhow::Result<KeyspaceReport> {
        let mut report = KeyspaceReport::new("suspended");
        let source = self.from.list_suspensions().await?;
        let target: BTreeMap<ClientId, SuspensionRecord> =
            self.to.list_suspensions().await?.into_iter().collect();
        let listed: BTreeSet<ClientId> = source.iter().map(|(client, _)| client.clone()).collect();

        let reads = source
            .into_iter()
            .map(|(client, record)| Read {
                target: target.get(&client).cloned().map(|held| Ok(Some(held))),
                source: Ok(Some(record)),
                key: client,
            })
            .collect();
        let (writes, _) = decide(&mut report, reads, ClientId::to_string, same_suspension);

        let unlisted = target.keys().filter(|client| !listed.contains(*client));
        let reads = each(unlisted, |client| async move {
            let held = self.from.read_suspension(client).await.map(|r| r.is_some());
            (client.clone(), held)
        })
        .await;
        let deletes = gone(&mut report, reads, ClientId::to_string);

        apply(self.dry_run, deletes, |client| async move {
            self.to.delete_suspension(&client).await
        })
        .await?;
        apply(self.dry_run, writes, |write| async move {
            self.to
                .write_suspension(
                    &write.key,
                    write.value.suspended_at,
                    &write.value.conflicting_job_id,
                )
                .await
        })
        .await?;
        Ok(report)
    }

    /// An object store cannot take a given modification time. So without
    /// `setter`, a device the target holds counts as unchanged at any time.
    async fn liveness(self, setter: Option<&dyn LivenessImport>) -> anyhow::Result<KeyspaceReport> {
        let mut report = KeyspaceReport::new("liveness");
        let source = self.from.list_liveness().await?;
        let target: BTreeMap<ClientId, DateTime<Utc>> =
            self.to.list_liveness().await?.into_iter().collect();
        let listed: BTreeSet<ClientId> = source.iter().map(|(client, _)| client.clone()).collect();

        let writes: Vec<_> = source
            .into_iter()
            .filter(|(client, at)| {
                let same = match (target.get(client), setter) {
                    (None, _) => false,
                    (Some(_), None) => true,
                    (Some(held), Some(_)) => to_second(*held) == to_second(*at),
                };
                report.record(client.to_string(), unchanged_or_written(same));
                !same
            })
            .collect();
        let deletes: Vec<_> = target
            .into_keys()
            .filter(|client| !listed.contains(client))
            .collect();
        deletes
            .iter()
            .for_each(|client| report.record(client.to_string(), Outcome::Deleted));

        apply(self.dry_run, deletes, |client| async move {
            self.to.delete_liveness(&client).await
        })
        .await?;
        apply(self.dry_run, writes, |(client, at)| async move {
            match setter {
                Some(setter) => setter.set_liveness(&client, at).await,
                None => self.to.stamp_liveness(&client).await,
            }
        })
        .await?;
        Ok(report)
    }

    async fn tmp(self) -> anyhow::Result<KeyspaceReport> {
        let mut report = KeyspaceReport::new("tmp");
        self.from
            .list_stale_tmp(Duration::ZERO)
            .await?
            .into_iter()
            .for_each(|key| report.record(key, Outcome::Skipped(STAGED.to_owned())));
        Ok(report)
    }

    /// The five run-state blobs of `queue-maintenance`. A missing key set reads
    /// as empty, so the copy writes an empty set for it.
    async fn run_state(self) -> anyhow::Result<KeyspaceReport> {
        let mut report = KeyspaceReport::new("state");
        let (from, to) = (self.from, self.to);
        let write = !self.dry_run;

        let indexed = (
            from.read_eligible_indexed().await,
            to.read_eligible_indexed().await,
        );
        if let Some(set) = compare(&mut report, "eligible-indexed", indexed.0, indexed.1)
            && write
        {
            to.write_eligible_indexed(&set).await?;
        }
        let workers = (
            from.read_eligible_workers().await,
            to.read_eligible_workers().await,
        );
        if let Some(set) = compare(&mut report, "eligible-workers", workers.0, workers.1)
            && write
        {
            to.write_eligible_workers(&set).await?;
        }
        let candidates = (
            from.read_gc_candidates().await,
            to.read_gc_candidates().await,
        );
        if let Some(set) = compare(&mut report, "gc-candidates", candidates.0, candidates.1)
            && write
        {
            to.write_gc_candidates(&set).await?;
        }
        let backlog = (from.read_backlog().await, to.read_backlog().await);
        if let Some(snapshot) = compare_snapshot(&mut report, "backlog", backlog.0, backlog.1)
            && write
        {
            to.write_backlog(&snapshot).await?;
        }
        let campaigns = (
            from.read_campaign_backlog().await,
            to.read_campaign_backlog().await,
        );
        if let Some(snapshot) =
            compare_snapshot(&mut report, "backlog-campaigns", campaigns.0, campaigns.1)
            && write
        {
            to.write_campaign_backlog(&snapshot).await?;
        }
        Ok(report)
    }
}

/// A keyspace of the job copy, timed across the phases that it shares with
/// the other.
struct Side {
    report: KeyspaceReport,
    elapsed: Duration,
}

impl Side {
    fn new(keyspace: &'static str) -> Self {
        Self {
            report: KeyspaceReport::new(keyspace),
            elapsed: Duration::ZERO,
        }
    }

    async fn time<T>(&mut self, work: impl Future<Output = T>) -> T {
        let started = Instant::now();
        let out = work.await;
        self.elapsed += started.elapsed();
        out
    }

    fn finish(self) -> TimedKeyspace {
        finished(self.report, self.elapsed)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Avail,
    Leased,
}

fn pick<'a>(avail: &'a mut Side, leased: &'a mut Side, kind: Kind) -> &'a mut Side {
    match kind {
        Kind::Avail => avail,
        Kind::Leased => leased,
    }
}

/// One `avail/` or `leased/` entry of a job. The derived order ranks last the
/// entry that the copy keeps: a lease before an `avail/` entry, then the later
/// lease expiry, then the later deadline.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum Entry {
    Avail(ExpiresAt),
    Leased {
        lease_expiry: DateTime<Utc>,
        client_id: ClientId,
        expires_at: ExpiresAt,
    },
}

impl Entry {
    fn kind(&self) -> Kind {
        match self {
            Self::Avail(_) => Kind::Avail,
            Self::Leased { .. } => Kind::Leased,
        }
    }

    fn expires_at(&self) -> ExpiresAt {
        match self {
            Self::Avail(expires_at) | Self::Leased { expires_at, .. } => *expires_at,
        }
    }

    fn key(&self, job: &JobId) -> String {
        match self {
            Self::Avail(expires_at) => avail_filename(job, *expires_at),
            Self::Leased {
                lease_expiry,
                client_id,
                expires_at,
            } => leased_key(job, *expires_at, client_id, *lease_expiry),
        }
    }

    async fn read(&self, store: &dyn TodoStore, job: &JobId) -> anyhow::Result<Option<Value>> {
        match self {
            Self::Avail(expires_at) => store.get_avail(job, *expires_at).await,
            Self::Leased {
                lease_expiry,
                client_id,
                expires_at,
            } => {
                store
                    .get_leased(job, *expires_at, client_id, *lease_expiry)
                    .await
            }
        }
    }

    async fn delete(&self, store: &dyn TodoStore, job: &JobId) -> anyhow::Result<()> {
        match self {
            Self::Avail(expires_at) => store.delete_avail(job, *expires_at).await,
            Self::Leased {
                lease_expiry,
                client_id,
                expires_at,
            } => {
                store
                    .delete_lease(job, *expires_at, client_id, *lease_expiry)
                    .await
            }
        }
    }

    /// Queues the job as ingestion does, then claims it if the entry is a lease.
    async fn write(&self, store: &dyn TodoStore, job: &JobId, body: &Value) -> anyhow::Result<()> {
        store.write_tmp(job, body).await?;
        store.promote_avail(job, self.expires_at()).await?;
        let Self::Leased {
            lease_expiry,
            client_id,
            expires_at,
        } = self
        else {
            return Ok(());
        };
        match store
            .claim_job(job, *expires_at, client_id, *lease_expiry)
            .await?
        {
            ClaimResult::Claimed(_) => Ok(()),
            ClaimResult::Gone => anyhow::bail!(
                "the target lost the copied lease {} at its claim",
                self.key(job)
            ),
        }
    }
}

type Entries = BTreeMap<JobId, Vec<Entry>>;

/// A listed key that names no entry the trait can address.
struct Unparsed {
    kind: Kind,
    key: String,
    reason: String,
}

fn parse_entries(avail: Vec<String>, leased: Vec<String>) -> (Entries, Vec<Unparsed>) {
    let avail = avail.into_iter().map(|key| {
        parse_avail_filename(&key)
            .map(|(job, exp)| (job, Entry::Avail(exp)))
            .map_err(|e| Unparsed {
                kind: Kind::Avail,
                reason: format!("the key does not parse: {e}"),
                key,
            })
    });
    let leased = leased.into_iter().map(|key| {
        parse_leased_key(&key)
            .map(|lease| {
                let entry = Entry::Leased {
                    lease_expiry: lease.lease_expiry,
                    client_id: lease.client_id,
                    expires_at: lease.expires_at,
                };
                (lease.job_id, entry)
            })
            .map_err(|e| Unparsed {
                kind: Kind::Leased,
                reason: format!("the key does not parse: {e:#}"),
                key,
            })
    });
    avail.chain(leased).fold(
        (Entries::new(), Vec::new()),
        |(mut entries, mut unparsed), parsed| {
            match parsed {
                Ok((job, entry)) => entries.entry(job).or_default().push(entry),
                Err(bad) => unparsed.push(bad),
            }
            (entries, unparsed)
        },
    )
}

/// One source entry, read from both stores. `target` is `None` when the
/// target's listing lacks the entry.
struct Read<K, V> {
    key: K,
    source: anyhow::Result<Option<V>>,
    target: Option<anyhow::Result<Option<V>>>,
}

/// A source entry that the target lacks or holds differently. `replaces` marks
/// a target entry under the same key.
struct Write<K, V> {
    key: K,
    value: V,
    replaces: bool,
}

/// Records each read and returns the entries to write. It also returns the
/// entries that it cannot copy, unreadable or vanished: the copy keeps what
/// the target holds for them.
fn decide<K, V>(
    report: &mut KeyspaceReport,
    reads: Vec<Read<K, V>>,
    id: impl Fn(&K) -> String,
    same: impl Fn(&V, &V) -> bool,
) -> (Vec<Write<K, V>>, Vec<K>) {
    let mut writes = Vec::new();
    let mut stuck = Vec::new();
    for read in reads {
        match read.source {
            Err(e) => {
                report.record(id(&read.key), unreadable(&e));
                stuck.push(read.key);
            }
            Ok(None) => {
                report.record(id(&read.key), Outcome::Skipped(VANISHED.to_owned()));
                stuck.push(read.key);
            }
            Ok(Some(value)) => {
                let unchanged = matches!(&read.target, Some(Ok(Some(held))) if same(held, &value));
                report.record(id(&read.key), unchanged_or_written(unchanged));
                if !unchanged {
                    writes.push(Write {
                        replaces: read.target.is_some(),
                        key: read.key,
                        value,
                    });
                }
            }
        }
    }
    (writes, stuck)
}

/// Target entries that the source's listing lacks, with whether a source read
/// finds each. A listing skips what it cannot read, so only an entry that the
/// read confirms gone is returned for the delete.
fn gone<K>(
    report: &mut KeyspaceReport,
    reads: Vec<(K, anyhow::Result<bool>)>,
    id: impl Fn(&K) -> String,
) -> Vec<K> {
    reads
        .into_iter()
        .filter_map(|(key, held)| {
            let outcome = match held {
                Ok(false) => Outcome::Deleted,
                Ok(true) => Outcome::Skipped(LISTING_SKIPPED.to_owned()),
                Err(e) => unreadable(&e),
            };
            let delete = matches!(outcome, Outcome::Deleted);
            report.record(id(&key), outcome);
            delete.then_some(key)
        })
        .collect()
}

/// A blob the target holds differently, or cannot read, is written.
fn compare<T: PartialEq>(
    report: &mut KeyspaceReport,
    name: &str,
    source: anyhow::Result<T>,
    target: anyhow::Result<T>,
) -> Option<T> {
    match source {
        Err(e) => {
            report.record(name.to_owned(), unreadable(&e));
            None
        }
        Ok(value) => {
            let unchanged = target.is_ok_and(|held| held == value);
            report.record(name.to_owned(), unchanged_or_written(unchanged));
            (!unchanged).then_some(value)
        }
    }
}

fn compare_snapshot<T: PartialEq>(
    report: &mut KeyspaceReport,
    name: &str,
    source: anyhow::Result<Option<T>>,
    target: anyhow::Result<Option<T>>,
) -> Option<T> {
    match source {
        Ok(None) if !matches!(target, Ok(None)) => {
            report.record(
                name.to_owned(),
                Outcome::Skipped(NO_SNAPSHOT_DELETE.to_owned()),
            );
            None
        }
        source => compare(report, name, source, target).flatten(),
    }
}

fn unchanged_or_written(unchanged: bool) -> Outcome {
    if unchanged {
        Outcome::Unchanged
    } else {
        Outcome::Written
    }
}

/// PostgreSQL keeps microseconds; a file can hold nanoseconds.
fn to_micros(at: DateTime<Utc>) -> DateTime<Utc> {
    at.trunc_subsecs(6)
}

/// An S3 modification time and a queue key keep seconds.
fn to_second(at: DateTime<Utc>) -> DateTime<Utc> {
    at.trunc_subsecs(0)
}

fn same_denial(a: &DenialRecord, b: &DenialRecord) -> bool {
    let normal = |r: &DenialRecord| DenialRecord {
        denied_at: r.denied_at.map(to_micros),
        ..r.clone()
    };
    normal(a) == normal(b)
}

fn same_suspension(a: &SuspensionRecord, b: &SuspensionRecord) -> bool {
    to_micros(a.suspended_at) == to_micros(b.suspended_at)
        && a.conflicting_job_id == b.conflicting_job_id
}

fn to_lost_set(lost: Vec<(ClientId, DateTime<Utc>)>) -> BTreeSet<(ClientId, DateTime<Utc>)> {
    lost.into_iter()
        .map(|(client, at)| (client, to_second(at)))
        .collect()
}

async fn timed(
    copy: impl Future<Output = anyhow::Result<KeyspaceReport>>,
) -> anyhow::Result<TimedKeyspace> {
    let started = Instant::now();
    let report = copy.await?;
    Ok(finished(report, started.elapsed()))
}

fn finished(report: KeyspaceReport, elapsed: Duration) -> TimedKeyspace {
    tracing::info!(
        keyspace = report.keyspace,
        read = report.read,
        written = report.written,
        unchanged = report.unchanged,
        deleted = report.deleted,
        skipped = report.skipped.len(),
        unreadable = report.unreadable.len(),
        elapsed_ms = millis(elapsed),
        "queue keyspace copied"
    );
    TimedKeyspace { report, elapsed }
}

fn millis(elapsed: Duration) -> u64 {
    u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
}

/// Runs `read` on each item, [`STORAGE_CONCURRENCY`] at once, in item order.
async fn each<T, R, F, Fut>(items: impl IntoIterator<Item = T>, read: F) -> Vec<R>
where
    F: FnMut(T) -> Fut,
    Fut: Future<Output = R>,
{
    stream::iter(items)
        .map(read)
        .buffered(STORAGE_CONCURRENCY)
        .collect()
        .await
}

/// Runs `change` on each item, [`STORAGE_CONCURRENCY`] at once, unless this is
/// a dry run.
async fn apply<T, F, Fut>(
    dry_run: bool,
    items: impl IntoIterator<Item = T>,
    change: F,
) -> anyhow::Result<()>
where
    F: FnMut(T) -> Fut,
    Fut: Future<Output = anyhow::Result<()>>,
{
    if dry_run {
        return Ok(());
    }
    stream::iter(items)
        .map(change)
        .buffer_unordered(STORAGE_CONCURRENCY)
        .try_collect::<Vec<()>>()
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::path::Path;
    use std::sync::Arc;

    use chrono::TimeDelta;
    use rstest::rstest;
    use serde_json::json;

    use pipette_core::config::StorageConfig;
    use pipette_store::{BacklogSnapshot, CampaignBacklogSnapshot, UnmigratedAvailKeyError};
    use pipette_store_sql::SqlTodoStore;
    use pipette_store_sql::scratch::TestDb;
    use pipette_store_testkit::{seed_avail, seed_leased_until};

    use super::*;

    fn now() -> anyhow::Result<DateTime<Utc>> {
        Ok("2026-11-01T08:30:00Z".parse()?)
    }

    fn job(id: &str) -> anyhow::Result<JobId> {
        Ok(JobId::try_new(id)?)
    }

    fn client(id: &str) -> anyhow::Result<ClientId> {
        Ok(ClientId::try_new(id)?)
    }

    fn body(id: &str) -> Value {
        json!({"job_id": id, "payload": [1, 2]})
    }

    fn fs_queue(dir: &Path) -> anyhow::Result<Arc<dyn TodoStore>> {
        pipette_store_fs::build_local_fs_todo_store(&StorageConfig::local_fs(dir.to_path_buf()))
    }

    fn dated() -> anyhow::Result<ExpiresAt> {
        Ok(ExpiresAt::At(now()? + TimeDelta::days(1)))
    }

    fn leased_until() -> anyhow::Result<DateTime<Utc>> {
        Ok(now()? + TimeDelta::minutes(5))
    }

    /// One entry or more in every keyspace.
    async fn seed(store: &dyn TodoStore) -> anyhow::Result<()> {
        let (a, b) = (client("ev1_a")?, client("ev1_b")?);
        seed_avail(store, "job-never", ExpiresAt::Never, &body("job-never")).await?;
        seed_avail(store, "job-dated", dated()?, &body("job-dated")).await?;
        seed_leased_until(
            store,
            "job-leased",
            ExpiresAt::Never,
            &a,
            leased_until()?,
            &body("job-leased"),
        )
        .await?;
        let denial = DenialRecord {
            denied_at: Some(now()?),
            failure_reason: Some("out of memory".to_owned()),
            ..DenialRecord::default()
        };
        store.write_denied(&job("job-dated")?, &b, &denial).await?;
        store
            .write_eligible(&a, &job("job-never")?, ExpiresAt::Never)
            .await?;
        store
            .write_eligible(&b, &job("job-dated")?, dated()?)
            .await?;
        store
            .write_lost_lease(&job("job-dated")?, &b, now()? - TimeDelta::hours(1))
            .await?;
        store.write_pending_reindex(&a).await?;
        store.write_pending_reindex_job(&job("job-never")?).await?;
        store
            .write_suspension(&b, now()?, &job("job-leased")?)
            .await?;
        store.stamp_liveness(&a).await?;
        store.stamp_liveness(&b).await?;
        store
            .write_tmp(&job("job-staged")?, &body("job-staged"))
            .await?;
        store
            .write_eligible_indexed(&HashSet::from(["job-never/never.json".to_owned()]))
            .await?;
        store
            .write_eligible_workers(&HashSet::from([a.clone()]))
            .await?;
        store
            .write_gc_candidates(&HashSet::from(["plans:old".to_owned()]))
            .await?;
        store
            .write_backlog(&BacklogSnapshot {
                counted_at: now()?,
                jobs: BTreeMap::from([(a, 2)]),
            })
            .await?;
        store
            .write_campaign_backlog(&CampaignBacklogSnapshot {
                counted_at: now()?,
                devices: BTreeMap::new(),
            })
            .await?;
        Ok(())
    }

    /// What a copy must carry over, in an order no backend sets. Liveness and
    /// reindex flags compare by client, as the copy does; `tmp/` is not copied.
    #[derive(Debug, PartialEq)]
    struct QueueState {
        jobs: BTreeMap<String, Option<Value>>,
        denied: BTreeMap<(JobId, ClientId), Option<DenialRecord>>,
        eligible: BTreeSet<(ClientId, JobId, ExpiresAt)>,
        lost: BTreeMap<JobId, BTreeSet<(ClientId, DateTime<Utc>)>>,
        pending_reindex: BTreeSet<ClientId>,
        pending_reindex_jobs: BTreeSet<JobId>,
        suspended: BTreeSet<(ClientId, DateTime<Utc>, JobId)>,
        liveness: BTreeSet<ClientId>,
        indexed: HashSet<String>,
        workers: HashSet<ClientId>,
        gc_candidates: HashSet<String>,
        backlog: Option<BacklogSnapshot>,
        campaign_backlog: Option<CampaignBacklogSnapshot>,
    }

    async fn state(store: &dyn TodoStore) -> anyhow::Result<QueueState> {
        let (entries, unparsed) =
            parse_entries(store.list_avail().await?, store.list_leased().await?);
        anyhow::ensure!(unparsed.is_empty(), "a queue key does not parse");
        let mut jobs = BTreeMap::new();
        for (job, entries) in entries {
            for entry in entries {
                jobs.insert(entry.key(&job), entry.read(store, &job).await?);
            }
        }
        let mut denied = BTreeMap::new();
        for (job, client) in store.list_all_denied().await? {
            let record = store.get_denied(&job, &client).await?;
            denied.insert((job, client), record);
        }
        let mut lost = BTreeMap::new();
        for job in store.list_lost_lease_jobs().await? {
            let leases = store.list_lost_leases(&job).await?;
            lost.insert(job, leases.into_iter().collect());
        }
        Ok(QueueState {
            jobs,
            denied,
            eligible: store.list_all_eligible().await?.into_iter().collect(),
            lost,
            pending_reindex: store
                .list_pending_reindex()
                .await?
                .into_iter()
                .map(|(client, _)| client)
                .collect(),
            pending_reindex_jobs: store
                .list_pending_reindex_jobs()
                .await?
                .into_iter()
                .collect(),
            suspended: store
                .list_suspensions()
                .await?
                .into_iter()
                .map(|(c, r)| (c, r.suspended_at, r.conflicting_job_id))
                .collect(),
            liveness: store
                .list_liveness()
                .await?
                .into_iter()
                .map(|(client, _)| client)
                .collect(),
            indexed: store.read_eligible_indexed().await?,
            workers: store.read_eligible_workers().await?,
            gc_candidates: store.read_gc_candidates().await?,
            backlog: store.read_backlog().await?,
            campaign_backlog: store.read_campaign_backlog().await?,
        })
    }

    type Counts = Vec<(&'static str, usize, usize, usize, usize, usize, usize)>;

    fn counts(report: &QueueCopyReport) -> Counts {
        report
            .keyspaces
            .iter()
            .map(|k| {
                let r = &k.report;
                (
                    r.keyspace,
                    r.read,
                    r.written,
                    r.unchanged,
                    r.deleted,
                    r.skipped.len(),
                    r.unreadable.len(),
                )
            })
            .collect()
    }

    fn keyspace<'a>(report: &'a QueueCopyReport, name: &str) -> anyhow::Result<&'a KeyspaceReport> {
        report
            .keyspaces
            .iter()
            .map(|k| &k.report)
            .find(|k| k.keyspace == name)
            .ok_or_else(|| anyhow::anyhow!("no {name} row"))
    }

    fn assert_quiet(report: &QueueCopyReport) {
        assert_eq!(
            (report.written(), report.deleted(), report.unreadable()),
            (0, 0, 0),
            "{:?}",
            counts(report)
        );
    }

    #[tokio::test]
    async fn a_queue_survives_an_import_and_an_export() -> anyhow::Result<()> {
        let Some(test_db) = TestDb::new().await? else {
            return Ok(());
        };
        let (a, b) = (tempfile::tempdir()?, tempfile::tempdir()?);
        let (source, back) = (fs_queue(a.path())?, fs_queue(b.path())?);
        let sql = SqlTodoStore::new(test_db.db().clone());
        seed(&*source).await?;

        let import = copy_queue(&*source, &sql, Some(&sql), false).await?;
        assert_eq!(import.unreadable(), 0);
        assert_eq!(keyspace(&import, "tmp")?.skipped.len(), 1);
        let source_times: BTreeMap<_, _> = source
            .list_liveness()
            .await?
            .into_iter()
            .map(|(c, at)| (c, to_second(at)))
            .collect();
        let sql_times: BTreeMap<_, _> = sql.list_liveness().await?.into_iter().collect();
        assert_eq!(sql_times, source_times);
        assert_quiet(&copy_queue(&*source, &sql, Some(&sql), false).await?);

        let export = copy_queue(&sql, &*back, None, false).await?;
        assert_eq!(export.unreadable(), 0);
        assert_eq!(state(&*back).await?, state(&*source).await?);
        assert_quiet(&copy_queue(&sql, &*back, None, false).await?);
        Ok(())
    }

    #[tokio::test]
    async fn a_second_copy_writes_and_deletes_nothing() -> anyhow::Result<()> {
        let (a, b) = (tempfile::tempdir()?, tempfile::tempdir()?);
        let (source, target) = (fs_queue(a.path())?, fs_queue(b.path())?);
        seed(&*source).await?;
        let first = copy_queue(&*source, &*target, None, false).await?;
        assert!(first.written() > 0);
        assert_eq!(state(&*target).await?, state(&*source).await?);
        assert_quiet(&copy_queue(&*source, &*target, None, false).await?);
        Ok(())
    }

    #[derive(Debug, Clone, Copy)]
    enum Change {
        Finished,
        Claimed,
        Recycled,
        Renewed,
        Redated,
        DenialCleared,
        EligibilityDropped,
        LostLeasesCleared,
        ReindexDone,
        SuspensionLifted,
        LivenessGone,
        StateRewritten,
    }

    async fn apply_change(store: &dyn TodoStore, change: Change) -> anyhow::Result<()> {
        let (a, b) = (client("ev1_a")?, client("ev1_b")?);
        match change {
            Change::Finished => {
                store
                    .delete_lease(&job("job-leased")?, ExpiresAt::Never, &a, leased_until()?)
                    .await?;
            }
            Change::Claimed => {
                let claimed = store
                    .claim_job(&job("job-dated")?, dated()?, &b, now()?)
                    .await?;
                anyhow::ensure!(matches!(claimed, ClaimResult::Claimed(_)));
            }
            Change::Recycled => {
                store
                    .recycle_lease(&job("job-leased")?, ExpiresAt::Never, &a, leased_until()?)
                    .await?;
            }
            Change::Renewed => {
                store
                    .renew_lease(
                        &job("job-leased")?,
                        &a,
                        leased_until()? + TimeDelta::minutes(5),
                    )
                    .await?;
            }
            Change::Redated => {
                store
                    .redate_avail(&job("job-dated")?, dated()?, ExpiresAt::Never)
                    .await?;
            }
            Change::DenialCleared => store.delete_denied(&job("job-dated")?, &b).await?,
            Change::EligibilityDropped => {
                store
                    .delete_eligible(&a, &job("job-never")?, ExpiresAt::Never)
                    .await?;
            }
            Change::LostLeasesCleared => {
                store.delete_lost_leases_for_job(&job("job-dated")?).await?;
            }
            Change::ReindexDone => {
                for (_, key) in store.list_pending_reindex().await? {
                    store.delete_pending_reindex(&key).await?;
                }
                store.delete_pending_reindex_job(&job("job-never")?).await?;
            }
            Change::SuspensionLifted => store.delete_suspension(&b).await?,
            Change::LivenessGone => store.delete_liveness(&b).await?,
            Change::StateRewritten => {
                store.write_eligible_indexed(&HashSet::new()).await?;
                store
                    .write_backlog(&BacklogSnapshot {
                        counted_at: now()? + TimeDelta::minutes(1),
                        jobs: BTreeMap::new(),
                    })
                    .await?;
            }
        }
        Ok(())
    }

    #[rstest]
    #[case::finished(Change::Finished)]
    #[case::claimed(Change::Claimed)]
    #[case::recycled(Change::Recycled)]
    #[case::renewed(Change::Renewed)]
    #[case::redated(Change::Redated)]
    #[case::denial_cleared(Change::DenialCleared)]
    #[case::eligibility_dropped(Change::EligibilityDropped)]
    #[case::lost_leases_cleared(Change::LostLeasesCleared)]
    #[case::reindex_done(Change::ReindexDone)]
    #[case::suspension_lifted(Change::SuspensionLifted)]
    #[case::liveness_gone(Change::LivenessGone)]
    #[case::state_rewritten(Change::StateRewritten)]
    #[tokio::test]
    async fn the_next_import_carries_a_change_on_the_source(
        #[case] change: Change,
    ) -> anyhow::Result<()> {
        let Some(test_db) = TestDb::new().await? else {
            return Ok(());
        };
        let dir = tempfile::tempdir()?;
        let source = fs_queue(dir.path())?;
        let sql = SqlTodoStore::new(test_db.db().clone());
        seed(&*source).await?;
        copy_queue(&*source, &sql, Some(&sql), false).await?;

        apply_change(&*source, change).await?;
        let delta = copy_queue(&*source, &sql, Some(&sql), false).await?;
        assert!(
            delta.written() + delta.deleted() > 0,
            "{:?}",
            counts(&delta)
        );
        assert_eq!(state(&sql).await?, state(&*source).await?);
        assert_quiet(&copy_queue(&*source, &sql, Some(&sql), false).await?);
        Ok(())
    }

    #[derive(Debug, Clone, Copy)]
    enum Duplicate {
        TwoAvail,
        AvailAndLease,
        TwoLeases,
        TwoEligible,
    }

    #[rstest]
    #[case::two_avail(Duplicate::TwoAvail, "avail")]
    #[case::avail_and_lease(Duplicate::AvailAndLease, "avail")]
    #[case::two_leases(Duplicate::TwoLeases, "leased")]
    #[case::two_eligible(Duplicate::TwoEligible, "eligible")]
    #[tokio::test]
    async fn the_import_keeps_one_entry_of_a_duplicate(
        #[case] duplicate: Duplicate,
        #[case] dropped_from: &str,
    ) -> anyhow::Result<()> {
        let Some(test_db) = TestDb::new().await? else {
            return Ok(());
        };
        let dir = tempfile::tempdir()?;
        let source = fs_queue(dir.path())?;
        let sql = SqlTodoStore::new(test_db.db().clone());
        let (a, b) = (client("ev1_a")?, client("ev1_b")?);
        let id = "job-twice";
        let requeue = || async {
            source.write_tmp(&job(id)?, &body(id)).await?;
            source.promote_avail(&job(id)?, ExpiresAt::Never).await
        };
        let lease = |client: &ClientId, until| Entry::Leased {
            lease_expiry: until,
            client_id: client.clone(),
            expires_at: ExpiresAt::Never,
        };
        let later = leased_until()? + TimeDelta::minutes(5);
        let kept = match duplicate {
            Duplicate::TwoAvail => {
                seed_avail(&*source, id, dated()?, &body(id)).await?;
                requeue().await?;
                Some(Entry::Avail(ExpiresAt::Never))
            }
            Duplicate::AvailAndLease => {
                seed_leased_until(
                    &*source,
                    id,
                    ExpiresAt::Never,
                    &a,
                    leased_until()?,
                    &body(id),
                )
                .await?;
                requeue().await?;
                Some(lease(&a, leased_until()?))
            }
            Duplicate::TwoLeases => {
                seed_leased_until(
                    &*source,
                    id,
                    ExpiresAt::Never,
                    &a,
                    leased_until()?,
                    &body(id),
                )
                .await?;
                seed_leased_until(&*source, id, ExpiresAt::Never, &b, later, &body(id)).await?;
                Some(lease(&b, later))
            }
            Duplicate::TwoEligible => {
                source.write_eligible(&a, &job(id)?, dated()?).await?;
                source
                    .write_eligible(&a, &job(id)?, ExpiresAt::Never)
                    .await?;
                None
            }
        };

        let report = copy_queue(&*source, &sql, Some(&sql), false).await?;
        assert_eq!(keyspace(&report, dropped_from)?.skipped.len(), 1);
        match kept {
            Some(entry) => {
                let (entries, _) = parse_entries(sql.list_avail().await?, sql.list_leased().await?);
                assert_eq!(entries.get(&job(id)?), Some(&vec![entry]));
            }
            None => assert_eq!(
                sql.list_all_eligible().await?,
                vec![(a, job(id)?, ExpiresAt::Never)]
            ),
        }
        assert_quiet(&copy_queue(&*source, &sql, Some(&sql), false).await?);
        Ok(())
    }

    #[tokio::test]
    async fn a_dry_run_counts_what_the_copy_then_does() -> anyhow::Result<()> {
        let Some(test_db) = TestDb::new().await? else {
            return Ok(());
        };
        let dir = tempfile::tempdir()?;
        let source = fs_queue(dir.path())?;
        let sql = SqlTodoStore::new(test_db.db().clone());
        seed(&*source).await?;
        let empty = state(&sql).await?;

        let dry = copy_queue(&*source, &sql, Some(&sql), true).await?;
        assert_eq!(state(&sql).await?, empty);
        assert!(sql.list_liveness().await?.is_empty());
        let real = copy_queue(&*source, &sql, Some(&sql), false).await?;
        assert!(real.written() > 0);
        assert_eq!(counts(&dry), counts(&real));
        Ok(())
    }

    #[tokio::test]
    async fn a_flat_avail_key_stops_the_copy_before_a_write() -> anyhow::Result<()> {
        let (a, b) = (tempfile::tempdir()?, tempfile::tempdir()?);
        let (source, target) = (fs_queue(a.path())?, fs_queue(b.path())?);
        seed(&*source).await?;
        std::fs::write(a.path().join("todo/avail/job-flat.never.json"), b"{}")?;

        let Err(error) = copy_queue(&*source, &*target, None, false).await else {
            anyhow::bail!("the copy took a flat avail/ key");
        };
        assert!(error.is::<UnmigratedAvailKeyError>(), "{error:#}");
        assert!(target.list_avail().await?.is_empty());
        assert!(target.list_all_eligible().await?.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn an_unreadable_source_body_keeps_the_target_copy() -> anyhow::Result<()> {
        let (a, b) = (tempfile::tempdir()?, tempfile::tempdir()?);
        let (source, target) = (fs_queue(a.path())?, fs_queue(b.path())?);
        seed(&*source).await?;
        copy_queue(&*source, &*target, None, false).await?;
        std::fs::write(
            a.path().join("todo/avail/job-never/never.json"),
            b"not json",
        )?;

        let report = copy_queue(&*source, &*target, None, false).await?;
        assert_eq!(keyspace(&report, "avail")?.unreadable.len(), 1);
        assert_eq!(report.deleted(), 0);
        assert_eq!(
            target
                .get_avail(&job("job-never")?, ExpiresAt::Never)
                .await?,
            Some(body("job-never"))
        );
        Ok(())
    }
}
