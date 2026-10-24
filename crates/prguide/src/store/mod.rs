//! Store paths, connections, and errors.

use std::{
    ffi::OsStr,
    fs,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicI64, Ordering},
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use rusqlite::{Connection, ErrorCode, OpenFlags, TransactionBehavior};

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

const MAX_MILLIS: i64 = 253_402_300_799_999;

/// Milliseconds since the Unix epoch, clamped to
/// 1970-01-01T00:00:00.000Z ..= 9999-12-31T23:59:59.999Z.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Timestamp(i64);

impl Timestamp {
    /// Clamps into the supported range.
    pub fn from_unix_millis(ms: i64) -> Self {
        Self(ms.clamp(0, MAX_MILLIS))
    }

    pub fn unix_millis(self) -> i64 {
        self.0
    }

    /// Adds `d` (whole milliseconds), clamping at the maximum.
    pub fn saturating_add(self, d: Duration) -> Self {
        let add = i64::try_from(d.as_millis()).unwrap_or(i64::MAX);
        Self::from_unix_millis(self.0.saturating_add(add))
    }

    /// Always 24 bytes: `YYYY-MM-DDTHH:MM:SS.mmmZ`, UTC.
    pub fn to_rfc3339(self) -> String {
        let days = self.0.div_euclid(86_400_000);
        let rem = self.0.rem_euclid(86_400_000);
        // Days-from-civil inverse (Hinnant), on the clamped range.
        let z = days + 719_468;
        let era = z.div_euclid(146_097);
        let doe = z.rem_euclid(146_097);
        let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let day = doy - (153 * mp + 2) / 5 + 1;
        let month = if mp < 10 { mp + 3 } else { mp - 9 };
        let year = yoe + era * 400 + i64::from(month <= 2);
        format!(
            "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{:03}Z",
            rem / 3_600_000,
            rem / 60_000 % 60,
            rem / 1000 % 60,
            rem % 1000
        )
    }
}

impl std::fmt::Display for Timestamp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.to_rfc3339())
    }
}

/// Where the store reads the time.
pub trait Clock: Send + Sync {
    fn now(&self) -> Timestamp;
}

/// `SystemTime::now()`, truncated to milliseconds. A time before the epoch gives the epoch.
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Timestamp {
        let ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
            .unwrap_or(0);
        Timestamp::from_unix_millis(ms)
    }
}

/// A clock that only moves when told to.
pub struct FakeClock {
    millis: AtomicI64,
}

impl FakeClock {
    pub fn new(start: Timestamp) -> Self {
        Self {
            millis: AtomicI64::new(start.0),
        }
    }

    pub fn set(&self, t: Timestamp) {
        self.millis.store(t.0, Ordering::SeqCst);
    }

    pub fn advance(&self, d: Duration) {
        let _ = self
            .millis
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |millis| {
                Some(Timestamp(millis).saturating_add(d).0)
            });
    }
}

impl Clock for FakeClock {
    fn now(&self) -> Timestamp {
        Timestamp(self.millis.load(Ordering::SeqCst))
    }
}

pub struct Store {
    dir: PathBuf,
    pr: PrRef,
    conn: Connection,
    clock: Arc<dyn Clock>,
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
            clock: Arc::new(SystemClock),
        })
    }

    /// Opens the store of the PR that run `run_id` belongs to.
    ///
    /// Agent commands get only `--run <id>`, so this is how they find their
    /// store. It looks at every PR store under `home` in sorted order, reads
    /// each one without changing it, and opens the first that has the run.
    /// No store has it (or there are no stores): `run_unknown`. Call it before
    /// any write transaction.
    pub fn open_for_run(home: &Path, run_id: &runs::RunId) -> Result<Self, StoreError> {
        let root = home.join("prs").join(GITHUB_HOST);
        let owners = match sorted_dirs(&root, true)? {
            Some(entries) => entries,
            None => {
                return Err(StoreError::RunUnknown {
                    run_id: run_id.to_string(),
                });
            }
        };
        for owner in owners {
            let Some(repos) = sorted_dirs(&owner, false)? else {
                continue;
            };
            for repo in repos {
                let Some(numbers) = sorted_dirs(&repo, false)? else {
                    continue;
                };
                for number in numbers {
                    let (Some(owner_name), Some(repo_name), Some(number_name)) = (
                        owner.file_name().and_then(|name| name.to_str()),
                        repo.file_name().and_then(|name| name.to_str()),
                        number.file_name().and_then(|name| name.to_str()),
                    ) else {
                        continue;
                    };
                    if owner_name != owner_name.to_ascii_lowercase()
                        || repo_name != repo_name.to_ascii_lowercase()
                    {
                        continue;
                    }
                    let Ok(number_value) = number_name.parse::<u64>() else {
                        continue;
                    };
                    let Ok(pr) = PrRef::new(owner_name, repo_name, number_value) else {
                        continue;
                    };
                    let db = number.join("store.db");
                    if !db.is_file() || !store_has_run(&db, run_id) {
                        continue;
                    }
                    return Self::open_in(home, &pr);
                }
            }
        }
        Err(StoreError::RunUnknown {
            run_id: run_id.to_string(),
        })
    }
    /// Replaces the clock (tests pass an `Arc<FakeClock>` they keep a clone of).
    pub fn with_clock(mut self, clock: Arc<dyn Clock>) -> Self {
        self.clock = clock;
        self
    }

    /// A clone of the clock, so code can read the time inside `tx`.
    pub fn clock(&self) -> Arc<dyn Clock> {
        Arc::clone(&self.clock)
    }

    pub fn now(&self) -> Timestamp {
        self.clock.now()
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

fn sorted_dirs(path: &Path, required: bool) -> Result<Option<Vec<PathBuf>>, StoreError> {
    let entries = match fs::read_dir(path) {
        Ok(entries) => entries,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) if required => {
            return Err(StoreError::Io {
                path: path.to_owned(),
                source,
            });
        }
        Err(_) => return Ok(None),
    };
    let mut dirs = Vec::new();
    for entry in entries.flatten() {
        if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            dirs.push(entry.path());
        }
    }
    dirs.sort_by(|left, right| left.file_name().cmp(&right.file_name()));
    Ok(Some(dirs))
}

fn store_has_run(db: &Path, run_id: &runs::RunId) -> bool {
    let Ok(conn) = Connection::open_with_flags(
        db,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    ) else {
        return false;
    };
    conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM runs WHERE id = ?1)",
        [run_id.as_str()],
        |row| row.get::<_, i64>(0),
    )
    .is_ok_and(|found| found != 0)
}

fn configure_wal(conn: &Connection) -> Result<(), StoreError> {
    configure_wal_with_retry(conn, Duration::from_secs(5), |conn| {
        conn.pragma_update_and_check(None, "journal_mode", "WAL", |row| row.get::<_, String>(0))
    })
}

fn configure_wal_with_retry(
    conn: &Connection,
    timeout: Duration,
    mut set_wal: impl FnMut(&Connection) -> rusqlite::Result<String>,
) -> Result<(), StoreError> {
    let mode: String = conn.pragma_query_value(None, "journal_mode", |row| row.get(0))?;
    if mode.eq_ignore_ascii_case("wal") {
        return Ok(());
    }
    let deadline = Instant::now() + timeout;
    loop {
        match set_wal(conn) {
            Ok(mode) => return require_pragma("journal_mode", &mode, "wal"),
            Err(error)
                if error.sqlite_error_code() == Some(ErrorCode::DatabaseBusy)
                    && Instant::now() < deadline =>
            {
                thread::sleep(Duration::from_millis(10));
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
    #[error("run {run_id} is active; take it over to start a new run")]
    RunActive { run_id: String },
    #[error("run {run_id} has stopped")]
    RunStopped { run_id: String },
    #[error("no run {run_id} in this store")]
    RunUnknown { run_id: String },
    #[error("no event {event_id} in this store")]
    EventUnknown { event_id: String },
    #[error("event {event_id} is not claimed by run {run_id}")]
    EventNotClaimed { event_id: String, run_id: String },
    #[error("another prguide wait is already listening on this PR (it holds {path})", path = .path.display())]
    WaitActive { path: PathBuf },
}

impl StoreError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::InvalidRepo { .. } => "invalid_repo",
            Self::HomeUnresolved { .. } => "home_unresolved",
            Self::StoreTooNew { .. } => "store_too_new",
            Self::Io { .. } => "store_io",
            Self::Sqlite(_) => "store_sqlite",
            Self::RunActive { .. } => "run_active",
            Self::RunStopped { .. } => "run_stopped",
            Self::RunUnknown { .. } => "run_unknown",
            Self::EventUnknown { .. } => "event_unknown",
            Self::EventNotClaimed { .. } => "event_not_claimed",
            Self::WaitActive { .. } => "wait_active",
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::time::Duration;

    use rusqlite::{Connection, ErrorCode};

    use super::{
        Clock, FakeClock, Timestamp, configure_wal_with_retry, require_pragma, runs::LEASE,
    };

    const T0: i64 = 1_792_713_600_000;

    #[test]
    fn timestamp_formats_fixed_width_rfc3339() {
        for (ms, want) in [
            (0, "1970-01-01T00:00:00.000Z"),
            (951_868_799_999, "2000-02-29T23:59:59.999Z"),
            (1_700_000_000_000, "2023-11-14T22:13:20.000Z"),
            (T0, "2026-10-23T00:00:00.000Z"),
            (4_107_542_399_999, "2100-02-28T23:59:59.999Z"),
            (4_107_542_400_000, "2100-03-01T00:00:00.000Z"),
            (253_402_300_799_999, "9999-12-31T23:59:59.999Z"),
        ] {
            let got = Timestamp::from_unix_millis(ms).to_rfc3339();
            assert_eq!(got.len(), 24);
            assert_eq!(got, want);
            assert_eq!(Timestamp::from_unix_millis(ms).to_string(), want);
        }
    }

    #[test]
    fn timestamp_clamps_and_saturates() {
        assert_eq!(
            Timestamp::from_unix_millis(-1).to_rfc3339(),
            "1970-01-01T00:00:00.000Z"
        );
        let max = Timestamp::from_unix_millis(i64::MAX);
        assert_eq!(max.to_rfc3339(), "9999-12-31T23:59:59.999Z");
        assert_eq!(max.saturating_add(Duration::from_secs(5)), max);
        assert_eq!(max.saturating_add(Duration::MAX), max);
        assert_eq!(
            Timestamp::from_unix_millis(T0)
                .saturating_add(LEASE)
                .to_rfc3339(),
            "2026-10-23T00:30:00.000Z"
        );
    }

    #[test]
    fn timestamp_text_order_is_time_order() {
        for ms in [
            999,
            59_999,
            86_399_999,
            1_785_542_399_999,
            1_798_761_599_999,
            951_868_799_999,
        ] {
            let a = Timestamp::from_unix_millis(ms);
            let b = Timestamp::from_unix_millis(ms + 1);
            assert!(a < b);
            assert!(a.to_rfc3339() < b.to_rfc3339());
        }
        assert_eq!(
            Timestamp::from_unix_millis(1_798_761_600_000).to_rfc3339(),
            "2027-01-01T00:00:00.000Z"
        );
    }

    #[test]
    fn fake_clock_moves_only_when_told() {
        let clock = FakeClock::new(Timestamp::from_unix_millis(T0));
        assert_eq!(clock.now(), clock.now());
        clock.advance(Duration::from_secs(60));
        assert_eq!(clock.now().unix_millis(), T0 + 60_000);
        clock.set(Timestamp::from_unix_millis(5));
        assert_eq!(clock.now().unix_millis(), 5);
        clock.set(Timestamp::from_unix_millis(i64::MAX));
        clock.advance(Duration::from_secs(1));
        assert_eq!(clock.now().unix_millis(), 253_402_300_799_999);
    }

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

    #[test]
    fn wal_retry_retries_busy_until_the_mode_is_wal() {
        let conn = Connection::open_in_memory().expect("in-memory database");
        let attempts = Cell::new(0);
        configure_wal_with_retry(&conn, Duration::from_secs(1), |_| {
            attempts.set(attempts.get() + 1);
            if attempts.get() == 1 {
                Err(rusqlite::Error::SqliteFailure(
                    rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_BUSY),
                    None,
                ))
            } else {
                Ok("wal".to_owned())
            }
        })
        .expect("retry succeeds");
        assert_eq!(attempts.get(), 2);
    }

    #[test]
    fn wal_retry_returns_busy_after_deadline_and_other_errors_immediately() {
        let conn = Connection::open_in_memory().expect("in-memory database");
        let busy_attempts = Cell::new(0);
        let busy = configure_wal_with_retry(&conn, Duration::ZERO, |_| {
            busy_attempts.set(busy_attempts.get() + 1);
            Err(rusqlite::Error::SqliteFailure(
                rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_BUSY),
                None,
            ))
        })
        .expect_err("busy past deadline fails");
        assert_eq!(busy_attempts.get(), 1);
        assert_eq!(busy.code(), "store_sqlite");
        let other_attempts = Cell::new(0);
        let other = configure_wal_with_retry(&conn, Duration::from_secs(1), |_| {
            other_attempts.set(other_attempts.get() + 1);
            Err(rusqlite::Error::SqliteFailure(
                rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_ERROR),
                None,
            ))
        })
        .expect_err("non-busy errors do not retry");
        assert_eq!(other_attempts.get(), 1);
        assert_eq!(other.code(), "store_sqlite");
        assert!(matches!(
            other,
            super::StoreError::Sqlite(ref error)
                if error.sqlite_error_code() == Some(ErrorCode::Unknown)
        ));
    }

    #[test]
    fn fake_clock_advance_is_atomic() {
        let clock = std::sync::Arc::new(FakeClock::new(Timestamp::from_unix_millis(T0)));
        let mut threads = Vec::new();
        for _ in 0..8 {
            let clock = clock.clone();
            threads.push(std::thread::spawn(move || {
                for _ in 0..10_000 {
                    clock.advance(Duration::from_millis(1));
                }
            }));
        }
        for thread in threads {
            thread.join().expect("thread");
        }
        assert_eq!(clock.now().unix_millis(), T0 + 80_000);
    }
}
