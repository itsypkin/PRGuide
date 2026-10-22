//! Store schema migrations.

use rusqlite::{Connection, TransactionBehavior};

use super::{StoreError, schema};

pub(crate) const MIGRATIONS: &[&str] = &[schema::V1];

pub(crate) fn migrate(conn: &mut Connection, migrations: &[&str]) -> Result<(), StoreError> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let found: i64 = tx.pragma_query_value(None, "user_version", |row| row.get(0))?;
    let Some(found_version) = usize::try_from(found).ok() else {
        return Err(StoreError::StoreTooNew {
            found,
            known: migrations.len(),
        });
    };
    if found_version > migrations.len() {
        return Err(StoreError::StoreTooNew {
            found,
            known: migrations.len(),
        });
    }

    for (index, migration) in migrations.iter().enumerate().skip(found_version) {
        tx.execute_batch(migration)?;
        let new_version = i64::try_from(index + 1).map_err(|error| {
            StoreError::Sqlite(rusqlite::Error::ToSqlConversionFailure(Box::new(error)))
        })?;
        tx.pragma_update(None, "user_version", new_version)?;
    }
    tx.commit()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use rusqlite::Connection;

    use super::{MIGRATIONS, migrate};
    use crate::store::{StoreError, schema};

    #[test]
    fn applies_pending_migrations_and_partial_upgrade() -> Result<(), StoreError> {
        let mut conn = Connection::open_in_memory()?;
        let first = ["CREATE TABLE a(x);"];
        migrate(&mut conn, &first)?;
        migrate(&mut conn, &["CREATE TABLE a(x);", "CREATE TABLE b(x);"])?;
        let version: i64 = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
        assert_eq!(version, 2);
        for table in ["a", "b"] {
            assert_eq!(
                conn.query_row(
                    "SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
                    [table],
                    |row| row.get::<_, i64>(0),
                )?,
                1
            );
        }
        Ok(())
    }

    #[test]
    fn migration_is_atomic() -> Result<(), StoreError> {
        let mut conn = Connection::open_in_memory()?;
        let error = migrate(&mut conn, &["CREATE TABLE a(x);", "NOT SQL"])
            .expect_err("invalid migration fails");
        assert_eq!(error.code(), "store_sqlite");
        let version: i64 = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
        assert_eq!(version, 0);
        assert!(conn.query_row("SELECT 1 FROM a", [], |_| Ok(())).is_err());
        Ok(())
    }

    #[test]
    fn rejects_too_new_versions_without_changes() -> Result<(), StoreError> {
        let mut conn = Connection::open_in_memory()?;
        conn.execute_batch("CREATE TABLE unchanged(x); PRAGMA user_version = 3;")?;
        let error = migrate(&mut conn, &["SELECT 1", "SELECT 1"]).expect_err("version is too new");
        assert!(matches!(
            error,
            StoreError::StoreTooNew { found: 3, known: 2 }
        ));
        assert_eq!(error.code(), "store_too_new");
        assert_eq!(
            conn.pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0))?,
            3
        );
        assert_eq!(
            conn.query_row(
                "SELECT count(*) FROM sqlite_master WHERE name = 'unchanged'",
                [],
                |row| row.get::<_, i64>(0)
            )?,
            1
        );
        conn.pragma_update(None, "user_version", -1)?;
        assert!(matches!(
            migrate(&mut conn, &["SELECT 1", "SELECT 1"]),
            Err(StoreError::StoreTooNew {
                found: -1,
                known: 2
            })
        ));
        assert_eq!(
            conn.pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0))?,
            -1
        );
        assert_eq!(
            conn.query_row(
                "SELECT count(*) FROM sqlite_master WHERE name = 'unchanged'",
                [],
                |row| row.get::<_, i64>(0)
            )?,
            1
        );
        Ok(())
    }

    #[test]
    fn v1_is_the_only_first_migration() {
        assert_eq!(MIGRATIONS.len(), 1);
        assert_eq!(MIGRATIONS[0], schema::V1);
    }
}
