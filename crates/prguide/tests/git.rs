mod support;

use prguide::git::{DiffSpec, Git, Refspec, parse_github_remote};
use std::{
    env, fs,
    process::{Command, Stdio},
    sync::mpsc,
    time::Duration,
};
use support::fixture_repo::FixtureRepo;

#[test]
fn github_remote_url_variants() {
    let matches = [
        ("https://github.com/octo/widget", ("octo", "widget")),
        ("https://github.com/octo/widget.git", ("octo", "widget")),
        ("https://github.com/octo/widget/", ("octo", "widget")),
        ("https://github.com/octo/widget.git/", ("octo", "widget")),
        ("https://GitHub.com/Octo/Widget.GIT", ("Octo", "Widget")),
        (
            "https://x-access-token:tok@github.com/octo/widget.git",
            ("octo", "widget"),
        ),
        ("https://github.com:443/octo/widget", ("octo", "widget")),
        ("http://github.com/octo/widget", ("octo", "widget")),
        ("ssh://git@github.com/octo/widget.git", ("octo", "widget")),
        ("ssh://git@github.com:22/octo/widget", ("octo", "widget")),
        ("git@github.com:octo/widget.git", ("octo", "widget")),
        ("git@github.com:octo/widget", ("octo", "widget")),
        ("github.com:octo/widget", ("octo", "widget")),
    ];
    for (url, expected) in matches {
        assert_eq!(
            parse_github_remote(url),
            Some((expected.0.into(), expected.1.into())),
            "{url}"
        );
    }
    for url in [
        "https://gitlab.com/octo/widget",
        "https://www.github.com/octo/widget",
        "https://gist.github.com/octo/widget",
        "https://github.com.evil.com/octo/widget",
        "https://notgithub.com/octo/widget",
        "https://github.com/octo",
        "https://github.com/octo/widget/extra",
        "https://github.com//widget",
        "git://github.com/octo/widget.git",
        "file:///tmp/widget.git",
        "/tmp/widget.git",
        "git@github.com:/octo/widget",
    ] {
        assert_eq!(parse_github_remote(url), None, "{url}");
    }
}

#[test]
fn remote_for_matches_real_remotes() {
    let fixture = FixtureRepo::new();
    let clone = fixture.clone("clone");
    let git = fixture.git(&clone);
    fixture.run(
        &clone,
        ["remote", "add", "z", "git@github.com:octo/widget.git"],
    );
    fixture.run(
        &clone,
        ["remote", "add", "a", "https://github.com/OCTO/Widget"],
    );
    fixture.run(
        &clone,
        [
            "remote",
            "add",
            "fork",
            "https://github.com/octo/widget-fork",
        ],
    );
    let origin = git.remote_for("OCTO", "Widget").unwrap();
    assert_eq!(origin.name, "origin");
    assert_eq!(origin.url, fixture.remote_path().display().to_string());
    fixture.run(&clone, ["remote", "remove", "origin"]);
    let matching = git.remote_for("octo", "widget").unwrap();
    assert_eq!(matching.name, "a");
    fixture.run(&clone, ["remote", "remove", "a"]);
    fixture.run(&clone, ["remote", "remove", "z"]);
    fixture.run(&clone, ["remote", "add", "gh", "gh:octo/widget"]);
    fixture.run(
        &clone,
        ["config", "url.https://github.com/.insteadOf", "gh:"],
    );
    let alias = git.remote_for("octo", "widget").unwrap();
    assert_eq!(alias.name, "gh");
    assert_eq!(alias.url, "https://github.com/octo/widget");
    fixture.run(&clone, ["remote", "remove", "gh"]);
    assert_eq!(
        git.remote_for("octo", "widget").unwrap_err().code(),
        "not_in_clone"
    );
}

#[test]
fn discover_finds_toplevel_and_common_dir() {
    let fixture = FixtureRepo::new();
    let clone = fixture.clone("clone");
    fs::create_dir_all(clone.join("sub/dir")).unwrap();
    let top = fixture.git(&clone).discover().unwrap();
    let sub = fixture.git(clone.join("sub/dir")).discover().unwrap();
    assert_eq!(top, sub);
    assert!(top.toplevel.is_absolute());
    assert_eq!(top.toplevel, fs::canonicalize(&clone).unwrap());
    assert_eq!(
        top.common_dir,
        fs::canonicalize(clone.join(".git")).unwrap()
    );
    let linked = fixture.path().join("linked");
    fixture.run(
        &clone,
        ["worktree", "add", "--detach", linked.to_str().unwrap()],
    );
    assert_eq!(
        fixture.git(&linked).discover().unwrap().common_dir,
        top.common_dir
    );
    assert_eq!(
        fixture.git(fixture.path()).discover().unwrap_err().code(),
        "not_in_clone"
    );
    assert_eq!(
        fixture
            .git(fixture.remote_path())
            .discover()
            .unwrap_err()
            .code(),
        "not_in_clone"
    );
}

#[test]
fn fetch_pr_head_and_base() {
    let fixture = FixtureRepo::new();
    let initial = fixture.head();
    fixture.push_pr(7, &initial);
    let clone = fixture.clone("clone");
    fixture.push_tag("hidden", &initial);
    let git = fixture.git(&clone);
    git.fetch("origin", &Refspec::for_pr(7, "main")).unwrap();
    assert_eq!(
        git.resolve_commit("refs/prguide/7/fetch/head").unwrap(),
        initial
    );
    assert_eq!(
        git.resolve_commit("refs/prguide/7/fetch/base").unwrap(),
        initial
    );
    assert!(git.resolve_commit("refs/tags/hidden").is_err());
    fixture.write("new", "x");
    let next = fixture.commit("next");
    fixture.run(fixture.work_path(), ["checkout", "--orphan", "replacement"]);
    fixture.run(fixture.work_path(), ["rm", "-rf", "."]);
    fixture.write("replacement", "x");
    let replacement = fixture.commit("replacement");
    assert_ne!(replacement, next);
    fixture.push_pr(7, &replacement);
    git.fetch("origin", &Refspec::for_pr(7, "main")).unwrap();
    assert_eq!(
        git.resolve_commit("refs/prguide/7/fetch/head").unwrap(),
        replacement
    );
    assert!(
        git.fetch("origin", &Refspec::for_pr(99, "main"))
            .unwrap_err()
            .to_string()
            .contains("refs/pull/99/head")
    );
    git.fetch("origin", &[]).unwrap();
}

#[test]
fn write_verify_delete_refs() {
    let fixture = FixtureRepo::new();
    let clone = fixture.clone("clone");
    let git = fixture.git(&clone);
    let sha = git.resolve_commit("HEAD").unwrap();
    let refs = (0..4)
        .map(|n| (format!("refs/prguide/7/v1/{n}"), sha.clone()))
        .collect::<Vec<_>>();
    git.write_refs(&refs).unwrap();
    git.write_refs(&refs).unwrap();
    assert!(git.verify_refs(&refs).unwrap().is_empty());
    fixture.write("different", "value");
    let actual = fixture.commit("actual");
    fixture.push("main");
    fixture.run(&clone, ["fetch", "origin", "main"]);
    fixture.run(&clone, ["update-ref", &refs[0].0, &actual]);
    fixture.run(&clone, ["update-ref", "-d", &refs[1].0]);
    assert_eq!(
        git.verify_refs(&refs).unwrap(),
        vec![
            prguide::git::RefMismatch {
                name: refs[0].0.clone(),
                expected: sha.clone(),
                actual: Some(actual)
            },
            prguide::git::RefMismatch {
                name: refs[1].0.clone(),
                expected: sha.clone(),
                actual: None
            },
        ]
    );
    let names = refs.iter().map(|(n, _)| n.clone()).collect::<Vec<_>>();
    git.delete_refs(&names).unwrap();
    git.delete_refs(&names).unwrap();
    assert!(git.list_refs("refs/prguide/7/").unwrap().is_empty());
}

#[test]
fn write_refs_is_atomic_and_guarded() {
    let fixture = FixtureRepo::new();
    let clone = fixture.clone("clone");
    let git = fixture.git(&clone);
    let sha = git.resolve_commit("HEAD").unwrap();
    assert_eq!(
        git.write_refs(&[
            ("refs/prguide/7/good".into(), sha.clone()),
            ("refs/prguide/7/bad".into(), "f".repeat(40))
        ])
        .unwrap_err()
        .code(),
        "git_failed"
    );
    assert!(git.list_refs("refs/prguide/7/").unwrap().is_empty());
    for bad in [
        "refs/heads/main",
        "refs/prguidex/1",
        "refs/prguide/7/a b",
        "refs/prguide/7/x\nupdate refs/heads/main deadbeef",
    ] {
        let valid = "refs/prguide/7/valid".to_owned();
        assert_eq!(
            git.write_refs(&[(valid.clone(), sha.clone()), (bad.into(), sha.clone())])
                .unwrap_err()
                .code(),
            "invalid_ref"
        );
        assert!(git.list_refs(&valid).unwrap().is_empty());
        git.write_refs(&[(valid.clone(), sha.clone())]).unwrap();
        assert_eq!(
            git.delete_refs(&[valid.clone(), bad.into()])
                .unwrap_err()
                .code(),
            "invalid_ref"
        );
        assert_eq!(git.resolve_commit(&valid).unwrap(), sha);
        git.delete_refs(&[valid]).unwrap();
    }
    for invalid in ["abc", &"A".repeat(40)] {
        assert_eq!(
            git.write_refs(&[("refs/prguide/7/x".into(), invalid.into())])
                .unwrap_err()
                .code(),
            "invalid_ref"
        );
    }
}

#[test]
fn gc_prune_keeps_pinned_objects() {
    let fixture = FixtureRepo::new();
    let clone = fixture.clone("clone");
    fixture.run(&clone, ["checkout", "-b", "scratch"]);
    fs::write(clone.join("p"), "p").unwrap();
    fixture.run(&clone, ["add", "p"]);
    fixture.run(&clone, ["commit", "-m", "p"]);
    let pinned = fixture.run(&clone, ["rev-parse", "HEAD"]);
    fs::write(clone.join("u"), "u").unwrap();
    fixture.run(&clone, ["add", "u"]);
    fixture.run(&clone, ["commit", "-m", "u"]);
    let unpinned = fixture.run(&clone, ["rev-parse", "HEAD"]);
    fixture.run(&clone, ["checkout", "main"]);
    fixture.run(&clone, ["branch", "-D", "scratch"]);
    fixture
        .git(&clone)
        .write_refs(&[("refs/prguide/7/p".into(), pinned.clone())])
        .unwrap();
    fixture.run(&clone, ["reflog", "expire", "--expire=now", "--all"]);
    fixture.run(&clone, ["gc", "--prune=now"]);
    fixture.run(&clone, ["cat-file", "-e", &pinned]);
    fixture.run(&clone, ["cat-file", "-e", &format!("{pinned}:p")]);
    assert!(
        fixture
            .git(&clone)
            .run(["cat-file", "-e", &unpinned])
            .is_err()
    );
}

#[test]
fn show_reads_blobs() {
    let fixture = FixtureRepo::new();
    fixture.write("text", "text\n");
    fixture.write("bin", b"a\0b");
    fixture.write("dir/inner", "inner");
    fixture.write("st*r.txt", "literal");
    fixture.write("stars.txt", "wild");
    let sha = fixture.commit("files");
    fixture.gitlink("link", &sha);
    let sha = fixture.commit("link");
    fixture.push("main");
    let clone = fixture.clone("clone");
    fs::create_dir_all(clone.join("sub")).unwrap();
    let git = fixture.git(clone.join("sub"));
    assert_eq!(git.show(&sha, "text").unwrap(), Some(b"text\n".to_vec()));
    assert_eq!(git.show(&sha, "bin").unwrap(), Some(b"a\0b".to_vec()));
    assert_eq!(git.show(&sha, "missing").unwrap(), None);
    assert_eq!(git.show(&sha, "dir").unwrap(), None);
    assert_eq!(git.show(&sha, "dir/").unwrap(), None);
    assert_eq!(
        git.show(&sha, "dir/inner").unwrap(),
        Some(b"inner".to_vec())
    );
    assert_eq!(git.show(&sha, "link").unwrap(), None);
    assert_eq!(
        git.show(&sha, "st*r.txt").unwrap(),
        Some(b"literal".to_vec())
    );
    assert_eq!(
        git.show(&"0".repeat(40), "text").unwrap_err().code(),
        "git_failed"
    );
}

#[test]
fn merge_base_and_parents() {
    let fixture = FixtureRepo::new();
    let root = fixture.head();
    fixture.branch("feature");
    fixture.checkout("feature");
    fixture.write("feature", "x");
    let feature = fixture.commit("feature");
    fixture.checkout("main");
    fixture.write("main", "x");
    let main = fixture.commit("main");
    let merge = fixture.merge("feature", "merge");
    let git = fixture.git(fixture.work_path());
    assert_eq!(git.merge_base(&main, &feature).unwrap(), Some(root.clone()));
    assert_eq!(git.rev_parents(&root).unwrap(), Vec::<String>::new());
    assert_eq!(git.rev_parents(&main).unwrap(), vec![root.clone()]);
    assert_eq!(git.rev_parents(&merge).unwrap(), vec![main, feature]);
    assert_eq!(git.resolve_commit("main").unwrap(), merge);
    fixture.run(fixture.work_path(), ["checkout", "--orphan", "orphan"]);
    fixture.run(fixture.work_path(), ["rm", "-rf", "."]);
    fixture.write("orphan", "x");
    let orphan = fixture.commit("orphan");
    assert_eq!(git.merge_base("main", &orphan).unwrap(), None);
    assert_eq!(
        git.merge_base("not-a-revision", "main").unwrap_err().code(),
        "git_failed"
    );
}

#[cfg(unix)]
#[test]
fn merge_base_uses_numeric_exit_status() {
    use std::os::unix::fs::PermissionsExt;

    let fixture = FixtureRepo::new();
    let bin = fixture.path().join("bin");
    fs::create_dir(&bin).unwrap();
    let git = bin.join("git");
    fs::write(&git, "#!/bin/sh\nexit 2\n").unwrap();
    let mut permissions = fs::metadata(&git).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&git, permissions).unwrap();

    assert_eq!(
        fixture
            .git(fixture.work_path())
            .env("PATH", bin)
            .merge_base("a", "b")
            .unwrap_err()
            .code(),
        "git_failed"
    );
}

fn expected_args(revision: &str, paths: &[&str]) -> Vec<String> {
    [
        "-c",
        "diff.suppressBlankEmpty=false",
        "-c",
        "core.attributesFile=/dev/null",
        "diff",
        "--no-ext-diff",
        "--no-textconv",
        "--no-color",
        "-M",
        "-l0",
        "--full-index",
        "-U3",
        "--src-prefix=a/",
        "--dst-prefix=b/",
        "--no-relative",
        "--inter-hunk-context=0",
        "-O/dev/null",
        "--submodule=short",
        "--ignore-submodules=none",
        "--diff-algorithm=myers",
        "--indent-heuristic",
    ]
    .into_iter()
    .map(String::from)
    .chain([revision, "--"].into_iter().map(String::from))
    .chain(paths.iter().map(|s| (*s).into()))
    .collect()
}

#[test]
fn diff_spec_args_are_fixed() {
    assert_eq!(
        DiffSpec::TwoDot {
            from: "a".into(),
            to: "b".into(),
            paths: vec!["x".into()]
        }
        .args(),
        expected_args("a..b", &["x"])
    );
    assert_eq!(
        DiffSpec::ThreeDot {
            base_tip: "a".into(),
            head: "b".into(),
            paths: vec![]
        }
        .args(),
        expected_args("a...b", &[])
    );
}

#[test]
fn three_dot_and_two_dot_diffs() {
    let fixture = FixtureRepo::new();
    fixture.write("delete", "gone");
    fixture.write("rename_source", "x");
    let base = fixture.commit("base");
    fixture.branch("feature");
    fixture.checkout("feature");
    fixture.write("feature", "feature");
    let feature = fixture.commit("feature");
    fixture.checkout("main");
    fixture.write("main", "main");
    let main = fixture.commit("main");
    fixture.checkout("main");
    fixture.push("main");
    fixture.checkout("feature");
    fixture.rename("rename_source", "renamed");
    fixture.remove("delete");
    let with_delete = fixture.commit("rename");
    fixture.set_executable("renamed", true);
    fixture.write("bin", b"a\0b");
    fixture.write("ü.txt", "utf8");
    fixture.gitlink("link", &base);
    let end = fixture.commit("details");
    fixture.push("feature");
    let clone = fixture.clone("clone");
    fixture.run(&clone, ["fetch", "origin", "feature"]);
    let git = fixture.git(&clone);
    let three = String::from_utf8(
        git.diff(&DiffSpec::ThreeDot {
            base_tip: main.clone(),
            head: end.clone(),
            paths: vec![],
        })
        .unwrap(),
    )
    .unwrap();
    let two = String::from_utf8(
        git.diff(&DiffSpec::TwoDot {
            from: main,
            to: end.clone(),
            paths: vec![],
        })
        .unwrap(),
    )
    .unwrap();
    assert!(three.contains("diff --git a/rename_source b/renamed"));
    assert!(!three.contains("diff --git a/main b/main"));
    assert!(two.contains("diff --git a/main b/main"));
    let renamed = String::from_utf8(
        git.diff(&DiffSpec::TwoDot {
            from: feature,
            to: with_delete.clone(),
            paths: vec![],
        })
        .unwrap(),
    )
    .unwrap();
    let detailed = String::from_utf8(
        git.diff(&DiffSpec::TwoDot {
            from: with_delete,
            to: end,
            paths: vec!["renamed".into()],
        })
        .unwrap(),
    )
    .unwrap();
    assert!(renamed.contains("rename from") && renamed.contains("rename to"));
    assert!(detailed.contains("old mode") && detailed.contains("new mode"));
    for path in ["bin", "ü.txt", "link"] {
        assert!(!detailed.contains(&format!("diff --git a/{path} b/{path}")));
    }
    assert!(
        three.contains("deleted file mode")
            && three.contains("Binary files")
            && three.contains("Subproject commit")
            && three.contains("diff --git a/ü.txt b/ü.txt")
    );
}

#[test]
fn diff_resolves_ref_names_and_ignores_commit_attributes() {
    let fixture = FixtureRepo::new();
    fixture.write(".gitattributes", "*.json -diff\n*.c diff=foo\n");
    fixture.write("attrs.json", "{\"old\": true}\n");
    fixture.write("attrs.c", "int old(void) { return 0; }\n");
    fixture.commit("base attributes");
    fixture.write("attrs.json", "{\"new\": true}\n");
    fixture.write("attrs.c", "int new(void) { return 1; }\n");
    fixture.commit("head attributes");
    fixture.push("main");
    let clone = fixture.clone("clone");
    fixture.run(&clone, ["config", "diff.foo.binary", "true"]);
    let git = fixture.git(&clone);
    let isolated = String::from_utf8(
        git.diff(&DiffSpec::TwoDot {
            from: "main~1".into(),
            to: "main".into(),
            paths: vec![],
        })
        .unwrap(),
    )
    .unwrap();
    assert!(
        isolated.contains("-{\"old\": true}")
            && isolated.contains("+{\"new\": true}")
            && isolated.contains("@@ -1 +1 @@"),
        "committed attributes must not affect diffs: {isolated}"
    );
    let ordinary = fixture.run(&clone, ["diff", "main~1", "main"]);
    assert!(
        ordinary.contains("Binary files"),
        "plain git diff must demonstrate that the attributes apply"
    );
}

#[test]
fn diff_works_in_blobless_clone_with_or_without_attributes() {
    for attributes in [false, true] {
        let fixture = FixtureRepo::new();
        if attributes {
            fixture.write(".gitattributes", "*.json -diff\n");
        }
        fixture.write("data.json", "{\"old\": true}\n");
        let from = fixture.commit("before");
        fixture.write("data.json", "{\"new\": true}\n");
        let to = fixture.commit("after");
        fixture.push("main");
        fixture.run(
            fixture.remote_path(),
            ["config", "uploadpack.allowFilter", "true"],
        );
        let full = fixture.clone("full");
        let blobless = fixture.path().join("blobless");
        fixture.run(
            fixture.path(),
            vec![
                "clone".into(),
                "--filter=blob:none".into(),
                "--no-checkout".into(),
                format!("file://{}", fixture.remote_path().display()),
                blobless.display().to_string(),
            ],
        );
        let spec = DiffSpec::TwoDot {
            from,
            to,
            paths: vec![],
        };
        assert_eq!(
            fixture.git(&blobless).diff(&spec).unwrap(),
            fixture.git(&full).diff(&spec).unwrap(),
            "attributes={attributes}"
        );
    }
}

#[cfg(unix)]
#[test]
fn diff_ignores_attributes_under_non_utf8_directory() {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;

    let fixture = FixtureRepo::new();
    fixture.write("data.json", "{\"old\": true}\n");
    let from = fixture.commit("before");
    fixture.write("data.json", "{\"new\": true}\n");
    let attribute = fixture
        .git(fixture.work_path())
        .run_with_stdin(["hash-object", "-w", "--stdin"], b"*.json -diff\n")
        .unwrap();
    let attribute = String::from_utf8(attribute).unwrap();
    let mut cacheinfo = format!("100644,{},", attribute.trim()).into_bytes();
    cacheinfo.extend_from_slice(b"bad\xff/.gitattributes");
    fixture
        .git(fixture.work_path())
        .run([
            OsString::from("update-index"),
            OsString::from("--add"),
            OsString::from("--cacheinfo"),
            OsString::from_vec(cacheinfo),
        ])
        .unwrap();
    fixture.run(fixture.work_path(), ["commit", "-am", "after"]);
    let to = fixture.head();
    fixture.push("main");
    let clone = fixture.clone("clone");
    let output = String::from_utf8(
        fixture
            .git(&clone)
            .diff(&DiffSpec::TwoDot {
                from,
                to,
                paths: vec![],
            })
            .unwrap(),
    )
    .unwrap();
    assert!(output.contains("+{\"new\": true}"), "{output}");
}
#[test]
fn diff_ignores_user_diff_config() {
    let fixture = FixtureRepo::new();
    let from = fixture.head();
    fixture.write("top", "one\n\nold\n1\n2\n3\n4\n5\n6\n7\n8\nold2\n");
    fixture.write("sub/in", "old\n");
    fixture.write("rename", "a1\na2\na3\na4\na5\na6\na7\na8\na9\na10\n");
    fixture.write("rename-two", "b1\nb2\nb3\nb4\nb5\nb6\nb7\nb8\nb9\nb10\n");
    fixture.write("attrs.json", "{\"old\": true}\n");
    let before_link = fixture.commit("before");
    fixture.rename("rename", "renamed");
    fixture.rename("rename-two", "renamed-two");
    fixture.write("renamed", "a1\na2\na3\na4\na5\na6\na7\na8\na9\nchanged\n");
    fixture.write(
        "renamed-two",
        "b1\nb2\nb3\nb4\nb5\nb6\nb7\nb8\nb9\nchanged\n",
    );
    fixture.write("attrs.json", "{\"new\": true}\n");
    fixture.write("top", "one\n\nnew\n1\n2\n3\n4\n5\n6\n7\n8\nnew2\n");
    fixture.write("sub/in", "new\n");
    fixture.gitlink("link", &from);
    let to = fixture.commit("after");
    fixture.push("main");
    let clone = fixture.clone("clone");
    fs::create_dir_all(clone.join("sub")).unwrap();
    fs::write(clone.join("order"), "sub/in\ntop\n").unwrap();
    let global_attributes = fixture.path().join("global-attributes");
    fs::write(&global_attributes, "*.json -diff\n").unwrap();
    let spec = DiffSpec::TwoDot {
        from: before_link,
        to,
        paths: vec![],
    };
    let baseline = fixture.git(&clone).diff(&spec).unwrap();
    assert!(
        String::from_utf8_lossy(&baseline).contains("rename from rename"),
        "fixture must require rename detection"
    );
    fs::write(clone.join(".gitattributes"), "*.json -diff\n").unwrap();
    fixture.run(&clone, ["add", ".gitattributes"]);
    for (key, value) in [
        ("diff.noprefix", "true"),
        ("diff.suppressBlankEmpty", "true"),
        ("diff.interHunkContext", "10"),
        ("diff.orderFile", "order"),
        ("diff.relative", "true"),
        ("diff.submodule", "log"),
        ("diff.ignoreSubmodules", "all"),
        ("diff.external", "false"),
        ("color.ui", "always"),
        ("diff.renames", "false"),
        ("diff.renameLimit", "1"),
        ("core.abbrev", "12"),
        ("core.attributesFile", global_attributes.to_str().unwrap()),
        ("diff.algorithm", "histogram"),
        ("diff.indentHeuristic", "false"),
    ] {
        fixture.run(&clone, ["config", key, value]);
    }
    fixture.run(
        &clone,
        [
            "config",
            "--global",
            "core.attributesFile",
            global_attributes.to_str().unwrap(),
        ],
    );
    assert_eq!(
        baseline,
        fixture
            .git(clone.join("sub"))
            .env("GIT_DIFF_OPTS", "--unified=0")
            .env("GIT_ATTR_SOURCE", "HEAD~1")
            .env("GIT_ICASE_PATHSPECS", "1")
            .env("GIT_GLOB_PATHSPECS", "1")
            .env("GIT_NOGLOB_PATHSPECS", "1")
            .env("GIT_CONFIG_PARAMETERS", "'diff.noprefix=true'")
            .env("GIT_CONFIG_COUNT", "1")
            .diff(&spec)
            .unwrap()
    );
}

#[test]
fn runner_reports_failures_and_ignores_git_dir() {
    let fixture = FixtureRepo::new();
    let clone = fixture.clone("clone");
    let other = FixtureRepo::new();
    other.write("x", "x");
    other.commit("x");
    other.push("main");
    let other_clone = other.clone("other");
    let git = fixture.git(&clone);
    let clone_head = git.resolve_commit("HEAD").unwrap();
    assert_ne!(
        other.git(&other_clone).resolve_commit("HEAD").unwrap(),
        clone_head
    );
    let error = git.run(["no-such-command"]).unwrap_err();
    assert_eq!(error.code(), "git_failed");
    assert!(error.to_string().contains("no-such-command"));
    assert!(error.to_string().contains("is not a git command"));
    assert_eq!(
        git.clone()
            .env("GIT_DIR", other_clone.join(".git"))
            .resolve_commit("HEAD")
            .unwrap(),
        clone_head
    );
}

#[test]
fn null_stdin_is_not_inherited_child() {
    if let Ok(dir) = env::var("PRGUIDE_NULL_STDIN_CHILD") {
        let (sender, receiver) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = sender.send(Git::new(dir).run(["hash-object", "--stdin"]));
        });
        let result = receiver
            .recv_timeout(Duration::from_secs(2))
            .expect("git inherited stdin instead of receiving null stdin")
            .unwrap();
        assert_eq!(result, b"e69de29bb2d1d6434b8b29ae775ad8c2e48c5391\n");
        return;
    }
    let fixture = FixtureRepo::new();
    let clone = fixture.clone("clone");
    let mut child = Command::new(env::current_exe().unwrap())
        .args([
            "--exact",
            "null_stdin_is_not_inherited_child",
            "--nocapture",
        ])
        .env("PRGUIDE_NULL_STDIN_CHILD", clone)
        .stdin(Stdio::piped())
        .spawn()
        .unwrap();
    let _stdin = child.stdin.take().unwrap();
    let status = child.wait().unwrap();
    assert!(status.success());
}

#[test]
fn list_refs_boundaries_and_guards() {
    let fixture = FixtureRepo::new();
    let clone = fixture.clone("clone");
    let git = fixture.git(&clone);
    let sha = git.resolve_commit("HEAD").unwrap();
    git.write_refs(&[
        ("refs/prguide/7/b".into(), sha.clone()),
        ("refs/prguide/7/a".into(), sha.clone()),
        ("refs/prguide/70/x".into(), sha.clone()),
    ])
    .unwrap();
    for prefix in ["refs/prguide/7", "refs/prguide/7/"] {
        assert_eq!(
            git.list_refs(prefix)
                .unwrap()
                .into_iter()
                .map(|x| x.0)
                .collect::<Vec<_>>(),
            vec!["refs/prguide/7/a", "refs/prguide/7/b"]
        );
    }
    assert!(git.list_refs("refs/prguide/7*").unwrap().is_empty());
    for prefix in ["", "heads/"] {
        assert_eq!(git.list_refs(prefix).unwrap_err().code(), "invalid_ref");
    }
    assert!(git.verify_refs(&[]).unwrap().is_empty());
}

#[test]
fn missing_git_binary_is_git_missing() {
    let fixture = FixtureRepo::new();
    let clone = fixture.clone("clone");
    assert_eq!(
        fixture
            .git(&clone)
            .env("PATH", fixture.path())
            .resolve_commit("HEAD")
            .unwrap_err()
            .code(),
        "git_missing"
    );
}
#[test]
fn remote_for_without_remotes() {
    let fixture = FixtureRepo::new();
    let clone = fixture.clone("clone");
    fixture.run(&clone, ["remote", "remove", "origin"]);
    assert_eq!(
        fixture
            .git(&clone)
            .remote_for("octo", "widget")
            .unwrap_err()
            .code(),
        "not_in_clone"
    );
}
#[test]
fn diff_path_filter_from_subdirectory() {
    let fixture = FixtureRepo::new();
    fixture.write("top", "one\ntwo\nthree\n");
    let from = fixture.commit("before");
    fixture.write("top", "one\nchanged\nthree\n");
    fixture.write("other", "changed");
    let to = fixture.commit("change");
    fixture.push("main");
    let clone = fixture.clone("clone");
    fs::create_dir_all(clone.join("sub")).unwrap();
    let spec = DiffSpec::TwoDot {
        from,
        to,
        paths: vec!["top".into()],
    };
    let top = fixture.git(&clone).diff(&spec).unwrap();
    assert!(!top.is_empty());
    assert!(!String::from_utf8_lossy(&top).contains("diff --git a/other b/other"));
    assert_eq!(
        top,
        fixture
            .git(clone.join("sub"))
            .env("GIT_DIFF_OPTS", "--unified=0")
            .diff(&spec)
            .unwrap()
    );
}
