//! Agent-interface workflow coverage through the real `taisan` binary:
//! every applicable matrix row from `docs/agent-interface.md` runs as
//! CLI invocations against a real installation — request work, inspect,
//! intervene, judge findings, recover a lost response — with replay
//! keys, expected versions, and revision preconditions enforced at the
//! CLI boundary. States a Command cannot reach on its own (a published
//! change request, a sealed review round, a paused or failed
//! Execution) are seeded through the same store the binary opens — the
//! fixture equivalent of prior history, matching the contract-test
//! pattern. Assertions read the binary's stdout envelope; the store
//! is a setup tool, never the assertion surface — with one
//! exception: `submit`'s recorded spec payload reaches no CLI
//! surface, so the submit test asserts its verbatim preservation on
//! the store record itself.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use workloop_config::{Installation, load_or_create};
use workloop_core::domain::{ExecutionId, ExecutionState, RepairPrerequisite, WorkId};
use workloop_core::identity::{
    ActorRole, ChangeRequestId, ChangeRequestRef, CommitRevision, DiffTarget, HostActorId,
    HostActorRef, HostInstanceId, RepositoryId, RepositoryRef,
};
use workloop_core::observation::{
    Completeness, HostActionStatus, NormalizedEffectFact, Observation, ObservationFact,
    OccurrenceInput, Outcome,
};
use workloop_core::store::{Effect, HostAction, HostActionIntent, Store};
use workloop_core::work::{self, WorkloopConfig};
use workloop_store::sqlite::SqliteStore;

const NOW: i64 = 1_700_000_000;

fn bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_workloop-cli"))
}

fn root(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("workloop-cli-ai-{}-{}", tag, std::process::id()));
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

/// A command that must complete: exit 0 and one JSON envelope on stdout.
fn run_at(root: &Path, args: &[&str]) -> serde_json::Value {
    let output = output_at(root, args);
    assert!(
        output.status.success(),
        "taisan {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    envelope(&output)
}

/// A command expected to fail: (exit code, envelope).
fn fail_at(root: &Path, args: &[&str]) -> (i32, serde_json::Value) {
    let output = output_at(root, args);
    let code = output.status.code().unwrap_or(-1);
    let env = envelope(&output);
    (code, env)
}

fn envelope(output: &Output) -> serde_json::Value {
    let stdout = String::from_utf8_lossy(&output.stdout);
    serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("stdout is one JSON object ({e}): {stdout}"))
}

fn submit_at(root: &Path, key: &str, intent: &str) -> serde_json::Value {
    run_at(
        root,
        &["submit", "--intent", intent, "--idempotency-key", key],
    )
}

fn work_detail(root: &Path, work_id: &str) -> serde_json::Value {
    run_at(root, &["work-detail", "--work", work_id])
}

/// The Work's current optimistic-concurrency version, as displayed to
/// the caller — the value every mutation precondition is built from.
fn work_version(root: &Path, work_id: &str) -> String {
    work_detail(root, work_id)["result"]["version"]
        .as_u64()
        .expect("detail carries a numeric version")
        .to_string()
}

// ---- Fixture history the CLI cannot reach on its own ----
// These helpers write the same records the loop's host/observation
// path would have produced: an applied open intent plus its receipt
// and head observation, a sealed review round, a paused or failed
// Execution. Every subsequent Command still goes through the binary.

fn installation(root: &Path) -> Installation {
    load_or_create(&root.join("config.json")).expect("installation loads")
}

fn workloop_config(root: &Path) -> WorkloopConfig {
    installation(root)
        .workloop_config()
        .expect("installation config is valid")
}

fn store(root: &Path) -> SqliteStore {
    SqliteStore::open(&root.join("workloop.db"), installation(root).capacity).expect("store opens")
}

fn host(root: &Path) -> HostInstanceId {
    HostInstanceId::new(installation(root).host_instance).unwrap()
}

fn repository(root: &Path) -> RepositoryRef {
    RepositoryRef::new(
        host(root),
        RepositoryId::new(installation(root).repository).unwrap(),
    )
}

fn publisher(root: &Path) -> HostActorRef {
    let installation = installation(root);
    HostActorRef {
        host: host(root),
        actor: HostActorId::new(installation.host_actors["publication"].clone()).unwrap(),
    }
}

fn change(root: &Path) -> ChangeRequestRef {
    ChangeRequestRef {
        repository: repository(root),
        change_request: ChangeRequestId::trusted("cr-1"),
        display_number: Some(1),
    }
}

fn rev(root: &Path, object: &str) -> CommitRevision {
    CommitRevision::new(repository(root), object).unwrap()
}

fn apply_effects(store: &SqliteStore, effects: &[Effect]) {
    store
        .transact(|tx| {
            for e in effects {
                tx.apply(e)?;
            }
            Ok(())
        })
        .expect("fixture history applies");
}

fn read_execution(store: &SqliteStore, id: &ExecutionId) -> workloop_core::domain::Execution {
    store
        .transact(|tx| tx.execution(id))
        .expect("read")
        .expect("execution exists")
}

/// Move an Execution to a target state through the store's guarded
/// update — prior history, never a substitute for the Command path.
fn force_state(store: &SqliteStore, id: &ExecutionId, to: ExecutionState) {
    let from = read_execution(store, id).state;
    let mut e = read_execution(store, id);
    e.state = to.clone();
    if to.is_terminal() {
        e.terminal_code = Some("fixture".into());
        e.outcome = Some(Outcome {
            state: to.clone(),
            code: Some("fixture".into()),
            detail: None,
            evidence_refs: vec![],
            recorded_seq: 0,
        });
    }
    apply_effects(
        store,
        &[Effect::UpdateExecution {
            execution: e,
            expected_state: from,
        }],
    );
}

/// A pause for human judgment, as the review-findings path records it.
fn pause_for_judgment(store: &SqliteStore, id: &ExecutionId) {
    let mut e = read_execution(store, id);
    let from = e.state.clone();
    e.state = ExecutionState::Paused;
    e.pause_reason = Some("review findings require human disposition".into());
    e.repair_prerequisites = vec![RepairPrerequisite::OperatorDecision];
    e.paused_since_unix_seconds = Some(NOW);
    apply_effects(
        store,
        &[Effect::UpdateExecution {
            execution: e,
            expected_state: from,
        }],
    );
}

/// The persisted open+head evidence `judge-findings` and
/// `release-judgment` compare against: the applied open intent, its
/// `ChangeOpened` receipt, and the head observation — the durable
/// trail a published change request leaves.
fn open_change(root: &Path, work_id: &str, head: &str) {
    let store = store(root);
    let work_id = WorkId(work_id.to_string());
    let created_seq = store.transact(|tx| tx.alloc_seq()).expect("seq allocates");
    apply_effects(
        &store,
        &[
            Effect::InsertHostActionIntent {
                intent: HostActionIntent {
                    action_id: "act-open".into(),
                    work_id: work_id.clone(),
                    host: host(root),
                    repository: repository(root),
                    role: ActorRole::Publication,
                    actor: publisher(root),
                    action: HostAction::OpenChangeRequest {
                        source_branch: format!("work/{}", work_id.0),
                        target_branch: "main".into(),
                        title: "fixture".into(),
                        body: String::new(),
                    },
                    payload_digest: "fixture".into(),
                    status: HostActionStatus::Applied,
                    authenticated_actor: None,
                    effects: vec![],
                    created_seq,
                },
            },
            Effect::InsertObservation {
                observation: Observation {
                    delivery: None,
                    subject_work_id: Some(work_id.clone()),
                    subject_execution_id: None,
                    fact: ObservationFact::HostActionResult {
                        action_id: "act-open".into(),
                        authenticated_actor: Some(publisher(root)),
                        status: HostActionStatus::Applied,
                        effects: vec![NormalizedEffectFact::ChangeOpened {
                            change: change(root),
                            head: rev(root, head),
                            source_branch: format!("work/{}", work_id.0),
                            target_branch: "main".into(),
                        }],
                    },
                    observed_unix_seconds: NOW,
                },
            },
            Effect::InsertObservation {
                observation: Observation {
                    delivery: None,
                    subject_work_id: Some(work_id.clone()),
                    subject_execution_id: None,
                    fact: ObservationFact::ChangeHead {
                        change: change(root),
                        head: rev(root, head),
                    },
                    observed_unix_seconds: NOW,
                },
            },
        ],
    );
}

/// A complete review delivery seals an immutable round over the exact
/// diff target — the same ingest path the loop's source refresh uses.
fn seal_review(root: &Path, work_id: &str, head: &str, source_keys: &[&str]) {
    let obs = Observation {
        delivery: None,
        subject_work_id: Some(WorkId(work_id.to_string())),
        subject_execution_id: None,
        fact: ObservationFact::Review {
            target: DiffTarget {
                change: change(root),
                head: rev(root, head),
                base: None,
                target_revision: None,
            },
            source: "reviewer".into(),
            source_revision: None,
            completeness: Completeness::Complete,
            occurrences: source_keys
                .iter()
                .map(|key| OccurrenceInput {
                    source_key: key.to_string(),
                    content_digest: None,
                    severity: None,
                    position: None,
                    thread_id: None,
                })
                .collect(),
        },
        observed_unix_seconds: NOW,
    };
    work::handle_observation(&store(root), &workloop_config(root), &obs, NOW).unwrap();
}

/// The occurrence ids on the Work's latest sealed round, as displayed
/// by `work-detail` — the exact set a judgment must cover.
fn displayed_occurrences(detail: &serde_json::Value) -> Vec<String> {
    detail["result"]["findings"]
        .as_array()
        .expect("detail carries a findings list")
        .iter()
        .map(|f| f["occurrence_id"].as_str().unwrap().to_string())
        .collect()
}

fn decision_args(occurrences: &[String], choice: &str) -> Vec<String> {
    occurrences
        .iter()
        .map(|id| format!("{id}:{choice}"))
        .collect()
}

// ---- Matrix row: Request work ----

#[test]
fn submit_preserves_the_recorded_request_and_means_queued() {
    let root = root("submit");
    let env = run_at(
        &root,
        &[
            "submit",
            "--intent",
            "implement the thing",
            "--issue",
            "local-repo:7",
            "--issue-revision",
            "issue-rev-9",
            "--issue-digest",
            "issue-digest-9",
            "--constraint",
            "no new dependencies",
            "--verification",
            "cargo test -p workloop-cli",
            "--idempotency-key",
            "cli-sub-1",
        ],
    );
    assert_eq!(env["status"], "accepted", "{env}");
    assert_eq!(env["command"]["kind"], "submit");
    assert_eq!(env["command"]["idempotency_key"], "cli-sub-1");
    let work_id = env["work_id"].as_str().unwrap().to_string();
    assert_eq!(env["result"]["kind"], "work_created");

    // Accepted means durably queued — never Work completion.
    let list = run_at(&root, &["work-list"]);
    let item = &list["result"]["items"][0];
    assert_eq!(item["work_id"], work_id);
    assert_eq!(item["phase"], "queued");
    assert!(item["outcome"].is_null());
    assert_eq!(
        work_detail(&root, &work_id)["result"]["executions"][0]["state"],
        "queued"
    );

    // Intent, declared sources, constraints, and the verification input
    // are preserved verbatim in the recorded spec.
    let store = store(&root);
    let spec = store
        .transact(|tx| {
            let work = tx.work(&WorkId(work_id.clone()))?.expect("work exists");
            tx.spec(&work.id, Some(work.approved_spec))
        })
        .expect("spec read")
        .expect("approved spec recorded");
    assert_eq!(spec.payload.intent, "implement the thing");
    assert_eq!(spec.payload.constraints, ["no new dependencies"]);
    assert_eq!(
        spec.payload.verification_argv,
        [vec!["cargo", "test", "-p", "workloop-cli"]]
    );
    let source = &spec.payload.sources[0];
    assert_eq!(source.issue.display_number, Some(7));
    assert_eq!(source.source_revision, "issue-rev-9");
    assert_eq!(source.content_digest, "issue-digest-9");
}

// ---- Matrix row: Inspect progress and evidence ----

#[test]
fn queries_read_recorded_state_with_unavailable_sources_visible() {
    let root = root("inspect");
    let env = submit_at(&root, "cli-ins-1", "inspect me");
    let work_id = env["work_id"].as_str().unwrap().to_string();
    let execution_id = env["execution_id"].as_str().unwrap().to_string();

    // `run-once` is the loop driver: dispatch admits the queued
    // Execution — the canonical identities stay consistent across
    // every query surface.
    run_at(&root, &["run-once"]);

    let list = run_at(&root, &["work-list"]);
    assert_eq!(list["status"], "accepted");
    assert_eq!(list["result"]["items"][0]["phase"], "implementing");
    assert_eq!(
        list["result"]["items"][0]["current_execution_id"],
        execution_id
    );

    let det = work_detail(&root, &work_id);
    assert_eq!(det["result"]["executions"][0]["state"], "ready");
    assert_eq!(det["result"]["executions"][0]["execution_id"], execution_id);
    assert_eq!(det["result"]["executions"][0]["ownership"], "confirmed");

    let capacity = run_at(&root, &["capacity"]);
    assert_eq!(capacity["result"]["used"], 1);
    assert_eq!(capacity["result"]["limit"], 1);
    assert!(
        capacity["result"]["reservations"]
            .as_array()
            .is_some_and(|r| !r.is_empty()),
        "admission's workspace/branch reservations stay visible"
    );

    let attention = run_at(&root, &["attention"]);
    assert_eq!(attention["status"], "accepted");
    assert_eq!(attention["result"].as_array().unwrap().len(), 0);

    // A published change with no review evidence read yet: the review
    // source stays visibly unavailable — an empty finding list never
    // presents as a clean result.
    open_change(&root, &work_id, "h-top");
    let det = work_detail(&root, &work_id);
    assert!(
        det["result"]["current_change"].is_array(),
        "the opened change request is on record: {det}"
    );
    let sources = det["sources"].as_array().unwrap();
    assert!(
        sources
            .iter()
            .any(|s| s["source"] == "review" && s["availability"] == "unavailable"),
        "unread review source surfaces as unavailable: {sources:?}"
    );
}

// ---- Matrix row: Intervene ----

#[test]
fn interventions_use_observed_versions_and_eligible_identities() {
    let root = root("intervene");

    // acknowledge: applies only to a currently derivable Attention
    // version — a paused Execution derives one.
    let env = submit_at(&root, "cli-int-ack", "ack work");
    let work_id = env["work_id"].as_str().unwrap().to_string();
    let execution_id = env["execution_id"].as_str().unwrap().to_string();
    pause_for_judgment(&store(&root), &ExecutionId(execution_id.clone()));

    let attention = run_at(&root, &["attention", "--work", &work_id]);
    let items = attention["result"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["reason"], "paused_judgment");
    let attention_id = items[0]["attention_id"].as_str().unwrap().to_string();

    let env = run_at(
        &root,
        &[
            "acknowledge",
            "--work",
            &work_id,
            "--expected-version",
            "1",
            "--attention",
            &attention_id,
            "--idempotency-key",
            "cli-int-ack2",
        ],
    );
    assert_eq!(env["status"], "accepted", "{env}");
    let attention = run_at(&root, &["attention", "--work", &work_id]);
    assert_eq!(
        attention["result"].as_array().unwrap().len(),
        0,
        "the acknowledged Attention version clears"
    );

    // An id the current state does not derive cannot be acknowledged —
    // nothing may hide a condition that has not surfaced.
    let (code, env) = fail_at(
        &root,
        &[
            "acknowledge",
            "--work",
            &work_id,
            "--expected-version",
            "1",
            "--attention",
            "att:not-displayed",
        ],
    );
    assert_eq!(code, 1, "{env}");
    assert_eq!(env["reason"]["code"], "ineligible");

    // resume: the paused Execution requeues on the observed version.
    let env = run_at(
        &root,
        &[
            "resume",
            "--work",
            &work_id,
            "--expected-version",
            "1",
            "--execution",
            &execution_id,
            "--idempotency-key",
            "cli-int-res",
        ],
    );
    assert_eq!(env["status"], "accepted", "{env}");
    assert_eq!(
        work_detail(&root, &work_id)["result"]["executions"][0]["state"],
        "queued"
    );

    // Ineligible: the named Execution is no longer paused.
    let (code, env) = fail_at(
        &root,
        &[
            "resume",
            "--work",
            &work_id,
            "--expected-version",
            &work_version(&root, &work_id),
            "--execution",
            &execution_id,
        ],
    );
    assert_eq!(code, 1, "{env}");
    assert_eq!(env["reason"]["code"], "ineligible");

    // retry: only on the confirmed-failed current Execution.
    let env = submit_at(&root, "cli-int-ret", "retry work");
    let work_id = env["work_id"].as_str().unwrap().to_string();
    let execution_id = env["execution_id"].as_str().unwrap().to_string();

    // A queued Execution is not retryable.
    let (code, env) = fail_at(
        &root,
        &[
            "retry",
            "--work",
            &work_id,
            "--expected-version",
            "1",
            "--expected-execution",
            &execution_id,
        ],
    );
    assert_eq!(code, 1, "{env}");
    assert_eq!(env["reason"]["code"], "ineligible");

    // The named Execution must be the Work's current one.
    let (code, env) = fail_at(
        &root,
        &[
            "retry",
            "--work",
            &work_id,
            "--expected-version",
            "1",
            "--expected-execution",
            "e-not-current",
        ],
    );
    assert_eq!(code, 1, "{env}");
    assert_eq!(env["reason"]["code"], "ineligible");

    force_state(
        &store(&root),
        &ExecutionId(execution_id.clone()),
        ExecutionState::Failed,
    );
    let env = run_at(
        &root,
        &[
            "retry",
            "--work",
            &work_id,
            "--expected-version",
            "1",
            "--expected-execution",
            &execution_id,
            "--idempotency-key",
            "cli-int-ret2",
        ],
    );
    assert_eq!(env["status"], "accepted", "{env}");
    assert_eq!(env["result"]["kind"], "execution_created");
    let successor = env["execution_id"].as_str().unwrap().to_string();
    assert_ne!(
        successor, execution_id,
        "retry mints a new Execution, it never revives the failed one"
    );

    // The accepted retry moved the version — the recorded precondition
    // is stale now.
    let (code, env) = fail_at(
        &root,
        &[
            "retry",
            "--work",
            &work_id,
            "--expected-version",
            "1",
            "--expected-execution",
            &successor,
        ],
    );
    assert_eq!(code, 1, "{env}");
    assert_eq!(env["reason"]["code"], "stale_work_version");

    // cancel: acceptance records the transition; a terminal Work is an
    // accepted no-op that reports the stored version, not a new one.
    let env = submit_at(&root, "cli-int-can", "cancel work");
    let work_id = env["work_id"].as_str().unwrap().to_string();
    let env = run_at(
        &root,
        &[
            "cancel",
            "--work",
            &work_id,
            "--expected-version",
            "1",
            "--idempotency-key",
            "cli-int-can2",
        ],
    );
    assert_eq!(env["status"], "accepted", "{env}");
    let det = work_detail(&root, &work_id);
    assert_eq!(det["result"]["executions"][0]["state"], "cancelled");
    let version = det["result"]["version"].as_u64().unwrap().to_string();

    let env = run_at(
        &root,
        &[
            "cancel",
            "--work",
            &work_id,
            "--expected-version",
            &version,
            "--idempotency-key",
            "cli-int-can3",
        ],
    );
    assert_eq!(env["status"], "accepted", "{env}");
    assert_eq!(
        work_detail(&root, &work_id)["result"]["version"]
            .as_u64()
            .unwrap()
            .to_string(),
        version,
        "a terminal-work cancel reports the stored version, it commits nothing"
    );

    let (code, env) = fail_at(
        &root,
        &["cancel", "--work", &work_id, "--expected-version", "99"],
    );
    assert_eq!(code, 1, "{env}");
    assert_eq!(env["reason"]["code"], "stale_work_version");
}

// ---- Matrix row: Judge review findings ----

#[test]
fn judgment_binds_the_current_head_and_exact_finding_set() {
    let root = root("judge");
    let env = submit_at(&root, "cli-jdg-1", "judge work");
    let work_id = env["work_id"].as_str().unwrap().to_string();
    let execution_id = env["execution_id"].as_str().unwrap().to_string();

    open_change(&root, &work_id, "h-1");
    seal_review(&root, &work_id, "h-1", &["a", "b"]);

    let det = work_detail(&root, &work_id);
    let occurrences = displayed_occurrences(&det);
    assert_eq!(occurrences.len(), 2);
    assert!(det["result"]["current_round_id"].is_string());
    let version = det["result"]["version"].as_u64().unwrap().to_string();

    let judge_args = |version: &str, head: &str, decisions: &[String]| {
        let mut args = vec![
            "judge-findings".to_string(),
            "--work".into(),
            work_id.clone(),
            "--expected-version".into(),
            version.to_string(),
            "--head".into(),
            head.to_string(),
        ];
        for d in decisions {
            args.push("--decision".into());
            args.push(d.clone());
        }
        args
    };
    fn as_refs(args: &[String]) -> Vec<&str> {
        args.iter().map(String::as_str).collect()
    }

    // A head the recorded evidence does not name is stale — the
    // rejection carries the closed vocabulary, never a silent retarget.
    let (code, env) = fail_at(
        &root,
        &as_refs(&judge_args(
            &version,
            "h-stale",
            &decision_args(&occurrences, "fix"),
        )),
    );
    assert_eq!(code, 1, "{env}");
    assert_eq!(env["status"], "rejected");
    assert_eq!(env["reason"]["code"], "stale_head");

    // Partial coverage: every sealed occurrence must be decided.
    let (code, env) = fail_at(
        &root,
        &as_refs(&judge_args(
            &version,
            "h-1",
            &decision_args(&occurrences[..1], "fix"),
        )),
    );
    assert_eq!(code, 1, "{env}");
    assert_eq!(env["reason"]["code"], "invalid_request");

    // Exact head plus the exact finding set: accepted.
    let env = run_at(
        &root,
        &as_refs(&judge_args(
            &version,
            "h-1",
            &decision_args(&occurrences, "fix"),
        )),
    );
    assert_eq!(env["status"], "accepted", "{env}");

    // release-judgment releases a recorded judgment only on a paused
    // Execution — a queued one is ineligible.
    let version = work_version(&root, &work_id);
    let (code, env) = fail_at(
        &root,
        &[
            "release-judgment",
            "--work",
            &work_id,
            "--expected-version",
            &version,
            "--head",
            "h-1",
        ],
    );
    assert_eq!(code, 1, "{env}");
    assert_eq!(env["reason"]["code"], "ineligible");

    // Findings that grew on the same head seal a new round: the prior
    // judgment covers nothing on the current surface — a release
    // against it cannot pass on the old evidence.
    seal_review(&root, &work_id, "h-1", &["a", "b", "c"]);
    let version = work_version(&root, &work_id);
    pause_for_judgment(&store(&root), &ExecutionId(execution_id));
    let (code, env) = fail_at(
        &root,
        &[
            "release-judgment",
            "--work",
            &work_id,
            "--expected-version",
            &version,
            "--head",
            "h-1",
        ],
    );
    assert_eq!(code, 1, "{env}");
    assert_eq!(env["reason"]["code"], "ineligible");

    // A decision set built on the pre-growth occurrence ids is
    // incomplete — never silently re-authorized.
    let (code, env) = fail_at(
        &root,
        &as_refs(&judge_args(
            &version,
            "h-1",
            &decision_args(&occurrences, "fix"),
        )),
    );
    assert_eq!(code, 1, "{env}");
    assert_eq!(env["reason"]["code"], "invalid_request");

    // Re-inspect: the displayed findings are the grown set; judging it
    // on the current version succeeds, then release requeues.
    let det = work_detail(&root, &work_id);
    let grown = displayed_occurrences(&det);
    assert_eq!(grown.len(), 3);
    let env = run_at(
        &root,
        &as_refs(&judge_args(&version, "h-1", &decision_args(&grown, "fix"))),
    );
    assert_eq!(env["status"], "accepted", "{env}");

    let env = run_at(
        &root,
        &[
            "release-judgment",
            "--work",
            &work_id,
            "--expected-version",
            &work_version(&root, &work_id),
            "--head",
            "h-1",
            "--idempotency-key",
            "cli-jdg-rel",
        ],
    );
    assert_eq!(env["status"], "accepted", "{env}");
    assert_eq!(
        work_detail(&root, &work_id)["result"]["executions"][0]["state"],
        "queued",
        "release readmits the paused Execution"
    );
}

// ---- Matrix row: Recover a lost command response ----

#[test]
fn a_lost_response_recovers_under_the_original_identity() {
    let root = root("recover");
    let env = submit_at(&root, "cli-lost-1", "recover me");
    let command_id = env["command"]["id"].as_str().unwrap().to_string();

    // Recovery by command id and by the caller's replay identity both
    // return the recorded decision.
    for args in [
        vec!["command-result", "--command-id", command_id.as_str()],
        vec![
            "command-result",
            "--origin",
            "cli",
            "--replay-key",
            "cli-lost-1",
        ],
    ] {
        let r = run_at(&root, &args);
        assert_eq!(r["status"], "accepted", "{args:?}: {r}");
        assert_eq!(r["result"]["command_id"], command_id);
    }

    // Identical replay returns the stored decision — the recovery path
    // — never a second mutation.
    let replay = submit_at(&root, "cli-lost-1", "recover me");
    assert_eq!(replay["command"]["id"], command_id);
    let list = run_at(&root, &["work-list"]);
    assert_eq!(list["result"]["items"].as_array().unwrap().len(), 1);

    // An unknown fate never justifies a fresh key; a changed payload
    // under the same key is a recorded conflict.
    let (code, env) = fail_at(
        &root,
        &[
            "submit",
            "--intent",
            "a different intent",
            "--idempotency-key",
            "cli-lost-1",
        ],
    );
    assert_eq!(code, 1, "{env}");
    assert_eq!(env["reason"]["code"], "idempotency_conflict");

    // A command-result for an unknown identity is a completed Query
    // with a null result — not an error.
    let r = run_at(&root, &["command-result", "--command-id", "cmd-nope"]);
    assert_eq!(r["status"], "accepted");
    assert!(r["result"].is_null());
}

// ---- Conversation closure ----

#[test]
fn conversation_closure_leaves_the_recorded_execution_queryable() {
    let root = root("closure");
    let env = submit_at(&root, "cli-cls-1", "outlive me");
    let work_id = env["work_id"].as_str().unwrap().to_string();
    let execution_id = env["execution_id"].as_str().unwrap().to_string();
    let command_id = env["command"]["id"].as_str().unwrap().to_string();
    run_at(&root, &["run-once"]);
    assert_eq!(
        work_detail(&root, &work_id)["result"]["executions"][0]["state"],
        "ready"
    );

    // Every invocation above has exited — the session is over. The
    // checks below are all fresh CLI processes against the same
    // recorded state: the durable Work and its Execution ownership
    // stay inspectable, and nothing inferred a cancellation.
    let det = work_detail(&root, &work_id);
    let executions = det["result"]["executions"].as_array().unwrap();
    assert_eq!(executions.len(), 1, "no duplicate launch or settlement");
    assert_eq!(executions[0]["execution_id"], execution_id);
    assert_eq!(executions[0]["state"], "ready");
    assert_eq!(executions[0]["ownership"], "confirmed");
    assert!(executions[0]["outcome"].is_null());

    let recovered = run_at(&root, &["command-result", "--command-id", &command_id]);
    assert_eq!(recovered["result"]["command_id"], command_id);

    assert_eq!(run_at(&root, &["attention"])["status"], "accepted");
    let capacity = run_at(&root, &["capacity"]);
    assert_eq!(capacity["result"]["used"], 1);

    // The loop itself reads the same recorded state: a bounded run
    // neither duplicates the writer nor infers a cancellation.
    let env = run_at(&root, &["run", "--ticks", "1", "--interval-ms", "0"]);
    assert_eq!(env["status"], "accepted");
    assert_eq!(env["command"]["kind"], "run");
    let det = work_detail(&root, &work_id);
    assert_eq!(det["result"]["executions"].as_array().unwrap().len(), 1);
    assert_eq!(det["result"]["executions"][0]["state"], "ready");

    // An explicit operator Command — not the ended conversation — is
    // what acts on the recorded Execution. Cancel acceptance records a
    // stop intent, never stop proof: a live Execution settles through
    // `stopping` until writer safety is established.
    let env = run_at(
        &root,
        &[
            "cancel",
            "--work",
            &work_id,
            "--expected-version",
            &work_version(&root, &work_id),
            "--idempotency-key",
            "cli-cls-2",
        ],
    );
    assert_eq!(env["status"], "accepted", "{env}");
    assert_eq!(
        work_detail(&root, &work_id)["result"]["executions"][0]["state"],
        "stopping"
    );
}
