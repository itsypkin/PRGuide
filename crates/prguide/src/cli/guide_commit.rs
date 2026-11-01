//! `prguide guide commit`: turn a checked draft into an immutable guide version.

use std::{io::Write, path::PathBuf, sync::Arc};

use serde::Serialize;

use crate::{
    Error,
    cli::{
        check::{escape_terminal_controls, resolve_draft},
        output::print_json,
    },
    git::{Git, GitError},
    guide::{GuideError, check::Diagnostic, commit::commit},
    store::{Clock, Store, SystemClock, prguide_home, runs::RunId, versions::GuideVersion},
};

#[derive(Debug, clap::Args)]
pub struct GuideCommitArgs {
    pub draft: PathBuf,
    #[arg(long)]
    pub run: String,
    #[arg(long)]
    pub caused_by: Option<String>,
}

/// Everything `guide commit` reads from its surroundings.
pub struct GuideCommitEnv {
    pub git: Git,
    pub home: PathBuf,
    pub clock: Arc<dyn Clock>,
}

/// Opens the run's store, resolves the draft as `check` does, then commits.
pub fn guide_commit(env: &GuideCommitEnv, args: &GuideCommitArgs) -> Result<GuideVersion, Error> {
    let run_id = RunId::new(&args.run);
    let mut store = Store::open_for_run(&env.home, &run_id)?.with_clock(Arc::clone(&env.clock));
    let resolved = resolve_draft(
        &mut store,
        &env.git,
        &run_id,
        &args.draft,
        "prguide guide commit",
    )?;
    let git = env.git.with_dir(&resolved.clone.toplevel);
    commit(
        &mut store,
        &git,
        &resolved.row,
        &run_id,
        args.caused_by.as_deref(),
    )
}

/// The `check_failed` line: `code`, `message`, `diagnostics`, in that order.
#[derive(Serialize)]
struct CheckFailedOut<'a> {
    code: &'static str,
    message: &'a str,
    diagnostics: &'a [Diagnostic],
}

pub fn run(args: GuideCommitArgs) -> Result<(), Error> {
    let cwd = std::env::current_dir()
        .map_err(|e| GitError::NotInClone(format!("cannot read the current directory: {e}")))?;
    let env = GuideCommitEnv {
        git: Git::new(cwd),
        home: prguide_home()?,
        clock: Arc::new(SystemClock),
    };
    match guide_commit(&env, &args) {
        Ok(version) => print_json(&version),
        Err(Error::Guide(GuideError::CheckFailed {
            message,
            diagnostics,
        })) => {
            let out = CheckFailedOut {
                code: "check_failed",
                message: &message,
                diagnostics: &diagnostics,
            };
            let text = serde_json::to_string(&out).map_err(|e| Error::Internal(e.to_string()))?;
            let text = escape_terminal_controls(&text);
            let mut stderr = std::io::stderr().lock();
            let _ = writeln!(stderr, "{text}").and_then(|()| stderr.flush());
            std::process::exit(1)
        }
        Err(other) => Err(other),
    }
}
