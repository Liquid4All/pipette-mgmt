use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};

use flate2::Compression;
use flate2::read::GzDecoder;
use flate2::write::GzEncoder;

use crate::types::{ClientId, JobId};

// ---------------------------------------------------------------------------
// Path helpers
// ---------------------------------------------------------------------------

/// `{dir}/{job_id}.json` — the path of a plain JSON submission body in any
/// submission directory (`incoming/`, a `score-queue/` stage, …).
pub fn submission_path(dir: &Path, job_id: &JobId) -> PathBuf {
    dir.join(format!("{job_id}.json"))
}

/// `{processed_dir}/{job_id}.json.gz`
pub fn processed_path(processed_dir: &Path, job_id: &JobId) -> PathBuf {
    processed_dir.join(format!("{job_id}.json.gz"))
}

// ---------------------------------------------------------------------------
// Read helpers
// ---------------------------------------------------------------------------

/// Try to read a plain JSON submission body at `{dir}/{job_id}.json`.
pub(crate) fn read_submission_json(
    dir: &Path,
    job_id: &JobId,
) -> anyhow::Result<Option<serde_json::Value>> {
    let path = submission_path(dir, job_id);
    if !path.exists() {
        return Ok(None);
    }
    let f = std::fs::File::open(&path)?;
    Ok(Some(serde_json::from_reader(f)?))
}

/// Try to read a processed submission at `{processed_dir}/{job_id}.json.gz`.
pub(crate) fn read_processed_json(
    processed_dir: &Path,
    job_id: &JobId,
) -> anyhow::Result<Option<serde_json::Value>> {
    let path = processed_path(processed_dir, job_id);
    if !path.exists() {
        return Ok(None);
    }
    let f = std::fs::File::open(&path)?;
    Ok(Some(serde_json::from_reader(GzDecoder::new(f))?))
}

// ---------------------------------------------------------------------------
// Writes / transitions
// ---------------------------------------------------------------------------

/// RAII guard that removes a temp file on drop unless `disarm`ed.
/// Used by every "write to tmp, then rename into place" path to keep
/// failed writes from leaking `.tmp-*` orphans.
pub(crate) struct TmpFileGuard {
    path: PathBuf,
    armed: bool,
}

impl TmpFileGuard {
    pub(crate) fn new(path: PathBuf) -> Self {
        Self { path, armed: true }
    }
    pub(crate) fn disarm(mut self) {
        self.armed = false;
    }
}

impl Drop for TmpFileGuard {
    fn drop(&mut self) {
        if self.armed {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// Atomic write (temp file + rename) of a JSON submission body to
/// `{dir}/{job_id}.json`. Used for `incoming/` and every `score-queue/` stage.
pub fn write_submission(
    dir: &Path,
    job_id: &JobId,
    data: &serde_json::Value,
) -> anyhow::Result<()> {
    std::fs::create_dir_all(dir)?;
    let final_path = submission_path(dir, job_id);
    let tmp = dir.join(format!(".tmp-{}.tmp", uuid::Uuid::new_v4()));
    let guard = TmpFileGuard::new(tmp.clone());
    let content = serde_json::to_string_pretty(data)?;
    std::fs::write(&tmp, content)?;
    std::fs::rename(&tmp, &final_path)?;
    guard.disarm();
    Ok(())
}

/// Atomic write of a gzipped JSON submission directly to
/// `{processed_dir}/{job_id}.json.gz` — used for failure
/// submissions, which land in their terminal state at write time
/// (no incoming detour, because the scorer has nothing to do with
/// them).
pub(crate) fn write_processed_direct(
    processed_dir: &Path,
    job_id: &JobId,
    data: &serde_json::Value,
) -> anyhow::Result<()> {
    std::fs::create_dir_all(processed_dir)?;
    let final_path = processed_path(processed_dir, job_id);
    let tmp = processed_dir.join(format!(".tmp-{}.tmp", uuid::Uuid::new_v4()));
    let guard = TmpFileGuard::new(tmp.clone());

    {
        let content = serde_json::to_vec(data)?;
        let output = std::fs::File::create(&tmp)?;
        let mut encoder = GzEncoder::new(output, Compression::default());
        std::io::Write::write_all(&mut encoder, &content)?;
        encoder.finish()?;
    }

    std::fs::rename(&tmp, &final_path)?;
    guard.disarm();
    Ok(())
}

/// Move `{incoming_dir}/{job_id}.json` to
/// `{processed_dir}/{job_id}.json.gz`, gzipping along the way.
///
/// Atomic boundary is the rename of the final `.json.gz` into place; if
/// we crash after the rename but before removing the incoming file the
/// next `mark_processed` cycle re-reads incoming, overwrites the
/// processed file (rename is atomic over an existing target), and
/// removes incoming. Processing is at-least-once but idempotent.
pub(crate) fn compress_submission_to_processed(
    incoming_dir: &Path,
    processed_dir: &Path,
    job_id: &JobId,
) -> anyhow::Result<()> {
    let incoming = submission_path(incoming_dir, job_id);
    std::fs::create_dir_all(processed_dir)?;
    let tmp = processed_dir.join(format!(".tmp-{}.tmp", uuid::Uuid::new_v4()));
    let final_path = processed_path(processed_dir, job_id);
    let guard = TmpFileGuard::new(tmp.clone());

    {
        let mut input = std::fs::File::open(&incoming)?;
        let output = std::fs::File::create(&tmp)?;
        let mut encoder = GzEncoder::new(output, Compression::default());
        std::io::copy(&mut input, &mut encoder)?;
        encoder.finish()?;
    }

    std::fs::rename(&tmp, &final_path)?;
    guard.disarm();
    std::fs::remove_file(&incoming)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Listing / search
// ---------------------------------------------------------------------------

/// Walk `{dir}/*.json` and return up to `limit` `JobId`s, skipping the
/// `.tmp-*` files an interrupted atomic write may leave behind. Iteration is
/// `read_dir` order; callers must not rely on time order. Used for `incoming/`
/// and every `score-queue/` stage.
pub fn list_submission_job_ids(dir: &Path, limit: NonZeroUsize) -> anyhow::Result<Vec<JobId>> {
    let mut jobs = Vec::new();
    if !dir.exists() {
        return Ok(jobs);
    }
    let limit = limit.get();
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let path = entry.path();
        let Some(file_name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        // Skip temp files left behind by an interrupted atomic write.
        if file_name.starts_with('.') {
            continue;
        }
        let Some(job_id) = file_name.strip_suffix(".json") else {
            continue;
        };
        // Filenames are written from validated ids, so a bad one is corruption/
        // tampering — skip it (logged) rather than fail the whole listing.
        let Ok(job_id) = JobId::try_new(job_id).inspect_err(|e| {
            tracing::warn!(file = %file_name, error = %e, "skipping incoming file with invalid job_id");
        }) else {
            continue;
        };
        jobs.push(job_id);
        if jobs.len() >= limit {
            break;
        }
    }
    Ok(jobs)
}

/// `{unverified_dir}/{client_id}/{job_id}.json` — held submission path
/// for one (client, job).
pub fn unverified_path(unverified_dir: &Path, client_id: &ClientId, job_id: &JobId) -> PathBuf {
    unverified_dir
        .join(client_id.as_str())
        .join(format!("{job_id}.json"))
}

/// Whether a directory entry is a held-submission `*.json` object
/// (excludes temp files from interrupted atomic writes).
fn is_unverified_object(file_name: &str) -> bool {
    !file_name.starts_with('.') && file_name.ends_with(".json")
}

/// Delete held submissions across every `{unverified_dir}/{client_id}/`
/// subtree whose filesystem `mtime` is older than `older_than`. Returns
/// `(deleted, kept)` counts. On `dry_run`, nothing is removed but the
/// counts still reflect what would be. A missing directory is treated
/// as empty.
///
/// Threshold is the file `mtime`, not the payload's `submitted_at` —
/// the unverified tree is opaque to the application, so age is judged
/// by the object, matching the S3 `LastModified` semantics. Empty
/// client subdirectories left behind by a live prune are removed.
pub fn prune_unverified_dir(
    unverified_dir: &Path,
    older_than: std::time::Duration,
    dry_run: bool,
) -> anyhow::Result<(usize, usize)> {
    if !unverified_dir.exists() {
        return Ok((0, 0));
    }
    // Saturate at the epoch for absurdly large thresholds so the
    // subtraction can't panic; a file can't predate it anyway.
    let cutoff = std::time::SystemTime::now()
        .checked_sub(older_than)
        .unwrap_or(std::time::UNIX_EPOCH);
    // Outer fold over client dirs, inner fold over each client's files;
    // the `(deleted, kept)` tuple threads through both. `try_fold`
    // propagates the first I/O error rather than dropping it, and the
    // per-client `remove_dir` cleanup runs between the inner folds.
    std::fs::read_dir(unverified_dir)?.try_fold(
        (0usize, 0usize),
        |acc, client_entry| -> anyhow::Result<(usize, usize)> {
            let client_entry = client_entry?;
            if !client_entry.file_type()?.is_dir() {
                return Ok(acc);
            }
            let client_dir = client_entry.path();
            let acc = std::fs::read_dir(&client_dir)?.try_fold(
                acc,
                |(deleted, kept), entry| -> anyhow::Result<(usize, usize)> {
                    let entry = entry?;
                    if !entry.file_type()?.is_file() {
                        return Ok((deleted, kept));
                    }
                    let path = entry.path();
                    let Some(file_name) = path.file_name().and_then(|n| n.to_str()) else {
                        return Ok((deleted, kept));
                    };
                    if !is_unverified_object(file_name) {
                        return Ok((deleted, kept));
                    }
                    if entry.metadata()?.modified()? < cutoff {
                        if !dry_run {
                            std::fs::remove_file(&path)?;
                        }
                        Ok((deleted + 1, kept))
                    } else {
                        Ok((deleted, kept + 1))
                    }
                },
            )?;
            // Best-effort cleanup of a now-empty client dir (live runs only).
            if !dry_run {
                let _ = std::fs::remove_dir(&client_dir);
            }
            Ok(acc)
        },
    )
}

/// List one client's held submissions as `(job_id, body)` pairs from
/// `{unverified_dir}/{client_id}/`. A missing directory yields an empty
/// vec.
pub fn list_unverified_client_dir(
    unverified_dir: &Path,
    client_id: &ClientId,
) -> anyhow::Result<Vec<(JobId, serde_json::Value)>> {
    let client_dir = unverified_dir.join(client_id.as_str());
    let read = match std::fs::read_dir(&client_dir) {
        Ok(r) => r,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    };
    // `map -> Ok(None)` skips non-objects; `Result::transpose` turns the
    // skips into `None` (dropped by `filter_map`) while a real I/O / parse
    // error stays `Some(Err(_))` and fails the final `collect`.
    read.map(
        |entry| -> anyhow::Result<Option<(JobId, serde_json::Value)>> {
            let entry = entry?;
            if !entry.file_type()?.is_file() {
                return Ok(None);
            }
            let path = entry.path();
            let Some(file_name) = path.file_name().and_then(|n| n.to_str()) else {
                return Ok(None);
            };
            if !is_unverified_object(file_name) {
                return Ok(None);
            }
            // Filenames are written from validated ids, so a bad one is
            // corruption/tampering — skip it (logged) rather than fail the
            // whole listing.
            let Ok(job_id) = JobId::try_new(file_name.strip_suffix(".json").unwrap_or(file_name))
                .inspect_err(|e| {
                    tracing::warn!(file = %file_name, error = %e, "skipping unverified file with invalid job_id");
                })
            else {
                return Ok(None);
            };
            let body = serde_json::from_slice(&std::fs::read(&path)?)?;
            Ok(Some((job_id, body)))
        },
    )
    .filter_map(Result::transpose)
    .collect()
}

/// Delete one held-submission object. A missing file is not an error
/// (idempotent — `promote` may race a concurrent prune).
pub fn delete_unverified_object(
    unverified_dir: &Path,
    client_id: &ClientId,
    job_id: &JobId,
) -> anyhow::Result<()> {
    let path = unverified_path(unverified_dir, client_id, job_id);
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    }
}

/// Delete a whole client's held-submission subtree, returning the count
/// of objects removed. On `dry_run`, nothing is removed but the count
/// still reflects what would be. A missing directory yields `0`.
pub fn delete_unverified_client_dir(
    unverified_dir: &Path,
    client_id: &ClientId,
    dry_run: bool,
) -> anyhow::Result<usize> {
    let client_dir = unverified_dir.join(client_id.as_str());
    let mut read = match std::fs::read_dir(&client_dir) {
        Ok(r) => r,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(e.into()),
    };
    // `try_fold` carries the count as the accumulator and short-circuits on
    // the first I/O error exactly like `?` in a loop — no error is dropped.
    let deleted = read.try_fold(0usize, |n, entry| -> anyhow::Result<usize> {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            return Ok(n);
        }
        let path = entry.path();
        let Some(file_name) = path.file_name().and_then(|n| n.to_str()) else {
            return Ok(n);
        };
        if !is_unverified_object(file_name) {
            return Ok(n);
        }
        if !dry_run {
            std::fs::remove_file(&path)?;
        }
        Ok(n + 1)
    })?;
    if !dry_run {
        let _ = std::fs::remove_dir(&client_dir);
    }
    Ok(deleted)
}

/// Find a single job by `job_id`, searching `incoming/` first then
/// every bucket under `processed/`. Returns `(body, state_label)` where
/// `state_label` is `"incoming"` or `"processed"`.
pub fn find_job(
    incoming_dir: &Path,
    processed_dir: &Path,
    job_id: &JobId,
) -> anyhow::Result<Option<(serde_json::Value, &'static str)>> {
    if let Some(val) = read_submission_json(incoming_dir, job_id)? {
        return Ok(Some((val, "incoming")));
    }
    if let Some(val) = read_processed_json(processed_dir, job_id)? {
        return Ok(Some((val, "processed")));
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use crate::TEST_LIST_LIMIT;
    use crate::storage::*;
    use anyhow::Context;

    #[test]
    fn test_submission_path_is_flat() {
        let base = Path::new("/data/submissions/incoming");
        assert_eq!(
            submission_path(base, &JobId::new_unchecked("550e8400")),
            PathBuf::from("/data/submissions/incoming/550e8400.json")
        );
    }

    #[test]
    fn test_processed_path_is_flat() {
        let base = Path::new("/data/submissions/processed");
        assert_eq!(
            processed_path(base, &JobId::new_unchecked("550e8400")),
            PathBuf::from("/data/submissions/processed/550e8400.json.gz")
        );
    }

    #[test]
    fn test_write_and_list_submissions() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let incoming = dir.path().join("incoming");
        let data = serde_json::json!({"job_id": "job1", "benchmark_id": "test"});
        write_submission(&incoming, &JobId::new_unchecked("job1"), &data)?;

        let jobs = list_submission_job_ids(&incoming, TEST_LIST_LIMIT)?;
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0], JobId::new_unchecked("job1"));
        Ok(())
    }

    #[test]
    fn test_list_submission_job_ids_respects_limit() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let incoming = dir.path().join("incoming");
        for i in 0..5 {
            write_submission(
                &incoming,
                &JobId::new_unchecked(format!("job{i}")),
                &serde_json::json!({"job_id": format!("job{i}")}),
            )?;
        }
        let three = NonZeroUsize::new(3).expect("3 is non-zero");
        let ten = NonZeroUsize::new(10).expect("10 is non-zero");
        assert_eq!(list_submission_job_ids(&incoming, three)?.len(), 3);
        assert_eq!(list_submission_job_ids(&incoming, ten)?.len(), 5);
        Ok(())
    }

    #[test]
    fn test_list_incoming_skips_tmp_files() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let incoming = dir.path().join("incoming");
        std::fs::create_dir_all(&incoming)?;
        // An interrupted atomic write may leave a `.tmp-*.tmp` orphan.
        std::fs::write(incoming.join(".tmp-orphan.tmp"), b"partial")?;
        write_submission(
            &incoming,
            &JobId::new_unchecked("job1"),
            &serde_json::json!({"job_id": "job1"}),
        )?;
        let jobs = list_submission_job_ids(&incoming, TEST_LIST_LIMIT)?;
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0], JobId::new_unchecked("job1"));
        Ok(())
    }

    #[test]
    fn test_find_job() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let incoming = dir.path().join("incoming");
        let processed = dir.path().join("processed");

        let data = serde_json::json!({"job_id": "job1", "client_id": "ev1_c"});
        write_submission(&incoming, &JobId::new_unchecked("job1"), &data)?;

        let (val, state) = find_job(&incoming, &processed, &JobId::new_unchecked("job1"))?
            .context("expected job")?;
        assert_eq!(state, "incoming");
        assert_eq!(val["job_id"], "job1");

        assert!(find_job(&incoming, &processed, &JobId::new_unchecked("nonexistent"))?.is_none());
        Ok(())
    }

    #[test]
    fn test_read_processed_json_returns_decoded_value() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let raw = serde_json::json!({"job_id": "job1", "data": "hello"}).to_string();
        let mut enc = GzEncoder::new(Vec::new(), Compression::default());
        std::io::Write::write_all(&mut enc, raw.as_bytes())?;
        std::fs::write(dir.path().join("job1.json.gz"), enc.finish()?)?;

        let val = read_processed_json(dir.path(), &JobId::new_unchecked("job1"))?
            .context("expected Some")?;
        assert_eq!(val["data"], "hello");
        Ok(())
    }

    #[test]
    fn test_read_helpers_none_when_missing() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        assert!(read_processed_json(dir.path(), &JobId::new_unchecked("nope"))?.is_none());
        assert!(read_submission_json(dir.path(), &JobId::new_unchecked("nope"))?.is_none());
        Ok(())
    }

    #[test]
    fn test_compress_submission_to_processed_round_trip() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let incoming_dir = dir.path().join("incoming");
        let processed_dir = dir.path().join("processed");
        std::fs::create_dir_all(&incoming_dir)?;

        let job_id = JobId::new_unchecked("job1");
        let raw = r#"{"job_id":"job1","data":"hello"}"#;
        let incoming = submission_path(&incoming_dir, &job_id);
        std::fs::write(&incoming, raw)?;

        compress_submission_to_processed(&incoming_dir, &processed_dir, &job_id)?;

        assert!(!incoming.exists());
        assert!(processed_dir.join("job1.json.gz").exists());
        let leftover_tmp: Vec<_> = std::fs::read_dir(&processed_dir)?
            .filter_map(Result::ok)
            .filter(|e| e.file_name().to_string_lossy().starts_with(".tmp-"))
            .collect();
        assert!(
            leftover_tmp.is_empty(),
            "{} tmp files left",
            leftover_tmp.len()
        );

        let val = read_processed_json(&processed_dir, &job_id)?.context("expected Some")?;
        assert_eq!(val["data"], "hello");
        Ok(())
    }

    #[test]
    fn test_compress_submission_cleans_tmp_on_error() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let processed_dir = dir.path().join("processed");
        let empty_incoming = dir.path().join("incoming");
        std::fs::create_dir_all(&empty_incoming)?;

        let result = compress_submission_to_processed(
            &empty_incoming,
            &processed_dir,
            &JobId::new_unchecked("job1"),
        );
        assert!(result.is_err());

        // processed_dir is created on entry; the failing compression
        // shouldn't leave .tmp orphans.
        let leftover_tmp: Vec<_> = std::fs::read_dir(&processed_dir)?
            .filter_map(Result::ok)
            .filter(|e| e.file_name().to_string_lossy().starts_with(".tmp-"))
            .collect();
        assert!(
            leftover_tmp.is_empty(),
            "{} tmp files left behind",
            leftover_tmp.len()
        );
        Ok(())
    }
}
