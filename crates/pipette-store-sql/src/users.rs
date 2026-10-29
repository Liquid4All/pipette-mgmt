//! [`SqlUserStore`] keeps the user keyspaces as tables, with no foreign keys:
//! as on the object stores, a claim, key, set or mark may precede and outlive
//! the account row; the caller removes them.

use anyhow::Context;
use async_trait::async_trait;
use serde_json::{Map, Value};
use sqlx::PgConnection;

use pipette_core::user::{User, UserKey};
use pipette_core::user_permission::UserPermissionSet;
use pipette_core::user_preauth::{
    UserPreauthConsumeOutcome, UserPreauthKey, UserPreauthRejectionError, UserPreauthSecret,
};
use pipette_plan_types::{FirstClerkSignIn, UserId, UserKeyId, UserPreauthKeyId, UserPreauthUsage};
use pipette_store::{EmailClaim, UserPermissionMutation, UserStore};

use crate::ClaimVanishedError;
use crate::db::SqlDatabase;
use crate::permissions::{PermissionContentionError, decode_permissions, encode_permissions};
use crate::retry::{RetryError, is_conflict, retry};

/// The user store shares no table with the client store, so neither listing can
/// return the principals of the other.
#[derive(Clone, Debug)]
pub(crate) struct SqlUserStore {
    db: SqlDatabase,
}

impl SqlUserStore {
    pub(crate) fn new(db: SqlDatabase) -> Self {
        Self { db }
    }
}

#[async_trait]
impl UserStore for SqlUserStore {
    async fn get_user(&self, user_id: &UserId) -> anyhow::Result<Option<User>> {
        let row: Option<(String, String, Option<String>)> = self
            .db
            .fetch_optional("get_user", || {
                sqlx::query_as(
                    "SELECT body, account_type, campaign_scope::text FROM users WHERE user_id = $1",
                )
                .bind(user_id.as_str())
            })
            .await?;
        row.map(|(body, account_type, campaign_scope)| {
            read_user(&body, &account_type, campaign_scope.as_deref())
                .with_context(|| format!("user record {user_id}"))
        })
        .transpose()
    }

    async fn put_user(&self, user: &User) -> anyhow::Result<()> {
        let body = user_body(user)?;
        let campaign_scope = user
            .campaign_scope
            .as_ref()
            .map(serde_json::to_string)
            .transpose()?;
        self.db
            .execute("put_user", || {
                sqlx::query(
                    "INSERT INTO users (user_id, body, account_type, campaign_scope)
                     VALUES ($1, $2, $3, $4::jsonb)
                     ON CONFLICT (user_id) DO UPDATE
                     SET body = EXCLUDED.body, account_type = EXCLUDED.account_type,
                         campaign_scope = EXCLUDED.campaign_scope",
                )
                .bind(user.user_id.as_str())
                .bind(body.as_str())
                .bind(user.account_type.as_ref())
                .bind(campaign_scope.as_deref())
            })
            .await?;
        Ok(())
    }

    async fn delete_user(&self, user_id: &UserId) -> anyhow::Result<()> {
        self.db
            .execute("delete_user", || {
                sqlx::query("DELETE FROM users WHERE user_id = $1").bind(user_id.as_str())
            })
            .await?;
        Ok(())
    }

    async fn list_users(&self) -> anyhow::Result<Vec<User>> {
        let rows: Vec<(String, String, String, Option<String>)> = self
            .db
            .fetch_all("list_users", || {
                sqlx::query_as(
                    "SELECT user_id, body, account_type, campaign_scope::text
                     FROM users ORDER BY user_id",
                )
            })
            .await?;
        // One corrupt account must not hide the roster, as on the object stores.
        Ok(rows
            .into_iter()
            .filter_map(|(user_id, body, account_type, campaign_scope)| {
                read_user(&body, &account_type, campaign_scope.as_deref())
                    .inspect_err(|e| {
                        tracing::warn!(%user_id, error = %e, "skipping malformed user record");
                    })
                    .ok()
            })
            .collect())
    }

    async fn claim_user_email(&self, email: &str, user_id: &UserId) -> anyhow::Result<EmailClaim> {
        let db = &self.db;
        let id = user_id.as_str();
        let claim = retry("claim_user_email", || {
            Box::pin(async move {
                let mut tx = db.begin().await?;
                let claim = claim_within(&mut tx, email, id).await?;
                tx.commit().await?;
                Ok(claim)
            })
        })
        .await?;
        match claim {
            Claim::Created => Ok(EmailClaim::Claimed),
            Claim::Held(Some(holder)) => Ok(EmailClaim::Taken(parse_holder(email, holder)?)),
            Claim::Held(None) => Err(ClaimVanishedError {
                claim: "address",
                name: email.to_owned(),
            }
            .into()),
        }
    }

    async fn lookup_user_by_email(&self, email: &str) -> anyhow::Result<Option<UserId>> {
        let holder: Option<(String,)> = self
            .db
            .fetch_optional("lookup_user_by_email", || {
                sqlx::query_as("SELECT user_id FROM user_email_index WHERE email = $1").bind(email)
            })
            .await?;
        holder
            .map(|(holder,)| parse_holder(email, holder))
            .transpose()
    }

    async fn release_user_email(&self, email: &str) -> anyhow::Result<()> {
        self.db
            .execute("release_user_email", || {
                sqlx::query("DELETE FROM user_email_index WHERE email = $1").bind(email)
            })
            .await?;
        Ok(())
    }

    async fn put_user_key(&self, user_id: &UserId, key: &UserKey) -> anyhow::Result<()> {
        let body = serde_json::to_string_pretty(key)?;
        self.db
            .execute("put_user_key", || {
                sqlx::query(
                    "INSERT INTO user_keys (user_id, key_id, body) VALUES ($1, $2, $3)
                     ON CONFLICT (user_id, key_id) DO UPDATE SET body = EXCLUDED.body",
                )
                .bind(user_id.as_str())
                .bind(key.key_id.as_str())
                .bind(body.as_str())
            })
            .await?;
        Ok(())
    }

    async fn get_user_key(
        &self,
        user_id: &UserId,
        key_id: &UserKeyId,
    ) -> anyhow::Result<Option<UserKey>> {
        let body: Option<(String,)> = self
            .db
            .fetch_optional("get_user_key", || {
                sqlx::query_as("SELECT body FROM user_keys WHERE user_id = $1 AND key_id = $2")
                    .bind(user_id.as_str())
                    .bind(key_id.as_str())
            })
            .await?;
        body.map(|(body,)| {
            serde_json::from_str(&body).with_context(|| format!("user key {user_id}/{key_id}"))
        })
        .transpose()
    }

    async fn list_user_keys(&self, user_id: &UserId) -> anyhow::Result<Vec<UserKey>> {
        let rows: Vec<(String, String)> = self
            .db
            .fetch_all("list_user_keys", || {
                sqlx::query_as(
                    "SELECT key_id, body FROM user_keys WHERE user_id = $1 ORDER BY key_id",
                )
                .bind(user_id.as_str())
            })
            .await?;
        let mut keys: Vec<UserKey> = rows
            .into_iter()
            .filter_map(|(key_id, body)| {
                serde_json::from_str(&body)
                    .inspect_err(|e| {
                        tracing::warn!(%user_id, %key_id, error = %e, "skipping malformed user key record");
                    })
                    .ok()
            })
            .collect();
        // Sort here, not on a column: `timestamptz` keeps microseconds, the
        // record nanoseconds. The stable sort keeps ties in key-id order.
        keys.sort_by_key(|key| key.added_at);
        Ok(keys)
    }

    async fn delete_user_key(&self, user_id: &UserId, key_id: &UserKeyId) -> anyhow::Result<()> {
        self.db
            .execute("delete_user_key", || {
                sqlx::query("DELETE FROM user_keys WHERE user_id = $1 AND key_id = $2")
                    .bind(user_id.as_str())
                    .bind(key_id.as_str())
            })
            .await?;
        Ok(())
    }

    async fn get_user_permissions(
        &self,
        user_id: &UserId,
    ) -> anyhow::Result<Option<UserPermissionSet>> {
        let body: Option<(String,)> = self
            .db
            .fetch_optional("get_user_permissions", || {
                sqlx::query_as("SELECT body FROM user_permissions WHERE user_id = $1")
                    .bind(user_id.as_str())
            })
            .await?;
        body.map(|(body,)| {
            decode_permissions(&body).with_context(|| format!("permissions for {user_id}"))
        })
        .transpose()
    }

    async fn put_user_permissions(
        &self,
        user_id: &UserId,
        set: &UserPermissionSet,
    ) -> anyhow::Result<()> {
        let body = encode_permissions(set)?;
        // The revision changes on a blind write too, so a mutation that read
        // the row before the blind write cannot write over it.
        self.db
            .execute("put_user_permissions", || {
                sqlx::query(
                    "INSERT INTO user_permissions (user_id, revision, body) VALUES ($1, 1, $2)
                     ON CONFLICT (user_id) DO UPDATE
                     SET body = EXCLUDED.body, revision = user_permissions.revision + 1",
                )
                .bind(user_id.as_str())
                .bind(body.as_str())
            })
            .await?;
        Ok(())
    }

    async fn delete_user_permissions(&self, user_id: &UserId) -> anyhow::Result<()> {
        self.db
            .execute("delete_user_permissions", || {
                sqlx::query("DELETE FROM user_permissions WHERE user_id = $1")
                    .bind(user_id.as_str())
            })
            .await?;
        Ok(())
    }

    /// Write back only over the revision read; a conflict reruns `f` on the
    /// winner's set.
    async fn mutate_user_permissions(
        &self,
        user_id: &UserId,
        f: &(dyn for<'s> Fn(&'s mut UserPermissionSet) -> bool + Send + Sync),
    ) -> anyhow::Result<UserPermissionMutation> {
        let db = &self.db;
        let id = user_id.as_str();
        let outcome = retry("mutate_user_permissions", || {
            Box::pin(async move {
                let mut tx = db.begin().await?;
                let mutation = mutate_within(&mut tx, id, f).await?;
                tx.commit().await?;
                Ok(mutation)
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
            Err(e) => Err(e),
        }
    }

    async fn put_user_preauth_key(&self, key: &UserPreauthKey) -> anyhow::Result<()> {
        let body = serde_json::to_string_pretty(key)?;
        self.db
            .execute("put_user_preauth_key", || {
                sqlx::query(
                    "INSERT INTO user_preauth (key_id, body) VALUES ($1, $2)
                     ON CONFLICT (key_id) DO UPDATE SET body = EXCLUDED.body",
                )
                .bind(key.key_id.as_str())
                .bind(body.as_str())
            })
            .await?;
        Ok(())
    }

    /// One transaction runs the spend. A revoke that races the spend conflicts
    /// with it and does not lose to it. A spend never leaves its record behind.
    async fn consume_user_preauth_key(
        &self,
        key_id: &UserPreauthKeyId,
        secret: &UserPreauthSecret,
    ) -> anyhow::Result<UserPreauthConsumeOutcome> {
        let db = &self.db;
        let id = key_id.as_str();
        retry("consume_user_preauth_key", || {
            Box::pin(async move {
                let mut tx = db.begin().await?;
                let outcome = consume_within(&mut tx, id, secret).await?;
                tx.commit().await?;
                Ok(outcome)
            })
        })
        .await
    }

    async fn list_user_preauth_keys(&self) -> anyhow::Result<Vec<UserPreauthKey>> {
        let rows: Vec<(String, String)> = self
            .db
            .fetch_all("list_user_preauth_keys", || {
                sqlx::query_as("SELECT key_id, body FROM user_preauth ORDER BY key_id")
            })
            .await?;
        // A corrupt key fails the listing on both object stores, so it does here.
        rows.into_iter()
            .map(|(key_id, body)| {
                serde_json::from_str(&body).with_context(|| format!("user pre-auth key {key_id}"))
            })
            .collect()
    }

    async fn delete_user_preauth_key(&self, key_id: &UserPreauthKeyId) -> anyhow::Result<()> {
        self.db
            .execute("delete_user_preauth_key", || {
                sqlx::query("DELETE FROM user_preauth WHERE key_id = $1").bind(key_id.as_str())
            })
            .await?;
        Ok(())
    }

    async fn list_spent_user_preauth_markers(&self) -> anyhow::Result<Vec<UserPreauthKeyId>> {
        let rows: Vec<(String,)> = self
            .db
            .fetch_all("list_spent_user_preauth_markers", || {
                sqlx::query_as("SELECT key_id FROM user_preauth_spent ORDER BY key_id")
            })
            .await?;
        Ok(rows
            .into_iter()
            .filter_map(|(key_id,)| {
                UserPreauthKeyId::try_new(key_id.clone())
                    .inspect_err(|e| {
                        tracing::warn!(%key_id, error = %e, "skipping unparseable spent marker");
                    })
                    .ok()
            })
            .collect())
    }

    async fn delete_spent_user_preauth_marker(
        &self,
        key_id: &UserPreauthKeyId,
    ) -> anyhow::Result<()> {
        self.db
            .execute("delete_spent_user_preauth_marker", || {
                sqlx::query("DELETE FROM user_preauth_spent WHERE key_id = $1")
                    .bind(key_id.as_str())
            })
            .await?;
        Ok(())
    }

    async fn mark_user_awaiting_sign_in(&self, user_id: &UserId) -> anyhow::Result<()> {
        self.db
            .execute("mark_user_awaiting_sign_in", || {
                sqlx::query(
                    "INSERT INTO user_awaiting_sign_in (user_id, marked_at) VALUES ($1, now())
                     ON CONFLICT (user_id) DO NOTHING",
                )
                .bind(user_id.as_str())
            })
            .await?;
        Ok(())
    }

    async fn user_awaits_sign_in(&self, user_id: &UserId) -> anyhow::Result<bool> {
        let marked: Option<(i32,)> = self
            .db
            .fetch_optional("user_awaits_sign_in", || {
                sqlx::query_as("SELECT 1 FROM user_awaiting_sign_in WHERE user_id = $1")
                    .bind(user_id.as_str())
            })
            .await?;
        Ok(marked.is_some())
    }

    async fn clear_user_awaiting_sign_in(&self, user_id: &UserId) -> anyhow::Result<()> {
        self.db
            .execute("clear_user_awaiting_sign_in", || {
                sqlx::query("DELETE FROM user_awaiting_sign_in WHERE user_id = $1")
                    .bind(user_id.as_str())
            })
            .await?;
        Ok(())
    }

    async fn record_user_first_clerk_sign_in(
        &self,
        user_id: &UserId,
        first: &FirstClerkSignIn,
    ) -> anyhow::Result<bool> {
        let body = serde_json::to_string(first)?;
        let inserted = self
            .db
            .execute("record_user_first_clerk_sign_in", || {
                sqlx::query(
                    "INSERT INTO user_first_clerk_sign_in (user_id, body) VALUES ($1, $2)
                     ON CONFLICT (user_id) DO NOTHING",
                )
                .bind(user_id.as_str())
                .bind(body.as_str())
            })
            .await?;
        Ok(inserted == 1)
    }

    async fn get_user_first_clerk_sign_in(
        &self,
        user_id: &UserId,
    ) -> anyhow::Result<Option<FirstClerkSignIn>> {
        let row: Option<(String,)> = self
            .db
            .fetch_optional("get_user_first_clerk_sign_in", || {
                sqlx::query_as("SELECT body FROM user_first_clerk_sign_in WHERE user_id = $1")
                    .bind(user_id.as_str())
            })
            .await?;
        row.map(|(body,)| {
            serde_json::from_str(&body)
                .with_context(|| format!("first Clerk sign-in record {user_id}"))
        })
        .transpose()
    }

    async fn delete_user_first_clerk_sign_in(&self, user_id: &UserId) -> anyhow::Result<()> {
        self.db
            .execute("delete_user_first_clerk_sign_in", || {
                sqlx::query("DELETE FROM user_first_clerk_sign_in WHERE user_id = $1")
                    .bind(user_id.as_str())
            })
            .await?;
        Ok(())
    }
}

enum Claim {
    Created,
    /// `None` only if the refusing snapshot held no row, which `REPEATABLE
    /// READ` rules out: a later commit fails the insert with a conflict.
    Held(Option<String>),
}

async fn claim_within(
    conn: &mut PgConnection,
    email: &str,
    user_id: &str,
) -> Result<Claim, sqlx::Error> {
    let created = sqlx::query(
        "INSERT INTO user_email_index (email, user_id) VALUES ($1, $2)
         ON CONFLICT (email) DO NOTHING",
    )
    .bind(email)
    .bind(user_id)
    .execute(&mut *conn)
    .await?
    .rows_affected();
    if created == 1 {
        return Ok(Claim::Created);
    }
    let holder =
        sqlx::query_scalar::<_, String>("SELECT user_id FROM user_email_index WHERE email = $1")
            .bind(email)
            .fetch_optional(&mut *conn)
            .await?;
    Ok(Claim::Held(holder))
}

/// `None` when the conditional write changed no row.
async fn mutate_within(
    conn: &mut PgConnection,
    user_id: &str,
    f: &(dyn for<'s> Fn(&'s mut UserPermissionSet) -> bool + Send + Sync),
) -> Result<Option<UserPermissionMutation>, sqlx::Error> {
    let read = sqlx::query_as::<_, (i64, String)>(
        "SELECT revision, body FROM user_permissions WHERE user_id = $1",
    )
    .bind(user_id)
    .fetch_optional(&mut *conn)
    .await?;
    let mut set = match &read {
        Some((_, body)) => decode_permissions(body)
            .map_err(|e| unreadable(format!("permissions for {user_id}"), e))?,
        None => UserPermissionSet::default(),
    };
    if !f(&mut set) {
        // Nothing written, so an absent row stays absent.
        return Ok(Some(UserPermissionMutation {
            changed: false,
            resulting: set,
        }));
    }
    let body = encode_permissions(&set).map_err(|e| sqlx::Error::Encode(e.into()))?;
    let written = match read {
        Some((revision, _)) => {
            sqlx::query(
                "UPDATE user_permissions SET body = $2, revision = revision + 1
                 WHERE user_id = $1 AND revision = $3",
            )
            .bind(user_id)
            .bind(&body)
            .bind(revision)
            .execute(&mut *conn)
            .await?
        }
        None => {
            sqlx::query(
                "INSERT INTO user_permissions (user_id, revision, body) VALUES ($1, 1, $2)
                 ON CONFLICT (user_id) DO NOTHING",
            )
            .bind(user_id)
            .bind(&body)
            .execute(&mut *conn)
            .await?
        }
    }
    .rows_affected();
    Ok((written == 1).then_some(UserPermissionMutation {
        changed: true,
        resulting: set,
    }))
}

async fn consume_within(
    conn: &mut PgConnection,
    key_id: &str,
    secret: &UserPreauthSecret,
) -> Result<UserPreauthConsumeOutcome, sqlx::Error> {
    let body = sqlx::query_scalar::<_, String>("SELECT body FROM user_preauth WHERE key_id = $1")
        .bind(key_id)
        .fetch_optional(&mut *conn)
        .await?;
    let Some(body) = body else {
        return Ok(UserPreauthConsumeOutcome::Rejected(
            UserPreauthRejectionError::NotFound,
        ));
    };
    let key: UserPreauthKey = serde_json::from_str(&body)
        .map_err(|e| unreadable(format!("user pre-auth key {key_id}"), e))?;
    let grant = match pipette_core::user_preauth::validate(&key, secret, chrono::Utc::now()) {
        Ok(grant) => grant,
        Err(rejection) => return Ok(UserPreauthConsumeOutcome::Rejected(rejection)),
    };
    match key.usage {
        UserPreauthUsage::MultiUse => return Ok(UserPreauthConsumeOutcome::Granted(grant)),
        UserPreauthUsage::SingleUse => {}
    }
    let spent = sqlx::query(
        "INSERT INTO user_preauth_spent (key_id, spent_at) VALUES ($1, now())
         ON CONFLICT (key_id) DO NOTHING",
    )
    .bind(key_id)
    .execute(&mut *conn)
    .await?
    .rows_affected();
    if spent == 0 {
        // The key was spent earlier and its record survived. The answer is
        // unknown, as on the object stores.
        return Ok(UserPreauthConsumeOutcome::Rejected(
            UserPreauthRejectionError::NotFound,
        ));
    }
    sqlx::query("DELETE FROM user_preauth WHERE key_id = $1")
        .bind(key_id)
        .execute(&mut *conn)
        .await?;
    Ok(UserPreauthConsumeOutcome::Granted(grant))
}

fn parse_holder(email: &str, holder: String) -> anyhow::Result<UserId> {
    UserId::try_new(holder).with_context(|| format!("address claim for {email}"))
}

/// A stored body that does not parse. It has no source, so an error chain
/// shows the parse error once.
#[derive(Debug, thiserror::Error)]
#[error("{what}: {error}")]
struct UnreadableBodyError {
    what: String,
    error: serde_json::Error,
}

/// A `Decode` error. The retry passes this error through.
fn unreadable(what: String, error: serde_json::Error) -> sqlx::Error {
    sqlx::Error::Decode(Box::new(UnreadableBodyError { what, error }))
}

/// A `users` body: the account without its type and its campaign scope, which
/// are the `account_type` and `campaign_scope` columns, so a rolled-back binary
/// rewriting the body cannot drop them.
fn user_body(user: &User) -> anyhow::Result<String> {
    let mut body = serde_json::to_value(user)?;
    if let Some(record) = body.as_object_mut() {
        record.remove("account_type");
        record.remove("campaign_scope");
    }
    Ok(serde_json::to_string_pretty(&body)?)
}

fn read_user(body: &str, account_type: &str, campaign_scope: Option<&str>) -> anyhow::Result<User> {
    let mut record: Map<String, Value> = serde_json::from_str(body)?;
    record.insert("account_type".into(), account_type.into());
    match campaign_scope {
        Some(scope) => record.insert("campaign_scope".into(), serde_json::from_str(scope)?),
        None => record.remove("campaign_scope"),
    };
    Ok(serde_json::from_value(Value::Object(record))?)
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use anyhow::Context;
    use chrono::{DateTime, Utc};
    use sqlx::AssertSqlSafe;

    use pipette_core::types::Mint;
    use pipette_core::user::{User, UserKey};
    use pipette_core::user_permission::UserPermissionSet;
    use pipette_core::user_preauth::{
        UserMintedKey, UserPreauthConsumeOutcome, UserPreauthParams, UserPreauthRejectionError,
        UserPreauthSecret,
    };
    use pipette_plan_types::{
        CampaignId, ContactEmail, NonEmptyTrimmedString, UserAccountType, UserId, UserPermission,
        UserPreauthKeyId, UserPreauthUsage, UserStatus,
    };
    use pipette_store::{EmailClaim, UserStore};

    use super::SqlUserStore;
    use crate::scratch::TestDb;
    use crate::testing::{RACERS, race};

    fn users(test_db: &TestDb) -> SqlUserStore {
        SqlUserStore::new(test_db.db().clone())
    }

    fn mint_key(
        usage: UserPreauthUsage,
        expires_at: Option<DateTime<Utc>>,
    ) -> anyhow::Result<(UserMintedKey, UserPreauthSecret)> {
        let minted = pipette_core::user_preauth::mint(
            UserPreauthParams {
                user_id: UserId::mint(),
                usage,
                expires_at,
                note: None,
            },
            Utc::now(),
        )?;
        let (_, secret) = minted
            .token
            .parse_parts()
            .context("a minted token must parse")?;
        Ok((minted, secret))
    }

    fn user_key(byte: u8, details: &str) -> anyhow::Result<UserKey> {
        Ok(UserKey::new(
            crate::testing::public_key(byte)?,
            NonEmptyTrimmedString::try_new(details)?,
            Utc::now(),
        ))
    }

    /// For a row that no store method would write.
    async fn plant(test_db: &TestDb, sql: &'static str, args: &[&str]) -> anyhow::Result<()> {
        args.iter()
            .fold(sqlx::query(sql), |query, arg| query.bind(*arg))
            .execute(test_db.db().pool())
            .await?;
        Ok(())
    }

    async fn text(
        test_db: &TestDb,
        sql: &'static str,
        arg: &str,
    ) -> anyhow::Result<Option<String>> {
        Ok(sqlx::query_scalar::<_, String>(sql)
            .bind(arg)
            .fetch_optional(test_db.db().pool())
            .await?)
    }

    #[tokio::test]
    async fn sql_user_account_roundtrip() -> anyhow::Result<()> {
        let test_db = crate::testing::test_db_or_skip!();
        pipette_store_testkit::assert_user_account_store_roundtrip(&users(&test_db)).await
    }

    #[tokio::test]
    async fn sql_user_awaiting_sign_in() -> anyhow::Result<()> {
        let test_db = crate::testing::test_db_or_skip!();
        pipette_store_testkit::assert_user_awaiting_sign_in_roundtrip(&users(&test_db)).await
    }

    #[tokio::test]
    async fn sql_user_first_clerk_sign_in() -> anyhow::Result<()> {
        let test_db = crate::testing::test_db_or_skip!();
        pipette_store_testkit::assert_user_first_clerk_sign_in_roundtrip(&users(&test_db)).await
    }

    #[tokio::test]
    async fn sql_user_store_permissions() -> anyhow::Result<()> {
        let test_db = crate::testing::test_db_or_skip!();
        pipette_store_testkit::assert_user_permission_store_roundtrip(
            &users(&test_db),
            &UserId::mint(),
        )
        .await
    }

    #[tokio::test]
    async fn sql_user_keys_round_trip() -> anyhow::Result<()> {
        let test_db = crate::testing::test_db_or_skip!();
        pipette_store_testkit::assert_user_key_store_roundtrip(
            &users(&test_db),
            &UserId::mint(),
            &UserId::mint(),
        )
        .await
    }

    #[tokio::test]
    async fn sql_user_preauth_store() -> anyhow::Result<()> {
        let test_db = crate::testing::test_db_or_skip!();
        let clients = crate::clients::SqlClientStore::new(test_db.db().clone());
        pipette_store_testkit::assert_user_preauth_store_contract(&users(&test_db), &clients).await
    }

    async fn row_counts(test_db: &TestDb) -> anyhow::Result<BTreeMap<String, i64>> {
        let tables = sqlx::query_scalar::<_, String>(
            "SELECT table_name::text FROM information_schema.tables
             WHERE table_schema = current_schema() AND table_type = 'BASE TABLE'",
        )
        .fetch_all(test_db.db().pool())
        .await?;
        let mut counts = BTreeMap::new();
        for table in tables {
            let quoted = table.replace('"', "\"\"");
            let count = sqlx::query_scalar::<_, i64>(AssertSqlSafe(format!(
                "SELECT count(*) FROM \"{quoted}\""
            )))
            .fetch_one(test_db.db().pool())
            .await?;
            counts.insert(table, count);
        }
        Ok(counts)
    }

    /// Nonzero row-count deltas since `before`.
    async fn moved(
        test_db: &TestDb,
        before: &BTreeMap<String, i64>,
    ) -> anyhow::Result<BTreeMap<String, i64>> {
        Ok(row_counts(test_db)
            .await?
            .into_iter()
            .filter_map(|(table, count)| {
                let delta = count - before.get(&table).copied().unwrap_or(0);
                (delta != 0).then_some((table, delta))
            })
            .collect())
    }

    #[tokio::test]
    async fn sql_user_preauth_writes_stay_in_user_tables() -> anyhow::Result<()> {
        let test_db = crate::testing::test_db_or_skip!();
        let store = users(&test_db);
        let before = row_counts(&test_db).await?;
        let invite = |delta: i64| BTreeMap::from([("user_preauth".to_owned(), delta)]);
        let spent = BTreeMap::from([("user_preauth_spent".to_owned(), 1)]);

        let (single, single_secret) = mint_key(UserPreauthUsage::SingleUse, None)?;
        store.put_user_preauth_key(&single.key).await?;
        assert_eq!(
            moved(&test_db, &before).await?,
            invite(1),
            "a put writes one invite"
        );
        assert!(matches!(
            store
                .consume_user_preauth_key(&single.key.key_id, &single_secret)
                .await?,
            UserPreauthConsumeOutcome::Granted(_)
        ));
        assert_eq!(
            moved(&test_db, &before).await?,
            spent,
            "a single-use spend swaps the invite for one marker"
        );

        let (multi, multi_secret) = mint_key(UserPreauthUsage::MultiUse, None)?;
        store.put_user_preauth_key(&multi.key).await?;
        for _ in 0..2 {
            assert!(matches!(
                store
                    .consume_user_preauth_key(&multi.key.key_id, &multi_secret)
                    .await?,
                UserPreauthConsumeOutcome::Granted(_)
            ));
        }
        let mut both = invite(1);
        both.extend(spent);
        assert_eq!(
            moved(&test_db, &before).await?,
            both,
            "a multi-use consume writes nothing"
        );

        store
            .delete_spent_user_preauth_marker(&single.key.key_id)
            .await?;
        store.delete_user_preauth_key(&multi.key.key_id).await?;
        assert!(
            moved(&test_db, &before).await?.is_empty(),
            "the deletes remove only what the user surface wrote"
        );
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn sql_concurrent_email_claims_have_one_winner() -> anyhow::Result<()> {
        let test_db = crate::testing::test_db_or_skip!();
        let store = users(&test_db);
        let email = "race@example.test";

        let claimants: Vec<UserId> = std::iter::repeat_with(UserId::mint).take(RACERS).collect();
        let outcomes = race(|n| {
            let (store, user_id) = (store.clone(), claimants[n].clone());
            async move { store.claim_user_email(email, &user_id).await }
        })
        .await?;
        let mut winners = Vec::new();
        let mut holders = Vec::new();
        for (user_id, outcome) in claimants.into_iter().zip(outcomes) {
            match outcome {
                EmailClaim::Claimed => winners.push(user_id),
                EmailClaim::Taken(holder) => holders.push(holder),
            }
        }
        assert_eq!(winners.len(), 1, "exactly one claim wins: {winners:?}");
        let winner = winners.first().context("expected a winner")?;
        assert!(
            holders.iter().all(|holder| holder == winner),
            "every loser is told who won: {holders:?}"
        );
        assert_eq!(
            store.lookup_user_by_email(email).await?,
            Some(winner.clone())
        );
        // No special case for the holder, as on the object stores.
        assert_eq!(
            store.claim_user_email(email, winner).await?,
            EmailClaim::Taken(winner.clone())
        );
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn sql_concurrent_single_use_consumes_grant_once() -> anyhow::Result<()> {
        let test_db = crate::testing::test_db_or_skip!();
        let store = users(&test_db);
        let (minted, secret) = mint_key(UserPreauthUsage::SingleUse, None)?;
        let key_id = minted.key.key_id.clone();
        store.put_user_preauth_key(&minted.key).await?;

        let outcomes = race(|_| {
            let (store, key_id, secret) = (store.clone(), key_id.clone(), secret.clone());
            async move { store.consume_user_preauth_key(&key_id, &secret).await }
        })
        .await?;
        let mut granted = 0;
        for outcome in outcomes {
            match outcome {
                UserPreauthConsumeOutcome::Granted(_) => granted += 1,
                UserPreauthConsumeOutcome::Rejected(rejection) => assert_eq!(
                    rejection,
                    UserPreauthRejectionError::NotFound,
                    "a loser reads the key as spent"
                ),
            }
        }
        assert_eq!(granted, 1, "a single-use key grants exactly once");
        assert_eq!(store.list_spent_user_preauth_markers().await?, vec![key_id]);
        assert!(
            store.list_user_preauth_keys().await?.is_empty(),
            "the winner deleted the record"
        );
        Ok(())
    }

    /// Starts from an absent set, so every grant races the row's insert.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn sql_concurrent_permission_grants_all_land() -> anyhow::Result<()> {
        let test_db = crate::testing::test_db_or_skip!();
        let store = users(&test_db);
        let user_id = UserId::mint();

        let grants: [UserPermission; RACERS] = [
            UserPermission::SubmitPlans,
            UserPermission::SubmitAnyPlans,
            UserPermission::ReadPlans,
            UserPermission::ReadAnyPlans,
            UserPermission::CancelPlans,
            UserPermission::CancelAnyPlans,
            UserPermission::ReadClients,
            UserPermission::ReadAnyClients,
        ];
        let outcomes = race(|n| {
            let (store, user_id, permission) = (store.clone(), user_id.clone(), grants[n]);
            async move {
                store
                    .mutate_user_permissions(&user_id, &move |set| set.insert_all([permission]))
                    .await
            }
        })
        .await?;
        for (permission, mutation) in grants.iter().zip(outcomes) {
            assert!(mutation.changed, "{permission} was not held before");
            assert!(
                mutation.resulting.names().contains(&permission.as_ref()),
                "the result a grant reports includes it: {permission}"
            );
        }
        assert_eq!(
            store.get_user_permissions(&user_id).await?,
            Some(UserPermissionSet::from_iter(grants))
        );
        Ok(())
    }

    /// Another writer lands between every read and its write-back, so the
    /// mutation runs out of attempts.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn sql_permission_contention_writes_nothing() -> anyhow::Result<()> {
        let test_db = crate::testing::test_db_or_skip!();
        let store = users(&test_db);
        let user_id = UserId::mint();
        let seeded = UserPermissionSet::from_iter([UserPermission::SubmitPlans]);
        store.put_user_permissions(&user_id, &seeded).await?;

        let interfered = Arc::new(AtomicUsize::new(0));
        let f = {
            let (pool, runtime) = (
                test_db.db().pool().clone(),
                tokio::runtime::Handle::current(),
            );
            let (id, interfered) = (user_id.as_str().to_owned(), interfered.clone());
            move |set: &mut UserPermissionSet| {
                let bumped = tokio::task::block_in_place(|| {
                    runtime.block_on(
                        sqlx::query(
                            "UPDATE user_permissions SET revision = revision + 1 WHERE user_id = $1",
                        )
                        .bind(id.as_str())
                        .execute(&pool),
                    )
                });
                if bumped.is_ok() {
                    interfered.fetch_add(1, Ordering::SeqCst);
                }
                set.insert_all([UserPermission::CancelPlans])
            }
        };
        let outcome = {
            let (store, user_id) = (store.clone(), user_id.clone());
            tokio::spawn(async move { store.mutate_user_permissions(&user_id, &f).await }).await?
        };

        let error = outcome
            .err()
            .context("a mutation that never wins must fail")?;
        assert!(
            format!("{error:#}").contains("nothing was written"),
            "{error:#}"
        );
        assert!(
            interfered.load(Ordering::SeqCst) > 1,
            "each attempt re-read and replayed the closure"
        );
        assert_eq!(
            store.get_user_permissions(&user_id).await?,
            Some(seeded),
            "the grant never landed"
        );
        Ok(())
    }

    /// A closure that refuses when it replays on another writer's set writes
    /// nothing and reports that set, so an account rank check keeps the grant.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn sql_a_refusal_on_replay_keeps_the_winners_set() -> anyhow::Result<()> {
        let test_db = crate::testing::test_db_or_skip!();
        let store = users(&test_db);
        let user_id = UserId::mint();
        let seeded = UserPermissionSet::from_iter([UserPermission::SubmitPlans]);
        store.put_user_permissions(&user_id, &seeded).await?;
        let promoted = UserPermissionSet::from_iter([UserPermission::ManageAnyOrgs]);

        let calls = Arc::new(AtomicUsize::new(0));
        let f = {
            let (writer, runtime) = (store.clone(), tokio::runtime::Handle::current());
            let (id, promoted, calls) = (user_id.clone(), promoted.clone(), calls.clone());
            move |set: &mut UserPermissionSet| {
                // The grant lands after the first read, so its write-back conflicts.
                if calls.fetch_add(1, Ordering::SeqCst) == 0 {
                    let _ = tokio::task::block_in_place(|| {
                        runtime.block_on(writer.put_user_permissions(&id, &promoted))
                    });
                }
                // Without the refusal, the replay would strip the grant.
                !set.holds(UserPermission::ManageAnyOrgs)
                    && set.remove_all(["submit_plans", "manage_any_orgs"])
            }
        };
        let mutation = {
            let (store, user_id) = (store.clone(), user_id.clone());
            tokio::spawn(async move { store.mutate_user_permissions(&user_id, &f).await }).await??
        };

        assert!(!mutation.changed);
        assert_eq!(mutation.resulting, promoted);
        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "the closure ran again on the winner's set"
        );
        assert_eq!(store.get_user_permissions(&user_id).await?, Some(promoted));
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn sql_concurrent_marks_and_clears_are_idempotent() -> anyhow::Result<()> {
        let test_db = crate::testing::test_db_or_skip!();
        let store = users(&test_db);
        let user_id = UserId::mint();

        race(|_| {
            let (store, user_id) = (store.clone(), user_id.clone());
            async move { store.mark_user_awaiting_sign_in(&user_id).await }
        })
        .await?;
        assert!(store.user_awaits_sign_in(&user_id).await?);

        race(|_| {
            let (store, user_id) = (store.clone(), user_id.clone());
            async move { store.clear_user_awaiting_sign_in(&user_id).await }
        })
        .await?;
        assert!(!store.user_awaits_sign_in(&user_id).await?);
        Ok(())
    }

    /// The secret is checked before the expiry, and a rejection writes nothing.
    #[tokio::test]
    async fn sql_user_preauth_rejections() -> anyhow::Result<()> {
        let test_db = crate::testing::test_db_or_skip!();
        let store = users(&test_db);
        let expired = Some(Utc::now() - chrono::Duration::seconds(1));
        let (minted, secret) = mint_key(UserPreauthUsage::SingleUse, expired)?;
        let key_id = minted.key.key_id.clone();
        store.put_user_preauth_key(&minted.key).await?;

        assert_eq!(
            store
                .consume_user_preauth_key(&key_id, &UserPreauthSecret::new("wrong-secret"))
                .await?,
            UserPreauthConsumeOutcome::Rejected(UserPreauthRejectionError::BadSecret),
        );
        assert_eq!(
            store.consume_user_preauth_key(&key_id, &secret).await?,
            UserPreauthConsumeOutcome::Rejected(UserPreauthRejectionError::Expired),
        );
        assert!(store.list_spent_user_preauth_markers().await?.is_empty());
        assert_eq!(store.list_user_preauth_keys().await?.len(), 1);

        store.delete_user_preauth_key(&key_id).await?;
        assert_eq!(
            store.consume_user_preauth_key(&key_id, &secret).await?,
            UserPreauthConsumeOutcome::Rejected(UserPreauthRejectionError::NotFound),
        );
        Ok(())
    }

    /// A record written back under a spent id reads as unknown and is left in
    /// place.
    #[tokio::test]
    async fn sql_spent_marker_overrules_a_surviving_record() -> anyhow::Result<()> {
        let test_db = crate::testing::test_db_or_skip!();
        let store = users(&test_db);
        let (minted, secret) = mint_key(UserPreauthUsage::SingleUse, None)?;
        let key_id = minted.key.key_id.clone();
        store.put_user_preauth_key(&minted.key).await?;

        assert!(matches!(
            store.consume_user_preauth_key(&key_id, &secret).await?,
            UserPreauthConsumeOutcome::Granted(_)
        ));
        let spent = "SELECT key_id FROM user_preauth_spent WHERE key_id = $1";
        assert!(text(&test_db, spent, key_id.as_str()).await?.is_some());

        store.put_user_preauth_key(&minted.key).await?;
        assert_eq!(
            store.consume_user_preauth_key(&key_id, &secret).await?,
            UserPreauthConsumeOutcome::Rejected(UserPreauthRejectionError::NotFound),
        );
        assert_eq!(store.list_user_preauth_keys().await?.len(), 1);
        Ok(())
    }

    /// The type is a column, so a rolled-back binary rewriting the body keeps
    /// it, and one inserting an account makes a person's.
    #[tokio::test]
    async fn sql_an_account_type_outlives_a_body_rewrite() -> anyhow::Result<()> {
        let test_db = crate::testing::test_db_or_skip!();
        let store = users(&test_db);
        let robot = User {
            account_type: UserAccountType::Service,
            ..User::try_new(ContactEmail::try_new("ops+ci@example.test")?, Utc::now())?
        };
        store.put_user(&robot).await?;
        let robot_id = robot.user_id.as_str();
        assert_eq!(
            text(
                &test_db,
                "SELECT account_type FROM users WHERE user_id = $1",
                robot_id
            )
            .await?,
            Some("service".into())
        );
        let body = text(
            &test_db,
            "SELECT body FROM users WHERE user_id = $1",
            robot_id,
        )
        .await?
        .context("the row was written")?;
        assert!(!body.contains("account_type"), "{body}");

        let legacy = |user: &User| -> anyhow::Result<String> {
            let mut body = serde_json::to_value(user)?;
            body.as_object_mut()
                .and_then(|record| record.remove("account_type"));
            Ok(serde_json::to_string_pretty(&body)?)
        };
        let disabled = User {
            status: UserStatus::Disabled,
            ..robot.clone()
        };
        plant(
            &test_db,
            "INSERT INTO users (user_id, body) VALUES ($1, $2)
             ON CONFLICT (user_id) DO UPDATE SET body = EXCLUDED.body",
            &[robot_id, &legacy(&disabled)?],
        )
        .await?;
        assert_eq!(store.get_user(&robot.user_id).await?, Some(disabled));

        let person = User::try_new(ContactEmail::try_new("ops@example.test")?, Utc::now())?;
        plant(
            &test_db,
            "INSERT INTO users (user_id, body) VALUES ($1, $2)",
            &[person.user_id.as_str(), &legacy(&person)?],
        )
        .await?;
        assert_eq!(store.get_user(&person.user_id).await?, Some(person));
        Ok(())
    }

    /// A campaign scope is a column as the type is, so a rolled-back binary
    /// rewriting the body cannot widen a campaign reader into a member.
    #[tokio::test]
    async fn sql_a_campaign_scope_outlives_a_body_rewrite() -> anyhow::Result<()> {
        let test_db = crate::testing::test_db_or_skip!();
        let store = users(&test_db);
        let reader = User {
            account_type: UserAccountType::Service,
            campaign_scope: Some(BTreeSet::from([CampaignId::mint()])),
            ..User::try_new(ContactEmail::try_new("ops+site@example.test")?, Utc::now())?
        };
        store.put_user(&reader).await?;
        let reader_id = reader.user_id.as_str();
        let body = text(
            &test_db,
            "SELECT body FROM users WHERE user_id = $1",
            reader_id,
        )
        .await?
        .context("the row was written")?;
        assert!(!body.contains("campaign_scope"), "{body}");

        // An older binary writes the body and the type, and no scope.
        let disabled = User {
            status: UserStatus::Disabled,
            ..reader.clone()
        };
        let mut legacy = serde_json::to_value(User {
            campaign_scope: None,
            ..disabled.clone()
        })?;
        legacy
            .as_object_mut()
            .and_then(|record| record.remove("account_type"));
        plant(
            &test_db,
            "UPDATE users SET body = $2 WHERE user_id = $1",
            &[reader_id, &serde_json::to_string_pretty(&legacy)?],
        )
        .await?;
        assert_eq!(
            store.get_user(&reader.user_id).await?,
            Some(disabled.clone())
        );
        let listed = store.list_users().await?;
        assert_eq!(
            listed.iter().find(|user| user.user_id == reader.user_id),
            Some(&disabled)
        );
        Ok(())
    }

    /// The claim row holds the bare id under the folded address.
    #[tokio::test]
    async fn sql_records_keep_the_object_store_bodies() -> anyhow::Result<()> {
        let test_db = crate::testing::test_db_or_skip!();
        let store = users(&test_db);
        let user = User::try_new(ContactEmail::try_new("Operator@Example.test")?, Utc::now())?;
        let user_id = user.user_id.as_str();
        store
            .claim_user_email(user.contact_email.as_str(), &user.user_id)
            .await?;
        store.put_user(&user).await?;

        assert_eq!(
            text(
                &test_db,
                "SELECT body FROM users WHERE user_id = $1",
                user_id
            )
            .await?,
            Some(super::user_body(&user)?)
        );
        assert_eq!(
            text(
                &test_db,
                "SELECT user_id FROM user_email_index WHERE email = $1",
                "operator@example.test"
            )
            .await?
            .as_deref(),
            Some(user_id)
        );

        let set = UserPermissionSet::from_iter([UserPermission::SubmitPlans]);
        store.put_user_permissions(&user.user_id, &set).await?;
        assert_eq!(
            text(
                &test_db,
                "SELECT body FROM user_permissions WHERE user_id = $1",
                user_id
            )
            .await?
            .as_deref(),
            Some("{\n  \"permissions\": [\n    \"submit_plans\"\n  ]\n}")
        );

        let key = user_key(0x11, "laptop")?;
        store.put_user_key(&user.user_id, &key).await?;
        let body = sqlx::query_scalar::<_, String>(
            "SELECT body FROM user_keys WHERE user_id = $1 AND key_id = $2",
        )
        .bind(user_id)
        .bind(key.key_id.as_str())
        .fetch_optional(test_db.db().pool())
        .await?;
        assert_eq!(body, Some(serde_json::to_string_pretty(&key)?));
        Ok(())
    }

    /// Listings that skip a bad body on the object stores skip it here; every
    /// other read fails without writing.
    #[tokio::test]
    async fn sql_unreadable_rows() -> anyhow::Result<()> {
        let test_db = crate::testing::test_db_or_skip!();
        let store = users(&test_db);

        let good = User::try_new(ContactEmail::try_new("good@example.test")?, Utc::now())?;
        store.put_user(&good).await?;
        let broken = UserId::mint();
        plant(
            &test_db,
            "INSERT INTO users (user_id, body) VALUES ($1, $2)",
            &[broken.as_str(), "{not json"],
        )
        .await?;
        assert_eq!(store.list_users().await?, vec![good.clone()]);
        assert!(store.get_user(&broken).await.is_err());

        let key = user_key(0x11, "laptop")?;
        store.put_user_key(&good.user_id, &key).await?;
        let broken_key = user_key(0x22, "desktop")?.key_id;
        plant(
            &test_db,
            "INSERT INTO user_keys (user_id, key_id, body) VALUES ($1, $2, $3)",
            &[good.user_id.as_str(), broken_key.as_str(), "[]"],
        )
        .await?;
        assert_eq!(store.list_user_keys(&good.user_id).await?, vec![key]);
        assert!(
            store
                .get_user_key(&good.user_id, &broken_key)
                .await
                .is_err()
        );

        let bad_set = "{\"permissions\": [\"Not A Name\"]}";
        plant(
            &test_db,
            "INSERT INTO user_permissions (user_id, revision, body) VALUES ($1, 1, $2)",
            &[good.user_id.as_str(), bad_set],
        )
        .await?;
        assert!(store.get_user_permissions(&good.user_id).await.is_err());
        assert!(
            store
                .mutate_user_permissions(&good.user_id, &|set| {
                    set.insert_all([UserPermission::CancelPlans])
                })
                .await
                .is_err()
        );
        assert_eq!(
            text(
                &test_db,
                "SELECT body FROM user_permissions WHERE user_id = $1",
                good.user_id.as_str()
            )
            .await?
            .as_deref(),
            Some(bad_set),
            "a mutation over an unreadable set writes nothing"
        );

        plant(
            &test_db,
            "INSERT INTO user_email_index (email, user_id) VALUES ($1, $2)",
            &["odd@example.test", "not-a-user-id"],
        )
        .await?;
        assert!(
            store
                .lookup_user_by_email("odd@example.test")
                .await
                .is_err()
        );
        assert!(
            store
                .claim_user_email("odd@example.test", &UserId::mint())
                .await
                .is_err()
        );

        let (minted, secret) = mint_key(UserPreauthUsage::SingleUse, None)?;
        store.put_user_preauth_key(&minted.key).await?;
        let broken_preauth = UserPreauthKeyId::try_new("00ff00ff00ff00ff")?;
        plant(
            &test_db,
            "INSERT INTO user_preauth (key_id, body) VALUES ($1, $2)",
            &[broken_preauth.as_str(), "{}"],
        )
        .await?;
        assert!(store.list_user_preauth_keys().await.is_err());
        assert!(
            store
                .consume_user_preauth_key(&broken_preauth, &secret)
                .await
                .is_err()
        );
        assert!(store.list_spent_user_preauth_markers().await?.is_empty());

        // A marker whose name is no key id is skipped, not fatal.
        plant(
            &test_db,
            "INSERT INTO user_preauth_spent (key_id, spent_at) VALUES ($1, now())",
            &["not/a/key/id"],
        )
        .await?;
        plant(
            &test_db,
            "INSERT INTO user_preauth_spent (key_id, spent_at) VALUES ($1, now())",
            &[minted.key.key_id.as_str()],
        )
        .await?;
        assert_eq!(
            store.list_spent_user_preauth_markers().await?,
            vec![minted.key.key_id.clone()]
        );
        Ok(())
    }
}
