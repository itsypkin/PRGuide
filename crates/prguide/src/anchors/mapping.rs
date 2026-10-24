//! Mapping an anchor's lines to a later revision by the endpoint diff.

use crate::git::{
    DiffSpec, Git,
    diff::{FileDiff, FileStatus, GitPath, LineKind, parse},
};

use super::StoredAnchor;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MappedRange {
    pub path: GitPath,
    pub start: u32,
    pub end: u32,
    /// Full SHA of the revision the lines now refer to.
    pub rev: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mapping {
    Mapped(MappedRange),
    Changed,
    Deleted,
}

impl Mapping {
    pub fn range(&self) -> Option<&MappedRange> {
        match self {
            Self::Mapped(range) => Some(range),
            _ => None,
        }
    }
}

fn has_old_side(file: &FileDiff, path: &[u8]) -> bool {
    match file.status {
        FileStatus::Modified | FileStatus::Deleted => file.path.as_bytes() == path,
        FileStatus::Renamed => file.old_path.as_ref().map(GitPath::as_bytes) == Some(path),
        FileStatus::Added => false,
    }
}

/// Where old line `x` lands when it lies outside every hunk.
fn shifted(hunks: &[crate::git::diff::Hunk], x: u32) -> Option<u32> {
    let shift: i64 = hunks
        .iter()
        .filter(|hunk| {
            let first = if hunk.old_lines == 0 {
                hunk.old_start + 1
            } else {
                hunk.old_start
            };
            first + hunk.old_lines <= x
        })
        .map(|hunk| i64::from(hunk.new_lines) - i64::from(hunk.old_lines))
        .sum();
    u32::try_from(i64::from(x) + shift).ok()
}

/// Map the anchor's lines from `anchor.rev` to `to_rev`.
pub fn map_lines(git: &Git, anchor: &StoredAnchor, to_rev: &str) -> Result<Mapping, crate::Error> {
    let to = git.resolve_commit(to_rev)?;
    let path = anchor.rev_path();
    let files = parse(&git.diff(&DiffSpec::TwoDot {
        from: anchor.rev.clone(),
        to: to.clone(),
        paths: vec![],
    })?)?;
    let (start, end) = (anchor.start, anchor.end);
    let Some(entry) = files
        .iter()
        .find(|file| has_old_side(file, path.as_bytes()))
    else {
        return Ok(Mapping::Mapped(MappedRange {
            path: GitPath::new(path.as_bytes().to_vec()),
            start,
            end,
            rev: to,
        }));
    };
    if entry.status == FileStatus::Deleted {
        return Ok(Mapping::Deleted);
    }
    if entry.binary || entry.submodule {
        return Ok(Mapping::Changed);
    }
    let mut new_start = None;
    let mut new_end = None;
    for hunk in &entry.hunks {
        let mut o = if hunk.old_lines == 0 {
            hunk.old_start + 1
        } else {
            hunk.old_start
        };
        let mut n = if hunk.new_lines == 0 {
            hunk.new_start + 1
        } else {
            hunk.new_start
        };
        for line in &hunk.lines {
            match line.kind {
                LineKind::Context => {
                    if o == start {
                        new_start = Some(n);
                    }
                    if o == end {
                        new_end = Some(n);
                    }
                    o += 1;
                    n += 1;
                }
                LineKind::Removed => {
                    if start <= o && o <= end {
                        return Ok(Mapping::Changed);
                    }
                    o += 1;
                }
                LineKind::Added => {
                    if start < o && o <= end {
                        return Ok(Mapping::Changed);
                    }
                    n += 1;
                }
            }
        }
    }
    let mapped = |known: Option<u32>, x: u32| known.or_else(|| shifted(&entry.hunks, x));
    let (Some(new_start), Some(new_end)) = (mapped(new_start, start), mapped(new_end, end)) else {
        return Ok(Mapping::Changed);
    };
    Ok(Mapping::Mapped(MappedRange {
        path: entry.path.clone(),
        start: new_start,
        end: new_end,
        rev: to,
    }))
}
