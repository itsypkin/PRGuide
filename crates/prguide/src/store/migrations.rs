//! Store schema migrations.

use rusqlite::{Connection, TransactionBehavior};

use super::{StoreError, schema};

pub(crate) const MIGRATIONS: &[&str] = &[schema::V1];

/// Reads `user_version` and rejects a store newer than this build knows.
fn checked_version(conn: &Connection, migrations: &[&str]) -> Result<usize, StoreError> {
    let found: i64 = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
    match usize::try_from(found) {
        Ok(version) if version <= migrations.len() => Ok(version),
        _ => Err(StoreError::StoreTooNew {
            found,
            known: migrations.len(),
        }),
    }
}

pub(crate) fn migrate(conn: &mut Connection, migrations: &[&str]) -> Result<(), StoreError> {
    // Fast path: no transaction, so no write lock when the schema is current.
    if checked_version(conn, migrations)? == migrations.len() {
        return Ok(());
    }
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    // Re-read under the lock: another process may have migrated meanwhile.
    let found_version = checked_version(&tx, migrations)?;

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

    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, Instant};

    use crate::store::{PrRef, Store};

    fn file_conn(dir: &std::path::Path) -> Connection {
        let conn = Connection::open(dir.join("test.db")).expect("open");
        conn.busy_timeout(Duration::ZERO).expect("timeout");
        let mode: String = conn
            .pragma_update_and_check(None, "journal_mode", "WAL", |r| r.get(0))
            .expect("wal");
        assert!(mode.eq_ignore_ascii_case("wal"));
        conn
    }

    fn version(conn: &Connection) -> i64 {
        conn.pragma_query_value(None, "user_version", |r| r.get(0))
            .expect("version")
    }

    fn wait_for(flag: &AtomicBool, name: &str) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !flag.load(Ordering::SeqCst) {
            assert!(Instant::now() < deadline, "busy handler {name} never fired");
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    #[test]
    fn a_current_store_opens_while_another_connection_holds_the_write_lock() {
        let home = tempfile::tempdir().expect("home");
        let pr = PrRef::new("octo", "repo", 1).expect("pr");
        let store = Store::open_in(home.path(), &pr).expect("create");
        let holder = Connection::open(store.dir().join("store.db")).expect("holder");
        holder.execute_batch("BEGIN IMMEDIATE").expect("begin");
        let second = Store::open_in(home.path(), &pr);
        assert!(second.is_ok());
        holder.execute_batch("ROLLBACK").expect("rollback");
    }

    #[test]
    fn migrate_on_a_current_store_takes_no_write_lock() {
        let dir = tempfile::tempdir().expect("dir");
        let list = ["CREATE TABLE a(x);"];
        let mut first = file_conn(dir.path());
        migrate(&mut first, &list).expect("first");
        let holder = file_conn(dir.path());
        holder.execute_batch("BEGIN IMMEDIATE").expect("begin");
        let mut third = file_conn(dir.path());
        migrate(&mut third, &list).expect("current");
        assert_eq!(version(&third), 1);
        let tables: i64 = third
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name = 'a'",
                [],
                |r| r.get(0),
            )
            .expect("count");
        assert_eq!(tables, 1);
        holder.execute_batch("ROLLBACK").expect("rollback");
    }

    #[test]
    fn a_too_new_store_is_rejected_without_the_write_lock() {
        for found in [3_i64, -1] {
            let dir = tempfile::tempdir().expect("dir");
            let setup = file_conn(dir.path());
            setup
                .pragma_update(None, "user_version", found)
                .expect("set");
            let holder = file_conn(dir.path());
            holder.execute_batch("BEGIN IMMEDIATE").expect("begin");
            let mut conn = file_conn(dir.path());
            let error = migrate(&mut conn, &["SELECT 1", "SELECT 1"]).expect_err("too new");
            assert!(matches!(error, StoreError::StoreTooNew { known: 2, .. }));
            assert!(matches!(error, StoreError::StoreTooNew { found: f, .. } if f == found));
            assert_eq!(error.code(), "store_too_new");
            assert_eq!(
                error.to_string(),
                format!(
                    "store schema version {found} is newer than this prguide supports (2); upgrade prguide"
                )
            );
            holder.execute_batch("ROLLBACK").expect("rollback");
            assert_eq!(version(&setup), found);
        }
    }

    fn log_store(dir: &std::path::Path) -> Connection {
        let conn = file_conn(dir);
        conn.execute_batch("CREATE TABLE log(x);").expect("log");
        conn
    }

    fn log_count(conn: &Connection) -> i64 {
        conn.query_row("SELECT count(*) FROM log", [], |r| r.get(0))
            .expect("count")
    }

    static FLAG_N4_A: AtomicBool = AtomicBool::new(false);
    static FLAG_N4_B: AtomicBool = AtomicBool::new(false);
    static FLAG_N5: AtomicBool = AtomicBool::new(false);

    fn handler_n4_a(count: i32) -> bool {
        FLAG_N4_A.store(true, Ordering::SeqCst);
        std::thread::sleep(Duration::from_millis(1));
        count < 10_000
    }
    fn handler_n4_b(count: i32) -> bool {
        FLAG_N4_B.store(true, Ordering::SeqCst);
        std::thread::sleep(Duration::from_millis(1));
        count < 10_000
    }
    fn handler_n5(count: i32) -> bool {
        FLAG_N5.store(true, Ordering::SeqCst);
        std::thread::sleep(Duration::from_millis(1));
        count < 10_000
    }

    #[test]
    fn two_concurrent_opens_of_an_old_store_migrate_once() {
        let dir = tempfile::tempdir().expect("dir");
        let setup = log_store(dir.path());
        let holder = file_conn(dir.path());
        holder.execute_batch("BEGIN IMMEDIATE").expect("begin");
        let list = ["INSERT INTO log VALUES (1);"];
        let spawn = |handler: fn(i32) -> bool| {
            let path = dir.path().to_path_buf();
            std::thread::spawn(move || {
                let mut conn = file_conn(&path);
                conn.busy_handler(Some(handler)).expect("handler");
                migrate(&mut conn, &list)
            })
        };
        let b = spawn(handler_n4_b);
        wait_for(&FLAG_N4_B, "B");
        let a = spawn(handler_n4_a);
        wait_for(&FLAG_N4_A, "A");
        holder.execute_batch("ROLLBACK").expect("rollback");
        b.join().expect("b").expect("b ok");
        a.join().expect("a").expect("a ok");
        assert_eq!(log_count(&setup), 1);
        assert_eq!(version(&setup), 1);
    }

    #[test]
    fn the_recheck_under_the_lock_rejects_a_store_made_too_new() {
        let dir = tempfile::tempdir().expect("dir");
        let setup = log_store(dir.path());
        let holder = file_conn(dir.path());
        holder.execute_batch("BEGIN IMMEDIATE").expect("begin");
        let path = dir.path().to_path_buf();
        let b = std::thread::spawn(move || {
            let mut conn = file_conn(&path);
            conn.busy_handler(Some(handler_n5)).expect("handler");
            migrate(&mut conn, &["INSERT INTO log VALUES (1);"])
        });
        wait_for(&FLAG_N5, "N5");
        holder
            .execute_batch("PRAGMA user_version = 2; COMMIT")
            .expect("bump");
        let error = b.join().expect("b").expect_err("too new");
        assert!(matches!(
            error,
            StoreError::StoreTooNew { found: 2, known: 1 }
        ));
        assert_eq!(error.code(), "store_too_new");
        assert_eq!(
            error.to_string(),
            "store schema version 2 is newer than this prguide supports (1); upgrade prguide"
        );
        assert_eq!(log_count(&setup), 0);
    }

    #[test]
    fn v1_is_the_only_first_migration() {
        assert_eq!(MIGRATIONS.len(), 1);
        assert_eq!(MIGRATIONS[0], schema::V1);
    }
}
