mod support;

use prguide::git::{DiffSpec, GitError, Refspec, parse_github_remote};
use support::fixture_repo::FixtureRepo;

#[test]
fn github_urls_and_error_codes_are_stable() {
    assert_eq!(
        parse_github_remote("https://GitHub.com/Octo/Widget.GIT"),
        Some(("Octo".into(), "Widget".into()))
    );
    assert_eq!(
        parse_github_remote("git@github.com:octo/widget.git"),
        Some(("octo".into(), "widget".into()))
    );
    assert_eq!(
        parse_github_remote("https://www.github.com/octo/widget"),
        None
    );
    assert_eq!(GitError::Missing.code(), "git_missing");
}

#[test]
fn clone_refs_fetch_and_reading_work() {
    let fixture = FixtureRepo::new();
    fixture.write("code.txt", b"one\n");
    let head = fixture.commit("code");
    fixture.push("main");
    fixture.push_pr(7, &head);
    let clone = fixture.clone("clone");
    let git = fixture.git(&clone);
    assert_eq!(git.remote_for("OCTO", "Widget").unwrap().name, "origin");
    git.fetch("origin", &Refspec::for_pr(7, "main")).unwrap();
    assert_eq!(
        git.resolve_commit("refs/prguide/7/fetch/head").unwrap(),
        head
    );
    assert_eq!(
        git.show(&head, "code.txt").unwrap(),
        Some(b"one\n".to_vec())
    );
    git.write_refs(&[("refs/prguide/7/v1/head".into(), head.clone())])
        .unwrap();
    assert!(
        git.verify_refs(&[("refs/prguide/7/v1/head".into(), head.clone())])
            .unwrap()
            .is_empty()
    );
    assert_eq!(git.list_refs("refs/prguide/7/").unwrap().len(), 3);
    git.delete_refs(&["refs/prguide/7/v1/head".into()]).unwrap();
}

#[test]
fn diff_args_and_path_filter_are_fixed() {
    let fixture = FixtureRepo::new();
    let first = fixture.head();
    fixture.write("top.txt", "two\n");
    let second = fixture.commit("change");
    fixture.push("main");
    let clone = fixture.clone("clone");
    let git = fixture.git(&clone);
    let spec = DiffSpec::TwoDot {
        from: first,
        to: second,
        paths: vec!["top.txt".into()],
    };
    assert!(
        String::from_utf8(git.diff(&spec).unwrap())
            .unwrap()
            .contains("a/top.txt")
    );
    assert!(spec.args().contains(&"--src-prefix=a/".into()));
}

#[test]
fn guards_and_missing_binary_are_reported() {
    let fixture = FixtureRepo::new();
    let clone = fixture.clone("clone");
    let git = fixture.git(&clone);
    let sha = git.resolve_commit("HEAD").unwrap();
    assert_eq!(
        git.write_refs(&[("refs/heads/main".into(), sha)])
            .unwrap_err()
            .code(),
        "invalid_ref"
    );
    assert_eq!(git.list_refs("").unwrap_err().code(), "invalid_ref");
    assert_eq!(
        git.env("PATH", fixture.path())
            .resolve_commit("HEAD")
            .unwrap_err()
            .code(),
        "git_missing"
    );
}
