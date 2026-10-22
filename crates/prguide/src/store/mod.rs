//! Store paths, connections, and errors.

use std::{
    ffi::OsStr,
    fs,
    path::{Path, PathBuf},
    thread,
    time::{Duration, Instant},
};

use rusqlite::{Connection, ErrorCode, TransactionBehavior};

pub mod deepdives;
pub mod events;
pub mod migrations;
pub mod outbox;
pub mod posts;
pub mod runs;
pub mod schema;
pub mod status;
pub mod threads;
pub mod versions;

/// The only host v1 stores PRs for; the path segment under `prs/`.
pub const GITHUB_HOST: &str = "github.com";

/// A PR on github.com: owner, repo and number. Validated on construction.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PrRef {
    owner: String,
    repo: String,
    number: u64,
}

impl PrRef {
    pub fn new(owner: &str, repo: &str, number: u64) -> Result<Self, StoreError> {
        if invalid_name(owner) || invalid_name(repo) {
            return Err(StoreError::InvalidRepo {
                owner: owner.to_owned(),
                repo: repo.to_owned(),
            });
        }
        Ok(Self {
            owner: owner.to_owned(),
            repo: repo.to_owned(),
            number,
        })
    }

    pub fn owner(&self) -> &str {
        &self.owner
    }
    pub fn repo(&self) -> &str {
        &self.repo
    }
    pub fn number(&self) -> u64 {
        self.number
    }
}

fn invalid_name(name: &str) -> bool {
    name.is_empty()
        || name == "."
        || name.contains('/')
        || name.contains("..")
        || name.contains('\0')
}

/// `$PRGUIDE_HOME`, or `$HOME/.prguide`. Reads the process environment.
pub fn prguide_home() -> Result<PathBuf, StoreError> {
    resolve_home(
        std::env::var_os("PRGUIDE_HOME").as_deref(),
        std::env::var_os("HOME").as_deref(),
    )
}

/// The pure rule behind `prguide_home`, testable without touching the environment.
pub fn resolve_home(
    prguide_home: Option<&OsStr>,
    home: Option<&OsStr>,
) -> Result<PathBuf, StoreError> {
    let chosen = match prguide_home.filter(|value| !value.is_empty()) {
        Some(value) => PathBuf::from(value),
        None => match home.filter(|value| !value.is_empty()) {
            Some(value) => PathBuf::from(value).join(".prguide"),
            None => {
                return Err(StoreError::HomeUnresolved {
                    reason: "PRGUIDE_HOME and HOME are unset".to_owned(),
                });
            }
        },
    };
    if chosen.is_absolute() {
        Ok(chosen)
    } else {
        Err(StoreError::HomeUnresolved {
            reason: format!("{} is not absolute", chosen.display()),
        })
    }
}

pub struct Store {
    dir: PathBuf,
    pr: PrRef,
    conn: Connection,
}

impl Store {
    /// `Store::open_in(&prguide_home()?, pr)`.
    pub fn open(pr: &PrRef) -> Result<Self, StoreError> {
        Self::open_in(&prguide_home()?, pr)
    }

    /// Opens or creates the store for `pr` under `home`.
    pub fn open_in(home: &Path, pr: &PrRef) -> Result<Self, StoreError> {
        let dir = home
            .join("prs")
            .join(GITHUB_HOST)
            .join(pr.owner.to_ascii_lowercase())
            .join(pr.repo.to_ascii_lowercase())
            .join(pr.number.to_string());
        fs::create_dir_all(&dir).map_err(|source| StoreError::Io {
            path: dir.clone(),
            source,
        })?;
        let mut conn = Connection::open(dir.join("store.db"))?;
        conn.busy_timeout(Duration::from_secs(5))?;
        configure_wal(&conn)?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        let foreign_keys: i64 = conn.pragma_query_value(None, "foreign_keys", |row| row.get(0))?;
        require_pragma("foreign_keys", &foreign_keys.to_string(), "1")?;
        migrations::migrate(&mut conn, migrations::MIGRATIONS)?;
        Ok(Self {
            dir,
            pr: pr.clone(),
            conn,
        })
    }

    /// `<home>/prs/github.com/<owner>/<repo>/<number>`.
    pub fn dir(&self) -> &Path {
        &self.dir
    }
    pub fn pr(&self) -> &PrRef {
        &self.pr
    }

    /// Runs `f` in one BEGIN IMMEDIATE transaction: commits on `Ok`, rolls back on `Err`.
    pub fn tx<T, E>(
        &mut self,
        f: impl FnOnce(&rusqlite::Transaction<'_>) -> Result<T, E>,
    ) -> Result<T, E>
    where
        E: From<StoreError>,
    {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(StoreError::Sqlite)
            .map_err(E::from)?;
        let result = f(&tx)?;
        tx.commit().map_err(StoreError::Sqlite).map_err(E::from)?;
        Ok(result)
    }

    /// The value changes when another connection commits a change to the database; this connection's own commits do not change it.
    pub fn data_version(&self) -> Result<i64, StoreError> {
        self.conn
            .pragma_query_value(None, "data_version", |row| row.get(0))
            .map_err(StoreError::from)
    }
}

fn configure_wal(conn: &Connection) -> Result<(), StoreError> {
    let mode: String = conn.pragma_query_value(None, "journal_mode", |row| row.get(0))?;
    if mode.eq_ignore_ascii_case("wal") {
        return Ok(());
    }
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match conn
            .pragma_update_and_check(None, "journal_mode", "WAL", |row| row.get::<_, String>(0))
        {
            Ok(mode) => return require_pragma("journal_mode", &mode, "wal"),
            Err(error)
                if error.sqlite_error_code() == Some(ErrorCode::DatabaseBusy)
                    && Instant::now() < deadline =>
            {
                thread::sleep(Duration::from_millis(10))
            }
            Err(error) => return Err(StoreError::Sqlite(error)),
        }
    }
}

fn require_pragma(name: &str, got: &str, want: &str) -> Result<(), StoreError> {
    if got.eq_ignore_ascii_case(want) {
        return Ok(());
    }
    Err(StoreError::Sqlite(rusqlite::Error::SqliteFailure(
        rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_ERROR),
        Some(format!("PRAGMA {name} is {got}, expected {want}")),
    )))
}

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("invalid GitHub repository name: {}/{}", .owner.escape_debug(), .repo.escape_debug())]
    InvalidRepo { owner: String, repo: String },
    #[error("cannot find the prguide home: {reason}")]
    HomeUnresolved { reason: String },
    #[error(
        "store schema version {found} is newer than this prguide supports ({known}); upgrade prguide"
    )]
    StoreTooNew { found: i64, known: usize },
    #[error("store I/O error at {}: {source}", path.display())]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("store database error: {0}")]
    Sqlite(#[from] rusqlite::Error),
}

impl StoreError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::InvalidRepo { .. } => "invalid_repo",
            Self::HomeUnresolved { .. } => "home_unresolved",
            Self::StoreTooNew { .. } => "store_too_new",
            Self::Io { .. } => "store_io",
            Self::Sqlite(_) => "store_sqlite",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::require_pragma;

    #[test]
    fn require_pragma_checks_values() {
        assert!(require_pragma("journal_mode", "wal", "wal").is_ok());
        assert!(require_pragma("journal_mode", "WAL", "wal").is_ok());
        let error = require_pragma("journal_mode", "delete", "wal").expect_err("mismatch fails");
        assert_eq!(error.code(), "store_sqlite");
        assert!(
            error
                .to_string()
                .contains("PRAGMA journal_mode is delete, expected wal")
        );
        assert_eq!(
            require_pragma("foreign_keys", "0", "1")
                .expect_err("mismatch fails")
                .code(),
            "store_sqlite"
        );
    }
}
