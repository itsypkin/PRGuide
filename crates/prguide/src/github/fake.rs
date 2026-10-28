//! In-memory scriptable GitHub implementation for tests.

use std::{
    collections::{HashMap, VecDeque},
    sync::{Mutex, PoisonError},
    thread,
    time::Duration,
};

use super::{
    Commit, GitHub, GitHubError, PostedComment, Pr, Repo, Review, ReviewCreated, ReviewRequest,
    check_commit_count,
};

/// The `submitted_at` the fake gives every stored review.
pub const FAKE_SUBMITTED_AT: &str = "2026-01-01T00:00:00Z";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FakeMethod {
    Pr,
    Commits,
    CreateReview,
    ReviewComments,
    ListReviews,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FakeCall {
    pub method: FakeMethod,
    pub repo: Repo,
    pub number: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FakeReviewRequest {
    pub repo: Repo,
    pub number: u64,
    pub request: ReviewRequest,
}

struct StoredReview {
    review: Review,
    comments: Vec<PostedComment>,
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
    reviews: HashMap<(Repo, u64), Vec<StoredReview>>,
    review_requests: Vec<FakeReviewRequest>,
    created_then_errors: VecDeque<GitHubError>,
    next_review_id: u64,
    next_comment_id: u64,
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
                reviews: HashMap::new(),
                review_requests: Vec::new(),
                created_then_errors: VecDeque::new(),
                next_review_id: 1,
                next_comment_id: 1,
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
    /// Queues an error returned by a `create_review` that did store the review.
    pub fn push_created_then_error(&self, error: GitHubError) {
        self.lock().created_then_errors.push_back(error);
    }
    /// Every `create_review` request, in order, including failed ones.
    pub fn review_requests(&self) -> Vec<FakeReviewRequest> {
        self.lock().review_requests.clone()
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
    fn create_review(
        &self,
        repo: &Repo,
        number: u64,
        request: &ReviewRequest,
    ) -> Result<ReviewCreated, GitHubError> {
        self.create_review_impl(repo, number, request)
    }
    fn review_comments(
        &self,
        repo: &Repo,
        number: u64,
        review_id: u64,
    ) -> Result<Vec<PostedComment>, GitHubError> {
        if let Some(error) = self.before_call(FakeMethod::ReviewComments, repo, number) {
            return Err(error);
        }
        Ok(self
            .lock()
            .reviews
            .get(&(repo.clone(), number))
            .and_then(|reviews| reviews.iter().find(|stored| stored.review.id == review_id))
            .map(|stored| stored.comments.clone())
            .unwrap_or_default())
    }
    fn list_reviews(&self, repo: &Repo, number: u64) -> Result<Vec<Review>, GitHubError> {
        if let Some(error) = self.before_call(FakeMethod::ListReviews, repo, number) {
            return Err(error);
        }
        Ok(self
            .lock()
            .reviews
            .get(&(repo.clone(), number))
            .map(|reviews| reviews.iter().map(|stored| stored.review.clone()).collect())
            .unwrap_or_default())
    }
}

impl FakeGitHub {
    fn create_review_impl(
        &self,
        repo: &Repo,
        number: u64,
        request: &ReviewRequest,
    ) -> Result<ReviewCreated, GitHubError> {
        self.lock().review_requests.push(FakeReviewRequest {
            repo: repo.clone(),
            number,
            request: request.clone(),
        });
        if let Some(error) = self.before_call(FakeMethod::CreateReview, repo, number) {
            return Err(error);
        }
        let mut state = self.lock();
        let id = state.next_review_id;
        state.next_review_id += 1;
        let mut comments = Vec::new();
        for comment in &request.comments {
            comments.push(PostedComment {
                id: state.next_comment_id,
                path: comment.path.clone(),
                line: Some(comment.line),
                body: comment.body.clone(),
            });
            state.next_comment_id += 1;
        }
        state
            .reviews
            .entry((repo.clone(), number))
            .or_default()
            .push(StoredReview {
                review: Review {
                    id,
                    body: request.body.clone(),
                    commit_id: Some(request.commit_id.clone()),
                    submitted_at: Some(FAKE_SUBMITTED_AT.to_owned()),
                },
                comments,
            });
        match state.created_then_errors.pop_front() {
            Some(error) => Err(error),
            None => Ok(ReviewCreated { id }),
        }
    }
}
