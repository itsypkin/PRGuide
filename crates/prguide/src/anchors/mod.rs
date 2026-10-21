//! Guide anchor handling, filled by T2-5.

pub mod block;
pub mod inline;
pub mod mapping;

#[derive(Debug, thiserror::Error)]
pub enum AnchorError {}

impl AnchorError {
    pub fn code(&self) -> &'static str {
        match *self {}
    }
}
