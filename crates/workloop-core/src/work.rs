//! The `work` decision core: evaluates Commands against the store snapshot
//! and emits `Effect`s the store applies in the same transaction. The store
//! enforces structure; everything here is policy — transition legality,
//! versions, budgets, and gates.

use crate::command::{
    CommandBody, CommandDecision, CommandEffect, CommandId, CommandRequest, FindingChoice,
    RejectionCode, WorkVersion, payload_digest,
};
use crate::domain::*;
use crate::observation::{
    ActionResult, ArtifactFact, CheckConclusion, Observation, ObservationFact, Outcome,
    ProcessStateFact, ReviewVerdict,
};
use crate::store::{Effect, PrActionIntent, PrActionStatus, ReadView, Store, StoreError, TxView};

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
        match tx.stored_decision(&req.origin, &req.actor, &req.idempotency_key)? {
            Some((stored_digest, decision)) => {
                if stored_digest == digest {
                    return Ok(decision);
                }
                let decision = rejected(
                    tx,
                    RejectionCode::IdempotencyConflict,
                    req.body.work_id().cloned(),
                    vec!["same idempotency key with a different payload".into()],
                )?;
                // The original decision keeps the bare key for replay; the
                // conflict is durable under a payload-derived key.
                let conflict_key = format!("{}#{digest}", req.idempotency_key);
                tx.save_decision(
                    &req.origin,
                    &req.actor,
                    &conflict_key,
                    &digest,
                    &decision,
                )?;
                return Ok(decision);
            }
            None => {}
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
                                )
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

        tx.save_decision(&req.origin, &req.actor, &req.idempotency_key, &digest, &decision)?;
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
        StoreError::Internal { .. } => None,
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
    let current = work_id
        .as_ref()
        .and_then(|w| tx.work(w).ok().flatten());
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
        } => decide_submit(tx, config, intent, issue_refs, constraints, verification_argv),
        CommandBody::ClaimResources {
            resources,
            ttl_seconds,
        } => decide_claim(tx, req, resources, *ttl_seconds, now_unix_seconds),
        CommandBody::ReleaseResources { resources } => decide_release(tx, req, resources),
        CommandBody::Retry {
            work_id,
            expected_work_version,
            expected_execution_id,
        } => decide_retry(tx, config, work_id, *expected_work_version, expected_execution_id, now_unix_seconds),
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
        } => decide_judge(tx, work_id, *expected_work_version, expected_pr_head, decisions),
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

fn decide_claim(
    tx: &mut dyn TxView,
    req: &CommandRequest,
    resources: &[WriteResource],
    ttl_seconds: u64,
    now_unix_seconds: i64,
) -> Verdict {
    if resources.is_empty() || ttl_seconds == 0 {
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
    res.expires_at_unix_seconds.is_some_and(|e| e <= now_unix_seconds)
}

fn decide_release(
    tx: &mut dyn TxView,
    req: &CommandRequest,
    resources: &[WriteResource],
) -> Verdict {
    let holder = ReservationHolder::External {
        actor_id: req.actor.external_id.clone(),
    };
    for res in resources {
        if let Some(existing) = tx.reservation(&res.key())? {
            if existing.holder != holder {
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
        if let Some(existing) = tx.reservation(&res.key())? {
            if !is_expired(&existing, now_unix_seconds) {
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
            rejected_now(tx, RejectionCode::Ineligible, Some(work_id.clone()), vec![
                "no active Execution to resume".into(),
            ]),
            vec![],
        ));
    };
    if &execution.id != execution_id || execution.state != ExecutionState::Paused {
        return Ok((
            rejected_now(tx, RejectionCode::Ineligible, Some(work_id.clone()), vec![
                "resume requires the named Execution to be paused".into(),
            ]),
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
    if let Some(named) = execution_id {
        if named != &execution.id {
            return Ok((
                rejected_now(tx, RejectionCode::Ineligible, Some(work_id.clone()), vec![
                    "execution_id is not the active Execution".into(),
                ]),
                vec![],
            ));
        }
    }
    let seq = tx.alloc_seq()?;
    let command_id = CommandId(tx.alloc_id("cmd")?);
    let bump = Effect::BumpWorkVersion {
        work_id: work_id.clone(),
        expected: work.version,
        next: work.version + 1,
    };
    let accept = |effects: Vec<Effect>| {
        (
            CommandDecision::Accepted {
                command_id: command_id.clone(),
                work_id: Some(work_id.clone()),
                work_version: Some(work.version + 1),
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
            rejected_now(tx, RejectionCode::InvalidRequest, Some(work_id.clone()), vec![
                "judge_findings requires at least one decision".into(),
            ]),
            vec![],
        ));
    }
    let mut seen = std::collections::BTreeSet::new();
    for d in decisions {
        if !seen.insert(&d.finding_id) {
            return Ok((
                rejected_now(tx, RejectionCode::InvalidRequest, Some(work_id.clone()), vec![
                    format!("duplicate finding_id {}", d.finding_id),
                ]),
                vec![],
            ));
        }
        if !matches!(
            d.choice,
            FindingChoice::Fix | FindingChoice::Dismiss | FindingChoice::Accept | FindingChoice::Wait
        ) {
            return Ok((
                rejected_now(tx, RejectionCode::InvalidRequest, Some(work_id.clone()), vec![]),
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
    _expected_pr_head: &str,
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
            rejected_now(tx, RejectionCode::Ineligible, Some(work_id.clone()), vec![
                "no active Execution".into(),
            ]),
            vec![],
        ));
    };
    if execution.state != ExecutionState::Paused {
        return Ok((
            rejected_now(tx, RejectionCode::Ineligible, Some(work_id.clone()), vec![
                "release_judgment requires a paused Execution".into(),
            ]),
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

    // Atomic recheck: another writer may have claimed since acceptance.
    let resources = work_resources(&work, config);
    for res in &resources {
        if let Some(existing) = tx.reservation(&res.key())? {
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
    let request = InvocationRequest {
        request_id: format!("{}:inv:0", execution.id.0),
        work_id: work.id.clone(),
        execution_id: execution.id.clone(),
        role: InvocationRole::Coding,
        payload: serde_json::to_value(&execution.input).unwrap_or_default(),
    };
    let mut next = execution.clone();
    next.state = ExecutionState::Ready;
    next.started_seq = Some(seq);
    next.started_unix_seconds = Some(now_unix_seconds);
    tx.apply(&Effect::UpdateExecution {
        execution: next.clone(),
        expected_state: ExecutionState::Queued,
    })?;
    tx.apply(&Effect::ClaimSlot {
        execution_id: execution_id.clone(),
    })?;
    for res in resources {
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

/// What happened to an ingested Observation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IngestReceipt {
    Recorded,
    /// A source event delivered twice; the store's dedupe index caught it.
    Duplicate,
    Rejected(String),
}

/// Slice-0 completion policy: a coding Invocation's required artifacts,
/// an applied `publish_pr` intent, and check + review evidence covering
/// the same current head produce `succeeded`.
pub fn handle_observation(
    store: &impl Store,
    config: &WorkloopConfig,
    obs: &Observation,
    now_unix_seconds: i64,
) -> Result<IngestReceipt, StoreError> {
    store.transact(|tx| {
        tx.apply(&Effect::InsertObservation {
            observation: obs.clone(),
        })?;
        assess(tx, config, obs, now_unix_seconds)?;
        Ok(IngestReceipt::Recorded)
    })
    .or_else(|e| match e {
        StoreError::ConstraintConflict { detail }
            if detail.contains("observations_dedupe") || detail.contains("UNIQUE") =>
        {
            Ok(IngestReceipt::Duplicate)
        }
        other => Err(other),
    })
}

/// A committed observation may move the Execution; every transition here is
/// evidence-driven — a worker claim is never an Outcome by itself.
fn assess(
    tx: &mut dyn TxView,
    config: &WorkloopConfig,
    obs: &Observation,
    now_unix_seconds: i64,
) -> Result<(), StoreError> {
    let Some(execution_id) = &obs.subject_execution_id else {
        return Ok(());
    };
    let Some(execution) = tx.execution(execution_id)? else {
        return Ok(());
    };
    match &obs.fact {
        ObservationFact::ProcessState {
            invocation_id,
            state,
            ..
        } => assess_process(tx, config, &execution, invocation_id, state, now_unix_seconds),
        ObservationFact::LaunchRefused { invocation_id, .. } => {
            mark_invocation(tx, invocation_id, InvocationState::Abandoned)?;
            // Definitely no process exists: release the slot, keep the
            // reservation for a repair, and park the Execution.
            let mut next = execution.clone();
            next.state = ExecutionState::Paused;
            next.pause_reason = Some("repair: launch refused before spawn".into());
            tx.apply(&Effect::UpdateExecution {
                execution: next,
                expected_state: execution.state.clone(),
            })?;
            tx.apply(&Effect::ReleaseSlot {
                execution_id: execution.id.clone(),
            })?;
            Ok(())
        }
        ObservationFact::Artifact { state, .. } => {
            assess_artifact(tx, config, &execution, state, now_unix_seconds)
        }
        ObservationFact::PrActionResult {
            action_id, result, ..
        } => {
            let status = match result {
                ActionResult::Applied => PrActionStatus::Applied,
                ActionResult::Failed => PrActionStatus::Failed,
                ActionResult::Unknown => PrActionStatus::Unknown,
            };
            tx.apply(&Effect::UpdateActionIntent {
                action_id: action_id.clone(),
                status,
            })?;
            if result == &ActionResult::Applied && execution.state == ExecutionState::Waiting {
                let mut next = execution.clone();
                next.waiting_reason = Some(WaitingReason::Ci);
                tx.apply(&Effect::UpdateExecution {
                    execution: next,
                    expected_state: execution.state.clone(),
                })?;
            }
            if result == &ActionResult::Failed && execution.state == ExecutionState::Waiting {
                let mut next = execution.clone();
                next.state = ExecutionState::Paused;
                next.pause_reason = Some("repair: GitHub action failed after head recheck".into());
                tx.apply(&Effect::UpdateExecution {
                    execution: next,
                    expected_state: execution.state.clone(),
                })?;
                tx.apply(&Effect::ReleaseSlot {
                    execution_id: execution.id.clone(),
                })?;
            }
            Ok(())
        }
        ObservationFact::PrChecks { .. } | ObservationFact::PrReview { .. } => {
            assess_completion(tx, &execution)
        }
        _ => Ok(()),
    }
}

fn mark_invocation(
    tx: &mut dyn TxView,
    invocation_id: &InvocationId,
    state: InvocationState,
) -> Result<(), StoreError> {
    for exec in tx.nonterminal_executions()? {
        for inv in tx.invocations_for_execution(&exec.id)? {
            if inv.id == *invocation_id && inv.state != state {
                let mut next = inv.clone();
                next.state = state;
                tx.apply(&Effect::UpdateInvocation {
                    invocation: next,
                    expected_state: inv.state.clone(),
                })?;
                return Ok(());
            }
        }
    }
    Ok(())
}

fn assess_process(
    tx: &mut dyn TxView,
    config: &WorkloopConfig,
    execution: &Execution,
    invocation_id: &InvocationId,
    state: &ProcessStateFact,
    now_unix_seconds: i64,
) -> Result<(), StoreError> {
    match state {
        ProcessStateFact::Running => match execution.state {
            ExecutionState::Ready => {
                let mut next = execution.clone();
                next.state = ExecutionState::Running;
                tx.apply(&Effect::UpdateExecution {
                    execution: next,
                    expected_state: execution.state.clone(),
                })
            }
            // A writer re-confirmed alive resolves `uncertain(continue)`.
            ExecutionState::Uncertain
                if execution.desired_action == Some(DesiredAction::Continue) =>
            {
                let mut next = execution.clone();
                next.state = ExecutionState::Running;
                next.desired_action = None;
                next.uncertain_since_unix_seconds = None;
                tx.apply(&Effect::UpdateExecution {
                    execution: next,
                    expected_state: execution.state.clone(),
                })
            }
            _ => Ok(()),
        },
        ProcessStateFact::Exited => {
            mark_invocation(tx, invocation_id, InvocationState::Done)?;
            match execution.state {
                ExecutionState::Running => {
                    let mut next = execution.clone();
                    next.state = ExecutionState::Waiting;
                    next.waiting_reason = Some(WaitingReason::Reconciliation);
                    tx.apply(&Effect::UpdateExecution {
                        execution: next,
                        expected_state: execution.state.clone(),
                    })?;
                    maybe_publish(tx, config, execution, now_unix_seconds)
                }
                ExecutionState::Stopping | ExecutionState::Uncertain => {
                    settle_after_writer_gone(tx, config, execution, now_unix_seconds)
                }
                _ => Ok(()),
            }
        }
        ProcessStateFact::Unknown => match execution.state {
            ExecutionState::Ready | ExecutionState::Running | ExecutionState::Waiting => {
                let mut next = execution.clone();
                next.state = ExecutionState::Uncertain;
                next.desired_action = Some(DesiredAction::Continue);
                next.uncertain_since_unix_seconds = Some(now_unix_seconds);
                tx.apply(&Effect::UpdateExecution {
                    execution: next,
                    expected_state: execution.state.clone(),
                })
            }
            _ => Ok(()),
        },
    }
}

/// The writer is gone: resolve `stopping`/`uncertain` to the recorded
/// desired action — terminal states release slot and reservation, a
/// recoverable pause releases only the slot.
fn settle_after_writer_gone(
    tx: &mut dyn TxView,
    config: &WorkloopConfig,
    execution: &Execution,
    now_unix_seconds: i64,
) -> Result<(), StoreError> {
    let desired = execution.desired_action.clone().unwrap_or(DesiredAction::Fail);
    let mut next = execution.clone();
    let seq = tx.alloc_seq()?;
    let release_reservations = match desired {
        DesiredAction::Cancel => {
            next.state = ExecutionState::Cancelled;
            next.terminal_code = Some("cancelled".into());
            next.outcome = Some(Outcome {
                state: ExecutionState::Cancelled,
                code: Some("cancelled".into()),
                detail: Some("writer stop proven".into()),
                evidence_refs: vec![],
                recorded_seq: seq,
            });
            true
        }
        DesiredAction::Fail => {
            next.state = ExecutionState::Failed;
            next.terminal_code = Some("stopped".into());
            next.outcome = Some(Outcome {
                state: ExecutionState::Failed,
                code: Some("stopped".into()),
                detail: Some("writer stop proven; recorded failure".into()),
                evidence_refs: vec![],
                recorded_seq: seq,
            });
            true
        }
        DesiredAction::Continue => {
            next.state = ExecutionState::Paused;
            next.pause_reason = Some("recoverable_stop".into());
            false
        }
    };
    next.desired_action = None;
    next.uncertain_since_unix_seconds = None;
    tx.apply(&Effect::UpdateExecution {
        execution: next,
        expected_state: execution.state.clone(),
    })?;
    tx.apply(&Effect::ReleaseSlot {
        execution_id: execution.id.clone(),
    })?;
    if release_reservations {
        let work = tx.work(&execution.work_id)?.ok_or_else(|| StoreError::Internal {
            detail: "execution without work".into(),
        })?;
        tx.apply(&Effect::DeleteReservations {
            resource_keys: resources_keys_of(tx, &work, execution)?,
            holder: ReservationHolder::Execution {
                execution_id: execution.id.clone(),
            },
        })?;
    }
    let _ = config;
    let _ = now_unix_seconds;
    Ok(())
}

fn assess_artifact(
    tx: &mut dyn TxView,
    config: &WorkloopConfig,
    execution: &Execution,
    state: &ArtifactFact,
    now_unix_seconds: i64,
) -> Result<(), StoreError> {
    match state {
        // Required evidence missing after a confirmed exit → bounded
        // recovery. The deadline is durable from the first transition in.
        ArtifactFact::Missing | ArtifactFact::Corrupt
            if execution.state == ExecutionState::Waiting
                || execution.state == ExecutionState::Running =>
        {
            let mut next = execution.clone();
            next.state = ExecutionState::Waiting;
            next.waiting_reason = Some(WaitingReason::ArtifactRecovery);
            if next.recovery_deadline_unix_seconds.is_none() {
                next.recovery_deadline_unix_seconds =
                    Some(now_unix_seconds + execution.budget.artifact_recovery_seconds as i64);
            }
            tx.apply(&Effect::UpdateExecution {
                execution: next,
                expected_state: execution.state.clone(),
            })
        }
        ArtifactFact::Present
            if execution.state == ExecutionState::Waiting
                || execution.state == ExecutionState::Running =>
        {
            maybe_publish(tx, config, execution, now_unix_seconds)
        }
        _ => Ok(()),
    }
}

/// Once the coding Invocation's evidence is complete, record the publish
/// intent — the outbox row exists before any GitHub write is attempted.
fn maybe_publish(
    tx: &mut dyn TxView,
    config: &WorkloopConfig,
    execution: &Execution,
    _now_unix_seconds: i64,
) -> Result<(), StoreError> {
    if execution.state != ExecutionState::Waiting {
        return Ok(());
    }
    if matches!(
        execution.waiting_reason,
        Some(WaitingReason::ArtifactRecovery)
    ) {
        // Recovery only proceeds once every required artifact is present.
        let invocations = tx.invocations_for_execution(&execution.id)?;
        let complete = invocations
            .iter()
            .rev()
            .find(|i| i.state == InvocationState::Done || i.state == InvocationState::Abandoned)
            .map(|i| {
                crate::projection::artifacts_complete(tx, &execution.id, i)
                    .unwrap_or(false)
            })
            .unwrap_or(false);
        if !complete {
            return Ok(());
        }
    } else {
        let invocations = tx.invocations_for_execution(&execution.id)?;
        let latest = invocations.last();
        let complete = match latest {
            Some(i) if i.state == InvocationState::Done => {
                crate::projection::artifacts_complete(tx, &execution.id, i)?
            }
            _ => false,
        };
        if !complete {
            return Ok(());
        }
    }
    let outstanding = crate::projection::outstanding_actions(tx, &execution.work_id)?;
    if !outstanding.is_empty() {
        return Ok(());
    }
    let work = tx.work(&execution.work_id)?.ok_or_else(|| StoreError::Internal {
        detail: "execution without work".into(),
    })?;
    let repository = work
        .issue_refs
        .first()
        .map(|r| r.repository.clone())
        .unwrap_or_else(|| config.default_repository.clone());
    let action_id = tx.alloc_id("act")?;
    tx.apply(&Effect::InsertPrActionIntent {
        intent: PrActionIntent {
            action_id,
            work_id: work.id.clone(),
            repository,
            action: crate::store::PrAction::PublishPr {
                branch: format!("work/{}", work.id.0),
                title: work.intent.chars().take(70).collect(),
                body: String::new(),
            },
            status: PrActionStatus::Pending,
        },
    })?;
    let mut next = execution.clone();
    next.waiting_reason = Some(WaitingReason::GithubWrite);
    tx.apply(&Effect::UpdateExecution {
        execution: next,
        expected_state: execution.state.clone(),
    })
}

/// `succeeded` requires check + review evidence covering the same current
/// head and no unresolved writer, gate, or outgoing action.
fn assess_completion(tx: &mut dyn TxView, execution: &Execution) -> Result<(), StoreError> {
    if execution.state != ExecutionState::Waiting {
        return Ok(());
    }
    let observations = tx.observations_for_work(&execution.work_id)?;
    let head = observations.iter().rev().find_map(|o| match &o.fact {
        ObservationFact::PrHead { head_sha, .. } => Some(head_sha.clone()),
        _ => None,
    });
    let Some(head) = head else { return Ok(()) };
    let checks_ok = observations.iter().any(|o| {
        matches!(
            &o.fact,
            ObservationFact::PrChecks {
                head_sha,
                conclusion: CheckConclusion::Success,
                ..
            } if head_sha == &head
        )
    });
    let review_ok = observations.iter().any(|o| {
        matches!(
            &o.fact,
            ObservationFact::PrReview {
                head_sha,
                verdict: ReviewVerdict::Approve,
                ..
            } if head_sha == &head
        )
    });
    if !(checks_ok && review_ok) {
        return Ok(());
    }
    if !crate::projection::outstanding_actions(tx, &execution.work_id)?.is_empty() {
        return Ok(());
    }
    let seq = tx.alloc_seq()?;
    let mut next = execution.clone();
    next.state = ExecutionState::Succeeded;
    next.terminal_code = Some("succeeded".into());
    next.outcome = Some(Outcome {
        state: ExecutionState::Succeeded,
        code: Some("succeeded".into()),
        detail: Some(format!("verified head {head}")),
        evidence_refs: vec![],
        recorded_seq: seq,
    });
    tx.apply(&Effect::UpdateExecution {
        execution: next,
        expected_state: execution.state.clone(),
    })?;
    tx.apply(&Effect::ReleaseSlot {
        execution_id: execution.id.clone(),
    })?;
    let work = tx.work(&execution.work_id)?.ok_or_else(|| StoreError::Internal {
        detail: "execution without work".into(),
    })?;
    tx.apply(&Effect::DeleteReservations {
        resource_keys: resources_keys_of(tx, &work, execution)?,
        holder: ReservationHolder::Execution {
            execution_id: execution.id.clone(),
        },
    })?;
    Ok(())
}

/// The internal dispatch scan: queued Executions in committed enqueue
/// order. A deferred head never blocks later items; each deferral is
/// recorded for the `capacity` Query.
pub fn dispatch_once(
    tx: &mut dyn TxView,
    config: &WorkloopConfig,
    now_unix_seconds: i64,
) -> Result<Vec<ExecutionId>, StoreError> {
    let mut admitted = Vec::new();
    for execution in tx.queued_executions()? {
        if admit_queued(tx, config, &execution.id, now_unix_seconds)?.is_some() {
            admitted.push(execution.id);
        }
    }
    Ok(admitted)
}

/// Timed decisions the loop owes the store: runtime limits, recovery
/// bounds, and budget exhaustion. Each is a committed decision checked
/// against the stored limit and the observed clock — never silent.
pub fn sweep(
    store: &impl Store,
    config: &WorkloopConfig,
    now_unix_seconds: i64,
) -> Result<(), StoreError> {
    store.transact(|tx| {
        for execution in tx.nonterminal_executions()? {
            match execution.state {
                ExecutionState::Ready | ExecutionState::Running | ExecutionState::Waiting => {
                    // Recovery bound is separate from the runtime budget.
                    if matches!(
                        execution.waiting_reason,
                        Some(WaitingReason::ArtifactRecovery)
                    ) {
                        let exhausted = execution.recovery_attempts_used
                            >= execution.budget.artifact_recovery_attempts
                            || execution
                                .recovery_deadline_unix_seconds
                                .is_some_and(|d| now_unix_seconds > d);
                        if exhausted {
                            fail_terminal(
                                tx,
                                &execution,
                                "artifact_recovery_exhausted",
                                "bounded artifact recovery exhausted; WIP preserved",
                            )?;
                            continue;
                        }
                    }
                    let over_runtime = execution.started_unix_seconds.is_some_and(|s| {
                        now_unix_seconds - s > execution.budget.runtime_limit_seconds as i64
                    });
                    if over_runtime {
                        let mut next = execution.clone();
                        next.state = ExecutionState::Stopping;
                        next.desired_action = Some(DesiredAction::Fail);
                        tx.apply(&Effect::UpdateExecution {
                            execution: next,
                            expected_state: execution.state.clone(),
                        })?;
                    }
                }
                ExecutionState::Queued => {
                    // An absolute deadline would be checked here; v0.1 has
                    // none configured. Queue time never consumes runtime.
                }
                _ => {}
            }
        }
        let _ = config;
        Ok(())
    })
}

fn fail_terminal(
    tx: &mut dyn TxView,
    execution: &Execution,
    code: &str,
    detail: &str,
) -> Result<(), StoreError> {
    let seq = tx.alloc_seq()?;
    let mut next = execution.clone();
    next.state = ExecutionState::Failed;
    next.terminal_code = Some(code.into());
    next.outcome = Some(Outcome {
        state: ExecutionState::Failed,
        code: Some(code.into()),
        detail: Some(detail.into()),
        evidence_refs: vec![],
        recorded_seq: seq,
    });
    tx.apply(&Effect::UpdateExecution {
        execution: next,
        expected_state: execution.state.clone(),
    })?;
    tx.apply(&Effect::ReleaseSlot {
        execution_id: execution.id.clone(),
    })?;
    let work = tx.work(&execution.work_id)?.ok_or_else(|| StoreError::Internal {
        detail: "execution without work".into(),
    })?;
    tx.apply(&Effect::DeleteReservations {
        resource_keys: resources_keys_of(tx, &work, execution)?,
        holder: ReservationHolder::Execution {
            execution_id: execution.id.clone(),
        },
    })?;
    Ok(())
}
