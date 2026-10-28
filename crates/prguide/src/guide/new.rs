//! `guide new` support: the `--commits` syntax, prefix expansion, the draft's
//! provenance and the `guide.yaml` text.

use serde::Serialize;

use super::{
    GuideError,
    model::{GuideManifest, ManifestPr, ManifestSelection, ManifestSnapshot, SelectionKind},
};
use crate::Error;

/// What a draft records about its origin: the manifest minus `slides`.
/// `provenance_json` is `serde_json::to_string` of this.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Provenance {
    pub format: String,
    pub pr: ManifestPr,
    pub snapshot: ManifestSnapshot,
    pub selection: ManifestSelection,
}

impl Provenance {
    /// The one builder `guide new` and `check` share.
    pub fn from_manifest(m: &GuideManifest) -> Provenance {
        Provenance {
            format: m.format.clone(),
            pr: m.pr.clone(),
            snapshot: m.snapshot.clone(),
            selection: m.selection.clone(),
        }
    }
}

/// Splits `<A>..<B>`: each side 4 to 64 hex digits, lowercased.
pub fn parse_commits_arg(value: &str) -> Result<(String, String), Error> {
    let usage = || {
        Error::Usage(format!(
            "--commits takes <sha>..<sha>: two commits of the PR, each a full SHA or a unique \
             prefix of 4 to 64 hex digits; got `{value}`"
        ))
    };
    let (a, b) = value.split_once("..").ok_or_else(usage)?;
    let ok = |s: &str| (4..=64).contains(&s.len()) && s.bytes().all(|c| c.is_ascii_hexdigit());
    if !ok(a) || !ok(b) {
        return Err(usage());
    }
    Ok((a.to_ascii_lowercase(), b.to_ascii_lowercase()))
}

/// Matches `given` as a prefix of the PR's commit SHAs; it must match exactly one.
pub fn expand_commit(given: &str, pr_commits: &[String]) -> Result<String, Error> {
    let matches: Vec<&String> = pr_commits
        .iter()
        .filter(|sha| sha.starts_with(given))
        .collect();
    match matches.as_slice() {
        [] => Err(GuideError::RangeNotInPr(format!("commit {given} is not in the PR")).into()),
        [one] => Ok((*one).clone()),
        many => Err(GuideError::CommitAmbiguous(format!(
            "commit prefix {given} matches {} commits in the PR: {}; give more digits",
            many.len(),
            many.iter()
                .map(|s| s.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ))
        .into()),
    }
}

/// A valid YAML double-quoted scalar.
fn quote(s: &str) -> String {
    let json = serde_json::Value::String(s.to_owned()).to_string();
    // JSON leaves DEL, the C1 controls, U+FFFE and U+FFFF raw; YAML rejects them, so escape them.
    let mut out = String::with_capacity(json.len());
    for c in json.chars() {
        if ('\u{7f}'..='\u{9f}').contains(&c) || c == '\u{fffe}' || c == '\u{ffff}' {
            out.push_str(&format!("\\u{:04X}", c as u32));
        } else {
            out.push(c);
        }
    }
    out
}

/// The exact `guide.yaml` of a new draft.
pub fn render_manifest(p: &Provenance) -> String {
    let kind = match p.selection.kind {
        SelectionKind::Full => "full",
        SelectionKind::Commits => "commits",
    };
    let mut out = String::from("# Written by prguide guide new. Edit only `slides`.\n");
    out.push_str(&format!("format: {}\n", quote(&p.format)));
    out.push_str(&format!(
        "pr: {{ host: {}, repo: {}, number: {} }}\n",
        quote(&p.pr.host),
        quote(&p.pr.repo),
        p.pr.number
    ));
    out.push_str(&format!(
        "snapshot: {{ base: {}, head: {} }}\n",
        quote(&p.snapshot.base),
        quote(&p.snapshot.head)
    ));
    out.push_str("selection:\n");
    out.push_str(&format!("  kind: {kind}\n"));
    out.push_str(&format!("  base: {}\n", quote(&p.selection.base)));
    out.push_str(&format!("  head: {}\n", quote(&p.selection.head)));
    if p.selection.commits.is_empty() {
        out.push_str("  commits: []\n");
    } else {
        out.push_str("  commits:\n");
        for sha in &p.selection.commits {
            out.push_str(&format!("    - {}\n", quote(sha)));
        }
    }
    out.push_str("slides: []\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::guide::parse::parse_manifest;

    /// m1 DEL and C1 controls survive a render and parse round trip.
    #[test]
    fn quote_escapes_del_and_c1_controls() {
        let nasty = "a\u{7f}b\u{85}c\u{9b}d\u{80}e\u{9f}f\u{fffe}g\u{ffff}";
        let p = Provenance {
            format: "prguide/v1".into(),
            pr: ManifestPr {
                host: "github.com".into(),
                repo: nasty.into(),
                number: 7,
            },
            snapshot: ManifestSnapshot {
                base: "aaaa".into(),
                head: "bbbb".into(),
            },
            selection: ManifestSelection {
                kind: SelectionKind::Full,
                base: "aaaa".into(),
                head: "bbbb".into(),
                commits: vec![],
            },
        };
        let text = render_manifest(&p);
        assert!(
            !text
                .chars()
                .any(|c| ('\u{7f}'..='\u{9f}').contains(&c) || c == '\u{fffe}' || c == '\u{ffff}')
        );
        let m = parse_manifest(&text).expect("parses");
        assert_eq!(m.pr.repo, nasty);
    }
}
