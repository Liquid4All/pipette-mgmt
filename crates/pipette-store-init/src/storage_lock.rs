//! Advisory lock over the storage root that every read-modify-write batch
//! command holds for its whole run, since the storage layer lets two
//! interleave and drop rows: `flock(2)` on `local_fs`, a lease on `s3`.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::os::unix::io::AsRawFd;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use object_store::path::Path as ObjPath;
use object_store::{
    ObjectMeta, ObjectStore, ObjectStoreExt, PutMode, PutOptions, PutPayload, UpdateVersion,
};
use serde::{Deserialize, Serialize};

use pipette_core::config::StorageConfig;
use pipette_store_s3::{build_s3_object_store, obj_path};

/// Key of the shared mutate lock, relative to the storage root.
const LOCK_KEY: &str = "locks/mutate.lock";

const MUTATE_LOCK: &str = "mutate";

/// A lost takeover race resolves in two iterations (lose the swap, then
/// see the winner's fresh lease and bail); four leaves margin.
const ACQUIRE_ATTEMPTS: usize = 4;

/// JSON, so an operator or `unlock` can read who holds it and until when.
#[derive(Serialize, Deserialize)]
struct LockBody {
    /// `release` deletes only on a matching token, so a holder whose
    /// lease was taken over spares the new holder's lock.
    token: String,
    /// Command that holds the lock, e.g. `"score"` or `"fix-thinking"`.
    holder: String,
    hostname: String,
    pid: u32,
    acquired_at: DateTime<Utc>,
    expires_at: DateTime<Utc>,
}

/// A held storage lock. Without [`StorageLock::release`], a drop still
/// frees a `local_fs` lock but leaves an `s3` lease to expire.
#[derive(Debug)]
pub struct StorageLock(LockKind);

#[derive(Debug)]
enum LockKind {
    /// The `flock(2)` is held for as long as `file` is open.
    LocalFs { file: File },
    S3 {
        store: Arc<dyn ObjectStore>,
        path: ObjPath,
        token: String,
        holder: String,
    },
}

impl StorageLock {
    /// Acquire the shared mutate lock, or fail if another command holds
    /// it. `ttl` is the S3 lease: it must outlast the longest run, or the
    /// lease can be taken over mid-write.
    pub async fn acquire(
        storage: &StorageConfig,
        holder: &str,
        ttl: Duration,
    ) -> anyhow::Result<Self> {
        Self::acquire_named(storage, MUTATE_LOCK, holder, ttl).await
    }

    /// An independent lock at `locks/{lock_name}.lock`, e.g. single-instance
    /// `score-eval`, which must not block the writers holding `mutate`.
    pub async fn acquire_named(
        storage: &StorageConfig,
        lock_name: &str,
        holder: &str,
        ttl: Duration,
    ) -> anyhow::Result<Self> {
        let lock_key = format!("locks/{lock_name}.lock");
        match storage {
            StorageConfig::LocalFs { data_dir } => acquire_local_fs(data_dir, &lock_key, holder),
            StorageConfig::S3 { .. } => {
                let (store, prefix) = build_s3_object_store(storage)?;
                acquire_s3(store, obj_path(&prefix, &lock_key), holder, ttl).await
            }
        }
    }

    /// Best-effort: failures are logged, since the command's work is done.
    pub async fn release(self) {
        match self.0 {
            LockKind::LocalFs { file } => {
                // Clear the holder details before `file` closes and drops
                // the `flock`, so a later inspection shows no dead holder.
                if let Err(e) = file.set_len(0) {
                    tracing::warn!(
                        error = %e,
                        "failed to clear storage lock holder details at release"
                    );
                }
            }
            LockKind::S3 {
                store,
                path,
                token,
                holder,
            } => release_s3(&store, &path, &token, &holder).await,
        }
    }
}

/// Inspect the storage mutate lock and, where appropriate, clear it.
pub async fn unlock(storage: &StorageConfig, force: bool) -> anyhow::Result<()> {
    match storage {
        StorageConfig::LocalFs { data_dir } => unlock_local_fs(data_dir),
        StorageConfig::S3 { .. } => {
            let (store, prefix) = build_s3_object_store(storage)?;
            unlock_s3(&store, &obj_path(&prefix, LOCK_KEY), force).await
        }
    }
}

fn acquire_local_fs(data_dir: &Path, lock_key: &str, holder: &str) -> anyhow::Result<StorageLock> {
    let path = data_dir.join(lock_key);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // `truncate(false)`: a contender reads the live holder's details;
    // the acquirer truncates only after it wins the `flock`.
    let mut file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(&path)?;

    match try_flock_exclusive(&file) {
        FlockResult::Acquired => {}
        FlockResult::Held => {
            let info = holder_info(&path);
            anyhow::bail!("{}", local_held_message(&path, &info));
        }
        FlockResult::Error(e) => {
            return Err(anyhow::anyhow!("failed to flock {}: {e}", path.display()));
        }
    }

    file.set_len(0)?;
    if let Err(e) = file.write_all(local_info_line(holder).as_bytes()) {
        tracing::warn!(
            holder,
            path = %path.display(),
            error = %e,
            "failed to record storage lock holder details; contenders will see none"
        );
    }
    tracing::info!(holder, path = %path.display(), "acquired storage lock");
    Ok(StorageLock(LockKind::LocalFs { file }))
}

fn holder_info(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|e| {
        tracing::warn!(
            path = %path.display(),
            error = %e,
            "failed to read storage lock holder details; showing none"
        );
        String::new()
    })
}

fn unlock_local_fs(data_dir: &Path) -> anyhow::Result<()> {
    let path = data_dir.join("locks").join("mutate.lock");
    let file = match OpenOptions::new().read(true).write(true).open(&path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            println!("No storage mutate lock file exists.");
            return Ok(());
        }
        Err(e) => return Err(e.into()),
    };
    match try_flock_exclusive(&file) {
        FlockResult::Acquired => {
            // The kernel frees a dead holder's lock, so `--force` has
            // nothing to clear.
            println!("No command is holding the storage mutate lock.");
            Ok(())
        }
        FlockResult::Held => {
            let info = holder_info(&path);
            let who = if info.trim().is_empty() {
                "details unavailable"
            } else {
                info.trim()
            };
            println!("The storage mutate lock is held by a running command [{who}].");
            println!(
                "On the local_fs backend the lock is released automatically when that \
                 process exits or dies, so there is nothing to clear by hand."
            );
            Ok(())
        }
        FlockResult::Error(e) => Err(anyhow::anyhow!("failed to flock {}: {e}", path.display())),
    }
}

enum FlockResult {
    Acquired,
    Held,
    Error(std::io::Error),
}

/// Non-blocking exclusive `flock`, released when every descriptor of the
/// open file description closes, process death included.
fn try_flock_exclusive(file: &File) -> FlockResult {
    // SAFETY: `file` owns a valid fd that outlives the call.
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc == 0 {
        return FlockResult::Acquired;
    }
    let err = std::io::Error::last_os_error();
    if err.kind() == std::io::ErrorKind::WouldBlock {
        FlockResult::Held
    } else {
        FlockResult::Error(err)
    }
}

fn local_info_line(holder: &str) -> String {
    format!(
        "holder={} host={} pid={} acquired_at={}",
        holder,
        hostname(),
        std::process::id(),
        Utc::now().to_rfc3339(),
    )
}

fn local_held_message(path: &Path, info: &str) -> String {
    let who = if info.trim().is_empty() {
        "holder details unavailable"
    } else {
        info.trim()
    };
    format!(
        "storage mutate lock {} is held by another command [{}]. Another `score` or \
         `fix-*` command is running; wait for it to finish. A crashed holder's lock is \
         released automatically.",
        path.display(),
        who,
    )
}

async fn acquire_s3(
    store: Arc<dyn ObjectStore>,
    path: ObjPath,
    holder: &str,
    ttl: Duration,
) -> anyhow::Result<StorageLock> {
    let token = uuid::Uuid::new_v4().to_string();
    let ttl = i64::try_from(ttl.as_secs())
        .ok()
        .and_then(chrono::Duration::try_seconds)
        .ok_or_else(|| anyhow::anyhow!("lock ttl {ttl:?} is out of range"))?;

    for _ in 0..ACQUIRE_ATTEMPTS {
        let now = Utc::now();
        let body = LockBody {
            token: token.clone(),
            holder: holder.to_string(),
            hostname: hostname(),
            pid: std::process::id(),
            acquired_at: now,
            expires_at: now + ttl,
        };
        let bytes = serde_json::to_vec_pretty(&body)?;

        match store
            .put_opts(
                &path,
                PutPayload::from(bytes.clone()),
                opts(PutMode::Create),
            )
            .await
        {
            Ok(_) => {
                tracing::info!(holder, path = %path, "acquired storage mutate lock");
                return Ok(s3_lock(store, path, token, holder));
            }
            // A live lease (bail) or a crashed holder's stale one (take over).
            Err(object_store::Error::AlreadyExists { .. }) => {
                let Some((existing_bytes, meta)) = get_lock_object(&store, &path).await? else {
                    // Released between our PUT and our GET — retry.
                    tracing::debug!(
                        holder,
                        path = %path,
                        "storage mutate lock released between the create and the read"
                    );
                    continue;
                };
                let existing: LockBody = serde_json::from_slice(&existing_bytes).map_err(|e| {
                    anyhow::anyhow!(
                        "storage mutate lock at {path} exists but is not readable ({e}); \
                             clear it with `pipette-mgmt unlock --force`"
                    )
                })?;
                if existing.expires_at > Utc::now() {
                    anyhow::bail!("{}", s3_held_message(&existing));
                }
                tracing::warn!(
                    stale_holder = %existing.holder,
                    stale_pid = existing.pid,
                    expired_at = %existing.expires_at,
                    "storage mutate lock lease expired; taking it over"
                );
                // Compare-and-swap on the version: a racer that read the
                // same stale version gets `Precondition` and re-reads.
                match store
                    .put_opts(
                        &path,
                        PutPayload::from(bytes.clone()),
                        opts(PutMode::Update(UpdateVersion {
                            e_tag: meta.e_tag.clone(),
                            version: meta.version.clone(),
                        })),
                    )
                    .await
                {
                    Ok(_) => {
                        tracing::info!(
                            holder,
                            path = %path,
                            "acquired storage mutate lock (took over expired lease)"
                        );
                        return Ok(s3_lock(store, path, token, holder));
                    }
                    // Lost the race, or the object changed or vanished.
                    Err(object_store::Error::Precondition { .. }) => {
                        tracing::debug!(
                            holder,
                            path = %path,
                            "lost the storage mutate lock takeover race"
                        );
                        continue;
                    }
                    Err(e) => return Err(e.into()),
                }
            }
            Err(e) => return Err(e.into()),
        }
    }

    anyhow::bail!(
        "could not acquire storage mutate lock at {path} after {ACQUIRE_ATTEMPTS} attempts \
         because another command kept re-taking it"
    )
}

async fn release_s3(store: &Arc<dyn ObjectStore>, path: &ObjPath, token: &str, holder: &str) {
    let bytes = match get_lock_object(store, path).await {
        Ok(Some((b, _))) => b,
        Ok(None) => {
            tracing::warn!(holder, "storage mutate lock already gone at release time");
            return;
        }
        Err(e) => {
            tracing::warn!(error = %e, "failed to read storage mutate lock at release");
            return;
        }
    };
    match serde_json::from_slice::<LockBody>(&bytes) {
        Ok(current) if current.token == token => match store.delete(path).await {
            Ok(()) => tracing::info!(holder, "released storage mutate lock"),
            Err(e) => tracing::warn!(
                error = %e,
                holder,
                "failed to delete storage mutate lock at release"
            ),
        },
        Ok(current) => tracing::warn!(
            holder,
            current_holder = %current.holder,
            "storage mutate lock lease was taken over by another command; not deleting"
        ),
        Err(e) => tracing::warn!(error = %e, "storage mutate lock unreadable at release"),
    }
}

async fn unlock_s3(
    store: &Arc<dyn ObjectStore>,
    path: &ObjPath,
    force: bool,
) -> anyhow::Result<()> {
    let bytes = match get_lock_object(store, path).await? {
        None => {
            println!("No storage mutate lock is held.");
            return Ok(());
        }
        Some((b, _)) => b,
    };

    let (holder, active) = match serde_json::from_slice::<LockBody>(&bytes) {
        Ok(body) => {
            let expired = body.expires_at <= Utc::now();
            println!(
                "Storage mutate lock:\n  \
                 holder:      {}\n  \
                 host:        {}\n  \
                 pid:         {}\n  \
                 acquired_at: {}\n  \
                 expires_at:  {} ({})",
                body.holder,
                body.hostname,
                body.pid,
                body.acquired_at.to_rfc3339(),
                body.expires_at.to_rfc3339(),
                if expired { "expired" } else { "active" },
            );
            if !expired && !force {
                anyhow::bail!(
                    "lock lease is still active, so a `score` or `fix-*` command may be \
                     running. Re-run `unlock --force` to break it anyway."
                );
            }
            (Some(body.holder), !expired)
        }
        Err(e) => {
            println!("Storage mutate lock at {path} is present but unreadable: {e}");
            if !force {
                anyhow::bail!("re-run `unlock --force` to remove the unreadable lock object");
            }
            (None, false)
        }
    };

    store.delete(path).await?;
    let holder = holder.as_deref();
    if active {
        tracing::warn!(holder, path = %path, "broke an active storage mutate lock");
    } else {
        tracing::info!(holder, path = %path, "cleared storage mutate lock");
    }
    println!("Lock cleared.");
    Ok(())
}

fn s3_lock(store: Arc<dyn ObjectStore>, path: ObjPath, token: String, holder: &str) -> StorageLock {
    StorageLock(LockKind::S3 {
        store,
        path,
        token,
        holder: holder.to_string(),
    })
}

fn opts(mode: PutMode) -> PutOptions {
    PutOptions {
        mode,
        ..Default::default()
    }
}

async fn get_lock_object(
    store: &Arc<dyn ObjectStore>,
    path: &ObjPath,
) -> anyhow::Result<Option<(Vec<u8>, ObjectMeta)>> {
    match store.get(path).await {
        Ok(result) => {
            let meta = result.meta.clone();
            let bytes = result.bytes().await?.to_vec();
            Ok(Some((bytes, meta)))
        }
        Err(object_store::Error::NotFound { .. }) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

fn s3_held_message(body: &LockBody) -> String {
    format!(
        "storage mutate lock is held by `{}` on {} (pid {}), acquired {}, lease expires {}. \
         Another `score` or `fix-*` command is running; wait for it to finish. If that \
         process is dead, clear the lock with `pipette-mgmt unlock`.",
        body.holder,
        body.hostname,
        body.pid,
        body.acquired_at.to_rfc3339(),
        body.expires_at.to_rfc3339(),
    )
}

/// Diagnostic only, so `HOSTNAME` with a fallback beats a dependency.
fn hostname() -> String {
    std::env::var("HOSTNAME")
        .ok()
        .filter(|h| !h.is_empty())
        .unwrap_or_else(|| "unknown".to_string())
}

#[cfg(test)]
mod tests {
    use anyhow::Context;
    use object_store::memory::InMemory;

    use super::*;

    fn ttl() -> Duration {
        Duration::from_secs(1800)
    }

    #[tokio::test]
    async fn local_fs_lock_is_exclusive() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let storage = StorageConfig::local_fs(dir.path().to_path_buf());

        let lock = StorageLock::acquire(&storage, "score", ttl()).await?;
        let err = StorageLock::acquire(&storage, "fix-thinking", ttl())
            .await
            .err()
            .context("the second acquire must be refused")?;
        let msg = err.to_string();
        assert!(msg.contains("held by another command"), "got: {msg}");
        assert!(msg.contains("holder=score"), "got: {msg}");

        lock.release().await;
        Ok(())
    }

    #[tokio::test]
    async fn score_eval_lock_is_exclusive_but_independent_of_mutate() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let storage = StorageConfig::local_fs(dir.path().to_path_buf());

        let lock = StorageLock::acquire_named(&storage, "score-eval", "score-eval", ttl()).await?;
        assert!(
            StorageLock::acquire_named(&storage, "score-eval", "score-eval", ttl())
                .await
                .is_err(),
            "a second score-eval run must not overlap the first",
        );
        let mutate = StorageLock::acquire(&storage, "process-submissions", ttl()).await?;
        mutate.release().await;
        lock.release().await;
        Ok(())
    }

    #[tokio::test]
    async fn local_fs_lock_is_reusable_after_release() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let storage = StorageConfig::local_fs(dir.path().to_path_buf());

        let lock = StorageLock::acquire(&storage, "score", ttl()).await?;
        lock.release().await;

        let lock2 = StorageLock::acquire(&storage, "fix-message-type", ttl()).await?;
        lock2.release().await;
        Ok(())
    }

    #[tokio::test]
    async fn local_fs_lock_is_freed_when_dropped_without_release() -> anyhow::Result<()> {
        // Process death closes every fd, so this models a crashed holder.
        let dir = tempfile::tempdir()?;
        let storage = StorageConfig::local_fs(dir.path().to_path_buf());

        {
            let _lock = StorageLock::acquire(&storage, "score", ttl()).await?;
        }
        let lock = StorageLock::acquire(&storage, "fix-thinking", ttl()).await?;
        lock.release().await;
        Ok(())
    }

    #[tokio::test]
    async fn local_fs_unlock_reports_status() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let storage = StorageConfig::local_fs(dir.path().to_path_buf());

        unlock(&storage, false).await?;

        let lock = StorageLock::acquire(&storage, "score", ttl()).await?;
        // unlock while held is informational and must not error.
        unlock(&storage, false).await?;
        lock.release().await;

        unlock(&storage, false).await?;
        Ok(())
    }

    fn mem() -> (Arc<dyn ObjectStore>, ObjPath) {
        (Arc::new(InMemory::new()), ObjPath::from(LOCK_KEY))
    }

    fn lock_body(holder: &str, token: &str, expires_in: chrono::Duration) -> LockBody {
        let now = Utc::now();
        LockBody {
            token: token.to_string(),
            holder: holder.to_string(),
            hostname: "h".to_string(),
            pid: 1,
            acquired_at: now,
            expires_at: now + expires_in,
        }
    }

    async fn write_raw_lock(
        store: &Arc<dyn ObjectStore>,
        path: &ObjPath,
        body: &LockBody,
    ) -> anyhow::Result<()> {
        store
            .put(path, PutPayload::from(serde_json::to_vec(body)?))
            .await?;
        Ok(())
    }

    #[tokio::test]
    async fn s3_second_acquire_is_rejected_while_held() -> anyhow::Result<()> {
        let (store, path) = mem();
        let lock = acquire_s3(store.clone(), path.clone(), "score", ttl()).await?;

        let err = acquire_s3(store.clone(), path.clone(), "fix-thinking", ttl())
            .await
            .err()
            .context("the contended acquire must be refused")?;
        assert!(err.to_string().contains("held by `score`"), "got: {err}");

        lock.release().await;
        Ok(())
    }

    #[tokio::test]
    async fn s3_release_allows_reacquire() -> anyhow::Result<()> {
        let (store, path) = mem();
        let lock = acquire_s3(store.clone(), path.clone(), "score", ttl()).await?;
        lock.release().await;
        assert!(get_lock_object(&store, &path).await?.is_none());

        let lock2 = acquire_s3(store.clone(), path.clone(), "fix-message-type", ttl()).await?;
        lock2.release().await;
        Ok(())
    }

    #[tokio::test]
    async fn s3_expired_lease_is_taken_over() -> anyhow::Result<()> {
        let (store, path) = mem();
        write_raw_lock(
            &store,
            &path,
            &lock_body("score", "old", chrono::Duration::hours(-1)),
        )
        .await?;

        let lock = acquire_s3(store.clone(), path.clone(), "fix-thinking", ttl()).await?;
        lock.release().await;
        Ok(())
    }

    #[tokio::test]
    async fn s3_live_lease_blocks() -> anyhow::Result<()> {
        let (store, path) = mem();
        write_raw_lock(
            &store,
            &path,
            &lock_body("fix-message-type", "live", chrono::Duration::minutes(5)),
        )
        .await?;

        let err = acquire_s3(store.clone(), path.clone(), "score", ttl())
            .await
            .err()
            .context("the contended acquire must be refused")?;
        assert!(err.to_string().contains("fix-message-type"), "got: {err}");
        Ok(())
    }

    #[tokio::test]
    async fn s3_takeover_cas_rejects_a_stale_racer() -> anyhow::Result<()> {
        let (store, path) = mem();
        write_raw_lock(
            &store,
            &path,
            &lock_body("score", "old", chrono::Duration::hours(-1)),
        )
        .await?;

        // A second racer reads the stale version before the takeover.
        let (_, stale_meta) = get_lock_object(&store, &path)
            .await?
            .context("the lock object must be present")?;

        let lock = acquire_s3(store.clone(), path.clone(), "fix-thinking", ttl()).await?;

        let result = store
            .put_opts(
                &path,
                PutPayload::from(b"racer-b".to_vec()),
                opts(PutMode::Update(UpdateVersion {
                    e_tag: stale_meta.e_tag.clone(),
                    version: stale_meta.version.clone(),
                })),
            )
            .await;
        assert!(
            matches!(result, Err(object_store::Error::Precondition { .. })),
            "stale-version takeover must fail with Precondition, got: {result:?}"
        );

        lock.release().await;
        Ok(())
    }

    #[tokio::test]
    async fn s3_release_does_not_delete_a_lock_taken_over_by_another_holder() -> anyhow::Result<()>
    {
        let (store, path) = mem();
        let lock = acquire_s3(store.clone(), path.clone(), "score", ttl()).await?;

        write_raw_lock(
            &store,
            &path,
            &lock_body(
                "fix-thinking",
                "someone-else",
                chrono::Duration::minutes(30),
            ),
        )
        .await?;

        lock.release().await;

        let (bytes, _) = get_lock_object(&store, &path)
            .await?
            .context("the lock object must be present")?;
        let current: LockBody = serde_json::from_slice(&bytes)?;
        assert_eq!(current.token, "someone-else");
        Ok(())
    }

    #[tokio::test]
    async fn s3_unlock_reports_when_nothing_is_held() -> anyhow::Result<()> {
        let (store, path) = mem();
        unlock_s3(&store, &path, false).await?;
        Ok(())
    }

    #[tokio::test]
    async fn s3_unlock_clears_a_stale_lock_without_force() -> anyhow::Result<()> {
        let (store, path) = mem();
        write_raw_lock(
            &store,
            &path,
            &lock_body("score", "old", chrono::Duration::hours(-1)),
        )
        .await?;

        unlock_s3(&store, &path, false).await?;
        assert!(get_lock_object(&store, &path).await?.is_none());
        Ok(())
    }

    #[tokio::test]
    async fn s3_unlock_refuses_an_active_lock_without_force() -> anyhow::Result<()> {
        let (store, path) = mem();
        let lock = acquire_s3(store.clone(), path.clone(), "score", ttl()).await?;

        let err = unlock_s3(&store, &path, false)
            .await
            .err()
            .context("unlock must refuse a live lease")?;
        assert!(err.to_string().contains("--force"), "got: {err}");
        assert!(get_lock_object(&store, &path).await?.is_some());

        unlock_s3(&store, &path, true).await?;
        assert!(get_lock_object(&store, &path).await?.is_none());

        lock.release().await;
        Ok(())
    }
}
