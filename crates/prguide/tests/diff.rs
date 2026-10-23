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
    let output = b"diff --git \"a/ta\\tb\\001\\177\xe9\" \"b/ta\\tb\\001\\177\xe9\"\nold mode 100644\nnew mode 100755\n";
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
    let range_files = range.files(&git).unwrap();
    assert_eq!(
        range_files
            .iter()
            .map(|file| file.path.as_bytes())
            .collect::<Vec<_>>(),
        vec![b"two".as_slice()]
    );
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

#[test]
fn unquote_every_escape() {
    let file = parse(
        b"diff --git \"a/\\a\\b\\t\\n\\v\\f\\r\\\"\\\\\\001\\177\xe9\" \"b/\\a\\b\\t\\n\\v\\f\\r\\\"\\\\\\001\\177\xe9\"\nold mode 100644\nnew mode 100755\n",
    )
    .unwrap()
    .remove(0);
    assert_eq!(
        file.path.as_bytes(),
        b"\x07\x08\t\n\x0b\x0c\r\"\\\x01\x7f\xe9"
    );
    let octal = parse(
        b"diff --git a/\xe9 b/y\nsimilarity index 100%\nrename from \"\\351\"\nrename to y\n",
    )
    .unwrap()
    .remove(0);
    assert_eq!(octal.old_path.unwrap().as_bytes(), b"\xe9");
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
    let (_, message) = parse_error(b"diff --git a/x b/x\ncopy from x\n");
    assert_eq!(message, "unknown line");
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
    for selection in [
        Selection::Commits {
            first: p1[..7].into(),
            last: p2.clone(),
        },
        Selection::Commits {
            first: "main".into(),
            last: p2.clone(),
        },
        Selection::Commits {
            first: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
            last: p2.clone(),
        },
        Selection::Commits {
            first: p1.clone(),
            last: "main".into(),
        },
    ] {
        assert_eq!(
            selection
                .resolve(&git, &snapshot, &[p1.clone(), p2.clone()])
                .unwrap_err()
                .code(),
            "range_not_in_pr"
        );
    }
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
    let look = files
        .iter()
        .find(|file| file.path.as_bytes() == b"look")
        .unwrap();
    assert_eq!(
        look.hunks[0]
            .lines
            .iter()
            .map(|line| (line.kind, line.text.as_slice()))
            .collect::<Vec<_>>(),
        vec![
            (LineKind::Removed, b"-- a/x".as_slice()),
            (LineKind::Removed, b"++ b/x".as_slice()),
            (LineKind::Added, b"-- a/y".as_slice()),
            (LineKind::Added, b"++ b/y".as_slice()),
        ]
    );
    let crlf = files
        .iter()
        .find(|file| file.path.as_bytes() == b"crlf")
        .unwrap();
    assert!(crlf.hunks[0].lines.iter().any(|line| line.text == b"B\r"));
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
    assert_eq!(
        git.diff(&resolved.diff_spec()).unwrap(),
        git.diff(&DiffSpec::ThreeDot {
            base_tip: main.clone(),
            head: pr.clone(),
            paths: vec![],
        })
        .unwrap()
    );
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
    let main = fixture.commit("main");
    fixture.checkout("pr");
    let merge = fixture.merge("main", "merge main");
    fixture.write("p3", "p3\n");
    let p3 = fixture.commit("p3");
    let git = fixture.git(fixture.work_path());
    let snapshot = Snapshot::new(&git, &p3, &main).unwrap();
    let commits = vec![p1.clone(), merge.clone(), p3.clone()];
    let full = Selection::Full.resolve(&git, &snapshot, &commits).unwrap();
    let paths = full
        .files(&git)
        .unwrap()
        .into_iter()
        .map(|file| file.path.as_bytes().to_vec())
        .collect::<Vec<_>>();
    assert!(paths.contains(&b"p1".to_vec()));
    assert!(paths.contains(&b"p3".to_vec()));
    assert!(!paths.contains(&b"main".to_vec()));
    for selection in [
        Selection::Commits {
            first: p1.clone(),
            last: p3.clone(),
        },
        Selection::Commits {
            first: merge.clone(),
            last: merge.clone(),
        },
    ] {
        assert_eq!(
            selection
                .resolve(&git, &snapshot, &commits)
                .unwrap_err()
                .code(),
            "range_has_merge"
        );
    }
    let p3_range = Selection::Commits {
        first: p3.clone(),
        last: p3.clone(),
    }
    .resolve(&git, &snapshot, &commits)
    .unwrap();
    assert_eq!(p3_range.base, merge);
    assert!(
        Selection::Commits {
            first: p1.clone(),
            last: p1,
        }
        .resolve(&git, &snapshot, &commits)
        .is_ok()
    );
}

#[test]
fn noncanonical_octal_and_malformed_quoted_headers_are_rejected() {
    for input in [
        b"diff --git \"a/x\\351\" \"b/x\\351\"\nold mode 100644\nnew mode 100755\n".as_slice(),
        b"diff --git \"a/x\"b/x\nold mode 100644\nnew mode 100755\n",
        b"diff --git \"a/x\" \"b/x\" trailing\nold mode 100644\nnew mode 100755\n",
        b"diff --git a/x\" b/x\"\nold mode 100644\nnew mode 100755\n",
    ] {
        assert_eq!(parse_error(input).0, 1);
    }
}

#[test]
#[cfg(unix)]
fn real_every_file_kind() {
    use std::fs;

    let fixture = FixtureRepo::new();
    fixture.write("modified", "before\n");
    fixture.write("deleted", "delete\n");
    fixture.write("pure-rename", "pure rename\n");
    fixture.write("edited-rename", "one\ntwo\nthree\nfour\nfive\n");
    fixture.write("rename-chmod", "rename chmod\n");
    fixture.write("chmod", "chmod\n");
    fixture.write("edit-chmod", "before\n");
    fixture.write("binary-modified", b"old\0binary");
    fixture.write("binary-deleted", b"delete\0binary");
    let mut binary_rename = vec![b'x'; 2048];
    binary_rename[0] = 0;
    fixture.write("binary-rename", binary_rename);
    fixture.write("binchmod", b"old\0binary");
    fixture.write("typechg", "regular\n");
    fixture.gitlink("link-updated", OID_A);
    fixture.gitlink("link-deleted", OID_B);
    let base = fixture.commit("base");

    fixture.write("modified", "after\n");
    fixture.remove("deleted");
    fixture.rename("pure-rename", "pure-renamed");
    fixture.rename("edited-rename", "edited-renamed");
    fixture.write("edited-renamed", "one\ntwo\nTHREE\nfour\nfive\n");
    fixture.rename("rename-chmod", "rename-chmodmed");
    fixture.set_executable("rename-chmodmed", true);
    fixture.set_executable("chmod", true);
    fixture.write("edit-chmod", "after\n");
    fixture.set_executable("edit-chmod", true);
    fixture.write("added", "added\n");
    fixture.write("empty-added", b"");
    fixture.write("binary-modified", b"new\0binary");
    fixture.remove("binary-deleted");
    let mut renamed_binary = vec![b'x'; 2048];
    renamed_binary[0] = 0;
    renamed_binary[1024] = b'y';
    fixture.rename("binary-rename", "binary renamed");
    fixture.write("binary renamed", renamed_binary);
    fixture.write("binchmod", b"new\0binary");
    fixture.set_executable("binchmod", true);
    fixture.gitlink("link-added", OID_A);
    fixture.gitlink("link-updated", OID_B);
    fixture.run(
        fixture.work_path(),
        ["update-index", "--force-remove", "link-deleted"],
    );
    fs::remove_dir_all(fixture.work_path().join("link-deleted")).unwrap();
    fixture.remove("typechg");
    std::os::unix::fs::symlink("target", fixture.work_path().join("typechg")).unwrap();
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
    let file = |path: &[u8]| {
        files
            .iter()
            .find(|file| file.path.as_bytes() == path)
            .unwrap()
    };
    assert_eq!(file(b"modified").status, FileStatus::Modified);
    assert_eq!(file(b"added").status, FileStatus::Added);
    assert_eq!(file(b"deleted").status, FileStatus::Deleted);
    assert_eq!(file(b"pure-renamed").status, FileStatus::Renamed);
    assert_eq!(
        file(b"pure-renamed").old_path.as_ref().unwrap().as_bytes(),
        b"pure-rename"
    );
    assert_eq!(file(b"pure-renamed").old_mode, None);
    assert_eq!(file(b"edited-renamed").similarity, Some(75));
    assert_eq!(file(b"edited-renamed").old_mode, Some(0o100644));
    assert!(file(b"rename-chmodmed").mode_only);
    assert!(file(b"chmod").mode_only);
    assert!(!file(b"edit-chmod").mode_only);
    assert!(file(b"binary-modified").binary);
    assert!(file(b"binary-deleted").binary);
    assert!(file(b"binary renamed").binary);
    assert!(!file(b"binchmod").mode_only);
    assert!(file(b"binchmod").binary);
    assert_eq!(file(b"binchmod").old_mode, Some(0o100644));
    assert_eq!(file(b"binchmod").new_mode, Some(0o100755));
    assert!(file(b"link-added").submodule);
    assert!(file(b"link-updated").submodule);
    assert!(file(b"link-deleted").submodule);
    assert!(file(b"link-added").hunks.is_empty());
    assert_eq!(file(b"empty-added").hunks.len(), 0);
    assert!(
        files
            .iter()
            .filter(|file| file.path.as_bytes() == b"typechg")
            .count()
            == 2
    );
    insta::assert_snapshot!(format!("{files:#?}"));
}

#[cfg(unix)]
#[test]
fn real_quoted_and_raw_paths() {
    use std::{ffi::OsString, os::unix::ffi::OsStringExt};

    let fixture = FixtureRepo::new();
    let git = fixture.git(fixture.work_path());
    let old = String::from_utf8(
        git.run_with_stdin(["hash-object", "-w", "--stdin"], b"old\n")
            .unwrap(),
    )
    .unwrap();
    let new = String::from_utf8(
        git.run_with_stdin(["hash-object", "-w", "--stdin"], b"new\n")
            .unwrap(),
    )
    .unwrap();
    let rename = String::from_utf8(
        git.run_with_stdin(["hash-object", "-w", "--stdin"], b"rename\n")
            .unwrap(),
    )
    .unwrap();
    let paths: [&[u8]; 12] = [
        b"sp ace.txt",
        b"trail.txt ",
        b"x b/y",
        b"ta\tb.txt",
        b"q\"uote.txt",
        b"back\\slash.txt",
        b"new\nline.txt",
        b"ctl\x01.txt",
        b"del\x7f.txt",
        b"esc\x1b.txt",
        "ü.txt".as_bytes(),
        b"caf\xe9.txt",
    ];
    let cacheinfo = |mode: &str, oid: &str, path: &[u8]| {
        let mut value = format!("{mode},{oid},").into_bytes();
        value.extend_from_slice(path);
        OsString::from_vec(value)
    };
    let mut add = vec![OsString::from("update-index"), OsString::from("--add")];
    for path in &paths {
        add.push(OsString::from("--cacheinfo"));
        add.push(cacheinfo("100644", old.trim(), path));
    }
    add.extend([
        OsString::from("--cacheinfo"),
        cacheinfo("100644", rename.trim(), b"r\tfrom.txt"),
    ]);
    git.run(add).unwrap();
    fixture.run(fixture.work_path(), ["commit", "-m", "base"]);
    let base = fixture.head();
    let mut change = vec![OsString::from("update-index")];
    for path in &paths {
        let mode = if *path == b"x b/y" || *path == b"ta\tb.txt" {
            "100755"
        } else {
            "100644"
        };
        let oid = if mode == "100755" {
            old.trim()
        } else {
            new.trim()
        };
        change.push(OsString::from("--cacheinfo"));
        change.push(cacheinfo(mode, oid, path));
    }
    change.extend([
        OsString::from("--force-remove"),
        OsString::from("r\tfrom.txt"),
        OsString::from("--add"),
        OsString::from("--cacheinfo"),
        cacheinfo("100644", rename.trim(), b"sp to.txt"),
    ]);
    git.run(change).unwrap();
    fixture.run(fixture.work_path(), ["commit", "-m", "change"]);
    let head = fixture.head();
    let files = parse(
        &git.diff(&DiffSpec::TwoDot {
            from: base,
            to: head,
            paths: vec![],
        })
        .unwrap(),
    )
    .unwrap();
    for path in paths {
        let found = files
            .iter()
            .find(|file| file.path.as_bytes() == path)
            .unwrap();
        assert_eq!(found.path.to_str().is_none(), path == b"caf\xe9.txt");
        assert!(!found.path.as_bytes().ends_with(b"\t"));
    }
    let renamed = files
        .iter()
        .find(|file| file.path.as_bytes() == b"sp to.txt")
        .unwrap();
    assert_eq!(
        renamed.old_path.as_ref().unwrap().as_bytes(),
        b"r\tfrom.txt"
    );
}

#[test]
fn real_hunk_headers_cover_empty_sides_and_no_section() {
    let fixture = FixtureRepo::new();
    fixture.write("counts", "one\ntwo\nthree\nfour\nfive\n");
    fixture.write("empty-old", b"");
    fixture.write("empty-new", "one\n");
    fixture.write("plain", "before\n");
    let base = fixture.commit("base");
    fixture.write("counts", "ONE\n");
    fixture.write("empty-old", "new\n");
    fixture.write("empty-new", b"");
    fixture.write("plain", "after\n");
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
    let file = |path: &[u8]| {
        files
            .iter()
            .find(|file| file.path.as_bytes() == path)
            .unwrap()
    };
    assert_eq!(file(b"counts").hunks[0].new_lines, 1);
    assert_eq!(file(b"empty-old").hunks[0].old_lines, 0);
    assert_eq!(file(b"empty-old").hunks[0].new_lines, 1);
    assert_eq!(file(b"empty-new").hunks[0].old_lines, 1);
    assert_eq!(file(b"empty-new").hunks[0].new_lines, 0);
    assert_eq!(file(b"plain").hunks[0].section, None);
}

#[test]
fn real_no_newline_markers_cover_each_side_and_context() {
    let fixture = FixtureRepo::new();
    fixture.write("old-only", b"old");
    fixture.write("new-only", b"old\n");
    fixture.write("both", b"old");
    fixture.write("context", b"old\nsame");
    let base = fixture.commit("base");
    fixture.write("old-only", b"new\n");
    fixture.write("new-only", b"new");
    fixture.write("both", b"new");
    fixture.write("context", b"new\nsame");
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
    let markers = |path: &[u8]| {
        files
            .iter()
            .find(|file| file.path.as_bytes() == path)
            .unwrap()
            .hunks[0]
            .lines
            .iter()
            .map(|line| (line.kind, line.no_newline))
            .collect::<Vec<_>>()
    };
    assert_eq!(
        markers(b"old-only"),
        vec![(LineKind::Removed, true), (LineKind::Added, false)]
    );
    assert_eq!(
        markers(b"new-only"),
        vec![(LineKind::Removed, false), (LineKind::Added, true)]
    );
    assert_eq!(
        markers(b"both"),
        vec![(LineKind::Removed, true), (LineKind::Added, true)]
    );
    assert_eq!(
        markers(b"context"),
        vec![
            (LineKind::Removed, false),
            (LineKind::Added, false),
            (LineKind::Context, true),
        ]
    );
}

#[test]
fn real_three_dot_and_two_dot_parse() {
    let fixture = FixtureRepo::new();
    fixture.write("delete", "gone");
    fixture.write("rename_source", "x");
    let base = fixture.commit("base");
    fixture.branch("feature");
    fixture.checkout("feature");
    fixture.write("feature", "feature");
    fixture.commit("feature");
    fixture.checkout("main");
    fixture.write("main", "main");
    let main = fixture.commit("main");
    fixture.checkout("feature");
    fixture.rename("rename_source", "renamed");
    fixture.remove("delete");
    fixture.commit("rename");
    fixture.set_executable("renamed", true);
    fixture.write("bin", b"a\0b");
    fixture.write("ü.txt", "utf8");
    fixture.gitlink("link", &base);
    let end = fixture.commit("details");
    let git = fixture.git(fixture.work_path());
    let names = |spec| {
        parse(&git.diff(&spec).unwrap())
            .unwrap()
            .into_iter()
            .map(|file| (file.path.as_bytes().to_vec(), file.status))
            .collect::<Vec<_>>()
    };
    let three = names(DiffSpec::ThreeDot {
        base_tip: main.clone(),
        head: end.clone(),
        paths: vec![],
    });
    assert_eq!(
        three,
        vec![
            (b"bin".to_vec(), FileStatus::Added),
            (b"delete".to_vec(), FileStatus::Deleted),
            (b"feature".to_vec(), FileStatus::Added),
            (b"link".to_vec(), FileStatus::Added),
            (b"renamed".to_vec(), FileStatus::Renamed),
            ("ü.txt".as_bytes().to_vec(), FileStatus::Added),
        ]
    );
    let mut two = names(DiffSpec::TwoDot {
        from: main,
        to: end,
        paths: vec![],
    });
    assert!(two.contains(&(b"main".to_vec(), FileStatus::Deleted)));
    two.retain(|(path, _)| path != b"main");
    assert_eq!(two, three);
}

#[test]
fn range_not_consecutive_checks_merges_before_chains() {
    let fixture = FixtureRepo::new();
    fixture.branch("pr");
    fixture.branch("side");
    fixture.checkout("pr");
    fixture.write("p1", "p1\n");
    let p1 = fixture.commit("p1");
    fixture.checkout("side");
    fixture.write("x", "x\n");
    let x = fixture.commit("x");
    fixture.checkout("pr");
    fixture.write("p2", "p2\n");
    let p2 = fixture.commit("p2");
    let merge = fixture.merge("side", "merge side");
    let git = fixture.git(fixture.work_path());
    let snapshot = Snapshot::new(&git, &merge, "main").unwrap();
    let commits = vec![p1.clone(), x.clone(), p2.clone(), merge.clone()];
    for (first, last) in [(x.clone(), p2.clone()), (p1.clone(), x.clone())] {
        let error = Selection::Commits { first, last }
            .resolve(&git, &snapshot, &commits)
            .unwrap_err();
        assert_eq!(error.code(), "range_not_in_pr");
    }
    let merge_error = Selection::Commits {
        first: x,
        last: merge,
    }
    .resolve(&git, &snapshot, &commits)
    .unwrap_err();
    assert_eq!(merge_error.code(), "range_has_merge");
    let paths = Selection::Full
        .resolve(&git, &snapshot, &commits)
        .unwrap()
        .files(&git)
        .unwrap()
        .into_iter()
        .map(|file| file.path.as_bytes().to_vec())
        .collect::<Vec<_>>();
    assert!(paths.contains(&b"p1".to_vec()));
    assert!(paths.contains(&b"p2".to_vec()));
    assert!(paths.contains(&b"x".to_vec()));
}
