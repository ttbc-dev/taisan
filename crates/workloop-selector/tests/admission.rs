//! Contract tests for the proposal → submit translation (U6.2): the
//! only path a proposal takes toward admission is an ordinary
//! `submit` CommandRequest through `handle_command`.
//!
//! These are contract results over the durable store, not live
//! evidence: no host, worker, or network is exercised.

use std::collections::BTreeSet;

use workloop_core::command::*;
use workloop_core::domain::*;
use workloop_core::identity::*;
use workloop_core::observation::{IssueState, Observation, ObservationFact};
use workloop_core::store::*;
use workloop_core::work::{HostTemplate, WorkloopConfig, handle_command, handle_observation};
use workloop_selector::admission::{SELECTOR_ORIGIN, replay_key, submit_command};
use workloop_selector::{
    IssueFact, IssueState as SelectorIssueState, Proposal, SelectionPolicy, SelectorSnapshot,
    SkipReason, propose,
};
use workloop_store::sqlite::SqliteStore;

const NOW: i64 = 1_700_000_000;

fn host() -> HostInstanceId {
    HostInstanceId::trusted("test-host")
}

fn repo(id: &str) -> RepositoryRef {
    RepositoryRef::new(host(), RepositoryId::trusted(id))
}

fn issue(number: &str) -> IssueRef {
    IssueRef {
        repository: repo("local-repo"),
        issue: IssueId::trusted(number),
        display_number: Some(number.parse().unwrap()),
    }
}

fn publisher() -> HostActorRef {
    HostActorRef {
        host: host(),
        actor: HostActorId::trusted("pub-agent"),
    }
}

fn actor() -> Actor {
    Actor {
        kind: ActorKind::Human,
        external_id: "op".into(),
    }
}

fn config() -> WorkloopConfig {
    WorkloopConfig {
        host: HostTemplate {
            adapter: AdapterId::trusted("fixture"),
            repository: repo("local-repo"),
            contract_version: 1,
            actors: vec![RoleBinding {
                role: ActorRole::Publication,
                actor: publisher(),
                credential_ref: "cred-publication".into(),
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

fn fact(issue: IssueRef, revision: &str, digest: &str, title: &str) -> IssueFact {
    IssueFact {
        issue,
        state: SelectorIssueState::Open,
        revision: revision.into(),
        content_digest: digest.into(),
        title: title.into(),
    }
}

fn policy(repos: &[&str]) -> SelectionPolicy {
    SelectionPolicy {
        revision: 7,
        allowlist: repos.iter().map(|r| repo(r)).collect(),
        excluded: BTreeSet::new(),
        max_proposals: 10,
        task_constraints: BTreeSet::from(["keep the diff minimal".to_string()]),
        verification_argv: vec!["cargo".into(), "test".into()],
        required_permissions: BTreeSet::from(["workspace_write".to_string()]),
    }
}

fn selector() -> AdapterId {
    AdapterId::trusted("reference-selector")
}

fn one_proposal(f: IssueFact, p: &SelectionPolicy) -> Proposal {
    let set = propose(
        &selector(),
        &SelectorSnapshot {
            issues: vec![f],
            ..SelectorSnapshot::default()
        },
        p,
    );
    assert!(
        set.skipped.is_empty(),
        "expected a proposal: {:?}",
        set.skipped
    );
    let [proposal] = &set.proposals[..] else {
        panic!("expected exactly one proposal")
    };
    proposal.clone()
}

fn manual_submit(key: &str, source: &IssueRef, revision: &str, digest: &str) -> CommandRequest {
    CommandRequest {
        actor: actor(),
        origin: Origin("cli".into()),
        idempotency_key: key.into(),
        body: CommandBody::Submit {
            intent: "manual".into(),
            sources: vec![SourceSnapshot {
                issue: source.clone(),
                source_revision: revision.into(),
                content_digest: digest.into(),
            }],
            constraints: vec![],
            verification_argv: vec![],
        },
    }
}

fn work_of(store: &SqliteStore, work_id: &WorkId) -> Work {
    store
        .transact(|tx| Ok(tx.work(work_id)?.expect("work recorded")))
        .unwrap()
}

/// The translation produces an ordinary submit request: proposal
/// identity becomes a deterministic, namespaced replay key; source
/// bindings land verbatim in `SourceSnapshot`; the task's verification
/// argv lands as exactly one spec entry.
#[test]
fn translation_preserves_proposal_fields_and_replay_identity() {
    let proposal = one_proposal(
        fact(issue("42"), "rev-1", "digest-1", "fix the flaky test"),
        &policy(&["local-repo"]),
    );
    let req = submit_command(&proposal, actor());

    assert_eq!(req.actor, actor());
    assert_eq!(req.origin, Origin(SELECTOR_ORIGIN.into()));
    assert_eq!(
        req.idempotency_key,
        format!("proposal/{}:{}", proposal.selector, proposal.key)
    );
    // Identical proposals translate to identical replay keys; new
    // semantic content produces a new identity — never a replay of the
    // old decision.
    assert_eq!(replay_key(&proposal), req.idempotency_key);
    let other = one_proposal(
        fact(issue("42"), "rev-2", "digest-2", "fix the flaky test"),
        &policy(&["local-repo"]),
    );
    assert_ne!(replay_key(&other), req.idempotency_key);

    let CommandBody::Submit {
        intent,
        sources,
        constraints,
        verification_argv,
    } = &req.body
    else {
        panic!("expected submit: {req:?}")
    };
    assert_eq!(intent, "fix the flaky test");
    assert_eq!(
        sources[..],
        [SourceSnapshot {
            issue: issue("42"),
            source_revision: "rev-1".into(),
            content_digest: "digest-1".into(),
        }]
    );
    assert_eq!(constraints[..], ["keep the diff minimal".to_string()]);
    assert_eq!(
        verification_argv[..],
        [vec!["cargo".to_string(), "test".to_string()]],
        "one task argv lands as exactly one verification command"
    );
}

/// Re-running the same proposal replays the saved admission decision —
/// no second Work, no new acceptance.
#[test]
fn resubmitting_the_same_proposal_replays_the_saved_decision() {
    let store = SqliteStore::in_memory(1).unwrap();
    let proposal = one_proposal(
        fact(issue("7"), "rev-1", "digest-1", "add the guard"),
        &policy(&["local-repo"]),
    );
    let req = submit_command(&proposal, actor());
    // The translator is deterministic: a fresh call yields the same
    // request identity.
    let req2 = submit_command(&proposal, actor());
    assert_eq!(req.idempotency_key, req2.idempotency_key);

    let first = handle_command(&store, &config(), &req, NOW).unwrap();
    let second = handle_command(&store, &config(), &req2, NOW + 60).unwrap();
    match (&first, &second) {
        (
            CommandDecision::Accepted {
                work_id: w1,
                command_id: c1,
                ..
            },
            CommandDecision::Accepted {
                work_id: w2,
                command_id: c2,
                ..
            },
        ) => {
            assert_eq!(w1, w2, "replay returns the same Work identity");
            assert_eq!(c1, c2, "replay returns the recorded command");
        }
        other => panic!("expected two accepted replays: {other:?}"),
    }
    let works = store.transact(|tx| Ok(tx.works()?.len())).unwrap();
    assert_eq!(works, 1, "one proposal creates exactly one Work");
}

/// A selector-driven submission claiming a source Issue already owned
/// by live Work gets Core's existing overlap rejection — the proposal
/// path cannot bypass admission.
#[test]
fn a_proposal_for_a_claimed_issue_is_rejected_as_overlap() {
    let store = SqliteStore::in_memory(1).unwrap();
    let claimed = issue("9");
    let manual = manual_submit("m-1", &claimed, "rev-1", "digest-1");
    match handle_command(&store, &config(), &manual, NOW).unwrap() {
        CommandDecision::Accepted { .. } => {}
        other => panic!("manual submit should accept: {other:?}"),
    }

    let proposal = one_proposal(
        fact(claimed, "rev-1", "digest-1", "second owner"),
        &policy(&["local-repo"]),
    );
    let decision = handle_command(
        &store,
        &config(),
        &submit_command(&proposal, actor()),
        NOW + 1,
    )
    .unwrap();
    match decision {
        CommandDecision::Rejected { reason, .. } => {
            assert_eq!(reason, RejectionCode::Ineligible);
        }
        other => panic!("overlap must be rejected, not accepted: {other:?}"),
    }
}

/// An installation that does not declare `issue_read` rejects a
/// source-bound proposal submission with Core's capability decision —
/// the translation cannot declare support it does not have.
#[test]
fn a_source_bound_proposal_needs_the_declared_capability() {
    let store = SqliteStore::in_memory(1).unwrap();
    let mut config = config();
    config
        .declared_capabilities
        .remove(&HostCapability::IssueRead);
    let proposal = one_proposal(
        fact(issue("5"), "rev-1", "digest-1", "needs source read"),
        &policy(&["local-repo"]),
    );
    let decision =
        handle_command(&store, &config, &submit_command(&proposal, actor()), NOW).unwrap();
    match decision {
        CommandDecision::Rejected { reason, .. } => {
            assert_eq!(reason, RejectionCode::UnsupportedCapability);
        }
        other => panic!("undeclared capability must reject: {other:?}"),
    }
}

/// A stale proposal is not laundered by translation: the recorded
/// snapshot is the freshness evidence, and a conflicting authoritative
/// read flips the source gate to `Changed` and pauses the queued
/// Execution for spec reapproval.
#[test]
fn a_stale_source_read_holds_the_work_for_reapproval() {
    let store = SqliteStore::in_memory(1).unwrap();
    let proposal = one_proposal(
        fact(issue("11"), "rev-old", "digest-old", "stale proposal"),
        &policy(&["local-repo"]),
    );
    let decision =
        handle_command(&store, &config(), &submit_command(&proposal, actor()), NOW).unwrap();
    let CommandDecision::Accepted {
        work_id: Some(work_id),
        ..
    } = decision
    else {
        panic!("submit should accept: {decision:?}")
    };
    assert_eq!(
        work_of(&store, &work_id).source_gate,
        SourceGate::NeedsCheck
    );

    // The authoritative re-read sees changed content: the gate flips
    // and the queued execution holds for a new spec revision.
    handle_observation(
        &store,
        &config(),
        &Observation {
            delivery: None,
            subject_work_id: Some(work_id.clone()),
            subject_execution_id: None,
            fact: ObservationFact::Issue {
                issue: issue("11"),
                source_revision: "rev-new".into(),
                content_digest: "digest-new".into(),
                state: IssueState::Open,
            },
            observed_unix_seconds: NOW + 10,
        },
        NOW + 10,
    )
    .unwrap();

    let work = work_of(&store, &work_id);
    assert_eq!(work.source_gate, SourceGate::Changed);
    let execution = store
        .transact(|tx| Ok(tx.active_execution(&work_id)?.expect("execution recorded")))
        .unwrap();
    assert_eq!(execution.state, ExecutionState::Paused);
    assert!(
        execution
            .repair_prerequisites
            .contains(&RepairPrerequisite::SpecReapproval),
        "the hold names spec reapproval: {:?}",
        execution.repair_prerequisites
    );
}

/// Out-of-allowlist facts never reach admission: `propose` refuses them
/// as explicit data, so there is nothing to translate. Combined with
/// the overlap and capability tests, the selector path cannot bypass
/// allowlist, admission, or idempotency.
#[test]
fn an_unallowlisted_source_is_refused_before_any_command() {
    let set = propose(
        &selector(),
        &SelectorSnapshot {
            issues: vec![fact(issue("13"), "rev-1", "digest-1", "foreign repository")],
            ..SelectorSnapshot::default()
        },
        &policy(&["other-repo"]),
    );
    assert!(set.proposals.is_empty());
    assert_eq!(
        set.skipped[..],
        [workloop_selector::SkippedIssue {
            issue: issue("13"),
            reason: SkipReason::NotAllowlisted,
        }]
    );
}
