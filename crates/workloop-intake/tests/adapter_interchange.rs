//! Adapter interchange through the real intake path (#217): the
//! packaged reference executable and the independent
//! `workloop-fixture-selector` fixture are driven through
//! `run_intake` against real stores — same scripted sources, same
//! intake parameters, only the selector binding's argv (and adapter
//! id) differs. A grouped fixture candidate then proves the
//! multi-source admission path end to end: one candidate, one
//! submit, one Work claiming every member atomically.

use std::path::PathBuf;
use std::time::Duration;

use workloop_core::command::{CommandBody, CommandDecision, CommandRequest};
use workloop_core::domain::{
    Actor, ActorKind, AllowedEffects, ExecutionBudget, Origin, WorkPolicy,
};
use workloop_core::identity::{
    ActorRole, AdapterId, CredentialRef, HostActorId, HostActorRef, HostCapability, HostInstanceId,
    IssueId, IssueRef, RepositoryId, RepositoryRef, RoleBinding, SourceSnapshot,
};
use workloop_core::store::{IntakeCandidateStatus, IntakeRunState, Store};
use workloop_core::work::{HostTemplate, WorkloopConfig, handle_command};
use workloop_intake::{IntakeParams, IntakeReport, SelectorBinding, run_intake};
use workloop_selector::admission;
use workloop_selector::{Proposal, Provenance, SourceBinding, TaskInput, proposal_key};
use workloop_store::sqlite::SqliteStore;

/// Real clock values: the adapter executables compare the request
/// deadline against their own wall clock, so fixture timestamps must
/// be current — a 2023 constant would read as already expired.
fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

fn now() -> i64 {
    now_ms() / 1000
}

fn tmp_root(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("workloop-xint-{}-{}", tag, std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn exe(name: &str) -> String {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join(format!("../../target/debug/{name}"))
        .to_string_lossy()
        .into_owned()
}

fn host() -> HostInstanceId {
    HostInstanceId::trusted("test-host")
}

fn repo() -> RepositoryRef {
    RepositoryRef::new(host(), RepositoryId::trusted("repo-a"))
}

fn issue(id: &str) -> IssueRef {
    IssueRef {
        repository: repo(),
        issue: IssueId::trusted(id),
        display_number: Some(1),
    }
}

fn actor() -> Actor {
    Actor {
        kind: ActorKind::Human,
        external_id: "op".into(),
    }
}

fn publisher() -> HostActorRef {
    HostActorRef {
        host: host(),
        actor: HostActorId::trusted("pub-agent"),
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
        declared_capabilities: vec![
            HostCapability::RepositoryRead,
            HostCapability::PushRef,
            HostCapability::ChangeRequestCreate,
            HostCapability::ChangeRequestRead,
            HostCapability::ChecksRead,
            HostCapability::ReviewFindingsRead,
            HostCapability::IssueRead,
        ]
        .into_iter()
        .collect(),
        lanes: Default::default(),
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

/// The intake fixture: one store, the shared config, and the fields
/// every run supplies identically — only `binding` varies per
/// adapter.
struct Fixture {
    store: SqliteStore,
    config: WorkloopConfig,
    actor: Actor,
    constraints: Vec<String>,
    argv: Vec<String>,
    permissions: Vec<String>,
    repositories: Vec<RepositoryRef>,
    cwd: PathBuf,
}

impl Fixture {
    fn new(dir: PathBuf) -> Self {
        Self {
            store: SqliteStore::in_memory(4).unwrap(),
            config: config(),
            actor: actor(),
            constraints: vec!["constraint-a".to_string()],
            argv: vec!["true".to_string()],
            permissions: vec!["perm-a".to_string()],
            repositories: vec![repo()],
            cwd: dir,
        }
    }

    fn run(&self, binding: &SelectorBinding) -> IntakeReport {
        let params = IntakeParams {
            trigger: workloop_core::store::IntakeTrigger::Manual,
            binding,
            actor: &self.actor,
            work_config: &self.config,
            repositories: &self.repositories,
            task_constraints: &self.constraints,
            verification_argv: &self.argv,
            required_permissions: &self.permissions,
            settings_revision: None,
            cwd: &self.cwd,
            timeout: Duration::from_secs(15),
        };
        run_intake(&self.store, &params, now(), now_ms()).expect("run_intake succeeds")
    }
}

/// A selector binding whose argv launches `exe` with the scripted
/// `--sources` file and the `intake` label filter — the exact shape
/// an installation profile's `[intake.selector]` resolves to.
fn binding(adapter: &str, exe_name: &str, sources: &std::path::Path) -> SelectorBinding {
    SelectorBinding {
        adapter: adapter.into(),
        selector_version: 1,
        policy_revision: 7,
        argv: vec![
            exe(exe_name),
            "select".into(),
            "--sources".into(),
            sources.to_string_lossy().into_owned(),
            "--label".into(),
            "intake".into(),
        ],
    }
}

/// One scripted fact in the sources-file shape both adapters read.
fn fact_json(
    id: &str,
    number: u64,
    created: i64,
    title: &str,
    group: Option<&str>,
) -> serde_json::Value {
    let mut value = serde_json::json!({
        "issue": {
            "repository": {
                "host": "test-host",
                "repository": "repo-a",
            },
            "issue": id,
            "display_number": number,
        },
        "state": "open",
        "labels": ["intake"],
        "created_unix_seconds": created,
        "title": title,
        "revision": format!("rev-{id}"),
        "content_digest": format!("digest-{id}"),
    });
    if let Some(group) = group {
        value["group"] = serde_json::json!(group);
    }
    value
}

fn write_sources(dir: &std::path::Path, issues: Vec<serde_json::Value>) -> PathBuf {
    let path = dir.join("sources.json");
    std::fs::write(
        &path,
        serde_json::to_string(&serde_json::json!({
            "repositories": {
                "test-host/repo-a": { "pages": [{ "issues": issues }] }
            }
        }))
        .unwrap(),
    )
    .unwrap();
    path
}

/// The admitted Work's spec, fetched inside one deferred read.
fn spec_of(
    store: &SqliteStore,
    work_id: &workloop_core::domain::WorkId,
) -> workloop_core::domain::SpecPayload {
    store
        .transact_deferred(|tx| tx.spec(work_id, None))
        .unwrap()
        .expect("spec exists")
        .payload
}

/// Changing only the selector binding's argv — the configuration an
/// installation profile carries — selects either adapter for new
/// intake. Both runs admit the same candidates through the same
/// submit path: identical intents, identical claimed issues,
/// identical durable statuses.
#[test]
fn either_adapter_argv_drives_the_same_intake_contract() {
    let dir = tmp_root("either");
    let sources = write_sources(
        &dir,
        vec![
            fact_json("i-1", 1, 100, "alpha work", None),
            fact_json("i-2", 2, 200, "bravo work", None),
        ],
    );

    let mut outcomes = Vec::new();
    for (adapter, exe_name) in [
        ("public-issue-selector", "public-issue-selector"),
        ("workloop-fixture-selector", "workloop-fixture-selector"),
    ] {
        let f = Fixture::new(dir.clone());
        let report = f.run(&binding(adapter, exe_name, &sources));
        assert_eq!(report.state, IntakeRunState::Completed, "{adapter}");
        assert!(report.launched, "{adapter}");
        assert_eq!(report.candidates.len(), 2, "{adapter}");
        let mut intents = Vec::new();
        let mut claimed = Vec::new();
        for candidate in &report.candidates {
            assert_eq!(
                candidate.status,
                IntakeCandidateStatus::Accepted,
                "{adapter}: {candidate:?}"
            );
            let work_id = candidate.work_id.clone().expect("accepted work");
            let spec = spec_of(&f.store, &work_id);
            intents.push(spec.intent.clone());
            for source in &spec.sources {
                claimed.push(source.issue.issue.as_str().to_string());
            }
        }
        outcomes.push((intents, claimed));
    }

    // Identical admitted semantics across both executables: same
    // intents in the same order, same issues claimed.
    assert_eq!(outcomes[0], outcomes[1]);
    assert_eq!(outcomes[0].0, ["alpha work", "bravo work"]);
    assert_eq!(outcomes[0].1, ["i-1", "i-2"]);
}

/// A grouped fixture candidate is ONE candidate end to end: one
/// durable candidate row, one submit, one Work whose spec claims
/// every member in member order — and exactly one pending-queue
/// position regardless of member count.
#[test]
fn a_grouped_candidate_admits_as_one_work() {
    let dir = tmp_root("group-admit");
    let sources = write_sources(
        &dir,
        vec![
            fact_json("i-z", 30, 300, "group new", Some("squad")),
            fact_json("i-s", 10, 200, "solo work", None),
            fact_json("i-a", 20, 100, "group old", Some("squad")),
        ],
    );
    let f = Fixture::new(dir.clone());
    let report = f.run(&binding(
        "workloop-fixture-selector",
        "workloop-fixture-selector",
        &sources,
    ));

    assert_eq!(report.state, IntakeRunState::Completed);
    assert_eq!(
        report.candidates.len(),
        2,
        "group + singleton = 2 candidates"
    );

    // The group ranks first (earliest member created=100) and admits
    // as exactly one Work.
    let group = &report.candidates[0];
    assert_eq!(group.status, IntakeCandidateStatus::Accepted, "{group:?}");
    let work_id = group.work_id.clone().expect("group work");
    let spec = spec_of(&f.store, &work_id);
    assert_eq!(spec.intent, "group old + group new");
    // Member order is (host, repository, issue): i-a before i-z.
    let claimed: Vec<&str> = spec
        .sources
        .iter()
        .map(|s| s.issue.issue.as_str())
        .collect();
    assert_eq!(claimed, ["i-a", "i-z"]);
    assert_eq!(spec.sources[0].source_revision, "rev-i-a");
    assert_eq!(spec.sources[1].source_revision, "rev-i-z");

    // Both member issues are claimed by that one Work.
    for member in ["i-a", "i-z"] {
        let claim = f
            .store
            .transact_deferred(|tx| tx.issue_claim(&issue(member)))
            .unwrap();
        assert_eq!(claim.as_ref(), Some(&work_id), "claim for {member}");
    }

    // The singleton admits as its own Work.
    let solo = &report.candidates[1];
    assert_eq!(solo.status, IntakeCandidateStatus::Accepted);
    assert_eq!(
        spec_of(&f.store, &solo.work_id.clone().unwrap()).intent,
        "solo work"
    );

    // Two Works, two pending positions — the group consumed exactly
    // one slot for two source issues.
    let (pending, works) = f
        .store
        .transact_deferred(|tx| Ok((tx.pending_queue_count()?, tx.works()?)))
        .unwrap();
    assert_eq!(pending, 2);
    assert_eq!(works.len(), 2);
}

/// Grouped admission rides the ordinary submit gate: when any member
/// conflicts, the WHOLE candidate is rejected and no member is
/// claimed — never a partial Work. This exercises the exact
/// `submit_command` → `handle_command` path `admit_one` uses.
#[test]
fn a_grouped_submit_rejects_atomically_on_member_conflict() {
    let dir = tmp_root("group-atomic");
    let store = SqliteStore::in_memory(4).unwrap();
    let cfg = config();
    let actor = actor();

    // i-a is already claimed by a live Work.
    let seeded = handle_command(
        &store,
        &cfg,
        &CommandRequest {
            actor: actor.clone(),
            origin: Origin("cli".into()),
            idempotency_key: "seed-i-a".into(),
            body: CommandBody::Submit {
                intent: "existing owner".into(),
                sources: vec![SourceSnapshot {
                    issue: issue("i-a"),
                    source_revision: "rev-0".into(),
                    content_digest: "digest-0".into(),
                }],
                constraints: vec![],
                verification_argv: vec![],
            },
        },
        now(),
    )
    .unwrap();
    assert!(matches!(seeded, CommandDecision::Accepted { .. }));

    // The grouped proposal admission would submit: two sources, one
    // intent — exactly what the fixture's group candidate produces.
    let mk_source = |id: &str| SourceBinding {
        issue: issue(id),
        revision: format!("rev-{id}"),
        content_digest: format!("digest-{id}"),
    };
    let sources = vec![mk_source("i-a"), mk_source("i-b")];
    let task = TaskInput {
        intent: "group old + group new".into(),
        constraints: vec!["constraint-a".to_string()],
        verification_argv: vec!["true".to_string()],
    };
    let permissions = vec!["perm-a".to_string()];
    let proposal = Proposal {
        key: proposal_key(
            "workloop-fixture-selector",
            1,
            7,
            &sources,
            &task,
            &permissions,
        ),
        selector: AdapterId::trusted("workloop-fixture-selector"),
        selector_version: 1,
        policy_revision: 7,
        sources,
        task,
        required_permissions: permissions,
        provenance: Provenance::default(),
    };
    let command = admission::submit_command(&proposal, actor.clone());
    let decision = handle_command(&store, &cfg, &command, now()).unwrap();

    // The whole submit is refused — the conflict on i-a rejects the
    // candidate, and i-b is never claimed.
    assert!(
        matches!(decision, CommandDecision::Rejected { .. }),
        "grouped submit must reject atomically: {decision:?}"
    );
    let claim_b = store
        .transact_deferred(|tx| tx.issue_claim(&issue("i-b")))
        .unwrap();
    assert!(claim_b.is_none(), "unconflicted member must stay unclaimed");
    let works = store.transact_deferred(|tx| tx.works()).unwrap();
    assert_eq!(works.len(), 1, "no partial Work may be created");
    let _ = dir;
}
