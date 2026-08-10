//! Process-wide advisory lock that serializes the read-modify-write
//! batch commands against each other.
//!
//! `score` and the `fix-*` family all read a Parquet partition (or a
//! submission body), mutate it in memory, and write it back. None of
//! that is coordinated by the storage layer — `for_each_metric_row`
//! and `write_partition_metrics` happily let two processes interleave
//! a read with another process's write, which silently drops rows.
//! This module is the missing mutex: one advisory lock over the
//! storage root that every mutating batch command must hold for its
//! whole run.
//!
//! The lock lives at `<storage-root>/locks/mutate.lock`. The mechanism
//! is backend-specific, because each backend has a different correct
//! primitive:
//!
//! * **`local_fs`** — an exclusive `flock(2)` on the lock file. The
//!   kernel releases the lock when the file descriptor is closed,
//!   including on process death (crash, `kill -9`, panic). A crashed
//!   holder therefore never leaves a stale lock, and there is no
//!   takeover step that could race.
//!
//! * **`s3`** — a lease object. Acquisition is an atomic
//!   create-if-absent (`PutMode::Create` → `If-None-Match: *`). The
//!   body carries an `expires_at`; a holder that dies without
//!   releasing leaves the object behind, and the next command run past
//!   `expires_at` takes the lease over. The takeover is a
//!   compare-and-swap on the object's version (`PutMode::Update`), so
//!   two processes racing the same stale lease cannot both win — the
//!   loser's conditional write fails and it re-reads.
//!
//! The S3 lease TTL is `Config::mutate_lock_ttl_secs`. It must sit
//! comfortably above the longest expected `score` / `fix-*` run: a run
//! that outlives its own lease can have the lock taken over mid-write.
//! `pipette-mgmt unlock` inspects, and where appropriate clears, the
//! lock by hand.

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

use crate::config::StorageConfig;
use crate::stores::build_s3_object_store;

/// Object key / file name of the shared mutate lock, relative to the storage
/// root. Other named locks live at `locks/{name}.lock`.
const LOCK_KEY: &str = "locks/mutate.lock";

/// Name of the shared mutate lock that serializes the read-modify-write
/// commands (`process-submissions`, `fix-*`, `requeue-eval`).
const MUTATE_LOCK: &str = "mutate";

/// Bound on S3 acquire attempts. A lost takeover race resolves in two
/// iterations (one to lose the compare-and-swap, one to see the
/// winner's fresh lease and bail); four leaves margin without looping
/// forever.
const ACQUIRE_ATTEMPTS: usize = 4;

/// On-disk body of an S3 lease, serialized as JSON so an operator (or
/// `pipette-mgmt unlock`) can read who holds it and when it expires.
#[derive(Serialize, Deserialize)]
struct LockBody {
    /// Random per-acquisition id. `release` only deletes the object if
    /// the stored token still matches, so a holder whose lease expired
    /// and was taken over does not delete the new holder's lock.
    token: String,
    /// Command that holds the lock, e.g. `"score"` or `"fix-thinking"`.
    holder: String,
    hostname: String,
    pid: u32,
    acquired_at: DateTime<Utc>,
    expires_at: DateTime<Utc>,
}

/// A held storage mutate lock. Acquired by the read-modify-write batch
/// commands for the duration of their run, and dropped via
/// [`StorageLock::release`].
///
/// For the `local_fs` variant, dropping the value (without calling
/// `release`) still frees the lock — closing the file descriptor
/// releases the `flock`. For the `s3` variant a drop without `release`
/// leaves the lease to expire on its own.
#[derive(Debug)]
pub struct StorageLock(LockKind);

#[derive(Debug)]
enum LockKind {
    /// `local_fs`: an exclusive `flock(2)` is held for as long as
    /// `file` is open. Released by closing it (here or on process
    /// death).
    LocalFs { file: File },
    /// `s3`: a lease object at `path`. Released by deleting it.
    S3 {
        store: Arc<dyn ObjectStore>,
        path: ObjPath,
        token: String,
        holder: String,
    },
}

impl StorageLock {
    /// Acquire the storage mutate lock for `holder`, or fail if another
    /// command currently holds it. `holder` is the command name shown
    /// in the contention message. `ttl` is the S3 lease duration; it is
    /// unused on `local_fs`, where the kernel manages lock lifetime.
    pub async fn acquire(
        storage: &StorageConfig,
        holder: &str,
        ttl: Duration,
    ) -> anyhow::Result<Self> {
        // The shared mutate lock; serializes the read-modify-write commands.
        Self::acquire_named(storage, MUTATE_LOCK, holder, ttl).await
    }

    /// Acquire an independent named advisory lock at `locks/{lock_name}.lock`.
    /// Unlike [`acquire`](Self::acquire), this does *not* touch the shared
    /// mutate lock — used by `score-eval`, which must run as a single instance
    /// (its multi-minute scoring calls must not overlap) without blocking the
    /// warehouse writers that hold `mutate`.
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
                acquire_s3(store, s3_lock_path(&prefix, &lock_key), holder, ttl).await
            }
        }
    }

    /// Release the lock. Best-effort: failures are logged, not
    /// propagated, since the command's real work has already finished.
    pub async fn release(self) {
        match self.0 {
            LockKind::LocalFs { file } => {
                // The exclusive `flock` is released when `file` closes
                // at the end of this scope. Clear the diagnostics first
                // so a later inspection doesn't show a dead holder.
                let _ = file.set_len(0);
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
/// Backs the `pipette-mgmt unlock` subcommand.
pub async fn unlock(storage: &StorageConfig, force: bool) -> anyhow::Result<()> {
    match storage {
        StorageConfig::LocalFs { data_dir } => unlock_local_fs(data_dir),
        StorageConfig::S3 { .. } => {
            let (store, prefix) = build_s3_object_store(storage)?;
            unlock_s3(&store, &s3_lock_path(&prefix, LOCK_KEY), force).await
        }
    }
}

// ---------------------------------------------------------------------------
// local_fs — flock(2)
// ---------------------------------------------------------------------------

fn acquire_local_fs(data_dir: &Path, lock_key: &str, holder: &str) -> anyhow::Result<StorageLock> {
    let path = data_dir.join(lock_key);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // `truncate(false)`: a contender must be able to read the live
    // holder's diagnostics; the acquirer truncates only after it wins
    // the `flock` below.
    let mut file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(&path)?;

    match try_flock_exclusive(&file) {
        FlockResult::Acquired => {}
        FlockResult::Held => {
            let info = std::fs::read_to_string(&path).unwrap_or_default();
            anyhow::bail!("{}", local_held_message(&path, &info));
        }
        FlockResult::Error(e) => {
            return Err(anyhow::anyhow!("failed to flock {}: {e}", path.display()));
        }
    }

    // We hold the lock. Record diagnostics for any contender's message.
    file.set_len(0)?;
    let _ = file.write_all(local_info_line(holder).as_bytes());
    tracing::info!(holder, path = %path.display(), "acquired storage lock");
    Ok(StorageLock(LockKind::LocalFs { file }))
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
            // Nothing holds it. On `local_fs` the kernel releases the
            // lock when the holder dies, so there is never a stale lock
            // to clear — `--force` would have nothing to do.
            println!("No command is holding the storage mutate lock.");
            Ok(())
        }
        FlockResult::Held => {
            let info = std::fs::read_to_string(&path).unwrap_or_default();
            let who = if info.trim().is_empty() {
                "details unavailable"
            } else {
                info.trim()
            };
            println!("The storage mutate lock is held by a running command [{who}].");
            println!(
                "On the local_fs backend the lock is released automatically when that \
                 process exits or dies — there is nothing to clear by hand."
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

/// Take a non-blocking exclusive `flock` on `file`. The lock is bound
/// to the open file description and released when every descriptor
/// referencing it is closed — including when the process dies.
fn try_flock_exclusive(file: &File) -> FlockResult {
    // SAFETY: `flock` is a POSIX syscall; `file` owns a valid fd and
    // outlives the call. `LOCK_NB` makes a contended lock return
    // `EWOULDBLOCK` instead of blocking the process.
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
         `fix-*` command is running — wait for it to finish. A crashed holder's lock is \
         released automatically.",
        path.display(),
        who,
    )
}

// ---------------------------------------------------------------------------
// s3 — lease object
// ---------------------------------------------------------------------------

fn s3_lock_path(prefix: &str, lock_key: &str) -> ObjPath {
    if prefix.is_empty() {
        ObjPath::from(lock_key)
    } else {
        ObjPath::from(format!("{prefix}/{lock_key}"))
    }
}

async fn acquire_s3(
    store: Arc<dyn ObjectStore>,
    path: ObjPath,
    holder: &str,
    ttl: Duration,
) -> anyhow::Result<StorageLock> {
    let token = uuid::Uuid::new_v4().to_string();
    let ttl = chrono::Duration::seconds(ttl.as_secs() as i64);

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
            // The lock object already exists: it is either a live lease
            // (bail) or a stale one left by a crashed holder (take it
            // over with a compare-and-swap).
            Err(object_store::Error::AlreadyExists { .. }) => {
                let Some((existing_bytes, meta)) = get_lock_object(&store, &path).await? else {
                    // Released between our PUT and our GET — retry.
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
                // Compare-and-swap on the object's version: only the
                // process whose `Update` matches the stale object's
                // version wins. A racer that read the same stale
                // version gets `Precondition` and re-reads.
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
                    // Lost the takeover race, or the object changed /
                    // vanished — re-read on the next iteration.
                    Err(object_store::Error::Precondition { .. }) => continue,
                    Err(e) => return Err(e.into()),
                }
            }
            Err(e) => return Err(e.into()),
        }
    }

    anyhow::bail!(
        "could not acquire storage mutate lock at {path} after {ACQUIRE_ATTEMPTS} attempts \
         — another command kept re-taking it"
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

    match serde_json::from_slice::<LockBody>(&bytes) {
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
                    "lock lease is still active — a `score` or `fix-*` command may be \
                     running. Re-run `unlock --force` to break it anyway."
                );
            }
        }
        Err(e) => {
            println!("Storage mutate lock at {path} is present but unreadable: {e}");
            if !force {
                anyhow::bail!("re-run `unlock --force` to remove the unreadable lock object");
            }
        }
    }

    store.delete(path).await?;
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
         Another `score` or `fix-*` command is running — wait for it to finish. If that \
         process is dead, clear the lock with `pipette-mgmt unlock`.",
        body.holder,
        body.hostname,
        body.pid,
        body.acquired_at.to_rfc3339(),
        body.expires_at.to_rfc3339(),
    )
}

// ---------------------------------------------------------------------------
// shared
// ---------------------------------------------------------------------------

/// Best-effort hostname for the contention message. Diagnostic only —
/// `pid` and `holder` carry the load — so an env-var lookup with an
/// `"unknown"` fallback is enough and avoids a dependency.
fn hostname() -> String {
    std::env::var("HOSTNAME")
        .ok()
        .filter(|h| !h.is_empty())
        .unwrap_or_else(|| "unknown".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use object_store::memory::InMemory;

    fn ttl() -> Duration {
        Duration::from_secs(1800)
    }

    // ---- local_fs (flock) -------------------------------------------------

    #[tokio::test]
    async fn local_fs_lock_is_exclusive() {
        let dir = tempfile::tempdir().unwrap();
        let storage = StorageConfig::local_fs(dir.path().to_path_buf());

        let lock = StorageLock::acquire(&storage, "score", ttl())
            .await
            .unwrap();
        let err = StorageLock::acquire(&storage, "fix-thinking", ttl())
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("held by another command"), "got: {msg}");
        // The contention message names the holder from the lock file.
        assert!(msg.contains("holder=score"), "got: {msg}");

        lock.release().await;
    }

    /// The `score-eval` lock serializes score-eval instances (two long runs
    /// can't overlap) but is a different object from the shared mutate lock,
    /// so it doesn't block the warehouse writers.
    #[tokio::test]
    async fn score_eval_lock_is_exclusive_but_independent_of_mutate() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let storage = StorageConfig::local_fs(dir.path().to_path_buf());

        let lock = StorageLock::acquire_named(&storage, "score-eval", "score-eval", ttl()).await?;
        // A second score-eval run is refused while the first holds the lock.
        assert!(
            StorageLock::acquire_named(&storage, "score-eval", "score-eval", ttl())
                .await
                .is_err(),
            "a second score-eval run must not overlap the first",
        );
        // But the mutate lock is a separate object — process-submissions etc.
        // can still run while score-eval holds its lock.
        let mutate = StorageLock::acquire(&storage, "process-submissions", ttl()).await?;
        mutate.release().await;
        lock.release().await;
        Ok(())
    }

    #[tokio::test]
    async fn local_fs_lock_is_reusable_after_release() {
        let dir = tempfile::tempdir().unwrap();
        let storage = StorageConfig::local_fs(dir.path().to_path_buf());

        let lock = StorageLock::acquire(&storage, "score", ttl())
            .await
            .unwrap();
        lock.release().await;

        let lock2 = StorageLock::acquire(&storage, "fix-message-type", ttl())
            .await
            .unwrap();
        lock2.release().await;
    }

    #[tokio::test]
    async fn local_fs_lock_is_freed_when_dropped_without_release() {
        // Closing the file descriptor releases the flock — this models
        // the crash case (process death closes every fd). Dropping the
        // `StorageLock` without calling `release` must not wedge it.
        let dir = tempfile::tempdir().unwrap();
        let storage = StorageConfig::local_fs(dir.path().to_path_buf());

        {
            let _lock = StorageLock::acquire(&storage, "score", ttl())
                .await
                .unwrap();
        }
        let lock = StorageLock::acquire(&storage, "fix-thinking", ttl())
            .await
            .unwrap();
        lock.release().await;
    }

    #[tokio::test]
    async fn local_fs_unlock_reports_status() {
        let dir = tempfile::tempdir().unwrap();
        let storage = StorageConfig::local_fs(dir.path().to_path_buf());

        // Nothing acquired yet.
        unlock(&storage, false).await.unwrap();

        let lock = StorageLock::acquire(&storage, "score", ttl())
            .await
            .unwrap();
        // unlock while held is informational and must not error.
        unlock(&storage, false).await.unwrap();
        lock.release().await;

        // After release the lock is free again.
        unlock(&storage, false).await.unwrap();
    }

    // ---- s3 (lease) -------------------------------------------------------

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

    async fn write_raw_lock(store: &Arc<dyn ObjectStore>, path: &ObjPath, body: &LockBody) {
        store
            .put(path, PutPayload::from(serde_json::to_vec(body).unwrap()))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn s3_second_acquire_is_rejected_while_held() {
        let (store, path) = mem();
        let lock = acquire_s3(store.clone(), path.clone(), "score", ttl())
            .await
            .unwrap();

        let err = acquire_s3(store.clone(), path.clone(), "fix-thinking", ttl())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("held by `score`"), "got: {err}");

        lock.release().await;
    }

    #[tokio::test]
    async fn s3_release_allows_reacquire() {
        let (store, path) = mem();
        let lock = acquire_s3(store.clone(), path.clone(), "score", ttl())
            .await
            .unwrap();
        lock.release().await;
        assert!(get_lock_object(&store, &path).await.unwrap().is_none());

        let lock2 = acquire_s3(store.clone(), path.clone(), "fix-message-type", ttl())
            .await
            .unwrap();
        lock2.release().await;
    }

    #[tokio::test]
    async fn s3_expired_lease_is_taken_over() {
        let (store, path) = mem();
        write_raw_lock(
            &store,
            &path,
            &lock_body("score", "old", chrono::Duration::hours(-1)),
        )
        .await;

        let lock = acquire_s3(store.clone(), path.clone(), "fix-thinking", ttl())
            .await
            .unwrap();
        lock.release().await;
    }

    #[tokio::test]
    async fn s3_live_lease_blocks() {
        let (store, path) = mem();
        write_raw_lock(
            &store,
            &path,
            &lock_body("fix-message-type", "live", chrono::Duration::minutes(5)),
        )
        .await;

        let err = acquire_s3(store.clone(), path.clone(), "score", ttl())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("fix-message-type"), "got: {err}");
    }

    /// The core race-safety guarantee for the stale-lease takeover:
    /// once one process compare-and-swaps a stale lease, a second
    /// process still holding the pre-takeover version cannot also win.
    #[tokio::test]
    async fn s3_takeover_cas_rejects_a_stale_racer() {
        let (store, path) = mem();
        write_raw_lock(
            &store,
            &path,
            &lock_body("score", "old", chrono::Duration::hours(-1)),
        )
        .await;

        // A racer reads the stale object's version before any takeover.
        let (_, stale_meta) = get_lock_object(&store, &path).await.unwrap().unwrap();

        // The first racer takes the lease over (a fresh compare-and-swap).
        let lock = acquire_s3(store.clone(), path.clone(), "fix-thinking", ttl())
            .await
            .unwrap();

        // The second racer, still holding the pre-takeover version,
        // attempts its own conditional write — it must be rejected.
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
    }

    #[tokio::test]
    async fn s3_release_does_not_delete_a_lock_taken_over_by_another_holder() {
        let (store, path) = mem();
        let lock = acquire_s3(store.clone(), path.clone(), "score", ttl())
            .await
            .unwrap();

        // Simulate the lease being taken over: a different holder's
        // body now sits at the lock path.
        write_raw_lock(
            &store,
            &path,
            &lock_body(
                "fix-thinking",
                "someone-else",
                chrono::Duration::minutes(30),
            ),
        )
        .await;

        lock.release().await;

        // The other holder's lock survives our (stale) release.
        let (bytes, _) = get_lock_object(&store, &path).await.unwrap().unwrap();
        let current: LockBody = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(current.token, "someone-else");
    }

    #[tokio::test]
    async fn s3_unlock_reports_when_nothing_is_held() {
        let (store, path) = mem();
        unlock_s3(&store, &path, false).await.unwrap();
    }

    #[tokio::test]
    async fn s3_unlock_clears_a_stale_lock_without_force() {
        let (store, path) = mem();
        write_raw_lock(
            &store,
            &path,
            &lock_body("score", "old", chrono::Duration::hours(-1)),
        )
        .await;

        unlock_s3(&store, &path, false).await.unwrap();
        assert!(get_lock_object(&store, &path).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn s3_unlock_refuses_an_active_lock_without_force() {
        let (store, path) = mem();
        let lock = acquire_s3(store.clone(), path.clone(), "score", ttl())
            .await
            .unwrap();

        let err = unlock_s3(&store, &path, false).await.unwrap_err();
        assert!(err.to_string().contains("--force"), "got: {err}");
        // The lock is left intact for the still-running command.
        assert!(get_lock_object(&store, &path).await.unwrap().is_some());

        unlock_s3(&store, &path, true).await.unwrap();
        assert!(get_lock_object(&store, &path).await.unwrap().is_none());

        lock.release().await;
    }
}
