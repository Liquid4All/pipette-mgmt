use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use strum::{AsRefStr, Display, EnumString};
use tabled::Tabled;

use crate::types::ClientId;
use crate::validated::{ContactEmail, EmptyStringError, NonEmptyTrimmedString, PublicKeyHex, Tag};
use crate::warehouse::DeviceFormFactor;

const SELF_HEALED_ORGANIZATION: &str = "<unset>";
const SELF_HEALED_CLIENT_DETAILS: &str = "legacy client";

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Display, EnumString, AsRefStr,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum ClientStatus {
    Pending,
    Approved,
}

/// Device hardware profile. The server normalizes each populated field into a
/// reserved-namespace capability flag (see [`DeviceProfile::normalized_flags`])
/// that feeds job matching (see `planner.md §Client Matching Rules` and
/// `docs/plan-ingestion.md` §3). Every field is optional — a client may register
/// none, some, or all of them. Embedded in [`Client`] via `#[serde(flatten)]` so
/// the stored JSON carries flat `device_*` keys.
///
/// Byte counts are `u64` (non-negative by nature); this deliberately diverges
/// from the submission path's `i64` (`submission.rs`) — that struct is separate
/// and its signed bounds checks are left untouched.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceProfile {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub device_name: Option<NonEmptyTrimmedString>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub device_form_factor: Option<DeviceFormFactor>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub device_os_name: Option<NonEmptyTrimmedString>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub device_os_version: Option<NonEmptyTrimmedString>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub device_chip_model: Option<NonEmptyTrimmedString>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub device_ram_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub device_gpu_model: Option<NonEmptyTrimmedString>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub device_gpu_vram_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub device_npu_model: Option<NonEmptyTrimmedString>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub device_npu_vram_bytes: Option<u64>,
}

/// Reserved capability namespaces the server derives from a client's `device_*`
/// profile (`docs/plan-ingestion.md` §3). These are server-owned: a client may
/// not report them directly (they would let a client assert a profile it does
/// not have), so [`is_reserved_capability`] rejects them on the wire. The
/// unreserved `runtime:` namespace and any free-form flag are reported by the
/// client.
pub(crate) const RESERVED_CAPABILITY_NAMESPACES: &[&str] = &[
    "os",
    "os_version",
    "device",
    "chip",
    "form_factor",
    "ram_bytes",
    "gpu",
    "gpu_vram_bytes",
    "npu",
    "npu_vram_bytes",
];

/// True when `flag`'s namespace (the token before its first `:`) is one the
/// server owns (see [`RESERVED_CAPABILITY_NAMESPACES`]). A flag with no `:` has
/// no namespace and is never reserved.
///
/// Assumes `flag` is already in canonical form (see [`slugify`]); the namespace
/// is matched byte-exactly against the lowercase list, so a non-canonical
/// spelling like `OS:` would slip past. Client-reported flags are rejected
/// unless canonical before this is consulted (`handlers::validate_capabilities`).
pub(crate) fn is_reserved_capability(flag: &str) -> bool {
    flag.split_once(':')
        .is_some_and(|(ns, _)| RESERVED_CAPABILITY_NAMESPACES.contains(&ns))
}

/// A flag's **canonical form**: lowercase with all whitespace removed, so
/// `"iPhone 17 Pro"` → `"iphone17pro"` and `"iOS"` → `"ios"`. This is the shape
/// [`DeviceProfile::normalized_flags`] emits and the only shape client-reported
/// capabilities may take. Matching is exact set-containment, so both sides of a
/// comparison — a client's flags and a plan's `requires` — must be canonical to
/// line up; a flag that is not equal to its own `slugify` can never match one
/// that is.
pub(crate) fn slugify(value: &str) -> String {
    value
        .chars()
        .filter(|c| !c.is_whitespace())
        .flat_map(char::to_lowercase)
        .collect()
}

impl DeviceProfile {
    /// True when no `device_*` field is set. Gates the registration reindex:
    /// an all-`None` profile has nothing to normalize into capability flags, so
    /// there is no point flagging the client for re-indexing.
    pub fn is_empty(&self) -> bool {
        *self == DeviceProfile::default()
    }

    /// The reserved-namespace capability flags this profile normalizes to
    /// (`docs/plan-ingestion.md` §3). Each populated field maps to exactly one
    /// flag; absent fields contribute nothing. String values are slugified;
    /// byte counts are matched exactly as their decimal value.
    pub fn normalized_flags(&self) -> BTreeSet<String> {
        let mut flags = BTreeSet::new();
        let mut string_flag = |ns: &str, value: Option<&NonEmptyTrimmedString>| {
            if let Some(v) = value {
                flags.insert(format!("{ns}:{}", slugify(v.as_str())));
            }
        };
        string_flag("device", self.device_name.as_ref());
        string_flag("os", self.device_os_name.as_ref());
        string_flag("os_version", self.device_os_version.as_ref());
        string_flag("chip", self.device_chip_model.as_ref());
        string_flag("gpu", self.device_gpu_model.as_ref());
        string_flag("npu", self.device_npu_model.as_ref());
        if let Some(ff) = self.device_form_factor {
            flags.insert(format!("form_factor:{}", slugify(ff.as_ref())));
        }
        if let Some(n) = self.device_ram_bytes {
            flags.insert(format!("ram_bytes:{n}"));
        }
        if let Some(n) = self.device_gpu_vram_bytes {
            flags.insert(format!("gpu_vram_bytes:{n}"));
        }
        if let Some(n) = self.device_npu_vram_bytes {
            flags.insert(format!("npu_vram_bytes:{n}"));
        }
        flags
    }
}

impl Client {
    /// The client's **effective capability set**: the flags normalized from its
    /// `device_*` profile unioned with the capabilities it reports directly
    /// (`docs/plan-ingestion.md` §3). This is the single set the matcher tests
    /// a job's `requires` against; it is computed on demand, never persisted.
    pub fn effective_capabilities(&self) -> BTreeSet<String> {
        let mut caps = self.device_profile.normalized_flags();
        caps.extend(self.capabilities.iter().cloned());
        caps
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Tabled)]
pub struct Client {
    #[tabled(rename = "Client ID")]
    pub client_id: ClientId,
    #[tabled(skip)]
    pub public_key: PublicKeyHex,
    /// Normal `Client` deserialization stays strict. Legacy records
    /// are repaired explicitly through `RepairableClient` at auth-store
    /// read boundaries.
    #[tabled(rename = "Organization")]
    pub organization: NonEmptyTrimmedString,
    #[tabled(rename = "Details", display = "truncate_details")]
    pub client_details: NonEmptyTrimmedString,
    #[tabled(rename = "Contact")]
    pub contact_email: ContactEmail,
    #[tabled(rename = "Status")]
    pub status: ClientStatus,
    #[tabled(rename = "Registered")]
    pub registered_at: DateTime<Utc>,
    /// Flattened to flat `device_*` keys in the stored JSON. Absent on legacy
    /// records → deserializes to an empty profile (every field `None`).
    ///
    /// Tags are deliberately **not** a field: they live in the `tags-index/`
    /// marker trees, never in this record (see the tag index below / `AuthStore`).
    #[tabled(skip)]
    #[serde(flatten)]
    pub device_profile: DeviceProfile,
    /// Free-form capability flags the client reports directly (e.g.
    /// `runtime:llama_cpp`). Unioned with the flags [`DeviceProfile::normalized_flags`]
    /// derives from `device_*` to form the client's effective capability set
    /// (`docs/plan-ingestion.md` §3). Reserved-namespace flags are server-owned
    /// and rejected on the wire (see `handlers::validate_capabilities`),
    /// so this set never carries `os:` / `chip:` / … itself. Absent on legacy
    /// records → empty; an empty set emits no `capabilities` key, matching the
    /// legacy shape.
    #[tabled(skip)]
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub capabilities: BTreeSet<String>,
}

#[derive(Debug, Deserialize)]
struct RepairableClient {
    client_id: ClientId,
    public_key: PublicKeyHex,
    #[serde(default)]
    organization: Option<String>,
    #[serde(default)]
    client_details: Option<String>,
    contact_email: ContactEmail,
    status: ClientStatus,
    registered_at: DateTime<Utc>,
    /// The device profile needs no repair — every field is already optional,
    /// so a legacy record with no `device_*` keys flattens to an empty profile.
    /// `device_form_factor` stays typed: the write path always validates before
    /// persisting, so a stored record can never carry an invalid value here.
    #[serde(flatten, default)]
    device_profile: DeviceProfile,
    #[serde(default)]
    capabilities: BTreeSet<String>,
}

impl RepairableClient {
    fn into_client(self) -> Result<Client, EmptyStringError> {
        let organization =
            optional_non_empty_string(self.organization, SELF_HEALED_ORGANIZATION.to_string())?;
        let client_details =
            optional_non_empty_string(self.client_details, SELF_HEALED_CLIENT_DETAILS.to_string())?;

        Ok(Client {
            client_id: self.client_id,
            public_key: self.public_key,
            organization,
            client_details,
            contact_email: self.contact_email,
            status: self.status,
            registered_at: self.registered_at,
            device_profile: self.device_profile,
            capabilities: self.capabilities,
        })
    }
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum ClientRecordError {
    #[error("invalid client record: {strict}; self-heal failed: {repair}")]
    RepairFailed {
        strict: serde_json::Error,
        repair: serde_json::Error,
    },
    #[error("self-heal produced an invalid fallback value: {0}")]
    InvalidFallback(#[from] EmptyStringError),
}

pub fn truncate_details(s: &NonEmptyTrimmedString) -> String {
    let truncated: String = s.as_str().chars().take(30).collect();
    if truncated.len() < s.as_str().len() {
        format!("{truncated}...")
    } else {
        truncated
    }
}

/// Derive the deterministic [`ClientId`] from a public key.
///
/// Takes `&PublicKeyHex` rather than `&str` so callers can't pass an
/// unvalidated string — the type guarantees `hex::decode` succeeds.
/// The only fallible step left is the non-empty check on the
/// resulting `"ev1_{...}"` string, which never actually fires
/// (SHA-256 always produces 32 bytes → 64 hex chars).
pub fn derive_client_id(public_key: &PublicKeyHex) -> anyhow::Result<ClientId> {
    let pk_bytes = hex::decode(public_key.as_str())
        .expect("PublicKeyHex invariant: stored value is always valid hex");
    let hash = Sha256::digest(&pk_bytes);
    Ok(ClientId::try_new(format!("ev1_{}", hex::encode(hash)))?)
}

pub(crate) fn parse_client_or_self_heal(data: &[u8]) -> Result<(Client, bool), ClientRecordError> {
    match serde_json::from_slice(data) {
        Ok(client) => Ok((client, false)),
        Err(original_error) => {
            let repairable: RepairableClient =
                serde_json::from_slice(data).map_err(|repair| ClientRecordError::RepairFailed {
                    strict: original_error,
                    repair,
                })?;
            Ok((repairable.into_client()?, true))
        }
    }
}

fn optional_non_empty_string(
    value: Option<String>,
    default: String,
) -> Result<NonEmptyTrimmedString, EmptyStringError> {
    if let Some(raw) = value
        && !raw.trim().is_empty()
    {
        return NonEmptyTrimmedString::try_new(raw);
    }
    NonEmptyTrimmedString::try_new(default)
}

/// A record that a client has presented a `v1` signature, written the first
/// time one verifies and never rewritten.
///
/// This is the state behind the signature migration: a client with a record is
/// refused the timestamp-only fallback, so a signature captured from it before
/// it migrated authenticates nothing (`docs/authentication.md` §2.3).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SignatureMigration {
    pub first_seen: DateTime<Utc>,
}

/// Whether recording a migration was the client's first, so that the transition
/// is logged once in the system's life rather than once per process that
/// observes it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MigrationRecord {
    /// This call wrote the marker: the client's first `v1` signature.
    First,
    /// A marker was already in place from an earlier request.
    Existing,
}

/// Whether a client has a signature-migration marker.
///
/// Stats the path rather than calling [`Path::exists`], which reports `false`
/// for a path it could not read — here that would report an unreadable marker
/// as an un-migrated client and hand back the fallback.
pub fn has_signature_migration(dir: &Path, client_id: &ClientId) -> anyhow::Result<bool> {
    match std::fs::metadata(dir.join(format!("{client_id}.json"))) {
        Ok(_) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e.into()),
    }
}

/// Record that a client has migrated, keeping whatever marker is already there.
///
/// The record is staged in full and then linked into place, so a marker is only
/// ever observable complete. `hard_link` fails with `AlreadyExists` when a
/// marker is already there, which is what preserves the first sighting: the
/// time already recorded is the one worth keeping.
///
/// A staged file left behind by an interrupted write is inert — its name lacks
/// the `.json` suffix [`list_signature_migrations`] selects on, and it is
/// keyed by a fresh UUID, so it can neither be mistaken for a marker nor
/// collide with a concurrent write for the same client.
pub fn record_signature_migration(
    dir: &Path,
    client_id: &ClientId,
    at: DateTime<Utc>,
) -> anyhow::Result<MigrationRecord> {
    std::fs::create_dir_all(dir)?;
    let path = dir.join(format!("{client_id}.json"));
    let staged = dir.join(format!("{client_id}.{}.staged", uuid::Uuid::new_v4()));
    std::fs::write(
        &staged,
        serde_json::to_vec_pretty(&SignatureMigration { first_seen: at })?,
    )?;
    let linked = std::fs::hard_link(&staged, &path);
    let _ = std::fs::remove_file(&staged);
    match linked {
        Ok(()) => Ok(MigrationRecord::First),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(MigrationRecord::Existing),
        Err(e) => Err(e.into()),
    }
}

/// Every recorded migration, for the operator view. Skips unreadable entries
/// per the `AuthStore::list_signature_migrations` contract.
pub fn list_signature_migrations(
    dir: &Path,
) -> anyhow::Result<Vec<(ClientId, SignatureMigration)>> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    };
    Ok(entries
        .filter_map(|entry| entry.ok())
        .filter_map(|entry| {
            let name = entry.file_name();
            let client_id = ClientId::try_new(name.to_str()?.strip_suffix(".json")?).ok()?;
            let record = serde_json::from_slice(&std::fs::read(entry.path()).ok()?).ok()?;
            Some((client_id, record))
        })
        .collect())
}

pub fn load_client(clients_dir: &Path, client_id: &ClientId) -> anyhow::Result<Option<Client>> {
    let path = clients_dir.join(format!("{client_id}.json"));
    if !path.exists() {
        return Ok(None);
    }
    let content = std::fs::read(&path)?;
    let (client, repaired) = parse_client_or_self_heal(&content)?;
    if repaired {
        if let Err(e) = save_client(clients_dir, &client) {
            tracing::warn!(
                path = %path.display(),
                client_id = %client.client_id,
                error = %e,
                "failed to persist self-healed client record"
            );
        } else {
            tracing::warn!(
                path = %path.display(),
                client_id = %client.client_id,
                "self-healed malformed client record"
            );
        }
    }
    Ok(Some(client))
}

pub fn save_client(clients_dir: &Path, client: &Client) -> anyhow::Result<()> {
    std::fs::create_dir_all(clients_dir)?;
    let path = clients_dir.join(format!("{}.json", client.client_id));
    let content = serde_json::to_string_pretty(client)?;
    std::fs::write(&path, content)?;
    Ok(())
}

pub fn delete_client(clients_dir: &Path, client_id: &ClientId) -> anyhow::Result<()> {
    let path = clients_dir.join(format!("{client_id}.json"));
    if path.exists() {
        std::fs::remove_file(&path)?;
    }
    Ok(())
}

pub fn list_all(clients_dir: &Path) -> anyhow::Result<Vec<Client>> {
    if !clients_dir.exists() {
        return Ok(Vec::new());
    }
    let mut clients: Vec<Client> =
        std::fs::read_dir(clients_dir)?.try_fold(Vec::new(), |mut clients, entry| {
            let path = entry?.path();
            if path.extension().is_none_or(|ext| ext != "json") {
                return anyhow::Ok(clients);
            }
            let content = std::fs::read(&path)?;
            match parse_client_or_self_heal(&content) {
                Ok((client, repaired)) => {
                    if repaired {
                        if let Err(e) = save_client(clients_dir, &client) {
                            tracing::warn!(
                                path = %path.display(),
                                client_id = %client.client_id,
                                error = %e,
                                "failed to persist self-healed client record"
                            );
                        } else {
                            tracing::warn!(
                                path = %path.display(),
                                client_id = %client.client_id,
                                "self-healed malformed client record"
                            );
                        }
                    }
                    clients.push(client);
                }
                Err(e) => {
                    tracing::warn!(
                        path = %path.display(),
                        error = %e,
                        "skipping malformed client record"
                    );
                }
            }
            anyhow::Ok(clients)
        })?;
    clients.sort_by_key(|c| std::cmp::Reverse(c.registered_at));
    Ok(clients)
}

pub fn find_by_public_key(clients_dir: &Path, public_key: &PublicKeyHex) -> anyhow::Result<bool> {
    let client_id = derive_client_id(public_key)?;
    let path = clients_dir.join(format!("{client_id}.json"));
    Ok(path.exists())
}

// ---------------------------------------------------------------------------
// Tag index (local_fs) — filesystem side of the two marker trees `AuthStore`
// documents. Each `(client, tag)` is an empty file:
//   forward:  {by_client_dir}/{client_id}/{tag}
//   reverse:  {by_tag_dir}/{tag}/{client_id}
// Tags are flat and client ids have no `/`, so every path is two segments.
// ---------------------------------------------------------------------------

fn forward_tag_path(by_client_dir: &Path, client_id: &ClientId, tag: &Tag) -> PathBuf {
    by_client_dir.join(client_id.as_str()).join(tag.as_str())
}

fn reverse_tag_path(by_tag_dir: &Path, tag: &Tag, client_id: &ClientId) -> PathBuf {
    by_tag_dir.join(tag.as_str()).join(client_id.as_str())
}

fn touch_marker(path: &Path) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, [])?;
    Ok(())
}

fn remove_marker(path: &Path) -> anyhow::Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    // Best-effort: drop the now-empty parent dir so an emptied tag/client leaves
    // no stray directory. Ignore errors (non-empty / racing).
    if let Some(parent) = path.parent() {
        let _ = std::fs::remove_dir(parent);
    }
    Ok(())
}

/// Add a `(client, tag)` membership — writes both markers; idempotent.
pub fn add_tag(
    by_client_dir: &Path,
    by_tag_dir: &Path,
    client_id: &ClientId,
    tag: &Tag,
) -> anyhow::Result<()> {
    touch_marker(&forward_tag_path(by_client_dir, client_id, tag))?;
    touch_marker(&reverse_tag_path(by_tag_dir, tag, client_id))?;
    Ok(())
}

/// Remove a `(client, tag)` membership from both trees. Idempotent.
pub fn remove_tag(
    by_client_dir: &Path,
    by_tag_dir: &Path,
    client_id: &ClientId,
    tag: &Tag,
) -> anyhow::Result<()> {
    remove_marker(&forward_tag_path(by_client_dir, client_id, tag))?;
    remove_marker(&reverse_tag_path(by_tag_dir, tag, client_id))?;
    Ok(())
}

/// Read one entry level of a marker directory, returning the leaf names parsed
/// through `parse` (corrupt entries are skipped). Missing dir → empty.
fn list_marker_names<T, F>(dir: &Path, parse: F) -> anyhow::Result<Vec<T>>
where
    F: Fn(&str) -> Option<T>,
{
    if !dir.exists() {
        return Ok(Vec::new());
    }
    // Preserve per-entry read errors (Err short-circuits the collect); skip
    // names that don't parse (a corrupt entry is ignored, not trusted).
    std::fs::read_dir(dir)?
        .map(|entry| Ok(entry?.path()))
        .filter_map(|path: anyhow::Result<PathBuf>| match path {
            Ok(p) => p
                .file_name()
                .and_then(|n| n.to_str())
                .and_then(&parse)
                .map(Ok),
            Err(e) => Some(Err(e)),
        })
        .collect()
}

pub fn list_client_tags(
    by_client_dir: &Path,
    client_id: &ClientId,
) -> anyhow::Result<BTreeSet<Tag>> {
    let dir = by_client_dir.join(client_id.as_str());
    Ok(list_marker_names(&dir, |name| Tag::try_new(name).ok())?
        .into_iter()
        .collect())
}

pub fn list_client_ids_by_tag(by_tag_dir: &Path, tag: &Tag) -> anyhow::Result<Vec<ClientId>> {
    let dir = by_tag_dir.join(tag.as_str());
    let mut ids = list_marker_names(&dir, |name| ClientId::try_new(name).ok())?;
    ids.sort();
    Ok(ids)
}

/// Every `(client_id, tag)` in the forward tree (`{by_client_dir}/{id}/{tag}`),
/// for the reverse-index reconcile.
pub fn list_all_forward_markers(by_client_dir: &Path) -> anyhow::Result<Vec<(ClientId, Tag)>> {
    list_marker_names(by_client_dir, |name| ClientId::try_new(name).ok())?
        .into_iter()
        .map(|id| -> anyhow::Result<Vec<(ClientId, Tag)>> {
            let tags = list_marker_names(&by_client_dir.join(id.as_str()), |name| {
                Tag::try_new(name).ok()
            })?;
            Ok(tags.into_iter().map(|tag| (id.clone(), tag)).collect())
        })
        .collect::<anyhow::Result<Vec<_>>>()
        .map(|nested| nested.into_iter().flatten().collect())
}

/// Every `(client_id, tag)` in the reverse tree (`{by_tag_dir}/{tag}/{id}`), for
/// the reconcile.
pub fn list_all_reverse_markers(by_tag_dir: &Path) -> anyhow::Result<Vec<(ClientId, Tag)>> {
    list_marker_names(by_tag_dir, |name| Tag::try_new(name).ok())?
        .into_iter()
        .map(|tag| -> anyhow::Result<Vec<(ClientId, Tag)>> {
            let ids = list_marker_names(&by_tag_dir.join(tag.as_str()), |name| {
                ClientId::try_new(name).ok()
            })?;
            Ok(ids.into_iter().map(|id| (id, tag.clone())).collect())
        })
        .collect::<anyhow::Result<Vec<_>>>()
        .map(|nested| nested.into_iter().flatten().collect())
}

/// Remove every tag membership for a client (used on delete): drop each reverse
/// marker, then the client's whole forward directory.
pub fn delete_all_tags(
    by_client_dir: &Path,
    by_tag_dir: &Path,
    client_id: &ClientId,
) -> anyhow::Result<()> {
    list_client_tags(by_client_dir, client_id)?
        .iter()
        .try_for_each(|tag| remove_marker(&reverse_tag_path(by_tag_dir, tag, client_id)))?;
    match std::fs::remove_dir_all(by_client_dir.join(client_id.as_str())) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::client::*;
    use anyhow::Context;
    use rstest::rstest;

    #[test]
    fn test_derive_client_id() -> anyhow::Result<()> {
        let pk = PublicKeyHex::try_new(
            "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789",
        )?;
        let id = derive_client_id(&pk)?;
        assert!(id.as_str().starts_with("ev1_"));
        assert_eq!(id.as_str().len(), 4 + 64); // "ev1_" + 64 hex chars

        // Same input should produce same output
        let id2 = derive_client_id(&pk)?;
        assert_eq!(id, id2);
        Ok(())
    }

    #[test]
    fn test_client_crud() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let pk = PublicKeyHex::try_new(
            "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789",
        )?;
        let client = Client {
            client_id: ClientId::try_new("ev1_test123")?,
            public_key: pk,
            organization: NonEmptyTrimmedString::try_new("test-org")?,
            client_details: NonEmptyTrimmedString::try_new("test client")?,
            contact_email: ContactEmail::try_new("test@example.com")?,
            status: ClientStatus::Pending,
            registered_at: Utc::now(),
            device_profile: Default::default(),
            capabilities: Default::default(),
        };

        save_client(dir.path(), &client)?;

        let id = ClientId::try_new("ev1_test123")?;
        let loaded = load_client(dir.path(), &id)?.context("expected client to exist")?;
        assert_eq!(loaded.client_id.as_str(), "ev1_test123");
        assert_eq!(loaded.status, ClientStatus::Pending);

        delete_client(dir.path(), &id)?;
        assert!(load_client(dir.path(), &id)?.is_none());
        Ok(())
    }

    #[test]
    fn test_list_all() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        for i in 0..3 {
            // Different last byte per iteration so the keys are
            // distinct but all 32-byte valid hex.
            let pk = PublicKeyHex::try_new(format!(
                "abcdef0123456789abcdef0123456789abcdef0123456789abcdef012345670{i}"
            ))?;
            let client = Client {
                client_id: ClientId::try_new(format!("ev1_client{i}"))?,
                public_key: pk,
                organization: NonEmptyTrimmedString::try_new(format!("org{i}"))?,
                client_details: NonEmptyTrimmedString::try_new("details")?,
                contact_email: ContactEmail::try_new("a@b.com")?,
                status: ClientStatus::Pending,
                registered_at: Utc::now(),
                device_profile: Default::default(),
                capabilities: Default::default(),
            };
            save_client(dir.path(), &client)?;
        }
        let all = list_all(dir.path())?;
        assert_eq!(all.len(), 3);
        Ok(())
    }

    #[test]
    fn test_list_all_self_heals_legacy_clients() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let pk = PublicKeyHex::try_new(
            "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789",
        )?;
        let client = Client {
            client_id: ClientId::try_new("ev1_valid")?,
            public_key: pk,
            organization: NonEmptyTrimmedString::try_new("test-org")?,
            client_details: NonEmptyTrimmedString::try_new("details")?,
            contact_email: ContactEmail::try_new("a@b.com")?,
            status: ClientStatus::Pending,
            registered_at: Utc::now(),
            device_profile: Default::default(),
            capabilities: Default::default(),
        };
        save_client(dir.path(), &client)?;

        std::fs::write(
            dir.path().join("ev1_bad.json"),
            r#"{
  "client_id": "ev1_bad",
  "public_key": "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789",
  "client_details": "",
  "contact_email": "bad@example.com",
  "status": "pending",
  "registered_at": "2026-01-01T00:00:00Z"
}"#,
        )?;

        let all = list_all(dir.path())?;
        assert_eq!(all.len(), 2);

        let repaired_raw: serde_json::Value =
            serde_json::from_slice(&std::fs::read(dir.path().join("ev1_bad.json"))?)?;
        assert_eq!(repaired_raw["organization"], SELF_HEALED_ORGANIZATION);
        assert_eq!(repaired_raw["client_details"], SELF_HEALED_CLIENT_DETAILS);

        let id = ClientId::try_new("ev1_bad")?;
        let repaired = load_client(dir.path(), &id)?.context("expected repaired client")?;
        assert_eq!(repaired.organization.as_str(), SELF_HEALED_ORGANIZATION);
        assert_eq!(repaired.client_details.as_str(), SELF_HEALED_CLIENT_DETAILS);
        Ok(())
    }

    use crate::warehouse::DeviceFormFactor;

    fn sample_client() -> anyhow::Result<Client> {
        Ok(Client {
            client_id: ClientId::try_new("ev1_dev")?,
            public_key: PublicKeyHex::try_new(
                "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789",
            )?,
            organization: NonEmptyTrimmedString::try_new("org")?,
            client_details: NonEmptyTrimmedString::try_new("details")?,
            contact_email: ContactEmail::try_new("a@b.com")?,
            status: ClientStatus::Approved,
            registered_at: "2026-01-01T00:00:00Z".parse()?,
            device_profile: DeviceProfile::default(),
            capabilities: Default::default(),
        })
    }

    #[test]
    fn test_device_profile_is_empty() -> anyhow::Result<()> {
        assert!(DeviceProfile::default().is_empty());

        let with_form_factor = DeviceProfile {
            device_form_factor: Some(DeviceFormFactor::Laptop),
            ..Default::default()
        };
        assert!(!with_form_factor.is_empty());

        // A numeric field alone is also enough to be non-empty.
        let with_ram = DeviceProfile {
            device_ram_bytes: Some(36_000_000_000),
            ..Default::default()
        };
        assert!(!with_ram.is_empty());
        Ok(())
    }

    #[test]
    fn test_empty_profile_omits_device_keys() -> anyhow::Result<()> {
        // `skip_serializing_if` keeps an empty profile from writing any
        // `device_*` keys, so stored records match the legacy shape.
        let raw = serde_json::to_value(sample_client()?)?;
        let obj = raw.as_object().context("expected object")?;
        assert!(
            obj.keys().all(|k| !k.starts_with("device_")),
            "empty profile should emit no device_* keys, got: {:?}",
            obj.keys().collect::<Vec<_>>()
        );
        Ok(())
    }

    #[test]
    fn test_client_device_profile_roundtrip_flat_keys() -> anyhow::Result<()> {
        let mut client = sample_client()?;
        client.device_profile = DeviceProfile {
            device_name: Some(NonEmptyTrimmedString::try_new("MacBook Pro")?),
            device_form_factor: Some(DeviceFormFactor::Laptop),
            device_ram_bytes: Some(36_000_000_000),
            device_gpu_model: Some(NonEmptyTrimmedString::try_new("M3 Pro GPU")?),
            device_gpu_vram_bytes: Some(18_000_000_000),
            ..Default::default()
        };

        let raw = serde_json::to_value(&client)?;
        // Flattened: the device fields are top-level, not nested under a
        // `device_profile` key.
        assert!(raw.get("device_profile").is_none());
        assert_eq!(raw["device_form_factor"], "laptop");
        assert_eq!(raw["device_ram_bytes"], 36_000_000_000_u64);

        let back: Client = serde_json::from_value(raw)?;
        assert_eq!(back.device_profile, client.device_profile);
        Ok(())
    }

    #[test]
    fn test_legacy_record_without_device_keys_deserializes_empty() -> anyhow::Result<()> {
        // A pre-Phase-7 record has no `device_*` keys; it must load with an
        // empty profile rather than failing to deserialize.
        let legacy = r#"{
  "client_id": "ev1_legacy",
  "public_key": "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789",
  "organization": "org",
  "client_details": "details",
  "contact_email": "a@b.com",
  "status": "approved",
  "registered_at": "2026-01-01T00:00:00Z"
}"#;
        let client: Client = serde_json::from_str(legacy)?;
        assert!(client.device_profile.is_empty());
        Ok(())
    }

    #[test]
    fn test_self_heal_preserves_device_profile() -> anyhow::Result<()> {
        // A record that needs repair (empty client_details) but carries device
        // fields keeps the profile through the `RepairableClient` path.
        let dir = tempfile::tempdir()?;
        std::fs::write(
            dir.path().join("ev1_dev.json"),
            r#"{
  "client_id": "ev1_dev",
  "public_key": "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789",
  "client_details": "",
  "contact_email": "a@b.com",
  "status": "approved",
  "registered_at": "2026-01-01T00:00:00Z",
  "device_form_factor": "embedded",
  "device_ram_bytes": 8000000000
}"#,
        )?;
        let id = ClientId::try_new("ev1_dev")?;
        let repaired = load_client(dir.path(), &id)?.context("expected repaired client")?;
        assert_eq!(repaired.client_details.as_str(), SELF_HEALED_CLIENT_DETAILS);
        assert_eq!(
            repaired.device_profile.device_form_factor,
            Some(DeviceFormFactor::Embedded)
        );
        assert_eq!(
            repaired.device_profile.device_ram_bytes,
            Some(8_000_000_000)
        );
        Ok(())
    }

    #[test]
    fn test_normalized_flags_cover_every_populated_field() -> anyhow::Result<()> {
        let profile = DeviceProfile {
            device_name: Some(NonEmptyTrimmedString::try_new("iPhone 17 Pro")?),
            device_form_factor: Some(DeviceFormFactor::Phone),
            device_os_name: Some(NonEmptyTrimmedString::try_new("iOS")?),
            device_os_version: Some(NonEmptyTrimmedString::try_new("26.1")?),
            device_chip_model: Some(NonEmptyTrimmedString::try_new("Apple A19 Pro")?),
            device_ram_bytes: Some(8_000_000_000),
            device_gpu_model: Some(NonEmptyTrimmedString::try_new("Apple GPU")?),
            device_gpu_vram_bytes: Some(4_000_000_000),
            device_npu_model: Some(NonEmptyTrimmedString::try_new("Apple Neural Engine")?),
            device_npu_vram_bytes: Some(2_000_000_000),
        };
        let expected: BTreeSet<String> = [
            "device:iphone17pro",
            "form_factor:phone",
            "os:ios",
            "os_version:26.1",
            "chip:applea19pro",
            "ram_bytes:8000000000",
            "gpu:applegpu",
            "gpu_vram_bytes:4000000000",
            "npu:appleneuralengine",
            "npu_vram_bytes:2000000000",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        assert_eq!(profile.normalized_flags(), expected);
        // An empty profile normalizes to nothing.
        assert!(DeviceProfile::default().normalized_flags().is_empty());

        // Drift guard: a fully-populated profile must emit exactly the reserved
        // namespaces and no others. This ties `normalized_flags` to
        // `RESERVED_CAPABILITY_NAMESPACES` — adding a `device_*` field's flag to
        // one but not the other (which would make that namespace client-spoofable
        // via `validate_capabilities`) fails here.
        let flags = profile.normalized_flags();
        let emitted: BTreeSet<&str> = flags
            .iter()
            .filter_map(|f| f.split_once(':').map(|(ns, _)| ns))
            .collect();
        let reserved: BTreeSet<&str> = RESERVED_CAPABILITY_NAMESPACES.iter().copied().collect();
        assert_eq!(emitted, reserved);
        Ok(())
    }

    #[test]
    fn test_effective_capabilities_union() -> anyhow::Result<()> {
        let mut client = sample_client()?;
        client.device_profile = DeviceProfile {
            device_os_name: Some(NonEmptyTrimmedString::try_new("iOS")?),
            ..Default::default()
        };
        client.capabilities = BTreeSet::from(["runtime:llama_cpp".to_string()]);
        assert_eq!(
            client.effective_capabilities(),
            BTreeSet::from(["os:ios".to_string(), "runtime:llama_cpp".to_string()])
        );
        Ok(())
    }

    #[rstest]
    // Server-owned namespaces.
    #[case::os("os:ios", true)]
    #[case::chip("chip:a19", true)]
    #[case::ram_bytes("ram_bytes:8", true)]
    #[case::gpu_vram_bytes("gpu_vram_bytes:4", true)]
    // Client-owned / free-form (including a versioned runtime with extra colons).
    #[case::runtime("runtime:llama_cpp", false)]
    #[case::runtime_versioned("runtime:llama_cpp:b9999", false)]
    #[case::no_namespace("job_retry", false)]
    // Boundaries: a bare token with no `:` has no namespace, and `os_version:`
    // is its own namespace distinct from `os:`.
    #[case::bare_token("os", false)]
    #[case::os_version_not_os("os_version:26.1", true)]
    fn test_is_reserved_capability(#[case] flag: &str, #[case] expected: bool) {
        assert_eq!(is_reserved_capability(flag), expected, "flag {flag:?}");
    }

    #[test]
    fn test_capabilities_roundtrip_and_legacy_default() -> anyhow::Result<()> {
        // A populated set round-trips as a top-level `capabilities` array.
        let mut client = sample_client()?;
        client.capabilities = BTreeSet::from(["runtime:llama_cpp".to_string()]);
        let raw = serde_json::to_value(&client)?;
        assert_eq!(
            raw["capabilities"],
            serde_json::json!(["runtime:llama_cpp"])
        );
        let back: Client = serde_json::from_value(raw)?;
        assert_eq!(back.capabilities, client.capabilities);

        // An empty set emits no key (legacy shape), and a legacy record with no
        // `capabilities` key loads as an empty set.
        let empty = serde_json::to_value(sample_client()?)?;
        assert!(empty.get("capabilities").is_none());
        let legacy: Client = serde_json::from_value(empty)?;
        assert!(legacy.capabilities.is_empty());
        Ok(())
    }

    #[test]
    fn test_tags_never_serialized_into_record() -> anyhow::Result<()> {
        // Tags live in the marker trees, not the record — the client JSON must
        // never carry a `tags` key.
        let raw = serde_json::to_value(sample_client()?)?;
        assert!(raw.get("tags").is_none(), "record must not serialize tags");
        Ok(())
    }

    #[test]
    fn test_tag_markers_both_directions() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let by_client = dir.path().join("tags-index/by-client");
        let by_tag = dir.path().join("tags-index/by-tag");
        let a = ClientId::try_new("ev1_a")?;
        let b = ClientId::try_new("ev1_b")?;
        let team = Tag::try_new("team-mobile")?;
        let east = Tag::try_new("us-east")?;

        add_tag(&by_client, &by_tag, &a, &team)?;
        add_tag(&by_client, &by_tag, &a, &east)?;
        add_tag(&by_client, &by_tag, &b, &team)?;
        // Idempotent re-add.
        add_tag(&by_client, &by_tag, &a, &team)?;

        // Forward: client → tags.
        assert_eq!(
            list_client_tags(&by_client, &a)?,
            BTreeSet::from([team.clone(), east.clone()])
        );
        // Reverse: tag → clients.
        assert_eq!(
            list_client_ids_by_tag(&by_tag, &team)?,
            vec![a.clone(), b.clone()]
        );
        assert_eq!(list_client_ids_by_tag(&by_tag, &east)?, vec![a.clone()]);

        // Remove one membership → gone from both directions.
        remove_tag(&by_client, &by_tag, &a, &east)?;
        assert!(list_client_ids_by_tag(&by_tag, &east)?.is_empty());
        assert_eq!(
            list_client_tags(&by_client, &a)?,
            BTreeSet::from([team.clone()])
        );

        // Delete client b → drops it from the reverse tree and clears its dir.
        delete_all_tags(&by_client, &by_tag, &b)?;
        assert_eq!(list_client_ids_by_tag(&by_tag, &team)?, vec![a.clone()]);
        assert!(list_client_tags(&by_client, &b)?.is_empty());

        // Unknown tag / untagged client → empty, no error.
        assert!(list_client_ids_by_tag(&by_tag, &Tag::try_new("nope")?)?.is_empty());
        Ok(())
    }
}
