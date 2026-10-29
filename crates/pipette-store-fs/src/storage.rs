use std::io::BufReader;
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};

use flate2::Compression;
use flate2::read::GzDecoder;
use flate2::write::GzEncoder;

use pipette_core::tmp_file::TmpFileGuard;
use pipette_plan_types::{ClientId, JobId, RuntimeOutput};
use pipette_store::CreateOutcome;

// ---------------------------------------------------------------------------
// Path helpers
// ---------------------------------------------------------------------------

/// `{dir}/{job_id}.json` in any submission directory.
pub fn submission_path(dir: &Path, job_id: &JobId) -> PathBuf {
    dir.join(format!("{job_id}.json"))
}

/// `{processed_dir}/{job_id}.json.gz`
pub fn processed_path(processed_dir: &Path, job_id: &JobId) -> PathBuf {
    processed_dir.join(format!("{job_id}.json.gz"))
}

/// `{runtime_output_dir}/{job_id}/`, which holds one object per client.
fn runtime_output_job_dir(runtime_output_dir: &Path, job_id: &JobId) -> PathBuf {
    runtime_output_dir.join(job_id.to_string())
}

// ---------------------------------------------------------------------------
// Read helpers
// ---------------------------------------------------------------------------

pub(crate) fn read_submission_json(
    dir: &Path,
    job_id: &JobId,
) -> anyhow::Result<Option<serde_json::Value>> {
    read_body_as(&submission_path(dir, job_id), false)
}

pub(crate) fn read_processed_json(
    processed_dir: &Path,
    job_id: &JobId,
) -> anyhow::Result<Option<serde_json::Value>> {
    read_body_as(&processed_path(processed_dir, job_id), true)
}

/// `None` if no file is at `path`. Streams, so a `T` with few fields reads a
/// large body without holding it.
pub(crate) fn read_body_as<T: serde::de::DeserializeOwned>(
    path: &Path,
    gz: bool,
) -> anyhow::Result<Option<T>> {
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    // `serde_json` reads one byte at a time, so each reader gets a buffer.
    let body = if gz {
        serde_json::from_reader(BufReader::new(GzDecoder::new(file)))?
    } else {
        serde_json::from_reader(BufReader::new(file))?
    };
    Ok(Some(body))
}

// ---------------------------------------------------------------------------
// Writes / transitions
// ---------------------------------------------------------------------------

/// Write `dest` through a `.tmp-*` sibling and a rename, so a reader never
/// sees a partial file; the guard reaps the sibling if `write` fails first.
pub(crate) fn replace_atomically(
    dir: &Path,
    dest: &Path,
    write: impl FnOnce(&Path) -> std::io::Result<()>,
) -> anyhow::Result<()> {
    std::fs::create_dir_all(dir)?;
    let tmp = dir.join(format!(".tmp-{}.tmp", uuid::Uuid::new_v4()));
    let guard = TmpFileGuard::new(tmp.clone());
    write(&tmp)?;
    std::fs::rename(&tmp, dest)?;
    guard.disarm();
    Ok(())
}

/// Create `dest` unless it exists. Staged and hard linked, so a loser never
/// reads the empty file a bare `create_new` leaves before the winner's write.
pub(crate) fn create_complete(
    dir: &Path,
    dest: &Path,
    contents: &[u8],
) -> anyhow::Result<CreateOutcome> {
    std::fs::create_dir_all(dir)?;
    let tmp = dir.join(format!(".tmp-{}.tmp", uuid::Uuid::new_v4()));
    // Never disarmed: once linked, the staging name is a spare second link.
    let _guard = TmpFileGuard::new(tmp.clone());
    std::fs::write(&tmp, contents)?;
    match std::fs::hard_link(&tmp, dest) {
        Ok(()) => Ok(CreateOutcome::Created),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(CreateOutcome::AlreadyExists),
        Err(e) => Err(e.into()),
    }
}

pub fn write_submission(
    dir: &Path,
    job_id: &JobId,
    data: &serde_json::Value,
) -> anyhow::Result<()> {
    let content = serde_json::to_string_pretty(data)?;
    replace_atomically(dir, &submission_path(dir, job_id), |tmp| {
        std::fs::write(tmp, content)
    })
}

/// Archives a body in its terminal state: a scored eval body, or a held
/// failure that `unverified promote` archives.
pub(crate) fn write_processed_direct(
    processed_dir: &Path,
    job_id: &JobId,
    data: &serde_json::Value,
) -> anyhow::Result<()> {
    write_gz_json(processed_dir, &processed_path(processed_dir, job_id), data)
}

pub(crate) fn write_runtime_output(
    runtime_output_dir: &Path,
    job_id: &JobId,
    client_id: &ClientId,
    output: &RuntimeOutput,
) -> anyhow::Result<()> {
    let dir = runtime_output_job_dir(runtime_output_dir, job_id);
    let dest = dir.join(format!("{client_id}.json.gz"));
    write_gz_json(&dir, &dest, output)
}

fn write_gz_json(dir: &Path, dest: &Path, data: &impl serde::Serialize) -> anyhow::Result<()> {
    let content = serde_json::to_vec(data)?;
    replace_atomically(dir, dest, |tmp| {
        let output = std::fs::File::create(tmp)?;
        let mut encoder = GzEncoder::new(output, Compression::default());
        std::io::Write::write_all(&mut encoder, &content)?;
        encoder.finish().map(drop)
    })
}

/// A crash between the rename and removing incoming is safe: the next cycle
/// redoes it, renaming over the existing target (at-least-once, idempotent).
pub(crate) fn compress_submission_to_processed(
    incoming_dir: &Path,
    processed_dir: &Path,
    job_id: &JobId,
) -> anyhow::Result<()> {
    let incoming = submission_path(incoming_dir, job_id);
    replace_atomically(
        processed_dir,
        &processed_path(processed_dir, job_id),
        |tmp| {
            let mut input = std::fs::File::open(&incoming)?;
            let output = std::fs::File::create(tmp)?;
            let mut encoder = GzEncoder::new(output, Compression::default());
            std::io::copy(&mut input, &mut encoder)?;
            encoder.finish().map(drop)
        },
    )?;
    std::fs::remove_file(&incoming)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Listing / search
// ---------------------------------------------------------------------------

/// Up to `limit` ids past `after`, in ascending id order (not time order),
/// skipping the `.tmp-*` files an interrupted write may leave.
pub fn list_submission_job_ids(
    dir: &Path,
    after: Option<&JobId>,
    limit: NonZeroUsize,
) -> anyhow::Result<Vec<JobId>> {
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let files = std::fs::read_dir(dir)?
        .map(|entry| {
            let entry = entry?;
            Ok(entry.file_type()?.is_file().then(|| entry.path()))
        })
        .collect::<std::io::Result<Vec<_>>>()?;
    let mut jobs: Vec<JobId> = files
        .iter()
        .flatten()
        .filter_map(|path| submission_job_id(path))
        .filter(|job_id| after.is_none_or(|after| job_id > after))
        .collect();
    jobs.sort_unstable();
    jobs.truncate(limit.get());
    Ok(jobs)
}

fn submission_job_id(path: &Path) -> Option<JobId> {
    let file_name = path.file_name()?.to_str()?;
    if file_name.starts_with('.') {
        return None;
    }
    let job_id = file_name.strip_suffix(".json")?;
    // Filenames are written from validated ids, so a bad one is corruption or
    // tampering: skip it (logged) rather than fail the whole listing.
    JobId::try_new(job_id)
        .inspect_err(|e| {
            tracing::warn!(file = %file_name, error = %e, "skipping incoming file with invalid job_id");
        })
        .ok()
}

/// `{unverified_dir}/{client_id}/{job_id}.json`
pub fn unverified_path(unverified_dir: &Path, client_id: &ClientId, job_id: &JobId) -> PathBuf {
    unverified_dir
        .join(client_id.as_str())
        .join(format!("{job_id}.json"))
}

fn is_unverified_object(file_name: &str) -> bool {
    !file_name.starts_with('.') && crate::is_json_name(file_name)
}

/// Age is the file `mtime`, not the payload's `submitted_at`: the tree is
/// opaque here, matching S3 `LastModified`. Returns `(deleted, kept)`.
pub fn prune_unverified_dir(
    unverified_dir: &Path,
    older_than: std::time::Duration,
    dry_run: bool,
) -> anyhow::Result<(usize, usize)> {
    if !unverified_dir.exists() {
        return Ok((0, 0));
    }
    // Saturate at the epoch so a huge threshold cannot panic.
    let cutoff = std::time::SystemTime::now()
        .checked_sub(older_than)
        .unwrap_or(std::time::UNIX_EPOCH);
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
            if !dry_run {
                crate::drop_empty_dir(&client_dir);
            }
            Ok(acc)
        },
    )
}

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

pub(crate) fn list_runtime_output(
    runtime_output_dir: &Path,
    job_id: &JobId,
) -> anyhow::Result<Vec<(ClientId, RuntimeOutput)>> {
    let read = match std::fs::read_dir(runtime_output_job_dir(runtime_output_dir, job_id)) {
        Ok(r) => r,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    };
    read.map(|entry| -> anyhow::Result<Option<(ClientId, RuntimeOutput)>> {
        let entry = entry?;
        let file_name = entry.file_name();
        // A `.tmp-*` sibling of an interrupted write has no `.json.gz` suffix.
        let Some(stem) = file_name
            .to_str()
            .and_then(|n| n.strip_suffix(".json.gz"))
        else {
            return Ok(None);
        };
        let Ok(client_id) = ClientId::try_new(stem).inspect_err(|e| {
            tracing::warn!(file = %stem, error = %e, "skipping runtime output with invalid client_id");
        }) else {
            return Ok(None);
        };
        let f = std::fs::File::open(entry.path())?;
        Ok(Some((client_id, serde_json::from_reader(GzDecoder::new(f))?)))
    })
    .filter_map(Result::transpose)
    .collect()
}

/// A missing file is not an error: `promote` may race a prune.
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
        crate::drop_empty_dir(&client_dir);
    }
    Ok(deleted)
}

/// Searches `incoming/` first, then `processed/`.
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
    use anyhow::Context;

    use pipette_store_testkit::TEST_LIST_LIMIT;

    use crate::storage::*;

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

        let jobs = list_submission_job_ids(&incoming, None, TEST_LIST_LIMIT)?;
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
        let three = NonZeroUsize::new(3).context("3 is non-zero")?;
        let ten = NonZeroUsize::new(10).context("10 is non-zero")?;
        assert_eq!(list_submission_job_ids(&incoming, None, three)?.len(), 3);
        assert_eq!(list_submission_job_ids(&incoming, None, ten)?.len(), 5);
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
        let jobs = list_submission_job_ids(&incoming, None, TEST_LIST_LIMIT)?;
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
