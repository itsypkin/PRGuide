//! Application-wide error conversion and stable error codes.

use crate::{anchors, config, git, github, guide, launcher, render, store, viewer};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0}")]
    NotImplemented(String),
    #[error("{0}")]
    Usage(String),
    #[error("internal error: {0}")]
    Internal(String),
    #[error(transparent)]
    Store(#[from] store::StoreError),
    #[error(transparent)]
    Git(#[from] git::GitError),
    #[error(transparent)]
    GitHub(#[from] github::GitHubError),
    #[error(transparent)]
    Anchor(#[from] anchors::AnchorError),
    #[error(transparent)]
    Guide(#[from] guide::GuideError),
    #[error(transparent)]
    Render(#[from] render::RenderError),
    #[error(transparent)]
    Config(#[from] config::ConfigError),
    #[error(transparent)]
    Launch(#[from] launcher::LaunchError),
    #[error(transparent)]
    Viewer(#[from] viewer::ViewerError),
}

impl Error {
    pub fn code(&self) -> &'static str {
        match self {
            Self::NotImplemented(_) => "not_implemented",
            Self::Usage(_) => "usage",
            Self::Internal(_) => "internal",
            Self::Store(error) => error.code(),
            Self::Git(error) => error.code(),
            Self::GitHub(error) => error.code(),
            Self::Anchor(error) => error.code(),
            Self::Guide(error) => error.code(),
            Self::Render(error) => error.code(),
            Self::Config(error) => error.code(),
            Self::Launch(error) => error.code(),
            Self::Viewer(error) => error.code(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Error;

    #[test]
    fn base_error_codes_are_stable() {
        assert_eq!(
            Error::NotImplemented(String::new()).code(),
            "not_implemented"
        );
        assert_eq!(Error::Usage(String::new()).code(), "usage");
        assert_eq!(Error::Internal(String::new()).code(), "internal");
    }
}
