//! Stage contracts: what a stage *means*, independent of the executor
//! that runs it. An executor cell — local, remote, or a future provider —
//! selects where a stage runs, never what it may skip. The rules below
//! were distilled from two production failure modes in the source
//! workflow (tracked here as issue #21): a converge stage that could
//! report success while its fix commits never reached the PR, and a
//! reviewer axis silently dropped for bot-authored PRs. The incident
//! analysis lives in `docs/source-workflow-audit.md`; the regression
//! scenarios are pinned by `tests/stage_contract.rs`.

use serde::{Deserialize, Serialize};

/// The structured result of a `converge` stage run. The executor returns
/// this record instead of a free-text verdict; the coordinator verifies
/// `pushed_sha` against the live PR head before adopting the result.
/// Publication is part of the stage's postcondition, exactly as
/// `propose` already models it for ship: bounded fix commits published to
/// the PR head, marked replies posted, draft→ready advanced exactly once.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConvergeRecord {
    /// The PR head at stage entry.
    pub head_before: String,
    /// The PR head the executor claims to have produced. It may exist
    /// only in the executor's workspace — a claim, not publication.
    pub head_after: String,
    /// The head the executor pushed toward the PR, if any. `None` means
    /// nothing was published: a fix claim without publication evidence.
    pub pushed_sha: Option<String>,
    /// Marked reply comment IDs the run posted on the PR threads.
    pub replies_posted: Vec<String>,
    /// Review thread IDs the run claims to have addressed.
    pub threads_addressed: Vec<String>,
}

/// How a `converge` record classifies against the live PR head.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ConvergeClassification {
    /// Publication verified: the pushed head is the produced head
    /// (`head_after`) and is the live PR head or an ancestor of it. The
    /// only classification that may count as `review-fixed`.
    Fixed { pushed_sha: String },
    /// Nothing was produced or claimed: the head did not move and
    /// nothing was pushed.
    NothingToFix,
    /// A fix was claimed but publication cannot be shown or does not
    /// verify against the live head. A failure that preserves the whole
    /// record as evidence — never a success.
    UnpublishedFailure {
        live_head: String,
        record: ConvergeRecord,
    },
}

impl ConvergeRecord {
    /// Classify against the live PR head. `pushed_is_ancestor_of_live`
    /// must come from the coordinator's own verification (GitHub compare
    /// or an ancestry read) of `self.pushed_sha`, never from the
    /// executor's claim. A result counts as fixed only when the produced
    /// head was the one published (`pushed_sha == head_after`) and that
    /// pushed head verifies against the live PR head — equal to it or an
    /// ancestor of it. Any other fix claim, including a push that
    /// carried a different head than the fix produced, is an
    /// `UnpublishedFailure` carrying the full record as evidence.
    pub fn classify(
        &self,
        live_head: &str,
        pushed_is_ancestor_of_live: bool,
    ) -> ConvergeClassification {
        // Nothing produced: the head did not move. A no-op push of that
        // unchanged head is still nothing to fix, not a published fix.
        if self.head_after == self.head_before {
            return match self.pushed_sha.as_deref() {
                None => ConvergeClassification::NothingToFix,
                Some(pushed) if pushed == self.head_after => ConvergeClassification::NothingToFix,
                Some(_) => ConvergeClassification::UnpublishedFailure {
                    live_head: live_head.to_string(),
                    record: self.clone(),
                },
            };
        }
        // A produced fix must be the head that was actually published.
        // Reporting a pushed SHA that is not the produced head means the
        // produced commits never reached the PR — even when that SHA
        // verifies against the live head (observed failure: the push
        // carried the old head, the fix stayed in the workspace).
        match self.pushed_sha.as_deref() {
            Some(pushed)
                if pushed == self.head_after
                    && (pushed == live_head || pushed_is_ancestor_of_live) =>
            {
                ConvergeClassification::Fixed {
                    pushed_sha: pushed.to_string(),
                }
            }
            // A new head claimed with no pushed SHA, a mismatched SHA,
            // or a push that does not verify against the live head: a
            // failure that preserves the whole record, never a success.
            // An ancestry flag about a push that does not carry the
            // produced head cannot manufacture one.
            _ => ConvergeClassification::UnpublishedFailure {
                live_head: live_head.to_string(),
                record: self.clone(),
            },
        }
    }
}

/// Where a stage runs. Choosing an executor never changes a stage's
/// postconditions — only its location.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Executor {
    Local,
    Remote,
}

/// The reviewer axis of the executor × reviewer matrix. Orthogonal to
/// `Executor`: an executor cell must never silently narrow it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Reviewer {
    Devin,
    Shadow,
    Both,
}

/// Why a configuration-time matrix check refused a cell. Refusal is the
/// only allowed outcome for an unsupported combination — never a silent
/// degradation of the reviewer axis.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum MatrixConfigError {
    UnsupportedExecutor {
        value: String,
    },
    UnsupportedReviewer {
        value: String,
    },
    UnsupportedCombination {
        executor: Executor,
        reviewer: Reviewer,
    },
}

/// The matrix cells configuration validation accepts today. Support is
/// declared here: a cell absent from this table is refused at
/// configuration-validation time rather than run with a degraded
/// reviewer axis. All six executor × reviewer cells are supported
/// because the adapter event-filter contract exempts lane-produced
/// bot-authored PRs and heads — a filter that drops them starves the
/// lane's own review loop (the observed silent-reviewer failure).
const SUPPORTED_MATRIX_CELLS: [(Executor, Reviewer); 6] = [
    (Executor::Local, Reviewer::Devin),
    (Executor::Local, Reviewer::Shadow),
    (Executor::Local, Reviewer::Both),
    (Executor::Remote, Reviewer::Devin),
    (Executor::Remote, Reviewer::Shadow),
    (Executor::Remote, Reviewer::Both),
];

/// The configuration-validation chokepoint for the executor × reviewer
/// matrix. It parses both axes and refuses any combination not declared
/// in `SUPPORTED_MATRIX_CELLS`. On success it returns the reviewer axis
/// exactly as configured: no executor cell may rewrite or default it,
/// and an unknown value is a configuration error rather than a fallback
/// (a remote executor must never silently downgrade shadow review).
pub fn validate_review_matrix(
    executor: &str,
    reviewer: &str,
) -> Result<(Executor, Reviewer), MatrixConfigError> {
    let executor = match executor {
        "local" => Executor::Local,
        "remote" => Executor::Remote,
        value => {
            return Err(MatrixConfigError::UnsupportedExecutor {
                value: value.to_string(),
            });
        }
    };
    let reviewer = match reviewer {
        "devin" => Reviewer::Devin,
        "shadow" => Reviewer::Shadow,
        "both" => Reviewer::Both,
        value => {
            return Err(MatrixConfigError::UnsupportedReviewer {
                value: value.to_string(),
            });
        }
    };
    if SUPPORTED_MATRIX_CELLS.contains(&(executor, reviewer)) {
        Ok((executor, reviewer))
    } else {
        Err(MatrixConfigError::UnsupportedCombination { executor, reviewer })
    }
}
