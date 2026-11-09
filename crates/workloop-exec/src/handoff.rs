//! Durable artifact handoff and verification (docs/worker-protocol.md):
//! `export_started → bundle → quarantine import → ref → acknowledged`.
//! Every stage boundary is a store commit, so a crash resumes from the
//! last committed row — the bundle file and quarantine objects are
//! transient transport, never the durable body. Workspace and out
//! directory stay retained until `acknowledged` (row *and* ref).

use std::path::{Path, PathBuf};
use std::process::Command;

use workloop_core::domain::*;
use workloop_core::store::{Effect, Store, StoreError};

/// On-disk anchors for handoff transport.
#[derive(Debug, Clone)]
pub struct HandoffLayout {
    /// Installation-owned bare repository holding `refs/taisan/*`.
    pub repository: PathBuf,
    /// Root containing per-Work workspaces (`ws-<work_id>`).
    pub workspace_root: PathBuf,
    /// Root containing per-Invocation out directories.
    pub out_root: PathBuf,
}

impl HandoffLayout {
    fn workspace_of(&self, work_id: &WorkId) -> PathBuf {
        self.workspace_root.join(format!("ws-{}", work_id.0))
    }

    fn out_dir_of(&self, handoff: &ArtifactHandoff) -> PathBuf {
        self.out_root
            .join(&handoff.execution_id.0)
            .join(&handoff.invocation_id.0)
    }
}

/// The durable ref one acknowledged artifact occupies.
pub fn artifact_ref(artifact_id: &str) -> String {
    format!("refs/taisan/artifacts/{artifact_id}")
}

/// The quarantine ref an in-flight import occupies.
pub fn quarantine_ref(artifact_id: &str) -> String {
    format!("refs/taisan/quarantine/{artifact_id}")
}

#[derive(Debug)]
pub enum HandoffError {
    Store(StoreError),
    /// A transport step could not be completed; the row is unchanged
    /// and the sweep retries within the recovery bound.
    Transport(String),
    Io(std::io::Error),
}

impl std::fmt::Display for HandoffError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Store(e) => write!(f, "{e}"),
            Self::Transport(d) => write!(f, "handoff transport: {d}"),
            Self::Io(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for HandoffError {}

impl From<StoreError> for HandoffError {
    fn from(e: StoreError) -> Self {
        Self::Store(e)
    }
}

impl From<std::io::Error> for HandoffError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

/// The export-side git profile (docs/worker-protocol.md): hooks and
/// fsmonitor disabled, no credential helpers, file protocol only — the
/// workspace's own config, hooks, and attributes never run trusted code.
/// Every git invocation in this module goes through here.
fn git_output(dir: &Path, args: &[&str]) -> Result<std::process::Output, HandoffError> {
    Ok(Command::new("git")
        .arg("-C")
        .arg(dir)
        .arg("-c")
        .arg("core.hooksPath=/dev/null")
        .arg("-c")
        .arg("core.fsmonitor=false")
        .arg("-c")
        .arg("credential.helper=")
        .arg("-c")
        .arg("protocol.allow=user")
        .args(args)
        .output()?)
}

fn git(dir: &Path, args: &[&str]) -> Result<String, HandoffError> {
    let output = git_output(dir, args)?;
    if !output.status.success() {
        return Err(HandoffError::Transport(format!(
            "git {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// `git merge-base --is-ancestor` distinguishes a real descendant (0)
/// from a non-descendant (1); any other exit is a transport failure,
/// never a quiet "no".
fn is_ancestor(dir: &Path, base: &str, produced: &str) -> Result<bool, HandoffError> {
    let out = git_output(dir, &["merge-base", "--is-ancestor", base, produced])?;
    match out.status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => Err(HandoffError::Transport(format!(
            "merge-base --is-ancestor: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ))),
    }
}

/// `git bundle verify` must pass *and* list the produced commit — a
/// verified bundle for the wrong tip is corrupt for this artifact.
/// Runs inside the installation-owned repository with the same
/// hardened profile as every other git call.
fn verify_bundle(repo: &Path, bundle: &Path, produced: &str) -> bool {
    let Ok(out) = git_output(
        repo,
        &["bundle", "verify", bundle.to_str().unwrap_or_default()],
    ) else {
        return false;
    };
    out.status.success()
        && (String::from_utf8_lossy(&out.stderr).contains(produced)
            || String::from_utf8_lossy(&out.stdout).contains(produced))
}

/// Start or resume the handoff for one finished Invocation's required
/// artifact. `seed.base` is the spec-recorded revision `produced` must
/// descend from; `artifact_id` is the required-artifact identity.
/// `max_attempts` bounds export/import retries — counting here is
/// separate from `max_invocations`.
pub fn drive(
    store: &impl Store,
    layout: &HandoffLayout,
    artifact_id: &str,
    seed: &HandoffSeed,
    max_attempts: u32,
) -> Result<ArtifactHandoff, HandoffError> {
    let existing = store.transact(|tx| tx.artifact_handoff(artifact_id))?;
    let mut handoff = match existing {
        Some(h) => h,
        None => {
            let mut h = ArtifactHandoff {
                artifact_id: artifact_id.to_string(),
                invocation_id: seed.invocation.id.clone(),
                request_id: seed.invocation.request_id.clone(),
                work_id: seed.work_id.clone(),
                execution_id: seed.invocation.execution_id.clone(),
                base: seed.base.clone(),
                produced: None,
                manifest_digest: None,
                state: HandoffState::ExportStarted,
                invalid_reason: None,
                attempt: 0,
                recorded_seq: 0,
            };
            store.transact(|tx| {
                h.recorded_seq = tx.alloc_seq()?;
                h.attempt = 1;
                tx.apply(&Effect::UpsertHandoff { handoff: h.clone() })
            })?;
            h
        }
    };
    advance(store, layout, &mut handoff, seed, max_attempts)?;
    if handoff.state == HandoffState::Acknowledged {
        run_verifications(store, layout, &handoff, seed)?;
    }
    Ok(handoff)
}

/// Each declared verification argv runs once per artifact on the exact
/// produced commit. Already-recorded requirements are skipped, so a
/// crash between runs resumes without duplicating results; an empty
/// argv has nothing to spawn.
fn run_verifications(
    store: &impl Store,
    layout: &HandoffLayout,
    handoff: &ArtifactHandoff,
    seed: &HandoffSeed,
) -> Result<(), HandoffError> {
    if seed.verification_argv.is_empty() {
        return Ok(());
    }
    let recorded: std::collections::HashSet<String> = store
        .transact(|tx| tx.verification_results(&handoff.artifact_id))?
        .into_iter()
        .map(|r| r.requirement)
        .collect();
    let scratch = layout.out_dir_of(handoff);
    for (i, argv) in seed.verification_argv.iter().enumerate() {
        if argv.is_empty() {
            continue;
        }
        let requirement = format!("verification_argv[{i}]");
        if recorded.contains(&requirement) {
            continue;
        }
        verify(
            store,
            layout,
            &handoff.artifact_id,
            &requirement,
            argv,
            &scratch,
        )?;
    }
    Ok(())
}

/// What `drive` needs beyond the durable row: the launch context that
/// is *not* re-readable after a crash (workspace path, out dir, base).
#[derive(Debug, Clone)]
pub struct HandoffSeed {
    pub invocation: Invocation,
    /// Spec-recorded base revision the produced commit must descend
    /// from (`SourceSnapshot.source_revision`).
    pub base: String,
    /// Work branch the worker committed on (`work/<work_id>`).
    pub branch: String,
    pub work_id: WorkId,
    /// `verification_argv` entries declared for the artifact.
    pub verification_argv: Vec<Vec<String>>,
}

/// Advance `handoff` by at least one stage, recovering whatever partial
/// transport state the last crash left. Idempotent: calling `advance`
/// on a terminal row is a no-op.
fn advance(
    store: &impl Store,
    layout: &HandoffLayout,
    handoff: &mut ArtifactHandoff,
    seed: &HandoffSeed,
    max_attempts: u32,
) -> Result<(), HandoffError> {
    loop {
        let step = match handoff.state {
            HandoffState::ExportStarted => export_stage(store, layout, handoff, seed, max_attempts),
            HandoffState::BundleWritten => import_stage(store, layout, handoff, seed),
            HandoffState::Imported => acknowledge_stage(store, layout, handoff),
            HandoffState::Acknowledged | HandoffState::Invalid => return Ok(()),
        };
        match step {
            // A transport failure is charged against the recovery bound:
            // the row either retries on the next sweep or settles
            // invalid — it never wedges non-terminal forever while the
            // workspace stays retained.
            Err(HandoffError::Transport(_)) => {
                if handoff.attempt >= max_attempts {
                    return mark_invalid(store, handoff, "recovery_exhausted");
                }
                handoff.attempt += 1;
                persist(store, handoff)?;
                return Ok(());
            }
            Err(e) => return Err(e),
            Ok(()) => {}
        }
        if matches!(
            handoff.state,
            HandoffState::Invalid | HandoffState::Acknowledged
        ) {
            return Ok(());
        }
        // The loop re-reads the match arms each pass; each arm commits
        // exactly one transition, so progress is guaranteed per pass.
        if handoff.state == HandoffState::ExportStarted {
            // export_stage either transitions or returns; reaching here
            // with the same state means a transport refusal — stop and
            // let the next sweep retry.
            return Ok(());
        }
    }
}

fn persist(store: &impl Store, handoff: &mut ArtifactHandoff) -> Result<(), StoreError> {
    store.transact(|tx| {
        handoff.recorded_seq = tx.alloc_seq()?;
        tx.apply(&Effect::UpsertHandoff {
            handoff: handoff.clone(),
        })
    })
}

fn mark_invalid(
    store: &impl Store,
    handoff: &mut ArtifactHandoff,
    reason: &str,
) -> Result<(), HandoffError> {
    handoff.state = HandoffState::Invalid;
    handoff.invalid_reason = Some(reason.to_string());
    persist(store, handoff)?;
    Ok(())
}

/// `export_started`: produce (or re-validate) the bundle. A crashed
/// export leaves an absent or truncated bundle — both re-export, never
/// trusted. A bundle that verifies is reused; `attempt` was already
/// charged when the row was created.
fn export_stage(
    store: &impl Store,
    layout: &HandoffLayout,
    handoff: &mut ArtifactHandoff,
    seed: &HandoffSeed,
    max_attempts: u32,
) -> Result<(), HandoffError> {
    let out_dir = layout.out_dir_of(handoff);
    let bundle = out_dir.join("export.bundle");
    let workspace = layout.workspace_of(&handoff.work_id);

    let produced = match git(&workspace, &["rev-parse", "--verify", &seed.branch]) {
        Ok(p) => p,
        Err(_) => {
            if handoff.attempt >= max_attempts {
                return mark_invalid(store, handoff, "recovery_exhausted");
            }
            handoff.attempt += 1;
            persist(store, handoff)?;
            return Ok(());
        }
    };

    // Stale check: produced must descend from the recorded base. A spec
    // without source pins records no base — the export covers the whole
    // produced history and the stale check does not apply.
    if !handoff.base.is_empty() && !is_ancestor(&workspace, &handoff.base, &produced)? {
        return mark_invalid(store, handoff, "stale_base");
    }

    if !(bundle.exists() && verify_bundle(&layout.repository, &bundle, &produced)) {
        if bundle.exists() {
            // Truncated/corrupt partial: not evidence — remove and
            // re-export within the bound.
            std::fs::remove_file(&bundle)?;
        }
        if handoff.attempt >= max_attempts {
            return mark_invalid(store, handoff, "recovery_exhausted");
        }
        std::fs::create_dir_all(&out_dir)?;
        // The bundle carries the work branch's full object closure so it
        // verifies and imports into a handoff repository without
        // prerequisites; `base..produced` remains the manifest's object
        // list, not the transport.
        git(
            &workspace,
            &[
                "bundle",
                "create",
                bundle.to_str().unwrap_or_default(),
                &seed.branch,
            ],
        )
        .inspect_err(|_| {
            let _ = std::fs::remove_file(&bundle);
        })?;
        if !verify_bundle(&layout.repository, &bundle, &produced) {
            let _ = std::fs::remove_file(&bundle);
            handoff.attempt += 1;
            if handoff.attempt >= max_attempts {
                return mark_invalid(store, handoff, "corrupt_bundle");
            }
            persist(store, handoff)?;
            return Ok(());
        }
    }

    handoff.produced = Some(produced);
    handoff.state = HandoffState::BundleWritten;
    persist(store, handoff)?;
    Ok(())
}

/// `bundle_written`: import the verified bundle into quarantine and
/// commit the `imported` row with the manifest digest. A crashed import
/// may leave quarantine objects; the sweep discards and re-imports.
fn import_stage(
    store: &impl Store,
    layout: &HandoffLayout,
    handoff: &mut ArtifactHandoff,
    seed: &HandoffSeed,
) -> Result<(), HandoffError> {
    let Some(produced) = handoff.produced.clone() else {
        return mark_invalid(store, handoff, "missing_produced");
    };
    let bundle = layout.out_dir_of(handoff).join("export.bundle");
    if !bundle.exists() || !verify_bundle(&layout.repository, &bundle, &produced) {
        // The transport body is gone/corrupt after commit — re-export
        // from the still-retained workspace rather than trusting
        // anything else.
        handoff.state = HandoffState::ExportStarted;
        persist(store, handoff)?;
        return Ok(());
    }

    // Discard any quarantine a crashed import left, then re-import the
    // bundle's work-branch ref under the artifact's quarantine name.
    let quarantine = quarantine_ref(&handoff.artifact_id);
    let _ = git(&layout.repository, &["update-ref", "-d", &quarantine]);
    git(
        &layout.repository,
        &[
            "fetch",
            bundle.to_str().unwrap_or_default(),
            &format!("+refs/heads/{}:{quarantine}", seed.branch),
        ],
    )?;
    git(&layout.repository, &["fsck", "--connectivity-only"])
        .map_err(|_| HandoffError::Transport("fsck connectivity failed".into()))?;

    let manifest = canonical_manifest(store, handoff, &produced, layout)?;
    handoff.manifest_digest = Some(sha256_hex(
        "artifact-manifest-v1",
        &canonical_json(&manifest),
    ));
    handoff.state = HandoffState::Imported;
    persist(store, handoff)?;
    Ok(())
}

/// `imported`: establish `refs/taisan/artifacts/<id>` under the
/// expected-absent guard, then commit `acknowledged` only after the ref
/// reads back as `produced`. Crash windows: ref present + row imported
/// → verify ref == produced and acknowledge; row without ref →
/// re-derive from quarantine after re-verifying the manifest.
fn acknowledge_stage(
    store: &impl Store,
    layout: &HandoffLayout,
    handoff: &mut ArtifactHandoff,
) -> Result<(), HandoffError> {
    let Some(produced) = handoff.produced.clone() else {
        return mark_invalid(store, handoff, "missing_produced");
    };
    let reference = artifact_ref(&handoff.artifact_id);

    match git(&layout.repository, &["rev-parse", "--verify", &reference]) {
        Ok(current) if current == produced => {
            handoff.state = HandoffState::Acknowledged;
            persist(store, handoff)?;
            return Ok(());
        }
        Ok(_) => {
            // A ref we did not write occupying the name is a conflict —
            // never overwrite another artifact's acknowledgement.
            return mark_invalid(store, handoff, "ref_conflict");
        }
        Err(_) => {}
    }

    // Recreate the ref only after the stored manifest still verifies
    // against the imported objects.
    let manifest = canonical_manifest(store, handoff, &produced, layout)?;
    let digest = sha256_hex("artifact-manifest-v1", &canonical_json(&manifest));
    if handoff.manifest_digest.as_deref() != Some(digest.as_str()) {
        return mark_invalid(store, handoff, "digest_mismatch");
    }
    if git(
        &layout.repository,
        &[
            "update-ref",
            &reference,
            &produced,
            "", // expected absent
        ],
    )
    .is_err()
    {
        // Lost a create race: re-read rather than overwrite.
        match git(&layout.repository, &["rev-parse", "--verify", &reference]) {
            Ok(current) if current == produced => {}
            _ => return mark_invalid(store, handoff, "ref_conflict"),
        }
    }
    handoff.state = HandoffState::Acknowledged;
    persist(store, handoff)?;
    Ok(())
}

/// Canonical manifest (docs/worker-protocol.md): `{format_version,
/// repository, base, produced, tree, sorted object ids of base..
/// produced}`.
fn canonical_manifest(
    store: &impl Store,
    handoff: &ArtifactHandoff,
    produced: &str,
    layout: &HandoffLayout,
) -> Result<serde_json::Value, HandoffError> {
    let tree = git(
        &layout.repository,
        &["rev-parse", &format!("{produced}^{{tree}}")],
    )?;
    let range = if handoff.base.is_empty() {
        produced.to_string()
    } else {
        format!("{}..{}", handoff.base, produced)
    };
    let objects_out = git(&layout.repository, &["rev-list", "--objects", &range])?;
    let mut objects: Vec<String> = objects_out
        .lines()
        .filter_map(|l| l.split_whitespace().next().map(String::from))
        .collect();
    objects.sort();
    objects.dedup();
    let repository = store
        .transact(|tx| tx.work(&handoff.work_id))?
        .map(|w| w.binding.repository.repository.as_str().to_string())
        .unwrap_or_default();
    Ok(serde_json::json!({
        "format_version": 1,
        "repository": repository,
        "base": handoff.base,
        "produced": produced,
        "tree": tree,
        "objects": objects,
    }))
}

/// Whether the workspace and the out-directory bundle may be reclaimed:
/// only an `acknowledged` row *and* a ref resolving to `produced`.
pub fn reclaimable(layout: &HandoffLayout, handoff: &ArtifactHandoff) -> bool {
    if handoff.state != HandoffState::Acknowledged {
        return false;
    }
    let Some(produced) = &handoff.produced else {
        return false;
    };
    git(
        &layout.repository,
        &["rev-parse", "--verify", &artifact_ref(&handoff.artifact_id)],
    )
    .map(|r| r == *produced)
    .unwrap_or(false)
}

/// Run one declared verification argv on an independent clean worktree
/// of the exact produced commit and record the result. Runs only on an
/// acknowledged handoff — never on a claim or an unverified tree.
pub fn verify(
    store: &impl Store,
    layout: &HandoffLayout,
    artifact_id: &str,
    requirement: &str,
    argv: &[String],
    scratch: &Path,
) -> Result<VerificationResult, HandoffError> {
    let handoff = store
        .transact(|tx| tx.artifact_handoff(artifact_id))?
        .ok_or_else(|| HandoffError::Transport("no handoff row".into()))?;
    if handoff.state != HandoffState::Acknowledged {
        return Err(HandoffError::Transport(format!(
            "handoff {artifact_id} not acknowledged ({:?})",
            handoff.state
        )));
    }
    let produced = handoff
        .produced
        .clone()
        .ok_or_else(|| HandoffError::Transport("acknowledged without produced".into()))?;
    let reference = artifact_ref(artifact_id);
    let resolved = git(&layout.repository, &["rev-parse", "--verify", &reference])?;
    if resolved != produced {
        return Err(HandoffError::Transport(format!(
            "ref {reference} resolves {resolved}, not {produced}"
        )));
    }

    let environment_digest = sha256_hex(
        "verification-env-v1",
        &canonical_json(&serde_json::json!({
            "argv": argv,
            "produced": produced,
            "ref": reference,
        })),
    );

    // Independent worktree of the exact commit; nothing inherits from
    // another command's checkout.
    let worktree = scratch.join(format!(
        "verify-{}-{}",
        artifact_id.replace('/', "-"),
        requirement.replace(['/', '[', ']'], "-")
    ));
    let _ = std::fs::remove_dir_all(&worktree);
    let (result, output) = match git(
        &layout.repository,
        &[
            "worktree",
            "add",
            "--detach",
            worktree.to_str().unwrap_or_default(),
            &produced,
        ],
    ) {
        Ok(_) => {
            let run = Command::new(&argv[0])
                .args(&argv[1..])
                .current_dir(&worktree)
                .env_clear()
                .env("PATH", std::env::var("PATH").unwrap_or_default())
                .output();
            match run {
                Ok(out) => {
                    let mut bytes = out.stdout;
                    bytes.extend_from_slice(&out.stderr);
                    bytes.truncate(64 * 1024);
                    let text = String::from_utf8_lossy(&bytes).into_owned();
                    (
                        if out.status.success() {
                            VerificationOutcome::Pass
                        } else {
                            VerificationOutcome::Fail
                        },
                        text,
                    )
                }
                Err(e) => (VerificationOutcome::Inconclusive, format!("spawn: {e}")),
            }
        }
        Err(e) => (VerificationOutcome::Inconclusive, format!("worktree: {e}")),
    };
    let _ = git(
        &layout.repository,
        &[
            "worktree",
            "remove",
            "--force",
            worktree.to_str().unwrap_or_default(),
        ],
    );

    let mut result = VerificationResult {
        id: String::new(),
        artifact_id: artifact_id.to_string(),
        requirement: requirement.to_string(),
        argv: argv.to_vec(),
        produced,
        environment_digest,
        result,
        output,
        recorded_seq: 0,
    };
    store.transact(|tx| {
        result.recorded_seq = tx.alloc_seq()?;
        result.id = tx.alloc_id("verify")?;
        tx.apply(&Effect::InsertVerificationResult {
            result: result.clone(),
        })
    })?;
    Ok(result)
}
