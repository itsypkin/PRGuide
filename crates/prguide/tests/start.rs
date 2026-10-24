mod support;

use std::{
    collections::BTreeMap,
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::Command,
    sync::Arc,
    time::{Duration, UNIX_EPOCH},
};

use prguide::{
    Error,
    cli::start::{StartArgs, StartEnv, StartOutput, pr_repo, start},
    git::Git,
    github::{
        Commit, CommitAuthor, GitHubError, Pr, PrState, Repo,
        fake::{FakeGitHub, FakeMethod},
    },
    store::{
        Clock, FakeClock, PrRef, Store, StoreError, Timestamp,
        runs::{self, StopReason},
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

fn git_version() -> (u32, u32) {
    let out = Command::new("git").arg("--version").output().unwrap();
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    let mut parts = text.trim().split(' ').nth(2).unwrap().split('.');
    (
        parts.next().unwrap().parse().unwrap(),
        parts.next().unwrap().parse().unwrap(),
    )
}

fn at_least(major: u32, minor: u32) -> bool {
    git_version() >= (major, minor)
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

fn marker_script(dir: &Path, name: &str, marker: &Path, tail: &str) -> PathBuf {
    fs::create_dir_all(dir).unwrap();
    let path = dir.join(name);
    fs::write(
        &path,
        format!(
            "#!/bin/sh\necho ran >> '{}'\n{tail}\nexit 0\n",
            marker.display()
        ),
    )
    .unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    path
}

fn gconfig(fx: &FixtureRepo, key: &str, value: &str) {
    fx.run(fx.path(), ["config", "--global", key, value]);
}

fn check_run_id(id: &str) {
    assert!(
        id.len() == 14
            && id.starts_with("r_")
            && id[2..]
                .bytes()
                .all(|b| b.is_ascii_digit() || b.is_ascii_lowercase()),
        "bad run id {id}"
    );
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

struct Ctx {
    fx: FixtureRepo,
    home: PathBuf,
    _home_guard: Option<TempDir>,
    clone: PathBuf,
    fake: FakeGitHub,
    clock: Arc<FakeClock>,
    repo: Repo,
    head: String,
}

impl Ctx {
    fn new() -> Self {
        Self::from_shape(s1_shape)
    }

    fn from_shape(shape: impl FnOnce(&FixtureRepo) -> String) -> Self {
        let fx = FixtureRepo::new();
        let head = shape(&fx);
        fx.push_pr(7, &head);
        let clone = fx.clone("rev");
        Self::assemble(fx, head, clone, None)
    }

    fn assemble(fx: FixtureRepo, head: String, clone: PathBuf, home: Option<PathBuf>) -> Self {
        let guard = home.is_none().then(|| TempDir::new().unwrap());
        let home = match (&home, &guard) {
            (Some(h), _) => h.clone(),
            (None, Some(g)) => g.path().to_path_buf(),
            _ => unreachable!(),
        };
        fs::create_dir_all(&home).unwrap();
        let ctx = Self {
            home: canon(&home),
            _home_guard: guard,
            clone: canon(&clone),
            fake: FakeGitHub::new(),
            clock: Arc::new(FakeClock::new(Timestamp::from_unix_millis(
                1_767_225_600_000,
            ))),
            repo: Repo::github("octo", "widget").unwrap(),
            head: head.clone(),
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
            head_repo: Some("octo/widget".into()),
            base_ref: "main".into(),
            base_sha: self.fx.run(self.fx.work_path(), ["rev-parse", "main"]),
            title: "Add things".into(),
            body: "A body.".into(),
            author: "octocat".into(),
            url: "https://github.com/octo/widget/pull/7".into(),
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

    /// Commit a file on the PR branch, push it as the PR head and tell the fake.
    fn advance(&mut self, path: &str, contents: &str) -> String {
        self.fx.write(path, contents);
        let head = self.fx.commit(&format!("advance {path}"));
        self.fx.push_pr(7, &head);
        self.publish(&head);
        self.head = head.clone();
        head
    }

    fn start_with(&self, git: Git, takeover: bool) -> Result<StartOutput, Error> {
        let clock: Arc<dyn Clock> = self.clock.clone();
        let env = StartEnv {
            git,
            github: &self.fake,
            home: self.home.clone(),
            clock,
        };
        start(&env, &StartArgs { pr: 7, takeover })
    }

    fn start(&self, takeover: bool) -> Result<StartOutput, Error> {
        self.start_with(self.fx.git(&self.clone), takeover)
    }

    fn ok(&self) -> StartOutput {
        let out = self.start(false).unwrap();
        check_run_id(out.run_id.as_str());
        out
    }

    fn store(&self) -> Store {
        let clock: Arc<dyn Clock> = self.clock.clone();
        Store::open_in(&self.home, &PrRef::new("octo", "widget", 7).unwrap())
            .unwrap()
            .with_clock(clock)
    }

    fn stop(&self, out: &StartOutput) {
        runs::stop_run(&mut self.store(), &out.run_id, StopReason::End).unwrap();
    }

    fn query(&self, sql: &str) -> Vec<Vec<String>> {
        self.store()
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

    fn exec(&self, sql: &str) {
        self.store()
            .tx(|tx| -> Result<_, StoreError> {
                tx.execute_batch(sql)?;
                Ok(())
            })
            .unwrap();
    }

    fn meta(&self) -> Vec<Vec<String>> {
        self.query("SELECT key, value FROM store_meta ORDER BY key")
    }

    fn runs(&self) -> Vec<Vec<String>> {
        self.query(
            "SELECT id, state, created_at, lease_until, ended_at, end_reason FROM runs ORDER BY id",
        )
    }

    fn wt(&self) -> PathBuf {
        self.home.join("prs/github.com/octo/widget/7/worktree")
    }

    fn snapshot_json(&self, out: &StartOutput) -> String {
        let text = serde_json::to_string_pretty(out).unwrap();
        text.replace(out.run_id.as_str(), "[RUN]")
            .replace(self.home.to_str().unwrap(), "[HOME]")
            .replace(canon(self.fx.path()).to_str().unwrap(), "[ROOT]")
    }
}

fn gitdir_of(wt: &Path) -> PathBuf {
    let text = fs::read_to_string(wt.join(".git")).unwrap();
    PathBuf::from(text.trim().strip_prefix("gitdir: ").unwrap())
}

fn worktree_count(fx: &FixtureRepo, clone: &Path, path: &Path) -> usize {
    let list = fx.run(clone, ["worktree", "list", "--porcelain"]);
    let needle = format!("worktree {}", path.display());
    list.lines().filter(|l| *l == needle).count()
}

fn dirty(fx: &FixtureRepo, dir: &Path) {
    fs::write(dir.join("README.md"), "changed\n").unwrap();
    fs::write(dir.join("staged.txt"), "staged\n").unwrap();
    fx.run(dir, ["add", "staged.txt"]);
    fs::write(dir.join("untracked.txt"), "untracked\n").unwrap();
    fs::write(dir.join("ignored.txt"), "ignored\n").unwrap();
}

// ---------------------------------------------------------------- tests

#[test]
fn first_start_output_snapshot() {
    let ctx = Ctx::new();
    let out = ctx.ok();
    insta::assert_snapshot!("first_start", ctx.snapshot_json(&out));
    assert_eq!(out.store, ctx.home.join("prs/github.com/octo/widget/7"));
    assert_exact(&ctx.fx, &out.worktree, &ctx.head);
    let common = canon(&ctx.clone.join(".git"));
    let meta = ctx.meta();
    let expected: Vec<Vec<String>> = [
        ("git_common_dir", common.to_string_lossy().into_owned()),
        ("host", "github.com".into()),
        ("number", "7".into()),
        ("owner", "octo".into()),
        ("repo", "widget".into()),
    ]
    .into_iter()
    .map(|(k, v)| vec![k.to_owned(), v])
    .collect();
    assert_eq!(meta, expected);
    let runs = ctx.runs();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0][0], out.run_id.as_str());
    assert_eq!(runs[0][1], "active");
}

#[test]
fn empty_pr_output_snapshot() {
    let ctx = Ctx::from_shape(|fx| {
        on_pr(fx);
        fx.run(fx.work_path(), ["commit", "--allow-empty", "-m", "empty"]);
        fx.head()
    });
    let out = ctx.ok();
    assert!(out.files.is_empty());
    assert!(out.guide_versions.is_empty());
    insta::assert_snapshot!("empty_pr", ctx.snapshot_json(&out));
}

#[test]
fn resume_lists_only_ready_guide_versions() {
    let ctx = Ctx::new();
    let out = ctx.ok();
    ctx.stop(&out);
    ctx.exec(
        "INSERT INTO guide_versions(version,state,caused_by,snapshot_base,snapshot_head,selection_kind,selection_base,selection_head,commits_json,refs_json,created_at,ready_at) \
         VALUES (1,'ready',NULL,'sb1','sh1','full','xb1','xh1','[]','[]','2026-01-01T00:00:00.000Z','2026-01-01T00:01:00.000Z'), \
                (2,'staging',NULL,'sb2','sh2','commits','xb2','xh2','[]','[]','2026-01-02T00:00:00.000Z',NULL);",
    );
    let out = ctx.ok();
    assert_eq!(out.guide_versions.len(), 1);
    insta::assert_snapshot!("resume_with_ready_versions", ctx.snapshot_json(&out));
}

#[test]
fn resume_moves_existing_worktree_to_the_new_head() {
    let mut ctx = Ctx::new();
    let first = ctx.ok();
    ctx.stop(&first);
    let git_file = first.worktree.join(".git");
    let before_bytes = fs::read(&git_file).unwrap();
    let old = UNIX_EPOCH + Duration::from_secs(1_000_000);
    fs::File::options()
        .write(true)
        .open(&git_file)
        .unwrap()
        .set_modified(old)
        .unwrap();
    let meta = ctx.meta();
    let head = ctx.advance("newer.txt", "newer\n");
    let second = ctx.ok();
    assert_ne!(first.run_id, second.run_id);
    assert_eq!(first.store, second.store);
    assert_eq!(first.worktree, second.worktree);
    assert_exact(&ctx.fx, &second.worktree, &head);
    assert_eq!(fs::read(&git_file).unwrap(), before_bytes);
    assert_eq!(fs::metadata(&git_file).unwrap().modified().unwrap(), old);
    assert_eq!(worktree_count(&ctx.fx, &ctx.clone, &second.worktree), 1);
    assert_eq!(ctx.meta(), meta);
}

#[test]
fn second_clone_fails_with_store_bound() {
    let ctx = Ctx::new();
    ctx.ok();
    let other = canon(&ctx.fx.clone("other"));
    let meta = ctx.meta();
    let runs = ctx.runs();
    let err = ctx.start_with(ctx.fx.git(&other), true).unwrap_err();
    assert_eq!(err.code(), "store_bound");
    let msg = err.to_string();
    assert!(
        msg.contains(canon(&ctx.clone.join(".git")).to_str().unwrap()),
        "{msg}"
    );
    assert!(
        msg.contains(canon(&other.join(".git")).to_str().unwrap()),
        "{msg}"
    );
    assert!(
        msg.contains("Run prguide start from the bound clone"),
        "{msg}"
    );
    assert_eq!(ctx.fx.run(&other, ["for-each-ref", "refs/prguide"]), "");
    let list = ctx.fx.run(&other, ["worktree", "list", "--porcelain"]);
    assert_eq!(
        list.lines().filter(|l| l.starts_with("worktree ")).count(),
        1
    );
    assert_eq!(ctx.meta(), meta);
    assert_eq!(ctx.runs(), runs);
}

#[test]
fn linked_worktree_subdirectory_and_symlink_bind_the_same_clone() {
    let ctx = Ctx::new();
    ctx.ok();
    let meta = ctx.meta();
    let sub = ctx.clone.join("subdir");
    fs::create_dir(&sub).unwrap();
    ctx.start_with(ctx.fx.git(&sub), true).unwrap();
    let linked = ctx.fx.path().join("linked");
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
    ctx.start_with(ctx.fx.git(&linked), true).unwrap();
    let link = ctx.fx.path().join("link");
    std::os::unix::fs::symlink(&ctx.clone, &link).unwrap();
    ctx.start_with(ctx.fx.git(&link), true).unwrap();
    assert_eq!(ctx.meta(), meta);
}

#[test]
fn dirty_worktree_is_reset() {
    let mut ctx = Ctx::from_shape(|fx| {
        simple_shape(
            fx,
            &[
                (".gitignore", "target/\n*.log\n"),
                ("a.txt", "a\n"),
                ("b.txt", "b\n"),
                ("run.sh", "echo\n"),
            ],
        )
    });
    let first = ctx.ok();
    let wt = first.worktree.clone();
    fs::write(wt.join("a.txt"), "modified\n").unwrap();
    fs::remove_file(wt.join("b.txt")).unwrap();
    fs::set_permissions(wt.join("run.sh"), fs::Permissions::from_mode(0o755)).unwrap();
    fs::write(wt.join("staged.txt"), "s\n").unwrap();
    ctx.fx.run(&wt, ["add", "staged.txt"]);
    fs::write(wt.join("untracked.txt"), "u\n").unwrap();
    fs::create_dir_all(wt.join("udir")).unwrap();
    fs::write(wt.join("udir/f"), "u\n").unwrap();
    fs::create_dir_all(wt.join("target")).unwrap();
    fs::write(wt.join("target/out.o"), "o\n").unwrap();
    fs::write(wt.join("build.log"), "l\n").unwrap();
    fs::create_dir_all(wt.join("ign")).unwrap();
    fs::write(wt.join("ign/x.log"), "l\n").unwrap();
    fs::create_dir_all(wt.join("nested")).unwrap();
    ctx.fx.run(&wt.join("nested"), ["init"]);
    fs::write(wt.join("later.txt"), "untracked in the way\n").unwrap();
    let merge_head = gitdir_of(&wt).join("MERGE_HEAD");
    fs::write(&merge_head, format!("{}\n", ctx.head)).unwrap();
    ctx.fx.run(&ctx.clone, ["config", "core.fileMode", "false"]);
    ctx.stop(&first);
    let head = ctx.advance("later.txt", "tracked\n");
    let second = ctx.ok();
    assert_exact(&ctx.fx, &second.worktree, &head);
    assert!(!merge_head.exists());
    let mode = fs::metadata(wt.join("run.sh"))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o111, 0, "mode {mode:o}");
}

#[test]
fn reviewer_state_untouched_in_plain_clone() {
    let ctx = Ctx::new();
    ctx.fx.run(
        &ctx.clone,
        ["config", "--local", "core.excludesFile", "/dev/null"],
    );
    fs::write(ctx.clone.join(".git/info/exclude"), "ignored.txt\n").unwrap();
    dirty(&ctx.fx, &ctx.clone);
    let before = state(&ctx.fx, &ctx.clone, &[]);
    ctx.ok();
    assert_eq!(state(&ctx.fx, &ctx.clone, &[]), before);
    let refs = ctx.fx.run(
        &ctx.clone,
        ["for-each-ref", "--format=%(refname)", "refs/prguide"],
    );
    assert!(!refs.is_empty());
    assert!(refs.lines().all(|l| l.starts_with("refs/prguide/7/")));
}

#[test]
fn reviewer_state_untouched_from_linked_worktree() {
    let ctx = Ctx::new();
    fs::write(ctx.clone.join(".git/info/exclude"), "ignored.txt\n").unwrap();
    let linked = ctx.fx.path().join("linked");
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
    dirty(&ctx.fx, &ctx.clone);
    let before = (
        state(&ctx.fx, &linked, &[]),
        state(&ctx.fx, &ctx.clone, std::slice::from_ref(&linked)),
    );
    ctx.start_with(ctx.fx.git(&linked), false).unwrap();
    let after = (
        state(&ctx.fx, &linked, &[]),
        state(&ctx.fx, &ctx.clone, std::slice::from_ref(&linked)),
    );
    assert_eq!(before, after);
}

#[test]
fn reviewer_state_untouched_in_blobless_clone() {
    let fx = FixtureRepo::new();
    let head = s1_shape(&fx);
    fx.push_pr(7, &head);
    for (k, v) in [
        ("uploadpack.allowFilter", "true"),
        ("uploadpack.allowAnySHA1InWant", "true"),
    ] {
        fx.run(fx.remote_path(), ["config", k, v]);
    }
    let clone = fx.path().join("blobless");
    let remote_url = format!("file://{}", fx.remote_path().display());
    fx.run(
        fx.path(),
        [
            "clone",
            "--filter=blob:none",
            remote_url.as_str(),
            clone.to_str().unwrap(),
        ],
    );
    let gh = "https://github.com/octo/widget.git";
    fx.run(&clone, ["config", "remote.origin.url", gh]);
    fx.run(
        &clone,
        ["config", &format!("url.{remote_url}.insteadOf"), gh],
    );
    let ctx = Ctx::assemble(fx, head.clone(), clone, None);
    dirty(&ctx.fx, &ctx.clone);
    let before = state(&ctx.fx, &ctx.clone, &[]);
    let out = ctx.ok();
    assert_exact(&ctx.fx, &out.worktree, &head);
    assert_eq!(state(&ctx.fx, &ctx.clone, &[]), before);
}

#[test]
fn run_active_without_takeover_leaves_worktree_untouched() {
    let mut ctx = Ctx::new();
    let first = ctx.ok();
    fs::write(first.worktree.join("marker.txt"), "keep\n").unwrap();
    let tree_before = tree_map(&first.worktree, &[]);
    ctx.advance("newer.txt", "n\n");
    let runs = ctx.runs();
    let err = ctx.start(false).unwrap_err();
    assert_eq!(err.code(), "run_active");
    let msg = err.to_string();
    assert!(
        msg.contains(first.run_id.as_str()) && msg.contains("take it over"),
        "{msg}"
    );
    assert!(first.worktree.join("marker.txt").exists());
    assert_eq!(tree_map(&first.worktree, &[]), tree_before);
    assert_eq!(
        tgit(&ctx.fx, &first.worktree, &["rev-parse", "HEAD"]),
        first.pr.head_sha
    );
    assert_eq!(
        ctx.fx
            .run(&ctx.clone, ["rev-parse", "refs/prguide/7/fetch/head"]),
        first.pr.head_sha
    );
    assert_eq!(ctx.runs(), runs);
}

#[test]
fn takeover_stops_the_live_run_and_starts_a_new_one() {
    let ctx = Ctx::new();
    let first = ctx.ok();
    let second = ctx.start(true).unwrap();
    assert_ne!(first.run_id, second.run_id);
    let old = ctx.query(&format!(
        "SELECT state, end_reason FROM runs WHERE id = '{}'",
        first.run_id.as_str()
    ));
    assert_eq!(old, vec![vec!["stopped", "takeover"]]);
    assert_eq!(
        ctx.query("SELECT id FROM runs WHERE state = 'active'")
            .len(),
        1
    );
}

#[test]
fn expired_run_does_not_block_start() {
    let ctx = Ctx::new();
    let first = ctx.ok();
    ctx.clock.advance(runs::LEASE + Duration::from_secs(1));
    ctx.start(false).unwrap();
    let old = ctx.query(&format!(
        "SELECT state FROM runs WHERE id = '{}'",
        first.run_id.as_str()
    ));
    assert_eq!(old, vec![vec!["expired"]]);
}

#[test]
fn registered_but_missing_worktree_is_recreated() {
    let ctx = Ctx::new();
    let first = ctx.ok();
    ctx.stop(&first);
    fs::remove_dir_all(&first.worktree).unwrap();
    let out = ctx.ok();
    assert_exact(&ctx.fx, &out.worktree, &ctx.head);
    assert_eq!(worktree_count(&ctx.fx, &ctx.clone, &out.worktree), 1);
}

#[test]
fn worktree_with_pruned_metadata_is_rebuilt() {
    let ctx = Ctx::new();
    let first = ctx.ok();
    ctx.stop(&first);
    fs::remove_dir_all(gitdir_of(&first.worktree)).unwrap();
    fs::write(first.worktree.join("stray.txt"), "stray\n").unwrap();
    let out = ctx.ok();
    assert_exact(&ctx.fx, &out.worktree, &ctx.head);
}

#[test]
fn worktree_without_git_file_is_rebuilt() {
    let ctx = Ctx::new();
    let first = ctx.ok();
    ctx.stop(&first);
    fs::remove_file(first.worktree.join(".git")).unwrap();
    fs::write(first.worktree.join("stray.txt"), "stray\n").unwrap();
    let out = ctx.ok();
    assert_exact(&ctx.fx, &out.worktree, &ctx.head);
}

fn dotfiles(fx: &FixtureRepo) -> (PathBuf, String) {
    let dir = fx.path().join("dotfiles");
    fs::create_dir_all(&dir).unwrap();
    fx.run(&dir, ["init", "-b", "main"]);
    fs::write(dir.join("tracked.txt"), "one\n").unwrap();
    fx.run(&dir, ["add", "-A"]);
    fx.run(&dir, ["commit", "-m", "dot"]);
    fs::write(dir.join("tracked.txt"), "two\n").unwrap();
    fs::write(dir.join("untracked.txt"), "u\n").unwrap();
    let sha = fx.run(&dir, ["rev-parse", "HEAD"]);
    (canon(&dir), sha)
}

#[test]
fn enclosing_repository_is_never_touched() {
    let fx = FixtureRepo::new();
    let head = s1_shape(&fx);
    fx.push_pr(7, &head);
    let clone = fx.clone("rev");
    let (dot, _) = dotfiles(&fx);
    fs::write(dot.join(".git/info/exclude"), "/prguide-home/\n").unwrap();
    let ctx = Ctx::assemble(fx, head, clone, Some(dot.join("prguide-home")));
    let first = ctx.ok();
    ctx.stop(&first);
    fs::remove_file(first.worktree.join(".git")).unwrap();
    let skip = [dot.join("prguide-home")];
    let before = state(&ctx.fx, &dot, &skip);
    let out = ctx.ok();
    assert_exact(&ctx.fx, &out.worktree, &ctx.head);
    assert_eq!(state(&ctx.fx, &dot, &skip), before);
}

#[test]
fn reset_worktree_never_reaches_an_enclosing_repository() {
    let fx = FixtureRepo::new();
    let (dot, sha) = dotfiles(&fx);
    let dir = dot.join("sub/wt");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("file"), "x\n").unwrap();
    let dir = canon(&dir);
    let before = state(&fx, &dot, &[]);
    let err = fx.git(&dot).reset_worktree(&dir, &sha).unwrap_err();
    assert_eq!(err.code(), "git_failed");
    assert!(err.to_string().contains("checkout"), "{err}");
    assert_eq!(state(&fx, &dot, &[]), before);
    assert_eq!(
        tgit(&fx, &dot, &["symbolic-ref", "HEAD"]),
        "refs/heads/main"
    );
    assert_eq!(
        fs::read_to_string(dot.join("tracked.txt")).unwrap(),
        "two\n"
    );
}

const ATTRS: &str = "*.flt filter=mine\n*.lfs filter=lfs -text\n*.crlf text eol=crlf\n*.enc text working-tree-encoding=UTF-16\n*.id ident\n";

#[test]
fn worktree_holds_blob_bytes_despite_attributes_and_filters() {
    let ctx = Ctx::from_shape(|fx| {
        on_pr(fx);
        for f in [
            "x.flt",
            "x.lfs",
            "x.crlf",
            "x.enc",
            "x.ga",
            "x.ia",
            "plain.txt",
        ] {
            fx.write(f, "a\nb\n");
        }
        fx.write("x.id", "$Id$\n");
        fx.commit("files");
        fx.write(".gitattributes", ATTRS);
        // Not `commit()`: its `add -A` would re-encode x.enc under the new attributes.
        fx.run(fx.work_path(), ["add", ".gitattributes"]);
        fx.run(fx.work_path(), ["commit", "-m", "attrs"]);
        fx.head()
    });
    let fx = &ctx.fx;
    let tools = fx.path().join("tools");
    let marker = fx.path().join("filter-marker");
    let smudge = marker_script(&tools, "smudge", &marker, "cat >/dev/null; echo SMUDGED");
    let failing = marker_script(&tools, "fail", &marker, "exit 1");
    gconfig(fx, "filter.mine.smudge", smudge.to_str().unwrap());
    gconfig(fx, "filter.mine.clean", smudge.to_str().unwrap());
    gconfig(fx, "filter.mine.required", "true");
    gconfig(fx, "filter.lfs.process", failing.to_str().unwrap());
    gconfig(fx, "filter.lfs.required", "true");
    gconfig(fx, "filter.a.b.smudge", smudge.to_str().unwrap());
    gconfig(fx, "core.autocrlf", "true");
    let global_attrs = fx.path().join("global-attrs");
    fs::write(&global_attrs, "*.ga text eol=crlf\n").unwrap();
    gconfig(fx, "core.attributesFile", global_attrs.to_str().unwrap());
    fs::write(ctx.clone.join(".git/info/attributes"), "*.ia filter=mine\n").unwrap();
    let all = [
        "x.flt",
        "x.lfs",
        "x.crlf",
        "x.enc",
        "x.id",
        "x.ga",
        "x.ia",
        "plain.txt",
    ];
    let mut checked: Vec<&str> = vec!["plain.txt", "x.ga", "x.ia", "x.flt", "x.lfs"];
    if at_least(2, 43) {
        checked.extend(["x.crlf", "x.enc", "x.id"]);
    } // before 2.43 attr.tree has no effect, so the PR's own attributes apply
    let verify = |wt: &Path| {
        for f in &checked {
            let blob =
                tgit_try(fx, wt, &["cat-file", "blob", &format!("{}:{f}", ctx.head)]).unwrap();
            assert_eq!(fs::read(wt.join(f)).unwrap(), blob, "{f}");
        }
        assert!(!marker.exists(), "a filter ran");
    };
    let first = ctx.ok();
    verify(&first.worktree);
    ctx.stop(&first);
    let wipe = |wt: &Path| {
        for f in all {
            fs::write(wt.join(f), "junk\n").unwrap();
        }
    };
    wipe(&first.worktree);
    let second = ctx.ok();
    verify(&second.worktree);
    if at_least(2, 43) {
        ctx.stop(&second);
        wipe(&second.worktree);
        let hostile = fx.git(&ctx.clone).env("GIT_ATTR_SOURCE", &ctx.head);
        let third = ctx.start_with(hostile, false).unwrap();
        verify(&third.worktree);
        assert_exact_with(fx, &third.worktree, &ctx.head, false);
    }
}

#[test]
fn hooks_never_run() {
    let mut ctx = Ctx::from_shape(|fx| simple_shape(fx, &[("a.txt", "a\n")]));
    // The second head is committed before any hook exists; it reaches the PR ref later through
    // the bare remote with hooks switched off (the global hooks apply there too).
    ctx.fx.write("b.txt", "b\n");
    let newer = ctx.fx.commit("second");
    ctx.fx.run(
        ctx.fx.work_path(),
        ["push", "origin", &format!("{newer}:refs/heads/second")],
    );
    let fx = &ctx.fx;
    let tools = fx.path().join("tools");
    let marker = fx.path().join("hook-marker");
    let ref_marker = fx.path().join("ref-hook-marker");
    let events = [
        "post-checkout",
        "reference-transaction",
        "post-index-change",
    ];
    let ref_body = format!(
        "#!/bin/sh\nwhile read old new ref; do case \"$ref\" in refs/prguide/*) ;; *) echo \"$ref\" >> '{}';; esac; done\nexit 0\n",
        ref_marker.display()
    );
    let local = ctx.clone.join(".git/hooks");
    let global = fx.path().join("ghooks");
    for dir in [&local, &global] {
        fs::create_dir_all(dir).unwrap();
        for event in events {
            let target = dir.join(event);
            if event == "reference-transaction" {
                fs::write(&target, &ref_body).unwrap();
            } else {
                fs::write(
                    &target,
                    format!("#!/bin/sh\necho ran >> '{}'\nexit 0\n", marker.display()),
                )
                .unwrap();
            }
            fs::set_permissions(&target, fs::Permissions::from_mode(0o755)).unwrap();
        }
    }
    gconfig(fx, "core.hooksPath", global.to_str().unwrap());
    let cfg = marker_script(&tools, "cfg", &marker, "");
    let ref_cfg = tools.join("ref-cfg");
    fs::write(&ref_cfg, &ref_body).unwrap();
    fs::set_permissions(&ref_cfg, fs::Permissions::from_mode(0o755)).unwrap();
    for event in ["post-checkout", "post-index-change"] {
        fx.run(
            fx.path(),
            ["config", "--global", "--add", "hook.h.event", event],
        );
    }
    gconfig(fx, "hook.h.command", cfg.to_str().unwrap());
    gconfig(fx, "hook.r.event", "reference-transaction");
    gconfig(fx, "hook.r.command", ref_cfg.to_str().unwrap());
    let first = ctx.ok();
    ctx.stop(&first);
    fs::write(first.worktree.join("junk.txt"), "j\n").unwrap();
    fs::write(first.worktree.join("a.txt"), "changed\n").unwrap();
    ctx.fx.run(
        ctx.fx.remote_path(),
        [
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "hook.reference-transaction.enabled=false",
            "update-ref",
            "refs/pull/7/head",
            &newer,
        ],
    );
    ctx.publish(&newer);
    ctx.head = newer.clone();
    let second = ctx.ok();
    // The fetch's own reference-transaction hook (refs/prguide/*) is a known residual; the test
    // hook ignores those refs, so any other ref name or any other hook file means a worktree
    // command fired a hook.
    assert!(!marker.exists(), "a hook ran");
    assert!(
        !ref_marker.exists(),
        "a reference-transaction hook ran for {:?}",
        fs::read_to_string(&ref_marker)
    );
    assert_exact(fx, &second.worktree, &ctx.head);
}

#[test]
fn hostile_checkout_config_in_each_scope() {
    for scope in ["global", "local", "env"] {
        let fx = FixtureRepo::new();
        let sub = fx.path().join("subrepo");
        fx.run(fx.path(), ["init", "-b", "main", "subrepo"]);
        fs::write(sub.join("s.txt"), "s\n").unwrap();
        fx.run(&sub, ["add", "-A"]);
        fx.run(&sub, ["commit", "-m", "s"]);
        fx.run(
            fx.work_path(),
            [
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "add",
                sub.to_str().unwrap(),
                "sm",
            ],
        );
        fx.write("f.sh", "echo\n");
        fx.commit("base");
        fx.push("main");
        on_pr(&fx);
        fx.write("g.txt", "g\n");
        let head = fx.commit("pr");
        fx.push_pr(7, &head);
        let clone = fx.clone("rev");
        fx.run(
            &clone,
            [
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "update",
                "--init",
            ],
        );
        let sm_head = fx.run(&clone.join("sm"), ["rev-parse", "HEAD"]);
        let ctx = Ctx::assemble(fx, head.clone(), clone, None);
        let marker = ctx.fx.path().join("fsmonitor-marker");
        let fsm = marker_script(&ctx.fx.path().join("tools"), "fsm", &marker, "");
        let fsm = fsm.to_str().unwrap().to_owned();
        let pairs = [
            ("core.fsmonitor", fsm.as_str()),
            ("submodule.recurse", "true"),
            ("core.fileMode", "false"),
        ];
        let mut git = ctx.fx.git(&ctx.clone);
        match scope {
            "global" => pairs.iter().for_each(|(k, v)| gconfig(&ctx.fx, k, v)),
            "local" => pairs.iter().for_each(|(k, v)| {
                ctx.fx.run(&ctx.clone, ["config", k, v]);
            }),
            _ => {
                let value = pairs
                    .iter()
                    .map(|(k, v)| format!("'{k}={v}'"))
                    .collect::<Vec<_>>()
                    .join(" ");
                git = git.env("GIT_CONFIG_PARAMETERS", value);
            }
        }
        let first = ctx.start_with(git.clone(), false).unwrap();
        fs::set_permissions(
            first.worktree.join("f.sh"),
            fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        ctx.stop(&first);
        let second = ctx
            .start_with(git, false)
            .unwrap_or_else(|e| panic!("{scope}: {e}"));
        assert!(!marker.exists(), "{scope}: fsmonitor ran");
        let mode = fs::metadata(second.worktree.join("f.sh"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o111, 0, "{scope}: mode {mode:o}");
        assert_eq!(
            ctx.fx.run(&ctx.clone.join("sm"), ["rev-parse", "HEAD"]),
            sm_head
        );
        assert_eq!(
            tgit(&ctx.fx, &second.worktree, &["rev-parse", "HEAD"]),
            head
        );
    }
}

#[test]
fn pr_symlink_is_a_plain_file_in_the_worktree() {
    let ctx = Ctx::from_shape(|fx| {
        on_pr(fx);
        std::os::unix::fs::symlink("../../../../.ssh/id_rsa", fx.work_path().join("escape"))
            .unwrap();
        fx.commit("link")
    });
    let out = ctx.ok();
    let path = out.worktree.join("escape");
    let meta = fs::symlink_metadata(&path).unwrap();
    assert!(meta.file_type().is_file());
    assert_eq!(fs::read(&path).unwrap(), b"../../../../.ssh/id_rsa");
}

#[test]
fn environment_config_and_hostile_variables_are_stripped() {
    fn sharedindex(ctx: &Ctx) -> Vec<String> {
        let mut found = Vec::new();
        let dir = ctx.clone.join(".git/worktrees");
        if let Ok(entries) = fs::read_dir(dir) {
            for entry in entries {
                for f in fs::read_dir(entry.unwrap().path()).unwrap() {
                    let name = f.unwrap().file_name().to_string_lossy().into_owned();
                    if name.starts_with("sharedindex.") {
                        found.push(name);
                    }
                }
            }
        }
        found
    }
    let variants: Vec<&str> = if at_least(2, 31) {
        vec!["parameters", "count"]
    } else {
        eprintln!("GIT_CONFIG_COUNT needs git 2.31; checking only GIT_CONFIG_PARAMETERS");
        vec!["parameters"]
    };
    for variant in variants {
        let ctx = Ctx::new();
        let marker = ctx.fx.path().join("fsm-marker");
        let fsm = marker_script(&ctx.fx.path().join("tools"), "fsm", &marker, "");
        let mut git = ctx
            .fx
            .git(&ctx.clone)
            .env("GIT_TEST_FSMONITOR", &fsm)
            .env("GIT_DIR", ctx.clone.join(".git"))
            .env("GIT_WORK_TREE", &ctx.clone)
            .env("GIT_INDEX_FILE", ctx.clone.join(".git/index"))
            .env("GIT_ICASE_PATHSPECS", "1")
            .env("GIT_GLOB_PATHSPECS", "1");
        git = if variant == "parameters" {
            git.env(
                "GIT_CONFIG_PARAMETERS",
                "'core.splitindex=true' 'index.version=4'",
            )
        } else {
            git.env("GIT_CONFIG_COUNT", "1")
                .env("GIT_CONFIG_KEY_0", "core.splitIndex")
                .env("GIT_CONFIG_VALUE_0", "true")
        };
        let before = state(&ctx.fx, &ctx.clone, &[]);
        let out = ctx.start_with(git, false).unwrap();
        assert_eq!(sharedindex(&ctx), Vec::<String>::new(), "{variant}");
        // index.version=4 is a second visible effect, for git versions where the
        // core.fsmonitor pin alone suppresses the split index.
        let index = fs::read(gitdir_of(&out.worktree).join("index")).unwrap();
        assert_ne!(&index[4..8], &[0, 0, 0, 4], "{variant}: index version 4");
        assert!(!marker.exists(), "{variant}: fsmonitor ran");
        assert_eq!(state(&ctx.fx, &ctx.clone, &[]), before, "{variant}");
        assert_exact(&ctx.fx, &out.worktree, &ctx.head);
    }
}

#[test]
fn replace_refs_do_not_change_the_worktree_or_files() {
    let ctx = Ctx::new();
    let first = ctx.ok();
    let base = ctx.fx.run(ctx.fx.work_path(), ["rev-parse", "main"]);
    ctx.fx.run(&ctx.clone, ["replace", &ctx.head, &base]);
    ctx.stop(&first);
    let second = ctx.ok();
    assert!(second.worktree.join("added.txt").exists());
    assert_exact(&ctx.fx, &second.worktree, &ctx.head);
    assert_eq!(
        serde_json::to_value(&first.files).unwrap(),
        serde_json::to_value(&second.files).unwrap()
    );
}

#[test]
fn pr_head_moved_fails_before_touching_the_worktree() {
    let ctx = Ctx::new();
    // An older commit of the PR itself, not the main tip.
    let base = ctx
        .fx
        .run(ctx.fx.work_path(), ["rev-parse", &format!("{}^", ctx.head)]);
    ctx.fake.set_pr(&ctx.repo, ctx.pr(&base));
    let err = ctx.start(false).unwrap_err();
    assert_eq!(err.code(), "pr_head_moved");
    let msg = err.to_string();
    assert!(msg.contains(&base) && msg.contains(&ctx.head), "{msg}");
    assert!(msg.contains("run prguide start again"), "{msg}");
    assert!(!ctx.wt().exists());
    assert!(ctx.runs().is_empty());
}

#[test]
fn pr_repo_follows_gh_rules() {
    fn repo_with(fx: &FixtureRepo, name: &str, remotes: &[(&str, &str)]) -> PathBuf {
        let dir = fx.path().join(name);
        fx.run(fx.path(), ["init", name]);
        for (n, url) in remotes {
            fx.run(&dir, ["remote", "add", n, url]);
        }
        dir
    }
    let fx = FixtureRepo::new();
    let gh = |s: &str| format!("https://github.com/{s}.git");
    let ok = |dir: &Path| pr_repo(&fx.git(dir)).unwrap();
    let pair = |o: &str, r: &str| (o.to_owned(), r.to_owned());

    let a = repo_with(&fx, "a", &[("origin", &gh("octo/widget"))]);
    assert_eq!(ok(&a), pair("octo", "widget"));

    let b = repo_with(
        &fx,
        "b",
        &[
            ("origin", &gh("me/widget")),
            ("upstream", &gh("octo/widget")),
        ],
    );
    assert_eq!(ok(&b), pair("octo", "widget"));

    fx.run(&b, ["config", "remote.origin.gh-resolved", "base"]);
    assert_eq!(ok(&b), pair("me", "widget"));
    fx.run(&b, ["config", "remote.origin.gh-resolved", "me/widget"]);
    assert_eq!(ok(&b), pair("octo", "widget"), "only `base` counts");

    let d = repo_with(
        &fx,
        "d",
        &[
            ("origin", &gh("octo/widget")),
            ("backup", &gh("octo/widget")),
        ],
    );
    assert_eq!(ok(&d), pair("octo", "widget"));
    let d2 = repo_with(
        &fx,
        "d2",
        &[
            ("backup", &gh("octo/widget")),
            ("mirror", &gh("octo/widget")),
        ],
    );
    assert_eq!(ok(&d2), pair("octo", "widget"));
    let d3 = repo_with(
        &fx,
        "d3",
        &[
            ("origin", &gh("octo/widget")),
            ("backup", &gh("Octo/Widget")),
        ],
    );
    assert_eq!(
        ok(&d3),
        pair("Octo", "Widget"),
        "first remote in name order"
    );

    let e = repo_with(&fx, "e", &[("a", &gh("x/one")), ("b", &gh("y/two"))]);
    let err = pr_repo(&fx.git(&e)).unwrap_err();
    assert_eq!(err.code(), "not_in_clone");
    let msg = err.to_string();
    assert!(
        msg.contains("x/one") && msg.contains("y/two") && msg.contains("gh repo set-default"),
        "{msg}"
    );

    let f = repo_with(&fx, "f", &[("github", &gh("g/r")), ("origin", &gh("o/r"))]);
    assert_eq!(ok(&f), pair("g", "r"));

    let g = repo_with(
        &fx,
        "g",
        &[("origin", "https://gitlab.com/octo/widget.git")],
    );
    let err = pr_repo(&fx.git(&g)).unwrap_err();
    assert_eq!(err.code(), "not_in_clone");
    assert!(
        err.to_string()
            .contains("no remote of this clone points at a github.com repository")
    );

    let h = repo_with(
        &fx,
        "h",
        &[
            ("origin", "https://gitlab.com/octo/widget.git"),
            ("upstream", &gh("octo/widget")),
        ],
    );
    assert_eq!(ok(&h), pair("octo", "widget"));

    let i = fx.clone("i");
    assert_eq!(ok(&i), pair("octo", "widget"));
}

#[test]
fn github_error_creates_no_store() {
    let ctx = Ctx::new();
    ctx.fake.push_error(
        FakeMethod::Pr,
        GitHubError::PrNotFound {
            repo: "octo/widget".into(),
            number: 7,
        },
    );
    let err = ctx.start(false).unwrap_err();
    assert_eq!(err.code(), "pr_not_found");
    assert!(err.to_string().contains("octo/widget#7"), "{err}");
    assert!(!ctx.home.join("prs").exists());

    let ctx = Ctx::new();
    let tmp = TempDir::new().unwrap();
    let dir = canon(tmp.path());
    let git = ctx
        .fx
        .git(&dir)
        .env("GIT_CEILING_DIRECTORIES", dir.parent().unwrap());
    let err = ctx.start_with(git, false).unwrap_err();
    assert_eq!(err.code(), "not_in_clone");
    assert!(err.to_string().contains("not inside a git clone"), "{err}");
    assert!(ctx.fake.calls().is_empty());
}

#[test]
fn fetch_failure_leaves_no_run_or_worktree() {
    let fx = FixtureRepo::new();
    let head = s1_shape(&fx);
    let clone = fx.clone("rev");
    let ctx = Ctx::assemble(fx, head, clone, None);
    let err = ctx.start(false).unwrap_err();
    assert_eq!(err.code(), "git_failed");
    assert!(err.to_string().contains("fetch"), "{err}");
    assert!(!ctx.wt().exists());
    assert!(ctx.runs().is_empty());
}

#[test]
fn binary_start_outside_a_clone_prints_json_error() {
    let tmp = TempDir::new().unwrap();
    let dir = canon(tmp.path());
    let output = assert_cmd::Command::new(env!("CARGO_BIN_EXE_prguide"))
        .args(["start", "7"])
        .current_dir(&dir)
        .env("HOME", &dir)
        .env("PRGUIDE_HOME", dir.join("ph"))
        .env("GIT_CEILING_DIRECTORIES", dir.parent().unwrap())
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert_eq!(stderr.lines().count(), 1);
    let value: serde_json::Value = serde_json::from_str(&stderr).unwrap();
    assert_eq!(value["code"], "not_in_clone");
    assert!(
        value["message"]
            .as_str()
            .unwrap()
            .contains("not inside a git clone")
    );
}

#[test]
fn worktree_of_another_clone_is_rebuilt() {
    let ctx = Ctx::new();
    let first = ctx.ok();
    ctx.stop(&first);
    let other = canon(&ctx.fx.clone("other"));
    let sha = ctx.fx.run(&other, ["rev-parse", "HEAD"]);
    fs::remove_dir_all(&first.worktree).unwrap();
    ctx.fx.run(
        &other,
        [
            "worktree",
            "add",
            "--detach",
            first.worktree.to_str().unwrap(),
            &sha,
        ],
    );
    let out = ctx.ok();
    let common = tgit(&ctx.fx, &out.worktree, &["rev-parse", "--git-common-dir"]);
    assert_eq!(
        canon(&out.worktree.join(common)),
        canon(&ctx.clone.join(".git"))
    );
    assert_exact(&ctx.fx, &out.worktree, &ctx.head);
}

#[test]
fn system_attributes_do_not_apply() {
    struct Guard(PathBuf);
    impl Drop for Guard {
        fn drop(&mut self) {
            let _ = fs::remove_file(&self.0);
        }
    }
    if std::env::var("PRGUIDE_TEST_SYSTEM_ATTRS").as_deref() != Ok("1") {
        eprintln!("S31 not run: set PRGUIDE_TEST_SYSTEM_ATTRS=1 (use the git 2.55 build)");
        return;
    }
    let out = Command::new("git")
        .args(["var", "GIT_ATTR_SYSTEM"])
        .output()
        .unwrap();
    let path = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim());
    if !out.status.success() || path.exists() || !path.parent().is_some_and(Path::is_dir) {
        eprintln!("S31 not run: no writable, absent system attributes path ({path:?})");
        return;
    }
    let ctx = Ctx::from_shape(|fx| {
        on_pr(fx);
        fx.write("x.sysattr", "a\nb\n");
        fx.commit("sys")
    });
    if fs::write(&path, "*.sysattr text eol=crlf\n").is_err() {
        eprintln!("S31 not run: {path:?} is not writable");
        return;
    }
    let _guard = Guard(path);
    let out = ctx.ok();
    assert_exact_with(&ctx.fx, &out.worktree, &ctx.head, false);
}

#[test]
fn unrelated_history_fails_without_starting_a_run() {
    let ctx = Ctx::from_shape(|fx| {
        fx.run(fx.work_path(), ["checkout", "--orphan", "other"]);
        fx.run(fx.work_path(), ["rm", "-rf", "."]);
        fx.write("solo.txt", "solo\n");
        fx.commit("root")
    });
    let err = ctx.start(false).unwrap_err();
    assert_eq!(err.code(), "no_base_commit");
    assert!(err.to_string().contains("no merge base"), "{err}");
    assert!(ctx.runs().is_empty());
}

#[test]
fn sparse_reviewer_clone_does_not_thin_the_worktree() {
    let ctx = Ctx::from_shape(|fx| {
        on_pr(fx);
        fx.write("keep/k", "k\n");
        fx.write("drop/d", "d\n");
        fx.write("top", "t\n");
        fx.commit("tree")
    });
    ctx.fx.run(&ctx.clone, ["sparse-checkout", "set", "keep"]);
    let first = ctx.ok();
    assert_exact(&ctx.fx, &first.worktree, &ctx.head);
    ctx.stop(&first);
    fs::remove_dir_all(first.worktree.join("drop")).unwrap();
    let second = ctx.ok();
    assert_exact(&ctx.fx, &second.worktree, &ctx.head);
}

#[test]
fn a_file_or_symlink_at_the_worktree_path_is_replaced() {
    let ctx = Ctx::new();
    let first = ctx.ok();
    ctx.stop(&first);
    let wt = ctx.wt();
    fs::remove_dir_all(&wt).unwrap();
    fs::write(&wt, "not a directory\n").unwrap();
    let second = ctx.ok();
    assert_exact(&ctx.fx, &second.worktree, &ctx.head);
    ctx.stop(&second);
    fs::remove_dir_all(&wt).unwrap();
    std::os::unix::fs::symlink(ctx.home.join("nowhere"), &wt).unwrap();
    let third = ctx.ok();
    assert!(!fs::symlink_metadata(&wt).unwrap().file_type().is_symlink());
    assert_exact(&ctx.fx, &third.worktree, &ctx.head);
}

#[test]
fn reset_worktree_rejects_a_bad_head_before_running_git() {
    let ctx = Ctx::new();
    let out = ctx.ok();
    let err = ctx
        .fx
        .git(&ctx.clone)
        .reset_worktree(&out.worktree, "abc")
        .unwrap_err();
    assert_eq!(err.code(), "invalid_ref");
    assert!(err.to_string().contains("40 or 64"), "{err}");
    assert_exact(&ctx.fx, &out.worktree, &ctx.head);
}

#[test]
fn pr_repo_with_base_on_remotes_naming_different_repos_fails() {
    let fx = FixtureRepo::new();
    let dir = fx.path().join("two-base");
    fx.run(fx.path(), ["init", "two-base"]);
    fx.run(
        &dir,
        [
            "remote",
            "add",
            "origin",
            "https://github.com/me/widget.git",
        ],
    );
    fx.run(
        &dir,
        [
            "remote",
            "add",
            "other",
            "https://github.com/octo/widget.git",
        ],
    );
    for r in ["origin", "other"] {
        fx.run(&dir, ["config", &format!("remote.{r}.gh-resolved"), "base"]);
    }
    let err = pr_repo(&fx.git(&dir)).unwrap_err();
    assert_eq!(err.code(), "not_in_clone");
    let msg = err.to_string();
    assert!(
        msg.contains("me/widget")
            && msg.contains("octo/widget")
            && msg.contains("gh repo set-default"),
        "{msg}"
    );
}

#[test]
fn ready_guide_versions_are_listed_oldest_first() {
    let ctx = Ctx::new();
    let out = ctx.ok();
    ctx.stop(&out);
    ctx.exec(
        "INSERT INTO guide_versions(version,state,caused_by,snapshot_base,snapshot_head,selection_kind,selection_base,selection_head,commits_json,refs_json,created_at,ready_at) \
         VALUES (3,'ready',NULL,'sb3','sh3','full','xb3','xh3','[]','[]','2026-01-03T00:00:00.000Z','2026-01-03T00:01:00.000Z'), \
                (2,'staging',NULL,'sb2','sh2','full','xb2','xh2','[]','[]','2026-01-02T00:00:00.000Z',NULL), \
                (1,'ready',NULL,'sb1','sh1','full','xb1','xh1','[]','[]','2026-01-01T00:00:00.000Z','2026-01-01T00:01:00.000Z');",
    );
    let out = ctx.ok();
    let versions: Vec<i64> = out.guide_versions.iter().map(|v| v.version).collect();
    assert_eq!(versions, vec![1, 3]);
}

#[test]
fn url_shorthand_remote_resolves_to_its_github_repo() {
    let fx = FixtureRepo::new();
    let dir = fx.path().join("shorthand");
    fx.run(fx.path(), ["init", "shorthand"]);
    fx.run(&dir, ["remote", "add", "origin", "gh:octo/widget"]);
    fx.run(&dir, ["config", "url.https://github.com/.insteadOf", "gh:"]);
    assert_eq!(
        pr_repo(&fx.git(&dir)).unwrap(),
        ("octo".to_owned(), "widget".to_owned())
    );
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

/// Start, stop, let `plant` rewrite `<store>/worktree`, then start again and check
/// that the dirty clone and the dirty linked worktree are byte-identical and the
/// new worktree is a real, exact one.
fn planted_worktree(plant: impl FnOnce(&Ctx, &Path, &Path)) {
    let ctx = Ctx::new();
    dirty(&ctx.fx, &ctx.clone);
    let first = ctx.ok();
    ctx.stop(&first);
    let wt = ctx.wt();
    let linked = dirty_linked(&ctx, "linked");
    plant(&ctx, &wt, &linked);
    let skip = vec![linked.clone()];
    let clone_before = state(&ctx.fx, &ctx.clone, &skip);
    let linked_before = state(&ctx.fx, &linked, &[]);
    let second = ctx.ok();
    assert_eq!(state(&ctx.fx, &ctx.clone, &skip), clone_before);
    assert_eq!(state(&ctx.fx, &linked, &[]), linked_before);
    let meta = fs::symlink_metadata(&wt).unwrap();
    assert!(meta.is_dir() && !meta.file_type().is_symlink());
    assert!(fs::symlink_metadata(wt.join(".git")).unwrap().is_file());
    assert_exact(&ctx.fx, &second.worktree, &ctx.head);
}

#[test]
fn symlinked_worktree_path_is_replaced_not_followed() {
    planted_worktree(|_, wt, linked| {
        fs::remove_dir_all(wt).unwrap();
        std::os::unix::fs::symlink(linked, wt).unwrap();
    });
}

#[test]
fn worktree_git_file_pointing_at_the_clone_is_rebuilt() {
    planted_worktree(|ctx, wt, _| {
        fs::write(
            wt.join(".git"),
            format!("gitdir: {}\n", ctx.clone.join(".git").display()),
        )
        .unwrap();
    });
}

#[test]
fn worktree_git_file_pointing_at_a_reviewer_worktree_is_rebuilt() {
    planted_worktree(|_, wt, linked| {
        let admin = gitdir_of(linked);
        fs::write(wt.join(".git"), format!("gitdir: {}\n", admin.display())).unwrap();
    });
}

#[test]
fn worktree_git_symlink_to_the_clone_git_dir_is_rebuilt() {
    planted_worktree(|ctx, wt, _| {
        fs::remove_file(wt.join(".git")).unwrap();
        std::os::unix::fs::symlink(ctx.clone.join(".git"), wt.join(".git")).unwrap();
    });
}

#[test]
fn worktree_config_core_worktree_elsewhere_is_rebuilt() {
    planted_worktree(|ctx, wt, _| {
        ctx.fx
            .run(&ctx.clone, ["config", "extensions.worktreeConfig", "true"]);
        ctx.fx.run(
            wt,
            [
                "config",
                "--worktree",
                "core.worktree",
                ctx.clone.to_str().unwrap(),
            ],
        );
    });
}

#[test]
fn worktree_git_symlink_to_a_reviewer_gitfile_is_rebuilt() {
    planted_worktree(|_, wt, linked| {
        fs::remove_file(wt.join(".git")).unwrap();
        std::os::unix::fs::symlink(linked.join(".git"), wt.join(".git")).unwrap();
    });
}

#[test]
fn pr_file_named_false_never_runs() {
    // git 2.30 reads core.fsmonitor as a hook path, so the value `false` would run
    // a program named `false` found on PATH (with `.` on PATH: the PR's own file).
    // Newer git reads a boolean. The test runs on every version.
    let marker_dir = TempDir::new().unwrap();
    let marker = marker_dir.path().join("marker");
    let marker_in_shape = marker.clone();
    let mut ctx = Ctx::from_shape(move |fx| {
        on_pr(fx);
        fx.write(
            "false",
            format!(
                "#!/bin/sh\necho ran >> '{}'\nexit 0\n",
                marker_in_shape.display()
            ),
        );
        let file = fx.work_path().join("false");
        fs::set_permissions(&file, fs::Permissions::from_mode(0o755)).unwrap();
        fx.commit("false")
    });
    let path = format!(".:{}", std::env::var("PATH").unwrap());
    let git = |ctx: &Ctx| ctx.fx.git(&ctx.clone).env("PATH", path.as_str());
    let first = ctx.start_with(git(&ctx), false).unwrap();
    ctx.stop(&first);
    ctx.advance("other.txt", "o\n");
    ctx.start_with(git(&ctx), false).unwrap();
    assert!(!marker.exists(), "a program named false ran");
}

#[test]
fn relative_worktree_config_leaves_reviewer_config_untouched() {
    // worktree.useRelativePaths exists from git 2.48; older git ignores it.
    if !at_least(2, 48) {
        eprintln!("skipped: git < 2.48");
        return;
    }
    let ctx = Ctx::new();
    gconfig(&ctx.fx, "worktree.useRelativePaths", "true");
    let config = ctx.clone.join(".git/config");
    let before = fs::read(&config).unwrap();
    let out = ctx.ok();
    assert_eq!(fs::read(&config).unwrap(), before);
    assert_exact(&ctx.fx, &out.worktree, &ctx.head);
}
