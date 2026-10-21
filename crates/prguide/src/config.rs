//! Configuration support, filled by T4-5.

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {}

impl ConfigError {
    pub fn code(&self) -> &'static str {
        match *self {}
    }
}
