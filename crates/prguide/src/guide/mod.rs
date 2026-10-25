//! Guide data and validation, filled by T3 tasks.

pub mod check;
pub mod commit;
pub mod model;
pub mod new;
pub mod parse;
pub mod range;

/// The guide errors. The range errors carry one message. Every parse and
/// draft error carries the file it is about (relative to the draft, `""` for
/// the draft itself), the field path inside that file and a message; they
/// are what `check` prints as diagnostics.
#[derive(Debug, thiserror::Error)]
pub enum GuideError {
    #[error("{0}")]
    RangeNotInPr(String),
    #[error("{0}")]
    RangeHasMerge(String),
    #[error("{0}")]
    NoBaseCommit(String),
    #[error("{message}")]
    YamlInvalid {
        file: String,
        field: String,
        message: String,
    },
    #[error("{message}")]
    YamlFeatureNotAllowed {
        file: String,
        field: String,
        message: String,
    },
    #[error("{message}")]
    FieldUnknown {
        file: String,
        field: String,
        message: String,
    },
    #[error("{message}")]
    FieldMissing {
        file: String,
        field: String,
        message: String,
    },
    #[error("{message}")]
    FieldInvalid {
        file: String,
        field: String,
        message: String,
    },
    #[error("{message}")]
    KeyDuplicate {
        file: String,
        field: String,
        message: String,
    },
    #[error("{message}")]
    FormatUnsupported {
        file: String,
        field: String,
        message: String,
    },
    #[error("{message}")]
    SlideIdInvalid {
        file: String,
        field: String,
        message: String,
    },
    #[error("{message}")]
    BlockIdInvalid {
        file: String,
        field: String,
        message: String,
    },
    #[error("{message}")]
    SlideIdMismatch {
        file: String,
        field: String,
        message: String,
    },
    #[error("{message}")]
    FrontMatterMissing {
        file: String,
        field: String,
        message: String,
    },
    #[error("{message}")]
    FrontMatterUnclosed {
        file: String,
        field: String,
        message: String,
    },
    #[error("{message}")]
    HtmlNotAllowed {
        file: String,
        field: String,
        message: String,
    },
    #[error("{message}")]
    DuplicateDiagramBlock {
        file: String,
        field: String,
        message: String,
    },
    #[error("{message}")]
    DiagramBlockNested {
        file: String,
        field: String,
        message: String,
    },
    #[error("{message}")]
    PathInvalid {
        file: String,
        field: String,
        message: String,
    },
    #[error("{message}")]
    SymlinkNotAllowed {
        file: String,
        field: String,
        message: String,
    },
    #[error("{message}")]
    UnexpectedFile {
        file: String,
        field: String,
        message: String,
    },
    #[error("{message}")]
    SlideFileNameInvalid {
        file: String,
        field: String,
        message: String,
    },
    #[error("{message}")]
    ManifestMissing {
        file: String,
        field: String,
        message: String,
    },
    #[error("{message}")]
    DraftInvalid {
        file: String,
        field: String,
        message: String,
    },
    #[error("{message}")]
    FileUnreadable {
        file: String,
        field: String,
        message: String,
    },
    #[error("{message}")]
    FileNotUtf8 {
        file: String,
        field: String,
        message: String,
    },
}

impl GuideError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::RangeNotInPr(_) => "range_not_in_pr",
            Self::RangeHasMerge(_) => "range_has_merge",
            Self::NoBaseCommit(_) => "no_base_commit",
            Self::YamlInvalid { .. } => "yaml_invalid",
            Self::YamlFeatureNotAllowed { .. } => "yaml_feature_not_allowed",
            Self::FieldUnknown { .. } => "field_unknown",
            Self::FieldMissing { .. } => "field_missing",
            Self::FieldInvalid { .. } => "field_invalid",
            Self::KeyDuplicate { .. } => "key_duplicate",
            Self::FormatUnsupported { .. } => "format_unsupported",
            Self::SlideIdInvalid { .. } => "slide_id_invalid",
            Self::BlockIdInvalid { .. } => "block_id_invalid",
            Self::SlideIdMismatch { .. } => "slide_id_mismatch",
            Self::FrontMatterMissing { .. } => "front_matter_missing",
            Self::FrontMatterUnclosed { .. } => "front_matter_unclosed",
            Self::HtmlNotAllowed { .. } => "html_not_allowed",
            Self::DuplicateDiagramBlock { .. } => "duplicate_diagram_block",
            Self::DiagramBlockNested { .. } => "diagram_block_nested",
            Self::PathInvalid { .. } => "path_invalid",
            Self::SymlinkNotAllowed { .. } => "symlink_not_allowed",
            Self::UnexpectedFile { .. } => "unexpected_file",
            Self::SlideFileNameInvalid { .. } => "slide_file_name_invalid",
            Self::ManifestMissing { .. } => "manifest_missing",
            Self::DraftInvalid { .. } => "draft_invalid",
            Self::FileUnreadable { .. } => "file_unreadable",
            Self::FileNotUtf8 { .. } => "file_not_utf8",
        }
    }

    /// The file a parse or draft error is about; `None` for the range errors.
    pub fn file(&self) -> Option<&str> {
        self.place().map(|(file, _)| file)
    }

    /// The field path inside that file; `None` for the range errors.
    pub fn field(&self) -> Option<&str> {
        self.place().map(|(_, field)| field)
    }

    fn place(&self) -> Option<(&str, &str)> {
        match self {
            Self::RangeNotInPr(_) | Self::RangeHasMerge(_) | Self::NoBaseCommit(_) => None,
            Self::YamlInvalid { file, field, .. }
            | Self::YamlFeatureNotAllowed { file, field, .. }
            | Self::FieldUnknown { file, field, .. }
            | Self::FieldMissing { file, field, .. }
            | Self::FieldInvalid { file, field, .. }
            | Self::KeyDuplicate { file, field, .. }
            | Self::FormatUnsupported { file, field, .. }
            | Self::SlideIdInvalid { file, field, .. }
            | Self::BlockIdInvalid { file, field, .. }
            | Self::SlideIdMismatch { file, field, .. }
            | Self::FrontMatterMissing { file, field, .. }
            | Self::FrontMatterUnclosed { file, field, .. }
            | Self::HtmlNotAllowed { file, field, .. }
            | Self::DuplicateDiagramBlock { file, field, .. }
            | Self::DiagramBlockNested { file, field, .. }
            | Self::PathInvalid { file, field, .. }
            | Self::SymlinkNotAllowed { file, field, .. }
            | Self::UnexpectedFile { file, field, .. }
            | Self::SlideFileNameInvalid { file, field, .. }
            | Self::ManifestMissing { file, field, .. }
            | Self::DraftInvalid { file, field, .. }
            | Self::FileUnreadable { file, field, .. }
            | Self::FileNotUtf8 { file, field, .. } => Some((file, field)),
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
        let cases = [
            (
                GuideError::YamlInvalid {
                    file: String::new(),
                    field: String::new(),
                    message: String::new(),
                },
                "yaml_invalid",
            ),
            (
                GuideError::YamlFeatureNotAllowed {
                    file: String::new(),
                    field: String::new(),
                    message: String::new(),
                },
                "yaml_feature_not_allowed",
            ),
            (
                GuideError::FieldUnknown {
                    file: String::new(),
                    field: String::new(),
                    message: String::new(),
                },
                "field_unknown",
            ),
            (
                GuideError::FieldMissing {
                    file: String::new(),
                    field: String::new(),
                    message: String::new(),
                },
                "field_missing",
            ),
            (
                GuideError::FieldInvalid {
                    file: String::new(),
                    field: String::new(),
                    message: String::new(),
                },
                "field_invalid",
            ),
            (
                GuideError::KeyDuplicate {
                    file: String::new(),
                    field: String::new(),
                    message: String::new(),
                },
                "key_duplicate",
            ),
            (
                GuideError::FormatUnsupported {
                    file: String::new(),
                    field: String::new(),
                    message: String::new(),
                },
                "format_unsupported",
            ),
            (
                GuideError::SlideIdInvalid {
                    file: String::new(),
                    field: String::new(),
                    message: String::new(),
                },
                "slide_id_invalid",
            ),
            (
                GuideError::BlockIdInvalid {
                    file: String::new(),
                    field: String::new(),
                    message: String::new(),
                },
                "block_id_invalid",
            ),
            (
                GuideError::SlideIdMismatch {
                    file: String::new(),
                    field: String::new(),
                    message: String::new(),
                },
                "slide_id_mismatch",
            ),
            (
                GuideError::FrontMatterMissing {
                    file: String::new(),
                    field: String::new(),
                    message: String::new(),
                },
                "front_matter_missing",
            ),
            (
                GuideError::FrontMatterUnclosed {
                    file: String::new(),
                    field: String::new(),
                    message: String::new(),
                },
                "front_matter_unclosed",
            ),
            (
                GuideError::HtmlNotAllowed {
                    file: String::new(),
                    field: String::new(),
                    message: String::new(),
                },
                "html_not_allowed",
            ),
            (
                GuideError::DuplicateDiagramBlock {
                    file: String::new(),
                    field: String::new(),
                    message: String::new(),
                },
                "duplicate_diagram_block",
            ),
            (
                GuideError::DiagramBlockNested {
                    file: String::new(),
                    field: String::new(),
                    message: String::new(),
                },
                "diagram_block_nested",
            ),
            (
                GuideError::PathInvalid {
                    file: String::new(),
                    field: String::new(),
                    message: String::new(),
                },
                "path_invalid",
            ),
            (
                GuideError::SymlinkNotAllowed {
                    file: String::new(),
                    field: String::new(),
                    message: String::new(),
                },
                "symlink_not_allowed",
            ),
            (
                GuideError::UnexpectedFile {
                    file: String::new(),
                    field: String::new(),
                    message: String::new(),
                },
                "unexpected_file",
            ),
            (
                GuideError::SlideFileNameInvalid {
                    file: String::new(),
                    field: String::new(),
                    message: String::new(),
                },
                "slide_file_name_invalid",
            ),
            (
                GuideError::ManifestMissing {
                    file: String::new(),
                    field: String::new(),
                    message: String::new(),
                },
                "manifest_missing",
            ),
            (
                GuideError::DraftInvalid {
                    file: String::new(),
                    field: String::new(),
                    message: String::new(),
                },
                "draft_invalid",
            ),
            (
                GuideError::FileUnreadable {
                    file: String::new(),
                    field: String::new(),
                    message: String::new(),
                },
                "file_unreadable",
            ),
            (
                GuideError::FileNotUtf8 {
                    file: String::new(),
                    field: String::new(),
                    message: String::new(),
                },
                "file_not_utf8",
            ),
        ];
        assert_eq!(cases.len(), 23);
        for (error, code) in &cases {
            assert_eq!(error.code(), *code);
        }
        let located = GuideError::HtmlNotAllowed {
            file: "slides/s1.md".into(),
            field: "body".into(),
            message: "m".into(),
        };
        assert_eq!(located.file(), Some("slides/s1.md"));
        assert_eq!(located.field(), Some("body"));
        assert_eq!(located.to_string(), "m");
        let old = GuideError::RangeNotInPr(String::new());
        assert_eq!(old.file(), None);
        assert_eq!(old.field(), None);
    }
}
