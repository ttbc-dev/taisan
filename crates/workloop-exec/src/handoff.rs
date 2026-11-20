//! Durable artifact handoff and verification (docs/worker-protocol.md):
//! `export_started → bundle → quarantine import → ref → acknowledged`.
//! Every stage boundary is a store commit, so a crash resumes from the
//! last committed row — the bundle file and quarantine objects are
//! transient transport, never the durable body. Workspace and out
//! directory stay retained until `acknowledged` (row *and* ref).

use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use workloop_core::domain::*;
use workloop_core::ports::{ProcessControlPort, WorkerEnv};
use workloop_core::store::{Effect, Store, StoreError};
use workloop_core::worker::IsolationProfile;

use crate::containment::{self, Gate, MountPlan};
use crate::egress::ConnectProxy;
use crate::isolation;

/// The contained verification boundary (`docs/v0.3.md` §14.5). When
/// present, every declared verification command runs inside the same
/// enforced profile Worker launches use: the produced tree and the
/// handoff repository are read-only, toolchain and dependency caches
/// are supervisor-mounted read-only, per-command scratch is separate,
/// and egress is a CONNECT proxy limited to the installation
/// allowlist plus the request's declared hosts. Verification carries
/// no runtime credential and no Worker model-API proxy; `None` keeps
/// the recorded v1 hygiene-only runner.
#[derive(Clone)]
pub struct VerificationSandbox {
    /// The process-control port the containment gate's negative
    /// probes run through — the same port Worker launches use.
    pub port: Arc<dyn ProcessControlPort + Send + Sync>,
    /// The approved isolation profile verification binds.
    pub profile: IsolationProfile,
    /// Pinned toolchain → `/taisan/toolchain` read-only, plus an
    /// own-path bind so declared argv naming it absolutely resolves.
    pub toolchain: Option<PathBuf>,
    /// Read-only dependency caches, bound at their own paths.
    pub caches: Vec<PathBuf>,
    /// Installation-default verification egress hosts (bare names).
    pub egress_allowlist: Vec<String>,
    /// Worker model-API hosts — an effective verification host that
    /// names one is refused, never granted (§14.5 disjointness).
    pub model_api_hosts: Vec<String>,
    /// Supervisor-owned directory holding bounded egress evidence —
    /// never inside a worktree. Proxy sockets land here when the path
    /// fits `sun_path`, else in the digest-derived owner-only
    /// fallback `egress::socket_dir` picks.
    pub egress_dir: PathBuf,
    /// The binary the in-sandbox relay runs as.
    pub relay_exe: PathBuf,
}

impl std::fmt::Debug for VerificationSandbox {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VerificationSandbox")
            .field("profile", &self.profile.id)
            .field("toolchain", &self.toolchain)
            .field("caches", &self.caches)
            .field("egress_allowlist", &self.egress_allowlist)
            .field("model_api_hosts", &self.model_api_hosts)
            .field("egress_dir", &self.egress_dir)
            .field("relay_exe", &self.relay_exe)
            .finish_non_exhaustive()
    }
}

/// On-disk anchors for handoff transport.
#[derive(Debug, Clone)]
pub struct HandoffLayout {
    /// Installation-owned bare repository holding `refs/taisan/*`.
    pub repository: PathBuf,
    /// Root containing per-Work workspaces (`ws-<work_id>`).
    pub workspace_root: PathBuf,
    /// Root containing per-Invocation out directories.
    pub out_root: PathBuf,
    /// The §14.5 verification sandbox; `None` is the recorded
    /// hygiene-only runner.
    pub verification: Option<VerificationSandbox>,
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
        .arg("protocol.allow=never")
        .arg("-c")
        .arg("protocol.file.allow=always")
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

/// Recover the handoff `base`: the fork point of the managed `branch`
/// against the workspace's non-work local branch (the checkout the
/// writer started from). A spec's `source_revision` is a host
/// issue-version token — never a commit — so it cannot bound git
/// ancestry; the durable record lives in `artifact_handoffs.base`.
/// Empty means the workspace records no base branch to bound against —
/// a git failure is an error, never a silent empty (an empty base
/// skips both the stale-base and nothing-produced checks). When
/// `branch` shares no history with the base branch, the base ref
/// itself is returned — it can never be an ancestor of `branch`, so
/// the export's stale check classifies the foreign history
/// `stale_base`.
pub fn derive_base(workspace: &Path, branch: &str) -> Result<String, HandoffError> {
    let refs = git(
        workspace,
        &["for-each-ref", "--format=%(refname:short)", "refs/heads/"],
    )?;
    let Some(base_ref) = refs
        .lines()
        .find(|r| !r.is_empty() && *r != branch && !r.starts_with("work/"))
    else {
        return Ok(String::new());
    };
    match git_output(workspace, &["merge-base", base_ref, branch]) {
        Ok(out) if out.status.success() => {
            Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
        }
        Ok(out) if out.status.code() == Some(1) => Ok(base_ref.to_string()),
        Ok(out) => Err(HandoffError::Transport(format!(
            "merge-base {base_ref} {branch}: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ))),
        Err(e) => Err(e),
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
/// artifact. `seed.base` is the workspace-derived fork point `produced`
/// must descend from; `artifact_id` is the required-artifact identity.
/// `max_attempts` bounds export/import transport attempts — counting
/// here is separate from `max_invocations`.
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
            match store.transact(|tx| {
                h.recorded_seq = tx.alloc_seq()?;
                tx.apply(&Effect::UpsertHandoff {
                    handoff: h.clone(),
                    expected_seq: 0,
                })
            }) {
                Ok(()) => h,
                // A concurrent supervisor created the row first —
                // continue from their committed row, never overwrite.
                Err(StoreError::ConstraintConflict { .. }) => store
                    .transact(|tx| tx.artifact_handoff(artifact_id))?
                    .unwrap_or(h),
                Err(e) => return Err(e.into()),
            }
        }
    };
    if let Err(e) = advance(store, layout, &mut handoff, seed, max_attempts) {
        // A compare-and-set conflict means another supervisor
        // advanced the row — the sweep re-reads it next pass instead
        // of aborting this one.
        if matches!(
            e,
            HandoffError::Store(StoreError::ConstraintConflict { .. })
        ) {
            return Ok(handoff);
        }
        return Err(e);
    }
    if handoff.state == HandoffState::Acknowledged {
        run_verifications(store, layout, &handoff, seed)?;
    }
    Ok(handoff)
}

/// Each declared verification argv runs once per artifact on the exact
/// produced commit. Recorded terminal results are final, so a crash
/// between runs resumes without duplicating evidence; an inconclusive
/// run is retried; an empty argv has nothing to spawn.
fn run_verifications(
    store: &impl Store,
    layout: &HandoffLayout,
    handoff: &ArtifactHandoff,
    seed: &HandoffSeed,
) -> Result<(), HandoffError> {
    if seed.verification_argv.is_empty() {
        return Ok(());
    }
    // Recorded terminal results are final *for the same argv and
    // produced commit* — the requirement slot is an index, and a new
    // spec revision can change the argv behind it, so argv (and
    // produced) must match before a Pass/Fail covers the requirement.
    // `Inconclusive` is retried — an inconclusive run (spawn, worktree,
    // or timeout failure) never satisfies or exhausts a requirement.
    let recorded: std::collections::HashMap<String, VerificationResult> = store
        .transact(|tx| tx.verification_results(&handoff.artifact_id))?
        .into_iter()
        .map(|r| (r.requirement.clone(), r))
        .collect();
    let scratch = layout.out_dir_of(handoff);
    for (i, argv) in seed.verification_argv.iter().enumerate() {
        if argv.is_empty() {
            continue;
        }
        let requirement = format!("verification_argv[{i}]");
        if let Some(r) = recorded.get(&requirement)
            && matches!(
                r.result,
                VerificationOutcome::Pass | VerificationOutcome::Fail
            )
            && r.argv == *argv
            && Some(&r.produced) == handoff.produced.as_ref()
        {
            continue;
        }
        verify(
            store,
            layout,
            &handoff.artifact_id,
            &requirement,
            argv,
            &scratch,
            &seed.verification_egress_hosts,
        )?;
    }
    Ok(())
}

/// What `drive` needs beyond the durable row: the launch context that
/// is *not* re-readable after a crash (workspace path, out dir, base).
#[derive(Debug, Clone)]
pub struct HandoffSeed {
    pub invocation: Invocation,
    /// Fork point of the managed branch in the workspace repo (see
    /// [`derive_base`]) — the revision `produced` must descend from.
    /// Empty when the workspace records no base branch.
    pub base: String,
    /// Work branch the worker committed on (`work/<work_id>`).
    pub branch: String,
    pub work_id: WorkId,
    /// `verification_argv` entries declared for the artifact.
    pub verification_argv: Vec<Vec<String>>,
    /// The request's immutable `required_capabilities
    /// .verification_egress_hosts` — the Work-declared half of the
    /// §14.5 effective egress set. Empty for v1 dispatches, which
    /// carry no declared hosts.
    pub verification_egress_hosts: Vec<String>,
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
        // `attempt` counts *failed* transport attempts, so a bound of N
        // performs exactly N: the entry check refuses attempt N+1 and
        // settles the row instead of silently consuming it.
        if !matches!(
            handoff.state,
            HandoffState::Acknowledged | HandoffState::Invalid
        ) && handoff.attempt >= max_attempts
        {
            return mark_invalid(store, handoff, "recovery_exhausted");
        }
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
        // The write is a compare-and-set on the seq this row was read
        // at — a concurrent supervisor's write lands first, and this
        // becomes a conflict instead of regressing the row.
        let expected_seq = handoff.recorded_seq;
        handoff.recorded_seq = tx.alloc_seq()?;
        tx.apply(&Effect::UpsertHandoff {
            handoff: handoff.clone(),
            expected_seq,
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
/// trusted. A bundle that verifies is reused. Transport failures
/// propagate to the driver, which charges them against the bound.
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

    let produced = git(&workspace, &["rev-parse", "--verify", &seed.branch])?;

    // The durable base is derived here, inside the bounded stage —
    // never from a host token, and never silently empty: a derivation
    // failure is a transport failure charged to the bound, so a lost
    // or unreadable workspace converges on `recovery_exhausted` like
    // every other transport loss. An empty result is the honest
    // "no base branch" case only.
    if handoff.base.is_empty() {
        handoff.base = derive_base(&workspace, &seed.branch)?;
    }

    // Stale check: produced must descend from the recorded base. A spec
    // without source pins records no base — the export covers the whole
    // produced history and the stale check does not apply.
    if !handoff.base.is_empty() && !is_ancestor(&workspace, &handoff.base, &produced)? {
        return mark_invalid(store, handoff, "stale_base");
    }

    // A branch that never moved past base produced nothing — for a
    // Coding/Fix writer that is an absent artifact, not a handoff.
    if matches!(
        seed.invocation.role,
        InvocationRole::Coding | InvocationRole::Fix
    ) && !handoff.base.is_empty()
        && produced == handoff.base
    {
        return mark_invalid(store, handoff, "nothing_produced");
    }

    if !(bundle.exists() && verify_bundle(&layout.repository, &bundle, &produced)) {
        if bundle.exists() {
            // Truncated/corrupt partial: not evidence — remove and
            // re-export within the bound.
            std::fs::remove_file(&bundle)?;
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
            // A created bundle that does not verify is corrupt evidence,
            // not transport: exhaust under its own reason.
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

    // Ref present from a crashed earlier pass: commit `acknowledged`
    // only when ref *and* manifest digest match — a stale digest under
    // the right ref is retained as invalid, never acknowledged.
    match git(&layout.repository, &["rev-parse", "--verify", &reference]) {
        Ok(current) if current == produced => {
            let manifest = canonical_manifest(store, handoff, &produced, layout)?;
            let digest = sha256_hex("artifact-manifest-v1", &canonical_json(&manifest));
            if handoff.manifest_digest.as_deref() != Some(digest.as_str()) {
                return mark_invalid(store, handoff, "digest_mismatch");
            }
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

/// Wall-clock bound for one verification command (docs/worker-protocol:
/// the runner is bounded by its own per-command timeout).
const VERIFICATION_COMMAND_TIMEOUT: Duration = Duration::from_secs(300);

enum VerificationRun {
    Finished(std::process::ExitStatus),
    TimedOut,
    SpawnFailed(std::io::Error),
    /// A typed pre-launch refusal — missing toolchain/cache, a
    /// containment-gate refusal, an unavailable proxy, or a failed
    /// toolchain preflight. Recorded `Inconclusive` like every other
    /// non-run outcome; the reason is the durable evidence.
    Refused(String),
}

/// Run `argv` with a bounded wall clock under the caller-built
/// environment. Output goes to `log_path`, not a pipe — a runner that
/// writes more than the pipe buffer would block on write and never
/// finish (the captured tail is read back after the child settles). A
/// timed-out runner is killed and recorded `Inconclusive`; a
/// signal-killed runner is `Inconclusive` too — an invented exit code
/// is not evidence.
fn run_verification_command(
    argv: &[String],
    cwd: &Path,
    log_path: &Path,
    env: &WorkerEnv,
    timeout: Duration,
) -> VerificationRun {
    let Some(program) = argv.first() else {
        return VerificationRun::SpawnFailed(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "empty verification argv",
        ));
    };
    let log = match std::fs::File::create(log_path) {
        Ok(f) => f,
        Err(e) => return VerificationRun::SpawnFailed(e),
    };
    let log_err = match log.try_clone() {
        Ok(f) => f,
        Err(e) => return VerificationRun::SpawnFailed(e),
    };
    let mut child = match Command::new(program)
        .args(&argv[1..])
        .current_dir(cwd)
        .env_clear()
        .envs(env.vars.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(log_err))
        // Own process group so a timeout kill reaches grandchildren
        // the runner spawned — the same containment the supervisor's
        // own spawn applies.
        .process_group(0)
        .spawn()
    {
        Ok(child) => child,
        Err(e) => return VerificationRun::SpawnFailed(e),
    };
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return VerificationRun::Finished(status),
            Ok(None) if Instant::now() >= deadline => {
                // The runner is its group leader: a group SIGKILL
                // reaps every descendant it spawned, then the leader
                // itself is reaped through the handle.
                unsafe { libc::kill(-(child.id() as i32), libc::SIGKILL) };
                let _ = child.kill();
                let _ = child.wait();
                return VerificationRun::TimedOut;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(25)),
            Err(e) => return VerificationRun::SpawnFailed(e),
        }
    }
}

/// The resolved §14.5 enforcement for one verification command:
/// either the recorded hygiene-only runner or the contained sandbox
/// context. `identity` is the `environment_digest` fragment — the
/// effective host set and toolchain/cache digests are part of the
/// recorded environment identity, never recomputed later.
enum Enforcement {
    Hygiene,
    Contained(Box<ContainedVerification>),
}

struct ContainedVerification {
    profile: IsolationProfile,
    port: Arc<dyn ProcessControlPort + Send + Sync>,
    toolchain: Option<PathBuf>,
    caches: Vec<PathBuf>,
    /// Effective egress hosts: installation allowlist ∪ the request's
    /// declared hosts, normalized and deduplicated.
    egress_hosts: Vec<String>,
    egress_dir: PathBuf,
    relay_exe: PathBuf,
    /// The gate's boundary identity — toolchain/cache digests plus
    /// the effective host set. Any change re-proves the battery.
    boundary_digest: String,
    identity: serde_json::Value,
}

/// Bounded content digest of one toolchain/cache path for the
/// environment identity: a file hashes its bytes (capped); a
/// directory hashes a sorted manifest of member names, kinds, sizes,
/// and mtimes — enough that an edit re-identifies the environment
/// without hashing unbounded trees.
fn verification_path_digest(path: &Path) -> Result<String, std::io::Error> {
    const MAX_FILE: u64 = 1024 * 1024;
    const MAX_ENTRIES: usize = 4096;
    let metadata = std::fs::symlink_metadata(path)?;
    if metadata.is_file() {
        use std::io::Read;
        let file = std::fs::File::open(path)?;
        let mut buf = Vec::new();
        file.take(MAX_FILE).read_to_end(&mut buf)?;
        return Ok(sha256_hex("verification-path-file-v1", &buf));
    }
    if !metadata.is_dir() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "not a file or directory",
        ));
    }
    let mut manifest = Vec::new();
    collect_path_manifest(path, path, &mut manifest, MAX_ENTRIES)?;
    manifest.sort();
    Ok(sha256_hex(
        "verification-path-dir-v1",
        &canonical_json(&serde_json::json!(manifest)),
    ))
}

fn collect_path_manifest(
    root: &Path,
    dir: &Path,
    out: &mut Vec<String>,
    budget: usize,
) -> Result<(), std::io::Error> {
    if out.len() >= budget {
        return Ok(());
    }
    for entry in std::fs::read_dir(dir)? {
        if out.len() >= budget {
            break;
        }
        let entry = entry?;
        let path = entry.path();
        let rel = path
            .strip_prefix(root)
            .unwrap_or(&path)
            .to_string_lossy()
            .into_owned();
        let metadata = entry.metadata()?;
        let mtime = metadata
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        if metadata.is_dir() {
            out.push(format!("dir:{rel}:{mtime}"));
            collect_path_manifest(root, &path, out, budget)?;
        } else if metadata.is_file() {
            out.push(format!("file:{rel}:{}:{mtime}", metadata.len()));
        }
    }
    Ok(())
}

/// Resolve the §14.5 enforcement for one command. `Err` is a typed
/// refusal reason recorded `Inconclusive` — a missing toolchain or
/// cache, an egress host that names a model-API destination, or an
/// unreadable boundary path is repairable configuration evidence,
/// never an open-network fallback.
fn prepare_enforcement(
    layout: &HandoffLayout,
    declared_egress_hosts: &[String],
) -> Result<Enforcement, String> {
    let Some(sandbox) = &layout.verification else {
        return Ok(Enforcement::Hygiene);
    };
    // Configured paths must exist before the first dispatch — a
    // missing toolchain or cache is a preflight failure, never a
    // download at verification time.
    if let Some(toolchain) = &sandbox.toolchain
        && !toolchain.exists()
    {
        return Err(format!(
            "verification.toolchain_missing: {}",
            toolchain.display()
        ));
    }
    for cache in &sandbox.caches {
        if !cache.exists() {
            return Err(format!("verification.cache_missing: {}", cache.display()));
        }
    }
    let mut hosts = sandbox.egress_allowlist.clone();
    for declared in declared_egress_hosts {
        let host = declared.trim().to_lowercase();
        if !host.is_empty() && !hosts.contains(&host) {
            hosts.push(host);
        }
    }
    hosts.sort();
    hosts.dedup();
    // Verification egress never contains a model-API host — an
    // effective host that names one is an admission gap, refused
    // rather than silently granted (§14.5).
    for host in &hosts {
        if sandbox
            .model_api_hosts
            .iter()
            .any(|model| model.eq_ignore_ascii_case(host))
        {
            return Err(format!(
                "verification.egress_denied: '{host}' is a model-API host"
            ));
        }
    }
    let toolchain_digest = match &sandbox.toolchain {
        None => serde_json::Value::Null,
        Some(path) => serde_json::Value::String(
            verification_path_digest(path)
                .map_err(|e| format!("verification.toolchain_unreadable: {e}"))?,
        ),
    };
    let cache_digests = sandbox
        .caches
        .iter()
        .map(|path| {
            verification_path_digest(path)
                .map_err(|e| format!("verification.cache_unreadable: {e}"))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let boundary_digest = sha256_hex(
        "verification-boundary-v1",
        &canonical_json(&serde_json::json!({
            "toolchain": toolchain_digest,
            "caches": cache_digests,
            "egress_hosts": hosts,
        })),
    );
    Ok(Enforcement::Contained(Box::new(ContainedVerification {
        profile: sandbox.profile.clone(),
        port: sandbox.port.clone(),
        toolchain: sandbox.toolchain.clone(),
        caches: sandbox.caches.clone(),
        egress_hosts: hosts.clone(),
        egress_dir: sandbox.egress_dir.clone(),
        relay_exe: sandbox.relay_exe.clone(),
        identity: serde_json::json!({
            "kind": "contained",
            "profile": sandbox.profile.id,
            "boundary": boundary_digest,
            "egress_hosts": hosts,
        }),
        boundary_digest,
    })))
}

/// The §14.5 setup preflight: a trivial command inside the enforced
/// profile with the toolchain and caches mounted — proves the mounts
/// exist and are read-only before the declared argv runs. `Ok(())`
/// only on a clean exit; every other outcome is a typed refusal.
fn prove_verification_mounts(
    ctx: &ContainedVerification,
    primitive: &Path,
    env: &WorkerEnv,
) -> Result<(), String> {
    let mut checks = Vec::new();
    if ctx.toolchain.is_some() {
        checks.push("test -d /taisan/toolchain".to_string());
    }
    // Cache paths reach the probe through the environment, never
    // spliced into the shell text — a path containing a quote or
    // space cannot rewrite the check.
    let mut probe_env = env.clone();
    for (i, cache) in ctx.caches.iter().enumerate() {
        let var = format!("TISAAN_VERIFY_CACHE_{i}");
        probe_env
            .vars
            .push((var.clone(), cache.to_string_lossy().into_owned()));
        checks.push(format!("test -d \"${var}\" && ! test -w \"${var}\""));
    }
    if checks.is_empty() {
        return Ok(());
    }
    let plan = MountPlan {
        workspace: None,
        request: None,
        input: None,
        out: None,
        gitconfig: None,
        runtime_auth: None,
        provider_config: None,
        proxy_sock: None,
        toolchain: ctx.toolchain.clone(),
        caches: ctx.caches.clone(),
        extra_binds: vec![],
        extra_rw_binds: vec![],
        worker: None,
    };
    let argv = vec!["/bin/sh".to_string(), "-c".to_string(), checks.join(" && ")];
    if let Some(reason) = containment::plan_denial(&plan) {
        return Err(format!("verification.toolchain_preflight_failed: {reason}"));
    }
    let outer = containment::bwrap_args(primitive, &plan, &argv, "/tmp");
    match ctx.port.run_probe(
        &outer,
        Path::new("/"),
        &probe_env,
        containment::PROBE_TIMEOUT,
    ) {
        Ok(workloop_core::ports::ProbeOutcome::Exited { success: true, .. }) => Ok(()),
        Ok(workloop_core::ports::ProbeOutcome::Exited { .. }) => {
            Err("verification.toolchain_preflight_failed: the mounted toolchain/cache check did not pass inside the profile".into())
        }
        Ok(workloop_core::ports::ProbeOutcome::Bounded) => Err(
            "verification.toolchain_preflight_failed: the mounted toolchain/cache check did not answer within the bound".into(),
        ),
        Err(workloop_core::ports::SpawnError::Refused { reason }) => Err(format!(
            "verification.toolchain_preflight_failed: spawn refused: {reason}"
        )),
        Err(workloop_core::ports::SpawnError::Unknown { reason }) => Err(format!(
            "verification.toolchain_preflight_failed: spawn unknown: {reason}"
        )),
    }
}

/// Run one declared argv inside the §14.5 sandbox: the containment
/// gate proves the negative-probe battery for this exact argv and
/// boundary identity, the toolchain/cache mounts are preflighted,
/// egress is the separate declared-host CONNECT proxy (never the
/// Worker model-API proxy, never a runtime credential), and the
/// produced tree plus handoff repository stay read-only while the
/// worktree's own gitdir is the single writable metadata bind.
#[allow(clippy::too_many_arguments)]
fn run_verification_contained(
    ctx: &ContainedVerification,
    layout: &HandoffLayout,
    argv: &[String],
    worktree: &Path,
    gitdir: &Path,
    log_path: &Path,
    env_root: &Path,
    scratch_out: &Path,
    tag: &str,
    timeout: Duration,
) -> VerificationRun {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let gate = match containment::gate(
        ctx.port.as_ref(),
        &layout.workspace_root,
        &ctx.profile,
        argv,
        Some(&ctx.boundary_digest),
        now,
    ) {
        Ok(gate) => gate,
        Err(containment::GateError::Refused(reason)) => {
            return VerificationRun::Refused(format!("verification.containment_refused: {reason}"));
        }
        Err(containment::GateError::Fault(detail)) => {
            return VerificationRun::Refused(format!("verification.containment_fault: {detail}"));
        }
    };
    let Gate::Ready { primitive, worker } = gate else {
        return VerificationRun::Refused(
            "verification.profile_uncontained: the isolation profile declares no sandbox primitive"
                .into(),
        );
    };
    let env = match isolation::prepare(&ctx.profile, env_root) {
        Ok(env) => env,
        Err(why) => {
            return VerificationRun::Refused(format!(
                "isolation.environment_preparation_failed: {why}"
            ));
        }
    };
    let contained = containment::contained_env(&env, None);
    if let Err(reason) = prove_verification_mounts(ctx, &primitive, &contained) {
        return VerificationRun::Refused(reason);
    }
    // The separate declared dependency-host policy: a per-command
    // CONNECT proxy on its own socket and evidence log. Empty hosts
    // start no proxy and leave egress fully denied.
    let proxy = if ctx.egress_hosts.is_empty() {
        None
    } else {
        // The socket name is digest-bounded and the directory
        // sun_path-safe: `tag` carries the artifact id and requirement
        // verbatim and a deep state root must not turn egress into an
        // unavailable-proxy refusal.
        let sock_tag = &sha256_hex("verify-egress", tag.as_bytes())[..16];
        let sock_dir = match crate::egress::socket_dir(&ctx.egress_dir) {
            Ok(dir) => dir,
            Err(e) => {
                return VerificationRun::Refused(format!("verification.egress_unavailable: {e}"));
            }
        };
        let sock = sock_dir.join(format!("verify-{sock_tag}.sock"));
        let evidence = ctx
            .egress_dir
            .join(format!("verify-{sock_tag}-egress.jsonl"));
        match ConnectProxy::start(&sock, ctx.egress_hosts.clone(), &evidence) {
            Ok(proxy) => Some(proxy),
            Err(e) => {
                return VerificationRun::Refused(format!("verification.egress_unavailable: {e}"));
            }
        }
    };
    let inner = argv.to_vec();
    let (exec_argv, proxy_url) = if proxy.is_some() {
        (
            containment::relay_wrapped(&inner, &ctx.relay_exe),
            Some(crate::egress::RELAY_PROXY_URL),
        )
    } else {
        (inner, None)
    };
    // The worktree binds read-only at its own path so the `.git`
    // pointer and the gitdir back-link resolve unchanged; the bare
    // repository binds read-only for objects and refs; the worktree's
    // own gitdir is the single writable metadata bind so `git status`
    // can refresh its index — it can never reach another artifact's
    // refs or objects.
    let mut extra_binds = vec![worktree.to_path_buf(), layout.repository.clone()];
    extra_binds.extend(containment::extra_binds(&exec_argv, Some(&worker)));
    if proxy.is_some() {
        let relay = ctx.relay_exe.clone();
        if relay.is_absolute() && relay.exists() && !extra_binds.contains(&relay) {
            extra_binds.push(relay);
        }
    }
    let plan = MountPlan {
        workspace: None,
        request: None,
        input: None,
        out: Some(scratch_out.to_path_buf()),
        gitconfig: Some(env_root.join("gitconfig")),
        runtime_auth: None,
        provider_config: None,
        proxy_sock: proxy.as_ref().map(|p| p.sock_path().to_path_buf()),
        toolchain: ctx.toolchain.clone(),
        caches: ctx.caches.clone(),
        extra_binds,
        extra_rw_binds: vec![gitdir.to_path_buf()],
        worker: Some(worker.clone()),
    };
    let _ = std::fs::create_dir_all(scratch_out);
    if let Some(reason) = containment::plan_denial(&plan) {
        return VerificationRun::Refused(format!("verification.containment_refused: {reason}"));
    }
    let inner_cwd = worktree.to_string_lossy().into_owned();
    let outer = containment::bwrap_args(&primitive, &plan, &exec_argv, &inner_cwd);
    let env = containment::contained_env(&env, proxy_url);
    let run = run_verification_command(&outer, worktree, log_path, &env, timeout);
    if let Some(proxy) = proxy {
        proxy.shutdown();
    }
    run
}

/// Run one declared verification argv on an independent clean worktree
/// of the exact produced commit and record the result. Runs only on an
/// acknowledged handoff — never on a claim or an unverified tree.
/// `declared_egress_hosts` is the request's immutable
/// `verification_egress_hosts` — the Work-declared half of the §14.5
/// effective egress set.
pub fn verify(
    store: &impl Store,
    layout: &HandoffLayout,
    artifact_id: &str,
    requirement: &str,
    argv: &[String],
    scratch: &Path,
    declared_egress_hosts: &[String],
) -> Result<VerificationResult, HandoffError> {
    verify_with_timeout(
        store,
        layout,
        artifact_id,
        requirement,
        argv,
        scratch,
        VERIFICATION_COMMAND_TIMEOUT,
        declared_egress_hosts,
    )
}

#[doc(hidden)]
#[allow(clippy::too_many_arguments)]
pub fn verify_with_timeout(
    store: &impl Store,
    layout: &HandoffLayout,
    artifact_id: &str,
    requirement: &str,
    argv: &[String],
    scratch: &Path,
    timeout: Duration,
    declared_egress_hosts: &[String],
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

    // The §14.5 enforcement resolves before the worktree: a missing
    // toolchain/cache or a denied egress host records the same bounded
    // Inconclusive evidence a failed command would — never an
    // open-network fallback.
    let enforcement = prepare_enforcement(layout, declared_egress_hosts);
    let environment_digest = enforcement.as_ref().ok().map(|e| match e {
        Enforcement::Hygiene => sha256_hex(
            "verification-env-v1",
            &canonical_json(&serde_json::json!({
                "argv": argv,
                "produced": produced,
                "ref": reference,
                "enforcement": {"kind": "hygiene-only"},
            })),
        ),
        Enforcement::Contained(ctx) => sha256_hex(
            "verification-env-v1",
            &canonical_json(&serde_json::json!({
                "argv": argv,
                "produced": produced,
                "ref": reference,
                "enforcement": ctx.identity,
            })),
        ),
    });
    let environment_digest = environment_digest.unwrap_or_else(|| {
        // An enforcement refusal still records an honest digest over
        // what was attempted — the refusal reason is the evidence.
        sha256_hex(
            "verification-env-v1",
            &canonical_json(&serde_json::json!({
                "argv": argv,
                "produced": produced,
                "ref": reference,
                "enforcement": {"kind": "refused"},
            })),
        )
    });

    // Independent worktree of the exact commit; nothing inherits from
    // another command's checkout.
    let tag = format!(
        "{}-{}",
        artifact_id.replace('/', "-"),
        requirement.replace(['/', '[', ']'], "-")
    );
    let worktree = scratch.join(format!("verify-{tag}"));
    let log_path = scratch.join(format!("verify-out-{tag}.log"));
    let _ = std::fs::create_dir_all(scratch);
    // A crashed earlier run can leave a registered worktree whose
    // directory was swept: prune stale registrations, clear the path,
    // then add fresh — `worktree add` refuses a registered path.
    let _ = git(&layout.repository, &["worktree", "prune"]);
    let _ = std::fs::remove_dir_all(&worktree);
    let (result, output) = match (
        &enforcement,
        git(
            &layout.repository,
            &[
                "worktree",
                "add",
                "--detach",
                worktree.to_str().unwrap_or_default(),
                &produced,
            ],
        ),
    ) {
        (Err(reason), _) => (VerificationOutcome::Inconclusive, reason.clone()),
        (Ok(_), Err(e)) => (VerificationOutcome::Inconclusive, format!("worktree: {e}")),
        (Ok(enforcement), Ok(_)) => {
            let env_root = scratch.join(format!("verify-env-{tag}"));
            // The worktree's own gitdir is the single writable
            // metadata bind: `git status` can refresh its index
            // without ever reaching another artifact's refs.
            let gitdir = git(&worktree, &["rev-parse", "--absolute-git-dir"])
                .map(PathBuf::from)
                .unwrap_or_else(|_| {
                    layout
                        .repository
                        .join("worktrees")
                        .join(format!("verify-{tag}"))
                });
            let run = match enforcement {
                Enforcement::Hygiene => {
                    // The recorded v1 hygiene-only environment:
                    // allowlisted env, fresh HOME/TMPDIR, git config
                    // scrubbed of credential helpers — required, not
                    // sufficient; the namespace slice is the
                    // contained path below.
                    let _ = std::fs::create_dir_all(env_root.join("home"));
                    let _ = std::fs::create_dir_all(env_root.join("tmp"));
                    let env = WorkerEnv {
                        vars: vec![
                            (
                                "PATH".to_string(),
                                std::env::var("PATH").unwrap_or_default(),
                            ),
                            (
                                "HOME".to_string(),
                                env_root.join("home").to_string_lossy().into_owned(),
                            ),
                            (
                                "TMPDIR".to_string(),
                                env_root.join("tmp").to_string_lossy().into_owned(),
                            ),
                            ("GIT_CONFIG_NOSYSTEM".to_string(), "1".to_string()),
                            ("GIT_CONFIG_GLOBAL".to_string(), "/dev/null".to_string()),
                        ],
                    };
                    run_verification_command(argv, &worktree, &log_path, &env, timeout)
                }
                Enforcement::Contained(ctx) => run_verification_contained(
                    ctx,
                    layout,
                    argv,
                    &worktree,
                    &gitdir,
                    &log_path,
                    &env_root,
                    &scratch.join(format!("verify-scratch-{tag}")),
                    &tag,
                    timeout,
                ),
            };
            // Read back only the bounded tail — never the whole file:
            // a large writer must not size the supervisor's memory.
            const TAIL: u64 = 64 * 1024;
            let captured = match std::fs::File::open(&log_path) {
                Ok(f) => {
                    use std::io::{Read, Seek, SeekFrom};
                    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
                    let mut r = std::io::BufReader::new(f);
                    let _ = r.seek(SeekFrom::Start(len.saturating_sub(TAIL)));
                    let mut bytes = Vec::new();
                    let _ = r.take(TAIL).read_to_end(&mut bytes);
                    String::from_utf8_lossy(&bytes).into_owned()
                }
                Err(_) => String::new(),
            };
            match run {
                VerificationRun::Finished(status) => (
                    match status.code() {
                        Some(0) => VerificationOutcome::Pass,
                        Some(_) => VerificationOutcome::Fail,
                        // A signal-killed runner is incomplete, never a
                        // fabricated exit code.
                        None => VerificationOutcome::Inconclusive,
                    },
                    captured,
                ),
                VerificationRun::TimedOut => (
                    VerificationOutcome::Inconclusive,
                    format!("runner exceeded {}s\n{captured}", timeout.as_secs()),
                ),
                VerificationRun::SpawnFailed(e) => {
                    (VerificationOutcome::Inconclusive, format!("spawn: {e}"))
                }
                VerificationRun::Refused(reason) => (VerificationOutcome::Inconclusive, reason),
            }
        }
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
