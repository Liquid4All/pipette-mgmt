//! At `REPEATABLE READ`, a transaction that conflicts with one committed first
//! fails with SQLSTATE `40001`; run again, the loser sees the winner's write.

use std::time::Duration;

use futures::future::BoxFuture;

use crate::migrate::RUN;

/// Each attempt reads again, so a loser sees the row of the winner. Only
/// sustained contention on one row uses all the attempts.
const ATTEMPTS: u32 = 8;

const BASE_DELAY: Duration = Duration::from_millis(5);
const MAX_DELAY: Duration = Duration::from_millis(500);

pub(crate) fn is_conflict(error: &sqlx::Error) -> bool {
    let sqlx::Error::Database(db) = error else {
        return false;
    };
    db.code().as_deref() == Some("40001")
}

/// The statement names a table (`42P01`) or a column (`42703`) that the
/// database lacks. The schema is older than the migrations of this binary.
fn is_schema_behind(error: &sqlx::Error) -> bool {
    let sqlx::Error::Database(db) = error else {
        return false;
    };
    matches!(db.code().as_deref(), Some("42P01" | "42703"))
}

/// The failure of a [`retry`]. The message names the operation, and the source
/// is the database error of the last attempt.
#[derive(Debug, thiserror::Error)]
pub(crate) enum RetryError {
    /// Each attempt lost a write conflict.
    #[error("{what}: still conflicting after {attempts} attempts")]
    Exhausted {
        what: &'static str,
        attempts: u32,
        #[source]
        source: sqlx::Error,
    },
    /// The statement names a table or a column that the database lacks.
    #[error("{what}: the database schema is older than this binary: {RUN}")]
    SchemaBehind {
        what: &'static str,
        #[source]
        source: sqlx::Error,
    },
    /// The attempt failed for another reason.
    #[error("{what}")]
    Failed {
        what: &'static str,
        #[source]
        source: sqlx::Error,
    },
}

impl RetryError {
    /// The database error of the last attempt.
    pub(crate) fn sqlx(&self) -> &sqlx::Error {
        match self {
            Self::Exhausted { source, .. }
            | Self::SchemaBehind { source, .. }
            | Self::Failed { source, .. } => source,
        }
    }
}

/// `op` must be the whole transaction, `BEGIN` to `COMMIT`, so a retry replays
/// its reads. A blob or queue write inside it would repeat with each attempt.
pub(crate) async fn retry<'a, T>(
    what: &'static str,
    mut op: impl FnMut() -> BoxFuture<'a, Result<T, sqlx::Error>> + Send,
) -> anyhow::Result<T> {
    let mut attempt = 0;
    loop {
        attempt += 1;
        match op().await {
            Ok(value) => return Ok(value),
            Err(e) if is_conflict(&e) && attempt < ATTEMPTS => {
                tracing::debug!(what, attempt, error = %e, "retrying after a write conflict");
                tokio::time::sleep(backoff(attempt)).await;
            }
            Err(source) => {
                let error = if is_conflict(&source) {
                    RetryError::Exhausted {
                        what,
                        attempts: attempt,
                        source,
                    }
                } else if is_schema_behind(&source) {
                    RetryError::SchemaBehind { what, source }
                } else {
                    RetryError::Failed { what, source }
                };
                return Err(error.into());
            }
        }
    }
}

/// Up to half is jitter, so two losers do not collide again on the next attempt.
fn backoff(attempt: u32) -> Duration {
    let exp = BASE_DELAY.saturating_mul(1 << attempt.min(16));
    let capped = exp.min(MAX_DELAY);
    let (random, _) = uuid::Uuid::new_v4().as_u64_pair();
    let half = capped / 2;
    let jitter_ms = u64::try_from(half.as_millis()).unwrap_or(0);
    half + Duration::from_millis(random.checked_rem(jitter_ms.max(1)).unwrap_or(0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_stays_under_the_cap() {
        for attempt in 1..=ATTEMPTS {
            assert!(backoff(attempt) <= MAX_DELAY);
        }
    }

    /// A statement against a schema that lacks its table or column names the
    /// fix. Any other failure does not name a fix.
    #[tokio::test]
    async fn a_schema_behind_the_binary_names_db_migrate() -> anyhow::Result<()> {
        let test_db = crate::testing::test_db_or_skip!();
        let cases = [
            ("SELECT 1 FROM no_such_table", true),
            ("SELECT no_such_column FROM campaigns", true),
            ("SELECT 1 / 0", false),
        ];
        for (sql, names_migrate) in cases {
            let db = test_db.db();
            let failed = retry("probe", || {
                Box::pin(async move { sqlx::query(sql).execute(db.pool()).await })
            })
            .await;
            let Err(e) = failed else {
                anyhow::bail!("{sql} succeeded");
            };
            assert_eq!(
                format!("{e:#}").contains("pipette-mgmt db migrate"),
                names_migrate,
                "{sql}: {e:#}"
            );
        }
        Ok(())
    }
}
