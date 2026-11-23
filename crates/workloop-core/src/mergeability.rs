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

/// The most sibling changes one merge may re-check in one pass. The
/// remainder is left for the next bounded pass rather than walking an
/// unbounded open-change list.
pub const MAX_CONFLICT_RECHECKS_PER_PASS: usize = 5;

/// How long after a recorded merge sibling re-checks stay due. The
/// window bounds the trigger latency; it is not a retry interval.
pub const CONFLICT_RECHECK_WINDOW_SECONDS: i64 = 60;

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

/// `Mergeability` is the short name the observation vocabulary uses.
pub use MergeabilityState as Mergeability;

/// Alias for callers naming the observed value rather than the state.
pub type MergeabilityObservation = MergeabilityState;

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

    /// Alias used by callers that name the identity rather than the
    /// record.
    pub fn identity(&self) -> String {
        self.episode_id()
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

/// Short aliases for callers that name the trigger by its class or by
/// the repair action.
pub type FixRequest = ConflictFixRequest;
pub type ConflictRepairRequest = ConflictFixRequest;
/// Alias for the bounded repair trigger's class.
pub type FixClassRequest = ConflictFixRequest;
/// Alias for callers naming the trigger by its effect.
pub type ConflictTrigger = ConflictFixRequest;

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

/// Build a Fix-class request directly from an episode and its paths.
pub fn fix_class_request(episode: ConflictEpisode, paths: Vec<String>) -> ConflictFixRequest {
    ConflictFixRequest::new(episode, paths)
}

/// Alias for `fix_class_request`.
pub fn conflict_fix_request(episode: ConflictEpisode, paths: Vec<String>) -> ConflictFixRequest {
    fix_class_request(episode, paths)
}

/// Alias used by callers that name the repair action.
pub fn repair_request(episode: ConflictEpisode, paths: Vec<String>) -> ConflictFixRequest {
    fix_class_request(episode, paths)
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
