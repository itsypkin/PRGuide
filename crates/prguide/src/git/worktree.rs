//! The prguide-owned git worktree: created or reset to an exact commit, with the
//! reviewer's config and the PR's attributes unable to run programs or change bytes.

use std::{
    ffi::OsString,
    fs,
    path::{Path, PathBuf},
};

use super::{Git, GitError, RepoClone};

const EMPTY_TREE: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";

/// Variables removed from every worktree command.
const REMOVED: [&str; 4] = [
    "GIT_CONFIG_PARAMETERS",
    "GIT_CONFIG_COUNT",
    "GIT_ATTR_SOURCE",
    "GIT_TEST_FSMONITOR",
];

fn valid_head(head: &str) -> Result<(), GitError> {
    if !matches!(head.len(), 40 | 64)
        || !head
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err(GitError::InvalidRef {
            name: head.into(),
            reason: "object ID must be 40 or 64 lowercase hexadecimal characters".into(),
        });
    }
    Ok(())
}

fn io_failed(path: &Path, error: impl std::fmt::Display) -> GitError {
    GitError::Failed {
        command: format!("remove {}", path.display()),
        status: "not run".into(),
        stderr: error.to_string(),
        exit_code: None,
    }
}

/// Names of the filter drivers in `git config -z --name-only` output.
fn filter_driver_names(config_z_output: &[u8]) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    for entry in config_z_output.split(|b| *b == 0) {
        let text = String::from_utf8_lossy(entry);
        let Some(rest) = text.strip_prefix("filter.") else {
            continue;
        };
        let Some(end) = rest.rfind('.') else {
            continue;
        };
        let driver = &rest[..end];
        if driver.is_empty() || driver.contains('=') || names.iter().any(|n| n == driver) {
            continue;
        }
        names.push(driver.to_owned());
    }
    names
}

fn pin_args() -> Vec<OsString> {
    let mut args = Vec::new();
    for pin in [
        "core.hooksPath=/dev/null",
        "hook.post-checkout.enabled=false",
        "hook.reference-transaction.enabled=false",
        "hook.post-index-change.enabled=false",
        "core.fsmonitor=false",
        "submodule.recurse=false",
        "core.fileMode=true",
        "core.symlinks=false",
        "core.autocrlf=false",
        "core.attributesFile=/dev/null",
    ] {
        args.push("-c".into());
        args.push(pin.into());
    }
    args.push("-c".into());
    args.push(format!("attr.tree={EMPTY_TREE}").into());
    args.push("-c".into());
    args.push("core.sparseCheckout=false".into());
    args
}

fn isolated(drivers: &[String], command: &[&str]) -> Vec<OsString> {
    let mut args = pin_args();
    for driver in drivers {
        for (key, value) in [
            ("smudge", ""),
            ("clean", ""),
            ("process", ""),
            ("required", "false"),
        ] {
            args.push("-c".into());
            args.push(format!("filter.{driver}.{key}={value}").into());
        }
    }
    args.extend(command.iter().map(OsString::from));
    args
}

impl Git {
    fn isolated_run(
        &self,
        drivers: &[String],
        command: &[&str],
        ceiling: Option<&Path>,
    ) -> Result<Vec<u8>, GitError> {
        let mut env = vec![
            (OsString::from("GIT_ATTR_NOSYSTEM"), OsString::from("1")),
            (OsString::from("GIT_TERMINAL_PROMPT"), OsString::from("0")),
        ];
        if let Some(ceiling) = ceiling {
            env.push((
                OsString::from("GIT_CEILING_DIRECTORIES"),
                ceiling.as_os_str().to_owned(),
            ));
        }
        self.execute(isolated(drivers, command), None, &REMOVED, &env)
    }

    /// Configured filter drivers, read with the same isolation (no filter options).
    fn configured_filters(&self, ceiling: Option<&Path>) -> Result<Vec<String>, GitError> {
        match self.isolated_run(
            &[],
            &["config", "-z", "--name-only", "--get-regexp", "^filter\\."],
            ceiling,
        ) {
            Ok(out) => Ok(filter_driver_names(&out)),
            Err(GitError::Failed {
                exit_code: Some(1),
                stderr,
                ..
            }) if stderr.is_empty() => Ok(Vec::new()),
            Err(e) => Err(e),
        }
    }

    fn worktree_healthy(
        &self,
        clone: &RepoClone,
        path: &Path,
        drivers: &[String],
    ) -> Result<bool, GitError> {
        let is_file = fs::symlink_metadata(path.join(".git")).is_ok_and(|m| m.is_file());
        if !is_file {
            return Ok(false);
        }
        let ceiling = path.parent();
        let out = match self.with_dir(path).isolated_run(
            drivers,
            &["rev-parse", "--git-common-dir", "--show-toplevel"],
            ceiling,
        ) {
            Ok(out) => out,
            Err(GitError::Failed { .. }) => return Ok(false),
            Err(e) => return Err(e),
        };
        let text = String::from_utf8_lossy(&out);
        let lines: Vec<&str> = text.lines().collect();
        if lines.len() < 2 {
            return Ok(false);
        }
        let common = PathBuf::from(lines[0]);
        let common = if common.is_absolute() {
            common
        } else {
            path.join(common)
        };
        let (Ok(common), Ok(top), Ok(path)) = (
            fs::canonicalize(common),
            fs::canonicalize(lines[1]),
            fs::canonicalize(path),
        ) else {
            return Ok(false);
        };
        Ok(common == clone.common_dir && top == path)
    }

    /// Put `path` at `head`: reset it if it is a healthy worktree of `clone`,
    /// otherwise (re)create it. `self` is rooted at the clone's toplevel.
    pub fn prepare_worktree(
        &self,
        clone: &RepoClone,
        path: &Path,
        head: &str,
    ) -> Result<(), GitError> {
        valid_head(head)?;
        let drivers = self.configured_filters(None)?;
        if self.worktree_healthy(clone, path, &drivers)? {
            return self.reset_worktree(path, head);
        }
        if let Ok(meta) = fs::symlink_metadata(path) {
            let removed = if meta.is_dir() {
                fs::remove_dir_all(path)
            } else {
                fs::remove_file(path)
            };
            removed.map_err(|e| io_failed(path, e))?;
        }
        let path_text = path.to_string_lossy();
        self.isolated_run(
            &drivers,
            &["worktree", "add", "-f", "--detach", &path_text, head],
            None,
        )
        .map(|_| ())
    }

    /// In an existing worktree: checkout --detach --force <head>, then clean -ffdx.
    pub fn reset_worktree(&self, path: &Path, head: &str) -> Result<(), GitError> {
        valid_head(head)?;
        let git = self.with_dir(path);
        let ceiling = path.parent();
        let drivers = git.configured_filters(ceiling)?;
        git.isolated_run(
            &drivers,
            &["checkout", "--detach", "--force", head],
            ceiling,
        )?;
        git.isolated_run(&drivers, &["clean", "-ffdx"], ceiling)
            .map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::filter_driver_names;

    #[test]
    fn filter_driver_names_parse_dotted_subsections() {
        assert_eq!(
            filter_driver_names(
                b"filter.lfs.process\0filter.lfs.required\0filter.a.b.smudge\0filter.x=y.clean\0"
            ),
            vec!["lfs".to_owned(), "a.b".to_owned()]
        );
        assert!(filter_driver_names(b"").is_empty());
    }
}
