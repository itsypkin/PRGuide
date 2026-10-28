mod support;

use std::{
    cell::RefCell,
    collections::BTreeMap,
    fs,
    os::unix::fs::symlink,
    path::{Path, PathBuf},
    sync::Arc,
    thread,
    time::{Duration, Instant},
};

use prguide::{
    Error,
    cli::{
        guide_new::{GuideNewArgs, GuideNewEnv, GuideNewOutput, guide_new},
        start::{StartArgs, StartEnv, StartOutput, start},
    },
    git::Git,
    github::{
        Commit, CommitAuthor, GitHub, GitHubError, Pr, PrState, Repo,
        fake::{FakeGitHub, FakeMethod},
    },
    guide::{
        model::{GuideManifest, ManifestPr, ManifestSelection, ManifestSnapshot, SelectionKind},
        new::{Provenance, expand_commit, parse_commits_arg, render_manifest},
        parse::{load_draft, parse_manifest},
    },
    store::{
        Clock, FakeClock, PrRef, Store, StoreError, Timestamp,
        runs::{self, RunId, StopReason},
    },
};
use support::fixture_repo::FixtureRepo;
use tempfile::TempDir;

// ---------------------------------------------------------------- helpers

fn canon(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap()
}

/// Options that keep the test's own git calls from running hostile programs.
const NEUTRAL: &[&str] = &[
    "--no-replace-objects",
    "-c",
    "core.fsmonitor=false",
    "-c",
    "core.hooksPath=/dev/null",
    "-c",
    "submodule.recurse=false",
    "-c",
    "hook.post-checkout.enabled=false",
    "-c",
    "hook.reference-transaction.enabled=false",
    "-c",
    "hook.post-index-change.enabled=false",
    "-c",
    "filter.mine.smudge=",
    "-c",
    "filter.mine.clean=",
    "-c",
    "filter.mine.process=",
    "-c",
    "filter.mine.required=false",
    "-c",
    "filter.lfs.process=",
    "-c",
    "filter.lfs.required=false",
    "-c",
    "filter.a.b.smudge=",
    "-c",
    "core.autocrlf=false",
    "-c",
    "core.attributesFile=/dev/null",
];

fn tgit_try(
    fx: &FixtureRepo,
    dir: &Path,
    args: &[&str],
) -> Result<Vec<u8>, prguide::git::GitError> {
    let mut all: Vec<&str> = NEUTRAL.to_vec();
    all.extend_from_slice(args);
    fx.git(dir).env("GIT_OPTIONAL_LOCKS", "0").run(all)
}

fn tgit(fx: &FixtureRepo, dir: &Path, args: &[&str]) -> String {
    let out = tgit_try(fx, dir, args).unwrap();
    String::from_utf8_lossy(&out).trim().to_owned()
}

/// Files (bytes) and empty directories (None) under `root`, minus the root `.git`.
fn tree_map(root: &Path, skip: &[PathBuf]) -> BTreeMap<String, Option<Vec<u8>>> {
    fn walk(
        root: &Path,
        dir: &Path,
        skip: &[PathBuf],
        out: &mut BTreeMap<String, Option<Vec<u8>>>,
    ) {
        let entries: Vec<_> = fs::read_dir(dir).unwrap().map(|e| e.unwrap()).collect();
        let rel = dir
            .strip_prefix(root)
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let mut any = false;
        for entry in entries {
            let path = entry.path();
            if dir == root && entry.file_name() == ".git" {
                continue;
            }
            if skip.contains(&path) {
                continue;
            }
            any = true;
            let kind = fs::symlink_metadata(&path).unwrap().file_type();
            if kind.is_dir() {
                walk(root, &path, skip, out);
            } else {
                let key = path
                    .strip_prefix(root)
                    .unwrap()
                    .to_string_lossy()
                    .into_owned();
                let bytes = if kind.is_symlink() {
                    fs::read_link(&path)
                        .unwrap()
                        .to_string_lossy()
                        .into_owned()
                        .into_bytes()
                } else {
                    fs::read(&path).unwrap()
                };
                out.insert(key, Some(bytes));
            }
        }
        if !any && dir != root {
            out.insert(rel, None);
        }
    }
    let mut out = BTreeMap::new();
    walk(root, root, skip, &mut out);
    out
}

#[derive(Debug, PartialEq)]
struct State {
    status: String,
    head: String,
    symbolic: String,
    index: Vec<u8>,
    files: BTreeMap<String, Option<Vec<u8>>>,
    refs: String,
}

fn state(fx: &FixtureRepo, dir: &Path, skip: &[PathBuf]) -> State {
    let status = tgit(fx, dir, &["status", "--porcelain=v1", "--ignored", "-z"]);
    let symbolic = match tgit_try(fx, dir, &["symbolic-ref", "-q", "HEAD"]) {
        Ok(out) => String::from_utf8_lossy(&out).into_owned(),
        Err(_) => "detached".into(),
    };
    let index_path = tgit(fx, dir, &["rev-parse", "--git-path", "index"]);
    let index_path = if Path::new(&index_path).is_absolute() {
        PathBuf::from(index_path)
    } else {
        dir.join(index_path)
    };
    let refs = tgit(
        fx,
        dir,
        &["for-each-ref", "--format=%(objectname) %(refname)"],
    )
    .lines()
    .filter(|l| !l.contains(" refs/prguide/"))
    .collect::<Vec<_>>()
    .join("\n");
    State {
        status,
        head: tgit(fx, dir, &["rev-parse", "HEAD"]),
        symbolic,
        index: fs::read(index_path).unwrap_or_default(),
        files: tree_map(dir, skip),
        refs,
    }
}

fn assert_exact(fx: &FixtureRepo, wt: &Path, head: &str) {
    assert_exact_with(fx, wt, head, true);
}

fn assert_exact_with(fx: &FixtureRepo, wt: &Path, head: &str, check_status: bool) {
    assert_eq!(tgit(fx, wt, &["rev-parse", "HEAD"]), head);
    if check_status {
        let status = tgit(fx, wt, &["status", "--porcelain", "--ignored"]);
        assert_eq!(status, "", "worktree is not clean");
    }
    let listing = tgit_try(fx, wt, &["ls-tree", "-r", "-z", head]).unwrap();
    let files = tree_map(wt, &[]);
    let mut expected = Vec::new();
    for entry in listing.split(|b| *b == 0).filter(|e| !e.is_empty()) {
        let text = String::from_utf8_lossy(entry).into_owned();
        let (meta, path) = text.split_once('\t').unwrap();
        let fields: Vec<&str> = meta.split(' ').collect();
        expected.push(path.to_owned());
        if fields[0] == "160000" {
            assert_eq!(files.get(path), Some(&None), "gitlink {path}");
        } else {
            let blob = tgit_try(fx, wt, &["cat-file", "blob", fields[2]]).unwrap();
            assert_eq!(files.get(path), Some(&Some(blob)), "bytes of {path}");
        }
    }
    expected.sort();
    let actual: Vec<String> = files.keys().cloned().collect();
    assert_eq!(actual, expected, "file set");
}

fn on_pr(fx: &FixtureRepo) {
    fx.run(fx.work_path(), ["checkout", "-b", "pr"]);
}

fn simple_shape(fx: &FixtureRepo, files: &[(&str, &str)]) -> String {
    let base: Vec<_> = files
        .iter()
        .filter(|(p, _)| p.starts_with("base/"))
        .collect();
    for (p, c) in base {
        fx.write(p, c);
    }
    if files.iter().any(|(p, _)| p.starts_with("base/")) {
        fx.commit("base");
        fx.push("main");
    }
    on_pr(fx);
    for (p, c) in files.iter().filter(|(p, _)| !p.starts_with("base/")) {
        fx.write(p, c);
    }
    fx.commit("pr")
}

fn s1_shape(fx: &FixtureRepo) -> String {
    let init = fx.head();
    fx.write("mod.txt", "a\nb\nc\nd\ne\n");
    fx.write("del.txt", "gone\n");
    fx.write("ren.txt", "r1\nr2\nr3\nr4\nr5\nr6\nr7\nr8\nr9\nr10\n");
    fx.write("exec.sh", "echo hi\n");
    fx.gitlink("sm", &init);
    let base = fx.commit("base");
    fx.push("main");
    on_pr(fx);
    fx.write("added.txt", "one\ntwo\nthree\n");
    fx.write("mod.txt", "a\nB\nB2\nc\nd\ne\n");
    fx.remove("del.txt");
    fx.commit("first");
    fx.rename("ren.txt", "renamed.txt");
    fx.write(
        "renamed.txt",
        "r1\nr2\nr3\nr4\nr5\nr6\nr7\nr8\nr9\nchanged\n",
    );
    fx.write("bin.dat", b"\0\x01\x02bin\0");
    fx.set_executable("exec.sh", true);
    fx.gitlink("sm", &base);
    fx.commit("second")
}

fn dirty(fx: &FixtureRepo, dir: &Path) {
    fs::write(dir.join("README.md"), "changed\n").unwrap();
    fs::write(dir.join("staged.txt"), "staged\n").unwrap();
    fx.run(dir, ["add", "staged.txt"]);
    fs::write(dir.join("untracked.txt"), "untracked\n").unwrap();
    fs::write(dir.join("ignored.txt"), "ignored\n").unwrap();
}

/// A dirty linked worktree of the reviewer's clone, outside the prguide home.
fn dirty_linked(ctx: &Ctx, name: &str) -> PathBuf {
    let linked = ctx.fx.path().join(name);
    ctx.fx.run(
        &ctx.clone,
        [
            "worktree",
            "add",
            "--detach",
            linked.to_str().unwrap(),
            "HEAD",
        ],
    );
    let linked = canon(&linked);
    dirty(&ctx.fx, &linked);
    linked
}

struct Three {
    base: String,
    tip: String,
    c1: String,
    c2: String,
    c3: String,
}

fn three_shape(fx: &FixtureRepo) -> Three {
    fx.write("base.txt", "b\n");
    let base = fx.commit("base");
    fx.push("main");
    on_pr(fx);
    fx.write("one.txt", "1\n");
    let c1 = fx.commit("c1");
    fx.write("one.txt", "1\n2\n");
    fx.write("two.txt", "2\n");
    let c2 = fx.commit("c2");
    fx.write("three.txt", "3\n");
    let c3 = fx.commit("c3");
    fx.checkout("main");
    fx.write("main.txt", "m\n");
    let tip = fx.commit("main tip");
    fx.push("main");
    fx.checkout("pr");
    Three {
        base,
        tip,
        c1,
        c2,
        c3,
    }
}

struct Merge {
    c1: String,
    c2: String,
    s1: String,
    m: String,
    c4: String,
}

fn merge_shape(fx: &FixtureRepo) -> Merge {
    on_pr(fx);
    fx.write("one.txt", "1\n");
    let c1 = fx.commit("c1");
    fx.branch("side");
    fx.write("two.txt", "2\n");
    let c2 = fx.commit("c2");
    fx.checkout("side");
    fx.write("side.txt", "s\n");
    let s1 = fx.commit("s1");
    fx.checkout("pr");
    let m = fx.merge("side", "merge side");
    fx.write("four.txt", "4\n");
    let c4 = fx.commit("c4");
    Merge { c1, c2, s1, m, c4 }
}

struct Ctx {
    fx: FixtureRepo,
    home: PathBuf,
    _home_guard: TempDir,
    clone: PathBuf,
    fake: FakeGitHub,
    clock: Arc<FakeClock>,
    repo: Repo,
    owner: String,
    name: String,
    head: String,
    run: RefCell<String>,
}

impl Ctx {
    fn new() -> Self {
        Self::from_shape(s1_shape)
    }

    fn from_shape(shape: impl FnOnce(&FixtureRepo) -> String) -> Self {
        Self::build("octo", "widget", shape)
    }

    fn with_name(owner: &str, name: &str) -> Self {
        Self::build(owner, name, s1_shape)
    }

    fn three() -> (Self, Three) {
        let mut t = None;
        let ctx = Self::from_shape(|fx| {
            let s = three_shape(fx);
            let head = s.c3.clone();
            t = Some(s);
            head
        });
        (ctx, t.unwrap())
    }

    fn merged() -> (Self, Merge) {
        let mut t = None;
        let ctx = Self::from_shape(|fx| {
            let s = merge_shape(fx);
            let head = s.c4.clone();
            t = Some(s);
            head
        });
        (ctx, t.unwrap())
    }

    fn build(owner: &str, name: &str, shape: impl FnOnce(&FixtureRepo) -> String) -> Self {
        let fx = FixtureRepo::with_name(owner, name);
        let head = shape(&fx);
        fx.push_pr(7, &head);
        let clone = fx.clone("rev");
        let guard = TempDir::new().unwrap();
        let ctx = Self {
            home: canon(guard.path()),
            _home_guard: guard,
            clone: canon(&clone),
            fake: FakeGitHub::new(),
            clock: Arc::new(FakeClock::new(Timestamp::from_unix_millis(
                1_767_225_600_000,
            ))),
            repo: Repo::github(owner, name).unwrap(),
            owner: owner.into(),
            name: name.into(),
            head: head.clone(),
            run: RefCell::new(String::new()),
            fx,
        };
        ctx.publish(&head);
        ctx
    }

    fn pr(&self, head: &str) -> Pr {
        Pr {
            number: 7,
            state: PrState::Open,
            merged_at: None,
            closed_at: None,
            head_sha: head.into(),
            head_repo: Some(format!("{}/{}", self.owner, self.name)),
            base_ref: "main".into(),
            base_sha: self.fx.run(self.fx.work_path(), ["rev-parse", "main"]),
            title: "Add things".into(),
            body: "A body.".into(),
            author: "octocat".into(),
            url: format!("https://github.com/{}/{}/pull/7", self.owner, self.name),
        }
    }

    fn publish(&self, head: &str) {
        self.fake.set_pr(&self.repo, self.pr(head));
        let log = self.fx.run(
            self.fx.work_path(),
            [
                "log".to_owned(),
                "--reverse".into(),
                "--format=%H%x1f%P%x1f%s".into(),
                format!("main..{head}"),
            ],
        );
        let commits = log
            .lines()
            .map(|line| {
                let f: Vec<&str> = line.split('\u{1f}').collect();
                Commit {
                    sha: f[0].into(),
                    parents: f[1].split_whitespace().map(str::to_owned).collect(),
                    message: f[2].into(),
                    author: CommitAuthor {
                        name: "Fixture".into(),
                        login: None,
                    },
                }
            })
            .collect();
        self.fake.set_commits(&self.repo, 7, commits);
    }

    fn advance(&mut self, path: &str, contents: &str) -> String {
        self.fx.write(path, contents);
        let head = self.fx.commit(&format!("advance {path}"));
        self.fx.push_pr(7, &head);
        self.publish(&head);
        self.head = head.clone();
        head
    }

    fn pr_commits(&self) -> Vec<Commit> {
        self.fake.commits(&self.repo, 7).unwrap()
    }

    fn start(&self) -> StartOutput {
        let clock: Arc<dyn Clock> = self.clock.clone();
        let env = StartEnv {
            git: self.fx.git(&self.clone),
            github: &self.fake,
            home: self.home.clone(),
            clock,
        };
        let out = start(
            &env,
            &StartArgs {
                pr: 7,
                takeover: false,
            },
        )
        .unwrap();
        *self.run.borrow_mut() = out.run_id.to_string();
        out
    }

    fn new_guide_run(
        &self,
        run: &str,
        git: Git,
        commits: Option<&str>,
    ) -> Result<GuideNewOutput, Error> {
        let clock: Arc<dyn Clock> = self.clock.clone();
        let env = GuideNewEnv {
            git,
            github: &self.fake,
            home: self.home.clone(),
            clock,
        };
        guide_new(
            &env,
            &GuideNewArgs {
                run: run.into(),
                commits: commits.map(str::to_owned),
            },
        )
    }

    fn new_guide(&self, git: Git, commits: Option<&str>) -> Result<GuideNewOutput, Error> {
        let run = self.run.borrow().clone();
        self.new_guide_run(&run, git, commits)
    }

    /// guide new from the clone.
    fn gn(&self, commits: Option<&str>) -> Result<GuideNewOutput, Error> {
        self.new_guide(self.fx.git(&self.clone), commits)
    }

    fn ok(&self, commits: Option<&str>) -> GuideNewOutput {
        self.gn(commits).unwrap()
    }

    fn pr_ref(&self) -> PrRef {
        PrRef::new(&self.owner, &self.name, 7).unwrap()
    }

    fn store(&self) -> Store {
        let clock: Arc<dyn Clock> = self.clock.clone();
        Store::open_in(&self.home, &self.pr_ref())
            .unwrap()
            .with_clock(clock)
    }

    fn stop(&self) {
        let run = RunId::new(self.run.borrow().clone());
        runs::stop_run(&mut self.store(), &run, StopReason::End).unwrap();
    }

    fn query(&self, sql: &str) -> Vec<Vec<String>> {
        query_store(self.store(), sql)
    }

    fn drafts_rows(&self) -> Vec<Vec<String>> {
        self.query(
            "SELECT id, run_id, path, provenance_json, created_at FROM drafts \
             ORDER BY created_at, id",
        )
    }

    fn run_row(&self) -> Vec<String> {
        self.query("SELECT state, end_reason, lease_until FROM runs")
            .remove(0)
    }

    fn store_dir(&self) -> PathBuf {
        self.home
            .join("prs/github.com")
            .join(self.owner.to_lowercase())
            .join(self.name.to_lowercase())
            .join("7")
    }

    fn wt(&self) -> PathBuf {
        self.store_dir().join("worktree")
    }

    /// Writes an untracked marker in the worktree and returns its state.
    fn leftover(&self) -> State {
        fs::write(self.wt().join("leftover.txt"), "left\n").unwrap();
        state(&self.fx, &self.wt(), &[])
    }

    fn nothing_written(&self, wt_before: &State) {
        assert!(self.drafts_rows().is_empty(), "a drafts row was written");
        let drafts = self.store_dir().join("drafts");
        if drafts.exists() {
            assert_eq!(
                fs::read_dir(&drafts).unwrap().count(),
                0,
                "drafts/ not empty"
            );
        }
        assert_eq!(&state(&self.fx, &self.wt(), &[]), wt_before);
    }

    fn snapshot_json(&self, out: &GuideNewOutput) -> String {
        let text = serde_json::to_string_pretty(out).unwrap();
        text.replace(&out.draft_id, "[DRAFT_ID]")
            .replace(self.home.to_str().unwrap(), "[HOME]")
    }
}

fn query_store(mut store: Store, sql: &str) -> Vec<Vec<String>> {
    store
        .tx(|tx| -> Result<_, StoreError> {
            let mut st = tx.prepare(sql)?;
            let n = st.column_count();
            let rows = st.query_map([], |r| {
                (0..n)
                    .map(|i| {
                        r.get::<_, rusqlite::types::Value>(i).map(|v| match v {
                            rusqlite::types::Value::Text(t) => t,
                            rusqlite::types::Value::Null => "NULL".into(),
                            other => format!("{other:?}"),
                        })
                    })
                    .collect::<Result<Vec<_>, _>>()
            })?;
            Ok(rows.collect::<Result<Vec<_>, _>>()?)
        })
        .unwrap()
}

/// Asserts the error's code and that the message holds every substring.
fn assert_err(result: Result<GuideNewOutput, Error>, code: &str, parts: &[&str]) {
    let err = result.unwrap_err();
    assert_eq!(err.code(), code, "{err}");
    let message = err.to_string();
    for part in parts {
        assert!(message.contains(part), "`{part}` not in `{message}`");
    }
}

fn manifest_of(draft: &Path) -> GuideManifest {
    load_draft(draft).manifest.unwrap()
}

fn ncalls(ctx: &Ctx) -> usize {
    ctx.fake.calls().len()
}

/// Waits until a new Commits call shows up, then runs `act`.
fn commits_calls(fake: &FakeGitHub) -> usize {
    fake.calls()
        .iter()
        .filter(|c| c.method == FakeMethod::Commits)
        .count()
}

fn on_commits_call(fake: &FakeGitHub, seen: usize, act: impl FnOnce()) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if commits_calls(fake) > seen {
            break;
        }
        assert!(Instant::now() < deadline, "no Commits call");
        thread::sleep(Duration::from_millis(5));
    }
    act();
}

// ---------------------------------------------------------------- tests

fn is_ulid(id: &str) -> bool {
    id.len() == 26
        && id
            .bytes()
            .all(|b| b.is_ascii_digit() || (b.is_ascii_uppercase() && !b"ILOU".contains(&b)))
}

/// G1 full draft output snapshot.
#[test]
fn full_draft_output_snapshot() {
    let ctx = Ctx::new();
    let started = ctx.start();
    let out = ctx.ok(None);
    insta::assert_snapshot!("full_draft", ctx.snapshot_json(&out));
    let store = ctx.home.join("prs/github.com/octo/widget/7");
    assert_eq!(out.draft, store.join("drafts").join(&out.draft_id));
    assert!(is_ulid(&out.draft_id), "{}", out.draft_id);
    assert_eq!(out.worktree, store.join("worktree"));
    assert_eq!(out.snapshot.base, started.snapshot.base);
    assert_eq!(out.snapshot.head, started.snapshot.head);
    assert_eq!(out.commits, started.commits);
    assert_eq!(out.commits.len(), 2);
}

/// G2 full draft manifest snapshot.
#[test]
fn full_draft_manifest_snapshot() {
    let ctx = Ctx::new();
    ctx.start();
    let out = ctx.ok(None);
    let text = fs::read_to_string(out.draft.join("guide.yaml")).unwrap();
    insta::assert_snapshot!("full_manifest", text);
}

/// G3 a new draft is a valid empty draft.
#[test]
fn full_draft_is_a_valid_empty_draft() {
    let ctx = Ctx::new();
    ctx.start();
    let out = ctx.ok(None);
    let load = load_draft(&out.draft);
    assert!(load.errors.is_empty(), "{:?}", load.errors);
    assert!(load.slide_files.is_empty());
    assert!(load.slides.is_empty());
    let shas: Vec<String> = out.commits.iter().map(|c| c.sha.clone()).collect();
    assert_eq!(
        load.manifest,
        Some(GuideManifest {
            format: "prguide/v1".into(),
            pr: ManifestPr {
                host: "github.com".into(),
                repo: "octo/widget".into(),
                number: 7
            },
            snapshot: ManifestSnapshot {
                base: out.snapshot.base.clone(),
                head: out.snapshot.head.clone()
            },
            selection: ManifestSelection {
                kind: SelectionKind::Full,
                base: out.snapshot.base.clone(),
                head: out.snapshot.head.clone(),
                commits: shas
            },
            slides: vec![],
        })
    );
    let mut names: Vec<String> = fs::read_dir(&out.draft)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    assert_eq!(names, ["guide.yaml", "slides"]);
    assert!(
        fs::symlink_metadata(out.draft.join("guide.yaml"))
            .unwrap()
            .file_type()
            .is_file()
    );
    assert!(
        fs::symlink_metadata(out.draft.join("slides"))
            .unwrap()
            .file_type()
            .is_dir()
    );
    assert_eq!(fs::read_dir(out.draft.join("slides")).unwrap().count(), 0);
    let drafts: Vec<String> = fs::read_dir(out.draft.parent().unwrap())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(drafts, std::slice::from_ref(&out.draft_id));
}

/// G4 a single-commit range, snapshot.
#[test]
fn range_single_commit_snapshot() {
    let (ctx, t) = Ctx::three();
    ctx.start();
    let arg = format!("{}..{}", &t.c2[..7], &t.c2[..7]);
    let out = ctx.ok(Some(&arg));
    insta::assert_snapshot!("range_draft", ctx.snapshot_json(&out));
    assert_eq!(out.selection.kind, SelectionKind::Commits);
    assert_eq!(out.selection.base, t.c1);
    assert_eq!(out.selection.head, t.c2);
    let shas: Vec<&str> = out.commits.iter().map(|c| c.sha.as_str()).collect();
    assert_eq!(shas, [t.c2.as_str()]);
    assert_eq!(out.snapshot.base, t.base);
    assert_eq!(out.snapshot.head, t.c3);
    let files: Vec<(&str, &str)> = out
        .files
        .iter()
        .map(|f| (f.path.as_str(), f.status))
        .collect();
    assert_eq!(files, [("one.txt", "modified"), ("two.txt", "added")]);
    let m = manifest_of(&out.draft);
    assert_eq!(
        m.selection,
        ManifestSelection {
            kind: SelectionKind::Commits,
            base: t.c1.clone(),
            head: t.c2.clone(),
            commits: vec![t.c2.clone()],
        }
    );
    assert_eq!(
        m.snapshot,
        ManifestSnapshot {
            base: t.base.clone(),
            head: t.c3.clone()
        }
    );
    assert!(load_draft(&out.draft).errors.is_empty());
}

/// G5 a two-commit range and GitHub's order.
#[test]
fn range_two_commits_and_order() {
    let (ctx, t) = Ctx::three();
    ctx.start();
    let out = ctx.ok(Some(&format!("{}..{}", t.c1, t.c2)));
    assert_eq!(out.selection.base, t.base);
    assert_eq!(out.selection.head, t.c2);
    assert_eq!(out.commits, ctx.pr_commits()[..2].to_vec());
    let full = ctx.ok(None);
    let shas: Vec<String> = full.commits.iter().map(|c| c.sha.clone()).collect();
    assert_eq!(shas, [t.c1.clone(), t.c2.clone(), t.c3.clone()]);
    let mut sorted = shas.clone();
    sorted.sort();
    assert_ne!(
        shas, sorted,
        "adjust three_shape so GitHub's order isn't sorted"
    );
}

/// G6 the drafts row is the provenance.
#[test]
fn drafts_row_is_the_provenance() {
    let (ctx, t) = Ctx::three();
    ctx.start();
    let out = ctx.ok(None);
    let rows = ctx.drafts_rows();
    assert_eq!(rows.len(), 1);
    let row = &rows[0];
    assert_eq!(row[0], out.draft_id);
    assert_eq!(row[1], *ctx.run.borrow());
    assert_eq!(row[2], out.draft.to_string_lossy());
    assert_eq!(row[4], "2026-01-01T00:00:00.000Z");
    let (b, c1, c2, c3) = (&t.base, &t.c1, &t.c2, &t.c3);
    let literal = format!(
        "{{\"format\":\"prguide/v1\",\"pr\":{{\"host\":\"github.com\",\"repo\":\"octo/widget\",\"number\":7}},\"snapshot\":{{\"base\":\"{b}\",\"head\":\"{c3}\"}},\"selection\":{{\"kind\":\"full\",\"base\":\"{b}\",\"head\":\"{c3}\",\"commits\":[\"{c1}\",\"{c2}\",\"{c3}\"]}}}}"
    );
    assert_eq!(row[3], literal);
    let built = Provenance::from_manifest(&manifest_of(&out.draft));
    assert_eq!(row[3], serde_json::to_string(&built).unwrap());
}

/// G7 the repo casing comes from store_meta.
#[test]
fn repo_casing_comes_from_store_meta() {
    let ctx = Ctx::with_name("Octo", "Widget");
    ctx.start();
    let out = ctx.ok(None);
    assert_eq!(manifest_of(&out.draft).pr.repo, "Octo/Widget");
    let rows = ctx.drafts_rows();
    assert!(
        rows[0][3].contains("\"repo\":\"Octo/Widget\""),
        "{}",
        rows[0][3]
    );
    assert!(
        out.draft
            .to_string_lossy()
            .contains("/octo/widget/7/drafts/")
    );
}

/// G8 the worktree moves to the new PR head.
#[test]
fn worktree_moves_to_new_pr_head_since_start() {
    let mut ctx = Ctx::new();
    ctx.start();
    let h2 = ctx.advance("newer.txt", "newer\n");
    let out = ctx.ok(None);
    assert_eq!(out.snapshot.head, h2);
    assert_exact(&ctx.fx, &out.worktree, &h2);
    assert_eq!(out.commits.last().unwrap().sha, h2);
    assert_eq!(manifest_of(&out.draft).selection.commits.last(), Some(&h2));
    assert_eq!(
        tgit(
            &ctx.fx,
            &ctx.clone,
            &["rev-parse", "refs/prguide/7/fetch/head"]
        ),
        h2
    );
}

/// G9 the worktree is reset when the head is unchanged.
#[test]
fn worktree_reset_when_head_unchanged() {
    let ctx = Ctx::from_shape(|fx| {
        simple_shape(
            fx,
            &[
                (".gitignore", "*.log\n"),
                ("a.txt", "a\n"),
                ("b.txt", "b\n"),
            ],
        )
    });
    ctx.start();
    let wt = ctx.wt();
    fs::write(wt.join("a.txt"), "changed\n").unwrap();
    fs::remove_file(wt.join("b.txt")).unwrap();
    fs::write(wt.join("untracked.txt"), "u\n").unwrap();
    fs::create_dir(wt.join("udir")).unwrap();
    fs::write(wt.join("udir/f"), "f\n").unwrap();
    fs::write(wt.join("build.log"), "log\n").unwrap();
    ctx.ok(None);
    assert_exact(&ctx.fx, &wt, &ctx.head);
}

/// G10 a range draft's worktree is at the snapshot head.
#[test]
fn range_draft_worktree_at_snapshot_head() {
    let (ctx, t) = Ctx::three();
    ctx.start();
    let out = ctx.ok(Some(&format!("{}..{}", t.c1, t.c1)));
    assert_exact(&ctx.fx, &out.worktree, &t.c3);
    assert_ne!(tgit(&ctx.fx, &out.worktree, &["rev-parse", "HEAD"]), t.c1);
}

/// G11 the reviewer's clone and linked worktree are untouched.
#[test]
fn reviewer_clone_and_linked_worktree_untouched() {
    let ctx = Ctx::new();
    ctx.start();
    dirty(&ctx.fx, &ctx.clone);
    let linked = dirty_linked(&ctx, "linked");
    let before = (
        state(&ctx.fx, &ctx.clone, std::slice::from_ref(&linked)),
        state(&ctx.fx, &linked, &[]),
    );
    ctx.ok(None);
    let after = (
        state(&ctx.fx, &ctx.clone, std::slice::from_ref(&linked)),
        state(&ctx.fx, &linked, &[]),
    );
    assert_eq!(before, after);
}

/// G12 a planted worktree is rebuilt, not reset.
#[test]
fn planted_worktree_is_rebuilt_not_reset() {
    let ctx = Ctx::new();
    ctx.start();
    dirty(&ctx.fx, &ctx.clone);
    let wt = ctx.wt();
    fs::write(
        wt.join(".git"),
        format!("gitdir: {}\n", ctx.clone.join(".git").display()),
    )
    .unwrap();
    let before = state(&ctx.fx, &ctx.clone, &[]);
    ctx.ok(None);
    assert_eq!(state(&ctx.fx, &ctx.clone, &[]), before);
    let meta = fs::symlink_metadata(&wt).unwrap();
    assert!(meta.is_dir() && !meta.file_type().is_symlink());
    assert!(fs::symlink_metadata(wt.join(".git")).unwrap().is_file());
    assert_exact(&ctx.fx, &wt, &ctx.head);
}

/// G13 guide new runs from inside the prguide worktree.
#[test]
fn runs_from_inside_the_prguide_worktree() {
    let ctx = Ctx::new();
    ctx.start();
    let wt = ctx.wt();
    fs::create_dir_all(wt.join("junk")).unwrap();
    fs::write(wt.join("junk/x"), "x\n").unwrap();
    let out = ctx.new_guide(ctx.fx.git(wt.join("junk")), None).unwrap();
    assert_eq!(out.draft.parent().unwrap(), ctx.store_dir().join("drafts"));
    assert_exact(&ctx.fx, &wt, &ctx.head);
}

/// G14 a second guide new makes a new draft.
#[test]
fn second_guide_new_makes_a_new_draft() {
    let ctx = Ctx::new();
    ctx.start();
    let d1 = ctx.ok(None);
    fs::write(d1.draft.join("slides/s1-overview.md"), "x\n").unwrap();
    let mut yaml = fs::read_to_string(d1.draft.join("guide.yaml")).unwrap();
    yaml.push_str("# more\n");
    fs::write(d1.draft.join("guide.yaml"), yaml).unwrap();
    let before = tree_map(&d1.draft, &[]);
    let d2 = ctx.ok(None);
    assert_ne!(d1.draft_id, d2.draft_id);
    assert_eq!(ctx.drafts_rows().len(), 2);
    assert_eq!(tree_map(&d1.draft, &[]), before);
    let mut names: Vec<String> = fs::read_dir(d1.draft.parent().unwrap())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    let mut want = vec![d1.draft_id.clone(), d2.draft_id.clone()];
    want.sort();
    assert_eq!(names, want);
}

/// G15 an unknown run.
#[test]
fn unknown_run() {
    let ctx = Ctx::new();
    ctx.start();
    let before = ctx.leftover();
    let calls = ncalls(&ctx);
    let result = ctx.new_guide_run("r_zzzzzzzzzzzz", ctx.fx.git(&ctx.clone), None);
    assert_err(result, "run_unknown", &["r_zzzzzzzzzzzz"]);
    assert_eq!(ncalls(&ctx), calls);
    ctx.nothing_written(&before);
}

/// G16 a stopped run writes nothing.
#[test]
fn stopped_run_writes_nothing() {
    let ctx = Ctx::new();
    ctx.start();
    ctx.stop();
    let before = ctx.leftover();
    let calls = ncalls(&ctx);
    assert_err(ctx.gn(None), "run_stopped", &[]);
    assert_eq!(ncalls(&ctx), calls);
    ctx.nothing_written(&before);
}

/// G17 an expired run is committed as expired.
#[test]
fn expired_run_is_committed_expired() {
    let ctx = Ctx::new();
    ctx.start();
    let before = ctx.leftover();
    let calls = ncalls(&ctx);
    ctx.clock.advance(Duration::from_secs(31 * 60));
    assert_err(ctx.gn(None), "run_stopped", &[]);
    let row = ctx.run_row();
    assert_eq!((row[0].as_str(), row[1].as_str()), ("expired", "expired"));
    assert_eq!(ncalls(&ctx), calls);
    ctx.nothing_written(&before);
}

/// G18 the lease is renewed.
#[test]
fn lease_is_renewed() {
    let ctx = Ctx::new();
    ctx.start();
    ctx.clock.advance(Duration::from_secs(20 * 60));
    ctx.ok(None);
    assert_eq!(ctx.run_row()[2], "2026-01-01T00:50:00.000Z");
}

/// Runs guide new while `act` fires on its GitHub commits call.
fn mid_command(ctx: &Ctx, act: impl FnOnce() + Send) -> Result<GuideNewOutput, Error> {
    ctx.fake
        .set_delay(FakeMethod::Commits, Duration::from_secs(2));
    let seen = commits_calls(&ctx.fake);
    thread::scope(|s| {
        let fake = &ctx.fake;
        s.spawn(move || on_commits_call(fake, seen, act));
        ctx.gn(None)
    })
}

fn stopper(ctx: &Ctx) -> impl FnOnce() + Send + use<> {
    let (home, pr, clock, run) = (
        ctx.home.clone(),
        ctx.pr_ref(),
        ctx.clock.clone(),
        RunId::new(ctx.run.borrow().clone()),
    );
    move || {
        let clock: Arc<dyn Clock> = clock;
        let mut store = Store::open_in(&home, &pr).unwrap().with_clock(clock);
        runs::stop_run(&mut store, &run, StopReason::End).unwrap();
    }
}

/// G19 the run is stopped mid-command.
#[test]
fn run_stopped_mid_command() {
    let ctx = Ctx::new();
    ctx.start();
    let result = mid_command(&ctx, stopper(&ctx));
    assert_err(result, "run_stopped", &[]);
    assert!(ctx.drafts_rows().is_empty());
    let drafts = ctx.store_dir().join("drafts");
    if drafts.exists() {
        assert_eq!(fs::read_dir(drafts).unwrap().count(), 0);
    }
    assert_eq!(ctx.run_row()[0], "stopped");
}

/// G20 the run expires mid-command.
#[test]
fn run_expired_mid_command() {
    let ctx = Ctx::new();
    ctx.start();
    let clock = ctx.clock.clone();
    let result = mid_command(&ctx, move || clock.advance(Duration::from_secs(31 * 60)));
    assert_err(result, "run_stopped", &[]);
    assert!(ctx.drafts_rows().is_empty());
    let drafts = ctx.store_dir().join("drafts");
    if drafts.exists() {
        assert_eq!(fs::read_dir(drafts).unwrap().count(), 0);
    }
    assert_eq!(ctx.run_row()[0], "expired");
}

/// G21 a reversed range.
#[test]
fn range_reversed() {
    let (ctx, t) = Ctx::three();
    ctx.start();
    let before = ctx.leftover();
    assert_err(
        ctx.gn(Some(&format!("{}..{}", t.c3, t.c1))),
        "range_not_in_pr",
        &["range is reversed"],
    );
    ctx.nothing_written(&before);
}

/// G22 an endpoint that is not in the PR.
#[test]
fn range_endpoint_not_in_pr() {
    let (ctx, t) = Ctx::three();
    ctx.start();
    let before = ctx.leftover();
    let a = &t.tip[..7];
    assert_err(
        ctx.gn(Some(&format!("{a}..{}", t.c2))),
        "range_not_in_pr",
        &[&format!("commit {a} is not in the PR")],
    );
    ctx.nothing_written(&before);
    let b = &t.base[..7];
    assert_err(
        ctx.gn(Some(&format!("{}..{b}", t.c1))),
        "range_not_in_pr",
        &[&format!("commit {b} is not in the PR")],
    );
    ctx.nothing_written(&before);
}

/// G23 a range with a merge.
#[test]
fn range_with_merge() {
    let (ctx, t) = Ctx::merged();
    ctx.start();
    let before = ctx.leftover();
    assert_err(
        ctx.gn(Some(&format!("{}..{}", t.c1, t.c4))),
        "range_has_merge",
        &[&t.m, "merge"],
    );
    ctx.nothing_written(&before);
}

/// G24 a full draft with a merge succeeds.
#[test]
fn full_draft_with_merge_succeeds() {
    let (ctx, t) = Ctx::merged();
    ctx.start();
    let out = ctx.ok(None);
    let list: Vec<String> = ctx.pr_commits().into_iter().map(|c| c.sha).collect();
    assert_eq!(list, [t.c1, t.c2, t.s1, t.m, t.c4]);
    assert_eq!(manifest_of(&out.draft).selection.commits, list);
}

/// G25 a range whose commits are not consecutive.
#[test]
fn range_not_consecutive() {
    let (ctx, t) = Ctx::three();
    ctx.start();
    let mut list = ctx.pr_commits();
    list.swap(1, 2);
    ctx.fake.set_commits(&ctx.repo, 7, list);
    let before = ctx.leftover();
    assert_err(
        ctx.gn(Some(&format!("{}..{}", t.c1, t.c2))),
        "range_not_in_pr",
        &[&format!(
            "commits {} and {} are not consecutive",
            t.c1, t.c3
        )],
    );
    ctx.nothing_written(&before);
}

/// G26 the --commits syntax.
#[test]
fn commits_arg_syntax() {
    let hex65 = "a".repeat(65);
    let bad = [
        "".to_owned(),
        "abcd".into(),
        "abcd..".into(),
        "..abcd".into(),
        "abcd...abcd".into(),
        "abc..abcd".into(),
        "abcd..abc".into(),
        "g123..abcd".into(),
        "HEAD~1..HEAD".into(),
        "ab cd..abcd".into(),
        format!("{hex65}..abcd"),
    ];
    for value in &bad {
        let err = parse_commits_arg(value).unwrap_err();
        assert_eq!(err.code(), "usage", "{value}");
        let message = err.to_string();
        assert!(message.contains("--commits") && message.contains(value.as_str()));
    }
    assert_eq!(
        parse_commits_arg("ABCD..abcd").unwrap(),
        ("abcd".to_owned(), "abcd".to_owned())
    );
    let h40 = "0123456789abcdef0123456789abcdef01234567";
    let h64 = "f".repeat(64);
    assert_eq!(
        parse_commits_arg(&format!("{h40}..{h64}")).unwrap(),
        (h40.to_owned(), h64.clone())
    );
    let ctx = Ctx::new();
    let result = ctx.new_guide_run("r_nope", ctx.fx.git(&ctx.clone), Some("abc"));
    assert_err(result, "usage", &["--commits"]);
}

/// G27 expansion rules.
#[test]
fn expand_commit_rules() {
    let s0 = format!("0a1b2c3d{}", "0".repeat(32));
    let s1 = format!("bbbbb1{}", "0".repeat(34));
    let s2 = format!("bbbbb2{}", "0".repeat(34));
    let list = vec![s0, s1.clone(), s2.clone()];
    assert_eq!(expand_commit(&s1[..6], &list).unwrap(), s1);
    assert_eq!(expand_commit(&s2, &list).unwrap(), s2);
    let (up, _) = parse_commits_arg(&format!("{0}..{0}", s1[..6].to_uppercase())).unwrap();
    assert_eq!(expand_commit(&up, &list).unwrap(), s1);
    let err = expand_commit("bbbbb", &list).unwrap_err();
    assert_eq!(err.code(), "commit_ambiguous");
    let message = err.to_string();
    assert!(message.contains("bbbbb") && message.contains("give more digits"));
    assert!(message.find(&s1).unwrap() < message.find(&s2).unwrap());
    let err = expand_commit("2c3d", &list).unwrap_err();
    assert_eq!(err.code(), "range_not_in_pr");
    assert!(err.to_string().contains("commit 2c3d is not in the PR"));
    let err = expand_commit(&"9".repeat(40), &list).unwrap_err();
    assert_eq!(err.code(), "range_not_in_pr");
}

/// G28 a failed fetch writes nothing.
#[test]
fn fetch_failure_writes_nothing() {
    let ctx = Ctx::new();
    ctx.start();
    ctx.fx.run(
        ctx.fx.remote_path(),
        ["update-ref", "-d", "refs/pull/7/head"],
    );
    let before = ctx.leftover();
    assert_err(ctx.gn(None), "git_failed", &["fetch"]);
    ctx.nothing_written(&before);
    assert_eq!(ctx.run_row()[0], "active");
}

/// G29 a moved PR head writes nothing.
#[test]
fn pr_head_moved_writes_nothing() {
    let ctx = Ctx::new();
    ctx.start();
    let first = ctx.pr_commits()[0].sha.clone();
    ctx.fake.set_pr(&ctx.repo, ctx.pr(&first));
    let before = ctx.leftover();
    let err = ctx.gn(None).unwrap_err();
    assert_eq!(err.code(), "pr_head_moved");
    let message = err.to_string();
    for part in [
        first.as_str(),
        ctx.head.as_str(),
        "run prguide guide new again",
    ] {
        assert!(message.contains(part), "{message}");
    }
    assert!(!message.contains("prguide start"));
    ctx.nothing_written(&before);
}

/// G30 another clone is store_bound.
#[test]
fn other_clone_is_store_bound() {
    let ctx = Ctx::new();
    ctx.start();
    let other = canon(&ctx.fx.clone("other"));
    let before = ctx.leftover();
    let calls = ncalls(&ctx);
    let result = ctx.new_guide(ctx.fx.git(&other), None);
    let bound = canon(&ctx.clone.join(".git")).display().to_string();
    let current = canon(&other.join(".git")).display().to_string();
    assert_err(result, "store_bound", &[&bound, &current, "guide new"]);
    assert_eq!(ncalls(&ctx), calls);
    ctx.nothing_written(&before);
}

/// G31 a GitHub error passes through.
#[test]
fn github_error_passes_through() {
    let ctx = Ctx::new();
    ctx.start();
    ctx.fake.push_error(
        FakeMethod::Pr,
        GitHubError::PrNotFound {
            repo: "octo/widget".into(),
            number: 7,
        },
    );
    let before = ctx.leftover();
    assert_err(ctx.gn(None), "pr_not_found", &[]);
    ctx.nothing_written(&before);
}

/// G32 drafts/ that is not a real directory.
#[test]
fn drafts_not_a_real_directory() {
    let ctx = Ctx::new();
    ctx.start();
    let drafts = ctx.store_dir().join("drafts");
    let elsewhere = ctx.fx.path().join("elsewhere");
    fs::create_dir(&elsewhere).unwrap();
    symlink(&elsewhere, &drafts).unwrap();
    assert_err(ctx.gn(None), "store_io", &["drafts"]);
    assert!(ctx.drafts_rows().is_empty());
    assert_eq!(fs::read_dir(&elsewhere).unwrap().count(), 0);
    fs::remove_file(&drafts).unwrap();
    fs::write(&drafts, "file bytes").unwrap();
    assert_err(ctx.gn(None), "store_io", &["drafts"]);
    assert!(ctx.drafts_rows().is_empty());
    assert_eq!(fs::read(&drafts).unwrap(), b"file bytes");
}

/// G33 the draft path is canonical.
#[test]
fn draft_path_is_canonical() {
    let ctx = Ctx::new();
    let tmp = TempDir::new().unwrap();
    let real = canon(tmp.path()).join("real");
    fs::create_dir(&real).unwrap();
    let link = canon(tmp.path()).join("link");
    symlink(&real, &link).unwrap();
    let clock: Arc<dyn Clock> = ctx.clock.clone();
    let started = start(
        &StartEnv {
            git: ctx.fx.git(&ctx.clone),
            github: &ctx.fake,
            home: link.clone(),
            clock: clock.clone(),
        },
        &StartArgs {
            pr: 7,
            takeover: false,
        },
    )
    .unwrap();
    let out = guide_new(
        &GuideNewEnv {
            git: ctx.fx.git(&ctx.clone),
            github: &ctx.fake,
            home: link.clone(),
            clock,
        },
        &GuideNewArgs {
            run: started.run_id.to_string(),
            commits: None,
        },
    )
    .unwrap();
    assert_eq!(out.draft, canon(&out.draft));
    assert!(out.draft.starts_with(&real));
    assert_eq!(out.worktree, canon(&out.worktree));
    let store = Store::open_in(&link, &ctx.pr_ref()).unwrap();
    let rows = query_store(store, "SELECT path FROM drafts");
    assert_eq!(rows[0][0], out.draft.to_string_lossy());
}

/// G34 an empty PR: full draft and a range.
#[test]
fn empty_pr_full_draft_and_range() {
    let ctx = Ctx::from_shape(|fx| {
        on_pr(fx);
        fx.run(fx.work_path(), ["commit", "--allow-empty", "-m", "empty"]);
        fx.head()
    });
    ctx.start();
    ctx.fake.set_commits(&ctx.repo, 7, vec![]);
    let out = ctx.ok(None);
    assert!(manifest_of(&out.draft).selection.commits.is_empty());
    let text = fs::read_to_string(out.draft.join("guide.yaml")).unwrap();
    assert!(text.contains("  commits: []\n"));
    assert!(out.commits.is_empty() && out.files.is_empty());
    assert_err(
        ctx.gn(Some(&format!("{0}..{0}", ctx.head))),
        "range_not_in_pr",
        &[],
    );
}

/// G35 the files match start's.
#[test]
fn files_match_start() {
    let ctx = Ctx::new();
    let started = ctx.start();
    let out = ctx.ok(None);
    assert_eq!(
        serde_json::to_string(&out.files).unwrap(),
        serde_json::to_string(&started.files).unwrap()
    );
}

fn binary(args: &[&str]) -> (Option<i32>, Vec<u8>, serde_json::Value) {
    let tmp = TempDir::new().unwrap();
    let dir = canon(tmp.path());
    let output = assert_cmd::Command::new(env!("CARGO_BIN_EXE_prguide"))
        .args(args)
        .current_dir(&dir)
        .env("HOME", &dir)
        .env("PRGUIDE_HOME", dir.join("ph"))
        .env("GIT_CEILING_DIRECTORIES", dir.parent().unwrap())
        .output()
        .unwrap();
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert_eq!(stderr.lines().count(), 1, "{stderr}");
    (
        output.status.code(),
        output.stdout,
        serde_json::from_str(&stderr).unwrap(),
    )
}

/// G36 the binary prints an unknown run as a JSON error.
#[test]
fn binary_unknown_run_json_error() {
    let (code, stdout, err) = binary(&["guide", "new", "--run", "r_nope"]);
    assert_eq!(code, Some(1));
    assert!(stdout.is_empty());
    assert_eq!(err["code"], "run_unknown");
    assert!(err["message"].as_str().unwrap().contains("r_nope"));
}

/// G37 the binary reports a bad --commits as usage (both argument forms).
#[test]
fn binary_bad_commits_is_usage() {
    let forms: [&[&str]; 2] = [
        &["guide", "new", "--run", "r_nope", "--commits=abc"],
        &["guide", "new", "--run", "r_nope", "--commits", "abc"],
    ];
    for args in forms {
        let (code, stdout, err) = binary(args);
        assert_eq!(code, Some(1));
        assert!(stdout.is_empty());
        assert_eq!(err["code"], "usage");
        assert!(err["message"].as_str().unwrap().contains("--commits"));
    }
}

fn provenance(kind: SelectionKind, commits: &[&str]) -> Provenance {
    Provenance {
        format: "prguide/v1".into(),
        pr: ManifestPr {
            host: "github.com".into(),
            repo: "Octo/Widget".into(),
            number: 7,
        },
        snapshot: ManifestSnapshot {
            base: "1234".into(),
            head: "1e000".into(),
        },
        selection: ManifestSelection {
            kind,
            base: "aaaa".into(),
            head: "bbbb".into(),
            commits: commits.iter().map(|s| s.to_string()).collect(),
        },
    }
}

/// G38 the exact guide.yaml layout.
#[test]
fn render_manifest_layout() {
    let p = provenance(SelectionKind::Commits, &["aaaa", "bbbb"]);
    let text = render_manifest(&p);
    assert_eq!(
        text,
        "# Written by prguide guide new. Edit only `slides`.\n\
         format: \"prguide/v1\"\n\
         pr: { host: \"github.com\", repo: \"Octo/Widget\", number: 7 }\n\
         snapshot: { base: \"1234\", head: \"1e000\" }\n\
         selection:\n\
         \x20 kind: commits\n\
         \x20 base: \"aaaa\"\n\
         \x20 head: \"bbbb\"\n\
         \x20 commits:\n\
         \x20   - \"aaaa\"\n\
         \x20   - \"bbbb\"\n\
         slides: []\n"
    );
    let m = parse_manifest(&text).unwrap();
    assert_eq!(Provenance::from_manifest(&m), p);
    assert!(m.slides.is_empty());
    let p = provenance(SelectionKind::Full, &[]);
    let text = render_manifest(&p);
    assert!(
        text.contains(
            "  kind: full\n  base: \"aaaa\"\n  head: \"bbbb\"\n  commits: []\nslides: []\n"
        )
    );
    let m = parse_manifest(&text).unwrap();
    assert_eq!(Provenance::from_manifest(&m), p);
    assert!(m.slides.is_empty());
}

/// G39 the lease renewed at entry covers a long command.
#[test]
fn lease_renewed_at_entry_covers_a_long_command() {
    let ctx = Ctx::new();
    ctx.start();
    ctx.clock.advance(Duration::from_secs(20 * 60));
    let clock = ctx.clock.clone();
    let result = mid_command(&ctx, move || clock.advance(Duration::from_secs(15 * 60)));
    result.unwrap();
    assert_eq!(ctx.drafts_rows().len(), 1);
    let row = ctx.run_row();
    assert_eq!(row[0], "active");
    assert_eq!(row[2], "2026-01-01T01:05:00.000Z");
}

/// G40 a missing store_meta key is internal.
#[test]
fn store_meta_key_missing_is_internal() {
    let ctx = Ctx::new();
    ctx.start();
    ctx.store()
        .tx(|tx| -> Result<_, StoreError> {
            tx.execute("DELETE FROM store_meta WHERE key = 'owner'", [])?;
            Ok(())
        })
        .unwrap();
    let before = ctx.leftover();
    let calls = ncalls(&ctx);
    assert_err(ctx.gn(None), "internal", &["owner"]);
    assert_eq!(ncalls(&ctx), calls);
    ctx.nothing_written(&before);
}

/// G40b an empty store_meta value is internal too.
#[test]
fn store_meta_empty_value_is_internal() {
    let ctx = Ctx::new();
    ctx.start();
    ctx.store()
        .tx(|tx| -> Result<_, StoreError> {
            tx.execute("UPDATE store_meta SET value = '' WHERE key = 'repo'", [])?;
            Ok(())
        })
        .unwrap();
    let before = ctx.leftover();
    assert_err(ctx.gn(None), "internal", &["repo"]);
    ctx.nothing_written(&before);
}

/// G41 a drafts dir that cannot be written is store_io and leaves no row.
#[cfg(unix)]
#[test]
fn unwritable_drafts_dir_is_store_io() {
    use std::os::unix::fs::PermissionsExt;
    let ctx = Ctx::new();
    ctx.start();
    let drafts = ctx.store_dir().join("drafts");
    fs::create_dir_all(&drafts).unwrap();
    fs::set_permissions(&drafts, fs::Permissions::from_mode(0o555)).unwrap();
    let r = ctx.gn(None);
    fs::set_permissions(&drafts, fs::Permissions::from_mode(0o755)).unwrap();
    if fs::write(drafts.join("probe"), b"").is_ok() {
        // Running as a user who ignores permissions (root): nothing to assert.
        return;
    }
    assert_err(r, "store_io", &["drafts"]);
    assert!(ctx.drafts_rows().is_empty());
}
