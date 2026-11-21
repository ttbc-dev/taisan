//! The GitHub transport seam: the API surfaces the real adapter calls.
//! Nothing here decides receipts or Observations — that is the
//! adapter's job — so a live implementation (HTTPS against the REST
//! surfaces named in `docs/github-adapter-mapping.md`) and a test
//! implementation satisfy the same trait and run the same adapter
//! code. The in-memory double used by the contract tests lives in the
//! test tree, never in this library surface:
//!
//! ```compile_fail
//! use workloop_github::api::InMemoryGitHub;
//! ```
//!
//! Every operation reports one of two failure classes:
//! - [`ApiError::Refused`] — a definite no-effect refusal (guard
//!   mismatch, validation error). The intent records `failed`.
//! - [`ApiError::Unknown`] — the response was lost; the effect may or
//!   may not have landed. The adapter records `unknown`, which stays
//!   outstanding until reconciled by action ID, and never retries it
//!   blindly.

use std::time::Instant;
use workloop_core::identity::{ChangeRequestId, HostActorRef, IssueRef, RepositoryRef};
use workloop_core::observation::{CheckConclusion, IssueState};

use crate::credential::{CredentialError, ResolvedCredential};

/// One transport-level failure of a GitHub API call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApiError {
    /// The host definitely refused the operation before any effect
    /// landed — a definite no-effect failure, safe to record `failed`.
    Refused { reason: String },
    /// The response was lost after the request was issued: the effect
    /// may or may not have landed. Never a `failed` claim and never a
    /// silent replay.
    Unknown { reason: String },
}

/// The credential's repository permission bits — the read behind a
/// capability report's `authorization` axis. A transport that cannot
/// answer reports the read as failed; the adapter records
/// `unknown`, never a guessed grant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RepositoryPermissions {
    /// Write-level access: ref updates, change-request writes,
    /// comments, and review/thread mutations.
    pub push: bool,
    /// Read-level access: repository, branch, change-request, review,
    /// check, and issue reads.
    pub pull: bool,
}

impl Default for RepositoryPermissions {
    /// No access: a transport that cannot answer must never default
    /// to a grant.
    fn default() -> Self {
        Self {
            push: false,
            pull: false,
        }
    }
}

/// A pull request as GitHub reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemotePull {
    /// The PR's stable node/database id — the change-request identity.
    /// The display number is metadata, never an equality key.
    pub id: String,
    /// The display number (`#N`) — presentation only.
    pub display_number: u64,
    /// The exact head revision the PR currently points at.
    pub head_sha: String,
    pub source_branch: String,
    pub target_branch: String,
    pub title: String,
    pub body: String,
    /// Whether GitHub reports the PR merged. Nothing in this adapter
    /// can set it: merge is a human/policy action outside the port.
    pub merged: bool,
}

/// One check run as GitHub reports it. `head_sha` is the exact
/// revision the run examined — a run is never re-labeled onto a newer
/// head.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteCheckRun {
    pub check_run_id: String,
    pub name: String,
    pub head_sha: String,
    pub conclusion: CheckConclusion,
    /// The app identity that produced the run (display metadata for
    /// the `producer` field of a Check Observation).
    pub producer: String,
}

/// One review as GitHub reports it. `commit_sha` is the `commit_id`
/// GitHub stamped on the review — the exact revision it examined.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteReview {
    pub review_id: String,
    pub commit_sha: String,
    /// The reviewing identity GitHub attributes the review to.
    pub author: String,
    pub body: String,
    /// Whether GitHub reports the review as requesting changes. The
    /// decision itself is a blocking finding even when the review
    /// carries no inline comments.
    pub changes_requested: bool,
}

/// One review finding — an inline review comment — as GitHub reports
/// it. `commit_sha` is the exact revision the finding's review
/// examined, so a finding never migrates onto a newer head.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteReviewFinding {
    /// The host's stable comment id — the finding's identity.
    pub finding_id: String,
    /// The review the finding was submitted under.
    pub review_id: String,
    /// The exact revision the finding's review examined.
    pub commit_sha: String,
    /// The finding text — the content the occurrence digest covers.
    pub body: String,
}

/// One issue comment on a pull request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteComment {
    pub comment_id: String,
    pub body: String,
    /// The workloop marker tag the posting intent carried, echoed by
    /// the host so a lost response can be reconciled by marker.
    pub marker: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteIssue {
    pub source_revision: String,
    pub content_digest: String,
    pub state: IssueState,
    /// The issue title exactly as the host reported it — retained into
    /// the immutable snapshot body a dispatch request binds; never
    /// re-fetched by a Worker.
    pub title: String,
    /// The issue body exactly as the host reported it (empty when the
    /// host reports none).
    pub body: String,
}

/// One successfully-read issue source carried out of a refresh pass:
/// the full content a retained snapshot body is built from. The
/// composition root persists these as immutable bodies; the refresh
/// itself never writes them — it reports what it read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IssueSnapshotRead {
    pub issue: IssueRef,
    pub source_revision: String,
    pub content_digest: String,
    pub title: String,
    pub body: String,
}

/// The transport seam. A live implementation issues HTTPS calls; the
/// contract tests supply their own in-memory implementation. The
/// adapter performs no write that does not go through this trait, and
/// every read it uses for a guard or a receipt comes back through it —
/// the guard point is the API call itself, immediately before the
/// write.
pub trait GitHubApi {
    /// The repository's recorded default-branch head — the
    /// repository-level read that establishes the configured
    /// repository is reachable before any subject read.
    fn read_repository(&mut self, repository: &RepositoryRef) -> Result<Option<String>, ApiError>;

    /// The credential's permission bits on `repository` — the
    /// authoritative read behind a capability report's
    /// `authorization` axis. Every transport must answer honestly:
    /// `Err` means the bits could not be confirmed, and the caller
    /// records `unknown`, never a guessed grant.
    fn read_permissions(
        &mut self,
        repository: &RepositoryRef,
    ) -> Result<RepositoryPermissions, ApiError>;

    /// The current head of `branch`, or `None` when the ref is absent.
    fn read_branch(
        &mut self,
        repository: &RepositoryRef,
        branch: &str,
    ) -> Result<Option<String>, ApiError>;

    /// Move `branch` to `new_head` under an exact prior-ref guard:
    /// `expected_prior` (`None` = absent) must still hold when the
    /// write is issued, or the call refuses with no effect. Returns
    /// the head the write set; the adapter re-reads the ref afterward
    /// for the receipt's read-back.
    fn write_ref(
        &mut self,
        repository: &RepositoryRef,
        branch: &str,
        new_head: &str,
        expected_prior: Option<&str>,
    ) -> Result<String, ApiError>;

    /// Read one pull request by its stable id.
    fn read_pull(
        &mut self,
        repository: &RepositoryRef,
        change_id: &ChangeRequestId,
    ) -> Result<RemotePull, ApiError>;

    /// Open a pull request from `source_branch` into `target_branch`.
    fn open_pull(
        &mut self,
        repository: &RepositoryRef,
        source_branch: &str,
        target_branch: &str,
        title: &str,
        body: &str,
    ) -> Result<RemotePull, ApiError>;

    /// Update the stored metadata fields; omitted fields are never
    /// rewritten. The head is not part of this call — the adapter
    /// re-reads it immediately before calling.
    fn update_pull(
        &mut self,
        repository: &RepositoryRef,
        change_id: &ChangeRequestId,
        title: Option<&str>,
        body: Option<&str>,
    ) -> Result<RemotePull, ApiError>;

    /// Post one comment carrying `marker` and `body`; the returned
    /// comment is the host's stored read-back.
    fn comment_on_pull(
        &mut self,
        repository: &RepositoryRef,
        change_id: &ChangeRequestId,
        marker: &str,
        body: &str,
    ) -> Result<RemoteComment, ApiError>;

    /// Find a comment by its marker on one pull request — the
    /// reconciliation read for a comment whose response was lost.
    fn find_comment(
        &mut self,
        repository: &RepositoryRef,
        change_id: &ChangeRequestId,
        marker: &str,
    ) -> Result<Option<RemoteComment>, ApiError>;

    /// List the pull requests opened from `source_branch` whose head is
    /// the recorded source revision — the reconciliation read for an
    /// `open_change_request` whose response was lost. Filtering by the
    /// recorded revision, not merely the branch, prevents adopting a
    /// change request this Work never opened.
    fn list_pulls_on_branch(
        &mut self,
        repository: &RepositoryRef,
        source_branch: &str,
        head_sha: &str,
    ) -> Result<Vec<RemotePull>, ApiError>;

    /// The check runs GitHub reports for the exact `head_sha`.
    fn check_runs(
        &mut self,
        repository: &RepositoryRef,
        head_sha: &str,
    ) -> Result<Vec<RemoteCheckRun>, ApiError>;

    /// The review stream of one pull request; each review keeps the
    /// commit it examined.
    fn reviews(
        &mut self,
        repository: &RepositoryRef,
        change_id: &ChangeRequestId,
    ) -> Result<Vec<RemoteReview>, ApiError>;

    /// The review findings (inline review comments) GitHub reports for
    /// one pull request; each finding keeps the commit its review
    /// examined. This is the surface behind `review_findings_read`:
    /// when it cannot be read, the adapter records reviews with an
    /// unavailable completeness — an empty finding set is never
    /// claimed without this source answering.
    fn review_findings(
        &mut self,
        repository: &RepositoryRef,
        change_id: &ChangeRequestId,
    ) -> Result<Vec<RemoteReviewFinding>, ApiError>;

    /// Whether `descendant`'s recorded ancestry contains `ancestor` —
    /// used to classify a push read-back (`Equal`/`Ancestor`/
    /// `Diverged`).
    fn contains_revision(
        &mut self,
        repository: &RepositoryRef,
        ancestor: &str,
        descendant: &str,
    ) -> Result<bool, ApiError>;

    fn read_issue(&mut self, _issue: &IssueRef) -> Result<RemoteIssue, ApiError> {
        Err(ApiError::Refused {
            reason: "issue reads are not supported by this transport".into(),
        })
    }

    /// The host-side login the transport's ambient credential acts
    /// as — `None` when the transport cannot prove an identity. A
    /// write receipt is only ever stamped with this proven identity,
    /// never the intent's configured actor.
    fn viewer(&mut self) -> Result<Option<String>, ApiError> {
        Ok(None)
    }

    /// Install the credential the transport's calls authenticate
    /// under — the value the adapter resolved for the current intent's
    /// role binding. `None` returns to the ambient identity.
    fn set_credential(&mut self, _credential: Option<ResolvedCredential>) {}

    /// Which host actor a resolved credential authenticates as — the
    /// adapter's authentication step for `credential_ref` binding. A
    /// transport without a credential surface never authenticates.
    fn authenticate(
        &mut self,
        _credential: &ResolvedCredential,
    ) -> Result<HostActorRef, CredentialError> {
        Err(CredentialError::unresolvable(
            "this transport cannot authenticate a resolved credential",
        ))
    }

    fn set_read_deadline(&mut self, _deadline: Option<Instant>) {}
}
