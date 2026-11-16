//! U3.6b write-path suite: the shared write-path scenarios — stale-head
//! guarded-write rejection, partial push/create chains, independent
//! per-effect receipts, and read-back of every applied effect — run
//! unchanged against two independently implemented adapters and
//! retain one U3.6 evidence record per scenario per adapter.
//!
//! `FixtureGithub` is the GitHub adapter (the GitHub-shaped fixture the
//! U3 contract targets); `SparseHost` is the independent fixture
//! implementation — different remote model, the same guarded-write
//! contract for the operations it declares. `SparseHost` declares no
//! change-request update, so update intents there receive its explicit
//! `unsupported` receipt; the suite asserts that receipt rather than
//! weakening the scenario.
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
use workloop_github::fixture::{FixtureGithub, RemoteMutation, RemoteRead};
use workloop_github::sparse_fixture::{SparseHost, SparseMutation};
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

fn racer() -> HostActorRef {
    HostActorRef {
        host: host(),
        actor: HostActorId::trusted("racer"),
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

fn config(adapter_id: &str) -> WorkloopConfig {
    WorkloopConfig {
        host: HostTemplate {
            adapter: AdapterId::trusted(adapter_id),
            repository: repo(),
            contract_version: 1,
            actors: vec![RoleBinding {
                role: ActorRole::Publication,
                actor: publisher(),
                credential_ref: CredentialRef::trusted("cred-publication"),
            }],
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

/// What a conforming write-path adapter accepts to drive. Two
/// implementations: different remote models, the same guarded-write
/// contract — the suite never pattern-matches a fixture's internals.
trait WritePathAdapter {
    /// The adapter implementation id a record binds.
    fn adapter_id(&self) -> &'static str;

    /// Whether the adapter's declared capability set names
    /// `capability` — the suite branches on support, never on fixture
    /// internals.
    fn declares(&self, capability: &str) -> bool;

    /// Test control: arm a one-shot execution fault for `operation` —
    /// the next attempt reports `Failed{reason}` without a remote
    /// write.
    fn force_failure(&mut self, operation: &str, reason: &str);

    /// Test control: queue a racing actor's branch move, applied after
    /// the next successful write and before its read-back.
    fn queue_branch_race(
        &mut self,
        repository: &RepositoryRef,
        branch: &str,
        head: &str,
        actor: &HostActorRef,
    );

    /// Test control: queue a racing actor's change-metadata write.
    /// Adapters without change-request update inherit the no-op — the
    /// scenario never calls it for an undeclared capability.
    fn queue_meta_race(
        &mut self,
        _change: &ChangeRequestRef,
        _title: Option<String>,
        _body: Option<String>,
    ) {
    }

    /// Drain pending intents through the outbox.
    fn apply(&mut self, store: &SqliteStore, config: &WorkloopConfig);

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
}

impl WritePathAdapter for FixtureGithub {
    fn adapter_id(&self) -> &'static str {
        "fixture-github"
    }
    fn declares(&self, capability: &str) -> bool {
        Self::declared_capabilities().contains(&capability)
    }
    fn force_failure(&mut self, operation: &str, reason: &str) {
        FixtureGithub::force_failure(self, operation, reason);
    }
    fn queue_branch_race(
        &mut self,
        repository: &RepositoryRef,
        branch: &str,
        head: &str,
        actor: &HostActorRef,
    ) {
        self.queue_remote_mutation(RemoteMutation::MoveBranch {
            repository: repository.clone(),
            branch: branch.to_string(),
            head: head.to_string(),
            actor: actor.clone(),
        });
    }
    fn queue_meta_race(
        &mut self,
        change: &ChangeRequestRef,
        title: Option<String>,
        body: Option<String>,
    ) {
        self.queue_remote_mutation(RemoteMutation::SetChangeMeta {
            change: change.clone(),
            title,
            body,
        });
    }
    fn apply(&mut self, store: &SqliteStore, config: &WorkloopConfig) {
        self.apply_outbox(store, config, NOW).unwrap();
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
}

impl WritePathAdapter for SparseHost {
    fn adapter_id(&self) -> &'static str {
        "sparse-host"
    }
    fn declares(&self, capability: &str) -> bool {
        Self::declared_capabilities().contains(&capability)
    }
    fn force_failure(&mut self, operation: &str, reason: &str) {
        SparseHost::force_failure(self, operation, reason);
    }
    fn queue_branch_race(
        &mut self,
        repository: &RepositoryRef,
        branch: &str,
        head: &str,
        actor: &HostActorRef,
    ) {
        self.queue_remote_mutation(SparseMutation::MoveBranch {
            repository: repository.clone(),
            branch: branch.to_string(),
            head: head.to_string(),
            actor: actor.clone(),
        });
    }
    fn apply(&mut self, store: &SqliteStore, config: &WorkloopConfig) {
        self.apply_outbox(store, config, NOW).unwrap();
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
                    payload_digest: "test".into(),
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
                .find(|i| i.action_id == action_id))
        })
        .unwrap()
        .unwrap()
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

/// U3.6b write-path scenario 1 — a stale head prevents the write at
/// the boundary: an exact prior that does not match the remote and an
/// absent prior against an existing ref are both refused with
/// `Failed{stale_head}`, the fresh remote position surfaces before
/// each refusal receipt, and the matching-prior control writes land.
fn stale_head_guarded_write_rejected(adapter: &mut dyn WritePathAdapter) -> ScenarioEvidence {
    let store = SqliteStore::in_memory(1).unwrap();
    let config = config(adapter.adapter_id());
    let (work_id, eid) = submit(&store, &config, "u36b-stale");
    admit(&store, &config, &eid);
    let branch = "work/w";

    // Control writes: absent prior against an absent ref applies; the
    // exact prior that matches the remote applies. Both are fast-
    // forward moves the remote accepts.
    let ok_absent = insert_intent(&store, &work_id, push_intent(branch, "head-1", None));
    // Exact prior head-old while the remote sits at head-1: refused.
    let stale_exact = insert_intent(
        &store,
        &work_id,
        push_intent(branch, "head-x", Some("head-old")),
    );
    // Absent prior while the remote exists: refused.
    let stale_absent = insert_intent(&store, &work_id, push_intent(branch, "head-y", None));
    // Exact prior head-1 matching the remote: applies, lands head-2.
    let ok_exact = insert_intent(
        &store,
        &work_id,
        push_intent(branch, "head-2", Some("head-1")),
    );
    adapter.apply(&store, &config);

    let ok_absent_i = intent(&store, &ok_absent);
    assert_eq!(
        ok_absent_i.status,
        HostActionStatus::Applied,
        "{}: the absent-prior guard accepts an absent ref",
        adapter.adapter_id()
    );
    let ok_exact_i = intent(&store, &ok_exact);
    assert_eq!(
        ok_exact_i.status,
        HostActionStatus::Applied,
        "{}: the exact-prior guard accepts the matching remote head",
        adapter.adapter_id()
    );
    for fact in [only_effect(&ok_absent_i), only_effect(&ok_exact_i)] {
        let NormalizedEffectFact::RefPublished {
            produced,
            observed,
            relation,
            ..
        } = fact
        else {
            panic!("push receipt missing RefPublished: {fact:?}")
        };
        assert_eq!(produced, observed, "no race: read-back agrees");
        assert_eq!(*relation, RefRelation::Equal);
    }

    // Each refusal is an explicit failed receipt with no effect fact —
    // and the remote ref is exactly where the last applied write left
    // it.
    for id in [&stale_exact, &stale_absent] {
        let i = intent(&store, id);
        assert_eq!(
            i.status,
            HostActionStatus::Failed {
                reason: "stale_head".into()
            },
            "{}: a stale head refuses the write",
            adapter.adapter_id()
        );
        assert!(i.effects.is_empty(), "a refused write has no effect fact");
        assert_eq!(i.authenticated_actor, Some(publisher()));
    }
    assert_eq!(
        adapter.remote_head(&repo(), branch),
        Some("head-2".to_string()),
        "{}: refused writes never moved the ref",
        adapter.adapter_id()
    );

    // Each refusal surfaced the fresh remote position — recorded
    // between the surrounding receipts, never after them.
    let observations = store
        .transact(|tx| tx.observations_for_work(&work_id))
        .unwrap();
    let surfaced: Vec<&str> = observations
        .iter()
        .filter_map(|o| match &o.fact {
            ObservationFact::BranchState {
                branch: b,
                state: BranchState::At { revision },
                ..
            } if b == branch => Some(revision.object.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(
        surfaced,
        vec!["head-1", "head-1"],
        "{}: each refusal surfaced the fresh remote position",
        adapter.adapter_id()
    );
    for id in [&stale_exact, &stale_absent] {
        let result_at = observations
            .iter()
            .position(|o| {
                matches!(
                    &o.fact,
                    ObservationFact::HostActionResult { action_id, .. } if action_id == id
                )
            })
            .unwrap();
        let window_start = observations[..result_at]
            .iter()
            .rposition(|o| matches!(o.fact, ObservationFact::HostActionResult { .. }))
            .map(|p| p + 1)
            .unwrap_or(0);
        assert!(
            observations[window_start..result_at]
                .iter()
                .any(|o| matches!(
                    &o.fact,
                    ObservationFact::BranchState { branch: b, .. } if b == branch
                )),
            "{}: the fresh position lands before the {id} refusal receipt",
            adapter.adapter_id()
        );
    }

    // Read-back of the applied effect: a fresh typed read agrees with
    // the applied push's observed post-state.
    let head_subject = ReadSubject::Head {
        repository: repo(),
        branch: branch.into(),
    };
    let head_read = adapter.read(&head_subject, &reader());
    assert_eq!(
        head_read.revision,
        Some(ReadRevision::Sha {
            revision: rev("head-2")
        }),
        "{}: the fresh read observes the applied push's head",
        adapter.adapter_id()
    );
    assert!(head_read.completeness.is_complete());
    let receipt = adapter.observe(
        &store,
        &config,
        Some(&work_id),
        &head_subject,
        &reader(),
        NOW,
    );
    assert_eq!(receipt, IngestReceipt::Recorded);

    ScenarioEvidence {
        action_identities: vec![
            ok_absent.clone(),
            stale_exact.clone(),
            stale_absent.clone(),
            ok_exact.clone(),
        ],
        observation_identities: delivery_identities(&store, std::slice::from_ref(&head_subject)),
        read_back_receipts: vec![
            ReadBackReceipt {
                target: format!("act:{ok_absent}"),
                observed: "receipt:Applied RefPublished produced=observed=head-1 \
                           relation=Equal — the absent-prior guard matched an absent ref"
                    .into(),
            },
            ReadBackReceipt {
                target: format!("act:{stale_exact}"),
                observed: "receipt:Failed{stale_head} — fresh BranchState head-1 \
                           surfaced before the receipt; the ref stayed put"
                    .into(),
            },
            ReadBackReceipt {
                target: format!("act:{stale_absent}"),
                observed: "receipt:Failed{stale_head} — the absent-prior guard \
                           refused an existing ref"
                    .into(),
            },
            ReadBackReceipt {
                target: format!("act:{ok_exact}"),
                observed: "receipt:Applied RefPublished produced=observed=head-2 \
                           relation=Equal — the exact prior head-1 matched"
                    .into(),
            },
            ReadBackReceipt {
                target: head_subject.key(),
                observed: format!(
                    "read-back:Head read head-2 @v{} — agrees with the applied push",
                    head_read.subject_version
                ),
            },
        ],
    }
}

/// U3.6b write-path scenario 2 — a push succeeds and the change-request
/// create faults: the successful push receipt and the failed create
/// receipt stay independent, no `ChangeOpened` fact exists anywhere,
/// and no publication completion is claimed.
fn partial_push_then_create_failure_keeps_independent_receipts(
    adapter: &mut dyn WritePathAdapter,
) -> ScenarioEvidence {
    let store = SqliteStore::in_memory(1).unwrap();
    let config = config(adapter.adapter_id());
    let (work_id, eid) = submit(&store, &config, "u36b-partial");
    admit(&store, &config, &eid);
    let branch = "work/w";

    let push_id = insert_intent(&store, &work_id, push_intent(branch, "head-1", None));
    let open_id = insert_intent(&store, &work_id, open_intent(branch, "partial"));
    adapter.force_failure("open_change_request", "remote rejected the create");
    adapter.apply(&store, &config);

    // The push's applied receipt survives the sibling fault: the ref
    // publish is real remote state, not rewritten by the create's
    // failure.
    let push = intent(&store, &push_id);
    assert_eq!(
        push.status,
        HostActionStatus::Applied,
        "{}: the push applied",
        adapter.adapter_id()
    );
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
    assert_eq!(*produced, rev("head-1"));
    assert_eq!(*observed, rev("head-1"), "read-back, not attempted");
    assert_eq!(*relation, RefRelation::Equal);
    assert_eq!(push.authenticated_actor, Some(publisher()));
    assert_eq!(
        adapter.remote_head(&repo(), branch),
        Some("head-1".to_string()),
        "{}: the push's effect is real remote state",
        adapter.adapter_id()
    );

    // The failed create is explicit and carries no effect fact — the
    // two receipts are bound to their own action identities.
    let open = intent(&store, &open_id);
    assert_eq!(
        open.status,
        HostActionStatus::Failed {
            reason: "remote rejected the create".into()
        },
        "{}: the create fault is a definite no-effect failure",
        adapter.adapter_id()
    );
    assert!(open.effects.is_empty());
    assert_eq!(open.authenticated_actor, Some(publisher()));
    assert_ne!(push_id, open_id, "independent action identities");

    // No publication claim: no intent anywhere carries a ChangeOpened
    // fact — partial success never reads as completed publication.
    let any_opened = store
        .transact(|tx| {
            Ok(tx
                .action_intents()?
                .iter()
                .flat_map(|i| i.effects.iter())
                .any(|e| matches!(e, NormalizedEffectFact::ChangeOpened { .. })))
        })
        .unwrap();
    assert!(
        !any_opened,
        "{}: no change-opened evidence exists — publication is not complete",
        adapter.adapter_id()
    );

    // Read-back of the applied effect.
    let head_subject = ReadSubject::Head {
        repository: repo(),
        branch: branch.into(),
    };
    let head_read = adapter.read(&head_subject, &reader());
    assert_eq!(
        head_read.revision,
        Some(ReadRevision::Sha {
            revision: rev("head-1")
        })
    );
    assert!(head_read.completeness.is_complete());
    let receipt = adapter.observe(
        &store,
        &config,
        Some(&work_id),
        &head_subject,
        &reader(),
        NOW,
    );
    assert_eq!(receipt, IngestReceipt::Recorded);

    ScenarioEvidence {
        action_identities: vec![push_id.clone(), open_id.clone()],
        observation_identities: delivery_identities(&store, std::slice::from_ref(&head_subject)),
        read_back_receipts: vec![
            ReadBackReceipt {
                target: format!("act:{push_id}"),
                observed: "receipt:Applied RefPublished produced=observed=head-1 \
                           relation=Equal — independent of the sibling fault"
                    .into(),
            },
            ReadBackReceipt {
                target: format!("act:{open_id}"),
                observed: "receipt:Failed{remote rejected the create} — a \
                           definite no-effect failure; no ChangeOpened fact"
                    .into(),
            },
            ReadBackReceipt {
                target: head_subject.key(),
                observed: format!(
                    "read-back:Head read head-1 @v{} — the push's effect is real remote state",
                    head_read.subject_version
                ),
            },
        ],
    }
}

/// U3.6b write-path scenario 3 — the push → create → update chain:
/// every effect keeps its own receipt bound to its own action
/// identity, the update's applied-or-explicit-unsupported receipt
/// follows the adapter's declared capability, and a faulted tail
/// update leaves every prior receipt intact.
fn push_create_update_chain_keeps_independent_effect_receipts(
    adapter: &mut dyn WritePathAdapter,
) -> ScenarioEvidence {
    let store = SqliteStore::in_memory(1).unwrap();
    let config = config(adapter.adapter_id());
    let (work_id, eid) = submit(&store, &config, "u36b-chain");
    admit(&store, &config, &eid);
    let branch = "work/w";
    let declares_update = adapter.declares("change_request_update");

    // Phase A: push then open — the create learns the change identity
    // and head from its own applied receipt.
    let push_id = insert_intent(&store, &work_id, push_intent(branch, "head-1", None));
    let open_id = insert_intent(&store, &work_id, open_intent(branch, "chain"));
    adapter.apply(&store, &config);

    let push = intent(&store, &push_id);
    assert_eq!(push.status, HostActionStatus::Applied);
    assert!(matches!(
        only_effect(&push),
        NormalizedEffectFact::RefPublished { .. }
    ));
    let open = intent(&store, &open_id);
    assert_eq!(open.status, HostActionStatus::Applied);
    let [NormalizedEffectFact::ChangeOpened { change, head, .. }] = open.effects.as_slice() else {
        panic!("open receipt missing ChangeOpened: {:?}", open.effects)
    };
    let change = change.clone();
    let opened_head = head.clone();
    assert_eq!(opened_head, rev("head-1"));

    // Phase B: the update chain link — applied where the capability is
    // declared, an explicit unsupported receipt where it is not.
    let update_id = insert_intent(
        &store,
        &work_id,
        HostAction::UpdateChangeRequest {
            change: change.clone(),
            expected_head: opened_head.clone(),
            title: Some("chain title".into()),
            body: Some("chain body".into()),
        },
    );
    adapter.apply(&store, &config);
    let update = intent(&store, &update_id);
    if declares_update {
        assert_eq!(
            update.status,
            HostActionStatus::Applied,
            "{}: the declared update applies",
            adapter.adapter_id()
        );
        let [
            NormalizedEffectFact::MetadataUpdated {
                change: c,
                head,
                fields_digest,
            },
        ] = update.effects.as_slice()
        else {
            panic!(
                "update receipt missing MetadataUpdated: {:?}",
                update.effects
            )
        };
        assert_eq!(*c, change);
        assert_eq!(*head, opened_head);
        // No race was queued: the digest covers the stored fields,
        // which equal the attempted ones.
        let stored = workloop_core::domain::sha256_hex(
            "change-meta-v1",
            &workloop_core::domain::canonical_json(&serde_json::json!({
                "title": Some("chain title"),
                "body": Some("chain body"),
            })),
        );
        assert_eq!(*fields_digest, stored);
    } else {
        match &update.status {
            HostActionStatus::Unsupported { reason } => assert!(
                reason.contains("update_change_request"),
                "{}: the unsupported receipt names the operation",
                adapter.adapter_id()
            ),
            other => panic!(
                "{}: an undeclared update is explicitly unsupported, got {other:?}",
                adapter.adapter_id()
            ),
        }
        assert!(update.effects.is_empty());
    }
    assert_eq!(update.authenticated_actor, Some(publisher()));

    // Phase C: the chain's tail faults — the prior receipts stay
    // exactly as they were; the faulted update claims nothing.
    let tail_id = insert_intent(
        &store,
        &work_id,
        HostAction::UpdateChangeRequest {
            change: change.clone(),
            expected_head: opened_head.clone(),
            title: Some("tail title".into()),
            body: None,
        },
    );
    adapter.force_failure("update_change_request", "remote rejected the update");
    adapter.apply(&store, &config);
    let tail = intent(&store, &tail_id);
    if declares_update {
        assert_eq!(
            tail.status,
            HostActionStatus::Failed {
                reason: "remote rejected the update".into()
            },
            "{}: the tail update fault is a definite failure",
            adapter.adapter_id()
        );
    } else {
        assert!(
            matches!(tail.status, HostActionStatus::Unsupported { .. }),
            "{}: the undeclared tail update stays explicitly unsupported",
            adapter.adapter_id()
        );
    }
    assert!(tail.effects.is_empty());
    assert_eq!(tail.authenticated_actor, Some(publisher()));

    // Every receipt is independent: distinct action identities, each
    // bound to the original publication actor, each with its own
    // status and effect facts.
    let ids = [
        push_id.clone(),
        open_id.clone(),
        update_id.clone(),
        tail_id.clone(),
    ];
    let unique: BTreeSet<_> = ids.iter().collect();
    assert_eq!(unique.len(), 4, "four independent action identities");
    for id in &ids {
        assert_eq!(
            intent(&store, id).authenticated_actor,
            Some(publisher()),
            "{id}: the receipt keeps the original binding"
        );
    }
    // Exactly one fact of each applied kind exists — the chain never
    // double-claims an effect.
    let intents = store.transact(|tx| tx.action_intents()).unwrap();
    let count = |pred: fn(&NormalizedEffectFact) -> bool| {
        intents
            .iter()
            .flat_map(|i| i.effects.iter())
            .filter(|e| pred(e))
            .count()
    };
    assert_eq!(
        count(|e| matches!(e, NormalizedEffectFact::RefPublished { .. })),
        1
    );
    assert_eq!(
        count(|e| matches!(e, NormalizedEffectFact::ChangeOpened { .. })),
        1
    );
    let metadata = count(|e| matches!(e, NormalizedEffectFact::MetadataUpdated { .. }));
    assert_eq!(metadata, usize::from(declares_update));

    // Read-back of every applied effect: fresh typed reads of the
    // branch head and the change head agree with the receipts.
    let head_subject = ReadSubject::Head {
        repository: repo(),
        branch: branch.into(),
    };
    let pr_subject = ReadSubject::Pr {
        change: change.clone(),
    };
    for (subject, expected) in [
        (&head_subject, rev("head-1")),
        (&pr_subject, opened_head.clone()),
    ] {
        let read = adapter.read(subject, &reader());
        assert_eq!(
            read.revision,
            Some(ReadRevision::Sha {
                revision: expected.clone()
            }),
            "{}: fresh read of {} agrees with the applied receipts",
            adapter.adapter_id(),
            subject.key()
        );
        assert!(read.completeness.is_complete());
        let receipt = adapter.observe(&store, &config, Some(&work_id), subject, &reader(), NOW);
        assert_eq!(receipt, IngestReceipt::Recorded);
    }

    ScenarioEvidence {
        action_identities: ids.iter().map(|id| id.to_string()).collect(),
        observation_identities: delivery_identities(
            &store,
            &[head_subject.clone(), pr_subject.clone()],
        ),
        read_back_receipts: vec![
            ReadBackReceipt {
                target: format!("act:{push_id}"),
                observed: "receipt:Applied RefPublished produced=observed=head-1 \
                           relation=Equal"
                    .into(),
            },
            ReadBackReceipt {
                target: format!("act:{open_id}"),
                observed: "receipt:Applied ChangeOpened head=head-1 — the \
                           change identity comes from the receipt itself"
                    .into(),
            },
            ReadBackReceipt {
                target: format!("act:{update_id}"),
                observed: if declares_update {
                    "receipt:Applied MetadataUpdated fields_digest covers the \
                     stored fields; the tail fault leaves it intact"
                        .into()
                } else {
                    "receipt:Unsupported{unsupported operation: \
                     update_change_request} — explicit, never applied"
                        .into()
                },
            },
            ReadBackReceipt {
                target: format!("act:{tail_id}"),
                observed: if declares_update {
                    "receipt:Failed{remote rejected the update} — the chain's \
                     tail fault leaves every prior receipt intact"
                        .into()
                } else {
                    "receipt:Unsupported — an undeclared capability is never \
                     an applied claim"
                        .into()
                },
            },
            ReadBackReceipt {
                target: head_subject.key(),
                observed: "read-back:Head read head-1 — agrees with RefPublished.observed".into(),
            },
            ReadBackReceipt {
                target: pr_subject.key(),
                observed: "read-back:Pr read head-1 — agrees with ChangeOpened.head".into(),
            },
        ],
    }
}

/// U3.6b write-path scenario 4 — a racing actor's write lands between
/// the adapter's write and its read-back: the receipts report the
/// observed post-state — `Ancestor` relation, the racer's change
/// head, the racer's stored fields — never the attempted values, and
/// fresh typed reads agree with what the receipts cite.
fn raced_write_read_back_reports_the_observed_post_state(
    adapter: &mut dyn WritePathAdapter,
) -> ScenarioEvidence {
    let store = SqliteStore::in_memory(1).unwrap();
    let config = config(adapter.adapter_id());
    let (work_id, eid) = submit(&store, &config, "u36b-raced");
    admit(&store, &config, &eid);
    let branch = "work/w";
    let declares_update = adapter.declares("change_request_update");

    // Phase A: the push writes head-1; the racer's descendant lands
    // before the read-back — the receipt must observe it.
    let push_id = insert_intent(&store, &work_id, push_intent(branch, "head-1", None));
    adapter.queue_branch_race(&repo(), branch, "head-2", &racer());
    adapter.apply(&store, &config);

    let push = intent(&store, &push_id);
    assert_eq!(push.status, HostActionStatus::Applied);
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
    assert_eq!(*produced, rev("head-1"), "the attempted value");
    assert_eq!(
        *observed,
        rev("head-2"),
        "{}: the read-back observed the racer's descendant, not the attempt",
        adapter.adapter_id()
    );
    assert_eq!(*relation, RefRelation::Ancestor);
    assert_eq!(
        adapter.remote_head(&repo(), branch),
        Some("head-2".to_string())
    );

    // The open reads the branch head the remote now holds — head-2 —
    // and its receipt reports that observed head.
    let open_id = insert_intent(&store, &work_id, open_intent(branch, "raced"));
    adapter.apply(&store, &config);
    let open = intent(&store, &open_id);
    assert_eq!(open.status, HostActionStatus::Applied);
    let [NormalizedEffectFact::ChangeOpened { change, head, .. }] = open.effects.as_slice() else {
        panic!("open receipt missing ChangeOpened: {:?}", open.effects)
    };
    let change = change.clone();
    let opened_head = head.clone();
    assert_eq!(
        opened_head,
        rev("head-2"),
        "{}: the create bound the post-race branch head",
        adapter.adapter_id()
    );

    // Phase B: the update writes its fields; a racing metadata write
    // lands before the read-back — the digest covers the stored
    // (observed) fields where the capability is declared.
    let update_id = insert_intent(
        &store,
        &work_id,
        HostAction::UpdateChangeRequest {
            change: change.clone(),
            expected_head: opened_head.clone(),
            title: Some("suite title".into()),
            body: Some("suite body".into()),
        },
    );
    if declares_update {
        adapter.queue_meta_race(&change, Some("racer title".into()), None);
    }
    adapter.apply(&store, &config);
    let update = intent(&store, &update_id);
    if declares_update {
        assert_eq!(update.status, HostActionStatus::Applied);
        let [
            NormalizedEffectFact::MetadataUpdated {
                change: c,
                head,
                fields_digest,
            },
        ] = update.effects.as_slice()
        else {
            panic!(
                "update receipt missing MetadataUpdated: {:?}",
                update.effects
            )
        };
        assert_eq!(*c, change);
        assert_eq!(*head, opened_head);
        let observed_digest = workloop_core::domain::sha256_hex(
            "change-meta-v1",
            &workloop_core::domain::canonical_json(&serde_json::json!({
                "title": Some("racer title"),
                "body": Some("suite body"),
            })),
        );
        let attempted_digest = workloop_core::domain::sha256_hex(
            "change-meta-v1",
            &workloop_core::domain::canonical_json(&serde_json::json!({
                "title": Some("suite title"),
                "body": Some("suite body"),
            })),
        );
        assert_eq!(
            *fields_digest,
            observed_digest,
            "{}: the digest cites the observed stored fields",
            adapter.adapter_id()
        );
        assert_ne!(
            *fields_digest,
            attempted_digest,
            "{}: the digest is not the attempted payload",
            adapter.adapter_id()
        );
    } else {
        assert!(
            matches!(update.status, HostActionStatus::Unsupported { .. }),
            "{}: the undeclared update is explicitly unsupported",
            adapter.adapter_id()
        );
        assert!(update.effects.is_empty());
    }

    // Fresh typed reads agree with every applied receipt's cited
    // post-state.
    let head_subject = ReadSubject::Head {
        repository: repo(),
        branch: branch.into(),
    };
    let pr_subject = ReadSubject::Pr {
        change: change.clone(),
    };
    for (subject, expected) in [
        (&head_subject, rev("head-2")),
        (&pr_subject, opened_head.clone()),
    ] {
        let read = adapter.read(subject, &reader());
        assert_eq!(
            read.revision,
            Some(ReadRevision::Sha {
                revision: expected.clone()
            }),
            "{}: fresh read of {} agrees with the receipts' observed state",
            adapter.adapter_id(),
            subject.key()
        );
        assert!(read.completeness.is_complete());
        let receipt = adapter.observe(&store, &config, Some(&work_id), subject, &reader(), NOW);
        assert_eq!(receipt, IngestReceipt::Recorded);
    }

    ScenarioEvidence {
        action_identities: vec![push_id.clone(), open_id.clone(), update_id.clone()],
        observation_identities: delivery_identities(
            &store,
            &[head_subject.clone(), pr_subject.clone()],
        ),
        read_back_receipts: vec![
            ReadBackReceipt {
                target: format!("act:{push_id}"),
                observed: "receipt:Applied RefPublished produced=head-1 \
                           observed=head-2 relation=Ancestor — the read-back \
                           observed the racer's descendant, not the attempt"
                    .into(),
            },
            ReadBackReceipt {
                target: format!("act:{open_id}"),
                observed: "receipt:Applied ChangeOpened head=head-2 — the \
                           remote's post-race branch head"
                    .into(),
            },
            ReadBackReceipt {
                target: format!("act:{update_id}"),
                observed: if declares_update {
                    "receipt:Applied MetadataUpdated fields_digest covers the \
                     stored (racer title, suite body) — observed fields, \
                     not the attempted payload"
                        .into()
                } else {
                    "receipt:Unsupported — an undeclared capability is never \
                     an applied claim"
                        .into()
                },
            },
            ReadBackReceipt {
                target: head_subject.key(),
                observed: "read-back:Head read head-2 — agrees with \
                           RefPublished.observed"
                    .into(),
            },
            ReadBackReceipt {
                target: pr_subject.key(),
                observed: "read-back:Pr read head-2 — agrees with \
                           ChangeOpened.head"
                    .into(),
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

/// The write-path scenario table: every row runs unchanged against
/// each registered adapter.
type Scenario = fn(&mut dyn WritePathAdapter) -> ScenarioEvidence;

const SCENARIOS: &[(&str, Scenario)] = &[
    (
        "stale_head_guarded_write_rejected",
        stale_head_guarded_write_rejected,
    ),
    (
        "partial_push_then_create_failure_keeps_independent_receipts",
        partial_push_then_create_failure_keeps_independent_receipts,
    ),
    (
        "push_create_update_chain_keeps_independent_effect_receipts",
        push_create_update_chain_keeps_independent_effect_receipts,
    ),
    (
        "raced_write_read_back_reports_the_observed_post_state",
        raced_write_read_back_reports_the_observed_post_state,
    ),
];

/// Run the write-path scenario table against one adapter kind — a
/// fresh adapter instance per scenario, so a scenario's remote
/// mutations never leak ordering-token state into the next scenario —
/// retaining one record per scenario when the checkout yields a
/// faithful stamp.
fn run_recorded(
    make_adapter: fn() -> Box<dyn WritePathAdapter>,
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
                command_or_scenario: format!("u36b-write-path::{id}"),
                action_identities: evidence.action_identities,
                observation_identities: evidence.observation_identities,
                read_back_receipts: evidence.read_back_receipts,
                result: ScenarioResult::Pass,
                claim_class: ClaimClass::FixtureContract,
                authorization_reference: None,
                isolation_scope: None,
            });
            RecordRow {
                scenario: id,
                adapter: adapter_id,
                record,
            }
        })
        .collect()
}

/// U3.6b named run: the write-path scenarios execute unchanged on the
/// GitHub adapter (`FixtureGithub`) and the independent fixture
/// (`SparseHost`), and every retained record passes the field-presence
/// check — one record per scenario per adapter, `fixture_contract`
/// throughout. The records are persisted under `target/` only when the
/// checkout yields a faithful revision stamp.
#[test]
fn write_path_records_retain_one_record_per_scenario_per_adapter() {
    let stamp = record_revisions(env!("CARGO_MANIFEST_DIR"));
    let mut rows = Vec::new();
    rows.extend(run_recorded(
        || Box::new(FixtureGithub::new()),
        stamp.as_ref(),
    ));
    rows.extend(run_recorded(|| Box::new(SparseHost::new()), stamp.as_ref()));

    assert_eq!(rows.len(), 8, "four scenarios × two adapters");
    let adapters: BTreeSet<_> = rows.iter().map(|r| r.adapter).collect();
    assert_eq!(adapters.len(), 2, "one run per adapter");
    let scenarios: BTreeSet<_> = rows.iter().map(|r| r.scenario).collect();
    assert_eq!(scenarios.len(), 4, "one record per scenario");

    let retained_path =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/u3.6b-records.json");
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
                "retain the U3.6b record set at {}: {e}",
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
                    "remove the stale U3.6b record set at {}: {e}",
                    retained_path.display()
                )
            });
        }
        eprintln!(
            "u3.6b: no faithful revision stamp (dirty or non-git checkout) — \
             the suite ran; no records were retained"
        );
    }
}

/// U3.6b acceptance: a named scenario lands a push then fails the
/// change-request create and asserts two independent receipts — push
/// `Applied`, create `Failed{reason}` — with no publication claim
/// recorded. Runs unchanged on both adapters.
#[test]
fn push_success_create_failure_keeps_two_independent_receipts_without_a_publication_claim() {
    let mut github = FixtureGithub::new();
    partial_push_then_create_failure_keeps_independent_receipts(&mut github);
    let mut sparse = SparseHost::new();
    partial_push_then_create_failure_keeps_independent_receipts(&mut sparse);
}

/// U3.6b acceptance: every applied effect's read-back receipt cites
/// the observed post-state from a fresh read — never the attempted
/// value. A racing branch move lands between the push write and its
/// read-back; the receipt reports the observed descendant and a fresh
/// typed read agrees with it. Runs unchanged on both adapters.
#[test]
fn applied_effect_receipts_cite_the_observed_post_state_not_the_attempted_value() {
    let makers: [fn() -> Box<dyn WritePathAdapter>; 2] = [
        || Box::new(FixtureGithub::new()),
        || Box::new(SparseHost::new()),
    ];
    for make in makers {
        let mut adapter = make();
        let store = SqliteStore::in_memory(1).unwrap();
        let config = config(adapter.adapter_id());
        let (work_id, eid) = submit(&store, &config, "u36b-readback-ac");
        admit(&store, &config, &eid);

        let push_id = insert_intent(&store, &work_id, push_intent("work/w", "head-1", None));
        adapter.queue_branch_race(&repo(), "work/w", "head-2", &racer());
        adapter.apply(&store, &config);

        let push = intent(&store, &push_id);
        assert_eq!(push.status, HostActionStatus::Applied);
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
        assert_eq!(*produced, rev("head-1"), "the attempted value");
        assert_eq!(*observed, rev("head-2"), "the observed post-state");
        assert_eq!(*relation, RefRelation::Ancestor);
        assert_ne!(observed, produced, "observed, never the attempt");

        // The cited observed state is what a fresh read reports — the
        // receipt did not echo the attempted payload.
        let head_subject = ReadSubject::Head {
            repository: repo(),
            branch: "work/w".into(),
        };
        let fresh = adapter.read(&head_subject, &reader());
        assert_eq!(
            fresh.revision,
            Some(ReadRevision::Sha {
                revision: rev("head-2")
            }),
            "{}: the fresh read agrees with the receipt's observed state",
            adapter.adapter_id()
        );
        assert!(fresh.completeness.is_complete());
    }
}
