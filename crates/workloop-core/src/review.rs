//! Review rounds and repair decisions: the Core contract connecting
//! append-only per-head review rounds, exact-set finding dispositions,
//! the two repair prerequisites, and reassessment bounded by the
//! Work-wide fix budget (`recovery.max_fix_invocations`, default 3).
//!
//! Core alone decides every transition here from supplied facts — the
//! loop drives timing and I/O, the supervisor stops and inspects, and
//! the host adapter executes effects. A `ReviewRound` is append-only
//! once recorded: `head_sha` and already-recorded findings never
//! change, and a finding observed later at the same head is appended,
//! growing the round's required decision set. A repair pass unlocks
//! only when the recorded current head equals the decisions'
//! `decided_head` AND the round's finding set is completely
//! dispositioned with at least one `Fix` selection — a pass exists to
//! repair the selected occurrences, so a round whose judgment selects
//! nothing never starts one; reaching the fix bound records
//! exhaustion and escalates rather than starting another pass.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use serde::{Deserialize, Serialize};

use crate::command::FindingChoice;
use crate::identity::CommitRevision;

/// The configuration key whose bound caps repair passes Work-wide.
pub const FIX_INVOCATION_BOUND: &str = "recovery.max_fix_invocations";

/// One finding observed inside a review round. `finding_id` is the
/// stable source identity; `content_digest` pins the observed content
/// so a same-id/different-content observation is a conflict, never an
/// edit of the recorded finding.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Finding {
    pub finding_id: String,
    #[serde(default)]
    pub content_digest: Option<String>,
}

/// An append-only review round bound to one exact head. Once recorded,
/// `head_sha` and every recorded finding are immutable: a finding
/// observed later at the same `head_sha` is appended through
/// `ReviewRepair::append_finding` and grows the round's required
/// decision set. There is at most one round per head — a re-observed
/// head reuses its round rather than installing a parallel one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewRound {
    pub round_id: String,
    pub head_sha: CommitRevision,
    findings: Vec<Finding>,
}

impl ReviewRound {
    pub fn findings(&self) -> &[Finding] {
        &self.findings
    }

    fn finding_ids(&self) -> BTreeSet<&str> {
        self.findings
            .iter()
            .map(|finding| finding.finding_id.as_str())
            .collect()
    }
}

/// One disposition submitted for one finding. A submission's finding
/// set must equal the round's current finding set exactly — omissions,
/// unknown ids, and duplicates are all invalid.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FindingDisposition {
    pub finding_id: String,
    pub disposition: FindingChoice,
}

/// The durable per-finding disposition record. Append-only: re-deciding
/// a finding appends a newer record and never rewrites the old one, so
/// disposition evidence survives later repair passes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FindingDecision {
    pub round_id: String,
    pub finding_id: String,
    pub disposition: FindingChoice,
    pub decided_head: CommitRevision,
}

/// The only two prerequisites that unlock a repair pass. Each is
/// reported independently when missing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RepairPrerequisite {
    /// The recorded current head equals the head the round's decisions
    /// were made against (`decided_head`).
    CurrentHead,
    /// The round's current finding set is completely dispositioned
    /// and selects at least one finding for repair (`Fix`). A fully
    /// dispositioned set with no `Fix` selection has no repair work.
    FindingsDecided,
}

/// Whether a round's repair prerequisites currently hold.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RepairGate {
    Unlocked,
    Blocked { missing: Vec<RepairPrerequisite> },
}

/// One authorized repair pass: a single fix invocation charged to the
/// Work-wide correction budget.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepairPass {
    /// 1-based fix-invocation ordinal within the Work.
    pub ordinal: u32,
    pub round_id: String,
    pub decided_head: CommitRevision,
}

/// The Work-wide fix bound is reached. Recorded once and kept as
/// durable evidence; every further repair attempt escalates on it —
/// another pass never starts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BoundExhaustion {
    /// The configuration key that bound the pass.
    pub bound: String,
    pub fix_invocations_used: u32,
    pub max_fix_invocations: u32,
    pub round_id: String,
    pub head_sha: CommitRevision,
}

/// The outcome of one repair attempt. `Held` and `Escalated` start no
/// pass and consume no budget.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RepairAttempt {
    Started { pass: RepairPass },
    Held { missing: Vec<RepairPrerequisite> },
    Escalated { exhaustion: BoundExhaustion },
}

/// Why Core refused a review-round or repair operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReviewError {
    UnknownRound {
        round_id: String,
    },
    /// The round id is already recorded against a different head.
    ConflictingRound {
        round_id: String,
    },
    /// The head already has a round under a different id.
    HeadAlreadyRecorded {
        head_sha: CommitRevision,
        round_id: String,
    },
    /// A finding id was re-observed with different content.
    ConflictingFinding {
        round_id: String,
        finding_id: String,
    },
    /// The decision's head is not the head the round covers. The heads
    /// are boxed so the error stays cheap to return on the hot
    /// validation path.
    HeadMismatch {
        decided_head: Box<CommitRevision>,
        round_head: Box<CommitRevision>,
    },
    /// The round's head is not the recorded current head: decisions and
    /// repair bind the current head only, so a stale round can never
    /// authorize a pass.
    StaleRound {
        round_id: String,
        round_head: CommitRevision,
    },
    /// The submitted dispositions do not equal the round's current
    /// finding set exactly.
    Coverage {
        missing: Vec<String>,
        unknown: Vec<String>,
        duplicate: Vec<String>,
    },
}

impl fmt::Display for ReviewError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownRound { round_id } => write!(f, "unknown review round {round_id}"),
            Self::ConflictingRound { round_id } => {
                write!(
                    f,
                    "review round {round_id} is already recorded on another head"
                )
            }
            Self::HeadAlreadyRecorded { head_sha, round_id } => {
                write!(f, "head {head_sha} already has review round {round_id}")
            }
            Self::ConflictingFinding {
                round_id,
                finding_id,
            } => write!(
                f,
                "finding {finding_id} in round {round_id} was re-observed with different content"
            ),
            Self::HeadMismatch {
                decided_head,
                round_head,
            } => write!(
                f,
                "decision head {decided_head} does not match round head {round_head}"
            ),
            Self::StaleRound {
                round_id,
                round_head,
            } => write!(
                f,
                "round {round_id} covers {round_head}, which is not the recorded current head"
            ),
            Self::Coverage {
                missing,
                unknown,
                duplicate,
            } => write!(
                f,
                "dispositions do not cover the round's finding set exactly \
                 (missing {missing:?}, unknown {unknown:?}, duplicate {duplicate:?})"
            ),
        }
    }
}

impl std::error::Error for ReviewError {}

/// The Core-side review/repair decision machine. It consumes only
/// supplied facts: the loop records heads and observed findings,
/// operators supply dispositions, and Core alone decides whether a
/// repair pass may start. Every record is append-only — rounds,
/// decisions, passes, and the exhaustion record are never rewritten —
/// so per-finding disposition evidence survives every later repair.
#[derive(Debug, Clone)]
pub struct ReviewRepair {
    rounds: Vec<ReviewRound>,
    decisions: Vec<FindingDecision>,
    current_head: Option<CommitRevision>,
    head_history: Vec<CommitRevision>,
    fix_invocations_used: u32,
    max_fix_invocations: u32,
    passes: Vec<RepairPass>,
    exhaustion: Option<BoundExhaustion>,
}

impl ReviewRepair {
    /// Bind the machine to the Work's frozen recovery policy. The fix
    /// bound is Work-wide and never replenishes: successor Executions,
    /// restarts, and new heads cannot reset it.
    pub fn new(max_fix_invocations: u32) -> Self {
        Self {
            rounds: Vec::new(),
            decisions: Vec::new(),
            current_head: None,
            head_history: Vec::new(),
            fix_invocations_used: 0,
            max_fix_invocations,
            passes: Vec::new(),
            exhaustion: None,
        }
    }

    /// Record an authoritative head transition decided by Core (for
    /// example the verified head a repair pass published). The first
    /// call establishes the current head; later calls move it.
    /// Decisions and repair prerequisites always evaluate against the
    /// latest record.
    pub fn record_current_head(&mut self, head: CommitRevision) {
        if self.current_head.as_ref() != Some(&head) {
            self.current_head = Some(head.clone());
            self.head_history.push(head);
        }
    }

    pub fn current_head(&self) -> Option<&CommitRevision> {
        self.current_head.as_ref()
    }

    pub fn head_history(&self) -> &[CommitRevision] {
        &self.head_history
    }

    /// Open the review round for one exact head. Re-opening the same
    /// `(round_id, head)` pair is idempotent; a round id rebound to a
    /// different head, or a second round id for an already-recorded
    /// head, is refused — there is exactly one round per head.
    pub fn open_round(
        &mut self,
        round_id: &str,
        head_sha: CommitRevision,
    ) -> Result<(), ReviewError> {
        if let Some(existing) = self.rounds.iter().find(|r| r.round_id == round_id) {
            return if existing.head_sha == head_sha {
                Ok(())
            } else {
                Err(ReviewError::ConflictingRound {
                    round_id: round_id.to_string(),
                })
            };
        }
        if let Some(existing) = self.rounds.iter().find(|r| r.head_sha == head_sha) {
            return Err(ReviewError::HeadAlreadyRecorded {
                head_sha,
                round_id: existing.round_id.clone(),
            });
        }
        self.rounds.push(ReviewRound {
            round_id: round_id.to_string(),
            head_sha,
            findings: Vec::new(),
        });
        Ok(())
    }

    /// Append a finding observed at the round's head. An identical
    /// re-observation is idempotent; the same finding id with different
    /// content is a conflict and changes nothing. Appending grows the
    /// round's required decision set — decisions recorded earlier no
    /// longer cover it.
    pub fn append_finding(&mut self, round_id: &str, finding: Finding) -> Result<(), ReviewError> {
        let round = self
            .rounds
            .iter_mut()
            .find(|r| r.round_id == round_id)
            .ok_or_else(|| ReviewError::UnknownRound {
                round_id: round_id.to_string(),
            })?;
        if let Some(existing) = round
            .findings
            .iter()
            .find(|f| f.finding_id == finding.finding_id)
        {
            return if *existing == finding {
                Ok(())
            } else {
                Err(ReviewError::ConflictingFinding {
                    round_id: round_id.to_string(),
                    finding_id: finding.finding_id,
                })
            };
        }
        round.findings.push(finding);
        Ok(())
    }

    /// Record dispositions for a round. `decided_head` must be the
    /// head the round covers AND the recorded current head — a
    /// decision for a different head, or on a stale round, is invalid.
    /// The submitted finding set must equal the round's current finding
    /// set exactly; the whole submission is refused otherwise, so a
    /// partial set is never recorded. Each disposition is stored as an
    /// append-only `FindingDecision`.
    pub fn record_decisions(
        &mut self,
        round_id: &str,
        decided_head: &CommitRevision,
        dispositions: Vec<FindingDisposition>,
    ) -> Result<(), ReviewError> {
        let round = self
            .round(round_id)
            .ok_or_else(|| ReviewError::UnknownRound {
                round_id: round_id.to_string(),
            })?;
        if decided_head != &round.head_sha {
            return Err(ReviewError::HeadMismatch {
                decided_head: Box::new(decided_head.clone()),
                round_head: Box::new(round.head_sha.clone()),
            });
        }
        if self.current_head.as_ref() != Some(&round.head_sha) {
            return Err(ReviewError::StaleRound {
                round_id: round_id.to_string(),
                round_head: round.head_sha.clone(),
            });
        }
        let expected = round.finding_ids();
        let mut submitted = BTreeSet::new();
        let mut unknown = BTreeSet::new();
        let mut duplicate = BTreeSet::new();
        for disposition in &dispositions {
            if !expected.contains(disposition.finding_id.as_str()) {
                unknown.insert(disposition.finding_id.clone());
                continue;
            }
            if !submitted.insert(disposition.finding_id.as_str()) {
                duplicate.insert(disposition.finding_id.clone());
            }
        }
        let missing: Vec<String> = expected
            .difference(&submitted)
            .map(|id| id.to_string())
            .collect();
        if !(missing.is_empty() && unknown.is_empty() && duplicate.is_empty()) {
            return Err(ReviewError::Coverage {
                missing,
                unknown: unknown.into_iter().collect(),
                duplicate: duplicate.into_iter().collect(),
            });
        }
        for disposition in dispositions {
            self.decisions.push(FindingDecision {
                round_id: round_id.to_string(),
                finding_id: disposition.finding_id,
                disposition: disposition.disposition,
                decided_head: decided_head.clone(),
            });
        }
        Ok(())
    }

    /// Evaluate the round's repair prerequisites. Unlocked requires
    /// both: the recorded current head equals the round's `decided_head`
    /// and the round's current finding set is completely dispositioned
    /// with at least one `Fix` selection. A pass exists to repair the
    /// selected occurrences, so a round whose judgment selects nothing —
    /// all `Dismiss`/`Accept`, all `Wait`, or no findings at all —
    /// stays blocked: decided non-fix findings settle without a pass
    /// and unresolved `Wait` findings route to Attention, neither of
    /// which consumes fix budget.
    pub fn repair_gate(&self, round_id: &str) -> Result<RepairGate, ReviewError> {
        let round = self
            .round(round_id)
            .ok_or_else(|| ReviewError::UnknownRound {
                round_id: round_id.to_string(),
            })?;
        let mut missing = Vec::new();
        if self.current_head.as_ref() != Some(&round.head_sha) {
            missing.push(RepairPrerequisite::CurrentHead);
        }
        let dispositions = self.dispositions_for(round_id);
        let coverage_complete = round
            .findings()
            .iter()
            .all(|finding| dispositions.contains_key(&finding.finding_id));
        let fix_selected = round
            .findings()
            .iter()
            .any(|finding| dispositions.get(&finding.finding_id) == Some(&FindingChoice::Fix));
        if !(coverage_complete && fix_selected) {
            missing.push(RepairPrerequisite::FindingsDecided);
        }
        Ok(if missing.is_empty() {
            RepairGate::Unlocked
        } else {
            RepairGate::Blocked { missing }
        })
    }

    /// Attempt to start a repair pass. A blocked gate holds without
    /// charging the budget; a reached fix bound records exhaustion and
    /// escalates; only an unlocked gate under the bound starts a pass
    /// and consumes one Work-wide fix invocation.
    pub fn begin_repair(&mut self, round_id: &str) -> Result<RepairAttempt, ReviewError> {
        let head_sha = self
            .round(round_id)
            .ok_or_else(|| ReviewError::UnknownRound {
                round_id: round_id.to_string(),
            })?
            .head_sha
            .clone();
        match self.repair_gate(round_id)? {
            RepairGate::Blocked { missing } => return Ok(RepairAttempt::Held { missing }),
            RepairGate::Unlocked => {}
        }
        if self.fix_invocations_used >= self.max_fix_invocations {
            if self.exhaustion.is_none() {
                self.exhaustion = Some(BoundExhaustion {
                    bound: FIX_INVOCATION_BOUND.to_string(),
                    fix_invocations_used: self.fix_invocations_used,
                    max_fix_invocations: self.max_fix_invocations,
                    round_id: round_id.to_string(),
                    head_sha: head_sha.clone(),
                });
            }
            return Ok(RepairAttempt::Escalated {
                exhaustion: self.exhaustion.clone().expect("exhaustion just recorded"),
            });
        }
        self.fix_invocations_used += 1;
        let pass = RepairPass {
            ordinal: self.fix_invocations_used,
            round_id: round_id.to_string(),
            decided_head: head_sha,
        };
        self.passes.push(pass.clone());
        Ok(RepairAttempt::Started { pass })
    }

    pub fn round(&self, round_id: &str) -> Option<&ReviewRound> {
        self.rounds.iter().find(|r| r.round_id == round_id)
    }

    pub fn round_for_head(&self, head_sha: &CommitRevision) -> Option<&ReviewRound> {
        self.rounds.iter().find(|r| &r.head_sha == head_sha)
    }

    pub fn rounds(&self) -> &[ReviewRound] {
        &self.rounds
    }

    /// Every recorded disposition, append-only — re-decisions add
    /// records rather than rewriting earlier ones.
    pub fn decisions(&self) -> &[FindingDecision] {
        &self.decisions
    }

    /// The latest recorded disposition per finding of one round.
    pub fn dispositions_for(&self, round_id: &str) -> BTreeMap<String, FindingChoice> {
        let mut latest = BTreeMap::new();
        for decision in self.decisions.iter().filter(|d| d.round_id == round_id) {
            latest.insert(decision.finding_id.clone(), decision.disposition.clone());
        }
        latest
    }

    pub fn passes(&self) -> &[RepairPass] {
        &self.passes
    }

    pub fn fix_invocations_used(&self) -> u32 {
        self.fix_invocations_used
    }

    pub fn max_fix_invocations(&self) -> u32 {
        self.max_fix_invocations
    }

    pub fn exhaustion(&self) -> Option<&BoundExhaustion> {
        self.exhaustion.as_ref()
    }
}
