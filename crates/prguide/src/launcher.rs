//! Browser and editor launcher support, filled by T4-5.

#[derive(Debug, thiserror::Error)]
pub enum LaunchError {}

impl LaunchError {
    pub fn code(&self) -> &'static str {
        match *self {}
    }
}
