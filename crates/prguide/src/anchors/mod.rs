//! Guide code blocks, line mapping across revisions and GitHub inline locations.

pub mod block;
pub mod inline;
pub mod mapping;

pub use block::{
    At, BlockContext, BlockKind, CodeBlock, ResolvedBlock, Side, StoredAnchor, Stub, Warning,
    resolve,
};
pub use inline::{InlineLoc, inline_location};
pub use mapping::{MappedRange, Mapping, map_lines};

#[derive(Debug, thiserror::Error)]
pub enum AnchorError {
    #[error("{0}")]
    PathNotInDiff(String),
    #[error("{0}")]
    PathNotUtf8(String),
    #[error("{0}")]
    PathNotFound(String),
    #[error("{0}")]
    WrongSide(String),
    #[error("{0}")]
    RangeRequired(String),
    #[error("{0}")]
    RangeNotAllowed(String),
    #[error("{message}")]
    LinesOutOfRange {
        field: &'static str,
        message: String,
    },
    #[error("{0}")]
    NoHunksInRange(String),
}

impl AnchorError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::PathNotInDiff(_) => "path_not_in_diff",
            Self::PathNotUtf8(_) => "path_not_utf8",
            Self::PathNotFound(_) => "path_not_found",
            Self::WrongSide(_) => "wrong_side",
            Self::RangeRequired(_) => "range_required",
            Self::RangeNotAllowed(_) => "range_not_allowed",
            Self::LinesOutOfRange { .. } => "lines_out_of_range",
            Self::NoHunksInRange(_) => "no_hunks_in_range",
        }
    }

    /// The guide field a diagnostic should point at.
    pub fn field(&self) -> &'static str {
        match self {
            Self::PathNotInDiff(_) | Self::PathNotUtf8(_) | Self::PathNotFound(_) => "path",
            Self::WrongSide(_) => "side",
            Self::RangeRequired(_) | Self::RangeNotAllowed(_) | Self::NoHunksInRange(_) => "start",
            Self::LinesOutOfRange { field, .. } => field,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{AnchorError, Side};

    #[test]
    fn error_codes_are_stable() {
        let s = || String::new();
        let cases: [(AnchorError, &str, &str); 9] = [
            (AnchorError::PathNotInDiff(s()), "path_not_in_diff", "path"),
            (AnchorError::PathNotUtf8(s()), "path_not_utf8", "path"),
            (AnchorError::PathNotFound(s()), "path_not_found", "path"),
            (AnchorError::WrongSide(s()), "wrong_side", "side"),
            (AnchorError::RangeRequired(s()), "range_required", "start"),
            (
                AnchorError::RangeNotAllowed(s()),
                "range_not_allowed",
                "start",
            ),
            (
                AnchorError::LinesOutOfRange {
                    field: "start",
                    message: s(),
                },
                "lines_out_of_range",
                "start",
            ),
            (
                AnchorError::LinesOutOfRange {
                    field: "end",
                    message: s(),
                },
                "lines_out_of_range",
                "end",
            ),
            (
                AnchorError::NoHunksInRange(s()),
                "no_hunks_in_range",
                "start",
            ),
        ];
        for (error, code, field) in cases {
            assert_eq!(error.code(), code);
            assert_eq!(error.field(), field);
        }
        assert_eq!(Side::Old.github(), "LEFT");
        assert_eq!(Side::New.github(), "RIGHT");
    }
}
