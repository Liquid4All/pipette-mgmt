//! The device lifecycle an operator drives, shared by `pipette-mgmt clients`
//! and the HTTP routes so a delete's five-store teardown has one copy. A
//! library crate prints nothing: each function writes and logs.

use std::collections::BTreeSet;

use futures::TryStreamExt;
use futures::stream;
use futures::stream::StreamExt;

use pipette_core::client::{Client, ClientStatus};
use pipette_core::client_permission::ClientPermissionSet;
use pipette_plan_types::{ClientId, ClientPermission, ContactEmail, NonEmptyTrimmedString};
use pipette_store::{
    ClientPermissionMutation, ClientStore, KeyIndexStore, OrgStore, STORAGE_CONCURRENCY, TagStore,
    TodoStore, purge_client_todo_state,
};

/// The mutable details of a device record. `organization` is the free-text
/// label the device sent, not its [`pipette_plan_types::ClientOwner`].
#[derive(Debug, Clone, Default)]
pub struct ClientUpdate {
    pub organization: Option<NonEmptyTrimmedString>,
    pub client_details: Option<NonEmptyTrimmedString>,
    pub contact_email: Option<ContactEmail>,
}

impl ClientUpdate {
    pub fn is_empty(&self) -> bool {
        self.organization.is_none() && self.client_details.is_none() && self.contact_email.is_none()
    }
}

/// What a write did, so a caller can report "no change".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Applied<T> {
    Changed(T),
    Unchanged,
}

/// What a status write did, and what it moved from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StatusChange {
    pub from: ClientStatus,
    pub to: ClientStatus,
}

/// Why an operation could not run.
#[derive(Debug, thiserror::Error)]
pub enum ClientAdminError {
    #[error("client {0} not found")]
    NotFound(ClientId),
    #[error("can only reject pending clients, {client_id} has status \"{status}\"")]
    NotPending {
        client_id: ClientId,
        status: ClientStatus,
    },
    #[error(
        "client {0} changed under {REPLACE_ATTEMPTS} successive edits; nothing was written, retry"
    )]
    Contended(ClientId),
    #[error(transparent)]
    Store(#[from] anyhow::Error),
}

/// Read-modify-write attempts before contention is an error: a race between two
/// writers resolves on the next attempt, and a small budget stops a stuck
/// record spinning.
const REPLACE_ATTEMPTS: usize = 3;

/// The record, or [`ClientAdminError::NotFound`]. Every operation below opens
/// with this, so a mistyped id is refused before anything is written.
pub async fn require(
    clients: &dyn ClientStore,
    client_id: &ClientId,
) -> Result<Client, ClientAdminError> {
    clients
        .get_client(client_id)
        .await?
        .ok_or_else(|| ClientAdminError::NotFound(client_id.clone()))
}

/// Apply `update`, returning the record and a line per field that moved; a
/// value equal to the stored one is not a change.
pub async fn update(
    clients: &dyn ClientStore,
    client_id: &ClientId,
    update: ClientUpdate,
) -> Result<Applied<(Client, Vec<String>)>, ClientAdminError> {
    let applied = replace(clients, client_id, |client| {
        let mut changes = Vec::new();
        if let Some(value) = &update.organization
            && client.organization != *value
        {
            changes.push(format!(
                "organization: {:?} -> {:?}",
                client.organization, value
            ));
            client.organization = value.clone();
        }
        if let Some(value) = &update.client_details
            && client.client_details != *value
        {
            changes.push(format!(
                "details: {:?} -> {:?}",
                client.client_details, value
            ));
            client.client_details = value.clone();
        }
        if let Some(value) = &update.contact_email
            && client.contact_email != *value
        {
            changes.push(format!(
                "contact_email: {:?} -> {:?}",
                client.contact_email, value
            ));
            client.contact_email = value.clone();
        }
        (!changes.is_empty()).then_some(changes)
    })
    .await?;
    if matches!(applied, Applied::Changed(_)) {
        tracing::info!(client_id = %client_id, "client details updated");
    }
    Ok(applied)
}

/// Move a device to status `to`, returning the record and the move, or report
/// that it is already there. Rejecting is [`reject`], a delete.
pub async fn set_status(
    clients: &dyn ClientStore,
    client_id: &ClientId,
    to: ClientStatus,
) -> Result<Applied<(Client, StatusChange)>, ClientAdminError> {
    let applied = replace(clients, client_id, |client| {
        let from = client.status;
        client.status = to;
        (from != to).then_some(StatusChange { from, to })
    })
    .await?;
    if let Applied::Changed((_, StatusChange { from, .. })) = &applied {
        tracing::info!(client_id = %client_id, previous_status = %from, %to, "client status set");
    }
    Ok(applied)
}

/// Read, apply `edit` (which may be replayed), and write only onto the record
/// read, so a concurrent operator edit or device PATCH is kept, not overwritten.
/// `edit` answering `None` is no change, and nothing is written.
async fn replace<T>(
    clients: &dyn ClientStore,
    client_id: &ClientId,
    edit: impl Fn(&mut Client) -> Option<T>,
) -> Result<Applied<(Client, T)>, ClientAdminError> {
    let mut attempts = 0;
    loop {
        let read = require(clients, client_id).await?;
        let mut client = read.clone();
        let Some(change) = edit(&mut client) else {
            return Ok(Applied::Unchanged);
        };
        if clients.replace_client(&read, &client).await? {
            return Ok(Applied::Changed((client, change)));
        }
        attempts += 1;
        if attempts == REPLACE_ATTEMPTS {
            return Err(ClientAdminError::Contended(client_id.clone()));
        }
        tracing::debug!(%client_id, "client record changed by a concurrent write");
    }
}

/// Refuse a pending registration: delete the record and free its keypair. Not
/// a status, since nothing reads a "rejected" row. Refused unless pending: an
/// approved device may be working, so use [`delete`] deliberately.
pub async fn reject(
    clients: &dyn ClientStore,
    key_index: &dyn KeyIndexStore,
    orgs: &dyn OrgStore,
    tags: &dyn TagStore,
    client_id: &ClientId,
) -> Result<(), ClientAdminError> {
    let client = require(clients, client_id).await?;
    if client.status != ClientStatus::Pending {
        return Err(ClientAdminError::NotPending {
            client_id: client_id.clone(),
            status: client.status,
        });
    }
    clients.delete_client(client_id).await?;
    if let Some(digest) = pipette_core::key_index::digest_of_client_id(client_id)
        && let Err(e) = key_index.release_public_key(&digest).await
    {
        tracing::warn!(
            client_id = %client_id,
            error = %e,
            "failed to release the public-key claim for a rejected client"
        );
    }
    release_ownership(orgs, client_id).await;
    release_tags(tags, client_id).await;
    tracing::info!(client_id = %client_id, "client rejected");
    Ok(())
}

/// Delete a device's identity and everything keyed on it. Idempotent, so a
/// rerun finishes a partial teardown; the bool says whether a record existed.
///
/// The record goes first: it is the existence authority, so every key left
/// after it is inert and the cleanups that follow are best-effort.
pub async fn delete(
    clients: &dyn ClientStore,
    key_index: &dyn KeyIndexStore,
    orgs: &dyn OrgStore,
    tags: &dyn TagStore,
    todo: &dyn TodoStore,
    client_id: &ClientId,
) -> Result<bool, ClientAdminError> {
    let existed = clients.get_client(client_id).await?.is_some();
    clients.delete_client(client_id).await?;

    // Best-effort from here: the identity is already gone, so aborting would
    // report the delete as failed when its essential work is done.
    if let Some(digest) = pipette_core::key_index::digest_of_client_id(client_id)
        && let Err(e) = key_index.release_public_key(&digest).await
    {
        tracing::warn!(
            client_id = %client_id,
            error = %e,
            "failed to release the public-key claim for a deleted client"
        );
    }
    release_ownership(orgs, client_id).await;
    release_tags(tags, client_id).await;
    purge_client_todo_state(todo, client_id).await;

    tracing::info!(client_id = %client_id, existed, "client deleted");
    Ok(existed)
}

/// Best-effort: registration clears a leftover before the id is reused.
async fn release_ownership(orgs: &dyn OrgStore, client_id: &ClientId) {
    if let Err(e) = orgs.delete_ownership(client_id).await {
        tracing::warn!(
            client_id = %client_id,
            error = %e,
            "failed to delete the ownership of a removed client"
        );
    }
}

async fn release_tags(tags: &dyn TagStore, client_id: &ClientId) {
    if let Err(e) = tags.delete_client_tags(client_id).await {
        tracing::warn!(
            client_id = %client_id,
            error = %e,
            "failed to delete the tags of a removed client"
        );
    }
}

/// Clear a device's suspension; idempotent. Touches only the `todo/` marker:
/// a suspension is queue state, not an approval decision.
pub async fn unsuspend(todo: &dyn TodoStore, client_id: &ClientId) -> anyhow::Result<bool> {
    let was_suspended = todo.read_suspension(client_id).await?.is_some();
    todo.delete_suspension(client_id).await?;
    if was_suspended {
        tracing::info!(client_id = %client_id, "client unsuspended");
    }
    Ok(was_suspended)
}

/// A device's resolved permission set: what its object holds, or nothing when
/// it has no object (granted by hand). Shared so no surface reads the set raw.
pub async fn resolved_permissions(
    clients: &dyn ClientStore,
    client_id: &ClientId,
) -> Result<ClientPermissionSet, ClientAdminError> {
    let stored = clients.get_client_permissions(client_id).await?;
    Ok(pipette_core::client_permission::effective(stored))
}

/// Add `permissions` to a device's set, and report the set as it now stands.
/// [`Applied::Unchanged`] means every name was already held.
pub async fn grant_permissions(
    clients: &dyn ClientStore,
    client_id: &ClientId,
    permissions: &BTreeSet<ClientPermission>,
) -> Result<Applied<ClientPermissionSet>, ClientAdminError> {
    let granted: Vec<ClientPermission> = permissions.iter().copied().collect();
    let mutation = clients
        .mutate_client_permissions(client_id, &move |set| {
            set.insert_all(granted.iter().copied())
        })
        .await?;
    if !mutation.changed {
        return Ok(Applied::Unchanged);
    }
    let added: Vec<&str> = permissions.iter().map(AsRef::as_ref).collect();
    tracing::info!(
        client_id = %client_id,
        ?added,
        resulting = ?mutation.resulting.names(),
        "client permissions granted"
    );
    Ok(Applied::Changed(mutation.resulting))
}

/// Take `remove` out of a device's set and put `add` in, as one
/// read-modify-write so two amends at once keep each other's work. An
/// `Unknown` name the object carries stays.
pub async fn amend_permissions(
    clients: &dyn ClientStore,
    client_id: &ClientId,
    add: &BTreeSet<ClientPermission>,
    remove: &BTreeSet<ClientPermission>,
) -> Result<ClientPermissionMutation, ClientAdminError> {
    let mutation = clients
        .mutate_client_permissions(client_id, &|set| {
            let before = set.clone();
            set.remove_all(remove.iter());
            set.insert_all(add.iter().copied());
            *set != before
        })
        .await?;
    if mutation.changed {
        tracing::info!(
            client_id = %client_id,
            added = ?wire_names(add),
            removed = ?wire_names(remove),
            resulting = ?mutation.resulting.names(),
            "client permissions amended"
        );
    }
    Ok(mutation)
}

/// Replace a device's whole set. An empty one clears it: the device keeps its
/// identity and may do nothing.
pub async fn set_permissions(
    clients: &dyn ClientStore,
    client_id: &ClientId,
    permissions: &BTreeSet<ClientPermission>,
) -> Result<ClientPermissionMutation, ClientAdminError> {
    let mutation = clients
        .mutate_client_permissions(client_id, &|set| set.replace(permissions.iter().copied()))
        .await?;
    if mutation.changed {
        tracing::info!(
            client_id = %client_id,
            requested = ?wire_names(permissions),
            resulting = ?mutation.resulting.names(),
            "client permissions replaced"
        );
    }
    Ok(mutation)
}

fn wire_names(permissions: &BTreeSet<ClientPermission>) -> Vec<&str> {
    permissions.iter().map(AsRef::as_ref).collect()
}

/// Flag each device in `client_ids` so the next `queue-maintenance` run
/// rebuilds its `todo/eligible/` markers against the current record. The
/// caller confirms each exists; one deleted since leaves a flag cleanup drops.
pub async fn flag_reindex(
    todo: &dyn TodoStore,
    client_ids: &[ClientId],
) -> Result<(), ClientAdminError> {
    stream::iter(client_ids.iter().map(|client_id| async move {
        todo.write_pending_reindex(client_id).await?;
        tracing::info!(
            client_id = %client_id,
            "operator flagged client for eligible-index reindex"
        );
        Ok::<(), ClientAdminError>(())
    }))
    .buffer_unordered(STORAGE_CONCURRENCY)
    .try_collect::<Vec<()>>()
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use chrono::Utc;

    use pipette_core::types::Mint;
    use pipette_plan_types::{ClientOwner, OrgId};
    use pipette_store::Stores;
    use pipette_store_fs::build_local_fs_stores;

    use super::*;

    fn stores() -> anyhow::Result<(tempfile::TempDir, Stores)> {
        let dir = tempfile::tempdir()?;
        let config = pipette_core::config::Config {
            storage: pipette_core::config::StorageConfig::local_fs(dir.path().to_path_buf()),
            auth_storage: pipette_core::config::StorageConfig::local_fs(dir.path().to_path_buf()),
            ..Default::default()
        };
        let stores = build_local_fs_stores(&config)?;
        Ok((dir, stores))
    }

    async fn seed(stores: &Stores, id: &str) -> anyhow::Result<Client> {
        let client = Client {
            client_id: ClientId::try_new(id)?,
            public_key: pipette_plan_types::PublicKeyHex::try_new("ab".repeat(32))?,
            organization: NonEmptyTrimmedString::try_new("lab")?,
            client_details: NonEmptyTrimmedString::try_new("rig")?,
            contact_email: ContactEmail::try_new("ops@example.test")?,
            status: ClientStatus::Pending,
            registered_at: Utc::now(),
            device_profile: Default::default(),
            capabilities: Default::default(),
        };
        stores.clients.put_client(&client).await?;
        Ok(client)
    }

    /// A body naming nothing reports `Unchanged` rather than writing.
    #[tokio::test]
    async fn an_update_reports_only_what_moved() -> anyhow::Result<()> {
        let (_dir, stores) = stores()?;
        let client = seed(&stores, "ev1_rig").await?;

        assert!(matches!(
            update(
                &*stores.clients,
                &client.client_id,
                ClientUpdate {
                    organization: Some(NonEmptyTrimmedString::try_new("lab")?),
                    ..Default::default()
                },
            )
            .await?,
            Applied::Unchanged,
        ));

        let Applied::Changed((updated, changes)) = update(
            &*stores.clients,
            &client.client_id,
            ClientUpdate {
                organization: Some(NonEmptyTrimmedString::try_new("other")?),
                contact_email: Some(ContactEmail::try_new("new@example.test")?),
                ..Default::default()
            },
        )
        .await?
        else {
            anyhow::bail!("a real change must report Changed");
        };
        assert_eq!(changes.len(), 2, "one line per field that moved");
        assert_eq!(updated.organization.as_str(), "other");
        assert_eq!(updated.contact_email.as_str(), "new@example.test");
        Ok(())
    }

    #[tokio::test]
    async fn a_status_write_reports_the_transition_once() -> anyhow::Result<()> {
        let (_dir, stores) = stores()?;
        let client = seed(&stores, "ev1_rig").await?;
        let (clients, client_id) = (&*stores.clients, &client.client_id);
        let change = |from, to| Some(StatusChange { from, to });

        // In order: each write starts from the status the one before left.
        let writes = [
            (
                ClientStatus::Approved,
                change(ClientStatus::Pending, ClientStatus::Approved),
            ),
            (ClientStatus::Approved, None),
            (
                ClientStatus::Pending,
                change(ClientStatus::Approved, ClientStatus::Pending),
            ),
        ];
        stream::iter(writes.map(anyhow::Ok))
            .try_for_each(|(to, expected)| async move {
                let moved = match set_status(clients, client_id, to).await? {
                    Applied::Changed((written, moved)) => {
                        assert_eq!(written.status, to, "the record a move to {to} answers");
                        Some(moved)
                    }
                    Applied::Unchanged => None,
                };
                assert_eq!(moved, expected, "a write to {to}");
                Ok(())
            })
            .await
    }

    /// Edits read together all land: one whose read went stale replays on the
    /// record the others left instead of overwriting it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn racing_edits_keep_each_other() -> anyhow::Result<()> {
        let Some(db) = pipette_store_sql::scratch::TestDb::new().await? else {
            return Ok(());
        };
        let (_dir, stores) = stores()?;
        let stores = pipette_store_sql::overlay(db.db(), stores);
        let client = seed(&stores, "ev1_rig").await?;
        let (clients, client_id) = (&*stores.clients, &client.client_id);
        let organization = ClientUpdate {
            organization: Some(NonEmptyTrimmedString::try_new("other")?),
            ..Default::default()
        };
        let details = ClientUpdate {
            client_details: Some(NonEmptyTrimmedString::try_new("bench")?),
            ..Default::default()
        };

        let (organization, details, status) = tokio::join!(
            update(clients, client_id, organization),
            update(clients, client_id, details),
            set_status(clients, client_id, ClientStatus::Approved),
        );
        assert!(matches!(organization?, Applied::Changed(_)));
        assert!(matches!(details?, Applied::Changed(_)));
        assert!(matches!(status?, Applied::Changed(_)));
        let stored = require(clients, client_id).await?;
        assert_eq!(
            (
                stored.organization.as_str(),
                stored.client_details.as_str(),
                stored.status
            ),
            ("other", "bench", ClientStatus::Approved),
            "every edit survives the others"
        );
        Ok(())
    }

    #[tokio::test]
    async fn an_unknown_client_is_refused_not_created() -> anyhow::Result<()> {
        let (_dir, stores) = stores()?;
        let absent = ClientId::try_new("ev1_nobody")?;

        assert!(matches!(
            set_status(&*stores.clients, &absent, ClientStatus::Approved).await,
            Err(ClientAdminError::NotFound(_))
        ));
        assert!(matches!(
            update(&*stores.clients, &absent, ClientUpdate::default()).await,
            Err(ClientAdminError::NotFound(_))
        ));
        assert!(stores.clients.get_client(&absent).await?.is_none());
        Ok(())
    }

    #[tokio::test]
    async fn a_delete_is_idempotent_and_reports_what_it_found() -> anyhow::Result<()> {
        let (_dir, stores) = stores()?;
        let client = seed(&stores, "ev1_rig").await?;

        assert!(
            delete(
                &*stores.clients,
                &*stores.key_index,
                &*stores.orgs,
                &*stores.tags,
                &*stores.todo,
                &client.client_id,
            )
            .await?
        );
        assert!(
            stores
                .clients
                .get_client(&client.client_id)
                .await?
                .is_none()
        );
        assert!(
            !delete(
                &*stores.clients,
                &*stores.key_index,
                &*stores.orgs,
                &*stores.tags,
                &*stores.todo,
                &client.client_id,
            )
            .await?,
            "a second run converges rather than erroring"
        );
        Ok(())
    }

    #[tokio::test]
    async fn a_removed_device_leaves_no_ownership_or_tags() -> anyhow::Result<()> {
        let (_dir, stores) = stores()?;
        let owner = ClientOwner::Org {
            org_id: OrgId::mint(),
        };
        let lab = pipette_plan_types::Tag::try_new("lab")?;
        let rejected = seed(&stores, "ev1_rejected").await?;
        let deleted = seed(&stores, "ev1_deleted").await?;
        for client in [&rejected, &deleted] {
            stores
                .orgs
                .set_ownership(&client.client_id, Some(owner.clone()))
                .await?;
            stores.tags.add_client_tag(&client.client_id, &lab).await?;
        }

        reject(
            &*stores.clients,
            &*stores.key_index,
            &*stores.orgs,
            &*stores.tags,
            &rejected.client_id,
        )
        .await?;
        delete(
            &*stores.clients,
            &*stores.key_index,
            &*stores.orgs,
            &*stores.tags,
            &*stores.todo,
            &deleted.client_id,
        )
        .await?;
        assert!(stores.orgs.list_ownerships().await?.is_empty());
        assert!(stores.tags.list_forward_tag_markers().await?.is_empty());
        assert!(stores.tags.list_reverse_tag_markers().await?.is_empty());
        Ok(())
    }
}
