//! Host-neutral conformance suite (U9.1): the same scenarios run
//! unchanged against independently implemented outbox fixtures. Each
//! scenario declares the capabilities it requires; a fixture runs a
//! scenario only when its declared set covers the requirement, and an
//! unsupported required capability stays explicit — never an applied
//! receipt, a skipped gate, or a substituted comment.
//!
//! The matrix in `docs/host-conformance-matrix.md` maps these scenario
//! IDs to the contract rows. Nothing here names a provider: assertions
//! bind recorded intents, receipts, and scoped identities only.

use std::collections::BTreeSet;

use workloop_core::command::*;
use workloop_core::domain::*;
use workloop_core::identity::*;
use workloop_core::observation::*;
use workloop_core::store::*;
use workloop_core::work::{HostTemplate, WorkloopConfig, admit_queued, handle_command};
use workloop_github::fixture::FixtureGithub;
use workloop_github::gitlab_fixture::GitlabFixture;
use workloop_github::sparse_fixture::SparseHost;
use workloop_store::sqlite::SqliteStore;

const NOW: i64 = 1_700_000_000;

fn host() -> HostInstanceId {
    HostInstanceId::trusted("test-host")
}

fn repo() -> RepositoryRef {
    RepositoryRef::new(host(), RepositoryId::trusted("local-repo"))
}

fn repo_b() -> RepositoryRef {
    RepositoryRef::new(host(), RepositoryId::trusted("other-repo"))
}

fn rev_on(repository: &RepositoryRef, object: &str) -> CommitRevision {
    CommitRevision::new(repository.clone(), object).unwrap()
}

fn rev(object: &str) -> CommitRevision {
    rev_on(&repo(), object)
}

fn publisher() -> HostActorRef {
    HostActorRef {
        host: host(),
        actor: HostActorId::trusted("pub-agent"),
    }
}

/// What a conforming fixture reports about itself and accepts to drive.
/// `FixtureGithub` and `SparseHost` are independent implementations:
/// different remote models, the same outbox contract.
trait OutboxHost {
    /// The capability names the fixture declares for its bound actors.
    fn declared_names(&self) -> BTreeSet<&'static str>;

    fn apply(&mut self, store: &SqliteStore, config: &WorkloopConfig);
    /// The write count, for assertions that an effect landed or did
    /// not. The history's encoding is fixture-internal — scenarios
    /// never pattern-match it.
    fn applied(&self) -> &[String];
    /// The head the remote records for `(repository, branch)` — a
    /// typed read so scoped-identity assertions need no string format.
    fn remote_head(&self, repository: &RepositoryRef, branch: &str) -> Option<String>;
    fn set_change_head(&mut self, repository: &RepositoryRef, change_id: &str, head: &str);
    fn emit(&self, store: &SqliteStore, config: &WorkloopConfig, observation: Observation);
    /// Seed one authoritative issue read for fixtures that declare
    /// `issue_read`. Fixtures that do not declare it leave the default.
    fn seed_issue(&mut self, _issue: &IssueRef, _source_revision: &str, _content_digest: &str) {}
    /// The scoped issue read, when this fixture declares `issue_read`.
    fn issue_read(&self, _issue: &IssueRef) -> Option<(String, String, IssueState)> {
        None
    }
}

impl OutboxHost for FixtureGithub {
    fn declared_names(&self) -> BTreeSet<&'static str> {
        FixtureGithub::declared_capabilities()
            .iter()
            .copied()
            .collect()
    }
    fn apply(&mut self, store: &SqliteStore, config: &WorkloopConfig) {
        self.apply_outbox(store, config, NOW).unwrap();
    }
    fn applied(&self) -> &[String] {
        self.applied()
    }
    fn remote_head(&self, repository: &RepositoryRef, branch: &str) -> Option<String> {
        self.remote_head(repository, branch)
    }
    fn set_change_head(&mut self, _repository: &RepositoryRef, change_id: &str, head: &str) {
        self.set_head(change_id, head);
    }
    fn emit(&self, store: &SqliteStore, config: &WorkloopConfig, observation: Observation) {
        self.emit(store, config, observation, NOW).unwrap();
    }
    fn seed_issue(&mut self, issue: &IssueRef, source_revision: &str, content_digest: &str) {
        self.set_issue(issue, source_revision, content_digest, IssueState::Open);
    }
    fn issue_read(&self, issue: &IssueRef) -> Option<(String, String, IssueState)> {
        self.issue_read(issue)
            .map(|read| (read.source_revision, read.content_digest, read.state))
    }
}

impl OutboxHost for GitlabFixture {
    fn declared_names(&self) -> BTreeSet<&'static str> {
        GitlabFixture::declared_capabilities()
            .iter()
            .copied()
            .collect()
    }
    fn apply(&mut self, store: &SqliteStore, config: &WorkloopConfig) {
        self.apply_outbox(store, config, NOW).unwrap();
    }
    fn applied(&self) -> &[String] {
        self.applied()
    }
    fn remote_head(&self, repository: &RepositoryRef, branch: &str) -> Option<String> {
        self.remote_head(repository, branch)
    }
    fn set_change_head(&mut self, repository: &RepositoryRef, change_id: &str, head: &str) {
        self.set_head(repository, change_id, head);
    }
    fn emit(&self, store: &SqliteStore, config: &WorkloopConfig, observation: Observation) {
        self.emit(store, config, observation, NOW).unwrap();
    }
}

impl OutboxHost for SparseHost {
    fn declared_names(&self) -> BTreeSet<&'static str> {
        SparseHost::declared_capabilities()
            .iter()
            .copied()
            .collect()
    }
    fn apply(&mut self, store: &SqliteStore, config: &WorkloopConfig) {
        self.apply_outbox(store, config, NOW).unwrap();
    }
    fn applied(&self) -> &[String] {
        self.applied()
    }
    fn remote_head(&self, repository: &RepositoryRef, branch: &str) -> Option<String> {
        self.remote_head(repository, branch)
    }
    fn set_change_head(&mut self, repository: &RepositoryRef, change_id: &str, head: &str) {
        self.set_head(repository, change_id, head);
    }
    fn emit(&self, store: &SqliteStore, config: &WorkloopConfig, observation: Observation) {
        self.emit(store, config, observation, NOW).unwrap();
    }
}

fn capabilities(names: &BTreeSet<&'static str>) -> BTreeSet<HostCapability> {
    names
        .iter()
        .map(|n| HostCapability::from_name(n).unwrap_or_else(|| panic!("capability name {n}")))
        .collect()
}

fn config_for(fixture: &dyn OutboxHost) -> WorkloopConfig {
    config_with(capabilities(&fixture.declared_names()), true)
}

fn config_with(declared: BTreeSet<HostCapability>, host_write: bool) -> WorkloopConfig {
    WorkloopConfig {
        host: HostTemplate {
            adapter: AdapterId::trusted("conformance-host"),
            repository: repo(),
            contract_version: 1,
            actors: vec![RoleBinding {
                role: ActorRole::Publication,
                actor: publisher(),
                credential_ref: CredentialRef::trusted("cred-publication"),
            }],
        },
        declared_capabilities: declared,
        lanes: LanePolicy::default(),
        required_checks: vec![],
        allowed_effects: AllowedEffects {
            workspace_write: true,
            host_write,
            network: false,
        },
        budget: ExecutionBudget::default(),
        policy: WorkPolicy::default(),
    }
}

fn submit(store: &SqliteStore, config: &WorkloopConfig, key: &str) -> (WorkId, ExecutionId) {
    let d = handle_command(
        store,
        config,
        &CommandRequest {
            actor: Actor {
                kind: ActorKind::Human,
                external_id: "op".into(),
            },
            origin: Origin("conformance".into()),
            idempotency_key: key.into(),
            body: CommandBody::Submit {
                intent: "conformance work".into(),
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

fn insert_intent(
    store: &SqliteStore,
    work_id: &WorkId,
    repository: &RepositoryRef,
    action: HostAction,
) -> String {
    store
        .transact(|tx| {
            let action_id = tx.alloc_id("act")?;
            let created_seq = tx.alloc_seq()?;
            tx.apply(&Effect::InsertHostActionIntent {
                intent: HostActionIntent {
                    action_id: action_id.clone(),
                    work_id: work_id.clone(),
                    host: host(),
                    repository: repository.clone(),
                    role: ActorRole::Publication,
                    actor: publisher(),
                    action,
                    payload_digest: "conformance".into(),
                    status: HostActionStatus::Pending,
                    authenticated_actor: None,
                    effects: vec![],
                    created_seq,
                },
            })?;
            Ok(action_id)
        })
        .unwrap()
}

fn intent(store: &SqliteStore, action_id: &str) -> HostActionIntent {
    store
        .transact(|tx| {
            Ok(tx
                .action_intents()?
                .into_iter()
                .find(|i| i.action_id == action_id)
                .expect("intent recorded"))
        })
        .unwrap()
}

// MATRIX-CODE-BEGIN
// The table and runner below are the host-neutral matrix: no provider
// name or provider condition may appear in this region. Each row names
// the contract capabilities it exercises; a fixture either runs the
// unchanged scenario (when its declared set covers the row) or produces
// an explicit unsupported outcome (when it does not). There is no skip
// path.

/// One host-neutral scenario: the capabilities it requires plus the
/// assertions every conforming fixture must satisfy. `run` panics on a
/// contract violation, so a completed scenario is a covered matrix row.
struct Scenario {
    id: &'static str,
    requires: &'static [&'static str],
    run: fn(&mut dyn OutboxHost, &SqliteStore, &WorkloopConfig),
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum MatrixOutcome {
    Covered,
    Unsupported { capability: &'static str },
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct MatrixResult {
    row: &'static str,
    outcome: MatrixOutcome,
}

fn covered(requires: &[&'static str], declared: &BTreeSet<&'static str>) -> bool {
    requires.iter().all(|r| declared.contains(r))
}

/// A concrete action for a capability whose operation is in the
/// executable vocabulary. Used only when a fixture does not declare the
/// capability: the fixture must refuse it explicitly.
fn unsupported_operation_action(capability: &str) -> Option<HostAction> {
    let change = ChangeRequestRef {
        repository: repo(),
        change_request: ChangeRequestId::trusted("conformance-unsupported"),
        display_number: Some(1),
    };
    match capability {
        "push_ref" => Some(HostAction::PushRef {
            branch: "conformance/unsupported".into(),
            produced: rev("conformance-head"),
        }),
        "change_request_create" => Some(HostAction::OpenChangeRequest {
            source_branch: "conformance/unsupported".into(),
            target_branch: "main".into(),
            title: "unsupported".into(),
            body: String::new(),
        }),
        "change_request_update" => Some(HostAction::UpdateChangeRequest {
            change,
            expected_head: rev("conformance-head"),
            title: None,
            body: None,
        }),
        "comment_write" | "comment_create" => Some(HostAction::PostComment {
            change,
            expected_head: rev("conformance-head"),
            marker: "conformance".into(),
            body: "unsupported".into(),
        }),
        _ => None,
    }
}

fn unsupported_operation_label(capability: &str) -> &str {
    match capability {
        "push_ref" => "push_ref",
        "change_request_create" => "open_change_request",
        "change_request_update" => "update_change_request",
        "comment_write" | "comment_create" => "post_comment",
        other => other,
    }
}

/// The contract operation a declarable-but-unimplemented capability
/// would need. The matrix asserts the operation is absent from the
/// executable vocabulary — a declared capability without an operation
/// cannot be silently counted as covered.
fn absent_operation_name(capability: &str) -> Option<&'static str> {
    match capability {
        "inline_comment" => Some("post_inline_comment"),
        "review_feedback" => Some("publish_review_feedback"),
        "approve" => Some("approve_change"),
        "request_changes" => Some("request_changes"),
        "thread_reply" => Some("thread_reply"),
        "thread_resolve" => Some("resolve_thread"),
        "mark_ready" | "ready" => Some("mark_ready"),
        _ => None,
    }
}

/// Turn one undeclared required capability into an explicit unsupported
/// result: an admission rejection, a `failed` receipt naming the
/// operation, or a type-level absent operation. Never a skip.
fn assert_unsupported_capability(
    fixture: &mut dyn OutboxHost,
    store: &SqliteStore,
    config: &WorkloopConfig,
    capability: &'static str,
) {
    let cap = HostCapability::from_name(capability)
        .unwrap_or_else(|| panic!("matrix capability {capability:?} is not in the contract set"));
    assert!(
        !config.declared_capabilities.contains(&cap),
        "fixture declared {capability:?}: the covered scenario must run, not be reported unsupported"
    );

    if capability == "issue_read" {
        let writes_before = fixture.applied().len();
        let decision = handle_command(
            store,
            config,
            &CommandRequest {
                actor: Actor {
                    kind: ActorKind::Human,
                    external_id: "op".into(),
                },
                origin: Origin("conformance".into()),
                idempotency_key: format!("unsupported-{capability}"),
                body: CommandBody::Submit {
                    intent: "requires an undeclared capability".into(),
                    sources: vec![SourceSnapshot {
                        issue: IssueRef {
                            repository: repo(),
                            issue: IssueId::trusted("1"),
                            display_number: Some(1),
                        },
                        source_revision: "r1".into(),
                        content_digest: "d1".into(),
                    }],
                    constraints: vec![],
                    verification_argv: vec![],
                },
            },
            NOW,
        )
        .unwrap();
        match decision {
            CommandDecision::Rejected { reason, .. } => {
                assert_eq!(reason, RejectionCode::UnsupportedCapability);
            }
            other => panic!("undeclared {capability} must reject admission: {other:?}"),
        }
        assert_eq!(
            fixture.applied().len(),
            writes_before,
            "an unsupported capability never writes"
        );
    } else if let Some(action) = unsupported_operation_action(capability) {
        let (work_id, _eid) = submit(store, config, &format!("unsupported-{capability}"));
        let action_id = insert_intent(store, &work_id, &repo(), action);
        let writes_before = fixture.applied().len();
        fixture.apply(store, config);
        match intent(store, &action_id).status {
            HostActionStatus::Failed { ref reason } => {
                let operation = unsupported_operation_label(capability);
                assert!(
                    reason.contains("unsupported") && reason.contains(operation),
                    "undeclared {capability} must fail explicitly as {operation}: {reason}"
                );
            }
            other => panic!("undeclared {capability} must not be applied or dropped: {other:?}"),
        }
        assert_eq!(
            fixture.applied().len(),
            writes_before,
            "an unsupported operation lands no write"
        );
    } else if let Some(operation) = absent_operation_name(capability) {
        assert!(
            serde_json::from_value::<HostAction>(serde_json::json!({ "action": operation }))
                .is_err(),
            "matrix operation {operation} for {capability:?} is in the vocabulary; it needs a covered scenario"
        );
    } else {
        panic!("matrix capability {capability:?} has no explicit unsupported probe");
    }
}

/// Reached only if a fixture declares an optional capability whose
/// executable operation is absent: that is a contract gap, not a row
/// that may pass silently.
fn declared_capability_without_operation(
    _fixture: &mut dyn OutboxHost,
    _store: &SqliteStore,
    _config: &WorkloopConfig,
) {
    panic!("a declared optional capability has no executable operation; the row cannot be covered");
}

/// HC-BASE-01/02: `push_ref` and `open_change_request` each produce an
/// applied receipt carrying the normalized effect facts, bound to the
/// intent's recorded repository.
fn baseline_effects_apply(
    fixture: &mut dyn OutboxHost,
    store: &SqliteStore,
    config: &WorkloopConfig,
) {
    let (work_id, _eid) = submit(store, config, "hc-base-1");
    insert_intent(
        store,
        &work_id,
        &repo(),
        HostAction::PushRef {
            branch: format!("work/{}", work_id.0),
            produced: rev("head-1"),
        },
    );
    fixture.apply(store, config);
    let intents = store.transact(|tx| tx.action_intents()).unwrap();
    let push = intents
        .iter()
        .find(|i| matches!(i.action, HostAction::PushRef { .. }))
        .unwrap();
    assert_eq!(push.status, HostActionStatus::Applied);
    match &push.effects[..] {
        [
            NormalizedEffectFact::RefPublished {
                repository,
                produced,
                observed,
                relation,
                ..
            },
        ] => {
            assert_eq!(*repository, repo(), "receipt keeps the intent binding");
            assert_eq!(produced, observed);
            assert_eq!(*relation, RefRelation::Equal);
        }
        other => panic!("push receipt missing RefPublished fact: {other:?}"),
    }
    assert_eq!(fixture.applied().len(), 1, "exactly one remote write");
    assert_eq!(
        fixture.remote_head(&repo(), &format!("work/{}", work_id.0)),
        Some("head-1".to_string())
    );

    insert_intent(
        store,
        &work_id,
        &repo(),
        HostAction::OpenChangeRequest {
            source_branch: format!("work/{}", work_id.0),
            target_branch: "main".into(),
            title: "t".into(),
            body: String::new(),
        },
    );
    fixture.apply(store, config);
    let intents = store.transact(|tx| tx.action_intents()).unwrap();
    let open = intents
        .iter()
        .find(|i| matches!(i.action, HostAction::OpenChangeRequest { .. }))
        .unwrap();
    assert_eq!(open.status, HostActionStatus::Applied);
    match &open.effects[..] {
        [NormalizedEffectFact::ChangeOpened { change, head, .. }] => {
            assert_eq!(change.repository, repo());
            assert_eq!(head.repository, repo());
        }
        other => panic!("open receipt missing ChangeOpened fact: {other:?}"),
    }
    assert_eq!(
        fixture.applied().len(),
        2,
        "push then open: two writes total"
    );
}

/// HC-BASE-03/04: every head-bound write (`update_change_request`,
/// `post_comment`) re-checks the change head immediately before
/// writing; a moved head aborts with a failed receipt and the fresh
/// head surfaces — never a write at the wrong target.
fn expected_head_mismatch_aborts_write(
    fixture: &mut dyn OutboxHost,
    store: &SqliteStore,
    config: &WorkloopConfig,
) {
    let (work_id, _eid) = submit(store, config, "hc-base-3");
    insert_intent(
        store,
        &work_id,
        &repo(),
        HostAction::PushRef {
            branch: format!("work/{}", work_id.0),
            produced: rev("head-1"),
        },
    );
    insert_intent(
        store,
        &work_id,
        &repo(),
        HostAction::OpenChangeRequest {
            source_branch: format!("work/{}", work_id.0),
            target_branch: "main".into(),
            title: "t".into(),
            body: String::new(),
        },
    );
    fixture.apply(store, config);
    let intents = store.transact(|tx| tx.action_intents()).unwrap();
    let (change, opened_head) = intents
        .iter()
        .find_map(|i| {
            i.effects.iter().find_map(|f| match f {
                NormalizedEffectFact::ChangeOpened { change, head, .. } => {
                    Some((change.clone(), head.clone()))
                }
                _ => None,
            })
        })
        .expect("opened change request");

    // The same write lands while the recorded head still matches: an
    // applied receipt carrying the metadata-update fact — the success
    // branch every head-bound-capable fixture implements, not only the
    // abort.
    insert_intent(
        store,
        &work_id,
        &repo(),
        HostAction::UpdateChangeRequest {
            change: change.clone(),
            expected_head: opened_head.clone(),
            title: Some("edited".into()),
            body: None,
        },
    );
    fixture.apply(store, config);
    let intents = store.transact(|tx| tx.action_intents()).unwrap();
    let update = intents
        .iter()
        .find(|i| matches!(i.action, HostAction::UpdateChangeRequest { .. }))
        .unwrap();
    assert_eq!(update.status, HostActionStatus::Applied);
    match &update.effects[..] {
        [
            NormalizedEffectFact::MetadataUpdated {
                change: c, head, ..
            },
        ] => {
            assert_eq!(*c, change);
            assert_eq!(*head, opened_head);
        }
        other => panic!("update receipt missing MetadataUpdated fact: {other:?}"),
    }

    // Move the remote head under the recorded intent. The guard is
    // exercised only because the expected head really was the opened
    // head — the mismatch must be caused by the move.
    let writes_before = fixture.applied().len();
    fixture.set_change_head(&repo(), change.change_request.as_str(), "moved-head");
    let aborted_update_id = insert_intent(
        store,
        &work_id,
        &repo(),
        HostAction::UpdateChangeRequest {
            change: change.clone(),
            expected_head: opened_head.clone(),
            title: Some("edited".into()),
            body: None,
        },
    );
    fixture.apply(store, config);
    let intents = store.transact(|tx| tx.action_intents()).unwrap();
    let update = intents
        .iter()
        .find(|i| i.action_id == aborted_update_id)
        .unwrap();
    assert!(
        matches!(update.status, HostActionStatus::Failed { .. }),
        "moved head aborts the write: {:?}",
        update.status
    );
    assert_eq!(
        fixture.applied().len(),
        writes_before,
        "the aborted update landed no write"
    );

    // The same guard binds comment writes: a comment posted against
    // the recorded head aborts identically.
    insert_intent(
        store,
        &work_id,
        &repo(),
        HostAction::PostComment {
            change: change.clone(),
            expected_head: opened_head.clone(),
            marker: "conformance".into(),
            body: "b".into(),
        },
    );
    fixture.apply(store, config);
    let intents = store.transact(|tx| tx.action_intents()).unwrap();
    let comment = intents
        .iter()
        .find(|i| matches!(i.action, HostAction::PostComment { .. }))
        .unwrap();
    assert!(
        matches!(comment.status, HostActionStatus::Failed { .. }),
        "moved head aborts the comment write: {:?}",
        comment.status
    );
    assert_eq!(
        fixture.applied().len(),
        writes_before,
        "the aborted comment write landed nothing either"
    );
}

/// HC-PART-01: an applied effect alongside an unresolved one keeps both
/// receipts separate — no combined-success claim, and the unknown
/// intent is never retried blindly.
fn partial_receipts_stay_separate(
    fixture: &mut dyn OutboxHost,
    store: &SqliteStore,
    config: &WorkloopConfig,
) {
    let (work_id, _eid) = submit(store, config, "hc-part-1");
    insert_intent(
        store,
        &work_id,
        &repo(),
        HostAction::PushRef {
            branch: format!("work/{}", work_id.0),
            produced: rev("head-1"),
        },
    );
    let open_id = insert_intent(
        store,
        &work_id,
        &repo(),
        HostAction::OpenChangeRequest {
            source_branch: format!("work/{}", work_id.0),
            target_branch: "main".into(),
            title: "t".into(),
            body: String::new(),
        },
    );
    // The create response is lost before the outbox drains: the push
    // applies while the open stays unresolved.
    fixture.emit(
        store,
        config,
        Observation {
            delivery: None,
            subject_work_id: Some(work_id.clone()),
            subject_execution_id: None,
            fact: ObservationFact::HostActionResult {
                action_id: open_id.clone(),
                authenticated_actor: Some(publisher()),
                status: HostActionStatus::Unknown {
                    reason: "response lost".into(),
                },
                effects: vec![],
            },
            observed_unix_seconds: NOW,
        },
    );
    fixture.apply(store, config);
    let intents = store.transact(|tx| tx.action_intents()).unwrap();
    let push = intents
        .iter()
        .find(|i| matches!(i.action, HostAction::PushRef { .. }))
        .unwrap();
    assert_eq!(push.status, HostActionStatus::Applied, "push receipt kept");
    assert_eq!(
        intent(store, &open_id).status,
        HostActionStatus::Unknown {
            reason: "response lost".into()
        },
        "the unresolved effect stays outstanding"
    );
    // A further drain does not replay the unknown intent.
    fixture.apply(store, config);
    assert_eq!(
        intent(store, &open_id).status,
        HostActionStatus::Unknown {
            reason: "response lost".into()
        }
    );
}

/// HC-SNAP-01: evidence binds to the immutable head it examined. A
/// complete evidence set recorded against a superseded head — check
/// green, review complete, assessment positive — cannot complete the
/// Work while the recorded change head is newer.
fn stale_head_evidence_cannot_complete(
    fixture: &mut dyn OutboxHost,
    store: &SqliteStore,
    config: &WorkloopConfig,
) {
    let (work_id, eid) = submit(store, config, "hc-snap-1");
    store
        .transact(|tx| admit_queued(tx, config, &eid, NOW).map(|_| ()))
        .unwrap();
    // Park the execution in Waiting/Ci with a finished writer and
    // applied publish/open intents — completion turns solely on the
    // current-head evidence lanes.
    store
        .transact(|tx| {
            let request = tx
                .pending_requests()?
                .into_iter()
                .find(|r| r.execution_id == eid)
                .expect("admitted coding request");
            let invocation_id = InvocationId(format!("{}-coding", eid.0));
            tx.apply(&Effect::InsertInvocation {
                invocation: Invocation {
                    id: invocation_id.clone(),
                    request_id: request.request_id,
                    execution_id: eid.clone(),
                    role: InvocationRole::Coding,
                    state: InvocationState::Done,
                    process_identity: None,
                    descendants: vec![],
                    required_artifacts: vec![RequiredArtifact {
                        kind: "artifact".into(),
                        path: "artifact.json".into(),
                        expected_digest: None,
                    }],
                    evidence_refs: vec![],
                },
            })?;
            tx.apply(&Effect::InsertObservation {
                observation: Observation {
                    delivery: None,
                    subject_work_id: None,
                    subject_execution_id: Some(eid.clone()),
                    fact: ObservationFact::Artifact {
                        invocation_id,
                        required_artifact: "artifact.json".into(),
                        state: ArtifactFact::Present {
                            digest: "artifact-digest".into(),
                        },
                    },
                    observed_unix_seconds: NOW,
                },
            })?;
            let mut execution = tx.execution(&eid)?.unwrap();
            execution.state = ExecutionState::Waiting;
            execution.waiting_reason = Some(WaitingReason::Ci);
            tx.apply(&Effect::UpdateExecution {
                execution,
                expected_state: ExecutionState::Ready,
            })?;
            for (action_id, action) in [
                (
                    "act-pub-push",
                    HostAction::PushRef {
                        branch: format!("work/{}", work_id.0),
                        produced: rev("artifact-digest"),
                    },
                ),
                (
                    "act-pub-open",
                    HostAction::OpenChangeRequest {
                        source_branch: format!("work/{}", work_id.0),
                        target_branch: "main".into(),
                        title: "t".into(),
                        body: "b".into(),
                    },
                ),
            ] {
                let created_seq = tx.alloc_seq()?;
                tx.apply(&Effect::InsertHostActionIntent {
                    intent: HostActionIntent {
                        action_id: action_id.into(),
                        work_id: work_id.clone(),
                        host: host(),
                        repository: repo(),
                        role: ActorRole::Publication,
                        actor: publisher(),
                        action,
                        payload_digest: "conformance".into(),
                        status: HostActionStatus::Applied,
                        authenticated_actor: None,
                        effects: vec![],
                        created_seq,
                    },
                })?;
            }
            Ok(())
        })
        .unwrap();

    let change = ChangeRequestRef {
        repository: repo(),
        change_request: ChangeRequestId::trusted("cr-1"),
        display_number: Some(1),
    };
    let target = |head: &str| DiffTarget {
        change: change.clone(),
        head: rev(head),
        base: None,
        target_revision: None,
    };
    let emit = |fact: ObservationFact| {
        fixture.emit(
            store,
            config,
            Observation {
                delivery: None,
                subject_work_id: Some(work_id.clone()),
                subject_execution_id: Some(eid.clone()),
                fact,
                observed_unix_seconds: NOW,
            },
        );
    };
    // The current head is sha-2.
    emit(ObservationFact::HostActionResult {
        action_id: "act-pub-open".into(),
        authenticated_actor: Some(publisher()),
        status: HostActionStatus::Applied,
        effects: vec![NormalizedEffectFact::ChangeOpened {
            change: change.clone(),
            head: rev("sha-2"),
            source_branch: format!("work/{}", work_id.0),
            target_branch: "main".into(),
        }],
    });
    emit(ObservationFact::ChangeHead {
        change: change.clone(),
        head: rev("sha-2"),
    });
    // A complete evidence set bound to the superseded head sha-1 —
    // green check, complete review, positive assessment — is valid
    // evidence for sha-1 only and must not complete the Work.
    emit(ObservationFact::Check {
        change: change.clone(),
        head: rev("sha-1"),
        check_id: "ci".into(),
        producer: publisher(),
        conclusion: CheckConclusion::Success,
    });
    emit(ObservationFact::Review {
        target: target("sha-1"),
        source: "reviewer".into(),
        source_revision: None,
        completeness: Completeness::Complete,
        occurrences: vec![],
    });
    emit(ObservationFact::Assessment {
        target: target("sha-1"),
        set_digest: "invariants".into(),
        complete: true,
        items: vec![AssessmentItem {
            id: "inv-1".into(),
            verdict: AssessmentVerdict::Positive,
            refs: vec![],
        }],
    });
    let e = store.transact(|tx| tx.execution(&eid)).unwrap().unwrap();
    assert_eq!(
        e.state,
        ExecutionState::Waiting,
        "evidence for a superseded head never completes the Work"
    );
}

/// HC-READ-01: an issue read is scoped by (repository, issue ID) and
/// preserves the source revision and content digest it observed. The
/// same display number on another repository is a different issue, not
/// an interchangeable read.
fn issue_reads_are_scoped_and_revision_bound(
    fixture: &mut dyn OutboxHost,
    _store: &SqliteStore,
    _config: &WorkloopConfig,
) {
    let issue = IssueRef {
        repository: repo(),
        issue: IssueId::trusted("issue-1"),
        display_number: Some(1),
    };
    fixture.seed_issue(&issue, "source-rev-1", "content-digest-1");
    let read = fixture
        .issue_read(&issue)
        .expect("a fixture declaring issue_read serves the read");
    assert_eq!(read.0, "source-rev-1");
    assert_eq!(read.1, "content-digest-1");
    assert_eq!(read.2, IssueState::Open);

    let same_number_elsewhere = IssueRef {
        repository: repo_b(),
        issue: IssueId::trusted("issue-1"),
        display_number: Some(1),
    };
    assert!(
        fixture.issue_read(&same_number_elsewhere).is_none(),
        "an issue read is scoped by repository; the shared display number is not an identity"
    );
}

/// HC-PERM-01: authorization loss is distinct from support: with
/// `host_write` denied the intents stay `pending` — no write, and no
/// fabricated failure or unsupported claim.
fn denied_permission_is_not_unsupported(
    fixture: &mut dyn OutboxHost,
    store: &SqliteStore,
    _config: &WorkloopConfig,
) {
    let denied = config_with(
        capabilities(&fixture.declared_names()),
        /* host_write */ false,
    );
    let (work_id, _eid) = submit(store, &denied, "hc-perm-1");
    let action_id = insert_intent(
        store,
        &work_id,
        &repo(),
        HostAction::PushRef {
            branch: format!("work/{}", work_id.0),
            produced: rev("head-1"),
        },
    );
    let writes_before = fixture.applied().len();
    fixture.apply(store, &denied);
    assert_eq!(
        intent(store, &action_id).status,
        HostActionStatus::Pending,
        "denied permission defers the intent; it is not a capability verdict"
    );
    assert_eq!(
        fixture.applied().len(),
        writes_before,
        "no write under denied grant"
    );
}

/// HC-SCOPE-01: identical branch names and display numbers on different
/// repositories produce separate receipts each bound to their recorded
/// repository — no display-identity aliasing.
fn scoped_identities_do_not_collide(
    fixture: &mut dyn OutboxHost,
    store: &SqliteStore,
    config: &WorkloopConfig,
) {
    let (work_a, _ea) = submit(store, config, "hc-scope-a");
    let (work_b, _eb) = submit(store, config, "hc-scope-b");
    // A repository namespace only this scenario opens against: its
    // first change request is display #1 no matter what the suite ran
    // before — the per-repository-numbering assertion stays true for
    // any scenario order or fixture state, not just the current one.
    let repo_scoped = RepositoryRef::new(host(), RepositoryId::trusted("hc-scoped-repo"));
    let mut push_ids = Vec::new();
    for (work, repository, object) in [
        (&work_a, repo(), "head-a"),
        (&work_b, repo_b(), "head-b"),
        (&work_a, repo_scoped.clone(), "head-scoped"),
    ] {
        push_ids.push(insert_intent(
            store,
            work,
            &repository,
            HostAction::PushRef {
                branch: "work/shared-name".into(),
                produced: rev_on(&repository, object),
            },
        ));
    }
    let mut open_ids = Vec::new();
    for (work, repository) in [
        (&work_a, repo()),
        (&work_b, repo_b()),
        (&work_a, repo_scoped.clone()),
    ] {
        open_ids.push(insert_intent(
            store,
            work,
            &repository,
            HostAction::OpenChangeRequest {
                source_branch: "work/shared-name".into(),
                target_branch: "main".into(),
                title: "t".into(),
                body: String::new(),
            },
        ));
    }
    fixture.apply(store, config);
    let intents = store.transact(|tx| tx.action_intents()).unwrap();
    let mut bound = BTreeSet::new();
    let mut display = std::collections::BTreeMap::new();
    let mine: Vec<_> = intents
        .iter()
        .filter(|i| push_ids.contains(&i.action_id) || open_ids.contains(&i.action_id))
        .collect();
    assert_eq!(mine.len(), 6, "three pushes and three opens");
    for i in mine {
        assert_eq!(i.status, HostActionStatus::Applied);
        match &i.effects[..] {
            [NormalizedEffectFact::RefPublished { repository, .. }] => {
                bound.insert(repository.clone());
            }
            [NormalizedEffectFact::ChangeOpened { change, .. }] => {
                bound.insert(change.repository.clone());
                display.insert(change.repository.clone(), change.display_number);
            }
            other => panic!("receipt missing binding: {other:?}"),
        }
    }
    assert_eq!(
        bound,
        BTreeSet::from([repo(), repo_b(), repo_scoped.clone()]),
        "each receipt keeps its own repository binding"
    );
    // The remote itself kept the writes separate: identical branch
    // names on three repositories each retain their own head object.
    assert_eq!(
        fixture.remote_head(&repo(), "work/shared-name"),
        Some("head-a".to_string()),
        "repo A's remote branch keeps its own head"
    );
    assert_eq!(
        fixture.remote_head(&repo_b(), "work/shared-name"),
        Some("head-b".to_string()),
        "repo B's remote branch keeps its own head"
    );
    assert_eq!(
        fixture.remote_head(&repo_scoped, "work/shared-name"),
        Some("head-scoped".to_string()),
        "the scoped repository's branch keeps its own head"
    );
    // Display numbers are a per-repository namespace: `hc-scoped-repo`
    // is opened only by this scenario, so its first change request is
    // #1 even though this scenario already numbered repositories A and
    // B — under a global counter it could not be 1, whatever earlier
    // scenarios left behind.
    assert_eq!(display.get(&repo_scoped), Some(&Some(1)));
}

/// HC-SWAP-01: a configuration change after an intent was recorded does
/// not retarget it — the receipt keeps the original recorded binding.
fn config_swap_keeps_recorded_binding(
    fixture: &mut dyn OutboxHost,
    store: &SqliteStore,
    config: &WorkloopConfig,
) {
    let (work_id, _eid) = submit(store, config, "hc-swap-1");
    insert_intent(
        store,
        &work_id,
        &repo(),
        HostAction::PushRef {
            branch: format!("work/{}", work_id.0),
            produced: rev("head-1"),
        },
    );
    // The installation's default repository moved, but the recorded
    // intent carries its original binding.
    let mut swapped = config_for(fixture);
    swapped.host.repository = repo_b();
    fixture.apply(store, &swapped);
    let intents = store.transact(|tx| tx.action_intents()).unwrap();
    let push = intents
        .iter()
        .find(|i| matches!(i.action, HostAction::PushRef { .. }))
        .unwrap();
    assert_eq!(push.repository, repo(), "intent keeps its recorded binding");
    match &push.effects[..] {
        [NormalizedEffectFact::RefPublished { repository, .. }] => {
            assert_eq!(*repository, repo(), "receipt binds the original repository");
        }
        other => panic!("push receipt missing binding: {other:?}"),
    }
}

/// HC-CAP-01: a Work whose required capabilities are not declared as
/// supported is rejected `unsupported_capability` — support failure is
/// explicit at admission, never a deferred surprise.
fn unsupported_capability_stays_explicit(
    _fixture: &mut dyn OutboxHost,
    store: &SqliteStore,
    _config: &WorkloopConfig,
) {
    let missing = config_with(
        BTreeSet::from([HostCapability::RepositoryRead, HostCapability::PushRef]),
        true,
    );
    // Declaring a source issue requires `issue_read`, which this
    // installation does not declare as supported.
    let d = handle_command(
        store,
        &missing,
        &CommandRequest {
            actor: Actor {
                kind: ActorKind::Human,
                external_id: "op".into(),
            },
            origin: Origin("conformance".into()),
            idempotency_key: "hc-cap-1".into(),
            body: CommandBody::Submit {
                intent: "needs issue_read".into(),
                sources: vec![SourceSnapshot {
                    issue: IssueRef {
                        repository: repo(),
                        issue: IssueId::trusted("1"),
                        display_number: Some(1),
                    },
                    source_revision: "r1".into(),
                    content_digest: "d1".into(),
                }],
                constraints: vec![],
                verification_argv: vec![],
            },
        },
        NOW,
    )
    .unwrap();
    match d {
        CommandDecision::Rejected { reason, .. } => {
            assert_eq!(reason, RejectionCode::UnsupportedCapability);
        }
        other => panic!("undeclared required capability must reject: {other:?}"),
    }
}

/// The shared suite, parameterized only over the fixture and its
/// declared capability set. Baseline scenarios run wherever their
/// requirements are declared.
fn scenarios() -> Vec<Scenario> {
    vec![
        Scenario {
            id: "HC-BASE-01/02 baseline effects apply with typed receipts",
            requires: &["push_ref", "change_request_create"],
            run: baseline_effects_apply,
        },
        Scenario {
            id: "HC-BASE-03/04 expected-head mismatch aborts head-bound writes",
            requires: &[
                "push_ref",
                "change_request_create",
                "change_request_update",
                "comment_create",
            ],
            run: expected_head_mismatch_aborts_write,
        },
        Scenario {
            id: "HC-PART-01 partial receipts stay separate",
            requires: &["push_ref", "change_request_create"],
            run: partial_receipts_stay_separate,
        },
        Scenario {
            id: "HC-SNAP-01 stale-head evidence cannot complete",
            requires: &[
                "push_ref",
                "change_request_create",
                "checks_read",
                "review_findings_read",
            ],
            run: stale_head_evidence_cannot_complete,
        },
        Scenario {
            id: "HC-READ-01 issue read is scoped and revision-bound",
            requires: &["issue_read"],
            run: issue_reads_are_scoped_and_revision_bound,
        },
        Scenario {
            id: "HC-PERM-01 denied permission is not unsupported",
            requires: &[],
            run: denied_permission_is_not_unsupported,
        },
        Scenario {
            id: "HC-SCOPE-01 scoped identities do not collide",
            requires: &["push_ref"],
            run: scoped_identities_do_not_collide,
        },
        Scenario {
            id: "HC-SWAP-01 config swap keeps recorded binding",
            requires: &["push_ref"],
            run: config_swap_keeps_recorded_binding,
        },
        Scenario {
            id: "HC-CAP-01 unsupported required capability stays explicit",
            requires: &[],
            run: unsupported_capability_stays_explicit,
        },
        Scenario {
            id: "HC-OPT-01 approve review write stays explicitly unsupported",
            requires: &["approve"],
            run: declared_capability_without_operation,
        },
        Scenario {
            id: "HC-OPT-02 ready marking stays explicitly unsupported",
            requires: &["mark_ready"],
            run: declared_capability_without_operation,
        },
        Scenario {
            id: "HC-OPT-03 request-changes review write stays explicitly unsupported",
            requires: &["request_changes"],
            run: declared_capability_without_operation,
        },
        Scenario {
            id: "HC-OPT-04 inline comment stays explicitly unsupported",
            requires: &["inline_comment"],
            run: declared_capability_without_operation,
        },
        Scenario {
            id: "HC-OPT-05 review feedback stays explicitly unsupported",
            requires: &["review_feedback"],
            run: declared_capability_without_operation,
        },
        Scenario {
            id: "HC-OPT-06 thread reply stays explicitly unsupported",
            requires: &["thread_reply"],
            run: declared_capability_without_operation,
        },
        Scenario {
            id: "HC-OPT-07 thread resolve stays explicitly unsupported",
            requires: &["thread_resolve"],
            run: declared_capability_without_operation,
        },
    ]
}

/// Execute every matrix row against one fixture. A row whose required
/// capabilities are declared runs the unchanged scenario; every
/// undeclared required capability is asserted to produce an explicit
/// unsupported result. The returned list has exactly one outcome per
/// row, so no row can be silently skipped.
fn run_suite(fixture: &mut dyn OutboxHost) -> Vec<MatrixResult> {
    let declared = fixture.declared_names();
    let mut results = Vec::new();
    for scenario in scenarios() {
        let store = SqliteStore::in_memory(2).unwrap();
        let config = config_for(fixture);
        let missing: Vec<&'static str> = scenario
            .requires
            .iter()
            .copied()
            .filter(|required| !declared.contains(required))
            .collect();
        if covered(scenario.requires, &declared) {
            (scenario.run)(fixture, &store, &config);
            results.push(MatrixResult {
                row: scenario.id,
                outcome: MatrixOutcome::Covered,
            });
        } else {
            for capability in &missing {
                assert_unsupported_capability(fixture, &store, &config, capability);
            }
            results.push(MatrixResult {
                row: scenario.id,
                outcome: MatrixOutcome::Unsupported {
                    capability: missing[0],
                },
            });
        }
    }
    results
}

fn covered_rows(results: &[MatrixResult]) -> Vec<&'static str> {
    results
        .iter()
        .filter(|r| r.outcome == MatrixOutcome::Covered)
        .map(|r| r.row)
        .collect()
}

fn unsupported_rows(results: &[MatrixResult]) -> Vec<(&'static str, &'static str)> {
    results
        .iter()
        .filter_map(|r| match r.outcome {
            MatrixOutcome::Unsupported { capability } => Some((r.row, capability)),
            MatrixOutcome::Covered => None,
        })
        .collect()
}
// MATRIX-CODE-END

struct RegisteredFixture {
    name: &'static str,
    build: fn() -> Box<dyn OutboxHost>,
}

/// Every host implementation registered with the matrix. The registry
/// is outside the host-neutral matrix region above: a fixture's type
/// name identifies the implementation for reporting, never a branch in
/// shared assertions.
fn registered_fixtures() -> Vec<RegisteredFixture> {
    vec![
        RegisteredFixture {
            name: "fixture-github",
            build: || Box::new(FixtureGithub::new()),
        },
        RegisteredFixture {
            name: "fixture-gitlab",
            build: || Box::new(GitlabFixture::new()),
        },
        RegisteredFixture {
            name: "fixture-sparse",
            build: || Box::new(SparseHost::new()),
        },
    ]
}

#[test]
fn conformance_matrix_runs_every_row_against_every_registered_fixture() {
    let rows = scenarios();
    let fixtures = registered_fixtures();
    assert!(fixtures.len() >= 3, "independent fixtures are registered");
    let mut total = 0;
    let mut unsupported = 0;
    for registration in &fixtures {
        let mut fixture = (registration.build)();
        let results = run_suite(&mut *fixture);
        assert_eq!(
            results.len(),
            rows.len(),
            "{}: every matrix row has an outcome",
            registration.name
        );
        total += results.len();
        unsupported += unsupported_rows(&results).len();
    }
    assert_eq!(total, rows.len() * fixtures.len());
    assert!(
        unsupported > 0,
        "undeclared optional capabilities produce explicit unsupported rows"
    );
}

#[test]
fn fixture_github_covers_the_full_baseline() {
    let mut fixture = FixtureGithub::new();
    let results = run_suite(&mut fixture);
    for result in &results {
        if result.row.starts_with("HC-OPT-") {
            assert!(
                matches!(result.outcome, MatrixOutcome::Unsupported { .. }),
                "optional row is explicit-unsupported: {result:?}"
            );
        } else {
            assert_eq!(
                result.outcome,
                MatrixOutcome::Covered,
                "baseline row is covered: {result:?}"
            );
        }
    }
}

#[test]
fn sparse_host_runs_the_same_suite_within_its_declared_set() {
    let mut fixture = SparseHost::new();
    let results = run_suite(&mut fixture);
    assert_eq!(
        unsupported_rows(&results),
        vec![
            (
                "HC-BASE-03/04 expected-head mismatch aborts head-bound writes",
                "change_request_update"
            ),
            (
                "HC-READ-01 issue read is scoped and revision-bound",
                "issue_read"
            ),
            (
                "HC-OPT-01 approve review write stays explicitly unsupported",
                "approve"
            ),
            (
                "HC-OPT-02 ready marking stays explicitly unsupported",
                "mark_ready"
            ),
            (
                "HC-OPT-03 request-changes review write stays explicitly unsupported",
                "request_changes"
            ),
            (
                "HC-OPT-04 inline comment stays explicitly unsupported",
                "inline_comment"
            ),
            (
                "HC-OPT-05 review feedback stays explicitly unsupported",
                "review_feedback"
            ),
            (
                "HC-OPT-06 thread reply stays explicitly unsupported",
                "thread_reply"
            ),
            (
                "HC-OPT-07 thread resolve stays explicitly unsupported",
                "thread_resolve"
            ),
        ]
    );
    assert_eq!(covered_rows(&results).len(), scenarios().len() - 9);
}

/// `GitlabFixture` — a remote model whose change-request identity is a
/// per-project IID and whose head follows the source branch — runs the
/// unchanged suite within its declared set. Its declared set differs
/// from `FixtureGithub`'s (no `issue_read`): the issue-read row and every
/// optional row produce explicit unsupported outcomes rather than
/// disappearing.
#[test]
fn gitlab_fixture_runs_the_same_suite_within_its_declared_set() {
    let mut fixture = GitlabFixture::new();
    let results = run_suite(&mut fixture);
    for result in &results {
        if result.row.starts_with("HC-OPT-") || result.row.starts_with("HC-READ-") {
            assert!(
                matches!(result.outcome, MatrixOutcome::Unsupported { .. }),
                "undeclared row is explicit-unsupported: {result:?}"
            );
        } else {
            assert_eq!(
                result.outcome,
                MatrixOutcome::Covered,
                "baseline row is covered: {result:?}"
            );
        }
    }
}

/// Static check: the matrix table and runner contain no provider-name
/// branch or provider-specific expectation. The fixture registry (which
/// necessarily names implementation types for reporting) is outside the
/// marked region; shared assertions bind only recorded intents,
/// receipts, and scoped identities.
#[test]
fn matrix_code_has_no_host_name_branches() {
    const SOURCE: &str = include_str!("host_conformance.rs");
    let begin = SOURCE
        .find("// MATRIX-CODE-BEGIN")
        .expect("matrix region start marker");
    let end = SOURCE
        .find("// MATRIX-CODE-END")
        .expect("matrix region end marker");
    let matrix = SOURCE[begin..end].to_ascii_lowercase();
    for provider in ["github", "gitlab"] {
        assert!(
            !matrix.contains(provider),
            "matrix code names provider {provider:?}; shared assertions must stay host-neutral"
        );
    }
    assert!(
        !matrix.contains("fixture.name ==") && !matrix.contains("fixture.name !="),
        "matrix code branches on a fixture name"
    );
    // The whole suite may name fixture types, but no conditional may
    // branch on a provider name anywhere.
    for line in SOURCE.lines() {
        let lower = line.to_ascii_lowercase();
        let names_provider = lower.contains("github") || lower.contains("gitlab");
        let branches = lower.contains("== ")
            || lower.contains("!= ")
            || lower.trim_start().starts_with("match ");
        assert!(
            !(names_provider && branches),
            "host-name branch in conformance suite: {line}"
        );
    }
}

/// Doc check: the U9.1 evidence record must defer any exchangeability
/// claim to #114 and must not assert a GitLab spike result or a
/// satisfied cross-host exchangeability claim.
#[test]
fn evidence_defers_exchangeability_to_114() {
    let evidence = include_str!("../../../docs/evidence/u9.1.md");
    let lower = evidence.to_ascii_lowercase();
    assert!(
        lower.contains("#114"),
        "evidence must name #114 as the owner of the exchangeability claim"
    );
    assert!(
        lower.contains("deferred"),
        "evidence must mark the exchangeability claim deferred"
    );
    for positive in [
        "exchangeability claim: pass",
        "exchangeability: pass",
        "exchangeability is satisfied",
        "exchangeability proven",
        "exchangeable across",
        "interchangeable across",
    ] {
        assert!(
            !lower.contains(positive),
            "evidence asserts a satisfied exchangeability claim: {positive:?}"
        );
    }
    for line in evidence.lines() {
        let l = line.to_ascii_lowercase();
        if l.contains("exchangeability") {
            assert!(
                l.contains("#114")
                    || l.contains("defer")
                    || l.contains("not claim")
                    || l.contains("no cross-host")
                    || l.contains("not asserted"),
                "satisfied exchangeability claim outside a deferral: {line}"
            );
        }
        if l.contains("spike") {
            assert!(
                l.contains("not run")
                    || l.contains("defer")
                    || l.contains("not claim")
                    || l.contains("no gitlab spike")
                    || l.contains("#114")
                    || l.contains("no live"),
                "evidence asserts a spike result: {line}"
            );
        }
    }
}

/// Doc check: the tested revision named by the U9.1 evidence record must
/// resolve to a repository commit that actually contains the exercised
/// suite. A patch digest may supplement that revision but cannot replace
/// a commit another checkout can reproduce.
#[test]
fn evidence_tested_revision_contains_the_exercised_suite() {
    let evidence = include_str!("../../../docs/evidence/u9.1.md");
    let row = evidence
        .lines()
        .find(|line| line.contains("Tested revision (slice commit"))
        .expect("evidence names a tested revision");
    let sha = row
        .split('`')
        .nth(1)
        .expect("Tested revision is a backticked commit id");
    assert_eq!(sha.len(), 40, "tested revision is a full commit id: {sha}");
    assert!(
        sha.chars().all(|c| c.is_ascii_hexdigit()),
        "tested revision is a hex commit id: {sha}"
    );
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let commit = std::process::Command::new("git")
        .arg("-C")
        .arg(&root)
        .args(["rev-parse", "--verify", &format!("{sha}^{{commit}}")])
        .output()
        .expect("git is available");
    assert!(
        commit.status.success(),
        "tested revision {sha} must resolve to a commit in this repository: {}",
        String::from_utf8_lossy(&commit.stderr)
    );
    let spec = format!("{sha}:crates/workloop-github/tests/host_conformance.rs");
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(&root)
        .args(["cat-file", "-e", &spec])
        .output()
        .expect("git is available");
    assert!(
        out.status.success(),
        "tested revision {sha} must contain the exercised suite: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Doc check: the exchangeability report's runner semantics and coverage
/// cells must agree with the matrix runner. A covered row runs unchanged;
/// an undeclared capability is an explicit unsupported outcome, never a
/// per-row skip.
#[test]
fn exchangeability_report_matches_the_matrix_runner() {
    let report = include_str!("../../../docs/host-exchangeability.md");
    let lower = report.to_ascii_lowercase();
    assert!(
        !lower.contains("is skipped for that row"),
        "report still describes a per-row skip path"
    );
    assert!(
        !lower.contains("declared-set skip path"),
        "report still calls undeclared coverage a skip path"
    );
    let base = report
        .lines()
        .find(|line| line.contains("HC-BASE-03/04"))
        .expect("report records the head-bound write row");
    let base_lower = base.to_ascii_lowercase();
    assert!(
        !base_lower.contains("skipped"),
        "head-bound write row is an explicit unsupported outcome, not a skip: {base}"
    );
    assert!(
        base_lower.contains("unsupported"),
        "head-bound write row records the undeclared capability: {base}"
    );
    let read = report
        .lines()
        .find(|line| line.contains("HC-READ-01"))
        .expect("report records the typed-read row");
    let read_lower = read.to_ascii_lowercase();
    assert!(
        read_lower.contains("covered") && read_lower.contains("issue_read"),
        "typed-read row records covered fixtures and the undeclared capability: {read}"
    );
    assert!(
        !read_lower.contains("read-model ops not in vocabulary"),
        "typed-read row is stale: {read}"
    );
}

/// The capability difference is part of the contract surface: this
/// fixture does not declare `issue_read`, so a source-bound Work is
/// rejected `unsupported_capability` at admission — the declared set
/// stays explicit, never an assumed default.
#[test]
fn gitlab_fixture_undeclared_issue_read_rejects_source_bound_work() {
    let fixture = GitlabFixture::new();
    let store = SqliteStore::in_memory(2).unwrap();
    let config = config_for(&fixture);
    let d = handle_command(
        &store,
        &config,
        &CommandRequest {
            actor: Actor {
                kind: ActorKind::Human,
                external_id: "op".into(),
            },
            origin: Origin("conformance".into()),
            idempotency_key: "u92-issue-read".into(),
            body: CommandBody::Submit {
                intent: "needs issue_read".into(),
                sources: vec![SourceSnapshot {
                    issue: IssueRef {
                        repository: repo(),
                        issue: IssueId::trusted("1"),
                        display_number: Some(1),
                    },
                    source_revision: "r1".into(),
                    content_digest: "d1".into(),
                }],
                constraints: vec![],
                verification_argv: vec![],
            },
        },
        NOW,
    )
    .unwrap();
    match d {
        CommandDecision::Rejected { reason, .. } => {
            assert_eq!(reason, RejectionCode::UnsupportedCapability);
        }
        other => panic!("undeclared issue_read must reject: {other:?}"),
    }
}

/// Approval and readiness stay outside the *write* vocabulary: their
/// capability names exist (a fixture may declare them for read-side
/// use), but no `HostAction` variant carries an approval, readiness, or
/// thread write, so an applied receipt for one is not constructible —
/// the unsupported state stays explicit at the type level on every
/// conforming fixture.
#[test]
fn approval_and_ready_stay_explicitly_unsupported() {
    for action in [
        serde_json::json!({ "action": "approve_change" }),
        serde_json::json!({ "action": "mark_ready" }),
        serde_json::json!({ "action": "resolve_thread" }),
    ] {
        assert!(
            serde_json::from_value::<HostAction>(action.clone()).is_err(),
            "action payload {action} cannot deserialize — no applied receipt is constructible"
        );
    }
}

/// On this host the IID namespace is per-project: the *first* merge
/// request on two different repositories is `!1` on both — two change
/// requests sharing `change_request` identity that stay distinct only
/// through the scoped (repository, IID) pair. Receipts, remote reads,
/// and head-bound writes must keep them apart.
#[test]
fn gitlab_fixture_reuses_iid_across_projects_without_aliasing() {
    let mut fixture = GitlabFixture::new();
    let store = SqliteStore::in_memory(2).unwrap();
    let config = config_for(&fixture);
    let (work_a, _ea) = submit(&store, &config, "u92-iid-a");
    let (work_b, _eb) = submit(&store, &config, "u92-iid-b");

    // Both repositories open their first merge request: identical
    // `!1` change-request identity, different scoped refs.
    let mut opens = Vec::new();
    for (work, repository, object) in [(&work_a, repo(), "head-a"), (&work_b, repo_b(), "head-b")] {
        insert_intent(
            &store,
            work,
            &repository,
            HostAction::PushRef {
                branch: format!("work/{object}"),
                produced: rev_on(&repository, object),
            },
        );
        let open_id = insert_intent(
            &store,
            work,
            &repository,
            HostAction::OpenChangeRequest {
                source_branch: format!("work/{object}"),
                target_branch: "main".into(),
                title: "t".into(),
                body: String::new(),
            },
        );
        opens.push(open_id);
    }
    fixture.apply(&store, &config);

    let mut changes = Vec::new();
    for open_id in &opens {
        match &intent(&store, open_id).effects[..] {
            [NormalizedEffectFact::ChangeOpened { change, .. }] => {
                assert_eq!(change.display_number, Some(1));
                changes.push(change.clone());
            }
            other => panic!("open receipt missing ChangeOpened: {other:?}"),
        }
    }
    let [change_a, change_b] = &changes[..] else {
        panic!("expected two opens")
    };
    assert_eq!(change_a.repository, repo());
    assert_eq!(change_b.repository, repo_b());
    // The unscoped identity collides exactly as it does on a real
    // IID-numbered host — only the repository scope separates them.
    assert_eq!(
        change_a.change_request, change_b.change_request,
        "both are `!1` in their own project namespace"
    );
    assert_ne!(
        change_a, change_b,
        "the scoped refs are different change requests"
    );

    // Moving repo A's `!1` head leaves repo B's `!1` untouched: the
    // head-bound write against repo B's recorded head still applies,
    // while the same write against repo A's old head aborts.
    fixture.set_head(&repo(), change_a.change_request.as_str(), "moved-a");
    let comment_b = insert_intent(
        &store,
        &work_b,
        &repo_b(),
        HostAction::PostComment {
            change: change_b.clone(),
            expected_head: rev_on(&repo_b(), "head-b"),
            marker: "scope-check".into(),
            body: "b".into(),
        },
    );
    let comment_a = insert_intent(
        &store,
        &work_a,
        &repo(),
        HostAction::PostComment {
            change: change_a.clone(),
            expected_head: rev_on(&repo(), "head-a"),
            marker: "scope-check".into(),
            body: "b".into(),
        },
    );
    fixture.apply(&store, &config);
    assert_eq!(
        intent(&store, &comment_b).status,
        HostActionStatus::Applied,
        "repo B's `!1` keeps its own head"
    );
    assert!(
        matches!(
            intent(&store, &comment_a).status,
            HostActionStatus::Failed { .. }
        ),
        "repo A's `!1` saw its head move"
    );
}

/// An `open_change_request` whose source branch does not exist on the
/// remote is a `failed` receipt on this host — never an invented head,
/// never an applied claim.
#[test]
fn gitlab_fixture_open_without_source_branch_is_a_failed_receipt() {
    let mut fixture = GitlabFixture::new();
    let store = SqliteStore::in_memory(2).unwrap();
    let config = config_for(&fixture);
    let (work_id, _eid) = submit(&store, &config, "u92-open-absent");
    let action_id = insert_intent(
        &store,
        &work_id,
        &repo(),
        HostAction::OpenChangeRequest {
            source_branch: "work/never-pushed".into(),
            target_branch: "main".into(),
            title: "t".into(),
            body: String::new(),
        },
    );
    fixture.apply(&store, &config);
    match intent(&store, &action_id).status {
        HostActionStatus::Failed { ref reason } => {
            assert!(
                reason.contains("source branch"),
                "absent branch fails explicitly: {reason}"
            );
        }
        other => panic!("absent-branch open must not apply: {other:?}"),
    }
    assert!(fixture.applied().is_empty(), "no write landed");
    assert!(
        fixture.remote_head(&repo(), "work/never-pushed").is_none(),
        "no head was invented"
    );
}

/// The head follows the source branch: pushing again to an open merge
/// request's source branch moves the change-request head, the remote
/// reports the fresh head, and a head-bound write recorded against the
/// superseded head aborts — the same contract outcome as a direct head
/// move, produced by the push mechanism.
#[test]
fn push_to_an_open_merge_requests_source_branch_surfaces_the_new_head() {
    let mut fixture = GitlabFixture::new();
    let store = SqliteStore::in_memory(2).unwrap();
    let config = config_for(&fixture);
    let (work_id, _eid) = submit(&store, &config, "u92-head-follow");
    insert_intent(
        &store,
        &work_id,
        &repo(),
        HostAction::PushRef {
            branch: "work/tracked".into(),
            produced: rev("head-1"),
        },
    );
    let open_id = insert_intent(
        &store,
        &work_id,
        &repo(),
        HostAction::OpenChangeRequest {
            source_branch: "work/tracked".into(),
            target_branch: "main".into(),
            title: "t".into(),
            body: String::new(),
        },
    );
    fixture.apply(&store, &config);
    let change = match &intent(&store, &open_id).effects[..] {
        [NormalizedEffectFact::ChangeOpened { change, .. }] => change.clone(),
        other => panic!("open receipt missing ChangeOpened: {other:?}"),
    };

    // A second push to the tracked branch moves the merge request's
    // head — the remote reports it as a fresh change_head observation.
    insert_intent(
        &store,
        &work_id,
        &repo(),
        HostAction::PushRef {
            branch: "work/tracked".into(),
            produced: rev("head-2"),
        },
    );
    fixture.apply(&store, &config);
    let heads: Vec<CommitRevision> = store
        .transact(|tx| {
            Ok(tx
                .all_observations()?
                .into_iter()
                .filter_map(|o| match o.fact {
                    ObservationFact::ChangeHead { change: c, head } if c == change => Some(head),
                    _ => None,
                })
                .collect())
        })
        .unwrap();
    assert!(
        heads.iter().any(|h| h == &rev("head-2")),
        "the moved head is reported: {heads:?}"
    );

    // A write recorded against the superseded head now aborts.
    let update_id = insert_intent(
        &store,
        &work_id,
        &repo(),
        HostAction::UpdateChangeRequest {
            change: change.clone(),
            expected_head: rev("head-1"),
            title: Some("edited".into()),
            body: None,
        },
    );
    fixture.apply(&store, &config);
    assert!(
        matches!(
            intent(&store, &update_id).status,
            HostActionStatus::Failed { .. }
        ),
        "the stale-head write aborts under the branch-following model"
    );
}

/// An unsupported operation is reported as a failed receipt naming the
/// operation — explicit, never counted as applied, never silently
/// dropped.
#[test]
fn sparse_host_unsupported_operations_stay_explicit() {
    let mut fixture = SparseHost::new();
    let store = SqliteStore::in_memory(2).unwrap();
    let config = config_for(&fixture);
    let (work_id, _eid) = submit(&store, &config, "hc-unsup-1");
    for (action, name) in [
        (
            HostAction::UpdateChangeRequest {
                change: ChangeRequestRef {
                    repository: repo(),
                    change_request: ChangeRequestId::trusted("cr-1"),
                    display_number: Some(1),
                },
                expected_head: rev("h"),
                title: None,
                body: None,
            },
            "update_change_request",
        ),
        (
            HostAction::PostComment {
                change: ChangeRequestRef {
                    repository: repo(),
                    change_request: ChangeRequestId::trusted("cr-1"),
                    display_number: Some(1),
                },
                expected_head: rev("h"),
                marker: "m".into(),
                body: "b".into(),
            },
            "post_comment",
        ),
    ] {
        let action_id = insert_intent(&store, &work_id, &repo(), action);
        fixture.apply(&store, &config);
        match intent(&store, &action_id).status {
            HostActionStatus::Failed { ref reason } => {
                assert!(
                    reason.contains("unsupported") && reason.contains(name),
                    "unsupported stays explicit: {reason}"
                );
            }
            other => panic!("unsupported operation must not be applied: {other:?}"),
        }
    }
    assert!(fixture.applied().is_empty(), "no unsupported write landed");
}
