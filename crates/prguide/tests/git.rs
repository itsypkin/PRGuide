mod support;

use prguide::git::{DiffSpec, GitError, Refspec, parse_github_remote};
use std::fs;
use support::fixture_repo::FixtureRepo;

#[test]
fn github_remote_url_variants() {
    for url in [
        "https://github.com/octo/widget",
        "https://github.com/octo/widget.git/",
        "https://GitHub.com/Octo/Widget.Git",
        "https://x:t@github.com/octo/widget.git",
        "https://github.com:443/octo/widget",
        "http://github.com/octo/widget",
        "ssh://git@github.com/octo/widget.git",
        "git@github.com:octo/widget.git",
        "github.com:octo/widget",
    ] {
        assert!(parse_github_remote(url).is_some(), "{url}");
    }
    for url in [
        "https://gitlab.com/octo/widget",
        "https://github.com//octo/widget",
        "https://github.com/octo",
        "https://github.com/octo/widget/x",
        "git://github.com/octo/widget",
        "file:///tmp/a",
        "git@github.com:/octo/widget",
    ] {
        assert_eq!(parse_github_remote(url), None, "{url}");
    }
    assert_eq!(
        parse_github_remote("https://GitHub.com/Octo/Widget.GIT"),
        Some(("Octo".into(), "Widget".into()))
    );
}

#[test]
fn remote_for_matches_real_remotes_and_no_remotes() {
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
    assert_eq!(git.remote_for("octo", "widget").unwrap().name, "origin");
    fixture.run(&clone, ["remote", "remove", "origin"]);
    assert_eq!(git.remote_for("octo", "widget").unwrap().name, "a");
    fixture.run(&clone, ["remote", "remove", "a"]);
    fixture.run(&clone, ["remote", "remove", "z"]);
    assert_eq!(
        git.remote_for("octo", "widget").unwrap_err().code(),
        "not_in_clone"
    );
}

#[test]
fn discover_finds_toplevel_and_common_dir() {
    let fixture = FixtureRepo::new();
    let clone = fixture.clone("clone");
    fs::create_dir_all(clone.join("sub")).unwrap();
    let top = fixture.git(&clone).discover().unwrap();
    let sub = fixture.git(clone.join("sub")).discover().unwrap();
    assert_eq!(top, sub);
    assert!(top.toplevel.is_absolute());
    assert_eq!(
        top.common_dir,
        fs::canonicalize(clone.join(".git")).unwrap()
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
    assert!(git.resolve_commit("refs/tags/hidden").is_err());
    fixture.write("new", "x");
    let next = fixture.commit("next");
    fixture.push_pr(7, &next);
    git.fetch("origin", &Refspec::for_pr(7, "main")).unwrap();
    assert_eq!(
        git.resolve_commit("refs/prguide/7/fetch/head").unwrap(),
        next
    );
    assert!(
        git.fetch("origin", &Refspec::for_pr(99, "main"))
            .unwrap_err()
            .to_string()
            .contains("refs/pull/99/head")
    );
}

#[test]
fn write_verify_delete_refs() {
    let fixture = FixtureRepo::new();
    let clone = fixture.clone("clone");
    let git = fixture.git(&clone);
    let sha = git.resolve_commit("HEAD").unwrap();
    let refs = vec![
        ("refs/prguide/7/a".into(), sha.clone()),
        ("refs/prguide/7/b".into(), sha.clone()),
    ];
    git.write_refs(&refs).unwrap();
    assert!(git.verify_refs(&refs).unwrap().is_empty());
    fixture.run(&clone, ["update-ref", "refs/prguide/7/a", &"0".repeat(40)]);
    let mismatch = git.verify_refs(&refs).unwrap();
    assert_eq!(mismatch.len(), 1);
    assert_eq!(mismatch[0].name, "refs/prguide/7/a");
    assert_eq!(
        git.write_refs(&[("refs/heads/main".into(), sha.clone())])
            .unwrap_err()
            .code(),
        "invalid_ref"
    );
    assert_eq!(
        git.write_refs(&[("refs/prguide/7/x".into(), "A".repeat(40))])
            .unwrap_err()
            .code(),
        "invalid_ref"
    );
    assert!(
        git.write_refs(&[
            ("refs/prguide/7/good".into(), sha),
            ("refs/prguide/7/bad".into(), "f".repeat(40))
        ])
        .is_err()
    );
    assert!(git.list_refs("refs/prguide/7/good").unwrap().is_empty());
    git.delete_refs(&refs.iter().map(|(n, _)| n.clone()).collect::<Vec<_>>())
        .unwrap();
    git.delete_refs(&refs.iter().map(|(n, _)| n.clone()).collect::<Vec<_>>())
        .unwrap();
    assert!(git.list_refs("refs/prguide/7/").unwrap().is_empty());
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
    let root = fixture.head();
    fixture.branch("feature");
    fixture.checkout("feature");
    fixture.write("bin", b"a\0b");
    let feature = fixture.commit("feature");
    fixture.checkout("main");
    fixture.write("main", "x");
    let main = fixture.commit("main");
    let merge = fixture.merge("feature", "merge");
    fixture.push("main");
    let clone = fixture.clone("clone");
    let git = fixture.git(&clone);
    assert_eq!(git.show(&feature, "bin").unwrap(), Some(b"a\0b".to_vec()));
    assert_eq!(git.show(&feature, "none").unwrap(), None);
    assert_eq!(git.merge_base(&main, &feature).unwrap(), Some(root));
    assert_eq!(git.rev_parents(&merge).unwrap(), vec![main, feature]);
    assert_eq!(git.resolve_commit("main").unwrap(), merge);
}

#[test]
fn diff_spec_args_are_fixed() {
    let spec = DiffSpec::TwoDot {
        from: "a".into(),
        to: "b".into(),
        paths: vec!["x".into()],
    };
    assert_eq!(
        spec.args(),
        vec![
            "-c",
            "diff.suppressBlankEmpty=false",
            "diff",
            "--no-ext-diff",
            "--no-textconv",
            "--no-color",
            "-M",
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
            "a..b",
            "--",
            "x"
        ]
    );
}

#[test]
fn diff_ignores_user_diff_config() {
    let fixture = FixtureRepo::new();
    let from = fixture.head();
    fixture.write("top", "one\n\nold\n");
    fixture.write("sub/in", "x\n");
    let to = fixture.commit("change");
    fixture.push("main");
    let clone = fixture.clone("clone");
    fs::create_dir_all(clone.join("sub")).unwrap();
    let spec = DiffSpec::TwoDot {
        from,
        to,
        paths: vec![],
    };
    let baseline = fixture.git(&clone).diff(&spec).unwrap();
    fixture.run(&clone, ["config", "diff.noprefix", "true"]);
    fixture.run(&clone, ["config", "diff.suppressBlankEmpty", "true"]);
    fixture.run(&clone, ["config", "diff.interHunkContext", "10"]);
    fixture.run(&clone, ["config", "diff.relative", "true"]);
    fixture.run(&clone, ["config", "diff.renames", "false"]);
    assert_eq!(
        baseline,
        fixture.git(clone.join("sub")).diff(&spec).unwrap()
    );
}

#[test]
fn runner_reports_failures_ignores_git_dir_and_null_stdin() {
    let fixture = FixtureRepo::new();
    let clone = fixture.clone("clone");
    let other = FixtureRepo::new();
    let other_clone = other.clone("other");
    other.write("x", "x");
    other.commit("x");
    let git = fixture.git(&clone);
    assert!(
        git.run(["no-such-command"])
            .unwrap_err()
            .to_string()
            .contains("no-such-command")
    );
    assert_eq!(
        git.clone()
            .env("GIT_DIR", other_clone.join(".git"))
            .resolve_commit("HEAD")
            .unwrap(),
        git.resolve_commit("HEAD").unwrap()
    );
    assert_eq!(
        git.run(["hash-object", "--stdin"]).unwrap(),
        b"e69de29bb2d1d6434b8b29ae775ad8c2e48c5391\n"
    );
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
        ("refs/prguide/70/x".into(), sha),
    ])
    .unwrap();
    assert_eq!(
        git.list_refs("refs/prguide/7/")
            .unwrap()
            .iter()
            .map(|x| x.0.as_str())
            .collect::<Vec<_>>(),
        vec!["refs/prguide/7/a", "refs/prguide/7/b"]
    );
    assert!(git.list_refs("refs/prguide/7*").unwrap().is_empty());
    assert_eq!(git.list_refs("").unwrap_err().code(), "invalid_ref");
    assert!(git.verify_refs(&[]).unwrap().is_empty());
    assert_eq!(
        git.env("PATH", fixture.path())
            .resolve_commit("HEAD")
            .unwrap_err()
            .code(),
        "git_missing"
    );
}

#[test]
fn git_error_codes_are_stable() {
    assert_eq!(GitError::Missing.code(), "git_missing");
}

#[test]
fn write_refs_is_atomic_and_guarded() {
    let fixture = FixtureRepo::new();
    let clone = fixture.clone("clone");
    let git = fixture.git(&clone);
    let sha = git.resolve_commit("HEAD").unwrap();
    assert!(
        git.write_refs(&[
            ("refs/prguide/7/good".into(), sha),
            ("refs/prguide/7/bad".into(), "f".repeat(40)),
        ])
        .is_err()
    );
    assert!(git.list_refs("refs/prguide/7/").unwrap().is_empty());
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
    assert_eq!(
        fixture
            .git(fixture.work_path())
            .merge_base(&main, &feature)
            .unwrap(),
        Some(root)
    );
}

#[test]
fn three_dot_and_two_dot_diffs() {
    let fixture = FixtureRepo::new();
    let from = fixture.head();
    fixture.write("added", "x");
    let to = fixture.commit("feature");
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
    assert!(output.contains("new file mode"));
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
    let from = fixture.head();
    fixture.write("top", "changed");
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
    assert_eq!(top, fixture.git(clone.join("sub")).diff(&spec).unwrap());
}
