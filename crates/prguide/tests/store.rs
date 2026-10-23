use std::{
    ffi::OsStr,
    fs,
    path::Path,
    sync::{Arc, Barrier},
    thread,
};

use prguide::store::Clock;
use prguide::store::{
    FakeClock, PrRef, Store, StoreError, SystemClock, Timestamp, resolve_home,
    runs::{self, EndReason, RunId, RunState, StopReason},
};
use rusqlite::{Connection, ErrorCode};
use tempfile::tempdir;

fn pr() -> PrRef {
    PrRef::new("octo", "repo", 42).expect("valid PR")
}

fn sqlite_error(error: rusqlite::Error, code: i32) {
    match error {
        rusqlite::Error::SqliteFailure(error, _) => assert_eq!(error.extended_code, code),
        other => panic!("expected SQLite failure, got {other:?}"),
    }
}

fn objects(store: &mut Store) -> Result<Vec<(String, String)>, StoreError> {
    store.tx(|tx| {
        tx.prepare(
            "SELECT type, name FROM sqlite_master \
             WHERE name NOT LIKE 'sqlite_autoindex_%' ORDER BY type, name",
        )?
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(StoreError::from)
    })
}

fn expected_objects() -> Vec<(String, String)> {
    vec![
        ("index".into(), "events_open".into()),
        ("index".into(), "one_active_run".into()),
        ("index".into(), "one_open_post".into()),
        ("table".into(), "deepdive_sections".into()),
        ("table".into(), "deepdives".into()),
        ("table".into(), "drafts".into()),
        ("table".into(), "events".into()),
        ("table".into(), "gh_threads".into()),
        ("table".into(), "guide_versions".into()),
        ("table".into(), "messages".into()),
        ("table".into(), "notices".into()),
        ("table".into(), "outbox_items".into()),
        ("table".into(), "posts".into()),
        ("table".into(), "runs".into()),
        ("table".into(), "sqlite_sequence".into()),
        ("table".into(), "store_meta".into()),
        ("table".into(), "threads".into()),
    ]
}

fn insert_version(
    tx: &rusqlite::Transaction<'_>,
    version: i64,
    caused_by: Option<&str>,
) -> rusqlite::Result<()> {
    tx.execute(
        "INSERT INTO guide_versions VALUES (?1,'ready',?2,'a','b','full','a','b','[]','[]','t','t')",
        (version, caused_by),
    )?;
    Ok(())
}

fn insert_thread(tx: &rusqlite::Transaction<'_>, id: &str, version: i64) -> rusqlite::Result<()> {
    tx.execute(
        "INSERT INTO threads VALUES (?1,'t',?2,'slide',NULL)",
        (id, version),
    )?;
    Ok(())
}

#[test]
fn fresh_store_reaches_version_1_with_exact_objects() -> Result<(), StoreError> {
    let temp = tempdir().expect("tempdir");
    let mut store = Store::open_in(temp.path(), &pr())?;
    let dir = temp.path().join("prs/github.com/octo/repo/42");
    assert_eq!(store.dir(), dir);
    assert!(dir.join("store.db").is_file());
    let version: i64 = store.tx(|tx| {
        tx.pragma_query_value(None, "user_version", |row| row.get(0))
            .map_err(StoreError::from)
    })?;
    assert_eq!(version, 1);
    assert_eq!(objects(&mut store)?, expected_objects());
    Ok(())
}

#[test]
fn connection_pragmas_and_tx_is_immediate() -> Result<(), StoreError> {
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
fn reopen_is_a_no_op() -> Result<(), StoreError> {
    let temp = tempdir().expect("tempdir");
    let before = {
        let mut store = Store::open_in(temp.path(), &pr())?;
        store.tx(|tx| {
            tx.execute("INSERT INTO store_meta VALUES ('key','value')", [])
                .map_err(StoreError::from)
        })?;
        store.tx(|tx| {
            tx.prepare("SELECT type, name, sql FROM sqlite_master ORDER BY name")?
                .query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, Option<String>>(2)?,
                    ))
                })?
                .collect::<Result<Vec<_>, _>>()
                .map_err(StoreError::from)
        })?
    };
    let mut reopened = Store::open_in(temp.path(), &pr())?;
    let version: i64 = reopened.tx(|tx| {
        tx.pragma_query_value(None, "user_version", |row| row.get(0))
            .map_err(StoreError::from)
    })?;
    let value: String = reopened.tx(|tx| {
        tx.query_row(
            "SELECT value FROM store_meta WHERE key = 'key'",
            [],
            |row| row.get(0),
        )
        .map_err(StoreError::from)
    })?;
    let after = reopened.tx(|tx| {
        tx.prepare("SELECT type, name, sql FROM sqlite_master ORDER BY name")?
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()
            .map_err(StoreError::from)
    })?;
    assert_eq!(version, 1);
    assert_eq!(value, "value");
    assert_eq!(before, after);
    Ok(())
}

#[test]
fn store_too_new_keeps_the_database_unmigrated() -> Result<(), StoreError> {
    let temp = tempdir().expect("tempdir");
    let dir = temp.path().join("prs/github.com/octo/repo/42");
    fs::create_dir_all(&dir).expect("directory");
    let conn = Connection::open(dir.join("store.db")).expect("database");
    conn.pragma_update(None, "user_version", 2)?; // Migration 1 is currently the only migration.
    drop(conn);
    let error = match Store::open_in(temp.path(), &pr()) {
        Ok(_) => panic!("version 2 is too new"),
        Err(error) => error,
    };
    assert_eq!(error.code(), "store_too_new");
    let conn = Connection::open(dir.join("store.db"))?;
    assert_eq!(
        conn.pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0))?,
        2
    );
    assert_eq!(
        conn.query_row(
            "SELECT count(*) FROM sqlite_master WHERE name = 'store_meta'",
            [],
            |row| row.get::<_, i64>(0)
        )?,
        0
    );
    Ok(())
}

#[test]
fn every_check_rejects_bad_values_and_accepts_valid_values() -> Result<(), StoreError> {
    let temp = tempdir().expect("tempdir");
    let mut store = Store::open_in(temp.path(), &pr())?;
    store.tx(|tx| {
        tx.execute("INSERT INTO runs VALUES ('run-ok','active','t','t',NULL,NULL)", [])?;
        sqlite_error(tx.execute("INSERT INTO runs VALUES ('run-bad','no','t','t',NULL,NULL)", []).expect_err("runs state"), rusqlite::ffi::SQLITE_CONSTRAINT_CHECK);
        for (id, reason) in [("null", None), ("quit", Some("quit")), ("end", Some("end")), ("takeover", Some("takeover")), ("expired", Some("expired"))] {
            tx.execute("INSERT INTO runs VALUES (?1,'stopped','t','t','t',?2)", (id, reason))?;
        }
        sqlite_error(tx.execute("INSERT INTO runs VALUES ('reason-bad','stopped','t','t','t','crash')", []).expect_err("end reason"), rusqlite::ffi::SQLITE_CONSTRAINT_CHECK);
        insert_version(tx, 1, None)?;
        sqlite_error(tx.execute("INSERT INTO guide_versions VALUES (2,'bad',NULL,'a','b','full','a','b','[]','[]','t','t')", []).expect_err("version state"), rusqlite::ffi::SQLITE_CONSTRAINT_CHECK);
        sqlite_error(tx.execute("INSERT INTO guide_versions VALUES (3,'ready',NULL,'a','b','bad','a','b','[]','[]','t','t')", []).expect_err("selection kind"), rusqlite::ffi::SQLITE_CONSTRAINT_CHECK);
        insert_thread(tx, "thread", 1)?;
        tx.execute("INSERT INTO messages VALUES ('message-ok','thread','reviewer','b','t')", [])?;
        sqlite_error(tx.execute("INSERT INTO messages VALUES ('message-bad','thread','bad','b','t')", []).expect_err("message author"), rusqlite::ffi::SQLITE_CONSTRAINT_CHECK);
        tx.execute("INSERT INTO events (id,type,state,created_at) VALUES ('event-ok','generate_request','pending','t')", [])?;
        sqlite_error(tx.execute("INSERT INTO events (id,type,state,created_at) VALUES ('event-type','bad','pending','t')", []).expect_err("event type"), rusqlite::ffi::SQLITE_CONSTRAINT_CHECK);
        sqlite_error(tx.execute("INSERT INTO events (id,type,state,created_at) VALUES ('event-state','comment','bad','t')", []).expect_err("event state"), rusqlite::ffi::SQLITE_CONSTRAINT_CHECK);
        for (id, kind) in [("kind-null", None), ("kind-message", Some("message")), ("kind-section", Some("deepdive_section")), ("kind-outbox", Some("outbox_item")), ("kind-version", Some("guide_version")), ("kind-ack", Some("ack"))] {
            tx.execute("INSERT INTO events (id,type,state,result_kind,created_at) VALUES (?1,'generate_request','done',?2,'t')", (id, kind))?;
        }
        sqlite_error(tx.execute("INSERT INTO events (id,type,state,result_kind,created_at) VALUES ('kind-bad','generate_request','done','reply','t')", []).expect_err("result kind"), rusqlite::ffi::SQLITE_CONSTRAINT_CHECK);
        tx.execute("INSERT INTO outbox_items VALUES ('outbox-ok','author_question','thread',NULL,'b','draft',NULL,NULL,'t','t')", [])?;
        sqlite_error(tx.execute("INSERT INTO outbox_items VALUES ('outbox-kind','bad','thread',NULL,'b','draft',NULL,NULL,'t','t')", []).expect_err("outbox kind"), rusqlite::ffi::SQLITE_CONSTRAINT_CHECK);
        sqlite_error(tx.execute("INSERT INTO outbox_items VALUES ('outbox-state','author_question','thread',NULL,'b','bad',NULL,NULL,'t','t')", []).expect_err("outbox state"), rusqlite::ffi::SQLITE_CONSTRAINT_CHECK);
        tx.execute("INSERT INTO posts VALUES ('post-ok','done','h','{}',NULL,NULL,'t','t')", [])?;
        sqlite_error(tx.execute("INSERT INTO posts VALUES ('post-bad','bad','h','{}',NULL,NULL,'t','t')", []).expect_err("post state"), rusqlite::ffi::SQLITE_CONSTRAINT_CHECK);
        Ok::<_, StoreError>(())
    })
}

#[test]
fn partial_unique_indexes_enforce_insert_and_update_rules() -> Result<(), StoreError> {
    let temp = tempdir().expect("tempdir");
    let mut store = Store::open_in(temp.path(), &pr())?;
    store.tx(|tx| {
        tx.execute(
            "INSERT INTO runs VALUES ('active','active','t','t',NULL,NULL)",
            [],
        )?;
        tx.execute(
            "INSERT INTO runs VALUES ('stopped-1','stopped','t','t','t','end')",
            [],
        )?;
        tx.execute(
            "INSERT INTO runs VALUES ('stopped-2','stopped','t','t','t','end')",
            [],
        )?;
        tx.execute(
            "INSERT INTO runs VALUES ('expired','expired','t','t','t','expired')",
            [],
        )?;
        sqlite_error(
            tx.execute(
                "INSERT INTO runs VALUES ('active-2','active','t','t',NULL,NULL)",
                [],
            )
            .expect_err("second active"),
            rusqlite::ffi::SQLITE_CONSTRAINT_UNIQUE,
        );
        sqlite_error(
            tx.execute(
                "UPDATE runs SET state = 'active' WHERE id = 'stopped-1'",
                [],
            )
            .expect_err("update active"),
            rusqlite::ffi::SQLITE_CONSTRAINT_UNIQUE,
        );
        tx.execute("UPDATE runs SET state = 'stopped' WHERE id = 'active'", [])?;
        tx.execute(
            "INSERT INTO runs VALUES ('active-3','active','t','t',NULL,NULL)",
            [],
        )?;
        for (id, state) in [("done-1", "done"), ("done-2", "done"), ("failed", "failed")] {
            tx.execute(
                "INSERT INTO posts VALUES (?1,?2,'h','{}',NULL,NULL,'t','t')",
                [id, state],
            )?;
        }
        tx.execute(
            "INSERT INTO posts VALUES ('flight','in_flight','h','{}',NULL,NULL,'t','t')",
            [],
        )?;
        sqlite_error(
            tx.execute(
                "INSERT INTO posts VALUES ('flight-2','in_flight','h','{}',NULL,NULL,'t','t')",
                [],
            )
            .expect_err("second flight"),
            rusqlite::ffi::SQLITE_CONSTRAINT_UNIQUE,
        );
        sqlite_error(
            tx.execute(
                "INSERT INTO posts VALUES ('unknown','unknown','h','{}',NULL,NULL,'t','t')",
                [],
            )
            .expect_err("unknown beside flight"),
            rusqlite::ffi::SQLITE_CONSTRAINT_UNIQUE,
        );
        tx.execute("UPDATE posts SET state = 'done' WHERE id = 'flight'", [])?;
        tx.execute(
            "INSERT INTO posts VALUES ('unknown','unknown','h','{}',NULL,NULL,'t','t')",
            [],
        )?;
        sqlite_error(
            tx.execute(
                "UPDATE posts SET state = 'in_flight' WHERE id = 'done-1'",
                [],
            )
            .expect_err("update flight"),
            rusqlite::ffi::SQLITE_CONSTRAINT_UNIQUE,
        );
        tx.execute("UPDATE posts SET state = 'done' WHERE id = 'unknown'", [])?;
        tx.execute(
            "INSERT INTO posts VALUES ('flight-3','in_flight','h','{}',NULL,NULL,'t','t')",
            [],
        )?;
        Ok::<_, StoreError>(())
    })
}

#[test]
fn unique_constraints_and_foreign_keys_are_enforced() -> Result<(), StoreError> {
    let temp = tempdir().expect("tempdir");
    let mut store = Store::open_in(temp.path(), &pr())?;
    store.tx(|tx| {
        tx.execute("INSERT INTO events (id,type,state,created_at) VALUES ('event-1','generate_request','pending','t')", [])?;
        sqlite_error(tx.execute("INSERT INTO events (id,type,state,created_at) VALUES ('event-1','generate_request','pending','t')", []).expect_err("event unique"), rusqlite::ffi::SQLITE_CONSTRAINT_UNIQUE);
        sqlite_error(tx.execute("INSERT INTO guide_versions VALUES (1,'ready','missing','a','b','full','a','b','[]','[]','t','t')", []).expect_err("missing caused by"), rusqlite::ffi::SQLITE_CONSTRAINT_FOREIGNKEY);
        insert_version(tx, 1, None)?;
        insert_version(tx, 2, Some("event-1"))?;
        sqlite_error(tx.execute("INSERT INTO guide_versions VALUES (3,'ready','event-1','a','b','full','a','b','[]','[]','t','t')", []).expect_err("caused by unique"), rusqlite::ffi::SQLITE_CONSTRAINT_UNIQUE);
        sqlite_error(tx.execute("DELETE FROM events WHERE id = 'event-1'", []).expect_err("referenced event"), rusqlite::ffi::SQLITE_CONSTRAINT_FOREIGNKEY);
        insert_thread(tx, "thread", 1)?;
        sqlite_error(tx.execute("INSERT INTO messages VALUES ('missing-thread','missing','reviewer','b','t')", []).expect_err("missing thread"), rusqlite::ffi::SQLITE_CONSTRAINT_FOREIGNKEY);
        tx.execute("INSERT INTO messages VALUES ('message','thread','reviewer','b','t')", [])?;
        tx.execute("INSERT INTO deepdives VALUES ('deepdive',1,'slide','t')", [])?;
        sqlite_error(tx.execute("INSERT INTO deepdives VALUES ('deepdive-2',1,'slide','t')", []).expect_err("deepdive unique"), rusqlite::ffi::SQLITE_CONSTRAINT_UNIQUE);
        tx.execute("INSERT INTO events (id,type,state,created_at) VALUES ('event-2','generate_request','pending','t')", [])?;
        tx.execute("INSERT INTO deepdive_sections VALUES ('section','deepdive',1,'b','event-2','t')", [])?;
        sqlite_error(tx.execute("INSERT INTO deepdive_sections VALUES ('section-2','deepdive',2,'b','event-2','t')", []).expect_err("section cause unique"), rusqlite::ffi::SQLITE_CONSTRAINT_UNIQUE);
        sqlite_error(tx.execute("INSERT INTO deepdive_sections VALUES ('section-3','deepdive',1,'b','event-1','t')", []).expect_err("section order unique"), rusqlite::ffi::SQLITE_CONSTRAINT_UNIQUE);
        sqlite_error(tx.execute("INSERT INTO outbox_items VALUES ('missing-post','author_question','thread',NULL,'b','draft','missing',NULL,'t','t')", []).expect_err("missing post"), rusqlite::ffi::SQLITE_CONSTRAINT_FOREIGNKEY);
        tx.execute("INSERT INTO posts VALUES ('post','done','h','{}',NULL,NULL,'t','t')", [])?;
        tx.execute("INSERT INTO outbox_items VALUES ('post-present','author_question','thread',NULL,'b','draft','post',NULL,'t','t')", [])?;
        let foreign_key_rows: i64 = tx.query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |row| row.get(0))?;
        assert_eq!(foreign_key_rows, 0);
        Ok::<_, StoreError>(())
    })
}

#[test]
fn tx_commits_rolls_back_and_data_version_tracks_other_connection() -> Result<(), StoreError> {
    let temp = tempdir().expect("tempdir");
    let mut a = Store::open_in(temp.path(), &pr())?;
    let mut b = Store::open_in(temp.path(), &pr())?;
    a.tx(|tx| {
        tx.execute("INSERT INTO store_meta VALUES ('committed','yes')", [])
            .map_err(StoreError::from)
    })?;
    let rollback = a.tx(|tx| {
        tx.execute("INSERT INTO store_meta VALUES ('rolled','no')", [])?;
        Err::<(), _>(StoreError::HomeUnresolved {
            reason: "unchanged".into(),
        })
    });
    assert!(
        matches!(rollback, Err(StoreError::HomeUnresolved { reason }) if reason == "unchanged")
    );
    let rolled: i64 = a.tx(|tx| {
        tx.query_row(
            "SELECT count(*) FROM store_meta WHERE key = 'rolled'",
            [],
            |row| row.get(0),
        )
        .map_err(StoreError::from)
    })?;
    assert_eq!(rolled, 0);
    let v0 = a.data_version()?;
    b.tx(|tx| {
        tx.execute("INSERT INTO store_meta VALUES ('other','yes')", [])
            .map_err(StoreError::from)
    })?;
    let v1 = a.data_version()?;
    assert_ne!(v0, v1);
    a.tx(|tx| {
        tx.execute("INSERT INTO store_meta VALUES ('self','yes')", [])
            .map_err(StoreError::from)
    })?;
    assert_eq!(a.data_version()?, v1);
    Ok(())
}

#[test]
fn tx_returns_store_sqlite_when_another_writer_holds_the_lock() -> Result<(), StoreError> {
    let temp = tempdir().expect("tempdir");
    let mut store = Store::open_in(temp.path(), &pr())?;
    let blocker = Connection::open(store.dir().join("store.db"))?;
    blocker.execute_batch("BEGIN IMMEDIATE")?;
    let error = store
        .tx(|_| Ok::<(), StoreError>(()))
        .expect_err("the lock prevents an immediate transaction");
    assert_eq!(error.code(), "store_sqlite");
    blocker.execute_batch("ROLLBACK")?;
    Ok(())
}

#[test]
fn tx_returns_store_sqlite_when_commit_fails() -> Result<(), StoreError> {
    let temp = tempdir().expect("tempdir");
    let mut store = Store::open_in(temp.path(), &pr())?;
    let error = store
        .tx(|tx| {
            tx.execute_batch("PRAGMA defer_foreign_keys = ON")?;
            tx.execute(
                "INSERT INTO messages VALUES ('deferred','missing','reviewer','body','t')",
                [],
            )?;
            Ok::<_, StoreError>(())
        })
        .expect_err("the deferred foreign key prevents commit");
    assert_eq!(error.code(), "store_sqlite");
    let count: i64 = store.tx(|tx| {
        tx.query_row(
            "SELECT count(*) FROM messages WHERE id = 'deferred'",
            [],
            |row| row.get(0),
        )
        .map_err(StoreError::from)
    })?;
    assert_eq!(count, 0);
    Ok(())
}

#[test]
fn paths_home_and_error_contracts_are_stable() -> Result<(), StoreError> {
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
    for (owner, repo) in [
        ("itsypkin", "PRGuide"),
        ("my.repo", ".github"),
        ("a-b_c", "repo"),
    ] {
        let value = PrRef::new(owner, repo, 7)?;
        assert_eq!((value.owner(), value.repo()), (owner, repo));
    }
    assert_eq!(
        resolve_home(Some(OsStr::new("/x")), Some(OsStr::new("/h")))?,
        Path::new("/x")
    );
    assert_eq!(
        resolve_home(None, Some(OsStr::new("/h")))?,
        Path::new("/h/.prguide")
    );
    assert_eq!(
        resolve_home(Some(OsStr::new("")), Some(OsStr::new("/h")))?,
        Path::new("/h/.prguide")
    );
    for values in [
        (None, None),
        (None, Some("")),
        (Some(""), None),
        (Some("rel"), Some("/h")),
        (Some("rel"), None),
        (None, Some("rel")),
    ] {
        assert_eq!(
            resolve_home(values.0.map(OsStr::new), values.1.map(OsStr::new))
                .expect_err("invalid home")
                .code(),
            "home_unresolved"
        );
    }
    let nul = PrRef::new("a\0b", "r", 1)
        .expect_err("bad name")
        .to_string();
    assert!(nul.contains("a\\0b") && !nul.contains('\0'));
    assert!(
        resolve_home(None, None)
            .expect_err("unset")
            .to_string()
            .ends_with("PRGUIDE_HOME and HOME are unset")
    );
    assert!(
        resolve_home(Some(OsStr::new("rel")), None)
            .expect_err("relative")
            .to_string()
            .ends_with("rel is not absolute")
    );
    assert!(
        resolve_home(None, Some(OsStr::new("rel")))
            .expect_err("relative")
            .to_string()
            .ends_with("rel/.prguide is not absolute")
    );
    let temp = tempdir().expect("tempdir");
    let mixed = PrRef::new("Octo", "RePo", 42)?;
    let mut first = Store::open_in(temp.path(), &mixed)?;
    first.tx(|tx| {
        tx.execute("INSERT INTO store_meta VALUES ('shared','yes')", [])
            .map_err(StoreError::from)
    })?;
    let mut second = Store::open_in(temp.path(), &pr())?;
    assert_eq!(first.dir(), second.dir());
    assert_eq!(mixed.owner(), "Octo");
    assert_eq!(
        second.tx(|tx| tx
            .query_row(
                "SELECT value FROM store_meta WHERE key='shared'",
                [],
                |row| row.get::<_, String>(0)
            )
            .map_err(StoreError::from))?,
        "yes"
    );
    assert_eq!(fs::read_dir(temp.path()).expect("home").count(), 1);
    assert!(temp.path().join("prs/github.com").is_dir());
    assert_eq!(
        PrRef::new("", "r", 1).expect_err("invalid").code(),
        "invalid_repo"
    );
    assert_eq!(
        resolve_home(None, None).expect_err("home").code(),
        "home_unresolved"
    );
    assert_eq!(
        StoreError::StoreTooNew { found: 2, known: 1 }.code(),
        "store_too_new"
    );
    assert_eq!(
        StoreError::Io {
            path: Path::new("/tmp/x").into(),
            source: std::io::Error::other("x"),
        }
        .code(),
        "store_io"
    );
    assert_eq!(
        StoreError::Sqlite(rusqlite::Error::InvalidQuery).code(),
        "store_sqlite"
    );
    Ok(())
}

#[test]
fn store_io_when_home_is_a_file() {
    let file = tempfile::NamedTempFile::new().expect("file");
    let error = match Store::open_in(file.path(), &pr()) {
        Ok(_) => panic!("file is not a home"),
        Err(error) => error,
    };
    assert_eq!(error.code(), "store_io");
}

#[test]
fn concurrent_fresh_opens_succeed() -> Result<(), StoreError> {
    for _ in 0..50 {
        let temp = tempdir().expect("tempdir");
        let home = Arc::new(temp.path().to_owned());
        let barrier = Arc::new(Barrier::new(4));
        let handles: Vec<_> = (0..4)
            .map(|_| {
                let home = Arc::clone(&home);
                let barrier = Arc::clone(&barrier);
                let pr = pr();
                thread::spawn(move || {
                    barrier.wait();
                    Store::open_in(&home, &pr)
                })
            })
            .collect();
        for handle in handles {
            handle.join().expect("thread").expect("open succeeds");
        }
        let mut store = Store::open_in(temp.path(), &pr())?;
        let version: i64 = store.tx(|tx| {
            tx.pragma_query_value(None, "user_version", |row| row.get(0))
                .map_err(StoreError::from)
        })?;
        assert_eq!(version, 1);
        assert_eq!(objects(&mut store)?, expected_objects());
    }
    Ok(())
}

// ---- runs and the lease ----

const T0: i64 = 1_792_713_600_000;
const MIN: i64 = 60_000;

fn at(ms: i64) -> Timestamp {
    Timestamp::from_unix_millis(ms)
}

fn setup() -> (tempfile::TempDir, Store, Arc<FakeClock>) {
    let temp = tempdir().expect("tempdir");
    let clock = Arc::new(FakeClock::new(at(T0)));
    let store = Store::open_in(temp.path(), &pr())
        .expect("open")
        .with_clock(clock.clone());
    (temp, store, clock)
}

type RunRow = (String, String, String, Option<String>, Option<String>);

fn run_row(store: &mut Store, id: &RunId) -> RunRow {
    store
        .tx(|tx| {
            tx.query_row(
                "SELECT state, created_at, lease_until, ended_at, end_reason FROM runs WHERE id = ?1",
                [id.as_str()],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )
            .map_err(StoreError::from)
        })
        .expect("run row")
}

fn run_count(store: &mut Store) -> i64 {
    store
        .tx(|tx| {
            tx.query_row("SELECT count(*) FROM runs", [], |r| r.get(0))
                .map_err(StoreError::from)
        })
        .expect("count")
}

type EventRow = (String, Option<String>, Option<String>);

fn event_row(store: &mut Store, id: &str) -> EventRow {
    store
        .tx(|tx| {
            tx.query_row(
                "SELECT state, claimed_by, claimed_at FROM events WHERE id = ?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .map_err(StoreError::from)
        })
        .expect("event row")
}

fn add_event(store: &mut Store, id: &str, state: &str, by: Option<&RunId>) {
    store
        .tx(|tx| {
            tx.execute(
                "INSERT INTO events (id,type,state,claimed_by,claimed_at,created_at) \
                 VALUES (?1,'comment',?2,?3,?4,'t')",
                rusqlite::params![id, state, by.map(|r| r.as_str()), by.map(|_| "t")],
            )
            .map_err(StoreError::from)
        })
        .expect("add event");
}

fn add_stopped_run(store: &mut Store, id: &str) {
    store
        .tx(|tx| {
            tx.execute(
                "INSERT INTO runs VALUES (?1,'stopped','t','t','t','quit')",
                [id],
            )
            .map_err(StoreError::from)
        })
        .expect("stopped run");
}

fn assert_pending(store: &mut Store, id: &str) {
    assert_eq!(event_row(store, id), ("pending".into(), None, None));
}

fn code<T: std::fmt::Debug>(r: Result<T, StoreError>) -> &'static str {
    r.expect_err("expected an error").code()
}

fn start_claimed(store: &mut Store) -> RunId {
    let a = runs::start_run(store, false).expect("start").id;
    add_event(store, "e1", "claimed", Some(&a));
    a
}

fn inject(temp: &tempfile::TempDir, sql: &str) {
    let dir = temp.path().join("prs/github.com/octo/repo/42/store.db");
    let conn = Connection::open(dir).expect("raw conn");
    conn.execute_batch(sql).expect("trigger");
}

const T_RELEASE: &str = "CREATE TRIGGER fail_release BEFORE UPDATE OF state ON events \
  WHEN NEW.state = 'pending' BEGIN SELECT RAISE(ABORT, 'injected'); END;";
const T_RUN: &str = "CREATE TRIGGER fail_run BEFORE UPDATE ON runs \
  WHEN NEW.state <> 'active' BEGIN SELECT RAISE(ABORT, 'injected'); END;";
const T_INSERT: &str = "CREATE TRIGGER fail_insert BEFORE INSERT ON runs \
  BEGIN SELECT RAISE(ABORT, 'injected'); END;";

#[test]
fn start_then_run_active_then_takeover() {
    let (_t, mut store, clock) = setup();
    let a = runs::start_run(&mut store, false).expect("start");
    assert_eq!(a.state, RunState::Active);
    assert_eq!(a.created_at, "2026-10-23T00:00:00.000Z");
    assert_eq!(a.lease_until, "2026-10-23T00:30:00.000Z");
    assert_eq!(
        run_row(&mut store, &a.id),
        (
            "active".into(),
            "2026-10-23T00:00:00.000Z".into(),
            "2026-10-23T00:30:00.000Z".into(),
            None,
            None
        )
    );
    add_event(&mut store, "e1", "claimed", Some(&a.id));
    clock.set(at(T0 + MIN));
    let err = runs::start_run(&mut store, false).expect_err("active");
    assert_eq!(err.code(), "run_active");
    assert!(matches!(&err, StoreError::RunActive { run_id } if run_id == a.id.as_str()));
    assert!(err.to_string().contains(a.id.as_str()));
    assert_eq!(run_count(&mut store), 1);
    assert_eq!(run_row(&mut store, &a.id).2, "2026-10-23T00:30:00.000Z");
    let b = runs::start_run(&mut store, true).expect("takeover");
    assert_ne!(a.id, b.id);
    assert_eq!(b.state, RunState::Active);
    assert_eq!(b.lease_until, "2026-10-23T00:31:00.000Z");
    assert_eq!(
        run_row(&mut store, &a.id),
        (
            "stopped".into(),
            "2026-10-23T00:00:00.000Z".into(),
            "2026-10-23T00:30:00.000Z".into(),
            Some("2026-10-23T00:01:00.000Z".into()),
            Some("takeover".into())
        )
    );
    assert_eq!(
        run_row(&mut store, &b.id),
        (
            "active".into(),
            "2026-10-23T00:01:00.000Z".into(),
            "2026-10-23T00:31:00.000Z".into(),
            None,
            None
        )
    );
    assert_pending(&mut store, "e1");
    assert_eq!(code(runs::require_active(&mut store, &a.id)), "run_stopped");
}

#[test]
fn undecodable_run_rows_are_store_sqlite() {
    let (t, mut store, _clock) = setup();
    inject(
        &t,
        "PRAGMA ignore_check_constraints=ON; \
         INSERT INTO runs VALUES('r_badstate','bogus','t','t',NULL,NULL); \
         INSERT INTO runs VALUES('r_badreason','stopped','t','t','t','bogus');",
    );
    for id in ["r_badstate", "r_badreason"] {
        assert_eq!(
            code(runs::require_active(&mut store, &RunId::new(id))),
            "store_sqlite"
        );
    }
}

#[test]
fn vanishing_rows_after_a_write_are_store_sqlite() {
    // start_run: the new row is deleted right after the insert.
    let (t, mut store, _c) = setup();
    inject(
        &t,
        "CREATE TRIGGER vanish AFTER INSERT ON runs BEGIN DELETE FROM runs WHERE id = NEW.id; END;",
    );
    assert_eq!(code(runs::start_run(&mut store, false)), "store_sqlite");
    assert_eq!(run_count(&mut store), 0);

    // gate renewal: the row is deleted right after the lease update.
    let (t, mut store, _c) = setup();
    let a = runs::start_run(&mut store, false).expect("start").id;
    inject(
        &t,
        "CREATE TRIGGER vanish AFTER UPDATE OF lease_until ON runs BEGIN DELETE FROM runs WHERE id = NEW.id; END;",
    );
    assert_eq!(code(runs::require_active(&mut store, &a)), "store_sqlite");
    assert_eq!(run_count(&mut store), 1);

    // end_run: the row is deleted right after the state update.
    let (t, mut store, _c) = setup();
    let a = runs::start_run(&mut store, false).expect("start").id;
    inject(
        &t,
        "CREATE TRIGGER vanish AFTER UPDATE OF state ON runs BEGIN DELETE FROM runs WHERE id = NEW.id; END;",
    );
    assert_eq!(
        code(runs::stop_run(&mut store, &a, StopReason::End)),
        "store_sqlite"
    );
    assert_eq!(run_count(&mut store), 1);
}

#[test]
fn calls_after_stop_return_run_stopped() {
    let (_t, mut store, clock) = setup();
    let a = start_claimed(&mut store);
    add_event(&mut store, "e2", "done", Some(&a));
    clock.set(at(T0 + 5 * MIN));
    let run = runs::stop_run(&mut store, &a, StopReason::End).expect("stop");
    assert_eq!(run.state, RunState::Stopped);
    assert_eq!(run.end_reason, Some(EndReason::End));
    assert_eq!(run.ended_at.as_deref(), Some("2026-10-23T00:05:00.000Z"));
    let row = run_row(&mut store, &a);
    assert_eq!(
        row,
        (
            "stopped".into(),
            "2026-10-23T00:00:00.000Z".into(),
            "2026-10-23T00:30:00.000Z".into(),
            Some("2026-10-23T00:05:00.000Z".into()),
            Some("end".into())
        )
    );
    assert_pending(&mut store, "e1");
    assert_eq!(
        event_row(&mut store, "e2"),
        ("done".into(), Some(a.to_string()), Some("t".into()))
    );
    clock.set(at(T0 + 10 * MIN));
    assert_eq!(code(runs::require_active(&mut store, &a)), "run_stopped");
    assert_eq!(
        code(runs::stop_run(&mut store, &a, StopReason::Quit)),
        "run_stopped"
    );
    assert_eq!(run_row(&mut store, &a), row);
    let b = runs::start_run(&mut store, false).expect("start b").id;
    runs::stop_run(&mut store, &b, StopReason::Quit).expect("quit");
    assert_eq!(run_row(&mut store, &b).4.as_deref(), Some("quit"));
}

#[test]
fn fake_clock_expires_run_and_releases_claims() {
    let (_t, mut store, clock) = setup();
    let a = start_claimed(&mut store);
    add_event(&mut store, "e2", "done", Some(&a));
    add_event(&mut store, "e3", "pending", None);
    add_stopped_run(&mut store, "x");
    add_event(&mut store, "e4", "claimed", Some(&RunId::new("x")));
    clock.set(at(T0 + 30 * MIN));
    assert_eq!(code(runs::require_active(&mut store, &a)), "run_stopped");
    let row = (
        "expired".to_string(),
        "2026-10-23T00:00:00.000Z".to_string(),
        "2026-10-23T00:30:00.000Z".to_string(),
        Some("2026-10-23T00:30:00.000Z".to_string()),
        Some("expired".to_string()),
    );
    assert_eq!(run_row(&mut store, &a), row);
    assert_pending(&mut store, "e1");
    assert_eq!(
        event_row(&mut store, "e2"),
        ("done".into(), Some(a.to_string()), Some("t".into()))
    );
    assert_pending(&mut store, "e3");
    assert_eq!(
        event_row(&mut store, "e4"),
        ("claimed".into(), Some("x".into()), Some("t".into()))
    );
    clock.set(at(T0 + 40 * MIN));
    assert_eq!(code(runs::require_active(&mut store, &a)), "run_stopped");
    assert_eq!(run_row(&mut store, &a), row);
}

#[test]
fn start_after_stopped_run_needs_no_takeover() {
    let (_t, mut store, _c) = setup();
    let a = runs::start_run(&mut store, false).expect("a").id;
    runs::stop_run(&mut store, &a, StopReason::Quit).expect("stop");
    let before = run_row(&mut store, &a);
    let b = runs::start_run(&mut store, false).expect("b");
    assert_ne!(a, b.id);
    assert_eq!(b.state, RunState::Active);
    assert_eq!(run_row(&mut store, &a), before);
}

#[test]
fn expired_active_run_does_not_block_start() {
    let (_t, mut store, clock) = setup();
    let a = start_claimed(&mut store);
    clock.set(at(T0 + 30 * MIN));
    let b = runs::start_run(&mut store, false).expect("b");
    assert_ne!(a, b.id);
    let row = run_row(&mut store, &a);
    assert_eq!(row.0, "expired");
    assert_eq!(row.3.as_deref(), Some("2026-10-23T00:30:00.000Z"));
    assert_eq!(row.4.as_deref(), Some("expired"));
    assert_pending(&mut store, "e1");
}

#[test]
fn takeover_of_a_lapsed_run_records_expired() {
    let (_t, mut store, clock) = setup();
    let a = start_claimed(&mut store);
    clock.set(at(T0 + 30 * MIN));
    runs::start_run(&mut store, true).expect("b");
    let row = run_row(&mut store, &a);
    assert_eq!(row.0, "expired");
    assert_eq!(row.4.as_deref(), Some("expired"));
    assert_pending(&mut store, "e1");
}

#[test]
fn lease_valid_one_ms_before_lease_until() {
    let (_t, mut store, clock) = setup();
    let a = runs::start_run(&mut store, false).expect("a").id;
    clock.set(at(T0 + 30 * MIN - 1));
    let run = runs::require_active(&mut store, &a).expect("valid");
    assert_eq!(run.state, RunState::Active);
    assert_eq!(run.lease_until, "2026-10-23T00:59:59.999Z");
    let row = run_row(&mut store, &a);
    assert_eq!(row.0, "active");
    assert_eq!(row.2, "2026-10-23T00:59:59.999Z");
}

#[test]
fn start_blocked_until_the_lease_ends() {
    let (_t, mut store, clock) = setup();
    let a = runs::start_run(&mut store, false).expect("a").id;
    clock.set(at(T0 + 30 * MIN - 1));
    assert_eq!(code(runs::start_run(&mut store, false)), "run_active");
    clock.set(at(T0 + 30 * MIN));
    runs::start_run(&mut store, false).expect("b");
    assert_eq!(run_row(&mut store, &a).0, "expired");
}

#[test]
fn require_active_renews_the_lease() {
    let (_t, mut store, clock) = setup();
    let a = runs::start_run(&mut store, false).expect("a").id;
    clock.set(at(T0 + 29 * MIN));
    runs::require_active(&mut store, &a).expect("29m");
    assert_eq!(run_row(&mut store, &a).2, "2026-10-23T00:59:00.000Z");
    clock.set(at(T0 + 58 * MIN));
    runs::require_active(&mut store, &a).expect("58m");
    assert_eq!(run_row(&mut store, &a).2, "2026-10-23T01:28:00.000Z");
    clock.set(at(T0 + 88 * MIN));
    assert_eq!(code(runs::require_active(&mut store, &a)), "run_stopped");
    assert_eq!(run_row(&mut store, &a).0, "expired");
}

fn atomic_case(
    trigger: &str,
    clock_at: i64,
    op: impl Fn(&mut Store, &RunId) -> Result<(), StoreError>,
    expect_runs: i64,
) {
    let (temp, mut store, clock) = setup();
    let a = start_claimed(&mut store);
    let before = run_row(&mut store, &a);
    inject(&temp, trigger);
    clock.set(at(clock_at));
    let err = op(&mut store, &a).expect_err("injected failure");
    assert_eq!(err.code(), "store_sqlite");
    assert_eq!(run_row(&mut store, &a), before);
    assert_eq!(before.0, "active");
    assert_eq!(
        event_row(&mut store, "e1"),
        ("claimed".into(), Some(a.to_string()), Some("t".into()))
    );
    assert_eq!(run_count(&mut store), expect_runs);
}

#[test]
fn stop_is_one_transaction() {
    for trigger in [T_RELEASE, T_RUN] {
        atomic_case(
            trigger,
            T0 + 5 * MIN,
            |s, a| runs::stop_run(s, a, StopReason::End).map(|_| ()),
            1,
        );
    }
}

#[test]
fn expiry_is_one_transaction() {
    for trigger in [T_RELEASE, T_RUN] {
        atomic_case(
            trigger,
            T0 + 30 * MIN,
            |s, a| runs::require_active(s, a).map(|_| ()),
            1,
        );
    }
}

#[test]
fn takeover_is_one_transaction() {
    for trigger in [T_RELEASE, T_RUN, T_INSERT] {
        atomic_case(
            trigger,
            T0 + 5 * MIN,
            |s, _| runs::start_run(s, true).map(|_| ()),
            1,
        );
    }
}

#[test]
fn run_ids_are_r_and_12_lowercase_base32() {
    let (_t, mut store, _c) = setup();
    let mut seen = std::collections::HashSet::new();
    for _ in 0..20 {
        let id = runs::start_run(&mut store, false).expect("start").id;
        let text = id.as_str();
        assert_eq!(text.len(), 14);
        assert!(text.starts_with("r_"));
        assert!(
            text[2..]
                .chars()
                .all(|c| "0123456789abcdefghjkmnpqrstvwxyz".contains(c))
        );
        assert!(seen.insert(text.to_owned()));
        runs::stop_run(&mut store, &id, StopReason::Quit).expect("stop");
    }
}

#[test]
fn unknown_run_is_run_unknown() {
    let (_t, mut store, _c) = setup();
    let a = runs::start_run(&mut store, false).expect("a").id;
    let before = run_row(&mut store, &a);
    let ghost = RunId::new("r_000000000000");
    for err in [
        runs::require_active(&mut store, &ghost).expect_err("ghost"),
        runs::stop_run(&mut store, &ghost, StopReason::End).expect_err("ghost"),
    ] {
        assert_eq!(err.code(), "run_unknown");
        assert!(err.to_string().contains("r_000000000000"));
    }
    assert_eq!(run_row(&mut store, &a), before);
}

#[test]
fn second_connection_sees_the_active_run() {
    let temp = tempdir().expect("tempdir");
    let clock = Arc::new(FakeClock::new(at(T0)));
    let mut one = Store::open_in(temp.path(), &pr())
        .expect("one")
        .with_clock(clock.clone());
    let mut two = Store::open_in(temp.path(), &pr())
        .expect("two")
        .with_clock(clock.clone());
    let a = runs::start_run(&mut one, false).expect("a").id;
    let err = runs::start_run(&mut two, false).expect_err("active");
    assert!(matches!(&err, StoreError::RunActive { run_id } if run_id == a.as_str()));
    let b = runs::start_run(&mut two, true).expect("b");
    assert_ne!(a, b.id);
    assert_eq!(code(runs::require_active(&mut one, &a)), "run_stopped");
}

#[test]
fn active_run_and_expire_lapsed_on_read() {
    let (_t, mut store, _c) = setup();
    let a = start_claimed(&mut store);
    let found = store
        .tx(|tx| runs::active_run(tx, at(T0 + 10 * MIN)))
        .expect("read")
        .expect("active");
    assert_eq!(found.id, a);
    assert_eq!(found.lease_until, "2026-10-23T00:30:00.000Z");
    assert_eq!(run_row(&mut store, &a).2, "2026-10-23T00:30:00.000Z");
    let none = store
        .tx(|tx| runs::active_run(tx, at(T0 + 30 * MIN)))
        .expect("read");
    assert!(none.is_none());
    let row = run_row(&mut store, &a);
    assert_eq!(row.0, "expired");
    assert_pending(&mut store, "e1");
    let again = store
        .tx(|tx| runs::expire_lapsed(tx, at(T0 + 40 * MIN)))
        .expect("again");
    assert!(again.is_none());
    assert_eq!(run_row(&mut store, &a), row);
}

#[test]
fn stop_run_of_a_lapsed_run_expires_it() {
    let (_t, mut store, clock) = setup();
    let a = start_claimed(&mut store);
    clock.set(at(T0 + 30 * MIN));
    assert_eq!(
        code(runs::stop_run(&mut store, &a, StopReason::End)),
        "run_stopped"
    );
    let row = run_row(&mut store, &a);
    assert_eq!(row.0, "expired");
    assert_eq!(row.4.as_deref(), Some("expired"));
    assert_pending(&mut store, "e1");
}

#[test]
fn default_clock_is_the_system_clock() {
    let temp = tempdir().expect("tempdir");
    let mut store = Store::open_in(temp.path(), &pr()).expect("open");
    let before = SystemClock.now().to_rfc3339();
    let run = runs::start_run(&mut store, false).expect("start");
    let after = SystemClock.now().to_rfc3339();
    let created = run_row(&mut store, &run.id).1;
    assert!(
        before <= created && created <= after,
        "{before} {created} {after}"
    );
}

#[test]
fn run_error_codes_are_stable() {
    for (error, want) in [
        (
            StoreError::RunActive {
                run_id: "r_a".into(),
            },
            "run_active",
        ),
        (
            StoreError::RunStopped {
                run_id: "r_a".into(),
            },
            "run_stopped",
        ),
        (
            StoreError::RunUnknown {
                run_id: "r_a".into(),
            },
            "run_unknown",
        ),
    ] {
        assert_eq!(error.code(), want);
        assert!(error.to_string().contains("r_a"));
    }
}

#[test]
fn schema_v1_snapshot() {
    let temp = tempdir().expect("tempdir");
    let mut store = Store::open_in(temp.path(), &pr()).expect("open");
    let rows: Vec<(String, Option<String>)> = store
        .tx(|tx| {
            tx.prepare("SELECT name, sql FROM sqlite_master ORDER BY name")?
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<Result<Vec<_>, _>>()
                .map_err(StoreError::from)
        })
        .expect("schema");
    let mut text = String::new();
    for (name, sql) in rows {
        text.push_str(&format!(
            "-- {name}\n{}\n",
            sql.as_deref().unwrap_or("NULL")
        ));
    }
    insta::assert_snapshot!("schema_v1_snapshot", text);
}
