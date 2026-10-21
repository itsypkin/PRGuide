//! Git integration module; T2-2 through T2-4 fill its child slots.

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
