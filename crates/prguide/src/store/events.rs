//! Event storage, delivery, and idempotent results.

use std::{
    fs::OpenOptions,
    io::ErrorKind,
    num::NonZeroU32,
    time::{Duration, Instant},
};

use fd_lock::RwLock;
use rusqlite::{OptionalExtension, Transaction, params};
use serde::{Serialize, Serializer, ser::SerializeMap};

use super::{
    Store, StoreError, Timestamp,
    runs::{self, RunGate, RunId},
};

/// `wait`'s batch bound when `--max` is not given.
pub const DEFAULT_MAX: u32 = 10;
/// How often a blocked `wait` looks at the store.
pub const POLL_INTERVAL: Duration = Duration::from_millis(250);
/// A blocked `wait` passes the run gate (renewing the lease) at most this often.
pub const RENEW_INTERVAL: Duration = Duration::from_secs(60);
/// The lock file in the store directory that a blocked `wait` holds.
pub const WAIT_LOCK: &str = "wait.lock";
/// How long `wait` keeps retrying a held lock before `wait_active`, and how often.
pub const LOCK_PATIENCE: Duration = Duration::from_millis(500);
pub const LOCK_RETRY: Duration = Duration::from_millis(50);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EventType {
    Comment,
    DeepdiveRequest,
    GenerateRequest,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct EventRange {
    pub first: Option<String>,
    pub last: Option<String>,
}

/// One event as `wait` returns it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Event {
    pub id: String,
    pub seq: i64,
    #[serde(rename = "type")]
    pub event_type: EventType,
    pub thread: Option<String>,
    pub message: Option<String>,
    pub range: Option<EventRange>,
}

/// The outcome of one call to `wait`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WaitOutcome {
    Events(Vec<Event>),
    Stopped,
}

impl Serialize for WaitOutcome {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut map = serializer.serialize_map(Some(1))?;
        match self {
            Self::Events(events) => map.serialize_entry("events", events)?,
            Self::Stopped => map.serialize_entry("stopped", &true)?,
        }
        map.end()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ResultKind {
    Message,
    DeepdiveSection,
    OutboxItem,
    GuideVersion,
    Ack,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ResultRef {
    pub kind: ResultKind,
    pub id: String,
}

fn event_type_name(value: EventType) -> &'static str {
    match value {
        EventType::Comment => "comment",
        EventType::DeepdiveRequest => "deepdive_request",
        EventType::GenerateRequest => "generate_request",
    }
}

fn result_kind_name(value: ResultKind) -> &'static str {
    match value {
        ResultKind::Message => "message",
        ResultKind::DeepdiveSection => "deepdive_section",
        ResultKind::OutboxItem => "outbox_item",
        ResultKind::GuideVersion => "guide_version",
        ResultKind::Ack => "ack",
    }
}

fn result_kind(value: String) -> Result<ResultKind, StoreError> {
    match value.as_str() {
        "message" => Ok(ResultKind::Message),
        "deepdive_section" => Ok(ResultKind::DeepdiveSection),
        "outbox_item" => Ok(ResultKind::OutboxItem),
        "guide_version" => Ok(ResultKind::GuideVersion),
        "ack" => Ok(ResultKind::Ack),
        _ => Err(StoreError::Sqlite(
            rusqlite::Error::FromSqlConversionFailure(
                0,
                rusqlite::types::Type::Text,
                format!("unknown result kind {value}").into(),
            ),
        )),
    }
}

fn row_event(row: &rusqlite::Row<'_>) -> rusqlite::Result<Event> {
    let event_type = match row.get::<_, String>(2)?.as_str() {
        "comment" => EventType::Comment,
        "deepdive_request" => EventType::DeepdiveRequest,
        "generate_request" => EventType::GenerateRequest,
        value => {
            return Err(rusqlite::Error::FromSqlConversionFailure(
                2,
                rusqlite::types::Type::Text,
                format!("unknown event type {value}").into(),
            ));
        }
    };
    let first: Option<String> = row.get(5)?;
    let last: Option<String> = row.get(6)?;
    Ok(Event {
        id: row.get(0)?,
        seq: row.get(1)?,
        event_type,
        thread: row.get(3)?,
        message: row.get(4)?,
        range: (first.is_some() || last.is_some()).then_some(EventRange { first, last }),
    })
}

fn insert_event(
    tx: &Transaction<'_>,
    now: Timestamp,
    event_type: EventType,
    thread: Option<&str>,
    message: Option<&str>,
    first: Option<&str>,
    last: Option<&str>,
) -> Result<Event, StoreError> {
    let id = ulid::Ulid::generate().to_string();
    tx.execute(
        "INSERT INTO events (id,type,state,claimed_by,claimed_at,thread_id,message_id,range_first,range_last,result_kind,result_id,done_at,created_at) VALUES (?1,?2,'pending',NULL,NULL,?3,?4,?5,?6,NULL,NULL,NULL,?7)",
        params![id, event_type_name(event_type), thread, message, first, last, now.to_rfc3339()],
    )?;
    let seq = tx.last_insert_rowid();
    Ok(Event {
        id,
        seq,
        event_type,
        thread: thread.map(str::to_owned),
        message: message.map(str::to_owned),
        range: (first.is_some() || last.is_some()).then(|| EventRange {
            first: first.map(str::to_owned),
            last: last.map(str::to_owned),
        }),
    })
}

pub fn append_comment(
    tx: &Transaction<'_>,
    now: Timestamp,
    thread_id: &str,
    message_id: &str,
) -> Result<Event, StoreError> {
    insert_event(
        tx,
        now,
        EventType::Comment,
        Some(thread_id),
        Some(message_id),
        None,
        None,
    )
}
pub fn append_deepdive_request(
    tx: &Transaction<'_>,
    now: Timestamp,
    thread_id: &str,
) -> Result<Event, StoreError> {
    insert_event(
        tx,
        now,
        EventType::DeepdiveRequest,
        Some(thread_id),
        None,
        None,
        None,
    )
}
pub fn append_generate_request(
    tx: &Transaction<'_>,
    now: Timestamp,
    first: Option<&str>,
    last: Option<&str>,
) -> Result<Event, StoreError> {
    insert_event(tx, now, EventType::GenerateRequest, None, None, first, last)
}

fn batch(
    tx: &Transaction<'_>,
    run_id: &RunId,
    now: Timestamp,
    max: NonZeroU32,
) -> Result<Vec<Event>, StoreError> {
    let limit = i64::from(max.get());
    let mut events = Vec::new();
    let mut stmt = tx.prepare("SELECT id,seq,type,thread_id,message_id,range_first,range_last FROM events WHERE state = 'claimed' AND claimed_by = ?1 ORDER BY seq LIMIT ?2")?;
    let claimed = stmt
        .query_map(params![run_id.as_str(), limit], row_event)?
        .collect::<Result<Vec<_>, _>>()?;
    events.extend(claimed);
    let remaining = limit.saturating_sub(i64::try_from(events.len()).unwrap_or(i64::MAX));
    if remaining > 0 {
        let mut stmt = tx.prepare("SELECT id,seq,type,thread_id,message_id,range_first,range_last FROM events WHERE state = 'pending' ORDER BY seq LIMIT ?1")?;
        let pending = stmt
            .query_map([remaining], row_event)?
            .collect::<Result<Vec<_>, _>>()?;
        for event in &pending {
            tx.execute("UPDATE events SET state = 'claimed', claimed_by = ?1, claimed_at = ?2 WHERE id = ?3 AND state = 'pending'", params![run_id.as_str(), now.to_rfc3339(), event.id])?;
        }
        events.extend(pending);
    }
    Ok(events)
}
fn probe_wait_lock(lock: &mut RwLock<std::fs::File>) -> std::io::Result<()> {
    let guard = lock.try_write()?;
    drop(guard);
    Ok(())
}

fn acquire_wait_lock<'a>(
    lock: &'a mut RwLock<std::fs::File>,
    path: &std::path::Path,
    start: Instant,
) -> Result<fd_lock::RwLockWriteGuard<'a, std::fs::File>, StoreError> {
    loop {
        match probe_wait_lock(lock) {
            Ok(()) => {
                return lock.try_write().map_err(|source| StoreError::Io {
                    path: path.to_owned(),
                    source,
                });
            }
            Err(error)
                if error.kind() == ErrorKind::WouldBlock && start.elapsed() < LOCK_PATIENCE =>
            {
                std::thread::sleep(LOCK_RETRY);
            }
            Err(error) if error.kind() == ErrorKind::WouldBlock => {
                return Err(StoreError::WaitActive {
                    path: path.to_owned(),
                });
            }
            Err(source) => {
                return Err(StoreError::Io {
                    path: path.to_owned(),
                    source,
                });
            }
        }
    }
}
fn check_wait(store: &Store, run_id: &RunId) -> Result<(bool, bool), StoreError> {
    store.conn.query_row(
        "SELECT (SELECT state FROM runs WHERE id = ?1), EXISTS (SELECT 1 FROM events WHERE state = 'pending' OR (state = 'claimed' AND claimed_by = ?1))",
        [run_id.as_str()],
        |row| Ok((row.get::<_, Option<String>>(0)?.as_deref() == Some("active"), row.get::<_, i64>(1)? != 0)),
    ).map_err(StoreError::from)
}

enum GateBatch {
    Stopped,
    Events(Vec<Event>),
}

/// The listening loop. `None` timeout waits until events arrive or the run stops.
pub fn wait(
    store: &mut Store,
    run_id: &RunId,
    timeout: Option<Duration>,
    max: NonZeroU32,
) -> Result<WaitOutcome, StoreError> {
    let clock = store.clock();
    let start = clock.now();
    let initial = store.tx(|tx| runs::gate(tx, start, run_id))?;
    let RunGate::Active(_) = initial else {
        return Ok(WaitOutcome::Stopped);
    };
    let deadline = timeout.map(|duration| start.saturating_add(duration));
    let lock_path = store.dir().join(WAIT_LOCK);
    let lock_file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&lock_path)
        .map_err(|source| StoreError::Io {
            path: lock_path.clone(),
            source,
        })?;
    let mut lock = RwLock::new(lock_file);
    let _guard = acquire_wait_lock(&mut lock, &lock_path, Instant::now())?;
    let mut last_gate = start;
    loop {
        let now = clock.now();
        let (active, claimable) = check_wait(store, run_id)?;
        if !active {
            return Ok(WaitOutcome::Stopped);
        }
        if claimable || now >= last_gate.saturating_add(RENEW_INTERVAL) {
            let current = clock.now();
            let outcome = store.tx(|tx| match runs::gate(tx, current, run_id)? {
                RunGate::Stopped(_) => Ok::<_, StoreError>(GateBatch::Stopped),
                RunGate::Active(_) => Ok(GateBatch::Events(batch(tx, run_id, current, max)?)),
            })?;
            match outcome {
                GateBatch::Stopped => return Ok(WaitOutcome::Stopped),
                GateBatch::Events(events) if !events.is_empty() => {
                    return Ok(WaitOutcome::Events(events));
                }
                GateBatch::Events(_) => last_gate = now,
            }
        }
        if deadline.is_some_and(|deadline| now >= deadline) {
            return Ok(WaitOutcome::Events(vec![]));
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

enum ResultOutcome {
    Stopped(String),
    Result(ResultRef),
}

enum ResultError<E> {
    Store(StoreError),
    Callback(E),
}

impl<E> From<StoreError> for ResultError<E> {
    fn from(error: StoreError) -> Self {
        Self::Store(error)
    }
}

/// The result protocol. `f` runs inside the transaction that marks the event done.
pub fn with_result<E>(
    store: &mut Store,
    run_id: &RunId,
    event_id: &str,
    kind: ResultKind,
    f: impl FnOnce(&Transaction<'_>) -> Result<String, E>,
) -> Result<ResultRef, E>
where
    E: From<StoreError>,
{
    let clock = store.clock();
    let outcome = store.tx(|tx| {
        match runs::gate(tx, clock.now(), run_id)? {
            RunGate::Stopped(run) => Ok(ResultOutcome::Stopped(run.id.to_string())),
            RunGate::Active(_) => {
                let event = tx.query_row("SELECT state,claimed_by,result_kind,result_id FROM events WHERE id = ?1", [event_id], |row| Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?, row.get::<_, Option<String>>(2)?, row.get::<_, Option<String>>(3)?))).optional().map_err(StoreError::from)?;
                let Some((state, claimed_by, stored_kind, stored_id)) = event else { return Err(StoreError::EventUnknown { event_id: event_id.to_owned() }.into()); };
                if state == "pending" || claimed_by.as_deref() != Some(run_id.as_str()) { return Err(StoreError::EventNotClaimed { event_id: event_id.to_owned(), run_id: run_id.to_string() }.into()); }
                if state == "done" {
                    let kind = stored_kind.ok_or_else(|| StoreError::Sqlite(rusqlite::Error::InvalidQuery))?;
                    let id = stored_id.ok_or_else(|| StoreError::Sqlite(rusqlite::Error::InvalidQuery))?;
                    return Ok(ResultOutcome::Result(ResultRef { kind: result_kind(kind)?, id }));
                }
                let id = f(tx).map_err(ResultError::Callback)?;
                tx.execute("UPDATE events SET state = 'done', result_kind = ?1, result_id = ?2, done_at = ?3 WHERE id = ?4 AND state = 'claimed' AND claimed_by = ?5", params![result_kind_name(kind), id, clock.now().to_rfc3339(), event_id, run_id.as_str()]).map_err(StoreError::from)?;
                Ok(ResultOutcome::Result(ResultRef { kind, id }))
            }
        }
    });
    let outcome = match outcome {
        Ok(outcome) => outcome,
        Err(ResultError::Store(error)) => return Err(error.into()),
        Err(ResultError::Callback(error)) => return Err(error),
    };
    match outcome {
        ResultOutcome::Stopped(run_id) => Err(StoreError::RunStopped { run_id }.into()),
        ResultOutcome::Result(result) => Ok(result),
    }
}

/// `with_result` with `ResultKind::Ack`; the result id is the event id.
pub fn ack(store: &mut Store, run_id: &RunId, event_id: &str) -> Result<ResultRef, StoreError> {
    with_result(store, run_id, event_id, ResultKind::Ack, |_| {
        Ok(event_id.to_owned())
    })
}
