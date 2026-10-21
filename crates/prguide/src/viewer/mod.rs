//! Terminal viewer entry point, filled by T6-1.

pub mod app;
pub mod clipboard;
pub mod code;
pub mod component;
pub mod compose;
pub mod crossversion;
pub mod deepdive;
pub mod diagram;
pub mod editor;
pub mod footer;
pub mod github_threads;
pub mod header;
pub mod keys;
pub mod listening;
pub mod markdown;
pub mod nav;
pub mod outbox;
pub mod picker;
pub mod post;
pub mod post_preview;
pub mod refresh;
pub mod select;
pub mod slide;
pub mod terminal;
pub mod threads;
pub mod versions;

use crate::Error;

#[derive(Debug)]
pub struct ViewerArgs {
    pub pr: Option<u64>,
    pub run: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum ViewerError {}

impl ViewerError {
    pub fn code(&self) -> &'static str {
        match *self {}
    }
}

pub fn run(_args: ViewerArgs) -> Result<(), Error> {
    Err(Error::NotImplemented(
        "the viewer is not implemented yet".into(),
    ))
}
