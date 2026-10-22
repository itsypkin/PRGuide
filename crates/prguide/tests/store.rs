use std::{
    ffi::OsStr,
    fs,
    path::Path,
    sync::{Arc, Barrier},
    thread,
};

use prguide::store::{PrRef, Store, StoreError, resolve_home};
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
        for id in ["done-1", "done-2", "failed"] {
            tx.execute(
                "INSERT INTO posts VALUES (?1,'done','h','{}',NULL,NULL,'t','t')",
                [id],
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
