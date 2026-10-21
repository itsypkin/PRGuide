//! GitHub integration module; T2-1 and T7-2 through T7-5 fill its child slots.

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
