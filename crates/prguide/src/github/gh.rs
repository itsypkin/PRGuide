//! GitHub command support through the `gh` CLI.

use std::{
    ffi::OsString,
    io::{Read, Write},
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

use serde::Deserialize;
use serde_json::Value;

use super::{
    API_VERSION, Commit, CommitAuthor, GITHUB_HOST, GitHub, GitHubError, PostedComment, Pr,
    PrState, Repo, Review, ReviewCreated, ReviewRequest, check_commit_count,
};

/// How long `create_review` waits for `gh` before it gives up (gh has no timeout of its own).
pub const POST_TIMEOUT: Duration = Duration::from_secs(30);

pub struct GhCli {
    program: OsString,
    env: Vec<(OsString, OsString)>,
    post_timeout: Duration,
}

impl GhCli {
    pub fn new() -> Self {
        Self::with_program("gh")
    }
    pub fn with_program(program: impl Into<OsString>) -> Self {
        Self {
            program: program.into(),
            env: Vec::new(),
            post_timeout: POST_TIMEOUT,
        }
    }
    pub fn post_timeout(mut self, timeout: Duration) -> Self {
        self.post_timeout = timeout;
        self
    }
    pub fn env(mut self, key: impl Into<OsString>, value: impl Into<OsString>) -> Self {
        self.env.push((key.into(), value.into()));
        self
    }
    /// `gh api` with the pinned version header and host, and the environment handling
    /// shared by every call. Callers add their own arguments and stdio.
    fn command(&self) -> Command {
        let header = format!("X-GitHub-Api-Version: {API_VERSION}");
        let mut command = Command::new(&self.program);
        command.args(["api", "-H", &header, "--hostname", GITHUB_HOST]);
        for (key, value) in &self.env {
            command.env(key, value);
        }
        command
            .env_remove("GH_FORCE_TTY")
            .env_remove("CLICOLOR_FORCE");
        command
    }

    fn spawn_error(&self, error: &std::io::Error) -> GitHubError {
        if error.kind() == std::io::ErrorKind::NotFound {
            GitHubError::GhMissing {
                program: self.program.to_string_lossy().into_owned(),
            }
        } else {
            GitHubError::GhFailed {
                status: None,
                stderr: clean_stderr(&format!(
                    "failed to run {}: {error}",
                    self.program.to_string_lossy()
                )),
            }
        }
    }

    fn api(&self, args: &[&str], not_found: GitHubError) -> Result<String, GitHubError> {
        let mut command = self.command();
        command
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let output = command.output().map_err(|error| self.spawn_error(&error))?;
        let stderr = String::from_utf8_lossy(&output.stderr);
        if !output.status.success() {
            return Err(map_failure(output.status.code(), &stderr, not_found));
        }
        String::from_utf8(output.stdout).map_err(|error| GitHubError::GhFailed {
            status: Some(0),
            stderr: clean_stderr(&format!("unexpected gh output: {error}")),
        })
    }

    fn api_for_pr(&self, args: &[&str], repo: &Repo, number: u64) -> Result<String, GitHubError> {
        self.api(
            args,
            GitHubError::PrNotFound {
                repo: repo.to_string(),
                number,
            },
        )
    }

    fn post_review(
        &self,
        endpoint: &str,
        request: &ReviewRequest,
    ) -> Result<ReviewCreated, GitHubError> {
        let unknown = |reason: String| GitHubError::GhUnknownOutcome { reason };
        let mut command = self.command();
        command
            .args(["--include", "--method", "POST", endpoint, "--input", "-"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command.spawn().map_err(|error| self.spawn_error(&error))?;
        let started = Instant::now();
        let (Some(stdin), Some(stdout), Some(stderr)) =
            (child.stdin.take(), child.stdout.take(), child.stderr.take())
        else {
            kill_and_reap(&mut child);
            return Err(unknown("gh's pipes were not available".into()));
        };
        let body = serde_json::to_vec(request).unwrap_or_default();
        // Never joined: a write error (gh exited without reading) is fine, and the thread
        // ends when the pipe closes.
        let writer = thread::Builder::new().spawn(move || {
            let mut stdin = stdin;
            let _ = stdin.write_all(&body);
        });
        let out_reader = thread::Builder::new().spawn(move || read_all(stdout));
        let err_reader = thread::Builder::new().spawn(move || read_all(stderr));
        let (Ok(_), Ok(out_reader), Ok(err_reader)) = (writer, out_reader, err_reader) else {
            kill_and_reap(&mut child);
            return Err(unknown("could not start a thread for gh's pipes".into()));
        };
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) => {}
                Err(error) => {
                    kill_and_reap(&mut child);
                    return Err(unknown(format!("could not wait for gh: {error}")));
                }
            }
            if started.elapsed() >= self.post_timeout {
                kill_and_reap(&mut child);
                return Err(unknown(format!(
                    "no response within {} ms",
                    self.post_timeout.as_millis()
                )));
            }
            thread::sleep(Duration::from_millis(10));
        };
        let stdout = out_reader.join().unwrap_or_default();
        let stderr = String::from_utf8_lossy(&err_reader.join().unwrap_or_default()).into_owned();
        decide_review(status.code(), &stdout, &stderr)
    }
}

impl Default for GhCli {
    fn default() -> Self {
        Self::new()
    }
}

impl GitHub for GhCli {
    fn pr(&self, repo: &Repo, number: u64) -> Result<Pr, GitHubError> {
        let endpoint = format!("repos/{}/{}/pulls/{number}", repo.owner(), repo.name());
        let output = self.api_for_pr(&[&endpoint], repo, number)?;
        serde_json::from_str::<ApiPr>(&output)
            .map(Into::into)
            .map_err(|error| GitHubError::GhFailed {
                status: Some(0),
                stderr: clean_stderr(&format!("unexpected gh output: {error}")),
            })
    }

    fn commits(&self, repo: &Repo, number: u64) -> Result<Vec<Commit>, GitHubError> {
        let endpoint = format!(
            "repos/{}/{}/pulls/{number}/commits?per_page=100",
            repo.owner(),
            repo.name()
        );
        let output = self.api_for_pr(&["--paginate", "--slurp", &endpoint], repo, number)?;
        let pages: Vec<Vec<ApiCommit>> =
            serde_json::from_str(&output).map_err(|error| GitHubError::GhFailed {
                status: Some(0),
                stderr: clean_stderr(&format!("unexpected gh output: {error}")),
            })?;
        let commits: Vec<Commit> = pages.into_iter().flatten().map(Into::into).collect();
        check_commit_count(repo, number, commits.len())?;
        Ok(commits)
    }

    fn create_review(
        &self,
        repo: &Repo,
        number: u64,
        request: &ReviewRequest,
    ) -> Result<ReviewCreated, GitHubError> {
        let endpoint = format!(
            "repos/{}/{}/pulls/{number}/reviews",
            repo.owner(),
            repo.name()
        );
        self.post_review(&endpoint, request)
    }

    fn review_comments(
        &self,
        repo: &Repo,
        number: u64,
        review_id: u64,
    ) -> Result<Vec<PostedComment>, GitHubError> {
        let endpoint = format!(
            "repos/{}/{}/pulls/{number}/comments?per_page=100",
            repo.owner(),
            repo.name()
        );
        let output = self.api_for_pr(&["--paginate", "--slurp", &endpoint], repo, number)?;
        let pages: Vec<Vec<ApiReviewComment>> =
            serde_json::from_str(&output).map_err(|error| unexpected_output(&error))?;
        let mut comments: Vec<PostedComment> = pages
            .into_iter()
            .flatten()
            .filter(|comment| comment.pull_request_review_id == Some(review_id))
            .map(|comment| PostedComment {
                id: comment.id,
                path: comment.path,
                line: comment.line,
                body: comment.body,
            })
            .collect();
        comments.sort_by_key(|comment| comment.id);
        Ok(comments)
    }

    fn list_reviews(&self, repo: &Repo, number: u64) -> Result<Vec<Review>, GitHubError> {
        let endpoint = format!(
            "repos/{}/{}/pulls/{number}/reviews?per_page=100",
            repo.owner(),
            repo.name()
        );
        let output = self.api_for_pr(&["--paginate", "--slurp", &endpoint], repo, number)?;
        let pages: Vec<Vec<ApiReview>> =
            serde_json::from_str(&output).map_err(|error| unexpected_output(&error))?;
        Ok(pages
            .into_iter()
            .flatten()
            .map(|review| Review {
                id: review.id,
                body: review.body.unwrap_or_default(),
                commit_id: review.commit_id,
                submitted_at: review.submitted_at,
            })
            .collect())
    }
}

#[derive(Deserialize)]
struct ApiReview {
    id: u64,
    body: Option<String>,
    commit_id: Option<String>,
    submitted_at: Option<String>,
}
#[derive(Deserialize)]
struct ApiReviewComment {
    id: u64,
    pull_request_review_id: Option<u64>,
    path: String,
    line: Option<u32>,
    body: String,
}

fn kill_and_reap(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

fn read_all(mut reader: impl Read) -> Vec<u8> {
    let mut bytes = Vec::new();
    let _ = reader.read_to_end(&mut bytes);
    bytes
}

/// Splits `gh api --include` stdout into the status code on its first line and the body.
fn parse_included(stdout: &[u8]) -> (Option<u16>, Option<&[u8]>) {
    let (first, mut rest) = match stdout.iter().position(|&byte| byte == b'\n') {
        Some(at) => (&stdout[..at], &stdout[at + 1..]),
        None => (stdout, &stdout[stdout.len()..]),
    };
    let Some(code) = status_code(first.strip_suffix(b"\r").unwrap_or(first)) else {
        return (None, None);
    };
    // Headers run up to the first empty line; the body is everything after it.
    loop {
        if rest.is_empty() {
            return (Some(code), None);
        }
        let (line, next) = match rest.iter().position(|&byte| byte == b'\n') {
            Some(at) => (&rest[..at], &rest[at + 1..]),
            None => (rest, &rest[rest.len()..]),
        };
        if line.strip_suffix(b"\r").unwrap_or(line).is_empty() {
            return (Some(code), Some(next));
        }
        rest = next;
    }
}

fn status_code(line: &[u8]) -> Option<u16> {
    let line = std::str::from_utf8(line).ok()?;
    let (proto, rest) = line.split_once(' ')?;
    if !matches!(proto, "HTTP/1.0" | "HTTP/1.1" | "HTTP/2" | "HTTP/2.0") {
        return None;
    }
    let digits = rest.get(..3)?;
    if !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let after = &rest[3..];
    if !after.is_empty() && !after.starts_with(' ') {
        return None;
    }
    digits.parse().ok()
}

/// The outcome of a review POST, decided from the status line only (never from stderr).
fn decide_review(
    exit: Option<i32>,
    stdout: &[u8],
    stderr: &str,
) -> Result<ReviewCreated, GitHubError> {
    let unknown = |reason: String| GitHubError::GhUnknownOutcome { reason };
    let (code, body) = parse_included(stdout);
    let Some(code) = code else {
        if exit == Some(4) {
            return Err(GitHubError::GhUnauthenticated {
                stderr: clean_stderr(stderr),
            });
        }
        let exit = exit.map_or_else(|| "signal".to_owned(), |code| code.to_string());
        let cleaned = clean_stderr(stderr);
        let detail = if cleaned.is_empty() {
            String::new()
        } else {
            format!(": {cleaned}")
        };
        return Err(unknown(format!(
            "no HTTP status from gh (exit {exit}){detail}"
        )));
    };
    let parsed = body.and_then(|body| serde_json::from_slice::<Value>(body).ok());
    match code {
        200..=299 => parsed
            .as_ref()
            .and_then(Value::as_object)
            .and_then(|object| object.get("id"))
            .and_then(Value::as_u64)
            .map(|id| ReviewCreated { id })
            .ok_or_else(|| unknown(format!("HTTP {code:03} with an unreadable body"))),
        400..=499 if code != 408 => Err(GitHubError::GhRejected {
            status: code,
            message: rejected_message(parsed.as_ref(), stderr),
        }),
        _ => Err(unknown(format!("HTTP {code:03}"))),
    }
}

fn rejected_message(body: Option<&Value>, stderr: &str) -> String {
    let object = body.and_then(Value::as_object);
    let non_empty = |value: &Value| {
        value
            .as_str()
            .filter(|text| !text.is_empty())
            .map(str::to_owned)
    };
    let message = object
        .and_then(|object| object.get("message"))
        .and_then(non_empty);
    let details: Vec<String> = match object.and_then(|object| object.get("errors")) {
        Some(Value::String(text)) if !text.is_empty() => vec![text.clone()],
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|item| match item {
                Value::String(_) => non_empty(item),
                Value::Object(map) => map.get("message").and_then(non_empty),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    };
    let joined = details.join("; ");
    let text = match (message, details.is_empty()) {
        (Some(message), false) => format!("{message}: {joined}"),
        (Some(message), true) => message,
        (None, false) => joined,
        (None, true) => {
            let cleaned = clean_stderr(stderr);
            if cleaned.is_empty() {
                "no message from GitHub".to_owned()
            } else {
                cleaned
            }
        }
    };
    clean_stderr(&text)
}

fn unexpected_output(error: &serde_json::Error) -> GitHubError {
    GitHubError::GhFailed {
        status: Some(0),
        stderr: clean_stderr(&format!("unexpected gh output: {error}")),
    }
}

pub(crate) fn map_failure(
    status: Option<i32>,
    stderr: &str,
    not_found: GitHubError,
) -> GitHubError {
    let cleaned_stderr = clean_stderr(stderr);
    let summary_status = stderr.lines().rev().find_map(summary_status);
    if status == Some(4) || summary_status == Some(401) {
        GitHubError::GhUnauthenticated {
            stderr: cleaned_stderr,
        }
    } else if summary_status == Some(404) {
        not_found
    } else {
        GitHubError::GhFailed {
            status,
            stderr: cleaned_stderr,
        }
    }
}

/// The HTTP status in a gh summary line: `gh: <text> (HTTP nnn)` or the bare `gh: HTTP nnn`.
fn summary_status(line: &str) -> Option<u16> {
    let rest = line.strip_prefix("gh: ")?;
    let digits = if rest.ends_with(')') && rest.contains("(HTTP ") {
        let (_, code) = rest.rsplit_once("(HTTP ")?;
        code.strip_suffix(')')?
    } else {
        rest.strip_prefix("HTTP ")?
    };
    if digits.len() == 3 && digits.bytes().all(|byte| byte.is_ascii_digit()) {
        digits.parse().ok()
    } else {
        None
    }
}

pub(crate) fn clean_stderr(stderr: &str) -> String {
    let stderr = stderr.trim();
    if stderr.len() <= 2000 {
        return stderr.to_owned();
    }
    let mut end = 2000;
    while !stderr.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &stderr[..end])
}

#[derive(Deserialize)]
struct ApiPr {
    number: u64,
    state: ApiPrState,
    merged_at: Option<String>,
    closed_at: Option<String>,
    head: ApiHead,
    base: ApiBase,
    title: String,
    body: Option<String>,
    user: ApiUser,
    html_url: String,
}
#[derive(Deserialize)]
#[serde(rename_all = "lowercase")]
enum ApiPrState {
    Open,
    Closed,
}
#[derive(Deserialize)]
struct ApiHead {
    sha: String,
    repo: Option<ApiRepo>,
}
#[derive(Deserialize)]
struct ApiRepo {
    full_name: String,
}
#[derive(Deserialize)]
struct ApiBase {
    #[serde(rename = "ref")]
    reference: String,
    sha: String,
}
#[derive(Deserialize)]
struct ApiUser {
    login: String,
}
impl From<ApiPr> for Pr {
    fn from(value: ApiPr) -> Self {
        Self {
            number: value.number,
            state: match value.state {
                ApiPrState::Open => PrState::Open,
                ApiPrState::Closed => PrState::Closed,
            },
            merged_at: value.merged_at,
            closed_at: value.closed_at,
            head_sha: value.head.sha,
            head_repo: value.head.repo.map(|repo| repo.full_name),
            base_ref: value.base.reference,
            base_sha: value.base.sha,
            title: value.title,
            body: value.body.unwrap_or_default(),
            author: value.user.login,
            url: value.html_url,
        }
    }
}

#[derive(Deserialize)]
struct ApiCommit {
    sha: String,
    parents: Vec<ApiParent>,
    commit: ApiCommitDetails,
    author: Option<ApiUser>,
}
#[derive(Deserialize)]
struct ApiParent {
    sha: String,
}
#[derive(Deserialize)]
struct ApiCommitDetails {
    message: String,
    author: ApiCommitAuthor,
}
#[derive(Deserialize)]
struct ApiCommitAuthor {
    name: String,
}
impl From<ApiCommit> for Commit {
    fn from(value: ApiCommit) -> Self {
        Self {
            sha: value.sha,
            parents: value.parents.into_iter().map(|parent| parent.sha).collect(),
            message: value.commit.message,
            author: CommitAuthor {
                name: value.commit.author.name,
                login: value.author.map(|author| author.login),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn maps_failures_and_cleans_stderr() {
        let missing = GitHubError::PrNotFound {
            repo: "a/b".into(),
            number: 1,
        };
        assert_eq!(
            map_failure(Some(4), "", missing.clone()),
            GitHubError::GhUnauthenticated {
                stderr: String::new()
            }
        );
        assert_eq!(
            map_failure(Some(1), "gh: Bad credentials (HTTP 401)", missing.clone()),
            GitHubError::GhUnauthenticated {
                stderr: "gh: Bad credentials (HTTP 401)".into()
            }
        );
        assert_eq!(
            map_failure(Some(1), "gh: Not Found (HTTP 404)", missing.clone()),
            missing
        );
        assert!(matches!(
            map_failure(Some(4), "(HTTP 404)", missing.clone()),
            GitHubError::GhUnauthenticated { .. }
        ));
        assert!(matches!(
            map_failure(Some(1), "other", missing.clone()),
            GitHubError::GhFailed {
                status: Some(1),
                ..
            }
        ));
        let signal = map_failure(None, "other", missing.clone());
        assert!(matches!(signal, GitHubError::GhFailed { status: None, .. }));
        assert!(signal.to_string().contains("exit signal"));
        let long_404 = format!("{}\ngh: Not Found (HTTP 404)", "x".repeat(2500));
        assert_eq!(
            map_failure(Some(1), &long_404, missing.clone()),
            missing.clone()
        );
        let long_401 = format!("{}\ngh: Bad credentials (HTTP 401)", "x".repeat(2500));
        assert!(matches!(
            map_failure(Some(1), &long_401, missing.clone()),
            GitHubError::GhUnauthenticated { ref stderr } if stderr.len() == 2003 && stderr.ends_with('…')
        ));
        let embedded_404_then_401 = format!(
            "{}(HTTP 404)\ngh: Bad credentials (HTTP 401)",
            "x".repeat(2500)
        );
        assert!(matches!(
            map_failure(Some(1), &embedded_404_then_401, missing.clone()),
            GitHubError::GhUnauthenticated { ref stderr } if stderr.len() == 2003 && stderr.ends_with('…')
        ));
        let embedded_401_then_404 =
            format!("{}(HTTP 401)\ngh: Not Found (HTTP 404)", "x".repeat(2500));
        assert_eq!(
            map_failure(Some(1), &embedded_401_then_404, missing.clone()),
            missing.clone()
        );
        assert!(matches!(
            map_failure(
                Some(1),
                "gh: Bad credentials (HTTP 401)\nresponse text (HTTP 404)\n",
                missing.clone()
            ),
            GitHubError::GhUnauthenticated { .. }
        ));
        assert_eq!(
            map_failure(
                Some(1),
                "gh: Not Found (HTTP 404)\ngh: x (HTTP 4040)\n",
                missing.clone()
            ),
            missing.clone()
        );
        assert!(matches!(
            map_failure(
                Some(1),
                "gh: Bad credentials (HTTP 401)\ngh: x (HTTP abc)\n",
                missing.clone()
            ),
            GitHubError::GhUnauthenticated { .. }
        ));
        let scope_hint = "gh: This API operation needs the \"repo\" scope. To request it, run:  gh auth refresh -h github.com -s repo";
        assert_eq!(
            map_failure(
                Some(1),
                &format!("gh: Not Found (HTTP 404)\n{scope_hint}\n"),
                missing.clone()
            ),
            missing.clone()
        );
        assert!(matches!(
            map_failure(
                Some(1),
                &format!("gh: Bad credentials (HTTP 401)\n{scope_hint}\n"),
                missing.clone()
            ),
            GitHubError::GhUnauthenticated { .. }
        ));
        assert_eq!(
            map_failure(
                Some(1),
                "gh: Not Found (HTTP 404)\nAuthorize in your web browser: https://github.com/orgs/x/sso?x\n",
                missing.clone()
            ),
            missing.clone()
        );
        assert!(matches!(
            map_failure(Some(1), " \n\t", missing.clone()),
            GitHubError::GhFailed {
                status: Some(1),
                stderr
            } if stderr.is_empty()
        ));
        assert!(matches!(
            map_failure(
                Some(1),
                "(HTTP 404) in a response body\ngh: Server Error (HTTP 502)",
                GitHubError::PrNotFound {
                    repo: "a/b".into(),
                    number: 1
                }
            ),
            GitHubError::GhFailed {
                status: Some(1),
                ..
            }
        ));
        let cleaned = clean_stderr(&"x".repeat(2500));
        assert_eq!(cleaned.len(), 2003);
        assert!(cleaned.ends_with('…'));
        let multibyte = format!("{}é", "x".repeat(1999));
        assert_eq!(clean_stderr(&multibyte), "x".repeat(1999) + "…");
        assert_eq!(clean_stderr(&"x".repeat(2000)), "x".repeat(2000));
        assert_eq!(clean_stderr(" \n stderr \t"), "stderr");
    }

    fn mapped(stderr: &str) -> GitHubError {
        map_failure(
            Some(1),
            stderr,
            GitHubError::PrNotFound {
                repo: "a/b".into(),
                number: 1,
            },
        )
    }
    fn not_found() -> GitHubError {
        GitHubError::PrNotFound {
            repo: "a/b".into(),
            number: 1,
        }
    }
    fn failed(stderr: &str) -> GitHubError {
        GitHubError::GhFailed {
            status: Some(1),
            stderr: stderr.trim().into(),
        }
    }
    fn unauthenticated(stderr: &str) -> GitHubError {
        GitHubError::GhUnauthenticated {
            stderr: stderr.trim().into(),
        }
    }

    /// U1 the parsed status decides.
    #[test]
    fn map_failure_decides_on_the_parsed_status() {
        let a = "gh: weird (HTTP 401) text (HTTP 404)";
        assert_eq!(mapped(a), not_found());
        let b = "gh: weird (HTTP 404) text (HTTP 401)";
        assert_eq!(mapped(b), unauthenticated(b));
        assert_eq!(mapped("gh: x (HTTP 404) y"), failed("gh: x (HTTP 404) y"));
        assert_eq!(
            mapped("gh: Teapot (HTTP 418)"),
            failed("gh: Teapot (HTTP 418)")
        );
    }

    /// U2 the bare form.
    #[test]
    fn map_failure_accepts_the_bare_http_form() {
        assert_eq!(mapped("gh: HTTP 404"), not_found());
        assert_eq!(mapped("gh: HTTP 401"), unauthenticated("gh: HTTP 401"));
        assert_eq!(mapped("gh: HTTP 502"), failed("gh: HTTP 502"));
        assert_eq!(mapped("gh: HTTP 404\r\n"), not_found());
        for text in [
            "gh: HTTP 4040",
            "gh: HTTP 40",
            "gh: HTTP 40a",
            "xgh: HTTP 404",
            "gh:  HTTP 404",
            "gh: http 404",
            "gh: HTTP 404 extra",
            "HTTP 404",
        ] {
            assert_eq!(mapped(text), failed(text), "{text}");
        }
        assert_eq!(
            mapped("gh: Not Found (HTTP 404)\ngh: HTTP 4040"),
            not_found()
        );
    }

    /// U3 the last summary line wins.
    #[test]
    fn map_failure_last_summary_line_wins() {
        let a = "gh: Not Found (HTTP 404)\ngh: Server Error (HTTP 502)";
        assert_eq!(mapped(a), failed(a));
        assert_eq!(
            mapped("gh: Server Error (HTTP 502)\ngh: Not Found (HTTP 404)"),
            not_found()
        );
        assert_eq!(
            mapped("gh: HTTP 401\ngh: HTTP 502\ngh: Not Found (HTTP 404)"),
            not_found()
        );
        let d = "gh: Not Found (HTTP 404)\ngh: HTTP 401\nAuthorize in your web browser: https://github.com/orgs/x/sso";
        assert_eq!(mapped(d), unauthenticated(d));
    }
}
