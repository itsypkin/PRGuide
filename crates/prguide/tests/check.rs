mod support;

use std::{
    collections::BTreeMap,
    fs,
    os::unix::fs::symlink,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use prguide::{
    Error,
    anchors::{At, BlockContext, CodeBlock, Side, resolve},
    cli::{
        check::{CheckArgs, CheckEnv, check},
        guide_new::{GuideNewArgs, GuideNewEnv, GuideNewOutput, guide_new},
        start::{StartArgs, StartEnv, start},
    },
    github::{Commit, CommitAuthor, Pr, PrState, Repo, fake::FakeGitHub},
    guide::{
        check::{CheckReport, Diagnostic, Severity, draft_path},
        model::block_field,
        parse::load_draft,
        range::{ResolvedSelection, Selection, Snapshot},
    },
    store::{
        Clock, FakeClock, PrRef, Store, StoreError, Timestamp,
        runs::{self, LEASE, RunId, StopReason},
    },
};
use support::fixture_repo::FixtureRepo;
use tempfile::TempDir;

// ---------------------------------------------------------------- helpers

fn canon(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap()
}

const HANDLER: &str = "{ id: handler, kind: diff, path: src/lib.rs, side: new, start: 4, end: 6 }";
const CTXB: &str = "{ id: ctx, kind: context, path: ctx.txt, start: 1, end: 3 }";
const FLAG5: &str = "{ at: { block: handler, line: 5 }, text: \"Check this.\" }";

fn slide(id: &str, kind: &str, title: &str, code: &[&str], flags: &[&str], body: &str) -> String {
    let mut s = format!("---\nid: {id}\nkind: {kind}\ntitle: {title}\n");
    if !code.is_empty() {
        s.push_str("code:\n");
        for c in code {
            s.push_str(&format!("  - {c}\n"));
        }
    }
    if !flags.is_empty() {
        s.push_str("flags:\n");
        for f in flags {
            s.push_str(&format!("  - {f}\n"));
        }
    }
    s.push_str(&format!("---\n{body}\n"));
    s
}

fn step(id: &str, code: &[&str], flags: &[&str]) -> String {
    slide(id, "step", "Step one", code, flags, "Step body.")
}

fn extra(id: &str) -> String {
    slide(id, "step", "Extra", &[], &[], "Extra.")
}

fn e(file: &str, field: &str, code: &'static str, message: &str) -> Diagnostic {
    Diagnostic {
        severity: Severity::Error,
        file: file.into(),
        field: field.into(),
        code,
        message: message.into(),
    }
}

fn w(file: &str, field: &str, code: &'static str, message: &str) -> Diagnostic {
    Diagnostic {
        severity: Severity::Warning,
        ..e(file, field, code, message)
    }
}

fn dblk(id: &str, path: &str, start: u32, end: u32) -> CodeBlock {
    CodeBlock::Diff {
        id: id.into(),
        path: path.into(),
        side: Side::New,
        start: Some(start),
        end: Some(end),
    }
}

fn cblk(id: &str, path: &str, start: u32, end: u32) -> CodeBlock {
    CodeBlock::Context {
        id: id.into(),
        path: path.into(),
        at: At::SnapshotHead,
        start,
        end,
    }
}

/// The diagnostics `load_draft` reports for a directory (all errors).
fn mapped(dir: &Path) -> Vec<Diagnostic> {
    load_draft(dir)
        .errors
        .iter()
        .map(|x| {
            e(
                x.file().unwrap(),
                x.field().unwrap(),
                x.code(),
                &x.to_string(),
            )
        })
        .collect()
}

fn dup_slide(i: usize, id: &str, j: usize) -> Diagnostic {
    e(
        "guide.yaml",
        &format!("slides[{i}]"),
        "duplicate_slide_id",
        &format!(
            "slide `{id}` is listed more than once (first at slides[{j}]); list each slide once"
        ),
    )
}

fn missing_slide(i: usize, id: &str) -> Diagnostic {
    e(
        "guide.yaml",
        &format!("slides[{i}]"),
        "slide_file_missing",
        &format!("slide `{id}` is listed, but slides/{id}.md does not exist"),
    )
}

fn unlisted(id: &str) -> Diagnostic {
    w(
        &format!("slides/{id}.md"),
        "",
        "unlisted_slide",
        &format!("slides/{id}.md is not listed in guide.yaml's `slides`; list it or delete it"),
    )
}

fn dup_block(file: &str, i: usize, id: &str, j: usize) -> Diagnostic {
    e(
        file,
        &format!("code[{i}].id"),
        "duplicate_block_id",
        &format!(
            "block id `{id}` is used more than once in this slide (first at code[{j}]); give each block its own id"
        ),
    )
}

fn out_of_range(k: usize, line: u32, block: &str, s: u32, en: u32) -> Diagnostic {
    e(
        "slides/step-one.md",
        &format!("flags[{k}].at.line"),
        "flag_line_out_of_range",
        &format!("line {line} is outside block `{block}`, which covers lines {s}-{en}"),
    )
}

fn draft_unknown_msg(arg: &Path, run: &str) -> String {
    format!(
        "{arg:?} is not a draft of run {run}: pass the `draft` path that prguide guide new printed for this run"
    )
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
}

fn shape_check(fx: &FixtureRepo) -> String {
    let lines = |p: &str, n: usize| (1..=n).map(|i| format!("{p}{i}\n")).collect::<String>();
    fx.write("src/lib.rs", lines("l", 10));
    fx.write("ctx.txt", lines("c", 5));
    fx.write("old.txt", lines("o", 3));
    fx.write("bin.dat", b"\0bin\0");
    fx.commit("base");
    fx.push("main");
    fx.run(fx.work_path(), ["checkout", "-b", "pr"]);
    let mut lib = lines("l", 10).replace("l5\n", "L5\n");
    fx.write("src/lib.rs", &lib);
    lib.clear();
    fx.write("new.txt", lines("n", 4));
    fx.commit("first");
    fx.remove("old.txt");
    fx.write("bin.dat", b"\0bin2\0");
    fx.commit("second")
}

impl Ctx {
    fn new() -> Self {
        Self::build(1_767_225_600_000)
    }

    fn live() -> Self {
        let ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64;
        Self::build(ms)
    }

    fn build(ms: i64) -> Self {
        let fx = FixtureRepo::new();
        let head = shape_check(&fx);
        fx.push_pr(7, &head);
        let clone = fx.clone("rev");
        let guard = TempDir::new().unwrap();
        let mut ctx = Self {
            home: canon(guard.path()),
            _guard: guard,
            clone: canon(&clone),
            fake: FakeGitHub::new(),
            clock: Arc::new(FakeClock::new(Timestamp::from_unix_millis(ms))),
            repo: Repo::github("octo", "widget").unwrap(),
            run: String::new(),
            out: empty_output(),
            draft: PathBuf::new(),
            fx,
        };
        ctx.publish(&head);
        ctx.run = ctx.start_run(false);
        ctx.out = ctx.gn(&ctx.run.clone());
        ctx.draft = ctx.out.draft.clone();
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

    fn start_run(&self, takeover: bool) -> String {
        let clock: Arc<dyn Clock> = self.clock.clone();
        let env = StartEnv {
            git: self.fx.git(&self.clone),
            github: &self.fake,
            home: self.home.clone(),
            clock,
        };
        start(&env, &StartArgs { pr: 7, takeover })
            .unwrap()
            .run_id
            .to_string()
    }

    fn gn(&self, run: &str) -> GuideNewOutput {
        let clock: Arc<dyn Clock> = self.clock.clone();
        let env = GuideNewEnv {
            git: self.fx.git(&self.clone),
            github: &self.fake,
            home: self.home.clone(),
            clock,
        };
        guide_new(
            &env,
            &GuideNewArgs {
                run: run.into(),
                commits: None,
            },
        )
        .unwrap()
    }

    fn env(&self) -> CheckEnv {
        let clock: Arc<dyn Clock> = self.clock.clone();
        CheckEnv {
            git: self.fx.git(&self.clone),
            home: self.home.clone(),
            clock,
        }
    }

    fn check_in(&self, run: &str, draft: &Path) -> Result<CheckReport, Error> {
        check(
            &self.env(),
            &CheckArgs {
                draft: draft.to_path_buf(),
                run: run.into(),
            },
        )
    }

    fn check_path(&self, draft: &Path) -> Result<CheckReport, Error> {
        self.check_in(&self.run, draft)
    }

    fn chk(&self) -> CheckReport {
        self.check_path(&self.draft).unwrap()
    }

    fn diags(&self) -> Vec<Diagnostic> {
        self.chk().diagnostics
    }

    fn store(&self) -> Store {
        let clock: Arc<dyn Clock> = self.clock.clone();
        Store::open_in(&self.home, &PrRef::new("octo", "widget", 7).unwrap())
            .unwrap()
            .with_clock(clock)
    }

    fn query(&self, sql: &str) -> Vec<Vec<String>> {
        query_store(self.store(), sql)
    }

    fn yaml(&self) -> String {
        fs::read_to_string(self.draft.join("guide.yaml")).unwrap()
    }

    fn set_yaml(&self, text: &str) {
        fs::write(self.draft.join("guide.yaml"), text).unwrap();
    }

    /// Replaces `from` by `to` on the first line starting with `prefix`.
    fn tamper(&self, prefix: &str, from: &str, to: &str) {
        let text = self.yaml();
        let mut done = false;
        let lines: Vec<String> = text
            .lines()
            .map(|l| {
                if !done && l.starts_with(prefix) {
                    assert!(l.contains(from), "`{from}` not in `{l}`");
                    done = true;
                    l.replacen(from, to, 1)
                } else {
                    l.to_owned()
                }
            })
            .collect();
        assert!(done, "no line starts with {prefix}");
        self.set_yaml(&(lines.join("\n") + "\n"));
    }

    fn set_slides(&self, ids: &[&str]) {
        let text = self.yaml();
        let lines: Vec<String> = text
            .lines()
            .map(|l| {
                if l.starts_with("slides:") {
                    format!("slides: [{}]", ids.join(", "))
                } else {
                    l.to_owned()
                }
            })
            .collect();
        self.set_yaml(&(lines.join("\n") + "\n"));
    }

    fn put(&self, id: &str, text: &str) {
        fs::write(self.draft.join("slides").join(format!("{id}.md")), text).unwrap();
    }

    fn rm(&self, id: &str) {
        fs::remove_file(self.draft.join("slides").join(format!("{id}.md"))).unwrap();
    }

    /// The valid deck V.
    fn valid(&self) {
        self.set_slides(&["overview", "arch", "step-one"]);
        self.put(
            "overview",
            &slide("overview", "overview", "Overview", &[], &[], "Overview."),
        );
        self.put(
            "arch",
            &slide("arch", "architecture", "The shape", &[], &[], "Shape."),
        );
        self.put("step-one", &step("step-one", &[HANDLER, CTXB], &[FLAG5]));
    }

    fn with_step(&self, code: &[&str], flags: &[&str]) {
        self.valid();
        self.put("step-one", &step("step-one", code, flags));
    }

    fn cx(&self) -> BlockContext {
        BlockContext::new(
            &self.fx.git(&self.clone),
            0,
            Snapshot {
                base: self.out.snapshot.base.clone(),
                head: self.out.snapshot.head.clone(),
            },
            ResolvedSelection {
                selection: Selection::Full,
                base: self.out.selection.base.clone(),
                head: self.out.selection.head.clone(),
                commits: self.out.commits.iter().map(|c| c.sha.clone()).collect(),
            },
        )
        .unwrap()
    }

    /// The error diagnostic `resolve` gives for a block, as check reports it.
    fn anchor(&self, file: &str, i: usize, block: &CodeBlock) -> Diagnostic {
        match resolve(&self.fx.git(&self.clone), block, &self.cx()) {
            Err(Error::Anchor(x)) => e(file, &block_field(i, x.field()), x.code(), &x.to_string()),
            other => panic!("expected an anchor error, got {other:?}"),
        }
    }

    fn anchor_warning(&self, file: &str, i: usize, block: &CodeBlock) -> Diagnostic {
        let r = resolve(&self.fx.git(&self.clone), block, &self.cx()).unwrap();
        assert_eq!(r.warnings.len(), 1);
        let x = &r.warnings[0];
        w(file, &block_field(i, x.field), x.code, &x.message)
    }

    fn set_row(&self, f: impl Fn(&str) -> String) {
        let json = self.query(&format!(
            "SELECT provenance_json FROM drafts WHERE id = '{}'",
            self.out.draft_id
        ))[0][0]
            .clone();
        let new = f(&json);
        let id = self.out.draft_id.clone();
        self.store()
            .tx(|tx| -> Result<(), StoreError> {
                tx.execute(
                    "UPDATE drafts SET provenance_json = ?1 WHERE id = ?2",
                    (&new, &id),
                )?;
                Ok(())
            })
            .unwrap();
    }

    fn bin(&self, cwd: &Path, draft: &Path, run: &str) -> std::process::Output {
        let home = self.fx.path().join("home");
        assert_cmd::Command::new(env!("CARGO_BIN_EXE_prguide"))
            .current_dir(cwd)
            .arg("check")
            .arg(draft)
            .args(["--run", run])
            .env("HOME", &home)
            .env("XDG_CONFIG_HOME", home.join(".config"))
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", home.join(".gitconfig"))
            .env("PRGUIDE_HOME", &self.home)
            .output()
            .unwrap()
    }

    fn bin_ok(&self) -> std::process::Output {
        self.bin(&self.clone, &self.draft, &self.run)
    }
}

/// `GuideNewOutput` has no `Default`; build one through serde-free means.
fn empty_output() -> GuideNewOutput {
    use prguide::cli::{guide_new::SelectionOut, start::SnapshotOut};
    GuideNewOutput {
        draft_id: String::new(),
        draft: PathBuf::new(),
        worktree: PathBuf::new(),
        snapshot: SnapshotOut {
            base: String::new(),
            head: String::new(),
        },
        selection: SelectionOut {
            kind: prguide::guide::model::SelectionKind::Full,
            base: String::new(),
            head: String::new(),
        },
        commits: Vec::new(),
        files: Vec::new(),
    }
}

fn assert_err(result: Result<CheckReport, Error>, code: &str, message: &str) {
    let err = result.unwrap_err();
    assert_eq!(err.code(), code, "{err}");
    assert_eq!(err.to_string(), message);
}

fn stderr_json(out: &std::process::Output) -> serde_json::Value {
    let text = String::from_utf8(out.stderr.clone()).unwrap();
    assert_eq!(text.lines().count(), 1, "{text}");
    serde_json::from_str(&text).unwrap()
}

fn no_hostile_bytes(b: &[u8]) {
    assert!(!b.contains(&0x1b) && !b.contains(&0x7f));
    for p in b.windows(2) {
        assert!(!(p[0] == 0xC2 && (0x80..=0x9F).contains(&p[1])), "C1");
        assert!(!(p[0] == 0xD8 && p[1] == 0x9C), "ALM");
    }
    for p in b.windows(3) {
        assert!(
            !(p[0] == 0xE2
                && ((p[1] == 0x80
                    && ([0x8E, 0x8F].contains(&p[2]) || (0xA8..=0xAE).contains(&p[2])))
                    || (p[1] == 0x81 && (0xA6..=0xA9).contains(&p[2])))),
            "bidi or separator"
        );
    }
}

// ---------------------------------------------------------------- output

/// C1 valid_draft_passes
#[test]
fn valid_draft_passes() {
    let ctx = Ctx::new();
    ctx.valid();
    assert_eq!(
        draft_path(&ctx.draft, ctx.fx.git(&ctx.clone).dir()),
        Some(ctx.draft.to_string_lossy().into_owned())
    );
    assert_eq!(
        ctx.chk(),
        CheckReport {
            ok: true,
            diagnostics: vec![]
        }
    );
}

/// C2 binary_valid_exit_0
#[test]
fn binary_valid_exit_0() {
    let ctx = Ctx::live();
    ctx.valid();
    let out = ctx.bin_ok();
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(out.stdout, b"{\"ok\":true,\"diagnostics\":[]}\n");
    assert!(out.stderr.is_empty());
}

/// C3 binary_errors_on_stdout_exit_1
#[test]
fn binary_errors_on_stdout_exit_1() {
    let ctx = Ctx::live();
    ctx.valid();
    ctx.set_slides(&["overview", "arch", "step-one", "step-one"]);
    let out = ctx.bin_ok();
    assert_eq!(out.status.code(), Some(1));
    assert!(out.stderr.is_empty());
    assert_eq!(
        String::from_utf8(out.stdout).unwrap(),
        "{\"ok\":false,\"diagnostics\":[{\"severity\":\"error\",\"file\":\"guide.yaml\",\"field\":\"slides[3]\",\"code\":\"duplicate_slide_id\",\"message\":\"slide `step-one` is listed more than once (first at slides[2]); list each slide once\"}]}\n"
    );
}

/// C4 binary_warning_only_exit_0
#[test]
fn binary_warning_only_exit_0() {
    let ctx = Ctx::live();
    ctx.valid();
    ctx.put("extra", &extra("extra"));
    let out = ctx.bin_ok();
    assert_eq!(out.status.code(), Some(0));
    assert!(out.stderr.is_empty());
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(
        v,
        serde_json::json!({"ok": true, "diagnostics": [{
            "severity": "warning", "file": "slides/extra.md", "field": "",
            "code": "unlisted_slide",
            "message": "slides/extra.md is not listed in guide.yaml's `slides`; list it or delete it"
        }]})
    );
}

/// C5 binary_run_unknown
#[test]
fn binary_run_unknown() {
    let ctx = Ctx::new();
    let out = ctx.bin(&ctx.clone, &ctx.draft, "r_nope");
    assert_eq!(out.status.code(), Some(1));
    assert!(out.stdout.is_empty());
    let v = stderr_json(&out);
    assert_eq!(v["code"], "run_unknown");
    assert!(v["message"].as_str().unwrap().contains("r_nope"));
}

/// C6 binary_run_stopped_first
#[test]
fn binary_run_stopped_first() {
    let ctx = Ctx::live();
    ctx.valid();
    runs::stop_run(
        &mut ctx.store(),
        &RunId::new(ctx.run.clone()),
        StopReason::End,
    )
    .unwrap();
    let fresh = TempDir::new().unwrap();
    for draft in [ctx.draft.clone(), fresh.path().join("nothing")] {
        let out = ctx.bin(&ctx.clone, &draft, &ctx.run);
        assert_eq!(out.status.code(), Some(1));
        assert!(out.stdout.is_empty());
        assert_eq!(stderr_json(&out)["code"], "run_stopped");
    }
}

/// C7 lapsed_lease_is_run_stopped
#[test]
fn lapsed_lease_is_run_stopped() {
    let ctx = Ctx::new();
    ctx.valid();
    ctx.clock.advance(LEASE + Duration::from_secs(1));
    let err = ctx.check_path(&ctx.draft).unwrap_err();
    assert_eq!(err.code(), "run_stopped");
    assert_eq!(
        ctx.query(&format!("SELECT state FROM runs WHERE id = '{}'", ctx.run)),
        vec![vec!["expired".to_string()]]
    );
}

/// C8 heartbeat_renews_lease
#[test]
fn heartbeat_renews_lease() {
    let ctx = Ctx::new();
    ctx.valid();
    ctx.clock.advance(Duration::from_secs(600));
    assert!(ctx.chk().ok);
    assert_eq!(
        ctx.query(&format!(
            "SELECT lease_until FROM runs WHERE id = '{}'",
            ctx.run
        )),
        vec![vec![ctx.clock.now().saturating_add(LEASE).to_rfc3339()]]
    );
}

// ---------------------------------------------------------------- A1

/// C9 draft_unknown_outside_store
#[test]
fn draft_unknown_outside_store() {
    let ctx = Ctx::live();
    ctx.valid();
    let tmp = TempDir::new().unwrap();
    let copy = tmp.path().join("copy");
    fs::create_dir_all(copy.join("slides")).unwrap();
    fs::copy(ctx.draft.join("guide.yaml"), copy.join("guide.yaml")).unwrap();
    let out = ctx.bin(&ctx.clone, &copy, &ctx.run);
    assert_eq!(out.status.code(), Some(1));
    assert!(out.stdout.is_empty());
    let v = stderr_json(&out);
    assert_eq!(v["code"], "draft_unknown");
    assert_eq!(v["message"], draft_unknown_msg(&copy, &ctx.run));
}

/// C10 draft_of_other_run
#[test]
fn draft_of_other_run() {
    let ctx = Ctx::new();
    let da = ctx.draft.clone();
    let run_b = ctx.start_run(true);
    assert_err(
        ctx.check_in(&run_b, &da),
        "draft_unknown",
        &draft_unknown_msg(&da, &run_b),
    );
    let db = ctx.gn(&run_b);
    let text = fs::read_to_string(db.draft.join("guide.yaml"))
        .unwrap()
        .replace("slides: []", "slides: [overview, arch, step-one]");
    fs::write(db.draft.join("guide.yaml"), text).unwrap();
    let put = |id: &str, t: String| {
        fs::write(db.draft.join("slides").join(format!("{id}.md")), t).unwrap()
    };
    put(
        "overview",
        slide("overview", "overview", "Overview", &[], &[], "Overview."),
    );
    put(
        "arch",
        slide("arch", "architecture", "The shape", &[], &[], "Shape."),
    );
    put("step-one", step("step-one", &[HANDLER, CTXB], &[FLAG5]));
    let report = ctx.check_in(&run_b, &db.draft).unwrap();
    assert!(report.ok, "{:?}", report.diagnostics);
}

/// C11 symlink_to_draft_is_unknown
#[test]
fn symlink_to_draft_is_unknown() {
    let ctx = Ctx::new();
    let tmp = TempDir::new().unwrap();
    let link = canon(tmp.path()).join("link");
    symlink(&ctx.draft, &link).unwrap();
    assert_err(
        ctx.check_path(&link),
        "draft_unknown",
        &draft_unknown_msg(&link, &ctx.run),
    );
}

/// C12 path_forms_accepted
#[test]
fn path_forms_accepted() {
    let ctx = Ctx::new();
    ctx.valid();
    let abs = ctx.draft.clone();
    let ups = ctx.clone.components().count() - 1;
    let rel = PathBuf::from(format!(
        "{}{}",
        "../".repeat(ups),
        abs.to_str().unwrap().trim_start_matches('/')
    ));
    let tmp = TempDir::new().unwrap();
    let alias = canon(tmp.path()).join("alias");
    symlink(&ctx.home, &alias).unwrap();
    let id = &ctx.out.draft_id;
    let through = alias.join(format!("prs/github.com/octo/widget/7/drafts/{id}"));
    let forms = [
        abs.clone(),
        PathBuf::from(format!("{}/", abs.display())),
        PathBuf::from(format!("{}/.", abs.display())),
        rel,
        through,
    ];
    for form in forms {
        let report = ctx
            .check_path(&form)
            .unwrap_or_else(|x| panic!("{form:?}: {x}"));
        assert_eq!(
            report,
            CheckReport {
                ok: true,
                diagnostics: vec![]
            },
            "{form:?}"
        );
    }
    assert_eq!(
        draft_path(&abs, ctx.fx.git(&ctx.clone).dir()),
        Some(abs.to_string_lossy().into_owned())
    );
}

/// C13 draft_root_symlink_is_diagnostic
#[test]
fn draft_root_symlink_is_diagnostic() {
    let ctx = Ctx::new();
    ctx.valid();
    let real = PathBuf::from(format!("{}.real", ctx.draft.display()));
    fs::rename(&ctx.draft, &real).unwrap();
    symlink(&real, &ctx.draft).unwrap();
    let report = ctx.chk();
    assert!(!report.ok);
    let expected = mapped(&ctx.draft);
    assert_eq!(expected.len(), 1);
    assert_eq!(expected[0].code, "symlink_not_allowed");
    assert_eq!(report.diagnostics, expected);
}

/// C14 dotdot_last_is_unknown
#[test]
fn dotdot_last_is_unknown() {
    let ctx = Ctx::new();
    let p = ctx.draft.join("slides").join("..");
    assert_err(
        ctx.check_path(&p),
        "draft_unknown",
        &draft_unknown_msg(&p, &ctx.run),
    );
}

/// C15 binary_store_bound
#[test]
fn binary_store_bound() {
    let ctx = Ctx::live();
    ctx.valid();
    let other = canon(&ctx.fx.clone("other"));
    let out = ctx.bin(&other, &ctx.draft, &ctx.run);
    assert_eq!(out.status.code(), Some(1));
    assert!(out.stdout.is_empty());
    let v = stderr_json(&out);
    assert_eq!(v["code"], "store_bound");
    let bound = canon(&ctx.clone.join(".git")).display().to_string();
    let current = canon(&other.join(".git")).display().to_string();
    assert_eq!(
        v["message"],
        format!(
            "the store for octo/widget#7 is bound to the clone whose git dir is {bound}; this clone's git dir is {current}. Run prguide check from the bound clone"
        )
    );
}

// ---------------------------------------------------------------- parse

/// C16 parse_errors_pass_through
#[test]
fn parse_errors_pass_through() {
    let ctx = Ctx::new();
    ctx.valid();
    ctx.put(
        "step-one",
        &slide("step-one", "step", "Step one", &[], &[], "<b>x</b>"),
    );
    fs::write(ctx.draft.join("notes.txt"), "n").unwrap();
    ctx.put(
        "arch",
        &slide("wrong", "architecture", "The shape", &[], &[], "Shape."),
    );
    let report = ctx.chk();
    let expected = mapped(&ctx.draft);
    assert_eq!(expected.len(), 3);
    assert_eq!(report.diagnostics, expected);
    assert!(!report.ok);
}

/// C17 manifest_error_skips_deck_only
#[test]
fn manifest_error_skips_deck_only() {
    let ctx = Ctx::new();
    ctx.with_step(
        &[
            HANDLER,
            CTXB,
            "{ id: handler, kind: context, path: ctx.txt, start: 1, end: 2 }",
        ],
        &[FLAG5],
    );
    ctx.put("extra", &extra("extra"));
    ctx.set_yaml(&(ctx.yaml() + "bogus: 1\n"));
    let loaded = mapped(&ctx.draft);
    assert_eq!(loaded.len(), 1);
    assert_eq!(loaded[0].code, "field_unknown");
    assert_eq!(loaded[0].file, "guide.yaml");
    assert_eq!(
        ctx.diags(),
        vec![
            loaded[0].clone(),
            dup_block("slides/step-one.md", 2, "handler", 0)
        ]
    );
}

// ---------------------------------------------------------------- provenance

fn prov_msg(field: &str) -> String {
    format!(
        "guide.yaml's provenance differs from what prguide guide new wrote (first difference: `{field}`); restore it, and edit only `slides`"
    )
}

/// C18 provenance_each_leaf
#[test]
fn provenance_each_leaf() {
    let ctx = Ctx::new();
    ctx.valid();
    let original = ctx.yaml();
    let base = ctx.out.snapshot.base.clone();
    let head = ctx.out.snapshot.head.clone();
    let commits: Vec<String> = ctx.out.commits.iter().map(|c| c.sha.clone()).collect();
    assert_eq!(commits.len(), 2);
    let z = |c: char| c.to_string().repeat(40);
    // (leaf, edit)
    type Edit<'a> = Box<dyn Fn(&Ctx) + 'a>;
    let cases: Vec<(&str, Edit)> = vec![
        (
            "pr.host",
            Box::new(|c| c.tamper("pr:", "github.com", "github.co")),
        ),
        (
            "pr.repo",
            Box::new(|c| c.tamper("pr:", "octo/widget", "octo/widgets")),
        ),
        (
            "pr.number",
            Box::new(|c| c.tamper("pr:", "number: 7", "number: 8")),
        ),
        (
            "snapshot.base",
            Box::new(|c| c.tamper("snapshot:", &base, &z('1'))),
        ),
        (
            "snapshot.head",
            Box::new(|c| c.tamper("snapshot:", &head, &z('2'))),
        ),
        (
            "selection.kind",
            Box::new(|c| c.tamper("  kind:", "full", "commits")),
        ),
        (
            "selection.base",
            Box::new(|c| c.tamper("  base:", &base, &z('3'))),
        ),
        (
            "selection.head",
            Box::new(|c| c.tamper("  head:", &head, &z('4'))),
        ),
        (
            "selection.commits",
            Box::new(|c| {
                let text = c.yaml().replace(&format!("    - \"{}\"\n", commits[1]), "");
                c.set_yaml(&text);
            }),
        ),
        (
            "selection.commits",
            Box::new(|c| {
                let (l0, l1) = (
                    format!("    - \"{}\"\n", commits[0]),
                    format!("    - \"{}\"\n", commits[1]),
                );
                let text = c.yaml().replace(&format!("{l0}{l1}"), &format!("{l1}{l0}"));
                c.set_yaml(&text);
            }),
        ),
        (
            "snapshot.head",
            Box::new(|c| c.tamper("snapshot:", &head, &head.to_uppercase())),
        ),
        (
            "snapshot.head",
            Box::new(|c| {
                c.tamper("snapshot:", &head, &z('5'));
                c.tamper("  head:", &head, &z('5'));
            }),
        ),
    ];
    for (leaf, edit) in cases {
        ctx.set_yaml(&original);
        edit(&ctx);
        assert_ne!(ctx.yaml(), original, "{leaf}");
        assert_eq!(
            ctx.diags(),
            vec![e("guide.yaml", leaf, "provenance_changed", &prov_msg(leaf))],
            "{leaf}"
        );
    }
}

/// C19 provenance_form_only
#[test]
fn provenance_form_only() {
    let ctx = Ctx::new();
    ctx.valid();
    let o = &ctx.out;
    let text = format!(
        "slides:\n  - overview\n  - arch\n  - step-one\nselection:\n  commits:\n    - '{}'\n    - '{}'\n  head: '{}'\n  base: '{}'\n  kind: 'full'\nsnapshot:\n  head: '{}'\n  base: '{}'\npr:\n  number: \"7\"\n  repo: 'octo/widget'\n  host: 'github.com'\nformat: 'prguide/v1'\n",
        o.commits[0].sha,
        o.commits[1].sha,
        o.selection.head,
        o.selection.base,
        o.snapshot.head,
        o.snapshot.base
    );
    ctx.set_yaml(&text);
    assert_eq!(
        ctx.chk(),
        CheckReport {
            ok: true,
            diagnostics: vec![]
        }
    );
}

/// C20 blocks_use_row_not_yaml
#[test]
fn blocks_use_row_not_yaml() {
    let ctx = Ctx::new();
    let fresh = "{ id: fresh, kind: context, path: new.txt, start: 1, end: 4 }";
    ctx.with_step(&[HANDLER, CTXB, fresh], &[FLAG5]);
    let base = ctx.out.snapshot.base.clone();
    let head = ctx.out.snapshot.head.clone();
    ctx.tamper("snapshot:", &head, &base);
    let block = cblk("fresh", "new.txt", 1, 4);
    let warning = ctx.anchor_warning("slides/step-one.md", 2, &block);
    assert_eq!(
        ctx.diags(),
        vec![
            e(
                "guide.yaml",
                "snapshot.head",
                "provenance_changed",
                &prov_msg("snapshot.head")
            ),
            warning
        ]
    );
}

// ---------------------------------------------------------------- deck

/// C21 empty_slides
#[test]
fn empty_slides() {
    let ctx = Ctx::new();
    assert_eq!(
        ctx.diags(),
        vec![e(
            "guide.yaml",
            "slides",
            "slide_file_missing",
            "`slides` is empty: list the deck's slide ids in order, the overview first"
        )]
    );
}

/// C22 listed_file_missing
#[test]
fn listed_file_missing() {
    let ctx = Ctx::new();
    ctx.valid();
    ctx.set_slides(&["overview", "arch", "step-one", "ghost"]);
    assert_eq!(ctx.diags(), vec![missing_slide(3, "ghost")]);
}

/// C23 missing_first_and_no_step_gate
#[test]
fn missing_first_and_no_step_gate() {
    let ctx = Ctx::new();
    ctx.valid();
    ctx.set_slides(&["ghost", "overview", "arch"]);
    ctx.rm("step-one");
    assert_eq!(ctx.diags(), vec![missing_slide(0, "ghost")]);
}

/// C24 duplicate_slide_id
#[test]
fn duplicate_slide_id() {
    let ctx = Ctx::new();
    ctx.valid();
    ctx.set_slides(&[
        "overview", "arch", "step-one", "step-one", "arch", "step-one",
    ]);
    assert_eq!(
        ctx.diags(),
        vec![
            dup_slide(3, "step-one", 2),
            dup_slide(4, "arch", 1),
            dup_slide(5, "step-one", 2)
        ]
    );
}

/// C25 overview_first
#[test]
fn overview_first() {
    let ctx = Ctx::new();
    ctx.valid();
    ctx.set_slides(&["step-one", "overview", "arch"]);
    let msg = |id: &str, kind: &str| {
        e(
            "guide.yaml",
            "slides[0]",
            "overview_first",
            &format!("the first slide must be the overview; `{id}` has kind {kind}"),
        )
    };
    assert_eq!(ctx.diags(), vec![msg("step-one", "step")]);
    ctx.set_slides(&["arch", "overview", "step-one"]);
    assert_eq!(ctx.diags(), vec![msg("arch", "architecture")]);
}

/// C26 broken_first_slide_no_kind_rules
#[test]
fn broken_first_slide_no_kind_rules() {
    let ctx = Ctx::new();
    ctx.valid();
    ctx.put("overview", "no front matter\n");
    let expected = mapped(&ctx.draft);
    assert_eq!(expected.len(), 1);
    assert_eq!(expected[0].code, "front_matter_missing");
    assert_eq!(ctx.diags(), expected);
}

/// C27 no_steps_counts_listed_only
#[test]
fn no_steps_counts_listed_only() {
    let ctx = Ctx::new();
    ctx.valid();
    ctx.set_slides(&["overview", "arch"]);
    let report = ctx.chk();
    assert_eq!(
        report.diagnostics,
        vec![
            e(
                "guide.yaml",
                "slides",
                "no_steps",
                "the deck has no step slide; add at least one slide with kind: step"
            ),
            unlisted("step-one")
        ]
    );
    assert!(!report.ok);
}

/// C28 no_architecture_warning
#[test]
fn no_architecture_warning() {
    let ctx = Ctx::new();
    ctx.valid();
    ctx.set_slides(&["overview", "step-one"]);
    let report = ctx.chk();
    assert_eq!(
        report.diagnostics,
        vec![
            w(
                "guide.yaml",
                "slides",
                "no_architecture",
                "the deck has no architecture slide"
            ),
            unlisted("arch")
        ]
    );
    assert!(report.ok);
}

/// C29 unlisted_includes_broken
#[test]
fn unlisted_includes_broken() {
    let ctx = Ctx::new();
    ctx.valid();
    ctx.put("extra", &extra("extra"));
    ctx.put("extra2", "no front matter\n");
    let loaded = mapped(&ctx.draft);
    assert_eq!(loaded.len(), 1);
    let report = ctx.chk();
    assert_eq!(
        report.diagnostics,
        vec![unlisted("extra"), loaded[0].clone(), unlisted("extra2")]
    );
    assert_eq!(loaded[0].file, "slides/extra2.md");
    assert!(!report.ok);
}

/// C30 unlisted_slide_still_checked
#[test]
fn unlisted_slide_still_checked() {
    let ctx = Ctx::new();
    ctx.valid();
    let a = "{ id: a, kind: context, path: ctx.txt, start: 1, end: 2 }";
    ctx.put("extra", &step("extra", &[a, a], &[]));
    assert_eq!(
        ctx.diags(),
        vec![unlisted("extra"), dup_block("slides/extra.md", 1, "a", 0)]
    );
}

// ---------------------------------------------------------------- slides

/// C31 duplicate_block_id
#[test]
fn duplicate_block_id() {
    let ctx = Ctx::new();
    let c2 = "{ id: c2, kind: context, path: ctx.txt, start: 2, end: 3 }";
    ctx.with_step(&[HANDLER, CTXB, HANDLER, c2, HANDLER], &[FLAG5]);
    assert_eq!(
        ctx.diags(),
        vec![
            dup_block("slides/step-one.md", 2, "handler", 0),
            dup_block("slides/step-one.md", 4, "handler", 0)
        ]
    );
}

/// C32 flag_block_unknown
#[test]
fn flag_block_unknown() {
    let ctx = Ctx::new();
    ctx.with_step(
        &[HANDLER, CTXB],
        &[FLAG5, "{ at: { block: nope }, text: \"x\" }"],
    );
    assert_eq!(
        ctx.diags(),
        vec![e(
            "slides/step-one.md",
            "flags[1].at.block",
            "flag_block_unknown",
            "the flag points at block `nope`, which this slide does not have"
        )]
    );
}

fn flag(block: &str, line: Option<u32>) -> String {
    match line {
        Some(l) => format!("{{ at: {{ block: {block}, line: {l} }}, text: \"t\" }}"),
        None => format!("{{ at: {{ block: {block} }}, text: \"t\" }}"),
    }
}

/// C33 flag_line_bounds
#[test]
fn flag_line_bounds() {
    let ctx = Ctx::new();
    let flags = [
        flag("handler", Some(3)),
        flag("handler", Some(4)),
        flag("handler", Some(6)),
        flag("handler", Some(7)),
        flag("handler", None),
        flag("ctx", Some(0)),
    ];
    let refs: Vec<&str> = flags.iter().map(String::as_str).collect();
    ctx.with_step(&[HANDLER, CTXB], &refs);
    assert_eq!(
        ctx.diags(),
        vec![
            out_of_range(0, 3, "handler", 4, 6),
            out_of_range(3, 7, "handler", 4, 6),
            out_of_range(5, 0, "ctx", 1, 3)
        ]
    );
}

/// C34 flag_line_on_stub_block
#[test]
fn flag_line_on_stub_block() {
    let ctx = Ctx::new();
    let blob = "{ id: blob, kind: diff, path: bin.dat, side: new }";
    let f0 = flag("blob", None);
    let f1 = flag("blob", Some(1));
    ctx.with_step(&[HANDLER, CTXB, blob], &[&f0, &f1]);
    assert_eq!(
        ctx.diags(),
        vec![e(
            "slides/step-one.md",
            "flags[1].at.line",
            "flag_line_out_of_range",
            "block `blob` has no line range, so a flag on it cannot name a line; remove `line`"
        )]
    );
}

/// C35 flag_uses_first_block
#[test]
fn flag_uses_first_block() {
    let ctx = Ctx::new();
    let again = "{ id: handler, kind: context, path: ctx.txt, start: 1, end: 2 }";
    let f0 = flag("handler", Some(5));
    let f1 = flag("handler", Some(1));
    ctx.with_step(&[HANDLER, CTXB, again], &[&f0, &f1]);
    assert_eq!(
        ctx.diags(),
        vec![
            dup_block("slides/step-one.md", 2, "handler", 0),
            out_of_range(1, 1, "handler", 4, 6)
        ]
    );
}

// ---------------------------------------------------------------- resolution

/// C36 anchor_errors_pass_through
#[test]
fn anchor_errors_pass_through() {
    let ctx = Ctx::new();
    let d1 = "{ id: d1, kind: diff, path: missing.txt, side: new, start: 1, end: 1 }";
    let d2 = "{ id: d2, kind: diff, path: src/lib.rs, side: new, start: 4, end: 99 }";
    let d3 = "{ id: d3, kind: diff, path: src/lib.rs, side: new, start: 9, end: 10 }";
    let d4 = "{ id: d4, kind: diff, path: old.txt, side: new, start: 1, end: 1 }";
    ctx.with_step(&[HANDLER, d1, d2, d3, d4], &[FLAG5]);
    ctx.put(
        "overview",
        &slide(
            "overview",
            "overview",
            "Overview",
            &["{ id: z, kind: context, path: ctx.txt, start: 0, end: 1 }"],
            &[],
            "Overview.",
        ),
    );
    let s = "slides/step-one.md";
    let expected = vec![
        ctx.anchor("slides/overview.md", 0, &cblk("z", "ctx.txt", 0, 1)),
        ctx.anchor(s, 1, &dblk("d1", "missing.txt", 1, 1)),
        ctx.anchor(s, 2, &dblk("d2", "src/lib.rs", 4, 99)),
        ctx.anchor(s, 3, &dblk("d3", "src/lib.rs", 9, 10)),
        ctx.anchor(
            s,
            4,
            &CodeBlock::Diff {
                id: "d4".into(),
                path: "old.txt".into(),
                side: Side::New,
                start: Some(1),
                end: Some(1),
            },
        ),
    ];
    let fields: Vec<(&str, &str)> = expected
        .iter()
        .map(|d| (d.field.as_str(), d.code))
        .collect();
    assert_eq!(
        fields,
        vec![
            ("code[0].start", "lines_out_of_range"),
            ("code[1].path", "path_not_in_diff"),
            ("code[2].end", "lines_out_of_range"),
            ("code[3].start", "no_hunks_in_range"),
            ("code[4].side", "wrong_side"),
        ]
    );
    assert_eq!(ctx.diags(), expected);
}

/// C37 context_overlap_warning
#[test]
fn context_overlap_warning() {
    let ctx = Ctx::new();
    let ov = "{ id: ov, kind: context, path: src/lib.rs, start: 4, end: 6 }";
    ctx.with_step(&[HANDLER, CTXB, ov], &[FLAG5]);
    let expected = ctx.anchor_warning("slides/step-one.md", 2, &cblk("ov", "src/lib.rs", 4, 6));
    assert_eq!(expected.field, "code[2].start");
    assert_eq!(expected.code, "context_overlaps_hunk");
    let report = ctx.chk();
    assert_eq!(report.diagnostics, vec![expected]);
    assert!(report.ok);
}

/// C38 worktree_not_read
#[test]
fn worktree_not_read() {
    let ctx = Ctx::new();
    let far = "{ id: far, kind: context, path: ctx.txt, start: 40, end: 45 }";
    ctx.with_step(&[HANDLER, CTXB, far], &[FLAG5]);
    fs::write(ctx.out.worktree.join("src/lib.rs"), "x\n").unwrap();
    let fifty: String = (1..=50).map(|i| format!("c{i}\n")).collect();
    fs::write(ctx.out.worktree.join("ctx.txt"), fifty).unwrap();
    let diags = ctx.diags();
    assert_eq!(diags.len(), 1, "{diags:?}");
    assert_eq!(diags[0].field, "code[2].end");
    assert_eq!(diags[0].code, "lines_out_of_range");
    assert_eq!(
        diags,
        vec![ctx.anchor("slides/step-one.md", 2, &cblk("far", "ctx.txt", 40, 45))]
    );
}

/// C39 git_error_aborts
#[test]
fn git_error_aborts() {
    let zeros = "0".repeat(40);
    // (a) only snapshot.head: the error comes from resolve.
    let ctx = Ctx::new();
    ctx.valid();
    let (base, head) = (ctx.out.snapshot.base.clone(), ctx.out.snapshot.head.clone());
    let from = format!(r#""snapshot":{{"base":"{base}","head":"{head}"}}"#);
    let to = format!(r#""snapshot":{{"base":"{base}","head":"{zeros}"}}"#);
    ctx.set_row(|j| {
        assert!(j.contains(&from));
        j.replace(&from, &to)
    });
    ctx.tamper("snapshot:", &head, &zeros);
    let err = ctx.check_path(&ctx.draft).unwrap_err();
    assert_eq!(err.code(), "git_failed", "{err}");
    // (b) snapshot.head and selection.head: the error comes from BlockContext::new.
    let ctx = Ctx::new();
    ctx.valid();
    let head = ctx.out.snapshot.head.clone();
    ctx.set_row(|j| j.replace(&head, &zeros));
    ctx.set_yaml(&ctx.yaml().replace(&head, &zeros));
    let err = ctx.check_path(&ctx.draft).unwrap_err();
    assert_eq!(err.code(), "git_failed", "{err}");
}

// ---------------------------------------------------------------- order

/// C40 diagnostics_sorted
#[test]
fn diagnostics_sorted() {
    let ctx = Ctx::new();
    ctx.valid();
    ctx.set_slides(&["overview", "arch", "step-one", "ghost", "ghost"]);
    let blocks: Vec<String> = (0..=10)
        .map(|i| match i {
            2 => {
                "{ id: b2, kind: diff, path: missing.txt, side: new, start: 1, end: 1 }".to_owned()
            }
            10 => "{ id: b10, kind: context, path: ctx.txt, start: 1, end: 99 }".to_owned(),
            _ => format!("{{ id: b{i}, kind: context, path: ctx.txt, start: 1, end: 1 }}"),
        })
        .collect();
    let refs: Vec<&str> = blocks.iter().map(String::as_str).collect();
    ctx.put("step-one", &step("step-one", &refs, &[]));
    ctx.put("aaa", &extra("aaa"));
    let a = "{ id: a, kind: context, path: ctx.txt, start: 1, end: 1 }";
    ctx.put("zed", &step("zed", &[a, a], &[]));
    fs::write(ctx.draft.join("a.txt"), "x").unwrap();
    let loaded = mapped(&ctx.draft);
    assert_eq!(loaded.len(), 1);
    let s = "slides/step-one.md";
    let expected = vec![
        loaded[0].clone(),
        missing_slide(3, "ghost"),
        dup_slide(4, "ghost", 3),
        missing_slide(4, "ghost"),
        unlisted("aaa"),
        ctx.anchor(
            s,
            2,
            &CodeBlock::Diff {
                id: "b2".into(),
                path: "missing.txt".into(),
                side: Side::New,
                start: Some(1),
                end: Some(1),
            },
        ),
        ctx.anchor(s, 10, &cblk("b10", "ctx.txt", 1, 99)),
        unlisted("zed"),
        dup_block("slides/zed.md", 1, "a", 0),
    ];
    let keys: Vec<(&str, &str, &str)> = expected
        .iter()
        .map(|d| (d.file.as_str(), d.field.as_str(), d.code))
        .collect();
    assert_eq!(
        keys,
        vec![
            ("a.txt", "", "unexpected_file"),
            ("guide.yaml", "slides[3]", "slide_file_missing"),
            ("guide.yaml", "slides[4]", "duplicate_slide_id"),
            ("guide.yaml", "slides[4]", "slide_file_missing"),
            ("slides/aaa.md", "", "unlisted_slide"),
            ("slides/step-one.md", "code[2].path", "path_not_in_diff"),
            ("slides/step-one.md", "code[10].end", "lines_out_of_range"),
            ("slides/zed.md", "", "unlisted_slide"),
            ("slides/zed.md", "code[1].id", "duplicate_block_id"),
        ]
    );
    assert_eq!(ctx.diags(), expected);
}

// ---------------------------------------------------------------- hostile

const HOSTILE_FLAG: &str =
    "{ at: { block: \"\\e[2J\\x85\u{202e}\\x7f\\u009b\" }, text: \"Hostile.\" }";

/// C41 binary_hostile_stdout_escaped
#[test]
fn binary_hostile_stdout_escaped() {
    let ctx = Ctx::live();
    ctx.with_step(&[HANDLER, CTXB], &[FLAG5, HOSTILE_FLAG]);
    let name = "x\u{9b}[31m\u{202e}\u{2028}.txt";
    fs::write(ctx.draft.join(name), "x").unwrap();
    let out = ctx.bin_ok();
    assert_eq!(out.status.code(), Some(1));
    no_hostile_bytes(&out.stdout);
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let list = v["diagnostics"].as_array().unwrap();
    let unexpected = list
        .iter()
        .find(|d| d["code"] == "unexpected_file")
        .unwrap();
    assert_eq!(unexpected["file"], name);
    let flagged = list
        .iter()
        .find(|d| d["code"] == "flag_block_unknown")
        .unwrap();
    assert!(
        flagged["message"]
            .as_str()
            .unwrap()
            .contains("\u{1b}[2J\u{85}\u{202e}\u{7f}\u{9b}")
    );
}

/// C42 binary_hostile_stderr_escaped
#[test]
fn binary_hostile_stderr_escaped() {
    let ctx = Ctx::live();
    let tmp = TempDir::new().unwrap();
    let arg = PathBuf::from(format!("{}/d\u{9b}\u{202e}\u{2028}", tmp.path().display()));
    let out = ctx.bin(&ctx.clone, &arg, &ctx.run);
    assert_eq!(out.status.code(), Some(1));
    assert!(out.stdout.is_empty());
    no_hostile_bytes(&out.stderr);
    let v = stderr_json(&out);
    assert_eq!(v["code"], "draft_unknown");
    assert_eq!(v["message"], draft_unknown_msg(&arg, &ctx.run));
}

// ---------------------------------------------------------------- read-only

type Tree = BTreeMap<String, (Vec<u8>, Option<SystemTime>)>;

fn tree(root: &Path, skip: &[PathBuf], mtimes: bool) -> Tree {
    fn walk(root: &Path, dir: &Path, skip: &[PathBuf], mtimes: bool, out: &mut Tree) {
        for entry in fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if skip.contains(&path) {
                continue;
            }
            let meta = fs::symlink_metadata(&path).unwrap();
            let key = path
                .strip_prefix(root)
                .unwrap()
                .to_string_lossy()
                .into_owned();
            let time = mtimes.then(|| meta.modified().unwrap());
            if meta.is_dir() {
                out.insert(key, (Vec::new(), time));
                walk(root, &path, skip, mtimes, out);
            } else if meta.file_type().is_symlink() {
                let t = fs::read_link(&path).unwrap().to_string_lossy().into_owned();
                out.insert(key, (t.into_bytes(), time));
            } else {
                out.insert(key, (fs::read(&path).unwrap(), time));
            }
        }
    }
    let mut out = Tree::new();
    walk(root, root, skip, mtimes, &mut out);
    out
}

#[derive(Debug, PartialEq)]
struct Snap {
    tables: Vec<(String, Vec<Vec<String>>)>,
    store: Tree,
    draft: Tree,
    worktree: Tree,
    refs: String,
}

fn snap(ctx: &Ctx) -> Snap {
    let names: Vec<String> = ctx
        .query(
            "SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%' \
             AND name <> 'check_audit' ORDER BY name",
        )
        .into_iter()
        .map(|r| r[0].clone())
        .collect();
    let mut tables = Vec::new();
    for t in names {
        let cols: Vec<String> = ctx
            .query(&format!("PRAGMA table_info(\"{t}\")"))
            .into_iter()
            .map(|r| r[1].clone())
            .collect();
        let select: Vec<String> = cols
            .iter()
            .map(|c| {
                if t == "runs" && c == "lease_until" {
                    "'-'".to_owned()
                } else {
                    format!("\"{c}\"")
                }
            })
            .collect();
        let rows = ctx.query(&format!(
            "SELECT {} FROM \"{t}\" ORDER BY rowid",
            select.join(", ")
        ));
        tables.push((t, rows));
    }
    let store_dir = ctx.home.join("prs/github.com/octo/widget/7");
    let skip: Vec<PathBuf> = [
        "store.db",
        "store.db-wal",
        "store.db-shm",
        "drafts",
        "worktree",
    ]
    .iter()
    .map(|n| store_dir.join(n))
    .collect();
    Snap {
        tables,
        store: tree(&store_dir, &skip, false),
        draft: tree(&ctx.draft, &[], true),
        worktree: tree(&ctx.out.worktree, &[], true),
        refs: ctx.fx.run(&ctx.clone, ["for-each-ref"]),
    }
}

fn install_audit(ctx: &Ctx) {
    ctx.store()
        .tx(|tx| -> Result<(), StoreError> {
            tx.execute_batch("CREATE TABLE check_audit(t TEXT)")?;
            let names: Vec<String> = tx
                .prepare(
                    "SELECT name FROM sqlite_master WHERE type = 'table' \
                     AND name NOT LIKE 'sqlite_%' AND name <> 'check_audit'",
                )?
                .query_map([], |r| r.get(0))?
                .collect::<Result<_, _>>()?;
            for t in names {
                let cols: Vec<String> = tx
                    .prepare(&format!("PRAGMA table_info(\"{t}\")"))?
                    .query_map([], |r| r.get(1))?
                    .collect::<Result<_, _>>()?;
                let update = if t == "runs" {
                    let others: Vec<String> = cols
                        .iter()
                        .filter(|c| *c != "lease_until")
                        .map(|c| format!("\"{c}\""))
                        .collect();
                    format!("UPDATE OF {}", others.join(", "))
                } else {
                    "UPDATE".to_owned()
                };
                for (n, when) in [("i", "INSERT"), ("d", "DELETE"), ("u", update.as_str())] {
                    tx.execute_batch(&format!(
                        "CREATE TRIGGER \"ca_{t}_{n}\" AFTER {when} ON \"{t}\" \
                         BEGIN INSERT INTO check_audit VALUES ('{t}'); END;"
                    ))?;
                }
            }
            Ok(())
        })
        .unwrap();
}

/// C43 check_is_read_only
#[test]
fn check_is_read_only() {
    for bad in [false, true] {
        let ctx = Ctx::new();
        ctx.valid();
        if bad {
            let d1 = "{ id: d1, kind: diff, path: missing.txt, side: new, start: 1, end: 1 }";
            let d2 = "{ id: d2, kind: diff, path: src/lib.rs, side: new, start: 4, end: 99 }";
            let d3 = "{ id: d3, kind: diff, path: src/lib.rs, side: new, start: 9, end: 10 }";
            let d4 = "{ id: d4, kind: diff, path: old.txt, side: new, start: 1, end: 1 }";
            let flags = [
                flag("handler", Some(3)),
                flag("handler", Some(7)),
                flag("ctx", Some(0)),
            ];
            let refs: Vec<&str> = flags.iter().map(String::as_str).collect();
            ctx.with_step(&[HANDLER, CTXB, d1, d2, d3, d4], &refs);
            ctx.put(
                "overview",
                &slide(
                    "overview",
                    "overview",
                    "Overview",
                    &["{ id: z, kind: context, path: ctx.txt, start: 0, end: 1 }"],
                    &[],
                    "Overview.",
                ),
            );
        }
        install_audit(&ctx);
        let before = snap(&ctx);
        let report = ctx.chk();
        assert_eq!(report.ok, !bad);
        assert_eq!(before, snap(&ctx));
        assert_eq!(
            ctx.query("SELECT count(*) FROM check_audit"),
            vec![vec!["Integer(0)".to_string()]]
        );
    }
}

// ---------------------------------------------------------------- seam, unit

/// C44 architecture_without_diagram_passes
#[test]
fn architecture_without_diagram_passes() {
    let ctx = Ctx::new();
    ctx.valid();
    let ok = CheckReport {
        ok: true,
        diagnostics: vec![],
    };
    assert_eq!(ctx.chk(), ok);
    ctx.put(
        "arch",
        &slide(
            "arch",
            "architecture",
            "The shape",
            &[],
            &[],
            "Shape.\n\n```mermaid\nnot a diagram\n```",
        ),
    );
    assert_eq!(ctx.chk(), ok);
}

/// X1 row_provenance_unparsable_is_internal
#[test]
fn row_provenance_unparsable_is_internal() {
    let ctx = Ctx::new();
    ctx.valid();
    ctx.set_row(|_| "{".into());
    let err = ctx.check_path(&ctx.draft).unwrap_err();
    assert_eq!(err.code(), "internal");
    assert!(
        err.to_string()
            .starts_with("internal error: the drafts row's provenance_json does not parse: "),
        "{err}"
    );
}

/// X2 row_commits_kind_without_commits_is_internal
#[test]
fn row_commits_kind_without_commits_is_internal() {
    let ctx = Ctx::new();
    ctx.valid();
    ctx.set_row(|j| {
        let start = j.find("\"commits\":[").unwrap();
        let end = j[start..].find(']').unwrap() + start + 1;
        format!("{}\"commits\":[]{}", &j[..start], &j[end..])
            .replace("\"kind\":\"full\"", "\"kind\":\"commits\"")
    });
    assert_err(
        ctx.check_path(&ctx.draft),
        "internal",
        "internal error: the drafts row's provenance_json has selection kind commits but no commits",
    );
}

/// X3 stdout_write_failure_is_internal: stdout is a pipe with no reader, so the
/// report cannot be written; the error goes to stderr as `internal`, exit 1.
#[test]
fn stdout_write_failure_is_internal() {
    let ctx = Ctx::live();
    ctx.valid();
    let home = ctx.fx.path().join("home");
    // A pipe with no reader: every write fails with EPIPE.
    let (r, w) = std::io::pipe().unwrap();
    drop(r);
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_prguide"))
        .current_dir(&ctx.clone)
        .arg("check")
        .arg(&ctx.draft)
        .args(["--run", &ctx.run])
        .env("HOME", &home)
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", home.join(".gitconfig"))
        .env("PRGUIDE_HOME", &ctx.home)
        .stdout(std::process::Stdio::from(w))
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    let line = String::from_utf8(out.stderr).unwrap();
    assert_eq!(line.lines().count(), 1);
    let v: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
    assert_eq!(v["code"], "internal");
}

/// X4 missing_store_meta_is_internal: a store_meta row is gone.
#[test]
fn missing_store_meta_is_internal() {
    let ctx = Ctx::new();
    ctx.valid();
    ctx.store()
        .tx(|tx| -> Result<(), StoreError> {
            tx.execute("DELETE FROM store_meta WHERE key = 'repo'", ())?;
            Ok(())
        })
        .unwrap();
    let err = ctx.check_path(&ctx.draft).unwrap_err();
    assert_eq!(err.code(), "internal");
    assert_eq!(err.to_string(), "internal error: store_meta has no `repo`");
}

/// X5 missing_parent_is_draft_unknown: the draft's parent does not exist.
#[test]
fn missing_parent_is_draft_unknown() {
    let ctx = Ctx::new();
    ctx.valid();
    let tmp = TempDir::new().unwrap();
    let err = ctx.check_path(&tmp.path().join("nope/x")).unwrap_err();
    assert_eq!(err.code(), "draft_unknown");
}

// ---------------------------------------------------------------- review round 1

/// X6 unlisted_slide_blocks_are_resolved: a block that does not resolve on an
/// unlisted slide still fails the check.
#[test]
fn unlisted_slide_blocks_are_resolved() {
    let ctx = Ctx::new();
    ctx.valid();
    let far = "{ id: far, kind: context, path: ctx.txt, start: 40, end: 45 }";
    ctx.put("extra", &step("extra", &[far], &[]));
    let report = ctx.chk();
    assert!(!report.ok);
    assert_eq!(
        report.diagnostics,
        vec![
            unlisted("extra"),
            ctx.anchor("slides/extra.md", 0, &cblk("far", "ctx.txt", 40, 45))
        ]
    );
}

/// X7 provenance_bytes_only_difference: the row differs from guide.yaml's
/// provenance in bytes only, so no leaf differs and the field is empty.
#[test]
fn provenance_bytes_only_difference() {
    let ctx = Ctx::new();
    ctx.valid();
    ctx.set_row(|j| format!("{j} "));
    assert_eq!(
        ctx.diags(),
        vec![e("guide.yaml", "", "provenance_changed", &prov_msg(""))]
    );
}

/// X8 unindexed_field_sorts_before_indexed: `slides` sorts before `slides[0]`.
#[test]
fn unindexed_field_sorts_before_indexed() {
    let ctx = Ctx::new();
    ctx.valid();
    ctx.set_slides(&["step-one", "overview"]);
    assert_eq!(
        ctx.diags(),
        vec![
            w(
                "guide.yaml",
                "slides",
                "no_architecture",
                "the deck has no architecture slide"
            ),
            e(
                "guide.yaml",
                "slides[0]",
                "overview_first",
                "the first slide must be the overview; `step-one` has kind step"
            ),
            unlisted("arch"),
        ]
    );
}

/// X9 duplicate_block_is_still_resolved: a repeated block id is flagged and
/// the repeat is resolved too.
#[test]
fn duplicate_block_is_still_resolved() {
    let ctx = Ctx::new();
    let again = "{ id: handler, kind: diff, path: missing.txt, side: new, start: 1, end: 1 }";
    ctx.with_step(&[HANDLER, CTXB, again], &[FLAG5]);
    let f = "slides/step-one.md";
    assert_eq!(
        ctx.diags(),
        vec![
            dup_block(f, 2, "handler", 0),
            ctx.anchor(f, 2, &dblk("handler", "missing.txt", 1, 1)),
        ]
    );
}

/// X10 unknown_draft_beats_not_in_clone: from a directory outside any clone,
/// an unknown draft is reported as draft_unknown, not not_in_clone.
#[test]
fn unknown_draft_beats_not_in_clone() {
    let ctx = Ctx::live();
    ctx.valid();
    let tmp = TempDir::new().unwrap();
    let out = ctx.bin(tmp.path(), &tmp.path().join("nope"), &ctx.run);
    assert_eq!(out.status.code(), Some(1));
    assert!(out.stdout.is_empty());
    assert_eq!(stderr_json(&out)["code"], "draft_unknown");
}

/// X11 no_code_means_no_git: with no code block anywhere, unreadable heads
/// are never touched.
#[test]
fn no_code_means_no_git() {
    let ctx = Ctx::new();
    ctx.valid();
    ctx.put("step-one", &step("step-one", &[], &[]));
    let zeros = "0".repeat(40);
    let head = ctx.out.snapshot.head.clone();
    ctx.set_row(|j| j.replace(&head, &zeros));
    ctx.set_yaml(&ctx.yaml().replace(&head, &zeros));
    assert_eq!(
        ctx.check_path(&ctx.draft).unwrap(),
        CheckReport {
            ok: true,
            diagnostics: vec![]
        }
    );
}

/// X12 provenance_and_deck_diagnostics_together: both kinds are reported.
#[test]
fn provenance_and_deck_diagnostics_together() {
    let ctx = Ctx::new();
    ctx.valid();
    ctx.tamper("pr:", "number: 7", "number: 8");
    ctx.set_slides(&["overview", "arch", "step-one", "arch"]);
    assert_eq!(
        ctx.diags(),
        vec![
            e(
                "guide.yaml",
                "pr.number",
                "provenance_changed",
                &prov_msg("pr.number")
            ),
            dup_slide(3, "arch", 1)
        ]
    );
}

/// X13 commits_kind_without_commits_is_internal_even_without_code: the row is
/// rejected before any block is looked at.
#[test]
fn commits_kind_without_commits_is_internal_even_without_code() {
    let ctx = Ctx::new();
    ctx.valid();
    ctx.put("step-one", &step("step-one", &[], &[]));
    ctx.set_row(|j| {
        let start = j.find("\"commits\":[").unwrap();
        let end = j[start..].find(']').unwrap() + start + 1;
        format!("{}\"commits\":[]{}", &j[..start], &j[end..])
            .replace("\"kind\":\"full\"", "\"kind\":\"commits\"")
    });
    assert_err(
        ctx.check_path(&ctx.draft),
        "internal",
        "internal error: the drafts row's provenance_json has selection kind commits but no commits",
    );
}

/// X14 lossy_path_does_not_match (Linux only: macOS cannot name a non-UTF-8
/// path): a row path holding U+FFFD must not match a path holding byte 0xFF.
#[cfg(target_os = "linux")]
#[test]
fn lossy_path_does_not_match() {
    use std::{ffi::OsStr, os::unix::ffi::OsStrExt};
    let ctx = Ctx::new();
    ctx.valid();
    let tmp = TempDir::new().unwrap();
    let root = canon(tmp.path());
    let id = ctx.out.draft_id.clone();
    let stored = root.join("h\u{FFFD}me").join(&id);
    let raw = root.join(OsStr::from_bytes(b"h\xFFme")).join(&id);
    fs::create_dir_all(&raw).unwrap();
    let s = stored.to_str().unwrap().to_owned();
    ctx.store()
        .tx(|tx| -> Result<(), StoreError> {
            tx.execute("UPDATE drafts SET path = ?1 WHERE id = ?2", (&s, &id))?;
            Ok(())
        })
        .unwrap();
    let err = ctx.check_path(&raw).unwrap_err();
    assert_eq!(err.code(), "draft_unknown", "{err}");
}
