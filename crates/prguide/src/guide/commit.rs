//! `prguide guide commit`: the five steps that turn a draft into an immutable
//! guide version (reserve, copy and check, pin refs, publish, finish).

use std::{
    ffi::OsString,
    fs::{self, File, OpenOptions},
    io::{self, ErrorKind, Read, Write},
    os::unix::{ffi::OsStrExt, fs::MetadataExt},
    path::Path,
};

use rusqlite::{OptionalExtension, Transaction};
use serde::Deserialize;

use super::{
    GuideError,
    check::{DraftRow, check_draft},
    model::{MANIFEST_FILE, SLIDES_DIR, SelectionKind, SlideId},
};
use crate::{
    Error,
    git::Git,
    store::{
        Store, StoreError,
        runs::{self, RunGate, RunId},
        versions::{
            self, COMMIT_LOCK, GuideVersion, PinnedRef, fault_point, guide_dir, staging_dir,
        },
    },
};

#[derive(Deserialize)]
struct Prov {
    snapshot: ProvSnapshot,
    selection: ProvSelection,
}

#[derive(Deserialize)]
struct ProvSnapshot {
    base: String,
    head: String,
}

#[derive(Deserialize)]
struct ProvSelection {
    kind: SelectionKind,
    base: String,
    head: String,
    commits: Vec<String>,
}

fn parse_provenance(json: &str) -> Result<Prov, Error> {
    let prov: Prov = serde_json::from_str(json).map_err(|e| {
        Error::Internal(format!(
            "the drafts row's provenance_json does not parse: {e}"
        ))
    })?;
    if prov.selection.kind == SelectionKind::Commits && prov.selection.commits.is_empty() {
        return Err(Error::Internal(
            "the drafts row's provenance_json has selection kind commits but no commits".into(),
        ));
    }
    Ok(prov)
}

/// The four pinned refs of version `k`, in their fixed order.
fn pinned_refs(n: u64, k: i64, prov: &Prov) -> Vec<PinnedRef> {
    let name = |leaf: &str| format!("refs/prguide/{n}/v{k}/{leaf}");
    vec![
        PinnedRef {
            name: name("snapshot-base"),
            sha: prov.snapshot.base.clone(),
        },
        PinnedRef {
            name: name("snapshot-head"),
            sha: prov.snapshot.head.clone(),
        },
        PinnedRef {
            name: name("selection-base"),
            sha: prov.selection.base.clone(),
        },
        PinnedRef {
            name: name("selection-head"),
            sha: prov.selection.head.clone(),
        },
    ]
}

/// A range as the mismatch message shows it; a missing side is `(none)`.
fn range_text(first: Option<&str>, last: Option<&str>) -> String {
    match (first, last) {
        (None, None) => "the full PR".to_owned(),
        (f, l) => format!(
            "commits {}..{}",
            f.unwrap_or("(none)"),
            l.unwrap_or("(none)")
        ),
    }
}

impl Prov {
    /// The selection's range as `(first, last)`; `None, None` for the full PR.
    fn range(&self) -> (Option<&str>, Option<&str>) {
        match self.selection.kind {
            SelectionKind::Full => (None, None),
            SelectionKind::Commits => (
                self.selection.commits.first().map(String::as_str),
                self.selection.commits.last().map(String::as_str),
            ),
        }
    }

    fn kind_name(&self) -> &'static str {
        match self.selection.kind {
            SelectionKind::Full => "full",
            SelectionKind::Commits => "commits",
        }
    }
}

enum Reserved {
    Stopped(String),
    Existing(Box<GuideVersion>),
    New { k: i64, refs: Vec<PinnedRef> },
}

fn guard_message(path: &Path) -> String {
    format!(
        "{} exists, but no version row owns it; prguide guide commit does not overwrite it",
        path.display()
    )
}

fn invalid_query() -> StoreError {
    StoreError::Sqlite(rusqlite::Error::InvalidQuery)
}

/// Step 1, in one transaction.
fn reserve(
    tx: &Transaction<'_>,
    store: &StoreFacts,
    run_id: &RunId,
    caused_by: Option<&str>,
    prov: &Prov,
) -> Result<Reserved, Error> {
    // Sampled inside the transaction, after any wait for the write lock.
    let now = store.clock.now();
    match runs::gate(tx, now, run_id)? {
        RunGate::Stopped(run) => return Ok(Reserved::Stopped(run.id.to_string())),
        RunGate::Active(_) => {}
    }
    if let Some(event) = caused_by {
        #[allow(clippy::type_complexity)]
        let found: Option<(
            String,
            Option<String>,
            String,
            Option<String>,
            Option<String>,
            Option<String>,
            Option<String>,
        )> = tx
            .query_row(
                "SELECT state, claimed_by, type, range_first, range_last, result_kind, result_id \
                 FROM events WHERE id = ?1",
                [event],
                |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                        r.get(6)?,
                    ))
                },
            )
            .optional()
            .map_err(StoreError::from)?;
        let Some((state, claimed_by, kind, first, last, result_kind, result_id)) = found else {
            return Err(StoreError::EventUnknown {
                event_id: event.to_owned(),
            }
            .into());
        };
        if state == "done" {
            let result_kind = result_kind.ok_or_else(invalid_query)?;
            let result_id = result_id.ok_or_else(invalid_query)?;
            if result_kind != "guide_version" {
                return Err(GuideError::EventCommandMismatch(format!(
                    "event {event} is already done with result {result_kind}, not a guide version"
                ))
                .into());
            }
            let k: i64 = result_id.parse().map_err(|e| {
                StoreError::Sqlite(rusqlite::Error::FromSqlConversionFailure(
                    0,
                    rusqlite::types::Type::Text,
                    Box::new(e),
                ))
            })?;
            return match versions::get(tx, &store.dir, k)? {
                Some(v) => Ok(Reserved::Existing(Box::new(v))),
                None => Err(Error::Internal(format!(
                    "event {event} is done with guide version {result_id}, which this store \
                     does not have"
                ))),
            };
        }
        if state == "pending" || claimed_by.as_deref() != Some(run_id.as_str()) {
            return Err(StoreError::EventNotClaimed {
                event_id: event.to_owned(),
                run_id: run_id.to_string(),
            }
            .into());
        }
        if kind != "generate_request" {
            return Err(GuideError::EventCommandMismatch(format!(
                "event {event} is a {kind}, not a generate_request; --caused-by takes the \
                 generate_request the guide answers"
            ))
            .into());
        }
        let have = prov.range();
        if (first.as_deref(), last.as_deref()) != have {
            return Err(GuideError::EventCommandMismatch(format!(
                "event {event} asks for {}, but the draft's selection is {}; commit a draft made \
                 by prguide guide new for that range",
                range_text(first.as_deref(), last.as_deref()),
                range_text(have.0, have.1),
            ))
            .into());
        }
    }
    let k: i64 = tx
        .query_row(
            "SELECT COALESCE(MAX(version), 0) + 1 FROM guide_versions",
            [],
            |r| r.get(0),
        )
        .map_err(StoreError::from)?;
    let published = guide_dir(&store.dir, k);
    if fs::symlink_metadata(&published).is_ok() {
        return Err(Error::Internal(guard_message(&published)));
    }
    let refs = pinned_refs(store.number, k, prov);
    let refs_json = serde_json::to_string(&refs).map_err(|e| Error::Internal(e.to_string()))?;
    let commits_json = serde_json::to_string(&prov.selection.commits)
        .map_err(|e| Error::Internal(e.to_string()))?;
    tx.execute(
        "INSERT INTO guide_versions (version, state, caused_by, snapshot_base, snapshot_head, \
         selection_kind, selection_base, selection_head, commits_json, refs_json, created_at, \
         ready_at) VALUES (?1, 'staging', ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, NULL)",
        rusqlite::params![
            k,
            caused_by,
            prov.snapshot.base,
            prov.snapshot.head,
            prov.kind_name(),
            prov.selection.base,
            prov.selection.head,
            commits_json,
            refs_json,
            now.to_rfc3339(),
        ],
    )
    .map_err(StoreError::from)?;
    Ok(Reserved::New { k, refs })
}

struct StoreFacts {
    dir: std::path::PathBuf,
    number: u64,
    clock: std::sync::Arc<dyn crate::store::Clock>,
}

const CHANGED: &str = "changed while it was being copied";

enum CopyErr {
    Reject { entry: String, what: &'static str },
    Fail(Error),
}

impl From<Error> for CopyErr {
    fn from(e: Error) -> Self {
        Self::Fail(e)
    }
}

fn reject(entry: impl Into<String>, what: &'static str) -> CopyErr {
    CopyErr::Reject {
        entry: entry.into(),
        what,
    }
}

fn sorted_names(dir: &Path) -> io::Result<Vec<OsString>> {
    let mut names = Vec::new();
    for entry in fs::read_dir(dir)? {
        names.push(entry?.file_name());
    }
    names.sort_by(|a, b| a.as_bytes().cmp(b.as_bytes()));
    Ok(names)
}

/// Copies one regular file, refusing it if it is not the file that was lstat'ed.
fn copy_file(src: &Path, dst: &Path, lstat: &fs::Metadata, shown: &str) -> Result<(), CopyErr> {
    fault_point!(CopyBeforeOpen);
    let changed = |_: io::Error| reject(shown, CHANGED);
    let mut file = File::open(src).map_err(changed)?;
    let meta = file.metadata().map_err(changed)?;
    if !meta.is_file() || meta.dev() != lstat.dev() || meta.ino() != lstat.ino() {
        return Err(reject(shown, CHANGED));
    }
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).map_err(changed)?;
    let store_io = |source| {
        CopyErr::Fail(
            StoreError::Io {
                path: dst.to_path_buf(),
                source,
            }
            .into(),
        )
    };
    let mut out = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(dst)
        .map_err(store_io)?;
    out.write_all(&bytes).map_err(store_io)?;
    Ok(())
}

/// Copies `guide.yaml` and `slides/<id>.md` from the draft into `dst`, which
/// already holds an empty `slides/`. Nothing is followed.
fn copy_draft(src: &Path, dst: &Path) -> Result<(), CopyErr> {
    let meta = fs::symlink_metadata(src).map_err(|_| reject(".", CHANGED))?;
    if meta.file_type().is_symlink() {
        return Err(reject(".", "is a symlink"));
    }
    if !meta.is_dir() {
        return Err(reject(".", "is not a directory"));
    }
    let names = sorted_names(src).map_err(|_| reject(".", CHANGED))?;
    let mut walk_slides = false;
    for name in &names {
        let shown = name.to_string_lossy().into_owned();
        let path = src.join(name);
        let meta = fs::symlink_metadata(&path).map_err(|_| reject(shown.clone(), CHANGED))?;
        let kind = meta.file_type();
        if name.as_bytes() == MANIFEST_FILE.as_bytes() {
            if kind.is_symlink() {
                return Err(reject(shown, "is a symlink"));
            }
            if !kind.is_file() {
                return Err(reject(shown, "is not a regular file"));
            }
            copy_file(&path, &dst.join(name), &meta, &shown)?;
        } else if name.as_bytes() == SLIDES_DIR.as_bytes() {
            if kind.is_symlink() {
                return Err(reject(shown, "is a symlink"));
            }
            if !kind.is_dir() {
                return Err(reject(shown, "is not a directory"));
            }
            walk_slides = true;
        } else if kind.is_symlink() {
            return Err(reject(shown, "is a symlink"));
        } else if kind.is_file() {
            return Err(reject(shown, "is not a file a draft may hold"));
        } else {
            return Err(reject(shown, "is not a regular file"));
        }
    }
    if !walk_slides {
        return Ok(());
    }
    let slides = src.join(SLIDES_DIR);
    let names = sorted_names(&slides).map_err(|_| reject(SLIDES_DIR, CHANGED))?;
    for name in &names {
        let shown = format!("{SLIDES_DIR}/{}", name.to_string_lossy());
        let path = slides.join(name);
        let meta = fs::symlink_metadata(&path).map_err(|_| reject(shown.clone(), CHANGED))?;
        let kind = meta.file_type();
        if kind.is_symlink() {
            return Err(reject(shown, "is a symlink"));
        }
        if !kind.is_file() {
            return Err(reject(shown, "is not a regular file"));
        }
        let valid = name
            .to_str()
            .and_then(|n| n.strip_suffix(".md"))
            .and_then(SlideId::parse)
            .is_some();
        if !valid {
            return Err(reject(shown, "is not a file a draft may hold"));
        }
        copy_file(&path, &dst.join(SLIDES_DIR).join(name), &meta, &shown)?;
    }
    Ok(())
}

fn io_err(path: &Path, source: io::Error) -> Error {
    StoreError::Io {
        path: path.to_path_buf(),
        source,
    }
    .into()
}

/// `path` must be a real directory (not a symlink to one).
fn require_real_dir(path: &Path) -> Result<(), Error> {
    let meta = fs::symlink_metadata(path).map_err(|e| io_err(path, e))?;
    if meta.file_type().is_dir() {
        Ok(())
    } else {
        Err(io_err(
            path,
            io::Error::other(
                "not a real directory; prguide writes guides only into a real directory",
            ),
        ))
    }
}

/// Removes a leftover staging dir, then creates `staging/v<k>` and its `slides/`.
fn prepare_staging(store_dir: &Path, k: i64) -> Result<(), Error> {
    let root = store_dir.join("staging");
    fs::create_dir_all(&root).map_err(|e| io_err(&root, e))?;
    require_real_dir(&root)?;
    let staged = staging_dir(store_dir, k);
    match fs::remove_dir_all(&staged) {
        Ok(()) => {}
        Err(e) if e.kind() == ErrorKind::NotFound => {}
        Err(e) => return Err(io_err(&staged, e)),
    }
    fs::create_dir(&staged).map_err(|e| io_err(&staged, e))?;
    let slides = staged.join(SLIDES_DIR);
    fs::create_dir(&slides).map_err(|e| io_err(&slides, e))
}

/// Step 4: moves the checked copy into `guides/`.
fn publish(store_dir: &Path, k: i64) -> Result<(), Error> {
    let root = store_dir.join("guides");
    fs::create_dir_all(&root).map_err(|e| io_err(&root, e))?;
    require_real_dir(&root)?;
    let published = guide_dir(store_dir, k);
    fs::rename(staging_dir(store_dir, k), &published).map_err(|e| io_err(&published, e))
}

/// Rolls version `k` back and hands back the error that caused it. If the
/// rollback itself fails, the original error still wins; reconciliation
/// finishes the job later.
fn abandon(store: &mut Store, git: &Git, k: i64, refs: &[PinnedRef], error: Error) -> Error {
    let _ = versions::rollback(store, git, k, refs);
    error
}

fn check_failed(message: String, diagnostics: Vec<super::check::Diagnostic>) -> Error {
    GuideError::CheckFailed {
        message,
        diagnostics,
    }
    .into()
}

/// Commits `row`'s draft as the next guide version and returns the stored row.
pub fn commit(
    store: &mut Store,
    git: &Git,
    row: &DraftRow,
    run_id: &RunId,
    caused_by: Option<&str>,
) -> Result<GuideVersion, Error> {
    let prov = parse_provenance(&row.provenance_json)?;
    let store_dir = store.dir().to_path_buf();
    let mut lock = versions::open_lock(&store_dir)?;
    let _guard = lock.write().map_err(|source| StoreError::Io {
        path: store_dir.join(COMMIT_LOCK),
        source,
    })?;
    versions::reconcile(store, git)?;

    let facts = StoreFacts {
        dir: store_dir.clone(),
        number: store.pr().number(),
        clock: store.clock(),
    };
    let reserved = store.tx(|tx| reserve(tx, &facts, run_id, caused_by, &prov))?;
    let (k, refs) = match reserved {
        Reserved::Stopped(run_id) => return Err(StoreError::RunStopped { run_id }.into()),
        Reserved::Existing(version) => return Ok(*version),
        Reserved::New { k, refs } => (k, refs),
    };
    fault_point!(AfterReserve);

    // Step 2: guard, stage a copy, check the copy.
    let published = guide_dir(&store_dir, k);
    if fs::symlink_metadata(&published).is_ok() {
        let error = Error::Internal(guard_message(&published));
        return Err(abandon(store, git, k, &refs, error));
    }
    if let Err(error) = prepare_staging(&store_dir, k) {
        return Err(abandon(store, git, k, &refs, error));
    }
    let staged = staging_dir(&store_dir, k);
    let draft_dir = Path::new(&row.path);
    match copy_draft(draft_dir, &staged) {
        Ok(()) => {}
        Err(CopyErr::Fail(error)) => return Err(abandon(store, git, k, &refs, error)),
        Err(CopyErr::Reject { entry, what }) => {
            let error = match check_draft(git, draft_dir, &row.provenance_json) {
                Ok(report) => check_failed(
                    format!(
                        "prguide guide commit copies only guide.yaml and slides/<id>.md, and \
                         `{entry}` {what}; fix it and run prguide guide commit again"
                    ),
                    report.diagnostics,
                ),
                Err(error) => error,
            };
            return Err(abandon(store, git, k, &refs, error));
        }
    }
    fault_point!(AfterCopy);
    match check_draft(git, &staged, &row.provenance_json) {
        Ok(report) if report.ok => {}
        Ok(report) => {
            let n = report
                .diagnostics
                .iter()
                .filter(|d| d.severity == super::check::Severity::Error)
                .count();
            let error = check_failed(
                format!("the draft has {n} error(s); fix them and run prguide guide commit again"),
                report.diagnostics,
            );
            return Err(abandon(store, git, k, &refs, error));
        }
        Err(error) => return Err(abandon(store, git, k, &refs, error)),
    }
    fault_point!(AfterCheck);

    // Step 3: pin the refs.
    let pairs: Vec<(String, String)> = refs
        .iter()
        .map(|r| (r.name.clone(), r.sha.clone()))
        .collect();
    if let Err(error) = git.write_refs(&pairs) {
        return Err(abandon(store, git, k, &refs, error.into()));
    }
    fault_point!(AfterPin);

    // Step 4: publish.
    if let Err(error) = publish(&store_dir, k) {
        return Err(abandon(store, git, k, &refs, error));
    }
    fault_point!(AfterPublish);

    // Step 5: no rollback from here on.
    let now = store.now();
    store.tx(|tx| versions::finish(tx, k, caused_by, now))?;
    let dir = store_dir;
    store
        .tx(|tx| versions::get(tx, &dir, k))?
        .ok_or_else(|| Error::Internal(format!("guide version {k} vanished after it was finished")))
}

#[cfg(test)]
mod tests {
    use std::{
        cell::RefCell,
        collections::BTreeMap,
        os::unix::fs::{MetadataExt, symlink},
        path::PathBuf,
        rc::Rc,
        sync::Arc,
        time::{Duration, SystemTime, UNIX_EPOCH},
    };

    use rusqlite::Connection;
    use tempfile::TempDir;

    use super::*;
    use crate::{
        guide::{
            model::{ManifestPr, ManifestSelection, ManifestSnapshot},
            new::{Provenance, render_manifest},
        },
        store::{
            FakeClock, PrRef, Timestamp,
            drafts::{self, NewDraft},
            events::{self, WaitOutcome},
            runs::{self, LEASE, StopReason},
            versions::fault::{self, Action, Point},
        },
    };

    fn now_ms() -> i64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64
    }

    fn envs(git: Git, root: &Path) -> Git {
        git.env("HOME", root)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", root.join("gitconfig"))
    }

    fn sh(git: &Git, args: &[&str]) -> String {
        String::from_utf8(git.run(args.iter().copied()).unwrap())
            .unwrap()
            .trim()
            .to_owned()
    }

    fn slide(id: &str, kind: &str) -> String {
        format!("---\nid: {id}\nkind: {kind}\ntitle: T {id}\n---\nBody of {id}.\n")
    }

    /// The tree of regular files below `dir`, by `/`-joined relative path.
    fn tree(dir: &Path) -> BTreeMap<String, Vec<u8>> {
        fn walk(base: &Path, dir: &Path, out: &mut BTreeMap<String, Vec<u8>>) {
            for e in fs::read_dir(dir).unwrap() {
                let p = e.unwrap().path();
                if p.is_dir() {
                    walk(base, &p, out);
                } else {
                    let rel = p.strip_prefix(base).unwrap().to_str().unwrap().to_owned();
                    out.insert(rel, fs::read(&p).unwrap());
                }
            }
        }
        let mut out = BTreeMap::new();
        walk(dir, dir, &mut out);
        out
    }

    fn exists(p: &Path) -> bool {
        fs::symlink_metadata(p).is_ok()
    }

    struct H {
        _tmp: TempDir,
        root: PathBuf,
        home: PathBuf,
        pr: PrRef,
        store: Store,
        git: Git,
        clock: Arc<FakeClock>,
        run: RunId,
        a: String,
        b: String,
        row: DraftRow,
        draft: PathBuf,
        event: String,
    }

    impl Drop for H {
        fn drop(&mut self) {
            fault::clear();
        }
    }

    impl H {
        fn new() -> Self {
            Self::build(None)
        }

        fn build(base_override: Option<&str>) -> Self {
            let tmp = TempDir::new().unwrap();
            let root = fs::canonicalize(tmp.path()).unwrap();
            fs::write(root.join("gitconfig"), "").unwrap();
            let repo = root.join("repo");
            fs::create_dir(&repo).unwrap();
            let git = envs(Git::new(&repo), &root);
            sh(&git, &["init", "-q", "-b", "main"]);
            let mut shas = Vec::new();
            for text in ["1\n", "2\n"] {
                fs::write(repo.join("a.txt"), text).unwrap();
                sh(&git, &["add", "a.txt"]);
                sh(
                    &git,
                    &[
                        "-c",
                        "user.name=t",
                        "-c",
                        "user.email=t@example.com",
                        "commit",
                        "-q",
                        "-m",
                        "c",
                    ],
                );
                shas.push(sh(&git, &["rev-parse", "HEAD"]));
            }
            let (a, b) = (shas[0].clone(), shas[1].clone());
            let common = fs::canonicalize(repo.join(".git")).unwrap();
            let home = root.join("home");
            fs::create_dir(&home).unwrap();
            let home = fs::canonicalize(home).unwrap();
            let pr = PrRef::new("octo", "widget", 7).unwrap();
            let clock = Arc::new(FakeClock::new(Timestamp::from_unix_millis(now_ms())));
            let mut store = Store::open_in(&home, &pr)
                .unwrap()
                .with_clock(Arc::clone(&clock) as Arc<dyn crate::store::Clock>);
            store
                .tx(|tx| -> Result<(), StoreError> {
                    for (k, v) in [
                        ("host", "github.com".to_owned()),
                        ("owner", "octo".to_owned()),
                        ("repo", "widget".to_owned()),
                        ("number", "7".to_owned()),
                        ("git_common_dir", common.to_str().unwrap().to_owned()),
                    ] {
                        tx.execute(
                            "INSERT INTO store_meta (key, value) VALUES (?1, ?2)",
                            [k, &v],
                        )?;
                    }
                    Ok(())
                })
                .unwrap();
            let run = runs::start_run(&mut store, false).unwrap().id;
            let base = base_override.unwrap_or(&a).to_owned();
            let prov = Provenance {
                format: "prguide/v1".into(),
                pr: ManifestPr {
                    host: "github.com".into(),
                    repo: "octo/widget".into(),
                    number: 7,
                },
                snapshot: ManifestSnapshot {
                    base: base.clone(),
                    head: b.clone(),
                },
                selection: ManifestSelection {
                    kind: SelectionKind::Full,
                    base,
                    head: b.clone(),
                    commits: vec![b.clone()],
                },
            };
            let provenance_json = serde_json::to_string(&prov).unwrap();
            let draft = store.dir().join("drafts").join("d1");
            fs::create_dir_all(draft.join("slides")).unwrap();
            fs::write(
                draft.join("guide.yaml"),
                render_manifest(&prov).replace("slides: []", "slides: [overview, step-one]"),
            )
            .unwrap();
            fs::write(
                draft.join("slides/overview.md"),
                slide("overview", "overview"),
            )
            .unwrap();
            fs::write(draft.join("slides/step-one.md"), slide("step-one", "step")).unwrap();
            let path = draft.to_str().unwrap().to_owned();
            store
                .tx(|tx| {
                    drafts::insert(
                        tx,
                        &NewDraft {
                            id: "d1",
                            run_id: run.as_str(),
                            path: &path,
                            provenance_json: &provenance_json,
                            created_at: "t",
                        },
                    )
                })
                .unwrap();
            let now = store.now();
            let event = store
                .tx(|tx| events::append_generate_request(tx, now, None, None))
                .unwrap()
                .id;
            let claimed = events::wait(
                &mut store,
                &run,
                Some(Duration::ZERO),
                std::num::NonZeroU32::new(10).unwrap(),
            )
            .unwrap();
            assert!(matches!(claimed, WaitOutcome::Events(e) if e.len() == 1));
            let row = DraftRow {
                id: "d1".into(),
                run_id: run.to_string(),
                path,
                provenance_json,
            };
            let git = envs(Git::new(&repo), &root);
            Self {
                _tmp: tmp,
                root,
                home,
                pr,
                store,
                git,
                clock,
                run,
                a,
                b,
                row,
                draft,
                event,
            }
        }

        fn commit(&mut self, caused_by: Option<&str>) -> Result<GuideVersion, Error> {
            commit(&mut self.store, &self.git, &self.row, &self.run, caused_by)
        }

        fn e(&self) -> String {
            self.event.clone()
        }

        fn raw(&self) -> Connection {
            Connection::open(self.store.dir().join("store.db")).unwrap()
        }

        fn count(&self, sql: &str) -> i64 {
            self.raw().query_row(sql, [], |r| r.get(0)).unwrap()
        }

        fn rows(&self) -> i64 {
            self.count("SELECT COUNT(*) FROM guide_versions")
        }

        fn notices(&self) -> i64 {
            self.count("SELECT COUNT(*) FROM notices")
        }

        fn state_of(&self, k: i64) -> Option<String> {
            self.raw()
                .query_row(
                    "SELECT state FROM guide_versions WHERE version = ?1",
                    [k],
                    |r| r.get(0),
                )
                .ok()
        }

        /// state, claimed_by, result_kind, result_id of the event.
        fn event_row(&self) -> (String, Option<String>, Option<String>, Option<String>) {
            self.raw()
                .query_row(
                    "SELECT state, claimed_by, result_kind, result_id FROM events WHERE id = ?1",
                    [&self.event],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
                )
                .unwrap()
        }

        fn claimed(&self) -> (String, Option<String>, Option<String>, Option<String>) {
            ("claimed".into(), Some(self.run.to_string()), None, None)
        }

        fn refs(&self, k: i64) -> Vec<(String, String)> {
            self.git
                .list_refs(&format!("refs/prguide/7/v{k}/"))
                .unwrap()
        }

        fn staged(&self, k: i64) -> PathBuf {
            staging_dir(self.store.dir(), k)
        }

        fn published(&self, k: i64) -> PathBuf {
            guide_dir(self.store.dir(), k)
        }

        fn lock_free(&self) -> bool {
            let mut lock = versions::open_lock(self.store.dir()).unwrap();
            lock.try_write().is_ok()
        }

        /// "Nothing left" for version `k`.
        fn nothing_left(&self, k: i64) {
            assert_eq!(self.state_of(k), None, "row v{k}");
            assert!(!exists(&self.staged(k)), "staging v{k}");
            assert!(!exists(&self.published(k)), "guides v{k}");
            assert!(self.refs(k).is_empty(), "refs v{k}");
            assert_eq!(self.notices(), 0, "notices");
            assert!(self.lock_free(), "lock");
        }

        fn reopen(&self) -> Store {
            Store::open_in(&self.home, &self.pr).unwrap()
        }

        fn pairs(&self, k: i64) -> Vec<(String, String)> {
            let (a, b) = (&self.a, &self.b);
            [
                "snapshot-base",
                "snapshot-head",
                "selection-base",
                "selection-head",
            ]
            .iter()
            .zip([a, b, a, b])
            .map(|(n, s)| (format!("refs/prguide/7/v{k}/{n}"), s.clone()))
            .collect()
        }

        fn draft_tree(&self) -> BTreeMap<String, Vec<u8>> {
            tree(&self.draft)
        }
    }

    fn crash_at(p: Point) {
        fault::set(move |q| {
            if q == p {
                Action::Crash
            } else {
                Action::Continue
            }
        });
    }

    fn expect_err(r: Result<GuideVersion, Error>) -> Error {
        match r {
            Ok(v) => panic!("expected an error, got version {}", v.version),
            Err(e) => e,
        }
    }

    // ---------------------------------------------------------------- U1-U4

    fn crash_rolls_back(point: Point, staged: bool, pinned: bool) {
        let mut h = H::new();
        crash_at(point);
        let e = h.e();
        let err = expect_err(h.commit(Some(&e)));
        assert!(err.to_string().contains("fault injected"), "{err}");
        fault::clear();
        assert_eq!(h.state_of(1).as_deref(), Some("staging"));
        assert_eq!(exists(&h.staged(1)), staged);
        assert_eq!(h.refs(1).len(), if pinned { 4 } else { 0 });
        drop(h.reopen());
        h.nothing_left(1);
        assert_eq!(h.event_row(), h.claimed());
    }

    /// U1
    #[test]
    fn crash_after_reserve_rolls_back_on_open() {
        crash_rolls_back(Point::AfterReserve, false, false);
    }

    /// U2
    #[test]
    fn crash_after_copy_rolls_back_on_open() {
        let mut h = H::new();
        let staged = h.staged(1);
        fault::set(move |p| {
            if p == Point::AfterCopy {
                assert!(staged.join("guide.yaml").exists());
                return Action::Crash;
            }
            Action::Continue
        });
        assert!(expect_err(h.commit(None)).to_string().contains("AfterCopy"));
        fault::clear();
        assert_eq!(h.state_of(1).as_deref(), Some("staging"));
        assert!(h.staged(1).exists());
        drop(h.reopen());
        h.nothing_left(1);
    }

    /// U3
    #[test]
    fn crash_after_check_rolls_back_on_open() {
        crash_rolls_back(Point::AfterCheck, true, false);
    }

    /// U4
    #[test]
    fn crash_after_pin_rolls_back_on_open() {
        let mut h = H::new();
        let pairs = h.pairs(1);
        let git = h.git.clone();
        fault::set(move |p| {
            if p == Point::AfterPin {
                let listed = git.list_refs("refs/prguide/7/v1/").unwrap();
                assert_eq!(listed.len(), 4);
                for pair in &pairs {
                    assert!(listed.contains(pair));
                }
                return Action::Crash;
            }
            Action::Continue
        });
        assert!(expect_err(h.commit(None)).to_string().contains("AfterPin"));
        fault::clear();
        assert_eq!(h.state_of(1).as_deref(), Some("staging"));
        assert!(h.staged(1).exists());
        assert_eq!(h.refs(1).len(), 4);
        drop(h.reopen());
        h.nothing_left(1);
    }

    // ---------------------------------------------------------------- U5-U9

    /// U5
    #[test]
    fn crash_after_publish_finishes_on_open() {
        let mut h = H::new();
        crash_at(Point::AfterPublish);
        let e = h.e();
        expect_err(h.commit(Some(&e)));
        fault::clear();
        drop(h.reopen());
        assert_eq!(h.state_of(1).as_deref(), Some("ready"));
        let ready_at: Option<String> = h
            .raw()
            .query_row("SELECT ready_at FROM guide_versions", [], |r| r.get(0))
            .unwrap();
        assert!(ready_at.is_some());
        assert_eq!(h.draft_tree().len(), 3);
        assert_eq!(tree(&h.published(1)), h.draft_tree());
        assert!(!exists(&h.staged(1)));
        assert!(h.git.verify_refs(&h.pairs(1)).unwrap().is_empty());
        assert_eq!(
            h.event_row(),
            (
                "done".into(),
                Some(h.run.to_string()),
                Some("guide_version".into()),
                Some("1".into())
            )
        );
        let notices: Vec<(String, String)> = h
            .raw()
            .prepare("SELECT kind, ref FROM notices")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(notices, vec![("guide_ready".to_owned(), "1".to_owned())]);
        assert!(h.lock_free());
    }

    /// U6
    #[test]
    fn crash_then_repeat_redoes() {
        for point in [
            Point::AfterReserve,
            Point::AfterCopy,
            Point::AfterCheck,
            Point::AfterPin,
        ] {
            let mut h = H::new();
            crash_at(point);
            let e = h.e();
            expect_err(h.commit(Some(&e)));
            fault::clear();
            let v = h.commit(Some(&e)).unwrap();
            assert_eq!((v.version, v.state.as_str()), (1, "ready"), "{point:?}");
            assert_eq!(h.rows(), 1);
            assert_eq!(h.notices(), 1);
            assert!(h.git.verify_refs(&h.pairs(1)).unwrap().is_empty());
            assert_eq!(h.event_row().0, "done");
        }
    }

    /// U7
    #[test]
    fn crash_after_publish_then_repeat_finishes() {
        let mut h = H::new();
        crash_at(Point::AfterPublish);
        let e = h.e();
        expect_err(h.commit(Some(&e)));
        fault::clear();
        let before = fs::metadata(h.published(1)).unwrap();
        let v = h.commit(Some(&e)).unwrap();
        assert_eq!(v.version, 1);
        let after = fs::metadata(h.published(1)).unwrap();
        assert_eq!((before.dev(), before.ino()), (after.dev(), after.ino()));
        assert_eq!(h.rows(), 1);
        assert_eq!(h.notices(), 1);
    }

    /// U8
    #[test]
    fn rollback_crash_midway_finishes_on_a_later_open() {
        let mut h = H::new();
        crash_at(Point::AfterPin);
        expect_err(h.commit(None));
        crash_at(Point::RollbackAfterDir);
        drop(h.reopen());
        assert_eq!(h.state_of(1).as_deref(), Some("staging"));
        assert!(!exists(&h.staged(1)));
        assert_eq!(h.refs(1).len(), 4);
        crash_at(Point::RollbackAfterRefs);
        drop(h.reopen());
        assert_eq!(h.state_of(1).as_deref(), Some("staging"));
        assert!(h.refs(1).is_empty());
        fault::clear();
        drop(h.reopen());
        h.nothing_left(1);
    }

    /// U9
    #[test]
    fn finish_crash_midway_finishes_on_a_later_open() {
        let mut h = H::new();
        crash_at(Point::AfterPublish);
        let e = h.e();
        expect_err(h.commit(Some(&e)));
        let pairs = h.pairs(1);
        h.git.delete_refs(&[pairs[0].0.clone()]).unwrap();
        h.git
            .write_refs(&[(pairs[3].0.clone(), h.a.clone())])
            .unwrap();
        crash_at(Point::FinishAfterRefs);
        drop(h.reopen());
        assert!(h.git.verify_refs(&pairs).unwrap().is_empty());
        assert_eq!(h.state_of(1).as_deref(), Some("staging"));
        assert_eq!(h.event_row(), h.claimed());
        assert_eq!(h.notices(), 0);
        fault::clear();
        drop(h.reopen());
        assert_eq!(h.state_of(1).as_deref(), Some("ready"));
        assert_eq!(h.event_row().2.as_deref(), Some("guide_version"));
        assert_eq!(h.notices(), 1);
    }

    // ---------------------------------------------------------------- U10

    /// U10
    #[test]
    fn open_mid_commit_rolls_nothing_back() {
        let mut h = H::new();
        let (home, pr, git) = (h.home.clone(), h.pr.clone(), h.git.clone());
        let store_dir = h.store.dir().to_path_buf();
        let seen = Rc::new(RefCell::new(Vec::new()));
        let seen2 = Rc::clone(&seen);
        fault::set(move |p| {
            if !matches!(
                p,
                Point::AfterReserve
                    | Point::AfterCopy
                    | Point::AfterCheck
                    | Point::AfterPin
                    | Point::AfterPublish
            ) {
                return Action::Continue;
            }
            drop(Store::open_in(&home, &pr).unwrap());
            let conn = Connection::open(store_dir.join("store.db")).unwrap();
            let state: String = conn
                .query_row(
                    "SELECT state FROM guide_versions WHERE version = 1",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(state, "staging", "{p:?}");
            match p {
                Point::AfterCopy | Point::AfterCheck | Point::AfterPin => {
                    assert!(staging_dir(&store_dir, 1).exists(), "{p:?}");
                }
                Point::AfterPublish => assert!(guide_dir(&store_dir, 1).exists()),
                _ => {}
            }
            let refs = git.list_refs("refs/prguide/7/v1/").unwrap().len();
            assert_eq!(
                refs,
                if matches!(p, Point::AfterPin | Point::AfterPublish) {
                    4
                } else {
                    0
                }
            );
            let mut lock = versions::open_lock(&store_dir).unwrap();
            let blocked = matches!(lock.try_write(), Err(e) if e.kind() == ErrorKind::WouldBlock);
            assert!(blocked, "{p:?}");
            seen2.borrow_mut().push(p);
            Action::Continue
        });
        let v = h.commit(None).unwrap();
        assert_eq!((v.version, v.state.as_str()), (1, "ready"));
        assert_eq!(seen.borrow().len(), 5);
    }

    // ---------------------------------------------------------------- U11, U12

    /// U11
    #[test]
    fn the_staged_copy_is_checked_and_published() {
        let mut h = H::new();
        let original = fs::read(h.draft.join("slides/step-one.md")).unwrap();
        let target = h.draft.join("slides/step-one.md");
        fault::set(move |p| {
            if p == Point::AfterCopy {
                fs::write(&target, "garbage").unwrap();
            }
            Action::Continue
        });
        h.commit(None).unwrap();
        assert_eq!(
            fs::read(h.published(1).join("slides/step-one.md")).unwrap(),
            original
        );
        fault::clear();

        let mut h = H::new();
        let target = h.staged(1).join("slides/step-one.md");
        fault::set(move |p| {
            if p == Point::AfterCopy {
                fs::write(&target, "garbage").unwrap();
            }
            Action::Continue
        });
        let err = expect_err(h.commit(None));
        let Error::Guide(GuideError::CheckFailed {
            message,
            diagnostics,
        }) = err
        else {
            panic!("not check_failed")
        };
        let n = diagnostics
            .iter()
            .filter(|d| d.severity == super::super::check::Severity::Error)
            .count();
        assert_eq!(
            message,
            format!("the draft has {n} error(s); fix them and run prguide guide commit again")
        );
        assert!(
            diagnostics
                .iter()
                .any(|d| d.code == "front_matter_missing" && d.file == "slides/step-one.md")
        );
        h.nothing_left(1);
    }

    /// U12
    #[test]
    fn swap_before_open_is_rejected() {
        let mut h = H::new();
        let outside = h.root.join("outside.md");
        fs::write(&outside, slide("step-one", "step")).unwrap();
        let original = h.draft.join("slides/step-one.md");
        let staged = h.staged(1);
        let calls = Rc::new(RefCell::new(0));
        let (o2, outside2) = (original.clone(), outside.clone());
        fault::set(move |p| {
            if p == Point::CopyBeforeOpen {
                *calls.borrow_mut() += 1;
                if *calls.borrow() == 3 {
                    assert!(staged.join("guide.yaml").exists());
                    assert!(staged.join("slides/overview.md").exists());
                    assert!(!staged.join("slides/step-one.md").exists());
                    fs::remove_file(&o2).unwrap();
                    symlink(&outside2, &o2).unwrap();
                }
            }
            Action::Continue
        });
        let err = expect_err(h.commit(None));
        fault::clear();
        let Error::Guide(GuideError::CheckFailed {
            message,
            diagnostics,
        }) = err
        else {
            panic!("not check_failed")
        };
        assert_eq!(
            message,
            "prguide guide commit copies only guide.yaml and slides/<id>.md, and \
             `slides/step-one.md` changed while it was being copied; fix it and run \
             prguide guide commit again"
        );
        let want = check_draft(&h.git, &h.draft, &h.row.provenance_json).unwrap();
        assert_eq!(
            serde_json::to_value(&diagnostics).unwrap(),
            serde_json::to_value(&want.diagnostics).unwrap()
        );
        h.nothing_left(1);
        let needle = fs::read(&outside).unwrap();
        for sub in ["staging", "guides"] {
            let dir = h.store.dir().join(sub);
            if dir.exists() {
                assert!(tree(&dir).values().all(|b| *b != needle));
            }
        }
    }

    // ---------------------------------------------------------------- U13

    /// Stops `run`, starts a new run and claims the event for it, in the hook.
    fn hijack(home: PathBuf, pr: PrRef, run: RunId, event: String, ack: bool, claim: bool) {
        let done = Rc::new(RefCell::new(false));
        fault::set(move |p| {
            if p == Point::AfterPin && !*done.borrow() {
                *done.borrow_mut() = true;
                let mut s = Store::open_in(&home, &pr).unwrap();
                runs::stop_run(&mut s, &run, StopReason::End).unwrap();
                if claim {
                    let b = runs::start_run(&mut s, false).unwrap().id;
                    events::wait(
                        &mut s,
                        &b,
                        Some(Duration::ZERO),
                        std::num::NonZeroU32::new(10).unwrap(),
                    )
                    .unwrap();
                    if ack {
                        events::ack(&mut s, &b, &event).unwrap();
                    }
                }
            }
            Action::Continue
        });
    }

    fn other_run(h: &H) -> RunId {
        let id: String = h
            .raw()
            .query_row("SELECT id FROM runs WHERE state = 'active'", [], |r| {
                r.get(0)
            })
            .unwrap();
        RunId::new(id)
    }

    /// U13
    #[test]
    fn stop_mid_commit_still_finishes() {
        let mut h = H::new();
        let e = h.e();
        hijack(
            h.home.clone(),
            h.pr.clone(),
            h.run.clone(),
            e.clone(),
            false,
            true,
        );
        let v = h.commit(Some(&e)).unwrap();
        fault::clear();
        assert_eq!((v.version, v.state.as_str()), (1, "ready"));
        let b = other_run(&h);
        assert_eq!(
            h.event_row(),
            (
                "done".into(),
                Some(b.to_string()),
                Some("guide_version".into()),
                Some("1".into())
            )
        );
        let row2 = DraftRow {
            id: "d2".into(),
            run_id: b.to_string(),
            ..h.row.clone()
        };
        let v = commit(&mut h.store, &h.git, &row2, &b, Some(&e)).unwrap();
        assert_eq!(v.version, 1);
        assert_eq!((h.rows(), h.notices()), (1, 1));
        // (d) a takeover run gets the same version.
        let c = runs::start_run(&mut h.store, true).unwrap().id;
        let v = commit(&mut h.store, &h.git, &row2, &c, Some(&e)).unwrap();
        assert_eq!(v.version, 1);
        assert_eq!(h.event_row().1, Some(b.to_string()));
        assert_eq!((h.rows(), h.notices()), (1, 1));

        // (b) a stop alone.
        let mut h = H::new();
        let e = h.e();
        hijack(
            h.home.clone(),
            h.pr.clone(),
            h.run.clone(),
            e.clone(),
            false,
            false,
        );
        h.commit(Some(&e)).unwrap();
        fault::clear();
        assert_eq!(
            h.event_row(),
            (
                "done".into(),
                None,
                Some("guide_version".into()),
                Some("1".into())
            )
        );

        // (c) acked by another run before the commit finishes.
        let mut h = H::new();
        let e = h.e();
        hijack(
            h.home.clone(),
            h.pr.clone(),
            h.run.clone(),
            e.clone(),
            true,
            true,
        );
        h.commit(Some(&e)).unwrap();
        fault::clear();
        let (state, _, kind, id) = h.event_row();
        assert_eq!(
            (state.as_str(), kind.as_deref(), id.as_deref()),
            ("done", Some("ack"), Some(e.as_str()))
        );
        assert_eq!(h.notices(), 1);
        assert_eq!(h.state_of(1).as_deref(), Some("ready"));
    }

    // ---------------------------------------------------------------- U14-U16

    const GUARD: &str = "guides/v1 exists, but no version row owns it";

    fn no_trace(h: &H) {
        assert_eq!(h.rows(), 0);
        assert!(!exists(&h.staged(1)));
        assert!(h.refs(1).is_empty());
    }

    /// U14
    #[test]
    fn existing_guides_dir_is_never_touched() {
        let mut h = H::new();
        fs::create_dir_all(h.published(1)).unwrap();
        fs::write(h.published(1).join("marker"), "m").unwrap();
        let err = expect_err(h.commit(None));
        assert_eq!(err.code(), "internal");
        assert!(err.to_string().contains(GUARD), "{err}");
        assert_eq!(fs::read(h.published(1).join("marker")).unwrap(), b"m");
        no_trace(&h);

        let mut h = H::new();
        fs::create_dir_all(h.store.dir().join("guides")).unwrap();
        let nowhere = h.root.join("nowhere");
        symlink(&nowhere, h.published(1)).unwrap();
        let err = expect_err(h.commit(None));
        assert_eq!(err.code(), "internal");
        assert!(err.to_string().contains(GUARD), "{err}");
        assert_eq!(fs::read_link(h.published(1)).unwrap(), nowhere);
        no_trace(&h);
    }

    /// U15
    #[test]
    fn pin_failure_rolls_back() {
        let mut h = H::build(Some(&"1".repeat(40)));
        let err = expect_err(h.commit(None));
        assert_eq!(err.code(), "git_failed");
        h.nothing_left(1);
    }

    /// U16
    #[test]
    fn publish_failure_rolls_back() {
        let mut h = H::new();
        let guides = h.store.dir().join("guides");
        fs::write(&guides, "file").unwrap();
        let err = expect_err(h.commit(None));
        assert_eq!(err.code(), "store_io");
        h.nothing_left(1);
        assert_eq!(fs::read(&guides).unwrap(), b"file");
    }

    // ---------------------------------------------------------------- U17

    /// U17
    #[test]
    fn commit_holds_the_lock_and_releases_it() {
        let mut h = H::new();
        let store_dir = h.store.dir().to_path_buf();
        fault::set(move |p| {
            if matches!(p, Point::AfterReserve | Point::AfterPublish) {
                let mut lock = versions::open_lock(&store_dir).unwrap();
                assert!(matches!(lock.try_write(), Err(e) if e.kind() == ErrorKind::WouldBlock));
            }
            Action::Continue
        });
        h.commit(None).unwrap();
        fault::clear();
        assert!(h.lock_free());

        let mut h = H::new();
        fs::write(h.draft.join("slides/step-one.md"), "garbage").unwrap();
        let err = expect_err(h.commit(None));
        assert_eq!(err.code(), "check_failed");
        assert!(h.lock_free());
    }

    // ---------------------------------------------------------------- U18-U20

    /// U18
    #[test]
    fn crash_after_reserve_never_adopts_a_foreign_guides_dir() {
        let mut h = H::new();
        fs::create_dir_all(h.published(1)).unwrap();
        fs::write(h.published(1).join("marker"), "m").unwrap();
        let seen = Rc::new(RefCell::new(Vec::new()));
        let seen2 = Rc::clone(&seen);
        fault::set(move |p| {
            seen2.borrow_mut().push(p);
            if p == Point::AfterReserve {
                Action::Crash
            } else {
                Action::Continue
            }
        });
        let e = h.e();
        let err = expect_err(h.commit(Some(&e)));
        fault::clear();
        assert_eq!(err.code(), "internal");
        assert!(err.to_string().contains(GUARD), "{err}");
        assert!(!seen.borrow().contains(&Point::AfterReserve));
        drop(h.reopen());
        assert_eq!(h.rows(), 0);
        assert_eq!(h.notices(), 0);
        assert_eq!(h.event_row(), h.claimed());
        assert_eq!(fs::read(h.published(1).join("marker")).unwrap(), b"m");
    }

    /// U19
    #[test]
    fn guard_refuses_a_guides_dir_made_after_reserve() {
        let mut h = H::new();
        let published = h.published(1);
        let id = Rc::new(RefCell::new((0, 0)));
        let id2 = Rc::clone(&id);
        let p2 = published.clone();
        fault::set(move |p| {
            if p == Point::AfterReserve {
                fs::create_dir_all(&p2).unwrap();
                let m = fs::metadata(&p2).unwrap();
                *id2.borrow_mut() = (m.dev(), m.ino());
            }
            Action::Continue
        });
        let err = expect_err(h.commit(None));
        fault::clear();
        assert_eq!(err.code(), "internal");
        assert!(err.to_string().contains(GUARD), "{err}");
        let m = fs::metadata(&published).unwrap();
        assert_eq!((m.dev(), m.ino()), *id.borrow());
        assert_eq!(fs::read_dir(&published).unwrap().count(), 0);
        no_trace(&h);
        assert_eq!(h.notices(), 0);
        assert!(h.lock_free());
    }

    /// U20
    #[test]
    fn staging_and_guides_must_be_real_directories() {
        let mut h = H::new();
        let outside = h.root.join("outside");
        fs::create_dir(&outside).unwrap();
        fs::create_dir(outside.join("v1")).unwrap();
        fs::write(outside.join("v1/keep.txt"), "k").unwrap();
        symlink(&outside, h.store.dir().join("staging")).unwrap();
        let err = expect_err(h.commit(None));
        assert_eq!(err.code(), "store_io");
        assert!(err.to_string().contains("not a real directory"), "{err}");
        assert_eq!(h.state_of(1), None);
        assert!(h.refs(1).is_empty());
        assert_eq!(h.notices(), 0);
        assert!(h.lock_free());
        assert_eq!(fs::read(outside.join("v1/keep.txt")).unwrap(), b"k");
        drop(h.reopen());
        assert_eq!(fs::read(outside.join("v1/keep.txt")).unwrap(), b"k");

        let mut h = H::new();
        let outside = h.root.join("outside");
        fs::create_dir(&outside).unwrap();
        let guides = h.store.dir().join("guides");
        symlink(&outside, &guides).unwrap();
        let err = expect_err(h.commit(None));
        assert_eq!(err.code(), "store_io");
        assert!(err.to_string().contains("not a real directory"), "{err}");
        h.nothing_left(1);
        assert_eq!(fs::read_dir(&outside).unwrap().count(), 0);
        assert_eq!(fs::read_link(&guides).unwrap(), outside);
    }

    // ---------------------------------------------------------------- U21-U23

    /// U21
    #[test]
    fn reserve_checks_the_run() {
        let mut h = H::new();
        runs::stop_run(&mut h.store, &h.run.clone(), StopReason::End).unwrap();
        let err = expect_err(h.commit(None));
        assert_eq!(err.code(), "run_stopped");
        assert_eq!(err.to_string(), format!("run {} has stopped", h.run));
        no_trace(&h);

        let mut h = H::new();
        h.clock.advance(LEASE);
        let err = expect_err(h.commit(None));
        assert_eq!(err.code(), "run_stopped");
        let state: String = h
            .raw()
            .query_row(
                "SELECT state FROM runs WHERE id = ?1",
                [h.run.as_str()],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(state, "expired");
        no_trace(&h);

        let mut h = H::new();
        runs::stop_run(&mut h.store, &h.run.clone(), StopReason::End).unwrap();
        let err = expect_err(h.commit(Some("nope")));
        assert_eq!(err.code(), "run_stopped");
    }

    fn insert_row(h: &H, version: i64, state: &str, refs_json: &str) {
        h.raw()
            .execute(
                "INSERT INTO guide_versions (version, state, caused_by, snapshot_base, \
                 snapshot_head, selection_kind, selection_base, selection_head, commits_json, \
                 refs_json, created_at, ready_at) VALUES (?1, ?2, NULL, 'x', 'x', 'full', 'x', \
                 'x', '[]', ?3, 't', ?4)",
                rusqlite::params![version, state, refs_json, (state == "ready").then_some("r")],
            )
            .unwrap();
    }

    /// U22
    #[test]
    fn corrupt_staging_row_fails_the_commit() {
        let mut h = H::new();
        insert_row(&h, 1, "staging", "not json");
        let err = expect_err(h.commit(None));
        assert_eq!(err.code(), "store_sqlite");
        assert_eq!(h.state_of(1).as_deref(), Some("staging"));
        assert_eq!(h.rows(), 1);
        assert!(!exists(&h.staged(2)));
        assert!(h.git.list_refs("refs/prguide/7/").unwrap().is_empty());
        assert_eq!(h.notices(), 0);
        assert!(h.lock_free());
    }

    /// U23
    #[test]
    fn version_follows_the_highest_row() {
        let mut h = H::new();
        insert_row(&h, 3, "ready", "[]");
        let v = h.commit(None).unwrap();
        assert_eq!(v.version, 4);
        assert_eq!(h.refs(4).len(), 4);
        assert!(h.published(4).exists());
    }

    // ---------------------------------------------------------------- U24-U27

    /// U24
    #[test]
    fn commit_reconciles_only_under_the_lock() {
        let mut h = H::new();
        crash_at(Point::AfterPin);
        expect_err(h.commit(None));
        fault::clear();
        let store_dir = h.store.dir().to_path_buf();
        let mut lock = versions::open_lock(&store_dir).unwrap();
        let guard = lock.write().unwrap();
        let (home, pr, git, row, run) = (
            h.home.clone(),
            h.pr.clone(),
            h.git.clone(),
            h.row.clone(),
            h.run.clone(),
        );
        let handle = std::thread::spawn(move || {
            let mut store = Store::open_in(&home, &pr).unwrap();
            commit(&mut store, &git, &row, &run, None)
        });
        std::thread::sleep(Duration::from_millis(500));
        assert!(!handle.is_finished());
        assert_eq!(h.state_of(1).as_deref(), Some("staging"));
        assert!(h.staged(1).exists());
        assert_eq!(h.refs(1).len(), 4);
        drop(guard);
        let v = handle.join().unwrap().unwrap();
        assert_eq!((v.version, v.state.as_str()), (1, "ready"));
        assert_eq!((h.rows(), h.notices()), (1, 1));
    }

    /// U25
    #[test]
    fn finish_failure_does_not_roll_back() {
        let mut h = H::new();
        let db = h.store.dir().join("store.db");
        fault::set(move |p| {
            if p == Point::AfterPublish {
                Connection::open(&db)
                    .unwrap()
                    .execute_batch(
                        "CREATE TRIGGER fail_notice BEFORE INSERT ON notices \
                         BEGIN SELECT RAISE(ABORT,'x'); END",
                    )
                    .unwrap();
            }
            Action::Continue
        });
        let e = h.e();
        let err = expect_err(h.commit(Some(&e)));
        fault::clear();
        assert_eq!(err.code(), "store_sqlite");
        assert_eq!(h.state_of(1).as_deref(), Some("staging"));
        let ready_at: Option<String> = h
            .raw()
            .query_row("SELECT ready_at FROM guide_versions", [], |r| r.get(0))
            .unwrap();
        assert_eq!(ready_at, None);
        assert_eq!(h.event_row(), h.claimed());
        assert_eq!(h.notices(), 0);
        assert!(h.published(1).exists());
        assert_eq!(h.refs(1).len(), 4);
        assert!(!exists(&h.staged(1)));
        h.raw().execute_batch("DROP TRIGGER fail_notice").unwrap();
        drop(h.reopen());
        assert_eq!(h.state_of(1).as_deref(), Some("ready"));
        assert_eq!(h.event_row().2.as_deref(), Some("guide_version"));
        assert_eq!(h.event_row().3.as_deref(), Some("1"));
        assert_eq!(h.notices(), 1);
    }

    /// U26
    #[test]
    fn reconcile_rolls_back_when_guides_entry_is_a_symlink() {
        let mut h = H::new();
        crash_at(Point::AfterPin);
        expect_err(h.commit(None));
        fault::clear();
        let outside = h.root.join("outside");
        fs::create_dir(&outside).unwrap();
        for (path, bytes) in h.draft_tree() {
            let to = outside.join(&path);
            fs::create_dir_all(to.parent().unwrap()).unwrap();
            fs::write(to, bytes).unwrap();
        }
        let before = tree(&outside);
        fs::create_dir_all(h.store.dir().join("guides")).unwrap();
        symlink(&outside, h.published(1)).unwrap();
        drop(h.reopen());
        assert_eq!(h.rows(), 0);
        assert!(!exists(&h.staged(1)));
        assert!(h.refs(1).is_empty());
        assert_eq!(h.notices(), 0);
        assert_eq!(fs::read_link(h.published(1)).unwrap(), outside);
        assert_eq!(tree(&outside), before);
    }

    /// U27
    #[test]
    fn leftover_staging_dir_is_replaced() {
        let mut h = H::new();
        fs::create_dir_all(h.staged(1)).unwrap();
        fs::write(h.staged(1).join("stray.txt"), "s").unwrap();
        h.commit(None).unwrap();
        assert_eq!(tree(&h.published(1)), h.draft_tree());

        let mut h = H::new();
        let outside = h.root.join("outside");
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("keep.txt"), "k").unwrap();
        fs::create_dir_all(h.store.dir().join("staging")).unwrap();
        symlink(&outside, h.staged(1)).unwrap();
        h.commit(None).unwrap();
        assert_eq!(fs::read(outside.join("keep.txt")).unwrap(), b"k");
        assert!(fs::symlink_metadata(h.published(1)).unwrap().is_dir());
        assert_eq!(tree(&h.published(1)), h.draft_tree());
    }

    // ---------------------------------------------------------------- lap 2

    fn set_provenance(h: &mut H, f: impl FnOnce(&mut serde_json::Value)) {
        let mut v: serde_json::Value = serde_json::from_str(&h.row.provenance_json).unwrap();
        f(&mut v);
        h.row.provenance_json = serde_json::to_string(&v).unwrap();
    }

    #[test]
    fn rollback_never_removes_through_a_symlinked_staging_dir() {
        let mut h = H::new();
        crash_at(Point::AfterReserve);
        expect_err(h.commit(None));
        fault::clear();
        let outside = h.root.join("outside");
        fs::create_dir_all(outside.join("v1")).unwrap();
        fs::write(outside.join("v1/keep.txt"), "k").unwrap();
        symlink(&outside, h.store.dir().join("staging")).unwrap();
        drop(h.reopen());
        assert_eq!(h.state_of(1), None);
        assert_eq!(fs::read(outside.join("v1/keep.txt")).unwrap(), b"k");
    }

    /// Wraps the fake clock; every read notes whether another connection can
    /// take the write lock at that moment, then moves the time past the lease.
    struct ProbeClock {
        inner: Arc<FakeClock>,
        db: PathBuf,
        reads: std::sync::Mutex<Vec<bool>>,
    }

    impl crate::store::Clock for ProbeClock {
        fn now(&self) -> Timestamp {
            let conn = Connection::open(&self.db).unwrap();
            conn.busy_timeout(Duration::ZERO).unwrap();
            let locked = match conn.execute_batch("BEGIN IMMEDIATE") {
                Ok(()) => {
                    conn.execute_batch("ROLLBACK").unwrap();
                    false
                }
                Err(_) => true,
            };
            self.reads.lock().unwrap().push(locked);
            self.inner.advance(LEASE);
            self.inner.now()
        }
    }

    #[test]
    fn the_reserve_gate_uses_the_time_after_the_write_lock_is_won() {
        let mut h = H::new();
        let probe = Arc::new(ProbeClock {
            inner: Arc::clone(&h.clock),
            db: h.store.dir().join("store.db"),
            reads: std::sync::Mutex::new(Vec::new()),
        });
        h.store = h
            .reopen()
            .with_clock(Arc::clone(&probe) as Arc<dyn crate::store::Clock>);
        let err = expect_err(h.commit(None));
        assert_eq!(err.code(), "run_stopped");
        let reads = probe.reads.lock().unwrap().clone();
        assert!(
            !reads.is_empty() && reads.iter().all(|locked| *locked),
            "every clock read happens while the write lock is held: {reads:?}"
        );
        let state: String = h
            .raw()
            .query_row(
                "SELECT state FROM runs WHERE id = ?1",
                [h.run.as_str()],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(state, "expired");
        no_trace(&h);
    }

    /// Runs `f` on a thread and fails the test if it does not finish in time.
    fn within_deadline<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(f());
        });
        rx.recv_timeout(Duration::from_secs(20))
            .expect("the commit did not finish in time")
    }

    #[test]
    fn provenance_errors_come_before_the_lock() {
        let mut h = H::new();
        h.row.provenance_json = "garbage".into();
        let mut lock = versions::open_lock(h.store.dir()).unwrap();
        let _held = lock.write().unwrap();
        let (h, err) = within_deadline(move || {
            let mut h = h;
            let err = expect_err(h.commit(None)).to_string();
            (h, err)
        });
        assert!(
            err.contains("the drafts row's provenance_json does not parse: "),
            "{err}"
        );
        no_trace(&h);
        drop(_held);

        let mut h = H::new();
        set_provenance(&mut h, |v| {
            v["selection"]["kind"] = "commits".into();
            v["selection"]["commits"] = serde_json::json!([]);
        });
        let mut lock = versions::open_lock(h.store.dir()).unwrap();
        let _held = lock.write().unwrap();
        let (h, err) = within_deadline(move || {
            let mut h = h;
            let err = expect_err(h.commit(None)).to_string();
            (h, err)
        });
        assert!(
            err.contains(
                "the drafts row's provenance_json has selection kind commits but no commits"
            ),
            "{err}"
        );
        no_trace(&h);
    }

    fn done_event(h: &H, kind: Option<&str>, id: Option<&str>) {
        h.raw()
            .execute(
                "UPDATE events SET state = 'done', result_kind = ?1, result_id = ?2, \
                 done_at = 't' WHERE id = ?3",
                rusqlite::params![kind, id, h.event],
            )
            .unwrap();
    }

    #[test]
    fn odd_done_events_are_reported() {
        #[allow(clippy::type_complexity)]
        let cases: [(Option<&str>, Option<&str>, &str, Option<&str>); 6] = [
            (None, None, "store_sqlite", None),
            (None, Some("1"), "store_sqlite", None),
            (Some("ack"), None, "store_sqlite", None),
            (Some("guide_version"), None, "store_sqlite", None),
            (Some("guide_version"), Some("x"), "store_sqlite", None),
            (
                Some("guide_version"),
                Some("9"),
                "internal",
                Some("is done with guide version 9, which this store does not have"),
            ),
        ];
        for (kind, id, code, text) in cases {
            let mut h = H::new();
            done_event(&h, kind, id);
            let e = h.e();
            let err = expect_err(h.commit(Some(&e)));
            assert_eq!(err.code(), code, "{kind:?} {id:?}: {err}");
            if let Some(text) = text {
                assert_eq!(err.to_string(), format!("internal error: event {e} {text}"));
            }
            no_trace(&h);
            assert_eq!(h.notices(), 0);
            assert!(h.lock_free());
        }
    }

    #[test]
    fn an_empty_git_common_dir_leaves_staging_rows_alone() {
        let h = H::new();
        h.raw()
            .execute(
                "UPDATE store_meta SET value = '' WHERE key = 'git_common_dir'",
                [],
            )
            .unwrap();
        insert_row(&h, 1, "staging", "[]");
        drop(h.reopen());
        assert_eq!(h.state_of(1).as_deref(), Some("staging"));
    }

    #[test]
    fn finish_refuses_an_absent_or_ready_row() {
        let mut h = H::new();
        let now = h.store.now();
        let e = h.e();
        let err = h
            .store
            .tx(|tx| versions::finish(tx, 1, Some(&e), now))
            .unwrap_err();
        assert!(err.to_string().contains("internal error"), "{err}");
        insert_row(&h, 2, "ready", "[]");
        let err = h
            .store
            .tx(|tx| versions::finish(tx, 2, Some(&e), now))
            .unwrap_err();
        assert!(err.to_string().contains("internal error"), "{err}");
        assert_eq!(h.notices(), 0);
        assert_eq!(h.event_row(), h.claimed());
    }

    #[test]
    fn a_destination_write_error_is_store_io_and_rolls_back() {
        let mut h = H::new();
        let staged = h.staged(1);
        fault::set(move |p| {
            if p == Point::CopyBeforeOpen && !staged.join("guide.yaml").exists() {
                fs::write(staged.join("guide.yaml"), "x").unwrap();
            }
            Action::Continue
        });
        let err = expect_err(h.commit(None));
        fault::clear();
        assert_eq!(err.code(), "store_io");
        h.nothing_left(1);
    }

    #[test]
    fn a_check_error_is_returned_and_rolled_back() {
        let mut h = H::new();
        set_provenance(&mut h, |v| {
            v.as_object_mut().unwrap().remove("format");
        });
        let err = expect_err(h.commit(None));
        assert_eq!(err.code(), "internal");
        assert!(err.to_string().contains("does not parse"), "{err}");
        h.nothing_left(1);

        let mut h = H::new();
        set_provenance(&mut h, |v| {
            v.as_object_mut().unwrap().remove("format");
        });
        fs::write(h.draft.join("notes.txt"), "n").unwrap();
        let err = expect_err(h.commit(None));
        assert_eq!(err.code(), "internal");
        assert!(err.to_string().contains("does not parse"), "{err}");
        h.nothing_left(1);
    }

    #[test]
    fn a_failed_rollback_keeps_the_original_error_and_the_row() {
        let mut h = H::new();
        fs::create_dir_all(h.store.dir().join("staging")).unwrap();
        fs::write(h.staged(1), "in the way").unwrap();
        let err = expect_err(h.commit(None));
        assert_eq!(err.code(), "store_io");
        assert_eq!(h.state_of(1).as_deref(), Some("staging"));
        fs::remove_file(h.staged(1)).unwrap();
        let v = h.commit(None).unwrap();
        assert_eq!(v.version, 1);
        assert_eq!(h.rows(), 1);
        assert_eq!(h.state_of(1).as_deref(), Some("ready"));

        let mut h = H::new();
        fs::create_dir_all(h.store.dir().join("staging")).unwrap();
        fs::write(h.staged(1), "in the way").unwrap();
        expect_err(h.commit(None));
        fs::remove_file(h.staged(1)).unwrap();
        drop(h.reopen());
        assert_eq!(h.state_of(1), None);
    }

    #[test]
    fn a_non_notfound_lstat_error_fails_reconcile() {
        let mut h = H::new();
        insert_row(&h, 1, "staging", "[]");
        fs::write(h.store.dir().join("guides"), "file").unwrap();
        drop(h.reopen());
        assert_eq!(h.state_of(1).as_deref(), Some("staging"));
        let err = expect_err(h.commit(None));
        assert_eq!(err.code(), "store_io");
        assert_eq!(h.state_of(1).as_deref(), Some("staging"));
    }

    #[test]
    fn an_unopenable_lock_file_does_not_fail_the_open() {
        let h = H::new();
        insert_row(&h, 1, "staging", "[]");
        fs::create_dir(h.store.dir().join(COMMIT_LOCK)).unwrap();
        drop(h.reopen());
        assert_eq!(h.state_of(1).as_deref(), Some("staging"));
    }

    /// R3: the commit's own error wins over a failing rollback.
    #[test]
    fn the_original_error_wins_over_a_failed_rollback() {
        for reopen in [true, false] {
            let mut h = H::new();
            let target = h.staged(1).join("slides/step-one.md");
            fault::set(move |p| {
                if p == Point::AfterCopy {
                    fs::write(&target, "garbage").unwrap();
                }
                Action::Continue
            });
            let refs_dir = h.root.join("repo/.git/refs/prguide/7/v1");
            fs::create_dir_all(&refs_dir).unwrap();
            let stale = refs_dir.join("snapshot-base.lock");
            fs::write(&stale, "").unwrap();
            let err = expect_err(h.commit(None));
            fault::clear();
            let Error::Guide(GuideError::CheckFailed {
                message,
                diagnostics,
            }) = err
            else {
                panic!("not check_failed")
            };
            let n = diagnostics
                .iter()
                .filter(|d| d.severity == super::super::check::Severity::Error)
                .count();
            assert_eq!(
                message,
                format!("the draft has {n} error(s); fix them and run prguide guide commit again")
            );
            assert!(diagnostics.iter().any(|d| d.code == "front_matter_missing"));
            assert_eq!(h.state_of(1).as_deref(), Some("staging"));
            assert!(!exists(&h.staged(1)));
            fs::remove_file(&stale).unwrap();
            if reopen {
                drop(h.reopen());
                assert_eq!(h.state_of(1), None);
            } else {
                let v = h.commit(None).unwrap();
                assert_eq!(v.version, 1);
                assert_eq!(h.rows(), 1);
            }
        }
    }

    struct RestoreMode(PathBuf);

    impl Drop for RestoreMode {
        fn drop(&mut self) {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(&self.0, fs::Permissions::from_mode(0o755));
        }
    }

    /// R4: a rollback that cannot lstat `staging/` stops before refs and row.
    #[test]
    fn a_rollback_that_cannot_inspect_staging_stops_early() {
        use std::os::unix::fs::PermissionsExt;
        let mut h = H::new();
        let dir = h.store.dir().to_path_buf();
        let _restore = RestoreMode(dir.clone());
        let d2 = dir.clone();
        fault::set(move |p| {
            if p == Point::AfterPin {
                fs::set_permissions(&d2, fs::Permissions::from_mode(0o000)).unwrap();
            }
            Action::Continue
        });
        let err = expect_err(h.commit(None));
        fault::clear();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
        if err.code() != "store_io" {
            // Permissions did not bite (running as root): nothing to test.
            return;
        }
        assert_eq!(h.state_of(1).as_deref(), Some("staging"));
        let mut want = h.pairs(1);
        want.sort();
        assert_eq!(h.refs(1), want);
        assert!(exists(&h.staged(1)));
        drop(h.reopen());
        h.nothing_left(1);
    }

    /// R5: a symlinked `guides/` is not a real directory, so reconcile rolls back.
    #[test]
    fn reconcile_rolls_back_when_guides_dir_is_a_symlink() {
        let mut h = H::new();
        crash_at(Point::AfterPin);
        expect_err(h.commit(None));
        fault::clear();
        let outside = h.root.join("outside");
        fs::create_dir_all(outside.join("v1")).unwrap();
        fs::write(outside.join("v1/planted.txt"), "p").unwrap();
        let before = tree(&outside);
        symlink(&outside, h.store.dir().join("guides")).unwrap();
        drop(h.reopen());
        assert_eq!(h.rows(), 0);
        assert!(!exists(&h.staged(1)));
        assert!(h.refs(1).is_empty());
        assert_eq!(h.notices(), 0);
        assert_eq!(
            fs::read_link(h.store.dir().join("guides")).unwrap(),
            outside
        );
        assert_eq!(tree(&outside), before);
    }

    fn mkfifo(path: &Path) {
        let status = std::process::Command::new("mkfifo")
            .arg(path)
            .status()
            .unwrap();
        assert!(status.success());
    }

    /// R6: every copy-reject text of A10, FIFOs included.
    #[test]
    fn copy_reject_table() {
        type Setup = fn(&Path, &Path);
        let cases: Vec<(&str, Setup, &str, &str)> = vec![
            (
                "draft dir is a file",
                |d, _| {
                    fs::remove_dir_all(d).unwrap();
                    fs::write(d, "x").unwrap();
                },
                ".",
                "is not a directory",
            ),
            (
                "guide.yaml symlink",
                |d, root| {
                    fs::write(root.join("g.yaml"), "x").unwrap();
                    fs::remove_file(d.join("guide.yaml")).unwrap();
                    symlink(root.join("g.yaml"), d.join("guide.yaml")).unwrap();
                },
                "guide.yaml",
                "is a symlink",
            ),
            (
                "guide.yaml fifo",
                |d, _| {
                    fs::remove_file(d.join("guide.yaml")).unwrap();
                    mkfifo(&d.join("guide.yaml"));
                },
                "guide.yaml",
                "is not a regular file",
            ),
            (
                "slides symlink",
                |d, root| {
                    fs::create_dir(root.join("sl")).unwrap();
                    fs::remove_dir_all(d.join("slides")).unwrap();
                    symlink(root.join("sl"), d.join("slides")).unwrap();
                },
                "slides",
                "is a symlink",
            ),
            (
                "slides regular file",
                |d, _| {
                    fs::remove_dir_all(d.join("slides")).unwrap();
                    fs::write(d.join("slides"), "x").unwrap();
                },
                "slides",
                "is not a directory",
            ),
            (
                "other top-level symlink",
                |d, root| symlink(root, d.join("zlink")).unwrap(),
                "zlink",
                "is a symlink",
            ),
            (
                "slide fifo",
                |d, _| {
                    fs::remove_file(d.join("slides/step-one.md")).unwrap();
                    mkfifo(&d.join("slides/step-one.md"));
                },
                "slides/step-one.md",
                "is not a regular file",
            ),
        ];
        for (name, setup, entry, what) in cases {
            let h = H::new();
            setup(&h.draft, &h.root);
            let want_message = format!(
                "prguide guide commit copies only guide.yaml and slides/<id>.md, and \
                 `{entry}` {what}; fix it and run prguide guide commit again"
            );
            let (h, outcome) = within_deadline(move || {
                let mut h = h;
                let want = check_draft(&h.git, &h.draft, &h.row.provenance_json);
                let got = h.commit(None);
                let out = match (got, want) {
                    (
                        Err(Error::Guide(GuideError::CheckFailed {
                            message,
                            diagnostics,
                        })),
                        Ok(w),
                    ) => Ok((
                        message,
                        serde_json::to_value(&diagnostics).unwrap(),
                        serde_json::to_value(&w.diagnostics).unwrap(),
                    )),
                    (Err(e), Err(w)) => Err((e.to_string(), w.to_string())),
                    (Err(e), Ok(_)) => Err((e.to_string(), "check ok".into())),
                    (Ok(_), _) => Err(("committed".into(), String::new())),
                };
                (h, out)
            });
            match outcome {
                Ok((message, got, want)) => {
                    assert_eq!(message, want_message, "{name}");
                    assert_eq!(got, want, "{name}");
                }
                Err((got, want)) => assert_eq!(got, want, "{name}: errors must agree"),
            }
            h.nothing_left(1);
        }
    }

    /// R6: an absent `slides/` copies fine; the check on the copy decides.
    #[test]
    fn absent_slides_dir_is_left_to_the_check() {
        let mut h = H::new();
        fs::remove_dir_all(h.draft.join("slides")).unwrap();
        let err = expect_err(h.commit(None));
        let Error::Guide(GuideError::CheckFailed { message, .. }) = err else {
            panic!("not check_failed: {err}")
        };
        assert!(message.starts_with("the draft has "), "{message}");
        h.nothing_left(1);
    }
}
