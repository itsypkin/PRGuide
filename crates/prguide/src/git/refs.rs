//! Operations for prguide-owned pinned references.

use super::{Git, GitError};

/// Namespace owned by prguide.
pub const PREFIX: &str = "refs/prguide/";
/// Return the pinned fetched PR head ref.
pub fn fetch_head_ref(number: u64) -> String {
    format!("{PREFIX}{number}/fetch/head")
}
/// Return the pinned fetched base ref.
pub fn fetch_base_ref(number: u64) -> String {
    format!("{PREFIX}{number}/fetch/base")
}
/// A pinned reference whose actual value differs from its expected value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefMismatch {
    pub name: String,
    pub expected: String,
    pub actual: Option<String>,
}

fn valid_name(name: &str) -> Result<(), GitError> {
    if !name.starts_with(PREFIX) {
        return Err(GitError::InvalidRef {
            name: name.into(),
            reason: format!("must start with {PREFIX}"),
        });
    }
    if name
        .chars()
        .any(|c| c.is_ascii_whitespace() || c.is_ascii_control())
    {
        return Err(GitError::InvalidRef {
            name: name.into(),
            reason: "contains whitespace or control characters".into(),
        });
    }
    Ok(())
}
fn valid_sha(name: &str, sha: &str) -> Result<(), GitError> {
    if !matches!(sha.len(), 40 | 64)
        || !sha
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err(GitError::InvalidRef {
            name: name.into(),
            reason: "object ID must be 40 or 64 lowercase hexadecimal characters".into(),
        });
    }
    Ok(())
}
/// Refuse names whose parent directories under the git directory are symlinks:
/// `--no-deref` protects only the leaf ref, git still follows a linked parent.
fn no_linked_parents(git: &Git, names: &[&String]) -> Result<(), GitError> {
    let out = git.run(["rev-parse", "--git-common-dir"])?;
    let text = String::from_utf8_lossy(&out);
    let common = git.dir().join(text.trim());
    for name in names {
        let parts: Vec<&str> = name.split('/').collect();
        let mut path = common.clone();
        for part in &parts[..parts.len() - 1] {
            path.push(part);
            match std::fs::symlink_metadata(&path) {
                Ok(m) if m.file_type().is_symlink() => {
                    return Err(GitError::InvalidRef {
                        name: (*name).clone(),
                        reason: "a directory in this ref's path is a symlink; prguide does not write refs through a link".into(),
                    });
                }
                Ok(_) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => break,
                Err(e) => {
                    return Err(GitError::InvalidRef {
                        name: (*name).clone(),
                        reason: format!("cannot inspect the ref's directory: {e}"),
                    });
                }
            }
        }
    }
    Ok(())
}
fn parse_refs(bytes: &[u8]) -> Vec<(String, String)> {
    String::from_utf8_lossy(bytes)
        .lines()
        .filter_map(|line| {
            line.split_once(' ')
                .map(|(sha, name)| (name.into(), sha.into()))
        })
        .collect()
}

impl Git {
    /// Atomically create or overwrite prguide-owned refs.
    pub fn write_refs(&self, refs: &[(String, String)]) -> Result<(), GitError> {
        for (name, sha) in refs {
            valid_name(name)?;
            valid_sha(name, sha)?;
        }
        if refs.is_empty() {
            return Ok(());
        }
        no_linked_parents(self, &refs.iter().map(|(n, _)| n).collect::<Vec<_>>())?;
        let input = refs
            .iter()
            .map(|(n, s)| format!("update {n} {s}\n"))
            .collect::<String>();
        self.run_with_stdin(["update-ref", "--no-deref", "--stdin"], input.as_bytes())
            .map(|_| ())
    }
    /// Return all missing or mismatched refs in input order.
    pub fn verify_refs(&self, refs: &[(String, String)]) -> Result<Vec<RefMismatch>, GitError> {
        if refs.is_empty() {
            return Ok(Vec::new());
        }
        let mut args = vec![
            "for-each-ref".to_owned(),
            "--format=%(objectname) %(refname)".to_owned(),
            "--".to_owned(),
        ];
        args.extend(refs.iter().map(|(n, _)| n.clone()));
        let actual = parse_refs(&self.run(args)?)
            .into_iter()
            .collect::<std::collections::HashMap<_, _>>();
        Ok(refs
            .iter()
            .filter_map(|(name, expected)| {
                let value = actual.get(name).cloned();
                (value.as_deref() != Some(expected)).then(|| RefMismatch {
                    name: name.clone(),
                    expected: expected.clone(),
                    actual: value,
                })
            })
            .collect())
    }
    /// Atomically delete prguide-owned refs; missing refs are harmless.
    pub fn delete_refs(&self, names: &[String]) -> Result<(), GitError> {
        for name in names {
            valid_name(name)?;
        }
        if names.is_empty() {
            return Ok(());
        }
        no_linked_parents(self, &names.iter().collect::<Vec<_>>())?;
        let input = names
            .iter()
            .map(|n| format!("delete {n}\n"))
            .collect::<String>();
        self.run_with_stdin(["update-ref", "--no-deref", "--stdin"], input.as_bytes())
            .map(|_| ())
    }
    /// List refs below a namespace boundary, sorted by name.
    pub fn list_refs(&self, prefix: &str) -> Result<Vec<(String, String)>, GitError> {
        if !prefix.starts_with("refs/") {
            return Err(GitError::InvalidRef {
                name: prefix.into(),
                reason: "must start with refs/".into(),
            });
        }
        let stripped = prefix.strip_suffix('/').unwrap_or(prefix);
        let mut values: Vec<_> = parse_refs(&self.run([
            "for-each-ref",
            "--format=%(objectname) %(refname)",
            "--",
            stripped,
        ])?)
        .into_iter()
        .filter(|(name, _)| name == stripped || name.starts_with(&format!("{stripped}/")))
        .collect();
        values.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(values)
    }
}
