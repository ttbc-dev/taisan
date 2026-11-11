//! s1-bridge scenario contract: the outbox drain reconciles `unknown`
//! intents — re-read the remote fact, settle by it, or re-attempt the
//! guarded write under a bounded attempt count — `applied` intents
//! re-arm only on bound-actor drift receipts, and the capability
//! report declares `supported` only for operations `attempt` can
//! actually apply. `gh`/`git` are stubbed by PATH-resolved shell
//! scripts whose answers the test drives through files under
//! `FAKE_DIR`; the store and the emitted receipts are real.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use workloop_config::load_or_create;
use workloop_core::command::*;
use workloop_core::domain::*;
use workloop_core::identity::*;
use workloop_core::observation::*;
use workloop_core::store::*;
use workloop_core::work::{WorkloopConfig, handle_command};
use workloop_store::sqlite::SqliteStore;

const NOW: i64 = 1_700_000_000;
const BRANCH: &str = "work/w-1";

/// Scripted `gh`: every API answer comes from a file under `FAKE_DIR`
/// so the test can move the remote between ticks.
const FAKE_GH: &str = r#"#!/usr/bin/env bash
set -u
if [ "$1" = "api" ]; then
  case "$2" in
    user) cat "$FAKE_DIR/login" ;;
    "repos/"*"/branches/"*) cat "$FAKE_DIR/remote_head" ;;
    "repos/"*"/compare/"*) cat "$FAKE_DIR/compare_status" ;;
    "repos/"*"/pulls?head="*) cat "$FAKE_DIR/pulls_head" ;;
    "repos/"*"/pulls/"*"/reviews") echo "[]" ;;
    "repos/"*"/commits/"*"/check-runs") echo "[]" ;;
    "repos/"*"/pulls/"*) cat "$FAKE_DIR/pull_json" ;;
    "repos/"*) echo '{"permissions":{"push":true,"pull":true}}' ;;
    *) echo "unhandled gh api: $*" >&2; exit 1 ;;
  esac
elif [ "$1" = "pr" ] && [ "$2" = "create" ]; then
  echo "$*" >> "$FAKE_DIR/pr_creates.log"
  echo "https://example.invalid/pull/7"
elif [ "$1" = "issue" ]; then
  echo '{}'
else
  echo "unhandled gh: $*" >&2
  exit 1
fi
exit 0
"#;

/// Scripted `git`: `push` lands only while `$FAKE_DIR/push_lands`
/// exists — it copies the recorded local head into the recorded remote
/// head, like a real push updating the ref.
const FAKE_GIT: &str = r#"#!/usr/bin/env bash
set -u
case "$1" in
  rev-parse) cat "$FAKE_DIR/local_head" ;;
  ls-remote) cat "$FAKE_DIR/prior_ref" ;;
  push)
    echo "$*" >> "$FAKE_DIR/pushes.log"
    if [ -f "$FAKE_DIR/push_lands" ]; then
      cat "$FAKE_DIR/local_head" > "$FAKE_DIR/remote_head"
    fi
    ;;
  fetch|diff) : ;;
  merge-base) exit 1 ;;
  *) : ;;
esac
exit 0
"#;

struct Fixture {
    fake: PathBuf,
    bin: PathBuf,
    db: PathBuf,
    config: PathBuf,
    workspace: PathBuf,
}

impl Fixture {
    fn new(name: &str) -> Self {
        let root = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("s1-{name}"));
        let _ = fs::remove_dir_all(&root);
        let fake = root.join("fake");
        let bin = root.join("bin");
        let workspace = root.join("workspaces");
        fs::create_dir_all(&fake).unwrap();
        fs::create_dir_all(&bin).unwrap();
        fs::create_dir_all(&workspace).unwrap();
        for (file, body) in [("gh", FAKE_GH), ("git", FAKE_GIT)] {
            let path = bin.join(file);
            fs::write(&path, body).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        }
        let config = root.join("installation.json");
        fs::write(
            &config,
            serde_json::to_string_pretty(&serde_json::json!({
                "actor_id": "op",
                "actor_kind": "human",
                "host_instance": "github.test",
                "repository": "owner/repo",
                "adapter": "s1-bridge",
                "contract_version": 1,
                "capabilities": [
                    "repository_read",
                    "push_ref",
                    "change_request_create",
                    "change_request_read",
                    "checks_read",
                    "review_findings_read",
                    "issue_read"
                ],
                "host_actors": { "publication": "pub-agent" },
                "required_checks": [],
                "capacity": 1,
                "worker_argv": [],
                "workspace_root": workspace,
                "allowed_effects": {
                    "workspace_write": true,
                    "host_write": true,
                    "network": false
                }
            }))
            .unwrap(),
        )
        .unwrap();
        let fx = Fixture {
            db: root.join("workloop.db"),
            fake,
            bin,
            config,
            workspace,
        };
        fx.write_fake("login", "pub-agent\n");
        fx.write_fake("local_head", "aaaa1111\n");
        fx.write_fake("prior_ref", &format!("cccc3333\trefs/heads/{BRANCH}\n"));
        fx.write_fake("remote_head", "bbbb2222\n");
        fx.write_fake("compare_status", "diverged\n");
        fx.write_fake("pulls_head", "[]\n");
        fx.write_fake("pull_json", "{}\n");
        fx
    }

    fn write_fake(&self, name: &str, body: &str) {
        fs::write(self.fake.join(name), body).unwrap();
    }

    fn mark_push_lands(&self) {
        self.write_fake("push_lands", "");
    }

    fn pushes(&self) -> Vec<String> {
        fs::read_to_string(self.fake.join("pushes.log"))
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }

    fn store(&self) -> SqliteStore {
        SqliteStore::open(&self.db, 1).unwrap()
    }

    fn config(&self) -> WorkloopConfig {
        load_or_create(&self.config)
            .unwrap()
            .workloop_config()
            .unwrap()
    }

    /// One bridge pass against the scripted remote. Output is asserted
    /// by callers when a failure is in scope; a nonzero exit is a test
    /// bug to surface, not a case to accept.
    fn tick(&self, extra: &[&str]) -> std::process::Output {
        let path = format!(
            "{}:{}",
            self.bin.display(),
            std::env::var("PATH").unwrap_or_default()
        );
        let out = Command::new(env!("CARGO_BIN_EXE_s1_bridge"))
            .arg("tick")
            .arg("--db")
            .arg(&self.db)
            .arg("--config")
            .arg(&self.config)
            .args(extra)
            .env("PATH", path)
            .env("FAKE_DIR", &self.fake)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "s1-bridge tick failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        out
    }

    /// The workspace clone the bridge pushes from — only its `.git`
    /// marker has to exist for the scripted `git`.
    fn seed_workspace(&self, work_id: &WorkId) {
        fs::create_dir_all(
            self.workspace
                .join(format!("ws-{}", work_id.0))
                .join(".git"),
        )
        .unwrap();
    }

    fn intent_status(&self, action_id: &str) -> HostActionStatus {
        self.store()
            .transact(|tx| {
                Ok(tx
                    .action_intents()?
                    .into_iter()
                    .find(|i| i.action_id == action_id)
                    .map(|i| i.status))
            })
            .unwrap()
            .unwrap()
    }

    fn outstanding(&self, work_id: &WorkId) -> Vec<String> {
        self.store()
            .transact(|tx| workloop_core::projection::outstanding_actions(tx, work_id))
            .unwrap()
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
                intent: "do the thing".into(),
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

/// Insert one outbox intent bound to the Work's publication actor,
/// exactly as `record_intent` would have recorded it.
fn insert_intent(
    store: &SqliteStore,
    work_id: &WorkId,
    action: HostAction,
    status: HostActionStatus,
    effects: Vec<NormalizedEffectFact>,
) -> String {
    store
        .transact(|tx| {
            let actor = tx
                .work(work_id)?
                .and_then(|w| w.binding.actor_for(ActorRole::Publication).cloned())
                .expect("publication binding");
            let action_id = tx.alloc_id("act")?;
            let created_seq = tx.alloc_seq()?;
            tx.apply(&Effect::InsertHostActionIntent {
                intent: HostActionIntent {
                    action_id: action_id.clone(),
                    work_id: work_id.clone(),
                    host: HostInstanceId::trusted("github.test"),
                    repository: RepositoryRef::new(
                        HostInstanceId::trusted("github.test"),
                        RepositoryId::trusted("owner/repo"),
                    ),
                    role: ActorRole::Publication,
                    actor,
                    action,
                    payload_digest: "test".into(),
                    status,
                    authenticated_actor: None,
                    effects,
                    created_seq,
                },
            })?;
            Ok(action_id)
        })
        .unwrap()
}

fn push_action() -> HostAction {
    HostAction::PushRef {
        branch: BRANCH.into(),
        produced: CommitRevision::new(
            RepositoryRef::new(
                HostInstanceId::trusted("github.test"),
                RepositoryId::trusted("owner/repo"),
            ),
            "produced-digest",
        )
        .unwrap(),
    }
}

#[test]
fn capability_report_declares_only_implemented_operations_supported() {
    let fx = Fixture::new("capability");
    fx.tick(&[]);
    let reports: Vec<Vec<CapabilityEntry>> = fx
        .store()
        .transact(|tx| {
            Ok(tx
                .all_observations()?
                .iter()
                .filter_map(|o| match &o.fact {
                    ObservationFact::CapabilityReport { capabilities, .. } => {
                        Some(capabilities.clone())
                    }
                    _ => None,
                })
                .collect())
        })
        .unwrap();
    assert_eq!(reports.len(), 1, "one capability report per tick run");
    let report = &reports[0];
    for capability in [
        HostCapability::RepositoryRead,
        HostCapability::BranchRead,
        HostCapability::ChangeRequestRead,
        HostCapability::IssueRead,
        HostCapability::ChecksRead,
        HostCapability::ReviewFindingsRead,
        HostCapability::PushRef,
        HostCapability::ChangeRequestCreate,
    ] {
        let entry = report
            .iter()
            .find(|e| e.capability == capability)
            .unwrap_or_else(|| panic!("report lacks {}", capability.name()));
        assert_eq!(
            entry.support,
            CapabilityState::Supported,
            "{} has a real success path",
            capability.name()
        );
    }
    // The writes `attempt` can only ever record `failed` must not be
    // authorized by the report — a `supported` claim would authorize
    // host effects the bridge cannot perform.
    for capability in [
        HostCapability::ChangeRequestUpdate,
        HostCapability::CommentWrite,
    ] {
        let entry = report
            .iter()
            .find(|e| e.capability == capability)
            .unwrap_or_else(|| panic!("report lacks {}", capability.name()));
        assert_ne!(
            entry.support,
            CapabilityState::Supported,
            "{} has no success path on this bridge",
            capability.name()
        );
        assert!(!entry.usable());
    }
}

#[test]
fn unknown_push_settles_from_the_remote_read_without_resend() {
    let fx = Fixture::new("settle");
    let (work_id, _) = submit(&fx.store(), &fx.config(), "k-settle");
    fx.seed_workspace(&work_id);
    // The remote already carries the pushed head — reconcile must
    // settle by the fact, never resend.
    fx.write_fake("remote_head", "aaaa1111\n");
    let action_id = insert_intent(
        &fx.store(),
        &work_id,
        push_action(),
        HostActionStatus::Unknown {
            reason: "push response lost".into(),
        },
        vec![],
    );
    fx.tick(&[]);
    assert_eq!(fx.intent_status(&action_id), HostActionStatus::Applied);
    assert!(
        fx.pushes().is_empty(),
        "a settled intent is never resent: {:?}",
        fx.pushes()
    );
    assert!(fx.outstanding(&work_id).is_empty());
}

#[test]
fn unknown_push_reattempts_the_guarded_write_when_remote_diverged() {
    let fx = Fixture::new("reattempt");
    let (work_id, _) = submit(&fx.store(), &fx.config(), "k-reattempt");
    fx.seed_workspace(&work_id);
    // Remote does not contain the pushed head; the scripted push lands.
    fx.mark_push_lands();
    let action_id = insert_intent(
        &fx.store(),
        &work_id,
        push_action(),
        HostActionStatus::Unknown {
            reason: "push response lost".into(),
        },
        vec![],
    );
    fx.tick(&[]);
    assert_eq!(fx.intent_status(&action_id), HostActionStatus::Applied);
    assert_eq!(fx.pushes().len(), 1, "exactly one re-attempted write");
    assert!(fx.outstanding(&work_id).is_empty());
}

#[test]
fn unknown_push_retires_at_the_reconcile_bound() {
    let fx = Fixture::new("bound");
    let (work_id, _) = submit(&fx.store(), &fx.config(), "k-bound");
    fx.seed_workspace(&work_id);
    // The scripted push never lands: every reconcile re-attempt reads
    // back the same diverged remote.
    let action_id = insert_intent(
        &fx.store(),
        &work_id,
        push_action(),
        HostActionStatus::Unknown {
            reason: "push response lost".into(),
        },
        vec![],
    );
    for _ in 0..3 {
        fx.tick(&["--reconcile-bound", "2"]);
    }
    assert_eq!(
        fx.intent_status(&action_id),
        HostActionStatus::Failed {
            reason: "reconcile bound 2 reached; remote effect still unverified".into()
        }
    );
    assert_eq!(
        fx.pushes().len(),
        2,
        "re-attempts stop at the bound — the intent does not reconcile forever"
    );
    assert!(
        fx.outstanding(&work_id).is_empty(),
        "a retired intent is not outstanding"
    );
}

/// The U8.2 gap end to end: an applied push drifts, the bridge's
/// verify pass records it `unknown` under the bound actor, Core
/// re-arms the intent, and the next drain's bounded reconcile
/// re-attempts the push to a clean `applied` — the publish chain is
/// re-armable instead of stuck behind a no-op receipt.
#[test]
fn applied_push_rearms_on_bound_actor_drift_and_resettles() {
    let fx = Fixture::new("rearm");
    let (work_id, _) = submit(&fx.store(), &fx.config(), "k-rearm");
    fx.seed_workspace(&work_id);
    // The intent's recorded receipt says `aaaa1111` landed; the remote
    // moved to `bbbb2222` and no longer contains it.
    let action_id = insert_intent(
        &fx.store(),
        &work_id,
        push_action(),
        HostActionStatus::Applied,
        vec![NormalizedEffectFact::RefPublished {
            repository: RepositoryRef::new(
                HostInstanceId::trusted("github.test"),
                RepositoryId::trusted("owner/repo"),
            ),
            branch: BRANCH.into(),
            produced: CommitRevision::new(
                RepositoryRef::new(
                    HostInstanceId::trusted("github.test"),
                    RepositoryId::trusted("owner/repo"),
                ),
                "produced-digest",
            )
            .unwrap(),
            observed: CommitRevision::new(
                RepositoryRef::new(
                    HostInstanceId::trusted("github.test"),
                    RepositoryId::trusted("owner/repo"),
                ),
                "aaaa1111",
            )
            .unwrap(),
            relation: RefRelation::Equal,
        }],
    );
    fx.tick(&[]);
    assert_eq!(
        fx.intent_status(&action_id),
        HostActionStatus::Unknown {
            reason: "remote work/w-1 no longer contains the pushed revision".into()
        },
        "the bound-actor drift receipt re-arms the applied intent"
    );
    assert_eq!(fx.outstanding(&work_id), vec![action_id.clone()]);

    fx.mark_push_lands();
    fx.tick(&[]);
    assert_eq!(
        fx.intent_status(&action_id),
        HostActionStatus::Applied,
        "the re-armed intent reconciles back through the guarded write"
    );
    assert_eq!(fx.pushes().len(), 1);
    assert!(fx.outstanding(&work_id).is_empty());
}
