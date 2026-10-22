//! In-memory scriptable GitHub implementation for tests.

use std::{
    collections::{HashMap, VecDeque},
    sync::{Mutex, PoisonError},
    thread,
    time::Duration,
};

use super::{Commit, GitHub, GitHubError, Pr, Repo, check_commit_count};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FakeMethod {
    Pr,
    Commits,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FakeCall {
    pub method: FakeMethod,
    pub repo: Repo,
    pub number: u64,
}

pub struct FakeGitHub {
    state: Mutex<State>,
}
struct State {
    prs: HashMap<(Repo, u64), Pr>,
    commits: HashMap<(Repo, u64), Vec<Commit>>,
    errors: HashMap<FakeMethod, VecDeque<GitHubError>>,
    delays: HashMap<FakeMethod, Duration>,
    calls: Vec<FakeCall>,
}

impl FakeGitHub {
    pub fn new() -> Self {
        Self {
            state: Mutex::new(State {
                prs: HashMap::new(),
                commits: HashMap::new(),
                errors: HashMap::new(),
                delays: HashMap::new(),
                calls: Vec::new(),
            }),
        }
    }
    pub fn set_pr(&self, repo: &Repo, pr: Pr) {
        self.lock().prs.insert((repo.clone(), pr.number), pr);
    }
    pub fn set_commits(&self, repo: &Repo, number: u64, commits: Vec<Commit>) {
        self.lock().commits.insert((repo.clone(), number), commits);
    }
    pub fn push_error(&self, method: FakeMethod, error: GitHubError) {
        self.lock()
            .errors
            .entry(method)
            .or_default()
            .push_back(error);
    }
    pub fn set_delay(&self, method: FakeMethod, delay: Duration) {
        self.lock().delays.insert(method, delay);
    }
    pub fn calls(&self) -> Vec<FakeCall> {
        self.lock().calls.clone()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
    fn before_call(&self, method: FakeMethod, repo: &Repo, number: u64) -> Option<GitHubError> {
        let delay = {
            let mut state = self.lock();
            state.calls.push(FakeCall {
                method,
                repo: repo.clone(),
                number,
            });
            state.delays.get(&method).copied()
        };
        if let Some(delay) = delay {
            thread::sleep(delay);
        }
        self.lock()
            .errors
            .get_mut(&method)
            .and_then(VecDeque::pop_front)
    }
}
impl Default for FakeGitHub {
    fn default() -> Self {
        Self::new()
    }
}

impl GitHub for FakeGitHub {
    fn pr(&self, repo: &Repo, number: u64) -> Result<Pr, GitHubError> {
        if let Some(error) = self.before_call(FakeMethod::Pr, repo, number) {
            return Err(error);
        }
        self.lock()
            .prs
            .get(&(repo.clone(), number))
            .cloned()
            .ok_or_else(|| GitHubError::PrNotFound {
                repo: repo.to_string(),
                number,
            })
    }
    fn commits(&self, repo: &Repo, number: u64) -> Result<Vec<Commit>, GitHubError> {
        if let Some(error) = self.before_call(FakeMethod::Commits, repo, number) {
            return Err(error);
        }
        let commits = self
            .lock()
            .commits
            .get(&(repo.clone(), number))
            .cloned()
            .ok_or_else(|| GitHubError::PrNotFound {
                repo: repo.to_string(),
                number,
            })?;
        check_commit_count(repo, number, commits.len())?;
        Ok(commits)
    }
}
