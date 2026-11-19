//! Helpers for the tests that run on a [`TestDb`](crate::scratch::TestDb).

use std::fmt::Debug;
use std::future::Future;
use std::sync::Arc;

use anyhow::Context;
use futures::future::Either;
use jiff::Timestamp;
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
        registered_at: Timestamp::now(),
        device_profile: Default::default(),
        capabilities: Default::default(),
    })
}

#[cfg(test)]
mod tests {
    use futures::{StreamExt, TryStreamExt};

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

        let has_rename_cascade = || {
            exists(
                "SELECT EXISTS (SELECT 1 FROM pg_constraint
                 WHERE connamespace = current_schema()::regnamespace
                     AND conname = 'descriptor_members_scope_name_key_fkey' AND confupdtype = 'c')",
            )
        };

        let has_run_evidence = || {
            exists(
                "SELECT EXISTS (SELECT 1 FROM information_schema.tables WHERE table_schema = current_schema() AND table_name = 'run_evidence')",
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
        assert!(has_rename_cascade().await?);
        assert!(has_run_evidence().await?);

        let has_job_queue = || {
            sqlx::query_scalar::<_, bool>(
                "SELECT to_regclass('queue_jobs') IS NOT NULL \
                 AND to_regclass('queue_tmp') IS NOT NULL",
            )
            .fetch_one(db.pool())
        };
        assert!(has_job_queue().await?);
        let has_outcome_repairs = || {
            sqlx::query_scalar::<_, bool>(
                "SELECT to_regclass('submission_outcome_repairs') IS NOT NULL",
            )
            .fetch_one(db.pool())
        };
        assert!(has_outcome_repairs().await?);
        let has_incremental_eligibility = || {
            sqlx::query_scalar::<_, bool>(
                "SELECT to_regclass('queue_workers') IS NOT NULL \
                 AND to_regclass('queue_reindex_jobs') IS NULL",
            )
            .fetch_one(db.pool())
        };
        assert!(has_incremental_eligibility().await?);
        let has_campaign_groups = || exists("SELECT to_regclass('campaign_groups') IS NOT NULL");
        let has_campaign_group_members =
            || exists("SELECT to_regclass('campaign_group_members') IS NOT NULL");
        assert!(has_campaign_groups().await?);
        assert!(has_campaign_group_members().await?);
        let has_campaign_group_policies =
            || exists("SELECT to_regclass('campaign_group_policies') IS NOT NULL");
        assert!(has_campaign_group_policies().await?);
        let has_maintenance_state = || {
            sqlx::query_scalar::<_, bool>(
                "SELECT to_regclass('queue_maintenance_state') IS NOT NULL",
            )
            .fetch_one(db.pool())
        };
        assert!(!has_maintenance_state().await?);
        let has_suspensions = || exists("SELECT to_regclass('queue_suspensions') IS NOT NULL");
        assert!(!has_suspensions().await?);
        let has_policy_state = || {
            exists(
                "SELECT EXISTS (SELECT 1 FROM information_schema.columns
                 WHERE table_schema = current_schema() AND table_name = 'campaign_group_members' AND column_name = 'policy_state')",
            )
        };
        assert!(has_policy_state().await?);
        let has_family = || {
            exists(
                "SELECT count(*) = 2 FROM information_schema.columns
                 WHERE table_schema = current_schema() AND table_name = 'queue_jobs'
                     AND column_name IN ('family_key', 'sizes')",
            )
        };
        assert!(has_family().await?);
        let has_rerun_results =
            || exists("SELECT to_regclass('campaign_group_rerun_results') IS NOT NULL");
        assert!(has_rerun_results().await?);
        let has_short_repetition = || {
            exists(
                "SELECT count(*) = 2 FROM information_schema.columns
                 WHERE table_schema = current_schema() AND table_name = 'campaign_group_rerun_results'
                     AND column_name IN ('short_repetition_ms', 'short_repetition_percent')",
            )
        };
        assert!(has_short_repetition().await?);
        let has_failed_codes = || {
            exists(
                "SELECT EXISTS (SELECT 1 FROM information_schema.columns
                 WHERE table_schema = current_schema() AND table_name = 'campaign_group_rerun_results'
                     AND column_name = 'codes')",
            )
        };
        assert!(has_failed_codes().await?);
        let has_job_states = || {
            exists(
                "SELECT EXISTS (SELECT 1 FROM information_schema.tables
                 WHERE table_schema = current_schema() AND table_name = 'campaign_group_job_states')",
            )
        };
        assert!(has_job_states().await?);
        let has_spans = || {
            exists(
                "SELECT to_regclass('plan_job_spans') IS NOT NULL
                     AND to_regclass('plan_lineages') IS NOT NULL",
            )
        };
        assert!(has_spans().await?);
        let has_plan_job_rows = || exists("SELECT to_regclass('plan_job_rows') IS NOT NULL");
        assert!(has_plan_job_rows().await?);
        let has_plan_tags = || exists("SELECT to_regclass('plan_tags') IS NOT NULL");
        assert!(has_plan_tags().await?);
        let moved = crate::migrate::migrate(db, Some(44), false).await?;
        assert_eq!((moved.to, moved.reverted), (Some(44), vec![45]));
        assert!(!has_plan_tags().await?);
        let has_plan_jobs = || exists("SELECT to_regclass('plan_jobs') IS NOT NULL");
        assert!(!has_plan_jobs().await?);
        let moved = crate::migrate::migrate(db, Some(43), false).await?;
        assert_eq!((moved.to, moved.reverted), (Some(43), vec![44]));
        assert!(has_plan_jobs().await?);
        assert!(has_plan_job_rows().await?);
        let moved = crate::migrate::migrate(db, Some(42), false).await?;
        assert_eq!((moved.to, moved.reverted), (Some(42), vec![43]));
        assert!(!has_plan_job_rows().await?);
        assert!(has_spans().await?);
        let moved = crate::migrate::migrate(db, Some(41), false).await?;
        assert_eq!((moved.to, moved.reverted), (Some(41), vec![42]));
        assert!(!has_spans().await?);
        let moved = crate::migrate::migrate(db, Some(40), false).await?;
        assert_eq!((moved.to, moved.reverted), (Some(40), vec![41]));
        assert!(has_job_states().await?);
        let moved = crate::migrate::migrate(db, Some(39), false).await?;
        assert_eq!((moved.to, moved.reverted), (Some(39), vec![40]));
        assert!(!has_job_states().await?);
        assert!(has_failed_codes().await?);
        let moved = crate::migrate::migrate(db, Some(38), false).await?;
        assert_eq!((moved.to, moved.reverted), (Some(38), vec![39]));
        assert!(!has_failed_codes().await?);
        assert!(has_short_repetition().await?);
        let moved = crate::migrate::migrate(db, Some(37), false).await?;
        assert_eq!((moved.to, moved.reverted), (Some(37), vec![38]));
        assert!(!has_short_repetition().await?);
        assert!(has_rerun_results().await?);
        let moved = crate::migrate::migrate(db, Some(36), false).await?;
        assert_eq!((moved.to, moved.reverted), (Some(36), vec![37]));
        assert!(!has_rerun_results().await?);
        assert!(has_policy_state().await?);
        let moved = crate::migrate::migrate(db, Some(35), false).await?;
        assert_eq!((moved.to, moved.reverted), (Some(35), vec![36]));
        assert!(!has_policy_state().await?);
        assert!(has_campaign_group_members().await?);
        let moved = crate::migrate::migrate(db, Some(34), false).await?;
        assert_eq!((moved.to, moved.reverted), (Some(34), vec![35]));
        assert!(!has_family().await?);
        let moved = crate::migrate::migrate(db, Some(33), false).await?;
        assert_eq!((moved.to, moved.reverted), (Some(33), vec![34]));
        assert!(has_suspensions().await?);
        let moved = crate::migrate::migrate(db, Some(32), false).await?;
        assert_eq!((moved.to, moved.reverted), (Some(32), vec![33]));
        assert!(!has_maintenance_state().await?);
        let moved = crate::migrate::migrate(db, Some(31), false).await?;
        assert_eq!((moved.to, moved.reverted), (Some(31), vec![32]));
        assert!(has_maintenance_state().await?);
        let moved = crate::migrate::migrate(db, Some(30), false).await?;
        assert_eq!((moved.to, moved.reverted), (Some(30), vec![31]));
        let moved = crate::migrate::migrate(db, Some(29), false).await?;
        assert_eq!((moved.to, moved.reverted), (Some(29), vec![30]));
        assert!(!has_campaign_group_policies().await?);
        assert!(has_campaign_groups().await?);
        let moved = crate::migrate::migrate(db, Some(28), false).await?;
        assert_eq!((moved.to, moved.reverted), (Some(28), vec![29]));
        assert!(!has_campaign_groups().await?);
        assert!(!has_campaign_group_members().await?);
        let has_queue_cascade = || {
            sqlx::query_scalar::<_, bool>(
                "SELECT to_regclass('plan_index_gc_candidates') IS NOT NULL",
            )
            .fetch_one(db.pool())
        };
        assert!(has_queue_cascade().await?);
        let moved = crate::migrate::migrate(db, Some(27), false).await?;
        assert_eq!((moved.to, moved.reverted), (Some(27), vec![28]));
        assert!(!has_queue_cascade().await?);
        let moved = crate::migrate::migrate(db, Some(23), false).await?;
        assert_eq!((moved.to, moved.reverted), (Some(23), vec![27, 26, 25, 24]));
        assert!(!has_incremental_eligibility().await?);
        assert!(has_job_queue().await?);
        let moved = crate::migrate::migrate(db, Some(21), false).await?;
        assert_eq!((moved.to, moved.reverted), (Some(21), vec![23, 22]));
        assert!(!has_job_queue().await?);
        assert!(!has_outcome_repairs().await?);

        let moved = crate::migrate::migrate(db, Some(20), false).await?;
        assert_eq!((moved.to, moved.reverted), (Some(20), vec![21]));
        assert!(!has_run_evidence().await?);
        assert!(has_rename_cascade().await?);

        let moved = crate::migrate::migrate(db, Some(19), false).await?;
        assert_eq!((moved.to, moved.reverted), (Some(19), vec![20]));
        assert!(!has_rename_cascade().await?);
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
                Some(45),
                vec![
                    3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23,
                    24, 25, 26, 27, 28, 29, 30, 31, 32, 33, 34, 35, 36, 37, 38, 39, 40, 41, 42, 43,
                    44, 45
                ]
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
        assert!(has_rename_cascade().await?);
        assert!(has_first_clerk_sign_in().await?);
        assert!(has_run_evidence().await?);
        assert!(has_job_queue().await?);
        assert!(has_incremental_eligibility().await?);
        assert!(has_campaign_groups().await?);
        assert!(has_campaign_group_members().await?);
        assert!(has_campaign_group_policies().await?);
        assert!(has_family().await?);
        assert!(has_policy_state().await?);
        assert!(has_rerun_results().await?);
        assert!(has_failed_codes().await?);
        assert!(has_plan_job_rows().await?);
        assert!(!has_plan_jobs().await?);
        crate::migrate::check(db).await
    }

    /// Migration `0041` gives a member state an older binary kept, which has no
    /// `triggered`, an empty one, so it reads; a state that has one keeps it.
    #[tokio::test]
    async fn the_triggered_migration_makes_older_member_states_read() -> anyhow::Result<()> {
        use jiff::Timestamp;

        use pipette_core::campaign::{CampaignGroup, CampaignOwner, GroupPolicyState, NewCampaign};
        use pipette_core::types::Mint;
        use pipette_plan_types::{CampaignId, PlanId, PolicyKind, UserId};
        use pipette_store::{CampaignGroupStore, CampaignStore};

        let test_db = crate::testing::test_db_or_skip!();
        let db = test_db.db();
        crate::migrate::migrate(db, Some(40), false).await?;
        let store = crate::campaign_groups::SqlCampaignGroupStore::new(db.clone());
        let owner = CampaignOwner::User {
            user_id: UserId::mint(),
        };
        let group = CampaignGroup::new("aa-perf".parse()?, owner.clone(), Timestamp::now());
        store.create_group(&group).await?;
        let campaigns = crate::campaigns::SqlCampaignStore::new(db.clone());
        let mut ids = Vec::new();
        for name in ["older", "newer"] {
            let campaign = NewCampaign::new(name.parse()?, owner.clone())
                .around(PlanId::from_uuid(uuid::Uuid::now_v7()), Timestamp::now());
            campaigns.put_campaign(&campaign).await?;
            ids.push(campaign.campaign_id);
        }
        ids.sort();
        store.add_members(&group.group_id, &ids).await?;
        let written = [
            r#"{"policies": [], "decided": ["job-1"], "filed": null, "refusal": null}"#,
            r#"{"policies": [], "triggered": ["spread"], "filed": null, "refusal": null}"#,
        ];
        for (campaign_id, state) in ids.iter().zip(written) {
            sqlx::query(
                "UPDATE campaign_group_members SET policy_state = $2 WHERE campaign_id = $1",
            )
            .bind(campaign_id.as_str())
            .bind(state)
            .execute(db.pool())
            .await?;
        }
        let read = || async {
            let states: Vec<(CampaignId, Result<Option<GroupPolicyState>, String>)> = store
                .policy_states(&group.group_id)
                .await?
                .into_iter()
                .map(|member| (member.campaign_id, member.policy_state.map_err(|e| e.key)))
                .collect();
            anyhow::Ok(states)
        };
        assert!(read().await?[0].1.is_err());

        crate::migrate::migrate(db, None, false).await?;
        let newer = GroupPolicyState {
            triggered: [PolicyKind::Spread].into(),
            ..GroupPolicyState::default()
        };
        assert_eq!(
            read().await?,
            [
                (ids[0].clone(), Ok(Some(GroupPolicyState::default()))),
                (ids[1].clone(), Ok(Some(newer)))
            ]
        );
        Ok(())
    }

    /// Migration `0039` gives a stored failed policy and a member state's
    /// failed policy each code it lists, else the three failed re-ran until
    /// then, at its `max_reruns`, which becomes 0, and the failed re-runs those
    /// three codes; the other kinds, smaller_failed among them, none. A move
    /// down gives each failed policy its largest limit as `max_reruns` and
    /// takes the codes off.
    #[tokio::test]
    async fn the_failed_codes_migration_keeps_what_failed_re_ran() -> anyhow::Result<()> {
        use jiff::Timestamp;

        use pipette_core::campaign::{CampaignGroup, CampaignOwner, GroupPolicyState, NewCampaign};
        use pipette_core::types::Mint;
        use pipette_plan_types::{
            CampaignGroupId, CampaignGroupPolicy, CampaignId, FailureCode, PlanId, PolicyKind,
            PolicySettings, UserId,
        };
        use pipette_store::{CampaignGroupStore, CampaignStore, JobStateChanges};

        let test_db = crate::testing::test_db_or_skip!();
        let db = test_db.db();
        let store = crate::campaign_groups::SqlCampaignGroupStore::new(db.clone());
        let owner = CampaignOwner::User {
            user_id: UserId::mint(),
        };
        let group = CampaignGroup::new("aa-perf".parse()?, owner.clone(), Timestamp::now());
        let listing = CampaignGroup::new("aa-listed".parse()?, owner.clone(), Timestamp::now());
        store.create_group(&group).await?;
        store.create_group(&listing).await?;
        let (group_id, listing_id) = (group.group_id, listing.group_id);
        let campaign = NewCampaign::new("nightly".parse()?, owner)
            .around(PlanId::from_uuid(uuid::Uuid::now_v7()), Timestamp::now());
        crate::campaigns::SqlCampaignStore::new(db.clone())
            .put_campaign(&campaign)
            .await?;
        let campaign_id = campaign.campaign_id;
        store
            .add_members(&group_id, std::slice::from_ref(&campaign_id))
            .await?;
        let on = |kind: PolicyKind, group_id: &CampaignGroupId| CampaignGroupPolicy {
            enabled: true,
            ..kind.defaults(group_id.clone())
        };
        let limited =
            |policy: &CampaignGroupPolicy, codes: &[(FailureCode, u32)]| CampaignGroupPolicy {
                settings: PolicySettings {
                    codes: Some(codes.iter().copied().collect()),
                    ..PolicySettings::default()
                },
                ..policy.clone()
            };
        let older = |policy: &CampaignGroupPolicy, max_reruns: u32| CampaignGroupPolicy {
            max_reruns,
            settings: PolicySettings::default(),
            ..policy.clone()
        };
        let failed = on(PolicyKind::Failed, &group_id);
        let spread = on(PolicyKind::Spread, &group_id);
        let smaller = on(PolicyKind::SmallerFailed, &group_id);
        crate::migrate::migrate(db, Some(38), false).await?;
        let stored = older(&failed, 3);
        let mut listed = serde_json::to_value(older(&on(PolicyKind::Failed, &listing_id), 2))?;
        listed["settings"]["codes"] = serde_json::json!(["harness_error", "device_not_ready"]);
        sqlx::query(
            "INSERT INTO campaign_group_policies (group_id, kind, body)
             VALUES ($1, 'failed', $2), ($1, 'smaller_failed', $3), ($4, 'failed', $5)",
        )
        .bind(group_id.as_str())
        .bind(serde_json::to_string_pretty(&stored)?)
        .bind(serde_json::to_string(&smaller)?)
        .bind(listing_id.as_str())
        .bind(listed.to_string())
        .execute(db.pool())
        .await?;
        sqlx::query(
            "INSERT INTO campaign_group_rerun_results (group_id, rerun_job_id, kind, campaign_id,
                 rerun_of, plan_id, filed_at, attempt, benchmark_id, before)
             SELECT $1, 'job-2', kind, 'campaign_x', 'job-1', 'plan-x', now(), 1, 'bench', 1
             FROM unnest(ARRAY['failed', 'swap']) AS kind",
        )
        .bind(group_id.as_str())
        .execute(db.pool())
        .await?;
        let state = |policies: &[&CampaignGroupPolicy]| GroupPolicyState {
            policies: policies.iter().copied().cloned().collect(),
            ..GroupPolicyState::default()
        };
        assert!(
            store
                .set_policy_state(
                    &group_id,
                    &campaign_id,
                    &state(&[&stored, &spread, &smaller]),
                    &JobStateChanges::default(),
                )
                .await?
        );
        let kept = || async {
            let state: String = sqlx::query_scalar(
                "SELECT policy_state FROM campaign_group_members WHERE campaign_id = $1",
            )
            .bind(campaign_id.as_str())
            .fetch_one(db.pool())
            .await?;
            anyhow::Ok(serde_json::from_str::<GroupPolicyState>(&state)?)
        };

        crate::migrate::migrate(db, None, false).await?;
        let defaults = [
            FailureCode::RuntimeError,
            FailureCode::DeviceError,
            FailureCode::Unspecified,
        ]
        .map(|code| (code, 3));
        let moved = limited(&failed, &defaults);
        assert_eq!(
            store.list_policies(&group_id).await?,
            [moved.clone(), smaller.clone()]
        );
        let codes = [
            (FailureCode::DeviceNotReady, 2),
            (FailureCode::HarnessError, 2),
        ];
        assert_eq!(
            store.list_policies(&listing_id).await?,
            [limited(&on(PolicyKind::Failed, &listing_id), &codes)]
        );
        let rerun_codes: Vec<_> = store
            .waiting_reruns(&group_id, &CampaignId::try_new("campaign_x")?)
            .await?
            .into_iter()
            .map(|rerun| (rerun.kind, rerun.codes))
            .collect();
        let three = defaults.map(|(code, _)| code).to_vec();
        assert_eq!(
            rerun_codes,
            [(PolicyKind::Failed, Some(three)), (PolicyKind::Swap, None)]
        );
        assert_eq!(kept().await?, state(&[&moved, &spread, &smaller]));

        let mixed = limited(
            &failed,
            &[
                (FailureCode::RuntimeError, 2),
                (FailureCode::DeviceNotReady, 4),
            ],
        );
        assert!(store.replace_policy(&moved, &mixed).await?);
        assert!(
            store
                .set_policy_state(
                    &group_id,
                    &campaign_id,
                    &state(&[&mixed, &spread, &smaller]),
                    &JobStateChanges::default(),
                )
                .await?
        );
        crate::migrate::migrate(db, Some(38), false).await?;
        let largest = older(&failed, 4);
        assert_eq!(
            store.list_policies(&group_id).await?,
            [largest.clone(), smaller.clone()]
        );
        assert_eq!(kept().await?, state(&[&largest, &spread, &smaller]));
        Ok(())
    }

    /// The `0031` body of `queue_match_clauses` gives the same stored
    /// projections as the `0025` body. A move down restores the `0025` body.
    #[tokio::test]
    async fn the_linear_matcher_projection_keeps_stored_projections() -> anyhow::Result<()> {
        use futures::stream::{self, StreamExt, TryStreamExt};

        let test_db = crate::testing::test_db_or_skip!();
        let db = test_db.db();
        crate::migrate::migrate(db, Some(31), false).await?;
        let linear = || {
            sqlx::query_scalar::<_, bool>(
                "SELECT prosrc LIKE '%array_append%' FROM pg_proc \
                 WHERE oid = 'queue_match_clauses(text)'::regprocedure",
            )
            .fetch_one(db.pool())
        };
        let roster: Vec<String> = (0..300).map(|i| format!("client-{i}-é")).collect();
        let bodies = [
            serde_json::to_string(&serde_json::json!({"clients": roster}))?,
            serde_json::to_string_pretty(&serde_json::json!({
                "requires": ["chip:a"],
                "any_of": [["tag:日本"], []],
                "exclude_clients": ["a\\b", "\"q\""],
                "spec": {"clients": ["ignored"]},
            }))?,
            r#"{"requires":["\u0000"],"requires":["chip:b"],"x":"\u0000"}"#.to_owned(),
            r#"{"clients":["\u0000"]}"#.to_owned(),
            "{".to_owned(),
        ];

        let moved = crate::migrate::migrate(db, Some(30), false).await?;
        assert_eq!((moved.to, moved.reverted), (Some(30), vec![31]));
        assert!(!linear().await?);
        stream::iter(bodies.iter().enumerate())
            .map(Ok::<_, anyhow::Error>)
            .try_for_each(|(i, body)| async move {
                sqlx::query("INSERT INTO queue_jobs (job_id, body, revision) VALUES ($1, $2, 1)")
                    .bind(format!("projection-{i}"))
                    .bind(body)
                    .execute(db.pool())
                    .await?;
                anyhow::Ok(())
            })
            .await?;

        let moved = crate::migrate::migrate(db, Some(31), false).await?;
        assert_eq!((moved.to, moved.applied), (Some(31), vec![31]));
        assert!(linear().await?);
        let differing: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM queue_jobs \
             WHERE match_clauses IS DISTINCT FROM queue_match_clauses(body)",
        )
        .fetch_one(db.pool())
        .await?;
        assert_eq!(differing, 0);

        let moved = crate::migrate::migrate(db, Some(30), false).await?;
        assert_eq!((moved.to, moved.reverted), (Some(30), vec![31]));
        assert!(!linear().await?);
        Ok(())
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
        // A migration below 0042 drops the spans. Write the fixture again
        // through the current writer.
        store.put_plan(&manifest).await?;
        assert_eq!(store.get_plan(&plan_id).await?, Some(manifest));
        assert_eq!(store.get_plan_notes(&plan_id).await?.note.as_deref(), note);
        Ok(())
    }

    #[tokio::test]
    async fn the_clause_migration_projects_existing_jobs_and_flags_denials() -> anyhow::Result<()> {
        let test_db = test_db_or_skip!();
        let db = test_db.db();
        crate::migrate::migrate(db, Some(24), false).await?;
        sqlx::query(
            "INSERT INTO queue_jobs (job_id, body, revision, needs_reindex) VALUES \
             ('old-denied', '{\"requires\":null,\"spec\":\"ignored\"}', 1, false), \
             ('old-healthy', '{}', 1, false), ('old-corrupt', 'not json', 1, false)",
        )
        .execute(db.pool())
        .await?;
        sqlx::query(
            "INSERT INTO queue_jobs (job_id, body, revision, needs_reindex, leased_by, lease_expiry) VALUES \
             ('old-leased', '{}', 1, false, 'holder', now() + interval '1 hour'), \
             ('old-leased-denied', '{}', 1, false, 'holder', now() + interval '1 hour'), \
             ('old-unmatchable', '{\"requires\":[\"os:linux\"],\"excludes\":[\"os:linux\"]}', 1, false, NULL, NULL)",
        )
        .execute(db.pool()).await?;
        sqlx::query(
            "INSERT INTO queue_denials (job_id, client_id) VALUES \
             ('old-denied', 'client'), ('old-leased-denied', 'client')",
        )
        .execute(db.pool())
        .await?;
        let nul_body = serde_json::json!({
            "requires": ["chip:chip-a"],
            "spec": {"extra": "\u{0000}", "nested": [{"requires": ["ignored"]}]},
        });
        sqlx::query(
            "INSERT INTO queue_jobs (job_id, body, revision, needs_reindex) \
             VALUES ('old-nul', $1, 1, false)",
        )
        .bind(serde_json::to_string(&nul_body)?)
        .execute(db.pool())
        .await?;
        crate::migrate::migrate(db, None, false).await?;
        let preserved: String =
            sqlx::query_scalar("SELECT body FROM queue_jobs WHERE job_id = 'old-nul'")
                .fetch_one(db.pool())
                .await?;
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&preserved)?,
            nul_body
        );
        let rows: Vec<(String, Option<serde_json::Value>, bool)> = sqlx::query_as(
            "SELECT job_id, match_clauses, needs_reindex FROM queue_jobs ORDER BY job_id",
        )
        .fetch_all(db.pool())
        .await?;
        assert_eq!(
            rows,
            vec![
                ("old-corrupt".into(), None, true),
                (
                    "old-denied".into(),
                    Some(serde_json::json!({"requires": null})),
                    true
                ),
                ("old-healthy".into(), Some(serde_json::json!({})), true),
                ("old-leased".into(), Some(serde_json::json!({})), false),
                (
                    "old-leased-denied".into(),
                    Some(serde_json::json!({})),
                    true
                ),
                (
                    "old-nul".into(),
                    Some(serde_json::json!({"requires": ["chip:chip-a"]})),
                    true
                ),
                (
                    "old-unmatchable".into(),
                    Some(serde_json::json!({"requires": ["os:linux"], "excludes": ["os:linux"]})),
                    true
                ),
            ]
        );
        crate::migrate::migrate(db, Some(24), false).await?;
        crate::migrate::migrate(db, None, false).await?;
        Ok(())
    }

    /// Migration `0024` moves the worker set into `queue_workers` and flags
    /// every job. Its down script puts the worker set back, flags each leased
    /// job that still needs a reindex, and restores each row's deadline.
    #[rstest::rstest]
    #[case::valid(Some(r#"["ev1_b","ev1_a"]"#), &["ev1_a", "ev1_b"])]
    #[case::duplicates(Some(r#"["ev1_a","ev1_a"]"#), &["ev1_a"])]
    #[case::empty(Some("[]"), &[])]
    #[case::missing(None, &[])]
    #[case::invalid_json(Some("not json"), &[])]
    #[case::object(Some(r#"{"worker":"ev1_a"}"#), &[])]
    #[case::string(Some(r#""ev1_a""#), &[])]
    #[case::number(Some("42"), &[])]
    #[case::null(Some("null"), &[])]
    #[case::mixed_array(Some(r#"["ev1_a",42]"#), &[])]
    #[case::null_element(Some(r#"["ev1_a",null]"#), &[])]
    #[tokio::test]
    async fn the_incremental_eligibility_migration_keeps_the_index_state(
        #[case] legacy_workers: Option<&str>,
        #[case] expected_workers: &[&str],
    ) -> anyhow::Result<()> {
        let test_db = crate::testing::test_db_or_skip!();
        let db = test_db.db();
        crate::migrate::migrate(db, Some(23), false).await?;
        sqlx::raw_sql(
            "INSERT INTO queue_jobs (job_id, body, expires_at, revision) \
             VALUES ('job-free', '{}', '2026-12-01T00:00:00Z', 1); \
             INSERT INTO queue_jobs (job_id, body, leased_by, lease_expiry, revision) \
             VALUES ('job-held', '{}', 'ev1_a', '2026-11-01T00:00:00Z', 1); \
             INSERT INTO queue_eligibility (client_id, job_id, expires_at) \
             VALUES ('ev1_a', 'job-free', '2026-01-01T00:00:00Z'); \
             INSERT INTO queue_reindex_jobs (job_id) VALUES ('job-free'); \
             DELETE FROM queue_maintenance_state; \
             INSERT INTO queue_maintenance_state (name, body) VALUES \
             ('eligible-indexed', '[\"job-free/never.json\"]'), ('gc-candidates', '[]');",
        )
        .execute(db.pool())
        .await?;
        if let Some(workers) = legacy_workers {
            sqlx::query(
                "INSERT INTO queue_maintenance_state (name, body) VALUES ('eligible-workers', $1)",
            )
            .bind(workers)
            .execute(db.pool())
            .await?;
        }
        let state_names = || {
            sqlx::query_scalar::<_, String>(
                "SELECT name FROM queue_maintenance_state ORDER BY name",
            )
            .fetch_all(db.pool())
        };

        crate::migrate::migrate(db, Some(24), false).await?;
        let workers: Vec<String> =
            sqlx::query_scalar("SELECT client_id FROM queue_workers ORDER BY client_id")
                .fetch_all(db.pool())
                .await?;
        assert_eq!(workers, expected_workers);
        assert_eq!(state_names().await?, ["gc-candidates"]);
        let unflagged: i64 =
            sqlx::query_scalar("SELECT count(*) FROM queue_jobs WHERE NOT needs_reindex")
                .fetch_one(db.pool())
                .await?;
        assert_eq!(unflagged, 0);

        crate::migrate::migrate(db, Some(23), false).await?;
        let workers: String = sqlx::query_scalar(
            "SELECT body FROM queue_maintenance_state WHERE name = 'eligible-workers'",
        )
        .fetch_one(db.pool())
        .await?;
        assert_eq!(
            serde_json::from_str::<Vec<String>>(&workers)?,
            expected_workers
        );
        let flagged: Vec<String> = sqlx::query_scalar("SELECT job_id FROM queue_reindex_jobs")
            .fetch_all(db.pool())
            .await?;
        assert_eq!(flagged, ["job-held"]);
        let deadline: Option<jiff_sqlx::Timestamp> =
            sqlx::query_scalar("SELECT expires_at FROM queue_eligibility")
                .fetch_one(db.pool())
                .await?;
        assert_eq!(
            deadline.map(jiff_sqlx::Timestamp::to_jiff),
            Some("2026-12-01T00:00:00Z".parse()?)
        );
        Ok(())
    }

    /// Migration `0028` deletes orphan eligibility, denial and lost-lease
    /// rows and absent clients' liveness, then adds cascading foreign keys.
    #[tokio::test]
    async fn the_queue_cascade_migration_drops_orphans_and_cascades() -> anyhow::Result<()> {
        let test_db = crate::testing::test_db_or_skip!();
        let db = test_db.db();
        crate::migrate::migrate(db, Some(27), false).await?;
        sqlx::raw_sql(
            "INSERT INTO clients (client_id, body, revision) VALUES ('ev1_live', '{}', 1); \
             INSERT INTO client_liveness (client_id, last_seen_at) \
             VALUES ('ev1_live', now()), ('ev1_gone', now()); \
             INSERT INTO queue_jobs (job_id, body, revision) VALUES ('job-live', '{}', 1); \
             INSERT INTO queue_eligibility (client_id, job_id) \
             VALUES ('ev1_a', 'job-live'), ('ev1_a', 'job-gone'); \
             INSERT INTO queue_denials (job_id, client_id) \
             VALUES ('job-live', 'ev1_a'), ('job-gone', 'ev1_a'); \
             INSERT INTO queue_lost_leases (job_id, client_id, lease_expiry) \
             VALUES ('job-live', 'ev1_a', '2026-11-01T00:00:00Z'), \
             ('job-gone', 'ev1_a', '2026-11-01T00:00:00Z'); \
             INSERT INTO queue_maintenance_state (name, body) \
             VALUES ('gc-candidates', '[\"plans:job-plans/x\"]'), ('backlog', '{}');",
        )
        .execute(db.pool())
        .await?;
        let jobs_named = |table: &'static str| {
            sqlx::query_scalar::<_, String>(match table {
                "queue_eligibility" => "SELECT job_id FROM queue_eligibility ORDER BY job_id",
                "queue_denials" => "SELECT job_id FROM queue_denials ORDER BY job_id",
                _ => "SELECT job_id FROM queue_lost_leases ORDER BY job_id",
            })
            .fetch_all(db.pool())
        };
        let tables = ["queue_eligibility", "queue_denials", "queue_lost_leases"];

        crate::migrate::migrate(db, Some(28), false).await?;
        futures::stream::iter(tables)
            .map(Ok::<_, anyhow::Error>)
            .try_for_each(|table| async move {
                assert_eq!(jobs_named(table).await?, ["job-live"], "{table}");
                anyhow::Ok(())
            })
            .await?;
        let state: Vec<String> =
            sqlx::query_scalar("SELECT name FROM queue_maintenance_state ORDER BY name")
                .fetch_all(db.pool())
                .await?;
        assert_eq!(state, ["backlog"]);
        let live_clients: Vec<String> =
            sqlx::query_scalar("SELECT client_id FROM client_liveness ORDER BY client_id")
                .fetch_all(db.pool())
                .await?;
        assert_eq!(live_clients, ["ev1_live"]);
        sqlx::query("DELETE FROM clients WHERE client_id = 'ev1_live'")
            .execute(db.pool())
            .await?;
        let remaining: i64 = sqlx::query_scalar("SELECT count(*) FROM client_liveness")
            .fetch_one(db.pool())
            .await?;
        assert_eq!(remaining, 0);

        sqlx::query("DELETE FROM queue_jobs WHERE job_id = 'job-live'")
            .execute(db.pool())
            .await?;
        futures::stream::iter(tables)
            .map(Ok::<_, anyhow::Error>)
            .try_for_each(|table| async move {
                assert!(jobs_named(table).await?.is_empty(), "{table}");
                anyhow::Ok(())
            })
            .await?;
        Ok(())
    }

    #[tokio::test]
    async fn the_last_grant_client_migration_cleans_orphans_and_cascades() -> anyhow::Result<()> {
        let test_db = crate::testing::test_db_or_skip!();
        let db = test_db.db();
        crate::migrate::migrate(db, Some(32), false).await?;
        sqlx::raw_sql(
            "INSERT INTO clients (client_id, body, revision) VALUES ('ev1_live', '{}', 1); \
             INSERT INTO queue_client_last_grant (client_id, job_id, granted_at) \
             VALUES ('ev1_live', 'finished', now()), ('ev1_gone', 'finished', now());",
        )
        .execute(db.pool())
        .await?;
        let grant_clients = || {
            sqlx::query_scalar::<_, String>(
                "SELECT client_id FROM queue_client_last_grant ORDER BY client_id",
            )
            .fetch_all(db.pool())
        };
        crate::migrate::migrate(db, Some(33), false).await?;
        assert_eq!(grant_clients().await?, ["ev1_live"]);
        sqlx::query("DELETE FROM clients WHERE client_id = 'ev1_live'")
            .execute(db.pool())
            .await?;
        assert!(grant_clients().await?.is_empty());

        crate::migrate::migrate(db, Some(32), false).await?;
        sqlx::query(
            "INSERT INTO queue_client_last_grant (client_id, job_id, granted_at) \
             VALUES ('ev1_gone', 'finished', now())",
        )
        .execute(db.pool())
        .await?;
        assert_eq!(grant_clients().await?, ["ev1_gone"]);
        crate::migrate::migrate(db, Some(33), false).await?;
        assert!(grant_clients().await?.is_empty());
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
