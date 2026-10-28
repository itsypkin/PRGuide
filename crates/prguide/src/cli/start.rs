//! `prguide start`: bind a store to this clone, put the worktree at the PR head
//! and start a run.

use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    sync::Arc,
};

use rusqlite::OptionalExtension;
use serde::Serialize;

use crate::{
    Error,
    cli::output::print_json,
    git::{
        Git, GitError, Refspec,
        diff::{FileDiff, FileStatus, LineKind},
        parse_github_remote,
        refs::{fetch_base_ref, fetch_head_ref},
    },
    github::{Commit, GitHub, PrState, Repo, gh::GhCli},
    guide::range::{Selection, Snapshot},
    store::{
        Clock, PrRef, Store, StoreError, SystemClock, prguide_home,
        runs::{self, RunId},
    },
};

#[derive(Debug, clap::Args)]
pub struct StartArgs {
    pub pr: u64,
    #[arg(long)]
    pub takeover: bool,
}

/// Everything `start` reads from its surroundings.
pub struct StartEnv<'a> {
    pub git: Git,
    pub github: &'a dyn GitHub,
    pub home: PathBuf,
    pub clock: Arc<dyn Clock>,
}

#[derive(Debug, Serialize)]
pub struct PrOut {
    pub repo: String,
    pub number: u64,
    pub url: String,
    pub state: PrState,
    pub title: String,
    pub body: String,
    pub author: String,
    pub base_ref: String,
    pub head_sha: String,
}

#[derive(Debug, Serialize)]
pub struct SnapshotOut {
    pub base: String,
    pub head: String,
}

#[derive(Debug, Serialize)]
pub struct FileOut {
    pub path: String,
    pub old_path: Option<String>,
    pub status: &'static str,
    pub additions: usize,
    pub deletions: usize,
    pub binary: bool,
    pub mode_only: bool,
    pub submodule: bool,
}

impl FileOut {
    pub fn from_diff(f: &FileDiff) -> FileOut {
        let count = |kind: LineKind| {
            f.hunks
                .iter()
                .flat_map(|h| &h.lines)
                .filter(|l| l.kind == kind)
                .count()
        };
        FileOut {
            path: f.path.to_string(),
            old_path: f.old_path.as_ref().map(ToString::to_string),
            status: match f.status {
                FileStatus::Added => "added",
                FileStatus::Modified => "modified",
                FileStatus::Deleted => "deleted",
                FileStatus::Renamed => "renamed",
            },
            additions: count(LineKind::Added),
            deletions: count(LineKind::Removed),
            binary: f.binary,
            mode_only: f.mode_only,
            submodule: f.submodule,
        }
    }
}

#[derive(Debug, Serialize)]
pub struct GuideVersionOut {
    pub version: i64,
    pub selection_kind: String,
    pub snapshot_base: String,
    pub snapshot_head: String,
    pub selection_base: String,
    pub selection_head: String,
    pub ready_at: Option<String>,
}

#[derive(Debug, PartialEq, Eq, Serialize)]
pub struct WarningOut {
    pub code: &'static str,
    pub paths: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct StartOutput {
    pub run_id: RunId,
    pub store: PathBuf,
    pub worktree: PathBuf,
    pub pr: PrOut,
    pub snapshot: SnapshotOut,
    pub commits: Vec<Commit>,
    pub files: Vec<FileOut>,
    pub guide_versions: Vec<GuideVersionOut>,
    /// Absent when empty, so output without a warning is unchanged.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<WarningOut>,
}

/// Runs `prguide start` against the real environment.
pub fn run(args: StartArgs) -> Result<(), Error> {
    let cwd = std::env::current_dir()
        .map_err(|e| GitError::NotInClone(format!("cannot read the current directory: {e}")))?;
    let gh = GhCli::new();
    let env = StartEnv {
        git: Git::new(cwd),
        github: &gh,
        home: prguide_home()?,
        clock: Arc::new(SystemClock),
    };
    print_json(&start(&env, &args)?)
}

/// The start sequence; each step's error passes through unchanged.
pub fn start(env: &StartEnv<'_>, args: &StartArgs) -> Result<StartOutput, Error> {
    let n = args.pr;
    let clone = env.git.discover()?;
    let git = env.git.with_dir(&clone.toplevel);
    let (owner, repo) = pr_repo(&git)?;
    let gh_repo = Repo::github(&owner, &repo)?;
    let remote = git.remote_for(&owner, &repo)?;
    let pr = env.github.pr(&gh_repo, n)?;
    let commits = env.github.commits(&gh_repo, n)?;
    let mut store = Store::open_in(&env.home, &PrRef::new(&owner, &repo, n)?)?
        .with_clock(Arc::clone(&env.clock));
    let store_dir = fs::canonicalize(store.dir()).map_err(|source| StoreError::Io {
        path: store.dir().to_path_buf(),
        source,
    })?;
    bind_store_meta(&mut store, &owner, &repo, n, &clone.common_dir)?;
    if !args.takeover {
        let clock = store.clock();
        let live = store.tx(|tx| runs::active_run(tx, clock.now()))?;
        if let Some(run) = live {
            return Err(StoreError::RunActive {
                run_id: run.id.to_string(),
            }
            .into());
        }
    }
    git.fetch(&remote.name, &Refspec::for_pr(n, &pr.base_ref))?;
    let head = git.resolve_commit(&fetch_head_ref(n))?;
    let base_tip = git.resolve_commit(&fetch_base_ref(n))?;
    if head != pr.head_sha {
        return Err(GitError::PrHeadMoved {
            number: n,
            expected: pr.head_sha,
            fetched: head,
        }
        .into());
    }
    let worktree = store_dir.join("worktree");
    git.prepare_worktree(&clone, &worktree, &head)?;
    let warnings: Vec<WarningOut> = git
        .path_collisions(&worktree, &head)?
        .into_iter()
        .map(|paths| WarningOut {
            code: "path_collision",
            paths,
        })
        .collect();
    let snapshot = Snapshot::new(&git, &head, &base_tip)?;
    let shas: Vec<String> = commits.iter().map(|c| c.sha.clone()).collect();
    let resolved = Selection::Full.resolve(&git, &snapshot, &shas)?;
    let files = resolved
        .files(&git)?
        .into_iter()
        .map(|f| FileOut::from_diff(&f))
        .collect();
    let guide_versions = ready_guide_versions(&mut store)?;
    let run = runs::start_run(&mut store, args.takeover)?;
    Ok(StartOutput {
        run_id: run.id,
        store: store_dir,
        worktree,
        pr: PrOut {
            repo: format!("{owner}/{repo}"),
            number: pr.number,
            url: pr.url,
            state: pr.state,
            title: pr.title,
            body: pr.body,
            author: pr.author,
            base_ref: pr.base_ref,
            head_sha: head.clone(),
        },
        snapshot: SnapshotOut {
            base: snapshot.base,
            head: snapshot.head,
        },
        commits,
        files,
        guide_versions,
        warnings,
    })
}

/// Which GitHub repository the PR number belongs to: gh's default, else the
/// only repo the remotes name, else gh's remote priority.
pub fn pr_repo(git: &Git) -> Result<(String, String), Error> {
    let optional = |args: &[&str]| -> Result<String, GitError> {
        match git.run(args) {
            Ok(out) => Ok(String::from_utf8_lossy(&out).into_owned()),
            Err(GitError::Failed {
                exit_code: Some(1),
                stderr,
                ..
            }) if stderr.is_empty() => Ok(String::new()),
            Err(e) => Err(e),
        }
    };
    let keyed = |text: &str, suffix: &str| -> Vec<(String, String)> {
        text.lines()
            .filter_map(|line| {
                let (key, value) = line.split_once(' ')?;
                let name = key.strip_prefix("remote.")?.strip_suffix(suffix)?;
                Some((name.to_owned(), value.to_owned()))
            })
            .collect()
    };
    let urls = keyed(
        &optional(&["config", "--get-regexp", "^remote\\..*\\.url$"])?,
        ".url",
    );
    let resolved = keyed(
        &optional(&["config", "--get-regexp", "^remote\\..*\\.gh-resolved$"])?,
        ".gh-resolved",
    );
    let verbose = String::from_utf8_lossy(&git.run(["remote", "-v"])?).into_owned();
    let mut fetches: BTreeMap<String, String> = BTreeMap::new();
    for line in verbose.lines() {
        if let Some(line) = line.strip_suffix(" (fetch)")
            && let Some((name, url)) = line.split_once('\t')
        {
            fetches.insert(name.to_owned(), url.to_owned());
        }
    }
    let mut names: Vec<&String> = urls.iter().map(|(n, _)| n).chain(fetches.keys()).collect();
    names.sort();
    names.dedup();
    let mut candidates: Vec<(String, (String, String))> = Vec::new();
    for name in names {
        let found = urls
            .iter()
            .filter(|(n, _)| n == name)
            .find_map(|(_, url)| parse_github_remote(url))
            .or_else(|| fetches.get(name).and_then(|url| parse_github_remote(url)));
        if let Some(found) = found {
            candidates.push((name.clone(), found));
        }
    }
    if candidates.is_empty() {
        return Err(GitError::NotInClone(
            "no remote of this clone points at a github.com repository".into(),
        )
        .into());
    }
    let same = |a: &(String, String), b: &(String, String)| {
        a.0.eq_ignore_ascii_case(&b.0) && a.1.eq_ignore_ascii_case(&b.1)
    };
    let base: Vec<&(String, (String, String))> = candidates
        .iter()
        .filter(|(name, _)| resolved.iter().any(|(n, v)| n == name && v == "base"))
        .collect();
    if let Some(first) = base.first() {
        if base.iter().all(|c| same(&c.1, &first.1)) {
            return Ok(first.1.clone());
        }
    } else if candidates.iter().all(|c| same(&c.1, &candidates[0].1)) {
        return Ok(candidates[0].1.clone());
    } else {
        for wanted in ["upstream", "github", "origin"] {
            if let Some(c) = candidates.iter().find(|c| c.0 == wanted) {
                return Ok(c.1.clone());
            }
        }
    }
    let mut repos: Vec<String> = Vec::new();
    for (_, (o, r)) in &candidates {
        let text = format!("{o}/{r}");
        if !repos.iter().any(|x| x.eq_ignore_ascii_case(&text)) {
            repos.push(text);
        }
    }
    repos.sort();
    Err(GitError::NotInClone(format!(
        "remotes point at several GitHub repositories ({}); run gh repo set-default to choose one",
        repos.join(", ")
    ))
    .into())
}

/// Records which clone this store belongs to. Future home: `store` (a store_meta helper).
fn bind_store_meta(
    store: &mut Store,
    owner: &str,
    repo: &str,
    number: u64,
    common_dir: &Path,
) -> Result<(), Error> {
    let current = common_dir.to_string_lossy().into_owned();
    store.tx(|tx| {
        let bound: Option<String> = tx
            .query_row(
                "SELECT value FROM store_meta WHERE key = 'git_common_dir'",
                [],
                |row| row.get(0),
            )
            .optional()
            .map_err(StoreError::from)?;
        match bound {
            None => {
                for (key, value) in [
                    ("host", "github.com".to_owned()),
                    ("owner", owner.to_owned()),
                    ("repo", repo.to_owned()),
                    ("number", number.to_string()),
                    ("git_common_dir", current.clone()),
                ] {
                    tx.execute(
                        "INSERT INTO store_meta (key, value) VALUES (?1, ?2)",
                        rusqlite::params![key, value],
                    )
                    .map_err(StoreError::from)?;
                }
                Ok(())
            }
            Some(v) if v == current => Ok(()),
            Some(v) => Err(GitError::StoreBound {
                pr: format!("{owner}/{repo}#{number}"),
                bound: v,
                current: current.clone(),
            }
            .into()),
        }
    })
}

/// Ready guide versions, oldest first. Future home: T3-4's `store/versions.rs`.
fn ready_guide_versions(store: &mut Store) -> Result<Vec<GuideVersionOut>, Error> {
    store.tx(|tx| {
        let mut stmt = tx
            .prepare(
                "SELECT version, selection_kind, snapshot_base, snapshot_head, selection_base, \
                 selection_head, ready_at FROM guide_versions WHERE state = 'ready' \
                 ORDER BY version",
            )
            .map_err(StoreError::from)?;
        let rows = stmt
            .query_map([], |row| {
                Ok(GuideVersionOut {
                    version: row.get(0)?,
                    selection_kind: row.get(1)?,
                    snapshot_base: row.get(2)?,
                    snapshot_head: row.get(3)?,
                    selection_base: row.get(4)?,
                    selection_head: row.get(5)?,
                    ready_at: row.get(6)?,
                })
            })
            .map_err(StoreError::from)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row.map_err(StoreError::from)?);
        }
        Ok::<_, Error>(out)
    })
}
