//! Source-refresh and bounded-outbox contract for the loop's phase 1
//! and phase 4 drivers: heads are read (never invented), a movement is
//! ingested once, an unchanged head is a no-op, and the outbox stops at
//! the per-tick bound reporting the untouched remainder.

use std::collections::BTreeSet;

use workloop_core::command::*;
use workloop_core::domain::*;
use workloop_core::identity::*;
use workloop_core::observation::*;
use workloop_core::store::*;
use workloop_core::work::{
    HostTemplate, WorkloopConfig, admit_queued, dispatch_once, handle_command,
};
use workloop_github::fixture::FixtureGithub;
use workloop_store::sqlite::SqliteStore;

const NOW: i64 = 1_700_000_000;

fn host() -> HostInstanceId {
    HostInstanceId::trusted("test-host")
}

fn repo() -> RepositoryRef {
    RepositoryRef::new(host(), RepositoryId::trusted("local-repo"))
}

fn publisher() -> HostActorRef {
    HostActorRef {
        host: host(),
        actor: HostActorId::trusted("pub-agent"),
    }
}

fn rev(object: &str) -> CommitRevision {
    CommitRevision::new(repo(), object).unwrap()
}

fn change_ref(id: &str) -> ChangeRequestRef {
    ChangeRequestRef {
        repository: repo(),
        change_request: ChangeRequestId::trusted(id),
        display_number: None,
    }
}

fn config() -> WorkloopConfig {
    WorkloopConfig {
        host: HostTemplate {
            adapter: AdapterId::trusted("fixture"),
            repository: repo(),
            contract_version: 1,
            actors: vec![RoleBinding {
                role: ActorRole::Publication,
                actor: publisher(),
                credential_ref: CredentialRef::trusted("cred"),
            }],
        },
        declared_capabilities: BTreeSet::from([
            HostCapability::RepositoryRead,
            HostCapability::PushRef,
            HostCapability::ChangeRequestCreate,
            HostCapability::ChangeRequestRead,
            HostCapability::ChecksRead,
            HostCapability::ReviewFindingsRead,
        ]),
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

fn submit(store: &SqliteStore, key: &str) -> (WorkId, ExecutionId) {
    let d = handle_command(
        store,
        &config(),
        &CommandRequest {
            actor: Actor {
                kind: ActorKind::Human,
                external_id: "op".into(),
            },
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

fn admit(store: &SqliteStore, execution_id: &ExecutionId) {
    store
        .transact(|tx| admit_queued(tx, &config(), execution_id, NOW).map(|_| ()))
        .unwrap();
}

fn issue(number: u64) -> IssueRef {
    IssueRef {
        repository: repo(),
        issue: IssueId::trusted(format!("issue-{number}")),
        display_number: Some(number),
    }
}

fn source(number: u64, revision: &str, digest: &str) -> SourceSnapshot {
    SourceSnapshot {
        issue: issue(number),
        source_revision: revision.into(),
        content_digest: digest.into(),
    }
}

fn config_with_issue_read() -> WorkloopConfig {
    let mut c = config();
    c.declared_capabilities.insert(HostCapability::IssueRead);
    c
}

fn submit_with_sources(
    store: &SqliteStore,
    key: &str,
    sources: Vec<SourceSnapshot>,
) -> (WorkId, ExecutionId) {
    let d = handle_command(
        store,
        &config_with_issue_read(),
        &CommandRequest {
            actor: Actor {
                kind: ActorKind::Human,
                external_id: "op".into(),
            },
            origin: Origin("cli".into()),
            idempotency_key: key.into(),
            body: CommandBody::Submit {
                intent: format!("intent-{key}"),
                sources,
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

fn gate_of(store: &SqliteStore, work_id: &WorkId) -> SourceGate {
    store
        .transact(|tx| Ok(tx.work(work_id)?.unwrap().source_gate))
        .unwrap()
}

fn execution_of(store: &SqliteStore, execution_id: &ExecutionId) -> Execution {
    store
        .transact(|tx| tx.execution(execution_id))
        .unwrap()
        .unwrap()
}

fn issue_observations(store: &SqliteStore, work_id: &WorkId) -> Vec<(String, String, IssueState)> {
    store
        .transact(|tx| tx.observations_for_work(work_id))
        .unwrap()
        .into_iter()
        .filter_map(|o| match o.fact {
            ObservationFact::Issue {
                source_revision,
                content_digest,
                state,
                ..
            } => Some((source_revision, content_digest, state)),
            _ => None,
        })
        .collect()
}

fn queue_intent(store: &SqliteStore, work_id: &WorkId, action: HostAction) {
    store
        .transact(|tx| {
            let action_id = tx.alloc_id("act")?;
            let created_seq = tx.alloc_seq()?;
            tx.apply(&Effect::InsertHostActionIntent {
                intent: HostActionIntent {
                    action_id,
                    work_id: work_id.clone(),
                    host: host(),
                    repository: repo(),
                    role: ActorRole::Publication,
                    actor: publisher(),
                    action,
                    payload_digest: "digest".into(),
                    status: HostActionStatus::Pending,
                    authenticated_actor: None,
                    effects: vec![],
                    created_seq,
                },
            })?;
            Ok(())
        })
        .unwrap();
}

fn queue_open_change(store: &SqliteStore, work_id: &WorkId) {
    queue_intent(
        store,
        work_id,
        HostAction::OpenChangeRequest {
            source_branch: format!("work/{}", work_id.0),
            target_branch: "main".into(),
            title: "t".into(),
            body: String::new(),
        },
    );
}

fn change_head_observations(store: &SqliteStore, work_id: &WorkId) -> Vec<String> {
    store
        .transact(|tx| tx.observations_for_work(work_id))
        .unwrap()
        .into_iter()
        .filter_map(|o| match o.fact {
            ObservationFact::ChangeHead { head, .. } => Some(head.object),
            _ => None,
        })
        .collect()
}

/// Two admitted Works, each with one applied change request.
fn two_changes() -> (SqliteStore, FixtureGithub, WorkId, WorkId) {
    let store = SqliteStore::in_memory(4).unwrap();
    let (w1, e1) = submit(&store, "k-1");
    let (w2, e2) = submit(&store, "k-2");
    admit(&store, &e1);
    admit(&store, &e2);
    queue_open_change(&store, &w1);
    queue_open_change(&store, &w2);
    let mut gh = FixtureGithub::new();
    gh.apply_outbox(&store, &config(), NOW).unwrap();
    (store, gh, w1, w2)
}

#[test]
fn refresh_ingests_a_moved_head_once_and_a_matching_head_is_a_noop() {
    let (store, mut gh, w1, _) = two_changes();
    let before = change_head_observations(&store, &w1);
    assert_eq!(before.len(), 1, "the outbox recorded the opening head");

    // The synthetic remote moves; the read-back ingests exactly one
    // new observation under the Work that owns the change.
    gh.set_head("cr-1", "sha-moved");
    let report = gh
        .refresh_sources(&store, &config(), &mut || NOW, None, None)
        .unwrap();
    assert_eq!(report.ingested, 1);
    assert_eq!(report.deferred, Vec::<String>::new());
    let after = change_head_observations(&store, &w1);
    assert_eq!(after.len(), 2);
    assert_eq!(after.last().unwrap(), "sha-moved");

    // A re-read with no movement ingests nothing — a restart re-entering
    // phase order observes the same facts.
    let again = gh
        .refresh_sources(&store, &config(), &mut || NOW + 1, None, None)
        .unwrap();
    assert_eq!(again.ingested, 0);
    assert_eq!(again.unchanged, 2, "both known changes match their records");
    assert_eq!(change_head_observations(&store, &w1).len(), 2);
}

/// A declared source with no authoritative read yet defers admission;
/// the first matching read flips the gate and dispatch proceeds — the
/// whole "submit → refresh → admit" wiring in one pass.
#[test]
fn a_matching_source_read_unblocks_dispatch() {
    let store = SqliteStore::in_memory(1).unwrap();
    let (w, e) = submit_with_sources(&store, "k-src", vec![source(9, "rev-1", "d-1")]);
    assert_eq!(gate_of(&store, &w), SourceGate::NeedsCheck);
    // NeedsCheck: dispatch owes a source read and records the deferral.
    store
        .transact(|tx| dispatch_once(tx, &config_with_issue_read(), NOW).map(|_| ()))
        .unwrap();
    assert_eq!(execution_of(&store, &e).state, ExecutionState::Queued);

    let mut gh = FixtureGithub::new();
    gh.set_issue(&issue(9), "rev-1", "d-1", IssueState::Open);
    let report = gh
        .refresh_sources(&store, &config(), &mut || NOW, None, None)
        .unwrap();
    assert_eq!(report.ingested, 1);
    assert_eq!(gate_of(&store, &w), SourceGate::Matches);
    store
        .transact(|tx| dispatch_once(tx, &config_with_issue_read(), NOW).map(|_| ()))
        .unwrap();
    assert_eq!(
        execution_of(&store, &e).state,
        ExecutionState::Ready,
        "the matched source read unblocks admission"
    );
}

/// A Work admitted through the selector proposal path — its
/// `SourceSnapshot` bound by `submit_command` — crosses the source gate
/// when the authoritative re-read recomputes the same canonical digest
/// at the same revision, and still holds when the read differs. Both
/// sides of the gate share `issue_content_digest`, and admission
/// canonicalizes the host instance, so a selector-bound identity and an
/// installation-bound identity for the same Issue coincide.
#[test]
fn a_selector_admitted_work_crosses_the_gate_on_a_matching_read() {
    let store = SqliteStore::in_memory(1).unwrap();
    let issue = IssueRef {
        repository: RepositoryRef::new(
            HostInstanceId::trusted("github.com"),
            RepositoryId::trusted("owner/repo"),
        ),
        issue: IssueId::trusted("45"),
        display_number: Some(45),
    };
    let digest = workloop_selector::issue_content_digest("a title", "a body");
    let proposal = workloop_selector::Proposal {
        key: "prop-1".into(),
        selector: AdapterId::trusted("public-issue-selector"),
        selector_version: 1,
        policy_revision: 1,
        sources: vec![workloop_selector::SourceBinding {
            issue: issue.clone(),
            revision: "rev-1".into(),
            content_digest: digest.clone(),
        }],
        task: workloop_selector::TaskInput {
            intent: "selector task".into(),
            constraints: vec![],
            verification_argv: vec![],
        },
        required_permissions: vec![],
        provenance: workloop_selector::Provenance::default(),
    };
    let request = workloop_selector::admission::submit_command(
        &proposal,
        Actor {
            kind: ActorKind::Human,
            external_id: "op".into(),
        },
    );
    let d = handle_command(&store, &config_with_issue_read(), &request, NOW).unwrap();
    let CommandDecision::Accepted {
        work_id: Some(w),
        effect: CommandEffect::WorkCreated { execution_id: e },
        ..
    } = d
    else {
        panic!("selector submit: {d:?}")
    };

    // The bridge-side re-read computes the same canonical digest for an
    // unchanged Issue: the gate matches and dispatch proceeds.
    let mut gh = FixtureGithub::new();
    gh.set_issue(&issue, "rev-1", &digest, IssueState::Open);
    gh.refresh_sources(&store, &config(), &mut || NOW, None, None)
        .unwrap();
    assert_eq!(gate_of(&store, &w), SourceGate::Matches);
    store
        .transact(|tx| dispatch_once(tx, &config_with_issue_read(), NOW).map(|_| ()))
        .unwrap();
    assert_eq!(execution_of(&store, &e).state, ExecutionState::Ready);

    // A genuinely changed read still fails closed.
    gh.set_issue(&issue, "rev-2", "changed-digest", IssueState::Open);
    gh.refresh_sources(&store, &config(), &mut || NOW + 1, None, None)
        .unwrap();
    assert_eq!(gate_of(&store, &w), SourceGate::Changed);
}

/// Endpoint and bare-host spellings of one host instance share a single
/// source claim: admission canonicalizes the host, so a selector-style
/// `https://api.github.com` source and an installation-style
/// `github.com` source for the same Issue collide instead of producing
/// two Works.
#[test]
fn endpoint_and_bare_host_spellings_share_one_source_claim() {
    let store = SqliteStore::in_memory(4).unwrap();
    let make = |host: &str| SourceSnapshot {
        issue: IssueRef {
            repository: RepositoryRef::new(
                HostInstanceId::trusted(host),
                RepositoryId::trusted("owner/repo"),
            ),
            issue: IssueId::trusted("45"),
            display_number: Some(45),
        },
        source_revision: "rev-1".into(),
        content_digest: "d".into(),
    };
    submit_with_sources(&store, "k-ep", vec![make("https://api.github.com")]);

    let d = handle_command(
        &store,
        &config_with_issue_read(),
        &CommandRequest {
            actor: Actor {
                kind: ActorKind::Human,
                external_id: "op".into(),
            },
            origin: Origin("cli".into()),
            idempotency_key: "k-bare".into(),
            body: CommandBody::Submit {
                intent: "bare-host submit".into(),
                sources: vec![make("github.com")],
                constraints: vec![],
                verification_argv: vec![],
            },
        },
        NOW,
    )
    .unwrap();
    match d {
        CommandDecision::Rejected { reason, .. } => {
            assert_eq!(reason, RejectionCode::Ineligible);
        }
        other => panic!("expected Ineligible rejection, got {other:?}"),
    }
}

/// A changed read holds dispatch: the queued Execution pauses for
/// reapproval instead of being admitted on a stale spec.
#[test]
fn a_changed_source_read_holds_dispatch() {
    let store = SqliteStore::in_memory(1).unwrap();
    let (w, e) = submit_with_sources(&store, "k-changed", vec![source(9, "rev-1", "d-1")]);
    let mut gh = FixtureGithub::new();
    gh.set_issue(&issue(9), "rev-2", "d-2", IssueState::Open);
    let report = gh
        .refresh_sources(&store, &config(), &mut || NOW, None, None)
        .unwrap();
    assert_eq!(report.ingested, 1);
    assert_eq!(gate_of(&store, &w), SourceGate::Changed);
    store
        .transact(|tx| dispatch_once(tx, &config_with_issue_read(), NOW).map(|_| ()))
        .unwrap();
    let execution = execution_of(&store, &e);
    assert_eq!(
        execution.state,
        ExecutionState::Paused,
        "a changed source holds the queued Execution for repair, never admits it"
    );
    assert_eq!(
        execution.repair_prerequisites,
        vec![RepairPrerequisite::SpecReapproval]
    );
}

/// A read the host cannot complete is `unavailable`, never "unchanged":
/// the queued Execution holds for a fresh matching read.
#[test]
fn an_unavailable_source_read_holds_dispatch() {
    let store = SqliteStore::in_memory(1).unwrap();
    let (w, e) = submit_with_sources(&store, "k-unavail", vec![source(9, "rev-1", "d-1")]);
    // No fixture row for the declared issue: the remote cannot answer.
    let gh = FixtureGithub::new();
    let report = gh
        .refresh_sources(&store, &config(), &mut || NOW, None, None)
        .unwrap();
    assert_eq!(report.ingested, 1, "the failed read is recorded");
    assert_eq!(
        issue_observations(&store, &w).last(),
        Some(&("".to_string(), "".to_string(), IssueState::Unavailable)),
        "unavailable is a recorded read, not an absence"
    );
    assert_eq!(gate_of(&store, &w), SourceGate::Unavailable);
    store
        .transact(|tx| dispatch_once(tx, &config_with_issue_read(), NOW).map(|_| ()))
        .unwrap();
    let execution = execution_of(&store, &e);
    assert_eq!(execution.state, ExecutionState::Paused);
    assert_eq!(
        execution.repair_prerequisites,
        vec![RepairPrerequisite::SourceRead]
    );
}

/// A closed source is a changed source: the snapshot it was approved
/// under no longer describes the remote.
#[test]
fn a_closed_source_counts_as_changed() {
    let store = SqliteStore::in_memory(1).unwrap();
    let (w, e) = submit_with_sources(&store, "k-closed", vec![source(9, "rev-1", "d-1")]);
    let mut gh = FixtureGithub::new();
    gh.set_issue(&issue(9), "rev-1", "d-1", IssueState::Closed);
    gh.refresh_sources(&store, &config(), &mut || NOW, None, None)
        .unwrap();
    assert_eq!(gate_of(&store, &w), SourceGate::Changed);
    assert_eq!(execution_of(&store, &e).state, ExecutionState::Paused);
}

/// The read-compare makes refresh idempotent: a re-read that matches the
/// recorded fact ingests nothing, so a restart re-entering phase order
/// is a no-op.
#[test]
fn an_identical_source_reread_is_a_noop() {
    let store = SqliteStore::in_memory(1).unwrap();
    let (w, _e) = submit_with_sources(&store, "k-idem", vec![source(9, "rev-1", "d-1")]);
    let mut gh = FixtureGithub::new();
    gh.set_issue(&issue(9), "rev-1", "d-1", IssueState::Open);
    gh.refresh_sources(&store, &config(), &mut || NOW, None, None)
        .unwrap();
    let again = gh
        .refresh_sources(&store, &config(), &mut || NOW + 1, None, None)
        .unwrap();
    assert_eq!(again.ingested, 0);
    assert_eq!(again.unchanged, 1);
    assert_eq!(
        issue_observations(&store, &w).len(),
        1,
        "one recorded read, not one per pass"
    );
}

/// Reads are scoped and deadline-bounded exactly like head reads.
#[test]
fn source_reads_are_scoped_and_deadline_bounded() {
    let store = SqliteStore::in_memory(4).unwrap();
    let (w1, _e1) = submit_with_sources(&store, "k-s1", vec![source(9, "rev-1", "d-1")]);
    let (w2, _e2) = submit_with_sources(&store, "k-s2", vec![source(10, "rev-a", "d-a")]);
    let mut gh = FixtureGithub::new();
    gh.set_issue(&issue(9), "rev-1", "d-1", IssueState::Open);
    gh.set_issue(&issue(10), "rev-a", "d-a", IssueState::Open);

    let scoped = gh
        .refresh_sources(&store, &config(), &mut || NOW, None, Some(&w1))
        .unwrap();
    assert_eq!(scoped.ingested, 1);
    assert_eq!(gate_of(&store, &w1), SourceGate::Matches);
    assert_eq!(
        gate_of(&store, &w2),
        SourceGate::NeedsCheck,
        "the out-of-scope Work's source was never read"
    );

    let late = gh
        .refresh_sources(&store, &config(), &mut || NOW, Some(NOW), None)
        .unwrap();
    assert_eq!(late.ingested, 0);
    assert_eq!(
        late.deferred,
        vec![issue(9).to_string(), issue(10).to_string()],
        "both unread sources wait for the next pass, in order"
    );
}

/// Only declared sources are read — remote activity nothing declared
/// is never ingested (R234).
#[test]
fn undeclared_remote_activity_is_never_ingested() {
    let store = SqliteStore::in_memory(1).unwrap();
    let (w, _e) = submit_with_sources(&store, "k-decl", vec![source(9, "rev-1", "d-1")]);
    let mut gh = FixtureGithub::new();
    gh.set_issue(&issue(9), "rev-1", "d-1", IssueState::Open);
    // A source no Work declares: it must not be read into this
    // installation's facts.
    gh.set_issue(&issue(99), "rev-x", "d-x", IssueState::Open);
    let report = gh
        .refresh_sources(&store, &config(), &mut || NOW, None, None)
        .unwrap();
    assert_eq!(report.ingested, 1, "only the declared source was read");
    assert_eq!(issue_observations(&store, &w).len(), 1);
}

#[test]
fn refresh_is_scoped_to_one_work() {
    let (store, mut gh, w1, w2) = two_changes();
    gh.set_head("cr-1", "sha-1");
    gh.set_head("cr-2", "sha-2");

    let report = gh
        .refresh_sources(&store, &config(), &mut || NOW, None, Some(&w1))
        .unwrap();
    assert_eq!(report.ingested, 1, "only the scoped Work's head moved here");
    assert_eq!(change_head_observations(&store, &w1).len(), 2);
    assert_eq!(
        change_head_observations(&store, &w2).len(),
        1,
        "the other Work's head was not read in this pass"
    );
}

/// Every change-bearing link attributes the change to its Work —
/// a recorded `ReviewPublished`/`ReplyPosted`/`ThreadResolved`/
/// `ReadyObserved` receipt or a pending intent carrying the change
/// counts, so a scoped refresh re-reads the head of a change the Work
/// touched through discussion/review effects alone.
#[test]
fn discussion_and_ready_links_attribute_their_change_to_the_work() {
    let store = SqliteStore::in_memory(4).unwrap();
    let (w, e) = submit(&store, "k-attr");
    admit(&store, &e);

    let mut gh = FixtureGithub::new();
    gh.set_change_head(&change_ref("cr-1"), "head-1", &publisher());
    gh.add_thread(&change_ref("cr-1"), "th-1");

    // The Work's only recorded links to cr-1 are the discussion/review
    // intents and their receipts — no UpdateChangeRequest intent and no
    // ChangeOpened/MetadataUpdated/CommentPosted effect anywhere.
    for action in [
        HostAction::PublishReviewFeedback {
            change: change_ref("cr-1"),
            head: rev("head-1"),
            body: "feedback".into(),
        },
        HostAction::ReplyToThread {
            change: change_ref("cr-1"),
            thread_id: "th-1".into(),
            marker: "m".into(),
            body: "reply".into(),
        },
        HostAction::ResolveThread {
            change: change_ref("cr-1"),
            thread_id: "th-1".into(),
            head: rev("head-1"),
        },
        HostAction::MarkReady {
            change: change_ref("cr-1"),
            expected_head: rev("head-1"),
        },
    ] {
        queue_intent(&store, &w, action);
    }
    gh.apply_outbox(&store, &config(), NOW).unwrap();
    assert!(
        store
            .transact(|tx| tx.action_intents())
            .unwrap()
            .iter()
            .all(|i| i.status == HostActionStatus::Applied),
        "every discussion/review effect applied"
    );

    // The remote moves; a Work-scoped refresh must re-read the head of
    // the change the Work's receipts recorded.
    gh.set_head("cr-1", "head-2");
    let report = gh
        .refresh_sources(&store, &config(), &mut || NOW, None, Some(&w))
        .unwrap();
    assert_eq!(
        report.ingested, 1,
        "the receipt-owned change's head moved: one read ingested"
    );
    assert_eq!(
        change_head_observations(&store, &w).last().unwrap(),
        "head-2"
    );
}

#[test]
fn refresh_defers_remaining_sources_at_the_deadline() {
    let (store, mut gh, w1, _) = two_changes();
    gh.set_head("cr-1", "sha-moved");
    // Deadline already reached: the source is reported, never read.
    let report = gh
        .refresh_sources(&store, &config(), &mut || NOW, Some(NOW), None)
        .unwrap();
    assert_eq!(report.ingested, 0);
    assert_eq!(
        report.deferred,
        vec![
            "cr-1".to_string(),
            "cr-2".to_string(),
            "merge-check:cr-1".to_string(),
            "merge-check:cr-2".to_string()
        ]
    );
    assert_eq!(
        change_head_observations(&store, &w1).len(),
        1,
        "a deferred source is handled next pass, never half-read"
    );
}

#[test]
fn the_outbox_stops_at_the_tick_bound_and_reports_the_remainder() {
    let store = SqliteStore::in_memory(4).unwrap();
    let mut gh = FixtureGithub::new();
    let mut works = Vec::new();
    for i in 0..3 {
        let (w, e) = submit(&store, &format!("k-{i}"));
        admit(&store, &e);
        queue_open_change(&store, &w);
        works.push(w);
    }

    let first = gh
        .apply_outbox_bounded(&store, &config(), &mut || NOW, None, None, 2)
        .unwrap();
    assert_eq!(first.attempted, 2);
    assert_eq!(first.applied, 2);
    assert_eq!(first.deferred.len(), 1, "the third intent waits");
    assert_eq!(gh.applied().len(), 2);

    let second = gh
        .apply_outbox_bounded(&store, &config(), &mut || NOW, None, None, 2)
        .unwrap();
    assert_eq!(second.attempted, 1);
    assert!(second.deferred.is_empty());
    assert_eq!(gh.applied().len(), 3, "every intent eventually applied");

    // All three heads are recorded now.
    for w in &works {
        assert_eq!(change_head_observations(&store, w).len(), 1);
    }
}

#[test]
fn permanently_pending_intents_do_not_starve_later_ready_intents() {
    let store = SqliteStore::in_memory(2).unwrap();
    let mut gh = FixtureGithub::new();
    let (w, e) = submit(&store, "k-stuck");
    admit(&store, &e);
    // These Works have no Execution, so their intents can never leave
    // Pending. Eight of them are exactly one tick's worth of attempts:
    // they must not consume the budget and starve the ready intent.
    for i in 0..8 {
        queue_open_change(&store, &WorkId(format!("missing-{i}")));
    }
    queue_open_change(&store, &w);

    let report = gh
        .apply_outbox_bounded(&store, &config(), &mut || NOW, None, None, 8)
        .unwrap();
    assert_eq!(
        report.attempted, 1,
        "only the intent that can run is a host attempt"
    );
    assert_eq!(report.applied, 1);
    assert_eq!(
        gh.applied().len(),
        1,
        "the later ready intent is reached within the same tick"
    );
}

#[test]
fn a_tick_deadline_stops_the_outbox_drain_and_defers_the_remainder() {
    let store = SqliteStore::in_memory(4).unwrap();
    let mut gh = FixtureGithub::new();
    for i in 0..3 {
        let (w, e) = submit(&store, &format!("k-{i}"));
        admit(&store, &e);
        queue_open_change(&store, &w);
    }

    // Deadline already reached when the drain starts: the phase-entry
    // style gate — no write is attempted, every pending intent is
    // reported for the next pass.
    let early = gh
        .apply_outbox_bounded(&store, &config(), &mut || NOW, Some(NOW), None, 8)
        .unwrap();
    assert_eq!(early.attempted, 0, "a late drain starts nothing");
    assert_eq!(
        early.deferred.len(),
        3,
        "every intent is reported, none dropped"
    );
    assert!(gh.applied().is_empty(), "no host write happened");

    // Deadline passes mid-drain: the item that started runs, the rest
    // of the loop checks the deadline before starting anything new.
    let mut calls = 0u32;
    let mid = gh
        .apply_outbox_bounded(
            &store,
            &config(),
            &mut || {
                calls += 1;
                if calls <= 2 { NOW } else { NOW + 60 }
            },
            Some(NOW + 30),
            None,
            8,
        )
        .unwrap();
    assert_eq!(mid.attempted, 1, "the item started before the deadline");
    assert_eq!(
        mid.deferred.len(),
        2,
        "the remainder waits for the next pass"
    );
    assert_eq!(gh.applied().len(), 1);
}

#[test]
fn a_scoped_outbox_never_touches_another_works_intent() {
    let (store, mut gh, w1, w2) = two_changes();
    // Fresh intents on both Works; drain only w1's scope.
    queue_open_change(&store, &w1);
    queue_open_change(&store, &w2);
    let report = gh
        .apply_outbox_bounded(&store, &config(), &mut || NOW, None, Some(&w1), 8)
        .unwrap();
    assert_eq!(report.attempted, 1, "only the scoped Work's intent ran");
    assert_eq!(report.applied, 1);
    let pending: Vec<String> = store
        .transact(|tx| tx.action_intents())
        .unwrap()
        .into_iter()
        .filter(|i| i.status == HostActionStatus::Pending)
        .map(|i| i.work_id.0)
        .collect();
    assert_eq!(
        pending,
        vec![w2.0.clone()],
        "the out-of-scope intent stayed pending for an unscoped pass"
    );
}
