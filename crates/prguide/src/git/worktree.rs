//! The prguide-owned git worktree: created or reset to an exact commit, with the
//! reviewer's config and the PR's attributes unable to run programs or change bytes.

use std::{
    collections::BTreeMap,
    ffi::{OsStr, OsString},
    fs,
    os::unix::{ffi::OsStrExt, fs::MetadataExt},
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
        "core.fsmonitor=",
        "submodule.recurse=false",
        "core.fileMode=true",
        "core.symlinks=false",
        "core.autocrlf=false",
        "core.attributesFile=/dev/null",
        // Without it, `clean -ffdx` deletes NFD-named files: readdir reports
        // their names precomposed, so git sees them as untracked.
        "core.precomposeunicode=false",
    ] {
        args.push("-c".into());
        args.push(pin.into());
    }
    args.push("-c".into());
    args.push(format!("attr.tree={EMPTY_TREE}").into());
    args.push("-c".into());
    args.push("core.sparseCheckout=false".into());
    // Else git >= 2.48 rewrites the reviewer's config for relative worktree paths.
    args.push("-c".into());
    args.push("worktree.useRelativePaths=false".into());
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

/// lstat each name under `root` (never following symlinks). Any error skips the name.
fn entry_keys<'a>(
    root: &Path,
    names: impl IntoIterator<Item = &'a [u8]>,
) -> Vec<((u64, u64), Vec<u8>)> {
    names
        .into_iter()
        .filter_map(|name| {
            fs::symlink_metadata(root.join(OsStr::from_bytes(name)))
                .ok()
                .map(|meta| ((meta.dev(), meta.ino()), name.to_vec()))
        })
        .collect()
}

/// Groups of two or more names sharing one (dev, ino), sorted by bytes inside
/// each group and then by group.
fn collision_groups(entries: Vec<((u64, u64), Vec<u8>)>) -> Vec<Vec<String>> {
    let mut by_key: BTreeMap<(u64, u64), Vec<Vec<u8>>> = BTreeMap::new();
    for (key, name) in entries {
        by_key.entry(key).or_default().push(name);
    }
    let mut groups: Vec<Vec<String>> = by_key
        .into_values()
        .filter(|names| names.len() >= 2)
        .map(|mut names| {
            names.sort();
            names
                .iter()
                .map(|n| String::from_utf8_lossy(n).into_owned())
                .collect()
        })
        .collect();
    groups.sort();
    groups
}

impl Git {
    /// Read-only: groups of PR-head tree paths that are one filesystem object
    /// in the worktree (case or Unicode-form collisions).
    pub fn path_collisions(&self, path: &Path, head: &str) -> Result<Vec<Vec<String>>, GitError> {
        valid_head(head)?;
        let out = self.with_dir(path).isolated_run(
            &[],
            &["ls-tree", "-r", "-t", "-z", "--full-tree", head],
            path.parent(),
        )?;
        let names = out.split(|b| *b == 0).filter_map(|record| {
            record
                .iter()
                .position(|b| *b == b'\t')
                .map(|i| &record[i + 1..])
        });
        Ok(collision_groups(entry_keys(path, names)))
    }

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
        // `path` must be a real directory (never a symlink to somewhere else) holding
        // a regular `.git` file.
        let real_dir = fs::symlink_metadata(path).is_ok_and(|m| m.is_dir());
        let is_file = fs::symlink_metadata(path.join(".git")).is_ok_and(|m| m.is_file());
        if !real_dir || !is_file {
            return Ok(false);
        }
        let ceiling = path.parent();
        let out = match self.with_dir(path).isolated_run(
            drivers,
            &[
                "rev-parse",
                "--git-common-dir",
                "--show-toplevel",
                "--absolute-git-dir",
            ],
            ceiling,
        ) {
            Ok(out) => out,
            Err(GitError::Failed { .. }) => return Ok(false),
            Err(e) => return Err(e),
        };
        let text = String::from_utf8_lossy(&out);
        let lines: Vec<&str> = text.lines().collect();
        if lines.len() < 3 {
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
        if common != clone.common_dir || top != path {
            return Ok(false);
        }
        // The git dir must be this worktree's own admin dir: its back-pointer
        // names `<path>/.git`. The clone's own git dir has none, and another
        // worktree's points elsewhere.
        let back = fs::read_to_string(Path::new(lines[2]).join("gitdir"))
            .ok()
            .and_then(|text| fs::canonicalize(text.trim_end()).ok());
        Ok(back.is_some_and(|b| Some(b) == fs::canonicalize(path.join(".git")).ok()))
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
    /// Trusts `<path>/.git`: callers must go through `prepare_worktree`, which
    /// checks the worktree first.
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
        git.isolated_run(&drivers, &["clean", "-ffdx"], ceiling)?;
        // `clean` can delete tracked files that something other than git
        // renamed (e.g. NFC to NFD); a second checkout restores them so the
        // worktree ends exactly at the head.
        git.isolated_run(
            &drivers,
            &["checkout", "--detach", "--force", head],
            ceiling,
        )
        .map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::{collision_groups, entry_keys, filter_driver_names};

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

    fn k(dev: u64, ino: u64, name: &str) -> ((u64, u64), Vec<u8>) {
        ((dev, ino), name.as_bytes().to_vec())
    }

    #[test]
    fn collision_groups_sort_paths_and_groups() {
        let input = vec![
            k(1, 3, "zeta"),
            k(1, 9, "alpha/b"),
            k(1, 3, "ZETA"),
            k(2, 3, "solo"),
            k(1, 9, "Alpha/b"),
            k(1, 3, "Zeta"),
            k(1, 9, "ALPHA/b"),
        ];
        assert_eq!(
            collision_groups(input),
            vec![
                vec!["ALPHA/b", "Alpha/b", "alpha/b"],
                vec!["ZETA", "Zeta", "zeta"]
            ]
        );
        assert!(collision_groups(vec![]).is_empty());
        assert!(collision_groups(vec![k(1, 1, "a"), k(1, 2, "b")]).is_empty());
    }

    #[test]
    fn entry_keys_use_lstat_and_skip_unreadable_names() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(dir.path().join("a"), "x").unwrap();
        std::os::unix::fs::symlink("a", dir.path().join("b")).unwrap();
        let keys = entry_keys(
            dir.path(),
            [b"a".as_slice(), b"b", b"missing", b"a/under-a-file"],
        );
        let names: Vec<&[u8]> = keys.iter().map(|(_, n)| n.as_slice()).collect();
        assert_eq!(names, vec![b"a".as_slice(), b"b"]);
        assert_ne!(keys[0].0, keys[1].0);
        assert!(collision_groups(keys).is_empty());
    }
}
