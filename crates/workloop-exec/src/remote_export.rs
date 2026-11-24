//! Coordinator-side input export (contract §6, #376): the immutable
//! request transfer one bound `request_id` produces for a remote
//! executor — the exact durable request body, the exact retained
//! input bodies the request's refs name, and the approved source
//! object closure as a self-verifying git bundle. Nothing else leaves
//! coordinator-controlled storage: the Worker request never carries a
//! path, credential, or live configuration, and the bundle is built
//! from a throwaway staging repository so the coordinator's own
//! source mirror is never mutated.
//!
//! The export is deterministic for one request: the same durable
//! rows produce the same manifest, so a replayed `prepare`
//! re-declares byte-identically and the receiver resumes rather than
//! restarting.

use std::path::Path;
use std::process::Command;

use workloop_core::executor::{ExecutorTransferV1, TransferItemV1, item_content_digest};
use workloop_core::store::{Store, StoreError};
use workloop_core::worker_v2::{
    DispatchRefusal, FailedEvidenceBody, FixOrigin, InvariantSetBody, IssueSnapshotBody,
    ReviewSourceBody, TaskBody, WorkerRequestV2, goal_body_digest,
};

use crate::bodies::{BodyError, BodyStore, CAS_CATEGORY, content_id};
use crate::handoff;
use crate::workspace::RepositorySource;

/// Item identities inside an input transfer. Paths are the
/// materialized surface the executor rebuilds; item ids are the flat
/// safe-identifier forms of those paths.
pub const REQUEST_ITEM: &str = "request";
/// `request.json` at the transfer root — the exact durable body.
pub const REQUEST_PATH: &str = "request.json";
/// The source object closure — a `git bundle` covering every commit
/// the request pins.
pub const SOURCE_ITEM: &str = "source-bundle";
/// `source.bundle` at the transfer root.
pub const SOURCE_PATH: &str = "source.bundle";
/// The ref prefix the staging repository names exported commits
/// under; the bundle carries these names so the executor can fetch
/// them and re-verify each tip byte-for-byte.
pub const EXPORT_REF_PREFIX: &str = "refs/taisan/export/";

/// One exported item: manifest identity plus its whole body.
#[derive(Debug, Clone)]
pub struct ExportedItem {
    pub item_id: String,
    pub path: String,
    pub body: Vec<u8>,
}

/// The complete input transfer for one request.
#[derive(Debug)]
pub struct ExportedTransfer {
    pub manifest: ExecutorTransferV1,
    pub items: Vec<ExportedItem>,
    /// The request head: the commit the managed workspace checks out
    /// (`work/<work_id>`); for Coding it is the base itself.
    pub head: String,
}

/// Why an export refused. `Refusal` is a typed dispatch refusal the
/// caller records like a local build refusal; `Fault` is
/// infrastructure — the pass fails and may retry.
#[derive(Debug)]
pub enum ExportError {
    Refusal(DispatchRefusal),
    Fault(String),
}

impl std::fmt::Display for ExportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Refusal(r) => write!(f, "{r}"),
            Self::Fault(d) => write!(f, "remote export fault: {d}"),
        }
    }
}

impl std::error::Error for ExportError {}

impl From<DispatchRefusal> for ExportError {
    fn from(r: DispatchRefusal) -> Self {
        Self::Refusal(r)
    }
}

impl From<StoreError> for ExportError {
    fn from(e: StoreError) -> Self {
        Self::Fault(e.to_string())
    }
}

fn fault(detail: impl Into<String>) -> ExportError {
    ExportError::Fault(detail.into())
}

fn invalid(detail: impl Into<String>) -> ExportError {
    ExportError::Refusal(DispatchRefusal::RequestInvalid {
        detail: detail.into(),
    })
}

fn missing(detail: impl Into<String>) -> ExportError {
    ExportError::Refusal(DispatchRefusal::SnapshotMissing {
        detail: detail.into(),
    })
}

/// One input body to export: the materialized file name, the
/// catalogue path it resolves under, the request-recorded byte count
/// when the ref pins one, and the wire-digest check the exact body
/// must pass before it may leave coordinator storage.
type VerifyBody = Box<dyn Fn(&[u8]) -> Result<(), String>>;

struct InputBody {
    name: String,
    path: String,
    expected_bytes: Option<u64>,
    verify: VerifyBody,
}

/// The commits the request's object closure must contain, and where
/// the coordinator reads each from — the local source mirror for
/// request base/review targets, the installation handoff repository
/// for acknowledged produced commits.
struct ClosureCommit {
    /// The staging ref basename (`base`, `head`, `predecessor`, ...).
    name: String,
    sha: String,
    /// `None` reads from the configured source; `Some` reads the
    /// artifact's quarantine-authority repository.
    artifact_id: Option<String>,
}

/// Every input body the request's refs name — the same set
/// [`crate::request_v2::materialize_invocation_io`] materializes for a
/// local Invocation, never a superset.
fn required_inputs(request: &WorkerRequestV2) -> Vec<InputBody> {
    let mut inputs = Vec::new();
    let push_goal_and_issues =
        |inputs: &mut Vec<InputBody>,
         goal: &workloop_core::worker_v2::GoalRef,
         issues: &[workloop_core::worker_v2::GroupedIssue]| {
            let goal_ref = goal.clone();
            inputs.push(InputBody {
                name: "goal.md".into(),
                path: goal.body_ref.clone(),
                expected_bytes: Some(goal.bytes),
                verify: Box::new(move |bytes| {
                    if goal_body_digest(bytes) != goal_ref.digest {
                        return Err("goal body digest mismatch".into());
                    }
                    Ok(())
                }),
            });
            for (index, entry) in issues.iter().enumerate() {
                let issue_ref = entry.body.clone();
                inputs.push(InputBody {
                    name: format!("issue-{index}.json"),
                    path: entry.body.body_ref.clone(),
                    expected_bytes: Some(entry.body.bytes),
                    verify: Box::new(move |bytes| {
                        let snapshot = IssueSnapshotBody::parse(bytes)
                            .map_err(|e| format!("issue snapshot malformed: {e}"))?;
                        if snapshot.wire_digest() != issue_ref.digest {
                            return Err("issue snapshot digest mismatch".into());
                        }
                        Ok(())
                    }),
                });
            }
        };
    match &request.task {
        TaskBody::Coding { goal, issues, .. } => {
            push_goal_and_issues(&mut inputs, goal, issues);
        }
        TaskBody::Fix {
            goal,
            issues,
            origin,
            ..
        } => {
            push_goal_and_issues(&mut inputs, goal, issues);
            if let FixOrigin::Prepublication { failed_evidence } = origin.as_ref() {
                for (index, evidence) in failed_evidence.iter().enumerate() {
                    let ev = evidence.clone();
                    inputs.push(InputBody {
                        name: format!("evidence-{index}.json"),
                        path: format!("bodies/failed-evidence/{}", evidence.digest),
                        expected_bytes: None,
                        verify: Box::new(move |bytes| {
                            let body = FailedEvidenceBody::parse(bytes)
                                .map_err(|e| format!("failed evidence malformed: {e}"))?;
                            if body.wire_digest() != ev.digest
                                || body.evidence_id != ev.evidence_id
                                || body.kind != ev.kind
                            {
                                return Err("failed evidence disagrees with the request ref".into());
                            }
                            Ok(())
                        }),
                    });
                }
            }
        }
        TaskBody::InvariantCheck { invariant_set, .. } => {
            let set_ref = invariant_set.clone();
            inputs.push(InputBody {
                name: "invariant-set.json".into(),
                path: format!("bodies/invariant-set/{}", invariant_set.digest),
                expected_bytes: None,
                verify: Box::new(move |bytes| {
                    let set = InvariantSetBody::parse(bytes)
                        .map_err(|e| format!("invariant set malformed: {e}"))?;
                    if set.wire_digest() != set_ref.digest
                        || set.id != set_ref.id
                        || set.revision != set_ref.revision
                    {
                        return Err("invariant set disagrees with the request ref".into());
                    }
                    Ok(())
                }),
            });
        }
        TaskBody::CodeReview { review_source, .. } => {
            let source_ref = review_source.clone();
            inputs.push(InputBody {
                name: "review-source.json".into(),
                path: format!("bodies/review-source/{}", review_source.digest),
                expected_bytes: Some(review_source.bytes),
                verify: Box::new(move |bytes| {
                    let source = ReviewSourceBody::parse(bytes)
                        .map_err(|e| format!("review source malformed: {e}"))?;
                    if source.wire_digest() != source_ref.digest {
                        return Err("review source digest mismatch".into());
                    }
                    if content_id(bytes) != source_ref.source_revision {
                        return Err("review source revision mismatch".into());
                    }
                    Ok(())
                }),
            });
        }
    }
    inputs
}

/// The commits the object closure must carry, plus the managed
/// workspace head the executor rebuilds at.
fn closure_commits(request: &WorkerRequestV2) -> Result<(Vec<ClosureCommit>, String), ExportError> {
    let mut commits = vec![ClosureCommit {
        name: "base".into(),
        sha: request.base.object.clone(),
        artifact_id: None,
    }];
    let head = match &request.task {
        TaskBody::Coding { .. } => request.base.object.clone(),
        TaskBody::Fix { predecessor, .. } => {
            commits.push(ClosureCommit {
                name: "predecessor".into(),
                sha: predecessor.produced.clone(),
                artifact_id: Some(predecessor.artifact_id.clone()),
            });
            predecessor.produced.clone()
        }
        TaskBody::InvariantCheck { artifact, .. } => {
            commits.push(ClosureCommit {
                name: "artifact".into(),
                sha: artifact.produced.clone(),
                artifact_id: Some(artifact.artifact_id.clone()),
            });
            artifact.produced.clone()
        }
        TaskBody::CodeReview { target, .. } => {
            if target.base != request.base.object {
                commits.push(ClosureCommit {
                    name: "review-base".into(),
                    sha: target.base.clone(),
                    artifact_id: None,
                });
            }
            commits.push(ClosureCommit {
                name: "review-head".into(),
                sha: target.head.clone(),
                artifact_id: None,
            });
            target.head.clone()
        }
    };
    // Dedup by sha — one commit is one ref, never bundled twice.
    let mut seen = std::collections::HashSet::new();
    commits.retain(|c| seen.insert(c.sha.clone()));
    Ok((commits, head))
}

fn git(repo: &Path, args: &[&str]) -> Result<String, ExportError> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .arg("-c")
        .arg("core.hooksPath=/dev/null")
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

/// Build the source-closure bundle in a throwaway staging repository
/// under `scratch` — never in the configured source mirror, which the
/// export only reads. Each named commit is fetched by its object id,
/// pinned under `refs/taisan/export/<name>`, and bundled; the bundle
/// is verified and its recorded tips re-checked before the bytes
/// become a transfer item.
fn build_source_bundle(
    scratch: &Path,
    source: &RepositorySource,
    handoff_repo: Option<&Path>,
    commits: &[ClosureCommit],
) -> Result<Vec<u8>, ExportError> {
    let staging = scratch.join("objects");
    if staging.exists() {
        std::fs::remove_dir_all(&staging).map_err(|e| fault(format!("clear staging: {e}")))?;
    }
    std::fs::create_dir_all(&staging).map_err(|e| fault(format!("create staging: {e}")))?;
    git(&staging, &["init", "--bare"])?;
    for commit in commits {
        let src = match &commit.artifact_id {
            None => source.path.clone(),
            Some(_) => handoff_repo
                .ok_or_else(|| {
                    invalid(
                        "request binds a produced artifact but no handoff repository is configured",
                    )
                })?
                .to_path_buf(),
        };
        let spec = match &commit.artifact_id {
            None => commit.sha.clone(),
            Some(artifact_id) => format!("+{}", handoff::artifact_ref(artifact_id)),
        };
        git(
            &staging,
            &[
                "fetch",
                "--no-tags",
                "--no-write-fetch-head",
                "--",
                &src.to_string_lossy(),
                &spec,
            ],
        )
        .map_err(|_| {
            invalid(format!(
                "object {} is not present in its source",
                commit.sha
            ))
        })?;
        if git(&staging, &["cat-file", "-t", &commit.sha])
            .map_err(|_| invalid(format!("object {} could not be imported", commit.sha)))?
            != "commit"
        {
            return Err(invalid(format!("object {} is not a commit", commit.sha)));
        }
        git(
            &staging,
            &[
                "update-ref",
                &format!("{EXPORT_REF_PREFIX}{}", commit.name),
                &commit.sha,
            ],
        )?;
    }
    let bundle = scratch.join("source.bundle");
    let mut args = vec!["bundle", "create"];
    let bundle_arg = bundle.to_string_lossy().into_owned();
    args.push(&bundle_arg);
    let ref_args: Vec<String> = commits
        .iter()
        .map(|c| format!("{EXPORT_REF_PREFIX}{}", c.name))
        .collect();
    args.extend(ref_args.iter().map(String::as_str));
    git(&staging, &args)?;
    // The bundle must verify and name the exact commits the request
    // pinned — a bundle for different tips is corrupt, not exportable.
    git(&staging, &["bundle", "verify", &bundle_arg])
        .map_err(|_| fault("export bundle failed self-verification"))?;
    let heads = git(&staging, &["bundle", "list-heads", &bundle_arg])?;
    for commit in commits {
        let line = format!("{} {EXPORT_REF_PREFIX}{}", commit.sha, commit.name);
        if !heads.lines().any(|l| l.trim() == line) {
            return Err(fault(format!(
                "export bundle does not pin {} at {}",
                commit.name, commit.sha
            )));
        }
    }
    std::fs::read(&bundle).map_err(|e| fault(format!("read bundle: {e}")))
}

/// Export the immutable input transfer for one bound request.
///
/// `source` is the coordinator's local mirror for the request's
/// repository; `handoff_repo` is the installation's artifact
/// repository a Fix/InvariantCheck predecessor resolves through —
/// required exactly when the request binds one. `scratch` is a
/// bounded export staging area that may be discarded after the
/// transfer completes.
#[allow(clippy::too_many_arguments)]
pub fn export_request_transfer(
    store: &impl Store,
    bodies: &BodyStore,
    source: &RepositorySource,
    handoff_repo: Option<&Path>,
    scratch: &Path,
    request_id: &str,
    transfer_id: &str,
    chunk_size: u64,
) -> Result<ExportedTransfer, ExportError> {
    let body = store
        .transact(|tx| tx.worker_request_body(request_id))?
        .ok_or_else(|| invalid("no durable request body for remote export"))?;
    if !body.verifies() {
        return Err(ExportError::Refusal(DispatchRefusal::BodyCorrupt {
            detail: "durable request body fails verification".into(),
        }));
    }
    let request = WorkerRequestV2::parse(&body.canonical_body).map_err(|e| {
        ExportError::Refusal(DispatchRefusal::BodyCorrupt {
            detail: format!("durable request body does not parse: {e}"),
        })
    })?;
    if request.request_id != request_id {
        return Err(invalid("durable request body names a different request"));
    }
    if request.base.repository != request.repository {
        return Err(invalid("request base names a foreign repository"));
    }
    if source.repository.host.as_str() != request.repository.host
        || source.repository.repository.as_str() != request.repository.repository
    {
        return Err(invalid(
            "configured source does not match the request repository",
        ));
    }

    let catalog = store.body_catalog();
    let mut items = Vec::new();
    items.push(ExportedItem {
        item_id: REQUEST_ITEM.into(),
        path: REQUEST_PATH.into(),
        body: body.canonical_body.clone(),
    });
    for input in required_inputs(&request) {
        let resolved = bodies
            .resolve_at(catalog, CAS_CATEGORY, &input.path)
            .map_err(|e| match e {
                BodyError::Missing { detail } => missing(detail),
                BodyError::Unavailable { body_id, state } => {
                    missing(format!("body {body_id} is {state}"))
                }
                BodyError::Corrupt { detail, .. } | BodyError::UnsafePath { detail } => {
                    ExportError::Refusal(DispatchRefusal::BodyCorrupt { detail })
                }
                BodyError::Fault(detail) => fault(detail),
            })?
            .ok_or_else(|| missing(format!("body {} is not retained", input.path)))?;
        if let Some(expected) = input.expected_bytes
            && resolved.bytes.len() as u64 != expected
        {
            return Err(ExportError::Refusal(DispatchRefusal::BodyCorrupt {
                detail: format!(
                    "body {} holds {} bytes, ref records {expected}",
                    input.path,
                    resolved.bytes.len()
                ),
            }));
        }
        (input.verify)(&resolved.bytes)
            .map_err(|detail| ExportError::Refusal(DispatchRefusal::BodyCorrupt { detail }))?;
        items.push(ExportedItem {
            item_id: format!("input-{}", input.name),
            path: format!("input/{}", input.name),
            body: resolved.bytes,
        });
    }

    let (commits, head) = closure_commits(&request)?;
    let bundle = build_source_bundle(scratch, source, handoff_repo, &commits)?;
    items.push(ExportedItem {
        item_id: SOURCE_ITEM.into(),
        path: SOURCE_PATH.into(),
        body: bundle,
    });

    let manifest = ExecutorTransferV1::declared(
        transfer_id,
        request_id,
        chunk_size,
        items
            .iter()
            .map(|i| TransferItemV1 {
                item_id: i.item_id.clone(),
                path: i.path.clone(),
                bytes: i.body.len() as u64,
                digest: item_content_digest(&i.body),
            })
            .collect(),
    )
    .map_err(|e| invalid(format!("exported transfer manifest refused: {e}")))?;
    Ok(ExportedTransfer {
        manifest,
        items,
        head,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn item_paths_are_flat_or_single_level() {
        // The materialized surface never carries a `..`/absolute
        // component — the manifest's own path check covers it, but the
        // export's fixed names are asserted too.
        for name in [REQUEST_PATH, SOURCE_PATH] {
            assert!(
                workloop_core::executor::RemoteHandoffV1::path_is_valid(name),
                "{name}"
            );
        }
    }
}
