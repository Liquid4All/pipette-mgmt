//! The command `db migrate` moves the schema to a migration, up before a
//! rollout and down before a binary rolls back. The command `serve` checks the
//! schema and does not change it.

use std::collections::BTreeMap;

use anyhow::Context;
use sqlx::Row;
use sqlx::migrate::Migrator;

use crate::SchemaMove;
use crate::db::SqlDatabase;

fn migrator() -> Migrator {
    let mut migrator = sqlx::migrate!("./migrations");
    // A rolled-back binary finds the migrations of the newer build applied.
    // Those migrations only add to the schema, so the binary needs no action.
    migrator.set_ignore_missing(true);
    migrator
}

pub(crate) const RUN: &str = "run `pipette-mgmt db migrate` first";

/// The newest migration this binary embeds.
pub(crate) fn latest() -> Option<i64> {
    migrator().iter().map(|m| m.version).max()
}

/// Apply up to `to` and revert above it. Without `to`, revert nothing, so a
/// stale binary never moves the schema down. The count includes versions a
/// concurrent runner applied while this run waited for its lock.
pub(crate) async fn migrate(
    db: &SqlDatabase,
    to: Option<i64>,
    dry_run: bool,
) -> anyhow::Result<SchemaMove> {
    run(db, &migrator(), to, dry_run).await
}

async fn run(
    db: &SqlDatabase,
    migrator: &Migrator,
    to: Option<i64>,
    dry_run: bool,
) -> anyhow::Result<SchemaMove> {
    if let Some(to) = to {
        ensure_embedded(migrator, to)?;
    }
    ensure_byte_order(db).await?;
    let before = match applied(db).await {
        Ok(before) => before,
        // Absent on a fresh database, which has applied nothing.
        Err(e)
            if e.downcast_ref::<sqlx::Error>()
                .is_some_and(is_undefined_table) =>
        {
            BTreeMap::new()
        }
        Err(e) => return Err(e.context("reading the applied migrations")),
    };
    let above = to.map_or_else(BTreeMap::new, |to| {
        before
            .range(to + 1..)
            .map(|(v, a)| (*v, a.clone()))
            .collect()
    });
    ensure_revertible(migrator, &above)?;
    let after: Vec<i64> = if dry_run {
        let pending = migrator.iter().filter(|m| {
            m.migration_type.is_up_migration()
                && !before.contains_key(&m.version)
                && to.is_none_or(|to| m.version <= to)
        });
        let kept = before.keys().filter(|v| !above.contains_key(v)).copied();
        kept.chain(pending.map(|m| m.version)).collect()
    } else {
        let mut conn = db.pool().acquire().await?;
        let moved = async {
            if let (Some(to), false) = (to, above.is_empty()) {
                migrator
                    .undo(&mut *conn, to)
                    .await
                    .context("reverting migrations")?;
            }
            match to {
                Some(to) => migrator.run_to(to, &mut *conn).await,
                None => migrator.run(&mut *conn).await,
            }
            .context("running migrations")
        }
        .await;
        if moved.is_err() {
            // sqlx errors out without unlocking; closing the session frees the lock.
            let _ = conn.close().await;
        }
        moved?;
        applied(db).await?.into_keys().collect()
    };
    Ok(SchemaMove {
        from: before.keys().max().copied(),
        to: after.iter().max().copied(),
        applied: after
            .iter()
            .filter(|v| !before.contains_key(v))
            .copied()
            .collect(),
        reverted: before
            .keys()
            .rev()
            .filter(|v| !after.contains(v))
            .copied()
            .collect(),
    })
}

/// sqlx skips a version it has no down for, and reverts a changed one blindly.
/// A refusal reverts nothing, so the schema never stops partway down.
fn ensure_revertible(
    migrator: &Migrator,
    above: &BTreeMap<i64, (Vec<u8>, bool)>,
) -> anyhow::Result<()> {
    for (version, (checksum, _)) in above {
        let up = migrator
            .iter()
            .find(|m| m.version == *version && m.migration_type.is_up_migration());
        let has_down = migrator
            .iter()
            .any(|m| m.version == *version && m.migration_type.is_down_migration());
        match up {
            Some(up) if has_down && checksum.as_slice() == &*up.checksum => {}
            Some(_) if has_down => anyhow::bail!(
                "migration {version} was changed after it was applied: revert it by hand"
            ),
            Some(_) => anyhow::bail!("migration {version} has no down script"),
            None => anyhow::bail!(
                "migration {version} is not in this binary: move down with the binary that applied it"
            ),
        }
    }
    Ok(())
}

/// A target names an embedded migration, so a typo cannot pass every version.
fn ensure_embedded(migrator: &Migrator, to: i64) -> anyhow::Result<()> {
    let versions: Vec<String> = migrator
        .iter()
        .filter(|m| m.migration_type.is_up_migration())
        .map(|m| m.version.to_string())
        .collect();
    anyhow::ensure!(
        migrator.version_exists(to),
        "this binary has no migration {to} (it has {})",
        versions.join(", ")
    );
    Ok(())
}

/// Refuse a schema behind this build. A newer one passes: migrations only add,
/// and after a rollback an older binary runs on a newer schema.
pub(crate) async fn check(db: &SqlDatabase) -> anyhow::Result<()> {
    ensure_byte_order(db).await?;
    let applied = applied(db)
        .await
        .with_context(|| format!("reading the applied migrations: {RUN}"))?;
    migrator()
        .iter()
        .filter(|m| m.migration_type.is_up_migration())
        .try_for_each(|m| match applied.get(&m.version) {
            None => anyhow::bail!(
                "migration {} ({}) is not applied: {RUN}",
                m.version,
                m.description
            ),
            Some((_, false)) => anyhow::bail!("migration {} failed partway: {RUN}", m.version),
            Some((checksum, true)) if checksum.as_slice() != &*m.checksum => {
                anyhow::bail!("migration {} was changed after it was applied", m.version)
            }
            Some(_) => Ok(()),
        })
}

/// SQLSTATE `42P01`: the statement names a table that does not exist.
fn is_undefined_table(error: &sqlx::Error) -> bool {
    let sqlx::Error::Database(db) = error else {
        return false;
    };
    db.code().as_deref() == Some("42P01")
}

async fn applied(db: &SqlDatabase) -> anyhow::Result<BTreeMap<i64, (Vec<u8>, bool)>> {
    let rows = sqlx::query("SELECT version, checksum, success FROM _sqlx_migrations")
        .fetch_all(db.pool())
        .await?;
    rows.iter()
        .map(|row| Ok((row.try_get(0)?, (row.try_get(1)?, row.try_get(2)?))))
        .collect()
}

/// Listings rely on byte order to match the object stores; the builtin
/// provider sorts the same as `C`.
async fn ensure_byte_order(db: &SqlDatabase) -> anyhow::Result<()> {
    let row = sqlx::query(
        "SELECT datlocprovider::text, datcollate FROM pg_database WHERE datname = current_database()",
    )
    .fetch_one(db.pool())
    .await
    .context("reading the database collation")?;
    let provider: String = row.try_get(0)?;
    let collate: String = row.try_get(1)?;
    let byte_order = match provider.as_str() {
        "b" => true,
        "c" => matches!(collate.as_str(), "C" | "POSIX" | "C.UTF-8" | "C.utf8"),
        _ => false,
    };
    anyhow::ensure!(
        byte_order,
        "the [database] database sorts text by locale (provider {provider:?}, collation {collate:?}); \
         listings need byte order: create it with TEMPLATE template0 LC_COLLATE 'C' LC_CTYPE 'C'"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use sqlx::SqlSafeStr;
    use sqlx::migrate::{Migration, MigrationType, Migrator};

    use crate::SchemaMove;
    use crate::db::SqlDatabase;

    /// `db migrate --to` moves down only through down scripts. `0001` and
    /// `0002` predate them.
    #[test]
    fn every_migration_after_the_second_has_a_down() {
        let migrator = super::migrator();
        let missing: Vec<i64> = migrator
            .iter()
            .filter(|m| m.migration_type.is_up_migration() && m.version > 2)
            .filter(|up| {
                !migrator
                    .iter()
                    .any(|m| m.version == up.version && m.migration_type.is_down_migration())
            })
            .map(|m| m.version)
            .collect();
        assert!(missing.is_empty(), "add a .down.sql for {missing:?}");
    }

    fn step(version: i64, migration_type: MigrationType, sql: &'static str) -> Migration {
        Migration::new(
            version,
            "step".into(),
            migration_type,
            sql.into_sql_str(),
            false,
        )
    }

    /// Version 1 has no down script, as `0001_records` has none. Versions 2 and 3
    /// each create a table and drop it on the way down.
    fn three_steps() -> Vec<Migration> {
        vec![
            step(1, MigrationType::Simple, "CREATE TABLE one (id INT)"),
            step(2, MigrationType::ReversibleUp, "CREATE TABLE two (id INT)"),
            step(2, MigrationType::ReversibleDown, "DROP TABLE two"),
            step(
                3,
                MigrationType::ReversibleUp,
                "CREATE TABLE three (id INT)",
            ),
            step(3, MigrationType::ReversibleDown, "DROP TABLE three"),
        ]
    }

    async fn tables(db: &SqlDatabase) -> anyhow::Result<Vec<String>> {
        Ok(sqlx::query_scalar(
            "SELECT tablename::text FROM pg_tables WHERE schemaname = current_schema() \
             AND tablename IN ('one', 'two', 'three') ORDER BY tablename",
        )
        .fetch_all(db.pool())
        .await?)
    }

    fn err_text(result: anyhow::Result<SchemaMove>) -> String {
        result.err().map(|e| format!("{e:#}")).unwrap_or_default()
    }

    fn moved(from: Option<i64>, to: Option<i64>, applied: &[i64], reverted: &[i64]) -> SchemaMove {
        SchemaMove {
            from,
            to,
            applied: applied.to_vec(),
            reverted: reverted.to_vec(),
        }
    }

    #[tokio::test]
    async fn a_target_moves_the_schema_up_and_down() -> anyhow::Result<()> {
        let Some(test_db) = crate::scratch::TestDb::unmigrated().await? else {
            return Ok(());
        };
        let (db, migrator) = (test_db.db(), Migrator::with_migrations(three_steps()));
        let run = |to, dry_run| super::run(db, &migrator, to, dry_run);

        assert_eq!(
            run(Some(2), true).await?,
            moved(None, Some(2), &[1, 2], &[])
        );
        assert_eq!(tables(db).await?, Vec::<String>::new());
        assert_eq!(
            run(Some(2), false).await?,
            moved(None, Some(2), &[1, 2], &[])
        );
        assert_eq!(
            run(Some(2), false).await?,
            moved(Some(2), Some(2), &[], &[])
        );
        assert_eq!(run(None, false).await?, moved(Some(2), Some(3), &[3], &[]));

        assert_eq!(
            run(Some(1), true).await?,
            moved(Some(3), Some(1), &[], &[3, 2])
        );
        assert_eq!(tables(db).await?, ["one", "three", "two"]);
        assert_eq!(
            run(Some(1), false).await?,
            moved(Some(3), Some(1), &[], &[3, 2])
        );
        assert_eq!(tables(db).await?, ["one"]);
        assert_eq!(
            run(Some(1), false).await?,
            moved(Some(1), Some(1), &[], &[])
        );

        assert_eq!(
            run(Some(3), false).await?,
            moved(Some(1), Some(3), &[2, 3], &[])
        );
        assert_eq!(
            run(Some(2), false).await?,
            moved(Some(3), Some(2), &[], &[3])
        );
        assert_eq!(run(None, false).await?, moved(Some(2), Some(3), &[3], &[]));
        Ok(())
    }

    #[tokio::test]
    async fn a_target_must_be_a_migration_of_the_binary() -> anyhow::Result<()> {
        let Some(test_db) = crate::scratch::TestDb::unmigrated().await? else {
            return Ok(());
        };
        let (db, migrator) = (test_db.db(), Migrator::with_migrations(three_steps()));
        let err = err_text(super::run(db, &migrator, Some(20), false).await);
        assert!(err.contains("no migration 20 (it has 1, 2, 3)"), "{err}");
        super::run(db, &migrator, None, false).await?;
        let err = err_text(super::run(db, &migrator, Some(0), false).await);
        assert!(err.contains("no migration 0"), "{err}");
        assert_eq!(tables(db).await?, ["one", "three", "two"]);
        Ok(())
    }

    /// Each refusal reverts nothing.
    #[tokio::test]
    async fn a_move_down_refuses_a_version_it_cannot_revert() -> anyhow::Result<()> {
        let Some(test_db) = crate::scratch::TestDb::unmigrated().await? else {
            return Ok(());
        };
        let db = test_db.db();
        let mut four = three_steps();
        four.push(step(4, MigrationType::Simple, "CREATE TABLE four (id INT)"));
        super::run(db, &Migrator::with_migrations(four.clone()), None, false).await?;
        let down = |steps| async move {
            err_text(super::run(db, &Migrator::with_migrations(steps), Some(1), false).await)
        };

        let err = down(four).await;
        assert!(err.contains("migration 4 has no down script"), "{err}");

        // An older binary lacks migration 4.
        let err = down(three_steps()).await;
        assert!(err.contains("migration 4 is not in this binary"), "{err}");

        let mut changed = three_steps();
        changed.push(step(
            4,
            MigrationType::ReversibleUp,
            "CREATE TABLE four (id BIGINT)",
        ));
        changed.push(step(4, MigrationType::ReversibleDown, "DROP TABLE four"));
        let err = down(changed).await;
        assert!(err.contains("migration 4 was changed"), "{err}");

        assert_eq!(tables(db).await?, ["one", "three", "two"]);
        Ok(())
    }

    /// `db migrate` prints this number, so it must be the newest file in `migrations/`.
    #[test]
    fn latest_is_the_newest_migration_file() -> anyhow::Result<()> {
        let newest = std::fs::read_dir(concat!(env!("CARGO_MANIFEST_DIR"), "/migrations"))?
            .filter_map(|entry| {
                entry
                    .ok()?
                    .file_name()
                    .to_str()?
                    .split_once('_')?
                    .0
                    .parse::<i64>()
                    .ok()
            })
            .max();
        assert!(newest.is_some());
        assert_eq!(super::latest(), newest);
        Ok(())
    }
}
