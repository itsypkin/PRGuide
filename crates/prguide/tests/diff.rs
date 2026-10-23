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

const OID_A: &str = "1111111111111111111111111111111111111111";
const OID_B: &str = "2222222222222222222222222222222222222222";

fn mode_only_header(name: &[u8]) -> Vec<u8> {
    let mut output = b"diff --git ".to_vec();
    output.extend_from_slice(name);
    output.extend_from_slice(b" ");
    output.extend_from_slice(name);
    output.extend_from_slice(b"\nold mode 100644\nnew mode 100755\n");
    output
}

#[test]
fn unquote_every_escape() {
    let file = parse(
        b"diff --git \"a/\\a\\b\\t\\n\\v\\f\\r\\\"\\\\\\001\\177\\351\" \"b/\\a\\b\\t\\n\\v\\f\\r\\\"\\\\\\001\\177\\351\"\nold mode 100644\nnew mode 100755\n",
    )
    .unwrap()
    .remove(0);
    assert_eq!(
        file.path.as_bytes(),
        b"\x07\x08\t\n\x0b\x0c\r\"\\\x01\x7f\xe9"
    );
}

#[test]
fn header_names_must_agree() {
    for (input, line) in [
        (
            b"diff --git a/x b/x\n--- a/y\n+++ b/x\n@@ -1 +1 @@\n-a\n+b\n".as_slice(),
            2,
        ),
        (
            b"diff --git a/x b/y\nold mode 100644\nnew mode 100755\n".as_slice(),
            1,
        ),
        (
            b"diff --git a/x b/x\nnew file mode 100644\n--- a/x\n+++ b/x\n@@ -0,0 +1 @@\n+x\n"
                .as_slice(),
            3,
        ),
    ] {
        assert_eq!(parse_error(input).0, line);
    }
}

#[test]
fn index_line_forms_are_strict() {
    let valid = format!(
        "diff --git a/x b/x\nindex {OID_A}..{OID_B} 100644\n--- a/x\n+++ b/x\n@@ -1 +1 @@\n-a\n+b\n"
    );
    let file = parse(valid.as_bytes()).unwrap().remove(0);
    assert_eq!(file.old_oid.as_deref(), Some(OID_A));
    assert_eq!(file.new_mode, Some(0o100644));
    let no_mode = format!("diff --git a/x b/x\nindex {OID_A}..{OID_B}\n");
    assert_eq!(parse(no_mode.as_bytes()).unwrap()[0].old_mode, None);
    for index in [
        "1111111..2222222",
        "1111111111111111111111111111111111111111..222222222222222222222222222222222222222",
        "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA..2222222222222222222222222222222222222222",
        "1111111111111111111111111111111111111111-2222222222222222222222222222222222222222",
        "1111111111111111111111111111111111111111..2222222222222222222222222222222222222222 10064",
        "1111111111111111111111111111111111111111..2222222222222222222222222222222222222222 100644 extra",
    ] {
        let input = format!("diff --git a/x b/x\nindex {index}\n");
        assert_eq!(parse_error(input.as_bytes()).0, 2);
    }
    let disagreement =
        format!("diff --git a/x b/x\nnew file mode 100644\nindex {OID_A}..{OID_B} 100755\n");
    assert_eq!(parse_error(disagreement.as_bytes()).0, 3);
}

#[test]
fn hunk_header_errors_are_strict() {
    for header in [
        "@@ -1 +1@@",
        "@@ -a +1 @@",
        "@@ -1 @@",
        "@@ -1 +1 @@x",
        "@@ -4294967296 +1 @@",
        "@@ -1 +1 @@ ",
        "@@ -1,0 +1,0 @@",
    ] {
        let input = format!("diff --git a/x b/x\n--- a/x\n+++ b/x\n{header}\n");
        assert_eq!(parse_error(input.as_bytes()).0, 4);
    }
}

#[test]
fn hunk_count_unknown_and_marker_errors_are_strict() {
    for (input, line) in [
        (
            b"diff --git a/x b/x\n--- a/x\n+++ b/x\n@@ -1,2 +1,2 @@\n-a\n+b\n".as_slice(),
            7,
        ),
        (
            b"diff --git a/x b/x\n--- a/x\n+++ b/x\n@@ -1 +0,0 @@\n-x\n-y\n".as_slice(),
            6,
        ),
        (
            b"diff --git a/x b/x\n--- a/x\n+++ b/x\n@@ -1 +1 @@\nxyz\n".as_slice(),
            5,
        ),
        (
            b"diff --git a/x b/x\n--- a/x\n+++ b/x\n@@ -1 +1 @@\n\n".as_slice(),
            5,
        ),
        (
            b"diff --git a/x b/x\n--- a/x\n+++ b/x\n@@ -1 +1 @@\n\\ marker\n".as_slice(),
            5,
        ),
        (
            b"diff --git a/x b/x\n--- a/x\n+++ b/x\n@@ -1 +1 @@\n-x\n\\ first\n\\ second\n+y\n"
                .as_slice(),
            7,
        ),
    ] {
        assert_eq!(parse_error(input).0, line);
    }
    let marker =
        b"diff --git a/x b/x\n--- a/x\n+++ b/x\n@@ -1 +1 @@\n-x\n\\ Keine neue Zeile\n+y\n";
    assert!(parse(marker).unwrap()[0].hunks[0].lines[0].no_newline);
}

#[test]
fn unknown_header_and_binary_lines_are_rejected() {
    for (input, line) in [
        (b"diff --git a/x b/x\ncopy from x\n".as_slice(), 2),
        (b"hello\n".as_slice(), 1),
        (b"diff --git a/x b/x\nGIT binary patch\n".as_slice(), 2),
        (
            b"diff --git a/x b/x\nBinary files a/x and b/x\n".as_slice(),
            2,
        ),
    ] {
        assert_eq!(parse_error(input).0, line);
    }
    for line in [
        b"Binary files a/x and b/x differ".as_slice(),
        b"Binary files /dev/null and b/x differ",
        b"Binary files a/x and /dev/null differ",
    ] {
        let mut input = b"diff --git a/x b/x\n".to_vec();
        input.extend_from_slice(line);
        input.push(b'\n');
        assert!(parse(&input).unwrap()[0].binary);
    }
}

#[test]
fn submodule_lines_and_similarity_are_checked() {
    let bad = format!(
        "diff --git a/sub b/sub\nindex {OID_A}..{OID_B} 160000\n--- a/sub\n+++ b/sub\n@@ -1 +1 @@\n-not a commit\n+Subproject commit {OID_B}\n"
    );
    assert_eq!(parse_error(bad.as_bytes()).0, 6);
    let plus = b"diff --git a/x b/x\nsimilarity index +5%\nrename from x\nrename to y\n";
    assert_eq!(parse_error(plus).0, 2);
}

#[test]
fn rename_headers_allow_mixed_quoting_and_ambiguous_raw_names() {
    let mixed = b"diff --git \"a/r\\tfrom.txt\" b/sp to.txt\nsimilarity index 100%\nrename from \"r\\tfrom.txt\"\nrename to sp to.txt\n";
    let file = parse(mixed).unwrap().remove(0);
    assert_eq!(file.old_path.unwrap().as_bytes(), b"r\tfrom.txt");
    assert_eq!(file.path.as_bytes(), b"sp to.txt");
    let raw =
        b"diff --git a/x b/y abcdefg\nsimilarity index 100%\nrename from x\nrename to y abcdefg\n";
    assert_eq!(parse(raw).unwrap()[0].path.as_bytes(), b"y abcdefg");
}

#[test]
fn noncanonical_headers_are_rejected() {
    let input = b"diff --git \"a/x\" \"b/x\"\nold mode 100644\nnew mode 100755\n";
    assert_eq!(parse_error(input).0, 1);
    let raw_control = b"diff --git a/a\x01 b/a\x01\nold mode 100644\nnew mode 100755\n";
    assert_eq!(parse_error(raw_control).0, 1);
}

#[test]
fn real_mixed_quoted_rename_parses() {
    let fixture = FixtureRepo::new();
    fixture.write("r\tfrom.txt", "content\n");
    let base = fixture.commit("base");
    fixture.rename("r\tfrom.txt", "sp to.txt");
    let head = fixture.commit("rename");
    let output = fixture
        .git(fixture.work_path())
        .diff(&DiffSpec::TwoDot {
            from: base,
            to: head,
            paths: vec![],
        })
        .unwrap();
    let file = parse(&output).unwrap().remove(0);
    assert_eq!(file.status, FileStatus::Renamed);
    assert_eq!(file.old_path.unwrap().as_bytes(), b"r\tfrom.txt");
    assert_eq!(file.path.as_bytes(), b"sp to.txt");
}

#[test]
fn selection_rejects_last_reversed_and_nonconsecutive() {
    let fixture = FixtureRepo::new();
    fixture.write("p1", "p1\n");
    let p1 = fixture.commit("p1");
    fixture.write("p2", "p2\n");
    let p2 = fixture.commit("p2");
    fixture.branch("side");
    fixture.checkout("side");
    fixture.write("x", "x\n");
    let x = fixture.commit("x");
    fixture.checkout("main");
    let git = fixture.git(fixture.work_path());
    let snapshot = Snapshot::new(&git, &p2, &p2).unwrap();
    for selection in [
        Selection::Commits {
            first: p2.clone(),
            last: p1.clone(),
        },
        Selection::Commits {
            first: p1.clone(),
            last: x.clone(),
        },
    ] {
        let error = selection
            .resolve(&git, &snapshot, &[p1.clone(), p2.clone()])
            .unwrap_err();
        assert_eq!(error.code(), "range_not_in_pr");
    }
    let error = Selection::Commits {
        first: x.clone(),
        last: p2.clone(),
    }
    .resolve(&git, &snapshot, &[p1.clone(), x.clone(), p2.clone()])
    .unwrap_err();
    assert_eq!(error.code(), "range_not_in_pr");
    assert!(error.to_string().contains(&x));
    assert!(error.to_string().contains(&p2));
}

#[test]
fn snapshot_rejects_missing_ref_and_unrelated_history() {
    let fixture = FixtureRepo::new();
    let main = fixture.head();
    fixture.run(fixture.work_path(), ["checkout", "--orphan", "orphan"]);
    fixture.write("orphan", "orphan\n");
    let orphan = fixture.commit("orphan");
    let git = fixture.git(fixture.work_path());
    assert_eq!(
        Snapshot::new(&git, "missing", &main).unwrap_err().code(),
        "git_failed"
    );
    assert_eq!(
        Snapshot::new(&git, &orphan, &main).unwrap_err().code(),
        "no_base_commit"
    );
}

#[test]
fn real_added_deleted_and_space_paths_parse() {
    let fixture = FixtureRepo::new();
    fixture.write("gone", "gone\n");
    let base = fixture.commit("base");
    fixture.remove("gone");
    fixture.write("sp ace", "added\n");
    let head = fixture.commit("change");
    let files = parse(
        &fixture
            .git(fixture.work_path())
            .diff(&DiffSpec::TwoDot {
                from: base,
                to: head,
                paths: vec![],
            })
            .unwrap(),
    )
    .unwrap();
    assert!(
        files
            .iter()
            .any(|file| file.status == FileStatus::Deleted && file.path.as_bytes() == b"gone")
    );
    assert!(
        files
            .iter()
            .any(|file| file.status == FileStatus::Added && file.path.as_bytes() == b"sp ace")
    );
}

#[test]
fn real_hunk_headers_and_sections_parse() {
    let fixture = FixtureRepo::new();
    fixture.write(
        "main.c",
        "int main() {\n  one();\n  two();\n  three();\n  four();\n  five();\n}\n",
    );
    let base = fixture.commit("base");
    fixture.write(
        "main.c",
        "int main() {\n  one();\n  two();\n  three();\n  FOUR();\n  five();\n}\n",
    );
    let head = fixture.commit("change");
    let file = parse(
        &fixture
            .git(fixture.work_path())
            .diff(&DiffSpec::TwoDot {
                from: base,
                to: head,
                paths: vec![],
            })
            .unwrap(),
    )
    .unwrap()
    .remove(0);
    assert_eq!(file.hunks[0].section.as_deref(), Some(&b"int main() {"[..]));
}

#[test]
fn real_no_newline_markers_parse() {
    let fixture = FixtureRepo::new();
    fixture.write("x", b"old");
    let base = fixture.commit("base");
    fixture.write("x", b"new");
    let head = fixture.commit("change");
    let file = parse(
        &fixture
            .git(fixture.work_path())
            .diff(&DiffSpec::TwoDot {
                from: base,
                to: head,
                paths: vec![],
            })
            .unwrap(),
    )
    .unwrap()
    .remove(0);
    assert!(file.hunks[0].lines.iter().all(|line| line.no_newline));
}

#[test]
fn real_hunk_lookalikes_and_crlf_parse() {
    let fixture = FixtureRepo::new();
    fixture.write("look", "-- a/x\n++ b/x\n");
    fixture.write("crlf", b"a\r\nb\r\n");
    let base = fixture.commit("base");
    fixture.write("look", "-- a/y\n++ b/y\n");
    fixture.write("crlf", b"a\r\nB\r\n");
    let head = fixture.commit("change");
    let files = parse(
        &fixture
            .git(fixture.work_path())
            .diff(&DiffSpec::TwoDot {
                from: base,
                to: head,
                paths: vec![],
            })
            .unwrap(),
    )
    .unwrap();
    assert!(
        files
            .iter()
            .flat_map(|file| &file.hunks)
            .flat_map(|hunk| &hunk.lines)
            .any(|line| line.text == b"B\r")
    );
}

#[test]
fn full_selection_files_and_diff_spec_use_snapshot_base() {
    let fixture = FixtureRepo::new();
    let fork = fixture.head();
    fixture.branch("pr");
    fixture.checkout("pr");
    fixture.write("pr", "pr\n");
    let pr = fixture.commit("pr");
    fixture.checkout("main");
    fixture.write("main", "main\n");
    let main = fixture.commit("main");
    let git = fixture.git(fixture.work_path());
    let snapshot = Snapshot::new(&git, &pr, &main).unwrap();
    let resolved = Selection::Full
        .resolve(&git, &snapshot, std::slice::from_ref(&pr))
        .unwrap();
    assert_eq!(resolved.base, fork);
    assert_eq!(resolved.files(&git).unwrap()[0].path.as_bytes(), b"pr");
    match resolved.diff_spec() {
        DiffSpec::ThreeDot { base_tip, head, .. } => {
            assert_eq!(base_tip, fork);
            assert_eq!(head, pr);
        }
        _ => panic!("full selection must use three-dot diff"),
    }
}

#[test]
fn merge_is_allowed_for_full_and_rejected_for_range() {
    let fixture = FixtureRepo::new();
    fixture.branch("pr");
    fixture.checkout("pr");
    fixture.write("p1", "p1\n");
    let p1 = fixture.commit("p1");
    fixture.checkout("main");
    fixture.write("main", "main\n");
    fixture.commit("main");
    fixture.checkout("pr");
    let merge = fixture.merge("main", "merge main");
    let git = fixture.git(fixture.work_path());
    let snapshot = Snapshot::new(&git, &merge, "main").unwrap();
    assert!(
        Selection::Full
            .resolve(&git, &snapshot, &[p1.clone(), merge.clone()])
            .is_ok()
    );
    let error = Selection::Commits {
        first: p1.clone(),
        last: merge.clone(),
    }
    .resolve(&git, &snapshot, &[p1, merge])
    .unwrap_err();
    assert_eq!(error.code(), "range_has_merge");
}
