//! Projection Query contract tests (`docs/v0.1.md`: `work_list`,
//! `work_detail`, `attention`, `capacity`, `command_result`). Queries are
//! read-only derived state — they never refresh the host or start a worker.

use std::collections::BTreeSet;

use workloop_core::command::*;
use workloop_core::domain::*;
use workloop_core::identity::*;
use workloop_core::observation::*;
use workloop_core::projection::*;
use workloop_core::store::*;
use workloop_core::work::{
    HostTemplate, WorkloopConfig, admit_queued, dispatch_once, handle_command, sweep,
};
use workloop_store::sqlite::SqliteStore;

const NOW: i64 = 1_700_000_000;

fn host() -> HostInstanceId {
    HostInstanceId::trusted("test-host")
}

fn repo() -> RepositoryRef {
    RepositoryRef::new(host(), RepositoryId::trusted("local-repo"))
}

fn rev(object: &str) -> CommitRevision {
    CommitRevision::new(repo(), object).unwrap()
}

fn change() -> ChangeRequestRef {
    ChangeRequestRef {
        repository: repo(),
        change_request: ChangeRequestId::trusted("cr-1"),
        display_number: Some(1),
    }
}

fn target(object: &str) -> DiffTarget {
    DiffTarget {
        change: change(),
        head: rev(object),
        base: None,
        target_revision: None,
    }
}

fn publisher() -> HostActorRef {
    HostActorRef {
        host: host(),
        actor: HostActorId::trusted("pub-agent"),
    }
}

fn config() -> WorkloopConfig {
    WorkloopConfig {
        placement: Ok(workloop_core::placement::PlacementPolicy::default()),
        host: HostTemplate {
            adapter: AdapterId::trusted("fixture"),
            repository: repo(),
            contract_version: 1,
            actors: vec![RoleBinding {
                role: ActorRole::Publication,
                actor: publisher(),
                credential_ref: CredentialRef::trusted("cred-publication"),
            }],
        },
        declared_capabilities: BTreeSet::from([
            HostCapability::RepositoryRead,
            HostCapability::PushRef,
            HostCapability::ChangeRequestCreate,
            HostCapability::ChangeRequestRead,
            HostCapability::ChecksRead,
            HostCapability::ReviewFindingsRead,
            HostCapability::IssueRead,
        ]),
        lanes: LanePolicy::default(),
        required_checks: vec![],
        allowed_effects: AllowedEffects {
            workspace_write: true,
            host_write: false,
            network: false,
        },
        budget: ExecutionBudget::default(),
        policy: WorkPolicy::default(),
    }
}

fn actor() -> Actor {
    Actor {
        kind: ActorKind::Human,
        external_id: "op".into(),
    }
}

fn submit(store: &SqliteStore, key: &str) -> (WorkId, ExecutionId) {
    let d = handle_command(
        store,
        &config(),
        &CommandRequest {
            actor: actor(),
            origin: Origin("cli".into()),
            idempotency_key: key.into(),
            body: CommandBody::Submit {
                intent: format!("intent-{key}"),
                sources: vec![],
                constraints: vec![],
                verification_argv: vec![],
            },
        },
        NOW,
    )
    .unwrap();
    let CommandDecision::Accepted {
        work_id,
        effect: CommandEffect::WorkCreated { execution_id },
        ..
    } = d
    else {
        panic!("submit {key}: {d:?}")
    };
    (work_id.unwrap(), execution_id)
}

/// The change the Work opened plus its observed head — the two facts
/// `current_target` derives the diff target from.
/// The applied intent the open receipt reconciles against — only a
/// receipt naming a recorded applied intent validates the opened change.
fn open_intent(store: &SqliteStore, work_id: &WorkId, action_id: &str) {
    store
        .transact(|tx| {
            if tx
                .action_intents()?
                .iter()
                .any(|i| i.action_id == action_id)
            {
                return Ok(());
            }
            let created_seq = tx.alloc_seq()?;
            tx.apply(&Effect::InsertHostActionIntent {
                intent: HostActionIntent {
                    action_id: action_id.into(),
                    work_id: work_id.clone(),
                    host: host(),
                    repository: repo(),
                    role: ActorRole::Publication,
                    actor: publisher(),
                    action: HostAction::OpenChangeRequest {
                        source_branch: format!("work/{}", work_id.0),
                        target_branch: "main".into(),
                        title: "seed".into(),
                        body: String::new(),
                    },
                    payload_digest: "test".into(),
                    status: HostActionStatus::Applied,
                    authenticated_actor: None,
                    effects: vec![],
                    created_seq,
                },
            })
        })
        .unwrap();
}

fn open_change(store: &SqliteStore, work_id: &WorkId, object: &str, at: i64) {
    open_intent(store, work_id, "act-open");
    for fact in [
        ObservationFact::HostActionResult {
            action_id: "act-open".into(),
            authenticated_actor: None,
            status: HostActionStatus::Applied,
            effects: vec![NormalizedEffectFact::ChangeOpened {
                change: change(),
                head: rev(object),
                source_branch: format!("work/{}", work_id.0),
                target_branch: "main".into(),
            }],
        },
        ObservationFact::ChangeHead {
            change: change(),
            head: rev(object),
        },
    ] {
        workloop_core::work::handle_observation(
            store,
            &config(),
            &Observation {
                delivery: None,
                subject_work_id: Some(work_id.clone()),
                subject_execution_id: None,
                fact,
                observed_unix_seconds: at,
            },
            at,
        )
        .unwrap();
    }
}

/// A complete review over `source_keys` on `object`'s target, through the
/// same ingest path a real delivery uses — seals an immutable round.
fn seal_review(
    store: &SqliteStore,
    work_id: &WorkId,
    object: &str,
    source_keys: &[&str],
    execution_id: Option<&ExecutionId>,
    at: i64,
) {
    workloop_core::work::handle_observation(
        store,
        &config(),
        &Observation {
            delivery: None,
            subject_work_id: Some(work_id.clone()),
            subject_execution_id: execution_id.cloned(),
            fact: ObservationFact::Review {
                target: target(object),
                source: "reviewer".into(),
                source_revision: None,
                completeness: Completeness::Complete,
                occurrences: source_keys
                    .iter()
                    .map(|key| OccurrenceInput {
                        source_key: (*key).into(),
                        content_digest: None,
                        severity: None,
                        position: None,
                        thread_id: None,
                        summary: None,
                    })
                    .collect(),
            },
            observed_unix_seconds: at,
        },
        at,
    )
    .unwrap();
}

fn found<T>(r: QueryResult<T>) -> T {
    match r {
        QueryResult::Found { value, .. } => value,
        QueryResult::NotFound => panic!("expected found, got not_found"),
        QueryResult::Forbidden => panic!("expected found, got forbidden"),
        QueryResult::StoreUnavailable => panic!("expected found, got store_unavailable"),
    }
}

fn execution(store: &SqliteStore, id: &ExecutionId) -> Execution {
    store.transact(|tx| tx.execution(id)).unwrap().unwrap()
}

#[test]
fn work_list_phases_derive_from_records() {
    let store = SqliteStore::in_memory(2).unwrap();
    let (_w1, _e1) = submit(&store, "q1");
    let (_w2, e2) = submit(&store, "q2");
    let view = found(store.transact(|tx| work_list(tx, NOW)).unwrap());
    assert_eq!(view.items.len(), 2);
    assert!(view.items.iter().all(|i| i.phase == WorkPhase::Queued));
    // Admitting the second flips its derived phase without a state column.
    store
        .transact(|tx| admit_queued(tx, &config(), &e2, NOW).map(|_| ()))
        .unwrap();
    let view = found(store.transact(|tx| work_list(tx, NOW)).unwrap());
    let item = view
        .items
        .iter()
        .find(|i| i.phase == WorkPhase::Implementing);
    assert!(item.is_some(), "admitted work derives implementing");
}

#[test]
fn work_detail_binds_findings_to_the_current_head() {
    let store = SqliteStore::in_memory(1).unwrap();
    let (work_id, eid) = submit(&store, "q3");
    // Head moves old → new; each head gets its own sealed review round.
    open_change(&store, &work_id, "sha-old", NOW);
    seal_review(
        &store,
        &work_id,
        "sha-old",
        &["old-finding"],
        Some(&eid),
        NOW,
    );
    open_change(&store, &work_id, "sha-new", NOW);
    seal_review(
        &store,
        &work_id,
        "sha-new",
        &["new-finding"],
        Some(&eid),
        NOW,
    );
    let QueryResult::Found { value, sources, .. } =
        store.transact(|tx| work_detail(tx, &work_id, NOW)).unwrap()
    else {
        panic!("expected found")
    };
    let (change_ref, head) = value.current_change.as_ref().unwrap();
    assert_eq!(change_ref.change_request.as_str(), "cr-1");
    assert_eq!(head.as_ref(), Some(&rev("sha-new")));
    // Only the current-head round's findings are actionable.
    assert_eq!(value.findings.len(), 1);
    assert_eq!(value.findings[0].source_key, "new-finding");
    assert!(value.findings[0].occurrence_id.starts_with("occ-"));
    // The review source was read for this head → known.
    assert!(
        sources
            .iter()
            .any(|s| s.source == "review:reviewer" && s.availability == Availability::Known)
    );
    let missing = store
        .transact(|tx| work_detail(tx, &WorkId("w-nope".into()), NOW))
        .unwrap();
    assert!(matches!(missing, QueryResult::NotFound));
}

#[test]
fn delayed_review_evidence_does_not_misrepresent_the_current_target() {
    for execution_scoped in [false, true] {
        let store = SqliteStore::in_memory(1).unwrap();
        let (work_id, execution_id) = submit(&store, "delayed-head");
        open_change(&store, &work_id, "sha-current", NOW + 10);
        seal_review(
            &store,
            &work_id,
            "sha-current",
            &["current-finding"],
            Some(&execution_id),
            NOW + 10,
        );
        // A stale-head review delivered late: recorded, never displaces
        // the current target's evidence — and invalidates nothing.
        let version_before = store
            .transact(|tx| Ok(tx.work(&work_id)?.unwrap().version))
            .unwrap();
        let stale_obs = Observation {
            delivery: None,
            subject_work_id: (!execution_scoped).then(|| work_id.clone()),
            subject_execution_id: execution_scoped.then(|| execution_id.clone()),
            fact: ObservationFact::Review {
                target: target("sha-delayed"),
                source: "stale-reviewer".into(),
                source_revision: None,
                completeness: Completeness::Complete,
                occurrences: vec![OccurrenceInput {
                    source_key: "stale-finding".into(),
                    content_digest: None,
                    severity: None,
                    position: None,
                    thread_id: None,
                    summary: None,
                }],
            },
            observed_unix_seconds: NOW,
        };
        workloop_core::work::handle_observation(&store, &config(), &stale_obs, NOW + 20).unwrap();
        let view = found(
            store
                .transact(|tx| work_detail(tx, &work_id, NOW + 20))
                .unwrap(),
        );
        let (pr, head) = view.current_change.unwrap();
        assert_eq!(head, Some(rev("sha-current")));
        assert_eq!(pr.change_request.as_str(), "cr-1");
        if execution_scoped {
            // An execution-scoped review is recorded but never seals a
            // round — the current round's findings stand.
            assert_eq!(view.findings.len(), 1);
            assert_eq!(view.findings[0].source_key, "current-finding");
        } else {
            // The stale work-scoped review seals a round for a
            // superseded target: recorded evidence that can neither
            // displace nor mask the current target's round.
            assert_eq!(view.findings.len(), 1);
            assert_eq!(view.findings[0].source_key, "current-finding");
        }
        assert!(
            view.findings
                .iter()
                .all(|f| f.source_key != "stale-finding")
        );
        // Evidence for a superseded target is not a current-state
        // change: recorded, but the version preconditions stand.
        assert_eq!(view.version, version_before);
    }
}

/// Regression: a round's occurrences return in review order even past
/// nine findings — the generated id pads the index so `ORDER BY id`
/// stays numeric (`occ-…-000010` must not sort before `occ-…-000002`).
#[test]
fn occurrence_order_stays_in_review_order_past_nine() {
    let store = SqliteStore::in_memory(1).unwrap();
    let (work_id, execution_id) = submit(&store, "many-findings");
    open_change(&store, &work_id, "sha-many", NOW);
    let keys: Vec<String> = (0..12).map(|i| format!("f-{i:02}")).collect();
    let refs: Vec<&str> = keys.iter().map(String::as_str).collect();
    seal_review(
        &store,
        &work_id,
        "sha-many",
        &refs,
        Some(&execution_id),
        NOW,
    );
    let view = found(store.transact(|tx| work_detail(tx, &work_id, NOW)).unwrap());
    let order: Vec<&str> = view
        .findings
        .iter()
        .map(|f| f.source_key.as_str())
        .collect();
    assert_eq!(order, refs, "occurrences must follow review order");
}

#[test]
fn work_detail_reports_the_terminal_outcome() {
    let store = SqliteStore::in_memory(1).unwrap();
    let (work_id, _eid) = submit(&store, "q20");
    let d = handle_command(
        &store,
        &config(),
        &CommandRequest {
            actor: actor(),
            origin: Origin("cli".into()),
            idempotency_key: "q20-cancel".into(),
            body: CommandBody::Cancel {
                work_id: work_id.clone(),
                expected_work_version: 1,
                execution_id: None,
            },
        },
        NOW,
    )
    .unwrap();
    assert!(matches!(d, CommandDecision::Accepted { .. }), "{d:?}");
    let view = found(store.transact(|tx| work_detail(tx, &work_id, NOW)).unwrap());
    let outcome = view.outcome.expect("terminal outcome surfaced in detail");
    assert_eq!(outcome.state, ExecutionState::Cancelled);
    assert_eq!(outcome.code.as_deref(), Some("cancelled"));
}

#[test]
fn attention_versions_and_acknowledgement() {
    let store = SqliteStore::in_memory(1).unwrap();
    let (work_id, eid) = submit(&store, "q4");
    // Park the execution paused → paused_judgment attention.
    store
        .transact(|tx| {
            let mut e = tx.execution(&eid)?.unwrap();
            e.state = ExecutionState::Paused;
            e.pause_reason = Some("repair: needs human".into());
            tx.apply(&Effect::UpdateExecution {
                execution: e,
                expected_state: ExecutionState::Queued,
            })
        })
        .unwrap();
    let items = found(store.transact(|tx| attention(tx, None, NOW)).unwrap());
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].reason, "paused_judgment");
    let att_id = items[0].attention_id.clone();

    // Acknowledging hides only that version.
    let d = handle_command(
        &store,
        &config(),
        &CommandRequest {
            actor: actor(),
            origin: Origin("cli".into()),
            idempotency_key: "q4-ack".into(),
            body: CommandBody::Acknowledge {
                work_id: work_id.clone(),
                expected_work_version: 1,
                attention_id: att_id.clone(),
            },
        },
        NOW,
    )
    .unwrap();
    assert!(matches!(d, CommandDecision::Accepted { .. }), "{d:?}");
    assert!(
        found(store.transact(|tx| attention(tx, None, NOW)).unwrap()).is_empty(),
        "acknowledged attention is hidden"
    );

    // A changed condition resurfaces under a new version id.
    store
        .transact(|tx| {
            let mut e = tx.execution(&eid)?.unwrap();
            e.pause_reason = Some("repair: different problem".into());
            tx.apply(&Effect::UpdateExecution {
                execution: e,
                expected_state: ExecutionState::Paused,
            })
        })
        .unwrap();
    let items = found(store.transact(|tx| attention(tx, None, NOW)).unwrap());
    assert_eq!(items.len(), 1);
    assert_ne!(items[0].attention_id, att_id, "new version resurfaces");
}

#[test]
fn merge_check_records_do_not_resurface_acknowledged_attention() {
    let store = SqliteStore::in_memory(1).unwrap();
    let (work_id, eid) = submit(&store, "q-mergecheck");
    store
        .transact(|tx| {
            let mut e = tx.execution(&eid)?.unwrap();
            e.state = ExecutionState::Paused;
            e.pause_reason = Some("repair: needs human".into());
            tx.apply(&Effect::UpdateExecution {
                execution: e,
                expected_state: ExecutionState::Queued,
            })
        })
        .unwrap();
    let items = found(store.transact(|tx| attention(tx, None, NOW)).unwrap());
    assert_eq!(items.len(), 1);
    let att_id = items[0].attention_id.clone();
    let d = handle_command(
        &store,
        &config(),
        &CommandRequest {
            actor: actor(),
            origin: Origin("cli".into()),
            idempotency_key: "q-mergecheck-ack".into(),
            body: CommandBody::Acknowledge {
                work_id: work_id.clone(),
                expected_work_version: 1,
                attention_id: att_id,
            },
        },
        NOW,
    )
    .unwrap();
    assert!(matches!(d, CommandDecision::Accepted { .. }), "{d:?}");
    assert!(
        found(store.transact(|tx| attention(tx, None, NOW)).unwrap()).is_empty(),
        "acknowledged attention is hidden"
    );

    // A merge-detection pass leaves one fresh ordering record per pass —
    // bookkeeping for the least-recently-checked order, not new Work
    // evidence. Counting it would mint a new occurrence id every pass
    // and resurface the acknowledged hold forever.
    for pass in 0..3i64 {
        workloop_core::work::handle_observation(
            &store,
            &config(),
            &Observation {
                delivery: Some(DeliveryId {
                    scope: DeliveryScope::Host { host: host() },
                    source: "github.merge".into(),
                    event_id: format!("merge-check:{}:cr-1@h#epoch-{pass}", work_id.0),
                }),
                subject_work_id: Some(work_id.clone()),
                subject_execution_id: None,
                fact: ObservationFact::MergeCheck {
                    change: change(),
                    head: rev("detected-head"),
                },
                observed_unix_seconds: NOW + pass,
            },
            NOW + pass,
        )
        .unwrap();
    }
    assert!(
        found(store.transact(|tx| attention(tx, None, NOW)).unwrap()).is_empty(),
        "merge-detection bookkeeping never resurfaces an acknowledged attention"
    );
}

#[test]
fn acknowledge_requires_a_currently_derivable_attention_version() {
    let store = SqliteStore::in_memory(1).unwrap();
    let (work_id, eid) = submit(&store, "q14");
    // Nothing is paused yet, but the id names the pause_reason a later
    // repair pause would produce: acknowledging it now would persist a
    // row that hides that condition the moment it surfaces.
    let phantom = format!(
        "att:{}:paused_judgment:repair: launch refused before spawn:{}:{}:0",
        work_id.0,
        eid.0,
        execution(&store, &eid).enqueued_seq
    );
    let ack = |key: &str, attention_id: &str| {
        handle_command(
            &store,
            &config(),
            &CommandRequest {
                actor: actor(),
                origin: Origin("cli".into()),
                idempotency_key: key.into(),
                body: CommandBody::Acknowledge {
                    work_id: work_id.clone(),
                    expected_work_version: 1,
                    attention_id: attention_id.to_string(),
                },
            },
            NOW,
        )
        .unwrap()
    };
    let d = ack("q14-ack-early", &phantom);
    assert!(
        matches!(&d, CommandDecision::Rejected { reason, .. } if *reason == RejectionCode::Ineligible),
        "{d:?}"
    );
    assert!(
        store
            .transact(|tx| tx.attention_acks(&work_id))
            .unwrap()
            .is_empty(),
        "no ack row is recorded for a version the state never derived"
    );

    // The condition arrives with exactly that pause_reason: the same id
    // is derivable now, so the acknowledgement is accepted and hides the
    // displayed version.
    store
        .transact(|tx| {
            let mut e = tx.execution(&eid)?.unwrap();
            e.state = ExecutionState::Paused;
            e.pause_reason = Some("repair: launch refused before spawn".into());
            tx.apply(&Effect::UpdateExecution {
                execution: e,
                expected_state: ExecutionState::Queued,
            })
        })
        .unwrap();
    let items = found(store.transact(|tx| attention(tx, None, NOW)).unwrap());
    assert_eq!(items.len(), 1);
    let shown = items[0].attention_id.clone();
    assert_eq!(
        shown, phantom,
        "the previously underivable version is now shown"
    );
    let d = ack("q14-ack-now", &shown);
    assert!(matches!(d, CommandDecision::Accepted { .. }), "{d:?}");
    assert!(
        found(store.transact(|tx| attention(tx, None, NOW)).unwrap()).is_empty(),
        "the derivable version is hidden by its acknowledgement"
    );
    // The condition still holds, so the version is still derivable: a
    // repeat acknowledgement is a no-op upsert, not a rejection.
    let d = ack("q14-ack-again", &shown);
    assert!(matches!(d, CommandDecision::Accepted { .. }), "{d:?}");
}

#[test]
fn repeated_pause_and_new_evidence_get_fresh_attention_versions() {
    let store = SqliteStore::in_memory(1).unwrap();
    let (work_id, eid) = submit(&store, "repeated-pause");
    let pause = |expected_state| {
        store
            .transact(|tx| {
                let mut execution = tx.execution(&eid)?.unwrap();
                execution.state = ExecutionState::Paused;
                execution.pause_reason = Some("repair: launch refused before spawn".into());
                tx.apply(&Effect::UpdateExecution {
                    execution,
                    expected_state,
                })
            })
            .unwrap();
    };
    pause(ExecutionState::Queued);
    let first = found(
        store
            .transact(|tx| attention(tx, Some(&work_id), NOW))
            .unwrap(),
    )[0]
    .attention_id
    .clone();
    handle_command(
        &store,
        &config(),
        &CommandRequest {
            actor: actor(),
            origin: Origin("cli".into()),
            idempotency_key: "ack-first-pause".into(),
            body: CommandBody::Acknowledge {
                work_id: work_id.clone(),
                expected_work_version: 1,
                attention_id: first.clone(),
            },
        },
        NOW,
    )
    .unwrap();
    assert!(
        found(
            store
                .transact(|tx| attention(tx, Some(&work_id), NOW + 3600))
                .unwrap()
        )
        .is_empty()
    );
    store
        .transact(|tx| {
            let mut execution = tx.execution(&eid)?.unwrap();
            execution.state = ExecutionState::Queued;
            execution.enqueued_seq = tx.alloc_seq()?;
            tx.apply(&Effect::UpdateExecution {
                execution,
                expected_state: ExecutionState::Paused,
            })
        })
        .unwrap();
    pause(ExecutionState::Queued);
    let second = found(
        store
            .transact(|tx| attention(tx, Some(&work_id), NOW))
            .unwrap(),
    );
    assert_eq!(
        second.len(),
        1,
        "a repeated pause must resurface after acknowledgement"
    );
    assert_ne!(second[0].attention_id, first);
    store
        .transact(|tx| {
            tx.apply(&Effect::InsertObservation {
                observation: Observation {
                    delivery: Some(DeliveryId {
                        scope: DeliveryScope::Local,
                        source: "test".into(),
                        event_id: "new-pause-evidence".into(),
                    }),
                    subject_work_id: Some(work_id.clone()),
                    subject_execution_id: None,
                    fact: ObservationFact::ChangeHead {
                        change: change(),
                        head: rev("new-evidence"),
                    },
                    observed_unix_seconds: NOW,
                },
            })
        })
        .unwrap();
    let third = found(
        store
            .transact(|tx| attention(tx, Some(&work_id), NOW))
            .unwrap(),
    );
    assert_eq!(third.len(), 1);
    assert_ne!(
        third[0].attention_id, second[0].attention_id,
        "new evidence changes the version"
    );
}

#[test]
fn uncertain_beyond_threshold_surfaces_attention() {
    let store = SqliteStore::in_memory(1).unwrap();
    let (_w, eid) = submit(&store, "q5");
    store
        .transact(|tx| admit_queued(tx, &config(), &eid, NOW).map(|_| ()))
        .unwrap();
    // Drop into uncertain just now — below the 15-minute threshold.
    let obs = Observation {
        delivery: None,
        subject_work_id: None,
        subject_execution_id: Some(eid.clone()),
        fact: ObservationFact::ProcessState {
            invocation_id: InvocationId("inv-x".into()),
            process_identity: String::new(),
            state: ProcessStateFact::Unknown,
            exit_code: None,
        },
        observed_unix_seconds: NOW,
    };
    workloop_core::work::handle_observation(&store, &config(), &obs, NOW).unwrap();
    assert!(
        found(store.transact(|tx| attention(tx, None, NOW)).unwrap()).is_empty(),
        "below threshold: no attention"
    );
    let later = NOW + 901;
    let items = found(store.transact(|tx| attention(tx, None, later)).unwrap());
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].reason, "uncertain_beyond_threshold");
    assert_eq!(
        items[0].investigation_threshold_seconds,
        Some(15 * 60),
        "threshold recorded"
    );
    assert!(!items[0].next_action.is_empty());
}

#[test]
fn capacity_explains_every_slot_reservation_and_deferral() {
    let store = SqliteStore::in_memory(1).unwrap();
    let (_w1, e1) = submit(&store, "q6");
    let (_w2, e2) = submit(&store, "q7");
    // One free slot: e1 admitted, e2 deferred — the deferral must not
    // block the queue's explanation.
    store
        .transact(|tx| dispatch_once(tx, &config(), NOW).map(|_| ()))
        .unwrap();
    let view = found(store.transact(|tx| capacity(tx)).unwrap());
    assert_eq!(view.used, 1);
    assert_eq!(view.limit, 1);
    assert_eq!(view.holders[0].execution_id, e1);
    assert!(view.reservations.len() >= 2, "workspace+branch held");
    assert!(
        view.reservations
            .iter()
            .all(|r| matches!(r.holder, ReservationHolder::Execution { .. }))
    );
    assert_eq!(view.deferred.len(), 1);
    assert_eq!(view.deferred[0].execution_id, e2);
    assert_eq!(view.deferred[0].reason, "capacity_exhausted");

    // An external claim with an expiry is visible in the same view.
    // Workspace keys are reserved for `execution`, so the external claim
    // names a branch pair.
    let squatter = WriteResource::Branch {
        repository: repo(),
        branch: "squatter".into(),
    };
    let d = handle_command(
        &store,
        &config(),
        &CommandRequest {
            actor: actor(),
            origin: Origin("cli".into()),
            idempotency_key: "q8-claim".into(),
            body: CommandBody::ClaimResources {
                resources: vec![squatter.clone()],
                ttl_seconds: 300,
            },
        },
        NOW,
    )
    .unwrap();
    assert!(matches!(d, CommandDecision::Accepted { .. }), "{d:?}");
    let view = found(store.transact(|tx| capacity(tx)).unwrap());
    let external = view
        .reservations
        .iter()
        .find(|r| r.resource_key == squatter.key())
        .expect("external reservation listed");
    assert!(matches!(
        external.holder,
        ReservationHolder::External { .. }
    ));
    assert_eq!(external.expires_at_unix_seconds, Some(NOW + 300));
}

#[test]
fn command_result_recovers_a_lost_response_by_id_and_replay_key() {
    let store = SqliteStore::in_memory(1).unwrap();
    let request = CommandRequest {
        actor: actor(),
        origin: Origin("cli".into()),
        idempotency_key: "q9-submit".into(),
        body: CommandBody::Submit {
            intent: "x".into(),
            sources: vec![],
            constraints: vec![],
            verification_argv: vec![],
        },
    };
    let decision = handle_command(&store, &config(), &request, NOW).unwrap();
    let command_id = decision.command_id().clone();

    let by_id = store
        .transact(|tx| {
            command_result(
                tx,
                &CommandResultLookup::Id {
                    command_id: command_id.clone(),
                },
            )
        })
        .unwrap();
    assert!(matches!(by_id, QueryResult::Found { .. }));

    let by_key = store
        .transact(|tx| {
            command_result(
                tx,
                &CommandResultLookup::ReplayKey {
                    origin: Origin("cli".into()),
                    actor: actor(),
                    idempotency_key: "q9-submit".into(),
                },
            )
        })
        .unwrap();
    let QueryResult::Found { value, .. } = by_key else {
        panic!("replay key must recover the decision")
    };
    assert_eq!(value.command_id(), &command_id);

    let missing = store
        .transact(|tx| {
            command_result(
                tx,
                &CommandResultLookup::Id {
                    command_id: CommandId("cmd-nope".into()),
                },
            )
        })
        .unwrap();
    assert!(matches!(missing, QueryResult::NotFound));
}

#[test]
fn runtime_limit_is_a_committed_stopping_decision() {
    let store = SqliteStore::in_memory(1).unwrap();
    let (_w, eid) = submit(&store, "q10");
    store
        .transact(|tx| admit_queued(tx, &config(), &eid, NOW).map(|_| ()))
        .unwrap();
    let e = execution(&store, &eid);
    assert_eq!(e.state, ExecutionState::Ready);
    let over = NOW + (6 * 3600) + 1;
    sweep(&store, &config(), over).unwrap();
    let e = execution(&store, &eid);
    assert_eq!(e.state, ExecutionState::Stopping);
    assert_eq!(e.desired_action, Some(DesiredAction::Fail));
    // Queue time does not consume the runtime budget: a queued execution
    // at the same clock is untouched.
    let (_w2, e2) = submit(&store, "q11");
    sweep(&store, &config(), over).unwrap();
    assert_eq!(execution(&store, &e2).state, ExecutionState::Queued);
}

#[test]
fn deferred_head_does_not_block_later_items() {
    let store = SqliteStore::in_memory(2).unwrap();
    // w1 wants the same branch the external actor holds; w2 is free.
    let (_w1, e1) = submit(&store, "q12");
    let (_w2, e2) = submit(&store, "q13");
    handle_command(
        &store,
        &config(),
        &CommandRequest {
            actor: Actor {
                kind: ActorKind::Human,
                external_id: "other-actor".into(),
            },
            origin: Origin("cli".into()),
            idempotency_key: "q14-claim".into(),
            body: CommandBody::ClaimResources {
                resources: vec![WriteResource::Branch {
                    repository: repo(),
                    branch: format!("work/{}", _w1.0),
                }],
                ttl_seconds: 600,
            },
        },
        NOW,
    )
    .unwrap();
    store
        .transact(|tx| dispatch_once(tx, &config(), NOW).map(|_| ()))
        .unwrap();
    assert_eq!(
        execution(&store, &e1).state,
        ExecutionState::Queued,
        "conflicting head stays queued"
    );
    assert_eq!(
        execution(&store, &e2).state,
        ExecutionState::Ready,
        "later independent item was admitted past the deferred head"
    );
}

#[test]
fn expired_external_claim_forfeits_exclusion() {
    let store = SqliteStore::in_memory(2).unwrap();
    let (_w, eid) = submit(&store, "q16");
    handle_command(
        &store,
        &config(),
        &CommandRequest {
            actor: Actor {
                kind: ActorKind::Human,
                external_id: "ext".into(),
            },
            origin: Origin("cli".into()),
            idempotency_key: "q15-claim".into(),
            body: CommandBody::ClaimResources {
                resources: vec![WriteResource::Branch {
                    repository: repo(),
                    branch: format!("work/{}", _w.0),
                }],
                ttl_seconds: 60,
            },
        },
        NOW,
    )
    .unwrap();
    // The claim's TTL lapsed: dispatch reclaims the resource and admits.
    store
        .transact(|tx| dispatch_once(tx, &config(), NOW + 61).map(|_| ()))
        .unwrap();
    assert_eq!(execution(&store, &eid).state, ExecutionState::Ready);
}

/// A store wrapper that fails exactly ONE transaction — the Nth
/// `transact` call — so a test can inject a per-Work store conflict
/// (or an infrastructure outage) at a precise item boundary.
struct Injected<'a, S> {
    inner: &'a S,
    calls: std::cell::Cell<u32>,
    fail_at: u32,
    internal: bool,
}

impl<S: Store> Store for Injected<'_, S> {
    fn transact<R>(
        &self,
        f: impl FnOnce(&mut dyn TxView) -> Result<R, StoreError>,
    ) -> Result<R, StoreError> {
        let n = self.calls.get() + 1;
        self.calls.set(n);
        if n == self.fail_at {
            return Err(if self.internal {
                StoreError::Internal {
                    detail: "injected store outage".into(),
                }
            } else {
                StoreError::ConstraintConflict {
                    detail: "injected per-item conflict".into(),
                }
            });
        }
        self.inner.transact(f)
    }
}

fn two_admitted_executions(tag: &str) -> (SqliteStore, ExecutionId, ExecutionId) {
    let store = SqliteStore::in_memory(2).unwrap();
    let (_w1, e1) = submit(&store, &format!("{tag}-1"));
    let (_w2, e2) = submit(&store, &format!("{tag}-2"));
    for eid in [&e1, &e2] {
        store
            .transact(|tx| admit_queued(tx, &config(), eid, NOW).map(|_| ()))
            .unwrap();
    }
    (store, e1, e2)
}

#[test]
fn one_works_conflict_is_recorded_and_the_sweep_continues() {
    let (store, e1, e2) = two_admitted_executions("sweep-conflict");
    // Call 1 lists the nonterminal executions; call 2 is the first
    // execution's own transaction — it fails with a store conflict.
    let injected = Injected {
        inner: &store,
        calls: std::cell::Cell::new(0),
        fail_at: 2,
        internal: false,
    };
    let over = NOW + (6 * 3600) + 1;
    let report =
        sweep(&injected, &config(), over).expect("a per-Work conflict is not a tick error");

    assert_eq!(report.examined, 2, "both Works were examined");
    assert_eq!(
        report.stops_recorded, 1,
        "the later Work still gets its decision: {report:?}"
    );
    assert_eq!(report.conflicts, vec![e1.clone()]);
    // The conflicted Work is untouched — its attempt rolled back —
    // while the independent Work's stop is committed.
    assert_eq!(execution(&store, &e1).state, ExecutionState::Ready);
    assert_eq!(execution(&store, &e2).state, ExecutionState::Stopping);
    // The conflict is recorded against the Work, never swallowed.
    let deferrals = store.transact(|tx| tx.deferrals()).unwrap();
    let recorded = deferrals
        .iter()
        .find(|d| d.execution_id == e1)
        .expect("the conflict is recorded against the Work");
    assert_eq!(recorded.reason, "sweep_conflict");
}

#[test]
fn a_sweep_conflict_deferral_is_cleared_when_the_sweep_next_reads_cleanly() {
    let (store, e1, _e2) = two_admitted_executions("sweep-clear");
    let injected = Injected {
        inner: &store,
        calls: std::cell::Cell::new(0),
        fail_at: 2,
        internal: false,
    };
    let report = sweep(&injected, &config(), NOW).expect("a per-Work conflict is not a tick error");
    assert_eq!(report.conflicts, vec![e1.clone()]);
    assert!(
        store
            .transact(|tx| tx.deferrals())
            .unwrap()
            .iter()
            .any(|d| d.execution_id == e1 && d.reason == "sweep_conflict"),
        "the conflict is recorded first"
    );

    // The next pass reads the same Work cleanly: the transient conflict
    // is resolved, not left on the capacity view forever.
    sweep(&store, &config(), NOW).unwrap();
    assert!(
        store
            .transact(|tx| tx.deferrals())
            .unwrap()
            .iter()
            .all(|d| d.execution_id != e1),
        "a clean sweep resolves the recorded conflict"
    );
}

#[test]
fn a_sweep_conflict_deferral_is_cleared_when_the_sweep_records_a_stop() {
    let (store, e1, _e2) = two_admitted_executions("sweep-clear-stop");
    let injected = Injected {
        inner: &store,
        calls: std::cell::Cell::new(0),
        fail_at: 2,
        internal: false,
    };
    let over = NOW + (6 * 3600) + 1;
    sweep(&injected, &config(), over).expect("a per-Work conflict is not a tick error");
    assert!(
        store
            .transact(|tx| tx.deferrals())
            .unwrap()
            .iter()
            .any(|d| d.execution_id == e1)
    );

    let report = sweep(&store, &config(), over).unwrap();
    assert_eq!(report.stops_recorded, 1, "the later pass stops the Work");
    assert_eq!(execution(&store, &e1).state, ExecutionState::Stopping);
    assert!(
        store
            .transact(|tx| tx.deferrals())
            .unwrap()
            .iter()
            .all(|d| d.execution_id != e1),
        "a recorded stop is no longer a deferred Execution"
    );
}

#[test]
fn an_infrastructure_failure_still_fails_the_sweep() {
    let (store, _e1, _e2) = two_admitted_executions("sweep-outage");
    let injected = Injected {
        inner: &store,
        calls: std::cell::Cell::new(0),
        fail_at: 2,
        internal: true,
    };
    let over = NOW + (6 * 3600) + 1;
    let err = sweep(&injected, &config(), over).unwrap_err();
    assert!(matches!(err, StoreError::Internal { .. }), "{err}");
}
