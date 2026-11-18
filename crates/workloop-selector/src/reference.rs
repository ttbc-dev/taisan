//! The packaged reference selection policy (#214): the deterministic
//! rule the `public-issue-selector` executable applies over a
//! [`Collection`]. It is deliberately small — open Issues carrying
//! the configured labels, inside the requested repository scope,
//! minus the request's exclusions — and it invents no semantics the
//! v0.1 proposal contract does not already carry.
//!
//! Ranking is fully specified so two runs over identical facts always
//! emit identical candidates in identical order:
//!
//! 1. `created_unix_seconds` ascending — oldest first;
//! 2. `display_number` ascending — Issue number order; issues with no
//!    display number sort after numbered ones (`u64::MAX`);
//! 3. scoped identity (`IssueRef` order: host, repository, issue id)
//!    — the documented tie-break across repositories.
//!
//! At most `max_candidates` one-Issue proposals are emitted; the
//! remainder is counted as truncated, never silently dropped.

use std::collections::BTreeSet;

use workloop_core::identity::AdapterId;

use crate::collect::{CollectedIssue, Collection};
use crate::protocol::{self, ParsedRequest};
use crate::{
    IssueState, Proposal, Provenance, SkipReason, SkippedIssue, SourceBinding, TaskInput, excluded,
    proposal_key,
};

/// The outcome of one reference selection: proposals in rank order
/// plus the typed accounting every non-proposed fact gets.
#[derive(Debug, Default)]
pub struct ReferenceSelection {
    pub proposals: Vec<Proposal>,
    /// Facts refused before ranking, with their bounded reason.
    pub skipped: Vec<SkippedIssue>,
    /// Eligible facts cut by `max_candidates`.
    pub truncated: usize,
}

/// Apply the reference policy to one completed collection.
///
/// `required_labels` is the executable's configured label filter:
/// empty means no label requirement; otherwise an Issue must carry
/// **every** configured label to be eligible. `fetched_unix_seconds`
/// is provenance only — it never enters proposal identity.
pub fn select_reference(
    collection: &Collection,
    request: &ParsedRequest,
    selector: &AdapterId,
    required_labels: &BTreeSet<String>,
    fetched_unix_seconds: Option<i64>,
) -> ReferenceSelection {
    let scope: BTreeSet<_> = request.repositories.iter().collect();
    let excluded_set: BTreeSet<_> = request.excluded_sources.iter().cloned().collect();

    let mut skipped = Vec::new();
    let mut eligible: Vec<&CollectedIssue> = Vec::new();
    for fact in &collection.issues {
        let reason = if !scope.contains(&fact.issue.repository) {
            Some(SkipReason::NotAllowlisted)
        } else if excluded(&excluded_set, &fact.issue) {
            Some(SkipReason::Excluded)
        } else if fact.state != IssueState::Open {
            Some(SkipReason::NotOpen)
        } else if !required_labels.is_subset(&fact.labels) {
            Some(SkipReason::MissingLabels)
        } else if !conforming_intent(&fact.title) {
            // The title becomes the candidate intent verbatim; a title
            // that cannot satisfy the wire rule would make the whole
            // response invalid, so the fact is refused instead.
            Some(SkipReason::InvalidIntent)
        } else {
            None
        };
        match reason {
            Some(reason) => skipped.push(SkippedIssue {
                issue: fact.issue.clone(),
                reason,
            }),
            None => eligible.push(fact),
        }
    }

    // Deterministic rank: creation time, then Issue number (unnumbered
    // last), then scoped identity across repositories.
    eligible.sort_by(|a, b| {
        a.created_unix_seconds
            .cmp(&b.created_unix_seconds)
            .then_with(|| {
                a.issue
                    .display_number
                    .unwrap_or(u64::MAX)
                    .cmp(&b.issue.display_number.unwrap_or(u64::MAX))
            })
            .then_with(|| a.issue.cmp(&b.issue))
    });

    let limit = request.max_candidates as usize;
    let truncated = eligible.len().saturating_sub(limit);
    let mut proposals = Vec::with_capacity(eligible.len().min(limit));
    for fact in eligible.into_iter().take(limit) {
        proposals.push(proposal(selector, request, fact, fetched_unix_seconds));
    }
    skipped.sort_by(|a, b| a.issue.cmp(&b.issue));
    ReferenceSelection {
        proposals,
        skipped,
        truncated,
    }
}

/// Whether `title` can serve verbatim as a candidate `task.intent`
/// under the wire contract: the schema's string shape plus the
/// intent-specific non-whitespace rule `validate_response` enforces.
fn conforming_intent(title: &str) -> bool {
    protocol::is_wire_string(title, protocol::MAX_INTENT_BYTES)
        && title.chars().any(|c| !c.is_whitespace())
}

fn proposal(
    selector: &AdapterId,
    request: &ParsedRequest,
    fact: &CollectedIssue,
    fetched_unix_seconds: Option<i64>,
) -> Proposal {
    let task = TaskInput {
        intent: fact.title.clone(),
        constraints: request.task_constraints.clone(),
        verification_argv: request.verification_argv.clone(),
    };
    let source = SourceBinding {
        issue: fact.issue.clone(),
        revision: fact.revision.clone(),
        content_digest: fact.content_digest.clone(),
    };
    let required_permissions = request.required_permissions.clone();
    let key = proposal_key(
        selector.as_str(),
        request.selector_version,
        request.policy_revision,
        std::slice::from_ref(&source),
        &task,
        &required_permissions,
    );
    Proposal {
        key,
        selector: selector.clone(),
        selector_version: request.selector_version,
        policy_revision: request.policy_revision,
        sources: vec![source],
        task,
        required_permissions,
        provenance: Provenance {
            // Stateless: the reference adapter keeps no cursor and no
            // acceptance ledger — every run re-reads the full scope.
            snapshot_cursor: None,
            fetched_unix_seconds,
        },
    }
}
