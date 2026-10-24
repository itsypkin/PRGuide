//! Agent status and `prguide status` coverage for T1-4.

use std::{
    fs::{self, OpenOptions},
    num::NonZeroU32,
    process::{Child, Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc,
    },
    time::{Duration, Instant},
};

use prguide::store::{
    FakeClock, PrRef, Store, StoreError, SystemClock, Timestamp,
    events::{self, WaitOutcome},
    runs::{self, LEASE, RunId, StopReason},
    status::{self, AgentStatus, ReadyVersion},
};
use serde_json::Value;
use tempfile::{TempDir, tempdir};

const T0: i64 = 1_792_713_600_000;

fn nz(value: u32) -> NonZeroU32 {
    NonZeroU32::new(value).expect("non-zero")
}

fn pr_for(repo: &str, number: u64) -> PrRef {
    PrRef::new("octo", repo, number).expect("valid PR")
}

fn pr() -> PrRef {
    pr_for("repo", 42)
}

fn setup() -> (TempDir, Store, Arc<FakeClock>) {
    let temp = tempdir().expect("tempdir");
    let clock = Arc::new(FakeClock::new(Timestamp::from_unix_millis(T0)));
    let store = Store::open_in(temp.path(), &pr())
        .expect("store")
        .with_clock(clock.clone());
    (temp, store, clock)
}

fn system_store(home: &TempDir) -> Store {
    Store::open_in(home.path(), &pr()).expect("system store")
}

fn start(store: &mut Store) -> RunId {
    runs::start_run(store, false).expect("run").id
}

fn append(store: &mut Store, clock: &FakeClock, count: usize) {
    use prguide::store::Clock;
    for _ in 0..count {
        store
            .tx(|tx| events::append_generate_request(tx, clock.now(), None, None))
            .expect("append");
    }
}

fn exec(store: &mut Store, sql: &str) {
    store
        .tx(|tx| {
            tx.execute_batch(sql)?;
            Ok::<_, StoreError>(())
        })
        .expect("sql");
}

fn insert_event(store: &mut Store, id: &str, state: &str, claimed_by: Option<&str>) {
    store
        .tx(|tx| {
            tx.execute(
                "INSERT INTO events(id,type,state,claimed_by,created_at) \
                 VALUES(?1,'generate_request',?2,?3,'t')",
                rusqlite::params![id, state, claimed_by],
            )?;
            Ok::<_, StoreError>(())
        })
        .expect("event");
}

fn insert_stopped_run(store: &mut Store, run: &str) {
    exec(
        store,
        &format!("INSERT INTO runs VALUES ('{run}','stopped','t','t','t','end')"),
    );
}

fn event_row(store: &mut Store, id: &str) -> (String, Option<String>, Option<String>) {
    store
        .tx(|tx| {
            tx.query_row(
                "SELECT state,claimed_by,claimed_at FROM events WHERE id=?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .map_err(StoreError::from)
        })
        .expect("event row")
}

fn run_row(store: &mut Store, run: &str) -> (String, String, Option<String>, Option<String>) {
    store
        .tx(|tx| {
            tx.query_row(
                "SELECT state,lease_until,ended_at,end_reason FROM runs WHERE id=?1",
                [run],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .map_err(StoreError::from)
        })
        .expect("run row")
}

fn insert_ready_version(store: &mut Store, version: i64, head: &str) {
    exec(
        store,
        &format!(
            "INSERT INTO guide_versions(version,state,caused_by,snapshot_base,snapshot_head,\
             selection_kind,selection_base,selection_head,commits_json,refs_json,created_at,ready_at) \
             VALUES({version},'ready',NULL,'sb','{head}','full','xb','xh','[]','{{}}','t','2026-01-01T00:00:00.000Z')"
        ),
    );
}

fn hold_exclusive(store: &Store) -> fd_lock::RwLockWriteGuard<'static, std::fs::File> {
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(store.dir().join(events::WAIT_LOCK))
        .expect("open lock");
    let lock = Box::leak(Box::new(fd_lock::RwLock::new(file)));
    lock.write().expect("write lock")
}

fn hold_shared(store: &Store) -> fd_lock::RwLockReadGuard<'static, std::fs::File> {
    let file = OpenOptions::new()
        .read(true)
        .open(store.dir().join(events::WAIT_LOCK))
        .expect("open lock");
    let lock = Box::leak(Box::new(fd_lock::RwLock::new(file)));
    lock.read().expect("read lock")
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
        .recv_timeout(Duration::from_secs(2))
        .expect("zero-timeout wait hung")
}

fn claim(home: &TempDir, clock: &Arc<FakeClock>, run: &RunId, max: u32) -> Vec<String> {
    match wait_now(home, clock.clone(), run.clone(), max).expect("wait") {
        WaitOutcome::Events(events) => events.into_iter().map(|e| e.id).collect(),
        WaitOutcome::Stopped => panic!("stopped"),
    }
}

fn not_listening(pending: u64, run: Option<&RunId>) -> AgentStatus {
    AgentStatus::NotListening {
        pending,
        active_run: run.cloned(),
    }
}

struct ChildGuard {
    child: Option<Child>,
}

impl ChildGuard {
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

fn error_json(output: &std::process::Output, code: &str, message: &str) {
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    let value: Value = serde_json::from_slice(&output.stderr).expect("error json");
    assert_eq!(value["code"], code);
    assert_eq!(value["message"], message);
}

fn names(dir: &std::path::Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .expect("list")
        .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

// ---- library ----

#[test]
fn no_active_run_is_not_listening_without_active_run() {
    let (_temp, mut store, clock) = setup();
    append(&mut store, &clock, 2);
    insert_event(&mut store, "done1", "done", None);
    let lock = store.dir().join(events::WAIT_LOCK);
    assert!(!lock.exists());
    assert_eq!(
        status::agent_status(&mut store).expect("status"),
        not_listening(2, None)
    );
    assert!(!lock.exists());
}

#[test]
fn active_run_with_nothing_claimed_is_not_listening() {
    let (_temp, mut store, clock) = setup();
    let a = start(&mut store);
    assert_eq!(
        status::agent_status(&mut store).expect("status"),
        not_listening(0, Some(&a))
    );
    append(&mut store, &clock, 3);
    assert_eq!(
        status::agent_status(&mut store).expect("status"),
        not_listening(3, Some(&a))
    );
    assert_eq!(
        status::status_report(&mut store).expect("report").pending,
        3
    );
}

#[test]
fn claims_by_the_active_run_are_working() {
    let (temp, mut store, clock) = setup();
    let a = start(&mut store);
    append(&mut store, &clock, 3);
    let got = claim(&temp, &clock, &a, 1);
    assert_eq!(got.len(), 1);
    assert_eq!(
        status::agent_status(&mut store).expect("status"),
        AgentStatus::Working
    );
    assert_eq!(
        status::status_report(&mut store).expect("report").pending,
        2
    );
    events::ack(&mut store, &a, &got[0]).expect("ack");
    assert_eq!(
        status::agent_status(&mut store).expect("status"),
        not_listening(2, Some(&a))
    );
}

#[test]
fn claims_by_another_run_are_not_working() {
    let (_temp, mut store, _clock) = setup();
    let a = start(&mut store);
    insert_stopped_run(&mut store, "r_x");
    insert_event(&mut store, "ex", "claimed", Some("r_x"));
    insert_event(&mut store, "ep", "pending", None);
    let before = event_row(&mut store, "ex");
    assert_eq!(
        status::agent_status(&mut store).expect("status"),
        not_listening(1, Some(&a))
    );
    assert_eq!(event_row(&mut store, "ex"), before);
}

#[test]
fn a_held_wait_lock_with_an_active_run_is_listening() {
    let (_temp, mut store, clock) = setup();
    let a = start(&mut store);
    append(&mut store, &clock, 1);
    let guard = hold_exclusive(&store);
    let report = status::status_report(&mut store).expect("report");
    assert_eq!(report.agent, AgentStatus::Listening);
    assert_eq!(report.pending, 1);
    drop(guard);
    assert_eq!(
        status::agent_status(&mut store).expect("status"),
        not_listening(1, Some(&a))
    );
}

#[test]
fn listening_wins_over_working() {
    let (temp, mut store, clock) = setup();
    let a = start(&mut store);
    append(&mut store, &clock, 2);
    assert_eq!(claim(&temp, &clock, &a, 1).len(), 1);
    let guard = hold_exclusive(&store);
    assert_eq!(
        status::agent_status(&mut store).expect("status"),
        AgentStatus::Listening
    );
    drop(guard);
    assert_eq!(
        status::agent_status(&mut store).expect("status"),
        AgentStatus::Working
    );
}

#[test]
fn a_held_wait_lock_without_an_active_run_is_not_listening() {
    let (_temp, mut store, clock) = setup();
    let a = start(&mut store);
    runs::stop_run(&mut store, &a, StopReason::End).expect("stop");
    append(&mut store, &clock, 1);
    let _guard = hold_exclusive(&store);
    assert_eq!(
        status::agent_status(&mut store).expect("status"),
        not_listening(1, None)
    );
}

#[test]
fn two_concurrent_probes_dont_see_each_other() {
    let (temp, mut store, clock) = setup();
    let a = start(&mut store);
    {
        let _ = hold_exclusive(&store);
    }
    let _shared = hold_shared(&store);
    assert_eq!(
        status::agent_status(&mut store).expect("status"),
        not_listening(0, Some(&a))
    );
    let home = temp.path().to_owned();
    let second = std::thread::spawn(move || {
        let mut other = Store::open_in(&home, &pr())
            .expect("store")
            .with_clock(clock);
        status::agent_status(&mut other).expect("status")
    })
    .join()
    .expect("join");
    assert_eq!(second, not_listening(0, Some(&a)));
}

#[test]
fn the_probe_releases_the_lock_at_once() {
    let (temp, mut store, clock) = setup();
    let a = start(&mut store);
    {
        let _ = hold_exclusive(&store);
    }
    status::agent_status(&mut store).expect("status");
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(store.dir().join(events::WAIT_LOCK))
        .expect("open");
    let mut lock = fd_lock::RwLock::new(file);
    assert!(lock.try_write().is_ok());
    drop(lock);
    assert_eq!(
        wait_now(&temp, clock, a, 1).expect("wait"),
        WaitOutcome::Events(vec![])
    );
}

#[test]
fn the_probe_never_creates_the_lock_file() {
    let (_temp, mut store, _clock) = setup();
    start(&mut store);
    let before = names(store.dir());
    status::agent_status(&mut store).expect("status");
    assert_eq!(names(store.dir()), before);
    assert!(!store.dir().join(events::WAIT_LOCK).exists());
}

#[test]
fn a_real_wait_thread_makes_it_listening() {
    let (temp, mut store, clock) = setup();
    let a = start(&mut store);
    let received = spawn_wait(&temp, clock.clone(), a, None, 1);
    let deadline = Instant::now() + Duration::from_secs(5);
    while status::agent_status(&mut store).expect("status") != AgentStatus::Listening {
        assert!(Instant::now() < deadline, "never listening");
        std::thread::sleep(Duration::from_millis(10));
    }
    append(&mut store, &clock, 1);
    let outcome = received
        .recv_timeout(Duration::from_secs(2))
        .expect("wait hung")
        .expect("wait");
    let WaitOutcome::Events(got) = outcome else {
        panic!("stopped")
    };
    assert_eq!(got.len(), 1);
    assert_eq!(
        status::agent_status(&mut store).expect("status"),
        AgentStatus::Working
    );
}

#[test]
fn wait_still_wins_against_concurrent_probes() {
    let (temp, mut store, clock) = setup();
    let a = start(&mut store);
    let stop = Arc::new(AtomicBool::new(false));
    let probes = Arc::new(AtomicUsize::new(0));
    let threads: Vec<_> = (0..2)
        .map(|_| {
            let (home, clock, stop, probes) = (
                temp.path().to_owned(),
                clock.clone(),
                stop.clone(),
                probes.clone(),
            );
            std::thread::spawn(move || {
                let mut store = Store::open_in(&home, &pr())
                    .expect("store")
                    .with_clock(clock);
                while !stop.load(Ordering::Relaxed) {
                    status::agent_status(&mut store).expect("probe");
                    probes.fetch_add(1, Ordering::Relaxed);
                }
            })
        })
        .collect();
    for _ in 0..100 {
        assert_eq!(
            wait_now(&temp, clock.clone(), a.clone(), 10).expect("wait"),
            WaitOutcome::Events(vec![])
        );
    }
    stop.store(true, Ordering::Relaxed);
    for thread in threads {
        thread.join().expect("join");
    }
    assert!(probes.load(Ordering::Relaxed) >= 1);
}

#[test]
fn a_lapsed_run_is_expired_by_the_status_read() {
    let (temp, mut store, clock) = setup();
    let a = start(&mut store);
    append(&mut store, &clock, 1);
    let claimed = claim(&temp, &clock, &a, 1);
    let e1 = claimed[0].clone();
    clock.advance(LEASE);
    assert_eq!(
        status::agent_status(&mut store).expect("status"),
        not_listening(1, None)
    );
    let (state, _, ended_at, reason) = run_row(&mut store, a.as_str());
    assert_eq!(state, "expired");
    assert_eq!(reason.as_deref(), Some("expired"));
    assert!(ended_at.is_some());
    assert_eq!(event_row(&mut store, &e1), ("pending".into(), None, None));
}

#[test]
fn lock_open_errors_are_store_io() {
    let (_temp, mut store, _clock) = setup();
    let a = start(&mut store);
    let lock = store.dir().join(events::WAIT_LOCK);
    std::os::unix::fs::symlink(&lock, &lock).expect("symlink");
    let error = status::agent_status(&mut store).expect_err("error");
    assert_eq!(error.code(), "store_io");
    let message = error.to_string();
    assert!(message.contains("store I/O error at"), "{message}");
    assert!(message.contains("wait.lock"), "{message}");
    runs::stop_run(&mut store, &a, StopReason::End).expect("stop");
    assert_eq!(
        status::agent_status(&mut store).expect("status"),
        not_listening(0, None)
    );
}

#[test]
fn status_report_lists_ready_versions_oldest_first() {
    let (_temp, mut store, _clock) = setup();
    exec(
        &mut store,
        "INSERT INTO guide_versions(version,state,caused_by,snapshot_base,snapshot_head,\
         selection_kind,selection_base,selection_head,commits_json,refs_json,created_at,ready_at) VALUES \
         (1,'ready',NULL,'sb1','sh1','full','xb1','xh1','[]','{}','c1','r1'),\
         (2,'staging',NULL,'sb2','sh2','full','xb2','xh2','[]','{}','c2',NULL),\
         (3,'ready',NULL,'sb3','sh3','commits','xb3','xh3','[]','{}','c3','r3')",
    );
    let report = status::status_report(&mut store).expect("report");
    let version = |n: i64, kind: &str| ReadyVersion {
        version: n,
        selection_kind: kind.into(),
        snapshot_base: format!("sb{n}"),
        snapshot_head: format!("sh{n}"),
        selection_base: format!("xb{n}"),
        selection_head: format!("xh{n}"),
        ready_at: Some(format!("r{n}")),
    };
    assert_eq!(
        report.guide_versions,
        vec![version(1, "full"), version(3, "commits")]
    );
}

#[test]
fn agent_status_json_shapes() {
    let (temp, mut store, clock) = setup();
    let a = start(&mut store);
    append(&mut store, &clock, 1);
    let with_run = status::agent_status(&mut store).expect("status");
    let listening = {
        let _guard = hold_exclusive(&store);
        status::agent_status(&mut store).expect("status")
    };
    claim(&temp, &clock, &a, 1);
    let working = status::agent_status(&mut store).expect("status");
    runs::stop_run(&mut store, &a, StopReason::End).expect("stop");
    let without = status::agent_status(&mut store).expect("status");
    let all = [listening, working, with_run, without];
    let names: Vec<_> = all.iter().map(AgentStatus::name).collect();
    assert_eq!(
        names,
        ["listening", "working", "not_listening", "not_listening"]
    );
    let text = serde_json::to_string_pretty(&all)
        .expect("json")
        .replace(a.as_str(), "[RUN]");
    insta::assert_snapshot!("agent_status_shapes", text);
}

// ---- CLI ----

fn cli_setup() -> (TempDir, Store, RunId) {
    let home = tempdir().expect("home");
    let mut store = system_store(&home);
    let run = start(&mut store);
    (home, store, run)
}

fn redacted_stdout(output: &std::process::Output, run: &RunId) -> String {
    assert!(output.status.success(), "{:?}", output);
    assert!(output.stderr.is_empty());
    let text = String::from_utf8(output.stdout.clone()).expect("utf8");
    assert!(text.ends_with('\n'));
    assert_eq!(text.matches('\n').count(), 1);
    let id = run.as_str();
    assert!(
        id.starts_with("r_")
            && id.len() == 14
            && id[2..]
                .bytes()
                .all(|b| b.is_ascii_digit() || b.is_ascii_lowercase())
    );
    let value: Value = serde_json::from_str(&text).expect("json");
    for key in ["created_at", "lease_until"] {
        let time = value["run"][key].as_str().expect("time");
        assert_eq!(time.len(), 24);
        assert!(time.contains('T') && time.ends_with('Z'));
    }
    let mut out = text.trim_end_matches('\n').replace(id, "[RUN]");
    for key in ["created_at", "lease_until"] {
        let time = value["run"][key].as_str().expect("time");
        out = out.replace(time, "[TIME]");
    }
    out
}

fn status_out(home: &TempDir, run: &RunId) -> std::process::Output {
    child_output(home, &["status", "--run", run.as_str()])
}

#[test]
fn cli_status_not_listening_snapshot() {
    let (home, mut store, run) = cli_setup();
    let clock = Arc::new(FakeClock::new(Timestamp::from_unix_millis(T0)));
    append(&mut store, &clock, 2);
    insert_ready_version(&mut store, 1, "h1");
    exec(
        &mut store,
        "INSERT INTO guide_versions(version,state,caused_by,snapshot_base,snapshot_head,\
         selection_kind,selection_base,selection_head,commits_json,refs_json,created_at,ready_at) \
         VALUES(2,'staging',NULL,'sb','h2','full','xb','xh','[]','{}','t',NULL)",
    );
    let output = status_out(&home, &run);
    insta::assert_snapshot!("cli_not_listening", redacted_stdout(&output, &run));
    assert!(!store.dir().join(events::WAIT_LOCK).exists());
}

#[test]
fn cli_status_working_snapshot() {
    let (home, mut store, run) = cli_setup();
    let clock = Arc::new(FakeClock::new(Timestamp::from_unix_millis(T0)));
    append(&mut store, &clock, 3);
    let waiter = child(
        &home,
        &[
            "wait",
            "--run",
            run.as_str(),
            "--max",
            "1",
            "--timeout",
            "0s",
        ],
    );
    let out = wait_child(waiter);
    assert!(out.status.success());
    let output = status_out(&home, &run);
    insta::assert_snapshot!("cli_working", redacted_stdout(&output, &run));
}

#[test]
fn cli_status_listening_snapshot() {
    let (home, mut store, run) = cli_setup();
    let waiter = child(&home, &["wait", "--run", run.as_str()]);
    wait_for_lock(&store.dir().join(events::WAIT_LOCK));
    let output = status_out(&home, &run);
    insta::assert_snapshot!("cli_listening", redacted_stdout(&output, &run));
    runs::stop_run(&mut store, &run, StopReason::End).expect("stop");
    let out = wait_child(waiter);
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim(),
        r#"{"stopped":true}"#
    );
}

fn rfc3339_plus(seconds: u64) -> String {
    use prguide::store::Clock;
    SystemClock
        .now()
        .saturating_add(Duration::from_secs(seconds))
        .to_rfc3339()
}

#[test]
fn cli_status_renews_the_lease() {
    let (home, mut store, run) = cli_setup();
    exec(
        &mut store,
        &format!(
            "UPDATE runs SET lease_until='{}' WHERE id='{}'",
            rfc3339_plus(60),
            run
        ),
    );
    let before = rfc3339_plus(LEASE.as_secs());
    let output = status_out(&home, &run);
    assert!(output.status.success());
    let value: Value = serde_json::from_slice(&output.stdout).expect("json");
    let stdout_lease = value["run"]["lease_until"].as_str().expect("lease");
    let (_, db_lease, _, _) = run_row(&mut store, run.as_str());
    assert!(db_lease >= before, "{db_lease} < {before}");
    assert_eq!(db_lease, stdout_lease);
}

#[test]
fn cli_status_on_a_stopped_run_is_run_stopped() {
    let (home, mut store, run) = cli_setup();
    runs::stop_run(&mut store, &run, StopReason::End).expect("stop");
    let before = run_row(&mut store, run.as_str());
    let output = status_out(&home, &run);
    error_json(&output, "run_stopped", &format!("run {run} has stopped"));
    assert_eq!(run_row(&mut store, run.as_str()), before);
}

#[test]
fn cli_status_on_a_lapsed_lease_is_run_stopped_and_commits_the_expiry() {
    let (home, mut store, run) = cli_setup();
    let clock = Arc::new(FakeClock::new(Timestamp::from_unix_millis(T0)));
    append(&mut store, &clock, 1);
    let out = wait_child(child(
        &home,
        &["wait", "--run", run.as_str(), "--timeout", "0s"],
    ));
    assert!(out.status.success());
    let claimed: serde_json::Value = serde_json::from_slice(&out.stdout).expect("wait json");
    assert_eq!(claimed["events"].as_array().expect("events").len(), 1);
    exec(
        &mut store,
        &format!("UPDATE runs SET lease_until='2000-01-01T00:00:00.000Z' WHERE id='{run}'"),
    );
    let output = status_out(&home, &run);
    error_json(&output, "run_stopped", &format!("run {run} has stopped"));
    let (state, _, _, reason) = run_row(&mut store, run.as_str());
    assert_eq!(state, "expired");
    assert_eq!(reason.as_deref(), Some("expired"));
    let id = store
        .tx(|tx| {
            tx.query_row("SELECT id FROM events", [], |r| r.get::<_, String>(0))
                .map_err(StoreError::from)
        })
        .expect("id");
    assert_eq!(event_row(&mut store, &id), ("pending".into(), None, None));
}

#[test]
fn cli_status_unknown_run_is_run_unknown() {
    let home = tempdir().expect("home");
    let output = child_output(&home, &["status", "--run", "r_nope"]);
    error_json(&output, "run_unknown", "no run r_nope in this store");
    assert!(names(home.path()).is_empty());
    let _other = cli_setup_in(&home);
    let output = child_output(&home, &["status", "--run", "r_nope"]);
    error_json(&output, "run_unknown", "no run r_nope in this store");
}

fn cli_setup_in(home: &TempDir) -> RunId {
    let mut store = system_store(home);
    start(&mut store)
}

#[test]
fn cli_status_finds_the_run_s_store() {
    let home = tempdir().expect("home");
    let mut store_a = system_store(&home);
    let a = start(&mut store_a);
    insert_ready_version(&mut store_a, 1, "aaa");
    let mut store_b = Store::open_in(home.path(), &pr_for("other", 7)).expect("store");
    let b = start(&mut store_b);
    insert_ready_version(&mut store_b, 1, "bbb");
    let out_b = String::from_utf8(status_out(&home, &b).stdout).expect("utf8");
    assert!(out_b.contains("bbb") && !out_b.contains("aaa"), "{out_b}");
    let out_a = String::from_utf8(status_out(&home, &a).stdout).expect("utf8");
    assert!(out_a.contains("aaa") && !out_a.contains("bbb"), "{out_a}");
}
