//! Test-only in-memory transport double for the GitHub adapter
//! contract tests. It lives outside the library so a production
//! composition root cannot select it as a transport.
#![allow(dead_code)]

use std::collections::BTreeMap;

use workloop_core::identity::{ChangeRequestId, IssueRef, RepositoryRef};
use workloop_core::observation::{CheckConclusion, IssueState};
use workloop_github::api::{
    ApiError, GitHubApi, RemoteCheckRun, RemoteComment, RemoteIssue, RemotePull, RemoteReview,
    RemoteReviewFinding,
};

/// One pull request the in-memory remote stores.
#[derive(Debug, Clone, PartialEq, Eq)]
struct PullRecord {
    id: String,
    display_number: u64,
    head_sha: String,
    source_branch: String,
    target_branch: String,
    title: String,
    body: String,
}

impl PullRecord {
    fn remote(&self) -> RemotePull {
        RemotePull {
            id: self.id.clone(),
            display_number: self.display_number,
            head_sha: self.head_sha.clone(),
            source_branch: self.source_branch.clone(),
            target_branch: self.target_branch.clone(),
            title: self.title.clone(),
            body: self.body.clone(),
            // No code path in this remote can merge: merge lives
            // outside the adapter's vocabulary.
            merged: false,
        }
    }
}

/// A GitHub-shaped in-memory remote behind [`GitHubApi`]. Its model
/// follows the REST surfaces (branches, pull requests, comments, check
/// runs per exact SHA, reviews with their `commit_id`) rather than any
/// fixture's internal maps, so adapter code cannot lean on fixture
/// internals.
#[derive(Debug, Default)]
pub struct InMemoryGitHub {
    /// `(repository, branch)` → head object id.
    branches: BTreeMap<(String, String), String>,
    /// `(repository, child)` → parent object id — ancestry for the
    /// read-back relation of a push.
    parents: BTreeMap<(String, String), String>,
    /// `(repository, change id)` → stored pull request.
    pulls: BTreeMap<(String, String), PullRecord>,
    /// Next stable PR node id — a host-global identity counter.
    next_pull: u64,
    /// `repository` → next display number: a per-repository namespace.
    next_number: BTreeMap<String, u64>,
    /// Posted comments in post order: `(repository, change id)`.
    comments: Vec<(String, String, RemoteComment)>,
    /// Posted check runs; a run is bound to the SHA it examined.
    checks: Vec<(String, RemoteCheckRun)>,
    /// `(repository, change id)` → reviews, oldest first.
    reviews: BTreeMap<(String, String), Vec<RemoteReview>>,
    /// `(repository, issue id)` → the issue's current read — scoped
    /// identity, so a shared display number elsewhere is a different
    /// issue.
    issues: BTreeMap<(String, String), RemoteIssue>,
    /// Posted review findings in submission order: `(repository, change id)`.
    findings: Vec<(String, String, RemoteReviewFinding)>,
    /// Every mutation, in order — the write history assertions use.
    writes: Vec<String>,
    /// `operation` → refusal consumed before the next matching call
    /// (no effect lands). One-shot per arming.
    refusals: BTreeMap<String, ApiError>,
    /// `operation` → lost response returned *after* the effect landed
    /// (or, for a read, instead of answering). One-shot per arming.
    lost: BTreeMap<String, ApiError>,
    /// The login `viewer()` attests — the ambient identity the double
    /// models; the suite's bound publisher by default.
    viewer: Option<String>,
    next_comment: u64,
}

impl InMemoryGitHub {
    pub fn new() -> Self {
        Self::default()
    }

    /// The mutation history, in order — what the adapter's receipts
    /// drove on this remote.
    pub fn writes(&self) -> &[String] {
        &self.writes
    }

    /// Test control: the login `viewer()` attests on the next calls —
    /// the ambient credential's verified identity.
    pub fn set_viewer(&mut self, login: &str) {
        self.viewer = Some(login.to_string());
    }

    /// The head the remote records for `(repository, branch)`.
    pub fn branch_head(&self, repository: &RepositoryRef, branch: &str) -> Option<String> {
        self.branches
            .get(&(repository.to_string(), branch.to_string()))
            .cloned()
    }

    /// The stored pull request, for assertions on post-effect state.
    pub fn pull(&self, repository: &RepositoryRef, change_id: &str) -> Option<RemotePull> {
        self.pulls
            .get(&(repository.to_string(), change_id.to_string()))
            .map(|p| p.remote())
    }

    /// Test control: seed a branch head (a branch GitHub already has).
    pub fn seed_branch(&mut self, repository: &RepositoryRef, branch: &str, head: &str) {
        self.note_object(repository, head);
        self.branches.insert(
            (repository.to_string(), branch.to_string()),
            head.to_string(),
        );
    }

    /// Test control: a racing actor moved a pull request's head —
    /// the stale-head condition a head-bound write must catch.
    pub fn set_pull_head(&mut self, repository: &RepositoryRef, change_id: &str, head: &str) {
        let key = (repository.to_string(), change_id.to_string());
        if let Some(existing) = self.pulls.get(&key) {
            let prior = existing.head_sha.clone();
            self.parents
                .insert((repository.to_string(), head.to_string()), prior);
        }
        self.note_object(repository, head);
        if let Some(pull) = self.pulls.get_mut(&key) {
            pull.head_sha = head.to_string();
        }
    }

    /// Test control: the authoritative read the remote reports for one
    /// source issue, keyed by scoped `(repository, issue id)` identity.
    pub fn set_issue(
        &mut self,
        issue: &IssueRef,
        source_revision: &str,
        content_digest: &str,
        state: IssueState,
    ) {
        self.issues.insert(
            (
                issue.repository.to_string(),
                issue.issue.as_str().to_string(),
            ),
            RemoteIssue {
                source_revision: source_revision.to_string(),
                content_digest: content_digest.to_string(),
                state,
                title: String::new(),
                body: String::new(),
            },
        );
    }

    /// The stored issue read, for assertions on post-effect state.
    pub fn issue_read(&self, issue: &IssueRef) -> Option<RemoteIssue> {
        self.issues
            .get(&(
                issue.repository.to_string(),
                issue.issue.as_str().to_string(),
            ))
            .cloned()
    }

    /// Test control: seed a check run bound to the exact SHA it
    /// examined — a run on a superseded head stays on that head.
    pub fn seed_check(
        &mut self,
        repository: &RepositoryRef,
        head_sha: &str,
        name: &str,
        run_id: &str,
        conclusion: CheckConclusion,
        producer: &str,
    ) {
        self.checks.push((
            repository.to_string(),
            RemoteCheckRun {
                check_run_id: run_id.to_string(),
                name: name.to_string(),
                head_sha: head_sha.to_string(),
                conclusion,
                producer: producer.to_string(),
            },
        ));
    }

    /// Test control: seed a review whose `commit_id` is the exact
    /// revision it examined.
    pub fn seed_review(
        &mut self,
        repository: &RepositoryRef,
        change_id: &str,
        review_id: &str,
        commit_sha: &str,
        author: &str,
    ) {
        self.reviews
            .entry((repository.to_string(), change_id.to_string()))
            .or_default()
            .push(RemoteReview {
                review_id: review_id.to_string(),
                commit_sha: commit_sha.to_string(),
                author: author.to_string(),
                body: String::new(),
                changes_requested: false,
            });
    }

    /// Test control: seed a review that requests changes — the
    /// host-native blocking decision, with its review body as the
    /// finding text.
    pub fn seed_changes_requested_review(
        &mut self,
        repository: &RepositoryRef,
        change_id: &str,
        review_id: &str,
        commit_sha: &str,
        author: &str,
        body: &str,
    ) {
        self.reviews
            .entry((repository.to_string(), change_id.to_string()))
            .or_default()
            .push(RemoteReview {
                review_id: review_id.to_string(),
                commit_sha: commit_sha.to_string(),
                author: author.to_string(),
                body: body.to_string(),
                changes_requested: true,
            });
    }

    /// Test control: seed one review finding (an inline review comment)
    /// on the revision its review examined.
    pub fn seed_review_finding(
        &mut self,
        repository: &RepositoryRef,
        change_id: &str,
        review_id: &str,
        commit_sha: &str,
        body: &str,
    ) {
        let finding_id = format!(
            "rf-{}-{}",
            review_id,
            self.findings
                .iter()
                .filter(|(_, change, _)| change == change_id)
                .count()
                + 1
        );
        self.findings.push((
            repository.to_string(),
            change_id.to_string(),
            RemoteReviewFinding {
                finding_id,
                review_id: review_id.to_string(),
                commit_sha: commit_sha.to_string(),
                body: body.to_string(),
            },
        ));
    }

    /// Test control: the next call of `operation` is refused before
    /// any effect lands (definite no-effect failure). One-shot.
    pub fn refuse_next(&mut self, operation: &str, reason: &str) {
        self.refusals.insert(
            operation.to_string(),
            ApiError::Refused {
                reason: reason.to_string(),
            },
        );
    }

    /// Test control: the next call of `operation` applies its effect
    /// (when it has one) and then loses the response — the ambiguous
    /// result an adapter must record `unknown` for. One-shot.
    pub fn lose_response(&mut self, operation: &str) {
        self.lost.insert(
            operation.to_string(),
            ApiError::Unknown {
                reason: "response lost".into(),
            },
        );
    }

    fn note_object(&mut self, repository: &RepositoryRef, object: &str) {
        self.parents
            .entry((repository.to_string(), object.to_string()))
            .or_default();
    }

    fn take_refusal(&mut self, operation: &str) -> Option<ApiError> {
        self.refusals.remove(operation)
    }

    fn take_lost(&mut self, operation: &str) -> Option<ApiError> {
        self.lost.remove(operation)
    }

    fn pull_key(repository: &RepositoryRef, change_id: &ChangeRequestId) -> (String, String) {
        (repository.to_string(), change_id.as_str().to_string())
    }
}

impl GitHubApi for InMemoryGitHub {
    fn read_repository(&mut self, repository: &RepositoryRef) -> Result<Option<String>, ApiError> {
        if let Some(err) = self.take_lost("read_repository") {
            return Err(err);
        }
        Ok(self.branch_head(repository, "main"))
    }

    fn read_branch(
        &mut self,
        repository: &RepositoryRef,
        branch: &str,
    ) -> Result<Option<String>, ApiError> {
        if let Some(err) = self.take_lost("read_branch") {
            return Err(err);
        }
        Ok(self.branch_head(repository, branch))
    }

    fn write_ref(
        &mut self,
        repository: &RepositoryRef,
        branch: &str,
        new_head: &str,
        expected_prior: Option<&str>,
    ) -> Result<String, ApiError> {
        if let Some(err) = self.take_refusal("write_ref") {
            return Err(err);
        }
        let actual = self.branch_head(repository, branch);
        if actual.as_deref() != expected_prior {
            // The exact prior-ref guard refuses before any effect —
            // the protocol precondition a live push enforces.
            return Err(ApiError::Refused {
                reason: "stale_head".into(),
            });
        }
        if let Some(prior) = actual.as_ref() {
            self.parents.insert(
                (repository.to_string(), new_head.to_string()),
                prior.clone(),
            );
        }
        self.note_object(repository, new_head);
        self.branches.insert(
            (repository.to_string(), branch.to_string()),
            new_head.to_string(),
        );
        // GitHub keeps every open pull request on its source branch's
        // current head: pushing the branch moves the pull's head too.
        for ((repo, _), pull) in self.pulls.iter_mut() {
            if *repo == repository.to_string() && pull.source_branch == branch {
                pull.head_sha = new_head.to_string();
            }
        }
        self.writes
            .push(format!("push_ref:{repository}:{new_head}"));
        if let Some(err) = self.take_lost("write_ref") {
            return Err(err);
        }
        Ok(new_head.to_string())
    }

    fn read_pull(
        &mut self,
        repository: &RepositoryRef,
        change_id: &ChangeRequestId,
    ) -> Result<RemotePull, ApiError> {
        if let Some(err) = self.take_lost("read_pull") {
            return Err(err);
        }
        self.pulls
            .get(&Self::pull_key(repository, change_id))
            .map(|p| p.remote())
            .ok_or_else(|| ApiError::Refused {
                reason: "pull request not found".into(),
            })
    }

    fn open_pull(
        &mut self,
        repository: &RepositoryRef,
        source_branch: &str,
        target_branch: &str,
        title: &str,
        body: &str,
    ) -> Result<RemotePull, ApiError> {
        if let Some(err) = self.take_refusal("open_pull") {
            return Err(err);
        }
        let head_sha =
            self.branch_head(repository, source_branch)
                .ok_or_else(|| ApiError::Refused {
                    reason: "source branch not found".into(),
                })?;
        self.next_pull += 1;
        let id = format!("pr-{}", self.next_pull);
        let display_number = {
            let counter = self.next_number.entry(repository.to_string()).or_insert(0);
            *counter += 1;
            *counter
        };
        let record = PullRecord {
            id: id.clone(),
            display_number,
            head_sha,
            source_branch: source_branch.to_string(),
            target_branch: target_branch.to_string(),
            title: title.to_string(),
            body: body.to_string(),
        };
        let remote = record.remote();
        self.pulls
            .insert((repository.to_string(), id.clone()), record);
        self.writes.push(format!("open_change:{repository}:{id}"));
        if let Some(err) = self.take_lost("open_pull") {
            return Err(err);
        }
        Ok(remote)
    }

    fn update_pull(
        &mut self,
        repository: &RepositoryRef,
        change_id: &ChangeRequestId,
        title: Option<&str>,
        body: Option<&str>,
    ) -> Result<RemotePull, ApiError> {
        if let Some(err) = self.take_refusal("update_pull") {
            return Err(err);
        }
        let key = Self::pull_key(repository, change_id);
        let Some(pull) = self.pulls.get_mut(&key) else {
            return Err(ApiError::Refused {
                reason: "pull request not found".into(),
            });
        };
        if let Some(title) = title {
            pull.title = title.to_string();
        }
        if let Some(body) = body {
            pull.body = body.to_string();
        }
        let remote = pull.remote();
        self.writes
            .push(format!("update_change:{repository}:{change_id}"));
        if let Some(err) = self.take_lost("update_pull") {
            return Err(err);
        }
        Ok(remote)
    }

    fn comment_on_pull(
        &mut self,
        repository: &RepositoryRef,
        change_id: &ChangeRequestId,
        marker: &str,
        body: &str,
    ) -> Result<RemoteComment, ApiError> {
        if let Some(err) = self.take_refusal("comment_on_pull") {
            return Err(err);
        }
        let key = Self::pull_key(repository, change_id);
        if !self.pulls.contains_key(&key) {
            return Err(ApiError::Refused {
                reason: "pull request not found".into(),
            });
        }
        self.next_comment += 1;
        let comment = RemoteComment {
            comment_id: format!("c-{}", self.next_comment),
            body: body.to_string(),
            marker: marker.to_string(),
        };
        self.comments.push((
            repository.to_string(),
            change_id.as_str().to_string(),
            comment.clone(),
        ));
        self.writes
            .push(format!("post_comment:{repository}:{change_id}"));
        if let Some(err) = self.take_lost("comment_on_pull") {
            return Err(err);
        }
        Ok(comment)
    }

    fn find_comment(
        &mut self,
        repository: &RepositoryRef,
        change_id: &ChangeRequestId,
        marker: &str,
    ) -> Result<Option<RemoteComment>, ApiError> {
        if let Some(err) = self.take_lost("find_comment") {
            return Err(err);
        }
        Ok(self
            .comments
            .iter()
            .find(|(repo, change, comment)| {
                repo == &repository.to_string()
                    && change == change_id.as_str()
                    && comment.marker == marker
            })
            .map(|(_, _, comment)| comment.clone()))
    }

    fn list_pulls_on_branch(
        &mut self,
        repository: &RepositoryRef,
        source_branch: &str,
        head_sha: &str,
    ) -> Result<Vec<RemotePull>, ApiError> {
        if let Some(err) = self.take_lost("list_pulls_on_branch") {
            return Err(err);
        }
        let repository = repository.to_string();
        Ok(self
            .pulls
            .iter()
            .filter(|((repo, _), pull)| {
                *repo == repository
                    && pull.source_branch == source_branch
                    && pull.head_sha == head_sha
            })
            .map(|(_, pull)| pull.remote())
            .collect())
    }

    fn check_runs(
        &mut self,
        repository: &RepositoryRef,
        head_sha: &str,
    ) -> Result<Vec<RemoteCheckRun>, ApiError> {
        if let Some(err) = self.take_lost("check_runs") {
            return Err(err);
        }
        let repository = repository.to_string();
        Ok(self
            .checks
            .iter()
            .filter(|(repo, run)| repo == &repository && run.head_sha == head_sha)
            .map(|(_, run)| run.clone())
            .collect())
    }

    fn reviews(
        &mut self,
        repository: &RepositoryRef,
        change_id: &ChangeRequestId,
    ) -> Result<Vec<RemoteReview>, ApiError> {
        if let Some(err) = self.take_lost("reviews") {
            return Err(err);
        }
        Ok(self
            .reviews
            .get(&Self::pull_key(repository, change_id))
            .cloned()
            .unwrap_or_default())
    }

    fn review_findings(
        &mut self,
        repository: &RepositoryRef,
        change_id: &ChangeRequestId,
    ) -> Result<Vec<RemoteReviewFinding>, ApiError> {
        if let Some(err) = self.take_lost("review_findings") {
            return Err(err);
        }
        let repository = repository.to_string();
        Ok(self
            .findings
            .iter()
            .filter(|(repo, change, _)| *repo == repository && change == change_id.as_str())
            .map(|(_, _, finding)| finding.clone())
            .collect())
    }

    fn read_issue(&mut self, issue: &IssueRef) -> Result<RemoteIssue, ApiError> {
        if let Some(err) = self.take_lost("read_issue") {
            return Err(err);
        }
        self.issue_read(issue).ok_or_else(|| ApiError::Refused {
            reason: "issue not found".into(),
        })
    }

    fn contains_revision(
        &mut self,
        repository: &RepositoryRef,
        ancestor: &str,
        descendant: &str,
    ) -> Result<bool, ApiError> {
        if let Some(err) = self.take_lost("contains_revision") {
            return Err(err);
        }
        let repository = repository.to_string();
        let mut current = descendant.to_string();
        // Bounded by the number of recorded objects — a cycle cannot
        // out-walk it.
        for _ in 0..=self.parents.len() {
            if current == ancestor {
                return Ok(true);
            }
            let Some(parent) = self
                .parents
                .get(&(repository.clone(), current.clone()))
                .filter(|p| !p.is_empty())
            else {
                return Ok(false);
            };
            current = parent.clone();
        }
        Ok(false)
    }

    fn viewer(&mut self) -> Result<String, ApiError> {
        if let Some(err) = self.take_refusal("viewer") {
            return Err(err);
        }
        if let Some(err) = self.take_lost("viewer") {
            return Err(err);
        }
        Ok(self
            .viewer
            .clone()
            .unwrap_or_else(|| "pub-agent".to_string()))
    }
}
