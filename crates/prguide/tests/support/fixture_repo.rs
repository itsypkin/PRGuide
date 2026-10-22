//! A scripted git fixture for every git-backed integration test.
//!
//! It is reachable only from `tests/*.rs` crates that declare `mod support`, never
//! from library unit tests. It redirects github.com clone URLs to its local bare
//! remote and isolates every command from user and system git configuration.

use prguide::git::Git;
use std::{
    cell::Cell,
    ffi::OsStr,
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
        let value = Self {
            work: root.path().join("work"),
            remote: root.path().join("remote.git"),
            root,
            owner: owner.into(),
            repo: repo.into(),
            counter: Cell::new(0),
        };
        value.raw(value.path(), ["init", "--bare", "-b", "main", "remote.git"]);
        value.raw(value.path(), ["init", "-b", "main", "work"]);
        value.raw(
            value.work_path(),
            ["remote", "add", "origin", "../remote.git"],
        );
        value.write("README.md", b"fixture\n");
        value.commit("initial");
        value.push("main");
        value
    }

    fn date(&self) -> String {
        format!("{} +0000", 1_767_225_600 + self.counter.get())
    }

    fn command(&self, dir: &Path) -> Command {
        let home = self.path().join("home");
        let mut command = Command::new("git");
        command
            .current_dir(dir)
            .env("HOME", &home)
            .env("XDG_CONFIG_HOME", home.join(".config"))
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", home.join(".gitconfig"))
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
            command.env_remove(key);
        }
        command
    }

    fn raw<I, S>(&self, dir: &Path, args: I) -> String
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
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
        let target = self.work.join(path);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(target, contents).unwrap();
    }
    pub fn remove(&self, path: &str) {
        fs::remove_file(self.work.join(path)).unwrap();
    }
    pub fn rename(&self, from: &str, to: &str) {
        self.raw(&self.work, ["mv", from, to]);
    }
    pub fn set_executable(&self, path: &str, executable: bool) {
        let target = self.work.join(path);
        let mut permissions = fs::metadata(&target).unwrap().permissions();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            permissions.set_mode(if executable { 0o755 } else { 0o644 });
        }
        fs::set_permissions(&target, permissions).unwrap();
        self.raw(
            &self.work,
            [
                "update-index",
                if executable {
                    "--chmod=+x"
                } else {
                    "--chmod=-x"
                },
                path,
            ],
        );
    }
    pub fn gitlink(&self, path: &str, sha: &str) {
        fs::create_dir_all(self.work.join(path)).unwrap();
        self.raw(
            &self.work,
            [
                "update-index",
                "--add",
                "--cacheinfo",
                &format!("160000,{sha},{path}"),
            ],
        );
    }
    pub fn commit(&self, message: &str) -> String {
        self.counter.set(self.counter.get() + 1);
        self.raw(&self.work, ["add", "-A"]);
        self.raw(&self.work, ["commit", "-m", message]);
        self.head()
    }
    pub fn branch(&self, name: &str) {
        self.raw(&self.work, ["branch", name]);
    }
    pub fn checkout(&self, name: &str) {
        self.raw(&self.work, ["checkout", name]);
    }
    pub fn merge(&self, name: &str, message: &str) -> String {
        self.counter.set(self.counter.get() + 1);
        self.raw(&self.work, ["merge", "--no-ff", name, "-m", message]);
        self.head()
    }
    pub fn head(&self) -> String {
        self.raw(&self.work, ["rev-parse", "HEAD"])
    }
    pub fn push(&self, branch: &str) {
        self.raw(
            &self.work,
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
            &self.work,
            [
                "push",
                "--force",
                "origin",
                &format!("{rev}:refs/pull/{number}/head"),
            ],
        );
    }
    pub fn push_tag(&self, name: &str, rev: &str) {
        self.raw(
            &self.work,
            [
                "push",
                "--force",
                "origin",
                &format!("{rev}:refs/tags/{name}"),
            ],
        );
    }
    pub fn clone(&self, name: &str) -> PathBuf {
        let target = self.path().join(name);
        self.raw(
            self.path(),
            [
                "clone",
                self.remote.to_str().unwrap(),
                target.to_str().unwrap(),
            ],
        );
        let url = format!("https://github.com/{}/{}.git", self.owner, self.repo);
        self.raw(&target, ["config", "remote.origin.url", &url]);
        self.raw(
            &target,
            [
                "config",
                &format!("url.{}.insteadOf", self.remote.display()),
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
        S: AsRef<OsStr>,
    {
        self.raw(dir, args)
    }
}
