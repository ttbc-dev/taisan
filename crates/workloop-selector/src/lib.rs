//! Reference Selector: deterministic single-issue proposals from a
//! snapshot + policy (U6.1, `docs/adapter-boundaries.md` Selector
//! contract, `docs/identity-storage.md` proposal identity).
//!
//! The selector is a pure function over its inputs. It receives an
//! explicit repository allowlist, a selection policy, a snapshot/cursor
//! of available source facts, and returns **proposals** — never
//! commands. A proposal does not reserve a slot, authorize a worker, or
//! create Work; Core validates shape, scope, freshness, idempotency,
//! and overlap at admission through the normal Command path. A
//! selector that returns zero proposals — or is absent entirely —
//! blocks nothing: manual `submit` and existing Work do not depend on
//! this crate, which holds no store handle, clock, or I/O.
//!
//! One Issue per Work: each proposal binds exactly one source Issue.
//! Rich grouping, triage, and scheduling are deferred by design.
//!
//! Proposal identity is a deterministic digest of the canonical
//! semantic payload — selector id/version, policy revision, sorted
//! stable source references with their revisions and content digests,
//! and the semantic task input. Bookkeeping is excluded: snapshot
//! cursors, fetch/run timestamps, display numbers, and mutable running
//! labels never enter the key, so the same snapshot/policy always
//! yields the same proposal identity and new Issue content yields a
//! new one.

pub mod admission;
pub mod collect;
pub mod protocol;
pub mod reference;
pub mod strict_json;

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use workloop_core::domain::{canonical_json, sha256_hex};
use workloop_core::identity::{AdapterId, IssueRef, RepositoryRef};

/// The semantic version this reference selector stamps on its
/// proposals — part of proposal identity, so a semantic change to what
/// a proposal *means* produces new identities.
pub const SELECTOR_VERSION: u32 = 1;

/// Digest tag prefixing every canonical proposal payload.
const PROPOSAL_TAG: &str = "proposal-v1";

/// One source fact in the snapshot: the selector's only view of an
/// Issue. `revision` and `content_digest` are adapter-supplied stable
/// values identifying the exact read; they ride the proposal so a
/// later admission check can detect a stale source.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IssueFact {
    pub issue: IssueRef,
    pub state: IssueState,
    /// The host-supplied source revision this fact was read at.
    pub revision: String,
    /// Digest of the Issue content this fact was read from.
    pub content_digest: String,
    /// The semantic task headline; part of the proposed intent.
    pub title: String,
}

/// Eligibility-relevant lifecycle state of a source Issue.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IssueState {
    Open,
    Closed,
}

/// The canonical Issue-content digest: `sha256` of `title\nbody` under
/// the `issue-content-v1` tag. Every boundary that binds an Issue read
/// into a [`IssueFact::content_digest`], a proposal `SourceBinding`, or
/// a spec `SourceSnapshot` — and every adapter that performs the
/// authoritative re-read the source gate compares against — must use
/// this one function. Two different digest rules over the same Issue
/// make every selector-admitted Work look `Changed` at the gate.
/// `revision` (the host's own update marker) and lifecycle `state`
/// travel separately and never enter the content digest.
pub fn issue_content_digest(title: &str, body: &str) -> String {
    sha256_hex("issue-content-v1", format!("{title}\n{body}").as_bytes())
}

/// The snapshot/cursor of available source facts a selector run sees.
/// `cursor` and `fetched_unix_seconds` are bookkeeping: they are carried
/// into proposal provenance but never into proposal identity.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SelectorSnapshot {
    pub issues: Vec<IssueFact>,
    /// Adapter bookkeeping cursor for incremental scans.
    pub cursor: Option<String>,
    /// When the snapshot was fetched — provenance only.
    pub fetched_unix_seconds: Option<i64>,
}

/// The selection policy a run is bounded by. `revision` identifies the
/// exact policy document and is part of proposal identity; a policy
/// change produces new identities even over an identical snapshot.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SelectionPolicy {
    /// Stable revision of this policy document.
    pub revision: u64,
    /// Repositories the selector may propose from. Facts outside the
    /// allowlist are refused — never quietly absorbed.
    pub allowlist: BTreeSet<RepositoryRef>,
    /// Issues excluded from selection regardless of eligibility. An
    /// entry matches a fact by issue identity or — for `repo:number`
    /// entries whose number the adapter does not use as its opaque
    /// IssueId — by the repository-scoped display number. Display
    /// numbers still take no part in identity comparison; this is a
    /// policy filter over identity, not a change to identity.
    pub excluded: BTreeSet<IssueRef>,
    /// Bound on emitted proposals per run.
    pub max_proposals: usize,
    /// Constraints every proposal's task input carries.
    pub task_constraints: BTreeSet<String>,
    /// Verification requirements attached to every task input.
    pub verification_argv: Vec<String>,
    /// Permissions the resulting Work will need — declared on the
    /// proposal, never expanded beyond this list.
    pub required_permissions: BTreeSet<String>,
}

/// A stable source reference bound into a proposal: the Issue's scoped
/// identity plus the exact read the proposal was built from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceBinding {
    pub issue: IssueRef,
    pub revision: String,
    pub content_digest: String,
}

/// The semantic task input of a proposal — what admission would carry
/// into a `submit` intent if the proposal is accepted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskInput {
    pub intent: String,
    pub constraints: Vec<String>,
    pub verification_argv: Vec<String>,
}

/// Where a proposal came from. Provenance is bookkeeping — carried for
/// audit, never part of `key`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Provenance {
    pub snapshot_cursor: Option<String>,
    pub fetched_unix_seconds: Option<i64>,
}

/// A single-issue proposal. `key` is the deterministic proposal
/// identity; replayed proposals with the same key re-present the same
/// admission decision downstream.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Proposal {
    /// Deterministic digest of the canonical semantic payload.
    pub key: String,
    pub selector: AdapterId,
    pub selector_version: u32,
    pub policy_revision: u64,
    /// Stable source references, sorted; exactly one for the reference
    /// single-issue selector.
    pub sources: Vec<SourceBinding>,
    pub task: TaskInput,
    /// Declared permissions the resulting Work will need.
    pub required_permissions: Vec<String>,
    pub provenance: Provenance,
}

/// Why a snapshot fact produced no proposal — a bounded vocabulary, so
/// a refusal is explicit data, not silence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkipReason {
    /// The fact's repository is outside the configured allowlist.
    NotAllowlisted,
    /// The Issue is excluded by policy.
    Excluded,
    /// The Issue is not open.
    NotOpen,
    /// Two snapshot facts claim the same Issue identity with different
    /// content — an inconsistent read, never silently picked between.
    ConflictingFacts,
    /// The Issue was eligible, but the run's `max_proposals` bound cut
    /// it from this run's output — truncation is explicit data, never
    /// indistinguishable from absence from the snapshot.
    TruncatedByBound,
    /// The Issue is open and in scope but does not carry every label
    /// the adapter's configured label filter requires (#214).
    MissingLabels,
    /// The fact's title cannot form a protocol-conforming intent —
    /// empty, whitespace-only, control-bearing, or over the intent
    /// byte bound — so emitting a candidate for it would produce a
    /// response `validate_response` must reject wholesale.
    InvalidIntent,
    /// The fact's issue ref cannot form a protocol-conforming wire
    /// source — currently a `display_number` of `0`, which the schema
    /// requires to be positive or absent — so emitting a candidate
    /// for it would produce a response `validate_response` must
    /// reject wholesale.
    InvalidIssueRef,
}

/// One snapshot fact that produced no proposal, with its reason.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkippedIssue {
    pub issue: IssueRef,
    pub reason: SkipReason,
}

/// The deterministic output of one selector run: proposals in stable
/// sorted order plus the typed reasons every other fact was refused.
/// `truncated` counts eligible candidates dropped by the proposal
/// bound — a bounded run must say so, never pass silently for "no
/// more work".
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProposalSet {
    pub proposals: Vec<Proposal>,
    pub skipped: Vec<SkippedIssue>,
    /// Eligible candidates cut by `policy.max_proposals`.
    #[serde(default)]
    pub truncated: usize,
}

/// Run the reference selector: one proposal per eligible Issue, in
/// deterministic `IssueRef` order, bounded by `policy.max_proposals`.
/// Eligible Issues past the bound are reported in `skipped` with
/// `SkipReason::TruncatedByBound` — a truncated run never passes
/// silently for "the snapshot held nothing more".
///
/// Pure and total — no store, clock, or I/O; an empty or all-refused
/// snapshot yields an empty `proposals`, never an error.
pub fn propose(
    selector: &AdapterId,
    snapshot: &SelectorSnapshot,
    policy: &SelectionPolicy,
) -> ProposalSet {
    // Dedup by Issue identity: identical repeated facts collapse; two
    // facts claiming one identity with different content conflict.
    let mut facts: BTreeMap<&IssueRef, &IssueFact> = BTreeMap::new();
    let mut conflicts: BTreeSet<&IssueRef> = BTreeSet::new();
    for fact in &snapshot.issues {
        // A recorded conflict is a property of the snapshot read, not of
        // pairwise order: once two facts disagree on an identity, no
        // later matching fact can re-admit it.
        if conflicts.contains(&fact.issue) {
            continue;
        }
        match facts.get(&fact.issue) {
            None => {
                facts.insert(&fact.issue, fact);
            }
            Some(existing) if *existing == fact => {}
            Some(_) => {
                facts.remove(&fact.issue);
                conflicts.insert(&fact.issue);
            }
        }
    }

    let mut skipped: Vec<SkippedIssue> = conflicts
        .iter()
        .map(|issue| SkippedIssue {
            issue: (*issue).clone(),
            reason: SkipReason::ConflictingFacts,
        })
        .collect();
    let mut eligible: Vec<&&IssueFact> = Vec::new();
    for (issue, fact) in &facts {
        let reason = if !policy.allowlist.contains(&issue.repository) {
            Some(SkipReason::NotAllowlisted)
        } else if excluded(&policy.excluded, issue) {
            Some(SkipReason::Excluded)
        } else if fact.state != IssueState::Open {
            Some(SkipReason::NotOpen)
        } else {
            None
        };
        match reason {
            Some(reason) => skipped.push(SkippedIssue {
                issue: (*issue).clone(),
                reason,
            }),
            None => eligible.push(fact),
        }
    }

    // Deterministic order (facts are BTreeMap-ordered by IssueRef) and
    // a deterministic bound: truncation never reshuffles, every
    // eligible Issue past the bound is reported in `skipped` — a
    // consumer can always tell a bound-dropped Issue from an absent
    // one — and the dropped count is reported rather than lost.
    let truncated = eligible.len().saturating_sub(policy.max_proposals);
    let mut proposals = Vec::with_capacity(eligible.len().min(policy.max_proposals));
    for (index, fact) in eligible.into_iter().enumerate() {
        if index < policy.max_proposals {
            proposals.push(proposal(selector, fact, snapshot, policy));
        } else {
            skipped.push(SkippedIssue {
                issue: fact.issue.clone(),
                reason: SkipReason::TruncatedByBound,
            });
        }
    }
    skipped.sort_by(|a, b| a.issue.cmp(&b.issue));
    ProposalSet {
        proposals,
        skipped,
        truncated,
    }
}

/// Whether policy excludes this fact's issue. An entry matches by
/// issue identity, or — when both sides carry a display number in the
/// same repository — by that display number: a `repo:number` policy
/// entry names the number an operator sees, which an adapter may not
/// use as its opaque IssueId. Display numbers never enter identity
/// comparison; this is a policy filter, and it never matches two
/// facts to one entry across repositories or on absent metadata.
pub(crate) fn excluded(excluded: &BTreeSet<IssueRef>, issue: &IssueRef) -> bool {
    excluded.contains(issue)
        || excluded.iter().any(|entry| {
            entry.repository == issue.repository
                && matches!(
                    (entry.display_number, issue.display_number),
                    (Some(a), Some(b)) if a == b
                )
        })
}

fn proposal(
    selector: &AdapterId,
    fact: &IssueFact,
    snapshot: &SelectorSnapshot,
    policy: &SelectionPolicy,
) -> Proposal {
    let task = TaskInput {
        intent: fact.title.clone(),
        constraints: policy.task_constraints.iter().cloned().collect(),
        verification_argv: policy.verification_argv.clone(),
    };
    let source = SourceBinding {
        issue: fact.issue.clone(),
        revision: fact.revision.clone(),
        content_digest: fact.content_digest.clone(),
    };
    let required_permissions: Vec<String> = policy.required_permissions.iter().cloned().collect();
    let key = proposal_key(
        selector.as_str(),
        SELECTOR_VERSION,
        policy.revision,
        std::slice::from_ref(&source),
        &task,
        &required_permissions,
    );
    Proposal {
        key,
        selector: selector.clone(),
        selector_version: SELECTOR_VERSION,
        policy_revision: policy.revision,
        sources: vec![source],
        task,
        required_permissions,
        provenance: Provenance {
            snapshot_cursor: snapshot.cursor.clone(),
            fetched_unix_seconds: snapshot.fetched_unix_seconds,
        },
    }
}

/// The deterministic digest a candidate's `key` must equal, computed
/// over the canonical semantic payload of exactly the fields the wire
/// contract carries. Exposed so both sides of the protocol — the
/// emitting adapter and the validating caller — recompute identity
/// from the same definition rather than trusting the wire value.
pub fn proposal_key(
    selector: &str,
    selector_version: u32,
    policy_revision: u64,
    sources: &[SourceBinding],
    task: &TaskInput,
    required_permissions: &[String],
) -> String {
    sha256_hex(
        PROPOSAL_TAG,
        &canonical_json(&semantic_payload(
            selector,
            selector_version,
            policy_revision,
            sources,
            task,
            required_permissions,
        )),
    )
}

/// The canonical semantic object a proposal key digests — the
/// `proposal-v1` payload itself. Callers persisting candidate
/// provenance store these canonical bytes as `payload` and the
/// [`proposal_key`] digest as `payload_digest`.
pub fn proposal_semantic_json(
    selector: &str,
    selector_version: u32,
    policy_revision: u64,
    sources: &[SourceBinding],
    task: &TaskInput,
    required_permissions: &[String],
) -> serde_json::Value {
    semantic_payload(
        selector,
        selector_version,
        policy_revision,
        sources,
        task,
        required_permissions,
    )
}

/// The canonical semantic fields a proposal key digests. Everything
/// bookkeeping — cursors, fetch/run timestamps, display numbers,
/// mutable labels — is absent by construction: it is never written
/// into this payload.
fn semantic_payload(
    selector: &str,
    selector_version: u32,
    policy_revision: u64,
    sources: &[SourceBinding],
    task: &TaskInput,
    required_permissions: &[String],
) -> serde_json::Value {
    serde_json::json!({
        "schema": PROPOSAL_TAG,
        "selector": selector,
        "selector_version": selector_version,
        "policy_revision": policy_revision,
        "sources": sources.iter().map(|s| {
            serde_json::json!({
                "host": s.issue.repository.host.as_str(),
                "repository": s.issue.repository.repository.as_str(),
                "issue": s.issue.issue.as_str(),
                "revision": s.revision,
                "content_digest": s.content_digest,
            })
        }).collect::<Vec<_>>(),
        "task": {
            "intent": task.intent,
            "constraints": task.constraints,
            "verification_argv": task.verification_argv,
        },
        "required_permissions": required_permissions,
    })
}
