//! PRGuide library entry points.

pub mod anchors;
pub mod cli;
pub mod config;
pub mod error;
pub mod git;
pub mod github;
pub mod guide;
pub mod launcher;
pub mod render;
pub mod store;
pub mod viewer;

pub use error::Error;
