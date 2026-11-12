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

    fn pr_creates(&self) -> Vec<String> {
        // Create bodies can embed newlines (the action marker), so
        // count invocation lines rather than raw lines.
        fs::read_to_string(self.fake.join("pr_creates.log"))
            .unwrap_or_default()
            .lines()
            .filter(|l| l.starts_with("pr create"))
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

    /// Recorded `host_action_result` receipts for one action.
    fn result_receipts(&self, action_id: &str) -> usize {
        self.store()
            .transact(|tx| {
                Ok(tx
                    .all_observations()?
                    .iter()
                    .filter(|o| {
                        matches!(
                            &o.fact,
                            ObservationFact::HostActionResult { action_id: id, .. }
                                if id == action_id
                        )
                    })
                    .count())
            })
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

/// The `RefPublished` effect an applied push intent recorded: remote
/// head `aaaa1111` observed equal to the pushed revision.
fn ref_published() -> NormalizedEffectFact {
    NormalizedEffectFact::RefPublished {
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
    fx.write_fake("prior_ref", &format!("aaaa1111\trefs/heads/{BRANCH}\n"));
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
fn unknown_push_holds_for_operator_at_the_reconcile_bound() {
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
    // Bound exhaustion holds the intent `unknown` for operator action —
    // it is never converted into a `failed` receipt, which Core would
    // charge to the transport account and blind-retry while the remote
    // effect is still unverified (docs/recovery-policy.md,
    // docs/evidence-requirements.md R42).
    assert_eq!(
        fx.intent_status(&action_id),
        HostActionStatus::Unknown {
            reason: "reconcile bound 2 reached; remote effect still unverified — held for operator action".into()
        }
    );
    assert_eq!(
        fx.pushes().len(),
        2,
        "re-attempts stop at the bound — the intent does not reconcile forever"
    );
    assert_eq!(
        fx.outstanding(&work_id),
        vec![action_id.clone()],
        "a held intent stays outstanding — no dependent write or terminal settle proceeds unverified"
    );
}

/// The verify receipt that re-arms an applied push is an evidence
/// refresh, not an attempt: it must not count against the reconcile
/// bound. At bound 1 the re-armed intent still gets its one reconcile
/// pass instead of exhausting on the verify receipt alone.
#[test]
fn rearmed_push_reconciles_against_attempt_receipts_only() {
    let fx = Fixture::new("bound-verify");
    let (work_id, _) = submit(&fx.store(), &fx.config(), "k-bound-verify");
    fx.seed_workspace(&work_id);
    let action_id = insert_intent(
        &fx.store(),
        &work_id,
        push_action(),
        HostActionStatus::Applied,
        vec![ref_published()],
    );
    fx.tick(&[]);
    assert!(matches!(
        fx.intent_status(&action_id),
        HostActionStatus::Unknown { .. }
    ));
    fx.mark_push_lands();
    fx.tick(&["--reconcile-bound", "1"]);
    assert_eq!(
        fx.intent_status(&action_id),
        HostActionStatus::Applied,
        "the re-armed intent gets its reconcile pass — the verify receipt did not spend the bound"
    );
    assert_eq!(fx.pushes().len(), 1);
}

/// Repeat drift to an already-reported head still re-arms: the verify
/// receipt's delivery id carries an ordinal, so a second
/// `verify:<sha>:diverged` records instead of deduping against the
/// first.
#[test]
fn applied_push_rearms_again_on_repeat_drift_to_the_same_head() {
    let fx = Fixture::new("rearm-repeat");
    let (work_id, _) = submit(&fx.store(), &fx.config(), "k-rearm-repeat");
    fx.seed_workspace(&work_id);
    let action_id = insert_intent(
        &fx.store(),
        &work_id,
        push_action(),
        HostActionStatus::Applied,
        vec![ref_published()],
    );
    // First drift re-arms; the re-attempted push lands and resettles.
    fx.tick(&[]);
    assert!(matches!(
        fx.intent_status(&action_id),
        HostActionStatus::Unknown { .. }
    ));
    fx.mark_push_lands();
    fx.tick(&[]);
    assert_eq!(fx.intent_status(&action_id), HostActionStatus::Applied);
    // The remote drifts back to the same head the first verify receipt
    // already reported diverged — the identical report must still
    // record and re-arm.
    fx.write_fake("remote_head", "bbbb2222\n");
    fx.tick(&[]);
    assert!(
        matches!(
            fx.intent_status(&action_id),
            HostActionStatus::Unknown { .. }
        ),
        "repeat drift to the same head re-arms: {:?}",
        fx.intent_status(&action_id)
    );
}

/// One intent's reconcile failure is contained: sibling intents still
/// reconcile and the pass still finishes. The failing intent itself
/// still records its reconcile receipt — an erroring read is unverified
/// evidence, not a reason to retry the same pass forever (the bound
/// must advance past it).
#[test]
fn reconcile_continues_past_one_intents_failure() {
    let fx = Fixture::new("isolation");
    let (work_id, _) = submit(&fx.store(), &fx.config(), "k-iso");
    fx.seed_workspace(&work_id);
    // First intent (insertion order is reconcile order): an unknown
    // push whose remote read-back cannot construct a CommitRevision —
    // `compare` reports `ahead` over an oversized observed head.
    let failing = insert_intent(
        &fx.store(),
        &work_id,
        push_action(),
        HostActionStatus::Unknown {
            reason: "push response lost".into(),
        },
        vec![],
    );
    // Second intent reconciles cleanly from the pulls read.
    let settled = insert_intent(
        &fx.store(),
        &work_id,
        HostAction::OpenChangeRequest {
            source_branch: BRANCH.into(),
            target_branch: "main".into(),
            title: "t".into(),
            body: "b".into(),
        },
        HostActionStatus::Unknown {
            reason: "create response lost".into(),
        },
        vec![],
    );
    fx.write_fake(
        "prior_ref",
        &format!("{}\trefs/heads/{BRANCH}\n", "a".repeat(600)),
    );
    fx.write_fake("remote_head", "\n");
    fx.write_fake("compare_status", "ahead\n");
    fx.write_fake(
        "pulls_head",
        &format!(
            r#"[{{"node_id":"PR_x","number":7,"head":{{"sha":"aaaa1111"}},"base":{{"ref":"main"}},"body":"b\n\n<!-- workloop-action:{settled} -->"}}]
"#,
        ),
    );
    fx.tick(&[]);
    assert_eq!(
        fx.intent_status(&settled),
        HostActionStatus::Applied,
        "the sibling intent reconciles past the failing one"
    );
    assert!(matches!(
        fx.intent_status(&failing),
        HostActionStatus::Unknown { .. }
    ));
    assert_eq!(
        fx.result_receipts(&failing),
        1,
        "the unparseable read-back still records the reconcile receipt — the bound advances"
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
        vec![ref_published()],
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

/// A provably absent remote branch is a definite "effect did not
/// land" — the reconcile re-attempts the guarded write instead of
/// recording `unknown` and burning the bound on an intent whose effect
/// never landed.
#[test]
fn unknown_push_reattempts_when_the_remote_branch_is_absent() {
    let fx = Fixture::new("absent");
    let (work_id, _) = submit(&fx.store(), &fx.config(), "k-absent");
    fx.seed_workspace(&work_id);
    fx.write_fake("prior_ref", "");
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
    assert_eq!(
        fx.pushes().len(),
        1,
        "the absent branch re-attempts the push"
    );
}

/// A reconcile re-try that cannot run leaves the effect `unknown` —
/// never `failed`, which Core would charge to the transport account
/// and blind-retry while the original effect is still unverified
/// (docs/evidence-requirements.md R42).
#[test]
fn reconcile_reattempt_without_workspace_stays_unknown() {
    let fx = Fixture::new("no-workspace");
    let (work_id, _) = submit(&fx.store(), &fx.config(), "k-no-ws");
    // Deliberately no seeded workspace clone: the re-attempt cannot run.
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
    assert!(
        matches!(
            fx.intent_status(&action_id),
            HostActionStatus::Unknown { .. }
        ),
        "an unverified effect never becomes a failed receipt: {:?}",
        fx.intent_status(&action_id)
    );
    assert!(fx.pushes().is_empty());
}

/// An unchanged applied push re-verifies under the same delivery id
/// and dedupes at the store: the verify pass must not mint a new
/// `applied` receipt every tick — each one grows the observation table
/// and resets the Work's no-progress clock.
#[test]
fn verify_receipt_dedupes_an_unchanged_applied_push() {
    let fx = Fixture::new("verify-dedupe");
    let (work_id, _) = submit(&fx.store(), &fx.config(), "k-verify-dedupe");
    fx.seed_workspace(&work_id);
    let action_id = insert_intent(
        &fx.store(),
        &work_id,
        push_action(),
        HostActionStatus::Applied,
        vec![ref_published()],
    );
    // The remote still carries the head the recorded receipt verified.
    fx.write_fake("remote_head", "aaaa1111\n");
    fx.tick(&[]);
    let after_first = fx.result_receipts(&action_id);
    assert!(after_first >= 1, "the first verify observation records");
    fx.tick(&[]);
    fx.tick(&[]);
    assert_eq!(
        fx.result_receipts(&action_id),
        after_first,
        "an unchanged push dedupes — no fresh applied observation per tick"
    );
    assert_eq!(fx.intent_status(&action_id), HostActionStatus::Applied);
}

/// A pre-existing open change from the same source branch to a
/// different base is not this intent's effect: the reconcile holds the
/// intent `unknown` instead of settling `applied` on a target the
/// remote never showed — and never re-issues `gh pr create` either,
/// since a list that is merely stale would make the re-issued create
/// the duplicate R-4 forbids.
#[test]
fn reconcile_open_holds_unknown_when_the_change_targets_another_base() {
    let fx = Fixture::new("open-base");
    let (work_id, _) = submit(&fx.store(), &fx.config(), "k-open-base");
    let action_id = insert_intent(
        &fx.store(),
        &work_id,
        HostAction::OpenChangeRequest {
            source_branch: BRANCH.into(),
            target_branch: "main".into(),
            title: "t".into(),
            body: "b".into(),
        },
        HostActionStatus::Unknown {
            reason: "create response lost".into(),
        },
        vec![],
    );
    fx.write_fake(
        "pulls_head",
        r#"[{"node_id":"PR_x","number":7,"head":{"sha":"aaaa1111"},"base":{"ref":"release"}}]
"#,
    );
    fx.tick(&[]);
    assert_ne!(
        fx.intent_status(&action_id),
        HostActionStatus::Applied,
        "a different-base change never settles the intent applied"
    );
    assert!(
        fx.pr_creates().is_empty(),
        "reconcile settles by read only — never a second create"
    );
    // Once the remote answers an open change on the intent's own base
    // carrying this intent's action marker, the next pass settles it —
    // the marker proves the found change is this intent's effect, not
    // a pre-existing one on the same branches.
    fx.write_fake(
        "pulls_head",
        &format!(
            r#"[{{"node_id":"PR_y","number":8,"head":{{"sha":"aaaa1111"}},"base":{{"ref":"main"}},"body":"b\n\n<!-- workloop-action:{action_id} -->"}}]
"#,
        ),
    );
    fx.tick(&[]);
    assert_eq!(fx.intent_status(&action_id), HostActionStatus::Applied);
}

/// A store carrying an older report that marked unimplemented writes
/// `supported` gets a corrected report superseding it — the recorded
/// report is never the final word when this build's support surface
/// differs (Core reads the latest report).
#[test]
fn a_stale_capability_report_is_superseded() {
    let fx = Fixture::new("cap-stale");
    let supported = |capability: HostCapability| CapabilityEntry {
        capability,
        support: CapabilityState::Supported,
        authorization: CapabilityState::Authorized,
    };
    // The pre-change report: every write wrongly `supported`.
    let stale = Observation {
        delivery: Some(DeliveryId {
            scope: DeliveryScope::Host {
                host: HostInstanceId::trusted("github.test"),
            },
            source: "github.s1-bridge".into(),
            event_id: "cap:0".into(),
        }),
        subject_work_id: None,
        subject_execution_id: None,
        fact: ObservationFact::CapabilityReport {
            host: HostInstanceId::trusted("github.test"),
            capabilities: [
                HostCapability::RepositoryRead,
                HostCapability::BranchRead,
                HostCapability::ChangeRequestRead,
                HostCapability::IssueRead,
                HostCapability::ChecksRead,
                HostCapability::ReviewFindingsRead,
                HostCapability::PushRef,
                HostCapability::ChangeRequestCreate,
                HostCapability::ChangeRequestUpdate,
                HostCapability::CommentWrite,
            ]
            .into_iter()
            .map(supported)
            .collect(),
        },
        observed_unix_seconds: NOW,
    };
    fx.store()
        .transact(|tx| tx.apply(&Effect::InsertObservation { observation: stale }))
        .unwrap();
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
    assert_eq!(
        reports.len(),
        2,
        "the stale report is superseded, not trusted forever"
    );
    let latest = reports.last().unwrap();
    for capability in [
        HostCapability::ChangeRequestUpdate,
        HostCapability::CommentWrite,
    ] {
        let entry = latest
            .iter()
            .find(|e| e.capability == capability)
            .unwrap_or_else(|| panic!("report lacks {}", capability.name()));
        assert_eq!(
            entry.support,
            CapabilityState::Denied,
            "the corrected report denies the unimplemented {}",
            capability.name()
        );
    }
    // A second tick emits no third report — a current report is not
    // re-emitted.
    fx.tick(&[]);
    let count = fx
        .store()
        .transact(|tx| {
            Ok(tx
                .all_observations()?
                .iter()
                .filter(|o| matches!(o.fact, ObservationFact::CapabilityReport { .. }))
                .count())
        })
        .unwrap();
    assert_eq!(count, 2, "a current report is not re-emitted per tick");
}

/// A lost create response must never produce a second `gh pr create`:
/// the read-back failure records `unknown` and hands the intent to the
/// reconcile read instead of letting the drain re-issue the write —
/// the lost-response case R-4 forbids a duplicate create.
#[test]
fn a_lost_create_response_never_creates_twice() {
    let fx = Fixture::new("lost-create");
    let (work_id, _) = submit(&fx.store(), &fx.config(), "k-lost-create");
    let action_id = insert_intent(
        &fx.store(),
        &work_id,
        HostAction::OpenChangeRequest {
            source_branch: BRANCH.into(),
            target_branch: "main".into(),
            title: "t".into(),
            body: "b".into(),
        },
        HostActionStatus::Pending,
        vec![],
    );
    // The create lands but the pulls read-back is unparseable.
    fx.write_fake("pulls_head", "not-json\n");
    fx.tick(&[]);
    fx.tick(&[]);
    assert_eq!(
        fx.pr_creates().len(),
        1,
        "exactly one create — the unverified effect never duplicates"
    );
    assert!(
        matches!(
            fx.intent_status(&action_id),
            HostActionStatus::Unknown { .. }
        ),
        "the unverified create stays outstanding: {:?}",
        fx.intent_status(&action_id)
    );
    // When the remote later lists the change this intent actually
    // created — identifiable by the embedded action marker — the
    // reconcile adopts it and settles, still without a second create.
    fx.write_fake(
        "pulls_head",
        &format!(
            r#"[{{"node_id":"PR_z","number":9,"head":{{"sha":"aaaa1111"}},"base":{{"ref":"main"}},"body":"b\n\n<!-- workloop-action:{action_id} -->"}}]
"#,
        ),
    );
    fx.tick(&[]);
    assert_eq!(fx.intent_status(&action_id), HostActionStatus::Applied);
    assert_eq!(fx.pr_creates().len(), 1);
}

/// A pre-existing open change on the same source/base pair is *not*
/// this intent's effect: after a lost create response nothing
/// distinguishes it from the change this intent may have made, so a
/// body without this action's marker stays `unknown` — adoption would
/// borrow another change's identity for the `ChangeOpened` effect.
#[test]
fn a_preexisting_change_without_the_marker_is_never_adopted() {
    let fx = Fixture::new("unmarked-pr");
    let (work_id, _) = submit(&fx.store(), &fx.config(), "k-unmarked-pr");
    let action_id = insert_intent(
        &fx.store(),
        &work_id,
        HostAction::OpenChangeRequest {
            source_branch: BRANCH.into(),
            target_branch: "main".into(),
            title: "t".into(),
            body: "b".into(),
        },
        HostActionStatus::Unknown {
            reason: "create response lost".into(),
        },
        vec![],
    );
    // The remote lists an open change on the intent's head/base that
    // carries a *different* marker (or none): ambiguity, not adoption.
    fx.write_fake(
        "pulls_head",
        r#"[{"node_id":"PR_old","number":5,"head":{"sha":"aaaa1111"},"base":{"ref":"main"},"body":"someone else's change\n\n<!-- workloop-action:act_other -->"}]
"#,
    );
    for _ in 0..2 {
        fx.tick(&[]);
    }
    assert!(
        matches!(
            fx.intent_status(&action_id),
            HostActionStatus::Unknown { .. }
        ),
        "a foreign or unmarked change is never adopted: {:?}",
        fx.intent_status(&action_id)
    );
    assert!(fx.pr_creates().is_empty());
}

/// `pending` does not prove the intent was never attempted: a crash
/// after the push landed but before the receipt wrote leaves the
/// intent `pending` with a landed-but-unrecorded effect. So a
/// first-visible drain failure — no workspace clone, local head
/// unreadable, prior-ref read failed — records `unknown`, never
/// `failed` (a `failed` receipt would tell Core the effect provably
/// did not happen and let it charge the transport account and
/// blind-retry — R42).
#[test]
fn a_pending_push_with_no_workspace_stays_unknown() {
    let fx = Fixture::new("pending-no-clone");
    let (work_id, _) = submit(&fx.store(), &fx.config(), "k-pending-no-clone");
    // No `seed_workspace` — the clone the push would run from is
    // missing, exactly the post-push-crash recovery shape.
    let action_id = insert_intent(
        &fx.store(),
        &work_id,
        push_action(),
        HostActionStatus::Pending,
        vec![],
    );
    // The remote shows the branch absent: if the push truly never ran,
    // the reconcile re-attempt hits the same missing clone.
    fx.write_fake("prior_ref", "");
    for _ in 0..2 {
        fx.tick(&[]);
    }
    assert!(
        matches!(
            fx.intent_status(&action_id),
            HostActionStatus::Unknown { .. }
        ),
        "a missing clone never proves the unrecorded push did not land: {:?}",
        fx.intent_status(&action_id)
    );
    assert!(fx.pushes().is_empty());
}

/// A reconcile read that resolves but cannot be validated — the remote
/// head read-back cannot construct a `CommitRevision` — still records
/// the pass's `unknown` receipt, so the bound advances past the
/// erroring read instead of retrying it every tick forever.
#[test]
fn reconcile_bound_advances_past_unparseable_remote_reads() {
    let fx = Fixture::new("bound-errors");
    let (work_id, _) = submit(&fx.store(), &fx.config(), "k-bound-errors");
    fx.seed_workspace(&work_id);
    let action_id = insert_intent(
        &fx.store(),
        &work_id,
        push_action(),
        HostActionStatus::Unknown {
            reason: "push response lost".into(),
        },
        vec![],
    );
    // An oversized remote head: `ls-remote` resolves, `compare`
    // answers, and the observed sha fails `CommitRevision::new`.
    fx.write_fake(
        "prior_ref",
        &format!("{}\trefs/heads/{BRANCH}\n", "a".repeat(600)),
    );
    fx.write_fake("compare_status", "ahead\n");
    for _ in 0..3 {
        fx.tick(&["--reconcile-bound", "2"]);
    }
    let HostActionStatus::Unknown { reason } = fx.intent_status(&action_id) else {
        panic!("expected unknown, got {:?}", fx.intent_status(&action_id));
    };
    assert_eq!(
        reason,
        "reconcile bound 2 reached; remote effect still unverified — held for operator action"
    );
    assert_eq!(
        fx.result_receipts(&action_id),
        3,
        "one receipt per reconcile pass plus the bound-exhaustion receipt"
    );
    assert!(fx.pushes().is_empty());
}
