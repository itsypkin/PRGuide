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
    fake::{FakeCall, FakeGitHub, FakeMethod},
    gh::GhCli,
};
use serde_json::{Value, json};
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
fn fixture_json(name: &str) -> Value {
    serde_json::from_slice(&fs::read(fixture(name)).unwrap()).unwrap()
}
fn synthetic_page(start: usize, count: usize) -> Vec<Value> {
    (start..start + count)
        .map(|i| json!({"sha": format!("{i:040x}"), "parents": [{"sha": format!("{:040x}", i.saturating_sub(1))}], "commit": {"message": format!("commit {i}"), "author": {"name": "Synth"}}, "author": {"login": "synth"}}))
        .collect()
}
fn synthetic_pages(pages: &[(usize, usize)]) -> String {
    serde_json::to_string(
        &pages
            .iter()
            .map(|(start, count)| synthetic_page(*start, *count))
            .collect::<Vec<_>>(),
    )
    .unwrap()
}
fn expected_string(value: &Value, key: &str) -> String {
    value[key].as_str().unwrap().to_owned()
}
fn assert_pr_matches_fixture(pr: Pr, expected: &Value) {
    assert_eq!(pr.number, expected["number"].as_u64().unwrap());
    assert_eq!(
        pr.state,
        if expected["state"] == "open" {
            PrState::Open
        } else {
            PrState::Closed
        }
    );
    assert_eq!(pr.merged_at.as_deref(), expected["merged_at"].as_str());
    assert_eq!(pr.closed_at.as_deref(), expected["closed_at"].as_str());
    assert_eq!(pr.head_sha, expected_string(&expected["head"], "sha"));
    assert_eq!(
        pr.head_repo.as_deref(),
        expected["head"]["repo"]["full_name"].as_str()
    );
    assert_eq!(pr.base_ref, expected_string(&expected["base"], "ref"));
    assert_eq!(pr.base_sha, expected_string(&expected["base"], "sha"));
    assert_eq!(pr.title, expected_string(expected, "title"));
    assert_eq!(pr.body, expected["body"].as_str().unwrap_or_default());
    assert_eq!(pr.author, expected_string(&expected["user"], "login"));
    assert_eq!(pr.url, expected_string(expected, "html_url"));
}
fn assert_commit_matches_fixture(commit: &Commit, expected: &Value) {
    assert_eq!(commit.sha, expected_string(expected, "sha"));
    assert_eq!(
        commit.parents,
        expected["parents"]
            .as_array()
            .unwrap()
            .iter()
            .map(|parent| expected_string(parent, "sha"))
            .collect::<Vec<_>>()
    );
    assert_eq!(
        commit.message,
        expected_string(&expected["commit"], "message")
    );
    assert_eq!(
        commit.author.name,
        expected_string(&expected["commit"]["author"], "name")
    );
    assert_eq!(
        commit.author.login.as_deref(),
        expected["author"]["login"].as_str()
    );
}

#[test]
fn parses_every_pr_fixture_field() {
    for (file, repository, number) in [
        ("pr-cli-14148-merged-fork.json", repo(), 14148),
        ("pr-cli-14783-open.json", repo(), 14783),
        ("pr-cli-5750-closed-deleted-fork.json", repo(), 5750),
        (
            "pr-ripgrep-3490-null-body.json",
            Repo::github("BurntSushi", "ripgrep").unwrap(),
            3490,
        ),
    ] {
        let dir = TempDir::new().unwrap();
        replay(&dir, file);
        assert_pr_matches_fixture(
            gh(&dir).pr(&repository, number).unwrap(),
            &fixture_json(file),
        );
    }
}

#[test]
fn parses_every_commit_fixture_field_and_order() {
    for (file, repository, number) in [
        ("commits-cli-14148.json", repo(), 14148),
        ("commits-cli-14770-merge.json", repo(), 14770),
        (
            "commits-nodejs-65095-null-author.json",
            Repo::github("nodejs", "node").unwrap(),
            65095,
        ),
    ] {
        let dir = TempDir::new().unwrap();
        replay(&dir, file);
        let commits = gh(&dir).commits(&repository, number).unwrap();
        let expected = fixture_json(file);
        let expected = expected.as_array().unwrap()[0].as_array().unwrap();
        assert_eq!(commits.len(), expected.len());
        for (commit, expected) in commits.iter().zip(expected) {
            assert_commit_matches_fixture(commit, expected);
        }
    }
}

#[test]
fn paginated_commits_flatten_in_order_and_pass_exact_arguments() {
    let dir = TempDir::new().unwrap();
    fs::write(
        dir.path().join("stdout"),
        synthetic_pages(&[(0, 100), (100, 6)]),
    )
    .unwrap();
    let commits = gh(&dir).commits(&repo(), 14148).unwrap();
    assert_eq!(commits.len(), 106);
    for (index, commit) in commits.iter().enumerate() {
        assert_eq!(commit.message, format!("commit {index}"));
    }
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
            "--paginate",
            "--slurp",
            "repos/cli/cli/pulls/14148/commits?per_page=100",
        ]
    );
}

#[test]
fn pr_arguments_and_force_color_environment_are_correct() {
    let dir = TempDir::new().unwrap();
    replay(&dir, "pr-cli-14148-merged-fork.json");
    gh(&dir)
        .env("GH_FORCE_TTY", "1")
        .env("CLICOLOR_FORCE", "1")
        .pr(&repo(), 14148)
        .unwrap();
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
            "repos/cli/cli/pulls/14148",
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
}

#[test]
fn maps_script_errors_and_parse_errors() {
    let dir = TempDir::new().unwrap();
    fs::write(dir.path().join("exit"), "1").unwrap();
    fs::write(dir.path().join("stderr"), "gh: Not Found (HTTP 404)\n").unwrap();
    for call in [true, false] {
        let error = if call {
            gh(&dir).pr(&repo(), 999_999_999).unwrap_err()
        } else {
            gh(&dir).commits(&repo(), 999_999_999).unwrap_err()
        };
        assert_eq!(
            error,
            GitHubError::PrNotFound {
                repo: "cli/cli".into(),
                number: 999_999_999
            }
        );
    }
    fs::write(dir.path().join("exit"), "4").unwrap();
    fs::write(dir.path().join("stderr"), "To get started with GitHub CLI, please run:  gh auth login\nAlternatively, populate the GH_TOKEN environment variable with a GitHub API authentication token.\n").unwrap();
    assert!(
        matches!(gh(&dir).pr(&repo(), 1), Err(GitHubError::GhUnauthenticated { stderr }) if stderr == "To get started with GitHub CLI, please run:  gh auth login\nAlternatively, populate the GH_TOKEN environment variable with a GitHub API authentication token.")
    );
    fs::write(dir.path().join("exit"), "1").unwrap();
    fs::write(
        dir.path().join("stderr"),
        "gh: Bad credentials (HTTP 401)\n",
    )
    .unwrap();
    assert!(
        matches!(gh(&dir).pr(&repo(), 1), Err(GitHubError::GhUnauthenticated { stderr }) if stderr == "gh: Bad credentials (HTTP 401)")
    );
    for stderr in [
        "Get \"https://api.github.com/repos/cli/cli/pulls/1\": proxyconnect tcp: dial tcp 127.0.0.1:9: connect: connection refused",
        "gh: The version you specified in the \"X-GitHub-API-Version\" request header, \"2027-01-01\", is not a supported version. The following versions are currently supported: \"2026-03-10\" (most recent) and \"2022-11-28\". (Bad Request)",
    ] {
        fs::write(dir.path().join("stderr"), stderr).unwrap();
        let error = gh(&dir).pr(&repo(), 1).unwrap_err();
        assert!(matches!(
            error,
            GitHubError::GhFailed {
                status: Some(1),
                ..
            }
        ));
        assert!(error.to_string().contains(stderr));
    }
    fs::write(dir.path().join("exit"), "0").unwrap();
    fs::write(dir.path().join("stdout"), "not json").unwrap();
    assert!(
        matches!(gh(&dir).pr(&repo(), 1), Err(GitHubError::GhFailed { status: Some(0), stderr }) if stderr.starts_with("unexpected gh output: "))
    );
    fs::write(dir.path().join("stdout"), [0xff]).unwrap();
    assert!(matches!(
        gh(&dir).pr(&repo(), 1),
        Err(GitHubError::GhFailed {
            status: Some(0),
            ..
        })
    ));
    fs::write(dir.path().join("stdout"), "not json").unwrap();
    assert!(matches!(
        gh(&dir).commits(&repo(), 1),
        Err(GitHubError::GhFailed {
            status: Some(0),
            ..
        })
    ));
}

#[cfg(unix)]
#[test]
fn finds_gh_on_child_path_and_reports_missing_or_nonexecutable_programs() {
    let dir = TempDir::new().unwrap();
    let missing_path = dir.path().join("does-not-exist");
    assert_eq!(
        GhCli::with_program(&missing_path)
            .pr(&repo(), 1)
            .unwrap_err(),
        GitHubError::GhMissing {
            program: missing_path.to_string_lossy().into_owned()
        }
    );
    let empty = TempDir::new().unwrap();
    assert_eq!(
        GhCli::new()
            .env("PATH", empty.path())
            .pr(&repo(), 1)
            .unwrap_err()
            .code(),
        "gh_missing"
    );
    let executable_dir = TempDir::new().unwrap();
    std::os::unix::fs::symlink(script(), executable_dir.path().join("gh")).unwrap();
    replay(&dir, "pr-cli-14148-merged-fork.json");
    assert_eq!(
        GhCli::new()
            .env("PATH", executable_dir.path())
            .env("FAKE_GH_DIR", dir.path())
            .pr(&repo(), 14148)
            .unwrap()
            .number,
        14148
    );
    let plain = dir.path().join("plain");
    fs::write(&plain, "no").unwrap();
    assert_eq!(
        GhCli::with_program(plain)
            .pr(&repo(), 1)
            .unwrap_err()
            .code(),
        "gh_failed"
    );
}

#[test]
fn rejects_large_commit_lists_but_accepts_249() {
    let dir = TempDir::new().unwrap();
    fs::write(
        dir.path().join("stdout"),
        synthetic_pages(&[(0, 100), (100, 100), (200, 50)]),
    )
    .unwrap();
    assert_eq!(
        gh(&dir).commits(&repo(), 1).unwrap_err().code(),
        "pr_too_large"
    );
    fs::write(
        dir.path().join("stdout"),
        synthetic_pages(&[(0, 100), (100, 100), (200, 49)]),
    )
    .unwrap();
    assert_eq!(gh(&dir).commits(&repo(), 1).unwrap().len(), 249);
    let fake = FakeGitHub::new();
    fake.set_commits(&repo(), 1, vec![commit(); 250]);
    assert_eq!(fake.commits(&repo(), 1).unwrap_err().code(), "pr_too_large");
    fake.set_commits(&repo(), 2, vec![commit(); 249]);
    assert_eq!(fake.commits(&repo(), 2).unwrap().len(), 249);
}

#[test]
fn validates_repositories() {
    for owner in ["cli", "my.repo", "my_repo-2", ".github"] {
        assert!(Repo::github(owner, "cli").is_ok());
    }
    assert!(Repo::new("GitHub.COM", "cli", "cli").is_ok());
    for host in ["www.github.com", "api.github.com", "ghe.example.com", ""] {
        assert_eq!(
            Repo::new(host, "x", "y").unwrap_err().code(),
            "unsupported_host"
        );
    }
    assert_eq!(
        Repo::new("ghe.example.com", "..", "x").unwrap_err().code(),
        "unsupported_host"
    );
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
fn fake_is_scriptable_thread_safe_and_records_all_calls() {
    fn assert_send_sync<T: Send + Sync + ?Sized>() {}
    assert_send_sync::<dyn GitHub>();
    assert_send_sync::<FakeGitHub>();
    assert_send_sync::<GhCli>();
    let fake = Arc::new(FakeGitHub::new());
    fake.set_pr(&repo(), pr(1));
    fake.set_commits(&repo(), 1, vec![commit()]);
    assert_eq!(fake.pr(&repo(), 1).unwrap(), pr(1));
    assert_eq!(fake.commits(&repo(), 1).unwrap(), vec![commit()]);
    assert_eq!(fake.commits(&repo(), 2).unwrap_err().code(), "pr_not_found");
    assert_eq!(fake.pr(&repo(), 2).unwrap_err().code(), "pr_not_found");
    fake.push_error(
        FakeMethod::Pr,
        GitHubError::GhFailed {
            status: Some(1),
            stderr: "x".into(),
        },
    );
    assert!(fake.pr(&repo(), 1).is_err());
    assert_eq!(fake.pr(&repo(), 1).unwrap(), pr(1));
    fake.push_error(
        FakeMethod::Pr,
        GitHubError::GhFailed {
            status: Some(1),
            stderr: "x".into(),
        },
    );
    assert_eq!(fake.commits(&repo(), 1).unwrap(), vec![commit()]);
    assert!(fake.pr(&repo(), 1).is_err());
    let calls = fake.calls();
    assert_eq!(
        calls,
        vec![
            FakeCall {
                method: FakeMethod::Pr,
                repo: repo(),
                number: 1
            },
            FakeCall {
                method: FakeMethod::Commits,
                repo: repo(),
                number: 1
            },
            FakeCall {
                method: FakeMethod::Commits,
                repo: repo(),
                number: 2
            },
            FakeCall {
                method: FakeMethod::Pr,
                repo: repo(),
                number: 2
            },
            FakeCall {
                method: FakeMethod::Pr,
                repo: repo(),
                number: 1
            },
            FakeCall {
                method: FakeMethod::Pr,
                repo: repo(),
                number: 1
            },
            FakeCall {
                method: FakeMethod::Commits,
                repo: repo(),
                number: 1
            },
            FakeCall {
                method: FakeMethod::Pr,
                repo: repo(),
                number: 1
            },
        ]
    );
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
    let mut observed = false;
    while Instant::now() < until {
        if fake.calls().len() >= calls.len() + 2 {
            observed = true;
            break;
        }
        thread::sleep(Duration::from_millis(5));
    }
    assert!(observed);
    assert!(!done.load(Ordering::SeqCst));
    handle.join().unwrap();
    let github: Arc<dyn GitHub> = fake;
    assert_eq!(
        thread::spawn(move || github.pr(&repo(), 1).unwrap())
            .join()
            .unwrap(),
        pr(1)
    );
}
