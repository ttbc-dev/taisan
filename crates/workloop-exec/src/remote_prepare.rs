//! Executor-side prepare validation and managed workspace rebuild
//! (contract §6, #376): a finalized input transfer becomes the remote
//! Invocation's materialized input only after every check the
//! coordinator would have run passes again here — manifest bounds
//! against the request's immutable limits, the durable request body's
//! identity against the bound record, each input body against its
//! wire ref, and the source bundle's object closure against the
//! request's pinned commits. The managed `ws-<work_id>` workspace is
//! rebuilt from the transferred objects alone — the executor never
//! fetches a repository host and never resolves a wire-supplied path.
//!
//! A replayed `prepare` re-validates and returns the same result: the
//! workspace receipt binds `request_digest` + head, an existing tree
//! under the same receipt is verified and reused, and a tree under a
//! different request is rebuilt — it is managed, never preserved.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use workloop_core::executor::{
    ExecutorIdentity, ExecutorLimits, ExecutorTransferV1, RemoteHandoffV1, TransferItemV1,
};
use workloop_core::worker_v2::{
    FailedEvidenceBody, FixOrigin, InvariantSetBody, IssueSnapshotBody, ReviewSourceBody, TaskBody,
    WorkerRequestV2, goal_body_digest,
};

use crate::bodies::content_id;
use crate::remote_export::{EXPORT_REF_PREFIX, REQUEST_ITEM, SOURCE_ITEM};

/// The per-Invocation io tree root (`input/`, `out/`, `request.json`).
#[derive(Debug)]
pub struct PreparedRemote {
    /// The validated wire request — the same `WorkerRequestV2` the
    /// local path materializes, never a remote dialect.
    pub request: WorkerRequestV2,
    /// `ws-<work_id>`: the managed workspace rebuilt from the
    /// transferred object closure.
    pub workspace: PathBuf,
    /// `work/<work_id>`: the branch the workspace HEAD pins.
    pub branch: String,
    /// The commit HEAD checked out — the request's head pin.
    pub head: String,
    /// `<io_root>/<invocation_id>` with `input/` (read-only bodies),
    /// `out/` (writable), and `request.json` (read-only, exact bytes).
    pub io_dir: PathBuf,
    pub input_dir: PathBuf,
    pub out_dir: PathBuf,
    pub request_path: PathBuf,
}

/// Why prepare refused. `Foreign`/`Invalid`/`DigestMismatch` are
/// contract-visible refusal classes (`request_identity_mismatch`,
/// `input_invalid`); `Fault` is local io/git infrastructure.
#[derive(Debug)]
pub enum PrepareError {
    /// The request body names a different request, invocation, or
    /// digest than the bound record — `request_identity_mismatch`.
    Foreign { detail: String },
    /// A body, closure, path, or limit failed validation —
    /// `input_invalid` / `limit_exceeded`.
    Invalid { detail: String },
    /// A transferred item failed its declared digest or byte count.
    DigestMismatch { detail: String },
    /// The transfer did not finalize — a `pending` outcome, never a
    /// partial prepare.
    Incomplete { detail: String },
    /// A filesystem or git fault.
    Fault(String),
}

impl std::fmt::Display for PrepareError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Foreign { detail } => write!(f, "request_identity_mismatch: {detail}"),
            Self::Invalid { detail } => write!(f, "input_invalid: {detail}"),
            Self::DigestMismatch { detail } => write!(f, "digest mismatch: {detail}"),
            Self::Incomplete { detail } => write!(f, "incomplete transfer: {detail}"),
            Self::Fault(d) => write!(f, "prepare fault: {d}"),
        }
    }
}

impl std::error::Error for PrepareError {}

impl From<std::io::Error> for PrepareError {
    fn from(e: std::io::Error) -> Self {
        Self::Fault(e.to_string())
    }
}

fn fault(detail: impl Into<String>) -> PrepareError {
    PrepareError::Fault(detail.into())
}

fn invalid(detail: impl Into<String>) -> PrepareError {
    PrepareError::Invalid {
        detail: detail.into(),
    }
}

fn fsync_dir(path: &Path) -> Result<(), PrepareError> {
    std::fs::File::open(path)
        .and_then(|f| f.sync_all())
        .map_err(|e| fault(format!("sync dir {}: {e}", path.display())))
}

/// Executor-side roots for one remote Invocation.
#[derive(Debug, Clone)]
pub struct RemotePrepareLayout {
    /// `ws-<work_id>` managed workspaces live under here.
    pub workspace_root: PathBuf,
    /// `<invocation_id>` io trees live under here.
    pub io_root: PathBuf,
    /// Bounded scratch for the workspace staging build.
    pub scratch: PathBuf,
}

fn git(dir: &Path, args: &[&str]) -> Result<String, PrepareError> {
    let output = Command::new("git")
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
        .output()
        .map_err(|e| fault(format!("spawn git: {e}")))?;
    if !output.status.success() {
        return Err(fault(format!(
            "git {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// The head ref the bundle must pin for this request — the commit the
/// managed workspace checks out (contract: the bundle's recorded tips
/// are re-verified, never the request's claim alone).
fn expected_head_ref(request: &WorkerRequestV2) -> &'static str {
    match &request.task {
        TaskBody::Coding { .. } => "base",
        TaskBody::Fix { .. } => "predecessor",
        TaskBody::InvariantCheck { .. } => "artifact",
        TaskBody::CodeReview { .. } => "review-head",
    }
}

/// The commit the workspace checks out — the branch's recorded
/// start, which collection also reads for its `nothing_produced`
/// verdict (a Fix starts at `predecessor.produced`, not `base`).
pub(crate) fn expected_head(request: &WorkerRequestV2) -> String {
    match &request.task {
        TaskBody::Coding { .. } => request.base.object.clone(),
        TaskBody::Fix { predecessor, .. } => predecessor.produced.clone(),
        TaskBody::InvariantCheck { artifact, .. } => artifact.produced.clone(),
        TaskBody::CodeReview { target, .. } => target.head.clone(),
    }
}

/// One input item's wire-ref check — the executor re-verifies every
/// digest binding the request records, not just the transport
/// digests (contract §6: object/head/digest validation before the
/// workspace exists).
fn verify_input(request: &WorkerRequestV2, name: &str, bytes: &[u8]) -> Result<(), PrepareError> {
    let check = |detail: String| invalid(detail);
    match &request.task {
        TaskBody::Coding { goal, issues, .. } | TaskBody::Fix { goal, issues, .. } => {
            if name == "goal.md" {
                if bytes.len() as u64 != goal.bytes || goal_body_digest(bytes) != goal.digest {
                    return Err(check("goal body fails its request ref".into()));
                }
                return Ok(());
            }
            if let Some(index) = name
                .strip_prefix("issue-")
                .and_then(|rest| rest.strip_suffix(".json"))
                .and_then(|n| n.parse::<usize>().ok())
            {
                let Some(entry) = issues.get(index) else {
                    return Err(check(format!("input {name:?} names no declared issue")));
                };
                if bytes.len() as u64 != entry.body.bytes {
                    return Err(check(format!("input {name:?} byte count disagrees")));
                }
                let snapshot = IssueSnapshotBody::parse(bytes)
                    .map_err(|e| check(format!("issue snapshot malformed: {e}")))?;
                if snapshot.wire_digest() != entry.body.digest {
                    return Err(check(format!("input {name:?} digest disagrees")));
                }
                return Ok(());
            }
            if let TaskBody::Fix { origin, .. } = &request.task
                && let FixOrigin::Prepublication { failed_evidence } = origin.as_ref()
                && let Some(index) = name
                    .strip_prefix("evidence-")
                    .and_then(|rest| rest.strip_suffix(".json"))
                    .and_then(|n| n.parse::<usize>().ok())
            {
                let Some(ev) = failed_evidence.get(index) else {
                    return Err(check(format!("input {name:?} names no declared evidence")));
                };
                let body = FailedEvidenceBody::parse(bytes)
                    .map_err(|e| check(format!("failed evidence malformed: {e}")))?;
                if body.wire_digest() != ev.digest
                    || body.evidence_id != ev.evidence_id
                    || body.kind != ev.kind
                {
                    return Err(check(format!("input {name:?} digest disagrees")));
                }
                return Ok(());
            }
            Err(check(format!(
                "input {name:?} is not declared by the request"
            )))
        }
        TaskBody::InvariantCheck { invariant_set, .. } => {
            if name != "invariant-set.json" {
                return Err(check(format!(
                    "input {name:?} is not declared by the request"
                )));
            }
            let set = InvariantSetBody::parse(bytes)
                .map_err(|e| check(format!("invariant set malformed: {e}")))?;
            if set.wire_digest() != invariant_set.digest
                || set.id != invariant_set.id
                || set.revision != invariant_set.revision
            {
                return Err(check("invariant set disagrees with the request ref".into()));
            }
            Ok(())
        }
        TaskBody::CodeReview { review_source, .. } => {
            if name != "review-source.json" {
                return Err(check(format!(
                    "input {name:?} is not declared by the request"
                )));
            }
            if bytes.len() as u64 != review_source.bytes {
                return Err(check("review source byte count disagrees".into()));
            }
            let source = ReviewSourceBody::parse(bytes)
                .map_err(|e| check(format!("review source malformed: {e}")))?;
            if source.wire_digest() != review_source.digest {
                return Err(check("review source digest disagrees".into()));
            }
            if content_id(bytes) != review_source.source_revision {
                return Err(check("review source revision disagrees".into()));
            }
            Ok(())
        }
    }
}

/// The materialized input names the request's refs require — the
/// same set `remote_export::required_inputs` ships: the goal, each
/// grouped issue's snapshot, the prepublication failed-evidence set,
/// the invariant set, or the review source. `verify_input` refuses
/// *undeclared* names; this set is what makes the required side
/// checked too — a finalized transfer missing a required body never
/// prepares (contract §6).
fn required_input_names(request: &WorkerRequestV2) -> Vec<String> {
    let mut names = Vec::new();
    match &request.task {
        TaskBody::Coding { issues, .. } | TaskBody::Fix { issues, .. } => {
            names.push("goal.md".to_string());
            names.extend((0..issues.len()).map(|i| format!("issue-{i}.json")));
            if let TaskBody::Fix { origin, .. } = &request.task
                && let FixOrigin::Prepublication { failed_evidence } = origin.as_ref()
            {
                names.extend((0..failed_evidence.len()).map(|i| format!("evidence-{i}.json")));
            }
        }
        TaskBody::InvariantCheck { .. } => names.push("invariant-set.json".into()),
        TaskBody::CodeReview { .. } => names.push("review-source.json".into()),
    }
    names
}

/// One finalized item's whole body, re-verified against the manifest
/// before it is ever materialized — the transport's digest check at
/// finalize is replayed here so a swapped file can never reach the
/// workspace.
fn verified_item(dir: &Path, item: &TransferItemV1) -> Result<Vec<u8>, PrepareError> {
    let path = crate::remote_transfer::item_path(dir, &item.item_id);
    let meta = std::fs::symlink_metadata(&path).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            PrepareError::Incomplete {
                detail: format!("item {:?} is absent", item.item_id),
            }
        } else {
            fault(format!("stat {}: {e}", path.display()))
        }
    })?;
    if meta.file_type().is_symlink() || !meta.is_file() {
        return Err(invalid(format!(
            "transferred item {:?} is not a regular file",
            item.item_id
        )));
    }
    let bytes = std::fs::read(&path).map_err(|e| fault(format!("read item: {e}")))?;
    if bytes.len() as u64 != item.bytes
        || workloop_core::executor::item_content_digest(&bytes) != item.digest
    {
        return Err(PrepareError::DigestMismatch {
            detail: format!("item {:?} fails its declared digest", item.item_id),
        });
    }
    Ok(bytes)
}

/// Write `bytes` read-only at `dest` — the same verify-after-write
/// and 0444 mode the local materialization applies, so the Worker
/// sees immutable input it cannot rewrite.
fn write_readonly(dest: &Path, bytes: &[u8]) -> Result<(), PrepareError> {
    let tmp = dest.with_extension("tmp");
    std::fs::write(&tmp, bytes).map_err(|e| fault(format!("write {}: {e}", tmp.display())))?;
    let written = std::fs::read(&tmp).map_err(|e| fault(format!("read {}: {e}", tmp.display())))?;
    if written != bytes {
        let _ = std::fs::remove_file(&tmp);
        return Err(fault(format!(
            "{} write verification failed",
            dest.display()
        )));
    }
    std::fs::File::open(&tmp)
        .and_then(|f| f.sync_all())
        .map_err(|e| fault(format!("sync {}: {e}", tmp.display())))?;
    std::fs::rename(&tmp, dest).map_err(|e| fault(format!("rename {}: {e}", tmp.display())))?;
    let mut perms = std::fs::metadata(dest)
        .map_err(|e| fault(format!("stat {}: {e}", dest.display())))?
        .permissions();
    perms.set_mode(0o444);
    std::fs::set_permissions(dest, perms)
        .map_err(|e| fault(format!("chmod {}: {e}", dest.display())))?;
    Ok(())
}

/// The prepare receipt: the durable record one rebuilt workspace
/// answers to — a replayed prepare validates against it rather than
/// trusting the tree's existence.
#[derive(serde::Serialize, serde::Deserialize)]
struct PrepareReceipt {
    request_digest: String,
    work_id: String,
    head: String,
    branch: String,
}

/// Build the managed workspace from the transferred bundle. Mirrors
/// `workspace::build_staging`: init, fetch the bundle's export refs,
/// pin `taisan-base` and checkout `work/<work_id>` at head, verify
/// HEAD, cleanliness, no alternates, no remotes, and a safe `.git`.
fn build_workspace(
    staging: &Path,
    bundle: &Path,
    base: &str,
    head: &str,
    head_ref: &str,
    branch: &str,
) -> Result<(), PrepareError> {
    git(staging, &["init"])?;
    let src = bundle.to_string_lossy().into_owned();
    git(
        staging,
        &[
            "fetch",
            "--no-tags",
            "--no-write-fetch-head",
            "--",
            &src,
            &format!("+{EXPORT_REF_PREFIX}base:refs/heads/taisan-base"),
        ],
    )
    .map_err(|_| invalid("base objects could not be imported from the bundle"))?;
    if git(staging, &["cat-file", "-t", base])
        .map_err(|_| invalid("request base is not present in the bundle"))?
        != "commit"
    {
        return Err(invalid("request base is not a commit"));
    }
    if head != base {
        git(
            staging,
            &[
                "fetch",
                "--no-tags",
                "--no-write-fetch-head",
                "--",
                &src,
                &format!("+{EXPORT_REF_PREFIX}{head_ref}:refs/heads/taisan-head"),
            ],
        )
        .map_err(|_| invalid("head objects could not be imported from the bundle"))?;
        if git(staging, &["cat-file", "-t", head])
            .map_err(|_| invalid("request head is not present in the bundle"))?
            != "commit"
        {
            return Err(invalid("request head is not a commit"));
        }
    }
    git(staging, &["checkout", "-b", branch, head])?;
    if git(staging, &["rev-parse", "--verify", "HEAD"])? != head {
        return Err(fault("checkout did not land on the pinned head"));
    }
    if !git(staging, &["status", "--porcelain"])?.is_empty() {
        return Err(fault("workspace is not clean after checkout"));
    }
    if staging.join(".git/objects/info/alternates").exists() {
        return Err(invalid("workspace carries object alternates"));
    }
    if !git(staging, &["remote"])?.is_empty() {
        return Err(invalid("workspace carries remotes"));
    }
    Ok(())
}

/// Validate a finalized input transfer and rebuild the managed
/// workspace + io tree.
///
/// `transfer_dir` is the receiver's `complete/<transfer_id>` — the
/// caller finalizes first; prepare never reads staging. `identity`
/// is the bound request record the request body must name.
pub fn prepare_remote(
    layout: &RemotePrepareLayout,
    transfer_dir: &Path,
    manifest: &ExecutorTransferV1,
    limits: &ExecutorLimits,
    identity: &ExecutorIdentity,
) -> Result<PreparedRemote, PrepareError> {
    // The request's immutable limits gate the declared set before any
    // body is touched (contract §8: the receiver enforces them before
    // accepting each chunk; the totals are re-checked at prepare).
    if manifest.total_bytes > limits.max_input_bytes {
        return Err(PrepareError::Invalid {
            detail: format!(
                "transfer total {} exceeds the input bound {}",
                manifest.total_bytes, limits.max_input_bytes
            ),
        });
    }
    if manifest.items.len() as u64 > limits.max_item_count {
        return Err(PrepareError::Invalid {
            detail: format!(
                "{} items exceed the bound {}",
                manifest.items.len(),
                limits.max_item_count
            ),
        });
    }
    if manifest.collection_id != identity.request_id {
        return Err(PrepareError::Foreign {
            detail: format!(
                "transfer collection {:?} is not the bound request",
                manifest.collection_id
            ),
        });
    }

    // The request body first — everything else validates against it.
    let request_item = manifest
        .item(REQUEST_ITEM)
        .ok_or_else(|| invalid("transfer declares no request body"))?;
    let request_bytes = verified_item(transfer_dir, request_item)?;
    let request = WorkerRequestV2::parse(&request_bytes)
        .map_err(|e| invalid(format!("request body does not parse: {e}")))?;
    if request.request_id != identity.request_id
        || request.invocation_id != identity.invocation_id
        || request.request_digest != identity.request_digest
    {
        return Err(PrepareError::Foreign {
            detail: "request body names a different request, invocation, or digest".into(),
        });
    }
    if request.base.repository != request.repository {
        return Err(invalid("request base names a foreign repository"));
    }
    let branch = format!("work/{}", request.work_id);
    if request.workspace.workspace_id != format!("ws-{}", request.work_id) {
        return Err(invalid("wire workspace_id disagrees with the managed id"));
    }
    let head = expected_head(&request);
    let head_ref = expected_head_ref(&request);

    // Every body the request's refs require must be declared in the
    // transfer — the per-item loop refuses undeclared names, and this
    // set equality refuses a finalized transfer that simply omits a
    // required input.
    for name in required_input_names(&request) {
        let declared = manifest
            .items
            .iter()
            .any(|item| item.path == format!("input/{name}"));
        if !declared {
            return Err(invalid(format!(
                "required input {name:?} is not declared by the transfer"
            )));
        }
    }

    // The object closure: the bundle verifies, names the export refs
    // the export pinned, and its recorded tips equal the request's —
    // a bundle for different commits is refused, never imported.
    let source_item = manifest
        .item(SOURCE_ITEM)
        .ok_or_else(|| invalid("transfer declares no source closure"))?;
    let bundle_bytes = verified_item(transfer_dir, source_item)?;
    let bundle_path = layout.scratch.join("source.bundle");
    std::fs::create_dir_all(&layout.scratch).map_err(|e| fault(format!("create scratch: {e}")))?;
    std::fs::write(&bundle_path, &bundle_bytes).map_err(|e| fault(format!("write bundle: {e}")))?;
    // `git bundle verify` runs inside a throwaway repo dir — the
    // command needs a repository context even though it only reads
    // the bundle.
    let probe = layout.scratch.join("probe");
    if !probe.join(".git").is_dir() {
        std::fs::create_dir_all(&probe).map_err(|e| fault(format!("create probe: {e}")))?;
        git(&probe, &["init"])?;
    }
    git(
        &probe,
        &["bundle", "verify", &bundle_path.to_string_lossy()],
    )
    .map_err(|_| invalid("source bundle fails verification"))?;
    let heads = git(
        &probe,
        &["bundle", "list-heads", &bundle_path.to_string_lossy()],
    )?;
    let expect = format!("{} {EXPORT_REF_PREFIX}base", request.base.object);
    if !heads.lines().any(|l| l.trim() == expect) {
        return Err(PrepareError::DigestMismatch {
            detail: "bundle does not pin the request base".into(),
        });
    }
    if head != request.base.object {
        let expect = format!("{head} {EXPORT_REF_PREFIX}{head_ref}");
        if !heads.lines().any(|l| l.trim() == expect) {
            return Err(PrepareError::DigestMismatch {
                detail: "bundle does not pin the request head".into(),
            });
        }
    }

    // Managed workspace: rebuild under scratch, then publish by rename.
    let final_ws = layout.workspace_root.join(&request.workspace.workspace_id);
    let receipt_path = layout
        .scratch
        .join(format!("prepared-{}.json", request.work_id));
    let receipt = PrepareReceipt {
        request_digest: request.request_digest.clone(),
        work_id: request.work_id.clone(),
        head: head.clone(),
        branch: branch.clone(),
    };
    let existing = match std::fs::read(&receipt_path) {
        Ok(raw) => serde_json::from_slice::<PrepareReceipt>(&raw).ok(),
        Err(_) => None,
    };
    let reuse = existing.as_ref().is_some_and(|r| {
        r.request_digest == receipt.request_digest
            && r.work_id == receipt.work_id
            && r.head == receipt.head
            && r.branch == receipt.branch
            && final_ws.is_dir()
            && git(&final_ws, &["rev-parse", "--verify", "HEAD"])
                .map(|h| h == head)
                .unwrap_or(false)
    });
    if !reuse {
        if final_ws.exists() {
            std::fs::remove_dir_all(&final_ws)
                .map_err(|e| fault(format!("clear managed workspace: {e}")))?;
        }
        let staging = layout.scratch.join("workspace");
        if staging.exists() {
            std::fs::remove_dir_all(&staging)
                .map_err(|e| fault(format!("clear workspace staging: {e}")))?;
        }
        std::fs::create_dir_all(&staging)
            .map_err(|e| fault(format!("create workspace staging: {e}")))?;
        if let Err(e) = build_workspace(
            &staging,
            &bundle_path,
            &request.base.object,
            &head,
            head_ref,
            &branch,
        ) {
            let _ = std::fs::remove_dir_all(&staging);
            return Err(e);
        }
        crate::workspace::verify_git_dir_safe(&staging.join(".git"))
            .map_err(|e| fault(format!("rebuilt workspace .git is not safe: {e}")))?;
        std::fs::create_dir_all(&layout.workspace_root)
            .map_err(|e| fault(format!("create workspace root: {e}")))?;
        std::fs::rename(&staging, &final_ws)
            .map_err(|e| fault(format!("publish workspace: {e}")))?;
        fsync_dir(&layout.workspace_root)?;
        let tmp = receipt_path.with_extension("tmp");
        std::fs::write(
            &tmp,
            serde_json::to_vec(&receipt).expect("receipt serializes"),
        )
        .map_err(|e| fault(format!("write receipt: {e}")))?;
        std::fs::rename(&tmp, &receipt_path).map_err(|e| fault(format!("rename receipt: {e}")))?;
        fsync_dir(&layout.scratch)?;
    }

    // The io tree: request.json read-only, input bodies materialized
    // read-only under their declared names, out/ fresh and writable.
    // A leftover tree from an interrupted earlier attempt is rebuilt.
    let io_dir = layout
        .io_root
        .join(crate::isolation::sanitize_key(&request.invocation_id));
    if io_dir.exists() {
        std::fs::remove_dir_all(&io_dir).map_err(|e| fault(format!("clear io dir: {e}")))?;
    }
    let input_dir = io_dir.join("input");
    let out_dir = io_dir.join("out");
    std::fs::create_dir_all(&input_dir).map_err(|e| fault(format!("create input: {e}")))?;
    std::fs::create_dir_all(&out_dir).map_err(|e| fault(format!("create out: {e}")))?;
    let request_path = io_dir.join("request.json");
    write_readonly(&request_path, &request_bytes)?;
    for item in &manifest.items {
        let Some(name) = item.path.strip_prefix("input/") else {
            continue;
        };
        if !RemoteHandoffV1::path_is_valid(name) || name.contains('/') {
            return Err(invalid(format!(
                "input item path {:?} is not safe",
                item.path
            )));
        }
        let bytes = verified_item(transfer_dir, item)?;
        verify_input(&request, name, &bytes)?;
        write_readonly(&input_dir.join(name), &bytes)?;
    }
    fsync_dir(&input_dir)?;
    fsync_dir(&io_dir)?;

    Ok(PreparedRemote {
        request,
        workspace: final_ws,
        branch,
        head,
        io_dir,
        input_dir,
        out_dir,
        request_path,
    })
}
