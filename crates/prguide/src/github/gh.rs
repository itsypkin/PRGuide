//! GitHub command support through the `gh` CLI.

use std::{
    ffi::OsString,
    process::{Command, Stdio},
};

use serde::Deserialize;

use super::{
    API_VERSION, Commit, CommitAuthor, GITHUB_HOST, GitHub, GitHubError, Pr, PrState, Repo,
    check_commit_count,
};

pub struct GhCli {
    program: OsString,
    env: Vec<(OsString, OsString)>,
}

impl GhCli {
    pub fn new() -> Self {
        Self::with_program("gh")
    }
    pub fn with_program(program: impl Into<OsString>) -> Self {
        Self {
            program: program.into(),
            env: Vec::new(),
        }
    }
    pub fn env(mut self, key: impl Into<OsString>, value: impl Into<OsString>) -> Self {
        self.env.push((key.into(), value.into()));
        self
    }

    fn api(&self, args: &[&str]) -> Result<String, GitHubError> {
        let header = format!("X-GitHub-Api-Version: {API_VERSION}");
        let mut command = Command::new(&self.program);
        command
            .args(["api", "-H", &header, "--hostname", GITHUB_HOST])
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (key, value) in &self.env {
            command.env(key, value);
        }
        command
            .env_remove("GH_FORCE_TTY")
            .env_remove("CLICOLOR_FORCE");
        let output = command.output().map_err(|error| {
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
        })?;
        let stderr = String::from_utf8_lossy(&output.stderr);
        if !output.status.success() {
            return Err(map_failure(
                output.status.code(),
                &stderr,
                GitHubError::PrNotFound {
                    repo: String::new(),
                    number: 0,
                },
            ));
        }
        String::from_utf8(output.stdout).map_err(|error| GitHubError::GhFailed {
            status: Some(0),
            stderr: clean_stderr(&format!("unexpected gh output: {error}")),
        })
    }

    fn api_for_pr(&self, args: &[&str], repo: &Repo, number: u64) -> Result<String, GitHubError> {
        match self.api(args) {
            Err(GitHubError::PrNotFound { .. }) => Err(GitHubError::PrNotFound {
                repo: repo.to_string(),
                number,
            }),
            result => result,
        }
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
}

pub(crate) fn map_failure(
    status: Option<i32>,
    stderr: &str,
    not_found: GitHubError,
) -> GitHubError {
    let stderr = clean_stderr(stderr);
    if status == Some(4) || stderr.contains("(HTTP 401)") {
        GitHubError::GhUnauthenticated { stderr }
    } else if stderr.contains("(HTTP 404)") {
        not_found
    } else {
        GitHubError::GhFailed { status, stderr }
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
        assert!(matches!(
            map_failure(Some(4), "(HTTP 404)", missing.clone()),
            GitHubError::GhUnauthenticated { .. }
        ));
        assert_eq!(map_failure(Some(1), "(HTTP 404)", missing.clone()), missing);
        assert!(matches!(
            map_failure(Some(1), "(HTTP 401)", missing.clone()),
            GitHubError::GhUnauthenticated { .. }
        ));
        assert!(matches!(
            map_failure(None, "other", missing),
            GitHubError::GhFailed { status: None, .. }
        ));
        let cleaned = clean_stderr(&"x".repeat(2500));
        assert_eq!(cleaned.len(), 2003);
        assert!(cleaned.ends_with('…'));
    }
}
