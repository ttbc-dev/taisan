//! U3.6a read-path suite: the shared read-path scenarios — stale-head
//! observation and duplicate delivery/event — run unchanged against two
//! independently implemented adapters and retain one U3.6 evidence
//! record per scenario per adapter.
//!
//! `FixtureGithub` is the GitHub adapter (the GitHub-shaped fixture the
//! U3 contract targets); `SparseHost` is the independent fixture
//! implementation — different remote model, the same typed-read
//! contract. `GitlabFixture` stays out of this slice: the issue scopes
//! the read path to the fixture and GitHub adapter runs.
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

use workloop_core::domain::*;
use workloop_core::identity::*;
use workloop_core::observation::*;
use workloop_core::projection::{current_read, read_satisfies};
use workloop_core::store::*;
use workloop_core::work::{HostTemplate, IngestReceipt, WorkloopConfig};
use workloop_github::evidence::{
    ClaimClass, ConfigIdentity, EvidenceRecord, ReadBackReceipt, SCHEMA_VERSION, ScenarioResult,
    record_revisions, validate_record_json,
};
use workloop_github::fixture::{FixtureGithub, RemoteRead};
use workloop_github::sparse_fixture::SparseHost;
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

fn config(adapter: &str) -> WorkloopConfig {
    WorkloopConfig {
        host: HostTemplate {
            adapter: AdapterId::trusted(adapter),
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

/// What a conforming read-path adapter accepts to drive. Two
/// implementations: different remote models, the same typed-read
/// contract — the suite never pattern-matches a fixture's internals.
trait ReadPathAdapter {
    /// The adapter implementation id a record binds.
    fn adapter_id(&self) -> &'static str;

    /// The delivery source namespace the adapter's `observe` stamps —
    /// delivery identities deduplicate within it, so a re-delivered
    /// event must cite the adapter's own source.
    fn poll_source(&self) -> &'static str;

    /// Test control: move the remote's change head — the racing actor
    /// whose delayed read arrives out of order.
    fn set_change_head(&mut self, change: &ChangeRequestRef, head: &str, actor: &HostActorRef);

    /// Resolve one typed read target against the remote — the observed
    /// revision, remote version, reported actor and completeness.
    fn read(&self, subject: &ReadSubject, reader: &HostActorRef) -> RemoteRead;

    /// Poll one typed read target and deliver the Observation through
    /// the shared ingest pipeline.
    fn observe(
        &self,
        store: &SqliteStore,
        config: &WorkloopConfig,
        subject: &ReadSubject,
        reader: &HostActorRef,
        now: i64,
    ) -> IngestReceipt;

    /// Feed one externally-shaped Observation through the shared
    /// ingest pipeline — the delayed-delivery path.
    fn emit(
        &self,
        store: &SqliteStore,
        config: &WorkloopConfig,
        observation: Observation,
        now: i64,
    ) -> IngestReceipt;
}

impl ReadPathAdapter for FixtureGithub {
    fn adapter_id(&self) -> &'static str {
        "fixture-github"
    }
    fn poll_source(&self) -> &'static str {
        "fixture"
    }
    fn set_change_head(&mut self, change: &ChangeRequestRef, head: &str, actor: &HostActorRef) {
        FixtureGithub::set_change_head(self, change, head, actor);
    }
    fn read(&self, subject: &ReadSubject, reader: &HostActorRef) -> RemoteRead {
        FixtureGithub::read(self, subject, reader)
    }
    fn observe(
        &self,
        store: &SqliteStore,
        config: &WorkloopConfig,
        subject: &ReadSubject,
        reader: &HostActorRef,
        now: i64,
    ) -> IngestReceipt {
        FixtureGithub::observe(self, store, config, None, subject, reader, now).unwrap()
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
}

impl ReadPathAdapter for SparseHost {
    fn adapter_id(&self) -> &'static str {
        "sparse-host"
    }
    fn poll_source(&self) -> &'static str {
        "sparse"
    }
    fn set_change_head(&mut self, change: &ChangeRequestRef, head: &str, _actor: &HostActorRef) {
        self.set_head(&change.repository, change.change_request.as_str(), head);
    }
    fn read(&self, subject: &ReadSubject, reader: &HostActorRef) -> RemoteRead {
        SparseHost::read(self, subject, reader)
    }
    fn observe(
        &self,
        store: &SqliteStore,
        config: &WorkloopConfig,
        subject: &ReadSubject,
        reader: &HostActorRef,
        now: i64,
    ) -> IngestReceipt {
        SparseHost::observe(self, store, config, None, subject, reader, now).unwrap()
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
}

/// Every `HostRead` fact recorded for one subject, in delivery order.
fn recorded_reads(store: &SqliteStore, subject: &ReadSubject) -> Vec<(Option<ReadRevision>, u64)> {
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
            } if s == subject => Some((revision.clone(), *subject_version)),
            _ => None,
        })
        .collect()
}

/// Deliver one captured read as a delayed delivery: no delivery
/// identity, so it lands as a distinct historical row rather than
/// deduplicating as a resend.
fn deliver_delayed(
    adapter: &dyn ReadPathAdapter,
    store: &SqliteStore,
    config: &WorkloopConfig,
    subject: &ReadSubject,
    read: RemoteRead,
    now: i64,
) -> IngestReceipt {
    adapter.emit(
        store,
        config,
        Observation {
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
}

/// The evidence a scenario run leaves behind: the observation delivery
/// identities it recorded and the read-back receipts it re-observed
/// through.
struct ScenarioEvidence {
    observation_identities: Vec<String>,
    read_back_receipts: Vec<ReadBackReceipt>,
}

/// HC read-path scenario 1 — a stale-head observation never moves the
/// recorded current head backward. The remote was at head-1 when the
/// stale read was taken; the remote then moved to head-2 and the newer
/// read landed first. The delayed older delivery is retained as
/// history, never a rollback.
fn stale_head_never_rolls_back(adapter: &mut dyn ReadPathAdapter) -> ScenarioEvidence {
    let store = SqliteStore::in_memory(1).unwrap();
    let config = config(adapter.adapter_id());
    let subject = ReadSubject::Pr { change: change() };

    adapter.set_change_head(&change(), "head-1", &writer());
    let stale = adapter.read(&subject, &reader());
    assert_eq!(
        stale.subject_version,
        1,
        "{}: the remote was at version 1 when the stale read was taken",
        adapter.adapter_id()
    );
    assert_eq!(
        stale.revision,
        Some(ReadRevision::Sha {
            revision: rev("head-1")
        })
    );

    adapter.set_change_head(&change(), "head-2", &writer());
    let receipt = adapter.observe(&store, &config, &subject, &reader(), NOW);
    assert_eq!(
        receipt,
        IngestReceipt::Recorded,
        "{}: the current read lands first",
        adapter.adapter_id()
    );
    let current = store.transact(|tx| current_read(tx, &subject)).unwrap();
    assert_eq!(
        current.map(|r| r.revision),
        Some(ReadRevision::Sha {
            revision: rev("head-2")
        }),
        "{}: the recorded current read is head-2",
        adapter.adapter_id()
    );

    // The delayed read of the older revision arrives now — retained as
    // history, and the recorded revision stays at head-2.
    let delayed = deliver_delayed(adapter, &store, &config, &subject, stale, NOW + 1);
    assert_eq!(delayed, IngestReceipt::Recorded);
    let current = store.transact(|tx| current_read(tx, &subject)).unwrap();
    assert_eq!(
        current.map(|r| r.revision),
        Some(ReadRevision::Sha {
            revision: rev("head-2")
        }),
        "{}: a stale delivery cannot roll the recorded head backward",
        adapter.adapter_id()
    );
    assert_eq!(
        recorded_reads(&store, &subject).len(),
        2,
        "{}: both deliveries are retained as history",
        adapter.adapter_id()
    );

    ScenarioEvidence {
        observation_identities: vec![
            format!("local:{}.poll:{}@2", adapter.poll_source(), subject.key()),
            format!("(delayed delivery, no identity):{}@1", subject.key()),
        ],
        read_back_receipts: vec![
            ReadBackReceipt {
                target: subject.key(),
                observed: "ingest:Recorded head-2 @v2 before the stale delivery".into(),
            },
            ReadBackReceipt {
                target: subject.key(),
                observed: "ingest:Recorded delayed head-1 @v1; current_read stayed head-2 @v2"
                    .into(),
            },
        ],
    }
}

/// HC read-path scenario 2 — duplicate delivery/event deduplicates to a
/// single effect: a re-poll of the unchanged subject returns
/// `Duplicate`, and re-delivery of the identical event fact under the
/// same delivery identity is likewise `Duplicate`. The evidence set
/// and the recorded current read are unchanged.
fn duplicate_delivery_deduplicates(adapter: &mut dyn ReadPathAdapter) -> ScenarioEvidence {
    let store = SqliteStore::in_memory(1).unwrap();
    let config = config(adapter.adapter_id());
    let subject = ReadSubject::Pr { change: change() };

    adapter.set_change_head(&change(), "head-2", &writer());
    let first = adapter.observe(&store, &config, &subject, &reader(), NOW);
    assert_eq!(first, IngestReceipt::Recorded, "{}", adapter.adapter_id());

    // A re-poll of the unchanged subject deduplicates on the delivery
    // identity (subject at the same remote version).
    let repoll = adapter.observe(&store, &config, &subject, &reader(), NOW + 1);
    assert_eq!(
        repoll,
        IngestReceipt::Duplicate,
        "{}: a re-poll of an unchanged subject deduplicates",
        adapter.adapter_id()
    );

    // A re-delivery of the identical event under the same delivery
    // identity deduplicates the same way — one effect, never two.
    let read = adapter.read(&subject, &reader());
    let redelivery = adapter.emit(
        &store,
        &config,
        Observation {
            delivery: Some(DeliveryId {
                scope: DeliveryScope::Local,
                source: format!("{}.poll", adapter.poll_source()),
                event_id: format!("{}@{}", subject.key(), read.subject_version),
            }),
            subject_work_id: None,
            subject_execution_id: None,
            fact: ObservationFact::HostRead {
                subject: subject.clone(),
                revision: read.revision,
                subject_version: read.subject_version,
                actor: read.actor,
                completeness: read.completeness,
            },
            observed_unix_seconds: NOW + 2,
        },
        NOW + 2,
    );
    assert_eq!(
        redelivery,
        IngestReceipt::Duplicate,
        "{}: a re-delivered identical event deduplicates",
        adapter.adapter_id()
    );

    let reads = recorded_reads(&store, &subject);
    assert_eq!(
        reads.len(),
        1,
        "{}: the evidence set holds the single delivered read",
        adapter.adapter_id()
    );
    let current = store.transact(|tx| current_read(tx, &subject)).unwrap();
    assert_eq!(
        current.map(|r| r.revision),
        Some(ReadRevision::Sha {
            revision: rev("head-2")
        })
    );
    assert!(
        store
            .transact(|tx| read_satisfies(
                tx,
                &subject,
                &ReadRevision::Sha {
                    revision: rev("head-2")
                }
            ))
            .unwrap(),
        "{}: the single recorded read satisfies the head-2 decision",
        adapter.adapter_id()
    );

    ScenarioEvidence {
        observation_identities: vec![format!(
            "local:{}.poll:{}@1",
            adapter.poll_source(),
            subject.key()
        )],
        read_back_receipts: vec![
            ReadBackReceipt {
                target: subject.key(),
                observed: "ingest:Duplicate re-poll; recorded set unchanged (1 row)".into(),
            },
            ReadBackReceipt {
                target: subject.key(),
                observed: "ingest:Duplicate re-delivered event; current_read head-2 @v1".into(),
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

/// The read-path scenario table: every row runs unchanged against each
/// registered adapter.
type Scenario = fn(&mut dyn ReadPathAdapter) -> ScenarioEvidence;

const SCENARIOS: &[(&str, Scenario)] = &[
    (
        "stale_head_observation_never_rolls_back",
        stale_head_never_rolls_back,
    ),
    (
        "duplicate_delivery_deduplicates_to_a_single_effect",
        duplicate_delivery_deduplicates,
    ),
];

/// Run the read-path scenario table against one adapter, retaining one
/// record per scenario when the checkout yields a faithful stamp.
fn run_recorded(
    adapter: &mut dyn ReadPathAdapter,
    stamp: Option<&(String, String)>,
) -> Vec<RecordRow> {
    SCENARIOS
        .iter()
        .map(|&(id, run)| {
            let evidence = run(adapter);
            let record = stamp.map(|(tested, base)| EvidenceRecord {
                schema_version: SCHEMA_VERSION,
                tested_revision: tested.clone(),
                base_revision: base.clone(),
                core_revision: tested.clone(),
                adapter_revision: tested.clone(),
                config_identity: ConfigIdentity::for_config(
                    &config(adapter.adapter_id()),
                    adapter.adapter_id(),
                ),
                command_or_scenario: format!("u36a-read-path::{id}"),
                action_identities: vec![],
                observation_identities: evidence.observation_identities,
                read_back_receipts: evidence.read_back_receipts,
                result: ScenarioResult::Pass,
                claim_class: ClaimClass::FixtureContract,
                authorization_reference: None,
            });
            RecordRow {
                scenario: id,
                adapter: adapter.adapter_id(),
                record,
            }
        })
        .collect()
}

/// U3.6a named run: the read-path scenarios execute unchanged on the
/// GitHub adapter (`FixtureGithub`) and the independent fixture
/// (`SparseHost`), and every retained record passes the field-presence
/// check — one record per scenario per adapter, `fixture_contract`
/// throughout. The records are persisted under `target/` only when the
/// checkout yields a faithful revision stamp.
#[test]
fn read_path_records_retain_one_record_per_scenario_per_adapter() {
    let stamp = record_revisions(env!("CARGO_MANIFEST_DIR"));
    let mut rows = Vec::new();
    rows.extend(run_recorded(&mut FixtureGithub::new(), stamp.as_ref()));
    rows.extend(run_recorded(&mut SparseHost::new(), stamp.as_ref()));

    assert_eq!(rows.len(), 4, "two scenarios × two adapters");
    let adapters: BTreeSet<_> = rows.iter().map(|r| r.adapter).collect();
    assert_eq!(adapters.len(), 2, "one run per adapter");
    let scenarios: BTreeSet<_> = rows.iter().map(|r| r.scenario).collect();
    assert_eq!(scenarios.len(), 2, "one record per scenario");

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
        let path =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/u3.6a-records.json");
        std::fs::write(
            &path,
            format!("{}\n", serde_json::to_string_pretty(&retained).unwrap()),
        )
        .unwrap_or_else(|e| panic!("retain the U3.6a record set at {}: {e}", path.display()));
    } else {
        for row in &rows {
            assert!(row.record.is_none(), "an unstamped run retains no records");
        }
        eprintln!(
            "u3.6a: no faithful revision stamp (dirty or non-git checkout) — \
             the suite ran; no records were retained"
        );
    }
}
