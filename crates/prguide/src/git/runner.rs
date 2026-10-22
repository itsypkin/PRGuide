//! Safe, deterministic invocation of the local git executable.

use std::{
    ffi::{OsStr, OsString},
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::{Command, ExitStatus, Stdio},
    thread,
};

use super::{GitError, parse_github_remote};
use crate::git::refs::{fetch_base_ref, fetch_head_ref};

const REMOVED_ENV: [&str; 7] = [
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_COMMON_DIR",
    "GIT_NAMESPACE",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
];

/// A local repository and environment in which to run git.
#[derive(Debug, Clone)]
pub struct Git {
    dir: PathBuf,
    envs: Vec<(OsString, OsString)>,
}

impl Git {
    /// Create a runner rooted at `dir`.
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self {
            dir: dir.into(),
            envs: Vec::new(),
        }
    }
    /// Adds an environment variable to every invocation (primarily for isolated tests).
    pub fn env(mut self, key: impl AsRef<OsStr>, value: impl AsRef<OsStr>) -> Self {
        self.envs
            .push((key.as_ref().to_owned(), value.as_ref().to_owned()));
        self
    }
    /// Clone this runner with a different working directory.
    pub fn with_dir(&self, dir: impl Into<PathBuf>) -> Self {
        Self {
            dir: dir.into(),
            envs: self.envs.clone(),
        }
    }
    /// Root directory used by this runner.
    pub fn dir(&self) -> &Path {
        &self.dir
    }
    /// Run git and return captured stdout on success.
    pub fn run<I, S>(&self, args: I) -> Result<Vec<u8>, GitError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.execute(args, None, &[])
    }
    /// Run git, supplying its standard input, and return captured stdout on success.
    pub fn run_with_stdin<I, S>(&self, args: I, stdin: &[u8]) -> Result<Vec<u8>, GitError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.execute(args, Some(stdin), &[])
    }

    fn command<I, S>(&self, args: I, extra_removed_env: &[&str]) -> (Command, Vec<OsString>)
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let mut argv = vec![
            OsString::from("--literal-pathspecs"),
            OsString::from("-c"),
            OsString::from("core.quotepath=off"),
            OsString::from("-C"),
            self.dir.clone().into_os_string(),
        ];
        argv.extend(args.into_iter().map(|s| s.as_ref().to_owned()));
        let mut cmd = Command::new("git");
        cmd.args(&argv)
            .envs(self.envs.iter().map(|(key, value)| (key, value)));
        for key in REMOVED_ENV
            .into_iter()
            .chain(extra_removed_env.iter().copied())
        {
            cmd.env_remove(key);
        }
        (cmd, argv)
    }
    fn failed(argv: &[OsString], status: Option<ExitStatus>, stderr: impl AsRef<[u8]>) -> GitError {
        GitError::Failed {
            command: format!(
                "git {}",
                argv.iter()
                    .map(|s| s.to_string_lossy())
                    .collect::<Vec<_>>()
                    .join(" ")
            ),
            status: status
                .as_ref()
                .map_or_else(|| "not run".to_owned(), ToString::to_string),
            stderr: String::from_utf8_lossy(stderr.as_ref())
                .trim_end()
                .to_owned(),
            exit_code: status.and_then(|status| status.code()),
        }
    }
    fn failed_operation(operation: &str, stderr: impl AsRef<[u8]>) -> GitError {
        GitError::Failed {
            command: operation.to_owned(),
            status: "not run".to_owned(),
            stderr: String::from_utf8_lossy(stderr.as_ref())
                .trim_end()
                .to_owned(),
            exit_code: None,
        }
    }

    fn run_removing_env<I, S>(
        &self,
        args: I,
        extra_removed_env: &[&str],
    ) -> Result<Vec<u8>, GitError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.execute(args, None, extra_removed_env)
    }
    fn execute<I, S>(
        &self,
        args: I,
        input: Option<&[u8]>,
        extra_removed_env: &[&str],
    ) -> Result<Vec<u8>, GitError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let (mut command, argv) = self.command(args, extra_removed_env);
        command.stdout(Stdio::piped()).stderr(Stdio::piped());
        if input.is_some() {
            command.stdin(Stdio::piped());
        } else {
            command.stdin(Stdio::null());
        }
        let mut child = command.spawn().map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                GitError::Missing
            } else {
                Self::failed(&argv, None, e.to_string())
            }
        })?;
        if let Some(input) = input {
            let mut pipe = child
                .stdin
                .take()
                .ok_or_else(|| Self::failed(&argv, None, "stdin was unavailable"))?;
            let data = input.to_vec();
            let writer = thread::spawn(move || pipe.write_all(&data));
            let output = child
                .wait_with_output()
                .map_err(|e| Self::failed(&argv, None, e.to_string()))?;
            if !output.status.success() {
                return Err(Self::failed(&argv, Some(output.status), output.stderr));
            }
            writer
                .join()
                .map_err(|_| Self::failed(&argv, None, "stdin writer panicked"))?
                .map_err(|e| Self::failed(&argv, None, e.to_string()))?;
            Ok(output.stdout)
        } else {
            let output = child
                .wait_with_output()
                .map_err(|e| Self::failed(&argv, None, e.to_string()))?;
            if output.status.success() {
                Ok(output.stdout)
            } else {
                Err(Self::failed(&argv, Some(output.status), output.stderr))
            }
        }
    }
    /// Discover canonical worktree and common git directories.
    pub fn discover(&self) -> Result<RepoClone, GitError> {
        let output = self
            .run(["rev-parse", "--git-common-dir", "--show-toplevel"])
            .map_err(|e| match e {
                GitError::Failed {
                    exit_code: Some(_),
                    stderr,
                    ..
                } => GitError::NotInClone(format!(
                    "not inside a git clone ({}): {}",
                    self.dir.display(),
                    stderr
                )),
                other => other,
            })?;
        let text = String::from_utf8_lossy(&output);
        let lines: Vec<_> = text.lines().collect();
        if lines.len() < 2 {
            return Err(GitError::NotInClone(format!(
                "not inside a git clone ({})",
                self.dir.display()
            )));
        }
        let common = PathBuf::from(lines[0]);
        let common = if common.is_absolute() {
            common
        } else {
            self.dir.join(common)
        };
        let common_dir = fs::canonicalize(common).map_err(|e| {
            GitError::NotInClone(format!(
                "not inside a git clone ({}): {}",
                self.dir.display(),
                e
            ))
        })?;
        let toplevel = fs::canonicalize(lines[1]).map_err(|e| {
            GitError::NotInClone(format!(
                "not inside a git clone ({}): {}",
                self.dir.display(),
                e
            ))
        })?;
        Ok(RepoClone {
            toplevel,
            common_dir,
        })
    }
    /// Find the remote matching a GitHub owner and repository.
    pub fn remote_for(&self, owner: &str, repo: &str) -> Result<Remote, GitError> {
        let raw = match self.run(["config", "--get-regexp", "^remote\\..*\\.url$"]) {
            Ok(v) => v,
            Err(GitError::Failed {
                exit_code: Some(1),
                stderr,
                ..
            }) if stderr.is_empty() => Vec::new(),
            Err(e) => return Err(e),
        };
        let mut urls: std::collections::BTreeMap<String, Vec<String>> = Default::default();
        for line in String::from_utf8_lossy(&raw).lines() {
            if let Some((key, value)) = line.split_once(' ')
                && let Some(name) = key
                    .strip_prefix("remote.")
                    .and_then(|s| s.strip_suffix(".url"))
            {
                urls.entry(name.into()).or_default().push(value.into());
            }
        }
        let effective = self.run(["remote", "-v"])?;
        let mut fetches = std::collections::BTreeMap::new();
        for line in String::from_utf8_lossy(&effective).lines() {
            if let Some(line) = line.strip_suffix(" (fetch)")
                && let Some((name, url)) = line.split_once('\t')
            {
                fetches.insert(name.to_owned(), url.to_owned());
            }
        }
        let mut candidates = Vec::new();
        for name in urls.keys().chain(fetches.keys()) {
            if candidates.iter().any(|n: &String| n == name) {
                continue;
            }
            let matches = urls
                .get(name)
                .into_iter()
                .flatten()
                .chain(fetches.get(name))
                .any(|url| {
                    parse_github_remote(url).is_some_and(|(o, r)| {
                        o.eq_ignore_ascii_case(owner) && r.eq_ignore_ascii_case(repo)
                    })
                });
            if matches {
                candidates.push(name.clone());
            }
        }
        candidates.sort();
        let name = candidates
            .iter()
            .find(|n| n.as_str() == "origin")
            .or_else(|| candidates.first())
            .cloned()
            .ok_or_else(|| {
                GitError::NotInClone(format!(
                    "no remote of this clone points at github.com/{owner}/{repo}"
                ))
            })?;
        let url = fetches
            .get(&name)
            .cloned()
            .or_else(|| urls.get(&name).and_then(|v| v.first()).cloned())
            .unwrap_or_default();
        Ok(Remote { name, url })
    }
    /// Fetch forced refspecs without tags or recursive submodules.
    pub fn fetch(&self, remote: &str, refspecs: &[Refspec]) -> Result<(), GitError> {
        if refspecs.is_empty() {
            return Ok(());
        }
        let mut args = vec![
            "fetch".into(),
            "--no-tags".into(),
            "--no-recurse-submodules".into(),
            remote.into(),
        ];
        args.extend(refspecs.iter().map(|r| format!("+{}:{}", r.src, r.dst)));
        self.clone()
            .env("GIT_TERMINAL_PROMPT", "0")
            .run(args)
            .map(|_| ())
    }
    /// Read a blob at a commit path without filters or text conversion.
    pub fn show(&self, sha: &str, path: &str) -> Result<Option<Vec<u8>>, GitError> {
        let entry = self.run(["ls-tree", "-z", "--full-tree", sha, "--", path])?;
        let Some(entry) = entry.split(|b| *b == 0).next().filter(|v| !v.is_empty()) else {
            return Ok(None);
        };
        let parts: Vec<_> = entry.splitn(2, |b| *b == b'\t').collect();
        let [meta, entry_path] = parts.as_slice() else {
            return Ok(None);
        };
        if *entry_path != path.as_bytes() {
            return Ok(None);
        }
        let fields: Vec<_> = meta.split(|b| *b == b' ').collect();
        if fields.len() != 3 || fields[1] != b"blob" {
            return Ok(None);
        }
        let oid = String::from_utf8_lossy(fields[2]).into_owned();
        self.run(["cat-file", "blob", &oid]).map(Some)
    }
    /// Find the merge base, or `None` for unrelated histories.
    pub fn merge_base(&self, a: &str, b: &str) -> Result<Option<String>, GitError> {
        match self.run(["merge-base", a, b]) {
            Ok(v) => Ok(Some(String::from_utf8_lossy(&v).trim().into())),
            Err(GitError::Failed {
                exit_code: Some(1),
                stderr,
                ..
            }) if stderr.is_empty() => Ok(None),
            Err(e) => Err(e),
        }
    }
    /// Return commit parents in git's order.
    pub fn rev_parents(&self, sha: &str) -> Result<Vec<String>, GitError> {
        Ok(
            String::from_utf8_lossy(&self.run(["rev-list", "--parents", "-n", "1", sha, "--"])?)
                .split_whitespace()
                .skip(1)
                .map(str::to_owned)
                .collect(),
        )
    }
    /// Resolve a revision to a full commit object ID.
    pub fn resolve_commit(&self, rev: &str) -> Result<String, GitError> {
        Ok(String::from_utf8_lossy(&self.run([
            "rev-parse",
            "--verify",
            "--end-of-options",
            &format!("{rev}^{{commit}}"),
        ])?)
        .trim()
        .into())
    }
    /// Produce a config-independent raw git diff.
    pub fn diff(&self, spec: &DiffSpec) -> Result<Vec<u8>, GitError> {
        let clone = self.discover()?;
        let isolated = tempfile::tempdir()
            .map_err(|e| Self::failed_operation("git diff setup", e.to_string()))?;
        let home = isolated.path().join("home");
        fs::create_dir_all(home.join(".config"))
            .map_err(|e| Self::failed_operation("git diff setup", e.to_string()))?;
        fs::write(home.join(".gitconfig"), "")
            .map_err(|e| Self::failed_operation("git diff setup", e.to_string()))?;
        let git_dir = isolated.path().join("git");
        let isolated_git = self
            .with_dir(isolated.path())
            .env("HOME", &home)
            .env("XDG_CONFIG_HOME", home.join(".config"))
            .env("GIT_ATTR_NOSYSTEM", "1")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", home.join(".gitconfig"));
        isolated_git.run(vec![
            OsString::from("init"),
            OsString::from("--bare"),
            OsString::from("--template="),
            git_dir.clone().into_os_string(),
        ])?;
        let objects_info = git_dir.join("objects/info");
        fs::create_dir_all(&objects_info)
            .map_err(|e| Self::failed_operation("git diff setup", e.to_string()))?;
        fs::write(
            objects_info.join("alternates"),
            format!("{}\n", clone.common_dir.join("objects").display()),
        )
        .map_err(|e| Self::failed_operation("git diff setup", e.to_string()))?;

        let (first, second) = match spec {
            DiffSpec::ThreeDot { base_tip, head, .. } => {
                (self.resolve_commit(base_tip)?, self.resolve_commit(head)?)
            }
            DiffSpec::TwoDot { from, to, .. } => {
                (self.resolve_commit(from)?, self.resolve_commit(to)?)
            }
        };
        isolated_git.run([
            OsStr::new("--git-dir"),
            git_dir.as_os_str(),
            OsStr::new("config"),
            OsStr::new("core.bare"),
            OsStr::new("false"),
        ])?;
        let worktree = isolated.path().join("worktree");
        fs::create_dir(&worktree)
            .map_err(|e| Self::failed_operation("git diff setup", e.to_string()))?;
        let attribute_paths = isolated_git.run([
            OsStr::new("--git-dir"),
            git_dir.as_os_str(),
            OsStr::new("ls-tree"),
            OsStr::new("-r"),
            OsStr::new("-z"),
            OsStr::new("--name-only"),
            OsStr::new(&second),
        ])?;
        let attribute_paths: Vec<_> = attribute_paths
            .split(|byte| *byte == 0)
            .filter(|path| *path == b".gitattributes" || path.ends_with(b"/.gitattributes"))
            .map(|path| OsString::from(String::from_utf8_lossy(path).into_owned()))
            .collect();
        isolated_git.run([
            OsStr::new("--git-dir"),
            git_dir.as_os_str(),
            OsStr::new("read-tree"),
            OsStr::new(&second),
        ])?;
        if !attribute_paths.is_empty() {
            let mut checkout = vec![
                OsString::from("--git-dir"),
                git_dir.clone().into_os_string(),
                OsString::from("--work-tree"),
                worktree.clone().into_os_string(),
                OsString::from("checkout"),
                OsString::from(&second),
                OsString::from("--"),
            ];
            checkout.extend(attribute_paths);
            isolated_git.run(checkout)?;
        }

        self.with_dir(&worktree)
            .env("HOME", &home)
            .env("XDG_CONFIG_HOME", home.join(".config"))
            .env("GIT_ATTR_NOSYSTEM", "1")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", home.join(".gitconfig"))
            .run_removing_env(
                [
                    vec![
                        OsString::from("--git-dir"),
                        git_dir.into_os_string(),
                        OsString::from("--work-tree"),
                        worktree.into_os_string(),
                    ],
                    spec.args_for(&first, &second)
                        .into_iter()
                        .map(OsString::from)
                        .collect(),
                ]
                .concat(),
                &["GIT_CONFIG_COUNT", "GIT_CONFIG_PARAMETERS", "GIT_DIFF_OPTS"],
            )
    }
}

/// Canonical worktree and shared git metadata paths.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoClone {
    pub toplevel: PathBuf,
    pub common_dir: PathBuf,
}
/// A matching remote and its effective fetch URL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Remote {
    pub name: String,
    pub url: String,
}
/// A forced fetch refspec source and destination.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refspec {
    pub src: String,
    pub dst: String,
}
impl Refspec {
    /// Build PR head and base pin refspecs.
    pub fn for_pr(number: u64, base_ref: &str) -> [Self; 2] {
        [
            Self {
                src: format!("refs/pull/{number}/head"),
                dst: fetch_head_ref(number),
            },
            Self {
                src: format!("refs/heads/{base_ref}"),
                dst: fetch_base_ref(number),
            },
        ]
    }
}
/// The fixed diff forms supported by the git layer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiffSpec {
    ThreeDot {
        base_tip: String,
        head: String,
        paths: Vec<String>,
    },
    TwoDot {
        from: String,
        to: String,
        paths: Vec<String>,
    },
}
impl DiffSpec {
    /// Build fixed argv after runner-global options.
    pub fn args(&self) -> Vec<String> {
        match self {
            Self::ThreeDot { base_tip, head, .. } => self.args_for(base_tip, head),
            Self::TwoDot { from, to, .. } => self.args_for(from, to),
        }
    }
    fn args_for(&self, first: &str, second: &str) -> Vec<String> {
        let mut a = vec![
            "-c".into(),
            "diff.suppressBlankEmpty=false".into(),
            "-c".into(),
            "core.attributesFile=/dev/null".into(),
            "diff".into(),
            "--no-ext-diff".into(),
            "--no-textconv".into(),
            "--no-color".into(),
            "-M".into(),
            "-l0".into(),
            "--full-index".into(),
            "-U3".into(),
            "--src-prefix=a/".into(),
            "--dst-prefix=b/".into(),
            "--no-relative".into(),
            "--inter-hunk-context=0".into(),
            "-O/dev/null".into(),
            "--submodule=short".into(),
            "--ignore-submodules=none".into(),
            "--diff-algorithm=myers".into(),
            "--indent-heuristic".into(),
        ];
        match self {
            Self::ThreeDot { .. } => a.push(format!("{first}...{second}")),
            Self::TwoDot { .. } => a.push(format!("{first}..{second}")),
        };
        a.push("--".into());
        a.extend(self.paths().iter().cloned());
        a
    }
    fn paths(&self) -> &Vec<String> {
        match self {
            Self::ThreeDot { paths, .. } | Self::TwoDot { paths, .. } => paths,
        }
    }
}
