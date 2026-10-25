//! The guide's data model: the manifest (`guide.yaml`), a slide's front matter
//! and the parsed slide. `parse.rs` builds these from text.

use serde::{Deserialize, Serialize};

use super::GuideError;
use crate::anchors::CodeBlock;

pub const MANIFEST_FILE: &str = "guide.yaml";
pub const SLIDES_DIR: &str = "slides";
pub const FORMAT: &str = "prguide/v1";

/// `[a-z0-9][a-z0-9-]{0,63}`, the whole string, ASCII only.
pub fn is_valid_id(s: &str) -> bool {
    let bytes = s.as_bytes();
    if bytes.is_empty() || bytes.len() > 64 {
        return false;
    }
    let first = bytes[0];
    if !(first.is_ascii_lowercase() || first.is_ascii_digit()) {
        return false;
    }
    bytes
        .iter()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'-')
}

/// A valid slide id. Serialises as a plain string.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct SlideId(String);

impl SlideId {
    pub fn parse(s: &str) -> Option<SlideId> {
        is_valid_id(s).then(|| SlideId(s.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The slide's file, relative to the guide directory: `slides/<id>.md`.
    pub fn file(&self) -> String {
        format!("{SLIDES_DIR}/{}.md", self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct GuideManifest {
    pub format: String,
    pub pr: ManifestPr,
    pub snapshot: ManifestSnapshot,
    pub selection: ManifestSelection,
    pub slides: Vec<SlideId>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ManifestPr {
    pub host: String,
    pub repo: String,
    pub number: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ManifestSnapshot {
    pub base: String,
    pub head: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ManifestSelection {
    pub kind: SelectionKind,
    pub base: String,
    pub head: String,
    pub commits: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SelectionKind {
    Full,
    Commits,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SlideKind {
    Overview,
    Architecture,
    Step,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FrontMatter {
    pub id: SlideId,
    pub kind: SlideKind,
    pub title: String,
    pub code: Vec<CodeBlock>,
    pub flags: Vec<Flag>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Flag {
    pub at: FlagAt,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FlagAt {
    pub block: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<u32>,
}

/// A diagram block pulled out of an architecture slide's body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DiagramBlock {
    /// The fence's content (its text events), as given.
    pub source: String,
    /// 1-based line of the opening fence in the slide file.
    pub line: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Slide {
    /// `slides/<id>.md`
    pub file: String,
    pub front: FrontMatter,
    /// CommonMark source after the front matter, with the pulled-out diagram
    /// blocks cut out (their byte ranges removed).
    pub body: String,
    /// 1-based file line where `body` starts.
    pub body_line: u32,
    /// Architecture slides only.
    pub mermaid: Option<DiagramBlock>,
    /// Architecture slides only.
    pub text_art: Option<DiagramBlock>,
}

/// What `load_draft` found. At most one error per file; `errors` is sorted by
/// `file` (bytes).
#[derive(Debug)]
pub struct DraftLoad {
    pub manifest: Option<GuideManifest>,
    /// Every validly named regular file in `slides/`, sorted.
    pub slide_files: Vec<SlideId>,
    /// The ones that parsed, sorted by id.
    pub slides: Vec<Slide>,
    pub errors: Vec<GuideError>,
}

/// The pulldown-cmark options every reader of a slide body uses: CommonMark only.
pub fn markdown_options() -> pulldown_cmark::Options {
    pulldown_cmark::Options::empty()
}

/// A code-block field path: `block_field(1, "end") == "code[1].end"`.
pub fn block_field(index: usize, field: &str) -> String {
    format!("code[{index}].{field}")
}
