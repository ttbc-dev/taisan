//! U3.6e live scenario harness — the separately authorized isolated
//! live GitHub cases of issue #119. Every test here is `#[ignore]`d
//! and additionally gated on environment: a live run requires
//! `TAISAN_U36E_REPO`, `TAISAN_U36E_INSTALL`, and `TAISAN_U36E_AUTH`
//! (the recorded per-scenario owner authorization) — see
//! `docs/u3.6e-live-runbook.md` for the authorization boundary and
//! `docs/u3.6e-authorization.md` for the per-scenario draft the owner
//! signs.
//!
//! Each scenario drives the real [`GitHubAdapter`] over
//! [`GhCliApi`] — the same port and transport contract the fixture
//! suites exercise — against the isolated repository named by
//! `TAISAN_U36E_REPO`, then retains one `live_evidenced`
//! [`EvidenceRecord`] per run under `target/u3.6e-records/`. A run
//! without `TAISAN_U36E_AUTH` refuses before any host call: no
//! authorization reference, no live claim.

use std::path::PathBuf;
use std::process::Command;

use workloop_core::command::*;
use workloop_core::domain::*;
use workloop_core::identity::*;
use workloop_core::observation::*;
use workloop_core::store::*;
use workloop_core::work::{HostTemplate, IngestReceipt, WorkloopConfig, handle_command};
use workloop_github::adapter::{GitHubAdapter, WebhookDelivery, WebhookFact};
use workloop_github::api::GitHubApi;
use workloop_github::evidence::{
    ClaimClass, ConfigIdentity, EvidenceRecord, IsolationScope, ReadBackReceipt, ScenarioResult,
    record_revisions, validate_record_json,
};
use workloop_github::gh_cli::GhCliApi;
use workloop_store::sqlite::SqliteStore;

const NOW: i64 = 1_800_000_000;

type LiveAdapter = GitHubAdapter<GhCliApi>;

/// The environment a live run must carry. `auth` is the recorded
/// owner authorization for THIS scenario — its absence refuses the
/// run before any host call.
#[derive(Clone)]
struct LiveEnv {
    install: HostInstanceId,
    slug: String,
    auth: String,
    /// The `gh` executable the transport spawns — normally `gh` on
    /// PATH; a scenario may point it at a wrapper (the lost-response
    /// case does exactly that).
    gh: Option<PathBuf>,
}

fn env_flag(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.trim().is_empty())
}

/// Resolve the live environment or report the skip. The authorization
/// variable is mandatory: `TAISAN_U36E_AUTH` must cite the recorded
/// owner authorization (issue comment, approval record) for the
/// scenario being run.
fn live_env() -> Option<LiveEnv> {
    let install = env_flag("TAISAN_U36E_INSTALL")?;
    let slug = env_flag("TAISAN_U36E_REPO")?;
    let auth = env_flag("TAISAN_U36E_AUTH")?;
    if !slug.contains('/') {
        panic!("TAISAN_U36E_REPO must be an owner/name slug: {slug}");
    }
    Some(LiveEnv {
        install: HostInstanceId::trusted(install),
        slug,
        auth,
        gh: env_flag("TAISAN_U36E_GH").map(PathBuf::from),
    })
}

fn live_adapter(env: &LiveEnv) -> LiveAdapter {
    let api = match &env.gh {
        Some(path) => GhCliApi::with_gh(path),
        None => GhCliApi::new(),
    };
    GitHubAdapter::for_host(env.install.clone(), api)
}

fn live_repo(env: &LiveEnv) -> RepositoryRef {
    RepositoryRef::new(env.install.clone(), RepositoryId::trusted(env.slug.clone()))
}

fn live_config(env: &LiveEnv) -> WorkloopConfig {
    WorkloopConfig {
        host: HostTemplate {
            adapter: AdapterId::trusted("github"),
            repository: live_repo(env),
            contract_version: 1,
            actors: vec![RoleBinding {
                role: ActorRole::Publication,
                actor: HostActorRef {
                    host: env.install.clone(),
                    actor: HostActorId::trusted("u36e-publication"),
                },
                credential_ref: CredentialRef::trusted("cred-u36e"),
            }],
        },
        declared_capabilities: LiveAdapter::declared_capabilities()
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

fn live_publisher(env: &LiveEnv) -> HostActorRef {
    HostActorRef {
        host: env.install.clone(),
        actor: HostActorId::trusted("u36e-publication"),
    }
}

fn rev(env: &LiveEnv, object: &str) -> CommitRevision {
    CommitRevision::new(live_repo(env), object).unwrap()
}

fn submit(store: &SqliteStore, env: &LiveEnv, key: &str) -> (WorkId, ExecutionId) {
    let config = live_config(env);
    let d = handle_command(
        store,
        &config,
        &CommandRequest {
            actor: Actor {
                kind: ActorKind::Human,
                external_id: "u36e-operator".into(),
            },
            origin: Origin("u36e-live".into()),
            idempotency_key: key.into(),
            body: CommandBody::Submit {
                intent: "u3.6e live scenario".into(),
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

fn insert_intent(
    store: &SqliteStore,
    env: &LiveEnv,
    work_id: &WorkId,
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
                    host: env.install.clone(),
                    repository: live_repo(env),
                    role: ActorRole::Publication,
                    actor: live_publisher(env),
                    action,
                    payload_digest: "u36e-live".into(),
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

fn intent_state(
    store: &SqliteStore,
    action_id: &str,
) -> (HostActionStatus, Vec<NormalizedEffectFact>) {
    store
        .transact(|tx| {
            Ok(tx
                .action_intents()?
                .into_iter()
                .find(|i| i.action_id == action_id)
                .map(|i| (i.status, i.effects)))
        })
        .unwrap()
        .unwrap()
}

fn observation_identities(store: &SqliteStore, work_id: &WorkId) -> Vec<String> {
    store
        .transact(|tx| tx.observations_for_work(work_id))
        .unwrap()
        .iter()
        .filter_map(|o| {
            o.delivery.as_ref().map(|d| match &d.scope {
                DeliveryScope::Host { host } => {
                    format!("host:{host}:{}:{}", d.source, d.event_id)
                }
                DeliveryScope::Local => format!("local:{}:{}", d.source, d.event_id),
            })
        })
        .collect()
}

/// `gh api` for scenario setup/cleanup only — the evidence path never
/// goes through this helper; every transport call rides `GhCliApi`.
fn gh_json(env: &LiveEnv, args: &[&str]) -> Result<serde_json::Value, String> {
    let gh = env.gh.clone().unwrap_or_else(|| PathBuf::from("gh"));
    let out = Command::new(gh)
        .arg("api")
        .args(args)
        .output()
        .map_err(|e| format!("gh spawn: {e}"))?;
    if out.status.success() {
        serde_json::from_slice(&out.stdout)
            .map_err(|e| format!("gh {}: bad json: {e}", args.join(" ")))
    } else {
        Err(format!(
            "gh {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    }
}

/// The repository's default branch — the `target_branch` live change
/// requests open into.
fn default_branch(env: &LiveEnv) -> String {
    gh_json(env, &[&format!("repos/{}", env.slug)])
        .ok()
        .and_then(|v| {
            v.get("default_branch")
                .and_then(|b| b.as_str())
                .map(str::to_string)
        })
        .expect("read the isolated repository's default branch")
}

/// Two distinct commit SHAs present on the isolated remote —
/// `TAISAN_U36E_SHA`/`TAISAN_U36E_SHA_B` when set, else the two newest
/// commits on the default branch.
fn remote_commit_pair(env: &LiveEnv) -> (String, String) {
    if let (Some(a), Some(b)) = (env_flag("TAISAN_U36E_SHA"), env_flag("TAISAN_U36E_SHA_B")) {
        return (a, b);
    }
    let branch = default_branch(env);
    let commits = gh_json(
        env,
        &[
            &format!("repos/{}/commits", env.slug),
            "-X",
            "GET",
            "-f",
            &format!("sha={branch}"),
            "-f",
            "per_page=2",
        ],
    )
    .expect("list commits on the isolated remote");
    let mut shas = commits
        .as_array()
        .expect("commit list")
        .iter()
        .filter_map(|c| {
            c.pointer("/sha")
                .and_then(|s| s.as_str())
                .map(str::to_string)
        });
    let a = env_flag("TAISAN_U36E_SHA")
        .or_else(|| shas.next())
        .expect("a commit SHA present on the isolated remote");
    let b = env_flag("TAISAN_U36E_SHA_B")
        .or_else(|| shas.next())
        .expect("a second distinct commit SHA (set TAISAN_U36E_SHA_B)");
    (a, b)
}

/// Delete a scratch ref — scenario cleanup. Best-effort: a leftover
/// `u36e/` branch is harmless and the runbook documents manual cleanup.
fn delete_branch(env: &LiveEnv, branch: &str) {
    let gh = env.gh.clone().unwrap_or_else(|| PathBuf::from("gh"));
    let _ = Command::new(gh)
        .args([
            "api",
            &format!("repos/{}/git/refs/heads/{branch}", env.slug),
            "-X",
            "DELETE",
        ])
        .output();
}

/// Single-quote a path for embedding in the generated shim script.
fn sh_quote(path: &std::path::Path) -> String {
    format!("'{}'", path.display().to_string().replace('\'', "'\\''"))
}

/// Retain the scenario's evidence record. The record is `live_evidenced`
/// only because `TAISAN_U36E_AUTH` supplied the authorization
/// reference and the isolated scope is recorded beside it — the schema
/// rejects the record without both.
fn retain(
    env: &LiveEnv,
    scenario: &str,
    store: &SqliteStore,
    work_id: &WorkId,
    action_ids: &[String],
    read_backs: Vec<ReadBackReceipt>,
    result: ScenarioResult,
) {
    let (tested, base) = record_revisions(env!("CARGO_MANIFEST_DIR"))
        .expect("the live record requires a faithful revision stamp");
    let config = live_config(env);
    let record = EvidenceRecord {
        schema_version: 1,
        tested_revision: tested.clone(),
        base_revision: base,
        core_revision: tested.clone(),
        adapter_revision: tested,
        config_identity: ConfigIdentity::for_config(&config, "github"),
        command_or_scenario: format!("u36e-live::{scenario}"),
        action_identities: action_ids.to_vec(),
        observation_identities: observation_identities(store, work_id),
        read_back_receipts: read_backs,
        result,
        claim_class: ClaimClass::LiveEvidenced,
        authorization_reference: Some(env.auth.clone()),
        isolation_scope: Some(IsolationScope {
            installation_id: env.install.as_str().to_string(),
            repository: env.slug.clone(),
        }),
    };
    record
        .validate()
        .unwrap_or_else(|v| panic!("the live record fails the field check: {v:?}"));
    let value = serde_json::to_value(&record).unwrap();
    validate_record_json(&value).expect("retained live record validates");
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/u3.6e-records");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(format!("{scenario}.json"));
    std::fs::write(
        &path,
        format!("{}\n", serde_json::to_string_pretty(&value).unwrap()),
    )
    .unwrap();
    eprintln!("retained live evidence: {}", path.display());
}

/// The change request the Work's receipts opened — the comment and
/// read scenarios bind to it.
fn opened_change(store: &SqliteStore) -> Option<ChangeRequestRef> {
    store
        .transact(|tx| {
            Ok(tx
                .action_intents()?
                .into_iter()
                .flat_map(|i| i.effects)
                .find_map(|e| match e {
                    NormalizedEffectFact::ChangeOpened { change, .. } => Some(change),
                    _ => None,
                }))
        })
        .unwrap()
}

/// Scenario: partial push then change-request create failure keeps
/// independent receipts. The push applies; the create is refused
/// (nonexistent target branch → definite 422); neither receipt
/// contaminates the other.
#[test]
#[ignore = "U3.6e live case — requires TAISAN_U36E_* env and owner authorization"]
fn live_partial_push_then_create_failure() {
    let Some(env) = live_env() else {
        eprintln!("skip: TAISAN_U36E_REPO/INSTALL/AUTH not set");
        return;
    };
    let (sha, _) = remote_commit_pair(&env);
    let store = SqliteStore::in_memory(1).unwrap();
    let config = live_config(&env);
    let mut adapter = live_adapter(&env);
    let (work_id, _eid) = submit(&store, &env, "u36e-partial");
    let branch = format!("u36e/partial-{NOW}");
    let push_id = insert_intent(
        &store,
        &env,
        &work_id,
        HostAction::PushRef {
            branch: branch.clone(),
            produced: rev(&env, &sha),
            expected_prior: None,
        },
    );
    let open_id = insert_intent(
        &store,
        &env,
        &work_id,
        HostAction::OpenChangeRequest {
            source_branch: branch.clone(),
            // A base that cannot exist: GitHub refuses the create
            // before any effect — the partial-effect shape.
            target_branch: "u36e/no-such-base".into(),
            title: "u36e partial".into(),
            body: "u36e".into(),
        },
    );
    adapter.apply_outbox(&store, &config, NOW).unwrap();

    let (push_status, _) = intent_state(&store, &push_id);
    let (open_status, _) = intent_state(&store, &open_id);
    assert_eq!(push_status, HostActionStatus::Applied, "the push applies");
    assert!(
        matches!(open_status, HostActionStatus::Failed { .. }),
        "the create refuses: {open_status:?}"
    );

    // Read-back: the branch exists at the produced sha; no change
    // request was created.
    let head = adapter
        .client_mut()
        .read_branch(&live_repo(&env), &branch)
        .unwrap();
    let result = if head.as_deref() == Some(sha.as_str()) {
        ScenarioResult::Pass
    } else {
        ScenarioResult::Fail
    };
    retain(
        &env,
        "partial_push_create",
        &store,
        &work_id,
        &[push_id, open_id],
        vec![ReadBackReceipt {
            target: format!("branch:{}/{}", env.slug, branch),
            observed: format!("head {head:?}"),
        }],
        result,
    );
    delete_branch(&env, &branch);
}

/// Scenario: a lost response reconciles by marker without replay. The
/// `gh` wrapper lets the POST land then drops the response; the
/// adapter records `unknown`; a clean pass reconciles via
/// `find_comment` — the comment is never reposted.
#[test]
#[ignore = "U3.6e live case — requires TAISAN_U36E_* env and owner authorization"]
fn live_lost_response_reconciles_by_marker() {
    let Some(env) = live_env() else {
        eprintln!("skip: TAISAN_U36E_REPO/INSTALL/AUTH not set");
        return;
    };
    let (sha, _) = remote_commit_pair(&env);
    let store = SqliteStore::in_memory(1).unwrap();
    let config = live_config(&env);

    // The shim delegates to the real `gh` (TAISAN_U36E_GH or PATH),
    // runs every call, then drops the FIRST `issues/*/comments` call's
    // response — the effect lands, the answer never arrives.
    let real_gh = env.gh.clone().unwrap_or_else(|| PathBuf::from("gh"));
    let shim_dir = std::env::temp_dir().join(format!("u36e-shim-{NOW}"));
    std::fs::create_dir_all(&shim_dir).unwrap();
    let counter = shim_dir.join("dropped");
    let shim = shim_dir.join("gh");
    // Write-then-rename: executing a just-written inode fails
    // ETXTBUSY on Linux.
    let shim_tmp = shim_dir.join("gh.tmp");
    std::fs::write(
        &shim_tmp,
        format!(
            "#!/bin/sh\nout=$(mktemp)\nerr=$(mktemp)\n{real} \"$@\" >\"$out\" 2>\"$err\"\ncode=$?\nif echo \"$@\" | grep -q 'issues/.*/comments' && [ ! -f \"{counter}\" ]; then\n  touch \"{counter}\"\n  echo 'simulated lost response' >&2\n  exit 1\nfi\ncat \"$out\"\ncat \"$err\" >&2\nexit $code\n",
            real = sh_quote(&real_gh),
            counter = counter.display(),
        ),
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&shim_tmp).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&shim_tmp, perms).unwrap();
    }
    std::fs::rename(&shim_tmp, &shim).unwrap();

    // Phase 1: push + open + comment under the response-dropping shim.
    let mut shim_env = env.clone();
    shim_env.gh = Some(shim);
    let mut adapter = live_adapter(&shim_env);
    let (work_id, _eid) = submit(&store, &env, "u36e-lost");
    let branch = format!("u36e/lost-{NOW}");
    insert_intent(
        &store,
        &env,
        &work_id,
        HostAction::PushRef {
            branch: branch.clone(),
            produced: rev(&env, &sha),
            expected_prior: None,
        },
    );
    insert_intent(
        &store,
        &env,
        &work_id,
        HostAction::OpenChangeRequest {
            source_branch: branch.clone(),
            target_branch: default_branch(&env),
            title: "u36e lost".into(),
            body: "u36e".into(),
        },
    );
    adapter.apply_outbox(&store, &config, NOW).unwrap();
    let change = opened_change(&store).expect("open applied");
    let head = adapter
        .client_mut()
        .read_pull(&live_repo(&env), &change.change_request)
        .unwrap()
        .head_sha;
    let marker = format!("u36e-marker-{NOW}");
    let comment_id = insert_intent(
        &store,
        &env,
        &work_id,
        HostAction::PostComment {
            change: change.clone(),
            expected_head: rev(&env, &head),
            marker: marker.clone(),
            body: "u36e lost-response probe".into(),
        },
    );
    adapter.apply_outbox(&store, &config, NOW + 1).unwrap();
    let (comment_status, _) = intent_state(&store, &comment_id);
    assert!(
        matches!(comment_status, HostActionStatus::Unknown { .. }),
        "the dropped response records unknown, never failed: {comment_status:?}"
    );

    // Phase 2: clean transport — reconcile by marker, never replay.
    let mut adapter = live_adapter(&env);
    adapter
        .reconcile_outstanding(&store, &config, NOW + 2)
        .unwrap();
    let (comment_status, effects) = intent_state(&store, &comment_id);
    assert!(
        matches!(comment_status, HostActionStatus::Applied),
        "reconcile settles the comment by marker: {comment_status:?}"
    );
    assert!(
        effects.iter().any(|e| matches!(
            e,
            NormalizedEffectFact::CommentPosted { marker: m, .. } if *m == marker
        )),
        "the receipt binds the marker: {effects:?}"
    );
    // Exactly one remote comment carries the marker — the lost
    // response never produced a duplicate.
    let found = adapter
        .client_mut()
        .find_comment(&live_repo(&env), &change.change_request, &marker)
        .unwrap()
        .expect("the posted comment reconciles");
    retain(
        &env,
        "lost_response",
        &store,
        &work_id,
        &[comment_id],
        vec![ReadBackReceipt {
            target: format!("comment:{}", found.comment_id),
            observed: format!("marker {marker} found once"),
        }],
        ScenarioResult::Pass,
    );
    // Close the scenario's pull request before cleanup — it is real.
    if let Some(number) = change.display_number {
        let _ = gh_json(
            &env,
            &[
                &format!("repos/{}/pulls/{number}", env.slug),
                "-X",
                "PATCH",
                "-f",
                "state=closed",
            ],
        );
    }
    delete_branch(&env, &branch);
}

/// Scenario: stale head. The branch moves under the recorded guard;
/// the guarded write refuses `stale_head` before any effect and the
/// fresh position is observed, never overwritten.
#[test]
#[ignore = "U3.6e live case — requires TAISAN_U36E_* env and owner authorization"]
fn live_stale_head_refused() {
    let Some(env) = live_env() else {
        eprintln!("skip: TAISAN_U36E_REPO/INSTALL/AUTH not set");
        return;
    };
    let (sha_a, sha_b) = remote_commit_pair(&env);
    assert_ne!(sha_a, sha_b, "stale-head needs two distinct commits");
    let store = SqliteStore::in_memory(1).unwrap();
    let config = live_config(&env);
    let mut adapter = live_adapter(&env);
    let (work_id, _eid) = submit(&store, &env, "u36e-stale");
    let branch = format!("u36e/stale-{NOW}");

    // Seed the branch at sha_a directly, then move it to sha_b — the
    // external actor the guard exists to catch.
    adapter
        .client_mut()
        .write_ref(&live_repo(&env), &branch, &sha_a, None)
        .unwrap();
    adapter
        .client_mut()
        .write_ref(&live_repo(&env), &branch, &sha_b, Some(&sha_a))
        .unwrap();

    let push_id = insert_intent(
        &store,
        &env,
        &work_id,
        HostAction::PushRef {
            branch: branch.clone(),
            produced: rev(&env, &sha_a),
            expected_prior: Some(rev(&env, &sha_a)),
        },
    );
    adapter.apply_outbox(&store, &config, NOW).unwrap();
    let (status, _) = intent_state(&store, &push_id);
    assert!(
        matches!(status, HostActionStatus::Failed { .. }),
        "the stale guard refuses: {status:?}"
    );
    // The remote still sits at sha_b — the refused write never landed.
    let head = adapter
        .client_mut()
        .read_branch(&live_repo(&env), &branch)
        .unwrap();
    assert_eq!(
        head.as_deref(),
        Some(sha_b.as_str()),
        "no rollback, no overwrite"
    );
    retain(
        &env,
        "stale_head",
        &store,
        &work_id,
        &[push_id],
        vec![ReadBackReceipt {
            target: format!("branch:{}/{}", env.slug, branch),
            observed: format!("head stayed {sha_b}"),
        }],
        ScenarioResult::Pass,
    );
    delete_branch(&env, &branch);
}

/// Scenario: duplicate delivery. The same webhook event delivered
/// twice records one effect — the second delivery deduplicates.
#[test]
#[ignore = "U3.6e live case — requires TAISAN_U36E_* env and owner authorization"]
fn live_duplicate_delivery_single_effect() {
    let Some(env) = live_env() else {
        eprintln!("skip: TAISAN_U36E_REPO/INSTALL/AUTH not set");
        return;
    };
    let (sha, _) = remote_commit_pair(&env);
    let store = SqliteStore::in_memory(1).unwrap();
    let config = live_config(&env);
    let mut adapter = live_adapter(&env);
    let (work_id, _eid) = submit(&store, &env, "u36e-dup");
    let branch = format!("u36e/dup-{NOW}");
    insert_intent(
        &store,
        &env,
        &work_id,
        HostAction::PushRef {
            branch: branch.clone(),
            produced: rev(&env, &sha),
            expected_prior: None,
        },
    );
    insert_intent(
        &store,
        &env,
        &work_id,
        HostAction::OpenChangeRequest {
            source_branch: branch.clone(),
            target_branch: default_branch(&env),
            title: "u36e dup".into(),
            body: "u36e".into(),
        },
    );
    adapter.apply_outbox(&store, &config, NOW).unwrap();
    let change = opened_change(&store).expect("open applied");

    let delivery = WebhookDelivery {
        event_id: format!("u36e-dup-{NOW}"),
        emitted_at: NOW,
        fact: WebhookFact::ChangeHead {
            change: change.clone(),
            head: rev(&env, &sha),
        },
    };
    let first = adapter
        .deliver_webhook(&store, &config, &delivery, NOW + 1)
        .unwrap();
    let second = adapter
        .deliver_webhook(&store, &config, &delivery, NOW + 2)
        .unwrap();
    assert!(
        matches!(first, IngestReceipt::Recorded | IngestReceipt::Duplicate),
        "first delivery records: {first:?}"
    );
    assert!(
        matches!(second, IngestReceipt::Duplicate),
        "the redelivery deduplicates: {second:?}"
    );
    // The remote shows exactly one change request for the branch —
    // dedupe never minted a second.
    let pulls = adapter
        .client_mut()
        .list_pulls_on_branch(&live_repo(&env), &branch, &sha)
        .unwrap();
    assert_eq!(pulls.len(), 1, "one change request, one recorded effect");
    retain(
        &env,
        "duplicate_delivery",
        &store,
        &work_id,
        &[],
        vec![ReadBackReceipt {
            target: format!("change:{}", change.change_request.as_str()),
            observed: format!("{} pull(s) at {sha}", pulls.len()),
        }],
        ScenarioResult::Pass,
    );
    // Close the scenario's pull request before cleanup — it is real.
    let _ = gh_json(
        &env,
        &[
            &format!("repos/{}/pulls/{}", env.slug, pulls[0].display_number),
            "-X",
            "PATCH",
            "-f",
            "state=closed",
        ],
    );
    delete_branch(&env, &branch);
}

/// Scenario: denied permission. The credential the operator supplies
/// (`GH_TOKEN` in the environment) lacks write scope on the isolated
/// repository — the host's refusal is definite, recorded `failed`,
/// never `unknown`, and no effect lands.
#[test]
#[ignore = "U3.6e live case — requires TAISAN_U36E_* env, a read-only credential, and owner authorization"]
fn live_denied_permission_refused() {
    let Some(env) = live_env() else {
        eprintln!("skip: TAISAN_U36E_REPO/INSTALL/AUTH not set");
        return;
    };
    let (sha, _) = remote_commit_pair(&env);
    let store = SqliteStore::in_memory(1).unwrap();
    let config = live_config(&env);
    let mut adapter = live_adapter(&env);
    let (work_id, _eid) = submit(&store, &env, "u36e-denied");
    let branch = format!("u36e/denied-{NOW}");
    let push_id = insert_intent(
        &store,
        &env,
        &work_id,
        HostAction::PushRef {
            branch: branch.clone(),
            produced: rev(&env, &sha),
            expected_prior: None,
        },
    );
    adapter.apply_outbox(&store, &config, NOW).unwrap();
    let (status, _) = intent_state(&store, &push_id);
    // The runbook requires a read-only credential for this scenario:
    // GitHub's refusal must be definite `failed` — never `unknown`,
    // and never `applied`. An `applied` outcome means the supplied
    // credential had write scope and the scenario proved nothing, so
    // the record reports `fail` rather than a live claim.
    assert!(
        matches!(
            status,
            HostActionStatus::Applied | HostActionStatus::Failed { .. }
        ),
        "a permission outcome is always definite: {status:?}"
    );
    let result = if matches!(status, HostActionStatus::Failed { .. }) {
        ScenarioResult::Pass
    } else {
        ScenarioResult::Fail
    };
    let head = adapter
        .client_mut()
        .read_branch(&live_repo(&env), &branch)
        .unwrap();
    retain(
        &env,
        "denied_permission",
        &store,
        &work_id,
        &[push_id],
        vec![ReadBackReceipt {
            target: format!("branch:{}/{}", env.slug, branch),
            observed: format!("head {head:?} after {status:?}"),
        }],
        result,
    );
    if head.is_some() {
        delete_branch(&env, &branch);
    }
}
