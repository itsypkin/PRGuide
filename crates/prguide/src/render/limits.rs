//! Hard size limits for diagrams, checked before and after rendering (§2.8).

use super::RenderError;

/// Largest accepted source, in bytes.
pub const MAX_SOURCE_BYTES: usize = 8 * 1024;
/// Largest accepted flowchart node count.
pub const MAX_NODES: usize = 40;
/// Largest accepted flowchart edge count.
pub const MAX_EDGES: usize = 80;
/// Largest accepted sequence participant count.
pub const MAX_PARTICIPANTS: usize = 12;
/// Deepest accepted subgraph nesting.
pub const MAX_NESTING: usize = 3;
/// Longest accepted label, in Unicode scalar values.
pub const MAX_LABEL_CHARS: usize = 60;
/// Largest accepted rendered output, in bytes.
pub const MAX_OUTPUT_BYTES: usize = 64 * 1024;

/// Which limit was exceeded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Limit {
    SourceBytes,
    Nodes,
    Edges,
    Participants,
    Nesting,
    LabelChars,
    OutputBytes,
}

impl Limit {
    /// Stable snake_case name used in messages.
    pub fn as_str(self) -> &'static str {
        match self {
            Limit::SourceBytes => "source_bytes",
            Limit::Nodes => "nodes",
            Limit::Edges => "edges",
            Limit::Participants => "participants",
            Limit::Nesting => "nesting",
            Limit::LabelChars => "label_chars",
            Limit::OutputBytes => "output_bytes",
        }
    }

    /// The maximum allowed value for this limit.
    pub fn max(self) -> usize {
        match self {
            Limit::SourceBytes => MAX_SOURCE_BYTES,
            Limit::Nodes => MAX_NODES,
            Limit::Edges => MAX_EDGES,
            Limit::Participants => MAX_PARTICIPANTS,
            Limit::Nesting => MAX_NESTING,
            Limit::LabelChars => MAX_LABEL_CHARS,
            Limit::OutputBytes => MAX_OUTPUT_BYTES,
        }
    }
}

/// Ok when `actual` <= the limit's maximum; else `RenderError::TooLarge`.
pub fn check(limit: Limit, actual: usize) -> Result<(), RenderError> {
    let max = limit.max();
    if actual <= max {
        Ok(())
    } else {
        Err(RenderError::TooLarge { limit, actual, max })
    }
}
