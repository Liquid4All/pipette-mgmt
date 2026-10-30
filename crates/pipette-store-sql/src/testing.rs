//! Helpers for the tests that run on a [`TestDb`](crate::scratch::TestDb).

use std::fmt::Debug;
use std::future::Future;
use std::sync::Arc;

use anyhow::Context;
use chrono::Utc;
use futures::future::Either;
use sqlx::postgres::PgConnectOptions;
use sqlx::{AssertSqlSafe, Connection, PgConnection};
use tokio::sync::Barrier;

use pipette_core::client::{Client, ClientStatus};
use pipette_plan_types::{ClientId, ContactEmail, JobId, NonEmptyTrimmedString, PublicKeyHex};

use crate::db::SqlDatabase;
use crate::scratch::{POOL_SIZE, run_on_drop, scratch_name};

/// Put the `submission_outcomes` row of `job_id`.
pub(crate) async fn put_outcome(
    db: &SqlDatabase,
    job_id: &str,
    outcome: &str,
    run_flags: Option<&[&str]>,
) -> anyhow::Result<()> {
    sqlx::query("INSERT INTO submission_outcomes (job_id, outcome, run_flags) VALUES ($1, $2, $3)")
        .bind(job_id)
        .bind(outcome)
        .bind(run_flags)
        .execute(db.pool())
        .await?;
    Ok(())
}

/// Twice the pool size of a [`TestDb`](crate::scratch::TestDb). Some tasks wait
/// for a connection. The other tasks conflict in the database.
pub(crate) const RACERS: usize = 2 * POOL_SIZE as usize;

/// An empty database on the test server, dropped with this value.
pub(crate) struct Scratch {
    admin: PgConnectOptions,
    name: String,
}

impl Scratch {
    /// Create a database with a new name on `admin`. `options` follow the name
    /// in the `CREATE DATABASE` statement.
    pub(crate) async fn create(admin: &PgConnectOptions, options: &str) -> sqlx::Result<Self> {
        let name = scratch_name();
        let mut conn = PgConnection::connect_with(admin).await?;
        sqlx::query(AssertSqlSafe(format!("CREATE DATABASE {name} {options}")))
            .execute(&mut conn)
            .await?;
        let scratch = Self {
            admin: admin.clone(),
            name,
        };
        conn.close().await?;
        Ok(scratch)
    }

    pub(crate) fn connect(&self, max_connections: u32) -> SqlDatabase {
        SqlDatabase::from_options(self.admin.clone().database(&self.name), max_connections)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let name = &self.name;
        run_on_drop(
            self.admin.clone(),
            vec![format!("DROP DATABASE IF EXISTS {name} WITH (FORCE)")],
        );
    }
}

/// Create a [`TestDb`](crate::scratch::TestDb). If no server exists, return
/// `Ok(())` from the test: `let test_db = test_db_or_skip!();`.
macro_rules! test_db_or_skip {
    () => {{
        let Some(test_db) = $crate::scratch::TestDb::new().await? else {
            return Ok(());
        };
        test_db
    }};
}
pub(crate) use test_db_or_skip;

/// Run `racer(n)` for each `n` below [`RACERS`] on its own task, released
/// together by a barrier. The outcomes come in the order of `n`.
pub(crate) async fn race<T, F, Fut>(racer: F) -> anyhow::Result<Vec<T>>
where
    T: Send + 'static,
    F: Fn(usize) -> Fut,
    Fut: Future<Output = anyhow::Result<T>> + Send + 'static,
{
    let start = Arc::new(Barrier::new(RACERS));
    let tasks = (0..RACERS).map(|n| {
        let (start, attempt) = (Arc::clone(&start), racer(n));
        tokio::spawn(async move {
            start.wait().await;
            attempt.await
        })
    });
    futures::future::try_join_all(tasks)
        .await?
        .into_iter()
        .collect()
}

/// Race a write of each version `version(n)` of one record; `get` must then
/// read one of the versions.
pub(crate) async fn assert_racing_writes_are_last_writer_wins<R, Put, PutFut, GetFut>(
    version: impl Fn(usize) -> anyhow::Result<R> + Send,
    put: Put,
    get: impl FnOnce() -> GetFut + Send,
) -> anyhow::Result<()>
where
    R: Clone + PartialEq + Debug + Send + Sync,
    Put: Fn(R) -> PutFut + Send + Sync,
    PutFut: Future<Output = anyhow::Result<()>> + Send + 'static,
    GetFut: Future<Output = anyhow::Result<Option<R>>> + Send,
{
    let versions = (0..RACERS)
        .map(version)
        .collect::<anyhow::Result<Vec<_>>>()?;
    race(|n| put(versions[n].clone())).await?;
    let read = get().await?.context("the record was written")?;
    assert!(versions.contains(&read), "{read:?}");
    Ok(())
}

/// Race one more write of `record` against deletes from the other racers.
/// Each must succeed, and `get` must then read absent or `record`.
pub(crate) async fn assert_racing_deletes_are_idempotent<
    R,
    Put,
    PutFut,
    Delete,
    DeleteFut,
    GetFut,
>(
    record: R,
    put: Put,
    delete: Delete,
    get: impl FnOnce() -> GetFut + Send,
) -> anyhow::Result<()>
where
    R: Clone + PartialEq + Debug + Send + Sync,
    Put: Fn(R) -> PutFut + Send + Sync,
    PutFut: Future<Output = anyhow::Result<()>> + Send + 'static,
    Delete: Fn(R) -> DeleteFut + Send + Sync,
    DeleteFut: Future<Output = anyhow::Result<()>> + Send + 'static,
    GetFut: Future<Output = anyhow::Result<Option<R>>> + Send,
{
    put(record.clone()).await?;
    race(|n| {
        if n == 0 {
            Either::Left(put(record.clone()))
        } else {
            Either::Right(delete(record.clone()))
        }
    })
    .await?;
    let left = get().await?;
    assert!(left.as_ref().is_none_or(|left| *left == record), "{left:?}");
    Ok(())
}

/// The `outcome` in the `submission_outcomes` row of `job_id`, if a row exists.
pub(crate) async fn stored_outcome(
    db: &SqlDatabase,
    job_id: &JobId,
) -> anyhow::Result<Option<String>> {
    Ok(
        sqlx::query_scalar("SELECT outcome FROM submission_outcomes WHERE job_id = $1")
            .bind(job_id.as_str())
            .fetch_optional(db.pool())
            .await?,
    )
}

/// A key whose bytes are all `byte`.
pub(crate) fn public_key(byte: u8) -> anyhow::Result<PublicKeyHex> {
    Ok(PublicKeyHex::try_new(
        format!("{byte:02x}").repeat(PublicKeyHex::BYTE_LEN),
    )?)
}

/// An approved client of the organization `acme`.
pub(crate) fn a_client(client_id: ClientId, public_key: PublicKeyHex) -> anyhow::Result<Client> {
    Ok(Client {
        client_id,
        public_key,
        organization: NonEmptyTrimmedString::try_new("acme")?,
        client_details: NonEmptyTrimmedString::try_new("details")?,
        contact_email: ContactEmail::try_new("a@b.com")?,
        status: ClientStatus::Approved,
        registered_at: Utc::now(),
        device_profile: Default::default(),
        capabilities: Default::default(),
    })
}

#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn the_schema_applies_and_is_current() -> anyhow::Result<()> {
        let test_db = crate::testing::test_db_or_skip!();
        crate::migrate::check(test_db.db()).await?;
        assert!(
            crate::migrate::migrate(test_db.db(), None, false)
                .await?
                .applied
                .is_empty()
        );
        Ok(())
    }

    /// A move down undoes the newer migrations, newest first, and a migrate
    /// puts them back. A migration with no down script stops the move before
    /// it undoes anything.
    #[tokio::test]
    async fn a_move_down_undoes_what_a_migrate_applies() -> anyhow::Result<()> {
        let test_db = crate::testing::test_db_or_skip!();
        let db = test_db.db();
        let exists = |sql: &'static str| async move {
            sqlx::query_scalar::<_, bool>(sql)
                .fetch_one(db.pool())
                .await
        };
        let has_type = || {
            exists(
                "SELECT EXISTS (SELECT 1 FROM information_schema.columns
                 WHERE table_schema = current_schema() AND table_name = 'users' AND column_name = 'account_type')",
            )
        };
        let has_outcomes = || {
            exists(
                "SELECT EXISTS (SELECT 1 FROM information_schema.tables
                 WHERE table_schema = current_schema() AND table_name = 'submission_outcomes')",
            )
        };

        let has_model_params = || {
            exists(
                "SELECT EXISTS (SELECT 1 FROM information_schema.tables
                 WHERE table_schema = current_schema() AND table_name = 'model_params')",
            )
        };

        let has_labels = || {
            exists(
                "SELECT EXISTS (SELECT 1 FROM information_schema.columns
                 WHERE table_schema = current_schema() AND table_name = 'campaigns' AND column_name = 'labels')",
            )
        };

        let has_job_cells = || {
            exists(
                "SELECT EXISTS (SELECT 1 FROM information_schema.tables
                 WHERE table_schema = current_schema() AND table_name = 'job_cells')",
            )
        };
        let has_run_flags = || {
            exists(
                "SELECT EXISTS (SELECT 1 FROM information_schema.columns
                 WHERE table_schema = current_schema() AND table_name = 'submission_outcomes' AND column_name = 'run_flags')",
            )
        };

        let has_note = || {
            exists(
                "SELECT count(*) = 2 FROM information_schema.columns
                 WHERE table_schema = current_schema() AND table_name = 'plans' AND column_name IN ('note', 'change')",
            )
        };

        let has_audit = || {
            exists(
                "SELECT EXISTS (SELECT 1 FROM information_schema.tables
                 WHERE table_schema = current_schema() AND table_name = 'audit_events')",
            )
        };

        let has_cell_scope = || {
            exists(
                "SELECT EXISTS (SELECT 1 FROM information_schema.columns
                 WHERE table_schema = current_schema() AND table_name = 'job_cells' AND column_name = 'scope')",
            )
        };

        let has_campaign_scope = || {
            exists(
                "SELECT EXISTS (SELECT 1 FROM information_schema.columns
                 WHERE table_schema = current_schema() AND table_name = 'users' AND column_name = 'campaign_scope')",
            )
        };

        let has_campaign_state = || {
            exists(
                "SELECT EXISTS (SELECT 1 FROM information_schema.columns
                 WHERE table_schema = current_schema() AND table_name = 'campaigns' AND column_name = 'state')",
            )
        };

        let has_descriptor_catalog = || {
            exists(
                "SELECT EXISTS (SELECT 1 FROM information_schema.tables WHERE table_schema = current_schema() AND table_name = 'descriptor_entries')",
            )
        };

        let has_catalog_import_runs = || {
            exists(
                "SELECT EXISTS (SELECT 1 FROM information_schema.tables WHERE table_schema = current_schema() AND table_name = 'catalog_import_runs')",
            )
        };

        let has_catalog_scope = || {
            exists(
                "SELECT count(*) = 2 FROM information_schema.columns
                 WHERE table_schema = current_schema() AND table_name IN ('descriptor_entries', 'descriptor_members') AND column_name = 'scope'",
            )
        };

        let has_model_members = || {
            exists(
                "SELECT count(*) = 4 FROM information_schema.columns
                 WHERE table_schema = current_schema() AND (table_name, column_name) IN (
                     ('descriptor_entries', 'descriptor_kind'),
                     ('descriptor_members', 'descriptor_type'),
                     ('descriptor_members', 'format'),
                     ('descriptor_members', 'quantization'))",
            )
        };

        let has_model_alias = || {
            exists(
                "SELECT EXISTS (SELECT 1 FROM information_schema.columns
                 WHERE table_schema = current_schema() AND table_name = 'descriptor_members' AND column_name = 'alias_digest')",
            )
        };

        let has_entries_hidden = || {
            exists(
                "SELECT EXISTS (SELECT 1 FROM information_schema.columns
                 WHERE table_schema = current_schema() AND table_name = 'catalog_import_runs' AND column_name = 'entries_hidden')",
            )
        };

        let has_first_clerk_sign_in = || {
            exists(
                "SELECT EXISTS (SELECT 1 FROM information_schema.tables WHERE table_schema = current_schema() AND table_name = 'user_first_clerk_sign_in')",
            )
        };

        let refused = crate::migrate::migrate(db, Some(1), false).await;
        let err = refused.err().map(|e| e.to_string()).unwrap_or_default();
        assert!(err.contains("migration 2 has no down script"), "{err}");
        assert!(has_first_clerk_sign_in().await?);
        assert!(has_type().await?);
        assert!(has_outcomes().await?);
        assert!(has_labels().await?);
        assert!(has_job_cells().await?);
        assert!(has_note().await?);
        assert!(has_audit().await?);
        assert!(has_cell_scope().await?);
        assert!(has_campaign_scope().await?);
        assert!(has_campaign_state().await?);
        assert!(has_descriptor_catalog().await?);
        assert!(has_catalog_import_runs().await?);
        assert!(has_catalog_scope().await?);
        assert!(has_model_members().await?);
        assert!(has_model_alias().await?);
        assert!(has_entries_hidden().await?);

        let moved = crate::migrate::migrate(db, Some(18), false).await?;
        assert_eq!((moved.to, moved.reverted), (Some(18), vec![19]));
        assert!(!has_entries_hidden().await?);
        assert!(has_catalog_import_runs().await?);
        assert!(has_model_alias().await?);

        let moved = crate::migrate::migrate(db, Some(16), false).await?;
        assert_eq!((moved.to, moved.reverted), (Some(16), vec![18, 17]));
        assert!(!has_model_members().await?);
        assert!(!has_model_alias().await?);
        assert!(has_catalog_scope().await?);

        let moved = crate::migrate::migrate(db, Some(15), false).await?;
        assert_eq!((moved.to, moved.reverted), (Some(15), vec![16]));
        assert!(!has_catalog_scope().await?);

        let moved = crate::migrate::migrate(db, Some(14), false).await?;
        assert_eq!((moved.to, moved.reverted), (Some(14), vec![15]));
        assert!(!has_catalog_import_runs().await?);

        let moved = crate::migrate::migrate(db, Some(13), false).await?;
        assert_eq!((moved.to, moved.reverted), (Some(13), vec![14]));
        assert!(!has_first_clerk_sign_in().await?);
        assert!(has_descriptor_catalog().await?);

        let moved = crate::migrate::migrate(db, Some(12), false).await?;
        assert_eq!((moved.to, moved.reverted), (Some(12), vec![13]));
        assert!(!has_descriptor_catalog().await?);

        let moved = crate::migrate::migrate(db, Some(11), false).await?;
        assert_eq!((moved.to, moved.reverted), (Some(11), vec![12]));
        assert!(!has_campaign_state().await?);
        assert!(has_campaign_scope().await?);

        // A campaign reader holds the revert back: dropping the column would
        // widen it into a member.
        sqlx::query(
            "INSERT INTO users (user_id, body, campaign_scope) VALUES ('user_reader', '{}', '[]')",
        )
        .execute(db.pool())
        .await?;
        let held = crate::migrate::migrate(db, Some(10), false).await;
        let err = held.err().map(|e| format!("{e:#}")).unwrap_or_default();
        assert!(err.contains("campaign readers exist"), "{err}");
        assert!(has_campaign_scope().await?);
        sqlx::query("DELETE FROM users WHERE user_id = 'user_reader'")
            .execute(db.pool())
            .await?;

        let moved = crate::migrate::migrate(db, Some(10), false).await?;
        assert_eq!((moved.to, moved.reverted), (Some(10), vec![11]));
        assert!(!has_campaign_scope().await?);
        assert!(has_cell_scope().await?);

        let moved = crate::migrate::migrate(db, Some(9), false).await?;
        assert_eq!((moved.to, moved.reverted), (Some(9), vec![10]));
        assert!(!has_cell_scope().await?);
        assert!(has_audit().await?);

        let moved = crate::migrate::migrate(db, Some(8), false).await?;
        assert_eq!((moved.to, moved.reverted), (Some(8), vec![9]));
        assert!(!has_audit().await?);
        assert!(has_note().await?);

        let moved = crate::migrate::migrate(db, Some(7), false).await?;
        assert_eq!((moved.to, moved.reverted), (Some(7), vec![8]));
        assert!(!has_note().await?);
        assert!(has_job_cells().await?);

        let moved = crate::migrate::migrate(db, Some(6), false).await?;
        assert_eq!((moved.to, moved.reverted), (Some(6), vec![7]));
        assert!(!has_job_cells().await?);
        assert!(!has_run_flags().await?);
        assert!(has_labels().await?);

        let moved = crate::migrate::migrate(db, Some(5), false).await?;
        assert_eq!((moved.to, moved.reverted), (Some(5), vec![6]));
        assert!(!has_labels().await?);
        assert!(has_model_params().await?);

        let moved = crate::migrate::migrate(db, Some(4), false).await?;
        assert_eq!((moved.to, moved.reverted), (Some(4), vec![5]));
        assert!(!has_model_params().await?);
        assert!(has_outcomes().await?);

        let moved = crate::migrate::migrate(db, Some(3), false).await?;
        assert_eq!((moved.to, moved.reverted), (Some(3), vec![4]));
        assert!(!has_outcomes().await?);
        assert!(has_type().await?);

        let moved = crate::migrate::migrate(db, Some(2), false).await?;
        assert_eq!((moved.to, moved.reverted), (Some(2), vec![3]));
        assert!(!has_type().await?);
        let stale = crate::migrate::check(db).await;
        let err = stale.err().map(|e| e.to_string()).unwrap_or_default();
        assert!(
            err.contains("migration 3 (user account type) is not applied"),
            "{err}"
        );

        let moved = crate::migrate::migrate(db, None, false).await?;
        assert_eq!(
            (moved.to, moved.applied),
            (
                Some(19),
                vec![3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19]
            )
        );
        assert!(has_type().await?);
        assert!(has_outcomes().await?);
        assert!(has_model_params().await?);
        assert!(has_labels().await?);
        assert!(has_job_cells().await?);
        assert!(has_run_flags().await?);
        assert!(has_note().await?);
        assert!(has_audit().await?);
        assert!(has_cell_scope().await?);
        assert!(has_campaign_scope().await?);
        assert!(has_campaign_state().await?);
        assert!(has_descriptor_catalog().await?);
        assert!(has_catalog_import_runs().await?);
        assert!(has_model_alias().await?);
        assert!(has_catalog_scope().await?);
        assert!(has_model_members().await?);
        assert!(has_entries_hidden().await?);
        assert!(has_first_clerk_sign_in().await?);
        crate::migrate::check(db).await
    }

    /// Migration `0008` copies `plan_name` from the body into `note`. It does
    /// not change the body. The migration trims the note, and a blank
    /// `plan_name` gives no note.
    #[rstest::rstest]
    #[case::as_is("afm-smoke", Some("afm-smoke"))]
    #[case::padded("  afm-smoke \n", Some("afm-smoke"))]
    #[case::blank("   ", None)]
    #[tokio::test]
    async fn a_plan_note_starts_from_the_body_s_plan_name(
        #[case] plan_name: &str,
        #[case] note: Option<&str>,
    ) -> anyhow::Result<()> {
        use pipette_store::PlanStore;

        let test_db = crate::testing::test_db_or_skip!();
        let db = test_db.db();
        let store = crate::plans::SqlPlanStore::new(db.clone());
        let plan_id = pipette_plan_types::PlanId::from_uuid(uuid::Uuid::now_v7());
        let manifest = pipette_core::plan::sample_manifest(
            plan_id.clone(),
            pipette_plan_types::PlanStatus::Active,
            true,
        )?;
        store.put_plan(&manifest).await?;
        crate::migrate::migrate(db, Some(7), false).await?;
        sqlx::query(
            "UPDATE plans SET body = jsonb_set(body::jsonb, '{plan_name}', to_jsonb($2::text))::text
             WHERE plan_id = $1",
        )
        .bind(plan_id.as_str())
        .bind(plan_name)
        .execute(db.pool())
        .await?;
        let body = || async {
            sqlx::query_as::<_, (String,)>("SELECT body FROM plans WHERE plan_id = $1")
                .bind(plan_id.as_str())
                .fetch_one(db.pool())
                .await
                .map(|(body,)| body)
        };
        let before = body().await?;

        crate::migrate::migrate(db, None, false).await?;
        assert_eq!(body().await?, before);
        assert_eq!(store.get_plan(&plan_id).await?, Some(manifest));
        assert_eq!(store.get_plan_notes(&plan_id).await?.note.as_deref(), note);
        Ok(())
    }

    /// Only a missing history table reads as "nothing applied". A history that
    /// exists but fails to read stops the run, so it reports no version as new.
    #[tokio::test]
    async fn an_unreadable_migration_history_is_an_error() -> anyhow::Result<()> {
        let test_db = crate::testing::test_db_or_skip!();
        sqlx::query("ALTER TABLE _sqlx_migrations RENAME COLUMN checksum TO checksum_moved")
            .execute(test_db.db().pool())
            .await?;
        let refused = crate::migrate::migrate(test_db.db(), None, false).await;
        let err = refused.err().map(|e| format!("{e:#}")).unwrap_or_default();
        assert!(err.contains("reading the applied migrations"), "{err}");
        Ok(())
    }

    #[tokio::test]
    async fn a_locale_collation_is_refused() -> anyhow::Result<()> {
        let Some(admin) = crate::scratch::server()? else {
            return Ok(());
        };
        let created = crate::testing::Scratch::create(
            &admin,
            "TEMPLATE template0 LOCALE_PROVIDER icu ICU_LOCALE 'en-US' LOCALE 'C'",
        )
        .await;
        let scratch = match created {
            Ok(scratch) => scratch,
            // CI ships ICU. A local server without it has nothing to refuse.
            Err(_) if std::env::var_os("CI").is_none() => return Ok(()),
            Err(e) => return Err(e.into()),
        };
        let db = scratch.connect(1);
        let refused = crate::migrate::migrate(&db, None, false).await;
        db.close().await;
        let err = refused.err().map(|e| e.to_string()).unwrap_or_default();
        assert!(err.contains("sorts text by locale"), "{err}");
        Ok(())
    }
}
