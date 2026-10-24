mod support;

use prguide::{
    anchors::*,
    git::{
        Git,
        diff::{FileDiff, FileStatus, GitPath, Hunk, Line, LineKind},
    },
    guide::range::{ResolvedSelection, Selection, Snapshot},
};
use support::fixture_repo::FixtureRepo;

// ---------- helpers ----------

fn nums(prefix: &str, n: u32) -> String {
    (1..=n).map(|i| format!("{prefix} {i}\n")).collect()
}

fn anchor_err<T: std::fmt::Debug>(result: Result<T, prguide::Error>) -> AnchorError {
    match result {
        Err(prguide::Error::Anchor(e)) => e,
        Err(other) => panic!("not an anchor error: {}", other.code()),
        Ok(value) => panic!("expected an error, got {value:?}"),
    }
}

#[track_caller]
fn expect_err<T: std::fmt::Debug>(
    result: Result<T, prguide::Error>,
    code: &str,
    field: &str,
    message: &str,
) {
    let e = anchor_err(result);
    assert_eq!(e.code(), code, "{e}");
    assert_eq!(e.field(), field, "{e}");
    assert!(e.to_string().contains(message), "{e} lacks {message}");
}

fn dblock(path: &str, side: Side, start: Option<u32>, end: Option<u32>) -> CodeBlock {
    CodeBlock::Diff {
        id: "b".into(),
        path: path.into(),
        side,
        start,
        end,
    }
}
fn dnew(path: &str, start: u32, end: u32) -> CodeBlock {
    dblock(path, Side::New, Some(start), Some(end))
}
fn dold(path: &str, start: u32, end: u32) -> CodeBlock {
    dblock(path, Side::Old, Some(start), Some(end))
}
fn cblock(path: &str, at: At, start: u32, end: u32) -> CodeBlock {
    CodeBlock::Context {
        id: "b".into(),
        path: path.into(),
        at,
        start,
        end,
    }
}

fn sha7(sha: &str) -> &str {
    &sha[..7]
}

fn context(
    fx: &FixtureRepo,
    base: &str,
    head: &str,
    commits: &[&str],
    sel: Selection,
) -> BlockContext {
    let git = fx.git(fx.work_path());
    let snapshot = Snapshot {
        base: base.into(),
        head: head.into(),
    };
    let commits: Vec<String> = commits.iter().map(|c| c.to_string()).collect();
    let selection = sel.resolve(&git, &snapshot, &commits).unwrap();
    BlockContext::new(&git, 1, snapshot, selection).unwrap()
}

// ---------- fixture R ----------

struct R {
    fx: FixtureRepo,
    git: Git,
    b: String,
    p1: String,
    p2: String,
}

impl R {
    fn new() -> R {
        let fx = FixtureRepo::new();
        fx.write("src/a.rs", nums("a", 60));
        fx.write("old.rs", nums("o", 30));
        fx.write("gone.rs", nums("g", 10));
        fx.write("logo.bin", b"\0bin v1\n");
        fx.write("tool.sh", "#!/bin/sh\necho tool\n");
        fx.gitlink("sub", &"1".repeat(40));
        fx.write("pure.rs", nums("u", 8));
        fx.write("crlf.txt", crlf(&[]));
        fx.write("ctx.rs", nums("c", 20));
        fx.write("v.txt", "base\n");
        let b = fx.commit("B");

        let a_lines: String = (1..=60)
            .map(|i| match i {
                20 => "A 20\n".to_string(),
                45 => "A 45\n".to_string(),
                _ => format!("a {i}\n"),
            })
            .collect();
        fx.write("src/a.rs", a_lines);
        fx.rename("old.rs", "new.rs");
        let new_lines: String = (1..=30)
            .map(|i| {
                if i == 10 {
                    "O 10\n".to_string()
                } else {
                    format!("o {i}\n")
                }
            })
            .collect();
        fx.write("new.rs", new_lines);
        fx.remove("gone.rs");
        fx.write("added.rs", nums("n", 5));
        fx.write("empty.txt", b"");
        fx.write("logo.bin", b"\0bin v2\n");
        fx.set_executable("tool.sh", true);
        fx.gitlink("sub", &"2".repeat(40));
        fx.rename("pure.rs", "pure2.rs");
        fx.write("crlf.txt", crlf(&[(2, "TWO")]));
        fx.write("v.txt", "p1\n");
        let p1 = fx.commit("p1");
        fx.write("v.txt", "p2\n");
        let p2 = fx.commit("p2");
        let git = fx.git(fx.work_path());
        R { fx, git, b, p1, p2 }
    }

    fn full(&self) -> BlockContext {
        let cx = context(
            &self.fx,
            &self.b,
            &self.p2,
            &[&self.p1, &self.p2],
            Selection::Full,
        );
        let hunk = |path: &str| -> Vec<(u32, u32, u32, u32)> {
            cx.diff
                .iter()
                .find(|f| f.path.as_bytes() == path.as_bytes())
                .unwrap()
                .hunks
                .iter()
                .map(|h| (h.old_start, h.old_lines, h.new_start, h.new_lines))
                .collect()
        };
        assert_eq!(hunk("src/a.rs"), vec![(17, 7, 17, 7), (42, 7, 42, 7)]);
        assert_eq!(hunk("new.rs"), vec![(7, 7, 7, 7)]);
        assert_eq!(hunk("gone.rs"), vec![(1, 10, 0, 0)]);
        assert_eq!(hunk("added.rs"), vec![(0, 0, 1, 5)]);
        assert_eq!(hunk("crlf.txt"), vec![(1, 5, 1, 5)]);
        for stub in ["empty.txt", "pure2.rs", "tool.sh", "logo.bin", "sub"] {
            assert!(hunk(stub).is_empty(), "{stub}");
        }
        cx
    }

    fn range(&self) -> BlockContext {
        let cx = context(
            &self.fx,
            &self.b,
            &self.p2,
            &[&self.p1, &self.p2],
            Selection::Commits {
                first: self.p1.clone(),
                last: self.p1.clone(),
            },
        );
        assert_eq!(cx.selection.base, self.b);
        assert_eq!(cx.selection.head, self.p1);
        cx
    }

    fn resolve(
        &self,
        cx: &BlockContext,
        block: &CodeBlock,
    ) -> Result<ResolvedBlock, prguide::Error> {
        resolve(&self.git, block, cx)
    }
}

fn crlf(edits: &[(usize, &str)]) -> String {
    let words = [
        "one", "two", "three", "four", "five", "six", "seven", "eight", "nine", "ten",
    ];
    words
        .iter()
        .enumerate()
        .map(|(i, w)| {
            let word = edits
                .iter()
                .find(|(n, _)| *n == i + 1)
                .map_or(*w, |(_, e)| *e);
            format!("{word}\r\n")
        })
        .collect()
}

fn text_of(lines: &[Vec<u8>]) -> Vec<&[u8]> {
    lines.iter().map(|l| l.as_slice()).collect()
}

// ---------- hand tests ----------

#[derive(Debug, serde::Deserialize)]
struct Doc {
    code: Vec<CodeBlock>,
}

#[test]
fn code_block_serde_matches_design() {
    let yaml = "\
code:
  - id: handler
    kind: diff
    path: src/api/orders.rs
    side: new
    start: 40
    end: 88
  - id: router
    kind: context          # unchanged code (Q20); no side
    at: snapshot-head      # the default; or selection-head | selection-base
    path: src/router.rs
    start: 12
    end: 20
  - id: noat
    kind: context
    path: x.rs
    start: 1
    end: 2
  - id: bare
    kind: diff
    path: y.bin
    side: old
";
    let doc: Doc = serde_saphyr::from_str(yaml).unwrap();
    assert_eq!(
        doc.code,
        vec![
            CodeBlock::Diff {
                id: "handler".into(),
                path: "src/api/orders.rs".into(),
                side: Side::New,
                start: Some(40),
                end: Some(88)
            },
            CodeBlock::Context {
                id: "router".into(),
                path: "src/router.rs".into(),
                at: At::SnapshotHead,
                start: 12,
                end: 20
            },
            CodeBlock::Context {
                id: "noat".into(),
                path: "x.rs".into(),
                at: At::SnapshotHead,
                start: 1,
                end: 2
            },
            CodeBlock::Diff {
                id: "bare".into(),
                path: "y.bin".into(),
                side: Side::Old,
                start: None,
                end: None
            },
        ]
    );
    assert_eq!(doc.code[0].id(), "handler");
    assert_eq!(doc.code[0].path(), "src/api/orders.rs");
    assert_eq!(doc.code[1].id(), "router");
    assert_eq!(doc.code[1].path(), "src/router.rs");
    for block in &doc.code {
        let json = serde_json::to_string(block).unwrap();
        assert_eq!(&serde_json::from_str::<CodeBlock>(&json).unwrap(), block);
    }
    let bare = serde_json::to_string(&doc.code[3]).unwrap();
    assert!(!bare.contains("start") && !bare.contains("end"), "{bare}");

    let bad = |body: &str| -> String {
        serde_saphyr::from_str::<Doc>(&format!("code:\n  - {body}"))
            .unwrap_err()
            .to_string()
    };
    assert!(
        bad("id: a\n    kind: diff\n    path: p\n    side: new\n    bogus: 1")
            .contains("unknown field"),
    );
    assert!(bad("id: a\n    kind: diff\n    path: p\n    side: left").contains("unknown variant"));
    assert!(bad("id: a\n    kind: context\n    path: p\n    start: 1").contains("missing field"));
    assert!(
        serde_saphyr::from_str::<Doc>(
            "code:\n  - id: a\n    kind: diff\n    path: p\n    side: new\n    start: -1"
        )
        .is_err()
    );
    assert!(bad("id: a\n    kind: other\n    path: p\n    side: new").contains("other"));
}

#[test]
fn stored_anchor_json_shape() {
    let rev = "a".repeat(40);
    let diff = StoredAnchor {
        version: 1,
        slide: "s2".into(),
        block: "b".into(),
        kind: BlockKind::Diff,
        path: "new.rs".into(),
        old_path: Some("old.rs".into()),
        side: Some(Side::Old),
        rev: rev.clone(),
        start: 10,
        end: 10,
        lines: vec!["o 10".into()],
    };
    let json = serde_json::to_string(&diff).unwrap();
    assert_eq!(
        json,
        format!(
            r#"{{"version":1,"slide":"s2","block":"b","kind":"diff","path":"new.rs","old_path":"old.rs","side":"old","rev":"{rev}","start":10,"end":10,"lines":["o 10"]}}"#
        )
    );
    assert_eq!(serde_json::from_str::<StoredAnchor>(&json).unwrap(), diff);
    assert_eq!(diff.rev_path(), "old.rs");

    let ctx = StoredAnchor {
        kind: BlockKind::Context,
        old_path: None,
        side: None,
        path: "c.rs".into(),
        ..diff.clone()
    };
    let json = serde_json::to_string(&ctx).unwrap();
    assert_eq!(
        json,
        format!(
            r#"{{"version":1,"slide":"s2","block":"b","kind":"context","path":"c.rs","rev":"{rev}","start":10,"end":10,"lines":["o 10"]}}"#
        )
    );
    assert_eq!(serde_json::from_str::<StoredAnchor>(&json).unwrap(), ctx);
    assert_eq!(ctx.rev_path(), "c.rs");

    let new_side = StoredAnchor {
        side: Some(Side::New),
        ..diff
    };
    assert_eq!(new_side.rev_path(), "new.rs");
}

// ---------- resolve: fixture R ----------

#[test]
fn resolve_diff_new_side_whole_hunks() {
    let r = R::new();
    let cx = r.full();
    let one = r.resolve(&cx, &dnew("src/a.rs", 20, 20)).unwrap();
    assert_eq!(one.rev, r.p2);
    assert_eq!(one.side, Some(Side::New));
    assert_eq!((one.start, one.end), (Some(20), Some(20)));
    assert_eq!(one.hunks.len(), 1);
    assert_eq!((one.hunks[0].new_start, one.hunks[0].new_lines), (17, 7));
    assert_eq!(one.hunks[0].lines.len(), 8);
    assert_eq!(one.kind, BlockKind::Diff);
    assert!(one.file.is_some());
    let both = r.resolve(&cx, &dnew("src/a.rs", 18, 46)).unwrap();
    assert_eq!(
        both.hunks.iter().map(|h| h.new_start).collect::<Vec<_>>(),
        vec![17, 42]
    );
    let ctx_line = r.resolve(&cx, &dnew("src/a.rs", 17, 17)).unwrap();
    assert_eq!(ctx_line.hunks.len(), 1);
    assert_eq!(ctx_line.hunks[0].new_start, 17);
    let second = r.resolve(&cx, &dnew("src/a.rs", 48, 48)).unwrap();
    assert_eq!(second.hunks.len(), 1);
    assert_eq!(second.hunks[0].new_start, 42);
}

#[test]
fn resolve_diff_old_side_uses_selection_base() {
    let r = R::new();
    let cx = r.full();
    let gone = r.resolve(&cx, &dold("gone.rs", 1, 3)).unwrap();
    assert_eq!(gone.rev, r.b);
    assert_eq!(gone.hunks.len(), 1);
    assert_eq!(gone.hunks[0].old_lines, 10);
    let renamed = r.resolve(&cx, &dold("new.rs", 10, 10)).unwrap();
    assert_eq!(renamed.rev, r.b);
    assert_eq!(renamed.old_path.as_ref().unwrap().as_bytes(), b"old.rs");
    assert_eq!(renamed.hunks.len(), 1);
    assert_eq!(renamed.hunks[0].old_start, 7);
    let new = r.resolve(&cx, &dnew("new.rs", 10, 10)).unwrap();
    assert_eq!(new.rev, r.p2);
}

#[test]
fn resolve_path_not_in_diff() {
    let r = R::new();
    let cx = r.full();
    expect_err(
        r.resolve(&cx, &dnew("ctx.rs", 1, 1)),
        "path_not_in_diff",
        "path",
        "ctx.rs is not in this guide's diff",
    );
    expect_err(
        r.resolve(&cx, &dnew("old.rs", 1, 1)),
        "path_not_in_diff",
        "path",
        "renamed to new.rs; cite the new path",
    );
}

#[test]
fn resolve_wrong_side() {
    let r = R::new();
    let cx = r.full();
    expect_err(
        r.resolve(&cx, &dnew("gone.rs", 1, 1)),
        "wrong_side",
        "side",
        "it was deleted; use side: old",
    );
    expect_err(
        r.resolve(&cx, &dold("added.rs", 1, 1)),
        "wrong_side",
        "side",
        "it was added; use side: new",
    );
}

#[test]
fn resolve_range_required() {
    let r = R::new();
    let cx = r.full();
    for block in [
        dblock("src/a.rs", Side::New, Some(20), None),
        dblock("src/a.rs", Side::New, None, None),
    ] {
        expect_err(
            r.resolve(&cx, &block),
            "range_required",
            "start",
            "needs both start and end",
        );
    }
}

#[test]
fn resolve_stubs() {
    let r = R::new();
    let cx = r.full();
    let bare = |path: &str, side| dblock(path, side, None, None);
    let cases = [
        ("logo.bin", Stub::Binary, "is a binary file;"),
        ("tool.sh", Stub::ModeOnly, "is a mode-only change;"),
        ("sub", Stub::Submodule, "is a submodule;"),
        ("pure2.rs", Stub::NoText, "is a change without text lines;"),
        ("empty.txt", Stub::NoText, "is a change without text lines;"),
    ];
    for (path, stub, message) in cases {
        let ok = r.resolve(&cx, &bare(path, Side::New)).unwrap();
        assert_eq!(ok.stub, Some(stub), "{path}");
        assert!(ok.hunks.is_empty());
        assert_eq!((ok.start, ok.end), (None, None));
        assert_eq!(ok.rev, r.p2);
        expect_err(
            r.resolve(&cx, &dnew(path, 1, 1)),
            "range_not_allowed",
            "start",
            message,
        );
    }
    expect_err(
        r.resolve(&cx, &dblock("logo.bin", Side::New, None, Some(1))),
        "range_not_allowed",
        "start",
        "is a binary file;",
    );
    expect_err(
        r.resolve(&cx, &dblock("logo.bin", Side::New, Some(1), None)),
        "range_not_allowed",
        "start",
        "is a binary file;",
    );
    let old = r.resolve(&cx, &bare("logo.bin", Side::Old)).unwrap();
    assert_eq!(old.stub, Some(Stub::Binary));
    assert_eq!(old.rev, r.b);

    // An empty file deleted alone.
    let fx = FixtureRepo::new();
    fx.write("e.txt", b"");
    let base = fx.commit("base");
    fx.remove("e.txt");
    let head = fx.commit("head");
    let cx = context(&fx, &base, &head, &[&head], Selection::Full);
    let git = fx.git(fx.work_path());
    let ok = resolve(&git, &bare("e.txt", Side::Old), &cx).unwrap();
    assert_eq!(ok.stub, Some(Stub::NoText));
    assert_eq!(ok.rev, base);
    expect_err(
        resolve(&git, &dold("e.txt", 1, 1), &cx),
        "range_not_allowed",
        "start",
        "is a change without text lines;",
    );
    expect_err(
        resolve(&git, &bare("e.txt", Side::New), &cx),
        "wrong_side",
        "side",
        "it was deleted",
    );
}

#[test]
fn resolve_lines_out_of_range() {
    let r = R::new();
    let cx = r.full();
    expect_err(
        r.resolve(&cx, &dnew("src/a.rs", 0, 20)),
        "lines_out_of_range",
        "start",
        "start must be at least 1",
    );
    expect_err(
        r.resolve(&cx, &dnew("src/a.rs", 30, 20)),
        "lines_out_of_range",
        "end",
        "start 30 is after end 20",
    );
    expect_err(
        r.resolve(&cx, &dnew("src/a.rs", 42, 61)),
        "lines_out_of_range",
        "end",
        &format!(
            "lines 42-61 are outside src/a.rs at side new ({}), which has 60 lines",
            sha7(&r.p2)
        ),
    );
    r.resolve(&cx, &dnew("src/a.rs", 42, 60)).unwrap();
    let e = anchor_err(r.resolve(&cx, &dold("gone.rs", 1, 11)));
    assert_eq!(e.code(), "lines_out_of_range");
    assert_eq!(e.field(), "end");
    assert!(e.to_string().contains("which has 10 lines"), "{e}");
    assert!(e.to_string().contains("side old"), "{e}");
    let e = anchor_err(r.resolve(&cx, &cblock("ctx.rs", At::SnapshotHead, 15, 21)));
    assert_eq!(e.code(), "lines_out_of_range");
    assert_eq!(e.field(), "end");
    assert!(e.to_string().contains("at snapshot-head"), "{e}");
    assert!(e.to_string().contains("which has 20 lines"), "{e}");
    r.resolve(&cx, &cblock("ctx.rs", At::SnapshotHead, 15, 20))
        .unwrap();

    // Line counting: empty blob has 0 lines; a last line without "\n" counts.
    let fx = FixtureRepo::new();
    let base = fx.head();
    fx.write("e.txt", b"");
    fx.write("n.txt", "a\nb\nc\nd\ne");
    let head = fx.commit("head");
    let cx = context(&fx, &base, &head, &[&head], Selection::Full);
    let git = fx.git(fx.work_path());
    expect_err(
        resolve(&git, &cblock("e.txt", At::SnapshotHead, 1, 1), &cx),
        "lines_out_of_range",
        "end",
        "which has 0 lines",
    );
    let ok = resolve(&git, &cblock("n.txt", At::SnapshotHead, 5, 5), &cx).unwrap();
    assert_eq!(text_of(&ok.lines), vec![b"e".as_slice()]);
    expect_err(
        resolve(&git, &cblock("n.txt", At::SnapshotHead, 5, 6), &cx),
        "lines_out_of_range",
        "end",
        "which has 5 lines",
    );
}

#[test]
fn resolve_no_hunks_in_range() {
    let r = R::new();
    let cx = r.full();
    for (s, e) in [(1, 10), (24, 41)] {
        expect_err(
            r.resolve(&cx, &dnew("src/a.rs", s, e)),
            "no_hunks_in_range",
            "start",
            &format!(
                "no change in src/a.rs overlaps lines {s}-{e} on side new; use a context block for unchanged code"
            ),
        );
    }
    r.resolve(&cx, &dnew("src/a.rs", 23, 23)).unwrap();
    r.resolve(&cx, &dnew("src/a.rs", 41, 42)).unwrap();
}

#[test]
fn resolve_context_at_each_revision() {
    let r = R::new();
    let cx = r.range();
    for (at, text, rev) in [
        (At::SnapshotHead, "p2", &r.p2),
        (At::SelectionHead, "p1", &r.p1),
        (At::SelectionBase, "base", &r.b),
    ] {
        let got = r.resolve(&cx, &cblock("v.txt", at, 1, 1)).unwrap();
        assert_eq!(text_of(&got.lines), vec![text.as_bytes()]);
        assert_eq!(&got.rev, rev);
        assert_eq!(got.kind, BlockKind::Context);
        assert_eq!(got.at, Some(at));
        assert_eq!(got.path, "v.txt");
        assert!(got.hunks.is_empty());
        assert!(got.side.is_none() && got.file.is_none());
    }
}

#[test]
fn resolve_range_guide_revisions() {
    let r = R::new();
    // New side of a diff block is numbered at selection.head (p1), not snapshot.head (p2).
    let got = r.resolve(&r.range(), &dnew("src/a.rs", 20, 20)).unwrap();
    assert_eq!(got.rev, r.p1);
    // selection-base is selection.base (p1 here), not snapshot.base.
    let cx = context(
        &r.fx,
        &r.b,
        &r.p2,
        &[&r.p1, &r.p2],
        Selection::Commits {
            first: r.p2.clone(),
            last: r.p2.clone(),
        },
    );
    assert_eq!(cx.selection.base, r.p1);
    let got = r
        .resolve(&cx, &cblock("v.txt", At::SelectionBase, 1, 1))
        .unwrap();
    assert_eq!(text_of(&got.lines), vec![b"p1".as_slice()]);
    assert_eq!(got.rev, r.p1);
}

#[test]
fn resolve_invalid_utf8_line_is_lossy_in_anchor() {
    let fx = FixtureRepo::new();
    fx.write("readme.txt", "hi\n");
    let base = fx.commit("base");
    fx.write("bad.txt", b"ok\n\xff\xfe bad\nend\n".to_vec());
    let head = fx.commit("head");
    let git = fx.git(fx.work_path());
    let cx = context(&fx, &base, &head, &[&head], Selection::Full);
    let got = resolve(&git, &cblock("bad.txt", At::SnapshotHead, 1, 3), &cx).unwrap();
    assert_eq!(got.lines[1], b"\xff\xfe bad".to_vec());
    let anchor = got.anchor("s", 2, 2).unwrap();
    assert_eq!(anchor.lines, vec!["\u{FFFD}\u{FFFD} bad".to_string()]);
}

#[cfg(unix)]
#[test]
fn resolve_typechange_both_sides() {
    let fx = FixtureRepo::new();
    fx.write("t.txt", nums("t", 3));
    let base = fx.commit("base");
    fx.remove("t.txt");
    std::os::unix::fs::symlink("p.rs", fx.work_path().join("t.txt")).unwrap();
    let head = fx.commit("head");
    let git = fx.git(fx.work_path());
    let cx = context(&fx, &base, &head, &[&head], Selection::Full);
    let new = resolve(&git, &dnew("t.txt", 1, 1), &cx).unwrap();
    assert_eq!(new.rev, head);
    assert_eq!(new.hunks.len(), 1);
    assert_eq!(new.file.unwrap().status, FileStatus::Added);
    let old = resolve(&git, &dold("t.txt", 1, 3), &cx).unwrap();
    assert_eq!(old.rev, base);
    assert_eq!(old.file.unwrap().status, FileStatus::Deleted);
}

#[test]
fn resolve_context_path_not_found() {
    let r = R::new();
    let cx = r.full();
    expect_err(
        r.resolve(&cx, &cblock("added.rs", At::SelectionBase, 1, 1)),
        "path_not_found",
        "path",
        &format!("added.rs is not a file at selection-base ({})", sha7(&r.b)),
    );
    expect_err(
        r.resolve(&cx, &cblock("sub", At::SnapshotHead, 1, 1)),
        "path_not_found",
        "path",
        "sub is not a file at snapshot-head",
    );
}

#[test]
fn resolve_context_binary_window() {
    let fx = FixtureRepo::new();
    let base = fx.head();
    let mut early = vec![b'x'; 7990];
    early.extend_from_slice(b"\0\nline2\n");
    let mut late = vec![b'x'; 9000];
    late.extend_from_slice(b"\0\nline2\n");
    fx.write("early.txt", &early);
    fx.write("late.txt", &late);
    let head = fx.commit("head");
    let cx = context(&fx, &base, &head, &[&head], Selection::Full);
    let git = fx.git(fx.work_path());
    expect_err(
        resolve(&git, &cblock("early.txt", At::SnapshotHead, 1, 1), &cx),
        "range_not_allowed",
        "start",
        "is a binary file at snapshot-head",
    );
    let ok = resolve(&git, &cblock("late.txt", At::SnapshotHead, 2, 2), &cx).unwrap();
    assert_eq!(text_of(&ok.lines), vec![b"line2".as_slice()]);

    let r = R::new();
    expect_err(
        r.resolve(&r.full(), &cblock("logo.bin", At::SnapshotHead, 1, 1)),
        "range_not_allowed",
        "start",
        "is a binary file",
    );
}

#[test]
fn context_overlaps_hunk_warning() {
    let r = R::new();
    let cx = r.full();
    let warn = |cx: &BlockContext, at, s, e| {
        r.resolve(cx, &cblock("src/a.rs", at, s, e))
            .unwrap()
            .warnings
    };
    assert_eq!(
        warn(&cx, At::SnapshotHead, 18, 22),
        vec![Warning {
            code: "context_overlaps_hunk",
            field: "start",
            message: "lines 18-22 of src/a.rs include lines changed in this guide's diff; use a diff block".into()
        }]
    );
    assert!(warn(&cx, At::SnapshotHead, 1, 16).is_empty());
    assert!(warn(&cx, At::SnapshotHead, 17, 19).is_empty());
    assert_eq!(warn(&cx, At::SelectionBase, 20, 20).len(), 1);
    let ctxw = |path: &str, s, e| {
        r.resolve(&cx, &cblock(path, At::SelectionBase, s, e))
            .unwrap()
            .warnings
    };
    assert_eq!(ctxw("gone.rs", 1, 1).len(), 1); // deleted file
    assert_eq!(ctxw("old.rs", 10, 10).len(), 1); // renamed file, old path
    assert!(ctxw("old.rs", 1, 3).is_empty());
    let rc = r.range();
    assert!(warn(&rc, At::SnapshotHead, 20, 20).is_empty());
    assert_eq!(warn(&rc, At::SelectionHead, 20, 20).len(), 1);
}

#[cfg(unix)]
#[test]
fn resolve_non_utf8_path() {
    use std::{ffi::OsString, os::unix::ffi::OsStringExt};

    let fx = FixtureRepo::new();
    let git = fx.git(fx.work_path());
    let blob = |text: String| -> String {
        String::from_utf8(
            git.run_with_stdin(["hash-object", "-w", "--stdin"], text.as_bytes())
                .unwrap(),
        )
        .unwrap()
        .trim()
        .to_string()
    };
    let cacheinfo = |oid: &str, path: &[u8]| {
        let mut value = format!("100644,{oid},").into_bytes();
        value.extend_from_slice(path);
        [OsString::from("--cacheinfo"), OsString::from_vec(value)]
    };
    let old_blob = blob(nums("r", 10));
    let edited: String = (1..=10)
        .map(|i| {
            if i == 2 {
                "R 2\n".to_string()
            } else {
                format!("r {i}\n")
            }
        })
        .collect();
    let new_blob = blob(edited);
    let cafe = blob(nums("k", 3));

    let mut add = vec![OsString::from("update-index"), OsString::from("--add")];
    add.extend(cacheinfo(&old_blob, b"r\xe9.txt"));
    git.run(add).unwrap();
    fx.run(fx.work_path(), ["commit", "-m", "base"]);
    let base = fx.head();

    let mut change = vec![OsString::from("update-index"), OsString::from("--add")];
    change.extend(cacheinfo(&cafe, b"caf\xe9.txt"));
    change.extend(cacheinfo(&new_blob, b"renamed.txt"));
    change.extend([
        OsString::from("--force-remove"),
        OsString::from_vec(b"r\xe9.txt".to_vec()),
    ]);
    git.run(change).unwrap();
    fx.run(fx.work_path(), ["commit", "-m", "head"]);
    let head = fx.head();
    let cx = context(&fx, &base, &head, &[&head], Selection::Full);

    expect_err(
        resolve(&git, &dnew("caf\u{FFFD}.txt", 1, 1), &cx),
        "path_not_utf8",
        "path",
        "names a file whose path is not valid UTF-8 (\"caf\\xe9.txt\")",
    );
    expect_err(
        resolve(&git, &dnew("café.txt", 1, 1), &cx),
        "path_not_in_diff",
        "path",
        "is not in this guide's diff",
    );
    expect_err(
        resolve(&git, &dold("renamed.txt", 2, 2), &cx),
        "path_not_utf8",
        "path",
        "was renamed from a path that is not valid UTF-8",
    );
    resolve(&git, &dnew("renamed.txt", 2, 2), &cx).unwrap();
    expect_err(
        resolve(&git, &dold("r\u{FFFD}.txt", 2, 2), &cx),
        "path_not_utf8",
        "path",
        "names a file whose path is not valid UTF-8 (\"r\\xe9.txt\")",
    );
}

#[test]
fn resolve_crlf_lines_keep_cr() {
    let r = R::new();
    let cx = r.full();
    let got = r
        .resolve(&cx, &cblock("crlf.txt", At::SelectionBase, 1, 2))
        .unwrap();
    assert_eq!(text_of(&got.lines), vec![b"one\r".as_slice(), b"two\r"]);
    let diff = r.resolve(&cx, &dnew("crlf.txt", 2, 2)).unwrap();
    assert!(
        diff.hunks[0]
            .lines
            .iter()
            .any(|l| l.kind == LineKind::Added && l.text == b"TWO\r")
    );
    let anchor = diff.anchor("s1", 1, 2).unwrap();
    assert_eq!(anchor.lines, vec!["one\r".to_string(), "TWO\r".to_string()]);
}

#[test]
fn anchor_from_blocks() {
    let r = R::new();
    let cx = r.full();
    let new = r.resolve(&cx, &dnew("src/a.rs", 18, 22)).unwrap();
    let anchor = new.anchor("s2", 19, 21).unwrap();
    assert_eq!(
        anchor,
        StoredAnchor {
            version: 1,
            slide: "s2".into(),
            block: "b".into(),
            kind: BlockKind::Diff,
            path: "src/a.rs".into(),
            old_path: None,
            side: Some(Side::New),
            rev: r.p2.clone(),
            start: 19,
            end: 21,
            lines: vec!["a 19".into(), "A 20".into(), "a 21".into()],
        }
    );
    let old = r.resolve(&cx, &dold("src/a.rs", 18, 22)).unwrap();
    assert_eq!(
        old.anchor("s", 19, 21).unwrap().lines,
        vec!["a 19", "a 20", "a 21"]
    );
    let renamed = r.resolve(&cx, &dold("new.rs", 10, 10)).unwrap();
    let anchor = renamed.anchor("s", 10, 10).unwrap();
    assert_eq!(anchor.old_path.as_deref(), Some("old.rs"));
    assert_eq!(anchor.rev_path(), "old.rs");
    assert_eq!(anchor.lines, vec!["o 10"]);
    let context = r
        .resolve(&cx, &cblock("ctx.rs", At::SnapshotHead, 3, 5))
        .unwrap();
    let anchor = context.anchor("s", 4, 5).unwrap();
    assert_eq!(anchor.lines, vec!["c 4", "c 5"]);
    assert_eq!(anchor.side, None);
    assert_eq!(anchor.kind, BlockKind::Context);

    for (block, s, e) in [(&new, 16, 18), (&new, 22, 42), (&context, 2, 4)] {
        expect_err(
            Err::<(), _>(block.anchor("s", s, e).unwrap_err().into()),
            "lines_out_of_range",
            "start",
            "are not all shown in block",
        );
    }
    let stub = r
        .resolve(&cx, &dblock("logo.bin", Side::New, None, None))
        .unwrap();
    expect_err(
        Err::<(), _>(stub.anchor("s", 1, 1).unwrap_err().into()),
        "range_not_allowed",
        "start",
        "has no lines to anchor",
    );
    expect_err(
        Err::<(), _>(new.anchor("s", 0, 1).unwrap_err().into()),
        "lines_out_of_range",
        "start",
        "start must be at least 1",
    );
    expect_err(
        Err::<(), _>(new.anchor("s", 21, 19).unwrap_err().into()),
        "lines_out_of_range",
        "end",
        "start 21 is after end 19",
    );
}

// ---------- mapping: fixture M ----------

struct M {
    fx: FixtureRepo,
    git: Git,
    a: String,
}

impl M {
    fn new() -> M {
        let fx = FixtureRepo::new();
        fx.write("f.txt", nums("line", 30));
        fx.write("c.txt", crlf_lines());
        fx.write("n.txt", "a\nb\nc\nd\ne");
        let a = fx.commit("A");
        let git = fx.git(fx.work_path());
        M { fx, git, a }
    }

    fn anchor(&self) -> StoredAnchor {
        self.anchor_on("f.txt", 10, 12)
    }

    fn anchor_on(&self, path: &str, start: u32, end: u32) -> StoredAnchor {
        StoredAnchor {
            version: 1,
            slide: "s".into(),
            block: "b".into(),
            kind: BlockKind::Context,
            path: path.into(),
            old_path: None,
            side: None,
            rev: self.a.clone(),
            start,
            end,
            lines: vec!["line 10".into(), "line 11".into(), "line 12".into()],
        }
    }

    /// Start from A (detached), apply `change`, commit, return the new commit.
    fn commit(&self, change: impl FnOnce(&FixtureRepo)) -> String {
        self.fx.checkout(&self.a);
        change(&self.fx);
        self.fx.commit("change")
    }

    fn map(&self, anchor: &StoredAnchor, to: &str) -> Mapping {
        map_lines(&self.git, anchor, to).unwrap()
    }

    fn after(&self, change: impl FnOnce(&FixtureRepo)) -> Mapping {
        let to = self.commit(change);
        self.map(&self.anchor(), &to)
    }
}

fn crlf_lines() -> String {
    (1..=10).map(|i| format!("l{i}\r\n")).collect()
}

fn edited(prefix: &str, n: u32, edit: impl Fn(u32) -> Option<String>) -> String {
    (1..=n)
        .map(|i| format!("{}\n", edit(i).unwrap_or_else(|| format!("{prefix} {i}"))))
        .collect()
}

#[track_caller]
fn assert_mapped(mapping: &Mapping, path: &str, start: u32, end: u32) {
    match mapping {
        Mapping::Mapped(range) => {
            assert_eq!(range.path.as_bytes(), path.as_bytes());
            assert_eq!((range.start, range.end), (start, end));
        }
        other => panic!("expected Mapped, got {other:?}"),
    }
}

#[test]
fn map_identity() {
    let m = M::new();
    let same = m.map(&m.anchor(), &m.a);
    assert_eq!(
        same,
        Mapping::Mapped(MappedRange {
            path: GitPath::new(b"f.txt".to_vec()),
            start: 10,
            end: 12,
            rev: m.a.clone()
        })
    );
    assert_eq!(same.range().unwrap().rev, m.a);
    let to = m.commit(|fx| fx.write("other.txt", "x\n"));
    m.fx.branch("other");
    let mapped = m.map(&m.anchor(), "other");
    assert_mapped(&mapped, "f.txt", 10, 12);
    assert_eq!(mapped.range().unwrap().rev, to);
    assert_eq!(mapped.range(), Some(mapped.range().unwrap()));
}

#[test]
fn map_insertion_above_shifts() {
    let m = M::new();
    let insert = |fx: &FixtureRepo| {
        fx.write(
            "f.txt",
            edited("line", 30, |i| {
                (i == 3).then(|| "line 3\nnew a\nnew b".to_string())
            }),
        )
    };
    let to = m.commit(insert);
    m.fx.branch("moved");
    for target in [to.as_str(), "moved"] {
        let mapping = m.map(&m.anchor(), target);
        assert_mapped(&mapping, "f.txt", 12, 14);
        assert_eq!(mapping.range().map(|r| r.rev.as_str()), Some(to.as_str()));
    }
    let after6 = |fx: &FixtureRepo| {
        fx.write(
            "f.txt",
            edited("line", 30, |i| (i == 6).then(|| "line 6\nnew".to_string())),
        )
    };
    assert_mapped(&m.after(after6), "f.txt", 11, 13);
    let delete = |fx: &FixtureRepo| fx.write("f.txt", nums("line", 30).replace("line 5\n", ""));
    assert_mapped(&m.after(delete), "f.txt", 9, 11);
    let both = |fx: &FixtureRepo| {
        let text = edited("line", 30, |i| {
            (i == 3).then(|| "line 3\nnew a\nnew b".to_string())
        })
        .replace("line 25\n", "");
        fx.write("f.txt", text)
    };
    assert_mapped(&m.after(both), "f.txt", 12, 14);
}

#[test]
fn map_edit_inside_is_changed() {
    let m = M::new();
    let edit = |n: u32| {
        move |fx: &FixtureRepo| {
            fx.write(
                "f.txt",
                edited("line", 30, |i| (i == n).then(|| "EDIT".into())),
            )
        }
    };
    for n in [11, 10, 12] {
        let result = m.after(edit(n));
        assert_eq!(result, Mapping::Changed, "line {n}");
        assert!(result.range().is_none());
    }
    assert_mapped(&m.after(edit(13)), "f.txt", 10, 12);
    assert_mapped(&m.after(edit(9)), "f.txt", 10, 12);
}

#[test]
fn map_insertion_inside_is_changed() {
    let m = M::new();
    let insert_after = |n: u32| {
        move |fx: &FixtureRepo| {
            fx.write(
                "f.txt",
                edited("line", 30, |i| (i == n).then(|| format!("line {n}\nnew"))),
            )
        }
    };
    assert_eq!(m.after(insert_after(10)), Mapping::Changed);
    assert_eq!(m.after(insert_after(11)), Mapping::Changed);
    assert_mapped(&m.after(insert_after(9)), "f.txt", 11, 13);
    assert_mapped(&m.after(insert_after(12)), "f.txt", 10, 12);
}

#[test]
fn map_deleted_lines_is_changed() {
    let m = M::new();
    let without = |lines: &'static [&'static str]| {
        move |fx: &FixtureRepo| {
            let mut text = nums("line", 30);
            for l in lines {
                text = text.replace(&format!("{l}\n"), "");
            }
            fx.write("f.txt", text)
        }
    };
    assert_eq!(m.after(without(&["line 11"])), Mapping::Changed);
    assert_eq!(m.after(without(&["line 10"])), Mapping::Changed);
    assert_eq!(
        m.after(without(&["line 10", "line 11", "line 12"])),
        Mapping::Changed
    );
}

#[test]
fn map_rename() {
    let m = M::new();
    assert_mapped(&m.after(|fx| fx.rename("f.txt", "g.txt")), "g.txt", 10, 12);
    assert_mapped(
        &m.after(|fx| {
            fx.rename("f.txt", "g.txt");
            fx.write(
                "g.txt",
                edited("line", 30, |i| {
                    (i == 3).then(|| "line 3\nnew a\nnew b".to_string())
                }),
            );
        }),
        "g.txt",
        12,
        14,
    );
    assert_eq!(
        m.after(|fx| {
            fx.rename("f.txt", "g.txt");
            fx.write(
                "g.txt",
                edited("line", 30, |i| (i == 11).then(|| "EDIT".into())),
            );
        }),
        Mapping::Changed
    );
}

#[test]
fn map_file_deleted_binary_and_mode() {
    let m = M::new();
    let deleted = m.after(|fx| fx.remove("f.txt"));
    assert_eq!(deleted, Mapping::Deleted);
    assert!(deleted.range().is_none());
    #[cfg(unix)]
    assert_eq!(
        m.after(|fx| {
            fx.remove("f.txt");
            std::os::unix::fs::symlink("c.txt", fx.work_path().join("f.txt")).unwrap();
        }),
        Mapping::Deleted
    );
    assert_eq!(
        m.after(|fx| {
            fx.write("f.txt", format!("\0{}", nums("line", 30)));
        }),
        Mapping::Changed
    );
    assert_mapped(
        &m.after(|fx| fx.set_executable("f.txt", true)),
        "f.txt",
        10,
        12,
    );

    // A gitlink moved from one commit to another.
    m.fx.checkout(&m.a);
    m.fx.gitlink("sub", &"1".repeat(40));
    let s1 = m.fx.commit("S1");
    m.fx.gitlink("sub", &"2".repeat(40));
    let s2 = m.fx.commit("S2");
    let mut anchor = m.anchor_on("sub", 1, 1);
    anchor.rev = s1;
    anchor.lines = vec!["x".into()];
    assert_eq!(m.map(&anchor, &s2), Mapping::Changed);
}

#[test]
fn map_deleted_and_readded_uses_endpoints() {
    let m = M::new();
    m.fx.checkout(&m.a);
    m.fx.remove("f.txt");
    let b = m.fx.commit("B");
    m.fx.write("f.txt", format!("top\n{}", nums("line", 30)));
    let c = m.fx.commit("C");
    assert_eq!(m.map(&m.anchor(), &b), Mapping::Deleted);
    assert_mapped(&m.map(&m.anchor(), &c), "f.txt", 11, 13);
}

#[test]
fn map_rename_with_new_file_at_old_path() {
    let m = M::new();
    let result = m.after(|fx| {
        fx.rename("f.txt", "g.txt");
        fx.write("f.txt", "brand new\n");
    });
    assert_eq!(result, Mapping::Changed);
}

#[test]
fn map_crlf_and_no_newline() {
    let m = M::new();
    let c = m.commit(|fx| {
        fx.write(
            "c.txt",
            (1..=10)
                .map(|i| {
                    if i == 2 {
                        "L2\r\n".to_string()
                    } else {
                        format!("l{i}\r\n")
                    }
                })
                .collect::<String>(),
        )
    });
    assert_mapped(&m.map(&m.anchor_on("c.txt", 5, 6), &c), "c.txt", 5, 6);
    let lf = m.commit(|fx| fx.write("c.txt", crlf_lines().replace('\r', "")));
    assert_eq!(m.map(&m.anchor_on("c.txt", 5, 6), &lf), Mapping::Changed);
    let n = m.commit(|fx| fx.write("n.txt", "a\nb\nc\nd\ne\nf"));
    assert_eq!(m.map(&m.anchor_on("n.txt", 4, 5), &n), Mapping::Changed);
    assert_mapped(&m.map(&m.anchor_on("n.txt", 1, 2), &n), "n.txt", 1, 2);
}

#[test]
fn map_old_side_anchor_follows_rev_path() {
    let r = R::new();
    let cx = r.full();
    let block = r.resolve(&cx, &dold("new.rs", 10, 10)).unwrap();
    let anchor = block.anchor("s", 10, 10).unwrap();
    assert_eq!(anchor.rev, r.b);
    assert_eq!(map_lines(&r.git, &anchor, &r.p2).unwrap(), Mapping::Changed);
    let early = StoredAnchor {
        start: 2,
        end: 2,
        ..anchor
    };
    assert_mapped(&map_lines(&r.git, &early, &r.p2).unwrap(), "new.rs", 2, 2);
}

#[test]
fn map_bad_rev_is_git_error() {
    let m = M::new();
    let err = map_lines(&m.git, &m.anchor(), "refs/heads/nope").unwrap_err();
    assert_eq!(err.code(), "git_failed");
}

#[test]
fn results_ignore_hostile_git_config() {
    let r = R::new();
    let m = M::new();
    let moved = m.commit(|fx| fx.rename("f.txt", "g.txt"));
    let hostile = "[core]\n\tautocrlf = true\n\teol = crlf\n\tquotepath = true\n\
[diff]\n\trenames = false\n\tnoprefix = true\n\tmnemonicPrefix = true\n\tcontext = 10\n\
\tinterHunkContext = 10\n\talgorithm = patience\n[color]\n\tui = always\n";
    std::fs::write(r.fx.path().join("home/.gitconfig"), hostile).unwrap();
    std::fs::write(m.fx.path().join("home/.gitconfig"), hostile).unwrap();

    assert_mapped(&m.map(&m.anchor(), &moved), "g.txt", 10, 12);

    let cx = r.full();
    let got = r
        .resolve(&cx, &cblock("crlf.txt", At::SelectionBase, 1, 2))
        .unwrap();
    assert_eq!(text_of(&got.lines), vec![b"one\r".as_slice(), b"two\r"]);
    let one = r.resolve(&cx, &dnew("src/a.rs", 20, 20)).unwrap();
    assert_eq!((one.hunks[0].new_start, one.hunks[0].new_lines), (17, 7));
}

// ---------- inline: fixture P ----------

struct P {
    diff: Vec<FileDiff>,
    head: String,
}

impl P {
    fn new() -> P {
        let fx = FixtureRepo::new();
        fx.write("p.rs", nums("p", 60));
        fx.write("gone.rs", nums("g", 5));
        fx.write("old.rs", nums("o", 30));
        fx.write("logo.bin", b"\0bin v1\n");
        fx.write("tool.sh", "#!/bin/sh\necho tool\n");
        fx.gitlink("sub", &"1".repeat(40));
        fx.write("same.rs", nums("s", 5));
        #[cfg(unix)]
        fx.write("t.txt", nums("t", 3));
        let base = fx.commit("base");

        let mut head_text = String::new();
        for i in 1..=60 {
            match i {
                20 => head_text.push_str("X 20\n"),
                50 => {}
                _ => head_text.push_str(&format!("p {i}\n")),
            }
            if i == 45 {
                head_text.push_str("ins a\nins b\n");
            }
        }
        fx.write("p.rs", head_text);
        fx.remove("gone.rs");
        fx.rename("old.rs", "new.rs");
        fx.write(
            "new.rs",
            edited("o", 30, |i| (i == 10).then(|| "O 10".into())),
        );
        fx.write("logo.bin", b"\0bin v2\n");
        fx.set_executable("tool.sh", true);
        fx.gitlink("sub", &"2".repeat(40));
        #[cfg(unix)]
        {
            fx.remove("t.txt");
            std::os::unix::fs::symlink("p.rs", fx.work_path().join("t.txt")).unwrap();
        }
        let head = fx.commit("head");
        let git = fx.git(fx.work_path());
        let snapshot = Snapshot {
            base,
            head: head.clone(),
        };
        let selection: ResolvedSelection = Selection::Full
            .resolve(&git, &snapshot, std::slice::from_ref(&head))
            .unwrap();
        let diff = selection.files(&git).unwrap();
        let p = diff.iter().find(|f| f.path.as_bytes() == b"p.rs").unwrap();
        assert_eq!(
            p.hunks
                .iter()
                .map(|h| (h.old_start, h.old_lines, h.new_start, h.new_lines))
                .collect::<Vec<_>>(),
            vec![(17, 7, 17, 7), (43, 11, 43, 12)]
        );
        #[cfg(unix)]
        assert_eq!(
            diff.iter()
                .filter(|f| f.path.as_bytes() == b"t.txt")
                .map(|f| f.status)
                .collect::<Vec<_>>(),
            vec![FileStatus::Deleted, FileStatus::Added]
        );
        P { diff, head }
    }

    fn loc(&self, path: &str, start: u32, end: u32, side: Side) -> Option<InlineLoc> {
        let mapped = MappedRange {
            path: GitPath::new(path.as_bytes().to_vec()),
            start,
            end,
            rev: self.head.clone(),
        };
        inline_location(&mapped, side, &self.diff)
    }
}

#[test]
fn inline_single_line() {
    let p = P::new();
    assert_eq!(
        p.loc("p.rs", 20, 20, Side::New),
        Some(InlineLoc {
            path: "p.rs".into(),
            line: 20,
            side: Side::New,
            start_line: None,
            start_side: None
        })
    );
}

#[test]
fn inline_range_in_one_hunk() {
    let p = P::new();
    assert_eq!(
        p.loc("p.rs", 18, 22, Side::New),
        Some(InlineLoc {
            path: "p.rs".into(),
            line: 22,
            side: Side::New,
            start_line: Some(18),
            start_side: Some(Side::New)
        })
    );
    assert!(p.loc("p.rs", 17, 23, Side::New).is_some());
    assert!(p.loc("p.rs", 16, 20, Side::New).is_none());
    assert!(p.loc("p.rs", 17, 24, Side::New).is_none());
    assert!(p.loc("p.rs", 22, 18, Side::New).is_none());
    assert!(p.loc("p.rs", 0, 18, Side::New).is_none());
}

#[test]
fn inline_range_across_two_hunks_is_none() {
    let p = P::new();
    assert!(p.loc("p.rs", 20, 45, Side::New).is_none());
    assert!(p.loc("p.rs", 20, 45, Side::Old).is_none());
}

#[test]
fn inline_outside_every_hunk_is_none() {
    let p = P::new();
    assert!(p.loc("p.rs", 30, 30, Side::New).is_none());
    assert!(p.loc("p.rs", 30, 30, Side::Old).is_none());
    assert!(p.loc("same.rs", 1, 1, Side::New).is_none());
    assert!(p.loc("missing.rs", 1, 1, Side::New).is_none());
}

#[test]
fn inline_left_side() {
    let p = P::new();
    let left = p.loc("p.rs", 20, 20, Side::Old).unwrap();
    assert_eq!((left.side, left.line), (Side::Old, 20));
    assert!(p.loc("p.rs", 50, 50, Side::Old).is_some());
    let range = p.loc("p.rs", 43, 53, Side::Old).unwrap();
    assert_eq!(
        (range.line, range.start_line, range.start_side),
        (53, Some(43), Some(Side::Old))
    );
    assert!(p.loc("p.rs", 43, 54, Side::Old).is_none());
    assert!(p.loc("p.rs", 43, 54, Side::New).is_some());
    let gone = p.loc("gone.rs", 1, 5, Side::Old).unwrap();
    assert_eq!(gone.path, "gone.rs");
    assert!(p.loc("gone.rs", 1, 1, Side::New).is_none());
    let renamed = p.loc("old.rs", 10, 10, Side::Old).unwrap();
    assert_eq!((renamed.path.as_str(), renamed.side), ("new.rs", Side::Old));
    assert_eq!(p.loc("new.rs", 10, 10, Side::New).unwrap().path, "new.rs");
    #[cfg(unix)]
    {
        assert_eq!(
            p.loc("t.txt", 1, 1, Side::New),
            Some(InlineLoc {
                path: "t.txt".into(),
                line: 1,
                side: Side::New,
                start_line: None,
                start_side: None
            })
        );
        let t = p.loc("t.txt", 1, 3, Side::Old).unwrap();
        assert_eq!(
            (t.line, t.start_line, t.start_side),
            (3, Some(1), Some(Side::Old))
        );
    }
}

#[test]
fn inline_context_lines_on_both_sides() {
    let p = P::new();
    assert!(p.loc("p.rs", 17, 17, Side::New).is_some());
    assert!(p.loc("p.rs", 17, 17, Side::Old).is_some());
    assert!(p.loc("p.rs", 46, 46, Side::Old).is_some());
    assert!(p.loc("p.rs", 46, 47, Side::New).is_some());
    assert!(p.loc("p.rs", 48, 48, Side::New).is_some());
}

#[test]
fn inline_stub_files_are_none() {
    let p = P::new();
    for path in ["logo.bin", "tool.sh", "sub"] {
        for side in [Side::New, Side::Old] {
            assert!(p.loc(path, 1, 1, side).is_none(), "{path}");
        }
    }
}

#[test]
fn inline_non_utf8_path_is_none() {
    let hunk = Hunk {
        old_start: 1,
        old_lines: 1,
        new_start: 1,
        new_lines: 1,
        section: None,
        lines: vec![
            Line {
                kind: LineKind::Removed,
                text: b"a".to_vec(),
                no_newline: false,
            },
            Line {
                kind: LineKind::Added,
                text: b"b".to_vec(),
                no_newline: false,
            },
        ],
    };
    let file = |path: &[u8]| FileDiff {
        path: GitPath::new(path.to_vec()),
        old_path: None,
        status: FileStatus::Modified,
        old_mode: Some(0o100644),
        new_mode: Some(0o100644),
        old_oid: None,
        new_oid: None,
        similarity: None,
        binary: false,
        mode_only: false,
        submodule: false,
        hunks: vec![hunk.clone()],
    };
    let mapped = |path: &[u8]| MappedRange {
        path: GitPath::new(path.to_vec()),
        start: 1,
        end: 1,
        rev: "x".repeat(40),
    };
    assert!(inline_location(&mapped(b"caf\xe9.txt"), Side::New, &[file(b"caf\xe9.txt")]).is_none());
    assert!(inline_location(&mapped(b"cafe.txt"), Side::New, &[file(b"cafe.txt")]).is_some());
}
