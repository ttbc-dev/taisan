//! Stage contracts: what a stage command means, independent of *where* it
//! runs. An executor cell selects where a stage executes — local, remote,
//! or a future provider — and never which postconditions it may skip.
//!
//! These types encode the executor-independence invariants recorded in
//! `docs/decisions.md` and the Command contract in `docs/architecture.md`:
//!
//! - converge success classification requires publication evidence —
//!   [`ConvergeRecord::classify`] adopts `ReviewFixed` only when the pushed
//!   head verifies against the live PR head;
//! - the executor and reviewer axes are orthogonal — a [`LaneSpec`] exists
//!   only through [`LaneMatrix::validate`], which refuses unsupported
//!   combinations at configuration time instead of silently degrading
//!   either axis.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::command::PrHeadSha;

/// Where a stage runs. An executor cell never redefines what the stage's
/// postconditions are.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutorKind {
    Local,
    Remote,
}

/// Which review path a stage uses — an axis orthogonal to the executor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewerKind {
    Devin,
    Shadow,
    Both,
}

/// A validated executor×reviewer combination. Its fields are private and
/// it does not derive `Deserialize`: the only construction path is
/// [`LaneMatrix::validate`], so an unsupported cell is refused at
/// configuration validation and can never be loaded silently.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LaneSpec {
    executor: ExecutorKind,
    reviewer: ReviewerKind,
}

impl LaneSpec {
    pub fn executor(&self) -> ExecutorKind {
        self.executor
    }
    pub fn reviewer(&self) -> ReviewerKind {
        self.reviewer
    }
}

/// The executor×reviewer cells an installation declares as supported.
/// Each entry must have a real implementation behind it — the contract
/// forbids silently degrading one axis because of the other.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LaneMatrix {
    supported: BTreeSet<(ExecutorKind, ReviewerKind)>,
}

/// Configuration-validation refusal for an undeclared cell.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnsupportedCombination {
    pub executor: ExecutorKind,
    pub reviewer: ReviewerKind,
}

impl std::fmt::Display for UnsupportedCombination {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "unsupported executor×reviewer combination: {:?}×{:?}",
            self.executor, self.reviewer
        )
    }
}

impl std::error::Error for UnsupportedCombination {}

impl LaneMatrix {
    pub fn new(cells: impl IntoIterator<Item = (ExecutorKind, ReviewerKind)>) -> Self {
        Self {
            supported: cells.into_iter().collect(),
        }
    }

    /// Configuration validation: a supported cell yields its `LaneSpec`;
    /// an unsupported one is refused — never silently degraded.
    pub fn validate(
        &self,
        executor: ExecutorKind,
        reviewer: ReviewerKind,
    ) -> Result<LaneSpec, UnsupportedCombination> {
        if self.supported.contains(&(executor, reviewer)) {
            Ok(LaneSpec { executor, reviewer })
        } else {
            Err(UnsupportedCombination { executor, reviewer })
        }
    }
}

/// A marked reply the executor posted to the PR while converging.
pub type ReplyRef = String;
/// A review thread the converge pass addressed.
pub type ThreadRef = String;

/// What a converge executor returns instead of a free-text verdict. The
/// record is the stage's postcondition made auditable: publication is part
/// of `converge`, exactly as `propose` models publication for ship — so
/// the structured heads, the pushed SHA, and the reply/thread bookkeeping
/// are required fields, not narrative.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConvergeRecord {
    /// The PR head the converge pass started from.
    pub head_before: PrHeadSha,
    /// The local head the pass ended on — its produced fix commits.
    pub head_after: PrHeadSha,
    /// The head actually pushed to the PR branch. `None` means nothing was
    /// pushed: either no fix was needed, or — with
    /// `head_after != head_before` — produced commits that never reached
    /// the PR, which is a failure, not a success.
    pub pushed_sha: Option<PrHeadSha>,
    /// Marked replies posted to the PR while converging.
    pub replies_posted: Vec<ReplyRef>,
    /// Review threads the pass addressed.
    pub threads_addressed: Vec<ThreadRef>,
}

/// How the coordinator classifies a converge record against the live PR.
/// `ReviewFixed` requires publication evidence; a claimed-but-unpublished
/// fix is a failure that preserves the record as evidence, never a
/// success.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConvergeClassification {
    /// `pushed_sha` equals the live PR head or is an ancestor of it.
    ReviewFixed,
    /// The record's pushed or produced head cannot be verified on the PR.
    PublicationUnverified,
    /// No fix was produced and nothing was pushed.
    NothingToFix,
}

impl ConvergeRecord {
    /// Classify the record against the *live* PR head. `pushed_is_ancestor`
    /// is the GitHub-integration port's answer to "is `pushed_sha` an
    /// ancestor of `live_head`" — the core is I/O-free and never infers
    /// ancestry from the record's own fields alone.
    pub fn classify(
        &self,
        live_head: &PrHeadSha,
        pushed_is_ancestor: bool,
    ) -> ConvergeClassification {
        match &self.pushed_sha {
            Some(sha) if sha == live_head || pushed_is_ancestor => {
                ConvergeClassification::ReviewFixed
            }
            Some(_) => ConvergeClassification::PublicationUnverified,
            None if self.head_after != self.head_before => {
                ConvergeClassification::PublicationUnverified
            }
            None => ConvergeClassification::NothingToFix,
        }
    }
}
