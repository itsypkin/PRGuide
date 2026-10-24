//! Agent status (listening, working, not listening) and the read behind `prguide status`.

use std::{fs::OpenOptions, io::ErrorKind};

use serde::Serialize;

use super::{Store, StoreError, events, runs, runs::RunId};

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum AgentStatus {
    Listening,
    Working,
    NotListening {
        pending: u64,
        active_run: Option<RunId>,
    },
}

impl AgentStatus {
    pub fn name(&self) -> &'static str {
        match self {
            Self::Listening => "listening",
            Self::Working => "working",
            Self::NotListening { .. } => "not_listening",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ReadyVersion {
    pub version: i64,
    pub selection_kind: String,
    pub snapshot_base: String,
    pub snapshot_head: String,
    pub selection_base: String,
    pub selection_head: String,
    pub ready_at: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusReport {
    pub agent: AgentStatus,
    pub pending: u64,
    pub guide_versions: Vec<ReadyVersion>,
}

pub fn agent_status(store: &mut Store) -> Result<AgentStatus, StoreError> {
    Ok(status_report(store)?.agent)
}

pub fn status_report(store: &mut Store) -> Result<StatusReport, StoreError> {
    let clock = store.clock();
    let (active, claimed, pending, guide_versions) = store.tx(|tx| {
        let now = clock.now();
        let active = runs::active_run(tx, now)?;
        let claimed: i64 = match &active {
            Some(run) => tx.query_row(
                "SELECT COUNT(*) FROM events WHERE state = 'claimed' AND claimed_by = ?1",
                [run.id.as_str()],
                |row| row.get(0),
            )?,
            None => 0,
        };
        let pending: i64 = tx.query_row(
            "SELECT COUNT(*) FROM events WHERE state = 'pending'",
            [],
            |row| row.get(0),
        )?;
        let mut stmt = tx.prepare(
            "SELECT version, selection_kind, snapshot_base, snapshot_head, selection_base, \
             selection_head, ready_at FROM guide_versions WHERE state = 'ready' \
             ORDER BY version",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(ReadyVersion {
                version: row.get(0)?,
                selection_kind: row.get(1)?,
                snapshot_base: row.get(2)?,
                snapshot_head: row.get(3)?,
                selection_base: row.get(4)?,
                selection_head: row.get(5)?,
                ready_at: row.get(6)?,
            })
        })?;
        let mut versions = Vec::new();
        for row in rows {
            versions.push(row?);
        }
        Ok::<_, StoreError>((active, claimed, pending, versions))
    })?;
    let (claimed, pending) = (count(claimed), count(pending));
    let agent = match active {
        None => AgentStatus::NotListening {
            pending,
            active_run: None,
        },
        Some(run) => {
            if lock_held(store)? {
                AgentStatus::Listening
            } else if claimed > 0 {
                AgentStatus::Working
            } else {
                AgentStatus::NotListening {
                    pending,
                    active_run: Some(run.id),
                }
            }
        }
    };
    Ok(StatusReport {
        agent,
        pending,
        guide_versions,
    })
}

/// Whether a `wait` holds `wait.lock` right now. Read-only, shared and brief.
fn lock_held(store: &Store) -> Result<bool, StoreError> {
    let path = store.dir().join(events::WAIT_LOCK);
    let io = |source| StoreError::Io {
        path: path.clone(),
        source,
    };
    let file = match OpenOptions::new().read(true).open(&path) {
        Ok(file) => file,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(io(e)),
    };
    let lock = fd_lock::RwLock::new(file);
    match lock.try_read() {
        Ok(guard) => {
            drop(guard);
            Ok(false)
        }
        Err(e) if e.kind() == ErrorKind::WouldBlock => Ok(true),
        Err(e) => Err(io(e)),
    }
}

fn count(value: i64) -> u64 {
    u64::try_from(value).unwrap_or(0)
}
