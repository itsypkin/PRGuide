//! Rendering support, filled by T5-1.

pub mod limits;
pub mod mmdflux;

#[derive(Debug, thiserror::Error)]
pub enum RenderError {}

impl RenderError {
    pub fn code(&self) -> &'static str {
        match *self {}
    }
}
