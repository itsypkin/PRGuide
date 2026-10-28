//! `prguide guide new`: re-read and fetch the PR, pin a snapshot and a
//! selection, move the worktree and create an empty draft.

use std::{
    fs, io,
    path::{Path, PathBuf},
    sync::Arc,
};

use rusqlite::OptionalExtension;
use serde::Serialize;

use crate::{
    Error,
    cli::{
        output::print_json,
        start::{FileOut, SnapshotOut},
    },
    git::{
        Git, Refspec,
        refs::{fetch_base_ref, fetch_head_ref},
    },
    github::{Commit, GitHub, Repo, gh::GhCli},
    guide::{
        GuideError,
        model::{
            FORMAT, MANIFEST_FILE, ManifestPr, ManifestSelection, ManifestSnapshot, SLIDES_DIR,
            SelectionKind,
        },
        new::{Provenance, expand_commit, parse_commits_arg, render_manifest},
        range::{Selection, Snapshot},
    },
    store::{
        Clock, Store, StoreError, SystemClock,
        drafts::{self, NewDraft},
        prguide_home,
        runs::{self, RunGate, RunId},
    },
};

#[derive(Debug, clap::Args)]
pub struct GuideNewArgs {
    #[arg(long)]
    pub run: String,
    #[arg(long)]
    pub commits: Option<String>,
}

/// Everything `guide new` reads from its surroundings.
pub struct GuideNewEnv<'a> {
    pub git: Git,
    pub github: &'a dyn GitHub,
    pub home: PathBuf,
    pub clock: Arc<dyn Clock>,
}

#[derive(Debug, Serialize)]
pub struct SelectionOut {
    pub kind: SelectionKind,
    pub base: String,
    pub head: String,
}

#[derive(Debug, Serialize)]
pub struct GuideNewOutput {
    pub draft_id: String,
    pub draft: PathBuf,
    pub worktree: PathBuf,
    pub snapshot: SnapshotOut,
    pub selection: SelectionOut,
    pub commits: Vec<Commit>,
    pub files: Vec<FileOut>,
}

/// Runs `prguide guide new` against the real environment.
pub fn run(args: GuideNewArgs) -> Result<(), Error> {
    let cwd = std::env::current_dir().map_err(|e| {
        crate::git::GitError::NotInClone(format!("cannot read the current directory: {e}"))
    })?;
    let gh = GhCli::new();
    let env = GuideNewEnv {
        git: Git::new(cwd),
        github: &gh,
        home: prguide_home()?,
        clock: Arc::new(SystemClock),
    };
    print_json(&guide_new(&env, &args)?)
}

/// Future home: store (a store_meta helper).
fn meta(tx: &rusqlite::Transaction<'_>, key: &str) -> Result<String, Error> {
    let value: Option<String> = tx
        .query_row("SELECT value FROM store_meta WHERE key = ?1", [key], |r| {
            r.get(0)
        })
        .optional()
        .map_err(StoreError::from)?;
    match value {
        Some(v) if !v.is_empty() => Ok(v),
        _ => Err(Error::Internal(format!("store_meta has no `{key}`"))),
    }
}

fn io_err(path: &Path) -> impl Fn(io::Error) -> StoreError + '_ {
    move |source| StoreError::Io {
        path: path.to_path_buf(),
        source,
    }
}

pub fn guide_new(env: &GuideNewEnv<'_>, args: &GuideNewArgs) -> Result<GuideNewOutput, Error> {
    let range = args.commits.as_deref().map(parse_commits_arg).transpose()?;
    let run_id = RunId::new(&args.run);
    let mut store = Store::open_for_run(&env.home, &run_id)?.with_clock(Arc::clone(&env.clock));
    runs::require_active(&mut store, &run_id)?;
    let (owner, repo, bound) = store.tx(|tx| {
        Ok::<_, Error>((
            meta(tx, "owner")?,
            meta(tx, "repo")?,
            meta(tx, "git_common_dir")?,
        ))
    })?;
    let n = store.pr().number();
    let store_dir = fs::canonicalize(store.dir()).map_err(io_err(store.dir()))?;
    let clone = env.git.discover()?;
    let current = clone.common_dir.to_string_lossy().into_owned();
    if current != bound {
        return Err(GuideError::StoreBound(format!(
            "the store for {owner}/{repo}#{n} is bound to the clone whose git dir is {bound}; \
             this clone's git dir is {current}. Run prguide guide new from the bound clone"
        ))
        .into());
    }
    let git = env.git.with_dir(&clone.toplevel);
    let remote = git.remote_for(&owner, &repo)?;
    let gh_repo = Repo::github(&owner, &repo)?;
    let pr = env.github.pr(&gh_repo, n)?;
    let commits = env.github.commits(&gh_repo, n)?;
    let shas: Vec<String> = commits.iter().map(|c| c.sha.clone()).collect();
    let selection = match &range {
        None => Selection::Full,
        Some((a, b)) => Selection::Commits {
            first: expand_commit(a, &shas)?,
            last: expand_commit(b, &shas)?,
        },
    };
    git.fetch(&remote.name, &Refspec::for_pr(n, &pr.base_ref))?;
    let head = git.resolve_commit(&fetch_head_ref(n))?;
    let base_tip = git.resolve_commit(&fetch_base_ref(n))?;
    if head != pr.head_sha {
        return Err(GuideError::PrHeadMoved(format!(
            "the PR head moved during guide new: GitHub reports {} but refs/pull/{n}/head \
             fetched {head}; run prguide guide new again",
            pr.head_sha
        ))
        .into());
    }
    let snapshot = Snapshot::new(&git, &head, &base_tip)?;
    let resolved = selection.resolve(&git, &snapshot, &shas)?;
    let files: Vec<FileOut> = resolved
        .files(&git)?
        .iter()
        .map(FileOut::from_diff)
        .collect();

    // All reads are done; now write.
    let worktree = store_dir.join("worktree");
    git.prepare_worktree(&clone, &worktree, &snapshot.head)?;

    let drafts_dir = store_dir.join("drafts");
    fs::create_dir_all(&drafts_dir).map_err(io_err(&drafts_dir))?;
    let meta_ok = fs::symlink_metadata(&drafts_dir)
        .map_err(io_err(&drafts_dir))?
        .is_dir();
    if !meta_ok {
        return Err(StoreError::Io {
            path: drafts_dir,
            source: io::Error::other(
                "not a real directory; prguide writes drafts only into a real directory",
            ),
        }
        .into());
    }
    let draft_id = ulid::Ulid::generate().to_string();
    let draft = drafts_dir.join(&draft_id);
    let partial = drafts_dir.join(format!("{draft_id}.partial"));
    let provenance = Provenance {
        format: FORMAT.to_owned(),
        pr: ManifestPr {
            host: "github.com".to_owned(),
            repo: format!("{owner}/{repo}"),
            number: n,
        },
        snapshot: ManifestSnapshot {
            base: snapshot.base.clone(),
            head: snapshot.head.clone(),
        },
        selection: ManifestSelection {
            kind: match selection {
                Selection::Full => SelectionKind::Full,
                Selection::Commits { .. } => SelectionKind::Commits,
            },
            base: resolved.base.clone(),
            head: resolved.head.clone(),
            commits: resolved.commits.clone(),
        },
    };
    let provenance_json =
        serde_json::to_string(&provenance).map_err(|e| Error::Internal(e.to_string()))?;
    let result = write_draft(
        &mut store,
        &run_id,
        &draft_id,
        &draft,
        &partial,
        &render_manifest(&provenance),
        &provenance_json,
    );
    if result.is_err() {
        let _ = fs::remove_dir_all(&partial);
    }
    result?;

    let selected: Vec<Commit> = commits
        .into_iter()
        .filter(|c| resolved.commits.contains(&c.sha))
        .collect();
    Ok(GuideNewOutput {
        draft_id,
        draft,
        worktree,
        snapshot: SnapshotOut {
            base: snapshot.base,
            head: snapshot.head,
        },
        selection: SelectionOut {
            kind: provenance.selection.kind,
            base: resolved.base,
            head: resolved.head,
        },
        commits: selected,
        files,
    })
}

/// A.11 steps (3) to (5): write the partial dir, then gate and insert the row
/// in one transaction, then rename.
fn write_draft(
    store: &mut Store,
    run_id: &RunId,
    draft_id: &str,
    draft: &Path,
    partial: &Path,
    manifest: &str,
    provenance_json: &str,
) -> Result<(), Error> {
    fs::create_dir(partial).map_err(io_err(partial))?;
    let manifest_path = partial.join(MANIFEST_FILE);
    fs::write(&manifest_path, manifest).map_err(io_err(&manifest_path))?;
    let slides = partial.join(SLIDES_DIR);
    fs::create_dir(&slides).map_err(io_err(&slides))?;
    let clock = store.clock();
    let path = draft.to_string_lossy().into_owned();
    let active = store.tx(|tx| {
        let now = clock.now();
        match runs::gate(tx, now, run_id)? {
            RunGate::Active(_) => {
                drafts::insert(
                    tx,
                    &NewDraft {
                        id: draft_id,
                        run_id: run_id.as_str(),
                        path: &path,
                        provenance_json,
                        created_at: &now.to_rfc3339(),
                    },
                )?;
                Ok::<_, Error>(true)
            }
            RunGate::Stopped(_) => Ok(false),
        }
    })?;
    if !active {
        return Err(StoreError::RunStopped {
            run_id: run_id.to_string(),
        }
        .into());
    }
    fs::rename(partial, draft).map_err(io_err(partial))?;
    Ok(())
}
