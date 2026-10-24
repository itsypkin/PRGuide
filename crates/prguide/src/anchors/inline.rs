//! Turning a mapped range into a GitHub inline-comment location.

use crate::git::diff::{FileDiff, FileStatus, Hunk};

use super::{MappedRange, Side};

/// Where a review comment goes on the PR's diff.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InlineLoc {
    pub path: String,
    pub line: u32,
    pub side: Side,
    pub start_line: Option<u32>,
    pub start_side: Option<Side>,
}

/// The line span a hunk covers on one side; a side with 0 lines has none.
pub(super) fn span(hunk: &Hunk, side: Side) -> Option<(u32, u32)> {
    let (start, lines) = match side {
        Side::Old => (hunk.old_start, hunk.old_lines),
        Side::New => (hunk.new_start, hunk.new_lines),
    };
    (lines > 0).then(|| (start, start + lines - 1))
}

/// Place `mapped` on the PR diff, or `None` when GitHub can't take it there.
pub fn inline_location(
    mapped: &MappedRange,
    side: Side,
    pr_diff: &[FileDiff],
) -> Option<InlineLoc> {
    let path = mapped.path.as_bytes();
    let entry = pr_diff.iter().find(|file| match side {
        Side::New => file.path.as_bytes() == path && file.status != FileStatus::Deleted,
        Side::Old => match file.status {
            FileStatus::Modified | FileStatus::Deleted => file.path.as_bytes() == path,
            FileStatus::Renamed => file.old_path.as_ref().map(|old| old.as_bytes()) == Some(path),
            FileStatus::Added => false,
        },
    })?;
    let name = entry.path.to_str()?;
    if mapped.start == 0 || mapped.start > mapped.end {
        return None;
    }
    entry.hunks.iter().find(|hunk| {
        span(hunk, side).is_some_and(|(first, last)| first <= mapped.start && mapped.end <= last)
    })?;
    let range = mapped.start < mapped.end;
    Some(InlineLoc {
        path: name.to_owned(),
        line: mapped.end,
        side,
        start_line: range.then_some(mapped.start),
        start_side: range.then_some(side),
    })
}
