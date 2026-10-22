//! A scripted git fixture for git-backed integration tests.
//!
//! It is reachable only from `tests/*.rs` crates that declare `mod support`, never
//! from library unit tests. It redirects github.com clone URLs to its local bare
//! remote and isolates every command from user and system git configuration.

use prguide::git::Git;
use std::{
    cell::Cell,
    fs,
    path::{Path, PathBuf},
    process::Command,
};
use tempfile::TempDir;

pub struct FixtureRepo {
    root: TempDir,
    work: PathBuf,
    remote: PathBuf,
    owner: String,
    repo: String,
    counter: Cell<u64>,
}
impl FixtureRepo {
    pub fn new() -> Self {
        Self::with_name("octo", "widget")
    }
    pub fn with_name(owner: &str, repo: &str) -> Self {
        let root = TempDir::new().unwrap();
        fs::create_dir_all(root.path().join("home/.config")).unwrap();
        fs::write(root.path().join("home/.gitconfig"), "").unwrap();
        let work = root.path().join("work");
        let remote = root.path().join("remote.git");
        let value = Self {
            root,
            work,
            remote,
            owner: owner.into(),
            repo: repo.into(),
            counter: Cell::new(0),
        };
        value.raw(value.path(), ["init", "--bare", "remote.git"]);
        value.raw(value.path(), ["clone", "remote.git", "work"]);
        value.write("README.md", b"fixture\n");
        value.commit("initial");
        value.push("main");
        value
    }
    fn date(&self) -> String {
        format!("{} +0000", 1_767_225_600 + self.counter.get())
    }
    fn command(&self, dir: &Path) -> Command {
        let mut c = Command::new("git");
        c.current_dir(dir)
            .env("HOME", self.path().join("home"))
            .env("XDG_CONFIG_HOME", self.path().join("home/.config"))
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", self.path().join("home/.gitconfig"))
            .env("GIT_AUTHOR_NAME", "Fixture")
            .env("GIT_AUTHOR_EMAIL", "fixture@example.com")
            .env("GIT_COMMITTER_NAME", "Fixture")
            .env("GIT_COMMITTER_EMAIL", "fixture@example.com")
            .env("GIT_AUTHOR_DATE", self.date())
            .env("GIT_COMMITTER_DATE", self.date());
        for key in [
            "GIT_DIR",
            "GIT_WORK_TREE",
            "GIT_INDEX_FILE",
            "GIT_COMMON_DIR",
            "GIT_NAMESPACE",
            "GIT_OBJECT_DIRECTORY",
            "GIT_ALTERNATE_OBJECT_DIRECTORIES",
        ] {
            c.env_remove(key);
        }
        c
    }
    fn raw<I, S>(&self, dir: &Path, args: I) -> String
    where
        I: IntoIterator<Item = S>,
        S: AsRef<std::ffi::OsStr>,
    {
        let output = self.command(dir).args(args).output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().into()
    }
    pub fn path(&self) -> &Path {
        self.root.path()
    }
    pub fn work_path(&self) -> &Path {
        &self.work
    }
    pub fn remote_path(&self) -> &Path {
        &self.remote
    }
    pub fn write(&self, path: &str, contents: impl AsRef<[u8]>) {
        let target = self.work_path().join(path);
        if let Some(p) = target.parent() {
            fs::create_dir_all(p).unwrap();
        }
        fs::write(target, contents).unwrap();
    }
    pub fn commit(&self, message: &str) -> String {
        self.counter.set(self.counter.get() + 1);
        self.raw(self.work_path(), ["add", "-A"]);
        self.raw(self.work_path(), ["commit", "-m", message]);
        self.head()
    }
    pub fn head(&self) -> String {
        self.raw(self.work_path(), ["rev-parse", "HEAD"])
    }
    pub fn push(&self, branch: &str) {
        self.raw(
            self.work_path(),
            [
                "push",
                "--force",
                "origin",
                &format!("HEAD:refs/heads/{branch}"),
            ],
        );
    }
    pub fn push_pr(&self, number: u64, rev: &str) {
        self.raw(
            self.work_path(),
            [
                "push",
                "--force",
                "origin",
                &format!("{rev}:refs/pull/{number}/head"),
            ],
        );
    }
    pub fn clone(&self, name: &str) -> PathBuf {
        let target = self.path().join(name);
        self.raw(
            self.path(),
            [
                "clone",
                self.remote_path().to_str().unwrap(),
                target.to_str().unwrap(),
            ],
        );
        self.raw(&target, ["checkout", "-B", "main", "origin/main"]);
        let url = format!("https://github.com/{}/{}.git", self.owner, self.repo);
        self.raw(&target, ["config", "remote.origin.url", &url]);
        self.raw(
            &target,
            [
                "config",
                &format!("url.{}.insteadOf", self.remote_path().display()),
                &url,
            ],
        );
        target
    }
    pub fn git(&self, dir: impl Into<PathBuf>) -> Git {
        let home = self.path().join("home");
        Git::new(dir)
            .env("HOME", &home)
            .env("XDG_CONFIG_HOME", home.join(".config"))
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", home.join(".gitconfig"))
            .env("GIT_AUTHOR_NAME", "Fixture")
            .env("GIT_AUTHOR_EMAIL", "fixture@example.com")
            .env("GIT_COMMITTER_NAME", "Fixture")
            .env("GIT_COMMITTER_EMAIL", "fixture@example.com")
            .env("GIT_AUTHOR_DATE", self.date())
            .env("GIT_COMMITTER_DATE", self.date())
    }
    pub fn run<I, S>(&self, dir: &Path, args: I) -> String
    where
        I: IntoIterator<Item = S>,
        S: AsRef<std::ffi::OsStr>,
    {
        self.raw(dir, args)
    }
}
