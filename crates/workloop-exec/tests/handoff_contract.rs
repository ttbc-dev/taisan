//! Durable artifact-handoff contract (docs/worker-protocol.md):
//! `export_started → bundle → quarantine import → ref → acknowledged`,
//! driven by real git repositories. Crash windows resume from the last
//! committed row; absent, invalid, and acknowledged artifacts stay
//! distinguishable; the workspace and bundle are reclaimable only after
//! the row is acknowledged *and* the ref resolves to `produced`.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use workloop_core::command::{CommandBody, CommandDecision, CommandEffect, CommandRequest};
use workloop_core::domain::*;
use workloop_core::identity::*;
use workloop_core::observation::{IssueState, Observation, ObservationFact};
use workloop_core::ports::{ProcessControlPort, ProcessEvidence, SignalOutcome};
use workloop_core::store::{Effect, Store};
use workloop_core::work::{
    HostTemplate, WorkloopConfig, admit_queued, handle_command, handle_observation,
};
use workloop_exec::handoff::{self, HandoffLayout, HandoffSeed, artifact_ref};
use workloop_exec::supervisor::{PosixProcessControl, Supervisor};
use workloop_store::sqlite::SqliteStore;

const NOW: i64 = 1_700_000_000;

fn fixture_bin() -> String {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/debug/workloop-fixture-worker")
        .to_string_lossy()
        .into_owned()
}

fn tmp_root(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("workloop-handoff-{}-{}", tag, std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn host() -> HostInstanceId {
    HostInstanceId::trusted("fixture")
}

fn repo() -> RepositoryRef {
    RepositoryRef {
        host: host(),
        repository: RepositoryId::trusted("repo"),
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
                actor: HostActorRef {
                    host: host(),
                    actor: HostActorId::trusted("pub-agent"),
                },
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
        ]),
        lanes: LanePolicy::default(),
        required_checks: vec![],
        allowed_effects: AllowedEffects {
            workspace_write: true,
            host_write: false,
            network: false,
        },
        budget: ExecutionBudget {
            artifact_recovery_attempts: 3,
            artifact_recovery_seconds: 60,
            ..ExecutionBudget::default()
        },
        policy: WorkPolicy::default(),
    }
}

fn actor() -> Actor {
    Actor {
        kind: ActorKind::Human,
        external_id: "op".into(),
    }
}

/// Submit+admit so `works`/`executions` rows exist for FK integrity.
fn admit(store: &SqliteStore, key: &str) -> (WorkId, ExecutionId) {
    let req = CommandRequest {
        actor: actor(),
        origin: Origin("cli".into()),
        idempotency_key: key.into(),
        body: CommandBody::Submit {
            intent: "produce an artifact".into(),
            sources: vec![],
            constraints: vec![],
            verification_argv: vec![],
        },
    };
    let d = handle_command(store, &config(), &req, NOW).unwrap();
    let (work_id, eid) = match d {
        CommandDecision::Accepted {
            work_id, effect, ..
        } => {
            let eid = match effect {
                CommandEffect::WorkCreated { execution_id } => execution_id,
                other => panic!("unexpected effect {other:?}"),
            };
            (work_id.unwrap(), eid)
        }
        other => panic!("submit rejected: {other:?}"),
    };
    let admitted = store
        .transact(|tx| admit_queued(tx, &config(), &eid, NOW))
        .unwrap();
    assert!(admitted.is_some(), "execution admitted");
    (work_id, eid)
}

fn done_invocation(eid: &ExecutionId, id: &str) -> Invocation {
    Invocation {
        id: InvocationId(id.into()),
        request_id: format!("{}:inv:0", eid.0),
        execution_id: eid.clone(),
        role: InvocationRole::Coding,
        state: InvocationState::Done,
        process_identity: None,
        required_artifacts: vec![RequiredArtifact {
            kind: "patch".into(),
            path: "artifact.json".into(),
            expected_digest: None,
        }],
        evidence_refs: vec![],
        descendants: vec![],
    }
}

/// A plain git helper for fixture setup (the implementation's own git
/// wrapper applies the hardened export profile; tests need identity
/// config too).
fn git(dir: &Path, args: &[&str]) -> String {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {}: {}",
        args.join(" "),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// Whether `reference` resolves at all inside `repo_dir`.
fn ref_exists(repo_dir: &Path, reference: &str) -> bool {
    std::process::Command::new("git")
        .arg("-C")
        .arg(repo_dir)
        .args(["rev-parse", "--verify", reference])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Workspace repo: base commit on the default branch, then a produced
/// commit on `work/<work_id>`; returns `(base, produced)`.
fn git_workspace(ws: &Path, work_id: &WorkId) -> (String, String) {
    std::fs::create_dir_all(ws).unwrap();
    git(ws, &["init", "-b", "main"]);
    git(ws, &["config", "user.email", "t@t"]);
    git(ws, &["config", "user.name", "t"]);
    std::fs::write(ws.join("seed.txt"), "base").unwrap();
    git(ws, &["add", "."]);
    git(ws, &["commit", "-m", "base"]);
    let base = git(ws, &["rev-parse", "HEAD"]);
    let branch = format!("work/{}", work_id.0);
    git(ws, &["checkout", "-b", &branch]);
    std::fs::write(ws.join("artifact.json"), "{\"ok\":true}").unwrap();
    git(ws, &["add", "."]);
    git(ws, &["commit", "-m", "produced"]);
    let produced = git(ws, &["rev-parse", "HEAD"]);
    (base, produced)
}

fn bare_repo(dir: &Path) -> PathBuf {
    let repo = dir.join("handoff.git");
    git(dir, &["init", "--bare", repo.to_str().unwrap()]);
    repo
}

/// `(layout, workspace dir)` under `root` for `work_id`.
fn layout(root: &Path, work_id: &WorkId) -> (HandoffLayout, PathBuf) {
    let ws_root = root.join("ws");
    let out_root = root.join("out");
    std::fs::create_dir_all(&ws_root).unwrap();
    std::fs::create_dir_all(&out_root).unwrap();
    (
        HandoffLayout {
            repository: bare_repo(root),
            workspace_root: ws_root.clone(),
            out_root,
        },
        ws_root.join(format!("ws-{}", work_id.0)),
    )
}

fn seed(invocation: &Invocation, work_id: &WorkId, base: &str) -> HandoffSeed {
    HandoffSeed {
        invocation: invocation.clone(),
        base: base.to_string(),
        branch: format!("work/{}", work_id.0),
        work_id: work_id.clone(),
        verification_argv: vec![],
    }
}

fn insert_invocation(store: &SqliteStore, invocation: &Invocation) {
    store
        .transact(|tx| {
            tx.apply(&Effect::InsertInvocation {
                invocation: invocation.clone(),
            })
        })
        .unwrap();
}

fn artifact_id(invocation: &Invocation) -> String {
    format!("{}/patch", invocation.id.0)
}

fn row(store: &SqliteStore, artifact_id: &str) -> Option<ArtifactHandoff> {
    store
        .transact(|tx| tx.artifact_handoff(artifact_id))
        .unwrap()
}

fn persist(store: &SqliteStore, handoff: &ArtifactHandoff) {
    store
        .transact(|tx| {
            tx.apply(&Effect::UpsertHandoff {
                handoff: handoff.clone(),
            })
        })
        .unwrap();
}

/// The crash residue: only the `export_started` row is committed.
fn seed_export_started(store: &SqliteStore, invocation: &Invocation, w: &WorkId, base: &str) {
    store
        .transact(|tx| {
            let seq = tx.alloc_seq()?;
            tx.apply(&Effect::UpsertHandoff {
                handoff: ArtifactHandoff {
                    artifact_id: artifact_id(invocation),
                    invocation_id: invocation.id.clone(),
                    request_id: invocation.request_id.clone(),
                    work_id: w.clone(),
                    execution_id: invocation.execution_id.clone(),
                    base: base.to_string(),
                    produced: None,
                    manifest_digest: None,
                    state: HandoffState::ExportStarted,
                    invalid_reason: None,
                    attempt: 1,
                    recorded_seq: seq,
                },
            })
        })
        .unwrap();
}

/// Full happy path: export → verify → import → ref → acknowledged; the
/// durable acknowledgement is row *and* ref resolving to produced.
#[test]
fn a_complete_handoff_acknowledges_row_and_ref() {
    let root = tmp_root("complete");
    let store = SqliteStore::in_memory(1).unwrap();
    let (w, e) = admit(&store, "h-complete");
    let (layout, ws_dir) = layout(&root, &w);
    let (base, produced) = git_workspace(&ws_dir, &w);
    let invocation = done_invocation(&e, "inv-h1");
    insert_invocation(&store, &invocation);
    let aid = artifact_id(&invocation);

    let h = handoff::drive(&store, &layout, &aid, &seed(&invocation, &w, &base), 3).unwrap();
    assert_eq!(h.state, HandoffState::Acknowledged);
    assert_eq!(h.produced.as_deref(), Some(produced.as_str()));
    assert!(h.manifest_digest.is_some());
    assert_eq!(
        git(&layout.repository, &["rev-parse", &artifact_ref(&aid)]),
        produced
    );
    assert!(handoff::reclaimable(&layout, &h));
}

/// Absent (no row), invalid (row + closed reason), and acknowledged are
/// three distinguishable answers — never collapsed.
#[test]
fn absent_invalid_and_acknowledged_stay_distinguishable() {
    let root = tmp_root("distinguish");
    let store = SqliteStore::in_memory(1).unwrap();
    let (w, e) = admit(&store, "h-dist");
    let (layout, ws_dir) = layout(&root, &w);
    let (base, _produced) = git_workspace(&ws_dir, &w);
    let invocation = done_invocation(&e, "inv-h2");
    insert_invocation(&store, &invocation);
    let aid = artifact_id(&invocation);

    assert!(row(&store, &aid).is_none(), "absent is no row");

    let h = handoff::drive(&store, &layout, &aid, &seed(&invocation, &w, &base), 3).unwrap();
    assert_eq!(h.state, HandoffState::Acknowledged);
    assert!(handoff::reclaimable(&layout, &h));

    let mut invalid = h.clone();
    invalid.state = HandoffState::Invalid;
    invalid.invalid_reason = Some("digest_mismatch".into());
    persist(&store, &invalid);
    let reread = row(&store, &aid).unwrap();
    assert_eq!(reread.state, HandoffState::Invalid);
    assert_eq!(reread.invalid_reason.as_deref(), Some("digest_mismatch"));
    assert!(!handoff::reclaimable(&layout, &reread));
}

/// A produced commit that does not descend from the recorded base is a
/// stale artifact — closed as `invalid(stale_base)`, never exported.
#[test]
fn a_stale_base_is_invalid() {
    let root = tmp_root("stale");
    let store = SqliteStore::in_memory(1).unwrap();
    let (w, e) = admit(&store, "h-stale");
    let (layout, ws_dir) = layout(&root, &w);
    let (base, _produced) = git_workspace(&ws_dir, &w);
    // An orphan produced line that does not descend from `base`.
    let branch = format!("work/{}", w.0);
    git(&ws_dir, &["checkout", "--orphan", "orphan-line"]);
    std::fs::write(ws_dir.join("other.txt"), "orphan").unwrap();
    git(&ws_dir, &["add", "."]);
    git(&ws_dir, &["commit", "-m", "orphan produced"]);
    git(&ws_dir, &["branch", "-f", &branch, "orphan-line"]);
    let invocation = done_invocation(&e, "inv-h3");
    insert_invocation(&store, &invocation);

    let h = handoff::drive(
        &store,
        &layout,
        &artifact_id(&invocation),
        &seed(&invocation, &w, &base),
        3,
    )
    .unwrap();
    assert_eq!(h.state, HandoffState::Invalid);
    assert_eq!(h.invalid_reason.as_deref(), Some("stale_base"));
    assert!(!ref_exists(
        &layout.repository,
        &artifact_ref(&h.artifact_id)
    ));
}

/// Crash after `export_started` with no bundle: the resume re-exports
/// from the retained workspace; nothing is lost or invented.
#[test]
fn crash_after_export_started_resumes_from_the_row() {
    let root = tmp_root("crash-export");
    let store = SqliteStore::in_memory(1).unwrap();
    let (w, e) = admit(&store, "h-crash-e");
    let (layout, ws_dir) = layout(&root, &w);
    let (base, produced) = git_workspace(&ws_dir, &w);
    let invocation = done_invocation(&e, "inv-h4");
    insert_invocation(&store, &invocation);
    let aid = artifact_id(&invocation);
    seed_export_started(&store, &invocation, &w, &base);

    let h = handoff::drive(&store, &layout, &aid, &seed(&invocation, &w, &base), 3).unwrap();
    assert_eq!(h.state, HandoffState::Acknowledged);
    assert_eq!(h.produced.as_deref(), Some(produced.as_str()));
}

/// A truncated bundle left by a crash is not evidence: it is deleted
/// and re-exported rather than trusted or retained.
#[test]
fn a_corrupt_bundle_is_reexported_not_trusted() {
    let root = tmp_root("corrupt");
    let store = SqliteStore::in_memory(1).unwrap();
    let (w, e) = admit(&store, "h-corrupt");
    let (layout, ws_dir) = layout(&root, &w);
    let (base, produced) = git_workspace(&ws_dir, &w);
    let invocation = done_invocation(&e, "inv-h5");
    insert_invocation(&store, &invocation);
    let aid = artifact_id(&invocation);
    seed_export_started(&store, &invocation, &w, &base);

    let bundle = layout
        .out_root
        .join(&e.0)
        .join(&invocation.id.0)
        .join("export.bundle");
    std::fs::create_dir_all(bundle.parent().unwrap()).unwrap();
    std::fs::write(&bundle, b"not-a-bundle").unwrap();

    let h = handoff::drive(&store, &layout, &aid, &seed(&invocation, &w, &base), 3).unwrap();
    assert_eq!(h.state, HandoffState::Acknowledged);
    assert_eq!(h.produced.as_deref(), Some(produced.as_str()));
    assert_eq!(
        git(&layout.repository, &["rev-parse", &artifact_ref(&aid)]),
        produced,
        "the re-exported bundle lands the real produced commit"
    );
}

/// Crash between the durable ref write and the ack commit: an
/// `imported` row whose ref already resolves to produced acknowledges;
/// one with no ref recreates it under the expected-absent guard after
/// re-verifying the stored manifest.
#[test]
fn imported_rows_recover_both_sides_of_the_ref_write() {
    let root = tmp_root("crash-ack");
    let store = SqliteStore::in_memory(1).unwrap();
    let (w, e) = admit(&store, "h-crash-a");
    let (layout, ws_dir) = layout(&root, &w);
    let (base, produced) = git_workspace(&ws_dir, &w);
    let invocation = done_invocation(&e, "inv-h6");
    insert_invocation(&store, &invocation);
    let aid = artifact_id(&invocation);
    let s = seed(&invocation, &w, &base);

    let complete = handoff::drive(&store, &layout, &aid, &s, 3).unwrap();
    assert_eq!(complete.state, HandoffState::Acknowledged);

    // Side A: crash after the ref write — row still `imported`, ref
    // already resolves to produced.
    let mut row_a = complete.clone();
    row_a.state = HandoffState::Imported;
    persist(&store, &row_a);
    let h = handoff::drive(&store, &layout, &aid, &s, 3).unwrap();
    assert_eq!(h.state, HandoffState::Acknowledged);

    // Side B: crash before the ref write — `imported` row, no ref.
    git(
        &layout.repository,
        &["update-ref", "-d", &artifact_ref(&aid)],
    );
    let mut row_b = complete.clone();
    row_b.state = HandoffState::Imported;
    persist(&store, &row_b);
    let h = handoff::drive(&store, &layout, &aid, &s, 3).unwrap();
    assert_eq!(h.state, HandoffState::Acknowledged);
    assert_eq!(
        git(&layout.repository, &["rev-parse", &artifact_ref(&aid)]),
        produced
    );
}

/// A ref the handoff did not write occupying the artifact name is a
/// conflict — invalid, never overwritten.
#[test]
fn a_foreign_ref_is_never_overwritten() {
    let root = tmp_root("refconflict");
    let store = SqliteStore::in_memory(1).unwrap();
    let (w, e) = admit(&store, "h-refconf");
    let (layout, ws_dir) = layout(&root, &w);
    let (base, produced) = git_workspace(&ws_dir, &w);
    let invocation = done_invocation(&e, "inv-h7");
    insert_invocation(&store, &invocation);
    let aid = artifact_id(&invocation);
    let s = seed(&invocation, &w, &base);

    let complete = handoff::drive(&store, &layout, &aid, &s, 3).unwrap();
    // Hand the artifact's ref to a different object.
    let foreign = git(&layout.repository, &["rev-parse", &format!("{produced}^")]);
    git(
        &layout.repository,
        &["update-ref", &artifact_ref(&aid), &foreign],
    );
    let mut imported = complete.clone();
    imported.state = HandoffState::Imported;
    persist(&store, &imported);

    let h = handoff::drive(&store, &layout, &aid, &s, 3).unwrap();
    assert_eq!(h.state, HandoffState::Invalid);
    assert_eq!(h.invalid_reason.as_deref(), Some("ref_conflict"));
    assert_eq!(
        git(&layout.repository, &["rev-parse", &artifact_ref(&aid)]),
        foreign,
        "the foreign ref is untouched"
    );
}

/// A stored manifest that no longer matches the imported objects is a
/// digest mismatch — invalid, never acknowledged, no ref established.
#[test]
fn a_manifest_mismatch_is_invalid() {
    let root = tmp_root("digest");
    let store = SqliteStore::in_memory(1).unwrap();
    let (w, e) = admit(&store, "h-digest");
    let (layout, ws_dir) = layout(&root, &w);
    let (base, _p) = git_workspace(&ws_dir, &w);
    let invocation = done_invocation(&e, "inv-h8");
    insert_invocation(&store, &invocation);
    let aid = artifact_id(&invocation);
    let s = seed(&invocation, &w, &base);

    let complete = handoff::drive(&store, &layout, &aid, &s, 3).unwrap();
    // Tamper the stored digest and drop the ref so the recreate path
    // must re-verify the manifest first.
    git(
        &layout.repository,
        &["update-ref", "-d", &artifact_ref(&aid)],
    );
    let mut tampered = complete.clone();
    tampered.state = HandoffState::Imported;
    tampered.manifest_digest = Some("tampered".into());
    persist(&store, &tampered);

    let h = handoff::drive(&store, &layout, &aid, &s, 3).unwrap();
    assert_eq!(h.state, HandoffState::Invalid);
    assert_eq!(h.invalid_reason.as_deref(), Some("digest_mismatch"));
    assert!(
        !ref_exists(&layout.repository, &artifact_ref(&aid)),
        "no ref is established for an invalid artifact"
    );
}

/// Bounded recovery: a workspace whose produced branch never resolves
/// charges one attempt per pass and closes `recovery_exhausted` at the
/// bound — distinct from a corrupt bundle or a stale base.
#[test]
fn recovery_exhaustion_is_a_distinct_reason() {
    let root = tmp_root("exhausted");
    let store = SqliteStore::in_memory(1).unwrap();
    let (w, e) = admit(&store, "h-exhaust");
    let (layout, ws_dir) = layout(&root, &w);
    let (base, _p) = git_workspace(&ws_dir, &w);
    // Remove the produced branch: the workspace cannot produce.
    let branch = format!("work/{}", w.0);
    git(&ws_dir, &["checkout", "main"]);
    git(&ws_dir, &["branch", "-D", &branch]);
    let invocation = done_invocation(&e, "inv-h9");
    insert_invocation(&store, &invocation);
    let aid = artifact_id(&invocation);
    let s = seed(&invocation, &w, &base);

    let mut last = None;
    for _ in 0..4 {
        last = Some(handoff::drive(&store, &layout, &aid, &s, 3).unwrap());
    }
    let h = last.unwrap();
    assert_eq!(h.state, HandoffState::Invalid);
    assert_eq!(h.invalid_reason.as_deref(), Some("recovery_exhausted"));
}

/// Verification runs on the exact handed-off commit in an independent
/// worktree — the result binds artifact, argv, and produced commit.
#[test]
fn verification_runs_on_the_handed_off_commit() {
    let root = tmp_root("verify");
    let store = SqliteStore::in_memory(1).unwrap();
    let (w, e) = admit(&store, "h-verify");
    let (layout, ws_dir) = layout(&root, &w);
    let (base, produced) = git_workspace(&ws_dir, &w);
    let invocation = done_invocation(&e, "inv-h10");
    insert_invocation(&store, &invocation);
    let aid = artifact_id(&invocation);
    let s = seed(&invocation, &w, &base);
    handoff::drive(&store, &layout, &aid, &s, 3).unwrap();

    let scratch = root.join("scratch");
    std::fs::create_dir_all(&scratch).unwrap();
    let pass = handoff::verify(
        &store,
        &layout,
        &aid,
        "verification_argv[0]",
        &["git".into(), "rev-parse".into(), "HEAD".into()],
        &scratch,
    )
    .unwrap();
    assert_eq!(pass.result, VerificationOutcome::Pass);
    assert_eq!(pass.produced, produced);
    assert!(pass.output.contains(&produced));

    let fail = handoff::verify(
        &store,
        &layout,
        &aid,
        "verification_argv[1]",
        &["sh".into(), "-c".into(), "exit 3".into()],
        &scratch,
    )
    .unwrap();
    assert_eq!(fail.result, VerificationOutcome::Fail);

    // Verification never runs before acknowledgement.
    assert!(
        handoff::verify(
            &store,
            &layout,
            "missing-artifact",
            "verification_argv[0]",
            &["true".into()],
            &scratch,
        )
        .is_err()
    );
}

/// Verification output bounded at 64 KiB is truncated at the byte level
/// before lossy decoding — a multibyte codepoint straddling the bound
/// must not panic String::truncate. `yes 'ああ'` emits a 7-byte line, so
/// byte 65536 always lands mid-codepoint.
#[test]
fn multibyte_verification_output_truncates_without_a_panic() {
    let root = tmp_root("verifyutf8");
    let store = SqliteStore::in_memory(1).unwrap();
    let (w, e) = admit(&store, "h-utf8");
    let (layout, ws_dir) = layout(&root, &w);
    let (base, produced) = git_workspace(&ws_dir, &w);
    let invocation = done_invocation(&e, "inv-h11");
    insert_invocation(&store, &invocation);
    let aid = artifact_id(&invocation);
    let s = seed(&invocation, &w, &base);
    handoff::drive(&store, &layout, &aid, &s, 3).unwrap();

    let scratch = root.join("scratch");
    std::fs::create_dir_all(&scratch).unwrap();
    let res = handoff::verify(
        &store,
        &layout,
        &aid,
        "verification_argv[0]",
        &[
            "sh".into(),
            "-c".into(),
            "yes 'ああ' | head -c 70000".into(),
        ],
        &scratch,
    )
    .unwrap();
    assert_eq!(res.result, VerificationOutcome::Pass);
    assert_eq!(res.produced, produced);
    assert!(
        res.output.len() <= 64 * 1024 + 8,
        "bounded output: {} bytes",
        res.output.len()
    );
}

/// A writer that may still be live blocks export entirely: `Done` is a
/// record, not stop proof — an unverified done claim resolves the
/// Invocation while the writer may still run. A handoff row opens only
/// after every recorded identity inspects stopped. Workspaces without
/// git objects keep the file-artifact path untouched.
#[test]
fn only_a_proven_dead_invocation_exports() {
    let root = tmp_root("live-writer");
    let store = SqliteStore::in_memory(1).unwrap();
    let (w, e) = admit(&store, "h-live");
    let ws_root = root.join("ws");
    std::fs::create_dir_all(&ws_root).unwrap();
    let ws_dir = ws_root.join(format!("ws-{}", w.0));
    let (_base, _produced) = git_workspace(&ws_dir, &w);
    let layout = HandoffLayout {
        repository: bare_repo(&root),
        workspace_root: ws_root.clone(),
        out_root: root.join("out"),
    };
    let sup = Supervisor::new(PosixProcessControl, ws_root.clone(), vec![], config())
        .with_handoff(layout);
    let port = PosixProcessControl;

    // A live recorded writer blocks export even when the Invocation is
    // recorded `Done` (an unverified done claim resolves it the same).
    let live_identity = port
        .spawn(&[fixture_bin(), "hang".into()], &ws_dir)
        .expect("spawn live writer");
    let mut live = done_invocation(&e, "inv-live");
    live.process_identity = Some(live_identity.clone());
    insert_invocation(&store, &live);
    sup.drive_handoffs(&store).unwrap();
    assert!(
        row(&store, &artifact_id(&live)).is_none(),
        "a Done record over a live writer opens no handoff row"
    );

    // Once the recorded identity inspects stopped the row opens and
    // acknowledges.
    assert_eq!(port.signal_verified(&live_identity, 9), SignalOutcome::Sent);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !matches!(
        port.inspect(&live_identity),
        ProcessEvidence::Stopped { .. }
    ) {
        assert!(
            std::time::Instant::now() < deadline,
            "live writer did not stop"
        );
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
    sup.drive_handoffs(&store).unwrap();
    let h = row(&store, &artifact_id(&live)).expect("handoff row opened");
    assert_eq!(h.state, HandoffState::Acknowledged);
    assert!(h.produced.is_some());
}

/// A persistent transport failure — `git bundle create` cannot write —
/// is charged against the artifact-recovery bound: the row settles
/// `invalid(recovery_exhausted)` instead of wedging `export_started`
/// while the workspace stays retained forever.
#[test]
fn transport_failure_charges_the_recovery_bound() {
    let root = tmp_root("transport-bound");
    let store = SqliteStore::in_memory(1).unwrap();
    let (w, e) = admit(&store, "h-tbound");
    let (layout, ws_dir) = layout(&root, &w);
    let (base, _produced) = git_workspace(&ws_dir, &w);
    let invocation = done_invocation(&e, "inv-tb");
    insert_invocation(&store, &invocation);
    let aid = artifact_id(&invocation);

    // The out directory exists but is not writable: every
    // `git bundle create` into it fails as a transport error.
    let out_dir = layout.out_root.join(&e.0).join(&invocation.id.0);
    std::fs::create_dir_all(&out_dir).unwrap();
    let mut perms = std::fs::metadata(&out_dir).unwrap().permissions();
    perms.set_readonly(true);
    std::fs::set_permissions(&out_dir, perms).unwrap();

    let s = seed(&invocation, &w, &base);
    handoff::drive(&store, &layout, &aid, &s, 2).unwrap();
    let first = row(&store, &aid).expect("row");
    assert_eq!(first.state, HandoffState::ExportStarted);
    assert_eq!(first.attempt, 1, "one charged failure, one attempt used");

    handoff::drive(&store, &layout, &aid, &s, 2).unwrap();
    let second = row(&store, &aid).expect("row");
    assert_eq!(second.state, HandoffState::ExportStarted);
    assert_eq!(second.attempt, 2, "the second attempt still runs");

    // The bound refuses attempt N+1 — N charged failures settle the row
    // without a further transport call.
    handoff::drive(&store, &layout, &aid, &s, 2).unwrap();
    let h = row(&store, &aid).unwrap();
    assert_eq!(h.state, HandoffState::Invalid);
    assert_eq!(h.invalid_reason.as_deref(), Some("recovery_exhausted"));
    assert_eq!(h.attempt, 2, "a bound of 2 performs 2 transport attempts");
}

/// An Invocation declaring several required artifacts gets one durable
/// handoff row per artifact — a second artifact is never silently
/// dropped behind the first's row.
#[test]
fn each_required_artifact_gets_its_own_handoff() {
    let root = tmp_root("multi-artifact");
    let store = SqliteStore::in_memory(1).unwrap();
    let (w, e) = admit(&store, "h-multi");
    let (layout, ws_dir) = layout(&root, &w);
    let (base, produced) = git_workspace(&ws_dir, &w);
    let mut invocation = done_invocation(&e, "inv-multi");
    invocation.required_artifacts.push(RequiredArtifact {
        kind: "log".into(),
        path: "run.log".into(),
        expected_digest: None,
    });
    insert_invocation(&store, &invocation);
    let s = seed(&invocation, &w, &base);
    for kind in ["patch", "log"] {
        let aid = format!("{}/{kind}", invocation.id.0);
        let h = handoff::drive(&store, &layout, &aid, &s, 3).unwrap();
        assert_eq!(h.state, HandoffState::Acknowledged);
        assert_eq!(h.artifact_id, aid);
        assert_eq!(
            git(&layout.repository, &["rev-parse", &artifact_ref(&aid)]),
            produced
        );
    }
}

/// Once the artifact is acknowledged, every declared verification argv
/// runs on the handed-off commit and is recorded; re-driving skips
/// recorded requirements instead of duplicating rows.
#[test]
fn an_acknowledged_handoff_runs_declared_verification() {
    let root = tmp_root("verify-wired");
    let store = SqliteStore::in_memory(1).unwrap();
    let (w, e) = admit(&store, "h-vwired");
    let (layout, ws_dir) = layout(&root, &w);
    let (base, produced) = git_workspace(&ws_dir, &w);
    let invocation = done_invocation(&e, "inv-vw");
    insert_invocation(&store, &invocation);
    let aid = artifact_id(&invocation);
    let mut s = seed(&invocation, &w, &base);
    s.verification_argv = vec![vec!["git".into(), "rev-parse".into(), "HEAD".into()]];

    let h = handoff::drive(&store, &layout, &aid, &s, 3).unwrap();
    assert_eq!(h.state, HandoffState::Acknowledged);
    let results = store.transact(|tx| tx.verification_results(&aid)).unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].requirement, "verification_argv[0]");
    assert_eq!(results[0].result, VerificationOutcome::Pass);
    assert_eq!(results[0].produced, produced);

    // Re-driving the acknowledged row records nothing twice.
    handoff::drive(&store, &layout, &aid, &s, 3).unwrap();
    assert_eq!(
        store
            .transact(|tx| tx.verification_results(&aid))
            .unwrap()
            .len(),
        1
    );
}

/// A Coding/Fix branch that never moved past base produced nothing —
/// `nothing_produced` is a distinct classification, not an exportable
/// artifact (docs/worker-protocol.md evidence table).
#[test]
fn a_branch_that_never_moved_is_nothing_produced() {
    let root = tmp_root("nothingprod");
    let store = SqliteStore::in_memory(1).unwrap();
    let (w, e) = admit(&store, "h-noprod");
    let (layout, ws_dir) = layout(&root, &w);
    // Work branch exists but holds no commit beyond base.
    std::fs::create_dir_all(&ws_dir).unwrap();
    git(&ws_dir, &["init", "-b", "main"]);
    git(&ws_dir, &["config", "user.email", "t@t"]);
    git(&ws_dir, &["config", "user.name", "t"]);
    std::fs::write(ws_dir.join("seed.txt"), "base").unwrap();
    git(&ws_dir, &["add", "."]);
    git(&ws_dir, &["commit", "-m", "base"]);
    let base = git(&ws_dir, &["rev-parse", "HEAD"]);
    git(&ws_dir, &["checkout", "-b", &format!("work/{}", w.0)]);

    let invocation = done_invocation(&e, "inv-noprod");
    insert_invocation(&store, &invocation);
    let aid = artifact_id(&invocation);
    let s = seed(&invocation, &w, &base);
    handoff::drive(&store, &layout, &aid, &s, 3).unwrap();
    let h = row(&store, &aid).unwrap();
    assert_eq!(h.state, HandoffState::Invalid);
    assert_eq!(h.invalid_reason.as_deref(), Some("nothing_produced"));
    assert!(
        !ref_exists(&layout.repository, &artifact_ref(&aid)),
        "an unchanged branch installs no artifact ref"
    );
}

/// `imported` row + a present ref is not enough: the stored manifest
/// digest must still match. A crashed pass that wrote the ref under a
/// stale digest is classified invalid, never acknowledged.
#[test]
fn a_present_ref_with_a_stale_digest_is_not_acknowledged() {
    let root = tmp_root("staledigest");
    let store = SqliteStore::in_memory(1).unwrap();
    let (w, e) = admit(&store, "h-sd");
    let (layout, ws_dir) = layout(&root, &w);
    let (base, _produced) = git_workspace(&ws_dir, &w);
    let invocation = done_invocation(&e, "inv-sd");
    insert_invocation(&store, &invocation);
    let aid = artifact_id(&invocation);
    let s = seed(&invocation, &w, &base);
    handoff::drive(&store, &layout, &aid, &s, 3).unwrap();
    assert!(ref_exists(&layout.repository, &artifact_ref(&aid)));

    // The crash window: ref present, row still `imported` — with a
    // corrupted manifest digest.
    let mut h = row(&store, &aid).unwrap();
    h.state = HandoffState::Imported;
    h.manifest_digest = Some("stale".into());
    persist(&store, &h);
    handoff::drive(&store, &layout, &aid, &s, 3).unwrap();
    let h = row(&store, &aid).unwrap();
    assert_eq!(h.state, HandoffState::Invalid);
    assert_eq!(h.invalid_reason.as_deref(), Some("digest_mismatch"));
}

/// A crashed verification leaves a registered worktree whose directory
/// was swept: the next run prunes the stale registration instead of
/// failing `worktree add` forever.
#[test]
fn a_stale_worktree_registration_does_not_wedge_verification() {
    let root = tmp_root("stalewt");
    let store = SqliteStore::in_memory(1).unwrap();
    let (w, e) = admit(&store, "h-swt");
    let (layout, ws_dir) = layout(&root, &w);
    let (base, produced) = git_workspace(&ws_dir, &w);
    let invocation = done_invocation(&e, "inv-swt");
    insert_invocation(&store, &invocation);
    let aid = artifact_id(&invocation);
    let s = seed(&invocation, &w, &base);
    handoff::drive(&store, &layout, &aid, &s, 3).unwrap();

    // Recreate the crash residue: `worktree add` registered the path,
    // then the process died before `worktree remove`; the directory was
    // swept but the registration remains.
    let scratch = layout.out_root.join(&e.0).join(&invocation.id.0);
    let worktree = scratch.join(format!(
        "verify-{}-{}",
        aid.replace('/', "-"),
        "verification_argv[0]".replace(['/', '[', ']'], "-")
    ));
    git(
        &layout.repository,
        &[
            "worktree",
            "add",
            "--detach",
            worktree.to_str().unwrap(),
            &produced,
        ],
    );
    std::fs::remove_dir_all(&worktree).unwrap();

    let res = handoff::verify(
        &store,
        &layout,
        &aid,
        "verification_argv[0]",
        &["git".into(), "rev-parse".into(), "HEAD".into()],
        &scratch,
    )
    .unwrap();
    assert_eq!(res.result, VerificationOutcome::Pass);
    assert_eq!(res.produced, produced);
}

/// An inconclusive run (spawn failure) is retried on the next pass —
/// it neither satisfies nor exhausts the requirement. The retried
/// result replaces the row; a terminal result is final.
#[test]
fn an_inconclusive_verification_is_retried() {
    let root = tmp_root("inconclusive");
    let store = SqliteStore::in_memory(1).unwrap();
    let (w, e) = admit(&store, "h-inc");
    let (layout, ws_dir) = layout(&root, &w);
    let (base, produced) = git_workspace(&ws_dir, &w);
    let invocation = done_invocation(&e, "inv-inc");
    insert_invocation(&store, &invocation);
    let aid = artifact_id(&invocation);

    let mut s = seed(&invocation, &w, &base);
    s.verification_argv = vec![vec!["no-such-verification-binary".into()]];
    handoff::drive(&store, &layout, &aid, &s, 3).unwrap();
    let results = store.transact(|tx| tx.verification_results(&aid)).unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].result, VerificationOutcome::Inconclusive);

    s.verification_argv = vec![vec!["git".into(), "rev-parse".into(), "HEAD".into()]];
    handoff::drive(&store, &layout, &aid, &s, 3).unwrap();
    let results = store.transact(|tx| tx.verification_results(&aid)).unwrap();
    assert_eq!(results.len(), 1, "one row per requirement");
    assert_eq!(results[0].result, VerificationOutcome::Pass);
    assert_eq!(results[0].produced, produced);
}

/// A timed-out or signal-killed runner is `Inconclusive` — never a
/// fabricated exit code (worker-protocol runner contract).
#[test]
fn a_timed_out_or_killed_runner_is_inconclusive() {
    let root = tmp_root("verifyrun");
    let store = SqliteStore::in_memory(1).unwrap();
    let (w, e) = admit(&store, "h-vt");
    let (layout, ws_dir) = layout(&root, &w);
    let (base, _produced) = git_workspace(&ws_dir, &w);
    let invocation = done_invocation(&e, "inv-vt");
    insert_invocation(&store, &invocation);
    let aid = artifact_id(&invocation);
    let s = seed(&invocation, &w, &base);
    handoff::drive(&store, &layout, &aid, &s, 3).unwrap();

    let scratch = root.join("scratch");
    std::fs::create_dir_all(&scratch).unwrap();
    let timed_out = handoff::verify_with_timeout(
        &store,
        &layout,
        &aid,
        "verification_argv[0]",
        &["sh".into(), "-c".into(), "sleep 5".into()],
        &scratch,
        Duration::from_millis(200),
    )
    .unwrap();
    assert_eq!(timed_out.result, VerificationOutcome::Inconclusive);
    assert!(timed_out.output.contains("timeout") || timed_out.output.contains("exceeded"));

    let killed = handoff::verify_with_timeout(
        &store,
        &layout,
        &aid,
        "verification_argv[1]",
        &["sh".into(), "-c".into(), "kill -9 $$".into()],
        &scratch,
        Duration::from_secs(10),
    )
    .unwrap();
    assert_eq!(killed.result, VerificationOutcome::Inconclusive);

    // An empty argv is a spec defect — recorded Inconclusive like any
    // other non-terminating attempt, never a panic.
    let empty = handoff::verify_with_timeout(
        &store,
        &layout,
        &aid,
        "verification_argv[2]",
        &[],
        &scratch,
        Duration::from_secs(10),
    )
    .unwrap();
    assert_eq!(empty.result, VerificationOutcome::Inconclusive);

    // A large writer cannot size the recorded output — only the
    // bounded tail is retained, never the whole log.
    let loud = handoff::verify_with_timeout(
        &store,
        &layout,
        &aid,
        "verification_argv[3]",
        &[
            "sh".into(),
            "-c".into(),
            "head -c 200000 /dev/zero | tr '\\0' 'x'; echo tail-marker".into(),
        ],
        &scratch,
        Duration::from_secs(30),
    )
    .unwrap();
    assert_eq!(loud.result, VerificationOutcome::Pass);
    assert!(
        loud.output.len() <= 66 * 1024,
        "retained output is bounded: {}",
        loud.output.len()
    );
    assert!(
        loud.output.contains("tail-marker"),
        "the tail is the retained portion"
    );

    // A timed-out runner's whole process group dies with it — a
    // grandchild it spawned never survives past the recorded
    // Inconclusive to keep writing.
    let sentinel = scratch.join("grandchild-survived");
    let group = handoff::verify_with_timeout(
        &store,
        &layout,
        &aid,
        "verification_argv[4]",
        &[
            "sh".into(),
            "-c".into(),
            format!("(sleep 2 && touch '{}') & wait", sentinel.display()),
        ],
        &scratch,
        Duration::from_millis(200),
    )
    .unwrap();
    assert_eq!(group.result, VerificationOutcome::Inconclusive);
    std::thread::sleep(Duration::from_secs(3));
    assert!(
        !sentinel.exists(),
        "the runner's grandchildren are killed with it"
    );
}

/// `source_revision` is a host issue-version token, not a git commit:
/// a source-bound Work derives the handoff base from the managed
/// branch's fork point in the workspace, and a valid produced commit
/// acknowledges instead of exhausting retries as invalid.
#[test]
fn a_source_bound_work_derives_base_from_the_workspace() {
    let root = tmp_root("sourcebound");
    let store = SqliteStore::in_memory(1).unwrap();
    let mut cfg = config();
    cfg.declared_capabilities.insert(HostCapability::IssueRead);
    let source = SourceSnapshot {
        issue: IssueRef {
            repository: repo(),
            issue: IssueId::trusted("issue-1"),
            display_number: Some(1),
        },
        source_revision: "issue-version-token".into(),
        content_digest: "cd-1".into(),
    };
    let req = CommandRequest {
        actor: Actor {
            kind: ActorKind::Human,
            external_id: "op".into(),
        },
        origin: Origin("cli".into()),
        idempotency_key: "h-srcb".into(),
        body: CommandBody::Submit {
            intent: "produce an artifact".into(),
            sources: vec![source.clone()],
            constraints: vec![],
            verification_argv: vec![],
        },
    };
    let d = handle_command(&store, &cfg, &req, NOW).unwrap();
    let (w, e) = match d {
        CommandDecision::Accepted {
            work_id, effect, ..
        } => (
            work_id.unwrap(),
            match effect {
                CommandEffect::WorkCreated { execution_id } => execution_id,
                other => panic!("unexpected effect {other:?}"),
            },
        ),
        other => panic!("submit rejected: {other:?}"),
    };
    // The matching authoritative read settles the source gate.
    handle_observation(
        &store,
        &cfg,
        &Observation {
            delivery: None,
            subject_work_id: Some(w.clone()),
            subject_execution_id: None,
            fact: ObservationFact::Issue {
                issue: source.issue.clone(),
                source_revision: source.source_revision.clone(),
                content_digest: source.content_digest.clone(),
                state: IssueState::Open,
            },
            observed_unix_seconds: NOW,
        },
        NOW,
    )
    .unwrap();
    assert!(
        store
            .transact(|tx| admit_queued(tx, &cfg, &e, NOW))
            .unwrap()
            .is_some(),
        "execution admitted"
    );

    let ws_root = root.join("ws");
    std::fs::create_dir_all(&ws_root).unwrap();
    let ws_dir = ws_root.join(format!("ws-{}", w.0));
    let (base, _produced) = git_workspace(&ws_dir, &w);
    let layout = HandoffLayout {
        repository: bare_repo(&root),
        workspace_root: ws_root.clone(),
        out_root: root.join("out"),
    };
    let sup = Supervisor::new(PosixProcessControl, ws_root.clone(), vec![], cfg)
        .with_handoff(layout.clone());

    let invocation = done_invocation(&e, "inv-srcb");
    insert_invocation(&store, &invocation);
    sup.drive_handoffs(&store).unwrap();
    let h = row(&store, &artifact_id(&invocation)).expect("handoff row opened");
    assert_eq!(
        h.base, base,
        "the fork point of the managed branch, not the issue version token"
    );
    assert_eq!(h.state, HandoffState::Acknowledged);
}

/// The requirement slot is an index into the *current* spec's argv —
/// a spec revision that changes the command behind `verification_argv[0]`
/// creates a new requirement the recorded Pass does not cover. The new
/// argv runs against the produced commit and replaces the stale row.
#[test]
fn a_changed_verification_argv_is_rerun() {
    let root = tmp_root("argvchange");
    let store = SqliteStore::in_memory(1).unwrap();
    let (w, e) = admit(&store, "h-argv");
    let (layout, ws_dir) = layout(&root, &w);
    let (base, _produced) = git_workspace(&ws_dir, &w);
    let invocation = done_invocation(&e, "inv-argv");
    insert_invocation(&store, &invocation);
    let aid = artifact_id(&invocation);
    let mut s = seed(&invocation, &w, &base);
    s.verification_argv = vec![vec!["sh".into(), "-c".into(), "true".into()]];
    handoff::drive(&store, &layout, &aid, &s, 3).unwrap();
    let results = store.transact(|tx| tx.verification_results(&aid)).unwrap();
    assert_eq!(results[0].result, VerificationOutcome::Pass);

    // The spec now declares a different command for the same slot.
    s.verification_argv = vec![vec!["sh".into(), "-c".into(), "exit 7".into()]];
    handoff::drive(&store, &layout, &aid, &s, 3).unwrap();
    let results = store.transact(|tx| tx.verification_results(&aid)).unwrap();
    assert_eq!(results.len(), 1, "the new argv replaces the stale row");
    assert_eq!(results[0].argv, vec!["sh", "-c", "exit 7"]);
    assert_eq!(
        results[0].result,
        VerificationOutcome::Fail,
        "the changed argv ran — the old Pass never covers it"
    );
}

/// A workspace whose git metadata is unreadable rides the bounded
/// recovery path — never a sweep-aborting error and never a silently
/// empty base that would skip the stale-base and nothing-produced
/// checks and acknowledge a foreign commit.
#[test]
fn an_unreadable_workspace_base_wedges_the_recovery_bound() {
    let root = tmp_root("badbase");
    let store = SqliteStore::in_memory(1).unwrap();
    let (w, e) = admit(&store, "h-badbase");
    let ws_root = root.join("ws");
    let ws_dir = ws_root.join(format!("ws-{}", w.0));
    std::fs::create_dir_all(ws_dir.join(".git")).unwrap();
    let layout = HandoffLayout {
        repository: bare_repo(&root),
        workspace_root: ws_root.clone(),
        out_root: root.join("out"),
    };
    let sup = Supervisor::new(PosixProcessControl, ws_root, vec![], config()).with_handoff(layout);
    let invocation = done_invocation(&e, "inv-badbase");
    insert_invocation(&store, &invocation);
    let aid = artifact_id(&invocation);
    // Each pass charges a transport attempt; the sweep itself never
    // aborts (other rows and the recovery pass still run), and the
    // workspace never gets an empty base.
    for _ in 0..3 {
        sup.drive_handoffs(&store).unwrap();
    }
    let h = row(&store, &aid).expect("handoff row opened");
    assert_eq!(h.state, HandoffState::ExportStarted);
    assert_eq!(h.attempt, 3, "each failed pass charged an attempt");
    sup.drive_handoffs(&store).unwrap();
    let h = row(&store, &aid).unwrap();
    assert_eq!(h.state, HandoffState::Invalid);
    assert_eq!(
        h.invalid_reason.as_deref(),
        Some("recovery_exhausted"),
        "a lost workspace converges on the bound, not on a crashed sweep"
    );
}
