//! Mergeability observation and bounded conflict repair triggers.
//!
//! A merge moves the target branch under every other open change. The
//! remaining managed changes must be re-checked, and a change that is
//! now conflicted needs exactly one bounded repair trigger bound to the
//! exact pair that produced the conflict: the conflicted source head and
//! the new target revision. Repeated scans of the same pair are the same
//! episode and must never mint a second trigger.
//!
//! This module owns the identity and request shape; adapters observe
//! mergeability and Core decides whether a trigger can be produced or
//! must escalate to human Attention.

use serde::{Deserialize, Serialize};

use crate::domain::InvocationRole;
use crate::domain::{canonical_json, sha256_hex};
use crate::identity::{ChangeRequestRef, CommitRevision, DiffTarget};

/// The most sibling changes one pass may re-check. The bound is over
/// the whole pass, not per merge: every merge scanned in a pass shares
/// it, so accumulated merge history cannot multiply the remote reads.
/// The remainder is left for the next bounded pass rather than walking
/// an unbounded open-change list — and the next pass does reach it:
/// unrecorded pairs and provisional `Unknown` verdicts are checked
/// before any settled re-read, so a steady open set cannot starve the
/// deferred tail.
pub const MAX_CONFLICT_RECHECKS_PER_PASS: usize = 5;

/// The most target branches one pass may scan for post-merge sibling
/// conflicts. Every historical merge on the same branch re-reads the
/// same fresh base and the same open siblings, so the scan is collapsed
/// per target and additionally bounded here. The remainder waits for a
/// later pass.
pub const MAX_MERGE_RECHECKS_PER_PASS: usize = 5;

/// The most merge-detection reads one pass may issue for recorded
/// changes that have no recorded merge. The bound is its own, not
/// shared with the target scans: sustained merge activity cannot
/// starve detection, and a backlog of unmerged recorded changes —
/// terminal Works included, whose change can still merge — cannot
/// starve the scans. A never-checked pair always sorts before a
/// checked one, so the whole backlog is reached within
/// ceil(N/bound) passes; the ordering key is second-granular
/// observed time, so within one second the leading checked set is
/// preferred until the key's second advances.
pub const MAX_MERGE_CHECKS_PER_PASS: usize = 5;

const EPISODE_TAG: &str = "conflict-episode-v1";

/// The host's mergeability verdict for one change at one exact pair.
/// `Unknown` is not a conflict and never mints a repair; an unreadable
/// or asynchronous host answer is reported as unknown, never guessed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MergeabilityState {
    Mergeable,
    Conflicting,
    Unknown,
}

/// One conflict episode: repository, change, source head, and target
/// revision. Both revisions participate in identity — a new target tip
/// on the same head is a new episode, while a steady conflicted pair is
/// the same episode.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ConflictEpisode {
    pub change: ChangeRequestRef,
    pub head: CommitRevision,
    pub base: CommitRevision,
}

impl ConflictEpisode {
    /// Construct an episode and fail closed when either revision belongs
    /// to another repository than the change.
    pub fn new(
        change: ChangeRequestRef,
        head: CommitRevision,
        base: CommitRevision,
    ) -> Result<Self, String> {
        if head.repository != change.repository {
            return Err("conflict head is not in the change's repository".into());
        }
        if base.repository != change.repository {
            return Err("conflict base is not in the change's repository".into());
        }
        Ok(Self { change, head, base })
    }

    /// Derive the episode from a diff target. The new base is the
    /// target revision; the recorded diff base is preferred when both
    /// are present because that is the exact pair the host compared.
    pub fn from_target(target: &DiffTarget) -> Result<Self, String> {
        let base = target
            .base
            .clone()
            .or_else(|| target.target_revision.clone())
            .ok_or_else(|| "conflict target carries no base revision".to_string())?;
        Self::new(target.change.clone(), target.head.clone(), base)
    }

    /// The conflicted source revision.
    pub fn source_revision(&self) -> &CommitRevision {
        &self.head
    }

    /// The new target revision the conflict was observed against.
    pub fn target_revision(&self) -> &CommitRevision {
        &self.base
    }

    /// Stable episode identity over the full `(repository, change, head,
    /// base)` tuple. Display numbers are presentation metadata and take
    /// no part.
    pub fn episode_id(&self) -> String {
        sha256_hex(
            EPISODE_TAG,
            &canonical_json(&serde_json::json!({
                "change": {
                    "repository": self.change.repository,
                    "change_request": self.change.change_request,
                },
                "head": self.head,
                "base": self.base,
            })),
        )
    }
}

/// The bounded repair trigger: a Fix-class request bound to both the
/// new target revision and the conflicted head. It carries the exact
/// pair into the request payload so a worker never repairs a moved
/// branch against stale evidence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConflictFixRequest {
    pub episode: ConflictEpisode,
    /// The files the host reported as conflicting. May be empty when the
    /// host gives only a verdict; the request still binds both revisions.
    pub paths: Vec<String>,
    /// The exact task payload. `source_revision`/`target_revision` are
    /// also carried as top-level aliases for consumers that read the
    /// payload directly.
    pub payload: serde_json::Value,
}

impl ConflictFixRequest {
    pub fn new(episode: ConflictEpisode, paths: Vec<String>) -> Self {
        let payload = conflict_payload(&episode, &paths);
        Self {
            episode,
            paths,
            payload,
        }
    }

    /// A conflict repair always runs as the bounded Fix role.
    pub fn role(&self) -> InvocationRole {
        InvocationRole::Fix
    }

    pub fn source_revision(&self) -> &CommitRevision {
        self.episode.source_revision()
    }

    pub fn target_revision(&self) -> &CommitRevision {
        self.episode.target_revision()
    }

    pub fn episode_id(&self) -> String {
        self.episode.episode_id()
    }
}

/// Why a conflict could not be turned into a repair request. The
/// conflicting paths are retained even when neither revision could be
/// established, so the human sees what the host did report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConflictEscalation {
    #[serde(default)]
    pub episode: Option<ConflictEpisode>,
    pub reason: String,
    #[serde(default)]
    pub paths: Vec<String>,
}

/// The complete decision for one observed mergeability state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConflictDisposition {
    /// The host reports no conflict (or an unknown answer that is not
    /// actionable).
    NotConflicting,
    /// A bounded Fix request bound to the exact pair.
    Repair(ConflictFixRequest),
    /// A conflict exists but no automatic trigger may be minted; the
    /// paths and reason go to human Attention.
    Escalate(ConflictEscalation),
}

/// Classify a mergeability observation with optional revision evidence.
///
/// - `Mergeable`/`Unknown` produces no trigger.
/// - `Conflicting` with both revisions produces the bounded Fix request.
/// - `Conflicting` without a usable pair escalates with its paths.
pub fn plan_conflict(
    target: Option<&DiffTarget>,
    state: MergeabilityState,
    paths: &[String],
    reason: Option<&str>,
) -> ConflictDisposition {
    if state != MergeabilityState::Conflicting {
        return ConflictDisposition::NotConflicting;
    }
    match target.and_then(|t| ConflictEpisode::from_target(t).ok()) {
        Some(episode) => {
            ConflictDisposition::Repair(ConflictFixRequest::new(episode, paths.to_vec()))
        }
        None => ConflictDisposition::Escalate(ConflictEscalation {
            episode: None,
            reason: reason
                .filter(|r| !r.is_empty())
                .unwrap_or("conflict could not be bound to a source and target revision")
                .to_string(),
            paths: paths.to_vec(),
        }),
    }
}

/// The canonical conflict task payload. Both naming conventions are
/// emitted so a consumer reading the wire object or a normalized field
/// binds the same pair.
pub fn conflict_payload(episode: &ConflictEpisode, paths: &[String]) -> serde_json::Value {
    serde_json::json!({
        "kind": "conflict_repair",
        "schema_version": 1,
        "episode_id": episode.episode_id(),
        "repository": episode.change.repository,
        "change_request": episode.change.change_request,
        "display_number": episode.change.display_number,
        "source_revision": episode.head,
        "target_revision": episode.base,
        "head": episode.head,
        "base": episode.base,
        "conflicting_paths": paths,
    })
}
