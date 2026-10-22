use std::{
    ffi::OsStr,
    sync::{Arc, Barrier},
    thread,
};

use prguide::store::{PrRef, Store, StoreError, resolve_home};
use rusqlite::{Connection, ErrorCode};
use tempfile::tempdir;

fn pr() -> PrRef {
    PrRef::new("octo", "repo", 42).expect("valid PR")
}
fn check_error(error: StoreError, code: i32) {
    match error {
        StoreError::Sqlite(rusqlite::Error::SqliteFailure(error, _)) => {
            assert_eq!(error.extended_code, code)
        }
        other => panic!("expected SQLite failure, got {other:?}"),
    }
}

#[test]
fn fresh_store_reaches_version_1() -> Result<(), StoreError> {
    let temp = tempdir().expect("tempdir");
    let mut store = Store::open_in(temp.path(), &pr())?;
    assert_eq!(store.dir(), temp.path().join("prs/github.com/octo/repo/42"));
    let (version, objects): (i64, Vec<(String, String)>) = store.tx(|tx| {
        let version = tx.pragma_query_value(None, "user_version", |row| row.get(0))?;
        let objects = tx.prepare("SELECT type, name FROM sqlite_master WHERE name NOT LIKE 'sqlite_autoindex_%' ORDER BY type, name")?.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?.collect::<Result<_, _>>()?;
        Ok::<_, StoreError>((version, objects))
    })?;
    assert_eq!(version, 1);
    assert_eq!(objects.len(), 17);
    assert!(objects.contains(&("table".to_owned(), "threads".to_owned())));
    assert!(objects.contains(&("index".to_owned(), "one_open_post".to_owned())));
    Ok(())
}

#[test]
fn connection_pragmas_and_tx_immediate() -> Result<(), StoreError> {
    let temp = tempdir().expect("tempdir");
    let mut store = Store::open_in(temp.path(), &pr())?;
    let db = store.dir().join("store.db");
    store.tx(|tx| {
        let journal: String = tx.pragma_query_value(None, "journal_mode", |row| row.get(0))?;
        let timeout: i64 = tx.pragma_query_value(None, "busy_timeout", |row| row.get(0))?;
        let foreign_keys: i64 = tx.pragma_query_value(None, "foreign_keys", |row| row.get(0))?;
        assert_eq!(journal.to_ascii_lowercase(), "wal");
        assert_eq!(timeout, 5000);
        assert_eq!(foreign_keys, 1);
        tx.query_row("SELECT 1", [], |_| Ok(()))?;
        let other = Connection::open(db)?;
        other.busy_timeout(std::time::Duration::ZERO)?;
        let error = other
            .execute_batch("BEGIN IMMEDIATE")
            .expect_err("write lock is held");
        assert_eq!(error.sqlite_error_code(), Some(ErrorCode::DatabaseBusy));
        Ok::<_, StoreError>(())
    })
}

#[test]
fn tx_commits_rolls_back_and_data_version_tracks_other_connection() -> Result<(), StoreError> {
    let temp = tempdir().expect("tempdir");
    let mut a = Store::open_in(temp.path(), &pr())?;
    let mut b = Store::open_in(temp.path(), &pr())?;
    a.tx(|tx| {
        tx.execute("INSERT INTO store_meta VALUES ('committed', 'yes')", [])?;
        Ok::<_, StoreError>(())
    })?;
    let rollback = a.tx(|tx| {
        tx.execute("INSERT INTO store_meta VALUES ('rolled', 'no')", [])?;
        Err::<(), _>(StoreError::HomeUnresolved {
            reason: "test".to_owned(),
        })
    });
    assert!(matches!(rollback, Err(StoreError::HomeUnresolved { .. })));
    let v0 = a.data_version()?;
    b.tx(|tx| {
        tx.execute("INSERT INTO store_meta VALUES ('other', 'yes')", [])?;
        Ok::<_, StoreError>(())
    })?;
    let v1 = a.data_version()?;
    assert_ne!(v0, v1);
    a.tx(|tx| {
        tx.execute("INSERT INTO store_meta VALUES ('self', 'yes')", [])?;
        Ok::<_, StoreError>(())
    })?;
    assert_eq!(a.data_version()?, v1);
    let count = a.tx(|tx| {
        tx.query_row(
            "SELECT count(*) FROM store_meta WHERE key = 'rolled'",
            [],
            |r| r.get::<_, i64>(0),
        )
        .map_err(StoreError::from)
    })?;
    assert_eq!(count, 0);
    Ok(())
}

#[test]
fn constraints_and_foreign_keys_are_enforced() -> Result<(), StoreError> {
    let temp = tempdir().expect("tempdir");
    let mut store = Store::open_in(temp.path(), &pr())?;
    store.tx(|tx| {
        tx.execute("INSERT INTO runs VALUES ('r1','active','t','t',NULL,NULL)", [])?;
        check_error(StoreError::from(tx.execute("INSERT INTO runs VALUES ('r2','active','t','t',NULL,NULL)", []).expect_err("unique")), rusqlite::ffi::SQLITE_CONSTRAINT_UNIQUE);
        check_error(StoreError::from(tx.execute("INSERT INTO runs VALUES ('r3','stopped','t','t','t','crash')", []).expect_err("check")), rusqlite::ffi::SQLITE_CONSTRAINT_CHECK);
        tx.execute(
            "INSERT INTO posts VALUES ('p0','done','h','{}',NULL,NULL,'t','t')",
            [],
        )?;
        tx.execute(
            "INSERT INTO posts VALUES ('p1','in_flight','h','{}',NULL,NULL,'t','t')",
            [],
        )?;
        check_error(
            StoreError::from(
                tx.execute(
                    "INSERT INTO posts VALUES ('p2','unknown','h','{}',NULL,NULL,'t','t')",
                    [],
                )
                .expect_err("partial unique"),
            ),
            rusqlite::ffi::SQLITE_CONSTRAINT_UNIQUE,
        );
        check_error(StoreError::from(tx.execute("INSERT INTO guide_versions VALUES (1,'ready','missing','a','b','full','a','b','[]','[]','t','t')", []).expect_err("foreign key")), rusqlite::ffi::SQLITE_CONSTRAINT_FOREIGNKEY);
        Ok::<_, StoreError>(())
    })
}

#[test]
fn paths_and_errors_are_stable() -> Result<(), StoreError> {
    for bad in ["", ".", "..", "a/b", "a..b", "/", "a\0b"] {
        assert_eq!(
            PrRef::new(bad, "repo", 1).expect_err("bad owner").code(),
            "invalid_repo"
        );
        assert_eq!(
            PrRef::new("owner", bad, 1).expect_err("bad repo").code(),
            "invalid_repo"
        );
    }
    let mixed = PrRef::new("Octo", "RePo", 42)?;
    let temp = tempdir().expect("tempdir");
    let store = Store::open_in(temp.path(), &mixed)?;
    assert_eq!(store.dir(), temp.path().join("prs/github.com/octo/repo/42"));
    assert_eq!(mixed.owner(), "Octo");
    assert_eq!(
        resolve_home(Some(OsStr::new("/x")), Some(OsStr::new("/h")))?,
        std::path::PathBuf::from("/x")
    );
    for values in [(None, None), (Some("rel"), Some("/h")), (None, Some("rel"))] {
        assert_eq!(
            resolve_home(values.0.map(OsStr::new), values.1.map(OsStr::new))
                .expect_err("invalid home")
                .code(),
            "home_unresolved"
        );
    }
    let error = PrRef::new("a\0b", "r", 1)
        .expect_err("bad name")
        .to_string();
    assert!(error.contains("a\\0b") && !error.contains('\0'));
    Ok(())
}

#[test]
fn too_new_and_store_io_fail_without_migration() -> Result<(), StoreError> {
    let temp = tempdir().expect("tempdir");
    let dir = temp.path().join("prs/github.com/octo/repo/42");
    std::fs::create_dir_all(&dir).expect("directory");
    let conn = Connection::open(dir.join("store.db")).expect("database");
    conn.pragma_update(None, "user_version", 2)?;
    drop(conn);
    assert!(matches!(
        Store::open_in(temp.path(), &pr()),
        Err(StoreError::StoreTooNew { found: 2, known: 1 })
    ));
    let file = tempfile::NamedTempFile::new().expect("file");
    let error = match Store::open_in(file.path(), &pr()) {
        Ok(_) => panic!("file is not a home"),
        Err(error) => error,
    };
    assert_eq!(error.code(), "store_io");
    Ok(())
}

#[test]
fn concurrent_fresh_opens_succeed() {
    for _ in 0..50 {
        let temp = tempdir().expect("tempdir");
        let home = Arc::new(temp.path().to_owned());
        let barrier = Arc::new(Barrier::new(4));
        let pr = pr();
        let handles: Vec<_> = (0..4)
            .map(|_| {
                let home = Arc::clone(&home);
                let barrier = Arc::clone(&barrier);
                let pr = pr.clone();
                thread::spawn(move || {
                    barrier.wait();
                    Store::open_in(&home, &pr)
                })
            })
            .collect();
        for handle in handles {
            assert!(handle.join().expect("thread").is_ok());
        }
    }
}
