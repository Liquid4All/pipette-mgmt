//! [`SqlClientStore`] keeps the client keyspaces as tables of the object
//! stores' JSON, with no foreign keys: registration writes a set before its
//! record, and a migration or spent marker outlives its record.

use std::num::NonZeroUsize;

use anyhow::Context;
use async_trait::async_trait;
use chrono::{DateTime, Utc};

use pipette_core::client::{
    Client, MigrationRecord, SignatureMigration, parse_client_or_self_heal,
};
use pipette_core::client_permission::ClientPermissionSet;
use pipette_core::client_preauth::{
    ClientPreauthConsumeOutcome, ClientPreauthKey, ClientPreauthRejectionError,
    ClientPreauthSecret, validate,
};
use pipette_core::types::{DeriveFromKey, id_from_key};
use pipette_plan_types::{ClientId, ClientPreauthKeyId, ClientPreauthUsage, IdError, PublicKeyHex};
use pipette_store::{ClientPage, ClientPermissionMutation, ClientStore};

use crate::db::SqlDatabase;
use crate::listing;
use crate::listing::Keyspace;
use crate::permissions::{PermissionContentionError, decode_permissions, encode_permissions};
use crate::retry::{RetryError, is_conflict, retry};

const CLIENTS: Keyspace = Keyspace {
    table: "clients",
    id_column: "client_id",
    report_prefix: "clients/",
};

#[derive(Clone)]
pub(crate) struct SqlClientStore {
    db: SqlDatabase,
}

impl SqlClientStore {
    pub(crate) fn new(db: SqlDatabase) -> Self {
        Self { db }
    }
}

/// A padded stored id is refused: trimmed, it would read as another client's.
fn client_id_verbatim(raw: String) -> Result<ClientId, IdError> {
    id_from_key(&raw, ClientId::try_new)
}

/// Write back only if the row holds the body and revision read. Best-effort:
/// the caller already holds the repaired client.
async fn persist_self_healed(
    db: &SqlDatabase,
    client_id: &ClientId,
    client: &Client,
    read: &str,
    revision: i64,
) {
    let client_id = client_id.as_str();
    let body = match serde_json::to_string_pretty(client) {
        Ok(body) => body,
        Err(e) => {
            tracing::warn!(%client_id, error = %e, "failed to serialize self-healed client record");
            return;
        }
    };
    // The body too: a deleted and re-created row restarts at revision 0.
    let written = db
        .execute("persist_self_healed_client", || {
            sqlx::query(
                "UPDATE clients SET body = $2, revision = revision + 1
                 WHERE client_id = $1 AND revision = $3 AND body = $4",
            )
            .bind(client_id)
            .bind(body.as_str())
            .bind(revision)
            .bind(read)
        })
        .await;
    match written {
        Ok(0) => tracing::warn!(
            %client_id,
            "skipped self-heal because client record changed during repair"
        ),
        Ok(_) => tracing::warn!(%client_id, "self-healed malformed client record"),
        Err(e) => tracing::warn!(
            %client_id,
            error = %format_args!("{e:#}"),
            "failed to persist self-healed client record"
        ),
    }
}

#[async_trait]
impl ClientStore for SqlClientStore {
    async fn get_client(&self, client_id: &ClientId) -> anyhow::Result<Option<Client>> {
        let db = &self.db;
        let row: Option<(String, i64)> = db
            .fetch_optional("get_client", || {
                sqlx::query_as("SELECT body, revision FROM clients WHERE client_id = $1")
                    .bind(client_id.as_str())
            })
            .await?;
        let Some((body, revision)) = row else {
            return Ok(None);
        };
        let (client, repaired) = parse_client_or_self_heal(body.as_bytes())
            .with_context(|| format!("reading the client record {client_id}"))?;
        if repaired {
            persist_self_healed(db, client_id, &client, &body, revision).await;
        }
        Ok(Some(client))
    }

    /// One statement for every device. A malformed record is repaired in the
    /// answer only; [`get_client`](Self::get_client) also repairs the table.
    async fn get_clients(&self, client_ids: &[ClientId]) -> anyhow::Result<Vec<Client>> {
        if client_ids.is_empty() {
            return Ok(Vec::new());
        }
        let ids: Vec<&str> = client_ids.iter().map(ClientId::as_str).collect();
        let rows: Vec<(String, String)> = self
            .db
            .fetch_all("get_clients", || {
                sqlx::query_as("SELECT client_id, body FROM clients WHERE client_id = ANY($1)")
                    .bind(ids.as_slice())
            })
            .await?;
        Ok(rows
            .into_iter()
            .filter_map(|(client_id, body)| {
                parse_client_or_self_heal(body.as_bytes())
                    .inspect_err(|e| {
                        tracing::warn!(%client_id, error = %format!("{e:#}"), "a client record did not read");
                    })
                    .ok()
                    .map(|(client, _repaired)| client)
            })
            .collect())
    }

    /// Last writer wins; device and operator edits write through [`Self::replace_client`].
    async fn put_client(&self, client: &Client) -> anyhow::Result<()> {
        let body = serde_json::to_string_pretty(client)?;
        self.db
            .execute("put_client", || {
                sqlx::query(
                    "INSERT INTO clients (client_id, body, revision) VALUES ($1, $2, 0)
                     ON CONFLICT (client_id)
                     DO UPDATE SET body = EXCLUDED.body, revision = clients.revision + 1",
                )
                .bind(client.client_id.as_str())
                .bind(body.as_str())
            })
            .await?;
        Ok(())
    }

    async fn replace_client(&self, read: &Client, updated: &Client) -> anyhow::Result<bool> {
        let db = &self.db;
        let client_id = read.client_id.as_str();
        let row: Option<(String, i64)> = db
            .fetch_optional("replace_client_read", || {
                sqlx::query_as("SELECT body, revision FROM clients WHERE client_id = $1")
                    .bind(client_id)
            })
            .await?;
        let Some((stored, revision)) = row else {
            return Ok(false);
        };
        let (current, _repaired) = parse_client_or_self_heal(stored.as_bytes())
            .with_context(|| format!("reading the client record {client_id}"))?;
        if current != *read {
            return Ok(false);
        }
        let body = serde_json::to_string_pretty(updated)?;
        // The body too: a deleted and re-created row restarts at revision 0.
        let written = db
            .execute("replace_client", || {
                sqlx::query(
                    "UPDATE clients SET body = $2, revision = revision + 1
                     WHERE client_id = $1 AND revision = $3 AND body = $4",
                )
                .bind(client_id)
                .bind(body.as_str())
                .bind(revision)
                .bind(stored.as_str())
            })
            .await?;
        Ok(written == 1)
    }

    async fn delete_client(&self, client_id: &ClientId) -> anyhow::Result<()> {
        let db = &self.db;
        let id = client_id.as_str();
        retry("delete_client", move || {
            Box::pin(async move {
                let mut tx = db.begin().await?;
                sqlx::query("DELETE FROM clients WHERE client_id = $1")
                    .bind(id)
                    .execute(&mut *tx)
                    .await?;
                sqlx::query("DELETE FROM client_permissions WHERE client_id = $1")
                    .bind(id)
                    .execute(&mut *tx)
                    .await?;
                tx.commit().await
            })
        })
        .await
    }

    async fn list_clients(
        &self,
        after: Option<&ClientId>,
        limit: NonZeroUsize,
    ) -> anyhow::Result<ClientPage> {
        let mut repaired = Vec::new();
        let page = listing::page(
            &self.db,
            CLIENTS,
            after,
            limit,
            client_id_verbatim,
            |client_id, body| {
                parse_client_or_self_heal(body.as_bytes()).map(|(client, was_repaired)| {
                    if was_repaired {
                        repaired.push(client_id.clone());
                    }
                    client
                })
            },
            |_| true,
        )
        .await?;
        // Healed through `get_client`, whose read carries the revision the
        // write-back is conditioned on; the page's read does not.
        for client_id in &repaired {
            if let Err(e) = self.get_client(client_id).await {
                tracing::warn!(%client_id, error = %format_args!("{e:#}"),
                    "failed to self-heal a listed client record");
            }
        }
        Ok(page)
    }

    /// Sorted here, as the object stores do, so the database collation cannot
    /// reorder the ids.
    async fn list_client_ids(&self) -> anyhow::Result<Vec<ClientId>> {
        let rows: Vec<(String,)> = self
            .db
            .fetch_all("list_client_ids", || {
                sqlx::query_as("SELECT client_id FROM clients")
            })
            .await?;
        let mut ids: Vec<ClientId> = rows
            .into_iter()
            .filter_map(|(raw,)| client_id_verbatim(raw).ok())
            .collect();
        ids.sort();
        Ok(ids)
    }

    /// A corrupt record still counts: the object stores answer with a `HEAD`.
    async fn has_public_key(&self, public_key: &PublicKeyHex) -> anyhow::Result<bool> {
        let client_id = ClientId::derive(public_key);
        let (exists,) = self
            .db
            .fetch_one("has_public_key", || {
                sqlx::query_as("SELECT EXISTS (SELECT 1 FROM clients WHERE client_id = $1)")
                    .bind(client_id.as_str())
            })
            .await?;
        Ok(exists)
    }

    async fn has_signature_migration(&self, client_id: &ClientId) -> anyhow::Result<bool> {
        let (exists,) = self
            .db
            .fetch_one("has_signature_migration", || {
                sqlx::query_as(
                    "SELECT EXISTS (SELECT 1 FROM signature_migration WHERE client_id = $1)",
                )
                .bind(client_id.as_str())
            })
            .await?;
        Ok(exists)
    }

    /// A racing duplicate fails with a conflict; the retry then finds the
    /// winner's row.
    async fn record_signature_migration(
        &self,
        client_id: &ClientId,
        at: DateTime<Utc>,
    ) -> anyhow::Result<MigrationRecord> {
        // In the body, not a `timestamptz`, which would drop the nanoseconds.
        let body = serde_json::to_string_pretty(&SignatureMigration { first_seen: at })?;
        let created = self
            .db
            .execute("record_signature_migration", || {
                sqlx::query(
                    "INSERT INTO signature_migration (client_id, body) VALUES ($1, $2)
                     ON CONFLICT (client_id) DO NOTHING",
                )
                .bind(client_id.as_str())
                .bind(body.as_str())
            })
            .await?;
        Ok(if created == 0 {
            MigrationRecord::Existing
        } else {
            MigrationRecord::First
        })
    }

    async fn list_signature_migrations(
        &self,
    ) -> anyhow::Result<Vec<(ClientId, SignatureMigration)>> {
        let rows: Vec<(String, String)> = self
            .db
            .fetch_all("list_signature_migrations", || {
                sqlx::query_as("SELECT client_id, body FROM signature_migration")
            })
            .await?;
        Ok(rows
            .into_iter()
            .filter_map(|(raw, body)| {
                let client_id = client_id_verbatim(raw).ok()?;
                let migration = serde_json::from_str::<SignatureMigration>(&body)
                    .inspect_err(|e| {
                        tracing::warn!(%client_id, error = %e,
                            "skipping malformed signature-migration marker");
                    })
                    .ok()?;
                Some((client_id, migration))
            })
            .collect())
    }

    async fn get_client_permissions(
        &self,
        client_id: &ClientId,
    ) -> anyhow::Result<Option<ClientPermissionSet>> {
        let body: Option<(String,)> = self
            .db
            .fetch_optional("get_client_permissions", || {
                sqlx::query_as("SELECT body FROM client_permissions WHERE client_id = $1")
                    .bind(client_id.as_str())
            })
            .await?;
        body.map(|(body,)| {
            decode_permissions(&body)
                .with_context(|| format!("reading the permission set of {client_id}"))
        })
        .transpose()
    }

    async fn list_client_permissions(
        &self,
    ) -> anyhow::Result<Vec<(ClientId, ClientPermissionSet)>> {
        let rows: Vec<(String, String)> = self
            .db
            .fetch_all("list_client_permissions", || {
                sqlx::query_as("SELECT client_id, body FROM client_permissions")
            })
            .await?;
        Ok(rows
            .into_iter()
            .filter_map(|(raw, body)| {
                let client_id = client_id_verbatim(raw).ok()?;
                let set = decode_permissions(&body)
                    .inspect_err(|e| {
                        tracing::warn!(%client_id, error = %e,
                            "skipping malformed client permission set");
                    })
                    .ok()?;
                Some((client_id, set))
            })
            .collect())
    }

    async fn put_client_permissions(
        &self,
        client_id: &ClientId,
        set: &ClientPermissionSet,
    ) -> anyhow::Result<()> {
        let body = encode_permissions(set)?;
        self.db
            .execute("put_client_permissions", || {
                sqlx::query(
                    "INSERT INTO client_permissions (client_id, body, revision) VALUES ($1, $2, 0)
                     ON CONFLICT (client_id)
                     DO UPDATE SET body = EXCLUDED.body, revision = client_permissions.revision + 1",
                )
                .bind(client_id.as_str())
                .bind(body.as_str())
            })
            .await?;
        Ok(())
    }

    async fn delete_client_permissions(&self, client_id: &ClientId) -> anyhow::Result<()> {
        self.db
            .execute("delete_client_permissions", || {
                sqlx::query("DELETE FROM client_permissions WHERE client_id = $1")
                    .bind(client_id.as_str())
            })
            .await?;
        Ok(())
    }

    /// The write is conditioned on the revision read; a conflict reruns `f`.
    async fn mutate_client_permissions(
        &self,
        client_id: &ClientId,
        f: &(dyn for<'s> Fn(&'s mut ClientPermissionSet) -> bool + Send + Sync),
    ) -> anyhow::Result<ClientPermissionMutation> {
        let db = &self.db;
        let id = client_id.as_str();
        let outcome = retry("mutate_client_permissions", move || {
            Box::pin(async move {
                let mut tx = db.begin().await?;
                let row: Option<(String, i64)> = sqlx::query_as(
                    "SELECT body, revision FROM client_permissions WHERE client_id = $1",
                )
                .bind(id)
                .fetch_optional(&mut *tx)
                .await?;
                let (mut set, revision) = match row {
                    Some((body, revision)) => (
                        decode_permissions(&body).map_err(|e| sqlx::Error::Decode(e.into()))?,
                        Some(revision),
                    ),
                    None => (ClientPermissionSet::default(), None),
                };
                if !f(&mut set) {
                    return Ok(Some(ClientPermissionMutation {
                        changed: false,
                        resulting: set,
                    }));
                }
                let body = encode_permissions(&set).map_err(|e| sqlx::Error::Encode(e.into()))?;
                let write = match revision {
                    Some(revision) => sqlx::query(
                        "UPDATE client_permissions SET body = $2, revision = revision + 1
                         WHERE client_id = $1 AND revision = $3",
                    )
                    .bind(id)
                    .bind(body)
                    .bind(revision),
                    None => sqlx::query(
                        "INSERT INTO client_permissions (client_id, body, revision)
                         VALUES ($1, $2, 0) ON CONFLICT (client_id) DO NOTHING",
                    )
                    .bind(id)
                    .bind(body),
                };
                // A snapshot that read the row sees it unchanged, so a miss is a
                // lost race the database did not report as a conflict.
                if write.execute(&mut *tx).await?.rows_affected() == 0 {
                    return Ok(None);
                }
                tx.commit().await?;
                Ok(Some(ClientPermissionMutation {
                    changed: true,
                    resulting: set,
                }))
            })
        })
        .await;
        match outcome {
            Ok(Some(mutation)) => Ok(mutation),
            Ok(None) => Err(PermissionContentionError {
                principal_id: id.to_owned(),
            }
            .into()),
            Err(e)
                if e.downcast_ref::<RetryError>()
                    .is_some_and(|e| is_conflict(e.sqlx())) =>
            {
                Err(e.context(PermissionContentionError {
                    principal_id: id.to_owned(),
                }))
            }
            Err(e) => Err(e.context(format!("mutating the permission set of {client_id}"))),
        }
    }

    async fn put_client_preauth_key(&self, key: &ClientPreauthKey) -> anyhow::Result<()> {
        let body = serde_json::to_string_pretty(key)?;
        self.db
            .execute("put_client_preauth_key", || {
                sqlx::query(
                    "INSERT INTO client_preauth (key_id, body) VALUES ($1, $2)
                     ON CONFLICT (key_id) DO UPDATE SET body = EXCLUDED.body",
                )
                .bind(key.key_id.as_str())
                .bind(body.as_str())
            })
            .await?;
        Ok(())
    }

    async fn get_client_preauth_key(
        &self,
        key_id: &ClientPreauthKeyId,
    ) -> anyhow::Result<Option<ClientPreauthKey>> {
        let body: Option<(String,)> = self
            .db
            .fetch_optional("get_client_preauth_key", || {
                sqlx::query_as("SELECT body FROM client_preauth WHERE key_id = $1")
                    .bind(key_id.as_str())
            })
            .await?;
        body.map(|(body,)| {
            serde_json::from_str(&body)
                .with_context(|| format!("reading the client pre-auth key {key_id}"))
        })
        .transpose()
    }

    /// The spent-row insert is what picks the one winner of a single-use spend.
    async fn consume_client_preauth_key(
        &self,
        key_id: &ClientPreauthKeyId,
        secret: &ClientPreauthSecret,
    ) -> anyhow::Result<ClientPreauthConsumeOutcome> {
        let db = &self.db;
        let id = key_id.as_str();
        retry("consume_client_preauth_key", move || {
            Box::pin(async move {
                let not_found =
                    ClientPreauthConsumeOutcome::Rejected(ClientPreauthRejectionError::NotFound);
                let mut tx = db.begin().await?;
                let body: Option<String> =
                    sqlx::query_scalar("SELECT body FROM client_preauth WHERE key_id = $1")
                        .bind(id)
                        .fetch_optional(&mut *tx)
                        .await?;
                let Some(body) = body else {
                    return Ok(not_found);
                };
                let key: ClientPreauthKey =
                    serde_json::from_str(&body).map_err(|e| sqlx::Error::Decode(e.into()))?;
                // A rejection writes nothing, so it never spends the key.
                let grant = match validate(&key, secret, Utc::now()) {
                    Ok(grant) => grant,
                    Err(rejection) => return Ok(ClientPreauthConsumeOutcome::Rejected(rejection)),
                };
                match key.usage {
                    ClientPreauthUsage::MultiUse => {
                        return Ok(ClientPreauthConsumeOutcome::Granted(grant));
                    }
                    ClientPreauthUsage::SingleUse => {}
                }
                let spent = sqlx::query(
                    "INSERT INTO client_preauth_spent (key_id, spent_at) VALUES ($1, now())
                     ON CONFLICT (key_id) DO NOTHING",
                )
                .bind(id)
                .execute(&mut *tx)
                .await?
                .rows_affected();
                // The key is already spent. Report it like every other refusal,
                // so a caller cannot use the endpoint to test a key.
                if spent == 0 {
                    return Ok(not_found);
                }
                sqlx::query("DELETE FROM client_preauth WHERE key_id = $1")
                    .bind(id)
                    .execute(&mut *tx)
                    .await?;
                tx.commit().await?;
                Ok(ClientPreauthConsumeOutcome::Granted(grant))
            })
        })
        .await
        .with_context(|| format!("consuming the client pre-auth key {key_id}"))
    }

    /// One corrupt record fails the whole listing, as on the object stores.
    async fn list_client_preauth_keys(&self) -> anyhow::Result<Vec<ClientPreauthKey>> {
        let rows: Vec<(String, String)> = self
            .db
            .fetch_all("list_client_preauth_keys", || {
                sqlx::query_as("SELECT key_id, body FROM client_preauth")
            })
            .await?;
        rows.into_iter()
            .map(|(key_id, body)| {
                serde_json::from_str(&body)
                    .with_context(|| format!("reading the client pre-auth key {key_id}"))
            })
            .collect()
    }

    async fn delete_client_preauth_key(&self, key_id: &ClientPreauthKeyId) -> anyhow::Result<()> {
        self.db
            .execute("delete_client_preauth_key", || {
                sqlx::query("DELETE FROM client_preauth WHERE key_id = $1").bind(key_id.as_str())
            })
            .await?;
        Ok(())
    }

    async fn list_spent_client_preauth_markers(&self) -> anyhow::Result<Vec<ClientPreauthKeyId>> {
        let ids: Vec<(String,)> = self
            .db
            .fetch_all("list_spent_client_preauth_markers", || {
                sqlx::query_as("SELECT key_id FROM client_preauth_spent ORDER BY key_id")
            })
            .await?;
        Ok(ids
            .into_iter()
            .filter_map(|(raw,)| {
                ClientPreauthKeyId::try_new(raw.as_str())
                    .inspect_err(|e| {
                        tracing::warn!(key_id = %raw, error = %e, "skipping unparseable spent marker");
                    })
                    .ok()
            })
            .collect())
    }

    async fn delete_spent_client_preauth_marker(
        &self,
        key_id: &ClientPreauthKeyId,
    ) -> anyhow::Result<()> {
        self.db
            .execute("delete_spent_client_preauth_marker", || {
                sqlx::query("DELETE FROM client_preauth_spent WHERE key_id = $1")
                    .bind(key_id.as_str())
            })
            .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::num::NonZeroUsize;

    use anyhow::Context;
    use chrono::{TimeDelta, Utc};

    use pipette_core::client::{Client, MigrationRecord};
    use pipette_core::client_permission::{ClientPermissionSet, StoredClientPermission};
    use pipette_core::client_preauth::{
        ClientPreauthConsumeOutcome, ClientPreauthParams, ClientPreauthRejectionError, mint,
    };
    use pipette_core::key_index::{KeyHolderPrincipalId, digest_of};
    use pipette_core::types::DeriveFromKey;
    use pipette_plan_types::PermissionName;
    use pipette_plan_types::{
        ClientId, ClientPermission, ClientPreauthUsage, NonEmptyTrimmedString,
    };
    use pipette_store::{ClientStore, KeyIndexStore};

    use super::{SqlClientStore, persist_self_healed};
    use crate::db::SqlDatabase;
    use crate::key_index::SqlKeyIndexStore;
    use crate::testing::{RACERS, a_client, public_key, race};

    /// A record written before `organization` was required and with a blank
    /// `client_details`: the strict parse refuses it, the repair accepts it.
    const LEGACY_RECORD: &str = r#"{
      "client_id": "ev1_legacy",
      "public_key": "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
      "client_details": "",
      "contact_email": "legacy@example.com",
      "status": "pending",
      "registered_at": "2026-01-01T00:00:00Z"
    }"#;

    /// The contract's raw-write hook. A `.json` leaf becomes the row its stem
    /// names, padded or not; another extension names no row.
    async fn write_raw(db: &SqlDatabase, key: String, bytes: Vec<u8>) -> anyhow::Result<()> {
        let (keyspace, leaf) = key
            .split_once('/')
            .context("a raw key names its keyspace")?;
        let sql = match keyspace {
            "clients" => {
                "INSERT INTO clients (client_id, body, revision) VALUES ($1, $2, 0)
                 ON CONFLICT (client_id)
                 DO UPDATE SET body = EXCLUDED.body, revision = clients.revision + 1"
            }
            "client-permissions" => {
                "INSERT INTO client_permissions (client_id, body, revision) VALUES ($1, $2, 0)
                 ON CONFLICT (client_id)
                 DO UPDATE SET body = EXCLUDED.body, revision = client_permissions.revision + 1"
            }
            "signature-migration" => {
                "INSERT INTO signature_migration (client_id, body) VALUES ($1, $2)
                 ON CONFLICT (client_id) DO UPDATE SET body = EXCLUDED.body"
            }
            other => anyhow::bail!("no client table holds the keyspace {other}"),
        };
        let Some(stem) = leaf.strip_suffix(".json") else {
            return Ok(());
        };
        sqlx::query(sql)
            .bind(stem)
            .bind(String::from_utf8(bytes)?)
            .execute(db.pool())
            .await?;
        Ok(())
    }

    async fn stored_client(db: &SqlDatabase, client_id: &str) -> anyhow::Result<(String, i64)> {
        Ok(
            sqlx::query_as("SELECT body, revision FROM clients WHERE client_id = $1")
                .bind(client_id)
                .fetch_one(db.pool())
                .await?,
        )
    }

    /// Write [`LEGACY_RECORD`] as the row of `ev1_legacy`.
    async fn write_legacy(db: &SqlDatabase) -> anyhow::Result<()> {
        write_raw(
            db,
            "clients/ev1_legacy.json".to_string(),
            LEGACY_RECORD.into(),
        )
        .await
    }

    /// Write [`LEGACY_RECORD`] and read it back as a repair reads it. The result
    /// is the repaired client, the stored body, and the revision of the read.
    async fn legacy_row(db: &SqlDatabase) -> anyhow::Result<(Client, String, i64)> {
        write_legacy(db).await?;
        let (legacy, read_at) = stored_client(db, "ev1_legacy").await?;
        let (healed, repaired) =
            pipette_core::client::parse_client_or_self_heal(legacy.as_bytes())?;
        assert!(repaired);
        Ok((healed, legacy, read_at))
    }

    fn single_use() -> ClientPreauthParams {
        ClientPreauthParams {
            usage: ClientPreauthUsage::SingleUse,
            expires_at: None,
            default_tags: BTreeSet::new(),
            default_organization: None,
            default_owner: None,
            default_permissions: None,
            note: None,
        }
    }

    #[tokio::test]
    async fn sql_client_listing_meets_the_shared_pagination_contract() -> anyhow::Result<()> {
        let test_db = crate::testing::test_db_or_skip!();
        let db = test_db.db();
        pipette_store_testkit::assert_client_listing_pagination(
            &SqlClientStore::new(db.clone()),
            |leaf, bytes| write_raw(db, format!("clients/{leaf}"), bytes),
        )
        .await
    }

    /// Read with others, a row that is no client record is left out, and a
    /// legacy one is repaired in the answer and not in the table.
    #[tokio::test]
    async fn a_corrupt_client_does_not_cost_the_batch() -> anyhow::Result<()> {
        let test_db = crate::testing::test_db_or_skip!();
        let db = test_db.db();
        let clients = SqlClientStore::new(db.clone());
        let a = a_client(ClientId::try_new("ev1_a")?, public_key(0x0a)?)?;
        clients.put_client(&a).await?;
        write_legacy(db).await?;
        write_raw(
            db,
            "clients/ev1_broken.json".to_string(),
            b"{not json".to_vec(),
        )
        .await?;

        let ids = ["ev1_broken", "ev1_legacy", "ev1_a"]
            .map(ClientId::try_new)
            .into_iter()
            .collect::<Result<Vec<_>, _>>()?;
        let mut read: Vec<String> = clients
            .get_clients(&ids)
            .await?
            .into_iter()
            .map(|client| client.client_id.to_string())
            .collect();
        read.sort();
        assert_eq!(read, vec!["ev1_a", "ev1_legacy"]);
        let (_, revision) = stored_client(db, "ev1_legacy").await?;
        assert_eq!(revision, 0, "the batch read repairs no row");
        Ok(())
    }

    #[tokio::test]
    async fn sql_signature_migration_keeps_the_first_sighting() -> anyhow::Result<()> {
        let test_db = crate::testing::test_db_or_skip!();
        let db = test_db.db();
        pipette_store_testkit::assert_signature_migration_keeps_first_sighting(
            &SqlClientStore::new(db.clone()),
            |key, bytes| write_raw(db, key, bytes),
        )
        .await
    }

    #[tokio::test]
    async fn sql_client_store_permissions() -> anyhow::Result<()> {
        let test_db = crate::testing::test_db_or_skip!();
        let db = test_db.db();
        let clients = SqlClientStore::new(db.clone());
        let a = a_client(ClientId::try_new("ev1_a")?, public_key(0x0a)?)?;
        clients.put_client(&a).await?;
        pipette_store_testkit::assert_client_permission_store_roundtrip(
            &clients,
            &a.client_id,
            |key, bytes| write_raw(db, key, bytes),
        )
        .await
    }

    #[tokio::test]
    async fn sql_client_replace_is_conditional() -> anyhow::Result<()> {
        let test_db = crate::testing::test_db_or_skip!();
        pipette_store_testkit::assert_client_replace_is_conditional(
            &SqlClientStore::new(test_db.db().clone()),
            &a_client(ClientId::try_new("ev1_a")?, public_key(0x0a)?)?,
        )
        .await
    }

    #[tokio::test]
    async fn sql_client_store_preauth_contract() -> anyhow::Result<()> {
        let test_db = crate::testing::test_db_or_skip!();
        pipette_store_testkit::assert_client_preauth_store_roundtrip(&SqlClientStore::new(
            test_db.db().clone(),
        ))
        .await
    }

    #[tokio::test]
    async fn get_client_self_heals_a_legacy_record_once() -> anyhow::Result<()> {
        let test_db = crate::testing::test_db_or_skip!();
        let db = test_db.db();
        let clients = SqlClientStore::new(db.clone());
        write_legacy(db).await?;

        let healed = clients
            .get_client(&ClientId::try_new("ev1_legacy")?)
            .await?
            .context("a repairable record reads as a client")?;
        assert_eq!(healed.organization.as_str(), "<unset>");
        assert_eq!(healed.client_details.as_str(), "legacy client");

        let (body, revision) = stored_client(db, "ev1_legacy").await?;
        let stored: Client = serde_json::from_str(&body)
            .context("the write-back leaves a record the strict parse accepts")?;
        assert_eq!(stored.organization.as_str(), "<unset>");
        assert_eq!(revision, 1);

        clients.get_client(&healed.client_id).await?;
        assert_eq!(stored_client(db, "ev1_legacy").await?.1, 1);
        Ok(())
    }

    #[tokio::test]
    async fn list_clients_self_heals_legacy_records() -> anyhow::Result<()> {
        let test_db = crate::testing::test_db_or_skip!();
        let db = test_db.db();
        let clients = SqlClientStore::new(db.clone());
        clients
            .put_client(&a_client(
                ClientId::try_new("ev1_valid")?,
                public_key(0x01)?,
            )?)
            .await?;
        write_legacy(db).await?;

        let listed = pipette_store::drain_clients(&clients).await?;
        assert_eq!(listed.items.len(), 2);
        assert!(listed.unreadable.is_empty());

        let (body, revision) = stored_client(db, "ev1_legacy").await?;
        let stored: Client = serde_json::from_str(&body)?;
        assert_eq!(stored.client_details.as_str(), "legacy client");
        assert_eq!(revision, 1);
        Ok(())
    }

    #[tokio::test]
    async fn a_self_heal_never_overwrites_a_newer_put() -> anyhow::Result<()> {
        let test_db = crate::testing::test_db_or_skip!();
        let db = test_db.db();
        let clients = SqlClientStore::new(db.clone());
        let (healed, legacy, read_at) = legacy_row(db).await?;

        let mut newer = healed.clone();
        newer.organization = NonEmptyTrimmedString::try_new("operator-org")?;
        clients.put_client(&newer).await?;
        persist_self_healed(db, &healed.client_id, &healed, &legacy, read_at).await;

        let current = clients
            .get_client(&healed.client_id)
            .await?
            .context("the record stays")?;
        assert_eq!(current.organization.as_str(), "operator-org");
        Ok(())
    }

    /// A deleted and re-created row is back at the revision the repair read,
    /// so the write-back must also find the body it read.
    #[tokio::test]
    async fn a_self_heal_never_overwrites_a_re_created_record() -> anyhow::Result<()> {
        let test_db = crate::testing::test_db_or_skip!();
        let db = test_db.db();
        let clients = SqlClientStore::new(db.clone());
        let (healed, legacy, read_at) = legacy_row(db).await?;

        clients.delete_client(&healed.client_id).await?;
        let reregistered = a_client(healed.client_id.clone(), healed.public_key.clone())?;
        clients.put_client(&reregistered).await?;
        let written = stored_client(db, "ev1_legacy").await?;
        assert_eq!(written.1, read_at);
        persist_self_healed(db, &healed.client_id, &healed, &legacy, read_at).await;

        assert_eq!(
            stored_client(db, "ev1_legacy").await?,
            written,
            "the re-registration stays"
        );
        Ok(())
    }

    #[tokio::test]
    async fn a_self_heal_writes_back_to_the_row_it_read() -> anyhow::Result<()> {
        let test_db = crate::testing::test_db_or_skip!();
        let db = test_db.db();
        let clients = SqlClientStore::new(db.clone());
        // Both rows hold the body naming `ev1_legacy`, so only the id the
        // update binds can tell them apart.
        for key in ["clients/ev1_misfiled.json", "clients/ev1_legacy.json"] {
            write_raw(db, key.to_string(), LEGACY_RECORD.into()).await?;
        }

        clients
            .get_client(&ClientId::try_new("ev1_misfiled")?)
            .await?
            .context("a repairable record reads as a client")?;
        let (healed, revision) = stored_client(db, "ev1_misfiled").await?;
        assert_eq!(revision, 1);
        serde_json::from_str::<Client>(&healed).context("the row read is the row healed")?;
        assert_eq!(
            stored_client(db, "ev1_legacy").await?,
            (LEGACY_RECORD.to_string(), 0),
            "the row the body names is not touched"
        );
        Ok(())
    }

    #[tokio::test]
    async fn has_public_key_asks_whether_the_derived_record_exists() -> anyhow::Result<()> {
        let test_db = crate::testing::test_db_or_skip!();
        let db = test_db.db();
        let clients = SqlClientStore::new(db.clone());
        let registered = public_key(0x11)?;
        clients
            .put_client(&a_client(
                ClientId::derive(&registered),
                registered.clone(),
            )?)
            .await?;
        assert!(clients.has_public_key(&registered).await?);
        assert!(!clients.has_public_key(&public_key(0x22)?).await?);

        let corrupt = public_key(0x33)?;
        let corrupt_id = ClientId::derive(&corrupt);
        write_raw(
            db,
            format!("clients/{corrupt_id}.json"),
            b"{not json".to_vec(),
        )
        .await?;
        assert!(clients.has_public_key(&corrupt).await?);

        clients
            .delete_client(&ClientId::derive(&registered))
            .await?;
        assert!(!clients.has_public_key(&registered).await?);
        Ok(())
    }

    #[tokio::test]
    async fn delete_client_takes_the_record_and_its_set_only() -> anyhow::Result<()> {
        let test_db = crate::testing::test_db_or_skip!();
        let clients = SqlClientStore::new(test_db.db().clone());
        let claims = SqlKeyIndexStore::new(test_db.db().clone());
        let key = public_key(0x44)?;
        let record = a_client(ClientId::derive(&key), key.clone())?;
        let client_id = record.client_id.clone();
        let holder = KeyHolderPrincipalId::Client(client_id.clone());
        claims.claim_public_key(&digest_of(&key), &holder).await?;
        clients
            .put_client_permissions(
                &client_id,
                &ClientPermissionSet::from_iter([ClientPermission::PerformWork]),
            )
            .await?;
        clients
            .record_signature_migration(&client_id, Utc::now())
            .await?;
        clients.put_client(&record).await?;

        clients.delete_client(&client_id).await?;
        assert!(clients.get_client(&client_id).await?.is_none());
        assert!(clients.get_client_permissions(&client_id).await?.is_none());
        assert!(
            clients.has_signature_migration(&client_id).await?,
            "the migration marker outlives the record"
        );
        assert_eq!(
            claims.holder_of_public_key(&digest_of(&key)).await?,
            Some(holder),
            "the key claim is the caller's to release"
        );
        // Idempotent.
        clients.delete_client(&client_id).await?;
        Ok(())
    }

    /// `list_clients` sorts object keys, where `ev1_a-1.json` comes first.
    #[tokio::test]
    async fn the_two_client_listings_keep_their_own_orders() -> anyhow::Result<()> {
        let test_db = crate::testing::test_db_or_skip!();
        let db = test_db.db();
        let clients = SqlClientStore::new(db.clone());
        for (name, byte) in [("ev1_a-1", 0x01), ("ev1_a", 0x02)] {
            clients
                .put_client(&a_client(ClientId::try_new(name)?, public_key(byte)?)?)
                .await?;
        }
        write_raw(db, "clients/ev1_b.json".to_string(), b"{not json".to_vec()).await?;

        let ids = clients.list_client_ids().await?;
        assert_eq!(
            ids.iter().map(ClientId::as_str).collect::<Vec<_>>(),
            ["ev1_a", "ev1_a-1", "ev1_b"]
        );
        let listed = pipette_store::drain_clients(&clients).await?;
        assert_eq!(
            listed
                .items
                .iter()
                .map(|c| c.client_id.as_str())
                .collect::<Vec<_>>(),
            ["ev1_a-1", "ev1_a"]
        );
        assert_eq!(listed.unreadable.len(), 1);
        Ok(())
    }

    #[tokio::test]
    async fn stray_ids_after_a_full_page_do_not_extend_the_listing() -> anyhow::Result<()> {
        let test_db = crate::testing::test_db_or_skip!();
        let db = test_db.db();
        let clients = SqlClientStore::new(db.clone());
        for (name, byte) in [("ev1_a", 0x01), ("ev1_b", 0x02), ("ev1_c", 0x03)] {
            clients
                .put_client(&a_client(ClientId::try_new(name)?, public_key(byte)?)?)
                .await?;
        }
        // Each sorts right after the valid id it extends.
        for key in ["clients/ev1_b.x.json", "clients/ev1_c.x.json"] {
            write_raw(db, key.to_string(), b"{not json".to_vec()).await?;
        }
        let two = NonZeroUsize::new(2).context("two is not zero")?;

        let page = clients.list_clients(None, two).await?;
        assert_eq!(
            page.resume_after.as_ref().map(ClientId::as_str),
            Some("ev1_b"),
            "a valid id past the stray one is more to come"
        );
        let page = clients
            .list_clients(Some(&ClientId::try_new("ev1_a")?), two)
            .await?;
        assert_eq!(
            page.items
                .iter()
                .map(|c| c.client_id.as_str())
                .collect::<Vec<_>>(),
            ["ev1_b", "ev1_c"]
        );
        assert!(page.unreadable.is_empty());
        assert_eq!(page.resume_after, None, "only a stray id is left");
        Ok(())
    }

    #[tokio::test]
    async fn list_client_ids_keeps_byte_order_under_any_collation() -> anyhow::Result<()> {
        let test_db = crate::testing::test_db_or_skip!();
        let db = test_db.db();
        let clients = SqlClientStore::new(db.clone());
        for (name, byte) in [("ev1_a", 0x01), ("ev1_B", 0x02)] {
            clients
                .put_client(&a_client(ClientId::try_new(name)?, public_key(byte)?)?)
                .await?;
        }
        sqlx::query(r#"ALTER TABLE clients ALTER COLUMN client_id TYPE text COLLATE "und-x-icu""#)
            .execute(db.pool())
            .await?;
        let collated: Vec<String> =
            sqlx::query_scalar("SELECT client_id FROM clients ORDER BY client_id")
                .fetch_all(db.pool())
                .await?;
        assert_eq!(collated, ["ev1_a", "ev1_B"], "the collation reorders");

        let ids = clients.list_client_ids().await?;
        assert_eq!(
            ids.iter().map(ClientId::as_str).collect::<Vec<_>>(),
            ["ev1_B", "ev1_a"]
        );
        Ok(())
    }

    #[tokio::test]
    async fn a_corrupt_permission_set_fails_its_reads_and_leaves_the_listing() -> anyhow::Result<()>
    {
        let test_db = crate::testing::test_db_or_skip!();
        let db = test_db.db();
        let clients = SqlClientStore::new(db.clone());
        let corrupt = ClientId::try_new("ev1_corrupt")?;
        let sound = ClientId::try_new("ev1_sound")?;
        write_raw(
            db,
            format!("client-permissions/{corrupt}.json"),
            b"{not json".to_vec(),
        )
        .await?;
        let granted = ClientPermissionSet::from_iter([ClientPermission::SubmitResults]);
        clients.put_client_permissions(&sound, &granted).await?;

        assert!(clients.get_client_permissions(&corrupt).await.is_err());
        assert!(
            clients
                .mutate_client_permissions(&corrupt, &|set| {
                    set.insert_all([ClientPermission::PerformWork])
                })
                .await
                .is_err(),
            "a mutation never starts over from a set it could not read"
        );
        assert_eq!(
            clients.list_client_permissions().await?,
            vec![(sound, granted)]
        );
        let body: String =
            sqlx::query_scalar("SELECT body FROM client_permissions WHERE client_id = $1")
                .bind(corrupt.as_str())
                .fetch_one(db.pool())
                .await?;
        assert_eq!(body, "{not json", "the failed mutation wrote nothing");
        Ok(())
    }

    /// The row holds the object stores' bytes, envelope included, so a set
    /// moves between the backends unchanged.
    #[tokio::test]
    async fn a_permission_set_is_stored_in_its_object_envelope() -> anyhow::Result<()> {
        let test_db = crate::testing::test_db_or_skip!();
        let db = test_db.db();
        let clients = SqlClientStore::new(db.clone());
        let client_id = ClientId::try_new("ev1_enveloped")?;
        clients
            .put_client_permissions(
                &client_id,
                &ClientPermissionSet::from_iter([
                    ClientPermission::SubmitResults,
                    ClientPermission::PerformWork,
                ]),
            )
            .await?;
        let body: String =
            sqlx::query_scalar("SELECT body FROM client_permissions WHERE client_id = $1")
                .bind(client_id.as_str())
                .fetch_one(db.pool())
                .await?;
        let body: serde_json::Value = serde_json::from_str(&body)?;
        assert_eq!(
            body.pointer("/permissions"),
            Some(&serde_json::Value::from(vec![
                "perform_work",
                "submit_results"
            ]))
        );
        Ok(())
    }

    #[tokio::test]
    async fn a_corrupt_preauth_record_fails_every_read_of_it() -> anyhow::Result<()> {
        let test_db = crate::testing::test_db_or_skip!();
        let db = test_db.db();
        let clients = SqlClientStore::new(db.clone());
        let minted = mint(single_use(), Utc::now())?;
        let (key_id, secret) = minted
            .token
            .parse_parts()
            .context("a minted token parses")?;
        sqlx::query("INSERT INTO client_preauth (key_id, body) VALUES ($1, $2)")
            .bind(key_id.as_str())
            .bind("{not json")
            .execute(db.pool())
            .await?;

        assert!(clients.get_client_preauth_key(&key_id).await.is_err());
        assert!(clients.list_client_preauth_keys().await.is_err());
        assert!(
            clients
                .consume_client_preauth_key(&key_id, &secret)
                .await
                .is_err()
        );
        assert!(
            clients
                .list_spent_client_preauth_markers()
                .await?
                .is_empty()
        );
        Ok(())
    }

    /// A crash on the object stores can leave a record beside its spent marker.
    #[tokio::test]
    async fn a_marker_beside_a_surviving_record_keeps_the_key_spent() -> anyhow::Result<()> {
        let test_db = crate::testing::test_db_or_skip!();
        let db = test_db.db();
        let clients = SqlClientStore::new(db.clone());
        let minted = mint(single_use(), Utc::now())?;
        let (key_id, secret) = minted
            .token
            .parse_parts()
            .context("a minted token parses")?;
        clients.put_client_preauth_key(&minted.key).await?;
        sqlx::query("INSERT INTO client_preauth_spent (key_id, spent_at) VALUES ($1, now())")
            .bind(key_id.as_str())
            .execute(db.pool())
            .await?;

        assert_eq!(
            clients.consume_client_preauth_key(&key_id, &secret).await?,
            ClientPreauthConsumeOutcome::Rejected(ClientPreauthRejectionError::NotFound)
        );
        assert_eq!(
            clients.get_client_preauth_key(&key_id).await?,
            Some(minted.key),
            "a lost spend deletes nothing"
        );
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn racing_migrations_keep_one_first_sighting() -> anyhow::Result<()> {
        let test_db = crate::testing::test_db_or_skip!();
        let clients = SqlClientStore::new(test_db.db().clone());
        let client_id = ClientId::try_new("ev1_racing")?;
        let base = Utc::now();
        let outcomes = race(|nth| {
            let (clients, client_id) = (clients.clone(), client_id.clone());
            async move {
                let at = base + TimeDelta::milliseconds(i64::try_from(nth)?);
                let outcome = clients.record_signature_migration(&client_id, at).await?;
                Ok((at, outcome))
            }
        })
        .await?;

        let firsts: Vec<_> = outcomes
            .iter()
            .filter(|(_, outcome)| *outcome == MigrationRecord::First)
            .map(|(at, _)| *at)
            .collect();
        assert_eq!(firsts.len(), 1, "exactly one racer writes the marker");
        let listed = clients.list_signature_migrations().await?;
        assert_eq!(listed.len(), 1);
        assert_eq!(
            listed.first().map(|(_, migration)| migration.first_seen),
            firsts.first().copied(),
            "the marker holds the winner's sighting"
        );
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn racing_consumes_spend_a_single_use_key_once() -> anyhow::Result<()> {
        let test_db = crate::testing::test_db_or_skip!();
        let clients = SqlClientStore::new(test_db.db().clone());
        let minted = mint(single_use(), Utc::now())?;
        let (key_id, secret) = minted
            .token
            .parse_parts()
            .context("a minted token parses")?;
        clients.put_client_preauth_key(&minted.key).await?;
        let outcomes = race(|_| {
            let (clients, key_id, secret) = (clients.clone(), key_id.clone(), secret.clone());
            async move { clients.consume_client_preauth_key(&key_id, &secret).await }
        })
        .await?;

        let granted = ClientPreauthConsumeOutcome::Granted(minted.key.grant());
        let not_found =
            ClientPreauthConsumeOutcome::Rejected(ClientPreauthRejectionError::NotFound);
        assert_eq!(outcomes.iter().filter(|o| **o == granted).count(), 1);
        assert_eq!(
            outcomes.iter().filter(|o| **o == not_found).count(),
            RACERS - 1,
            "every loser reads as unknown: {outcomes:?}"
        );
        assert_eq!(
            clients.list_spent_client_preauth_markers().await?,
            std::slice::from_ref(&key_id)
        );
        assert_eq!(clients.get_client_preauth_key(&key_id).await?, None);
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn racing_replaces_from_one_read_land_once() -> anyhow::Result<()> {
        let test_db = crate::testing::test_db_or_skip!();
        let clients = SqlClientStore::new(test_db.db().clone());
        let client = a_client(ClientId::try_new("ev1_replaced")?, public_key(0x0b)?)?;
        clients.put_client(&client).await?;
        let read = clients
            .get_client(&client.client_id)
            .await?
            .context("the seeded record reads back")?;
        let outcomes = race(|nth| {
            let (clients, read) = (clients.clone(), read.clone());
            async move {
                let updated = Client {
                    client_details: NonEmptyTrimmedString::try_new(format!("edit {nth}"))?,
                    ..read.clone()
                };
                let replaced = clients.replace_client(&read, &updated).await?;
                Ok((updated, replaced))
            }
        })
        .await?;

        let winners: Vec<_> = outcomes
            .iter()
            .filter(|(_, replaced)| *replaced)
            .map(|(updated, _)| updated)
            .collect();
        assert_eq!(winners.len(), 1, "exactly one replace lands: {outcomes:?}");
        assert_eq!(
            clients.get_client(&client.client_id).await?.as_ref(),
            winners.first().copied(),
            "the record holds the winner's edit"
        );
        Ok(())
    }

    /// The set starts absent, so the first writers race on the insert as well.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn racing_permission_grants_all_land() -> anyhow::Result<()> {
        let test_db = crate::testing::test_db_or_skip!();
        let clients = SqlClientStore::new(test_db.db().clone());
        let client_id = ClientId::try_new("ev1_contested")?;
        let names = (0..RACERS)
            .map(|nth| PermissionName::try_new(format!("grant_{nth}")))
            .collect::<Result<Vec<_>, _>>()?;
        let outcomes = race(|nth| {
            let (clients, client_id) = (clients.clone(), client_id.clone());
            let member = StoredClientPermission::classify(names[nth].clone());
            async move {
                clients
                    .mutate_client_permissions(&client_id, &move |set| set.insert(member.clone()))
                    .await
            }
        })
        .await?;
        assert!(outcomes.iter().all(|mutation| mutation.changed));

        let stored = clients
            .get_client_permissions(&client_id)
            .await?
            .context("the grants created the set")?;
        assert_eq!(
            stored.names(),
            names.iter().map(PermissionName::as_str).collect::<Vec<_>>()
        );
        Ok(())
    }
}
