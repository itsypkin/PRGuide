//! Git integration module; T2-2 through T2-4 fill its child slots.

pub mod diff;
pub mod refs;
pub mod runner;
pub mod worktree;

pub use refs::RefMismatch;
pub use runner::{DiffSpec, Git, Refspec, Remote, RepoClone};

/// Errors produced by git integration.
#[derive(Debug, thiserror::Error)]
pub enum GitError {
    /// A git command returned a non-zero status.
    #[error("git command failed ({status}): {command}: {stderr}")]
    Failed {
        command: String,
        status: String,
        stderr: String,
    },
    /// The git executable was unavailable.
    #[error("git executable not found on PATH")]
    Missing,
    /// The directory is not an appropriate clone, or has no matching remote.
    #[error("{0}")]
    NotInClone(String),
    /// A ref name or object ID was rejected before invoking git.
    #[error("invalid ref {name:?}: {reason}")]
    InvalidRef { name: String, reason: String },
}

impl GitError {
    /// Stable machine-readable error code.
    pub fn code(&self) -> &'static str {
        match self {
            Self::Failed { .. } => "git_failed",
            Self::Missing => "git_missing",
            Self::NotInClone(_) => "not_in_clone",
            Self::InvalidRef { .. } => "invalid_ref",
        }
    }
}

/// Parse a github.com remote URL, preserving the owner and repository casing.
pub fn parse_github_remote(url: &str) -> Option<(String, String)> {
    let url = url.strip_suffix('/').unwrap_or(url);
    let path = if let Some((scheme, rest)) = url.split_once("://") {
        if !matches!(scheme, "https" | "http" | "ssh") {
            return None;
        }
        let authority_end = rest.find('/').unwrap_or(rest.len());
        let authority = &rest[..authority_end];
        let host_port = authority.rsplit('@').next()?;
        let host = host_port.split(':').next()?;
        if !host.eq_ignore_ascii_case("github.com") {
            return None;
        }
        &rest[authority_end..]
    } else {
        let (host, path) = url.rsplit_once(':')?;
        if path.starts_with('/') || !host.rsplit('@').next()?.eq_ignore_ascii_case("github.com") {
            return None;
        }
        path
    };
    let path = path.strip_prefix('/').unwrap_or(path);
    let parts: Vec<_> = path.split('/').collect();
    if parts.len() != 2 || parts.iter().any(|part| part.is_empty()) {
        return None;
    }
    let repo = parts[1]
        .strip_suffix(".git")
        .filter(|_| parts[1].ends_with(".git"))
        .or_else(|| {
            parts[1]
                .get(..parts[1].len().saturating_sub(4))
                .filter(|_| {
                    parts[1].len() >= 4
                        && parts[1][parts[1].len() - 4..].eq_ignore_ascii_case(".git")
                })
        })
        .unwrap_or(parts[1]);
    if repo.is_empty() {
        return None;
    }
    Some((parts[0].to_owned(), repo.to_owned()))
}

#[cfg(test)]
mod tests {
    use super::GitError;

    #[test]
    fn error_codes_are_stable() {
        assert_eq!(GitError::Missing.code(), "git_missing");
        assert_eq!(GitError::NotInClone("x".into()).code(), "not_in_clone");
        assert_eq!(
            GitError::InvalidRef {
                name: "x".into(),
                reason: "x".into()
            }
            .code(),
            "invalid_ref"
        );
        assert_eq!(
            GitError::Failed {
                command: "x".into(),
                status: "x".into(),
                stderr: "x".into()
            }
            .code(),
            "git_failed"
        );
    }
}
