//! CLI contract tests for `evidence export` / `evidence artifact`
//! (proposal T3): exactly one frozen `QueryEnvelope` on stdout, the
//! documented exit classes, boundary validation for paths and digests,
//! and safe artifact publication. Pages are checked by the same
//! `decode_export_envelope` / `validate_snapshot` /
//! `validate_artifact_result` functions the frozen `rui-evidence-v1`
//! fixture suite exercises.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use workloop_core::command::{FindingChoice, FindingDecision};
use workloop_core::domain::*;
use workloop_core::identity::*;
use workloop_core::store::Effect;
use workloop_evidence_protocol::*;
use workloop_store::sqlite::SqliteStore;

fn bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_workloop-cli"))
}

fn root(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "workloop-evidence-cli-{}-{}",
        tag,
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn output_at(root: &Path, args: &[&str]) -> Output {
    let db = root.join("workloop.db");
    let config = root.join("config.json");
    let mut full = vec![
        "--db".into(),
        db.to_string_lossy().into_owned(),
        "--config".into(),
        config.to_string_lossy().into_owned(),
    ];
    full.extend(args.iter().map(|s| s.to_string()));
    Command::new(bin())
        .args(&full)
        .output()
        .expect("taisan runs")
}

fn export_envelope(output: &Output) -> QueryEnvelope<ExportPage> {
    decode_export_envelope(&output.stdout)
        .unwrap_or_else(|e| panic!("stdout is a valid export envelope ({e:?})"))
}

// --- a recorded history (same shape as the store contract suite) ---

fn host() -> HostInstanceId {
    HostInstanceId::trusted("test-host")
}

fn repo() -> RepositoryRef {
    RepositoryRef::new(host(), RepositoryId::trusted("local-repo"))
}

fn populate(db: &Path) {
    let store = SqliteStore::open(db, 2).unwrap();
    let binding = HostBinding::new(
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
    .unwrap();
    let work = Work {
        id: WorkId("w-1".into()),
        version: 1,
        binding,
        spec: 1,
        approved_spec: 1,
        source_gate: SourceGate::NotApplicable,
        policy: WorkPolicy::default(),
        created_seq: 0,
    };
    let spec = SpecSnapshot {
        work_id: work.id.clone(),
        revision: 1,
        payload: SpecPayload {
            intent: "synthetic intent".into(),
            sources: vec![],
            constraints: vec!["no network".into()],
            verification_argv: vec![vec!["cargo".into(), "test".into()]],
            required_checks: vec![],
            lanes: LanePolicy::default(),
        },
        digest: "d".repeat(64),
        approved_by: Actor {
            kind: ActorKind::Human,
            external_id: "tester".into(),
        },
        recorded_seq: 0,
    };
    let mut execution = Execution {
        id: ExecutionId("e-1".into()),
        work_id: work.id.clone(),
        ordinal: 0,
        state: ExecutionState::Running,
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
    };
    let effects = vec![
        Effect::InsertWork {
            work: work.clone(),
            spec,
            issue_refs: vec![],
            claims: vec![],
            usage: WorkUsage::new(&work.id),
        },
        Effect::InsertExecution {
            execution: execution.clone(),
        },
        Effect::InsertRequest {
            request: InvocationRequest {
                request_id: "req-c1".into(),
                work_id: work.id.clone(),
                execution_id: execution.id.clone(),
                role: InvocationRole::Coding,
                spec_revision: 1,
                payload: serde_json::Value::Null,
                adapter_binding: None,
            },
        },
        Effect::InsertInvocation {
            invocation: Invocation {
                id: InvocationId("inv-c1".into()),
                request_id: "req-c1".into(),
                execution_id: execution.id.clone(),
                role: InvocationRole::Coding,
                state: InvocationState::Done,
                process_identity: None,
                descendants: vec![],
                required_artifacts: vec![],
                evidence_refs: vec![],
            },
        },
        Effect::UpsertHandoff {
            handoff: ArtifactHandoff {
                artifact_id: "art-c1".into(),
                invocation_id: InvocationId("inv-c1".into()),
                request_id: "req-c1".into(),
                work_id: work.id.clone(),
                execution_id: execution.id.clone(),
                base: "base-1".into(),
                produced: Some("head-1".into()),
                manifest_digest: None,
                state: HandoffState::Acknowledged,
                invalid_reason: None,
                attempt: 0,
                recorded_seq: 0,
            },
            expected_seq: 0,
        },
        Effect::InsertReviewRound {
            round: ReviewRound {
                id: "round-1".into(),
                work_id: work.id.clone(),
                target: DiffTarget {
                    change: ChangeRequestRef {
                        repository: repo(),
                        change_request: ChangeRequestId::trusted("cr-1"),
                        display_number: Some(1),
                    },
                    head: CommitRevision::new(repo(), "head-1").unwrap(),
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
            occurrences: vec![FindingOccurrence {
                id: "occ-1".into(),
                round_id: "round-1".into(),
                source_key: "key-occ-1".into(),
                content_digest: None,
                severity: Some("warn".into()),
                position: None,
                thread_id: None,
            }],
        },
        Effect::InsertRoundJudgment {
            judgment: RoundJudgment {
                id: "j-1".into(),
                work_id: work.id.clone(),
                round_id: "round-1".into(),
                set_digest: "s".repeat(64),
                decisions: vec![FindingDecision {
                    occurrence_id: "occ-1".into(),
                    choice: FindingChoice::Fix,
                    rationale: None,
                }],
                actor_id: "tester".into(),
                seq: 0,
            },
        },
        Effect::InsertRequest {
            request: InvocationRequest {
                request_id: "req-f1".into(),
                work_id: work.id.clone(),
                execution_id: execution.id.clone(),
                role: InvocationRole::Fix,
                spec_revision: 1,
                payload: serde_json::Value::Null,
                adapter_binding: None,
            },
        },
        Effect::UpsertHandoff {
            handoff: ArtifactHandoff {
                artifact_id: "art-f1".into(),
                invocation_id: InvocationId("inv-f1".into()),
                request_id: "req-f1".into(),
                work_id: work.id.clone(),
                execution_id: execution.id.clone(),
                base: "head-1".into(),
                produced: Some("head-2".into()),
                manifest_digest: None,
                state: HandoffState::Acknowledged,
                invalid_reason: None,
                attempt: 0,
                recorded_seq: 0,
            },
            expected_seq: 0,
        },
        Effect::InsertVerificationResult {
            result: VerificationResult {
                id: "v-1".into(),
                artifact_id: "art-f1".into(),
                requirement: "verification_argv[0]".into(),
                argv: vec!["cargo".into(), "test".into()],
                produced: "head-2".into(),
                environment_digest: "e".repeat(64),
                result: VerificationOutcome::Pass,
                output: "ok".into(),
                recorded_seq: 0,
            },
        },
        {
            execution.state = ExecutionState::Succeeded;
            execution.outcome = Some(workloop_core::observation::Outcome {
                state: ExecutionState::Succeeded,
                code: None,
                detail: None,
                evidence_refs: vec![],
                recorded_seq: 11,
            });
            Effect::UpdateExecution {
                execution,
                expected_state: ExecutionState::Running,
            }
        },
    ];
    for e in &effects {
        store
            .append_event("effect", &serde_json::to_string(e).unwrap())
            .unwrap();
    }
}

fn artifact_bytes(db: &Path) -> BTreeMap<String, Vec<u8>> {
    let snap = SqliteStore::open(db, 2)
        .unwrap()
        .evidence_history()
        .unwrap();
    snap.events
        .iter()
        .map(|e| (format!("event:{}", e.seq), e.payload.clone().into_bytes()))
        .collect()
}

#[test]
fn export_full_sequence_validates_against_the_frozen_contract() {
    let root = root("full");
    populate(&root.join("workloop.db"));
    let output = output_at(&root, &["evidence", "export", "--limit", "500"]);
    let env = export_envelope(&output);
    env.validate_export().unwrap();
    assert!(matches!(env.schema_version, SchemaVersion::V1));
    assert_eq!(
        env.command.as_ref().unwrap().kind,
        CommandKind::EvidenceExport
    );
    let page = env.result.as_ref().unwrap();
    let req = ExportRequest {
        protocol_version: ProtocolVersion::V1,
        after: None,
        cursor: None,
        limit: 500,
    };
    let snapshot = validate_snapshot(
        &[(req, page.clone())],
        None,
        &artifact_bytes(&root.join("workloop.db")),
    )
    .unwrap();
    assert!(snapshot.unsupported_links().is_empty());
    assert_eq!(env.as_of, Some(page.snapshot_seq));
    assert!(!page.entities.is_empty());
}

#[test]
fn paginated_export_and_delta_after_checkpoint() {
    let root = root("paged");
    populate(&root.join("workloop.db"));
    // Drain with limit 2 — several pages, each carrying next_cursor.
    let mut pages: Vec<(ExportRequest, ExportPage)> = Vec::new();
    let mut request = ExportRequest {
        protocol_version: ProtocolVersion::V1,
        after: None,
        cursor: None,
        limit: 2,
    };
    let checkpoint;
    loop {
        let mut args = vec!["evidence", "export", "--limit", "2"];
        let cursor_arg;
        if let Some(c) = &request.cursor {
            cursor_arg = c.clone();
            args.push("--cursor");
            args.push(&cursor_arg);
        }
        let output = output_at(&root, &args);
        let env = export_envelope(&output);
        env.validate_export().unwrap();
        let page = env.result.unwrap();
        let done = page.checkpoint.is_some();
        let used_request = request.clone();
        request.cursor = page.next_cursor.clone();
        pages.push((used_request, page));
        if done {
            checkpoint = pages.last().unwrap().1.checkpoint.clone().unwrap();
            break;
        }
    }
    assert!(pages.len() > 1);
    let bytes = artifact_bytes(&root.join("workloop.db"));
    let history = validate_snapshot(&pages, None, &bytes).unwrap();
    // A delta export after the committed checkpoint validates as the
    // next sequence over the same retained history.
    let output = output_at(
        &root,
        &[
            "evidence",
            "export",
            "--after",
            &checkpoint,
            "--limit",
            "500",
        ],
    );
    let env = export_envelope(&output);
    env.validate_export().unwrap();
    let delta = env.result.unwrap();
    assert_eq!(delta.after_seq, Some(pages[0].1.snapshot_seq));
    let delta_req = ExportRequest {
        protocol_version: ProtocolVersion::V1,
        after: Some(checkpoint),
        cursor: None,
        limit: 500,
    };
    validate_snapshot(&[(delta_req, delta)], Some(&history), &bytes).unwrap();
}

#[test]
fn invalid_cursor_is_a_bounded_rejection() {
    let root = root("badcursor");
    populate(&root.join("workloop.db"));
    let output = output_at(&root, &["evidence", "export", "--cursor", "garbage"]);
    // The frozen exit-class table maps invalid_cursor to class 2.
    assert_eq!(output.status.code(), Some(2));
    let env = export_envelope(&output);
    assert_eq!(env.status, QueryStatus::Error);
    assert_eq!(env.reason.as_ref().unwrap().code, ErrorCode::InvalidCursor);
    env.validate_export().unwrap();
    // A result never leaks a completed checkpoint on failure.
    assert!(env.result.is_none());
}

#[test]
fn argument_shape_failures_are_usage() {
    let root = root("usage");
    populate(&root.join("workloop.db"));
    // Both continuation flags at once is a clap conflict → the CLI's
    // usage envelope, exit 2.
    let output = output_at(
        &root,
        &["evidence", "export", "--after", "a", "--cursor", "c"],
    );
    assert_eq!(output.status.code(), Some(2));
    let env: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(env["schema_version"], "1");
    assert_eq!(env["reason"]["code"], "usage");
    // Out-of-range limit.
    let output = output_at(&root, &["evidence", "export", "--limit", "600"]);
    assert_eq!(output.status.code(), Some(2));
    // A malformed digest never reaches the store.
    let output = output_at(
        &root,
        &[
            "evidence", "artifact", "--id", "event:1", "--sha256", "nothex", "--output", "out.bin",
        ],
    );
    assert_eq!(output.status.code(), Some(2));
}

#[test]
fn artifact_writes_exact_bytes_and_refuses_an_existing_path() {
    let root = root("art");
    let db = root.join("workloop.db");
    populate(&db);
    // Resolve a real descriptor from an export page.
    let output = output_at(&root, &["evidence", "export", "--limit", "500"]);
    let env = export_envelope(&output);
    let descriptor = env.result.unwrap().artifacts[0].clone();
    let out_path = root.join("retrieved.json");
    let out = out_path.to_string_lossy().into_owned();
    let output = output_at(
        &root,
        &[
            "evidence",
            "artifact",
            "--id",
            &descriptor.artifact_id,
            "--sha256",
            &descriptor.sha256,
            "--output",
            &out,
        ],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let wire: QueryEnvelope<ArtifactResult> = serde_json::from_slice(&output.stdout).unwrap();
    wire.validate_artifact_receipt().unwrap();
    let bytes = std::fs::read(&out_path).unwrap();
    let request = ArtifactRequest {
        protocol_version: ProtocolVersion::V1,
        artifact_id: descriptor.artifact_id.clone(),
        sha256: descriptor.sha256.clone(),
    };
    validate_artifact_result(&request, &descriptor, &wire.result.unwrap(), &bytes).unwrap();
    // The bytes are the exact retained payload.
    let snap = SqliteStore::open(&db, 2)
        .unwrap()
        .evidence_history()
        .unwrap();
    let payload = &snap
        .events
        .iter()
        .find(|e| format!("event:{}", e.seq) == descriptor.artifact_id)
        .unwrap()
        .payload;
    assert_eq!(bytes, payload.as_bytes());
    // Refuse to overwrite the file it just wrote.
    let output = output_at(
        &root,
        &[
            "evidence",
            "artifact",
            "--id",
            &descriptor.artifact_id,
            "--sha256",
            &descriptor.sha256,
            "--output",
            &out,
        ],
    );
    assert_eq!(output.status.code(), Some(2));
    // A digest mismatch is a bounded rejection and publishes nothing.
    let other = root.join("other.json");
    let output = output_at(
        &root,
        &[
            "evidence",
            "artifact",
            "--id",
            &descriptor.artifact_id,
            "--sha256",
            &"0".repeat(64),
            "--output",
            &other.to_string_lossy(),
        ],
    );
    // digest_mismatch maps to exit class 5 per the frozen table.
    assert_eq!(output.status.code(), Some(5));
    let env: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(env["reason"]["code"], "digest_mismatch");
    assert!(!other.exists(), "no partial artifact is published");
}

#[test]
fn help_documents_the_supported_protocol() {
    let output = Command::new(bin())
        .args(["evidence", "--help"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(text.contains("rui-evidence/1"), "{text}");
}
