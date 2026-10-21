//! GitHub integration, filled by T2 and T7 tasks.

pub mod fake;
pub mod gh;
pub mod plan;
pub mod threads;

#[derive(Debug, thiserror::Error)]
pub enum GitHubError {}

impl GitHubError {
    pub fn code(&self) -> &'static str {
        match *self {}
    }
}
