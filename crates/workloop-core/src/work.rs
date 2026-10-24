//! The `work` decision core: evaluates Commands against the store snapshot
//! and emits `Effect`s the store applies in the same transaction. The store
//! enforces structure; everything here is policy — transition legality,
//! versions, budgets, and gates.

use crate::command::{
    CommandBody, CommandDecision, CommandEffect, CommandId, CommandRequest, FindingChoice,
    RejectionCode, WorkVersion, payload_digest,
};
use crate::domain::*;
use crate::observation::Outcome;
use crate::store::{Effect, ReadView, Store, StoreError, TxView};

/// Installation-level configuration supplied by the composition root.
#[derive(Debug, Clone)]
pub struct WorkloopConfig {
    /// Used for the branch reservation when a Work names no issue refs.
    pub default_repository: String,
    pub allowed_effects: AllowedEffects,
    pub budget: ExecutionBudget,
}

/// Handle one Command: idempotency check → validation → decide → apply →
/// persist the decision. Runs in a single store transaction; a constraint
/// conflict triggers one re-read and re-decide before rejection.
pub fn handle_command(
    store: &impl Store,
    config: &WorkloopConfig,
    req: &CommandRequest,
    now_unix_seconds: i64,
) -> Result<CommandDecision, StoreError> {
    let digest = payload_digest(&req.origin, &req.actor, &req.idempotency_key, &req.body);

    store.transact(|tx| {
        if let Some((stored_digest, decision)) =
            tx.stored_decision(&req.origin, &req.actor, &req.idempotency_key)?
        {
            if stored_digest == digest {
                return Ok(decision);
            }
            // The original decision keeps the bare key for replay; the
            // conflict is durable under a payload-derived key. A replay
            // of the same conflicting payload returns that stored
            // conflict decision.
            let conflict_key = format!("{}#{digest}", req.idempotency_key);
            if let Some((conflict_digest, conflict_decision)) =
                tx.stored_decision(&req.origin, &req.actor, &conflict_key)?
                && conflict_digest == digest
            {
                return Ok(conflict_decision);
            }
            let decision = rejected(
                tx,
                RejectionCode::IdempotencyConflict,
                req.body.work_id().cloned(),
                vec!["same idempotency key with a different payload".into()],
            )?;
            tx.save_decision(&req.origin, &req.actor, &conflict_key, &digest, &decision)?;
            return Ok(decision);
        }

        // Re-decide passes let a constraint conflict resolve against fresh
        // state; each attempt runs in a savepoint so partial effects are
        // rolled back before the next read.
        let mut attempts = 0;
        let decision = loop {
            attempts += 1;
            tx.begin_attempt()?;
            match decide(tx, config, req, now_unix_seconds) {
                Ok((decision, effects)) => match apply_all(tx, &effects) {
                    Ok(()) => {
                        tx.commit_attempt()?;
                        break decision;
                    }
                    Err(e) => {
                        tx.revert_attempt()?;
                        match conflict_rejection(tx, req, &e)? {
                            Some(rejection) => break rejection,
                            None if attempts < 3 => continue,
                            None => {
                                break rejected_now(
                                    tx,
                                    RejectionCode::Ineligible,
                                    req.body.work_id().cloned(),
                                    vec![e.to_string()],
                                );
                            }
                        }
                    }
                },
                Err(e) => {
                    tx.revert_attempt()?;
                    return Err(e);
                }
            }
        };

        tx.save_decision(
            &req.origin,
            &req.actor,
            &req.idempotency_key,
            &digest,
            &decision,
        )?;
        Ok(decision)
    })
}

fn apply_all(tx: &mut dyn TxView, effects: &[Effect]) -> Result<(), StoreError> {
    for e in effects {
        tx.apply(e)?;
    }
    Ok(())
}

/// Decide whether a store conflict is retryable (re-read and re-decide) or
/// a deterministic rejection.
fn conflict_rejection(
    tx: &mut dyn TxView,
    req: &CommandRequest,
    err: &StoreError,
) -> Result<Option<CommandDecision>, StoreError> {
    let code = match err {
        StoreError::VersionConflict { .. } => Some(RejectionCode::StaleWorkVersion),
        // A lost race on reservations, slots, or uniqueness: the world
        // changed under the read — re-decide once before judging.
        StoreError::ConstraintConflict { .. }
        | StoreError::StateConflict { .. }
        | StoreError::NoFreeSlot => None,
        // A transient I/O failure must not become a durable rejection under
        // the caller's key: abort the transaction and let them retry.
        StoreError::Internal { .. } => return Err(err.clone()),
    };
    match code {
        Some(c) => Ok(Some(rejected_now(
            tx,
            c,
            req.body.work_id().cloned(),
            vec![err.to_string()],
        ))),
        None => Ok(None),
    }
}

fn rejected(
    tx: &mut dyn TxView,
    code: RejectionCode,
    work_id: Option<WorkId>,
    evidence: Vec<String>,
) -> Result<CommandDecision, StoreError> {
    Ok(rejected_now(tx, code, work_id, evidence))
}

fn rejected_now(
    tx: &mut dyn TxView,
    code: RejectionCode,
    work_id: Option<WorkId>,
    evidence: Vec<String>,
) -> CommandDecision {
    let current = work_id.as_ref().and_then(|w| tx.work(w).ok().flatten());
    CommandDecision::Rejected {
        command_id: CommandId(
            tx.alloc_id("cmd")
                .unwrap_or_else(|_| "cmd-unknown".to_string()),
        ),
        reason: code,
        current_work_id: work_id,
        current_version: current.map(|w| w.version),
        evidence,
        committed_at: tx.last_seq().unwrap_or(0),
    }
}

/// The version precondition shared by every existing-Work Command.
fn check_work(
    tx: &dyn ReadView,
    work_id: &WorkId,
    expected_version: WorkVersion,
) -> Result<Result<Work, CommandDecision>, StoreError> {
    let Some(work) = tx.work(work_id)? else {
        return Ok(Err(CommandDecision::Rejected {
            command_id: CommandId("cmd-unknown".into()),
            reason: RejectionCode::InvalidRequest,
            current_work_id: Some(work_id.clone()),
            current_version: None,
            evidence: vec!["unknown work_id".into()],
            committed_at: tx.last_seq()?,
        }));
    };
    if work.version != expected_version {
        return Ok(Err(CommandDecision::Rejected {
            command_id: CommandId("cmd-unknown".into()),
            reason: RejectionCode::StaleWorkVersion,
            current_work_id: Some(work_id.clone()),
            current_version: Some(work.version),
            evidence: vec![],
            committed_at: tx.last_seq()?,
        }));
    }
    Ok(Ok(work))
}

/// The `{workspace, (repository, branch)}` pair a Work's Executions reserve.
pub fn work_resources(work: &Work, config: &WorkloopConfig) -> Vec<WriteResource> {
    let repository = work
        .issue_refs
        .first()
        .map(|r| r.repository.clone())
        .unwrap_or_else(|| config.default_repository.clone());
    vec![
        WriteResource::Workspace {
            workspace_id: format!("ws-{}", work.id.0),
        },
        WriteResource::Branch {
            repository,
            branch: format!("work/{}", work.id.0),
        },
    ]
}

type Verdict = Result<(CommandDecision, Vec<Effect>), StoreError>;

fn decide(
    tx: &mut dyn TxView,
    config: &WorkloopConfig,
    req: &CommandRequest,
    now_unix_seconds: i64,
) -> Verdict {
    match &req.body {
        CommandBody::Submit {
            intent,
            issue_refs,
            constraints,
            verification_argv,
        } => decide_submit(
            tx,
            config,
            intent,
            issue_refs,
            constraints,
            verification_argv,
        ),
        CommandBody::ClaimResources {
            resources,
            ttl_seconds,
        } => decide_claim(tx, req, resources, *ttl_seconds, now_unix_seconds),
        CommandBody::ReleaseResources { resources } => decide_release(tx, req, resources),
        CommandBody::Retry {
            work_id,
            expected_work_version,
            expected_execution_id,
        } => decide_retry(
            tx,
            config,
            work_id,
            *expected_work_version,
            expected_execution_id,
            now_unix_seconds,
        ),
        CommandBody::Resume {
            work_id,
            expected_work_version,
            execution_id,
        } => decide_resume(tx, work_id, *expected_work_version, execution_id),
        CommandBody::Cancel {
            work_id,
            expected_work_version,
            execution_id,
        } => decide_cancel(tx, work_id, *expected_work_version, execution_id.as_ref()),
        CommandBody::Acknowledge {
            work_id,
            expected_work_version,
            attention_id,
        } => decide_acknowledge(tx, req, work_id, *expected_work_version, attention_id),
        CommandBody::JudgeFindings {
            work_id,
            expected_work_version,
            expected_pr_head,
            decisions,
        } => decide_judge(
            tx,
            work_id,
            *expected_work_version,
            expected_pr_head,
            decisions,
        ),
        CommandBody::ReleaseJudgment {
            work_id,
            expected_work_version,
            expected_pr_head,
        } => decide_release_judgment(tx, work_id, *expected_work_version, expected_pr_head),
    }
}

fn decide_submit(
    tx: &mut dyn TxView,
    config: &WorkloopConfig,
    intent: &str,
    issue_refs: &[GitHubIssueRef],
    constraints: &[String],
    verification_argv: &[Vec<String>],
) -> Verdict {
    let seq = tx.alloc_seq()?;
    let work = Work {
        id: WorkId(tx.alloc_id("w")?),
        version: 1,
        revision: 1,
        intent: intent.to_string(),
        issue_refs: issue_refs.to_vec(),
        constraints: constraints.to_vec(),
        verification_argv: verification_argv.to_vec(),
        created_seq: seq,
    };
    let execution = Execution {
        id: ExecutionId(tx.alloc_id("e")?),
        work_id: work.id.clone(),
        ordinal: 0,
        state: ExecutionState::Queued,
        waiting_reason: None,
        desired_action: None,
        terminal_code: None,
        pause_reason: None,
        outcome: None,
        enqueued_seq: seq,
        allowed_effects: config.allowed_effects.clone(),
        input: InvocationInput {
            input: serde_json::json!({
                "intent": work.intent,
                "constraints": work.constraints,
                "verification_argv": work.verification_argv,
            }),
            resume: false,
            next_execution_input: None,
            previous_execution_outcome: None,
        },
        budget: config.budget.clone(),
        invocations_used: 0,
        review_rounds_used: 0,
        started_seq: None,
        started_unix_seconds: None,
        recovery_attempts_used: 0,
        recovery_deadline_unix_seconds: None,
        uncertain_since_unix_seconds: None,
    };
    let command_id = CommandId(tx.alloc_id("cmd")?);
    Ok((
        CommandDecision::Accepted {
            command_id,
            work_id: Some(work.id.clone()),
            work_version: Some(work.version),
            effect: CommandEffect::WorkCreated {
                execution_id: execution.id.clone(),
            },
            committed_at: seq,
        },
        vec![
            Effect::InsertWork { work },
            Effect::InsertExecution { execution },
        ],
    ))
}

/// An external claim names only `(repository, branch)` pairs; the managed
/// workspace root is reserved for `execution`.
fn external_claimable(res: &WriteResource) -> bool {
    matches!(res, WriteResource::Branch { .. })
}

fn decide_claim(
    tx: &mut dyn TxView,
    req: &CommandRequest,
    resources: &[WriteResource],
    ttl_seconds: u64,
    now_unix_seconds: i64,
) -> Verdict {
    if resources.is_empty() || ttl_seconds == 0 || resources.iter().any(|r| !external_claimable(r))
    {
        return Ok((
            rejected_now(tx, RejectionCode::InvalidRequest, None, vec![]),
            vec![],
        ));
    }
    let holder = ReservationHolder::External {
        actor_id: req.actor.external_id.clone(),
    };
    let expires_at = now_unix_seconds + ttl_seconds as i64;
    let mut effects = Vec::new();
    for res in resources {
        let key = res.key();
        match tx.reservation(&key)? {
            Some(existing) if existing.holder == holder => {
                // Same-actor renewal extends the TTL.
                effects.push(Effect::RenewReservation {
                    resource_key: key,
                    holder: holder.clone(),
                    expires_at_unix_seconds: expires_at,
                });
            }
            Some(existing) if !is_expired(&existing, now_unix_seconds) => {
                return Ok((
                    rejected_now(
                        tx,
                        RejectionCode::ResourceConflict,
                        None,
                        vec![serde_json::to_string(&existing).unwrap_or_default()],
                    ),
                    vec![],
                ));
            }
            Some(expired) => {
                // The expired claim has forfeited exclusion; remove it under
                // its own holder before inserting ours.
                effects.push(Effect::DeleteReservations {
                    resource_keys: vec![key.clone()],
                    holder: expired.holder.clone(),
                });
                effects.push(Effect::InsertReservation {
                    reservation: ResourceReservation {
                        resource_key: key,
                        resource: res.clone(),
                        holder: holder.clone(),
                        expires_at_unix_seconds: Some(expires_at),
                    },
                });
            }
            None => {
                effects.push(Effect::InsertReservation {
                    reservation: ResourceReservation {
                        resource_key: key,
                        resource: res.clone(),
                        holder: holder.clone(),
                        expires_at_unix_seconds: Some(expires_at),
                    },
                });
            }
        }
    }
    let command_id = CommandId(tx.alloc_id("cmd")?);
    let seq = tx.alloc_seq()?;
    Ok((
        CommandDecision::Accepted {
            command_id,
            work_id: None,
            work_version: None,
            effect: CommandEffect::ResourcesClaimed {
                expires_at: Some(expires_at),
            },
            committed_at: seq,
        },
        effects,
    ))
}

fn is_expired(res: &ResourceReservation, now_unix_seconds: i64) -> bool {
    res.expires_at_unix_seconds
        .is_some_and(|e| e <= now_unix_seconds)
}

fn decide_release(
    tx: &mut dyn TxView,
    req: &CommandRequest,
    resources: &[WriteResource],
) -> Verdict {
    if resources.iter().any(|r| !external_claimable(r)) {
        return Ok((
            rejected_now(tx, RejectionCode::InvalidRequest, None, vec![]),
            vec![],
        ));
    }
    let holder = ReservationHolder::External {
        actor_id: req.actor.external_id.clone(),
    };
    for res in resources {
        if let Some(existing) = tx.reservation(&res.key())?
            && existing.holder != holder
        {
            return Ok((
                rejected_now(
                    tx,
                    RejectionCode::Unauthorized,
                    None,
                    vec![serde_json::to_string(&existing).unwrap_or_default()],
                ),
                vec![],
            ));
        }
    }
    let command_id = CommandId(tx.alloc_id("cmd")?);
    let seq = tx.alloc_seq()?;
    Ok((
        CommandDecision::Accepted {
            command_id,
            work_id: None,
            work_version: None,
            effect: CommandEffect::ResourcesReleased,
            committed_at: seq,
        },
        vec![Effect::DeleteReservations {
            resource_keys: resources.iter().map(WriteResource::key).collect(),
            holder,
        }],
    ))
}

fn decide_retry(
    tx: &mut dyn TxView,
    config: &WorkloopConfig,
    work_id: &WorkId,
    expected_version: WorkVersion,
    expected_execution_id: &ExecutionId,
    now_unix_seconds: i64,
) -> Verdict {
    let work = match check_work(tx, work_id, expected_version)? {
        Ok(w) => w,
        Err(mut d) => {
            fix_command_id(tx, &mut d);
            return Ok((d, vec![]));
        }
    };
    let executions = tx.executions_for_work(work_id)?;
    let current = executions.last();
    match current {
        Some(e) if &e.id == expected_execution_id => {}
        _ => {
            return Ok((
                rejected_now(
                    tx,
                    RejectionCode::Ineligible,
                    Some(work_id.clone()),
                    vec!["expected_execution_id is not the current Execution".into()],
                ),
                vec![],
            ));
        }
    }
    let current = current.expect("checked above");
    if current.state != ExecutionState::Failed {
        return Ok((
            rejected_now(
                tx,
                RejectionCode::Ineligible,
                Some(work_id.clone()),
                vec!["retry requires a confirmed failed Execution".into()],
            ),
            vec![],
        ));
    }
    // Acceptance-time reservation pre-check; dispatch rechecks atomically.
    let resources = work_resources(&work, config);
    for res in &resources {
        if let Some(existing) = tx.reservation(&res.key())?
            && !is_expired(&existing, now_unix_seconds)
        {
            return Ok((
                rejected_now(
                    tx,
                    RejectionCode::ResourceConflict,
                    Some(work_id.clone()),
                    vec![serde_json::to_string(&existing).unwrap_or_default()],
                ),
                vec![],
            ));
        }
    }
    let seq = tx.alloc_seq()?;
    let execution = Execution {
        id: ExecutionId(tx.alloc_id("e")?),
        work_id: work_id.clone(),
        ordinal: current.ordinal + 1,
        state: ExecutionState::Queued,
        waiting_reason: None,
        desired_action: None,
        terminal_code: None,
        pause_reason: None,
        outcome: None,
        enqueued_seq: seq,
        allowed_effects: current.allowed_effects.clone(),
        input: InvocationInput {
            input: current.input.input.clone(),
            resume: false,
            next_execution_input: current.input.next_execution_input.clone(),
            previous_execution_outcome: current.outcome.clone(),
        },
        budget: config.budget.clone(),
        invocations_used: 0,
        review_rounds_used: 0,
        started_seq: None,
        started_unix_seconds: None,
        recovery_attempts_used: 0,
        recovery_deadline_unix_seconds: None,
        uncertain_since_unix_seconds: None,
    };
    let command_id = CommandId(tx.alloc_id("cmd")?);
    Ok((
        CommandDecision::Accepted {
            command_id,
            work_id: Some(work_id.clone()),
            work_version: Some(work.version + 1),
            effect: CommandEffect::ExecutionCreated {
                execution_id: execution.id.clone(),
            },
            committed_at: seq,
        },
        vec![
            Effect::BumpWorkVersion {
                work_id: work_id.clone(),
                expected: work.version,
                next: work.version + 1,
            },
            Effect::InsertExecution { execution },
        ],
    ))
}

fn decide_resume(
    tx: &mut dyn TxView,
    work_id: &WorkId,
    expected_version: WorkVersion,
    execution_id: &ExecutionId,
) -> Verdict {
    let work = match check_work(tx, work_id, expected_version)? {
        Ok(w) => w,
        Err(mut d) => {
            fix_command_id(tx, &mut d);
            return Ok((d, vec![]));
        }
    };
    let Some(execution) = tx.active_execution(work_id)? else {
        return Ok((
            rejected_now(
                tx,
                RejectionCode::Ineligible,
                Some(work_id.clone()),
                vec!["no active Execution to resume".into()],
            ),
            vec![],
        ));
    };
    if &execution.id != execution_id || execution.state != ExecutionState::Paused {
        return Ok((
            rejected_now(
                tx,
                RejectionCode::Ineligible,
                Some(work_id.clone()),
                vec!["resume requires the named Execution to be paused".into()],
            ),
            vec![],
        ));
    }
    let seq = tx.alloc_seq()?;
    let mut next = execution.clone();
    next.state = ExecutionState::Queued;
    next.enqueued_seq = seq;
    let command_id = CommandId(tx.alloc_id("cmd")?);
    Ok((
        CommandDecision::Accepted {
            command_id,
            work_id: Some(work_id.clone()),
            work_version: Some(work.version + 1),
            effect: CommandEffect::StateChanged,
            committed_at: seq,
        },
        vec![
            Effect::BumpWorkVersion {
                work_id: work_id.clone(),
                expected: work.version,
                next: work.version + 1,
            },
            Effect::UpdateExecution {
                execution: next,
                expected_state: ExecutionState::Paused,
            },
            Effect::ClearDeferral {
                execution_id: execution_id.clone(),
            },
        ],
    ))
}

fn decide_cancel(
    tx: &mut dyn TxView,
    work_id: &WorkId,
    expected_version: WorkVersion,
    execution_id: Option<&ExecutionId>,
) -> Verdict {
    let work = match check_work(tx, work_id, expected_version)? {
        Ok(w) => w,
        Err(mut d) => {
            fix_command_id(tx, &mut d);
            return Ok((d, vec![]));
        }
    };
    let Some(execution) = tx.active_execution(work_id)? else {
        // Nothing to cancel: an already-terminal Work gets an accepted no-op
        // with no new version.
        let command_id = CommandId(tx.alloc_id("cmd")?);
        let seq = tx.alloc_seq()?;
        return Ok((
            CommandDecision::Accepted {
                command_id,
                work_id: Some(work_id.clone()),
                work_version: Some(work.version),
                effect: CommandEffect::StateChanged,
                committed_at: seq,
            },
            vec![],
        ));
    };
    if let Some(named) = execution_id
        && named != &execution.id
    {
        return Ok((
            rejected_now(
                tx,
                RejectionCode::Ineligible,
                Some(work_id.clone()),
                vec!["execution_id is not the active Execution".into()],
            ),
            vec![],
        ));
    }
    let seq = tx.alloc_seq()?;
    let command_id = CommandId(tx.alloc_id("cmd")?);
    let bump = Effect::BumpWorkVersion {
        work_id: work_id.clone(),
        expected: work.version,
        next: work.version + 1,
    };
    let accept = |effects: Vec<Effect>| {
        // The reported version must agree with the emitted bump: an accepted
        // no-op carries no new version.
        let bumped = effects
            .iter()
            .any(|e| matches!(e, Effect::BumpWorkVersion { .. }));
        (
            CommandDecision::Accepted {
                command_id: command_id.clone(),
                work_id: Some(work_id.clone()),
                work_version: Some(work.version + u64::from(bumped)),
                effect: CommandEffect::StateChanged,
                committed_at: seq,
            },
            effects,
        )
    };
    match execution.state {
        ExecutionState::Queued => {
            let mut next = execution.clone();
            next.state = ExecutionState::Cancelled;
            next.terminal_code = Some("cancelled".into());
            next.outcome = Some(Outcome {
                state: ExecutionState::Cancelled,
                code: Some("cancelled".into()),
                detail: Some("cancelled while queued".into()),
                evidence_refs: vec![],
                recorded_seq: seq,
            });
            Ok(accept(vec![
                bump,
                Effect::UpdateExecution {
                    execution: next,
                    expected_state: ExecutionState::Queued,
                },
                Effect::ClearDeferral {
                    execution_id: execution.id.clone(),
                },
            ]))
        }
        ExecutionState::Paused => {
            // No writer can act: direct cancellation, reservation released.
            let mut next = execution.clone();
            next.state = ExecutionState::Cancelled;
            next.terminal_code = Some("cancelled".into());
            next.outcome = Some(Outcome {
                state: ExecutionState::Cancelled,
                code: Some("cancelled".into()),
                detail: Some("cancelled while paused".into()),
                evidence_refs: vec![],
                recorded_seq: seq,
            });
            Ok(accept(vec![
                bump,
                Effect::UpdateExecution {
                    execution: next,
                    expected_state: ExecutionState::Paused,
                },
                Effect::DeleteReservations {
                    resource_keys: resources_keys_of(tx, &work, &execution)?,
                    holder: ReservationHolder::Execution {
                        execution_id: execution.id.clone(),
                    },
                },
            ]))
        }
        ExecutionState::Ready | ExecutionState::Running | ExecutionState::Waiting => {
            let mut next = execution.clone();
            next.state = ExecutionState::Stopping;
            next.desired_action = Some(DesiredAction::Cancel);
            Ok(accept(vec![
                bump,
                Effect::UpdateExecution {
                    execution: next,
                    expected_state: execution.state.clone(),
                },
            ]))
        }
        ExecutionState::Uncertain => {
            if execution.desired_action == Some(DesiredAction::Continue) {
                let mut next = execution.clone();
                next.desired_action = Some(DesiredAction::Cancel);
                Ok(accept(vec![
                    bump,
                    Effect::UpdateExecution {
                        execution: next,
                        expected_state: ExecutionState::Uncertain,
                    },
                ]))
            } else {
                // Desired fail/cancel already in progress: accepted no-op.
                Ok(accept(vec![]))
            }
        }
        ExecutionState::Stopping => Ok(accept(vec![])),
        ExecutionState::Succeeded | ExecutionState::Failed | ExecutionState::Cancelled => {
            Ok(accept(vec![]))
        }
    }
}

fn resources_keys_of(
    tx: &dyn ReadView,
    _work: &Work,
    execution: &Execution,
) -> Result<Vec<String>, StoreError> {
    Ok(tx
        .reservations()?
        .into_iter()
        .filter(|r| {
            r.holder
                == ReservationHolder::Execution {
                    execution_id: execution.id.clone(),
                }
        })
        .map(|r| r.resource_key)
        .collect())
}

fn decide_acknowledge(
    tx: &mut dyn TxView,
    req: &CommandRequest,
    work_id: &WorkId,
    expected_version: WorkVersion,
    attention_id: &str,
) -> Verdict {
    let work = match check_work(tx, work_id, expected_version)? {
        Ok(w) => w,
        Err(mut d) => {
            fix_command_id(tx, &mut d);
            return Ok((d, vec![]));
        }
    };
    let seq = tx.alloc_seq()?;
    let command_id = CommandId(tx.alloc_id("cmd")?);
    Ok((
        CommandDecision::Accepted {
            command_id,
            work_id: Some(work_id.clone()),
            work_version: Some(work.version),
            effect: CommandEffect::StateChanged,
            committed_at: seq,
        },
        vec![Effect::InsertAttentionAck {
            work_id: work_id.clone(),
            attention_id: attention_id.to_string(),
            actor_id: req.actor.external_id.clone(),
        }],
    ))
}

fn decide_judge(
    tx: &mut dyn TxView,
    work_id: &WorkId,
    expected_version: WorkVersion,
    expected_pr_head: &str,
    decisions: &[crate::command::FindingDecision],
) -> Verdict {
    let work = match check_work(tx, work_id, expected_version)? {
        Ok(w) => w,
        Err(mut d) => {
            fix_command_id(tx, &mut d);
            return Ok((d, vec![]));
        }
    };
    if decisions.is_empty() {
        return Ok((
            rejected_now(
                tx,
                RejectionCode::InvalidRequest,
                Some(work_id.clone()),
                vec!["judge_findings requires at least one decision".into()],
            ),
            vec![],
        ));
    }
    let mut seen = std::collections::BTreeSet::new();
    for d in decisions {
        if !seen.insert(&d.finding_id) {
            return Ok((
                rejected_now(
                    tx,
                    RejectionCode::InvalidRequest,
                    Some(work_id.clone()),
                    vec![format!("duplicate finding_id {}", d.finding_id)],
                ),
                vec![],
            ));
        }
        if !matches!(
            d.choice,
            FindingChoice::Fix
                | FindingChoice::Dismiss
                | FindingChoice::Accept
                | FindingChoice::Wait
        ) {
            return Ok((
                rejected_now(
                    tx,
                    RejectionCode::InvalidRequest,
                    Some(work_id.clone()),
                    vec![],
                ),
                vec![],
            ));
        }
    }
    let seq = tx.alloc_seq()?;
    let command_id = CommandId(tx.alloc_id("cmd")?);
    Ok((
        CommandDecision::Accepted {
            command_id,
            work_id: Some(work_id.clone()),
            work_version: Some(work.version + 1),
            effect: CommandEffect::StateChanged,
            committed_at: seq,
        },
        vec![
            Effect::BumpWorkVersion {
                work_id: work_id.clone(),
                expected: work.version,
                next: work.version + 1,
            },
            Effect::InsertJudgment {
                work_id: work_id.clone(),
                expected_pr_head: expected_pr_head.to_string(),
                decisions: decisions.to_vec(),
            },
        ],
    ))
}

fn decide_release_judgment(
    tx: &mut dyn TxView,
    work_id: &WorkId,
    expected_version: WorkVersion,
    expected_pr_head: &str,
) -> Verdict {
    // Releases the human gate: valid on a paused Execution, requeues it.
    let work = match check_work(tx, work_id, expected_version)? {
        Ok(w) => w,
        Err(mut d) => {
            fix_command_id(tx, &mut d);
            return Ok((d, vec![]));
        }
    };
    let Some(execution) = tx.active_execution(work_id)? else {
        return Ok((
            rejected_now(
                tx,
                RejectionCode::Ineligible,
                Some(work_id.clone()),
                vec!["no active Execution".into()],
            ),
            vec![],
        ));
    };
    if execution.state != ExecutionState::Paused {
        return Ok((
            rejected_now(
                tx,
                RejectionCode::Ineligible,
                Some(work_id.clone()),
                vec!["release_judgment requires a paused Execution".into()],
            ),
            vec![],
        ));
    }
    // The gate releases only against a recorded judgment for the head the
    // caller expects; a stale-head release must not requeue the work.
    let Some((recorded_head, _decisions)) = tx.latest_judgment(work_id)? else {
        return Ok((
            rejected_now(
                tx,
                RejectionCode::Ineligible,
                Some(work_id.clone()),
                vec!["no recorded judgment to release".into()],
            ),
            vec![],
        ));
    };
    if recorded_head != expected_pr_head {
        return Ok((
            rejected_now(
                tx,
                RejectionCode::StalePrHead,
                Some(work_id.clone()),
                vec![recorded_head],
            ),
            vec![],
        ));
    }
    let seq = tx.alloc_seq()?;
    let mut next = execution.clone();
    next.state = ExecutionState::Queued;
    next.enqueued_seq = seq;
    let command_id = CommandId(tx.alloc_id("cmd")?);
    Ok((
        CommandDecision::Accepted {
            command_id,
            work_id: Some(work_id.clone()),
            work_version: Some(work.version + 1),
            effect: CommandEffect::StateChanged,
            committed_at: seq,
        },
        vec![
            Effect::BumpWorkVersion {
                work_id: work_id.clone(),
                expected: work.version,
                next: work.version + 1,
            },
            Effect::UpdateExecution {
                execution: next,
                expected_state: ExecutionState::Paused,
            },
            Effect::ClearDeferral {
                execution_id: execution.id.clone(),
            },
        ],
    ))
}

fn fix_command_id(tx: &mut dyn TxView, decision: &mut CommandDecision) {
    if let CommandDecision::Rejected { command_id, .. } = decision {
        *command_id = CommandId(
            tx.alloc_id("cmd")
                .unwrap_or_else(|_| "cmd-unknown".to_string()),
        );
    }
}

/// The dispatch-time admission: `queued → ready` in one transaction —
/// slot claim, writer-resource reservation, and the first durable request.
/// Returns the admitted Execution, or records a deferral and returns None.
pub fn admit_queued(
    tx: &mut dyn TxView,
    config: &WorkloopConfig,
    execution_id: &ExecutionId,
    now_unix_seconds: i64,
) -> Result<Option<Execution>, StoreError> {
    let Some(execution) = tx.execution(execution_id)? else {
        return Ok(None);
    };
    if execution.state != ExecutionState::Queued {
        return Ok(None);
    }
    let work = tx
        .work(&execution.work_id)?
        .ok_or_else(|| StoreError::Internal {
            detail: format!("execution {} has no work", execution.id.0),
        })?;

    // Atomic recheck: another writer may have claimed since acceptance. A
    // resumed Execution retains its own reservations; only a foreign holder
    // conflicts.
    let self_holder = ReservationHolder::Execution {
        execution_id: execution_id.clone(),
    };
    let resources = work_resources(&work, config);
    let mut held = std::collections::BTreeSet::new();
    for res in &resources {
        if let Some(existing) = tx.reservation(&res.key())? {
            if existing.holder == self_holder {
                held.insert(res.key());
                continue;
            }
            if !is_expired(&existing, now_unix_seconds) {
                tx.apply(&Effect::RecordDeferral {
                    execution_id: execution_id.clone(),
                    reason: "resource_conflict".into(),
                    diagnostic: serde_json::to_string(&existing).unwrap_or_default(),
                })?;
                return Ok(None);
            }
            // An expired claim forfeits; remove it under its own holder.
            tx.apply(&Effect::DeleteReservations {
                resource_keys: vec![res.key()],
                holder: existing.holder.clone(),
            })?;
        }
    }
    if tx.slot_holders()?.len() >= tx.slot_limit()? as usize {
        tx.apply(&Effect::RecordDeferral {
            execution_id: execution_id.clone(),
            reason: "capacity_exhausted".into(),
            diagnostic: "no free capacity slot".into(),
        })?;
        return Ok(None);
    }

    let seq = tx.alloc_seq()?;
    // The request ID derives from the next durable Invocation sequence.
    let request = InvocationRequest {
        request_id: format!("{}:inv:{}", execution.id.0, execution.invocations_used),
        work_id: work.id.clone(),
        execution_id: execution.id.clone(),
        role: InvocationRole::Coding,
        payload: serde_json::to_value(&execution.input).unwrap_or_default(),
    };
    let mut next = execution.clone();
    next.state = ExecutionState::Ready;
    next.invocations_used += 1;
    // The first dispatch anchors the runtime budget; re-admission retains it.
    if next.started_seq.is_none() {
        next.started_seq = Some(seq);
        next.started_unix_seconds = Some(now_unix_seconds);
    }
    tx.apply(&Effect::UpdateExecution {
        execution: next.clone(),
        expected_state: ExecutionState::Queued,
    })?;
    tx.apply(&Effect::ClaimSlot {
        execution_id: execution_id.clone(),
    })?;
    for res in resources {
        if held.contains(&res.key()) {
            continue;
        }
        tx.apply(&Effect::InsertReservation {
            reservation: ResourceReservation {
                resource_key: res.key(),
                resource: res,
                holder: ReservationHolder::Execution {
                    execution_id: execution_id.clone(),
                },
                expires_at_unix_seconds: None,
            },
        })?;
    }
    tx.apply(&Effect::InsertRequest { request })?;
    tx.apply(&Effect::ClearDeferral {
        execution_id: execution_id.clone(),
    })?;
    Ok(Some(next))
}
