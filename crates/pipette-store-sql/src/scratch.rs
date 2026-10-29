//! A scratch schema for tests, on the server at `TEST_DATABASE_URL`, for
//! example `postgres://postgres@127.0.0.1:5432/postgres`. If the variable is
//! not set, a test skips on a local machine. In CI, the test fails.

use sqlx::postgres::PgConnectOptions;
use sqlx::{AssertSqlSafe, Connection, PgConnection};

use crate::db::SqlDatabase;

const URL_VAR: &str = "TEST_DATABASE_URL";

/// The connections in the pool of a [`TestDb`].
pub(crate) const POOL_SIZE: u32 = 4;

/// The database that holds every [`TestDb`]'s schema. A database per test is
/// slow: the server creates databases one at a time, and each drop forces a
/// checkpoint.
const SHARED: &str = "pipette_test";

/// Listings sort in byte order, as the object stores list keys.
const BYTE_ORDER: &str = "TEMPLATE template0 ENCODING 'UTF8' LC_COLLATE 'C' LC_CTYPE 'C'";

/// The server at `TEST_DATABASE_URL`. Return `None` if `TEST_DATABASE_URL` is
/// not set and the run is not in CI.
pub(crate) fn server() -> anyhow::Result<Option<PgConnectOptions>> {
    let Ok(url) = std::env::var(URL_VAR) else {
        anyhow::ensure!(
            std::env::var_os("CI").is_none(),
            "{URL_VAR} must be set in CI"
        );
        eprintln!("skipping: {URL_VAR} is unset");
        return Ok(None);
    };
    Ok(Some(url.parse()?))
}

/// A new name, which is also a valid identifier.
pub(crate) fn scratch_name() -> String {
    format!("pipette_test_{}", uuid::Uuid::new_v4().simple())
}

/// Run `statements` on `admin` from `drop`, ignoring failures.
pub(crate) fn run_on_drop(admin: PgConnectOptions, statements: Vec<String>) {
    // A thread and a runtime of its own: `drop` cannot await, and the
    // test's runtime may be the current-thread one being torn down.
    let dropped = std::thread::spawn(move || {
        let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        else {
            return;
        };
        runtime.block_on(async move {
            if let Ok(mut conn) = PgConnection::connect_with(&admin).await {
                for statement in statements {
                    let _ = sqlx::query(AssertSqlSafe(statement))
                        .execute(&mut conn)
                        .await;
                }
            }
        });
    });
    let _ = dropped.join();
}

/// The [`SHARED`] database on `admin`, created on first use.
async fn shared(admin: &PgConnectOptions) -> sqlx::Result<PgConnectOptions> {
    let mut conn = PgConnection::connect_with(admin).await?;
    let exists: bool =
        sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM pg_database WHERE datname = $1)")
            .bind(SHARED)
            .fetch_one(&mut conn)
            .await?;
    if !exists {
        let created = sqlx::query(AssertSqlSafe(format!(
            "CREATE DATABASE {SHARED} {BYTE_ORDER}"
        )))
        .execute(&mut conn)
        .await;
        match created {
            // Another test created it first: `42P04`, or `23505` on the catalog
            // index when both creates ran at once.
            Err(sqlx::Error::Database(e))
                if matches!(e.code().as_deref(), Some("42P04" | "23505")) => {}
            created => {
                created?;
            }
        }
    }
    conn.close().await?;
    Ok(admin.clone().database(SHARED))
}

/// An empty schema in the [`SHARED`] database, dropped with this value. Its
/// connections use its name as `application_name`, so the drop can end them.
struct Schema {
    shared: PgConnectOptions,
    name: String,
}

impl Schema {
    async fn create(admin: &PgConnectOptions) -> sqlx::Result<Self> {
        let shared = shared(admin).await?;
        let name = scratch_name();
        let mut conn = PgConnection::connect_with(&shared).await?;
        sqlx::query(AssertSqlSafe(format!("CREATE SCHEMA {name}")))
            .execute(&mut conn)
            .await?;
        conn.close().await?;
        Ok(Self { shared, name })
    }

    fn connect(&self, max_connections: u32) -> SqlDatabase {
        let options = self
            .shared
            .clone()
            .application_name(&self.name)
            .options([("search_path", &self.name)]);
        SqlDatabase::from_options(options, max_connections)
    }
}

impl Drop for Schema {
    fn drop(&mut self) {
        let name = &self.name;
        run_on_drop(
            self.shared.clone(),
            vec![
                format!(
                    "SELECT pg_terminate_backend(pid) FROM pg_stat_activity \
                     WHERE application_name = '{name}'"
                ),
                format!("DROP SCHEMA IF EXISTS {name} CASCADE"),
            ],
        );
    }
}

/// A new, migrated schema, dropped with this value.
pub struct TestDb {
    db: SqlDatabase,
    _schema: Schema,
}

impl TestDb {
    /// `None` if no server exists to test against and the run is not in CI.
    pub async fn new() -> anyhow::Result<Option<Self>> {
        let Some(test_db) = Self::unmigrated().await? else {
            return Ok(None);
        };
        crate::migrate::migrate(&test_db.db, None, false).await?;
        Ok(Some(test_db))
    }

    /// A schema with no tables, for the tests of the migrator.
    pub async fn unmigrated() -> anyhow::Result<Option<Self>> {
        let Some(admin) = server()? else {
            return Ok(None);
        };
        let schema = Schema::create(&admin).await?;
        Ok(Some(Self {
            db: schema.connect(POOL_SIZE),
            _schema: schema,
        }))
    }

    pub fn db(&self) -> &SqlDatabase {
        &self.db
    }
}
