//! Runs, the 30-minute lease and the gate every run-carrying call passes.

use std::time::Duration;

use rusqlite::{OptionalExtension, Transaction, params};
use serde::Serialize;

use super::{Store, StoreError, Timestamp};

/// The lease every active run holds; each gate call renews it.
pub const LEASE: Duration = Duration::from_secs(30 * 60);

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
#[serde(transparent)]
pub struct RunId(String);

impl RunId {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for RunId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RunState {
    Active,
    Stopped,
    Expired,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EndReason {
    Quit,
    End,
    Takeover,
    Expired,
}

/// What a caller may pass to `stop_run`; takeover and expiry are internal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopReason {
    Quit,
    End,
}

/// One `runs` row. Times are the stored RFC 3339 strings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Run {
    pub id: RunId,
    pub state: RunState,
    pub created_at: String,
    pub lease_until: String,
    pub ended_at: Option<String>,
    pub end_reason: Option<EndReason>,
}

/// What the gate found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunGate {
    /// Active, and its lease has just been renewed.
    Active(Run),
    /// Stopped or expired, including a run this call has just expired.
    Stopped(Run),
}

const COLUMNS: &str = "id, state, created_at, lease_until, ended_at, end_reason";

fn conversion_error(index: usize, value: String) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(
        index,
        rusqlite::types::Type::Text,
        format!("unexpected value {value:?}").into(),
    )
}

fn parse_state(index: usize, value: String) -> rusqlite::Result<RunState> {
    match value.as_str() {
        "active" => Ok(RunState::Active),
        "stopped" => Ok(RunState::Stopped),
        "expired" => Ok(RunState::Expired),
        _ => Err(conversion_error(index, value)),
    }
}

fn parse_reason(index: usize, value: Option<String>) -> rusqlite::Result<Option<EndReason>> {
    match value.as_deref() {
        None => Ok(None),
        Some("quit") => Ok(Some(EndReason::Quit)),
        Some("end") => Ok(Some(EndReason::End)),
        Some("takeover") => Ok(Some(EndReason::Takeover)),
        Some("expired") => Ok(Some(EndReason::Expired)),
        Some(_) => Err(conversion_error(index, value.unwrap_or_default())),
    }
}

fn reason_str(reason: EndReason) -> &'static str {
    match reason {
        EndReason::Quit => "quit",
        EndReason::End => "end",
        EndReason::Takeover => "takeover",
        EndReason::Expired => "expired",
    }
}

fn from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Run> {
    Ok(Run {
        id: RunId(row.get(0)?),
        state: parse_state(1, row.get(1)?)?,
        created_at: row.get(2)?,
        lease_until: row.get(3)?,
        ended_at: row.get(4)?,
        end_reason: parse_reason(5, row.get(5)?)?,
    })
}

fn fetch(tx: &Transaction<'_>, id: &str) -> Result<Option<Run>, StoreError> {
    Ok(tx
        .query_row(
            &format!("SELECT {COLUMNS} FROM runs WHERE id = ?1"),
            [id],
            from_row,
        )
        .optional()?)
}

fn fetch_active(tx: &Transaction<'_>) -> Result<Option<Run>, StoreError> {
    Ok(tx
        .query_row(
            &format!("SELECT {COLUMNS} FROM runs WHERE state = 'active'"),
            [],
            from_row,
        )
        .optional()?)
}

fn lapsed(run: &Run, now: Timestamp) -> bool {
    now.to_rfc3339() >= run.lease_until
}

/// Ends an active run and sends its claims back to `pending`, in `tx`.
fn end_run(
    tx: &Transaction<'_>,
    now: Timestamp,
    id: &RunId,
    state: RunState,
    reason: EndReason,
) -> Result<Run, StoreError> {
    let state_str = match state {
        RunState::Active => "active",
        RunState::Stopped => "stopped",
        RunState::Expired => "expired",
    };
    tx.execute(
        "UPDATE runs SET state = ?1, ended_at = ?2, end_reason = ?3 WHERE id = ?4",
        params![state_str, now.to_rfc3339(), reason_str(reason), id.as_str()],
    )?;
    tx.execute(
        "UPDATE events SET state = 'pending', claimed_by = NULL, claimed_at = NULL \
         WHERE state = 'claimed' AND claimed_by = ?1",
        [id.as_str()],
    )?;
    fetch_updated(tx, id.as_str())
}

/// Re-reads a row this transaction just wrote; a missing row is a SQLite-level failure.
fn fetch_updated(tx: &Transaction<'_>, id: &str) -> Result<Run, StoreError> {
    fetch(tx, id)?.ok_or_else(|| StoreError::from(rusqlite::Error::QueryReturnedNoRows))
}

fn expire(tx: &Transaction<'_>, now: Timestamp, run: &Run) -> Result<Run, StoreError> {
    end_run(tx, now, &run.id, RunState::Expired, EndReason::Expired)
}

fn new_id() -> String {
    let text = ulid::Ulid::generate().to_string();
    let tail = text.get(text.len().saturating_sub(12)..).unwrap_or("");
    format!("r_{}", tail.to_ascii_lowercase())
}

/// If the active run's lease has passed, ends it as expired and returns it.
pub fn expire_lapsed(tx: &Transaction<'_>, now: Timestamp) -> Result<Option<Run>, StoreError> {
    match fetch_active(tx)? {
        Some(run) if lapsed(&run, now) => Ok(Some(expire(tx, now, &run)?)),
        _ => Ok(None),
    }
}

/// The active run, after expiring a lapsed one. Does not renew the lease.
pub fn active_run(tx: &Transaction<'_>, now: Timestamp) -> Result<Option<Run>, StoreError> {
    expire_lapsed(tx, now)?;
    fetch_active(tx)
}

/// The gate for every call that carries a run id.
pub fn gate(tx: &Transaction<'_>, now: Timestamp, run_id: &RunId) -> Result<RunGate, StoreError> {
    let Some(run) = fetch(tx, run_id.as_str())? else {
        return Err(StoreError::RunUnknown {
            run_id: run_id.to_string(),
        });
    };
    if run.state != RunState::Active {
        return Ok(RunGate::Stopped(run));
    }
    if lapsed(&run, now) {
        return Ok(RunGate::Stopped(expire(tx, now, &run)?));
    }
    tx.execute(
        "UPDATE runs SET lease_until = ?1 WHERE id = ?2 AND state = 'active'",
        params![now.saturating_add(LEASE).to_rfc3339(), run_id.as_str()],
    )?;
    let renewed = fetch_updated(tx, run_id.as_str())?;
    Ok(RunGate::Active(renewed))
}

/// Starts a run; with `takeover`, ends a live one first.
pub fn start_run(store: &mut Store, takeover: bool) -> Result<Run, StoreError> {
    let clock = store.clock();
    store.tx(|tx| {
        let now = clock.now();
        expire_lapsed(tx, now)?;
        if let Some(active) = fetch_active(tx)? {
            if !takeover {
                return Err(StoreError::RunActive {
                    run_id: active.id.to_string(),
                });
            }
            end_run(tx, now, &active.id, RunState::Stopped, EndReason::Takeover)?;
        }
        let id = new_id();
        tx.execute(
            "INSERT INTO runs (id, state, created_at, lease_until, ended_at, end_reason) \
             VALUES (?1, 'active', ?2, ?3, NULL, NULL)",
            params![id, now.to_rfc3339(), now.saturating_add(LEASE).to_rfc3339()],
        )?;
        fetch_updated(tx, &id)
    })
}

/// Passes the gate or fails with `run_stopped`; an expiry seen here is committed first.
pub fn require_active(store: &mut Store, run_id: &RunId) -> Result<Run, StoreError> {
    let clock = store.clock();
    match store.tx(|tx| gate(tx, clock.now(), run_id))? {
        RunGate::Active(run) => Ok(run),
        RunGate::Stopped(run) => Err(StoreError::RunStopped {
            run_id: run.id.to_string(),
        }),
    }
}

enum StopOutcome {
    Stopped(Run),
    Refused(Run),
}

/// Ends an active run with `reason`.
pub fn stop_run(store: &mut Store, run_id: &RunId, reason: StopReason) -> Result<Run, StoreError> {
    let clock = store.clock();
    let outcome = store.tx(|tx| {
        let now = clock.now();
        let Some(run) = fetch(tx, run_id.as_str())? else {
            return Err(StoreError::RunUnknown {
                run_id: run_id.to_string(),
            });
        };
        if run.state != RunState::Active {
            return Ok(StopOutcome::Refused(run));
        }
        if lapsed(&run, now) {
            return Ok(StopOutcome::Refused(expire(tx, now, &run)?));
        }
        let end = match reason {
            StopReason::Quit => EndReason::Quit,
            StopReason::End => EndReason::End,
        };
        Ok(StopOutcome::Stopped(end_run(
            tx,
            now,
            &run.id,
            RunState::Stopped,
            end,
        )?))
    })?;
    match outcome {
        StopOutcome::Stopped(run) => Ok(run),
        StopOutcome::Refused(run) => Err(StoreError::RunStopped {
            run_id: run.id.to_string(),
        }),
    }
}
