//! Code blocks of a guide slide, and their resolution against a diff or revision.

use serde::{Deserialize, Serialize};

use crate::{
    Error,
    git::{
        Git,
        diff::{FileDiff, FileStatus, GitPath, Hunk, LineKind},
    },
    guide::range::{ResolvedSelection, Snapshot},
};

use super::{AnchorError, inline::span};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Side {
    Old,
    New,
}

impl Side {
    /// The value GitHub's review-comment API uses for this side.
    pub fn github(self) -> &'static str {
        match self {
            Self::Old => "LEFT",
            Self::New => "RIGHT",
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Old => "old",
            Self::New => "new",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum At {
    #[default]
    SnapshotHead,
    SelectionHead,
    SelectionBase,
}

impl At {
    fn name(self) -> &'static str {
        match self {
            Self::SnapshotHead => "snapshot-head",
            Self::SelectionHead => "selection-head",
            Self::SelectionBase => "selection-base",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase", deny_unknown_fields)]
pub enum CodeBlock {
    Diff {
        id: String,
        path: String,
        side: Side,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        start: Option<u32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        end: Option<u32>,
    },
    Context {
        id: String,
        path: String,
        #[serde(default)]
        at: At,
        start: u32,
        end: u32,
    },
}

impl CodeBlock {
    pub fn id(&self) -> &str {
        match self {
            Self::Diff { id, .. } | Self::Context { id, .. } => id,
        }
    }

    pub fn path(&self) -> &str {
        match self {
            Self::Diff { path, .. } | Self::Context { path, .. } => path,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BlockKind {
    Diff,
    Context,
}

/// What a guide version's blocks resolve against.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockContext {
    pub version: u32,
    pub snapshot: Snapshot,
    pub selection: ResolvedSelection,
    pub diff: Vec<FileDiff>,
}

impl BlockContext {
    pub fn new(
        git: &Git,
        version: u32,
        snapshot: Snapshot,
        selection: ResolvedSelection,
    ) -> Result<Self, Error> {
        let diff = selection.files(git)?;
        Ok(Self {
            version,
            snapshot,
            selection,
            diff,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stub {
    Binary,
    Submodule,
    ModeOnly,
    NoText,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Warning {
    pub code: &'static str,
    pub field: &'static str,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedBlock {
    pub version: u32,
    pub id: String,
    pub kind: BlockKind,
    pub path: String,
    pub old_path: Option<GitPath>,
    pub side: Option<Side>,
    pub at: Option<At>,
    pub rev: String,
    pub start: Option<u32>,
    pub end: Option<u32>,
    pub stub: Option<Stub>,
    pub file: Option<FileDiff>,
    pub hunks: Vec<Hunk>,
    pub lines: Vec<Vec<u8>>,
    pub warnings: Vec<Warning>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredAnchor {
    pub version: u32,
    pub slide: String,
    pub block: String,
    pub kind: BlockKind,
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub old_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub side: Option<Side>,
    pub rev: String,
    pub start: u32,
    pub end: u32,
    pub lines: Vec<String>,
}

impl StoredAnchor {
    /// The file's path at `rev`.
    pub fn rev_path(&self) -> &str {
        match (&self.side, &self.old_path) {
            (Some(Side::Old), Some(old)) => old,
            _ => &self.path,
        }
    }
}

fn sha7(rev: &str) -> &str {
    rev.get(..7).unwrap_or(rev)
}

/// Split a blob into lines: `\n` ends a line, `\r` stays, a last line needs no `\n`.
fn split_lines(blob: &[u8]) -> Vec<Vec<u8>> {
    let mut lines: Vec<Vec<u8>> = blob.split(|b| *b == b'\n').map(<[u8]>::to_vec).collect();
    if blob.is_empty() || blob.ends_with(b"\n") {
        lines.pop();
    }
    lines
}

fn check_bounds(
    start: u32,
    end: u32,
    count: usize,
    path: &str,
    place: &str,
    rev: &str,
) -> Result<(), AnchorError> {
    if start == 0 {
        return Err(AnchorError::LinesOutOfRange {
            field: "start",
            message: "start must be at least 1".into(),
        });
    }
    if start > end {
        return Err(AnchorError::LinesOutOfRange {
            field: "end",
            message: format!("start {start} is after end {end}"),
        });
    }
    if usize::try_from(end).map_or(true, |end| end > count) {
        return Err(AnchorError::LinesOutOfRange {
            field: "end",
            message: format!(
                "lines {start}-{end} are outside {path} at {place} ({}), which has {count} lines",
                sha7(rev)
            ),
        });
    }
    Ok(())
}

fn read_blob(git: &Git, rev: &str, path: &str, place: &str) -> Result<Vec<u8>, Error> {
    git.show(rev, path)?.ok_or_else(|| {
        AnchorError::PathNotFound(format!("{path} is not a file at {place} ({})", sha7(rev))).into()
    })
}

/// Resolve one code block of a guide version.
pub fn resolve(git: &Git, block: &CodeBlock, cx: &BlockContext) -> Result<ResolvedBlock, Error> {
    match block {
        CodeBlock::Diff {
            id,
            path,
            side,
            start,
            end,
        } => resolve_diff(git, cx, id, path, *side, *start, *end),
        CodeBlock::Context {
            id,
            path,
            at,
            start,
            end,
        } => resolve_context(git, cx, id, path, *at, *start, *end),
    }
}

fn has_side(file: &FileDiff, side: Side) -> bool {
    match side {
        Side::New => file.status != FileStatus::Deleted,
        Side::Old => file.status != FileStatus::Added,
    }
}

fn find_entry<'a>(
    cx: &'a BlockContext,
    path: &str,
    side: Side,
) -> Result<&'a FileDiff, AnchorError> {
    let candidates: Vec<&FileDiff> = cx
        .diff
        .iter()
        .filter(|file| file.path.as_bytes() == path.as_bytes())
        .collect();
    if let Some(found) = candidates.iter().find(|file| has_side(file, side)) {
        return Ok(found);
    }
    if !candidates.is_empty() {
        let (was, other) = match side {
            Side::New => ("deleted", "old"),
            Side::Old => ("added", "new"),
        };
        return Err(AnchorError::WrongSide(format!(
            "{path} has no {} side in this guide's diff: it was {was}; use side: {other}",
            side.name()
        )));
    }
    if let Some(renamed) = cx.diff.iter().find(|file| {
        file.status == FileStatus::Renamed
            && file.old_path.as_ref().map(GitPath::as_bytes) == Some(path.as_bytes())
    }) {
        return Err(AnchorError::PathNotInDiff(format!(
            "{path} is not in this guide's diff: it was renamed to {}; cite the new path",
            renamed.path
        )));
    }
    let lossy_match = |name: &GitPath| name.to_str().is_none() && name.to_string() == path;
    for file in &cx.diff {
        let hit = [Some(&file.path), file.old_path.as_ref()]
            .into_iter()
            .flatten()
            .find(|name| lossy_match(name));
        if let Some(name) = hit {
            return Err(AnchorError::PathNotUtf8(format!(
                "{path} names a file whose path is not valid UTF-8 ({name:?}); a guide can't cite it, so describe it in the slide text"
            )));
        }
    }
    Err(AnchorError::PathNotInDiff(format!(
        "{path} is not in this guide's diff"
    )))
}

fn resolve_diff(
    git: &Git,
    cx: &BlockContext,
    id: &str,
    path: &str,
    side: Side,
    start: Option<u32>,
    end: Option<u32>,
) -> Result<ResolvedBlock, Error> {
    let entry = find_entry(cx, path, side)?;
    let rev = match side {
        Side::Old => &cx.selection.base,
        Side::New => &cx.selection.head,
    };
    let blob_path = match (side, entry.status, &entry.old_path) {
        (Side::Old, FileStatus::Renamed, Some(old)) => {
            let Some(old) = old.to_str() else {
                return Err(AnchorError::PathNotUtf8(format!(
                    "{path} was renamed from a path that is not valid UTF-8 ({old:?}); cite it with side: new"
                ))
                .into());
            };
            old
        }
        _ => path,
    };
    let mut resolved = ResolvedBlock {
        version: cx.version,
        id: id.into(),
        kind: BlockKind::Diff,
        path: path.into(),
        old_path: entry.old_path.clone(),
        side: Some(side),
        at: None,
        rev: rev.clone(),
        start: None,
        end: None,
        stub: None,
        file: None,
        hunks: vec![],
        lines: vec![],
        warnings: vec![],
    };
    if entry.hunks.is_empty() {
        let (what, stub) = if entry.submodule {
            ("a submodule", Stub::Submodule)
        } else if entry.binary {
            ("a binary file", Stub::Binary)
        } else if entry.mode_only {
            ("a mode-only change", Stub::ModeOnly)
        } else {
            ("a change without text lines", Stub::NoText)
        };
        if start.is_some() || end.is_some() {
            return Err(AnchorError::RangeNotAllowed(format!(
                "{path} is {what}; cite it without start and end"
            ))
            .into());
        }
        resolved.stub = Some(stub);
        return Ok(resolved);
    }
    let (Some(start), Some(end)) = (start, end) else {
        return Err(AnchorError::RangeRequired(format!(
            "{path} has text changes; a diff block needs both start and end"
        ))
        .into());
    };
    let place = format!("side {}", side.name());
    let blob = read_blob(git, rev, blob_path, &place)?;
    check_bounds(start, end, split_lines(&blob).len(), path, &place, rev)?;
    let hunks: Vec<Hunk> = entry
        .hunks
        .iter()
        .filter(|hunk| span(hunk, side).is_some_and(|(first, last)| first <= end && start <= last))
        .cloned()
        .collect();
    if hunks.is_empty() {
        return Err(AnchorError::NoHunksInRange(format!(
            "no change in {path} overlaps lines {start}-{end} on side {}; use a context block for unchanged code",
            side.name()
        ))
        .into());
    }
    resolved.start = Some(start);
    resolved.end = Some(end);
    resolved.file = Some(entry.clone());
    resolved.hunks = hunks;
    Ok(resolved)
}

fn resolve_context(
    git: &Git,
    cx: &BlockContext,
    id: &str,
    path: &str,
    at: At,
    start: u32,
    end: u32,
) -> Result<ResolvedBlock, Error> {
    let rev = match at {
        At::SnapshotHead => &cx.snapshot.head,
        At::SelectionHead => &cx.selection.head,
        At::SelectionBase => &cx.selection.base,
    };
    let place = at.name();
    let blob = read_blob(git, rev, path, place)?;
    if blob.iter().take(8000).any(|byte| *byte == 0) {
        return Err(AnchorError::RangeNotAllowed(format!(
            "{path} is a binary file at {place} ({}); a context block needs a text file",
            sha7(rev)
        ))
        .into());
    }
    let all = split_lines(&blob);
    check_bounds(start, end, all.len(), path, place, rev)?;
    let lines = all[start as usize - 1..end as usize].to_vec();
    let mut warnings = vec![];
    if changed_in_range(cx, rev, path, start, end) {
        warnings.push(Warning {
            code: "context_overlaps_hunk",
            field: "start",
            message: format!(
                "lines {start}-{end} of {path} include lines changed in this guide's diff; use a diff block"
            ),
        });
    }
    Ok(ResolvedBlock {
        version: cx.version,
        id: id.into(),
        kind: BlockKind::Context,
        path: path.into(),
        old_path: None,
        side: None,
        at: Some(at),
        rev: rev.clone(),
        start: Some(start),
        end: Some(end),
        stub: None,
        file: None,
        hunks: vec![],
        lines,
        warnings,
    })
}

/// Whether `start..=end` of `path` at `rev` holds a line this guide's diff changed.
fn changed_in_range(cx: &BlockContext, rev: &str, path: &str, start: u32, end: u32) -> bool {
    let path = path.as_bytes();
    let hits = |file: &FileDiff, kind: LineKind| {
        let mut found = false;
        for hunk in &file.hunks {
            let mut number = if kind == LineKind::Added {
                hunk.new_start
            } else {
                hunk.old_start
            };
            for line in &hunk.lines {
                let counts_here = match line.kind {
                    LineKind::Context => true,
                    other => other == kind,
                };
                if !counts_here {
                    continue;
                }
                if line.kind == kind && start <= number && number <= end {
                    found = true;
                }
                number += 1;
            }
        }
        found
    };
    if rev == cx.selection.head
        && cx.diff.iter().any(|f| {
            f.status != FileStatus::Deleted && f.path.as_bytes() == path && hits(f, LineKind::Added)
        })
    {
        return true;
    }
    rev == cx.selection.base
        && cx.diff.iter().any(|f| {
            let on_old_side = match f.status {
                FileStatus::Modified | FileStatus::Deleted => f.path.as_bytes() == path,
                FileStatus::Renamed => f.old_path.as_ref().map(GitPath::as_bytes) == Some(path),
                FileStatus::Added => false,
            };
            on_old_side && hits(f, LineKind::Removed)
        })
}

impl ResolvedBlock {
    /// A thread anchor on lines `start..=end` of this block.
    pub fn anchor(&self, slide: &str, start: u32, end: u32) -> Result<StoredAnchor, AnchorError> {
        let (Some(first), Some(last)) = (self.start, self.end) else {
            return Err(AnchorError::RangeNotAllowed(format!(
                "block {} has no lines to anchor",
                self.id
            )));
        };
        if start == 0 {
            return Err(AnchorError::LinesOutOfRange {
                field: "start",
                message: "start must be at least 1".into(),
            });
        }
        if start > end {
            return Err(AnchorError::LinesOutOfRange {
                field: "end",
                message: format!("start {start} is after end {end}"),
            });
        }
        let not_shown = || AnchorError::LinesOutOfRange {
            field: "start",
            message: format!("lines {start}-{end} are not all shown in block {}", self.id),
        };
        let mut texts: Vec<Vec<u8>> = Vec::new();
        match (self.kind, self.side) {
            (BlockKind::Diff, Some(side)) => {
                for number in start..=end {
                    texts.push(self.hunk_line(side, number).ok_or_else(not_shown)?);
                }
            }
            _ => {
                if start < first || end > last {
                    return Err(not_shown());
                }
                texts.extend_from_slice(
                    &self.lines[(start - first) as usize..=(end - first) as usize],
                );
            }
        }
        Ok(StoredAnchor {
            version: self.version,
            slide: slide.into(),
            block: self.id.clone(),
            kind: self.kind,
            path: self.path.clone(),
            old_path: self.old_path.as_ref().map(GitPath::to_string),
            side: self.side,
            rev: self.rev.clone(),
            start,
            end,
            lines: texts
                .iter()
                .map(|text| String::from_utf8_lossy(text).into_owned())
                .collect(),
        })
    }

    /// The text of line `number` on `side`, if one of this block's hunks shows it.
    fn hunk_line(&self, side: Side, number: u32) -> Option<Vec<u8>> {
        for hunk in &self.hunks {
            let (first, last) = span(hunk, side)?;
            if number < first || number > last {
                continue;
            }
            let wanted = match side {
                Side::New => LineKind::Added,
                Side::Old => LineKind::Removed,
            };
            let mut current = first;
            for line in &hunk.lines {
                if line.kind == LineKind::Context || line.kind == wanted {
                    if current == number {
                        return Some(line.text.clone());
                    }
                    current += 1;
                }
            }
        }
        None
    }
}
