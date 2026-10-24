//! Snapshot and commit-range selection for a guide.

use crate::{
    git::{
        DiffSpec, Git,
        diff::{FileDiff, parse},
    },
    guide::GuideError,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    pub base: String,
    pub head: String,
}

impl Snapshot {
    /// Build a pinned snapshot from fetched PR head and base-tip revisions.
    pub fn new(git: &Git, head: &str, base_tip: &str) -> Result<Self, crate::Error> {
        let head = git.resolve_commit(head)?;
        let base_tip = git.resolve_commit(base_tip)?;
        let base = git.merge_base(&head, &base_tip)?.ok_or_else(|| {
            GuideError::NoBaseCommit(format!("no merge base between {head} and {base_tip}"))
        })?;
        Ok(Self { base, head })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Selection {
    Full,
    Commits { first: String, last: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedSelection {
    pub selection: Selection,
    pub base: String,
    pub head: String,
    pub commits: Vec<String>,
}

impl Selection {
    /// Resolve a selection against full commit IDs in GitHub's PR order.
    pub fn resolve(
        &self,
        git: &Git,
        snapshot: &Snapshot,
        pr_commits: &[String],
    ) -> Result<ResolvedSelection, crate::Error> {
        match self {
            Self::Full => Ok(ResolvedSelection {
                selection: self.clone(),
                base: snapshot.base.clone(),
                head: snapshot.head.clone(),
                commits: pr_commits.to_vec(),
            }),
            Self::Commits { first, last } => {
                let first_index = pr_commits
                    .iter()
                    .position(|commit| commit == first)
                    .ok_or_else(|| {
                        GuideError::RangeNotInPr(format!("commit {first} is not in the PR"))
                    })?;
                let last_index = pr_commits
                    .iter()
                    .position(|commit| commit == last)
                    .ok_or_else(|| {
                        GuideError::RangeNotInPr(format!("commit {last} is not in the PR"))
                    })?;
                if first_index > last_index {
                    return Err(GuideError::RangeNotInPr(format!(
                        "range is reversed: {first} comes after {last} in the PR"
                    ))
                    .into());
                }
                let commits = pr_commits[first_index..=last_index].to_vec();
                let mut parents = Vec::with_capacity(commits.len());
                for commit in &commits {
                    let parent_list = git.rev_parents(commit)?;
                    if parent_list.len() > 1 {
                        return Err(GuideError::RangeHasMerge(format!(
                            "commit {commit} is a merge ({} parents); a range can't contain a merge",
                            parent_list.len()
                        )).into());
                    }
                    if parent_list.is_empty() {
                        return Err(GuideError::NoBaseCommit(format!(
                            "commit {commit} has no parent"
                        ))
                        .into());
                    }
                    parents.push(parent_list);
                }
                for (offset, parent_list) in parents.iter().enumerate().skip(1) {
                    if parent_list[0] != commits[offset - 1] {
                        return Err(GuideError::RangeNotInPr(format!(
                            "commits {} and {} are not consecutive",
                            commits[offset - 1],
                            commits[offset]
                        ))
                        .into());
                    }
                }
                Ok(ResolvedSelection {
                    selection: self.clone(),
                    base: parents[0][0].clone(),
                    head: commits
                        .last()
                        .cloned()
                        .ok_or_else(|| GuideError::RangeNotInPr("commit range is empty".into()))?,
                    commits,
                })
            }
        }
    }
}

impl ResolvedSelection {
    pub fn diff_spec(&self) -> DiffSpec {
        match self.selection {
            Selection::Full => DiffSpec::ThreeDot {
                base_tip: self.base.clone(),
                head: self.head.clone(),
                paths: vec![],
            },
            Selection::Commits { .. } => DiffSpec::TwoDot {
                from: self.base.clone(),
                to: self.head.clone(),
                paths: vec![],
            },
        }
    }

    pub fn files(&self, git: &Git) -> Result<Vec<FileDiff>, crate::Error> {
        Ok(parse(&git.diff(&self.diff_spec())?)?)
    }
}
