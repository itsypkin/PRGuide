//! Guide versions: the `guide_versions` rows, the commit lock, and
//! reconciliation of versions a crashed `guide commit` left in `staging`.

use std::{
    fs::{self, File, OpenOptions},
    io::ErrorKind,
    path::{Path, PathBuf},
};

use fd_lock::RwLock;
use rusqlite::{OptionalExtension, Transaction, types::Type};
use serde::{Deserialize, Serialize};

use super::{Store, StoreError, Timestamp};
use crate::{Error, git::Git};

/// Test builds: calls the fault hook and, on a crash, returns an error from
/// the enclosing function at once (no cleanup). Release builds: nothing.
#[cfg(test)]
macro_rules! fault_point {
    ($p:ident) => {
        if let $crate::store::versions::fault::Action::Crash =
            $crate::store::versions::fault::call($crate::store::versions::fault::Point::$p)
        {
            return Err(
                $crate::Error::Internal(format!("fault injected at {}", stringify!($p))).into(),
            );
        }
    };
}

#[cfg(not(test))]
macro_rules! fault_point {
    ($p:ident) => {};
}

pub(crate) use fault_point;

#[cfg(test)]
pub(crate) mod fault {
    use std::cell::RefCell;

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(crate) enum Point {
        AfterReserve,
        CopyBeforeOpen,
        AfterCopy,
        AfterCheck,
        AfterPin,
        AfterPublish,
        RollbackAfterDir,
        RollbackAfterRefs,
        FinishAfterRefs,
    }

    pub(crate) enum Action {
        Continue,
        Crash,
    }

    type Hook = Box<dyn FnMut(Point) -> Action>;

    thread_local! {
        static HOOK: RefCell<Option<Hook>> = const { RefCell::new(None) };
    }

    pub(crate) fn set(hook: impl FnMut(Point) -> Action + 'static) {
        HOOK.with(|h| *h.borrow_mut() = Some(Box::new(hook)));
    }

    pub(crate) fn clear() {
        HOOK.with(|h| *h.borrow_mut() = None);
    }

    /// Takes the hook out while it runs, so points reached inside it see none.
    pub(crate) fn call(point: Point) -> Action {
        let Some(mut hook) = HOOK.with(|h| h.borrow_mut().take()) else {
            return Action::Continue;
        };
        let action = hook(point);
        HOOK.with(|h| {
            let mut slot = h.borrow_mut();
            if slot.is_none() {
                *slot = Some(hook);
            }
        });
        action
    }
}

/// The lock file in the store directory that a commit holds from its
/// reconciliation to its last step.
pub const COMMIT_LOCK: &str = "commit.lock";

/// One pinned git ref of a version.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PinnedRef {
    pub name: String,
    pub sha: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct VersionSnapshot {
    pub base: String,
    pub head: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct VersionSelection {
    pub kind: String,
    pub base: String,
    pub head: String,
    pub commits: Vec<String>,
}

/// One `guide_versions` row, parsed strictly.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct GuideVersion {
    pub version: i64,
    pub state: String,
    pub dir: PathBuf,
    pub caused_by: Option<String>,
    pub snapshot: VersionSnapshot,
    pub selection: VersionSelection,
    pub refs: Vec<PinnedRef>,
    pub created_at: String,
    pub ready_at: Option<String>,
}

const COLUMNS: &str = "version, state, caused_by, snapshot_base, snapshot_head, selection_kind, \
                       selection_base, selection_head, commits_json, refs_json, created_at, ready_at";

/// `<store>/staging/v<k>`.
pub fn staging_dir(store_dir: &Path, k: i64) -> PathBuf {
    store_dir.join("staging").join(format!("v{k}"))
}

/// `<store>/guides/v<k>`.
pub fn guide_dir(store_dir: &Path, k: i64) -> PathBuf {
    store_dir.join("guides").join(format!("v{k}"))
}

fn json_column<T: serde::de::DeserializeOwned>(col: usize, text: &str) -> rusqlite::Result<T> {
    serde_json::from_str(text)
        .map_err(|e| rusqlite::Error::FromSqlConversionFailure(col, Type::Text, Box::new(e)))
}

fn from_row(store_dir: &Path, row: &rusqlite::Row<'_>) -> rusqlite::Result<GuideVersion> {
    let version: i64 = row.get(0)?;
    let commits_json: String = row.get(8)?;
    let refs_json: String = row.get(9)?;
    Ok(GuideVersion {
        version,
        state: row.get(1)?,
        dir: guide_dir(store_dir, version),
        caused_by: row.get(2)?,
        snapshot: VersionSnapshot {
            base: row.get(3)?,
            head: row.get(4)?,
        },
        selection: VersionSelection {
            kind: row.get(5)?,
            base: row.get(6)?,
            head: row.get(7)?,
            commits: json_column(8, &commits_json)?,
        },
        refs: json_column(9, &refs_json)?,
        created_at: row.get(10)?,
        ready_at: row.get(11)?,
    })
}

fn list(
    tx: &Transaction<'_>,
    store_dir: &Path,
    state: &str,
) -> Result<Vec<GuideVersion>, StoreError> {
    let mut stmt = tx.prepare(&format!(
        "SELECT {COLUMNS} FROM guide_versions WHERE state = ?1 ORDER BY version"
    ))?;
    let rows = stmt.query_map([state], |row| from_row(store_dir, row))?;
    Ok(rows.collect::<Result<Vec<_>, _>>()?)
}

/// Ready versions, oldest first.
pub fn ready(tx: &Transaction<'_>, store_dir: &Path) -> Result<Vec<GuideVersion>, StoreError> {
    list(tx, store_dir, "ready")
}

/// The row for version `k`, in any state.
pub fn get(
    tx: &Transaction<'_>,
    store_dir: &Path,
    k: i64,
) -> Result<Option<GuideVersion>, StoreError> {
    Ok(tx
        .query_row(
            &format!("SELECT {COLUMNS} FROM guide_versions WHERE version = ?1"),
            [k],
            |row| from_row(store_dir, row),
        )
        .optional()?)
}

/// Step 5, in the caller's transaction: the row becomes ready, the event (if
/// any) becomes done, and a `guide_ready` notice is inserted.
pub fn finish(
    tx: &Transaction<'_>,
    k: i64,
    caused_by: Option<&str>,
    now: Timestamp,
) -> Result<(), Error> {
    let now = now.to_rfc3339();
    let changed = tx
        .execute(
            "UPDATE guide_versions SET state = 'ready', ready_at = ?1 \
             WHERE version = ?2 AND state = 'staging'",
            rusqlite::params![now, k],
        )
        .map_err(StoreError::from)?;
    if changed != 1 {
        return Err(Error::Internal(format!(
            "guide version {k} is not a staging row, so it cannot be finished"
        )));
    }
    if let Some(event) = caused_by {
        tx.execute(
            "UPDATE events SET state = 'done', result_kind = 'guide_version', \
             result_id = ?1, done_at = ?2 WHERE id = ?3 AND state <> 'done'",
            rusqlite::params![k.to_string(), now, event],
        )
        .map_err(StoreError::from)?;
    }
    tx.execute(
        "INSERT INTO notices (kind, ref, created_at) VALUES ('guide_ready', ?1, ?2)",
        rusqlite::params![k.to_string(), now],
    )
    .map_err(StoreError::from)?;
    Ok(())
}

/// Opens (creating) the commit lock file.
pub fn open_lock(store_dir: &Path) -> Result<RwLock<File>, StoreError> {
    let path = store_dir.join(COMMIT_LOCK);
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)
        .map_err(|source| StoreError::Io { path, source })?;
    Ok(RwLock::new(file))
}

fn io(path: &Path) -> impl Fn(std::io::Error) -> StoreError + '_ {
    move |source| StoreError::Io {
        path: path.to_path_buf(),
        source,
    }
}

/// Rolls back version `k`: its staging directory, then its refs, then its
/// row. Never touches `guides/`.
pub fn rollback(store: &mut Store, git: &Git, k: i64, refs: &[PinnedRef]) -> Result<(), Error> {
    let staging = staging_dir(store.dir(), k);
    // Only remove through a real `staging/` directory: removing through a
    // symlinked parent would delete data outside the store.
    let parent = store.dir().join("staging");
    let parent_is_real = match fs::symlink_metadata(&parent) {
        Ok(meta) => meta.is_dir(),
        Err(e) if e.kind() == ErrorKind::NotFound => false,
        Err(e) => return Err(io(&parent)(e).into()),
    };
    if parent_is_real {
        match fs::remove_dir_all(&staging) {
            Ok(()) => {}
            Err(e) if e.kind() == ErrorKind::NotFound => {}
            Err(e) => return Err(io(&staging)(e).into()),
        }
    }
    fault_point!(RollbackAfterDir);
    let names: Vec<String> = refs.iter().map(|r| r.name.clone()).collect();
    git.delete_refs(&names)?;
    fault_point!(RollbackAfterRefs);
    store.tx(|tx| {
        tx.execute(
            "DELETE FROM guide_versions WHERE version = ?1 AND state = 'staging'",
            [k],
        )?;
        Ok::<_, StoreError>(())
    })?;
    Ok(())
}

/// Finishes or rolls back every staging row. The caller holds the commit lock.
pub fn reconcile(store: &mut Store, git: &Git) -> Result<(), Error> {
    let store_dir = store.dir().to_path_buf();
    let rows = store.tx(|tx| list(tx, &store_dir, "staging"))?;
    for row in rows {
        let published = guide_dir(&store_dir, row.version);
        let is_dir = match fs::symlink_metadata(&published) {
            Ok(meta) => meta.is_dir(),
            Err(e) if e.kind() == ErrorKind::NotFound => false,
            Err(e) => return Err(io(&published)(e).into()),
        };
        // `guides/` itself must be a real directory, or `guides/v<k>` may
        // live outside the store.
        let guides_real = is_dir
            && match fs::symlink_metadata(store_dir.join("guides")) {
                Ok(meta) => meta.is_dir(),
                Err(e) if e.kind() == ErrorKind::NotFound => false,
                Err(e) => return Err(io(&store_dir.join("guides"))(e).into()),
            };
        if !guides_real {
            rollback(store, git, row.version, &row.refs)?;
            continue;
        }
        let wanted: Vec<(String, String)> = row
            .refs
            .iter()
            .map(|r| (r.name.clone(), r.sha.clone()))
            .collect();
        let bad: Vec<(String, String)> = git
            .verify_refs(&wanted)?
            .into_iter()
            .map(|m| (m.name, m.expected))
            .collect();
        git.write_refs(&bad)?;
        fault_point!(FinishAfterRefs);
        let now = store.now();
        let caused_by = row.caused_by.clone();
        store.tx(|tx| finish(tx, row.version, caused_by.as_deref(), now))?;
    }
    Ok(())
}

/// Best-effort reconciliation on every `Store::open_in`. Never fails.
pub(super) fn reconcile_on_open(store: &mut Store) {
    let _ = try_reconcile_on_open(store);
}

fn try_reconcile_on_open(store: &mut Store) -> Option<()> {
    let any: i64 = store
        .conn
        .query_row(
            "SELECT EXISTS (SELECT 1 FROM guide_versions WHERE state = 'staging')",
            [],
            |r| r.get(0),
        )
        .ok()?;
    if any == 0 {
        return None;
    }
    let common: Option<String> = store
        .conn
        .query_row(
            "SELECT value FROM store_meta WHERE key = 'git_common_dir'",
            [],
            |r| r.get(0),
        )
        .optional()
        .ok()?;
    let common = common.filter(|c| !c.is_empty())?;
    let mut lock = open_lock(store.dir()).ok()?;
    let _guard = lock.try_write().ok()?;
    reconcile(store, &Git::new(common)).ok()
}
