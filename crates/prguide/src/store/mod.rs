//! SQLite storage support, filled by T1 tasks.

pub mod deepdives;
pub mod events;
pub mod migrations;
pub mod outbox;
pub mod posts;
pub mod runs;
pub mod schema;
pub mod status;
pub mod threads;
pub mod versions;

#[derive(Debug, thiserror::Error)]
pub enum StoreError {}

impl StoreError {
    pub fn code(&self) -> &'static str {
        match *self {}
    }
}
