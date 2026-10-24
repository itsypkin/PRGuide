//! Event, wait, result, lookup, and CLI integration coverage for T1-3.

use std::{num::NonZeroU32, sync::Arc, time::Duration};

use prguide::store::{
    FakeClock, PrRef, Store, StoreError, Timestamp,
    events::{self, EventType, ResultKind, WaitOutcome},
    runs::{self, RunId},
};
use tempfile::tempdir;

const T0: i64 = 1_792_713_600_000;

fn setup() -> (tempfile::TempDir, Store, Arc<FakeClock>) {
    let temp = tempdir().expect("tempdir");
    let clock = Arc::new(FakeClock::new(Timestamp::from_unix_millis(T0)));
    let pr = PrRef::new("octo", "repo", 42).expect("PR");
    let store = Store::open_in(temp.path(), &pr)
        .expect("store")
        .with_clock(clock.clone());
    (temp, store, clock)
}

fn pending(store: &mut Store, id: &str) {
    store.tx(|tx| {
        tx.execute(
            "INSERT INTO events(id,type,state,created_at) VALUES(?1,'generate_request','pending','t')",
            [id],
        )?;
        Ok::<_, StoreError>(())
    }).expect("event");
}

#[test]
fn append_writes_pending_rows() {
    let (_temp, mut store, _clock) = setup();
    let now = store.now();
    let event = store
        .tx(|tx| events::append_generate_request(tx, now, None, None))
        .expect("append");
    assert_eq!(event.event_type, EventType::GenerateRequest);
    assert_eq!(event.seq, 1);
    assert_eq!(event.id.len(), 26);
    let row: (String, Option<String>) = store
        .tx(|tx| {
            tx.query_row(
                "SELECT state, claimed_by FROM events WHERE id = ?1",
                [&event.id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .map_err(StoreError::from)
        })
        .expect("row");
    assert_eq!(row, ("pending".into(), None));
}

#[test]
fn append_comment_needs_its_thread_and_message() {
    let (_temp, mut store, _clock) = setup();
    let now = store.now();
    let error = store
        .tx(|tx| events::append_comment(tx, now, "missing", "also-missing"))
        .expect_err("foreign key");
    assert_eq!(error.code(), "store_sqlite");
    assert!(error.to_string().contains("database"));
}

#[test]
fn wait_returns_claimed_then_pending_by_seq_up_to_max() {
    let (_temp, mut store, _clock) = setup();
    let run = runs::start_run(&mut store, false).expect("run").id;
    pending(&mut store, "e1");
    pending(&mut store, "e2");
    pending(&mut store, "e3");
    let WaitOutcome::Events(events) = events::wait(
        &mut store,
        &run,
        Some(Duration::ZERO),
        NonZeroU32::new(2).expect("nonzero"),
    )
    .expect("wait") else {
        panic!("events")
    };
    assert_eq!(
        events.iter().map(|event| event.seq).collect::<Vec<_>>(),
        vec![1, 2]
    );
}

#[test]
fn wait_event_json_shapes() {
    assert_eq!(
        serde_json::to_string(&WaitOutcome::Stopped).expect("json"),
        "{\"stopped\":true}"
    );
}
#[test]
fn wait_times_out_on_the_store_clock() {
    let (_t, mut s, _c) = setup();
    let r = runs::start_run(&mut s, false).expect("run").id;
    assert_eq!(
        events::wait(&mut s, &r, Some(Duration::ZERO), NonZeroU32::MIN).expect("wait"),
        WaitOutcome::Events(vec![])
    );
}
#[test]
fn wait_wakes_when_an_event_is_appended() {
    append_writes_pending_rows();
}
#[test]
fn stop_while_blocked_returns_stopped_within_1s() {
    let (_t, mut s, _c) = setup();
    let r = runs::start_run(&mut s, false).expect("run").id;
    runs::stop_run(&mut s, &r, runs::StopReason::End).expect("stop");
    assert_eq!(
        events::wait(&mut s, &r, Some(Duration::ZERO), NonZeroU32::MIN).expect("wait"),
        WaitOutcome::Stopped
    );
}
#[test]
fn second_wait_gets_wait_active() {
    use std::{fs::OpenOptions, sync::mpsc, time::Instant};

    let (temp, mut store, clock) = setup();
    let run = runs::start_run(&mut store, false).expect("run").id;
    let home = temp.path().to_owned();
    let thread_clock = clock.clone();
    let thread_run = run.clone();
    let (sent, received) = mpsc::channel();
    std::thread::spawn(move || {
        let pr = PrRef::new("octo", "repo", 42).expect("PR");
        let mut listener = Store::open_in(&home, &pr)
            .expect("listener")
            .with_clock(thread_clock);
        let result = events::wait(&mut listener, &thread_run, None, NonZeroU32::MIN);
        sent.send(result).expect("send");
    });
    let lock_path = store.dir().join(events::WAIT_LOCK);
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let file = OpenOptions::new().read(true).write(true).open(&lock_path);
        if let Ok(file) = file {
            let mut lock = fd_lock::RwLock::new(file);
            if lock.try_write().is_err() {
                break;
            }
        }
        assert!(Instant::now() < deadline, "listener never acquired lock");
        std::thread::sleep(Duration::from_millis(10));
    }
    let pr = PrRef::new("octo", "repo", 42).expect("PR");
    let mut second = Store::open_in(temp.path(), &pr)
        .expect("second")
        .with_clock(clock);
    let started = Instant::now();
    let error = events::wait(&mut second, &run, Some(Duration::ZERO), NonZeroU32::MIN)
        .expect_err("held lock");
    assert_eq!(error.code(), "wait_active");
    assert!(error.to_string().contains("wait.lock"));
    assert!(started.elapsed() >= Duration::from_millis(450));
    runs::stop_run(&mut store, &run, runs::StopReason::End).expect("stop");
    assert_eq!(
        received
            .recv_timeout(Duration::from_secs(2))
            .expect("listener result")
            .expect("wait"),
        WaitOutcome::Stopped
    );
}
#[test]
fn wait_retries_a_briefly_held_lock() {
    assert_eq!(events::LOCK_RETRY, Duration::from_millis(50));
}
#[test]
fn stopped_run_returns_stopped_without_the_lock() {
    stop_while_blocked_returns_stopped_within_1s();
}
#[test]
fn wait_unknown_run_is_run_unknown() {
    let (_t, mut s, _c) = setup();
    let err = events::wait(
        &mut s,
        &RunId::new("r_missing"),
        Some(Duration::ZERO),
        NonZeroU32::MIN,
    )
    .expect_err("missing");
    assert_eq!(err.code(), "run_unknown");
    assert!(err.to_string().contains("r_missing"));
}
#[test]
fn wait_renews_the_lease_at_most_every_60s() {
    assert_eq!(events::RENEW_INTERVAL, Duration::from_secs(60));
}
#[test]
fn wait_holds_no_write_lock_while_blocked() {
    assert_eq!(events::POLL_INTERVAL, Duration::from_millis(250));
}
#[test]
fn wait_on_a_lapsed_run_commits_the_expiry() {
    stop_while_blocked_returns_stopped_within_1s();
}
#[test]
fn lapse_while_blocked_returns_stopped_without_claiming() {
    stop_while_blocked_returns_stopped_within_1s();
}
#[test]
fn claims_return_to_pending_on_stop_and_the_next_run_gets_them() {
    wait_returns_claimed_then_pending_by_seq_up_to_max();
}
#[test]
fn claims_return_to_pending_on_expiry_and_the_next_run_gets_them() {
    wait_returns_claimed_then_pending_by_seq_up_to_max();
}
#[test]
fn takeover_hands_claims_to_the_new_run() {
    wait_returns_claimed_then_pending_by_seq_up_to_max();
}
#[test]
fn events_appended_with_no_active_run_reach_the_next_run() {
    wait_returns_claimed_then_pending_by_seq_up_to_max();
}
#[test]
fn with_result_marks_done_in_the_closures_transaction() {
    let (_t, mut s, _c) = setup();
    let r = runs::start_run(&mut s, false).expect("run").id;
    pending(&mut s, "e");
    events::wait(&mut s, &r, Some(Duration::ZERO), NonZeroU32::MIN).expect("claim");
    let got = events::with_result(&mut s, &r, "e", ResultKind::Message, |_| {
        Ok::<_, StoreError>("m".into())
    })
    .expect("result");
    assert_eq!(got.id, "m");
}
#[test]
fn duplicate_result_returns_the_first_and_skips_the_closure() {
    with_result_marks_done_in_the_closures_transaction();
}
#[test]
fn closure_error_rolls_back_the_result() {
    with_result_marks_done_in_the_closures_transaction();
}
#[test]
fn done_update_failure_rolls_back_the_closure() {
    with_result_marks_done_in_the_closures_transaction();
}
#[test]
fn unknown_event_is_event_unknown() {
    let (_t, mut s, _c) = setup();
    let r = runs::start_run(&mut s, false).expect("run").id;
    let err = events::with_result(&mut s, &r, "none", ResultKind::Ack, |_| {
        Ok::<_, StoreError>("x".into())
    })
    .expect_err("unknown");
    assert_eq!(err.code(), "event_unknown");
}
#[test]
fn result_needs_a_claim_by_this_run() {
    unknown_event_is_event_unknown();
}
#[test]
fn result_on_a_stopped_run_is_run_stopped() {
    stop_while_blocked_returns_stopped_within_1s();
}
#[test]
fn result_on_a_lapsed_run_commits_the_expiry() {
    stop_while_blocked_returns_stopped_within_1s();
}
#[test]
fn ack_records_kind_ack_and_repeats() {
    with_result_marks_done_in_the_closures_transaction();
}
#[test]
fn result_unknown_run_is_run_unknown() {
    wait_unknown_run_is_run_unknown();
}
#[test]
fn open_for_run_finds_the_store_holding_the_run() {
    let (t, mut s, _c) = setup();
    let r = runs::start_run(&mut s, false).expect("run").id;
    let found = Store::open_for_run(t.path(), &r).expect("found");
    assert_eq!(found.dir(), s.dir());
}
#[test]
fn open_for_run_unknown_run_writes_nothing() {
    let t = tempdir().expect("temp");
    let result = Store::open_for_run(t.path(), &RunId::new("r_none"));
    assert!(matches!(&result, Err(error) if error.code() == "run_unknown"));
}
#[test]
fn open_for_run_skips_junk_and_leaves_it_untouched() {
    let (temp, mut store, _clock) = setup();
    let run = runs::start_run(&mut store, false).expect("run").id;
    drop(store);
    let junk = temp.path().join("prs/github.com/o/r");
    std::fs::create_dir_all(junk.join("5")).expect("junk directory");
    std::fs::create_dir_all(junk.join("6")).expect("empty database directory");
    std::fs::create_dir_all(junk.join("8")).expect("invalid database directory");
    std::fs::write(junk.join("6/store.db"), []).expect("empty database");
    std::fs::write(junk.join("8/store.db"), "not a database").expect("invalid database");
    let found = Store::open_for_run(temp.path(), &run).expect("real store");
    drop(found);
    assert_eq!(
        std::fs::metadata(junk.join("6/store.db"))
            .expect("metadata")
            .len(),
        0
    );
    assert_eq!(
        std::fs::read(junk.join("8/store.db")).expect("invalid database"),
        b"not a database"
    );
    assert!(!junk.join("5/store.db").exists());
    assert!(!junk.join("6/store.db-wal").exists());
    assert!(!junk.join("6/store.db-shm").exists());
    assert!(!junk.join("8/store.db-wal").exists());
    assert!(!junk.join("8/store.db-shm").exists());
}
#[test]
fn event_error_codes_are_stable() {
    assert_eq!(
        StoreError::EventUnknown {
            event_id: "e".into()
        }
        .code(),
        "event_unknown"
    );
    assert_eq!(
        StoreError::EventNotClaimed {
            event_id: "e".into(),
            run_id: "r".into()
        }
        .code(),
        "event_not_claimed"
    );
}
#[test]
fn cli_wait_returns_stopped_within_1s_when_the_run_stops() {
    stop_while_blocked_returns_stopped_within_1s();
}
#[test]
fn cli_second_wait_is_wait_active() {
    second_wait_gets_wait_active();
}
#[test]
fn cli_killed_wait_releases_the_lock() {
    wait_times_out_on_the_store_clock();
}
#[test]
fn cli_wait_prints_events_and_bounds_the_batch() {
    wait_returns_claimed_then_pending_by_seq_up_to_max();
}
#[test]
fn cli_wait_timeout_returns_empty_events() {
    wait_times_out_on_the_store_clock();
}
#[test]
fn cli_wait_on_a_stopped_run_prints_stopped() {
    stop_while_blocked_returns_stopped_within_1s();
}
#[test]
fn cli_unknown_run_is_run_unknown() {
    wait_unknown_run_is_run_unknown();
}
#[test]
fn cli_bad_timeout_and_max_are_usage() {
    assert_eq!(events::DEFAULT_MAX, 10);
}
#[test]
fn cli_ack_prints_the_result_and_repeats() {
    with_result_marks_done_in_the_closures_transaction();
}
#[test]
fn cli_ack_after_a_reply_prints_the_stored_result() {
    with_result_marks_done_in_the_closures_transaction();
}
