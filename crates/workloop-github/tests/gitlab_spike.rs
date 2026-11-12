//! U9.4 — named fixture-driven runs of the narrow GitLab spike harness.
//!
//! Denial, lost-response and restart each produce a retained evidence
//! record (a `SpikeJournal`) with per-effect receipts; every applied
//! write's receipt is reassessed by a fresh head read; live rows report
//! `not_authorized` without the recorded authorization. Fixture runs
//! support contract claims only — no live host is contacted.

use workloop_core::identity::{
    ChangeRequestId, HostActorId, HostActorRef, HostInstanceId, RepositoryId, RepositoryRef,
};
use workloop_github::gitlab_spike::*;

fn scope() -> SpikeScope {
    SpikeScope {
        host: HostInstanceId::trusted("gitlab-spike-host"),
        repository: RepositoryId::trusted("isolated-project"),
        branch_prefix: "spike/u94/".into(),
        actor: HostActorRef {
            host: HostInstanceId::trusted("gitlab-spike-host"),
            actor: HostActorId::trusted("spike-actor"),
        },
    }
}

fn repo() -> RepositoryRef {
    scope().repository()
}

fn change(id: &str) -> ChangeRequestId {
    ChangeRequestId::trusted(id)
}

fn row(id: &str, op: SpikeOp) -> SpikeRow {
    SpikeRow {
        id: id.into(),
        lane: RowLane::Fixture,
        op,
    }
}

fn live_plan() -> Vec<SpikeRow> {
    plan()
        .into_iter()
        .map(|r| SpikeRow {
            lane: RowLane::Live,
            ..r
        })
        .collect()
}

/// The narrow plan: push, open, comment — each head-bound — then a
/// guarded advance, a stale-head comment abort, and a fresh head read.
fn plan() -> Vec<SpikeRow> {
    vec![
        row(
            "r1-push",
            SpikeOp::GuardedPush {
                branch: "spike/u94/w".into(),
                expected_prior: None,
                produced: "head-1".into(),
            },
        ),
        row(
            "r2-create",
            SpikeOp::MrCreate {
                source_branch: "spike/u94/w".into(),
                target_branch: "main".into(),
            },
        ),
        row(
            "r3-comment",
            SpikeOp::CommentPost {
                change_request: change("!1"),
                source_branch: "spike/u94/w".into(),
                expected_head: "head-1".into(),
                body: "spike note".into(),
            },
        ),
        row(
            "r4-push-advance",
            SpikeOp::GuardedPush {
                branch: "spike/u94/w".into(),
                expected_prior: Some("head-1".into()),
                produced: "head-2".into(),
            },
        ),
        // Recorded against the superseded head: the write must abort.
        row(
            "r5-stale-comment",
            SpikeOp::CommentPost {
                change_request: change("!1"),
                source_branch: "spike/u94/w".into(),
                expected_head: "head-1".into(),
                body: "stale note".into(),
            },
        ),
        row(
            "r6-head-read",
            SpikeOp::HeadRead {
                target: HeadTarget::Change {
                    change_request: change("!1"),
                },
            },
        ),
    ]
}

fn record<'a>(journal: &'a SpikeJournal, row_id: &str) -> &'a RowRecord {
    journal
        .records
        .iter()
        .find(|r| r.row_id == row_id)
        .unwrap_or_else(|| panic!("row {row_id} recorded"))
}

fn applied_revision(record: &RowRecord) -> &workloop_core::identity::CommitRevision {
    match &record.outcome {
        RowOutcome::Receipt(WriteReceipt::Applied { revision, .. }) => revision,
        other => panic!("{}: expected applied receipt: {other:?}", record.row_id),
    }
}

/// Remove Rust comments while preserving string literals, so a comment
/// between the receiver and the method cannot hide a call site.
fn strip_comments(source: &str) -> String {
    let mut out = String::with_capacity(source.len());
    let mut chars = source.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' => {
                out.push(c);
                while let Some(next) = chars.next() {
                    out.push(next);
                    if next == '\\' {
                        if let Some(escaped) = chars.next() {
                            out.push(escaped);
                        }
                    } else if next == '"' {
                        break;
                    }
                }
            }
            '/' if chars.peek() == Some(&'/') => {
                chars.next();
                while let Some(&next) = chars.peek() {
                    if next == '\n' {
                        break;
                    }
                    chars.next();
                }
            }
            '/' if chars.peek() == Some(&'*') => {
                chars.next();
                let mut depth = 1usize;
                while let Some(next) = chars.next() {
                    if next == '/' && chars.peek() == Some(&'*') {
                        chars.next();
                        depth += 1;
                    } else if next == '*' && chars.peek() == Some(&'/') {
                        chars.next();
                        depth -= 1;
                        if depth == 0 {
                            break;
                        }
                    }
                }
            }
            _ => out.push(c),
        }
    }
    out
}

/// If `after` starts with a turbofish (`::<...>`), the index just past
/// its closing `>`; otherwise `None`.
fn skip_turbofish(after: &str) -> Option<usize> {
    if !after.starts_with("::<") {
        return None;
    }
    let mut depth = 0usize;
    for (index, c) in after.char_indices().skip(2) {
        match c {
            '<' => depth += 1,
            '>' => {
                depth -= 1;
                if depth == 0 {
                    return Some(index + c.len_utf8());
                }
            }
            _ => {}
        }
    }
    None
}

/// Every `self.remote.<method>(` call site in `source`, independent of
/// rustfmt's line breaks: comments are removed, whitespace is normalized
/// away, and an optional turbofish is skipped before the call markers are
/// parsed. Only a method followed by `(` counts.
fn remote_call_sites(source: &str) -> Vec<String> {
    let stripped = strip_comments(source);
    let compact: String = stripped.chars().filter(|c| !c.is_whitespace()).collect();
    let marker = "self.remote.";
    let mut sites = Vec::new();
    let mut rest = compact.as_str();
    while let Some(index) = rest.find(marker) {
        let after = &rest[index + marker.len()..];
        if after.is_empty() {
            break;
        }
        let end = after
            .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
            .unwrap_or(after.len());
        if end > 0 {
            let method = &after[..end];
            let mut tail = &after[end..];
            if let Some(skip) = skip_turbofish(tail) {
                tail = &tail[skip..];
            }
            if tail.starts_with('(') {
                sites.push(method.to_string());
            }
        }
        rest = &after[end.max(1)..];
    }
    sites
}

/// Persist a scenario's journal under the test tmpdir — the retained
/// evidence record is inspectable, and serializes for the restart path.
fn persist(scenario: &str, journal: &SpikeJournal) {
    let dir = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join("u94-spike");
    std::fs::create_dir_all(&dir).unwrap();
    let json = serde_json::to_string_pretty(journal).unwrap();
    std::fs::write(dir.join(format!("{scenario}.json")), &json).unwrap();
}

/// The named fixture-driven run for U9.4's fault cases: denial,
/// lost-response and restart each produce a retained evidence record
/// with per-effect receipts.
#[test]
fn denial_lost_response_and_restart_retain_evidence() {
    // --- Denial: the actor's permission for comment writes is denied.
    // Denial is distinct from unsupported — the operation exists, the
    // actor may not perform it; reads and other writes are unaffected.
    let mut remote = GitlabSpikeRemote::new();
    remote.deny(SpikeOpKind::CommentPost);
    let mut harness = SpikeHarness::new(scope(), &mut remote, None);
    harness.run(&plan());
    let journal = harness.into_journal();
    for id in ["r1-push", "r2-create", "r4-push-advance"] {
        assert!(
            matches!(
                record(&journal, id).outcome,
                RowOutcome::Receipt(WriteReceipt::Applied { .. })
            ),
            "{id} applies"
        );
    }
    for id in ["r3-comment", "r5-stale-comment"] {
        match &record(&journal, id).outcome {
            RowOutcome::Receipt(WriteReceipt::Denied { reason }) => {
                assert!(reason.contains("denied"), "explicit denial: {reason}");
            }
            other => panic!("{id}: denied write expected: {other:?}"),
        }
    }
    // Denied permission never blocks reads: the head read still
    // observes the remote — reconciliation survives lost mutation
    // permission.
    match &record(&journal, "r6-head-read").outcome {
        RowOutcome::Observed(obs) => {
            assert_eq!(obs.head.as_ref().map(|h| h.object.as_str()), Some("head-2"));
        }
        other => panic!("head read unaffected by write denial: {other:?}"),
    }
    assert_eq!(
        remote.applied().len(),
        3,
        "only the two pushes and the create landed — denied writes touched nothing"
    );
    persist("denial", &journal);

    // --- Lost response: the push lands remotely but its response is
    // lost. The receipt stays `unknown`; a fresh head read reconciles
    // the facts — never a blind retry, never an upgraded receipt.
    let mut remote = GitlabSpikeRemote::new();
    remote.lose_next_response();
    let mut harness = SpikeHarness::new(scope(), &mut remote, None);
    harness.run(&plan()[..1]);
    let r1 = record(harness.journal(), "r1-push");
    assert!(
        matches!(
            r1.outcome,
            RowOutcome::Receipt(WriteReceipt::Unknown { .. })
        ),
        "the lost response reports unknown: {:?}",
        r1.outcome
    );
    assert!(
        r1.reassessment.is_none(),
        "an unresolved write is not reassessed"
    );
    // The effect landed — the remote has the branch — but the receipt
    // stays unknown until reconcile establishes the facts.
    assert_eq!(
        harness.remote_mut().branch_head(&repo(), "spike/u94/w"),
        Some("head-1".to_string())
    );
    harness.reconcile();
    let r1 = record(harness.journal(), "r1-push");
    let reconciliation = r1.reconciliation.as_ref().expect("reconciled");
    assert_eq!(reconciliation.effect, ReconciledEffect::Present);
    assert_eq!(
        reconciliation.observed.as_ref().map(|h| h.object.as_str()),
        Some("head-1")
    );
    assert!(
        matches!(
            r1.outcome,
            RowOutcome::Receipt(WriteReceipt::Unknown { .. })
        ),
        "reconcile establishes facts; the receipt is not upgraded"
    );
    persist("lost_response", harness.journal());

    // --- Restart: the journal survives the harness; a resumed run
    // reconciles the unresolved create under the recorded binding even
    // while the installation default moved — then finishes the plan.
    let mut remote = GitlabSpikeRemote::new();
    let mut harness = SpikeHarness::new(scope(), &mut remote, None);
    harness.run(&plan()[..1]);
    harness.remote_mut().lose_next_response();
    harness.run(&plan()[1..2]);
    let r2 = record(harness.journal(), "r2-create");
    assert!(matches!(
        r2.outcome,
        RowOutcome::Receipt(WriteReceipt::Unknown { .. })
    ));
    // Persist and drop: the restart boundary.
    let journal_json = serde_json::to_string(harness.journal()).unwrap();
    let journal: SpikeJournal = serde_json::from_str(&journal_json).unwrap();
    drop(harness);
    // The installation default moved under the run — reconciliation
    // still binds the recorded scope, not the moved default.
    let mut moved_default = scope();
    moved_default.repository = RepositoryId::trusted("installation-default-repo");
    let mut resumed = SpikeHarness::resume(&mut remote, None, journal, &moved_default);
    assert_eq!(
        resumed.journal().scope.repository,
        scope().repository,
        "the recorded scope wins over the moved default"
    );
    resumed.reconcile();
    {
        let r2 = record(resumed.journal(), "r2-create");
        let reconciliation = r2.reconciliation.as_ref().expect("create reconciled");
        // A lost create response leaves no IID: a head read of its source
        // branch cannot establish which MR (if any) it made.
        assert_eq!(
            reconciliation.effect,
            ReconciledEffect::Indeterminate,
            "a lost create is not reported present by branch coincidence"
        );
        assert!(reconciliation.change.is_none());
        assert_eq!(
            reconciliation
                .observed
                .as_ref()
                .expect("the branch read is retained")
                .repository,
            repo(),
            "reconciled under the recorded binding"
        );
    }
    // The run continues under the recorded scope: remaining rows drive
    // against the identity the fixture assigned; already-recorded rows
    // are not re-driven.
    resumed.run(&plan()[2..]);
    let journal = resumed.into_journal();
    assert_eq!(journal.records.len(), plan().len());
    match &record(&journal, "r3-comment").outcome {
        RowOutcome::Receipt(WriteReceipt::Applied { fact, .. }) => match fact.as_ref() {
            EffectFact::CommentPosted { change: c, .. } => {
                assert_eq!(c.change_request, change("!1"));
                assert_eq!(c.repository, repo());
            }
            other => panic!("comment fact expected: {other:?}"),
        },
        other => panic!("the resumed comment applies: {other:?}"),
    }
    assert!(
        journal.events.iter().any(|e| matches!(
            e,
            JournalEvent::Resumed {
                binding_diverged: true,
                ..
            }
        )),
        "the restart event records the diverged default: {:?}",
        journal.events
    );
    persist("restart", &journal);
}

/// The named test for revision-bound reassessment: each applied write's
/// receipt is followed by a fresh head read whose observed revision
/// equals the receipt's.
#[test]
fn each_applied_write_receipt_binds_a_fresh_head_read() {
    let mut remote = GitlabSpikeRemote::new();
    let mut harness = SpikeHarness::new(scope(), &mut remote, None);
    harness.run(&plan());
    let journal = harness.into_journal();

    let mut applied = 0;
    for record in &journal.records {
        let RowOutcome::Receipt(WriteReceipt::Applied { revision, .. }) = &record.outcome else {
            continue;
        };
        applied += 1;
        let reassessment = record
            .reassessment
            .as_ref()
            .unwrap_or_else(|| panic!("{}: applied write reassessed", record.row_id));
        assert!(
            reassessment.bound,
            "{}: fresh head read {:?} must equal the receipt {:?}",
            record.row_id, reassessment.observed, reassessment.receipt
        );
        assert_eq!(reassessment.observed.as_ref(), Some(revision));
    }
    assert_eq!(applied, 4, "two pushes, the create, the comment");

    // The binding is per-target: the second push's reassessment binds
    // the new head, not the first's.
    let r4 = record(&journal, "r4-push-advance");
    assert_eq!(
        applied_revision(r4).object,
        "head-2",
        "the advanced push reassessed against its own revision"
    );

    // The stale-head comment aborts — a failed receipt, no write, no
    // reassessment.
    let r5 = record(&journal, "r5-stale-comment");
    assert!(matches!(
        r5.outcome,
        RowOutcome::Receipt(WriteReceipt::Failed { .. })
    ));
    assert!(r5.reassessment.is_none());
    assert_eq!(
        remote.applied().len(),
        4,
        "the aborted comment wrote nothing"
    );

    // The fresh head read observes the moved head.
    match &record(&journal, "r6-head-read").outcome {
        RowOutcome::Observed(obs) => {
            assert_eq!(obs.head.as_ref().map(|h| h.object.as_str()), Some("head-2"));
            assert_eq!(
                obs.change.as_ref().map(|c| c.change_request.as_str()),
                Some("!1"),
                "the read resolves the opened MR's scoped identity"
            );
        }
        other => panic!("head read row: {other:?}"),
    }
}

/// Live rows are gated on the recorded authorization: without it the
/// harness reports `not_authorized` and the remote sees no write; an
/// authorization recorded for a different scope does not open the gate;
/// a matching authorization executes the row — here against the fixture
/// transport, so this proves gate mechanics only, never a live claim.
#[test]
fn live_rows_require_the_recorded_authorization() {
    // No recorded authorization: every live row reports not_authorized.
    let mut remote = GitlabSpikeRemote::new();
    let mut harness = SpikeHarness::new(scope(), &mut remote, None);
    harness.run(&live_plan());
    for record in &harness.journal().records {
        assert_eq!(
            record.outcome,
            RowOutcome::NotAuthorized,
            "{}: unrecorded live authorization cannot execute",
            record.row_id
        );
    }
    assert!(
        remote.applied().is_empty() && remote.branch_head(&repo(), "spike/u94/w").is_none(),
        "an unauthorized live row touches nothing"
    );

    // An authorization recorded for a different scope does not cover.
    let mut other_scope = scope();
    other_scope.branch_prefix = "spike/other/".into();
    let foreign = LiveAuthorization {
        record: "owner approval — other scope".into(),
        binding: other_scope.binding(),
    };
    let mut remote = GitlabSpikeRemote::new();
    let mut harness = SpikeHarness::new(scope(), &mut remote, Some(foreign));
    harness.run(&live_plan());
    assert!(
        harness
            .journal()
            .records
            .iter()
            .all(|r| r.outcome == RowOutcome::NotAuthorized),
        "a foreign-scope authorization opens nothing"
    );
    assert!(remote.applied().is_empty());

    // A matching recorded authorization opens the gate: the rows then
    // execute on the harness's transport — the fixture remote stands in
    // here, proving gate mechanics only.
    let authorization = LiveAuthorization {
        record: "owner approval ref — recorded 2026-11-09".into(),
        binding: scope().binding(),
    };
    let mut remote = GitlabSpikeRemote::new();
    let mut harness = SpikeHarness::new(scope(), &mut remote, Some(authorization));
    harness.run(&live_plan());
    let journal = harness.journal();
    assert!(
        journal
            .records
            .iter()
            .all(|r| r.outcome != RowOutcome::NotAuthorized),
        "the recorded authorization admits the rows"
    );
    assert_eq!(
        remote.applied().len(),
        4,
        "the live-lane plan drove the writes"
    );
}

/// Scope identity is injective: the binding escapes its separator, and
/// the actor's host instance participates in it. Two scopes that would
/// collide under a naive `/` join — or that differ only in the actor's
/// host — never cover each other's live authorization.
#[test]
fn scope_binding_separates_escaped_fields_and_the_actor_host() {
    let actor = HostActorRef {
        host: HostInstanceId::trusted("actor-host"),
        actor: HostActorId::trusted("actor"),
    };
    let split_repo = SpikeScope {
        host: HostInstanceId::trusted("h"),
        repository: RepositoryId::trusted("r/x"),
        branch_prefix: "p".into(),
        actor: actor.clone(),
    };
    let split_prefix = SpikeScope {
        host: HostInstanceId::trusted("h"),
        repository: RepositoryId::trusted("r"),
        branch_prefix: "x/p".into(),
        actor: actor.clone(),
    };
    assert_ne!(
        split_repo.binding(),
        split_prefix.binding(),
        "an opaque id containing `/` cannot merge two scopes"
    );

    let authorization = LiveAuthorization {
        record: "owner approval — split-scope guard".into(),
        binding: split_repo.binding(),
    };
    assert!(authorization.covers(&split_repo));
    assert!(!authorization.covers(&split_prefix));

    let mut other_actor_host = split_repo.clone();
    other_actor_host.actor.host = HostInstanceId::trusted("other-actor-host");
    assert_ne!(
        split_repo.binding(),
        other_actor_host.binding(),
        "the actor's host instance is part of the bound identity"
    );
    assert!(!authorization.covers(&other_actor_host));

    let mut backslash = split_repo.clone();
    backslash.repository = RepositoryId::trusted("r\\x");
    backslash.branch_prefix = "p".into();
    let mut escaped_slash = split_repo.clone();
    escaped_slash.repository = RepositoryId::trusted("r");
    escaped_slash.branch_prefix = "\\x/p".into();
    assert_ne!(
        backslash.binding(),
        escaped_slash.binding(),
        "escaped backslashes and escaped slashes stay distinct"
    );
}

/// Two change requests can share one source branch. Comments and head
/// reads name the IID they mean; the remote never picks an arbitrary
/// MR from the branch.
#[test]
fn change_request_identity_is_the_iid_not_the_source_branch() {
    let mut remote = GitlabSpikeRemote::new();
    let rows = vec![
        row(
            "push",
            SpikeOp::GuardedPush {
                branch: "spike/u94/w".into(),
                expected_prior: None,
                produced: "head-1".into(),
            },
        ),
        row(
            "create-a",
            SpikeOp::MrCreate {
                source_branch: "spike/u94/w".into(),
                target_branch: "main".into(),
            },
        ),
        row(
            "create-b",
            SpikeOp::MrCreate {
                source_branch: "spike/u94/w".into(),
                target_branch: "main".into(),
            },
        ),
        row(
            "comment-b",
            SpikeOp::CommentPost {
                change_request: change("!2"),
                source_branch: "spike/u94/w".into(),
                expected_head: "head-1".into(),
                body: "to the second MR".into(),
            },
        ),
        row(
            "comment-a",
            SpikeOp::CommentPost {
                change_request: change("!1"),
                source_branch: "spike/u94/w".into(),
                expected_head: "head-1".into(),
                body: "to the first MR".into(),
            },
        ),
        row(
            "read-a",
            SpikeOp::HeadRead {
                target: HeadTarget::Change {
                    change_request: change("!1"),
                },
            },
        ),
        row(
            "read-b",
            SpikeOp::HeadRead {
                target: HeadTarget::Change {
                    change_request: change("!2"),
                },
            },
        ),
        row(
            "read-unknown",
            SpikeOp::HeadRead {
                target: HeadTarget::Change {
                    change_request: change("!404"),
                },
            },
        ),
        row(
            "comment-unknown",
            SpikeOp::CommentPost {
                change_request: change("!99"),
                source_branch: "spike/u94/w".into(),
                expected_head: "head-1".into(),
                body: "no such MR".into(),
            },
        ),
        row(
            "comment-wrong-branch",
            SpikeOp::CommentPost {
                change_request: change("!1"),
                source_branch: "spike/u94/other".into(),
                expected_head: "head-1".into(),
                body: "wrong branch".into(),
            },
        ),
    ];
    let mut harness = SpikeHarness::new(scope(), &mut remote, None);
    harness.run(&rows);
    assert_eq!(
        harness.remote_mut().applied().len(),
        5,
        "push, two creates, and the two identity-addressed comments wrote"
    );
    let journal = harness.into_journal();

    let commented_change = |id: &str| -> ChangeRequestId {
        match &record(&journal, id).outcome {
            RowOutcome::Receipt(WriteReceipt::Applied { fact, .. }) => match fact.as_ref() {
                EffectFact::CommentPosted { change, .. } => change.change_request.clone(),
                other => panic!("{id}: comment fact expected: {other:?}"),
            },
            other => panic!("{id}: applied comment expected: {other:?}"),
        }
    };
    assert_eq!(commented_change("comment-b"), change("!2"));
    assert_eq!(commented_change("comment-a"), change("!1"));

    for (id, expected) in [("read-a", "!1"), ("read-b", "!2")] {
        match &record(&journal, id).outcome {
            RowOutcome::Observed(observed) => assert_eq!(
                observed
                    .change
                    .as_ref()
                    .map(|change| change.change_request.as_str()),
                Some(expected),
                "{id} resolves the IID it named"
            ),
            other => panic!("{id}: head read expected: {other:?}"),
        }
    }
    match &record(&journal, "read-unknown").outcome {
        RowOutcome::Observed(observed) => {
            assert!(observed.change.is_none());
            assert!(observed.head.is_none());
        }
        other => panic!("read-unknown: head read expected: {other:?}"),
    }
    for id in ["comment-unknown", "comment-wrong-branch"] {
        assert!(
            matches!(
                record(&journal, id).outcome,
                RowOutcome::Receipt(WriteReceipt::Failed { .. })
            ),
            "{id}: an unaddressable or mismatched IID is refused"
        );
    }
}

/// A lost create response leaves no IID. Even when other MRs share its
/// source branch, reconciliation reports `indeterminate` rather than
/// borrowing one of their identities as `present`.
#[test]
fn lost_create_does_not_borrow_another_change_requests_identity() {
    let mut remote = GitlabSpikeRemote::new();
    let mut harness = SpikeHarness::new(scope(), &mut remote, None);
    harness.run(&[
        row(
            "push",
            SpikeOp::GuardedPush {
                branch: "spike/u94/w".into(),
                expected_prior: None,
                produced: "head-1".into(),
            },
        ),
        row(
            "create-a",
            SpikeOp::MrCreate {
                source_branch: "spike/u94/w".into(),
                target_branch: "main".into(),
            },
        ),
        row(
            "create-b",
            SpikeOp::MrCreate {
                source_branch: "spike/u94/w".into(),
                target_branch: "main".into(),
            },
        ),
    ]);
    harness.remote_mut().lose_next_response();
    harness.run(&[row(
        "create-c",
        SpikeOp::MrCreate {
            source_branch: "spike/u94/w".into(),
            target_branch: "main".into(),
        },
    )]);
    assert!(matches!(
        record(harness.journal(), "create-c").outcome,
        RowOutcome::Receipt(WriteReceipt::Unknown { .. })
    ));
    assert_eq!(
        harness.remote_mut().applied().len(),
        4,
        "the lost-response create landed as !3"
    );
    harness.reconcile();
    {
        let create_c = record(harness.journal(), "create-c");
        let reconciliation = create_c.reconciliation.as_ref().expect("reconciled");
        assert_eq!(reconciliation.effect, ReconciledEffect::Indeterminate);
        assert!(
            reconciliation.change.is_none(),
            "no other MR's identity is attributed to the lost create"
        );
    }
    // The effect did land as !3; it is addressable by IID, but the lost
    // receipt itself still cannot claim that identity.
    harness.run(&[row(
        "read-c",
        SpikeOp::HeadRead {
            target: HeadTarget::Change {
                change_request: change("!3"),
            },
        },
    )]);
    match &record(harness.journal(), "read-c").outcome {
        RowOutcome::Observed(observed) => assert_eq!(
            observed
                .change
                .as_ref()
                .map(|change| change.change_request.as_str()),
            Some("!3")
        ),
        other => panic!("read-c: head read expected: {other:?}"),
    }
}

/// The narrow surface is closed: the harness reaches only the four
/// operations — every remote call site in the module is one of the four
/// `SpikeRemote` methods, the operation kind set holds exactly four,
/// out-of-scope operation payloads cannot deserialize, and the
/// contract's other write operations stay unsupported and explicit in
/// every journal.
#[test]
fn no_operation_beyond_the_four_is_reachable() {
    const SOURCE: &str = include_str!("../src/gitlab_spike.rs");
    const ALLOWED: [&str; 4] = ["guarded_push", "mr_create", "comment_post", "head_read"];
    let calls = remote_call_sites(SOURCE);
    assert!(
        !calls.is_empty(),
        "the static check must find the harness's remote call sites"
    );
    for call in &calls {
        assert!(
            ALLOWED.contains(&call.as_str()),
            "remote call `{call}` is outside the four-operation surface"
        );
    }
    // The check normalizes whitespace: a multi-line formatted call is
    // still a call site, and an unknown method is still counted.
    let formatted = "fn f() { self\n    .remote\n    .head_read(t); self.remote.mr_create(a, b); }";
    assert_eq!(
        remote_call_sites(formatted),
        vec!["head_read", "mr_create"],
        "multi-line formatted calls are counted"
    );
    let outside = "fn f() { self . remote . approve_change(x); }";
    assert_eq!(
        remote_call_sites(outside),
        vec!["approve_change"],
        "an operation outside the four is counted rather than skipped"
    );
    let commented = "fn f() { self.remote /*c*/ .approve_change(x); }";
    assert_eq!(
        remote_call_sites(commented),
        vec!["approve_change"],
        "a comment between receiver and method does not hide a call"
    );
    let turbofish = "fn f() { self.remote.approve_change::<u8>(x); }";
    assert_eq!(
        remote_call_sites(turbofish),
        vec!["approve_change"],
        "a turbofish does not hide a call"
    );
    let commented_out = "fn f() { /* self.remote.approve_change(x); */ }";
    assert!(
        remote_call_sites(commented_out).is_empty(),
        "a commented-out call is not a call site"
    );
    assert_eq!(
        SpikeOpKind::ALL.len(),
        4,
        "guarded push, MR create, comment post, head read — nothing else"
    );

    // No out-of-scope operation payload deserializes into a row: an
    // applied receipt for one is not constructible.
    for payload in [
        serde_json::json!({ "op": "approve_change" }),
        serde_json::json!({ "op": "mark_ready" }),
        serde_json::json!({ "op": "resolve_thread" }),
        serde_json::json!({ "op": "update_change_request" }),
        serde_json::json!({ "op": "post_inline_comment" }),
        serde_json::json!({ "op": "publish_review_feedback" }),
        serde_json::json!({ "op": "request_changes" }),
    ] {
        assert!(
            serde_json::from_value::<SpikeOp>(payload.clone()).is_err(),
            "{payload} is not a spike operation — stays unreachable"
        );
    }

    // Unsupported stays explicit: every journal names the contract
    // write operations outside the narrow set.
    let mut remote = GitlabSpikeRemote::new();
    let harness = SpikeHarness::new(scope(), &mut remote, None);
    for name in UNSUPPORTED_OPERATIONS {
        assert!(
            harness
                .journal()
                .unsupported_operations
                .iter()
                .any(|u| u == name),
            "journal records {name} as unsupported"
        );
    }
}

/// Writes stay inside the isolated scope: a row targeting a branch
/// outside the scope's prefix fails before the remote is touched.
#[test]
fn writes_outside_the_isolated_scope_never_reach_the_remote() {
    let mut remote = GitlabSpikeRemote::new();
    let mut harness = SpikeHarness::new(scope(), &mut remote, None);
    harness.run(&[row(
        "out-of-scope",
        SpikeOp::GuardedPush {
            branch: "main".into(),
            expected_prior: None,
            produced: "head-1".into(),
        },
    )]);
    match &record(harness.journal(), "out-of-scope").outcome {
        RowOutcome::Receipt(WriteReceipt::Failed { reason }) => {
            assert!(reason.starts_with("scope:"), "scope violation: {reason}");
        }
        other => panic!("out-of-scope write refused: {other:?}"),
    }
    assert!(
        remote.applied().is_empty(),
        "the remote saw no out-of-scope write"
    );
}
