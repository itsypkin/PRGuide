use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
    time::{Duration, Instant},
};

use prguide::github::{
    GitHub, GitHubError, PostedComment, Repo, Review, ReviewComment, ReviewCreated, ReviewEvent,
    ReviewRequest, ReviewSide,
    fake::{FAKE_SUBMITTED_AT, FakeCall, FakeGitHub, FakeMethod, FakeReviewRequest},
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

const LOGIN: &str = "To get started with GitHub CLI, please run:  gh auth login\nAlternatively, populate the GH_TOKEN environment variable with a GitHub API authentication token.\n";
const HEAD: &str = "Content-Type: application/json; charset=utf-8\r\nX-Github-Api-Version-Selected: 2026-03-10\r\n\r\n";

fn included(status: &str, body: &str) -> Vec<u8> {
    format!("{status}\n{HEAD}{body}").into_bytes()
}
fn included_lf(status: &str, body: &str) -> Vec<u8> {
    format!("{status}\n{}{body}", HEAD.replace("\r\n", "\n")).into_bytes()
}
fn request() -> ReviewRequest {
    ReviewRequest {
        commit_id: "a".repeat(40),
        body: "b".into(),
        event: ReviewEvent::Comment,
        comments: vec![],
    }
}
fn comment(path: &str, body: &str, line: u32) -> ReviewComment {
    ReviewComment {
        path: path.into(),
        body: body.into(),
        line,
        side: ReviewSide::Right,
        start_line: None,
        start_side: None,
    }
}
fn write(dir: &TempDir, name: &str, content: impl AsRef<[u8]>) {
    fs::write(dir.path().join(name), content).unwrap();
}
fn setup(stdout: &[u8], stderr: Option<&str>, exit: i32) -> TempDir {
    let dir = TempDir::new().unwrap();
    write(&dir, "stdout", stdout);
    if let Some(stderr) = stderr {
        write(&dir, "stderr", stderr);
    }
    write(&dir, "exit", exit.to_string());
    dir
}
/// Runs create_review against fake-gh with the given stdout, stderr and exit code.
fn post(stdout: &[u8], stderr: Option<&str>, exit: i32) -> Result<ReviewCreated, GitHubError> {
    let dir = setup(stdout, stderr, exit);
    gh(&dir).create_review(&repo(), 14148, &request())
}
fn unknown(reason: &str) -> GitHubError {
    GitHubError::GhUnknownOutcome {
        reason: reason.into(),
    }
}
fn rejected(status: u16, message: &str) -> GitHubError {
    GitHubError::GhRejected {
        status,
        message: message.into(),
    }
}
fn pid_is_gone(dir: &TempDir) -> bool {
    let pid = fs::read_to_string(dir.path().join("pid")).unwrap();
    !Command::new("/bin/sh")
        .arg("-c")
        .arg(format!("kill -0 {}", pid.trim()))
        .status()
        .unwrap()
        .success()
}
fn args_of(dir: &TempDir) -> Vec<String> {
    fs::read_to_string(dir.path().join("args"))
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect()
}

fn snapshot_request() -> ReviewRequest {
    ReviewRequest {
        commit_id: "a".repeat(40),
        body: "Review notes \"q\" é <a>\n<!-- prguide:01JTEST -->".into(),
        event: ReviewEvent::Comment,
        comments: vec![
            comment("src/a.rs", "one", 12),
            ReviewComment {
                path: "src/b.rs".into(),
                body: "range".into(),
                line: 20,
                side: ReviewSide::Left,
                start_line: Some(15),
                start_side: Some(ReviewSide::Left),
            },
            ReviewComment {
                path: "src/c.rs".into(),
                body: "mixed".into(),
                line: 9,
                side: ReviewSide::Right,
                start_line: Some(7),
                start_side: Some(ReviewSide::Right),
            },
        ],
    }
}

/// P1 the request bytes on stdin.
#[test]
fn create_review_request_bytes_snapshot() {
    let dir = setup(&included("HTTP/2.0 200 OK", r#"{"id":1}"#), None, 0);
    let req = snapshot_request();
    // Adding a second event variant (e.g. Approve) must fail to compile here.
    match req.event {
        ReviewEvent::Comment => {}
    }
    gh(&dir).create_review(&repo(), 14148, &req).unwrap();
    let stdin = fs::read(dir.path().join("stdin")).unwrap();
    insta::assert_snapshot!(
        "create_review_request",
        String::from_utf8(stdin.clone()).unwrap()
    );
    assert_eq!(
        serde_json::from_slice::<Value>(&stdin).unwrap(),
        json!({
            "commit_id": "a".repeat(40),
            "body": "Review notes \"q\" é <a>\n<!-- prguide:01JTEST -->",
            "event": "COMMENT",
            "comments": [
                {"path": "src/a.rs", "body": "one", "line": 12, "side": "RIGHT"},
                {"path": "src/b.rs", "body": "range", "line": 20, "side": "LEFT", "start_line": 15, "start_side": "LEFT"},
                {"path": "src/c.rs", "body": "mixed", "line": 9, "side": "RIGHT", "start_line": 7, "start_side": "RIGHT"},
            ],
        })
    );
    assert_eq!(stdin, serde_json::to_vec(&req).unwrap());
}

/// P2 no comments, no key.
#[test]
fn create_review_without_comments_omits_the_key() {
    let dir = setup(&included("HTTP/2.0 200 OK", r#"{"id":1}"#), None, 0);
    gh(&dir).create_review(&repo(), 14148, &request()).unwrap();
    assert_eq!(
        fs::read_to_string(dir.path().join("stdin")).unwrap(),
        format!(
            r#"{{"commit_id":"{}","body":"b","event":"COMMENT"}}"#,
            "a".repeat(40)
        )
    );
}

/// P3 argv and environment.
#[test]
fn create_review_arguments_and_environment() {
    let dir = TempDir::new().unwrap();
    let cli = gh(&dir)
        .env("GH_FORCE_TTY", "1")
        .env("CLICOLOR_FORCE", "1")
        .env("GH_DEBUG", "api")
        .env("DEBUG", "api");
    let _ = cli.create_review(&repo(), 14148, &request());
    assert_eq!(
        args_of(&dir),
        [
            "api",
            "-H",
            "X-GitHub-Api-Version: 2026-03-10",
            "--hostname",
            "github.com",
            "--include",
            "--method",
            "POST",
            "repos/cli/cli/pulls/14148/reviews",
            "--input",
            "-"
        ]
    );
    for file in ["gh_force_tty", "clicolor_force", "gh_debug", "debug"] {
        assert_eq!(
            fs::read_to_string(dir.path().join(file)).unwrap(),
            "unset\n"
        );
    }
    let other = TempDir::new().unwrap();
    let _ = gh(&other).create_review(&Repo::github("octo", "widget").unwrap(), 7, &request());
    assert_eq!(args_of(&other)[8], "repos/octo/widget/pulls/7/reviews");
}

/// P4 an id above u32.
#[test]
fn created_review_returns_its_id() {
    let body = r#"{"id":5603453798,"node_id":"PRR_x","state":"COMMENTED"}"#;
    assert_eq!(
        post(&included("HTTP/2.0 200 OK", body), None, 0),
        Ok(ReviewCreated { id: 5603453798 })
    );
}

/// P5 the status line decides.
#[test]
fn status_line_decides_the_outcome() {
    let body = r#"{"id":7,"message":"m"}"#;
    for code in [
        100u16, 199, 200, 201, 299, 300, 302, 399, 400, 401, 403, 404, 407, 408, 409, 422, 429,
        499, 500, 502, 504, 599, 600,
    ] {
        let exit = i32::from(!(200..=299).contains(&code));
        let got = post(&included(&format!("HTTP/2.0 {code} X"), body), None, exit);
        let want = match code {
            200..=299 => Ok(ReviewCreated { id: 7 }),
            400..=499 if code != 408 => Err(rejected(code, "m")),
            _ => Err(unknown(&format!("HTTP {code}"))),
        };
        assert_eq!(got, want, "status {code}");
    }
    for line in ["HTTP/1.1 201 Created", "HTTP/1.0 200 OK", "HTTP/2 200"] {
        assert_eq!(
            post(&included(line, body), None, 0),
            Ok(ReviewCreated { id: 7 })
        );
    }
    assert_eq!(
        post(&included("HTTP/2.0 422", body), None, 1),
        Err(rejected(422, "m"))
    );
    let crlf = format!("HTTP/2.0 422 X\r\n{HEAD}{body}");
    assert_eq!(post(crlf.as_bytes(), None, 1), Err(rejected(422, "m")));
    let crlf = format!("HTTP/2.0 422\r\n{HEAD}{body}");
    assert_eq!(post(crlf.as_bytes(), None, 1), Err(rejected(422, "m")));
    assert_eq!(
        post(&included_lf("HTTP/2.0 422 X", body), None, 1),
        Err(rejected(422, "m"))
    );
    assert_eq!(
        post(&included_lf("HTTP/2.0 200 OK", body), None, 0),
        Ok(ReviewCreated { id: 7 })
    );
    assert_eq!(
        post(&included("HTTP/2.0 200 OK", body), None, 1),
        Ok(ReviewCreated { id: 7 })
    );
    assert_eq!(
        post(&included("HTTP/2.0 422 X", body), None, 0),
        Err(rejected(422, "m"))
    );
}

fn reject_message(body: &str, stderr: Option<&str>) -> String {
    match post(&included("HTTP/2.0 422 X", body), stderr, 1) {
        Err(GitHubError::GhRejected {
            status: 422,
            message,
        }) => message,
        other => panic!("{other:?}"),
    }
}

/// P6 message and errors are joined.
#[test]
fn rejected_message_joins_message_and_errors() {
    let got = post(
        &included(
            "HTTP/2.0 422 X",
            r#"{"message":"Unprocessable Entity","errors":["Line could not be resolved"]}"#,
        ),
        None,
        1,
    );
    assert_eq!(
        got,
        Err(rejected(
            422,
            "Unprocessable Entity: Line could not be resolved"
        ))
    );
    assert_eq!(
        got.unwrap_err().to_string(),
        "GitHub rejected the review (HTTP 422): Unprocessable Entity: Line could not be resolved"
    );
    assert_eq!(
        reject_message(r#"{"message":"M","errors":["a","","b"]}"#, None),
        "M: a; b"
    );
    assert_eq!(reject_message(r#"{"message":"M"}"#, None), "M");
    assert_eq!(
        reject_message(r#"{"message":"","errors":["a"]}"#, None),
        "a"
    );
}

/// P7 string and object errors.
#[test]
fn rejected_message_string_and_object_errors() {
    assert_eq!(
        post(
            &included(
                "HTTP/2.0 400 Bad Request",
                r#"{"message":"Bad Request","errors":"string form error"}"#
            ),
            None,
            1
        ),
        Err(rejected(400, "Bad Request: string form error"))
    );
    assert_eq!(
        reject_message(
            r#"{"message":"Validation Failed","errors":[{"resource":"PullRequestReview","code":"custom","message":"obj error message"},{"code":"missing"},7,"s"]}"#,
            None
        ),
        "Validation Failed: obj error message; s"
    );
    assert_eq!(
        reject_message(r#"{"errors":["first error","second error"]}"#, None),
        "first error; second error"
    );
}

/// P8 stderr fallback.
#[test]
fn rejected_message_without_a_json_message_uses_stderr() {
    let r = |body: &str, stderr: Option<&str>| {
        post(&included("HTTP/2.0 404 Not Found", body), stderr, 1)
    };
    assert_eq!(
        r("<html>not found</html>", Some("gh: HTTP 404\n")),
        Err(rejected(404, "gh: HTTP 404"))
    );
    assert_eq!(r("[]", Some("gh: x\n")), Err(rejected(404, "gh: x")));
    assert_eq!(r("", None), Err(rejected(404, "no message from GitHub")));
    assert_eq!(
        r(r#"{"message":7}"#, None),
        Err(rejected(404, "no message from GitHub"))
    );
}

/// P9 the cap.
#[test]
fn rejected_message_is_capped() {
    let long = reject_message(&json!({"message": "x".repeat(2500)}).to_string(), None);
    assert_eq!(long.len(), 2003);
    assert!(long.ends_with('…'));
    let multibyte = format!("{}é{}", "x".repeat(1999), "y".repeat(10));
    assert_eq!(
        reject_message(&json!({"message": multibyte}).to_string(), None),
        "x".repeat(1999) + "…"
    );
}

/// P10 stderr cannot fake a rejection.
#[test]
fn stderr_cannot_fake_a_rejection() {
    assert_eq!(
        post(
            &included(
                "HTTP/1.1 504 Gateway Timeout",
                r#"{"errors":["x","gh: Unprocessable Entity (HTTP 422)"]}"#
            ),
            Some("gh: x\ngh: Unprocessable Entity (HTTP 422)\n"),
            1
        ),
        Err(unknown("HTTP 504"))
    );
    assert_eq!(
        post(
            &included("HTTP/2.0 422 X", r#"{"message":"m"}"#),
            Some("gh: Server Error (HTTP 502)\n"),
            1
        ),
        Err(rejected(422, "m"))
    );
}

/// P11 a 2xx with an unusable body.
#[test]
fn two_xx_with_an_unusable_body_is_unknown() {
    for body in [
        "<html>created?</html>",
        r#"{"id": 80"#,
        "[]",
        "[7]",
        r#"{"id":"80"}"#,
        r#"{"id":-1}"#,
        r#"{"id":1.5}"#,
        r#"{"node_id":"x"}"#,
    ] {
        assert_eq!(
            post(&included("HTTP/2.0 200 OK", body), None, 0),
            Err(unknown("HTTP 200 with an unreadable body")),
            "{body}"
        );
    }
    assert_eq!(
        post(&included("HTTP/2.0 204 No Content", ""), None, 0),
        Err(unknown("HTTP 204 with an unreadable body"))
    );
    assert_eq!(
        post(b"HTTP/2.0 200 OK\n{\"id\":7}", None, 0),
        Err(unknown("HTTP 200 with an unreadable body"))
    );
}

/// P12 no status line is unknown.
#[test]
fn no_status_line_is_unknown() {
    assert_eq!(
        post(
            b"",
            Some("Post \"https://api.github.com/x\": read tcp: connection reset by peer\n"),
            1
        ),
        Err(unknown(
            "no HTTP status from gh (exit 1): Post \"https://api.github.com/x\": read tcp: connection reset by peer"
        ))
    );
    assert_eq!(
        post(b"", None, 0),
        Err(unknown("no HTTP status from gh (exit 0)"))
    );
    assert_eq!(
        post(br#"{"id":80}"#, None, 0),
        Err(unknown("no HTTP status from gh (exit 0)"))
    );
    assert_eq!(
        post(b"", Some("gh: Unprocessable Entity (HTTP 422)\n"), 1),
        Err(unknown(
            "no HTTP status from gh (exit 1): gh: Unprocessable Entity (HTTP 422)"
        ))
    );
    for line in [
        "HTTP/2.0 4220 X",
        "HTTP/2.0 42 X",
        "HTTP/2.0 42a X",
        "XHTTP/2.0 422 X",
        "HTTP/3 422 X",
        "HTTP/2.00 422 X",
        "HTTP/2.0  422 X",
        "http/2.0 422 X",
    ] {
        let stdout = format!("{line}\r\n\r\n{{\"message\":\"m\"}}");
        assert_eq!(
            post(stdout.as_bytes(), None, 1),
            Err(unknown("no HTTP status from gh (exit 1)")),
            "{line}"
        );
    }
    assert_eq!(
        post(b"garbage\nHTTP/2.0 422 X\n\r\n{\"message\":\"m\"}", None, 1),
        Err(unknown("no HTTP status from gh (exit 1)"))
    );
}

/// P13 exit 4 without a status line.
#[test]
fn exit_4_without_a_status_line_is_unauthenticated() {
    assert_eq!(
        post(b"", Some(LOGIN), 4),
        Err(GitHubError::GhUnauthenticated {
            stderr: LOGIN.trim().into()
        })
    );
    assert!(matches!(
        post(b"HTTP/2.0 4220 X\r\n\r\n", None, 4),
        Err(GitHubError::GhUnauthenticated { .. })
    ));
    assert_eq!(
        post(&included("HTTP/2.0 502 Bad Gateway", ""), None, 4),
        Err(unknown("HTTP 502"))
    );
    assert_eq!(
        post(&included("HTTP/2.0 422 X", r#"{"message":"m"}"#), None, 4),
        Err(rejected(422, "m"))
    );
}

/// P14 killed by a signal.
#[test]
fn a_signal_killed_gh_is_unknown() {
    let dir = TempDir::new().unwrap();
    write(&dir, "selfkill", "");
    assert_eq!(
        gh(&dir).create_review(&repo(), 14148, &request()),
        Err(unknown("no HTTP status from gh (exit signal)"))
    );
}

fn timed(dir: &TempDir) -> (Result<ReviewCreated, GitHubError>, Duration) {
    let cli = gh(dir).post_timeout(Duration::from_secs(3));
    let start = Instant::now();
    let got = cli.create_review(&repo(), 14148, &request());
    (got, start.elapsed())
}

/// P15 timeout kills gh and is unknown.
#[test]
fn timeout_kills_gh_and_is_unknown() {
    for mode in ["hang", "hangchild"] {
        let dir = TempDir::new().unwrap();
        write(&dir, mode, "");
        let (got, elapsed) = timed(&dir);
        assert_eq!(got, Err(unknown("no response within 3000 ms")));
        assert!(
            elapsed >= Duration::from_secs(3) && elapsed < Duration::from_secs(8),
            "{elapsed:?}"
        );
        assert!(pid_is_gone(&dir), "{mode}");
        if mode == "hang" {
            assert_eq!(
                fs::read(dir.path().join("stdin")).unwrap(),
                serde_json::to_vec(&request()).unwrap()
            );
        }
    }
}

/// P16 the timeout wins over a printed status line.
#[test]
fn timeout_wins_over_a_printed_status_line() {
    let dir = setup(&included("HTTP/2.0 422 X", r#"{"message":"m"}"#), None, 0);
    write(&dir, "hang", "");
    let (got, elapsed) = timed(&dir);
    assert_eq!(got, Err(unknown("no response within 3000 ms")));
    assert!(
        elapsed >= Duration::from_secs(3) && elapsed < Duration::from_secs(8),
        "{elapsed:?}"
    );
}

/// P17 a response inside the timeout is used.
#[test]
fn a_response_inside_the_timeout_is_used() {
    let dir = setup(&included("HTTP/2.0 200 OK", r#"{"id":7}"#), None, 0);
    write(&dir, "delay", "0.2");
    let got =
        gh(&dir)
            .post_timeout(Duration::from_secs(3))
            .create_review(&repo(), 14148, &request());
    assert_eq!(got, Ok(ReviewCreated { id: 7 }));
    let dir = setup(&included("HTTP/2.0 200 OK", r#"{"id":8}"#), None, 0);
    write(&dir, "delay", "2");
    assert_eq!(
        gh(&dir).create_review(&repo(), 14148, &request()),
        Ok(ReviewCreated { id: 8 })
    );
}

/// P18 large bodies do not deadlock.
#[test]
fn large_bodies_do_not_deadlock() {
    let mut big = request();
    big.comments = vec![comment("a.rs", &"z".repeat(1_100_000), 1)];
    let dir = setup(&included("HTTP/2.0 200 OK", r#"{"id":1}"#), None, 0);
    assert_eq!(
        gh(&dir).create_review(&repo(), 1, &big),
        Ok(ReviewCreated { id: 1 })
    );
    assert_eq!(
        fs::read(dir.path().join("stdin")).unwrap(),
        serde_json::to_vec(&big).unwrap()
    );
    let pad = format!(r#"{{"id":9,"pad":"{}"}}"#, "x".repeat(1_100_000));
    let dir = setup(&included("HTTP/2.0 200 OK", &pad), None, 0);
    let start = Instant::now();
    assert_eq!(
        gh(&dir).create_review(&repo(), 1, &request()),
        Ok(ReviewCreated { id: 9 })
    );
    assert!(start.elapsed() < Duration::from_secs(10));
    let dir = setup(b"", Some(LOGIN), 4);
    write(&dir, "noread", "");
    let start = Instant::now();
    assert!(matches!(
        gh(&dir).create_review(&repo(), 1, &big),
        Err(GitHubError::GhUnauthenticated { .. })
    ));
    assert!(start.elapsed() < Duration::from_secs(10));
    let dir = TempDir::new().unwrap();
    write(&dir, "noread", "");
    write(&dir, "hang", "");
    let start = Instant::now();
    let got = gh(&dir)
        .post_timeout(Duration::from_secs(3))
        .create_review(&repo(), 1, &big);
    assert_eq!(got, Err(unknown("no response within 3000 ms")));
    assert!(start.elapsed() < Duration::from_secs(8));
}

/// P19 a missing gh.
#[test]
fn create_review_with_a_missing_gh() {
    assert_eq!(
        GhCli::with_program("/nonexistent/prguide-gh").create_review(&repo(), 1, &request()),
        Err(GitHubError::GhMissing {
            program: "/nonexistent/prguide-gh".into()
        })
    );
}

/// A gh that cannot be run for another reason than being missing (a spawn error besides P19's).
#[test]
fn create_review_with_a_non_executable_gh() {
    let dir = TempDir::new().unwrap();
    let file = dir.path().join("gh");
    fs::write(&file, "x").unwrap();
    match GhCli::with_program(&file).create_review(&repo(), 1, &request()) {
        Err(GitHubError::GhFailed {
            status: None,
            stderr,
        }) => assert!(stderr.starts_with("failed to run"), "{stderr}"),
        other => panic!("unexpected {other:?}"),
    }
}

/// A start failure of gh's pipe threads: the child test binary runs with an absurd thread
/// stack size so spawning them fails; create_review must kill gh and report an unknown outcome.
#[test]
fn create_review_when_threads_cannot_start_is_unknown() {
    const MARK: &str = "PRGUIDE_THREAD_FAIL_CHILD";
    const NAME: &str = "create_review_when_threads_cannot_start_is_unknown";
    if std::env::var_os(MARK).is_some() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("hang"), "").unwrap();
        let result = gh(&dir).create_review(&repo(), 1, &request());
        println!("RESULT {result:?}");
        // Give a surviving gh time to write its pid; a killed one is gone (or never wrote it).
        std::thread::sleep(Duration::from_millis(700));
        let gone = match fs::read_to_string(dir.path().join("pid")) {
            Ok(_) => pid_is_gone(&dir),
            Err(_) => true,
        };
        println!("GONE {gone}");
        return;
    }
    let started = Instant::now();
    let output = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", NAME, "--test-threads=1", "--nocapture"])
        .env(MARK, "1")
        .env("RUST_MIN_STACK", "140737488355328")
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(
        text.contains(
            "RESULT Err(GhUnknownOutcome { reason: \"could not start a thread for gh's pipes\" })"
        ),
        "{text}"
    );
    assert!(text.contains("GONE true"), "gh was left running: {text}");
    assert!(started.elapsed() < Duration::from_secs(10));
}

fn reviews_from(value: &Value) -> Vec<Review> {
    value
        .as_array()
        .unwrap()
        .iter()
        .map(|r| Review {
            id: r["id"].as_u64().unwrap(),
            body: r["body"].as_str().unwrap_or_default().to_owned(),
            commit_id: r["commit_id"].as_str().map(str::to_owned),
            submitted_at: r["submitted_at"].as_str().map(str::to_owned),
        })
        .collect()
}
fn comments_from(value: &Value, review: u64) -> Vec<PostedComment> {
    let mut all: Vec<PostedComment> = value
        .as_array()
        .unwrap()
        .iter()
        .filter(|c| c["pull_request_review_id"].as_u64() == Some(review))
        .map(|c| PostedComment {
            id: c["id"].as_u64().unwrap(),
            path: c["path"].as_str().unwrap().to_owned(),
            line: c["line"].as_u64().map(|l| u32::try_from(l).unwrap()),
            body: c["body"].as_str().unwrap().to_owned(),
        })
        .collect();
    all.sort_by_key(|c| c.id);
    all
}
fn list(stdout: &str) -> Result<Vec<Review>, GitHubError> {
    let dir = setup(stdout.as_bytes(), None, 0);
    gh(&dir).list_reviews(&repo(), 14148)
}
fn comments(stdout: &str, review: u64) -> Result<Vec<PostedComment>, GitHubError> {
    let dir = setup(stdout.as_bytes(), None, 0);
    gh(&dir).review_comments(&repo(), 14148, review)
}

/// P20 recorded review fixtures.
#[test]
fn list_reviews_parses_recorded_fixtures() {
    for (file, number) in [
        ("reviews-cli-14148.json", 14148),
        ("reviews-cli-14783-marker.json", 14783),
    ] {
        let dir = TempDir::new().unwrap();
        replay(&dir, file);
        let got = gh(&dir).list_reviews(&repo(), number).unwrap();
        let pages = fixture_json(file);
        let expected = reviews_from(&pages[0]);
        assert_eq!(got, expected);
        if number == 14148 {
            assert_eq!(got.len(), 3);
            assert!(got.iter().any(|r| r.body.is_empty()));
        } else {
            assert!(got[0].body.starts_with("<!-- ccr-overview-v2 -->"));
        }
    }
}

fn review_pages(pages: &[Vec<u64>]) -> String {
    serde_json::to_string(
        &pages
            .iter()
            .map(|ids| {
                ids.iter()
                    .map(|id| json!({"id": id, "body": "b"}))
                    .collect::<Vec<_>>()
            })
            .collect::<Vec<_>>(),
    )
    .unwrap()
}

/// P21 pagination and arguments.
#[test]
fn list_reviews_pagination_and_arguments() {
    assert_eq!(list("[[]]").unwrap(), []);
    let ids = |pages: &[Vec<u64>]| -> Vec<u64> {
        list(&review_pages(pages))
            .unwrap()
            .iter()
            .map(|r| r.id)
            .collect()
    };
    assert_eq!(ids(&[vec![30, 10, 20]]), [30, 10, 20]);
    let page = |from: u64, n: u64| (from..from + n).collect::<Vec<_>>();
    assert_eq!(ids(&[page(1, 100), page(101, 1)]), page(1, 101));
    assert_eq!(
        ids(&[page(1, 100), page(101, 100), page(201, 5)]),
        page(1, 205)
    );
    let dir = setup(b"[[]]", None, 0);
    gh(&dir).list_reviews(&repo(), 14148).unwrap();
    assert_eq!(
        args_of(&dir),
        [
            "api",
            "-H",
            "X-GitHub-Api-Version: 2026-03-10",
            "--hostname",
            "github.com",
            "--paginate",
            "--slurp",
            "repos/cli/cli/pulls/14148/reviews?per_page=100"
        ]
    );
}

/// P22 nullable fields.
#[test]
fn list_reviews_nullable_fields() {
    assert_eq!(
        list(r#"[[{"id":5603453798,"body":null,"commit_id":null}]]"#).unwrap(),
        [Review {
            id: 5603453798,
            body: String::new(),
            commit_id: None,
            submitted_at: None
        }]
    );
    let got = list(r#"[[{"id":1,"body":"b","submitted_at":null},{"id":2,"body":"b","submitted_at":"2026-10-21T17:13:38Z"}]]"#).unwrap();
    assert_eq!(got[0].submitted_at, None);
    assert_eq!(got[1].submitted_at.as_deref(), Some("2026-10-21T17:13:38Z"));
}

/// P23 recorded comment fixtures.
#[test]
fn review_comments_parse_recorded_fixtures() {
    let dir = TempDir::new().unwrap();
    replay(&dir, "comments-cli-14783-range.json");
    let got = gh(&dir)
        .review_comments(&repo(), 14783, 5603453798)
        .unwrap();
    assert_eq!(
        got,
        comments_from(
            &fixture_json("comments-cli-14783-range.json")[0],
            5603453798
        )
    );
    assert_eq!(
        got.iter().map(|c| c.id).collect::<Vec<_>>(),
        [4339733038, 4339733147, 4339733201]
    );
    assert_eq!(
        got.iter().map(|c| c.line).collect::<Vec<_>>(),
        [Some(600), Some(303), Some(176)]
    );
    let dir = TempDir::new().unwrap();
    replay(&dir, "comments-cli-14148.json");
    let cli = gh(&dir);
    let got = cli.review_comments(&repo(), 14148, 4942882815).unwrap();
    assert_eq!(
        got,
        comments_from(&fixture_json("comments-cli-14148.json")[0], 4942882815)
    );
    assert_eq!(got.iter().map(|c| c.id).collect::<Vec<_>>(), [3788541562]);
    assert_eq!(got[0].line, Some(16));
    let got = cli.review_comments(&repo(), 14148, 4942057440).unwrap();
    assert_eq!(got.iter().map(|c| c.id).collect::<Vec<_>>(), [3787910859]);
    assert_eq!(cli.review_comments(&repo(), 14148, 1).unwrap(), []);
}

fn item(id: u64, review: Option<u64>) -> Value {
    let mut value =
        json!({"id": id, "path": format!("p{id}"), "body": format!("b{id}"), "line": id});
    if let Some(review) = review {
        value["pull_request_review_id"] = json!(review);
    }
    value
}

/// P24 filter, sort, pagination, arguments.
#[test]
fn review_comments_filter_sort_pagination_and_arguments() {
    let mut four = item(4, None);
    four["pull_request_review_id"] = Value::Null;
    let pages = json!([
        [item(9, Some(7)), item(3, Some(8)), item(5, Some(7)), four],
        [item(1, Some(7)), item(2, None), item(6, Some(8))]
    ])
    .to_string();
    let ids = |review| -> Vec<u64> {
        comments(&pages, review)
            .unwrap()
            .iter()
            .map(|c| c.id)
            .collect()
    };
    assert_eq!(ids(7), [1, 5, 9]);
    assert_eq!(ids(8), [3, 6]);
    assert_eq!(ids(99), Vec::<u64>::new());
    assert_eq!(comments("[[]]", 7).unwrap(), []);
    let big = json!([
        (101..=200)
            .rev()
            .map(|id| item(id, Some(7)))
            .collect::<Vec<_>>(),
        [item(50, Some(7))]
    ])
    .to_string();
    let got = comments(&big, 7).unwrap();
    assert_eq!(got.len(), 101);
    assert_eq!((got[0].id, got[1].id, got[100].id), (50, 101, 200));
    assert_eq!(got[0].path, "p50");
    let dir = setup(b"[[]]", None, 0);
    gh(&dir).review_comments(&repo(), 14148, 7).unwrap();
    assert_eq!(
        args_of(&dir)[5..],
        [
            "--paginate",
            "--slurp",
            "repos/cli/cli/pulls/14148/comments?per_page=100"
        ]
    );
}

/// P25 line null or absent.
#[test]
fn review_comments_line_null_or_absent_is_none() {
    let line = |extra: &str| {
        let text =
            format!(r#"[[{{"id":1,"pull_request_review_id":7,"path":"p","body":"b"{extra}}}]]"#);
        comments(&text, 7).unwrap()[0].line
    };
    assert_eq!(line(r#","line":null"#), None);
    assert_eq!(line(""), None);
    assert_eq!(line(r#","line":3,"original_line":9"#), Some(3));
}

/// P26 read errors map through map_failure.
#[test]
fn reads_map_errors() {
    let not_found = GitHubError::PrNotFound {
        repo: "cli/cli".into(),
        number: 14148,
    };
    for which in 0..2 {
        let run = |stdout: &str, stderr: Option<&str>, exit: i32| {
            let dir = setup(stdout.as_bytes(), stderr, exit);
            let cli = gh(&dir);
            if which == 0 {
                cli.list_reviews(&repo(), 14148).map(|_| ())
            } else {
                cli.review_comments(&repo(), 14148, 1).map(|_| ())
            }
        };
        assert_eq!(
            run("", Some("gh: Not Found (HTTP 404)"), 1),
            Err(not_found.clone())
        );
        assert_eq!(run("", Some("gh: HTTP 404"), 1), Err(not_found.clone()));
        assert!(matches!(
            run("", Some(LOGIN), 4),
            Err(GitHubError::GhUnauthenticated { .. })
        ));
        assert!(matches!(
            run("", Some("gh: HTTP 401"), 1),
            Err(GitHubError::GhUnauthenticated { .. })
        ));
        for stdout in ["not json", r#"[{"id":1}]"#] {
            assert!(matches!(
                run(stdout, None, 0),
                Err(GitHubError::GhFailed { status: Some(0), stderr }) if stderr.starts_with("unexpected gh output: ")
            ));
        }
    }
}

fn two_comment_request(body: &str) -> ReviewRequest {
    ReviewRequest {
        body: body.into(),
        comments: vec![comment("a.rs", "c1", 1), comment("b.rs", "c2", 2)],
        ..request()
    }
}

/// F1 the fake creates reviews and records requests.
#[test]
fn fake_creates_reviews_and_records_requests() {
    let fake = FakeGitHub::new();
    let (one, two) = (repo(), Repo::github("cli", "cli").unwrap());
    // Reviews are keyed by repo and number: another repo's PR 1 sees none.
    let other = Repo::github("octo", "widget").unwrap();
    {
        let f = FakeGitHub::new();
        f.create_review(&other, 1, &request()).unwrap();
        assert_eq!(f.list_reviews(&one, 1), Ok(vec![]));
        assert_eq!(f.review_comments(&one, 1, 1), Ok(vec![]));
    }
    let r1 = two_comment_request("first");
    let r2 = ReviewRequest {
        body: "second".into(),
        comments: vec![comment("c.rs", "c3", 3)],
        ..request()
    };
    let r3 = ReviewRequest {
        body: "third".into(),
        comments: vec![comment("d.rs", "c4", 4)],
        ..request()
    };
    assert_eq!(
        fake.create_review(&one, 1, &r1),
        Ok(ReviewCreated { id: 1 })
    );
    assert_eq!(
        fake.create_review(&one, 1, &r2),
        Ok(ReviewCreated { id: 2 })
    );
    assert_eq!(
        fake.create_review(&two, 2, &r3),
        Ok(ReviewCreated { id: 3 })
    );
    let at = |number, request: &ReviewRequest| FakeReviewRequest {
        repo: one.clone(),
        number,
        request: request.clone(),
    };
    assert_eq!(fake.review_requests(), [at(1, &r1), at(1, &r2), at(2, &r3)]);
    let review = |id, body: &str| Review {
        id,
        body: body.into(),
        commit_id: Some("a".repeat(40)),
        submitted_at: Some(FAKE_SUBMITTED_AT.into()),
    };
    assert_eq!(
        fake.list_reviews(&one, 1),
        Ok(vec![review(1, "first"), review(2, "second")])
    );
    let posted = |id, path: &str, body: &str, line| PostedComment {
        id,
        path: path.into(),
        line: Some(line),
        body: body.into(),
    };
    assert_eq!(
        fake.review_comments(&one, 1, 1),
        Ok(vec![posted(1, "a.rs", "c1", 1), posted(2, "b.rs", "c2", 2)])
    );
    assert_eq!(
        fake.review_comments(&one, 1, 2),
        Ok(vec![posted(3, "c.rs", "c3", 3)])
    );
    assert_eq!(
        fake.review_comments(&one, 2, 3),
        Ok(vec![posted(4, "d.rs", "c4", 4)])
    );
    assert_eq!(fake.review_comments(&one, 1, 3), Ok(vec![]));
    let call = |method, number| FakeCall {
        method,
        repo: one.clone(),
        number,
    };
    use FakeMethod::{CreateReview, ListReviews, ReviewComments};
    assert_eq!(
        fake.calls(),
        [
            call(CreateReview, 1),
            call(CreateReview, 1),
            call(CreateReview, 2),
            call(ListReviews, 1),
            call(ReviewComments, 1),
            call(ReviewComments, 1),
            call(ReviewComments, 2),
            call(ReviewComments, 1),
        ]
    );
}

/// F2 a rejection stores nothing.
#[test]
fn fake_rejection_stores_nothing() {
    let fake = FakeGitHub::new();
    fake.push_error(FakeMethod::CreateReview, rejected(422, "m"));
    let req = two_comment_request("x");
    assert_eq!(
        fake.create_review(&repo(), 1, &req),
        Err(rejected(422, "m"))
    );
    assert_eq!(fake.review_requests().len(), 1);
    assert_eq!(fake.review_requests()[0].request, req);
    assert_eq!(fake.list_reviews(&repo(), 1), Ok(vec![]));
    assert_eq!(
        fake.create_review(&repo(), 1, &req),
        Ok(ReviewCreated { id: 1 })
    );
    assert_eq!(fake.review_comments(&repo(), 1, 1).unwrap()[0].id, 1);
}

/// F3 created, then an unknown outcome.
#[test]
fn fake_created_then_unknown_shows_in_list_reviews() {
    let fake = FakeGitHub::new();
    fake.push_created_then_error(unknown("timeout"));
    let req = two_comment_request("body <!-- prguide:01J -->");
    assert_eq!(
        fake.create_review(&repo(), 1, &req),
        Err(unknown("timeout"))
    );
    let reviews = fake.list_reviews(&repo(), 1).unwrap();
    assert_eq!(reviews.len(), 1);
    assert_eq!(reviews[0].id, 1);
    assert_eq!(reviews[0].body, "body <!-- prguide:01J -->");
    assert_eq!(fake.review_comments(&repo(), 1, 1).unwrap().len(), 2);
    assert_eq!(
        fake.create_review(&repo(), 1, &req),
        Ok(ReviewCreated { id: 2 })
    );
}

/// F4 an unknown outcome without creation.
#[test]
fn fake_unknown_without_creation_stores_nothing() {
    let fake = FakeGitHub::new();
    fake.push_error(FakeMethod::CreateReview, unknown("timeout"));
    assert_eq!(
        fake.create_review(&repo(), 1, &request()),
        Err(unknown("timeout"))
    );
    assert_eq!(fake.list_reviews(&repo(), 1), Ok(vec![]));
}

/// F5 errors and delays apply to the new methods.
#[test]
fn fake_errors_and_delays_apply_to_new_methods() {
    let fake = FakeGitHub::new();
    let err = unknown("e");
    fake.push_error(FakeMethod::ListReviews, err.clone());
    fake.push_error(FakeMethod::ReviewComments, err.clone());
    assert_eq!(fake.list_reviews(&repo(), 1), Err(err.clone()));
    assert_eq!(fake.list_reviews(&repo(), 1), Ok(vec![]));
    assert_eq!(fake.review_comments(&repo(), 1, 1), Err(err));
    assert_eq!(fake.review_comments(&repo(), 1, 1), Ok(vec![]));
    fake.set_delay(FakeMethod::CreateReview, Duration::from_millis(300));
    let start = Instant::now();
    fake.create_review(&repo(), 1, &request()).unwrap();
    assert!(start.elapsed() >= Duration::from_millis(300));
}
