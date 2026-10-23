//! Guide data and validation, filled by T3 tasks.

pub mod check;
pub mod commit;
pub mod model;
pub mod new;
pub mod parse;
pub mod range;

#[derive(Debug, thiserror::Error)]
pub enum GuideError {
    #[error("{0}")]
    RangeNotInPr(String),
    #[error("{0}")]
    RangeHasMerge(String),
    #[error("{0}")]
    NoBaseCommit(String),
}

impl GuideError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::RangeNotInPr(_) => "range_not_in_pr",
            Self::RangeHasMerge(_) => "range_has_merge",
            Self::NoBaseCommit(_) => "no_base_commit",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::GuideError;

    #[test]
    fn error_codes_are_stable() {
        assert_eq!(
            GuideError::RangeNotInPr(String::new()).code(),
            "range_not_in_pr"
        );
        assert_eq!(
            GuideError::RangeHasMerge(String::new()).code(),
            "range_has_merge"
        );
        assert_eq!(
            GuideError::NoBaseCommit(String::new()).code(),
            "no_base_commit"
        );
    }
}
