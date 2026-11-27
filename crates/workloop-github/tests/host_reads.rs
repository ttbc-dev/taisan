//! Typed host reads (U3.2): `observe` delivers a `HostRead` Observation
//! per subject kind, bound to the exact per-kind revision and the actor
//! the remote reports. The recorded read ledger orders by the remote's
//! subject version — a delayed older read is history, never a rollback —
//! and diff snapshots are immutable by construction.

use std::collections::BTreeSet;

use workloop_core::domain::*;
use workloop_core::identity::*;
use workloop_core::observation::*;
use workloop_core::projection::{current_read, read_satisfies};
use workloop_core::store::*;
use workloop_core::work::{HostTemplate, IngestReceipt, WorkloopConfig};
use workloop_github::fixture::FixtureGithub;
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

fn issue() -> IssueRef {
    IssueRef {
        repository: repo(),
        issue: IssueId::trusted("issue-1"),
        display_number: Some(3),
    }
}

fn change() -> ChangeRequestRef {
    ChangeRequestRef {
        repository: repo(),
        change_request: ChangeRequestId::trusted("cr-1"),
        display_number: Some(7),
    }
}

fn reader() -> HostActorRef {
    HostActorRef {
        host: host(),
        actor: HostActorId::trusted("read-bot"),
    }
}

fn writer() -> HostActorRef {
    HostActorRef {
        host: host(),
        actor: HostActorId::trusted("push-agent"),
    }
}

fn config() -> WorkloopConfig {
    WorkloopConfig {
        placement: Ok(workloop_core::placement::PlacementPolicy::default()),
        host: HostTemplate {
            adapter: AdapterId::trusted("fixture"),
            repository: repo(),
            contract_version: 1,
            actors: vec![RoleBinding {
                role: ActorRole::Publication,
                actor: writer(),
                credential_ref: CredentialRef::trusted("cred-publication"),
            }],
        },
        declared_capabilities: BTreeSet::new(),
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

/// Every `HostRead` fact recorded for one subject, in delivery order.
fn recorded_reads(
    store: &SqliteStore,
    subject: &ReadSubject,
) -> Vec<(Option<ReadRevision>, u64, HostActorRef, Completeness)> {
    store
        .transact(|tx| tx.all_observations())
        .unwrap()
        .iter()
        .filter_map(|o| match &o.fact {
            ObservationFact::HostRead {
                subject: s,
                revision,
                subject_version,
                actor,
                completeness,
            } if s == subject => Some((
                revision.clone(),
                *subject_version,
                actor.clone(),
                completeness.clone(),
            )),
            _ => None,
        })
        .collect()
}

/// AC: every observation records its subject kind, subject id, the
/// per-kind revision form, observed actor and completeness — exercised
/// once per subject kind.
#[test]
fn each_subject_kind_records_its_required_revision_form() {
    let store = SqliteStore::in_memory(1).unwrap();
    let config = config();
    let mut fixture = FixtureGithub::new();

    fixture.set_repository(&repo(), "main-head", &writer());
    fixture.set_issue(&issue(), "rev-9", "digest-abc", IssueState::Open);
    fixture.set_issue_actor(&issue(), &writer());
    fixture.set_change_head(&change(), "head-3", &writer());
    fixture.set_branch(&repo(), "work/w1", "head-3", &writer());
    fixture.set_check(&change(), "ci", "run-77", "head-3", &writer());
    fixture.add_review(&change(), "rev-4", &writer());

    let cases: Vec<(ReadSubject, ReadRevision)> = vec![
        (
            ReadSubject::Repository { repository: repo() },
            ReadRevision::Sha {
                revision: rev("main-head"),
            },
        ),
        (
            ReadSubject::Issue { issue: issue() },
            ReadRevision::Issue {
                source_revision: "rev-9".into(),
                content_digest: "digest-abc".into(),
            },
        ),
        (
            ReadSubject::Pr { change: change() },
            ReadRevision::Sha {
                revision: rev("head-3"),
            },
        ),
        (
            ReadSubject::Head {
                repository: repo(),
                branch: "work/w1".into(),
            },
            ReadRevision::Sha {
                revision: rev("head-3"),
            },
        ),
        (
            ReadSubject::Check {
                change: change(),
                check: "ci".into(),
            },
            ReadRevision::Check {
                check_run_id: "run-77".into(),
                head: rev("head-3"),
            },
        ),
        (
            ReadSubject::Review { change: change() },
            ReadRevision::Review {
                review_id: "rev-4".into(),
            },
        ),
    ];

    for (subject, expected) in cases {
        fixture
            .observe(&store, &config, None, &subject, &reader(), NOW)
            .unwrap();
        let reads = recorded_reads(&store, &subject);
        assert_eq!(reads.len(), 1, "{subject:?} produced one read");
        let (revision, _version, actor, completeness) = &reads[0];
        assert_eq!(revision.as_ref(), Some(&expected), "{subject:?} revision");
        assert_eq!(*completeness, Completeness::Complete, "{subject:?}");
        assert_eq!(*actor, writer(), "{subject:?} binds the reported actor");
    }

    // The diff subject: a snapshot for one (base, head) pair. The base
    // object is fetched but never published — seed it explicitly.
    fixture.seed_objects(&["base-1"]);
    let snapshot = DiffSnapshot::new(change(), rev("base-1"), rev("head-3"));
    let subject = ReadSubject::Diff {
        snapshot: snapshot.clone(),
    };
    fixture
        .observe(&store, &config, None, &subject, &reader(), NOW)
        .unwrap();
    let reads = recorded_reads(&store, &subject);
    assert_eq!(
        reads[0].0,
        Some(ReadRevision::DiffSnapshot {
            snapshot_id: snapshot.snapshot_id()
        }),
        "diff revision is the immutable snapshot identity"
    );
    assert_eq!(reads[0].3, Completeness::Complete);
}

/// AC: a read whose required revision field is absent records
/// `Partial{reason}` and is never treated as current.
#[test]
fn read_missing_required_revision_is_partial_and_never_current() {
    let store = SqliteStore::in_memory(1).unwrap();
    let config = config();
    let mut fixture = FixtureGithub::new();

    // A check-run seeded without its check-run id: the required
    // revision field is absent.
    fixture.set_check(&change(), "ci", "", "head-3", &writer());
    let subject = ReadSubject::Check {
        change: change(),
        check: "ci".into(),
    };
    fixture
        .observe(&store, &config, None, &subject, &reader(), NOW)
        .unwrap();
    let reads = recorded_reads(&store, &subject);
    assert_eq!(reads.len(), 1);
    assert_eq!(reads[0].0, None, "a partial read carries no revision");
    assert!(
        matches!(reads[0].3, Completeness::Partial { .. }),
        "missing revision field is Partial: {:?}",
        reads[0].3
    );
    let absent = store.transact(|tx| current_read(tx, &subject)).unwrap();
    assert_eq!(absent, None, "a partial read is never treated as current");
}

/// AC: a delayed/stale observation delivered after a newer one leaves
/// the recorded head at the newer revision.
#[test]
fn delayed_older_read_never_moves_the_recorded_revision_backward() {
    let store = SqliteStore::in_memory(1).unwrap();
    let config = config();
    let mut fixture = FixtureGithub::new();
    let subject = ReadSubject::Pr { change: change() };

    // The remote was at head-1 (version 1); the read taken then is the
    // stale delivery we replay later. Record it without delivery identity
    // so it lands as a distinct row — a delayed delivery, not a resend.
    fixture.set_change_head(&change(), "head-1", &writer());
    let stale = fixture.read(&subject, &reader());
    assert_eq!(stale.subject_version, 1);

    // The remote moved to head-2 (version 2); the newer read lands first.
    fixture.set_change_head(&change(), "head-2", &writer());
    fixture
        .observe(&store, &config, None, &subject, &reader(), NOW)
        .unwrap();
    let current = store.transact(|tx| current_read(tx, &subject)).unwrap();
    assert_eq!(
        current.map(|r| r.revision),
        Some(ReadRevision::Sha {
            revision: rev("head-2")
        })
    );

    // The delayed read of the older revision arrives now — retained as
    // history, and the recorded revision stays at head-2.
    handle_stale_delivery(&store, &config, &subject, stale, NOW + 1);
    let current = store.transact(|tx| current_read(tx, &subject)).unwrap();
    assert_eq!(
        current.map(|r| r.revision),
        Some(ReadRevision::Sha {
            revision: rev("head-2")
        }),
        "a stale delivery cannot roll the recorded head backward"
    );
    // Both deliveries are retained as history.
    assert_eq!(recorded_reads(&store, &subject).len(), 2);
}

/// AC: re-delivering an already-recorded observation is idempotent —
/// the recorded head and the evidence set are unchanged.
#[test]
fn redelivered_read_is_idempotent() {
    let store = SqliteStore::in_memory(1).unwrap();
    let config = config();
    let mut fixture = FixtureGithub::new();
    let subject = ReadSubject::Pr { change: change() };

    fixture.set_change_head(&change(), "head-2", &writer());
    fixture
        .observe(&store, &config, None, &subject, &reader(), NOW)
        .unwrap();
    let receipt = fixture
        .observe(&store, &config, None, &subject, &reader(), NOW)
        .unwrap();
    assert_eq!(
        receipt,
        IngestReceipt::Duplicate,
        "a re-poll of an unchanged subject deduplicates"
    );
    let reads = recorded_reads(&store, &subject);
    assert_eq!(reads.len(), 1, "the evidence set is unchanged");
    let current = store.transact(|tx| current_read(tx, &subject)).unwrap();
    assert_eq!(
        current.map(|r| r.revision),
        Some(ReadRevision::Sha {
            revision: rev("head-2")
        })
    );
}

/// AC: a diff snapshot is immutable by construction — two reads of the
/// same `(base, head)` pair return the identical snapshot identity, and
/// the record carries no mutation path.
#[test]
fn diff_snapshot_identity_is_stable_per_pair() {
    let store = SqliteStore::in_memory(1).unwrap();
    let config = config();
    let mut fixture = FixtureGithub::new();

    let snapshot = DiffSnapshot::new(change(), rev("base-1"), rev("head-3"));
    let same = DiffSnapshot::new(change(), rev("base-1"), rev("head-3"));
    let moved = DiffSnapshot::new(change(), rev("base-1"), rev("head-4"));

    // (base, head) pair identity is deterministic and identical across
    // reads; a different pair is a different snapshot.
    assert_eq!(snapshot.snapshot_id(), same.snapshot_id());
    assert_ne!(snapshot.snapshot_id(), moved.snapshot_id());
    assert_eq!(snapshot, same);

    // `display_number` is presentation metadata: the same triple under
    // a different display number is the identical snapshot — equal
    // under the ref's identity and identical under `snapshot_id`.
    let mut relabeled = change();
    relabeled.display_number = Some(99);
    let display = DiffSnapshot::new(relabeled, rev("base-1"), rev("head-3"));
    assert_eq!(snapshot, display);
    assert_eq!(snapshot.snapshot_id(), display.snapshot_id());

    // Two reads of the same pair record the identical snapshot identity.
    fixture.seed_objects(&["base-1", "head-3"]);
    let subject = ReadSubject::Diff {
        snapshot: snapshot.clone(),
    };
    for _ in 0..2 {
        fixture
            .observe(&store, &config, None, &subject, &reader(), NOW)
            .unwrap();
    }
    let reads = recorded_reads(&store, &subject);
    assert_eq!(reads.len(), 1, "identical re-reads deduplicate");
    assert_eq!(
        reads[0].0,
        Some(ReadRevision::DiffSnapshot {
            snapshot_id: snapshot.snapshot_id()
        })
    );
}

/// AC: a diff subject's version moves when its base/head objects arrive
/// on the remote — a partial read followed by the complete read of the
/// same snapshot records both deliveries instead of colliding on one
/// delivery identity.
#[test]
fn diff_read_records_the_later_complete_read_after_objects_arrive() {
    let store = SqliteStore::in_memory(1).unwrap();
    let config = config();
    let mut fixture = FixtureGithub::new();

    let snapshot = DiffSnapshot::new(change(), rev("base-1"), rev("head-3"));
    let subject = ReadSubject::Diff {
        snapshot: snapshot.clone(),
    };

    // Neither object is on the remote yet: the read is partial.
    let receipt = fixture
        .observe(&store, &config, None, &subject, &reader(), NOW)
        .unwrap();
    assert_eq!(receipt, IngestReceipt::Recorded);
    let reads = recorded_reads(&store, &subject);
    assert_eq!(reads.len(), 1);
    assert!(
        matches!(reads[0].3, Completeness::Partial { .. }),
        "objects not yet on the remote read partial: {:?}",
        reads[0].3
    );
    let absent = store.transact(|tx| current_read(tx, &subject)).unwrap();
    assert_eq!(absent, None);

    // The objects arrive; the same snapshot now reads complete under a
    // new subject version — a distinct delivery identity, not a
    // conflicting resend of the earlier partial read.
    fixture.seed_objects(&["base-1", "head-3"]);
    let receipt = fixture
        .observe(&store, &config, None, &subject, &reader(), NOW + 1)
        .unwrap();
    assert_eq!(
        receipt,
        IngestReceipt::Recorded,
        "the later complete read is recorded, never rejected as a resend"
    );
    let reads = recorded_reads(&store, &subject);
    assert_eq!(reads.len(), 2, "both deliveries are retained");
    let current = store.transact(|tx| current_read(tx, &subject)).unwrap();
    assert_eq!(
        current.map(|r| r.revision),
        Some(ReadRevision::DiffSnapshot {
            snapshot_id: snapshot.snapshot_id()
        }),
        "the complete read is the recorded current read"
    );
}

/// AC: an observation that drives a decision carries both the revision
/// it observed and the actor identity — a decision bound to revision r
/// does not fire while only r−1 evidence exists.
#[test]
fn decision_bound_to_revision_needs_exact_evidence() {
    let store = SqliteStore::in_memory(1).unwrap();
    let config = config();
    let mut fixture = FixtureGithub::new();
    let subject = ReadSubject::Pr { change: change() };

    // Only r−1 (head-1) evidence exists: a decision bound to head-2
    // cannot fire.
    fixture.set_change_head(&change(), "head-1", &writer());
    fixture
        .observe(&store, &config, None, &subject, &reader(), NOW)
        .unwrap();
    let bound = ReadRevision::Sha {
        revision: rev("head-2"),
    };
    let satisfied = store
        .transact(|tx| read_satisfies(tx, &subject, &bound))
        .unwrap();
    assert!(
        !satisfied,
        "r−1 evidence cannot satisfy a decision bound to r"
    );

    // Once the current read carries head-2, the decision can fire — and
    // the satisfying read binds the reporting actor.
    fixture.set_change_head(&change(), "head-2", &writer());
    fixture
        .observe(&store, &config, None, &subject, &reader(), NOW + 1)
        .unwrap();
    let satisfied = store
        .transact(|tx| read_satisfies(tx, &subject, &bound))
        .unwrap();
    assert!(satisfied);
    let current = store.transact(|tx| current_read(tx, &subject)).unwrap();
    assert_eq!(current.unwrap().actor, writer());
}

/// Deliver one captured read as a delayed delivery: no delivery
/// identity, so it lands as a distinct historical row rather than
/// deduplicating as a resend.
fn handle_stale_delivery(
    store: &SqliteStore,
    config: &WorkloopConfig,
    subject: &ReadSubject,
    read: workloop_github::fixture::RemoteRead,
    now: i64,
) {
    workloop_core::work::handle_observation(
        store,
        config,
        &Observation {
            delivery: None,
            subject_work_id: None,
            subject_execution_id: None,
            fact: ObservationFact::HostRead {
                subject: subject.clone(),
                revision: read.revision,
                subject_version: read.subject_version,
                actor: read.actor,
                completeness: read.completeness,
            },
            observed_unix_seconds: now,
        },
        now,
    )
    .unwrap();
}
