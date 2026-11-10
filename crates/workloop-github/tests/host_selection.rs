//! U9.3: configuration selects the host for new Work only. After the
//! installation's default host changes, a newly submitted Work binds
//! the new adapter/repository/host while existing Work — and its
//! unresolved effects — keeps the binding recorded at acceptance.
//! Nothing migrates across hosts; reconciliation continues under the
//! original binding.

use std::collections::BTreeSet;

use workloop_core::command::*;
use workloop_core::domain::*;
use workloop_core::identity::*;
use workloop_core::observation::*;
use workloop_core::store::*;
use workloop_core::work::{HostTemplate, WorkloopConfig, handle_command};
use workloop_github::fixture::FixtureGithub;
use workloop_github::gitlab_fixture::GitlabFixture;
use workloop_store::sqlite::SqliteStore;

const NOW: i64 = 1_700_000_000;

fn host_a() -> HostInstanceId {
    HostInstanceId::trusted("test-host-a")
}

fn host_b() -> HostInstanceId {
    HostInstanceId::trusted("test-host-b")
}

/// The same repository ID on two different host instances — distinct
/// repositories under the contract's identity rules.
fn repo_on(host: HostInstanceId) -> RepositoryRef {
    RepositoryRef::new(host, RepositoryId::trusted("repo"))
}

fn rev_on(repository: &RepositoryRef, object: &str) -> CommitRevision {
    CommitRevision::new(repository.clone(), object).unwrap()
}

fn actor_on(host: HostInstanceId, name: &str) -> HostActorRef {
    HostActorRef {
        host,
        actor: HostActorId::trusted(name),
    }
}

const FULL_BASELINE: &[&str] = &[
    "repository_read",
    "branch_read",
    "push_ref",
    "change_request_create",
    "change_request_read",
    "change_request_update",
    "comment_create",
    "checks_read",
    "review_findings_read",
    "issue_read",
];

fn declared() -> BTreeSet<HostCapability> {
    FULL_BASELINE
        .iter()
        .map(|n| HostCapability::from_name(n).unwrap())
        .collect()
}

fn config(adapter: &str, repository: RepositoryRef, actor: HostActorRef) -> WorkloopConfig {
    WorkloopConfig {
        host: HostTemplate {
            adapter: AdapterId::trusted(adapter),
            repository,
            contract_version: 1,
            actors: vec![RoleBinding {
                role: ActorRole::Publication,
                actor,
                credential_ref: CredentialRef::trusted("cred-publication"),
            }],
        },
        declared_capabilities: declared(),
        lanes: LanePolicy::default(),
        required_checks: vec![],
        allowed_effects: AllowedEffects {
            workspace_write: true,
            host_write: true,
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
            origin: Origin("host-selection".into()),
            idempotency_key: key.into(),
            body: CommandBody::Submit {
                intent: "host selection work".into(),
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
    host: HostInstanceId,
    repository: &RepositoryRef,
    actor: HostActorRef,
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
                    host,
                    repository: repository.clone(),
                    role: ActorRole::Publication,
                    actor,
                    action,
                    payload_digest: "host-selection".into(),
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

fn work(store: &SqliteStore, work_id: &WorkId) -> Work {
    store
        .transact(|tx| Ok(tx.work(work_id)?.expect("work recorded")))
        .unwrap()
}

/// Configuration swap selects the host for *new* Work only: Work A
/// keeps its adapter/repository/host binding, and its unresolved open
/// intent is neither drained nor retargeted by the new host's adapter —
/// it reconciles under its original binding even while the
/// installation default points at host B.
#[test]
fn config_swap_selects_host_for_new_work_and_keeps_existing_bindings() {
    let store = SqliteStore::in_memory(2).unwrap();
    let mut fixture_a = FixtureGithub::for_host(host_a());
    let mut fixture_b = GitlabFixture::for_host(host_b());
    let repo_a = repo_on(host_a());
    let repo_b = repo_on(host_b());
    let pub_a = actor_on(host_a(), "pub-a");
    let pub_b = actor_on(host_b(), "pub-b");

    // Work A is accepted under the original configuration.
    let config_a = config("adapter-a", repo_a.clone(), pub_a.clone());
    let (work_a, _ea) = submit(&store, &config_a, "u93-a");
    let binding_a = work(&store, &work_a).binding;
    assert_eq!(binding_a.adapter, AdapterId::trusted("adapter-a"));
    assert_eq!(binding_a.repository, repo_a);

    // A push applies on host A; the following open loses its response
    // and stays an unresolved effect bound to (host A, repo A).
    let branch_a = format!("work/{}", work_a.0);
    insert_intent(
        &store,
        &work_a,
        host_a(),
        &repo_a,
        pub_a.clone(),
        HostAction::PushRef {
            branch: branch_a.clone(),
            produced: rev_on(&repo_a, "head-a-1"),
        },
    );
    let open_a = insert_intent(
        &store,
        &work_a,
        host_a(),
        &repo_a,
        pub_a.clone(),
        HostAction::OpenChangeRequest {
            source_branch: branch_a.clone(),
            target_branch: "main".into(),
            title: "t".into(),
            body: String::new(),
        },
    );
    fixture_a
        .emit(
            &store,
            &config_a,
            Observation {
                delivery: None,
                subject_work_id: Some(work_a.clone()),
                subject_execution_id: None,
                fact: ObservationFact::HostActionResult {
                    action_id: open_a.clone(),
                    authenticated_actor: Some(pub_a.clone()),
                    status: HostActionStatus::Unknown {
                        reason: "response lost".into(),
                    },
                    effects: vec![],
                },
                observed_unix_seconds: NOW,
            },
            NOW,
        )
        .unwrap();
    fixture_a.apply_outbox(&store, &config_a, NOW).unwrap();
    assert_eq!(fixture_a.applied().len(), 1, "only the push landed");
    assert_eq!(
        intent(&store, &open_a).status,
        HostActionStatus::Unknown {
            reason: "response lost".into()
        },
        "the open stays an outstanding effect"
    );

    // The installation default moves to adapter B on another host
    // instance. Core/Command semantics are unchanged: the same submit
    // is accepted the same way, and only the binding differs.
    let config_b = config("adapter-b", repo_b.clone(), pub_b.clone());
    let (work_b, _eb) = submit(&store, &config_b, "u93-b");
    let binding_b = work(&store, &work_b).binding;
    assert_eq!(binding_b.adapter, AdapterId::trusted("adapter-b"));
    assert_eq!(binding_b.repository, repo_b);
    assert_eq!(
        binding_b.required_capabilities, binding_a.required_capabilities,
        "the required-capability profile is identical across the swap"
    );
    assert_eq!(
        work(&store, &work_a).binding,
        binding_a,
        "Work A keeps the binding recorded at acceptance"
    );

    // Work B's intents belong to host B. Host A's adapter drains none
    // of them — they stay pending for their own adapter.
    let branch_b = format!("work/{}", work_b.0);
    insert_intent(
        &store,
        &work_b,
        host_b(),
        &repo_b,
        pub_b.clone(),
        HostAction::PushRef {
            branch: branch_b.clone(),
            produced: rev_on(&repo_b, "head-b-1"),
        },
    );
    insert_intent(
        &store,
        &work_b,
        host_b(),
        &repo_b,
        pub_b.clone(),
        HostAction::OpenChangeRequest {
            source_branch: branch_b.clone(),
            target_branch: "main".into(),
            title: "t".into(),
            body: String::new(),
        },
    );
    fixture_a.apply_outbox(&store, &config_b, NOW).unwrap();
    assert_eq!(
        fixture_a.applied().len(),
        1,
        "host A's adapter writes nothing bound to host B"
    );
    assert!(
        store
            .transact(|tx| Ok(tx
                .action_intents()?
                .into_iter()
                .filter(|i| i.work_id == work_b)
                .all(|i| i.status == HostActionStatus::Pending)))
            .unwrap(),
        "host B intents wait for their own adapter"
    );

    // Symmetrically, host B's adapter must leave the existing work's
    // host-A intents pending — including one queued *after* the swap.
    let push_a2 = insert_intent(
        &store,
        &work_a,
        host_a(),
        &repo_a,
        pub_a.clone(),
        HostAction::PushRef {
            branch: "work/follow-up".into(),
            produced: rev_on(&repo_a, "head-a-2"),
        },
    );

    // Host B's adapter drains its own intents under the swapped
    // configuration — Command/queue semantics carry over unchanged.
    fixture_b.apply_outbox(&store, &config_b, NOW).unwrap();
    assert_eq!(fixture_b.applied().len(), 2);
    assert_eq!(
        intent(&store, &push_a2).status,
        HostActionStatus::Pending,
        "host B's adapter must not drain host-A intents"
    );
    assert!(
        fixture_b.remote_head(&repo_a, "work/follow-up").is_none(),
        "the foreign intent touched nothing on host B"
    );
    assert_eq!(
        fixture_b.remote_head(&repo_b, &branch_b),
        Some("head-b-1".to_string())
    );
    assert_eq!(
        intent(&store, &open_a).status,
        HostActionStatus::Unknown {
            reason: "response lost".into()
        },
        "the outstanding effect is not retargeted by the new host's adapter"
    );
    assert!(
        fixture_b.remote_head(&repo_a, &branch_a).is_none(),
        "host B's remote never sees Work A's branch"
    );

    // Work A's host-A intent still drains — on its own adapter, under
    // its original binding, while the installation default is host B.
    fixture_a.apply_outbox(&store, &config_b, NOW).unwrap();
    assert_eq!(
        intent(&store, &push_a2).status,
        HostActionStatus::Applied,
        "host A's adapter drains what its own outbox queued"
    );
    assert_eq!(
        fixture_a.remote_head(&repo_a, "work/follow-up"),
        Some("head-a-2".to_string())
    );

    // The unresolved effect still reconciles — under its original
    // (host A, repo A) binding, even while the installation default is
    // host B.
    let change_a = ChangeRequestRef {
        repository: repo_a.clone(),
        change_request: ChangeRequestId::trusted("cr-a"),
        display_number: Some(1),
    };
    fixture_a
        .emit(
            &store,
            &config_b,
            Observation {
                delivery: None,
                subject_work_id: Some(work_a.clone()),
                subject_execution_id: None,
                fact: ObservationFact::HostActionResult {
                    action_id: open_a.clone(),
                    authenticated_actor: Some(pub_a.clone()),
                    status: HostActionStatus::Applied,
                    effects: vec![NormalizedEffectFact::ChangeOpened {
                        change: change_a,
                        head: rev_on(&repo_a, "head-a-1"),
                        source_branch: branch_a.clone(),
                        target_branch: "main".into(),
                    }],
                },
                observed_unix_seconds: NOW,
            },
            NOW,
        )
        .unwrap();
    let resolved = intent(&store, &open_a);
    assert_eq!(resolved.status, HostActionStatus::Applied);
    assert_eq!(
        resolved.repository, repo_a,
        "reconciliation keeps the original repository binding"
    );
    match &resolved.effects[..] {
        [NormalizedEffectFact::ChangeOpened { change, .. }] => {
            assert_eq!(change.repository, repo_a);
        }
        other => panic!("reconciled receipt missing binding: {other:?}"),
    }
}
