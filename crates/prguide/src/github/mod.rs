//! GitHub integration types and implementations.

use std::fmt;

pub mod fake;
pub mod gh;
pub mod plan;
pub mod threads;

/// REST API version sent on every call.
pub const API_VERSION: &str = "2026-03-10";
/// The only host prguide v1 supports.
pub const GITHUB_HOST: &str = "github.com";
/// GitHub's pull-request commit list stops at this many commits.
pub const MAX_PR_COMMITS: usize = 250;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Repo {
    owner: String,
    name: String,
}

impl Repo {
    pub fn new(host: &str, owner: &str, name: &str) -> Result<Self, GitHubError> {
        if !host.eq_ignore_ascii_case(GITHUB_HOST) {
            return Err(GitHubError::UnsupportedHost { host: host.into() });
        }
        if !valid_part(owner) || !valid_part(name) {
            return Err(GitHubError::InvalidRepo {
                owner: owner.into(),
                name: name.into(),
            });
        }
        Ok(Self {
            owner: owner.into(),
            name: name.into(),
        })
    }

    pub fn github(owner: &str, name: &str) -> Result<Self, GitHubError> {
        Self::new(GITHUB_HOST, owner, name)
    }
    pub fn owner(&self) -> &str {
        &self.owner
    }
    pub fn name(&self) -> &str {
        &self.name
    }
    pub fn host(&self) -> &str {
        GITHUB_HOST
    }
}

fn valid_part(part: &str) -> bool {
    !part.is_empty()
        && part != "."
        && part != ".."
        && part
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

impl fmt::Display for Repo {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}/{}", self.owner, self.name)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PrState {
    Open,
    Closed,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Pr {
    pub number: u64,
    pub state: PrState,
    pub merged_at: Option<String>,
    pub closed_at: Option<String>,
    pub head_sha: String,
    pub head_repo: Option<String>,
    pub base_ref: String,
    pub base_sha: String,
    pub title: String,
    pub body: String,
    pub author: String,
    pub url: String,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct CommitAuthor {
    pub name: String,
    pub login: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Commit {
    pub sha: String,
    pub parents: Vec<String>,
    pub message: String,
    pub author: CommitAuthor,
}

/// The only review event prguide can send: it can never approve.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub enum ReviewEvent {
    #[serde(rename = "COMMENT")]
    Comment,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub enum ReviewSide {
    #[serde(rename = "LEFT")]
    Left,
    #[serde(rename = "RIGHT")]
    Right,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ReviewComment {
    pub path: String,
    pub body: String,
    pub line: u32,
    pub side: ReviewSide,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start_line: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start_side: Option<ReviewSide>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ReviewRequest {
    pub commit_id: String,
    pub body: String,
    pub event: ReviewEvent,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub comments: Vec<ReviewComment>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct ReviewCreated {
    pub id: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Review {
    pub id: u64,
    pub body: String,
    pub commit_id: Option<String>,
    pub submitted_at: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct PostedComment {
    pub id: u64,
    pub path: String,
    pub line: Option<u32>,
    pub body: String,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum GitHubError {
    #[error("prguide v1 supports only github.com, not {host}")]
    UnsupportedHost { host: String },
    #[error("invalid GitHub repository name: {owner}/{name}")]
    InvalidRepo { owner: String, name: String },
    #[error("GitHub CLI not found ({program}); install gh from https://cli.github.com")]
    GhMissing { program: String },
    #[error("gh is not logged in to github.com; run gh auth login ({stderr})")]
    GhUnauthenticated { stderr: String },
    #[error("pull request {repo}#{number} not found, or no access to it")]
    PrNotFound { repo: String, number: u64 },
    #[error(
        "pull request {repo}#{number} has 250 or more commits; GitHub lists only the first 250, so prguide can't review it"
    )]
    PrTooLarge { repo: String, number: u64 },
    #[error("gh failed (exit {}): {stderr}", status.map_or_else(|| "signal".to_owned(), |status| status.to_string()))]
    GhFailed { status: Option<i32>, stderr: String },
    #[error("GitHub rejected the review (HTTP {status}): {message}")]
    GhRejected { status: u16, message: String },
    #[error("GitHub's answer to the review is unknown ({reason}); it may have been posted")]
    GhUnknownOutcome { reason: String },
}

impl GitHubError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::UnsupportedHost { .. } => "unsupported_host",
            Self::InvalidRepo { .. } => "invalid_repo",
            Self::GhMissing { .. } => "gh_missing",
            Self::GhUnauthenticated { .. } => "gh_unauthenticated",
            Self::PrNotFound { .. } => "pr_not_found",
            Self::PrTooLarge { .. } => "pr_too_large",
            Self::GhFailed { .. } => "gh_failed",
            Self::GhRejected { .. } => "gh_rejected",
            Self::GhUnknownOutcome { .. } => "gh_unknown_outcome",
        }
    }
}

pub fn check_commit_count(repo: &Repo, number: u64, count: usize) -> Result<(), GitHubError> {
    if count >= MAX_PR_COMMITS {
        return Err(GitHubError::PrTooLarge {
            repo: repo.to_string(),
            number,
        });
    }
    Ok(())
}

pub trait GitHub: Send + Sync {
    fn pr(&self, repo: &Repo, number: u64) -> Result<Pr, GitHubError>;
    fn commits(&self, repo: &Repo, number: u64) -> Result<Vec<Commit>, GitHubError>;
    fn create_review(
        &self,
        repo: &Repo,
        number: u64,
        request: &ReviewRequest,
    ) -> Result<ReviewCreated, GitHubError>;
    fn review_comments(
        &self,
        repo: &Repo,
        number: u64,
        review_id: u64,
    ) -> Result<Vec<PostedComment>, GitHubError>;
    fn list_reviews(&self, repo: &Repo, number: u64) -> Result<Vec<Review>, GitHubError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// U4 codes are stable and convert.
    #[test]
    fn codes_are_stable_and_convert() {
        let errors = [
            GitHubError::UnsupportedHost { host: "x".into() },
            GitHubError::InvalidRepo {
                owner: "x".into(),
                name: "y".into(),
            },
            GitHubError::GhMissing {
                program: "gh".into(),
            },
            GitHubError::GhUnauthenticated {
                stderr: String::new(),
            },
            GitHubError::PrNotFound {
                repo: "x/y".into(),
                number: 1,
            },
            GitHubError::PrTooLarge {
                repo: "x/y".into(),
                number: 1,
            },
            GitHubError::GhFailed {
                status: Some(1),
                stderr: String::new(),
            },
            GitHubError::GhRejected {
                status: 422,
                message: "m".into(),
            },
            GitHubError::GhUnknownOutcome { reason: "r".into() },
        ];
        let codes = [
            "unsupported_host",
            "invalid_repo",
            "gh_missing",
            "gh_unauthenticated",
            "pr_not_found",
            "pr_too_large",
            "gh_failed",
            "gh_rejected",
            "gh_unknown_outcome",
        ];
        for (error, code) in errors.into_iter().zip(codes) {
            assert_eq!(error.code(), code);
            assert_eq!(crate::error::Error::from(error).code(), code);
        }
        assert_eq!(
            GitHubError::GhRejected {
                status: 422,
                message: "m".into()
            }
            .to_string(),
            "GitHub rejected the review (HTTP 422): m"
        );
        assert_eq!(
            GitHubError::GhUnknownOutcome { reason: "r".into() }.to_string(),
            "GitHub's answer to the review is unknown (r); it may have been posted"
        );
    }
}
