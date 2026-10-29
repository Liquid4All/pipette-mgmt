use std::time::Duration;

use anyhow::Context;
use sqlx::postgres::{PgArguments, PgConnectOptions, PgPoolOptions, PgRow, PgSslMode};
use sqlx::query::{Query, QueryAs};
use sqlx::{FromRow, PgPool, Postgres, Transaction};

use pipette_core::config::{DatabaseConfig, PostgresSslMode};

use crate::retry::retry;

/// The isolation level as a literal, so that [`BEGIN`] can embed it.
macro_rules! isolation {
    () => {
        "repeatable read"
    };
}

/// Every store's writes, and its race tests, run at this level. A write that
/// lost a race fails with SQLSTATE `40001`; [`retry`] reruns it.
const ISOLATION: &str = isolation!();

/// Names the level, so a transaction runs at it whatever the session default.
const BEGIN: &str = concat!("BEGIN ISOLATION LEVEL ", isolation!());

type PgQuery = Query<'static, Postgres, PgArguments>;
type PgQueryAs<'q, T> = QueryAs<'q, Postgres, T, PgArguments>;

#[derive(Clone, Debug)]
pub struct SqlDatabase {
    pool: PgPool,
}

impl SqlDatabase {
    /// Connects lazily, so a command that reads no record does not depend on
    /// the database.
    pub fn connect(config: &DatabaseConfig) -> Self {
        // Start from the libpq `PG*` variables, which include `PGPASSWORD`.
        // The password belongs in `PGPASSWORD` and never in the config file.
        let mut options = PgConnectOptions::new_without_pgpass().application_name("pipette-mgmt");
        if let Some(host) = &config.host {
            options = options.host(host);
        }
        if let Some(port) = config.port {
            options = options.port(port);
        }
        if let Some(user) = &config.user {
            options = options.username(user);
        }
        if let Some(database) = &config.database {
            options = options.database(database);
        }
        if let Some(ssl_mode) = config.ssl_mode {
            options = options.ssl_mode(pg_ssl_mode(ssl_mode));
        }
        Self::from_options(options, config.max_connections.get())
    }

    pub(crate) fn from_options(options: PgConnectOptions, max_connections: u32) -> Self {
        let pool = PgPoolOptions::new()
            .max_connections(max_connections)
            .min_connections(0)
            .acquire_timeout(Duration::from_secs(10))
            // A bare statement outside `begin` then runs at the level of the
            // stores too.
            .after_connect(|conn, _meta| {
                Box::pin(async move {
                    sqlx::query("SELECT set_config('default_transaction_isolation', $1, false)")
                        .bind(ISOLATION)
                        .execute(&mut *conn)
                        .await?;
                    Ok(())
                })
            })
            .connect_lazy_with(options);
        Self { pool }
    }

    pub(crate) fn pool(&self) -> &PgPool {
        &self.pool
    }

    pub(crate) async fn begin(&self) -> Result<Transaction<'static, Postgres>, sqlx::Error> {
        self.pool.begin_with(BEGIN).await
    }

    /// Run one statement in its own transaction with [`retry`], returning the
    /// rows changed. `query` rebuilds the statement for each attempt.
    pub(crate) async fn execute(
        &self,
        what: &'static str,
        query: impl Fn() -> PgQuery + Send + Sync,
    ) -> anyhow::Result<u64> {
        retry(what, || {
            let query = query();
            Box::pin(async move { Ok(query.execute(self.pool()).await?.rows_affected()) })
        })
        .await
    }

    /// Like [`execute`](Self::execute), for a statement that reads every row.
    pub(crate) async fn fetch_all<'q, T>(
        &self,
        what: &'static str,
        query: impl Fn() -> PgQueryAs<'q, T> + Send + Sync,
    ) -> anyhow::Result<Vec<T>>
    where
        T: for<'r> FromRow<'r, PgRow> + Send + Unpin,
    {
        retry(what, || {
            let query = query();
            Box::pin(async move { query.fetch_all(self.pool()).await })
        })
        .await
    }

    /// Like [`execute`](Self::execute), for a statement that reads at most one
    /// row.
    pub(crate) async fn fetch_optional<'q, T>(
        &self,
        what: &'static str,
        query: impl Fn() -> PgQueryAs<'q, T> + Send + Sync,
    ) -> anyhow::Result<Option<T>>
    where
        T: for<'r> FromRow<'r, PgRow> + Send + Unpin,
    {
        retry(what, || {
            let query = query();
            Box::pin(async move { query.fetch_optional(self.pool()).await })
        })
        .await
    }

    /// Like [`execute`](Self::execute), for a statement that reads one row.
    pub(crate) async fn fetch_one<'q, T>(
        &self,
        what: &'static str,
        query: impl Fn() -> PgQueryAs<'q, T> + Send + Sync,
    ) -> anyhow::Result<T>
    where
        T: for<'r> FromRow<'r, PgRow> + Send + Unpin,
    {
        retry(what, || {
            let query = query();
            Box::pin(async move { query.fetch_one(self.pool()).await })
        })
        .await
    }

    pub async fn close(&self) {
        self.pool.close().await;
    }
}

fn pg_ssl_mode(mode: PostgresSslMode) -> PgSslMode {
    match mode {
        PostgresSslMode::Disable => PgSslMode::Disable,
        PostgresSslMode::Prefer => PgSslMode::Prefer,
        PostgresSslMode::Require => PgSslMode::Require,
        PostgresSslMode::VerifyFull => PgSslMode::VerifyFull,
    }
}

pub(crate) async fn ping(db: &SqlDatabase) -> anyhow::Result<()> {
    sqlx::query("SELECT 1")
        .execute(db.pool())
        .await
        .context("the [database] server did not answer")?;
    Ok(())
}
