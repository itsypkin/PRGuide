//! Store schema migrations.

use rusqlite::{Connection, TransactionBehavior};

use super::{StoreError, schema};

pub(crate) const MIGRATIONS: &[&str] = &[schema::V1];

pub(crate) fn migrate(conn: &mut Connection, migrations: &[&str]) -> Result<(), StoreError> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let found: i64 = tx.pragma_query_value(None, "user_version", |row| row.get(0))?;
    let found_version = usize::try_from(found).ok();
    if found_version.is_none_or(|version| version > migrations.len()) {
        return Err(StoreError::StoreTooNew {
            found,
            known: migrations.len(),
        });
    }
    let found_version = found_version.ok_or(StoreError::StoreTooNew {
        found,
        known: migrations.len(),
    })?;

    for (index, migration) in migrations.iter().enumerate().skip(found_version) {
        tx.execute_batch(migration)?;
        let new_version = i64::try_from(index + 1).map_err(|_| StoreError::StoreTooNew {
            found: i64::MAX,
            known: migrations.len(),
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
        Ok(())
    }

    #[test]
    fn migration_is_atomic() -> Result<(), StoreError> {
        let mut conn = Connection::open_in_memory()?;
        assert!(migrate(&mut conn, &["CREATE TABLE a(x);", "NOT SQL"]).is_err());
        let version: i64 = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
        assert_eq!(version, 0);
        assert!(conn.query_row("SELECT 1 FROM a", [], |_| Ok(())).is_err());
        Ok(())
    }

    #[test]
    fn rejects_too_new_versions() -> Result<(), StoreError> {
        let mut conn = Connection::open_in_memory()?;
        conn.pragma_update(None, "user_version", 3)?;
        assert!(matches!(
            migrate(&mut conn, &["SELECT 1", "SELECT 1"]),
            Err(StoreError::StoreTooNew { found: 3, known: 2 })
        ));
        conn.pragma_update(None, "user_version", -1)?;
        assert!(matches!(
            migrate(&mut conn, &["SELECT 1", "SELECT 1"]),
            Err(StoreError::StoreTooNew {
                found: -1,
                known: 2
            })
        ));
        Ok(())
    }

    #[test]
    fn v1_is_the_only_first_migration() {
        assert_eq!(MIGRATIONS, &[schema::V1]);
    }
}
