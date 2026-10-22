use std::{
    fs,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use prguide::github::{
    Commit, CommitAuthor, GitHub, GitHubError, Pr, PrState, Repo,
    fake::{FakeGitHub, FakeMethod},
    gh::GhCli,
};
use tempfile::TempDir;

fn repo() -> Repo {
    Repo::github("cli", "cli").unwrap()
}
fn script() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/gh/fake-gh")
}
fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/gh")
        .join(name)
}
fn gh(dir: &TempDir) -> GhCli {
    GhCli::with_program(script()).env("FAKE_GH_DIR", dir.path())
}
fn replay(dir: &TempDir, name: &str) {
    fs::copy(fixture(name), dir.path().join("stdout")).unwrap();
}
fn synthetic(count: usize) -> String {
    serde_json::to_string(&vec![(0..count).map(|i| serde_json::json!({"sha":format!("{i:040x}"),"parents":[{"sha":"0000000000000000000000000000000000000000"}],"commit":{"message":format!("commit {i}"),"author":{"name":"Synth"}},"author":{"login":"synth"}})).collect::<Vec<_>>()]).unwrap()
}

#[test]
fn parses_pr_fixtures() {
    let cases = [
        (
            "pr-cli-14148-merged-fork.json",
            14148,
            PrState::Closed,
            Some("heaths/cli"),
        ),
        (
            "pr-cli-14783-open.json",
            14783,
            PrState::Open,
            Some("mrecachinas/cli"),
        ),
        (
            "pr-cli-5750-closed-deleted-fork.json",
            5750,
            PrState::Closed,
            None,
        ),
    ];
    for (file, number, state, head_repo) in cases {
        let dir = TempDir::new().unwrap();
        replay(&dir, file);
        let pr = gh(&dir).pr(&repo(), number).unwrap();
        assert_eq!(pr.number, number);
        assert_eq!(pr.state, state);
        assert_eq!(pr.head_repo.as_deref(), head_repo);
    }
    let dir = TempDir::new().unwrap();
    replay(&dir, "pr-ripgrep-3490-null-body.json");
    assert_eq!(
        gh(&dir)
            .pr(&Repo::github("BurntSushi", "ripgrep").unwrap(), 3490)
            .unwrap()
            .body,
        ""
    );
}

#[test]
fn parses_commit_fixtures_and_preserves_order() {
    let dir = TempDir::new().unwrap();
    replay(&dir, "commits-cli-14148.json");
    let commits = gh(&dir).commits(&repo(), 14148).unwrap();
    assert_eq!(commits.len(), 2);
    assert_eq!(commits[0].sha, "389b60d0b6df69299ccdd36f9a757fe7cf7f5b6a");
    assert!(commits[0].message.starts_with("Update glamour to v2"));
    assert_eq!(commits[1].author.login.as_deref(), Some("heaths"));
    let dir = TempDir::new().unwrap();
    replay(&dir, "commits-cli-14770-merge.json");
    let commits = gh(&dir).commits(&repo(), 14770).unwrap();
    assert_eq!(commits[2].parents.len(), 2);
    let dir = TempDir::new().unwrap();
    replay(&dir, "commits-nodejs-65095-null-author.json");
    let commits = gh(&dir)
        .commits(&Repo::github("nodejs", "node").unwrap(), 65095)
        .unwrap();
    assert_eq!(
        commits[0].author,
        CommitAuthor {
            name: "ulofiai".into(),
            login: None
        }
    );
}

#[test]
fn gh_arguments_environment_and_pagination_are_correct() {
    let dir = TempDir::new().unwrap();
    replay(&dir, "pr-cli-14148-merged-fork.json");
    let client = gh(&dir).env("GH_FORCE_TTY", "1").env("CLICOLOR_FORCE", "1");
    client.pr(&repo(), 14148).unwrap();
    assert_eq!(
        fs::read_to_string(dir.path().join("args"))
            .unwrap()
            .lines()
            .collect::<Vec<_>>(),
        [
            "api",
            "-H",
            "X-GitHub-Api-Version: 2026-03-10",
            "--hostname",
            "github.com",
            "repos/cli/cli/pulls/14148"
        ]
    );
    assert_eq!(
        fs::read_to_string(dir.path().join("gh_force_tty"))
            .unwrap()
            .trim(),
        "unset"
    );
    assert_eq!(
        fs::read_to_string(dir.path().join("clicolor_force"))
            .unwrap()
            .trim(),
        "unset"
    );
    let dir = TempDir::new().unwrap();
    fs::write(dir.path().join("stdout"), synthetic(106)).unwrap();
    let commits = gh(&dir).commits(&repo(), 1).unwrap();
    assert_eq!(commits.len(), 106);
    for (i, commit) in commits.iter().enumerate() {
        assert_eq!(commit.message, format!("commit {i}"));
    }
    assert!(
        fs::read_to_string(dir.path().join("args"))
            .unwrap()
            .contains("--slurp\n")
    );
}

#[test]
fn maps_script_errors_and_process_errors() {
    let dir = TempDir::new().unwrap();
    fs::write(dir.path().join("exit"), "1").unwrap();
    fs::write(dir.path().join("stderr"), "gh: Not Found (HTTP 404)\n").unwrap();
    assert_eq!(
        gh(&dir).pr(&repo(), 999).unwrap_err(),
        GitHubError::PrNotFound {
            repo: "cli/cli".into(),
            number: 999
        }
    );
    fs::write(dir.path().join("exit"), "4").unwrap();
    assert!(matches!(
        gh(&dir).pr(&repo(), 1),
        Err(GitHubError::GhUnauthenticated { .. })
    ));
    fs::write(dir.path().join("exit"), "1").unwrap();
    fs::write(dir.path().join("stderr"), "gh: Bad credentials (HTTP 401)").unwrap();
    assert!(matches!(
        gh(&dir).pr(&repo(), 1),
        Err(GitHubError::GhUnauthenticated { .. })
    ));
    fs::write(dir.path().join("stderr"), "network down").unwrap();
    assert!(matches!(
        gh(&dir).pr(&repo(), 1),
        Err(GitHubError::GhFailed {
            status: Some(1),
            ..
        })
    ));
    let missing = GhCli::with_program(dir.path().join("missing"));
    assert_eq!(missing.pr(&repo(), 1).unwrap_err().code(), "gh_missing");
    fs::write(dir.path().join("plain"), "no").unwrap();
    assert_eq!(
        GhCli::with_program(dir.path().join("plain"))
            .pr(&repo(), 1)
            .unwrap_err()
            .code(),
        "gh_failed"
    );
}

#[test]
fn rejects_large_commit_lists() {
    let dir = TempDir::new().unwrap();
    fs::write(dir.path().join("stdout"), synthetic(250)).unwrap();
    assert_eq!(
        gh(&dir).commits(&repo(), 1).unwrap_err().code(),
        "pr_too_large"
    );
    let fake = FakeGitHub::new();
    fake.set_commits(&repo(), 1, vec![commit(); 250]);
    assert_eq!(fake.commits(&repo(), 1).unwrap_err().code(), "pr_too_large");
}

#[test]
fn validates_repositories() {
    assert!(Repo::new("GitHub.COM", "my.repo", ".github").is_ok());
    for host in ["www.github.com", "api.github.com", "ghe.example.com", ""] {
        assert_eq!(
            Repo::new(host, "x", "y").unwrap_err().code(),
            "unsupported_host"
        );
    }
    for part in ["", ".", "..", "a/b", "a b", "a?b", "a#b", "a%2fb"] {
        assert_eq!(Repo::github(part, "x").unwrap_err().code(), "invalid_repo");
        assert_eq!(Repo::github("x", part).unwrap_err().code(), "invalid_repo");
    }
}

fn commit() -> Commit {
    Commit {
        sha: "a".into(),
        parents: vec![],
        message: "message".into(),
        author: CommitAuthor {
            name: "name".into(),
            login: None,
        },
    }
}
fn pr(number: u64) -> Pr {
    Pr {
        number,
        state: PrState::Open,
        merged_at: None,
        closed_at: None,
        head_sha: "a".into(),
        head_repo: None,
        base_ref: "main".into(),
        base_sha: "b".into(),
        title: "t".into(),
        body: String::new(),
        author: "u".into(),
        url: "u".into(),
    }
}

#[test]
fn fake_is_scriptable_thread_safe_and_does_not_hold_lock_while_delayed() {
    fn assert_send_sync<T: Send + Sync + ?Sized>() {}
    assert_send_sync::<dyn GitHub>();
    assert_send_sync::<FakeGitHub>();
    assert_send_sync::<GhCli>();
    let fake = Arc::new(FakeGitHub::new());
    fake.set_pr(&repo(), pr(1));
    fake.push_error(
        FakeMethod::Pr,
        GitHubError::GhFailed {
            status: Some(1),
            stderr: "x".into(),
        },
    );
    assert!(fake.pr(&repo(), 1).is_err());
    assert_eq!(fake.pr(&repo(), 1).unwrap(), pr(1));
    fake.set_delay(FakeMethod::Pr, Duration::from_millis(50));
    let start = Instant::now();
    fake.pr(&repo(), 1).unwrap();
    assert!(start.elapsed() >= Duration::from_millis(50));
    fake.set_delay(FakeMethod::Pr, Duration::from_millis(500));
    let done = Arc::new(AtomicBool::new(false));
    let copy = fake.clone();
    let complete = done.clone();
    let handle = thread::spawn(move || {
        copy.pr(&repo(), 1).unwrap();
        complete.store(true, Ordering::SeqCst);
    });
    let until = Instant::now() + Duration::from_secs(2);
    while fake.calls().len() < 3 && Instant::now() < until {
        thread::sleep(Duration::from_millis(5));
    }
    assert!(!done.load(Ordering::SeqCst));
    handle.join().unwrap();
}
