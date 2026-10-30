//! Filesystem layout for the pre-auth keys the two auth stores serve —
//! `{dir}/{key_id}.json` for the record and `{dir}/{key_id}.spent` for the
//! marker that retires a single-use one.
//!
//! One layout over two directories, so a listing of one kind never returns the
//! other; validation is each kind's own, in `pipette_core`.

use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::Serialize;
use serde::de::DeserializeOwned;

/// Every caller holds a validated [`pipette_plan_types::ClientPreauthKeyId`] or
/// [`pipette_plan_types::UserPreauthKeyId`], both of which match `[A-Za-z0-9_-]` and
/// so are one path segment with no `/` and no `..` in them.
fn record_path(dir: &Path, key_id: &str) -> PathBuf {
    dir.join(format!("{key_id}.json"))
}

/// The path the spent marker lives at — a sibling of the record, told apart by
/// suffix, so one directory read sees both.
fn marker_path(dir: &Path, key_id: &str) -> PathBuf {
    dir.join(format!("{key_id}.spent"))
}

/// Persist a freshly minted record. Unconditional: the `key_id` is 64 bits of
/// randomness minted a moment ago, so there is nothing to race with.
pub(super) fn save<K: Serialize>(dir: &Path, key_id: &str, key: &K) -> anyhow::Result<()> {
    std::fs::create_dir_all(dir)?;
    std::fs::write(record_path(dir, key_id), serde_json::to_vec_pretty(key)?)?;
    Ok(())
}

/// Absent is the answer for every way a key stops existing — spent, revoked,
/// pruned — because all three delete the record. The caller reports them alike,
/// so the route is not an enumeration oracle.
pub(super) fn load<K: DeserializeOwned>(dir: &Path, key_id: &str) -> anyhow::Result<Option<K>> {
    match std::fs::read(record_path(dir, key_id)) {
        Ok(data) => Ok(Some(serde_json::from_slice(&data)?)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// Skips non-`.json` entries, so the spent markers beside them are invisible
/// here, but lets read and parse errors propagate.
pub(super) fn list<K: DeserializeOwned>(dir: &Path) -> anyhow::Result<Vec<K>> {
    if !dir.exists() {
        return Ok(Vec::new());
    }
    std::fs::read_dir(dir)?
        .map(|entry| -> anyhow::Result<Option<K>> {
            let path = entry?.path();
            if path.extension().is_none_or(|ext| ext != "json") {
                return Ok(None);
            }
            Ok(Some(serde_json::from_slice(&std::fs::read(&path)?)?))
        })
        .filter_map(Result::transpose)
        .collect()
}

/// Whether this caller is the one that spent the key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Spend {
    Claimed,
    AlreadySpent,
}

/// The marker, not the record's deletion, is the spend: `create_new` is
/// `O_EXCL`, so one concurrent consume wins. If the caller's later delete of the
/// record never lands, the marker still makes the next consume lose.
pub(super) fn claim_spent(dir: &Path, key_id: &str, now: DateTime<Utc>) -> anyhow::Result<Spend> {
    std::fs::create_dir_all(dir)?;
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(marker_path(dir, key_id))
    {
        Ok(mut file) => {
            std::io::Write::write_all(&mut file, now.to_rfc3339().as_bytes())?;
            Ok(Spend::Claimed)
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(Spend::AlreadySpent),
        Err(e) => Err(e.into()),
    }
}

/// Delete a record. Idempotent — a missing file is a no-op, which is what lets
/// a revoke race a spend without either failing.
pub(super) fn delete_record(dir: &Path, key_id: &str) -> anyhow::Result<()> {
    remove_idempotent(&record_path(dir, key_id))
}

/// Key ids carrying a spent marker, whether or not their record survives. An
/// invalid name is skipped: this backs a prune sweep, which one name should not
/// stop.
pub(super) fn list_spent<I>(dir: &Path) -> anyhow::Result<Vec<I>>
where
    I: TryFrom<String>,
    I::Error: std::fmt::Display,
{
    if !dir.exists() {
        return Ok(Vec::new());
    }
    Ok(std::fs::read_dir(dir)?
        .filter_map(|entry| crate::readable_entry(dir, entry))
        .filter_map(|entry| {
            let path = entry.path();
            let name = path.file_name()?.to_str()?;
            I::try_from(name.strip_suffix(".spent")?.to_string())
                .inspect_err(|e| {
                    tracing::warn!(
                        path = %path.display(),
                        error = %e,
                        "skipping unparseable spent marker"
                    );
                })
                .ok()
        })
        .collect())
}

/// Sound only once the key has no record left: while a record survives, its
/// marker is the one thing keeping the key spent.
pub(super) fn delete_spent(dir: &Path, key_id: &str) -> anyhow::Result<()> {
    remove_idempotent(&marker_path(dir, key_id))
}

fn remove_idempotent(path: &Path) -> anyhow::Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    }
}
