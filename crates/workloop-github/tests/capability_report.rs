//! The production adapter's capability-report evidence (#355): every
//! source refresh re-reads the credential's real permission bits and
//! records a `CapabilityReport` whose `support` axis mirrors the
//! adapter's declared operation surface and whose `authorization` axis
//! mirrors the permission read — `unknown` when the read cannot
//! confirm, never a guessed grant. An unchanged report costs no
//! durable rows; a changed one becomes the latest evidence Core's
//! publish gate reads.

use workloop_core::domain::*;
use workloop_core::identity::*;
use workloop_core::observation::*;
use workloop_core::store::*;
use workloop_core::work::{HostTemplate, WorkloopConfig};
use workloop_github::adapter::GitHubAdapter;
use workloop_store::sqlite::SqliteStore;

mod support;

use support::InMemoryGitHub;

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

type Adapter = GitHubAdapter<InMemoryGitHub>;

fn config() -> WorkloopConfig {
    WorkloopConfig {
        host: HostTemplate {
            adapter: AdapterId::trusted("github"),
            repository: repo(),
            contract_version: 1,
            actors: vec![RoleBinding {
                role: ActorRole::Publication,
                actor: publisher(),
                credential_ref: CredentialRef::trusted("cred-publication"),
            }],
        },
        declared_capabilities: Adapter::declared_capabilities()
            .iter()
            .map(|name| HostCapability::from_name(name).expect("contract capability name"))
            .collect(),
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

/// Every capability report recorded for `host`, oldest first.
fn capability_reports(store: &SqliteStore) -> Vec<Vec<CapabilityEntry>> {
    store
        .transact(|tx| tx.all_observations())
        .unwrap()
        .iter()
        .filter_map(|o| match &o.fact {
            ObservationFact::CapabilityReport {
                host: h,
                capabilities,
            } if canonical_host_instance(h.as_str())
                == canonical_host_instance(host().as_str()) =>
            {
                Some(capabilities.clone())
            }
            _ => None,
        })
        .collect()
}

fn entry(entries: &[CapabilityEntry], capability: HostCapability) -> &CapabilityEntry {
    entries
        .iter()
        .find(|e| e.capability == capability)
        .expect("report covers the declared capability")
}

#[test]
fn refresh_reports_supported_authorized_capabilities_once() {
    let store = SqliteStore::in_memory(1).unwrap();
    let config = config();
    let mut adapter = Adapter::new(InMemoryGitHub::new());
    adapter
        .refresh_sources(&store, &config, &mut || NOW, None, None)
        .unwrap();
    let reports = capability_reports(&store);
    assert_eq!(reports.len(), 1, "the first refresh records one report");
    let entries = &reports[0];
    assert_eq!(entries.len(), config.declared_capabilities.len());
    for e in entries {
        assert_eq!(
            e.support,
            CapabilityState::Supported,
            "every declared name is adapter-implemented: {:?}",
            e.capability
        );
    }
    // Write-class capabilities ride the `push` bit, read-class the
    // `pull` bit — the fake credential holds both.
    assert_eq!(
        entry(entries, HostCapability::PushRef).authorization,
        CapabilityState::Authorized
    );
    assert_eq!(
        entry(entries, HostCapability::ChangeRequestCreate).authorization,
        CapabilityState::Authorized
    );
    assert_eq!(
        entry(entries, HostCapability::IssueRead).authorization,
        CapabilityState::Authorized
    );
    // An unchanged credential costs no durable rows on the next pass.
    adapter
        .refresh_sources(&store, &config, &mut || NOW + 1, None, None)
        .unwrap();
    assert_eq!(
        capability_reports(&store).len(),
        1,
        "an identical report is not re-recorded"
    );
}

#[test]
fn a_declared_but_unimplemented_capability_reports_denied_support() {
    let store = SqliteStore::in_memory(1).unwrap();
    let mut config = config();
    config
        .declared_capabilities
        .insert(HostCapability::MarkReady);
    let mut adapter = Adapter::new(InMemoryGitHub::new());
    adapter
        .refresh_sources(&store, &config, &mut || NOW, None, None)
        .unwrap();
    let reports = capability_reports(&store);
    assert_eq!(reports.len(), 1);
    let mark_ready = entry(&reports[0], HostCapability::MarkReady);
    assert_eq!(mark_ready.support, CapabilityState::Denied);
    assert_eq!(mark_ready.authorization, CapabilityState::Authorized);
    assert!(!mark_ready.usable());
}

#[test]
fn permission_drift_replaces_the_latest_report() {
    let store = SqliteStore::in_memory(1).unwrap();
    let config = config();
    let mut adapter = Adapter::new(InMemoryGitHub::new());
    adapter
        .refresh_sources(&store, &config, &mut || NOW, None, None)
        .unwrap();
    // The credential loses write access: the next refresh records a
    // new report — support unchanged, write authorization denied.
    adapter.client_mut().set_permissions(false, true);
    adapter
        .refresh_sources(&store, &config, &mut || NOW + 1, None, None)
        .unwrap();
    let reports = capability_reports(&store);
    assert_eq!(reports.len(), 2, "changed authorization is new evidence");
    let latest = reports.last().unwrap();
    assert_eq!(
        entry(latest, HostCapability::PushRef).authorization,
        CapabilityState::Denied
    );
    assert_eq!(
        entry(latest, HostCapability::PushRef).support,
        CapabilityState::Supported,
        "support is the adapter's surface, never the credential's"
    );
    assert_eq!(
        entry(latest, HostCapability::IssueRead).authorization,
        CapabilityState::Authorized,
        "read authorization is unaffected by the push bit"
    );
    // Restoring access records a third report — each transition is
    // durable evidence, never folded into the old row.
    adapter.client_mut().set_permissions(true, true);
    adapter
        .refresh_sources(&store, &config, &mut || NOW + 2, None, None)
        .unwrap();
    assert_eq!(capability_reports(&store).len(), 3);
}

#[test]
fn an_unanswered_permission_read_records_unknown_authorization() {
    let store = SqliteStore::in_memory(1).unwrap();
    let config = config();
    let mut remote = InMemoryGitHub::new();
    // The permission endpoint's response is lost: authorization is
    // unconfirmed — recorded `unknown`, never `authorized`.
    remote.lose_response("read_permissions");
    let mut adapter = Adapter::new(remote);
    adapter
        .refresh_sources(&store, &config, &mut || NOW, None, None)
        .unwrap();
    let reports = capability_reports(&store);
    assert_eq!(reports.len(), 1);
    for e in &reports[0] {
        assert_eq!(
            e.authorization,
            CapabilityState::Unknown,
            "{:?} authorization is unconfirmed, not granted",
            e.capability
        );
        assert!(
            !e.usable(),
            "unknown authorization never satisfies the publish gate"
        );
    }
}

#[test]
fn reports_for_other_hosts_do_not_mask_this_hosts_latest() {
    let store = SqliteStore::in_memory(1).unwrap();
    let config = config();
    // A foreign-host report already on record must not satisfy this
    // host's comparison — canonical host identity decides.
    workloop_core::work::handle_observation(
        &store,
        &config,
        &Observation {
            delivery: None,
            subject_work_id: None,
            subject_execution_id: None,
            fact: ObservationFact::CapabilityReport {
                host: HostInstanceId::trusted("other-host"),
                capabilities: vec![CapabilityEntry {
                    capability: HostCapability::PushRef,
                    support: CapabilityState::Supported,
                    authorization: CapabilityState::Authorized,
                }],
            },
            observed_unix_seconds: NOW,
        },
        NOW,
    )
    .unwrap();
    let mut adapter = Adapter::new(InMemoryGitHub::new());
    adapter
        .refresh_sources(&store, &config, &mut || NOW + 1, None, None)
        .unwrap();
    let reports = capability_reports(&store);
    assert_eq!(reports.len(), 1, "only this host's report is recorded");
    assert_eq!(reports[0].len(), config.declared_capabilities.len());
}

/// A resolved publication credential probes under its own proven
/// actor: the seat authenticates as the bound publisher, the
/// permission read runs under the bound credential, and the report's
/// authorization axis is the real bit — pull-only denies the write
/// capabilities while reads stay authorized.
#[test]
fn a_resolved_probe_reports_the_bound_actor_permissions() {
    let store = SqliteStore::in_memory(1).unwrap();
    let mut config = config();
    config.host.actors[0].credential_ref = CredentialRef::trusted("env:PUB_TOKEN");
    let mut book = workloop_github::fixture::CredentialBook::new();
    book.seat_env("PUB_TOKEN", "pub-secret");
    let mut gh = InMemoryGitHub::new();
    gh.seat_credential("pub-secret", publisher());
    gh.set_permissions(false, true);
    let mut adapter = Adapter::new(gh).with_credentials(Box::new(book));
    adapter
        .refresh_sources(&store, &config, &mut || NOW, None, None)
        .unwrap();
    let reports = capability_reports(&store);
    assert_eq!(reports.len(), 1);
    let entries = &reports[0];
    assert_eq!(
        entry(entries, HostCapability::PushRef).authorization,
        CapabilityState::Denied,
        "the bound credential's pull-only grant denies writes"
    );
    assert_eq!(
        entry(entries, HostCapability::IssueRead).authorization,
        CapabilityState::Authorized
    );
}

/// A publication credential that authenticates as a foreign account
/// never lends its permissions to the report: the authorization axis
/// is `unknown` for every entry, and the probe installs nothing on
/// the transport afterwards.
#[test]
fn a_foreign_probe_credential_reports_unknown_authorization() {
    let store = SqliteStore::in_memory(1).unwrap();
    let mut config = config();
    config.host.actors[0].credential_ref = CredentialRef::trusted("env:PUB_TOKEN");
    let mut book = workloop_github::fixture::CredentialBook::new();
    book.seat_env("PUB_TOKEN", "foreign-secret");
    let mut gh = InMemoryGitHub::new();
    gh.seat_credential(
        "foreign-secret",
        HostActorRef {
            host: host(),
            actor: HostActorId::trusted("other-account"),
        },
    );
    gh.set_permissions(true, true);
    let mut adapter = Adapter::new(gh).with_credentials(Box::new(book));
    adapter
        .refresh_sources(&store, &config, &mut || NOW, None, None)
        .unwrap();
    let reports = capability_reports(&store);
    assert_eq!(reports.len(), 1);
    for e in &reports[0] {
        assert_eq!(
            e.authorization,
            CapabilityState::Unknown,
            "a foreign credential's permissions are never granted: {e:?}"
        );
    }
}
