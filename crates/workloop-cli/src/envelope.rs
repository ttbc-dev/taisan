//! The always-JSON result envelope from `docs/execution-loop.md#cli-contract`.
//! Every command prints exactly one of these on stdout; human diagnostics
//! stay on stderr. `status` is the closed vocabulary
//! `accepted | rejected | unknown | error`; queries never emit `unknown`.
//! `schema_version` is pinned to `"1"` — a consumer refuses a newer
//! version rather than parsing optimistically.

use workloop_core::command::{
    CommandBody, CommandDecision, CommandEffect, CommandRequest, RejectionCode,
};
use workloop_core::domain::{Actor, EventSequence};
use workloop_core::projection::{QueryResult, SourceStatus};
use workloop_core::store::StoreError;

use serde::Serialize;

/// Exit code classes per the CLI contract: 0 ok, 1 rejected, 2 usage,
/// 3 config, 4 store, 5 source unavailable, 6 unknown.
pub const EXIT_OK: i32 = 0;
pub const EXIT_REJECTED: i32 = 1;
pub const EXIT_USAGE: i32 = 2;
pub const EXIT_CONFIG: i32 = 3;
pub const EXIT_STORE: i32 = 4;
pub const EXIT_SOURCE: i32 = 5;
pub const EXIT_UNKNOWN: i32 = 6;

/// Reason/detail text is bounded so a provider stack trace or a fat
/// config error can never bloat or corrupt the contract surface.
const MESSAGE_BOUND: usize = 240;
const EVIDENCE_BOUND: usize = 16;
const EVIDENCE_ITEM_BOUND: usize = 240;

fn bounded(s: &str, limit: usize) -> String {
    if s.chars().count() <= limit {
        return s.to_string();
    }
    let mut out: String = s.chars().take(limit.saturating_sub(1)).collect();
    out.push('…');
    out
}

#[derive(Debug, Clone, Serialize)]
pub struct CommandIdentity {
    pub id: Option<String>,
    pub kind: String,
    pub actor: String,
    pub idempotency_key: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Reason {
    pub code: String,
    pub message: String,
}

/// The one object printed per result. Core members are always present —
/// `command`, `work_id`, `execution_id`, `result`, and `reason` are
/// `null` when they do not apply; `sources`/`as_of` appear on completed
/// queries only.
#[derive(Debug, Clone, Serialize)]
pub struct Envelope {
    pub schema_version: &'static str,
    pub status: &'static str,
    pub command: Option<CommandIdentity>,
    pub work_id: Option<String>,
    pub execution_id: Option<String>,
    pub result: Option<serde_json::Value>,
    pub reason: Option<Reason>,
    pub evidence_refs: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sources: Option<Vec<SourceStatus>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub as_of: Option<EventSequence>,
}

fn base(status: &'static str) -> Envelope {
    Envelope {
        schema_version: "1",
        status,
        command: None,
        work_id: None,
        execution_id: None,
        result: None,
        reason: None,
        evidence_refs: vec![],
        sources: None,
        as_of: None,
    }
}

/// The acting identity as a stable `kind:external_id` label.
pub fn actor_label(actor: &Actor) -> String {
    let kind = serde_json::to_value(&actor.kind)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_else(|| "actor".into());
    format!("{kind}:{}", actor.external_id)
}

/// The Command kind for the envelope's `command.kind`.
pub fn command_kind(body: &CommandBody) -> &'static str {
    match body {
        CommandBody::Submit { .. } => "submit",
        CommandBody::ClaimResources { .. } => "claim_resources",
        CommandBody::ReleaseResources { .. } => "release_resources",
        CommandBody::Retry { .. } => "retry",
        CommandBody::Resume { .. } => "resume",
        CommandBody::Cancel { .. } => "cancel",
        CommandBody::Acknowledge { .. } => "acknowledge",
        CommandBody::JudgeFindings { .. } => "judge_findings",
        CommandBody::ReleaseJudgment { .. } => "release_judgment",
    }
}

/// The caller's replay identity, present on every Command result —
/// including errors — so a lost response can always be recovered by
/// `(origin, actor, idempotency_key)`.
pub fn request_identity(req: &CommandRequest) -> CommandIdentity {
    CommandIdentity {
        id: None,
        kind: command_kind(&req.body).into(),
        actor: actor_label(&req.actor),
        idempotency_key: Some(req.idempotency_key.clone()),
    }
}

/// A query carries no mutation identity; `kind` names the read.
pub fn query_identity(kind: &str, actor: &Actor, key: Option<&str>) -> CommandIdentity {
    CommandIdentity {
        id: None,
        kind: kind.into(),
        actor: actor_label(actor),
        idempotency_key: key.map(str::to_string),
    }
}

fn rejection_exit(code: &RejectionCode) -> i32 {
    match code {
        // The failure class wins over the rejection class: the caller
        // must distinguish an unavailable declared source from a domain
        // refusal without scraping prose.
        RejectionCode::SourceUnavailable => EXIT_SOURCE,
        _ => EXIT_REJECTED,
    }
}

fn decision_envelope(req: &CommandRequest, decision: &CommandDecision) -> (Envelope, i32) {
    let mut command = request_identity(req);
    match decision {
        CommandDecision::Accepted {
            command_id,
            work_id,
            effect,
            ..
        } => {
            command.id = Some(command_id.0.clone());
            let mut e = base("accepted");
            e.command = Some(command);
            e.work_id = work_id.as_ref().map(|w| w.0.clone());
            e.execution_id = match effect {
                CommandEffect::WorkCreated { execution_id }
                | CommandEffect::ExecutionCreated { execution_id } => Some(execution_id.0.clone()),
                _ => None,
            };
            e.result = serde_json::to_value(effect).ok();
            (e, EXIT_OK)
        }
        CommandDecision::Rejected {
            command_id,
            reason,
            current_work_id,
            evidence,
            ..
        } => {
            command.id = Some(command_id.0.clone());
            let mut e = base("rejected");
            e.command = Some(command);
            e.work_id = current_work_id.as_ref().map(|w| w.0.clone());
            let code = serde_json::to_value(reason)
                .ok()
                .and_then(|v| v.as_str().map(str::to_string))
                .unwrap_or_else(|| format!("{reason:?}").to_lowercase());
            e.reason = Some(Reason {
                message: evidence
                    .first()
                    .map(|s| bounded(s, MESSAGE_BOUND))
                    .unwrap_or_else(|| code.clone()),
                code,
            });
            e.evidence_refs = evidence
                .iter()
                .take(EVIDENCE_BOUND)
                .map(|s| bounded(s, EVIDENCE_ITEM_BOUND))
                .collect();
            (e, rejection_exit(reason))
        }
    }
}

/// Map a completed Query to its envelope. `work_id` names the Work the
/// Query targeted, when it names one.
pub fn query_envelope<T: Serialize>(
    result: &QueryResult<T>,
    identity: CommandIdentity,
    work_id: Option<String>,
) -> (Envelope, i32) {
    match result {
        QueryResult::Found {
            value,
            as_of,
            sources,
        } => {
            let mut e = base("accepted");
            e.command = Some(identity);
            e.work_id = work_id;
            e.result = serde_json::to_value(value).ok();
            e.sources = Some(sources.clone());
            e.as_of = Some(*as_of);
            (e, EXIT_OK)
        }
        // A not-found item is a completed Query — accepted with a null
        // result, distinct from an unreadable store or a refused read.
        QueryResult::NotFound => {
            let mut e = base("accepted");
            e.command = Some(identity);
            e.work_id = work_id;
            (e, EXIT_OK)
        }
        QueryResult::Forbidden => {
            let mut e = base("error");
            e.command = Some(identity);
            e.work_id = work_id;
            e.reason = Some(Reason {
                code: "forbidden".into(),
                message: "the query was refused by policy".into(),
            });
            (e, EXIT_REJECTED)
        }
        QueryResult::StoreUnavailable => {
            let mut e = base("error");
            e.command = Some(identity);
            e.work_id = work_id;
            e.reason = Some(Reason {
                code: "store_unavailable".into(),
                message: "the store could not be read".into(),
            });
            (e, EXIT_STORE)
        }
    }
}

/// The error classes the CLI itself can raise. `Unknown` is reserved for
/// a mutation whose durable fate could not be determined.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorClass {
    Usage,
    Config,
    Store,
}

impl ErrorClass {
    fn exit(self) -> i32 {
        match self {
            Self::Usage => EXIT_USAGE,
            Self::Config => EXIT_CONFIG,
            Self::Store => EXIT_STORE,
        }
    }

    fn code(self) -> &'static str {
        match self {
            Self::Usage => "usage",
            Self::Config => "config",
            Self::Store => "store",
        }
    }
}

/// A failure where nothing was recorded as accepted. The `command`
/// identity stays attached whenever the caller's replay identity is
/// known — recovery of a lost response keys on it.
pub fn error_envelope(
    identity: Option<CommandIdentity>,
    class: ErrorClass,
    message: &str,
) -> (Envelope, i32) {
    let mut e = base("error");
    e.command = identity;
    e.reason = Some(Reason {
        code: class.code().into(),
        message: bounded(message, MESSAGE_BOUND),
    });
    (e, class.exit())
}

/// The mutation's fate cannot be determined. Recovery is `command-result`
/// by the original `(origin, actor, idempotency_key)` or an identical
/// replay — never a fresh key or a rewritten payload.
pub fn unknown_envelope(identity: CommandIdentity, detail: &str) -> (Envelope, i32) {
    let mut e = base("unknown");
    e.command = Some(identity);
    e.reason = Some(Reason {
        code: "unknown".into(),
        message: bounded(detail, MESSAGE_BOUND),
    });
    (e, EXIT_UNKNOWN)
}

/// A mutation decision with the lost-response path applied: a handler
/// error is followed by a `command_result` lookup under the caller's
/// replay key. A recorded decision is emitted as the original result —
/// replaying the durable decision *is* the recovery here; a
/// definitively absent decision is a plain store-class error; and an
/// unreadable lookup result stays `unknown`. The TUI session differs
/// deliberately: it stages a found-but-unread decision as `unknown`
/// until the user runs `recover`, where the CLI emits it directly.
pub fn mutation_envelope(
    req: &CommandRequest,
    outcome: &Result<CommandDecision, StoreError>,
    lookup: impl FnOnce() -> Result<QueryResult<CommandDecision>, StoreError>,
) -> (Envelope, i32) {
    match outcome {
        Ok(decision) => decision_envelope(req, decision),
        Err(store_error) => match lookup() {
            Ok(QueryResult::Found { value, .. }) => decision_envelope(req, &value),
            // The attempt definitely did not commit: a local fault.
            Ok(QueryResult::NotFound) => error_envelope(
                Some(request_identity(req)),
                ErrorClass::Store,
                &store_error.to_string(),
            ),
            _ => unknown_envelope(
                request_identity(req),
                &format!(
                    "mutation fate undetermined ({store_error}); recover with \
                     command-result --origin <origin> --replay-key {}",
                    req.idempotency_key
                ),
            ),
        },
    }
}

/// Store open and transaction failures map to the store class; a legacy
/// or unversioned file is a store refusal, not a config error.
pub fn store_error(e: &StoreError) -> (Envelope, i32) {
    error_envelope(None, ErrorClass::Store, &e.to_string())
}

/// A one-line human diagnostic for stderr. stdout stays envelope-only.
pub fn diagnostic(e: &Envelope) -> Option<String> {
    e.reason
        .as_ref()
        .map(|r| format!("taisan: {}: {}", r.code, r.message))
}

#[cfg(test)]
mod tests {
    use super::*;
    use workloop_core::command::{CommandBody, CommandId};
    use workloop_core::domain::{Actor, ActorKind, Origin, WorkId};

    fn req(key: &str) -> CommandRequest {
        CommandRequest {
            actor: Actor {
                kind: ActorKind::Human,
                external_id: "actor-x".into(),
            },
            origin: Origin("cli".into()),
            idempotency_key: key.into(),
            body: CommandBody::Submit {
                intent: "i".into(),
                sources: vec![],
                constraints: vec![],
                verification_argv: vec![],
            },
        }
    }

    fn store_error() -> StoreError {
        StoreError::Internal {
            detail: "disk gone".into(),
        }
    }

    #[test]
    fn lost_response_with_unreadable_decision_stays_unknown() {
        let r = req("k1");
        let (env, code) = mutation_envelope(&r, &Err(store_error()), || Err(store_error()));
        assert_eq!(code, EXIT_UNKNOWN);
        assert_eq!(env.status, "unknown");
        assert_eq!(env.reason.as_ref().unwrap().code, "unknown");
        let cmd = env.command.as_ref().unwrap();
        assert_eq!(cmd.idempotency_key.as_deref(), Some("k1"));
        assert_eq!(env.schema_version, "1");
    }

    #[test]
    fn lost_response_with_absent_decision_is_a_plain_error() {
        let r = req("k1");
        let (env, code) = mutation_envelope(&r, &Err(store_error()), || Ok(QueryResult::NotFound));
        assert_eq!(code, EXIT_STORE);
        assert_eq!(env.status, "error");
        assert_eq!(env.reason.as_ref().unwrap().code, "store");
    }

    #[test]
    fn lost_response_with_recorded_decision_replays_it() {
        let r = req("k1");
        let recorded = CommandDecision::Accepted {
            command_id: CommandId("cmd-1".into()),
            work_id: Some(WorkId("w-1".into())),
            work_version: Some(1),
            effect: CommandEffect::WorkCreated {
                execution_id: workloop_core::domain::ExecutionId("e-1".into()),
            },
            committed_at: 7,
        };
        let found = recorded.clone();
        let (env, code) = mutation_envelope(&r, &Err(store_error()), || {
            Ok(QueryResult::Found {
                value: found,
                as_of: 7,
                sources: vec![],
            })
        });
        assert_eq!(code, EXIT_OK);
        assert_eq!(env.status, "accepted");
        assert_eq!(env.command.as_ref().unwrap().id.as_deref(), Some("cmd-1"));
    }

    #[test]
    fn reason_messages_are_bounded() {
        let huge = "x".repeat(MESSAGE_BOUND * 4);
        let (env, _) = error_envelope(None, ErrorClass::Store, &huge);
        let message = &env.reason.as_ref().unwrap().message;
        assert!(message.chars().count() <= MESSAGE_BOUND);
        assert!(message.ends_with('…'));
    }

    #[test]
    fn rejected_exit_codes() {
        assert_eq!(
            rejection_exit(&RejectionCode::StaleWorkVersion),
            EXIT_REJECTED
        );
        assert_eq!(
            rejection_exit(&RejectionCode::IdempotencyConflict),
            EXIT_REJECTED
        );
        assert_eq!(
            rejection_exit(&RejectionCode::SourceUnavailable),
            EXIT_SOURCE
        );
    }
}
