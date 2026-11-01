//! `prguide check`: validate a draft and print the diagnostics on stdout,
//! whatever the outcome.

use std::{
    io::{self, Write},
    path::{Path, PathBuf},
    sync::Arc,
};

use crate::{
    Error,
    git::{Git, GitError, RepoClone},
    guide::{
        GuideError,
        check::{CheckReport, DraftRow, check_draft, draft_path, draft_row, store_binding},
    },
    store::{Clock, Store, SystemClock, prguide_home, runs},
};

#[derive(Debug, clap::Args)]
pub struct CheckArgs {
    pub draft: PathBuf,
    #[arg(long)]
    pub run: String,
}

/// Everything `check` reads from its surroundings.
pub struct CheckEnv {
    pub git: Git,
    pub home: PathBuf,
    pub clock: Arc<dyn Clock>,
}

/// A draft resolved against its run, its store and the current clone.
pub(crate) struct ResolvedDraft {
    pub row: DraftRow,
    pub clone: RepoClone,
}

/// The run gate, the drafts row and the store binding, in `check`'s order.
/// `command` names the calling command in the `store_bound` message.
pub(crate) fn resolve_draft(
    store: &mut Store,
    git: &Git,
    run_id: &runs::RunId,
    draft: &Path,
    command: &str,
) -> Result<ResolvedDraft, Error> {
    runs::require_active(store, run_id)?;
    let unknown = || {
        GuideError::DraftUnknown(format!(
            "{:?} is not a draft of run {}: pass the `draft` path that prguide guide new \
             printed for this run",
            draft, run_id
        ))
    };
    let path = draft_path(draft, git.dir()).ok_or_else(unknown)?;
    let (row, (owner, repo, bound)) = store.tx(|tx| {
        let row = draft_row(tx, run_id, &path)?.ok_or_else(unknown)?;
        Ok::<_, Error>((row, store_binding(tx)?))
    })?;
    let n = store.pr().number();
    let clone = git.discover()?;
    let current = clone.common_dir.to_string_lossy().into_owned();
    if current != bound {
        return Err(GuideError::StoreBound(format!(
            "the store for {owner}/{repo}#{n} is bound to the clone whose git dir is {bound}; \
             this clone's git dir is {current}. Run {command} from the bound clone"
        ))
        .into());
    }
    Ok(ResolvedDraft { row, clone })
}

/// Steps 2 to 7 of the failure order, then `check_draft`. Never prints.
pub fn check(env: &CheckEnv, args: &CheckArgs) -> Result<CheckReport, Error> {
    let run_id = runs::RunId::new(&args.run);
    let mut store = Store::open_for_run(&env.home, &run_id)?.with_clock(Arc::clone(&env.clock));
    let resolved = resolve_draft(&mut store, &env.git, &run_id, &args.draft, "prguide check")?;
    drop(store);
    check_draft(
        &env.git.with_dir(&resolved.clone.toplevel),
        Path::new(&resolved.row.path),
        &resolved.row.provenance_json,
    )
}

/// Rewrites the characters terminals or readers may act on (DEL, C1, bidi
/// controls, line and paragraph separators) as `\u` escapes. The input is JSON
/// text, so they can only be inside strings.
pub(crate) fn escape_terminal_controls(json: &str) -> String {
    let mut out = String::with_capacity(json.len());
    for c in json.chars() {
        if matches!(c,
            '\u{7f}'..='\u{9f}'
            | '\u{61c}'
            | '\u{200e}'
            | '\u{200f}'
            | '\u{2028}'
            | '\u{2029}'
            | '\u{202a}'..='\u{202e}'
            | '\u{2066}'..='\u{2069}')
        {
            out.push_str(&format!("\\u{:04x}", c as u32));
        } else {
            out.push(c);
        }
    }
    out
}

pub fn run(args: CheckArgs) -> Result<(), Error> {
    let cwd = std::env::current_dir()
        .map_err(|e| GitError::NotInClone(format!("cannot read the current directory: {e}")))?;
    let env = CheckEnv {
        git: Git::new(cwd),
        home: prguide_home()?,
        clock: Arc::new(SystemClock),
    };
    let report = check(&env, &args)?;
    let text = serde_json::to_string(&report).map_err(|e| Error::Internal(e.to_string()))?;
    let text = escape_terminal_controls(&text);
    {
        let mut stdout = io::stdout().lock();
        writeln!(stdout, "{text}")
            .and_then(|()| stdout.flush())
            .map_err(|e| Error::Internal(e.to_string()))?;
    }
    if report.ok {
        Ok(())
    } else {
        std::process::exit(1)
    }
}

#[cfg(test)]
mod tests {
    use super::escape_terminal_controls;

    /// U2 escape_terminal_controls_round_trips.
    #[test]
    fn escape_terminal_controls_round_trips() {
        let set = ('\u{7f}'..='\u{9f}')
            .chain(['\u{61c}', '\u{200e}', '\u{200f}', '\u{2028}', '\u{2029}'])
            .chain('\u{202a}'..='\u{202e}')
            .chain('\u{2066}'..='\u{2069}');
        for c in set {
            let s = format!("a{c}b");
            let json = serde_json::to_string(&s).unwrap();
            let escaped = escape_terminal_controls(&json);
            assert!(!escaped.contains(c), "{:04x}", c as u32);
            assert!(escaped.contains(&format!("\\u{:04x}", c as u32)));
            assert_eq!(serde_json::from_str::<String>(&escaped).unwrap(), s);
        }
        let plain = serde_json::to_string("é→\u{1b}abc").unwrap();
        assert_eq!(escape_terminal_controls(&plain), plain);
    }
}
