//! Contract tests for the `rui-evidence/1` producer: the exporter
//! replays the append-only `event_log`, pins an open page sequence to
//! its watermark, emits gaps instead of fabricating facts, and serves
//! artifact bytes exactly. Every page is checked against the frozen
//! `workloop-evidence-protocol` validator — the same rules the local
//! consumer enforces.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use workloop_core::command::{FindingChoice, FindingDecision};
use workloop_core::domain::*;
use workloop_core::identity::*;
use workloop_core::store::Effect;
use workloop_evidence_protocol::*;
use workloop_store::evidence;
use workloop_store::sqlite::SqliteStore;

const NOW: i64 = 1_700_000_000;

fn db_path(tag: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!(
        "workloop-evidence-{}-{}.db",
        tag,
        std::process::id()
    ));
    let _ = std::fs::remove_file(&p);
    p
}

fn host() -> HostInstanceId {
    HostInstanceId::trusted("test-host")
}

fn repo() -> RepositoryRef {
    RepositoryRef::new(host(), RepositoryId::trusted("local-repo"))
}

fn actor() -> Actor {
    Actor {
        kind: ActorKind::Human,
        external_id: "tester".into(),
    }
}

fn binding() -> HostBinding {
    HostBinding::new(
        AdapterId::trusted("fixture"),
        repo(),
        1,
        [],
        vec![RoleBinding {
            role: ActorRole::Publication,
            actor: HostActorRef {
                host: host(),
                actor: HostActorId::trusted("pub-agent"),
            },
            credential_ref: CredentialRef::trusted("cred"),
        }],
    )
    .unwrap()
}

fn spec(work_id: &str, revision: u64) -> SpecSnapshot {
    SpecSnapshot {
        work_id: WorkId(work_id.into()),
        revision,
        payload: SpecPayload {
            intent: "synthetic intent".into(),
            sources: vec![],
            constraints: vec!["no network".into()],
            verification_argv: vec![vec!["cargo".into(), "test".into()]],
            required_checks: vec![],
            lanes: LanePolicy::default(),
        },
        digest: "d".repeat(64),
        approved_by: actor(),
        recorded_seq: 0,
    }
}

fn insert_work(work_id: &str) -> Effect {
    Effect::InsertWork {
        work: Work {
            id: WorkId(work_id.into()),
            version: 1,
            binding: binding(),
            spec: 1,
            approved_spec: 1,
            source_gate: SourceGate::NotApplicable,
            policy: WorkPolicy::default(),
            created_seq: 0,
        },
        spec: spec(work_id, 1),
        issue_refs: vec![],
        claims: vec![],
        usage: WorkUsage::new(&WorkId(work_id.into())),
    }
}

fn execution(exec_id: &str, work_id: &str, state: ExecutionState) -> Execution {
    Execution {
        id: ExecutionId(exec_id.into()),
        work_id: WorkId(work_id.into()),
        ordinal: 0,
        state,
        waiting_reason: None,
        desired_action: None,
        terminal_code: None,
        pause_reason: None,
        repair_prerequisites: vec![],
        outcome: None,
        enqueued_seq: 0,
        allowed_effects: AllowedEffects {
            workspace_write: true,
            host_write: false,
            network: false,
        },
        input: InvocationInput {
            input: serde_json::Value::Null,
            resume: false,
            next_execution_input: None,
            previous_execution_outcome: None,
        },
        budget: ExecutionBudget::default(),
        invocations_used: 0,
        review_rounds_used: 0,
        started_seq: None,
        started_unix_seconds: None,
        recovery_attempts_used: 0,
        recovery_deadline_unix_seconds: None,
        uncertain_since_unix_seconds: None,
        paused_since_unix_seconds: None,
        next_role: None,
    }
}

fn request(req_id: &str, work_id: &str, exec_id: &str, role: InvocationRole) -> Effect {
    Effect::InsertRequest {
        request: InvocationRequest {
            request_id: req_id.into(),
            work_id: WorkId(work_id.into()),
            execution_id: ExecutionId(exec_id.into()),
            role,
            spec_revision: 1,
            payload: serde_json::Value::Null,
            adapter_binding: None,
        },
    }
}

fn invocation(inv_id: &str, req_id: &str, exec_id: &str) -> Effect {
    Effect::InsertInvocation {
        invocation: Invocation {
            id: InvocationId(inv_id.into()),
            request_id: req_id.into(),
            execution_id: ExecutionId(exec_id.into()),
            role: InvocationRole::Coding,
            state: InvocationState::Done,
            process_identity: None,
            descendants: vec![],
            required_artifacts: vec![],
            evidence_refs: vec![],
        },
    }
}

fn handoff(
    artifact: &str,
    req_id: &str,
    work_id: &str,
    exec_id: &str,
    produced: Option<&str>,
    state: HandoffState,
) -> Effect {
    Effect::UpsertHandoff {
        handoff: ArtifactHandoff {
            artifact_id: artifact.into(),
            invocation_id: InvocationId(format!("inv-{req_id}")),
            request_id: req_id.into(),
            work_id: WorkId(work_id.into()),
            execution_id: ExecutionId(exec_id.into()),
            base: "base-1".into(),
            produced: produced.map(str::to_string),
            manifest_digest: None,
            state,
            invalid_reason: None,
            attempt: 0,
            recorded_seq: 0,
        },
        expected_seq: 0,
    }
}

fn head(object: &str) -> CommitRevision {
    CommitRevision::new(repo(), object).unwrap()
}

fn round(round_id: &str, work_id: &str, head: &str, occurrences: &[&str]) -> Effect {
    Effect::InsertReviewRound {
        round: ReviewRound {
            id: round_id.into(),
            work_id: WorkId(work_id.into()),
            target: DiffTarget {
                change: ChangeRequestRef {
                    repository: repo(),
                    change_request: ChangeRequestId::trusted("cr-1"),
                    display_number: Some(1),
                },
                head: crate_head(head),
                base: None,
                target_revision: None,
            },
            source: "reviewer".into(),
            source_revision: None,
            completeness: "complete".into(),
            set_digest: "s".repeat(64),
            generation: 1,
            seq: 0,
        },
        occurrences: occurrences
            .iter()
            .map(|id| FindingOccurrence {
                id: id.to_string(),
                round_id: round_id.into(),
                source_key: format!("key-{id}"),
                content_digest: None,
                severity: Some("warn".into()),
                position: None,
                thread_id: None,
            })
            .collect(),
    }
}

fn crate_head(object: &str) -> CommitRevision {
    head(object)
}

fn judgment(
    judgment_id: &str,
    round_id: &str,
    work_id: &str,
    decisions: &[(&str, FindingChoice)],
) -> Effect {
    Effect::InsertRoundJudgment {
        judgment: RoundJudgment {
            id: judgment_id.into(),
            work_id: WorkId(work_id.into()),
            round_id: round_id.into(),
            set_digest: "s".repeat(64),
            decisions: decisions
                .iter()
                .map(|(occ, choice)| FindingDecision {
                    occurrence_id: occ.to_string(),
                    choice: choice.clone(),
                    rationale: None,
                })
                .collect(),
            actor_id: "tester".into(),
            seq: 0,
        },
    }
}

fn verification(vid: &str, artifact: &str, produced: &str, outcome: VerificationOutcome) -> Effect {
    Effect::InsertVerificationResult {
        result: VerificationResult {
            id: vid.into(),
            artifact_id: artifact.into(),
            requirement: "verification_argv[0]".into(),
            argv: vec!["cargo".into(), "test".into()],
            produced: produced.into(),
            environment_digest: "e".repeat(64),
            result: outcome,
            output: "ok".into(),
            recorded_seq: 0,
        },
    }
}

fn append(store: &SqliteStore, effects: &[Effect]) {
    for e in effects {
        store
            .append_event("effect", &serde_json::to_string(e).unwrap())
            .unwrap();
    }
}

/// A synthetic but complete history: work → execution → coding
/// invocation → produced head → review round with one finding → fix
/// judgment → fix request + handoff → passing verification → terminal
/// execution with a recorded outcome.
fn full_history() -> Vec<Effect> {
    let mut exec = execution("e-1", "w-1", ExecutionState::Succeeded);
    exec.outcome = Some(workloop_core::observation::Outcome {
        state: ExecutionState::Succeeded,
        code: None,
        detail: None,
        evidence_refs: vec![],
        recorded_seq: 12,
    });
    vec![
        insert_work("w-1"),
        Effect::InsertExecution {
            execution: execution("e-1", "w-1", ExecutionState::Running),
        },
        request("req-c1", "w-1", "e-1", InvocationRole::Coding),
        invocation("inv-c1", "req-c1", "e-1"),
        handoff(
            "art-c1",
            "req-c1",
            "w-1",
            "e-1",
            Some("head-1"),
            HandoffState::Acknowledged,
        ),
        round("round-1", "w-1", "head-1", &["occ-1"]),
        judgment("j-1", "round-1", "w-1", &[("occ-1", FindingChoice::Fix)]),
        request("req-f1", "w-1", "e-1", InvocationRole::Fix),
        invocation("inv-f1", "req-f1", "e-1"),
        handoff(
            "art-f1",
            "req-f1",
            "w-1",
            "e-1",
            Some("head-2"),
            HandoffState::Acknowledged,
        ),
        verification("v-1", "art-f1", "head-2", VerificationOutcome::Pass),
        Effect::UpdateExecution {
            execution: exec,
            expected_state: ExecutionState::Running,
        },
    ]
}

fn snapshot(store: &SqliteStore) -> evidence::HistorySnapshot {
    store.evidence_history().unwrap()
}

fn artifact_bytes(snap: &evidence::HistorySnapshot) -> BTreeMap<String, Vec<u8>> {
    snap.events
        .iter()
        .map(|e| (format!("event:{}", e.seq), e.payload.clone().into_bytes()))
        .collect()
}

/// Drain an export to completion, validating every page against the
/// frozen validator and the cumulative snapshot rules.
fn drain(store: &SqliteStore, limit: u16) -> (Vec<(ExportRequest, ExportPage)>, ValidatedSnapshot) {
    let snap = snapshot(store);
    let mut pages = Vec::new();
    let mut req = ExportRequest {
        protocol_version: ProtocolVersion::V1,
        after: None,
        cursor: None,
        limit,
    };
    loop {
        let page = evidence::export_page(&req, &snap, NOW).unwrap();
        validate_page(&req, &page).unwrap();
        let done = page.checkpoint.is_some();
        let cursor = page.next_cursor.clone();
        pages.push((req.clone(), page));
        if done {
            break;
        }
        req.cursor = cursor;
    }
    let snapshot = validate_snapshot(&pages, None, &artifact_bytes(&snap)).unwrap();
    (pages, snapshot)
}

#[test]
fn export_projects_recorded_history_not_the_projection() {
    let store = SqliteStore::open(&db_path("full"), 2).unwrap();
    append(&store, &full_history());
    let (pages, snapshot) = drain(&store, 500);
    assert_eq!(pages.len(), 1);
    let page = &pages[0].1;
    assert_eq!(page.retained_from_seq, 1);
    assert_eq!(page.snapshot_seq, 12);
    assert!(page.checkpoint.is_some());
    // Every entity kind the wire vocabulary names is exported.
    let types: BTreeSet<_> = snapshot
        .unsupported_links()
        .iter()
        .map(|r| r.source_id.clone())
        .collect();
    assert!(types.is_empty(), "all links resolved: {types:?}");
    let entity_types: BTreeSet<EntityType> = page
        .entities
        .iter()
        .map(|r| {
            let body = parse_record(r, RecordKind::Entity).unwrap();
            match body.data {
                RecordData::Entity { entity } => entity.entity_type(),
                _ => unreachable!(),
            }
        })
        .collect();
    for t in [
        EntityType::Work,
        EntityType::Execution,
        EntityType::Invocation,
        EntityType::ReviewRound,
        EntityType::FindingOccurrence,
        EntityType::RepairAttempt,
        EntityType::Verification,
        EntityType::Outcome,
        EntityType::Requirement,
        EntityType::Invariant,
    ] {
        assert!(entity_types.contains(&t), "missing {t:?}");
    }
}

#[test]
fn two_findings_are_immutable_revisions() {
    let store = SqliteStore::open(&db_path("corr"), 2).unwrap();
    let effects = vec![
        insert_work("w-1"),
        Effect::InsertExecution {
            execution: execution("e-1", "w-1", ExecutionState::Running),
        },
        request("req-c1", "w-1", "e-1", InvocationRole::Coding),
        handoff(
            "art-c1",
            "req-c1",
            "w-1",
            "e-1",
            Some("head-1"),
            HandoffState::Acknowledged,
        ),
        round("round-1", "w-1", "head-1", &["occ-1"]),
        judgment("j-1", "round-1", "w-1", &[("occ-1", FindingChoice::Fix)]),
        // A correction: the same occurrence re-judged — never a rewrite.
        judgment(
            "j-2",
            "round-1",
            "w-1",
            &[("occ-1", FindingChoice::Dismiss)],
        ),
    ];
    append(&store, &effects);
    let (pages, _) = drain(&store, 500);
    let dispositions: Vec<Disposition> = pages[0]
        .1
        .entities
        .iter()
        .filter_map(|r| {
            let body = parse_record(r, RecordKind::Entity).unwrap();
            match body.data {
                RecordData::Entity {
                    entity:
                        EntityPayload::FindingOccurrence {
                            disposition,
                            occurrence_id,
                            ..
                        },
                } if occurrence_id == "occ-1" => Some(disposition),
                _ => None,
            }
        })
        .collect();
    assert_eq!(
        dispositions,
        vec![
            Disposition::Unknown,
            Disposition::Open,
            Disposition::Dismissed
        ]
    );
}

#[test]
fn writes_between_pages_do_not_perturb_an_open_sequence() {
    let store = SqliteStore::open(&db_path("iso"), 2).unwrap();
    append(&store, &full_history());
    let first_snap = snapshot(&store);
    let req1 = ExportRequest {
        protocol_version: ProtocolVersion::V1,
        after: None,
        cursor: None,
        limit: 2,
    };
    let page1 = evidence::export_page(&req1, &first_snap, NOW).unwrap();
    validate_page(&req1, &page1).unwrap();
    let cursor = page1.next_cursor.clone().unwrap();
    // A new recorded fact lands after the watermark was pinned.
    append(
        &store,
        &[Effect::InsertExecution {
            execution: execution("e-2", "w-1", ExecutionState::Queued),
        }],
    );
    // Continue the sequence against a FRESH snapshot read: the cursor
    // still replays the pinned watermark, so the post-watermark
    // execution never appears.
    let second_snap = snapshot(&store);
    let mut req = ExportRequest {
        protocol_version: ProtocolVersion::V1,
        after: None,
        cursor: Some(cursor),
        limit: 2,
    };
    let mut pages = vec![(req1.clone(), page1.clone())];
    loop {
        let page = evidence::export_page(&req, &second_snap, NOW).unwrap();
        validate_page(&req, &page).unwrap();
        let cursor = page.next_cursor.clone();
        pages.push((req.clone(), page));
        if cursor.is_none() {
            break;
        }
        req.cursor = cursor;
    }
    let ids: BTreeSet<&str> = pages
        .iter()
        .flat_map(|(_, p)| p.entities.iter().chain(p.links.iter()))
        .map(|r| r.source_id.as_str())
        .collect();
    assert!(!ids.contains("exec:e-2"));
    validate_snapshot(&pages, None, &artifact_bytes(&first_snap)).unwrap();
    // The committed checkpoint resumes the delta: e-2 appears exactly
    // there, at the new watermark.
    let checkpoint = pages.last().unwrap().1.checkpoint.clone().unwrap();
    let delta_req = ExportRequest {
        protocol_version: ProtocolVersion::V1,
        after: Some(checkpoint),
        cursor: None,
        limit: 500,
    };
    let delta = evidence::export_page(&delta_req, &second_snap, NOW).unwrap();
    validate_page(&delta_req, &delta).unwrap();
    assert_eq!(delta.after_seq, Some(pages[0].1.snapshot_seq));
    assert!(delta.entities.iter().any(|r| r.source_id == "exec:e-2"));
    let history = validate_snapshot(&pages, None, &artifact_bytes(&second_snap)).unwrap();
    validate_snapshot(
        &[(delta_req, delta)],
        Some(&history),
        &artifact_bytes(&second_snap),
    )
    .unwrap();
}

#[test]
fn cursor_retry_returns_the_same_logical_records() {
    let store = SqliteStore::open(&db_path("retry"), 2).unwrap();
    append(&store, &full_history());
    let snap = snapshot(&store);
    let req1 = ExportRequest {
        protocol_version: ProtocolVersion::V1,
        after: None,
        cursor: None,
        limit: 3,
    };
    let p1 = evidence::export_page(&req1, &snap, NOW).unwrap();
    let req2 = ExportRequest {
        cursor: p1.next_cursor.clone(),
        ..req1.clone()
    };
    let a = evidence::export_page(&req2, &snap, NOW).unwrap();
    let b = evidence::export_page(&req2, &snap, NOW).unwrap();
    assert_eq!(a, b);
    // Exactly-once across the whole sequence.
    let (pages, _) = drain(&store, 3);
    let mut keys = BTreeSet::new();
    for (_, p) in &pages {
        for r in p.entities.iter().chain(p.links.iter()) {
            assert!(
                keys.insert((r.change_seq, &r.source_id, &r.revision)),
                "duplicate record {} {}",
                r.source_id,
                r.revision
            );
        }
    }
}

#[test]
fn token_binding_rejects_wrong_installation_epoch_and_expiry() {
    let store = SqliteStore::open(&db_path("bind"), 2).unwrap();
    append(&store, &full_history());
    let snap = snapshot(&store);
    let base = ExportToken {
        kind: TokenKind::Cursor,
        installation_id: snap.installation_id.clone(),
        store_epoch: snap.store_epoch.clone(),
        watermark: snap.watermark,
        retained_from_seq: snap.retained_from_seq,
        after_seq: None,
        snapshot_id: "s".into(),
        position: 0,
        gap_position: 0,
        minted_unix_seconds: NOW,
    };
    let ask = |token: &ExportToken| {
        let req = ExportRequest {
            protocol_version: ProtocolVersion::V1,
            after: None,
            cursor: Some(token.encode()),
            limit: 10,
        };
        evidence::export_page(&req, &snap, NOW).unwrap_err().code
    };
    assert_eq!(
        ask(&ExportToken {
            installation_id: "other".into(),
            ..base.clone()
        }),
        ErrorCode::InstallationMismatch
    );
    assert_eq!(
        ask(&ExportToken {
            store_epoch: "9".into(),
            ..base.clone()
        }),
        ErrorCode::EpochMismatch
    );
    assert_eq!(
        ask(&ExportToken {
            minted_unix_seconds: NOW - SNAPSHOT_TTL_SECONDS - 1,
            ..base.clone()
        }),
        ErrorCode::SnapshotExpired
    );
    // A checkpoint whose watermark fell below retained history is
    // expired; the producer never mints one, but a stale caller-held
    // token must still fail closed.
    let stale = ExportToken {
        kind: TokenKind::Checkpoint,
        watermark: 0,
        retained_from_seq: 0,
        ..base.clone()
    };
    let req = ExportRequest {
        protocol_version: ProtocolVersion::V1,
        after: Some(stale.encode()),
        cursor: None,
        limit: 10,
    };
    assert_eq!(
        evidence::export_page(&req, &snap, NOW).unwrap_err().code,
        ErrorCode::CheckpointExpired
    );
    // A token pinned above the store's current watermark names
    // history a restore or rewind removed: it expires rather than
    // serving a stale page or an empty delta.
    assert_eq!(
        ask(&ExportToken {
            watermark: snap.watermark + 1,
            ..base.clone()
        }),
        ErrorCode::SnapshotExpired
    );
    let future_checkpoint = ExportToken {
        kind: TokenKind::Checkpoint,
        watermark: snap.watermark + 1,
        ..base.clone()
    };
    let req = ExportRequest {
        protocol_version: ProtocolVersion::V1,
        after: Some(future_checkpoint.encode()),
        cursor: None,
        limit: 10,
    };
    assert_eq!(
        evidence::export_page(&req, &snap, NOW).unwrap_err().code,
        ErrorCode::CheckpointExpired
    );
    // Kind confusion: a checkpoint is not a cursor.
    let wrong_kind = ExportToken {
        kind: TokenKind::Checkpoint,
        ..base.clone()
    };
    let req = ExportRequest {
        protocol_version: ProtocolVersion::V1,
        after: None,
        cursor: Some(wrong_kind.encode()),
        limit: 10,
    };
    assert_eq!(
        evidence::export_page(&req, &snap, NOW).unwrap_err().code,
        ErrorCode::InvalidCursor
    );
}

#[test]
fn missing_recorded_facts_emit_gaps_never_fabrication() {
    let store = SqliteStore::open(&db_path("gaps"), 2).unwrap();
    append(
        &store,
        &[
            insert_work("w-1"),
            // A round whose reviewed head was never produced by a
            // recorded handoff: no execution binding exists.
            round("round-x", "w-1", "never-produced", &["occ-x"]),
        ],
    );
    let (pages, snapshot) = drain(&store, 500);
    let page = &pages[0].1;
    // The round itself is not a record — only its finding (named by
    // its own immutable identity) plus the gaps carrying what is
    // missing.
    assert!(
        !page.entities.iter().any(|r| r.source_id == "round:round-x"),
        "the unbound round must not fabricate an execution"
    );
    assert!(page.gaps.iter().any(|g| {
        matches!(
            &g.scope,
            GapScope::Revision { record } if record.source_id == "round:round-x"
        )
    }));
    // The finding's body was never retained: a body gap names it.
    assert!(page.gaps.iter().any(|g| {
        g.evidence_class == EvidenceClass::Body
            && matches!(
                &g.scope,
                GapScope::Revision { record } if record.source_id == "finding:occ-x"
            )
    }));
    // Its round endpoint is a relation gap, not an invented ref.
    assert!(page.gaps.iter().any(|g| {
        g.evidence_class == EvidenceClass::Relation
            && matches!(
                &g.scope,
                GapScope::Revision { record } if record.source_id == "round:round-x"
            )
    }));
    assert!(!snapshot.unsupported_links().is_empty());
}

#[test]
fn artifact_retrieval_returns_exact_retained_bytes() {
    let store = SqliteStore::open(&db_path("art"), 2).unwrap();
    append(&store, &full_history());
    let snap = snapshot(&store);
    let event = &snap.events[1];
    let req = ArtifactRequest {
        protocol_version: ProtocolVersion::V1,
        artifact_id: format!("event:{}", event.seq),
        sha256: sha256(event.payload.as_bytes()),
    };
    let (descriptor, bytes) = evidence::artifact_bytes(&req, &snap).unwrap();
    assert_eq!(bytes, event.payload.as_bytes());
    assert_eq!(descriptor.sha256, req.sha256);
    assert_eq!(descriptor.byte_length, bytes.len() as u64);
    assert_eq!(descriptor.availability, Availability::Available);
    validate_descriptor(&descriptor).unwrap();
    validate_artifact(&descriptor, &bytes).unwrap();
    // Wrong digest → digest_mismatch; unknown id → artifact_unavailable.
    let mut bad = req.clone();
    bad.sha256 = "0".repeat(64);
    assert_eq!(
        evidence::artifact_bytes(&bad, &snap).unwrap_err().code,
        ErrorCode::DigestMismatch
    );
    let mut missing = req.clone();
    missing.artifact_id = "event:9999".into();
    assert_eq!(
        evidence::artifact_bytes(&missing, &snap).unwrap_err().code,
        ErrorCode::ArtifactUnavailable
    );
    let mut foreign = req.clone();
    foreign.artifact_id = "other-namespace:1".into();
    assert_eq!(
        evidence::artifact_bytes(&foreign, &snap).unwrap_err().code,
        ErrorCode::ArtifactUnavailable
    );
    // A non-canonical alias parses to the same seq but names no
    // retained artifact: the descriptor id is minted from the seq, so
    // serving it would fail request/result id equality downstream.
    let mut alias = req.clone();
    alias.artifact_id = format!("event:0{}", event.seq);
    assert_eq!(
        evidence::artifact_bytes(&alias, &snap).unwrap_err().code,
        ErrorCode::ArtifactUnavailable
    );
    assert_eq!(
        evidence::artifact_from_payload(
            &alias,
            &snap.installation_id,
            &snap.store_epoch,
            event.seq,
            &event.payload,
        )
        .unwrap_err()
        .code,
        ErrorCode::ArtifactUnavailable
    );
    let (d, b) = evidence::artifact_from_payload(
        &req,
        &snap.installation_id,
        &snap.store_epoch,
        event.seq,
        &event.payload,
    )
    .unwrap();
    assert_eq!(d.artifact_id, req.artifact_id);
    assert_eq!(b, event.payload.as_bytes());
}

#[test]
fn export_never_writes_or_needs_the_projection() {
    let store = SqliteStore::open(&db_path("ro"), 2).unwrap();
    append(&store, &full_history());
    let before = store.count_events("effect").unwrap();
    let _ = drain(&store, 2);
    assert_eq!(store.count_events("effect").unwrap(), before);
    // Concurrent readers share the connection: two interleaved exports
    // see identical record content — each is its own export sequence,
    // so only the snapshot identity and its tokens may differ.
    let snap = snapshot(&store);
    let req = ExportRequest {
        protocol_version: ProtocolVersion::V1,
        after: None,
        cursor: None,
        limit: 5,
    };
    let a = evidence::export_page(&req, &snap, NOW).unwrap();
    let snap2 = snapshot(&store);
    let b = evidence::export_page(&req, &snap2, NOW).unwrap();
    assert_ne!(a.snapshot_id, b.snapshot_id);
    assert_eq!(a.entities, b.entities);
    assert_eq!(a.links, b.links);
    assert_eq!(a.artifacts, b.artifacts);
    assert_eq!(a.gaps, b.gaps);
    assert_eq!(a.snapshot_seq, b.snapshot_seq);
    assert_eq!(a.retained_from_seq, b.retained_from_seq);
    assert_eq!(a.after_seq, b.after_seq);
}

#[test]
fn idle_deltas_mint_fresh_snapshot_ids() {
    // Two --after polls with no writes between them produce the same
    // binding (after=W, watermark=W); the contract requires each new
    // export sequence to carry a snapshot id unseen in the validated
    // chain, so the producer must mint fresh ids.
    let store = SqliteStore::open(&db_path("idle"), 2).unwrap();
    append(&store, &full_history());
    let snap = snapshot(&store);
    let artifact_bytes = artifact_bytes(&snap);
    let full_req = ExportRequest {
        protocol_version: ProtocolVersion::V1,
        after: None,
        cursor: None,
        limit: 500,
    };
    let full_page = evidence::export_page(&full_req, &snap, NOW).unwrap();
    validate_page(&full_req, &full_page).unwrap();
    let history = validate_snapshot(
        &[(full_req.clone(), full_page.clone())],
        None,
        &artifact_bytes,
    )
    .unwrap();
    let mut checkpoint = full_page.checkpoint.clone().unwrap();
    let mut history = history;
    for _ in 0..2 {
        let req = ExportRequest {
            protocol_version: ProtocolVersion::V1,
            after: Some(checkpoint.clone()),
            cursor: None,
            limit: 500,
        };
        let page = evidence::export_page(&req, &snap, NOW).unwrap();
        validate_page(&req, &page).unwrap();
        history =
            validate_snapshot(&[(req, page.clone())], Some(&history), &artifact_bytes).unwrap();
        checkpoint = page.checkpoint.clone().unwrap();
    }
}

#[test]
fn derived_bindings_cite_their_supporting_events() {
    let store = SqliteStore::open(&db_path("support"), 2).unwrap();
    append(&store, &full_history());
    let (pages, _snapshot) = drain(&store, 500);
    let bodies: Vec<RecordBody> = pages
        .iter()
        .flat_map(|(_, p)| {
            p.entities
                .iter()
                .map(|r| parse_record(r, RecordKind::Entity).unwrap())
                .chain(
                    p.links
                        .iter()
                        .map(|r| parse_record(r, RecordKind::Link).unwrap()),
                )
        })
        .collect();
    let cited = |source_id: &str| -> BTreeSet<String> {
        bodies
            .iter()
            .find(|b| b.source_id == source_id)
            .map(|b| {
                b.source_refs
                    .iter()
                    .map(|s| s.artifact_id.clone())
                    .collect()
            })
            .unwrap_or_default()
    };
    let cited_relation = |relation: Relation| -> BTreeSet<String> {
        bodies
            .iter()
            .find(|b| matches!(&b.data, RecordData::Link { relation: r, .. } if *r == relation))
            .map(|b| {
                b.source_refs
                    .iter()
                    .map(|s| s.artifact_id.clone())
                    .collect()
            })
            .unwrap_or_default()
    };
    // full_history seqs: 5 = coding handoff (produced head-1),
    // 6 = review round, 7 = fix judgment, 10 = fix handoff.
    // The round's execution binding derives from the produced-head
    // handoff, so the round entity and its link cite that event.
    assert_eq!(
        cited("round:round-1"),
        BTreeSet::from(["event:5".to_string(), "event:6".to_string()])
    );
    assert_eq!(
        cited_relation(Relation::ExecutionHasRound),
        BTreeSet::from(["event:5".to_string(), "event:6".to_string()])
    );
    // The repair's round and addressed-finding bindings derive from
    // the authorizing judgment, so the attempt entity and its links
    // cite the judgment event alongside the minting handoff.
    assert_eq!(
        cited("repair:art-f1"),
        BTreeSet::from(["event:7".to_string(), "event:10".to_string()])
    );
    for relation in [Relation::RoundHasRepair, Relation::RepairAddressesFinding] {
        assert_eq!(
            cited_relation(relation),
            BTreeSet::from(["event:7".to_string(), "event:10".to_string()]),
            "{relation:?}"
        );
    }
    // The review-scope outcome's execution binding cites the same
    // produced-head handoff as the round's.
    assert_eq!(
        cited("outcome:review:j-1"),
        BTreeSet::from(["event:5".to_string(), "event:7".to_string()])
    );
    // Recorded-fact links keep citing only their minting event.
    assert_eq!(
        cited_relation(Relation::ExecutionHasRepair),
        BTreeSet::from(["event:10".to_string()])
    );
}

#[test]
fn store_epoch_is_a_durable_history_identity() {
    // The epoch is minted once per store file and survives reopen —
    // it is the history namespace tokens bind, not the schema version,
    // so additive migrations must not invalidate live cursors.
    let path = db_path("epoch");
    let store = SqliteStore::open(&path, 2).unwrap();
    append(&store, &full_history());
    let epoch = store.evidence_identity().unwrap().1;
    assert_eq!(snapshot(&store).store_epoch, epoch);
    drop(store);
    let store = SqliteStore::open(&path, 2).unwrap();
    assert_eq!(store.evidence_identity().unwrap().1, epoch);
}
