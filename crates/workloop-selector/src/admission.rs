//! Proposal → Command translation (U6.2).
//!
//! The only path a proposal takes toward admission is an ordinary
//! `submit` `CommandRequest`. The translation adds no authority: the
//! request travels the same `handle_command` path as a manual `submit`,
//! so capability coverage, source overlap, duplicate-source and
//! idempotency gates apply identically. The selector holds no store
//! handle and never decides anything itself.

use workloop_core::command::{CommandBody, CommandRequest};
use workloop_core::domain::{Actor, Origin};
use workloop_core::identity::SourceSnapshot;

use crate::Proposal;

/// The replay-key namespace for selector-driven submissions. A
/// proposal-derived key can never collide with an operator-chosen or
/// generated key.
pub const REPLAY_KEY_PREFIX: &str = "proposal/";

/// The request origin every selector-driven submission carries —
/// distinct from the manual `cli` origin so a replayed proposal is
/// attributable to the admission path that produced it.
pub const SELECTOR_ORIGIN: &str = "selector";

/// Translate a proposal into the ordinary `submit` Command it is
/// admitted through.
///
/// `actor` is the installation's acting identity, stamped into the spec
/// as the approving actor exactly as for a manual submission.
///
/// Replay identity is derived, never fresh: the same proposal always
/// yields the same [`replay_key`], so a re-run replays the saved
/// decision under `(origin, actor, key)` instead of creating new Work.
/// Source bindings land in `SourceSnapshot` verbatim — the recorded
/// `source_revision`/`content_digest` stay the freshness evidence the
/// source gate compares later authoritative reads against. Declared
/// `required_permissions` are proposal/policy provenance only — they
/// shape the proposal key and record what the policy asked for, but
/// they are not a command field and are not recorded on the admitted
/// Work; capability coverage is Core's check at admission, not a
/// translation concern.
pub fn submit_command(proposal: &Proposal, actor: Actor) -> CommandRequest {
    CommandRequest {
        actor,
        origin: Origin(SELECTOR_ORIGIN.into()),
        idempotency_key: replay_key(proposal),
        body: CommandBody::Submit {
            intent: proposal.task.intent.clone(),
            sources: proposal
                .sources
                .iter()
                .map(|b| SourceSnapshot {
                    issue: b.issue.clone(),
                    source_revision: b.revision.clone(),
                    content_digest: b.content_digest.clone(),
                })
                .collect(),
            constraints: proposal.task.constraints.clone(),
            // `TaskInput` carries one verification argv; the spec
            // records a list of verification commands, so a non-empty
            // argv lands as exactly one entry.
            verification_argv: if proposal.task.verification_argv.is_empty() {
                Vec::new()
            } else {
                vec![proposal.task.verification_argv.clone()]
            },
        },
    }
}

/// The deterministic replay key for one proposal, namespaced so it can
/// never collide with an operator-chosen or generated key. `selector`
/// is part of the key, so two different selectors proposing identical
/// content cannot replay each other's decisions.
pub fn replay_key(proposal: &Proposal) -> String {
    format!("{REPLAY_KEY_PREFIX}{}:{}", proposal.selector, proposal.key)
}
