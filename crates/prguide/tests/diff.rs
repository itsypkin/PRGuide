mod support;

use prguide::{
    git::{
        DiffSpec, GitError,
        diff::{FileStatus, LineKind, parse},
    },
    guide::range::{Selection, Snapshot},
};
use support::fixture_repo::FixtureRepo;

fn parse_error(input: &[u8]) -> (usize, String) {
    match parse(input).unwrap_err() {
        GitError::DiffParse { line, message } => (line, message),
        error => panic!("expected diff parse error, got {error:?}"),
    }
}

#[test]
fn empty_input_is_no_files() {
    assert_eq!(parse(b"").unwrap(), Vec::new());
}

#[test]
fn parses_text_hunks_modes_and_markers() {
    let output = b"diff --git a/x b/x\nindex 1111111111111111111111111111111111111111..2222222222222222222222222222222222222222 100644\n--- a/x\n+++ b/x\n@@ -1,2 +1,2 @@ section\n-old\n+new\n same\n\\ No newline at end of file\n";
    let files = parse(output).unwrap();
    let file = &files[0];
    assert_eq!(file.path.as_bytes(), b"x");
    assert_eq!(file.status, FileStatus::Modified);
    assert_eq!(file.old_mode, Some(0o100644));
    assert_eq!(file.new_mode, Some(0o100644));
    let hunk = &file.hunks[0];
    assert_eq!(hunk.section.as_deref(), Some(&b"section"[..]));
    assert_eq!(hunk.lines[0].kind, LineKind::Removed);
    assert_eq!(hunk.lines[1].kind, LineKind::Added);
    assert!(hunk.lines[2].no_newline);
}

#[test]
fn quoted_names_preserve_exact_bytes() {
    let output = b"diff --git \"a/ta\\tb\\001\\177\\351\" \"b/ta\\tb\\001\\177\\351\"\nold mode 100644\nnew mode 100755\n";
    let file = &parse(output).unwrap()[0];
    assert_eq!(file.path.as_bytes(), b"ta\tb\x01\x7f\xe9");
    assert_eq!(file.path.to_str(), None);
    assert!(file.mode_only);
}

#[test]
fn header_order_names_and_grammar_errors_have_lines() {
    for (input, expected) in [
        (b"hello\n".as_slice(), 1),
        (b"diff --git a/x b/x\nindex 1111111111111111111111111111111111111111..2222222222222222222222222222222222222222 100644\nnew file mode 100644\n".as_slice(), 3),
        (b"diff --git a/x b/x\n--- a/y\n+++ b/x\n@@ -1 +1 @@\n-a\n+b\n".as_slice(), 2),
        (b"diff --git a/x b/x\n--- a/x\n+++ b/x\n@@ -1 +1 @@x\n-a\n+b\n".as_slice(), 4),
        (b"diff --git a/x b/x\n--- a/x\n+++ b/x\n@@ -1 +1 @@\n\\ marker\n".as_slice(), 5),
    ] {
        let (line, message) = parse_error(input);
        assert_eq!(line, expected, "{input:?}");
        assert!(!message.is_empty());
    }
}

#[test]
fn hunk_count_and_final_newline_errors_are_strict() {
    let incomplete =
        b"diff --git a/x b/x\n--- a/x\n+++ b/x\n@@ -1,2 +1,2 @@\n-a\n+b\ndiff --git a/y b/y\n";
    assert_eq!(parse_error(incomplete).0, 7);
    let overflow = b"diff --git a/x b/x\n--- a/x\n+++ b/x\n@@ -1 +1 @@\n-a\n+b\n+c\n";
    assert_eq!(parse_error(overflow).0, 7);
    let missing_newline = b"diff --git a/x b/x\nold mode 100644\nnew mode 100755";
    assert_eq!(parse_error(missing_newline).0, 3);
}

#[test]
fn binary_and_submodule_forms_are_checked() {
    let binary = b"diff --git a/x b/x\nindex 1111111111111111111111111111111111111111..2222222222222222222222222222222222222222 100644\nBinary files a/x and b/x differ\n";
    assert!(parse(binary).unwrap()[0].binary);
    let submodule = b"diff --git a/sub b/sub\nindex 1111111111111111111111111111111111111111..2222222222222222222222222222222222222222 160000\n--- a/sub\n+++ b/sub\n@@ -1 +1 @@\n-Subproject commit 1111111111111111111111111111111111111111\n+Subproject commit 2222222222222222222222222222222222222222\n";
    let file = &parse(submodule).unwrap()[0];
    assert!(file.submodule);
    assert!(file.hunks.is_empty());
    let bad_binary = b"diff --git a/x b/x\nBinary files a/x and b/x\n";
    assert_eq!(parse_error(bad_binary).0, 2);
}

#[test]
fn real_diff_parses_rename_binary_and_mode_only() {
    let fixture = FixtureRepo::new();
    fixture.write("old", "same text\n");
    fixture.write("binary", b"a\0b");
    fixture.write("chmod", "old\n");
    let base = fixture.commit("base");
    fixture.rename("old", "new name is longer");
    fixture.set_executable("new name is longer", true);
    fixture.write("binary", b"a\0c");
    fixture.set_executable("chmod", true);
    let head = fixture.commit("change");
    let output = fixture
        .git(fixture.work_path())
        .diff(&DiffSpec::TwoDot {
            from: base,
            to: head,
            paths: vec![],
        })
        .unwrap();
    let files = parse(&output).unwrap();
    let renamed = files
        .iter()
        .find(|file| file.status == FileStatus::Renamed)
        .unwrap();
    assert_eq!(renamed.old_path.as_ref().unwrap().as_bytes(), b"old");
    assert_eq!(renamed.path.as_bytes(), b"new name is longer");
    assert!(renamed.mode_only);
    let binary = files
        .iter()
        .find(|file| file.path.as_bytes() == b"binary")
        .unwrap();
    assert!(binary.binary);
    let chmod = files
        .iter()
        .find(|file| file.path.as_bytes() == b"chmod")
        .unwrap();
    assert!(chmod.mode_only);
    insta::assert_snapshot!(format!("{files:#?}"));
}

#[test]
fn snapshot_and_selections_use_merge_base_and_first_parent() {
    let fixture = FixtureRepo::new();
    let fork = fixture.head();
    fixture.branch("pr");
    fixture.checkout("pr");
    fixture.write("one", "one\n");
    let first = fixture.commit("first");
    fixture.write("two", "two\n");
    let last = fixture.commit("last");
    fixture.checkout("main");
    fixture.write("main", "main\n");
    fixture.commit("main moves");
    let git = fixture.git(fixture.work_path());
    let snapshot = Snapshot::new(&git, &last, "main").unwrap();
    assert_eq!(snapshot.base, fork);
    let full = Selection::Full
        .resolve(&git, &snapshot, &[first.clone(), last.clone()])
        .unwrap();
    assert_eq!(full.commits, vec![first.clone(), last.clone()]);
    assert!(matches!(full.diff_spec(), DiffSpec::ThreeDot { .. }));
    let range = Selection::Commits {
        first: last.clone(),
        last: last.clone(),
    }
    .resolve(&git, &snapshot, &[first.clone(), last.clone()])
    .unwrap();
    assert_eq!(range.base, first);
    assert_eq!(range.base, git.rev_parents(&last).unwrap()[0]);
    assert!(matches!(range.diff_spec(), DiffSpec::TwoDot { .. }));
}

#[test]
fn selection_rejects_not_in_pr_and_root_ranges() {
    let fixture = FixtureRepo::new();
    let root = fixture.head();
    fixture.write("x", "x\n");
    let commit = fixture.commit("commit");
    let git = fixture.git(fixture.work_path());
    let snapshot = Snapshot::new(&git, &commit, &commit).unwrap();
    let error = Selection::Commits {
        first: "deadbeef".into(),
        last: commit.clone(),
    }
    .resolve(&git, &snapshot, std::slice::from_ref(&commit))
    .unwrap_err();
    assert_eq!(error.code(), "range_not_in_pr");
    let root_range = vec![root.clone()];
    let error = Selection::Commits {
        first: root.clone(),
        last: root,
    }
    .resolve(&git, &snapshot, &root_range)
    .unwrap_err();
    assert_eq!(error.code(), "no_base_commit");
}
