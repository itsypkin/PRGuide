mod support;

use std::{
    collections::BTreeMap,
    fs,
    os::unix::{fs::symlink, process::CommandExt},
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    sync::{Arc, mpsc},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use prguide::{
    Error,
    cli::{
        check::{CheckArgs, CheckEnv, check},
        guide_commit::{GuideCommitArgs, GuideCommitEnv, guide_commit},
        guide_new::{GuideNewArgs, GuideNewEnv, GuideNewOutput, guide_new},
        start::{StartArgs, StartEnv, start},
    },
    github::{Commit, CommitAuthor, Pr, PrState, Repo, fake::FakeGitHub},
    store::{
        Clock, FakeClock, PrRef, Store, StoreError, Timestamp,
        events::{self, WaitOutcome},
        runs::{LEASE, RunId},
        versions::{self, GuideVersion},
    },
};
use rusqlite::Connection;
use support::fixture_repo::FixtureRepo;
use tempfile::TempDir;

const CAP: Duration = Duration::from_secs(30);

fn canon(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap()
}

fn slide(id: &str, kind: &str, flags: &[&str]) -> String {
    let mut s = format!("---\nid: {id}\nkind: {kind}\ntitle: T {id}\n");
    if !flags.is_empty() {
        s.push_str("flags:\n");
        for f in flags {
            s.push_str(&format!("  - {f}\n"));
        }
    }
    s.push_str(&format!("---\nBody of {id}.\n"));
    s
}

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

fn shape(fx: &FixtureRepo) -> String {
    fx.write("a.txt", "a\n");
    fx.commit("base");
    fx.push("main");
    fx.run(fx.work_path(), ["checkout", "-b", "pr"]);
    fx.write("b.txt", "b\n");
    fx.commit("first");
    fx.write("c.txt", "c\n");
    fx.commit("second")
}

/// A command with the isolated environment; it runs in `cwd`.
fn run_capped(mut cmd: Command) -> Output {
    let (pid, rx) = spawn_capped(&mut cmd);
    wait_capped(pid, rx)
}

fn spawn_capped(cmd: &mut Command) -> (u32, mpsc::Receiver<Output>) {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let child = cmd.spawn().unwrap();
    let pid = child.id();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(child.wait_with_output().unwrap());
    });
    (pid, rx)
}

fn wait_capped(pid: u32, rx: mpsc::Receiver<Output>) -> Output {
    match rx.recv_timeout(CAP) {
        Ok(out) => out,
        Err(_) => {
            let _ = Command::new("kill")
                .args(["-KILL", &pid.to_string()])
                .status();
            panic!("the binary did not finish within {CAP:?}");
        }
    }
}

struct Ctx {
    fx: FixtureRepo,
    home: PathBuf,
    _guard: TempDir,
    clone: PathBuf,
    fake: FakeGitHub,
    clock: Arc<FakeClock>,
    repo: Repo,
    run: String,
    out: GuideNewOutput,
    draft: PathBuf,
    c1: String,
    c2: String,
}

impl Ctx {
    fn new() -> Self {
        let fx = FixtureRepo::new();
        let head = shape(&fx);
        fx.push_pr(7, &head);
        let clone = fx.clone("rev");
        let guard = TempDir::new().unwrap();
        let ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64;
        let revs = fx.run(
            fx.work_path(),
            ["rev-list", "--reverse", &format!("main..{head}")],
        );
        let shas: Vec<String> = revs.lines().map(str::to_owned).collect();
        let mut ctx = Self {
            home: canon(guard.path()),
            _guard: guard,
            clone: canon(&clone),
            fake: FakeGitHub::new(),
            clock: Arc::new(FakeClock::new(Timestamp::from_unix_millis(ms))),
            repo: Repo::github("octo", "widget").unwrap(),
            run: String::new(),
            out: GuideNewOutput {
                draft_id: String::new(),
                draft: PathBuf::new(),
                worktree: PathBuf::new(),
                snapshot: prguide::cli::start::SnapshotOut {
                    base: String::new(),
                    head: String::new(),
                },
                selection: prguide::cli::guide_new::SelectionOut {
                    kind: prguide::guide::model::SelectionKind::Full,
                    base: String::new(),
                    head: String::new(),
                },
                commits: vec![],
                files: vec![],
            },
            draft: PathBuf::new(),
            c1: shas[0].clone(),
            c2: shas[1].clone(),
            fx,
        };
        ctx.publish(&head);
        ctx.run = ctx.start_run();
        ctx.out = ctx.gn(&ctx.run.clone(), None);
        ctx.draft = ctx.out.draft.clone();
        ctx.deck(&ctx.draft);
        ctx
    }

    fn publish(&self, head: &str) {
        self.fake.set_pr(
            &self.repo,
            Pr {
                number: 7,
                state: PrState::Open,
                merged_at: None,
                closed_at: None,
                head_sha: head.into(),
                head_repo: Some("octo/widget".into()),
                base_ref: "main".into(),
                base_sha: self.fx.run(self.fx.work_path(), ["rev-parse", "main"]),
                title: "Add things".into(),
                body: "A body.".into(),
                author: "octocat".into(),
                url: "https://github.com/octo/widget/pull/7".into(),
            },
        );
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

    fn clock_arc(&self) -> Arc<dyn Clock> {
        self.clock.clone()
    }

    fn start_run(&self) -> String {
        let env = StartEnv {
            git: self.fx.git(&self.clone),
            github: &self.fake,
            home: self.home.clone(),
            clock: self.clock_arc(),
        };
        start(
            &env,
            &StartArgs {
                pr: 7,
                takeover: false,
            },
        )
        .unwrap()
        .run_id
        .to_string()
    }

    fn gn(&self, run: &str, commits: Option<&str>) -> GuideNewOutput {
        let env = GuideNewEnv {
            git: self.fx.git(&self.clone),
            github: &self.fake,
            home: self.home.clone(),
            clock: self.clock_arc(),
        };
        guide_new(
            &env,
            &GuideNewArgs {
                run: run.into(),
                commits: commits.map(str::to_owned),
            },
        )
        .unwrap()
    }

    /// A fresh draft with the valid deck written into it.
    fn new_draft(&self, commits: Option<&str>) -> PathBuf {
        let out = self.gn(&self.run, commits);
        self.deck(&out.draft);
        out.draft
    }

    fn deck(&self, draft: &Path) {
        self.deck_of(
            draft,
            &[
                ("overview", slide("overview", "overview", &[])),
                ("arch", slide("arch", "architecture", &[])),
                ("step-one", slide("step-one", "step", &[])),
            ],
        );
    }

    fn deck_of(&self, draft: &Path, slides: &[(&str, String)]) {
        let yaml = fs::read_to_string(draft.join("guide.yaml")).unwrap();
        let ids: Vec<&str> = slides.iter().map(|(i, _)| *i).collect();
        let lines: Vec<String> = yaml
            .lines()
            .map(|l| {
                if l.starts_with("slides:") {
                    format!("slides: [{}]", ids.join(", "))
                } else {
                    l.to_owned()
                }
            })
            .collect();
        fs::write(draft.join("guide.yaml"), lines.join("\n") + "\n").unwrap();
        let dir = draft.join("slides");
        for e in fs::read_dir(&dir).unwrap() {
            fs::remove_file(e.unwrap().path()).unwrap();
        }
        for (id, text) in slides {
            fs::write(dir.join(format!("{id}.md")), text).unwrap();
        }
    }

    fn env(&self) -> GuideCommitEnv {
        GuideCommitEnv {
            git: self.fx.git(&self.clone),
            home: self.home.clone(),
            clock: self.clock_arc(),
        }
    }

    fn commit(
        &self,
        draft: &Path,
        run: &str,
        caused_by: Option<&str>,
    ) -> Result<GuideVersion, Error> {
        guide_commit(
            &self.env(),
            &GuideCommitArgs {
                draft: draft.to_path_buf(),
                run: run.into(),
                caused_by: caused_by.map(str::to_owned),
            },
        )
    }

    fn ok(&self, draft: &Path, caused_by: Option<&str>) -> GuideVersion {
        self.commit(draft, &self.run, caused_by).unwrap()
    }

    fn err(&self, draft: &Path, caused_by: Option<&str>) -> Error {
        match self.commit(draft, &self.run, caused_by) {
            Ok(v) => panic!("expected an error, got v{}", v.version),
            Err(e) => e,
        }
    }

    fn sdir(&self) -> PathBuf {
        self.home.join("prs/github.com/octo/widget/7")
    }

    fn store(&self) -> Store {
        Store::open_in(&self.home, &PrRef::new("octo", "widget", 7).unwrap())
            .unwrap()
            .with_clock(self.clock_arc())
    }

    fn raw(&self) -> Connection {
        let c = Connection::open(self.sdir().join("store.db")).unwrap();
        c.busy_timeout(Duration::from_secs(5)).unwrap();
        c
    }

    fn count(&self, sql: &str) -> i64 {
        self.raw().query_row(sql, [], |r| r.get(0)).unwrap()
    }

    fn rows(&self) -> i64 {
        self.count("SELECT COUNT(*) FROM guide_versions")
    }

    fn notices(&self) -> Vec<(String, String)> {
        self.raw()
            .prepare("SELECT kind, ref FROM notices ORDER BY seq")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    }

    fn git(&self) -> prguide::git::Git {
        self.fx.git(&self.clone)
    }

    fn refs(&self, k: i64) -> Vec<(String, String)> {
        self.git()
            .list_refs(&format!("refs/prguide/7/v{k}/"))
            .unwrap()
    }

    /// A generate_request claimed by the run (or left pending).
    fn event(&self, first: Option<&str>, last: Option<&str>, claim: bool) -> String {
        let mut store = self.store();
        let now = store.now();
        let id = store
            .tx(|tx| events::append_generate_request(tx, now, first, last))
            .unwrap()
            .id;
        if claim {
            self.claim(&mut store);
        }
        id
    }

    fn claim(&self, store: &mut Store) {
        let r = events::wait(
            store,
            &RunId::new(&self.run),
            Some(Duration::ZERO),
            std::num::NonZeroU32::new(10).unwrap(),
        )
        .unwrap();
        assert!(matches!(r, WaitOutcome::Events(_)));
    }

    /// state, claimed_by, result_kind, result_id, done_at.
    #[allow(clippy::type_complexity)]
    fn event_row(
        &self,
        id: &str,
    ) -> (
        String,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
    ) {
        self.raw()
            .query_row(
                "SELECT state, claimed_by, result_kind, result_id, done_at FROM events \
                 WHERE id = ?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )
            .unwrap()
    }

    fn lock_free(&self) -> bool {
        let mut lock = versions::open_lock(&self.sdir()).unwrap();
        lock.try_write().is_ok()
    }

    /// Nothing is left of version `k`. `rows` is how many version rows remain.
    fn nothing_left(&self, k: i64, rows: i64, notices: usize) {
        assert_eq!(
            self.count(&format!(
                "SELECT COUNT(*) FROM guide_versions WHERE version = {k}"
            )),
            0
        );
        assert_eq!(self.rows(), rows);
        assert!(!exists(&self.sdir().join(format!("staging/v{k}"))));
        assert!(!exists(&self.sdir().join(format!("guides/v{k}"))));
        assert!(self.refs(k).is_empty());
        assert_eq!(self.notices().len(), notices);
        assert!(self.lock_free());
    }

    fn cmd(&self, cwd: &Path) -> Command {
        let home = self.fx.path().join("home");
        let mut c = Command::new(env!("CARGO_BIN_EXE_prguide"));
        c.current_dir(cwd)
            .env("HOME", &home)
            .env("XDG_CONFIG_HOME", home.join(".config"))
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", home.join(".gitconfig"))
            .env("PRGUIDE_HOME", &self.home);
        c
    }

    fn bin_commit_cmd(&self, cwd: &Path, draft: &Path, caused_by: Option<&str>) -> Command {
        let mut c = self.cmd(cwd);
        c.args(["guide", "commit"])
            .arg(draft)
            .args(["--run", &self.run]);
        if let Some(e) = caused_by {
            c.arg(format!("--caused-by={e}"));
        }
        c
    }

    fn bin_commit(&self, draft: &Path, caused_by: Option<&str>) -> Output {
        run_capped(self.bin_commit_cmd(&self.clone, draft, caused_by))
    }

    fn bin_check(&self, draft: &Path) -> Output {
        let mut c = self.cmd(&self.clone);
        c.arg("check").arg(draft).args(["--run", &self.run]);
        run_capped(c)
    }

    /// `prguide status --run R` from a directory outside any repo.
    fn bin_status(&self) -> Output {
        let outside = TempDir::new().unwrap();
        let mut c = self.cmd(outside.path());
        c.args(["status", "--run", &self.run]);
        run_capped(c)
    }

    /// check's diagnostics for the draft, as JSON.
    fn check_diags(&self, draft: &Path) -> serde_json::Value {
        let report = check(
            &CheckEnv {
                git: self.git(),
                home: self.home.clone(),
                clock: self.clock_arc(),
            },
            &CheckArgs {
                draft: draft.to_path_buf(),
                run: self.run.clone(),
            },
        )
        .unwrap();
        serde_json::to_value(&report.diagnostics).unwrap()
    }

    fn insert_version(&self, k: i64, state: &str, caused_by: Option<&str>, refs_json: &str) {
        self.raw()
            .execute(
                "INSERT INTO guide_versions (version, state, caused_by, snapshot_base, \
                 snapshot_head, selection_kind, selection_base, selection_head, commits_json, \
                 refs_json, created_at, ready_at) VALUES (?1, ?2, ?3, 'x', 'x', 'full', 'x', \
                 'x', '[]', ?4, 't', ?5)",
                rusqlite::params![
                    k,
                    state,
                    caused_by,
                    refs_json,
                    (state == "ready").then_some("r")
                ],
            )
            .unwrap();
    }

    fn refs_json(&self, k: i64) -> String {
        let o = &self.out;
        let pairs: Vec<_> = [
            ("snapshot-base", &o.snapshot.base),
            ("snapshot-head", &o.snapshot.head),
            ("selection-base", &o.selection.base),
            ("selection-head", &o.selection.head),
        ]
        .iter()
        .map(|(n, s)| serde_json::json!({"name": format!("refs/prguide/7/v{k}/{n}"), "sha": s}))
        .collect();
        serde_json::to_string(&pairs).unwrap()
    }

    fn pair_list(&self, k: i64) -> Vec<(String, String)> {
        let v: Vec<serde_json::Value> = serde_json::from_str(&self.refs_json(k)).unwrap();
        v.iter()
            .map(|r| {
                (
                    r["name"].as_str().unwrap().into(),
                    r["sha"].as_str().unwrap().into(),
                )
            })
            .collect()
    }

    /// A hand-built "crashed after pin" state for version `k`.
    fn crashed_after_pin(&self, k: i64, caused_by: Option<&str>) {
        self.insert_version(k, "staging", caused_by, &self.refs_json(k));
        self.git().write_refs(&self.pair_list(k)).unwrap();
        let staged = self.sdir().join(format!("staging/v{k}"));
        copy_tree(&self.draft, &staged);
    }
}

fn copy_tree(from: &Path, to: &Path) {
    for (rel, bytes) in tree(from) {
        let p = to.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, bytes).unwrap();
    }
}

const COPY_PREFIX: &str = "prguide guide commit copies only guide.yaml and slides/<id>.md, and ";
const COPY_SUFFIX: &str = "; fix it and run prguide guide commit again";

fn copy_message(entry: &str, what: &str) -> String {
    format!("{COPY_PREFIX}`{entry}` {what}{COPY_SUFFIX}")
}

fn check_failed(err: Error) -> (String, serde_json::Value) {
    match err {
        Error::Guide(prguide::guide::GuideError::CheckFailed {
            message,
            diagnostics,
        }) => (message, serde_json::to_value(&diagnostics).unwrap()),
        other => panic!("expected check_failed, got {}: {other}", other.code()),
    }
}

fn codes(diags: &serde_json::Value) -> Vec<String> {
    diags
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["code"].as_str().unwrap().to_owned())
        .collect()
}

fn stderr_json(out: &Output) -> serde_json::Value {
    let text = String::from_utf8(out.stderr.clone()).unwrap();
    assert_eq!(text.lines().count(), 1, "{text}");
    serde_json::from_str(&text).unwrap()
}

// ---------------------------------------------------------------- G1-G6

/// G1
#[test]
fn full_guide_commits_v1() {
    let ctx = Ctx::new();
    let other = ctx.event(None, None, true);
    let before = ctx.event_row(&other);
    let draft_before = tree(&ctx.draft);
    let v = ctx.ok(&ctx.draft, None);
    assert_eq!((v.version, v.state.as_str()), (1, "ready"));
    assert_eq!(v.dir, ctx.sdir().join("guides/v1"));
    assert_eq!(v.caused_by, None);
    assert_eq!(v.snapshot.base, ctx.out.snapshot.base);
    assert_eq!(v.snapshot.head, ctx.out.snapshot.head);
    assert_eq!(v.selection.kind, "full");
    assert_eq!(v.selection.base, ctx.out.selection.base);
    assert_eq!(v.selection.head, ctx.out.selection.head);
    let shas: Vec<String> = ctx.out.commits.iter().map(|c| c.sha.clone()).collect();
    assert_eq!(v.selection.commits, shas);
    let names = [
        "snapshot-base",
        "snapshot-head",
        "selection-base",
        "selection-head",
    ];
    let want_shas = [
        &ctx.out.snapshot.base,
        &ctx.out.snapshot.head,
        &ctx.out.selection.base,
        &ctx.out.selection.head,
    ];
    assert_eq!(v.refs.len(), 4);
    for (i, r) in v.refs.iter().enumerate() {
        assert_eq!(r.name, format!("refs/prguide/7/v1/{}", names[i]));
        assert_eq!(&r.sha, want_shas[i]);
    }
    let mut listed = ctx.refs(1);
    let mut mine: Vec<(String, String)> = v
        .refs
        .iter()
        .map(|r| (r.name.clone(), r.sha.clone()))
        .collect();
    listed.sort();
    mine.sort();
    assert_eq!(listed, mine);
    let now = ctx.clock.now().to_rfc3339();
    assert_eq!(v.created_at, now);
    assert_eq!(v.ready_at.as_deref(), Some(now.as_str()));
    assert_eq!(tree(&v.dir), draft_before);
    assert_eq!(tree(&v.dir).len(), 4);
    assert!(!exists(&ctx.sdir().join("staging/v1")));
    assert_eq!(ctx.notices(), vec![("guide_ready".into(), "1".into())]);
    assert_eq!(tree(&ctx.draft), draft_before);
    assert_eq!(ctx.event_row(&other), before);
}

/// G2
#[test]
fn range_guide_with_caused_by_marks_the_event_done() {
    let ctx = Ctx::new();
    let range = format!("{0}..{0}", ctx.c1);
    let draft = ctx.new_draft(Some(&range));
    let e = ctx.event(Some(&ctx.c1), Some(&ctx.c1), true);
    let v = ctx.ok(&draft, Some(&e));
    assert_eq!(v.caused_by.as_deref(), Some(e.as_str()));
    assert_eq!(v.selection.kind, "commits");
    let (state, _, kind, id, done_at) = ctx.event_row(&e);
    assert_eq!(state, "done");
    assert_eq!(kind.as_deref(), Some("guide_version"));
    assert_eq!(id.as_deref(), Some("1"));
    assert!(done_at.is_some());
}

/// G3
#[test]
fn full_event_matches_a_full_draft() {
    let ctx = Ctx::new();
    let e = ctx.event(None, None, true);
    ctx.ok(&ctx.draft, Some(&e));
    assert_eq!(ctx.event_row(&e).0, "done");
}

/// G4
#[test]
fn second_commit_is_v2() {
    let ctx = Ctx::new();
    let d2 = ctx.new_draft(None);
    let v1 = ctx.ok(&ctx.draft, None);
    let v2 = ctx.ok(&d2, None);
    assert_eq!((v1.version, v2.version), (1, 2));
    assert_eq!((v1.state.as_str(), v2.state.as_str()), ("ready", "ready"));
    assert_eq!((ctx.refs(1).len(), ctx.refs(2).len()), (4, 4));
    assert_eq!(
        ctx.notices(),
        vec![
            ("guide_ready".into(), "1".into()),
            ("guide_ready".into(), "2".into())
        ]
    );
}

/// G5
#[test]
fn binary_repeat_prints_the_same_stdout() {
    let ctx = Ctx::new();
    let e = ctx.event(None, None, true);
    let first = ctx.bin_commit(&ctx.draft, Some(&e));
    let second = ctx.bin_commit(&ctx.draft, Some(&e));
    for o in [&first, &second] {
        assert_eq!(
            o.status.code(),
            Some(0),
            "{}",
            String::from_utf8_lossy(&o.stderr)
        );
        assert!(o.stderr.is_empty());
    }
    assert_eq!(first.stdout, second.stdout);
    let text = String::from_utf8(first.stdout.clone()).unwrap();
    assert_eq!(text.matches('\n').count(), 1);
    assert!(text.ends_with('\n'));
    let v: serde_json::Value = serde_json::from_str(&text).unwrap();
    let keys: Vec<&str> = v.as_object().unwrap().keys().map(String::as_str).collect();
    assert_eq!(keys.len(), 9);
    assert_eq!(v["version"], 1);
    assert_eq!(v["state"], "ready");
    assert_eq!(v["caused_by"], e);
    assert_eq!(v["refs"].as_array().unwrap().len(), 4);
    assert!(text.starts_with("{\"version\":1,\"state\":\"ready\",\"dir\":"));
    assert_eq!((ctx.rows(), ctx.notices().len()), (1, 1));
    assert_eq!(ctx.refs(1).len(), 4);
}

/// G6
#[test]
fn repeat_with_another_draft_returns_the_version() {
    let ctx = Ctx::new();
    let e = ctx.event(None, None, true);
    let v1 = ctx.ok(&ctx.draft, Some(&e));
    let d2 = ctx.new_draft(None);
    let again = ctx.ok(&d2, Some(&e));
    assert_eq!(v1, again);
    assert_eq!(ctx.rows(), 1);
}

// ---------------------------------------------------------------- G7-G12

/// G7
#[test]
fn binary_check_failed_refuses() {
    let ctx = Ctx::new();
    ctx.deck_of(
        &ctx.draft,
        &[("overview", slide("overview", "overview", &[]))],
    );
    let e = ctx.event(None, None, true);
    let out = ctx.bin_commit(&ctx.draft, Some(&e));
    assert_eq!(out.status.code(), Some(1));
    assert!(out.stdout.is_empty());
    let text = String::from_utf8(out.stderr.clone()).unwrap();
    assert!(
        text.starts_with("{\"code\":\"check_failed\",\"message\":"),
        "{text}"
    );
    let v = stderr_json(&out);
    let mut keys: Vec<&str> = v.as_object().unwrap().keys().map(String::as_str).collect();
    keys.sort();
    assert_eq!(keys, ["code", "diagnostics", "message"]);
    assert_eq!(v["code"], "check_failed");
    assert_eq!(
        v["message"],
        "the draft has 1 error(s); fix them and run prguide guide commit again"
    );
    assert_eq!(v["diagnostics"].as_array().unwrap().len(), 2);
    ctx.nothing_left(1, 0, 0);
    let checked = ctx.bin_check(&ctx.draft);
    let c: serde_json::Value = serde_json::from_slice(&checked.stdout).unwrap();
    assert_eq!(v["diagnostics"], c["diagnostics"]);
    ctx.nothing_left(1, 0, 0);
    assert_eq!(ctx.event_row(&e).0, "claimed");
}

/// G8
#[test]
fn binary_check_failed_is_escaped() {
    let ctx = Ctx::new();
    let flag = "{ at: { block: \"\\e[2J\\x85\u{202e}\\x7f\\u009b\" }, text: \"Hostile.\" }";
    ctx.deck_of(
        &ctx.draft,
        &[
            ("overview", slide("overview", "overview", &[])),
            ("step-one", slide("step-one", "step", &[flag])),
        ],
    );
    let out = ctx.bin_commit(&ctx.draft, None);
    assert_eq!(out.status.code(), Some(1));
    let raw = String::from_utf8(out.stderr.clone()).unwrap();
    for c in ['\u{1b}', '\u{85}', '\u{202e}', '\u{7f}', '\u{9b}'] {
        assert!(!raw.contains(c), "raw {:04x}", c as u32);
    }
    for esc in ["\\u0085", "\\u202e", "\\u007f", "\\u009b"] {
        assert!(raw.contains(esc), "{esc}");
    }
    let v = stderr_json(&out);
    let hostile = v["diagnostics"]
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["code"] == "flag_block_unknown")
        .unwrap();
    assert!(
        hostile["message"]
            .as_str()
            .unwrap()
            .contains("\u{1b}[2J\u{85}\u{202e}\u{7f}\u{9b}")
    );

    let ctx = Ctx::new();
    let name = "x\u{9b}\u{202e}.txt";
    fs::write(ctx.draft.join(name), "x").unwrap();
    let out = ctx.bin_commit(&ctx.draft, None);
    assert_eq!(out.status.code(), Some(1));
    let raw = String::from_utf8(out.stderr.clone()).unwrap();
    assert!(!raw.contains('\u{9b}') && !raw.contains('\u{202e}'));
    assert!(raw.contains("\\u009b") && raw.contains("\\u202e"));
    let v = stderr_json(&out);
    assert_eq!(v["code"], "check_failed");
    assert_eq!(
        v["message"],
        copy_message(name, "is not a file a draft may hold")
    );
}

/// G9
#[test]
fn copy_rejects_a_symlinked_slide() {
    let ctx = Ctx::new();
    let outside = ctx.home.join("outside.md");
    fs::write(&outside, slide("extra", "step", &[])).unwrap();
    symlink(&outside, ctx.draft.join("slides/extra.md")).unwrap();
    let (message, diags) = check_failed(ctx.err(&ctx.draft, None));
    assert_eq!(message, copy_message("slides/extra.md", "is a symlink"));
    ctx.nothing_left(1, 0, 0);
    assert_eq!(diags, ctx.check_diags(&ctx.draft));
    assert!(codes(&diags).contains(&"symlink_not_allowed".to_owned()));
    ctx.nothing_left(1, 0, 0);
    assert_eq!(
        fs::read(&outside).unwrap(),
        slide("extra", "step", &[]).into_bytes()
    );
}

/// G10
#[test]
fn copy_rejects_an_unexpected_file() {
    let ctx = Ctx::new();
    fs::write(ctx.draft.join("notes.txt"), "n").unwrap();
    let (message, diags) = check_failed(ctx.err(&ctx.draft, None));
    assert_eq!(
        message,
        copy_message("notes.txt", "is not a file a draft may hold")
    );
    assert!(codes(&diags).contains(&"unexpected_file".to_owned()));
    ctx.nothing_left(1, 0, 0);
}

/// G11
#[test]
fn copy_rejects_a_subdirectory_and_a_bad_slide_name() {
    let ctx = Ctx::new();
    fs::create_dir(ctx.draft.join("assets")).unwrap();
    let (message, diags) = check_failed(ctx.err(&ctx.draft, None));
    assert_eq!(message, copy_message("assets", "is not a regular file"));
    assert!(codes(&diags).contains(&"unexpected_file".to_owned()));
    ctx.nothing_left(1, 0, 0);

    let ctx = Ctx::new();
    fs::write(ctx.draft.join("slides/Bad.md"), slide("bad", "step", &[])).unwrap();
    let (message, diags) = check_failed(ctx.err(&ctx.draft, None));
    assert_eq!(
        message,
        copy_message("slides/Bad.md", "is not a file a draft may hold")
    );
    assert!(codes(&diags).contains(&"slide_file_name_invalid".to_owned()));
    ctx.nothing_left(1, 0, 0);

    let ctx = Ctx::new();
    fs::write(ctx.draft.join("zz.txt"), "z").unwrap();
    fs::write(ctx.draft.join("slides/Bad.md"), slide("bad", "step", &[])).unwrap();
    let (message, _) = check_failed(ctx.err(&ctx.draft, None));
    assert_eq!(
        message,
        copy_message("zz.txt", "is not a file a draft may hold")
    );
    ctx.nothing_left(1, 0, 0);
}

/// G12
#[test]
fn copy_rejects_a_symlinked_draft_dir() {
    let ctx = Ctx::new();
    let moved = ctx.draft.with_file_name("moved-away");
    fs::rename(&ctx.draft, &moved).unwrap();
    symlink(&moved, &ctx.draft).unwrap();
    let (message, _) = check_failed(ctx.err(&ctx.draft, None));
    assert_eq!(message, copy_message(".", "is a symlink"));
    ctx.nothing_left(1, 0, 0);
}

// ---------------------------------------------------------------- G13-G19

fn mismatch_a(e: &str, kind: &str) -> String {
    format!(
        "event {e} is a {kind}, not a generate_request; --caused-by takes the generate_request \
         the guide answers"
    )
}

/// G13
#[test]
fn event_command_mismatch_not_a_generate_request() {
    let ctx = Ctx::new();
    ctx.insert_version(1, "ready", None, "[]");
    let (comment, deepdive) = {
        let mut store = ctx.store();
        let now = store.now();
        let ids = store
            .tx(|tx| -> Result<_, StoreError> {
                tx.execute(
                    "INSERT INTO threads (id, created_at, origin_version, slide_id) \
                     VALUES ('t1', 't', 1, 'overview')",
                    [],
                )?;
                tx.execute(
                    "INSERT INTO messages (id, thread_id, author, body, created_at) \
                     VALUES ('m1', 't1', 'reviewer', 'hi', 't')",
                    [],
                )?;
                let c = events::append_comment(tx, now, "t1", "m1")?.id;
                let d = events::append_deepdive_request(tx, now, "t1")?.id;
                Ok((c, d))
            })
            .unwrap();
        ctx.claim(&mut store);
        ids
    };
    let commits_draft = ctx.new_draft(Some(&format!("{0}..{0}", ctx.c2)));
    for (e, kind) in [(&comment, "comment"), (&deepdive, "deepdive_request")] {
        for draft in [&ctx.draft, &commits_draft] {
            let err = ctx.err(draft, Some(e));
            assert_eq!(err.code(), "event_command_mismatch");
            assert_eq!(err.to_string(), mismatch_a(e, kind));
            ctx.nothing_left(2, 1, 0);
            assert_eq!(ctx.event_row(e).0, "claimed");
        }
    }
    let out = ctx.bin_commit(&ctx.draft, Some(&comment));
    assert_eq!(out.status.code(), Some(1));
    assert!(out.stdout.is_empty());
    let v = stderr_json(&out);
    let mut keys: Vec<&str> = v.as_object().unwrap().keys().map(String::as_str).collect();
    keys.sort();
    assert_eq!(keys, ["code", "message"]);
    assert_eq!(v["code"], "event_command_mismatch");
    // A pending deepdive_request is not claimed.
    let pending = {
        let mut store = ctx.store();
        let now = store.now();
        store
            .tx(|tx| events::append_deepdive_request(tx, now, "t1"))
            .unwrap()
            .id
    };
    let err = ctx.err(&ctx.draft, Some(&pending));
    assert_eq!(err.code(), "event_not_claimed");
    assert_eq!(
        err.to_string(),
        format!("event {pending} is not claimed by run {}", ctx.run)
    );
}

/// G14
#[test]
fn event_command_mismatch_range() {
    let ctx = Ctx::new();
    let (c1, c2) = (ctx.c1.clone(), ctx.c2.clone());
    let commits = ctx.new_draft(Some(&format!("{c2}..{c2}")));
    let have_commits = format!("commits {c2}..{c2}");
    #[allow(clippy::type_complexity)]
    let cases: Vec<(Option<&str>, Option<&str>, &PathBuf, String, String)> = vec![
        (
            None,
            None,
            &commits,
            "the full PR".into(),
            have_commits.clone(),
        ),
        (
            Some(&c2),
            Some(&c2),
            &ctx.draft,
            have_commits.clone(),
            "the full PR".into(),
        ),
        (
            Some(&c1),
            Some(&c2),
            &commits,
            format!("commits {c1}..{c2}"),
            have_commits.clone(),
        ),
        (
            Some(&c2),
            Some(&c1),
            &commits,
            format!("commits {c2}..{c1}"),
            have_commits.clone(),
        ),
        (
            Some(&c2),
            None,
            &commits,
            format!("commits {c2}..(none)"),
            have_commits.clone(),
        ),
    ];
    for (first, last, draft, want, have) in cases {
        let e = ctx.event(first, last, true);
        let err = ctx.err(draft, Some(&e));
        assert_eq!(err.code(), "event_command_mismatch");
        assert_eq!(
            err.to_string(),
            format!(
                "event {e} asks for {want}, but the draft's selection is {have}; commit a draft \
                 made by prguide guide new for that range"
            )
        );
        ctx.nothing_left(1, 0, 0);
    }
}

/// G15
#[test]
fn event_command_mismatch_done_with_another_result() {
    let ctx = Ctx::new();
    let e = ctx.event(None, None, true);
    events::ack(&mut ctx.store(), &RunId::new(&ctx.run), &e).unwrap();
    let err = ctx.err(&ctx.draft, Some(&e));
    assert_eq!(err.code(), "event_command_mismatch");
    assert_eq!(
        err.to_string(),
        format!("event {e} is already done with result ack, not a guide version")
    );
    ctx.nothing_left(1, 0, 0);
}

/// G16
#[test]
fn event_not_claimed_pending_or_other_run() {
    let ctx = Ctx::new();
    let pending = ctx.event(None, None, false);
    let err = ctx.err(&ctx.draft, Some(&pending));
    assert_eq!(err.code(), "event_not_claimed");
    assert_eq!(
        err.to_string(),
        format!("event {pending} is not claimed by run {}", ctx.run)
    );
    ctx.nothing_left(1, 0, 0);

    let other = ctx.event(None, None, false);
    ctx.raw()
        .execute(
            "INSERT INTO runs (id, state, created_at, lease_until, ended_at, end_reason) \
             VALUES ('r_stopped0001', 'stopped', 't', 't', 't', 'end')",
            [],
        )
        .unwrap();
    ctx.raw()
        .execute(
            "UPDATE events SET state = 'claimed', claimed_by = 'r_stopped0001' WHERE id = ?1",
            [&other],
        )
        .unwrap();
    let err = ctx.err(&ctx.draft, Some(&other));
    assert_eq!(err.code(), "event_not_claimed");
    assert_eq!(
        err.to_string(),
        format!("event {other} is not claimed by run {}", ctx.run)
    );
    ctx.nothing_left(1, 0, 0);
}

/// G17
#[test]
fn event_unknown_id() {
    let ctx = Ctx::new();
    let err = ctx.err(&ctx.draft, Some("nope"));
    assert_eq!(err.code(), "event_unknown");
    assert_eq!(err.to_string(), "no event nope in this store");
    ctx.nothing_left(1, 0, 0);
}

/// G18
#[test]
fn resolution_errors_match_check() {
    let ctx = Ctx::new();
    let err = ctx.commit(&ctx.draft, "nope", None).err().unwrap();
    assert_eq!(err.code(), "run_unknown");
    assert_eq!(err.to_string(), "no run nope in this store");

    let nowhere = ctx.home.join("not-a-draft");
    fs::create_dir(&nowhere).unwrap();
    let err = ctx.err(&nowhere, None);
    assert_eq!(err.code(), "draft_unknown");
    assert_eq!(
        err.to_string(),
        format!(
            "{nowhere:?} is not a draft of run {}: pass the `draft` path that prguide guide new \
             printed for this run",
            ctx.run
        )
    );

    let other = canon(&ctx.fx.clone("other"));
    let env = GuideCommitEnv {
        git: ctx.fx.git(&other),
        home: ctx.home.clone(),
        clock: ctx.clock_arc(),
    };
    let err = guide_commit(
        &env,
        &GuideCommitArgs {
            draft: ctx.draft.clone(),
            run: ctx.run.clone(),
            caused_by: None,
        },
    )
    .err()
    .unwrap();
    assert_eq!(err.code(), "store_bound");
    assert_eq!(
        err.to_string(),
        format!(
            "the store for octo/widget#7 is bound to the clone whose git dir is {}; this \
             clone's git dir is {}. Run prguide guide commit from the bound clone",
            ctx.clone.join(".git").display(),
            other.join(".git").display()
        )
    );
    ctx.nothing_left(1, 0, 0);

    ctx.clock.advance(LEASE);
    let err = ctx.err(&ctx.draft, None);
    assert_eq!(err.code(), "run_stopped");
    assert_eq!(err.to_string(), format!("run {} has stopped", ctx.run));
    ctx.nothing_left(1, 0, 0);
}

/// G19
#[test]
fn refs_come_from_provenance() {
    let ctx = Ctx::new();
    let base = ctx.out.snapshot.base.clone();
    ctx.fx
        .run(&ctx.out.worktree, ["checkout", "--detach", &base]);
    ctx.fx.run(
        &ctx.clone,
        ["update-ref", "refs/prguide/7/fetch/head", &base],
    );
    let v = ctx.ok(&ctx.draft, None);
    let got: Vec<&str> = v.refs.iter().map(|r| r.sha.as_str()).collect();
    assert_eq!(
        got,
        [
            ctx.out.snapshot.base.as_str(),
            ctx.out.snapshot.head.as_str(),
            ctx.out.selection.base.as_str(),
            ctx.out.selection.head.as_str()
        ]
    );
    let listed = ctx.refs(1);
    for r in &v.refs {
        assert!(listed.contains(&(r.name.clone(), r.sha.clone())));
    }
}

// ---------------------------------------------------------------- G20-G22

/// G20
#[test]
fn binary_concurrent_commits_get_distinct_versions() {
    let ctx = Ctx::new();
    let d2 = ctx.new_draft(None);
    let (p1, r1) = spawn_capped(&mut ctx.bin_commit_cmd(&ctx.clone, &ctx.draft, None));
    let (p2, r2) = spawn_capped(&mut ctx.bin_commit_cmd(&ctx.clone, &d2, None));
    let outs = [wait_capped(p1, r1), wait_capped(p2, r2)];
    let mut versions = Vec::new();
    for o in &outs {
        assert_eq!(
            o.status.code(),
            Some(0),
            "{}",
            String::from_utf8_lossy(&o.stderr)
        );
        let v: serde_json::Value = serde_json::from_slice(&o.stdout).unwrap();
        versions.push(v["version"].as_i64().unwrap());
    }
    versions.sort();
    assert_eq!(versions, [1, 2]);
    assert_eq!(
        ctx.count("SELECT COUNT(*) FROM guide_versions WHERE state = 'ready'"),
        2
    );
    assert_eq!((ctx.refs(1).len(), ctx.refs(2).len()), (4, 4));
    assert_eq!(ctx.notices().len(), 2);
}

/// G21
#[test]
fn binary_concurrent_repeats_make_one_version() {
    let ctx = Ctx::new();
    let e = ctx.event(None, None, true);
    let (p1, r1) = spawn_capped(&mut ctx.bin_commit_cmd(&ctx.clone, &ctx.draft, Some(&e)));
    let (p2, r2) = spawn_capped(&mut ctx.bin_commit_cmd(&ctx.clone, &ctx.draft, Some(&e)));
    let (a, b) = (wait_capped(p1, r1), wait_capped(p2, r2));
    for o in [&a, &b] {
        assert_eq!(
            o.status.code(),
            Some(0),
            "{}",
            String::from_utf8_lossy(&o.stderr)
        );
    }
    assert_eq!(a.stdout, b.stdout);
    assert_eq!((ctx.rows(), ctx.notices().len()), (1, 1));
    assert_eq!(ctx.event_row(&e).0, "done");
}

/// G22
#[test]
fn binary_commit_waits_for_the_lock() {
    let ctx = Ctx::new();
    let mut lock = versions::open_lock(&ctx.sdir()).unwrap();
    let guard = lock.write().unwrap();
    let (pid, rx) = spawn_capped(&mut ctx.bin_commit_cmd(&ctx.clone, &ctx.draft, None));
    std::thread::sleep(Duration::from_millis(3000));
    assert!(rx.try_recv().is_err());
    assert_eq!(ctx.rows(), 0);
    let conn = ctx.raw();
    conn.busy_timeout(Duration::from_secs(1)).unwrap();
    conn.execute_batch(
        "BEGIN IMMEDIATE; UPDATE store_meta SET value = value WHERE key = 'host'; COMMIT;",
    )
    .unwrap();
    drop(guard);
    let out = wait_capped(pid, rx);
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["version"], 1);
}

// ---------------------------------------------------------------- G23-G27

/// G23
#[test]
fn binary_open_leaves_staging_alone_while_the_lock_is_held() {
    let ctx = Ctx::new();
    ctx.crashed_after_pin(1, None);
    let mut lock = versions::open_lock(&ctx.sdir()).unwrap();
    let guard = lock.write().unwrap();
    let out = ctx.bin_status();
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(ctx.rows(), 1);
    assert!(ctx.sdir().join("staging/v1/guide.yaml").exists());
    assert_eq!(ctx.refs(1).len(), 4);
    drop(guard);
    let out = ctx.bin_status();
    assert_eq!(out.status.code(), Some(0));
    ctx.nothing_left(1, 0, 0);
}

/// G24
#[test]
fn binary_open_finishes_a_published_version() {
    let ctx = Ctx::new();
    let e = ctx.event(None, None, true);
    ctx.insert_version(1, "staging", Some(&e), &ctx.refs_json(1));
    copy_tree(&ctx.draft, &ctx.sdir().join("guides/v1"));
    let pairs = ctx.pair_list(1);
    ctx.git().write_refs(&pairs[..2]).unwrap();
    ctx.crashed_after_pin_v2();
    let out = ctx.bin_status();
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        ctx.raw()
            .query_row(
                "SELECT state FROM guide_versions WHERE version = 1",
                [],
                |r| r.get::<_, String>(0)
            )
            .unwrap(),
        "ready"
    );
    assert!(ctx.git().verify_refs(&pairs).unwrap().is_empty());
    let (state, _, kind, id, _) = ctx.event_row(&e);
    assert_eq!(
        (state.as_str(), kind.as_deref(), id.as_deref()),
        ("done", Some("guide_version"), Some("1"))
    );
    assert_eq!(ctx.notices(), vec![("guide_ready".into(), "1".into())]);
    assert_eq!(
        ctx.count("SELECT COUNT(*) FROM guide_versions WHERE version = 2"),
        0
    );
    assert!(!exists(&ctx.sdir().join("staging/v2")));
}

impl Ctx {
    /// A staging row v2 with no refs and no guides dir, but a staging dir.
    fn crashed_after_pin_v2(&self) {
        self.insert_version(2, "staging", None, &self.refs_json(2));
        copy_tree(&self.draft, &self.sdir().join("staging/v2"));
    }
}

/// G25
#[test]
fn binary_open_survives_a_failed_reconcile() {
    let ctx = Ctx::new();
    ctx.insert_version(1, "staging", None, "not json");
    let out = ctx.bin_status();
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert!(v.is_object());
    assert_eq!(
        ctx.count("SELECT COUNT(*) FROM guide_versions WHERE state = 'staging'"),
        1
    );
}

/// G26
#[test]
fn open_takes_no_write_lock_without_staging_rows() {
    let ctx = Ctx::new();
    ctx.insert_version(1, "ready", None, "[]");
    let holder = ctx.raw();
    holder.execute_batch("BEGIN IMMEDIATE").unwrap();
    let started = Instant::now();
    let store = Store::open_in(&ctx.home, &PrRef::new("octo", "widget", 7).unwrap());
    assert!(store.is_ok());
    assert!(started.elapsed() < Duration::from_secs(4));
    holder.execute_batch("ROLLBACK").unwrap();
}

/// G27
#[test]
fn binary_kill_during_pin_rolls_back_on_the_next_command() {
    let ctx = Ctx::new();
    let tmp = TempDir::new().unwrap();
    let real = String::from_utf8(
        Command::new("sh")
            .args(["-c", "command -v git"])
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap()
    .trim()
    .to_owned();
    let bin = tmp.path().join("bin");
    fs::create_dir(&bin).unwrap();
    let wrapper = bin.join("git");
    fs::write(
        &wrapper,
        format!(
            "#!/bin/sh\ncase \" $* \" in\n  *\" update-ref \"*)\n    if [ -e \"$MARK\" ]; then\n      \
             touch \"$REACHED\"\n      exec sleep 60\n    fi;;\nesac\nexec {real} \"$@\"\n"
        ),
    )
    .unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o755)).unwrap();
    }
    let (mark, reached) = (tmp.path().join("mark"), tmp.path().join("reached"));
    fs::write(&mark, "").unwrap();
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap());
    let mut cmd = ctx.bin_commit_cmd(&ctx.clone, &ctx.draft, None);
    cmd.env("PATH", &path)
        .env("MARK", &mark)
        .env("REACHED", &reached)
        .process_group(0)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut child = cmd.spawn().unwrap();
    let pgid = child.id();
    let started = Instant::now();
    while !reached.exists() {
        assert!(
            started.elapsed() < CAP,
            "the commit never reached update-ref"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    Command::new("kill")
        .args(["-KILL", "--", &format!("-{pgid}")])
        .status()
        .unwrap();
    child.wait().unwrap();
    assert_eq!(
        ctx.count("SELECT COUNT(*) FROM guide_versions WHERE version = 1 AND state = 'staging'"),
        1
    );
    assert!(ctx.sdir().join("staging/v1").exists());
    assert!(ctx.refs(1).is_empty());
    assert!(ctx.lock_free());
    fs::remove_file(&mark).unwrap();
    let out = ctx.bin_status();
    assert_eq!(out.status.code(), Some(0));
    ctx.nothing_left(1, 0, 0);
    let again = ctx.bin_commit(&ctx.draft, None);
    assert_eq!(
        again.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&again.stderr)
    );
    let v: serde_json::Value = serde_json::from_slice(&again.stdout).unwrap();
    assert_eq!(
        (v["version"].as_i64(), v["state"].as_str()),
        (Some(1), Some("ready"))
    );
}

// ---------------------------------------------------------------- G28, G29

/// G28
#[test]
fn versions_api_lists_ready_versions() {
    let ctx = Ctx::new();
    let mut store = ctx.store();
    let dir = store.dir().to_path_buf();
    let refs = r#"[{"name":"refs/prguide/7/v1/snapshot-base","sha":"abc"}]"#;
    store
        .tx(|tx| -> Result<(), StoreError> {
            for (k, state) in [(3, "ready"), (2, "staging"), (1, "ready")] {
                tx.execute(
                    "INSERT INTO guide_versions (version, state, caused_by, snapshot_base, \
                     snapshot_head, selection_kind, selection_base, selection_head, \
                     commits_json, refs_json, created_at, ready_at) VALUES (?1, ?2, NULL, 'sb', \
                     'sh', 'commits', 'xb', 'xh', '[\"c1\",\"c2\"]', ?3, 't', 'r')",
                    rusqlite::params![k, state, refs],
                )?;
            }
            Ok(())
        })
        .unwrap();
    let ready = store.tx(|tx| versions::ready(tx, &dir)).unwrap();
    assert_eq!(ready.iter().map(|v| v.version).collect::<Vec<_>>(), [1, 3]);
    for v in &ready {
        assert_eq!(v.dir, dir.join(format!("guides/v{}", v.version)));
        assert_eq!(v.refs.len(), 1);
        assert_eq!(v.refs[0].sha, "abc");
        assert_eq!(v.selection.commits, ["c1", "c2"]);
    }
    let two = store.tx(|tx| versions::get(tx, &dir, 2)).unwrap().unwrap();
    assert_eq!(two.state, "staging");
    assert!(store.tx(|tx| versions::get(tx, &dir, 9)).unwrap().is_none());
    store
        .tx(|tx| {
            tx.execute(
                "INSERT INTO guide_versions (version, state, caused_by, snapshot_base, \
                 snapshot_head, selection_kind, selection_base, selection_head, commits_json, \
                 refs_json, created_at, ready_at) VALUES (4, 'ready', NULL, 'a', 'b', 'full', \
                 'c', 'd', '[]', 'x', 't', 'r')",
                [],
            )?;
            Ok::<_, StoreError>(())
        })
        .unwrap();
    let err = store.tx(|tx| versions::ready(tx, &dir)).unwrap_err();
    assert_eq!(err.code(), "store_sqlite");

    // Strict parsing: an unknown ref field and malformed commits both fail.
    let bad = [
        (r#"[{"name":"n","sha":"s","extra":true}]"#, "[]"),
        ("[]", "x"),
        ("[]", "[1]"),
    ];
    for (refs_json, commits_json) in bad {
        let mut ctx = Ctx::new();
        let mut store = ctx.store();
        let dir = store.dir().to_path_buf();
        store
            .tx(|tx| {
                tx.execute(
                    "INSERT INTO guide_versions (version, state, caused_by, snapshot_base, \
                     snapshot_head, selection_kind, selection_base, selection_head, \
                     commits_json, refs_json, created_at, ready_at) VALUES (1, 'ready', NULL, \
                     'a', 'b', 'full', 'c', 'd', ?1, ?2, 't', 'r')",
                    rusqlite::params![commits_json, refs_json],
                )?;
                Ok::<_, StoreError>(())
            })
            .unwrap();
        let err = store.tx(|tx| versions::ready(tx, &dir)).unwrap_err();
        assert_eq!(err.code(), "store_sqlite", "{refs_json} {commits_json}");
        let err = store.tx(|tx| versions::get(tx, &dir, 1)).unwrap_err();
        assert_eq!(err.code(), "store_sqlite", "{refs_json} {commits_json}");
        let _ = &mut ctx;
    }
}

fn relative(from: &Path, to: &Path) -> PathBuf {
    let from: Vec<_> = from.components().collect();
    let to: Vec<_> = to.components().collect();
    let common = from.iter().zip(&to).take_while(|(a, b)| a == b).count();
    let mut out = PathBuf::new();
    for _ in common..from.len() {
        out.push("..");
    }
    for c in &to[common..] {
        out.push(c);
    }
    out
}

/// G29
#[test]
fn draft_argument_relative_to_the_clone() {
    let ctx = Ctx::new();
    let rel = relative(&ctx.clone, &ctx.draft);
    assert!(rel.is_relative() && rel.starts_with(".."));
    let v = ctx.ok(&rel, None);
    assert_eq!(v.version, 1);
    assert_eq!(tree(&v.dir), tree(&ctx.draft));
}

#[test]
fn reconcile_rollback_touches_only_its_own_refs() {
    let ctx = Ctx::new();
    ctx.ok(&ctx.draft, None);
    let sha = ctx.out.snapshot.head.clone();
    let other = "refs/prguide/8/fetch/head".to_owned();
    ctx.git()
        .write_refs(&[(other.clone(), sha.clone())])
        .unwrap();
    let names = ["refs/prguide/7/v1/snapshot-head", other.as_str()];
    let json: Vec<_> = names
        .iter()
        .map(|n| serde_json::json!({"name": n, "sha": sha}))
        .collect();
    ctx.insert_version(2, "staging", None, &serde_json::to_string(&json).unwrap());
    assert_eq!(ctx.bin_status().status.code(), Some(0));
    assert_eq!(ctx.refs(1).len(), 4);
    assert_eq!(ctx.git().list_refs("refs/prguide/8").unwrap().len(), 1);
    assert_eq!(
        ctx.count("SELECT COUNT(*) FROM guide_versions WHERE version = 2 AND state = 'staging'"),
        1
    );
}

#[test]
fn reconcile_finish_writes_only_its_own_refs() {
    let ctx = Ctx::new();
    ctx.ok(&ctx.draft, None);
    let before = ctx.refs(1);
    fs::create_dir_all(ctx.sdir().join("guides/v2")).unwrap();
    let json = serde_json::json!([{"name": "refs/prguide/7/v1/selection-head", "sha": ctx.out.snapshot.base}]);
    ctx.insert_version(2, "staging", None, &json.to_string());
    assert_eq!(ctx.bin_status().status.code(), Some(0));
    assert_eq!(ctx.refs(1), before);
    assert_eq!(
        ctx.count("SELECT COUNT(*) FROM guide_versions WHERE version = 2 AND state = 'staging'"),
        1
    );
}

#[test]
fn a_symlinked_commit_lock_creates_nothing_outside() {
    let ctx = Ctx::new();
    let outside = TempDir::new().unwrap();
    let target = outside.path().join("created");
    symlink(&target, ctx.sdir().join("commit.lock")).unwrap();
    let err = ctx.err(&ctx.draft, None);
    assert_eq!(err.code(), "store_io");
    assert!(!exists(&target));
}
