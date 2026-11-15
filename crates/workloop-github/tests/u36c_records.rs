//! U3.6c reconciliation suite: the shared reconciliation and drift
//! scenarios — lost responses leaving `unknown` effects reconciled by
//! action identity without blind replay, permission drift revoking a
//! write grant mid-Work (`denied` on the authorization axis, never
//! `unsupported` on the support axis), actor/credential replacement
//! that cannot retarget outstanding actions, restart plus
//! post-configuration-change reconciliation under the original
//! binding, and partial receipts across restart — run unchanged
//! against two independently implemented adapters and retain one U3.6
//! evidence record per scenario per adapter.
//!
//! `FixtureGithub` is the GitHub adapter (the GitHub-shaped fixture the
//! U3 contract targets); `SparseHost` is the independent fixture
//! implementation — a scoped event journal with its own remote model.
//! This slice brings the sparse remote to the same reconciliation
//! contract: read-only `reconcile_outbox`, reconcile-before-write
//! adoption, the credential gate, per-actor `denied` grants, and
//! host-instance binding — so every scenario runs identically on both.
//!
//! The retained records are the `docs/repository-host-contract.md`
//! U3.6 evidence format: every record passes the field-presence check
//! (`evidence::validate_record_json`), stamps `fixture_contract`, and —
//! when the checkout yields a faithful revision stamp — is persisted
//! under `target/` so the retained evidence can be re-derived from the
//! run itself. A dirty or non-git checkout runs the identical
//! assertions, builds no records, and retains none — a retained record
//! must cite a commit that describes the exercised tree.

use std::collections::BTreeSet;
use std::path::PathBuf;

use workloop_core::command::*;
use workloop_core::domain::*;
use workloop_core::identity::*;
use workloop_core::observation::*;
use workloop_core::store::*;
use workloop_core::work::{
    HostTemplate, IngestReceipt, WorkloopConfig, admit_queued, handle_command,
};
use workloop_github::evidence::{
    ClaimClass, ConfigIdentity, EvidenceRecord, ReadBackReceipt, SCHEMA_VERSION, ScenarioResult,
    record_revisions, validate_record_json,
};
use workloop_github::fixture::{
    CredentialBlock, CredentialBook, FixtureGithub, ReconcileReport, RemoteRead,
};
use workloop_github::sparse_fixture::SparseHost;
use workloop_store::sqlite::SqliteStore;

const NOW: i64 = 1_700_000_000;
/// Process-unique suffix for the file-backed restart scenarios — the
/// record suite and the named tests run the same scenario in
/// parallel, so the temp dir must not collide.
static TEMP_SEQ: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

fn temp_store_dir(tag: &str) -> PathBuf {
    let seq = TEMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "workloop-u36c-{tag}-{}-{NOW}-{seq}",
        std::process::id()
    ))
}
/// The credential slot the actor-replacement scenario seats and
/// rotates — an `env:` reference, never a secret value in the record.
const CRED_PUBLICATION: &str = "env:TAISAN_U36C_CRED";
const CRED_PUBLICATION_NEXT: &str = "env:TAISAN_U36C_CRED_NEXT";

fn host() -> HostInstanceId {
    HostInstanceId::trusted("test-host")
}

fn repo() -> RepositoryRef {
    RepositoryRef::new(host(), RepositoryId::trusted("local-repo"))
}

fn other_repo() -> RepositoryRef {
    RepositoryRef::new(host(), RepositoryId::trusted("other-repo"))
}

fn rev(object: &str) -> CommitRevision {
    CommitRevision::new(repo(), object).unwrap()
}

fn reader() -> HostActorRef {
    HostActorRef {
        host: host(),
        actor: HostActorId::trusted("read-bot"),
    }
}

fn publisher() -> HostActorRef {
    HostActorRef {
        host: host(),
        actor: HostActorId::trusted("pub-agent"),
    }
}

fn other_agent() -> HostActorRef {
    HostActorRef {
        host: host(),
        actor: HostActorId::trusted("other-agent"),
    }
}

fn change(id: &str) -> ChangeRequestRef {
    ChangeRequestRef {
        repository: repo(),
        change_request: ChangeRequestId::trusted(id),
        display_number: Some(1),
    }
}

/// The declared capability set each adapter reports, mapped into Core's
/// vocabulary — the configuration identity a record binds.
fn declared_set(adapter_id: &str) -> BTreeSet<HostCapability> {
    let names: &[&str] = match adapter_id {
        "fixture-github" => FixtureGithub::declared_capabilities(),
        "sparse-host" => SparseHost::declared_capabilities(),
        _ => &[],
    };
    names
        .iter()
        .filter_map(|name| HostCapability::from_name(name))
        .collect()
}

fn publication_binding(actor: &HostActorRef, credential_ref: &str) -> RoleBinding {
    RoleBinding {
        role: ActorRole::Publication,
        actor: actor.clone(),
        credential_ref: CredentialRef::trusted(credential_ref),
    }
}

fn config_full(
    adapter_id: &str,
    repository: &RepositoryRef,
    actors: Vec<RoleBinding>,
) -> WorkloopConfig {
    WorkloopConfig {
        host: HostTemplate {
            adapter: AdapterId::trusted(adapter_id),
            repository: repository.clone(),
            contract_version: 1,
            actors,
        },
        declared_capabilities: declared_set(adapter_id),
        lanes: LanePolicy::default(),
        required_checks: vec!["ci".into()],
        allowed_effects: AllowedEffects {
            workspace_write: true,
            host_write: true,
            network: false,
        },
        budget: ExecutionBudget::default(),
        policy: WorkPolicy::default(),
    }
}

fn config(adapter_id: &str) -> WorkloopConfig {
    config_full(
        adapter_id,
        &repo(),
        vec![publication_binding(&publisher(), "cred-publication")],
    )
}

/// Whether `capability` is a host write — the axis permission drift
/// revokes while every read capability stays authorized.
fn is_write_capability(capability: &HostCapability) -> bool {
    matches!(
        capability,
        HostCapability::PushRef
            | HostCapability::ChangeRequestCreate
            | HostCapability::ChangeRequestUpdate
            | HostCapability::CommentWrite
            | HostCapability::ChecksWrite
            | HostCapability::InlineComment
            | HostCapability::ReviewFeedback
            | HostCapability::Approve
            | HostCapability::RequestChanges
            | HostCapability::ThreadReply
            | HostCapability::ThreadResolve
            | HostCapability::MarkReady
    )
}

/// The capability report a conforming adapter emits: every declared
/// capability supported; `drifted` revokes authorization on every
/// write capability while reads stay authorized — the two axes stay
/// distinct (`Supported` + `Denied`, never `Unsupported`).
fn capability_entries(adapter_id: &str, drifted: bool) -> Vec<CapabilityEntry> {
    declared_set(adapter_id)
        .into_iter()
        .map(|capability| CapabilityEntry {
            capability,
            support: CapabilityState::Supported,
            authorization: if drifted && is_write_capability(&capability) {
                CapabilityState::Denied
            } else {
                CapabilityState::Authorized
            },
        })
        .collect()
}

/// What a conforming reconciliation adapter accepts to drive. Two
/// implementations: different remote models, the same
/// reconcile-by-read contract — the suite never pattern-matches a
/// fixture's internals.
trait ReconcilePathAdapter {
    /// The adapter implementation id a record binds.
    fn adapter_id(&self) -> &'static str;

    /// Drain pending intents through the outbox.
    fn apply(&mut self, store: &SqliteStore, config: &WorkloopConfig);

    /// Read-only reconciliation of every outstanding intent.
    fn reconcile(&self, store: &SqliteStore, config: &WorkloopConfig) -> ReconcileReport;

    /// Resolve one typed read target against the remote.
    fn read(&self, subject: &ReadSubject, reader: &HostActorRef) -> RemoteRead;

    /// Poll one typed read target and deliver the Observation through
    /// the shared ingest pipeline.
    fn observe(
        &self,
        store: &SqliteStore,
        config: &WorkloopConfig,
        work_id: Option<&WorkId>,
        subject: &ReadSubject,
        reader: &HostActorRef,
        now: i64,
    ) -> IngestReceipt;

    /// The head the remote records for `(repository, branch)`.
    fn remote_head(&self, repository: &RepositoryRef, branch: &str) -> Option<String>;

    /// The remote write history — reconciliation must never extend it.
    fn applied(&self) -> &[String];

    /// Feed a synthetic Observation through the shared ingest
    /// pipeline — how capability reports and artifact facts arrive.
    fn emit(
        &self,
        store: &SqliteStore,
        config: &WorkloopConfig,
        observation: Observation,
        now: i64,
    ) -> IngestReceipt;

    /// Test control: the remote already shows `head` on
    /// `(repository, branch)`, moved by `actor`.
    fn set_branch(
        &mut self,
        repository: &RepositoryRef,
        branch: &str,
        head: &str,
        actor: &HostActorRef,
    );

    /// Test control: the remote already shows `change` opened from
    /// `source_branch` onto `target_branch` at `head`, attributed to
    /// `actor` — a landed create whose receipt was lost.
    fn plant_change(
        &mut self,
        change: &ChangeRequestRef,
        source_branch: &str,
        target_branch: &str,
        head: &str,
        actor: &HostActorRef,
    );

    /// Test control: `actor` lacks the grant for `operation` —
    /// attempts record `Failed{reason: denied}`.
    fn deny(&mut self, operation: &str, actor: &HostActorRef);

    /// Install or replace the credential environment.
    fn install_credentials(&mut self, book: CredentialBook);

    /// Test control: mutate the seated credential environment between
    /// passes — e.g. rotating a slot's value mid-Work.
    fn credentials_mut(&mut self) -> Option<&mut CredentialBook>;

    /// A fresh adapter bound to a different host instance — the
    /// surviving installation after the recorded binding's adapter is
    /// removed. It must never examine or settle the outstanding
    /// intents: evidence surfaces `unavailable`, never a retarget.
    fn adapter_for_other_host(&self) -> Box<dyn ReconcilePathAdapter>;
}

impl ReconcilePathAdapter for FixtureGithub {
    fn adapter_id(&self) -> &'static str {
        "fixture-github"
    }
    fn apply(&mut self, store: &SqliteStore, config: &WorkloopConfig) {
        self.apply_outbox(store, config, NOW).unwrap();
    }
    fn reconcile(&self, store: &SqliteStore, config: &WorkloopConfig) -> ReconcileReport {
        self.reconcile_outbox(store, config, NOW).unwrap()
    }
    fn read(&self, subject: &ReadSubject, reader: &HostActorRef) -> RemoteRead {
        FixtureGithub::read(self, subject, reader)
    }
    fn observe(
        &self,
        store: &SqliteStore,
        config: &WorkloopConfig,
        work_id: Option<&WorkId>,
        subject: &ReadSubject,
        reader: &HostActorRef,
        now: i64,
    ) -> IngestReceipt {
        FixtureGithub::observe(self, store, config, work_id, subject, reader, now).unwrap()
    }
    fn remote_head(&self, repository: &RepositoryRef, branch: &str) -> Option<String> {
        FixtureGithub::remote_head(self, repository, branch)
    }
    fn applied(&self) -> &[String] {
        FixtureGithub::applied(self)
    }
    fn emit(
        &self,
        store: &SqliteStore,
        config: &WorkloopConfig,
        observation: Observation,
        now: i64,
    ) -> IngestReceipt {
        FixtureGithub::emit(self, store, config, observation, now).unwrap()
    }
    fn set_branch(
        &mut self,
        repository: &RepositoryRef,
        branch: &str,
        head: &str,
        actor: &HostActorRef,
    ) {
        FixtureGithub::set_branch(self, repository, branch, head, actor);
    }
    fn plant_change(
        &mut self,
        change: &ChangeRequestRef,
        source_branch: &str,
        target_branch: &str,
        head: &str,
        actor: &HostActorRef,
    ) {
        FixtureGithub::register_change(self, change, source_branch, target_branch, head, actor);
    }
    fn deny(&mut self, operation: &str, actor: &HostActorRef) {
        FixtureGithub::deny(self, operation, actor);
    }
    fn install_credentials(&mut self, book: CredentialBook) {
        FixtureGithub::install_credentials(self, book);
    }
    fn credentials_mut(&mut self) -> Option<&mut CredentialBook> {
        FixtureGithub::credentials_mut(self)
    }
    fn adapter_for_other_host(&self) -> Box<dyn ReconcilePathAdapter> {
        Box::new(FixtureGithub::for_host(HostInstanceId::trusted(
            "other-host",
        )))
    }
}

impl ReconcilePathAdapter for SparseHost {
    fn adapter_id(&self) -> &'static str {
        "sparse-host"
    }
    fn apply(&mut self, store: &SqliteStore, config: &WorkloopConfig) {
        self.apply_outbox(store, config, NOW).unwrap();
    }
    fn reconcile(&self, store: &SqliteStore, config: &WorkloopConfig) -> ReconcileReport {
        self.reconcile_outbox(store, config, NOW).unwrap()
    }
    fn read(&self, subject: &ReadSubject, reader: &HostActorRef) -> RemoteRead {
        SparseHost::read(self, subject, reader)
    }
    fn observe(
        &self,
        store: &SqliteStore,
        config: &WorkloopConfig,
        work_id: Option<&WorkId>,
        subject: &ReadSubject,
        reader: &HostActorRef,
        now: i64,
    ) -> IngestReceipt {
        SparseHost::observe(self, store, config, work_id, subject, reader, now).unwrap()
    }
    fn remote_head(&self, repository: &RepositoryRef, branch: &str) -> Option<String> {
        SparseHost::remote_head(self, repository, branch)
    }
    fn applied(&self) -> &[String] {
        SparseHost::applied(self)
    }
    fn emit(
        &self,
        store: &SqliteStore,
        config: &WorkloopConfig,
        observation: Observation,
        now: i64,
    ) -> IngestReceipt {
        SparseHost::emit(self, store, config, observation, now).unwrap()
    }
    fn set_branch(
        &mut self,
        repository: &RepositoryRef,
        branch: &str,
        head: &str,
        actor: &HostActorRef,
    ) {
        SparseHost::set_branch(self, repository, branch, head, actor);
    }
    fn plant_change(
        &mut self,
        change: &ChangeRequestRef,
        source_branch: &str,
        target_branch: &str,
        head: &str,
        actor: &HostActorRef,
    ) {
        SparseHost::plant_change(self, change, source_branch, target_branch, head, actor);
    }
    fn deny(&mut self, operation: &str, actor: &HostActorRef) {
        SparseHost::deny(self, operation, actor);
    }
    fn install_credentials(&mut self, book: CredentialBook) {
        SparseHost::install_credentials(self, book);
    }
    fn credentials_mut(&mut self) -> Option<&mut CredentialBook> {
        SparseHost::credentials_mut(self)
    }
    fn adapter_for_other_host(&self) -> Box<dyn ReconcilePathAdapter> {
        Box::new(SparseHost::for_host(HostInstanceId::trusted("other-host")))
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
            origin: Origin("cli".into()),
            idempotency_key: key.into(),
            body: CommandBody::Submit {
                intent: "publish the change".into(),
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
        panic!("submit: {d:?}")
    };
    (work_id.unwrap(), execution_id)
}

fn admit(store: &SqliteStore, config: &WorkloopConfig, eid: &ExecutionId) {
    store
        .transact(|tx| admit_queued(tx, config, eid, NOW).map(|_| ()))
        .unwrap();
}

/// Persist one durable host-action intent exactly as the outbox would
/// record it — pending, unexecuted, bound to the publication actor.
fn insert_intent(store: &SqliteStore, work_id: &WorkId, action: HostAction) -> String {
    insert_intent_with(store, work_id, action, HostActionStatus::Pending)
}

fn insert_intent_with(
    store: &SqliteStore,
    work_id: &WorkId,
    action: HostAction,
    status: HostActionStatus,
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
                    repository: repo(),
                    role: ActorRole::Publication,
                    actor: publisher(),
                    action,
                    payload_digest: "u36c".into(),
                    status,
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
                .find(|i| i.action_id == action_id))
        })
        .unwrap()
        .unwrap()
}

fn intents(store: &SqliteStore) -> Vec<HostActionIntent> {
    store.transact(|tx| tx.action_intents()).unwrap()
}

fn read_execution(store: &SqliteStore, id: &ExecutionId) -> Execution {
    store.transact(|tx| tx.execution(id)).unwrap().unwrap()
}

/// Apply raw effects to set up states commands can't reach yet — the
/// same fixture the contract suite uses.
fn apply_effects(store: &SqliteStore, effects: &[Effect]) {
    store
        .transact(|tx| {
            for e in effects {
                tx.apply(e)?;
            }
            Ok(())
        })
        .expect("setup effects apply");
}

/// Move an Execution to a target state through the store's guarded
/// update.
fn force_state(store: &SqliteStore, id: &ExecutionId, to: ExecutionState) {
    let from = read_execution(store, id).state;
    let mut e = read_execution(store, id);
    e.state = to.clone();
    if to.is_terminal() {
        e.terminal_code = Some("setup".into());
        e.outcome = Some(Outcome {
            state: to.clone(),
            code: Some("setup".into()),
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

/// The finished coding invocation the publish chain waits on.
fn recorded_invocation(eid: &ExecutionId, id: &str) -> Invocation {
    Invocation {
        id: InvocationId(id.into()),
        request_id: format!("{}:inv:0", eid.0),
        execution_id: eid.clone(),
        role: InvocationRole::Coding,
        state: InvocationState::Prepared,
        process_identity: None,
        required_artifacts: vec![],
        evidence_refs: vec![],
        descendants: vec![],
    }
}

/// Deliver one Observation through the adapter's emit path.
fn emit_fact(
    adapter: &dyn ReconcilePathAdapter,
    store: &SqliteStore,
    config: &WorkloopConfig,
    work_id: &WorkId,
    eid: &ExecutionId,
    fact: ObservationFact,
) {
    adapter.emit(
        store,
        config,
        Observation {
            delivery: None,
            subject_work_id: Some(work_id.clone()),
            subject_execution_id: Some(eid.clone()),
            fact,
            observed_unix_seconds: NOW,
        },
        NOW,
    );
}

/// Arm the publish chain exactly as the runtime leaves it: a Waiting
/// execution, a finished coding invocation carrying one required
/// artifact, an authorized capability report, and the artifact's
/// `Present` fact — Core's `maybe_publish` records the stage intents
/// itself.
fn arm_publish_chain(
    adapter: &dyn ReconcilePathAdapter,
    store: &SqliteStore,
    config: &WorkloopConfig,
    work_id: &WorkId,
    eid: &ExecutionId,
    artifact_digest: &str,
) {
    force_state(store, eid, ExecutionState::Waiting);
    let mut invocation = recorded_invocation(eid, "u36c-coding");
    invocation.state = InvocationState::Done;
    invocation.required_artifacts = vec![RequiredArtifact {
        kind: "artifact.json".into(),
        path: "artifact.json".into(),
        expected_digest: Some(artifact_digest.into()),
    }];
    apply_effects(
        store,
        &[Effect::InsertInvocation {
            invocation: invocation.clone(),
        }],
    );
    emit_fact(
        adapter,
        store,
        config,
        work_id,
        eid,
        ObservationFact::CapabilityReport {
            host: host(),
            capabilities: capability_entries(adapter.adapter_id(), false),
        },
    );
    emit_fact(
        adapter,
        store,
        config,
        work_id,
        eid,
        ObservationFact::Artifact {
            invocation_id: invocation.id.clone(),
            required_artifact: "artifact.json".into(),
            state: ArtifactFact::Present {
                digest: artifact_digest.into(),
            },
        },
    );
}

/// The latest capability report recorded for `host`, flattened to its
/// entries — the evidence a drift scenario cites.
fn latest_capability_entries(store: &SqliteStore) -> Vec<CapabilityEntry> {
    store
        .transact(|tx| tx.all_observations())
        .unwrap()
        .iter()
        .filter_map(|o| match &o.fact {
            ObservationFact::CapabilityReport {
                host: h,
                capabilities,
            } if *h == host() => Some(capabilities.clone()),
            _ => None,
        })
        .next_back()
        .unwrap_or_default()
}

/// Push a branch head through the guarded write path.
fn push_intent(branch: &str, produced: &str, expected_prior: Option<&str>) -> HostAction {
    HostAction::PushRef {
        branch: branch.into(),
        produced: rev(produced),
        expected_prior: expected_prior.map(rev),
    }
}

/// Open a change request from `branch` to `main`.
fn open_intent(branch: &str, title: &str) -> HostAction {
    HostAction::OpenChangeRequest {
        source_branch: branch.into(),
        target_branch: "main".into(),
        title: title.into(),
        body: "b".into(),
    }
}

fn unknown() -> HostActionStatus {
    HostActionStatus::Unknown {
        reason: "response lost".into(),
    }
}

/// Every `HostRead` fact recorded for one subject, in delivery order —
/// `(delivery identity as recorded, revision, subject_version)`.
fn recorded_reads(
    store: &SqliteStore,
    subject: &ReadSubject,
) -> Vec<(Option<DeliveryId>, Option<ReadRevision>, u64)> {
    store
        .transact(|tx| tx.all_observations())
        .unwrap()
        .iter()
        .filter_map(|o| match &o.fact {
            ObservationFact::HostRead {
                subject: s,
                revision,
                subject_version,
                ..
            } if s == subject => Some((o.delivery.clone(), revision.clone(), *subject_version)),
            _ => None,
        })
        .collect()
}

/// The identity a retained record cites for one recorded delivery:
/// `(scope, source, event_id)` read back from the store — never
/// reconstructed from a declared source namespace, so the record cites
/// only identities the adapter actually stamped.
fn delivery_identity(delivery: &DeliveryId) -> String {
    match &delivery.scope {
        DeliveryScope::Local => format!("local:{}:{}", delivery.source, delivery.event_id),
        DeliveryScope::Host { host } => {
            format!("host:{host}:{}:{}", delivery.source, delivery.event_id)
        }
    }
}

/// The delivery identities recorded for the given subjects, in order.
fn delivery_identities(store: &SqliteStore, subjects: &[ReadSubject]) -> Vec<String> {
    subjects
        .iter()
        .flat_map(|subject| {
            recorded_reads(store, subject)
                .iter()
                .filter_map(|(delivery, ..)| delivery.as_ref().map(delivery_identity))
                .collect::<Vec<_>>()
        })
        .collect()
}

/// The single normalized effect fact an applied receipt carries.
fn only_effect(intent: &HostActionIntent) -> &NormalizedEffectFact {
    match intent.effects.as_slice() {
        [fact] => fact,
        other => panic!("expected exactly one effect fact, got {other:?}"),
    }
}

/// The evidence a scenario run leaves behind: the durable action
/// identities it exercised, the observation delivery identities it
/// recorded, and the read-back receipts its facts were re-observed
/// through.
struct ScenarioEvidence {
    action_identities: Vec<String>,
    observation_identities: Vec<String>,
    read_back_receipts: Vec<ReadBackReceipt>,
}

/// U3.6c reconciliation scenario 1 — a lost response leaves an
/// `unknown` effect; reconciliation settles it by action identity,
/// payload, and the original actor/binding — a read-only comparison
/// against the remote, never a replayed write. A definitively absent
/// effect settles `failed`; the write history never moves.
fn lost_response_unknown_effects_reconcile_by_action_identity_without_replay(
    adapter: &mut dyn ReconcilePathAdapter,
) -> ScenarioEvidence {
    let store = SqliteStore::in_memory(1).unwrap();
    let config = config(adapter.adapter_id());
    let (work_id, eid) = submit(&store, &config, "u36c-lost");
    admit(&store, &config, &eid);

    // The remote already shows the effects whose receipts were lost:
    // a branch head and a change request attributed to the bound
    // actor — and nothing at all for the absent case.
    adapter.set_branch(&repo(), "work/lost", "head-9", &publisher());
    adapter.plant_change(
        &change("cr-lost"),
        "work/lost",
        "main",
        "head-9",
        &publisher(),
    );

    let push_id = insert_intent_with(
        &store,
        &work_id,
        push_intent("work/lost", "head-9", None),
        unknown(),
    );
    let open_id = insert_intent_with(
        &store,
        &work_id,
        open_intent("work/lost", "lost"),
        unknown(),
    );
    let absent_id = insert_intent_with(
        &store,
        &work_id,
        push_intent("work/never", "head-x", None),
        unknown(),
    );

    // Nothing has gone through the write path yet — every remote fact
    // above was planted, modelling effects that landed before their
    // receipts did.
    let writes_before = adapter.applied().len();
    let report = adapter.reconcile(&store, &config);
    assert_eq!(
        report.examined,
        3,
        "{}: every outstanding intent read",
        adapter.adapter_id()
    );
    assert_eq!(
        report.applied,
        2,
        "{}: the two landed effects settle",
        adapter.adapter_id()
    );
    assert_eq!(
        report.failed,
        1,
        "{}: definitive absence settles failed",
        adapter.adapter_id()
    );
    assert_eq!(report.still_unknown, 0);
    assert!(report.credential_blocked.is_empty());

    // Each intent settles under its own recorded identity: the push's
    // receipt cites the remote's attribution and the observed head;
    // the open's receipt recovers the remote-issued change identity
    // the lost receipt never captured.
    let push = intent(&store, &push_id);
    assert_eq!(push.status, HostActionStatus::Applied);
    assert_eq!(push.actor, publisher(), "original binding kept");
    assert_eq!(push.repository, repo());
    assert_eq!(push.authenticated_actor, Some(publisher()));
    let [
        NormalizedEffectFact::RefPublished {
            produced,
            observed,
            relation,
            ..
        },
    ] = push.effects.as_slice()
    else {
        panic!("push receipt missing RefPublished: {:?}", push.effects)
    };
    assert_eq!(*produced, rev("head-9"));
    assert_eq!(*observed, rev("head-9"), "the read's observed head");
    assert_eq!(*relation, RefRelation::Equal);

    let open = intent(&store, &open_id);
    assert_eq!(open.status, HostActionStatus::Applied);
    assert_eq!(open.actor, publisher());
    assert_eq!(open.authenticated_actor, Some(publisher()));
    let [
        NormalizedEffectFact::ChangeOpened {
            change: opened,
            head,
            source_branch,
            target_branch,
        },
    ] = open.effects.as_slice()
    else {
        panic!("open receipt missing ChangeOpened: {:?}", open.effects)
    };
    assert_eq!(
        *opened,
        change("cr-lost"),
        "the remote-issued identity recovered"
    );
    assert_eq!(*head, rev("head-9"));
    assert_eq!(source_branch, "work/lost");
    assert_eq!(target_branch, "main");

    // Definitive absence settles the unknown intent `failed` — a
    // definite no-effect outcome, never an applied claim.
    match &intent(&store, &absent_id).status {
        HostActionStatus::Failed { reason } => {
            assert!(
                reason.contains("not observed"),
                "the verdict says why: {reason}"
            )
        }
        other => panic!("definitive absence settles failed: {other:?}"),
    }

    // No blind replay: reconciliation performed host reads only — the
    // write history is untouched and the remote state is identical.
    assert_eq!(
        adapter.applied().len(),
        writes_before,
        "{}: reconciliation wrote nothing",
        adapter.adapter_id()
    );
    assert_eq!(
        adapter.remote_head(&repo(), "work/lost"),
        Some("head-9".to_string())
    );

    // Read-back of the settled effects: fresh typed reads agree with
    // the receipts' observed post-state.
    let head_subject = ReadSubject::Head {
        repository: repo(),
        branch: "work/lost".into(),
    };
    let pr_subject = ReadSubject::Pr {
        change: change("cr-lost"),
    };
    for (subject, expected) in [(&head_subject, rev("head-9")), (&pr_subject, rev("head-9"))] {
        let read = adapter.read(subject, &reader());
        assert_eq!(
            read.revision,
            Some(ReadRevision::Sha {
                revision: expected.clone()
            }),
            "{}: fresh read of {} agrees with the settled receipts",
            adapter.adapter_id(),
            subject.key()
        );
        assert!(read.completeness.is_complete());
        let receipt = adapter.observe(&store, &config, Some(&work_id), subject, &reader(), NOW);
        assert_eq!(receipt, IngestReceipt::Recorded);
    }

    ScenarioEvidence {
        action_identities: vec![push_id.clone(), open_id.clone(), absent_id.clone()],
        observation_identities: delivery_identities(
            &store,
            &[head_subject.clone(), pr_subject.clone()],
        ),
        read_back_receipts: vec![
            ReadBackReceipt {
                target: format!("act:{push_id}"),
                observed: "receipt:Applied RefPublished produced=observed=head-9 \
                           relation=Equal — settled from the remote's own read"
                    .into(),
            },
            ReadBackReceipt {
                target: format!("act:{open_id}"),
                observed: "receipt:Applied ChangeOpened cr-lost head=head-9 — \
                           the remote-issued identity recovered by (repository, \
                           source, target) match"
                    .into(),
            },
            ReadBackReceipt {
                target: format!("act:{absent_id}"),
                observed: "receipt:Failed{reconcile: effect not observed} — \
                           definitive absence is a definite no-effect outcome"
                    .into(),
            },
            ReadBackReceipt {
                target: head_subject.key(),
                observed: "read-back:Head read head-9 — agrees with RefPublished.observed".into(),
            },
            ReadBackReceipt {
                target: pr_subject.key(),
                observed: "read-back:Pr read head-9 — agrees with ChangeOpened.head".into(),
            },
        ],
    }
}

/// U3.6c drift scenario — an operation's permission is revoked
/// mid-Work: new dependent effects record `denied` on the
/// authorization axis (distinct from `unsupported` on the support
/// axis), previously applied receipts stay intact, the unknown-effect
/// intent keeps its original binding and settles read-only, the Work
/// pauses for an operator decision, and no new dependent effect is
/// admitted while the drift stands.
fn permission_drift_denies_dependent_effects_and_keeps_read_only_reconciliation(
    adapter: &mut dyn ReconcilePathAdapter,
) -> ScenarioEvidence {
    let store = SqliteStore::in_memory(1).unwrap();
    let config = config(adapter.adapter_id());
    let (work_id, eid) = submit(&store, &config, "u36c-drift");
    admit(&store, &config, &eid);

    // Arm and land the publish chain through the real path: Core's
    // maybe_publish records the stage intents, the outbox applies
    // them, and both receipts are `applied` before the drift.
    arm_publish_chain(adapter, &store, &config, &work_id, &eid, "artifact-digest");
    adapter.apply(&store, &config);
    let chain: Vec<HostActionIntent> = intents(&store)
        .into_iter()
        .filter(|i| i.work_id == work_id)
        .collect();
    assert_eq!(chain.len(), 2, "push + open intents recorded");
    assert!(
        chain.iter().all(|i| i.status == HostActionStatus::Applied),
        "both stage effects applied before the drift: {chain:?}"
    );
    let push_id = chain
        .iter()
        .find(|i| matches!(i.action, HostAction::PushRef { .. }))
        .unwrap()
        .action_id
        .clone();
    let open_id = chain
        .iter()
        .find(|i| matches!(i.action, HostAction::OpenChangeRequest { .. }))
        .unwrap()
        .action_id
        .clone();
    let branch = format!("work/{}", work_id.0);

    // An unknown-effect intent bound to the same Work and actor: its
    // remote effect landed but the receipt was lost.
    let retry_branch = format!("{branch}-retry");
    let unknown_id = insert_intent_with(
        &store,
        &work_id,
        push_intent(&retry_branch, "head-z", None),
        unknown(),
    );
    adapter.set_branch(&repo(), &retry_branch, "head-z", &publisher());

    // The drift: every declared write capability loses authorization
    // for the bound actor — support stays declared on the support
    // axis, the grant is revoked on the authorization axis. The
    // remote enforces the same verdict at the operation level.
    adapter.deny("push_ref", &publisher());
    adapter.deny("open_change_request", &publisher());
    emit_fact(
        adapter,
        &store,
        &config,
        &work_id,
        &eid,
        ObservationFact::CapabilityReport {
            host: host(),
            capabilities: capability_entries(adapter.adapter_id(), true),
        },
    );

    // The drifted report is the recorded evidence: write capabilities
    // report `denied` on the authorization axis while still
    // `supported` — never `unsupported` — and every read capability
    // stays authorized.
    let entries = latest_capability_entries(&store);
    assert!(!entries.is_empty(), "the drifted report was recorded");
    for entry in &entries {
        if is_write_capability(&entry.capability) {
            assert_eq!(
                entry.support,
                CapabilityState::Supported,
                "{}: {} stays supported on the support axis",
                adapter.adapter_id(),
                entry.capability.name()
            );
            assert_eq!(
                entry.authorization,
                CapabilityState::Denied,
                "{}: {} reports denied on the authorization axis",
                adapter.adapter_id(),
                entry.capability.name()
            );
        } else {
            assert_eq!(
                entry.authorization,
                CapabilityState::Authorized,
                "{}: read capability {} stays authorized",
                adapter.adapter_id(),
                entry.capability.name()
            );
        }
    }

    // A new dependent effect attempted under the drift records
    // `denied` — an authorization verdict, terminal for the intent,
    // never retried and never an `unsupported` receipt.
    let dep_branch = format!("{branch}-dep");
    let dep_id = insert_intent(&store, &work_id, push_intent(&dep_branch, "head-y", None));
    adapter.apply(&store, &config);
    let dep = intent(&store, &dep_id);
    assert_eq!(
        dep.status,
        HostActionStatus::Failed {
            reason: "denied".into()
        },
        "{}: the dependent effect records denied",
        adapter.adapter_id()
    );
    assert!(
        !matches!(dep.status, HostActionStatus::Unsupported { .. }),
        "denied is an authorization verdict, never unsupported"
    );
    assert!(dep.effects.is_empty(), "a denied write has no effect fact");
    assert_eq!(dep.authenticated_actor, Some(publisher()));
    assert_eq!(
        adapter.remote_head(&repo(), &dep_branch),
        None,
        "{}: the denied write never reached the remote",
        adapter.adapter_id()
    );

    // The previously applied receipts stay intact — facts, not
    // re-verified state.
    for id in [&push_id, &open_id] {
        let settled = intent(&store, id);
        assert_eq!(
            settled.status,
            HostActionStatus::Applied,
            "{id} receipt intact"
        );
        assert_eq!(settled.effects.len(), 1, "{id} keeps its effect fact");
    }

    // Reconciliation continues with reads only: the unknown intent
    // settles from the remote's own read under its original binding —
    // the denied grant governs writes, never the read path.
    let writes_before = adapter.applied().len();
    let report = adapter.reconcile(&store, &config);
    assert_eq!(report.examined, 1, "only the unknown intent is outstanding");
    assert_eq!(report.applied, 1);
    assert!(report.credential_blocked.is_empty());
    let settled = intent(&store, &unknown_id);
    assert_eq!(settled.status, HostActionStatus::Applied);
    assert_eq!(settled.actor, publisher(), "original binding kept");
    assert_eq!(settled.repository, repo());
    assert_eq!(
        settled.authenticated_actor,
        Some(publisher()),
        "the remote's attribution authenticates the receipt"
    );
    assert!(matches!(
        only_effect(&settled),
        NormalizedEffectFact::RefPublished { .. }
    ));
    assert_eq!(
        adapter.applied().len(),
        writes_before,
        "{}: reconciliation under drift wrote nothing",
        adapter.adapter_id()
    );

    // The denied verdict paused the Work for an operator decision —
    // terminal for the intent, never a transport retry. `stopping`
    // with a recorded pause desire settles to `paused` once writer
    // safety is proven.
    let execution = read_execution(&store, &eid);
    assert_eq!(execution.state, ExecutionState::Paused);
    assert!(
        execution.pause_reason.is_some(),
        "the pause names its reason"
    );
    assert!(
        execution
            .repair_prerequisites
            .contains(&RepairPrerequisite::OperatorDecision),
        "the pause waits on an operator decision"
    );

    // No new dependent effect is admitted while the drift stands: a
    // fresh artifact would re-arm publication, but the drifted
    // capability report holds every write capability denied — Core's
    // intent gate records nothing.
    let recorded = intents(&store).len();
    emit_fact(
        adapter,
        &store,
        &config,
        &work_id,
        &eid,
        ObservationFact::Artifact {
            invocation_id: InvocationId("u36c-coding".into()),
            required_artifact: "artifact.json".into(),
            state: ArtifactFact::Present {
                digest: "artifact-digest-2".into(),
            },
        },
    );
    assert_eq!(
        intents(&store).len(),
        recorded,
        "{}: no new dependent intent admitted under drift",
        adapter.adapter_id()
    );

    // Reads stay served — the reconciliation read set remained
    // authorized through the drift.
    let retry_subject = ReadSubject::Head {
        repository: repo(),
        branch: retry_branch.clone(),
    };
    let change_ref = match only_effect(&intent(&store, &open_id)) {
        NormalizedEffectFact::ChangeOpened { change, .. } => change.clone(),
        other => panic!("open receipt missing ChangeOpened: {other:?}"),
    };
    let pr_subject = ReadSubject::Pr { change: change_ref };
    for subject in [&retry_subject, &pr_subject] {
        let read = adapter.read(subject, &reader());
        assert!(
            read.completeness.is_complete(),
            "{}: read {} still served under drift",
            adapter.adapter_id(),
            subject.key()
        );
        let receipt = adapter.observe(&store, &config, Some(&work_id), subject, &reader(), NOW);
        assert_eq!(receipt, IngestReceipt::Recorded);
    }

    ScenarioEvidence {
        action_identities: vec![
            push_id.clone(),
            open_id.clone(),
            unknown_id.clone(),
            dep_id.clone(),
        ],
        observation_identities: delivery_identities(
            &store,
            &[retry_subject.clone(), pr_subject.clone()],
        ),
        read_back_receipts: vec![
            ReadBackReceipt {
                target: format!("act:{push_id}"),
                observed: "receipt:Applied RefPublished — intact through the drift".into(),
            },
            ReadBackReceipt {
                target: format!("act:{open_id}"),
                observed: "receipt:Applied ChangeOpened — intact through the drift".into(),
            },
            ReadBackReceipt {
                target: format!("act:{unknown_id}"),
                observed: "reconcile:Applied RefPublished head-z — settled read-only \
                           under the original binding while writes were denied"
                    .into(),
            },
            ReadBackReceipt {
                target: format!("act:{dep_id}"),
                observed: "receipt:Failed{denied} — authorization verdict on the \
                           grant axis; support stayed declared, the write never \
                           reached the remote"
                    .into(),
            },
            ReadBackReceipt {
                target: "capability-report".into(),
                observed: "drifted CapabilityReport: every declared write capability \
                           Supported+Denied, every read capability Authorized"
                    .into(),
            },
            ReadBackReceipt {
                target: retry_subject.key(),
                observed: "read-back:Head read head-z — reads stayed authorized".into(),
            },
            ReadBackReceipt {
                target: pr_subject.key(),
                observed: "read-back:Pr read — reads stayed authorized".into(),
            },
        ],
    }
}

/// U3.6c actor scenario — credential replacement cannot retarget
/// outstanding actions: a same-actor renewal is a no-op for Work,
/// intents, and unresolved effects; a different-actor credential
/// blocks writes and reconciliation without rebinding; an adapter
/// bound to another host examines nothing — the removed binding's
/// evidence is `unavailable`, never a retarget; and a role rebound to
/// a different actor applies only to Work accepted after the change.
fn actor_replacement_cannot_retarget_outstanding_actions(
    adapter: &mut dyn ReconcilePathAdapter,
) -> ScenarioEvidence {
    let store = SqliteStore::in_memory(1).unwrap();
    let config_a = config_full(
        adapter.adapter_id(),
        &repo(),
        vec![publication_binding(&publisher(), CRED_PUBLICATION)],
    );
    let (work_id, eid) = submit(&store, &config_a, "u36c-actor");
    admit(&store, &config_a, &eid);

    let mut book = CredentialBook::default();
    book.seat_env("TAISAN_U36C_CRED", "token-v1");
    book.seat_actor("token-v1", publisher());
    adapter.install_credentials(book);

    // Outstanding work: one applied push and two unknown effects on
    // distinct branches, plus one pending dependent push.
    let push_id = insert_intent(&store, &work_id, push_intent("work/actor", "head-1", None));
    adapter.apply(&store, &config_a);
    assert_eq!(intent(&store, &push_id).status, HostActionStatus::Applied);

    let open_a = insert_intent_with(
        &store,
        &work_id,
        open_intent("work/actor-a", "a"),
        unknown(),
    );
    let open_b = insert_intent_with(
        &store,
        &work_id,
        open_intent("work/actor-b", "b"),
        unknown(),
    );
    adapter.plant_change(
        &change("cr-actor-a"),
        "work/actor-a",
        "main",
        "head-a",
        &publisher(),
    );
    adapter.plant_change(
        &change("cr-actor-b"),
        "work/actor-b",
        "main",
        "head-b",
        &publisher(),
    );
    let dep_id = insert_intent(
        &store,
        &work_id,
        push_intent("work/actor-dep", "head-d", None),
    );

    // Same-actor renewal: the slot now holds a new secret
    // authenticating as the SAME actor — a no-op for the Work, its
    // intents, and its unresolved effects.
    adapter
        .credentials_mut()
        .unwrap()
        .seat_env("TAISAN_U36C_CRED", "token-v2");
    adapter
        .credentials_mut()
        .unwrap()
        .seat_actor("token-v2", publisher());
    let report = adapter.reconcile(&store, &config_a);
    assert_eq!(report.applied, 2, "both unknown effects settle");
    assert_eq!(report.examined, 3, "the pending dependent is read too");
    assert!(report.credential_blocked.is_empty());
    for id in [&open_a, &open_b] {
        let settled = intent(&store, id);
        assert_eq!(settled.status, HostActionStatus::Applied);
        assert_eq!(settled.actor, publisher(), "binding untouched by renewal");
        assert_eq!(settled.authenticated_actor, Some(publisher()));
    }
    assert_eq!(
        intent(&store, &dep_id).status,
        HostActionStatus::Pending,
        "absence is not failure for a possibly-unattempted intent"
    );
    let work = store.transact(|tx| tx.work(&work_id)).unwrap().unwrap();
    assert_eq!(
        work.binding
            .actors
            .iter()
            .find(|b| b.role == ActorRole::Publication)
            .unwrap()
            .actor,
        publisher(),
        "the Work's binding is unchanged by credential renewal"
    );

    // Different-actor replacement: the slot now authenticates as
    // another actor. Neither writes nor reconciliation may act on the
    // outstanding intent — it is never rebound.
    adapter
        .credentials_mut()
        .unwrap()
        .seat_env("TAISAN_U36C_CRED", "token-v3");
    adapter
        .credentials_mut()
        .unwrap()
        .seat_actor("token-v3", other_agent());
    let report = adapter.reconcile(&store, &config_a);
    assert_eq!(
        report.credential_blocked,
        vec![CredentialBlock {
            action_id: dep_id.clone(),
            code: "credential.actor_mismatch",
        }],
        "the rotated credential blocks reconciliation"
    );
    assert_eq!(report.examined, 0, "nothing was examined past the gate");
    adapter.apply(&store, &config_a);
    let dep = intent(&store, &dep_id);
    assert_eq!(
        dep.status,
        HostActionStatus::Pending,
        "a mismatched credential leaves the intent pending — never rebound"
    );
    assert_eq!(dep.actor, publisher(), "the recorded actor is kept");
    assert_eq!(dep.repository, repo(), "the recorded binding is kept");

    // The retained old adapter/configuration is gone: the surviving
    // adapter serves another host instance, so it examines nothing —
    // the outstanding intent's evidence is `unavailable`, and no
    // adapter retargets it.
    let foreign = adapter.adapter_for_other_host();
    let report = foreign.reconcile(&store, &config_a);
    assert_eq!(report.examined, 0, "a foreign binding examines nothing");
    assert!(report.credential_blocked.is_empty());
    assert!(
        foreign.applied().is_empty(),
        "the foreign adapter wrote nothing"
    );
    assert_eq!(
        intent(&store, &dep_id).status,
        HostActionStatus::Pending,
        "the intent stays outstanding — unavailable, never retargeted"
    );

    // A role rebound to a different actor applies only to Work
    // accepted after the change: the new Work binds the new actor
    // while the outstanding intent keeps the original.
    let config_b = config_full(
        adapter.adapter_id(),
        &repo(),
        vec![publication_binding(&other_agent(), CRED_PUBLICATION_NEXT)],
    );
    let (work2, _eid2) = submit(&store, &config_b, "u36c-actor-next");
    let work2_binding = store
        .transact(|tx| tx.work(&work2))
        .unwrap()
        .unwrap()
        .binding;
    assert_eq!(
        work2_binding
            .actors
            .iter()
            .find(|b| b.role == ActorRole::Publication)
            .unwrap()
            .actor,
        other_agent(),
        "the new Work binds the new actor"
    );
    assert_eq!(
        intent(&store, &dep_id).actor,
        publisher(),
        "the outstanding intent keeps its original actor"
    );

    // Read-back of the settled effects.
    let subjects = [
        ReadSubject::Pr {
            change: change("cr-actor-a"),
        },
        ReadSubject::Pr {
            change: change("cr-actor-b"),
        },
    ];
    for subject in &subjects {
        let read = adapter.read(subject, &reader());
        assert!(read.completeness.is_complete());
        let receipt = adapter.observe(&store, &config_a, Some(&work_id), subject, &reader(), NOW);
        assert_eq!(receipt, IngestReceipt::Recorded);
    }

    ScenarioEvidence {
        action_identities: vec![
            push_id.clone(),
            open_a.clone(),
            open_b.clone(),
            dep_id.clone(),
        ],
        observation_identities: delivery_identities(&store, &subjects),
        read_back_receipts: vec![
            ReadBackReceipt {
                target: format!("act:{open_a}"),
                observed: "reconcile:Applied ChangeOpened cr-actor-a — settled \
                           under the same-actor renewal, binding untouched"
                    .into(),
            },
            ReadBackReceipt {
                target: format!("act:{open_b}"),
                observed: "reconcile:Applied ChangeOpened cr-actor-b — settled \
                           under the same-actor renewal, binding untouched"
                    .into(),
            },
            ReadBackReceipt {
                target: format!("act:{dep_id}"),
                observed: "unavailable: credential.actor_mismatch blocks reconcile \
                           and apply; the foreign-bound adapter examines nothing — \
                           the intent keeps publisher/local-repo and stays pending"
                    .into(),
            },
            ReadBackReceipt {
                target: "work-binding".into(),
                observed: "work1 keeps pub-agent; the post-change Work binds \
                           other-agent — binding changes apply only to newly \
                           accepted Work"
                    .into(),
            },
        ],
    }
}

/// U3.6c restart scenario — the store restarts and the installation
/// configuration moves to another repository and actor; outstanding
/// work reconciles under the binding recorded at intent time, never
/// the live configuration.
fn restart_and_config_change_reconcile_under_the_original_binding(
    adapter: &mut dyn ReconcilePathAdapter,
) -> ScenarioEvidence {
    let dir = temp_store_dir("restart");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let db = dir.join("store.db");
    let config_a = config(adapter.adapter_id());
    let config_b = config_full(
        adapter.adapter_id(),
        &other_repo(),
        vec![publication_binding(&other_agent(), "cred-other")],
    );

    // Process 1: the push applies and its receipt is durable; the
    // open's remote effect landed but its receipt was lost.
    let (work_id, push_id, open_id) = {
        let store = SqliteStore::open(&db, 1).unwrap();
        let (work_id, eid) = submit(&store, &config_a, "u36c-restart");
        admit(&store, &config_a, &eid);
        let push_id = insert_intent(&store, &work_id, push_intent("work/re", "head-1", None));
        let open_id = insert_intent_with(
            &store,
            &work_id,
            open_intent("work/re", "restart"),
            unknown(),
        );
        adapter.plant_change(&change("cr-re"), "work/re", "main", "head-1", &publisher());
        adapter.apply(&store, &config_a);
        assert_eq!(intent(&store, &push_id).status, HostActionStatus::Applied);
        assert_eq!(intent(&store, &open_id).status, unknown());
        (work_id, push_id, open_id)
    };

    // Restart: the durable records survive; the remote — a different
    // process — kept the landed effect.
    let store = SqliteStore::open(&db, 1).unwrap();
    assert_eq!(intent(&store, &push_id).status, HostActionStatus::Applied);
    assert_eq!(intent(&store, &open_id).status, unknown());

    // The installation moved to another repository and actor between
    // the intent and the read: reconciliation still compares the
    // persisted intent's recorded fields against the observed remote.
    let report = adapter.reconcile(&store, &config_b);
    assert_eq!(report.applied, 1, "the recorded fields reconcile");
    let open = intent(&store, &open_id);
    assert_eq!(open.status, HostActionStatus::Applied);
    assert_eq!(open.actor, publisher(), "recorded actor untouched");
    assert_eq!(open.repository, repo(), "recorded repository untouched");
    assert_eq!(open.authenticated_actor, Some(publisher()));
    let [
        NormalizedEffectFact::ChangeOpened {
            change: opened,
            head,
            ..
        },
    ] = open.effects.as_slice()
    else {
        panic!("open receipt missing ChangeOpened: {:?}", open.effects)
    };
    assert_eq!(*opened, change("cr-re"));
    assert_eq!(*head, rev("head-1"));

    // The push's applied receipt is untouched — reconciliation never
    // revisits a terminal intent.
    let push = intent(&store, &push_id);
    assert_eq!(push.status, HostActionStatus::Applied);
    assert!(matches!(
        only_effect(&push),
        NormalizedEffectFact::RefPublished { .. }
    ));

    // Read-back under the restarted store.
    let subjects = [
        ReadSubject::Head {
            repository: repo(),
            branch: "work/re".into(),
        },
        ReadSubject::Pr {
            change: change("cr-re"),
        },
    ];
    for subject in &subjects {
        let read = adapter.read(subject, &reader());
        assert!(read.completeness.is_complete());
        let receipt = adapter.observe(&store, &config_a, Some(&work_id), subject, &reader(), NOW);
        assert_eq!(receipt, IngestReceipt::Recorded);
    }
    let _ = std::fs::remove_dir_all(&dir);

    ScenarioEvidence {
        action_identities: vec![push_id.clone(), open_id.clone()],
        observation_identities: delivery_identities(&store, &subjects),
        read_back_receipts: vec![
            ReadBackReceipt {
                target: format!("act:{push_id}"),
                observed: "receipt:Applied RefPublished — durable across the restart, \
                           untouched by reconciliation"
                    .into(),
            },
            ReadBackReceipt {
                target: format!("act:{open_id}"),
                observed: "reconcile:Applied ChangeOpened cr-re head=head-1 — settled \
                           under the recorded binding while the live config named \
                           another repository and actor"
                    .into(),
            },
            ReadBackReceipt {
                target: subjects[0].key(),
                observed: "read-back:Head read head-1 — agrees with RefPublished.observed".into(),
            },
            ReadBackReceipt {
                target: subjects[1].key(),
                observed: "read-back:Pr read head-1 — agrees with ChangeOpened.head".into(),
            },
        ],
    }
}

/// U3.6c partial-receipt scenario — each constituent effect of the
/// publication capability keeps its own durable action identity and
/// receipt; a restart between constituents reconciles each
/// independently against its recorded intent, no constituent is
/// replayed blindly, and no combined-success claim is made from
/// partial facts.
fn partial_receipts_across_restart_reconcile_each_constituent_independently(
    adapter: &mut dyn ReconcilePathAdapter,
) -> ScenarioEvidence {
    let dir = temp_store_dir("partial");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let db = dir.join("store.db");
    let config = config(adapter.adapter_id());

    // Process 1: the push constituent applies and its receipt is
    // durable; the open constituent's remote effect landed but its
    // receipt was lost — partial facts, not a combined claim.
    let (work_id, push_id, open_id) = {
        let store = SqliteStore::open(&db, 1).unwrap();
        let (work_id, eid) = submit(&store, &config, "u36c-partial");
        admit(&store, &config, &eid);
        let push_id = insert_intent(&store, &work_id, push_intent("work/part", "head-1", None));
        let open_id = insert_intent_with(
            &store,
            &work_id,
            open_intent("work/part", "partial"),
            unknown(),
        );
        adapter.plant_change(
            &change("cr-part"),
            "work/part",
            "main",
            "head-1",
            &publisher(),
        );
        adapter.apply(&store, &config);
        assert_eq!(intent(&store, &push_id).status, HostActionStatus::Applied);
        assert_eq!(intent(&store, &open_id).status, unknown());
        // Partial facts are exactly that: the push's RefPublished
        // exists, no ChangeOpened does — publication is not claimed.
        let all_intents = intents(&store);
        let facts = all_intents
            .iter()
            .flat_map(|i| i.effects.iter())
            .collect::<Vec<_>>();
        assert_eq!(
            facts
                .iter()
                .filter(|f| matches!(f, NormalizedEffectFact::RefPublished { .. }))
                .count(),
            1
        );
        assert!(
            !facts
                .iter()
                .any(|f| matches!(f, NormalizedEffectFact::ChangeOpened { .. })),
            "no combined-success claim from partial facts"
        );
        (work_id, push_id, open_id)
    };

    // Restart: both receipts' durable state survives — one applied,
    // one still unknown.
    let store = SqliteStore::open(&db, 1).unwrap();
    assert_eq!(intent(&store, &push_id).status, HostActionStatus::Applied);
    assert_eq!(intent(&store, &open_id).status, unknown());
    let writes_before = adapter.applied().len();

    // Reconciliation examines only the outstanding constituent: the
    // applied push is terminal and is never revisited; the unknown
    // open settles from the remote's own read — each constituent
    // independently, against its recorded intent.
    let report = adapter.reconcile(&store, &config);
    assert_eq!(
        report.examined, 1,
        "only the outstanding constituent is read"
    );
    assert_eq!(report.applied, 1);
    let open = intent(&store, &open_id);
    assert_eq!(open.status, HostActionStatus::Applied);
    assert_eq!(open.actor, publisher());
    assert_eq!(open.authenticated_actor, Some(publisher()));
    let [
        NormalizedEffectFact::ChangeOpened {
            change: opened,
            head,
            ..
        },
    ] = open.effects.as_slice()
    else {
        panic!("open receipt missing ChangeOpened: {:?}", open.effects)
    };
    assert_eq!(*opened, change("cr-part"));
    assert_eq!(*head, rev("head-1"));

    // No constituent was replayed: the write history is exactly what
    // the pre-restart drain recorded.
    assert_eq!(
        adapter.applied().len(),
        writes_before,
        "{}: reconciliation replayed no constituent",
        adapter.adapter_id()
    );

    // After settlement each constituent holds its own receipt bound
    // to its own action identity — two independent facts, never one
    // combined claim.
    let push = intent(&store, &push_id);
    assert_eq!(push.status, HostActionStatus::Applied);
    assert!(matches!(
        only_effect(&push),
        NormalizedEffectFact::RefPublished { .. }
    ));
    assert_ne!(push_id, open_id, "independent action identities");

    // Read-back of each constituent's observed post-state.
    let subjects = [
        ReadSubject::Head {
            repository: repo(),
            branch: "work/part".into(),
        },
        ReadSubject::Pr {
            change: change("cr-part"),
        },
    ];
    for subject in &subjects {
        let read = adapter.read(subject, &reader());
        assert!(read.completeness.is_complete());
        let receipt = adapter.observe(&store, &config, Some(&work_id), subject, &reader(), NOW);
        assert_eq!(receipt, IngestReceipt::Recorded);
    }
    let _ = std::fs::remove_dir_all(&dir);

    ScenarioEvidence {
        action_identities: vec![push_id.clone(), open_id.clone()],
        observation_identities: delivery_identities(&store, &subjects),
        read_back_receipts: vec![
            ReadBackReceipt {
                target: format!("act:{push_id}"),
                observed: "receipt:Applied RefPublished — durable across the \
                           restart; the terminal constituent is never revisited"
                    .into(),
            },
            ReadBackReceipt {
                target: format!("act:{open_id}"),
                observed: "reconcile:Applied ChangeOpened cr-part head=head-1 — \
                           the outstanding constituent settled independently; \
                           no replay, no combined claim from partial facts"
                    .into(),
            },
            ReadBackReceipt {
                target: subjects[0].key(),
                observed: "read-back:Head read head-1 — agrees with RefPublished.observed".into(),
            },
            ReadBackReceipt {
                target: subjects[1].key(),
                observed: "read-back:Pr read head-1 — agrees with ChangeOpened.head".into(),
            },
        ],
    }
}

/// One record row: the scenario id, the adapter the run drove, and the
/// record built from the run's evidence — `None` when the checkout
/// yields no faithful stamp.
struct RecordRow {
    scenario: &'static str,
    adapter: &'static str,
    record: Option<EvidenceRecord>,
}

/// The reconciliation scenario table: every row runs unchanged
/// against each registered adapter.
type Scenario = fn(&mut dyn ReconcilePathAdapter) -> ScenarioEvidence;

const SCENARIOS: &[(&str, Scenario)] = &[
    (
        "lost_response_unknown_effects_reconcile_by_action_identity_without_replay",
        lost_response_unknown_effects_reconcile_by_action_identity_without_replay,
    ),
    (
        "permission_drift_denies_dependent_effects_and_keeps_read_only_reconciliation",
        permission_drift_denies_dependent_effects_and_keeps_read_only_reconciliation,
    ),
    (
        "actor_replacement_cannot_retarget_outstanding_actions",
        actor_replacement_cannot_retarget_outstanding_actions,
    ),
    (
        "restart_and_config_change_reconcile_under_the_original_binding",
        restart_and_config_change_reconcile_under_the_original_binding,
    ),
    (
        "partial_receipts_across_restart_reconcile_each_constituent_independently",
        partial_receipts_across_restart_reconcile_each_constituent_independently,
    ),
];

/// Run the reconciliation scenario table against one adapter kind — a
/// fresh adapter instance per scenario, so a scenario's remote
/// mutations never leak ordering-token state into the next scenario —
/// retaining one record per scenario when the checkout yields a
/// faithful stamp.
fn run_recorded(
    make_adapter: fn() -> Box<dyn ReconcilePathAdapter>,
    stamp: Option<&(String, String)>,
) -> Vec<RecordRow> {
    SCENARIOS
        .iter()
        .map(|&(id, run)| {
            let mut adapter = make_adapter();
            let adapter_id = adapter.adapter_id();
            let evidence = run(adapter.as_mut());
            let record = stamp.map(|(tested, base)| EvidenceRecord {
                schema_version: SCHEMA_VERSION,
                tested_revision: tested.clone(),
                base_revision: base.clone(),
                core_revision: tested.clone(),
                adapter_revision: tested.clone(),
                config_identity: ConfigIdentity::for_config(&config(adapter_id), adapter_id),
                command_or_scenario: format!("u36c-reconciliation::{id}"),
                action_identities: evidence.action_identities,
                observation_identities: evidence.observation_identities,
                read_back_receipts: evidence.read_back_receipts,
                result: ScenarioResult::Pass,
                claim_class: ClaimClass::FixtureContract,
                authorization_reference: None,
            });
            RecordRow {
                scenario: id,
                adapter: adapter_id,
                record,
            }
        })
        .collect()
}

/// U3.6c named run: the reconciliation and drift scenarios execute
/// unchanged on the GitHub adapter (`FixtureGithub`) and the
/// independent fixture (`SparseHost`), and every retained record
/// passes the field-presence check — one record per scenario per
/// adapter, `fixture_contract` throughout. The records are persisted
/// under `target/` only when the checkout yields a faithful revision
/// stamp.
#[test]
fn reconciliation_records_retain_one_record_per_scenario_per_adapter() {
    let stamp = record_revisions(env!("CARGO_MANIFEST_DIR"));
    let mut rows = Vec::new();
    rows.extend(run_recorded(
        || Box::new(FixtureGithub::new()),
        stamp.as_ref(),
    ));
    rows.extend(run_recorded(|| Box::new(SparseHost::new()), stamp.as_ref()));

    assert_eq!(rows.len(), 10, "five scenarios × two adapters");
    let adapters: BTreeSet<_> = rows.iter().map(|r| r.adapter).collect();
    assert_eq!(adapters.len(), 2, "one run per adapter");
    let scenarios: BTreeSet<_> = rows.iter().map(|r| r.scenario).collect();
    assert_eq!(scenarios.len(), 5, "one record per scenario");

    let retained_path =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/u3.6c-records.json");
    if let Some((tested, base)) = &stamp {
        let records: Vec<serde_json::Value> = rows
            .iter()
            .map(|r| {
                let record = r.record.as_ref().expect("stamped run retains a record");
                assert_eq!(record.tested_revision, *tested);
                assert_eq!(record.base_revision, *base);
                assert_eq!(record.claim_class, ClaimClass::FixtureContract);
                assert!(
                    record.authorization_reference.is_none(),
                    "a fixture record carries no authorization_reference"
                );
                serde_json::to_value(record).unwrap()
            })
            .collect();
        for value in &records {
            validate_record_json(value)
                .unwrap_or_else(|v| panic!("retained record fails the field check: {v:?}"));
        }
        let retained = serde_json::json!({ "records": records });
        std::fs::write(
            &retained_path,
            format!("{}\n", serde_json::to_string_pretty(&retained).unwrap()),
        )
        .unwrap_or_else(|e| {
            panic!(
                "retain the U3.6c record set at {}: {e}",
                retained_path.display()
            )
        });
    } else {
        for row in &rows {
            assert!(row.record.is_none(), "an unstamped run retains no records");
        }
        // An unstamped run retains none — a stale record set from an
        // earlier stamped run must not survive this one.
        if retained_path.exists() {
            std::fs::remove_file(&retained_path).unwrap_or_else(|e| {
                panic!(
                    "remove the stale U3.6c record set at {}: {e}",
                    retained_path.display()
                )
            });
        }
        eprintln!(
            "u3.6c: no faithful revision stamp (dirty or non-git checkout) — \
             the suite ran; no records were retained"
        );
    }
}

/// U3.6c acceptance: lost responses leave `unknown` effects that
/// reconcile by action identity without blind replay. Runs unchanged
/// on both adapters.
#[test]
fn lost_response_unknown_effects_reconcile_by_action_identity_without_a_replay() {
    let mut github = FixtureGithub::new();
    lost_response_unknown_effects_reconcile_by_action_identity_without_replay(&mut github);
    let mut sparse = SparseHost::new();
    lost_response_unknown_effects_reconcile_by_action_identity_without_replay(&mut sparse);
}

/// U3.6c acceptance: permission drift mid-Work records `denied` on
/// the authorization axis for new dependent effects — distinct from
/// `unsupported` — keeps applied receipts and unknown-effect binding
/// intact, and reconciliation continues with reads only. Runs
/// unchanged on both adapters.
#[test]
fn permission_drift_denies_dependent_effects_and_keeps_reconciliation_read_only() {
    let mut github = FixtureGithub::new();
    permission_drift_denies_dependent_effects_and_keeps_read_only_reconciliation(&mut github);
    let mut sparse = SparseHost::new();
    permission_drift_denies_dependent_effects_and_keeps_read_only_reconciliation(&mut sparse);
}

/// U3.6c acceptance: actor/credential replacement cannot retarget
/// outstanding actions — same-actor renewal is a no-op, a
/// different-actor binding applies only to new Work, and a removed
/// adapter surfaces `unavailable`, never a retarget. Runs unchanged
/// on both adapters.
#[test]
fn actor_replacement_never_retargets_outstanding_actions() {
    let mut github = FixtureGithub::new();
    actor_replacement_cannot_retarget_outstanding_actions(&mut github);
    let mut sparse = SparseHost::new();
    actor_replacement_cannot_retarget_outstanding_actions(&mut sparse);
}

/// U3.6c acceptance: restart plus post-configuration-change
/// reconciliation runs under the original binding, and partial
/// receipts reconcile each constituent independently with no
/// combined-success claim. Runs unchanged on both adapters.
#[test]
fn restart_and_partial_receipts_reconcile_under_the_original_binding() {
    let mut github = FixtureGithub::new();
    restart_and_config_change_reconcile_under_the_original_binding(&mut github);
    partial_receipts_across_restart_reconcile_each_constituent_independently(&mut github);
    let mut sparse = SparseHost::new();
    restart_and_config_change_reconcile_under_the_original_binding(&mut sparse);
    partial_receipts_across_restart_reconcile_each_constituent_independently(&mut sparse);
}
