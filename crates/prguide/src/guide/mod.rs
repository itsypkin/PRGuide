//! Guide data and validation, filled by T3 tasks.

pub mod check;
pub mod commit;
pub mod model;
pub mod new;
pub mod parse;
pub mod range;

#[derive(Debug, thiserror::Error)]
pub enum GuideError {}

impl GuideError {
    pub fn code(&self) -> &'static str {
        match *self {}
    }
}
