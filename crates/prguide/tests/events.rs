//! Event, wait, result, lookup, and CLI integration coverage for T1-3.

use std::{
    fs::{self, OpenOptions},
    num::NonZeroU32,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{Arc, mpsc},
    time::{Duration, Instant},
};

use prguide::store::{
    FakeClock, PrRef, Store, StoreError, Timestamp,
    events::{self, EventType, ResultKind, WaitOutcome},
    runs::{self, RunId, StopReason},
};
use rusqlite::{Connection, params};
use serde_json::Value;
use tempfile::{TempDir, tempdir};

const T0: i64 = 1_792_713_600_000;
const PR: (&str, &str, u64) = ("octo", "repo", 42);

fn nz(value: u32) -> NonZeroU32 {
    NonZeroU32::new(value).expect("non-zero")
}

fn setup() -> (TempDir, Store, Arc<FakeClock>) {
    let temp = tempdir().expect("tempdir");
    let clock = Arc::new(FakeClock::new(Timestamp::from_unix_millis(T0)));
    let store = Store::open_in(temp.path(), &pr())
        .expect("store")
        .with_clock(clock.clone());
    (temp, store, clock)
}

fn pr() -> PrRef {
    PrRef::new(PR.0, PR.1, PR.2).expect("valid PR")
}

fn system_store(home: &TempDir) -> Store {
    Store::open_in(home.path(), &pr()).expect("system store")
}

fn open_with_clock(home: &TempDir, clock: Arc<FakeClock>) -> Store {
    Store::open_in(home.path(), &pr())
        .expect("store")
        .with_clock(clock)
}

fn insert_pending(store: &mut Store, id: &str) {
    store
        .tx(|tx| {
            tx.execute(
                "INSERT INTO events(id,type,state,created_at) VALUES(?1,'generate_request','pending','t')",
                [id],
            )?;
            Ok::<_, StoreError>(())
        })
        .expect("event");
}

fn insert_event(
    store: &mut Store,
    id: &str,
    state: &str,
    claimed_by: Option<&RunId>,
    claimed_at: Option<&str>,
) {
    store
        .tx(|tx| {
            tx.execute(
                "INSERT INTO events(id,type,state,claimed_by,claimed_at,created_at) VALUES(?1,'generate_request',?2,?3,?4,'t')",
                params![id, state, claimed_by.map(RunId::as_str), claimed_at],
            )?;
            Ok::<_, StoreError>(())
        })
        .expect("event");
}

fn insert_stopped_run(store: &mut Store, run: &RunId) {
    store
        .tx(|tx| {
            tx.execute(
                "INSERT INTO runs VALUES (?1,'stopped','t','t','t','end')",
                [run.as_str()],
            )?;
            Ok::<_, StoreError>(())
        })
        .expect("stopped run");
}

type EventRow = (
    String,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
);

fn event_row(store: &mut Store, id: &str) -> EventRow {
    store
        .tx(|tx| {
            tx.query_row(
        "SELECT state,claimed_by,claimed_at,result_kind,result_id,done_at FROM events WHERE id=?1",
        [id],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)),
    ).map_err(StoreError::from)
        })
        .expect("event row")
}

fn run_row(store: &mut Store, run: &RunId) -> (String, String, Option<String>) {
    store
        .tx(|tx| {
            tx.query_row(
                "SELECT state,lease_until,ended_at FROM runs WHERE id=?1",
                [run.as_str()],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .map_err(StoreError::from)
        })
        .expect("run row")
}

fn assert_error<T>(result: Result<T, StoreError>, code: &str, text: &str) {
    let error = match result {
        Ok(_) => panic!("expected error"),
        Err(error) => error,
    };
    assert_eq!(error.code(), code);
    assert!(error.to_string().contains(text), "{}", error);
}

fn wait_for_lock(path: &std::path::Path) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Ok(file) = OpenOptions::new().read(true).write(true).open(path) {
            let mut lock = fd_lock::RwLock::new(file);
            if lock.try_write().is_err() {
                return;
            }
        }
        assert!(Instant::now() < deadline, "listener never acquired lock");
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn spawn_wait(
    home: &TempDir,
    clock: Arc<FakeClock>,
    run: RunId,
    timeout: Option<Duration>,
    max: u32,
) -> mpsc::Receiver<Result<WaitOutcome, StoreError>> {
    let home = home.path().to_owned();
    let (sent, received) = mpsc::channel();
    std::thread::spawn(move || {
        let mut store = Store::open_in(&home, &pr())
            .expect("thread store")
            .with_clock(clock);
        sent.send(events::wait(&mut store, &run, timeout, nz(max)))
            .expect("send");
    });
    received
}

fn wait_now(
    home: &TempDir,
    clock: Arc<FakeClock>,
    run: RunId,
    max: u32,
) -> Result<WaitOutcome, StoreError> {
    spawn_wait(home, clock, run, Some(Duration::ZERO), max)
        .recv_timeout(Duration::from_secs(1))
        .expect("zero-timeout wait hung")
}

fn listing(root: &Path) -> Vec<(PathBuf, Option<Vec<u8>>)> {
    fn visit(root: &Path, path: &Path, entries: &mut Vec<(PathBuf, Option<Vec<u8>>)>) {
        let mut children = fs::read_dir(path)
            .expect("list")
            .map(|entry| entry.expect("entry").path())
            .collect::<Vec<_>>();
        children.sort();
        for child in children {
            let relative = child.strip_prefix(root).expect("relative").to_owned();
            if child.is_dir() {
                entries.push((relative, None));
                visit(root, &child, entries);
            } else {
                entries.push((relative, Some(fs::read(&child).expect("file"))));
            }
        }
    }

    let mut entries = Vec::new();
    visit(root, root, &mut entries);
    entries
}

fn append_generate(store: &mut Store) -> String {
    let now = store.now();
    store
        .tx(|tx| events::append_generate_request(tx, now, None, None))
        .expect("append")
        .id
}

fn seed_thread_and_message(store: &mut Store) {
    store.tx(|tx| {
        tx.execute("INSERT INTO guide_versions VALUES (1,'ready',NULL,'a','b','full','a','b','[]','[]','t','t')", [])?;
        tx.execute("INSERT INTO threads VALUES ('t1','t',1,'slide',NULL)", [])?;
        tx.execute("INSERT INTO messages VALUES ('m1','t1','reviewer','body','t')", [])?;
        Ok::<_, StoreError>(())
    }).expect("thread and message");
}

#[test]
fn append_writes_pending_rows() {
    let (_temp, mut store, _clock) = setup();
    seed_thread_and_message(&mut store);
    let now = store.now();
    let events = store
        .tx(|tx| {
            Ok::<_, StoreError>(vec![
                events::append_comment(tx, now, "t1", "m1")?,
                events::append_deepdive_request(tx, now, "t1")?,
                events::append_generate_request(tx, now, None, None)?,
                events::append_generate_request(tx, now, Some("aaa"), Some("bbb"))?,
            ])
        })
        .expect("append");
    assert_eq!(
        events.iter().map(|e| e.seq).collect::<Vec<_>>(),
        vec![1, 2, 3, 4]
    );
    assert_eq!(events[0].event_type, EventType::Comment);
    assert_eq!(events[1].event_type, EventType::DeepdiveRequest);
    assert_eq!(events[2].event_type, EventType::GenerateRequest);
    assert_eq!(
        events[3].range.as_ref().expect("range").first.as_deref(),
        Some("aaa")
    );
    for event in &events {
        assert_eq!(event.id.len(), 26);
        assert!(
            event
                .id
                .bytes()
                .all(|b| b"0123456789ABCDEFGHJKMNPQRSTVWXYZ".contains(&b))
        );
        assert_eq!(
            event_row(&mut store, &event.id),
            ("pending".into(), None, None, None, None, None)
        );
    }
    assert_eq!(
        store
            .tx(|tx| tx
                .query_row("SELECT created_at FROM events WHERE seq=1", [], |r| r
                    .get::<_, String>(0))
                .map_err(StoreError::from))
            .expect("time"),
        now.to_rfc3339()
    );
}

#[test]
fn append_comment_needs_its_thread_and_message() {
    let (_temp, mut store, _clock) = setup();
    let now = store.now();
    let error = store
        .tx(|tx| events::append_comment(tx, now, "missing", "missing"))
        .expect_err("foreign key");
    assert_eq!(error.code(), "store_sqlite");
    assert!(error.to_string().contains("FOREIGN KEY"));
    assert_eq!(
        store
            .tx(|tx| tx
                .query_row("SELECT count(*) FROM events", [], |r| r.get::<_, i64>(0))
                .map_err(StoreError::from))
            .expect("count"),
        0
    );
}

#[test]
fn wait_returns_claimed_then_pending_by_seq_up_to_max() {
    let (temp, mut store, clock) = setup();
    let run = runs::start_run(&mut store, false).expect("run").id;
    insert_event(&mut store, "ez", "pending", None, None);
    insert_event(&mut store, "ey", "claimed", Some(&run), Some("old"));
    for id in [
        "ex", "ew", "ev", "eu", "et", "es", "er", "eq", "ep", "eo", "en", "em",
    ] {
        insert_pending(&mut store, id);
    }
    let WaitOutcome::Events(got) = wait_now(&temp, clock.clone(), run.clone(), 3).expect("wait")
    else {
        panic!("events")
    };
    assert_eq!(
        got.iter().map(|event| event.seq).collect::<Vec<_>>(),
        vec![2, 1, 3]
    );
    assert_eq!(event_row(&mut store, "ey").2.as_deref(), Some("old"));
    for id in ["ez", "ex"] {
        assert_eq!(
            event_row(&mut store, id),
            (
                "claimed".into(),
                Some(run.to_string()),
                Some("2026-10-23T00:00:00.000Z".into()),
                None,
                None,
                None
            )
        );
    }
    for id in [
        "ew", "ev", "eu", "et", "es", "er", "eq", "ep", "eo", "en", "em",
    ] {
        assert_eq!(
            event_row(&mut store, id),
            ("pending".into(), None, None, None, None, None)
        );
    }
    let x = RunId::new("r_x");
    insert_stopped_run(&mut store, &x);
    insert_event(&mut store, "other", "claimed", Some(&x), Some("old"));
    let WaitOutcome::Events(again) =
        wait_now(&temp, clock.clone(), run.clone(), 5).expect("second")
    else {
        panic!("events")
    };
    assert_eq!(
        again.iter().map(|e| e.seq).collect::<Vec<_>>(),
        (1..=5).collect::<Vec<_>>()
    );
    let WaitOutcome::Events(bound) = wait_now(&temp, clock, run.clone(), 3).expect("bound") else {
        panic!("events")
    };
    assert_eq!(
        bound.iter().map(|e| e.seq).collect::<Vec<_>>(),
        vec![1, 2, 3]
    );
    assert_eq!(
        event_row(&mut store, "other"),
        (
            "claimed".into(),
            Some(x.to_string()),
            Some("old".into()),
            None,
            None,
            None
        )
    );
}

#[test]
fn wait_event_json_shapes() {
    let (temp, mut store, clock) = setup();
    seed_thread_and_message(&mut store);
    let run = runs::start_run(&mut store, false).expect("run").id;
    let now = store.now();
    let inserted = store
        .tx(|tx| {
            Ok::<_, StoreError>(vec![
                events::append_comment(tx, now, "t1", "m1")?,
                events::append_deepdive_request(tx, now, "t1")?,
                events::append_generate_request(tx, now, None, None)?,
                events::append_generate_request(tx, now, Some("aaa"), Some("bbb"))?,
            ])
        })
        .expect("append");
    let outcome = wait_now(&temp, clock, run, 10).expect("wait");
    let text = serde_json::to_string(&outcome).expect("json");
    assert_eq!(
        text,
        format!(
            "{{\"events\":[{{\"id\":\"{}\",\"seq\":1,\"type\":\"comment\",\"thread\":\"t1\",\"message\":\"m1\",\"range\":null}},{{\"id\":\"{}\",\"seq\":2,\"type\":\"deepdive_request\",\"thread\":\"t1\",\"message\":null,\"range\":null}},{{\"id\":\"{}\",\"seq\":3,\"type\":\"generate_request\",\"thread\":null,\"message\":null,\"range\":null}},{{\"id\":\"{}\",\"seq\":4,\"type\":\"generate_request\",\"thread\":null,\"message\":null,\"range\":{{\"first\":\"aaa\",\"last\":\"bbb\"}}}}]}}",
            inserted[0].id, inserted[1].id, inserted[2].id, inserted[3].id
        )
    );
    assert_eq!(
        serde_json::to_string(&WaitOutcome::Stopped).expect("json"),
        "{\"stopped\":true}"
    );
    assert_eq!(
        serde_json::to_string(&WaitOutcome::Events(vec![])).expect("json"),
        "{\"events\":[]}"
    );
}

#[test]
fn wait_times_out_on_the_store_clock() {
    let (temp, mut store, clock) = setup();
    let run = runs::start_run(&mut store, false).expect("run").id;
    let received = spawn_wait(
        &temp,
        clock.clone(),
        run.clone(),
        Some(Duration::from_secs(300)),
        10,
    );
    std::thread::sleep(Duration::from_millis(600));
    assert!(received.try_recv().is_err());
    clock.advance(Duration::from_secs(300));
    assert_eq!(
        received
            .recv_timeout(Duration::from_secs(2))
            .expect("return")
            .expect("wait"),
        WaitOutcome::Events(vec![])
    );
    assert_eq!(
        wait_now(&temp, clock, run, 10).expect("zero"),
        WaitOutcome::Events(vec![])
    );
}

#[test]
fn wait_wakes_when_an_event_is_appended() {
    let (temp, mut store, clock) = setup();
    let run = runs::start_run(&mut store, false).expect("run").id;
    let received = spawn_wait(&temp, clock.clone(), run.clone(), None, 10);
    wait_for_lock(&store.dir().join(events::WAIT_LOCK));
    let mut writer = open_with_clock(&temp, clock);
    let id = append_generate(&mut writer);
    let WaitOutcome::Events(got) = received
        .recv_timeout(Duration::from_secs(2))
        .expect("wake")
        .expect("wait")
    else {
        panic!("events")
    };
    assert_eq!(
        got.iter().map(|e| e.id.as_str()).collect::<Vec<_>>(),
        vec![id.as_str()]
    );
    assert_eq!(event_row(&mut store, &id).1.as_deref(), Some(run.as_str()));
}

#[test]
fn stop_while_blocked_returns_stopped_within_1s() {
    let (temp, mut store, clock) = setup();
    let run = runs::start_run(&mut store, false).expect("run").id;
    let received = spawn_wait(&temp, clock, run.clone(), None, 10);
    wait_for_lock(&store.dir().join(events::WAIT_LOCK));
    let start = Instant::now();
    runs::stop_run(&mut store, &run, StopReason::End).expect("stop");
    assert_eq!(
        received
            .recv_timeout(Duration::from_secs(2))
            .expect("return")
            .expect("wait"),
        WaitOutcome::Stopped
    );
    assert!(start.elapsed() < Duration::from_secs(1));
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(store.dir().join(events::WAIT_LOCK))
        .expect("lock file");
    assert!(fd_lock::RwLock::new(file).try_write().is_ok());
}

#[test]
fn second_wait_gets_wait_active() {
    let (temp, mut store, clock) = setup();
    let run = runs::start_run(&mut store, false).expect("run").id;
    let received = spawn_wait(&temp, clock.clone(), run.clone(), None, 10);
    wait_for_lock(&store.dir().join(events::WAIT_LOCK));
    let mut second = open_with_clock(&temp, clock.clone());
    let start = Instant::now();
    assert_error(
        events::wait(&mut second, &run, Some(Duration::ZERO), nz(10)),
        "wait_active",
        "wait.lock",
    );
    assert!(start.elapsed() >= Duration::from_millis(450));
    assert!(start.elapsed() < Duration::from_secs(3));
    runs::stop_run(&mut store, &run, StopReason::End).expect("stop");
    assert_eq!(
        received
            .recv_timeout(Duration::from_secs(2))
            .expect("return")
            .expect("wait"),
        WaitOutcome::Stopped
    );
    let b = runs::start_run(&mut store, false).expect("run b").id;
    assert_eq!(
        wait_now(&temp, clock, b, 10).expect("wait b"),
        WaitOutcome::Events(vec![])
    );
}

#[test]
fn wait_retries_a_briefly_held_lock() {
    let (temp, mut store, clock) = setup();
    let run = runs::start_run(&mut store, false).expect("run").id;
    let path = store.dir().join(events::WAIT_LOCK);
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&path)
        .expect("lock");
    let mut held = fd_lock::RwLock::new(file);
    let guard = held.write().expect("hold lock");
    let received = spawn_wait(&temp, clock, run, Some(Duration::ZERO), 10);
    std::thread::sleep(Duration::from_millis(150));
    drop(guard);
    assert_eq!(
        received
            .recv_timeout(Duration::from_secs(2))
            .expect("return")
            .expect("wait"),
        WaitOutcome::Events(vec![])
    );
}

#[test]
fn stopped_run_returns_stopped_without_the_lock() {
    let (_temp, mut store, _clock) = setup();
    let run = runs::start_run(&mut store, false).expect("run").id;
    runs::stop_run(&mut store, &run, StopReason::End).expect("stop");
    let path = store.dir().join(events::WAIT_LOCK);
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path)
        .expect("lock");
    let mut lock = fd_lock::RwLock::new(file);
    let _guard = lock.write().expect("hold");
    let start = Instant::now();
    assert_eq!(
        events::wait(&mut store, &run, None, nz(10)).expect("wait"),
        WaitOutcome::Stopped
    );
    assert!(start.elapsed() < Duration::from_millis(450));
}

#[test]
fn wait_unknown_run_is_run_unknown() {
    let (_temp, mut store, _clock) = setup();
    assert_error(
        events::wait(
            &mut store,
            &RunId::new("r_000000000000"),
            Some(Duration::ZERO),
            nz(10),
        ),
        "run_unknown",
        "r_000000000000",
    );
}

#[test]
fn wait_reports_lock_open_errors() {
    let (_temp, mut store, _clock) = setup();
    let run = runs::start_run(&mut store, false).expect("run").id;
    let dir = store.dir().to_owned();
    fs::rename(&dir, dir.with_extension("moved")).expect("move store directory");
    assert_error(
        events::wait(&mut store, &run, Some(Duration::ZERO), nz(10)),
        "store_io",
        "wait.lock",
    );
}

#[test]
fn wait_renews_the_lease_at_most_every_60s() {
    let (temp, mut store, clock) = setup();
    let run = runs::start_run(&mut store, false).expect("run").id;
    let x = RunId::new("r_x");
    insert_stopped_run(&mut store, &x);
    insert_event(&mut store, "other", "claimed", Some(&x), Some("old"));
    clock.set(Timestamp::from_unix_millis(T0 + 10_000));
    let received = spawn_wait(&temp, clock.clone(), run.clone(), None, 10);
    wait_for_lock(&store.dir().join(events::WAIT_LOCK));
    assert_eq!(run_row(&mut store, &run).1, "2026-10-23T00:30:10.000Z");
    for offset in [20_000, 69_999] {
        clock.set(Timestamp::from_unix_millis(T0 + offset));
        std::thread::sleep(Duration::from_millis(600));
        assert_eq!(run_row(&mut store, &run).1, "2026-10-23T00:30:10.000Z");
    }
    clock.set(Timestamp::from_unix_millis(T0 + 70_000));
    let deadline = Instant::now() + Duration::from_secs(2);
    while run_row(&mut store, &run).1 == "2026-10-23T00:30:10.000Z" {
        assert!(Instant::now() < deadline, "lease not renewed");
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(run_row(&mut store, &run).1, "2026-10-23T00:31:10.000Z");
    clock.set(Timestamp::from_unix_millis(T0 + 129_999));
    std::thread::sleep(Duration::from_millis(600));
    assert_eq!(run_row(&mut store, &run).1, "2026-10-23T00:31:10.000Z");
    clock.set(Timestamp::from_unix_millis(T0 + 130_000));
    let deadline = Instant::now() + Duration::from_secs(2);
    while run_row(&mut store, &run).1 == "2026-10-23T00:31:10.000Z" {
        assert!(Instant::now() < deadline, "lease not renewed twice");
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(run_row(&mut store, &run).1, "2026-10-23T00:32:10.000Z");
    runs::stop_run(&mut store, &run, StopReason::End).expect("stop");
    assert_eq!(
        received
            .recv_timeout(Duration::from_secs(2))
            .expect("return")
            .expect("wait"),
        WaitOutcome::Stopped
    );
}

#[test]
fn wait_holds_no_write_lock_while_blocked() {
    let (temp, mut store, clock) = setup();
    let run = runs::start_run(&mut store, false).expect("run").id;
    let received = spawn_wait(&temp, clock, run.clone(), None, 10);
    wait_for_lock(&store.dir().join(events::WAIT_LOCK));
    let conn = Connection::open(store.dir().join("store.db")).expect("connection");
    conn.busy_timeout(Duration::ZERO).expect("timeout");
    for _ in 0..8 {
        conn.execute_batch("BEGIN IMMEDIATE; COMMIT;")
            .expect("write lock free");
        std::thread::sleep(Duration::from_millis(125));
    }
    runs::stop_run(&mut store, &run, StopReason::End).expect("stop");
    assert_eq!(
        received
            .recv_timeout(Duration::from_secs(2))
            .expect("return")
            .expect("wait"),
        WaitOutcome::Stopped
    );
}

#[test]
fn wait_on_a_lapsed_run_commits_the_expiry() {
    let (temp, mut store, clock) = setup();
    let run = runs::start_run(&mut store, false).expect("run").id;
    let id = append_generate(&mut store);
    let _ = wait_now(&temp, clock.clone(), run.clone(), 10).expect("claim");
    clock.set(Timestamp::from_unix_millis(T0 + 45 * 60_000));
    assert_eq!(
        wait_now(&temp, clock, run.clone(), 10).expect("lapse"),
        WaitOutcome::Stopped
    );
    assert_eq!(
        run_row(&mut store, &run),
        (
            "expired".into(),
            "2026-10-23T00:30:00.000Z".into(),
            Some("2026-10-23T00:45:00.000Z".into())
        )
    );
    assert_eq!(
        event_row(&mut store, &id),
        ("pending".into(), None, None, None, None, None)
    );
}

#[test]
fn lapse_while_blocked_returns_stopped_without_claiming() {
    let (temp, mut store, clock) = setup();
    let run = runs::start_run(&mut store, false).expect("run").id;
    let received = spawn_wait(&temp, clock.clone(), run.clone(), None, 10);
    wait_for_lock(&store.dir().join(events::WAIT_LOCK));
    clock.set(Timestamp::from_unix_millis(T0 + 31 * 60_000));
    let mut writer = open_with_clock(&temp, clock);
    let id = append_generate(&mut writer);
    assert_eq!(
        received
            .recv_timeout(Duration::from_secs(2))
            .expect("return")
            .expect("wait"),
        WaitOutcome::Stopped
    );
    assert_eq!(run_row(&mut store, &run).0, "expired");
    assert_eq!(event_row(&mut store, &id).0, "pending");
    assert_eq!(event_row(&mut store, &id).1, None);
}

#[test]
fn claims_return_to_pending_on_stop_and_the_next_run_gets_them() {
    let (temp, mut store, clock) = setup();
    let a = runs::start_run(&mut store, false).expect("a").id;
    for id in ["e1", "e2", "e3"] {
        insert_pending(&mut store, id);
    }
    let _ = wait_now(&temp, clock.clone(), a.clone(), 2).expect("claim");
    runs::stop_run(&mut store, &a, StopReason::End).expect("stop");
    assert_eq!(event_row(&mut store, "e1").0, "pending");
    let b = runs::start_run(&mut store, false).expect("b").id;
    let WaitOutcome::Events(got) = wait_now(&temp, clock, b.clone(), 10).expect("wait") else {
        panic!("events")
    };
    assert_eq!(
        got.iter().map(|e| e.id.as_str()).collect::<Vec<_>>(),
        vec!["e1", "e2", "e3"]
    );
    for id in ["e1", "e2", "e3"] {
        assert_eq!(event_row(&mut store, id).1.as_deref(), Some(b.as_str()));
    }
}

#[test]
fn claims_return_to_pending_on_expiry_and_the_next_run_gets_them() {
    let (temp, mut store, clock) = setup();
    let a = runs::start_run(&mut store, false).expect("a").id;
    for id in ["e1", "e2", "e3"] {
        insert_pending(&mut store, id);
    }
    let _ = wait_now(&temp, clock.clone(), a.clone(), 2).expect("claim");
    clock.set(Timestamp::from_unix_millis(T0 + 30 * 60_000));
    let b = runs::start_run(&mut store, false).expect("b").id;
    let WaitOutcome::Events(got) = wait_now(&temp, clock, b.clone(), 10).expect("wait") else {
        panic!("events")
    };
    assert_eq!(
        got.iter()
            .map(|event| event.id.as_str())
            .collect::<Vec<_>>(),
        vec!["e1", "e2", "e3"]
    );
    assert_eq!(run_row(&mut store, &a).0, "expired");
    for id in ["e1", "e2", "e3"] {
        assert_eq!(
            event_row(&mut store, id),
            (
                "claimed".into(),
                Some(b.to_string()),
                Some("2026-10-23T00:30:00.000Z".into()),
                None,
                None,
                None
            )
        );
    }
}

#[test]
fn takeover_hands_claims_to_the_new_run() {
    let (temp, mut store, clock) = setup();
    let a = runs::start_run(&mut store, false).expect("a").id;
    insert_pending(&mut store, "e1");
    let _ = wait_now(&temp, clock.clone(), a, 10).expect("claim");
    let b = runs::start_run(&mut store, true).expect("b").id;
    let WaitOutcome::Events(got) = wait_now(&temp, clock.clone(), b.clone(), 10).expect("wait")
    else {
        panic!("events")
    };
    assert_eq!(got[0].id, "e1");
    assert_eq!(event_row(&mut store, "e1").1.as_deref(), Some(b.as_str()));
}

#[test]
fn events_appended_with_no_active_run_reach_the_next_run() {
    let (temp, mut store, clock) = setup();
    insert_pending(&mut store, "e1");
    let a = runs::start_run(&mut store, false).expect("a").id;
    let WaitOutcome::Events(first) =
        wait_now(&temp, clock.clone(), a.clone(), 10).expect("first wait")
    else {
        panic!("events")
    };
    assert_eq!(
        first
            .iter()
            .map(|event| event.id.as_str())
            .collect::<Vec<_>>(),
        ["e1"]
    );
    runs::stop_run(&mut store, &a, StopReason::End).expect("stop");
    insert_pending(&mut store, "e2");
    let b = runs::start_run(&mut store, false).expect("b").id;
    let WaitOutcome::Events(got) = wait_now(&temp, clock.clone(), b, 10).expect("wait") else {
        panic!("events")
    };
    assert_eq!(
        got.iter()
            .map(|event| event.id.as_str())
            .collect::<Vec<_>>(),
        ["e1", "e2"]
    );
}

fn claimed_event() -> (TempDir, Store, Arc<FakeClock>, RunId, String) {
    let (temp, mut store, clock) = setup();
    let run = runs::start_run(&mut store, false).expect("run").id;
    let id = append_generate(&mut store);
    let _ = wait_now(&temp, clock.clone(), run.clone(), 10).expect("claim");
    (temp, store, clock, run, id)
}

#[test]
fn with_result_marks_done_in_the_closures_transaction() {
    let (_temp, mut store, clock, run, id) = claimed_event();
    clock.advance(Duration::from_secs(10));
    let result = events::with_result(&mut store, &run, &id, ResultKind::Message, |tx| {
        tx.execute(
            "INSERT INTO notices(kind,ref,created_at) VALUES ('guide_ready','x','t')",
            [],
        )?;
        Ok::<_, StoreError>("m1".into())
    })
    .expect("result");
    assert_eq!(
        result,
        events::ResultRef {
            kind: ResultKind::Message,
            id: "m1".into()
        }
    );
    assert_eq!(
        event_row(&mut store, &id),
        (
            "done".into(),
            Some(run.to_string()),
            Some("2026-10-23T00:00:00.000Z".into()),
            Some("message".into()),
            Some("m1".into()),
            Some("2026-10-23T00:00:10.000Z".into())
        )
    );
    assert_eq!(
        store
            .tx(|tx| tx
                .query_row("SELECT count(*) FROM notices", [], |r| r.get::<_, i64>(0))
                .map_err(StoreError::from))
            .expect("notices"),
        1
    );
    assert_eq!(run_row(&mut store, &run).1, "2026-10-23T00:30:10.000Z");
}

#[test]
fn stored_result_requires_complete_known_reference() {
    let (_temp, mut store, _clock, run, id) = claimed_event();
    let _ = events::with_result(&mut store, &run, &id, ResultKind::Message, |_| {
        Ok::<_, StoreError>("m1".into())
    })
    .expect("done");
    store
        .tx(|tx| {
            tx.execute("UPDATE events SET result_kind=NULL WHERE id=?1", [&id])?;
            Ok::<_, StoreError>(())
        })
        .expect("null kind");
    assert_error(
        events::with_result(&mut store, &run, &id, ResultKind::Ack, |_| {
            Ok::<_, StoreError>("other".into())
        }),
        "store_sqlite",
        "store database error",
    );
    store
        .tx(|tx| {
            tx.execute(
                "UPDATE events SET result_kind='message',result_id=NULL WHERE id=?1",
                [&id],
            )?;
            Ok::<_, StoreError>(())
        })
        .expect("null id");
    assert_error(
        events::with_result(&mut store, &run, &id, ResultKind::Ack, |_| {
            Ok::<_, StoreError>("other".into())
        }),
        "store_sqlite",
        "store database error",
    );
    store
        .tx(|tx| {
            tx.pragma_update(None, "ignore_check_constraints", "ON")?;
            tx.execute(
                "UPDATE events SET result_kind='bogus',result_id='m1' WHERE id=?1",
                [&id],
            )?;
            Ok::<_, StoreError>(())
        })
        .expect("invalid kind");
    assert_error(
        events::with_result(&mut store, &run, &id, ResultKind::Ack, |_| {
            Ok::<_, StoreError>("other".into())
        }),
        "store_sqlite",
        "unknown result kind bogus",
    );
}

#[test]
fn duplicate_result_returns_the_first_and_skips_the_closure() {
    let (_temp, mut store, clock, run, id) = claimed_event();
    let _ = events::with_result(&mut store, &run, &id, ResultKind::Message, |_| {
        Ok::<_, StoreError>("m1".into())
    })
    .expect("first");
    let before = event_row(&mut store, &id);
    clock.advance(Duration::from_secs(1));
    let mut called = false;
    let result = events::with_result(&mut store, &run, &id, ResultKind::Ack, |_| {
        called = true;
        Ok::<_, StoreError>("other".into())
    })
    .expect("repeat");
    assert_eq!(
        result,
        events::ResultRef {
            kind: ResultKind::Message,
            id: "m1".into()
        }
    );
    assert!(!called);
    assert_eq!(event_row(&mut store, &id), before);
}

#[test]
fn closure_error_rolls_back_the_result() {
    let (_temp, mut store, _clock, run, id) = claimed_event();
    let result = events::with_result(&mut store, &run, &id, ResultKind::Message, |tx| {
        tx.execute(
            "INSERT INTO notices(kind,ref,created_at) VALUES ('guide_ready','x','t')",
            [],
        )?;
        Err::<String, _>(StoreError::EventUnknown {
            event_id: "marker".into(),
        })
    });
    assert_error(result, "event_unknown", "marker");
    assert_eq!(
        store
            .tx(|tx| tx
                .query_row("SELECT count(*) FROM notices", [], |r| r.get::<_, i64>(0))
                .map_err(StoreError::from))
            .expect("notices"),
        0
    );
    assert_eq!(event_row(&mut store, &id).0, "claimed");
}

#[test]
fn done_update_failure_rolls_back_the_closure() {
    let (temp, mut store, _clock, run, id) = claimed_event();
    let conn =
        Connection::open(temp.path().join("prs/github.com/octo/repo/42/store.db")).expect("raw");
    conn.execute_batch("CREATE TRIGGER fail_done BEFORE UPDATE OF state ON events WHEN NEW.state='done' BEGIN SELECT RAISE(ABORT,'injected'); END;").expect("trigger");
    let result = events::with_result(&mut store, &run, &id, ResultKind::Message, |tx| {
        tx.execute(
            "INSERT INTO notices(kind,ref,created_at) VALUES ('guide_ready','x','t')",
            [],
        )?;
        Ok::<_, StoreError>("m1".into())
    });
    assert_error(result, "store_sqlite", "injected");
    assert_eq!(
        store
            .tx(|tx| tx
                .query_row("SELECT count(*) FROM notices", [], |r| r.get::<_, i64>(0))
                .map_err(StoreError::from))
            .expect("notices"),
        0
    );
    assert_eq!(event_row(&mut store, &id).0, "claimed");
}

#[test]
fn unknown_event_is_event_unknown() {
    let (_temp, mut store, _clock, run, _id) = claimed_event();
    let mut called = false;
    let result = events::with_result(&mut store, &run, "nope", ResultKind::Ack, |_| {
        called = true;
        Ok::<_, StoreError>("x".into())
    });
    assert_error(result, "event_unknown", "nope");
    assert!(!called);
}

#[test]
fn result_needs_a_claim_by_this_run() {
    let (_temp, mut store, _clock, a, _id) = claimed_event();
    insert_pending(&mut store, "pending");
    let x = RunId::new("r_x");
    store
        .tx(|tx| {
            tx.execute(
                "INSERT INTO runs VALUES (?1,'stopped','t','t','t','end')",
                [x.as_str()],
            )?;
            Ok::<_, StoreError>(())
        })
        .expect("x");
    insert_event(&mut store, "other", "claimed", Some(&x), Some("t"));
    insert_event(&mut store, "done", "done", Some(&x), Some("t"));
    store
        .tx(|tx| {
            tx.execute(
                "UPDATE events SET result_kind='message',result_id='m' WHERE id='done'",
                [],
            )?;
            Ok::<_, StoreError>(())
        })
        .expect("done");
    for id in ["pending", "other", "done"] {
        let before = event_row(&mut store, id);
        let mut called = false;
        let error = events::with_result(&mut store, &a, id, ResultKind::Ack, |_| {
            called = true;
            Ok::<_, StoreError>("x".into())
        })
        .expect_err("not claimed");
        assert_eq!(error.code(), "event_not_claimed");
        assert!(error.to_string().contains(id));
        assert!(error.to_string().contains(a.as_str()));
        assert!(!called);
        assert_eq!(event_row(&mut store, id), before);
    }
}

#[test]
fn result_on_a_stopped_run_is_run_stopped() {
    let (_temp, mut store, _clock, run, id) = claimed_event();
    let _ = events::with_result(&mut store, &run, &id, ResultKind::Message, |_| {
        Ok::<_, StoreError>("m1".into())
    })
    .expect("done");
    runs::stop_run(&mut store, &run, StopReason::End).expect("stop");
    let mut called = false;
    let result = events::with_result(&mut store, &run, &id, ResultKind::Ack, |_| {
        called = true;
        Ok::<_, StoreError>("x".into())
    });
    assert_error(result, "run_stopped", run.as_str());
    assert!(!called);
}

#[test]
fn result_on_a_lapsed_run_commits_the_expiry() {
    let (_temp, mut store, clock, run, id) = claimed_event();
    clock.set(Timestamp::from_unix_millis(T0 + 45 * 60_000));
    let mut called = false;
    let result = events::with_result(&mut store, &run, &id, ResultKind::Ack, |_| {
        called = true;
        Ok::<_, StoreError>("x".into())
    });
    assert_error(result, "run_stopped", run.as_str());
    assert!(!called);
    assert_eq!(
        run_row(&mut store, &run),
        (
            "expired".into(),
            "2026-10-23T00:30:00.000Z".into(),
            Some("2026-10-23T00:45:00.000Z".into())
        )
    );
    assert_eq!(
        event_row(&mut store, &id),
        ("pending".into(), None, None, None, None, None)
    );
}

#[test]
fn ack_records_kind_ack_and_repeats() {
    let (temp, mut store, clock, run, id) = claimed_event();
    assert_eq!(
        events::ack(&mut store, &run, &id).expect("ack"),
        events::ResultRef {
            kind: ResultKind::Ack,
            id: id.clone()
        }
    );
    let before = event_row(&mut store, &id);
    assert_eq!(
        events::ack(&mut store, &run, &id).expect("repeat"),
        events::ResultRef {
            kind: ResultKind::Ack,
            id: id.clone()
        }
    );
    assert_eq!(event_row(&mut store, &id), before);
    let id2 = append_generate(&mut store);
    let _ = wait_now(&temp, clock, run.clone(), 10).expect("claim");
    let _ = events::with_result(&mut store, &run, &id2, ResultKind::Message, |_| {
        Ok::<_, StoreError>("m2".into())
    })
    .expect("message");
    assert_eq!(
        events::ack(&mut store, &run, &id2).expect("stored"),
        events::ResultRef {
            kind: ResultKind::Message,
            id: "m2".into()
        }
    );
}

#[test]
fn result_unknown_run_is_run_unknown() {
    let (_temp, mut store, _clock) = setup();
    let mut called = false;
    let result = events::with_result(
        &mut store,
        &RunId::new("r_000000000000"),
        "e",
        ResultKind::Ack,
        |_| {
            called = true;
            Ok::<_, StoreError>("x".into())
        },
    );
    assert_error(result, "run_unknown", "r_000000000000");
    assert!(!called);
}

#[test]
fn open_for_run_finds_the_store_holding_the_run() {
    let temp = tempdir().expect("temp");
    let mut first =
        Store::open_in(temp.path(), &PrRef::new("Octo", "Repo", 42).expect("pr")).expect("first");
    let mut other =
        Store::open_in(temp.path(), &PrRef::new("octo", "other", 7).expect("pr")).expect("other");
    let mut third =
        Store::open_in(temp.path(), &PrRef::new("zed", "repo", 1).expect("pr")).expect("third");
    let first_run = runs::start_run(&mut first, false).expect("first").id;
    let second = runs::start_run(&mut other, false).expect("second").id;
    let third_id = runs::start_run(&mut third, false).expect("third").id;
    for (run, dir) in [
        (&first_run, first.dir()),
        (&second, other.dir()),
        (&third_id, third.dir()),
    ] {
        let mut found = Store::open_for_run(temp.path(), run).expect("found");
        assert_eq!(found.dir(), dir);
        runs::require_active(&mut found, run).expect("active");
    }
}

#[test]
fn open_for_run_unknown_run_writes_nothing() {
    let temp = tempdir().expect("temp");
    assert_error(
        Store::open_for_run(temp.path(), &RunId::new("r_none")),
        "run_unknown",
        "r_none",
    );
    assert!(fs::read_dir(temp.path()).expect("empty").next().is_none());
    let mut store = system_store(&temp);
    let _ = runs::start_run(&mut store, false).expect("run");
    drop(store);
    let before = listing(temp.path());
    assert_error(
        Store::open_for_run(temp.path(), &RunId::new("r_none")),
        "run_unknown",
        "r_none",
    );
    let after = listing(temp.path())
        .into_iter()
        .filter(|(path, _)| {
            !matches!(
                path.file_name().and_then(|name| name.to_str()),
                Some("store.db-wal" | "store.db-shm")
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(after, before);
}

#[test]
fn open_for_run_skips_junk_and_leaves_it_untouched() {
    let (temp, mut store, _clock) = setup();
    let run = runs::start_run(&mut store, false).expect("run").id;
    drop(store);
    let root = temp.path().join("prs/github.com");
    let uppercase = root.join("AAA/repo/1");
    fs::create_dir_all(&uppercase).expect("uppercase");
    fs::copy(
        root.join("octo/repo/42/store.db"),
        uppercase.join("store.db"),
    )
    .expect("copy store");

    fs::write(root.join("afile"), "file").expect("file");
    let junk = root.join("o/r");
    fs::create_dir_all(junk.join("abc")).expect("abc");
    fs::write(junk.join("abc/store.db"), "text").expect("db");
    fs::create_dir_all(junk.join("5")).expect("missing");
    fs::create_dir_all(junk.join("6")).expect("empty");
    fs::write(junk.join("6/store.db"), []).expect("empty");
    fs::create_dir_all(junk.join("8")).expect("bad");
    fs::write(junk.join("8/store.db"), "not a database").expect("bad db");
    let case_sensitive = !root.join("aaa").exists();
    let found = Store::open_for_run(temp.path(), &run).expect("found");
    assert_eq!(found.dir(), root.join("octo/repo/42"));
    drop(found);
    if case_sensitive {
        assert!(
            !root.join("aaa").exists(),
            "mixed-case candidate was reopened"
        );
    }
    assert_eq!(
        fs::metadata(junk.join("6/store.db")).expect("empty").len(),
        0
    );
    assert_eq!(
        fs::read(junk.join("8/store.db")).expect("bad"),
        b"not a database"
    );
    for path in [
        junk.join("5/store.db"),
        junk.join("6/store.db-wal"),
        junk.join("6/store.db-shm"),
        junk.join("8/store.db-wal"),
        junk.join("8/store.db-shm"),
    ] {
        assert!(!path.exists(), "{}", path.display());
    }
}

#[test]
fn open_for_run_chooses_first_sorted_store_for_duplicate_run() {
    let temp = tempdir().expect("temp");
    let source_pr = PrRef::new("bbb", "repo", 1).expect("pr");
    let mut source = Store::open_in(temp.path(), &source_pr).expect("source");
    let run = runs::start_run(&mut source, false).expect("run").id;
    let source_db = source.dir().join("store.db");
    drop(source);
    let conn = Connection::open(&source_db).expect("checkpoint connection");
    conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")
        .expect("checkpoint");
    drop(conn);
    for owner in ["aaa", "ccc"] {
        let dir = temp.path().join(format!("prs/github.com/{owner}/repo/1"));
        fs::create_dir_all(&dir).expect("candidate directory");
        fs::copy(&source_db, dir.join("store.db")).expect("copy store");
    }
    let found = Store::open_for_run(temp.path(), &run).expect("found");
    assert_eq!(found.dir(), temp.path().join("prs/github.com/aaa/repo/1"));
}

#[cfg(unix)]
#[test]
fn open_for_run_skips_unreadable_candidate_directory() {
    use std::os::unix::fs::PermissionsExt;

    struct PermissionRestore(PathBuf);

    impl Drop for PermissionRestore {
        fn drop(&mut self) {
            let _ = fs::set_permissions(&self.0, fs::Permissions::from_mode(0o755));
        }
    }

    let (temp, mut store, _clock) = setup();
    let run = runs::start_run(&mut store, false).expect("run").id;
    let unreadable_pr = PrRef::new("aaa", "repo", 1).expect("pr");
    let mut unreadable_store = Store::open_in(temp.path(), &unreadable_pr).expect("candidate");
    let _other = runs::start_run(&mut unreadable_store, false)
        .expect("other")
        .id;
    drop(unreadable_store);
    let unreadable_owner = temp.path().join("prs/github.com/aaa");
    fs::set_permissions(&unreadable_owner, fs::Permissions::from_mode(0o000))
        .expect("remove read permission");
    let restore = PermissionRestore(unreadable_owner);
    let found = Store::open_for_run(temp.path(), &run).expect("find readable store");
    assert_eq!(found.dir(), store.dir());
    drop(restore);
}

#[test]
fn event_error_codes_are_stable() {
    let cases = [
        (
            StoreError::EventUnknown {
                event_id: "e".into(),
            },
            "event_unknown",
            "no event e in this store",
        ),
        (
            StoreError::EventNotClaimed {
                event_id: "e".into(),
                run_id: "r".into(),
            },
            "event_not_claimed",
            "event e is not claimed by run r",
        ),
        (
            StoreError::WaitActive {
                path: "/tmp/wait.lock".into(),
            },
            "wait_active",
            "another prguide wait is already listening on this PR (it holds /tmp/wait.lock)",
        ),
    ];
    for (error, code, message) in cases {
        assert_eq!(error.code(), code);
        assert_eq!(error.to_string(), message);
    }
}

struct ChildGuard {
    child: Option<Child>,
}

impl ChildGuard {
    fn kill(&mut self) -> std::io::Result<()> {
        self.child.as_mut().expect("child").kill()
    }

    fn wait(&mut self) -> std::io::Result<std::process::ExitStatus> {
        self.child.as_mut().expect("child").wait()
    }

    fn into_child(mut self) -> Child {
        self.child.take().expect("child")
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn child(home: &TempDir, args: &[&str]) -> ChildGuard {
    ChildGuard {
        child: Some(
            Command::new(env!("CARGO_BIN_EXE_prguide"))
                .args(args)
                .env("PRGUIDE_HOME", home.path())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .expect("child"),
        ),
    }
}

fn child_output(home: &TempDir, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_prguide"))
        .args(args)
        .env("PRGUIDE_HOME", home.path())
        .output()
        .expect("child")
}

fn wait_child(child: ChildGuard) -> std::process::Output {
    let child = child.into_child();
    let (sent, received) = mpsc::channel();
    std::thread::spawn(move || {
        sent.send(child.wait_with_output().expect("output"))
            .expect("send");
    });
    received
        .recv_timeout(Duration::from_secs(10))
        .expect("child hung")
}

fn json_error(output: &std::process::Output, code: &str, contains: &str) {
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    let value: Value = serde_json::from_slice(&output.stderr).expect("error json");
    assert_eq!(value["code"], code);
    assert!(
        value["message"]
            .as_str()
            .expect("message")
            .contains(contains)
    );
}

#[test]
fn cli_wait_returns_stopped_within_1s_when_the_run_stops() {
    let temp = tempdir().expect("temp");
    let mut store = system_store(&temp);
    let run = runs::start_run(&mut store, false).expect("run").id;
    let listener = child(&temp, &["wait", "--run", run.as_str()]);
    wait_for_lock(&store.dir().join(events::WAIT_LOCK));
    let start = Instant::now();
    runs::stop_run(&mut store, &run, StopReason::End).expect("stop");
    let output = wait_child(listener);
    assert!(start.elapsed() < Duration::from_secs(1));
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(output.stdout, b"{\"stopped\":true}\n");
    assert!(output.stderr.is_empty());
}

#[test]
fn cli_second_wait_is_wait_active() {
    let temp = tempdir().expect("temp");
    let mut store = system_store(&temp);
    let run = runs::start_run(&mut store, false).expect("run").id;
    let listener = child(&temp, &["wait", "--run", run.as_str()]);
    wait_for_lock(&store.dir().join(events::WAIT_LOCK));
    json_error(
        &child_output(&temp, &["wait", "--run", run.as_str(), "--timeout", "0s"]),
        "wait_active",
        "wait.lock",
    );
    runs::stop_run(&mut store, &run, StopReason::End).expect("stop");
    assert_eq!(wait_child(listener).stdout, b"{\"stopped\":true}\n");
}

#[test]
fn cli_killed_wait_releases_the_lock() {
    let temp = tempdir().expect("temp");
    let mut store = system_store(&temp);
    let run = runs::start_run(&mut store, false).expect("run").id;
    let mut listener = child(&temp, &["wait", "--run", run.as_str()]);
    wait_for_lock(&store.dir().join(events::WAIT_LOCK));
    listener.kill().expect("kill");
    let _ = listener.wait().expect("reap");
    let output = child_output(&temp, &["wait", "--run", run.as_str(), "--timeout", "0s"]);
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(output.stdout, b"{\"events\":[]}\n");
}

#[test]
fn cli_wait_prints_events_and_bounds_the_batch() {
    let temp = tempdir().expect("temp");
    let mut store = system_store(&temp);
    let run = runs::start_run(&mut store, false).expect("run").id;
    let ids = (0..15)
        .map(|_| append_generate(&mut store))
        .collect::<Vec<_>>();
    let first_event = format!(
        "{{\"events\":[{{\"id\":\"{}\",\"seq\":1,\"type\":\"generate_request\",\"thread\":null,\"message\":null,\"range\":null}}",
        ids[0]
    );
    let output = child_output(&temp, &["wait", "--run", run.as_str(), "--timeout", "0s"]);
    assert_eq!(output.status.code(), Some(0));
    assert!(
        output.stdout.starts_with(first_event.as_bytes()),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    let value: Value = serde_json::from_slice(&output.stdout).expect("json");
    let event_seqs = value["events"]
        .as_array()
        .expect("events")
        .iter()
        .map(|event| event["seq"].as_i64().expect("seq"))
        .collect::<Vec<_>>();
    assert_eq!(event_seqs, (1..=10).collect::<Vec<_>>());
    runs::stop_run(&mut store, &run, StopReason::End).expect("stop");
    let second = runs::start_run(&mut store, false).expect("run").id;
    let output = child_output(
        &temp,
        &[
            "wait",
            "--run",
            second.as_str(),
            "--max",
            "3",
            "--timeout",
            "0s",
        ],
    );
    assert_eq!(output.status.code(), Some(0));
    let value: Value = serde_json::from_slice(&output.stdout).expect("json");
    assert_eq!(value["events"].as_array().expect("events").len(), 3);
}
#[test]
fn cli_wait_timeout_returns_empty_events() {
    let temp = tempdir().expect("temp");
    let mut store = system_store(&temp);
    let run = runs::start_run(&mut store, false).expect("run").id;
    let start = Instant::now();
    let output = child_output(&temp, &["wait", "--run", run.as_str(), "--timeout", "1s"]);
    assert!(start.elapsed() >= Duration::from_secs(1));
    assert!(start.elapsed() < Duration::from_secs(5));
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(output.stdout, b"{\"events\":[]}\n");
    assert!(output.stderr.is_empty());
}

#[test]
fn cli_wait_on_a_stopped_run_prints_stopped() {
    let temp = tempdir().expect("temp");
    let mut store = system_store(&temp);
    let run = runs::start_run(&mut store, false).expect("run").id;
    runs::stop_run(&mut store, &run, StopReason::End).expect("stop");
    let output = child_output(&temp, &["wait", "--run", run.as_str()]);
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(output.stdout, b"{\"stopped\":true}\n");
}

#[test]
fn cli_unknown_run_is_run_unknown() {
    let temp = tempdir().expect("temp");
    let _store = system_store(&temp);
    for args in [
        ["wait", "--run", "r_000000000000", "--timeout", "0s"].as_slice(),
        ["ack", "e", "--run", "r_000000000000"].as_slice(),
    ] {
        json_error(&child_output(&temp, args), "run_unknown", "r_000000000000");
    }
}

#[test]
fn cli_bad_timeout_and_max_are_usage() {
    let temp = tempdir().expect("temp");
    for input in [
        "",
        "4",
        "m",
        "4x",
        "-1s",
        "+4m",
        " 4m",
        "4m ",
        "4 m",
        "1.5m",
        "4M",
        "4S",
        "4m30s",
        "4ms",
        "٤m",
        "18446744073709551616s",
        "307445734561825861m",
    ] {
        let output = child_output(
            &temp,
            &["wait", "--run", "r_x", &format!("--timeout={input}")],
        );
        json_error(&output, "usage", input);
        let value: Value = serde_json::from_slice(&output.stderr).expect("error json");
        assert!(
            value["message"]
                .as_str()
                .expect("message")
                .contains("invalid --timeout")
        );
    }
    json_error(
        &child_output(&temp, &["wait", "--run", "r_x", "--max", "0"]),
        "usage",
        "--max",
    );
    assert!(fs::read_dir(temp.path()).expect("empty").next().is_none());
}

#[test]
fn cli_ack_prints_the_result_and_repeats() {
    let temp = tempdir().expect("temp");
    let mut store = system_store(&temp);
    let run = runs::start_run(&mut store, false).expect("run").id;
    let id = append_generate(&mut store);
    let _ = events::wait(&mut store, &run, Some(Duration::ZERO), nz(10)).expect("claim");
    let expected =
        format!("{{\"event\":\"{id}\",\"result_kind\":\"ack\",\"result_id\":\"{id}\"}}\n");
    for _ in 0..2 {
        let output = child_output(&temp, &["ack", &id, "--run", run.as_str()]);
        assert_eq!(output.status.code(), Some(0));
        assert_eq!(output.stdout, expected.as_bytes());
    }
    let row = event_row(&mut store, &id);
    assert_eq!(row.0, "done");
    assert_eq!(row.3.as_deref(), Some("ack"));
    assert_eq!(row.4.as_deref(), Some(id.as_str()));
    let pending = append_generate(&mut store);
    json_error(
        &child_output(&temp, &["ack", "none", "--run", run.as_str()]),
        "event_unknown",
        "none",
    );
    json_error(
        &child_output(&temp, &["ack", &pending, "--run", run.as_str()]),
        "event_not_claimed",
        &pending,
    );
    runs::stop_run(&mut store, &run, StopReason::End).expect("stop");
    json_error(
        &child_output(&temp, &["ack", &id, "--run", run.as_str()]),
        "run_stopped",
        run.as_str(),
    );
}

#[test]
fn cli_ack_after_a_reply_prints_the_stored_result() {
    let temp = tempdir().expect("temp");
    let mut store = system_store(&temp);
    let run = runs::start_run(&mut store, false).expect("run").id;
    let id = append_generate(&mut store);
    let _ = events::wait(&mut store, &run, Some(Duration::ZERO), nz(10)).expect("claim");
    let _ = events::with_result(&mut store, &run, &id, ResultKind::Message, |_| {
        Ok::<_, StoreError>("m1".into())
    })
    .expect("reply");
    let output = child_output(&temp, &["ack", &id, "--run", run.as_str()]);
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(
        output.stdout,
        format!("{{\"event\":\"{id}\",\"result_kind\":\"message\",\"result_id\":\"m1\"}}\n")
            .as_bytes()
    );
}
