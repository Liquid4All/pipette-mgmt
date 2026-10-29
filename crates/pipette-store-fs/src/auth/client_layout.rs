//! Filesystem layout for the client records and signature migration markers
//! that [`super::LocalFsClientStore`] serves. Permission sets are
//! [`super::client_permission_layout`]'s.

use std::num::NonZeroUsize;
use std::path::Path;

use chrono::{DateTime, Utc};

use pipette_core::client::{
    Client, MigrationRecord, SignatureMigration, parse_client_or_self_heal,
};
use pipette_core::types::DeriveFromKey;
use pipette_plan_types::{ClientId, PublicKeyHex};
use pipette_store::{ClientPage, UnreadableObjectError, flat_keyspace_id};

/// Not [`Path::exists`]: it reports `false` for a path it could not read, which
/// here would report an unreadable marker as an un-migrated client.
pub(super) fn has_signature_migration(dir: &Path, client_id: &ClientId) -> anyhow::Result<bool> {
    match std::fs::metadata(dir.join(format!("{client_id}.json"))) {
        Ok(_) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e.into()),
    }
}

/// Staged in full and hard-linked into place, so a marker is only ever seen
/// complete; `hard_link` refuses an existing marker, which keeps the first
/// sighting. A staged leftover lacks the `.json` suffix and carries a fresh
/// UUID, so it is inert.
pub(super) fn record_signature_migration(
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
    crate::remove_staged(&staged);
    match linked {
        Ok(()) => Ok(MigrationRecord::First),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(MigrationRecord::Existing),
        Err(e) => Err(e.into()),
    }
}

/// Skips unreadable entries per the `ClientStore::list_signature_migrations`
/// contract.
pub(super) fn list_signature_migrations(
    dir: &Path,
) -> anyhow::Result<Vec<(ClientId, SignatureMigration)>> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    };
    Ok(entries
        .filter_map(|entry| crate::readable_entry(dir, entry))
        .filter_map(|entry| {
            let name = entry.file_name();
            let client_id = flat_keyspace_id(name.to_str()?, ClientId::try_new)?;
            let path = entry.path();
            let data = match std::fs::read(&path) {
                Ok(data) => data,
                // Removed between the listing and the read.
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
                Err(e) => {
                    tracing::warn!(
                        path = %path.display(),
                        error = %e,
                        "skipping unreadable signature-migration marker"
                    );
                    return None;
                }
            };
            serde_json::from_slice::<SignatureMigration>(&data)
                .inspect_err(|e| {
                    tracing::warn!(
                        path = %path.display(),
                        error = %e,
                        "skipping malformed signature-migration marker"
                    );
                })
                .ok()
                .map(|record| (client_id, record))
        })
        .collect())
}

pub(super) fn load_client(
    clients_dir: &Path,
    client_id: &ClientId,
) -> anyhow::Result<Option<Client>> {
    let path = clients_dir.join(format!("{client_id}.json"));
    if !path.exists() {
        return Ok(None);
    }
    let content = std::fs::read(&path)?;
    let (client, repaired) = parse_client_or_self_heal(&content)?;
    if repaired {
        report_self_heal(clients_dir, &path, &client);
    }
    Ok(Some(client))
}

pub(super) fn save_client(clients_dir: &Path, client: &Client) -> anyhow::Result<()> {
    std::fs::create_dir_all(clients_dir)?;
    let path = clients_dir.join(format!("{}.json", client.client_id));
    let content = serde_json::to_string_pretty(client)?;
    std::fs::write(&path, content)?;
    Ok(())
}

/// Writes `updated` only if the record still equals `read`. An exclusive lock
/// on the record serializes the conditional writers, across processes too; a
/// blind [`save_client`] does not take it.
pub(super) fn replace_client(
    clients_dir: &Path,
    read: &Client,
    updated: &Client,
) -> anyhow::Result<bool> {
    let path = clients_dir.join(format!("{}.json", read.client_id));
    let record = match std::fs::File::open(&path) {
        Ok(record) => record,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e.into()),
    };
    record.lock()?;
    if load_client(clients_dir, &read.client_id)?.as_ref() != Some(read) {
        return Ok(false);
    }
    save_client(clients_dir, updated)?;
    Ok(true)
}

pub(super) fn delete_client(clients_dir: &Path, client_id: &ClientId) -> anyhow::Result<()> {
    let path = clients_dir.join(format!("{client_id}.json"));
    if path.exists() {
        std::fs::remove_file(&path)?;
    }
    Ok(())
}

/// Every client id in `clients/`, ascending, from the directory listing alone:
/// a paged fleet listing reads only the page's records. Sorted, because a page
/// over an unordered set is not a page.
pub(super) fn list_ids(clients_dir: &Path) -> anyhow::Result<Vec<ClientId>> {
    if !clients_dir.exists() {
        return Ok(Vec::new());
    }
    let mut ids: Vec<ClientId> = std::fs::read_dir(clients_dir)?
        .filter_map(|entry| crate::readable_entry(clients_dir, entry))
        .filter_map(|entry| {
            entry
                .file_name()
                .to_str()
                .and_then(|name| flat_keyspace_id(name, ClientId::try_new))
        })
        .collect();
    ids.sort();
    Ok(ids)
}

/// One page of `clients/`, in ascending key order, per the
/// `ClientStore::list_clients` contract.
pub(super) fn list_page(
    clients_dir: &Path,
    after: Option<&ClientId>,
    limit: NonZeroUsize,
) -> anyhow::Result<ClientPage> {
    if !clients_dir.exists() {
        return Ok(ClientPage::default());
    }
    // A leaf that names no client is not an object of this keyspace, so it is
    // neither a row nor an unreadable entry.
    let mut candidates: Vec<(String, ClientId)> = std::fs::read_dir(clients_dir)?
        .map(|entry| -> anyhow::Result<Option<_>> {
            let entry = entry?;
            let Ok(leaf) = entry.file_name().into_string() else {
                return Ok(None);
            };
            Ok(flat_keyspace_id(&leaf, ClientId::try_new).map(|client_id| (leaf, client_id)))
        })
        .collect::<anyhow::Result<Vec<_>>>()?
        .into_iter()
        .flatten()
        .collect();
    // `read_dir` order is arbitrary; the sort makes key order a contract.
    candidates.sort_by(|(a, _), (b, _)| a.cmp(b));
    // `after` is an exclusive lower bound on the key.
    if let Some(after) = after {
        let after = format!("{}.json", after.as_str());
        candidates.retain(|(leaf, _)| *leaf > after);
    }

    let mut page = ClientPage::default();
    let mut examined = 0usize;
    for (leaf, client_id) in &candidates {
        examined += 1;
        let path = clients_dir.join(leaf);
        let content = match std::fs::read(&path) {
            Ok(content) => content,
            // Removed since `read_dir`: as if listed a moment later.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e.into()),
        };
        match parse_client_or_self_heal(&content) {
            Ok((client, repaired)) => {
                // Idempotent; paging retries a repair at most once per page.
                if repaired {
                    report_self_heal(clients_dir, &path, &client);
                }
                page.items.push(client);
            }
            Err(e) => page.unreadable.push(UnreadableObjectError {
                key: path.display().to_string(),
                reason: e.to_string(),
            }),
        }
        if page.items.len() == limit.get() {
            // The last key examined, so the next page also skips the broken
            // ones this one reported.
            page.resume_after = (examined < candidates.len()).then(|| client_id.clone());
            break;
        }
    }
    Ok(page)
}

/// Write a repaired record back, and say so. Best-effort: a failed write leaves
/// the record readable through the same self-heal on the next read.
fn report_self_heal(clients_dir: &Path, path: &Path, client: &Client) {
    if let Err(e) = save_client(clients_dir, client) {
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

pub(super) fn find_by_public_key(
    clients_dir: &Path,
    public_key: &PublicKeyHex,
) -> anyhow::Result<bool> {
    let client_id = ClientId::derive(public_key);
    let path = clients_dir.join(format!("{client_id}.json"));
    Ok(path.exists())
}

#[cfg(test)]
mod tests {
    use anyhow::Context;
    use rstest::rstest;

    use pipette_core::client::{
        ClientStatus, SELF_HEALED_CLIENT_DETAILS, SELF_HEALED_CONTACT_EMAIL,
        SELF_HEALED_ORGANIZATION,
    };
    use pipette_plan_types::{ContactEmail, DeviceFormFactor, NonEmptyTrimmedString};
    use pipette_store_testkit::TEST_LIST_LIMIT;

    use super::*;

    /// The first 62 of a key's 64 hex chars; [`test_public_key`] supplies the
    /// last two so a test can mint distinct-but-valid keys.
    const PUBLIC_KEY_PREFIX: &str =
        "abcdef0123456789abcdef0123456789abcdef0123456789abcdef01234567";

    fn test_public_key(nth: u8) -> String {
        format!("{PUBLIC_KEY_PREFIX}{nth:02}")
    }

    fn sample_client(id: &str, public_key: &str, organization: &str) -> anyhow::Result<Client> {
        Ok(Client {
            client_id: ClientId::try_new(id)?,
            public_key: PublicKeyHex::try_new(public_key)?,
            organization: NonEmptyTrimmedString::try_new(organization)?,
            client_details: NonEmptyTrimmedString::try_new("details")?,
            contact_email: ContactEmail::try_new("a@b.com")?,
            status: ClientStatus::Pending,
            registered_at: Utc::now(),
            device_profile: Default::default(),
            capabilities: Default::default(),
        })
    }

    /// A legacy record (no `organization`, empty `client_details`) with
    /// `overrides` merged over it.
    fn legacy_record(id: &str, overrides: serde_json::Value) -> anyhow::Result<Vec<u8>> {
        let mut record = serde_json::json!({
            "client_id": id,
            "public_key": test_public_key(89),
            "client_details": "",
            "contact_email": "a@b.com",
            "status": "approved",
            "registered_at": "2026-01-01T00:00:00Z",
        });
        let fields = overrides
            .as_object()
            .context("overrides must be a JSON object")?
            .clone();
        record
            .as_object_mut()
            .context("skeleton is a JSON object")?
            .extend(fields);
        Ok(serde_json::to_vec(&record)?)
    }

    #[test]
    fn test_client_crud() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let client = sample_client("ev1_test123", &test_public_key(89), "test-org")?;

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
    fn test_list_page() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        (0..3).try_for_each(|i| {
            let client = sample_client(
                &format!("ev1_client{i}"),
                &test_public_key(i),
                &format!("org{i}"),
            )?;
            save_client(dir.path(), &client)
        })?;
        let all = list_page(dir.path(), None, TEST_LIST_LIMIT)?;
        assert_eq!(all.items.len(), 3);
        Ok(())
    }

    /// `ClientId` trims, so a padded copy of a record's file would otherwise
    /// list as a second `ev1_a` and move the page cursor off its key.
    #[test]
    fn test_a_padded_file_is_not_a_client() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        save_client(
            dir.path(),
            &sample_client("ev1_a", &test_public_key(1), "org")?,
        )?;
        std::fs::copy(
            dir.path().join("ev1_a.json"),
            dir.path().join(" ev1_a.json"),
        )?;
        assert_eq!(list_ids(dir.path())?, vec![ClientId::try_new("ev1_a")?]);
        assert_eq!(list_page(dir.path(), None, TEST_LIST_LIMIT)?.items.len(), 1);
        Ok(())
    }

    #[test]
    fn test_list_page_self_heals_legacy_clients() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let client = sample_client("ev1_valid", &test_public_key(89), "test-org")?;
        save_client(dir.path(), &client)?;

        std::fs::write(
            dir.path().join("ev1_bad.json"),
            legacy_record("ev1_bad", serde_json::json!({ "status": "pending" }))?,
        )?;

        let all = list_page(dir.path(), None, TEST_LIST_LIMIT)?;
        assert_eq!(all.items.len(), 2);

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

    /// A stored address the narrowing now refuses (records carrying one exist)
    /// is replaced with the placeholder rather than dropping the record.
    #[rstest]
    #[case("user@localhost", "dotless domain")]
    #[case("Ops <ops@example.com>", "display name")]
    #[case("\"odd name\"@example.com", "quoted local part")]
    fn test_contact_email_the_narrowing_refuses_is_self_healed(
        #[case] email: &str,
        #[case] desc: &str,
    ) -> anyhow::Result<()> {
        assert!(
            ContactEmail::try_new(email).is_err(),
            "{desc} must be refused at registration, or this test proves nothing"
        );

        let dir = tempfile::tempdir()?;
        std::fs::write(
            dir.path().join("ev1_legacy_email.json"),
            legacy_record(
                "ev1_legacy_email",
                serde_json::json!({
                    "organization": "org",
                    "client_details": "details",
                    "contact_email": email,
                }),
            )?,
        )?;

        let id = ClientId::try_new("ev1_legacy_email")?;
        let loaded = load_client(dir.path(), &id)?
            .with_context(|| format!("{desc} record must still load"))?;
        assert_eq!(loaded.contact_email.as_str(), SELF_HEALED_CONTACT_EMAIL);

        assert_eq!(
            list_page(dir.path(), None, TEST_LIST_LIMIT)?.items.len(),
            1,
            "{desc} must not be dropped"
        );
        Ok(())
    }

    #[test]
    fn test_self_heal_preserves_device_profile() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        std::fs::write(
            dir.path().join("ev1_dev.json"),
            legacy_record(
                "ev1_dev",
                serde_json::json!({
                    "device_form_factor": "embedded",
                    "device_ram_bytes": 8_000_000_000u64,
                }),
            )?,
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
}
