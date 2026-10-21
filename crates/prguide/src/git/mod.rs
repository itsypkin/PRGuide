//! Git integration, filled by T2 tasks.

pub mod diff;
pub mod refs;
pub mod runner;
pub mod worktree;

#[derive(Debug, thiserror::Error)]
pub enum GitError {}

impl GitError {
    pub fn code(&self) -> &'static str {
        match *self {}
    }
}
