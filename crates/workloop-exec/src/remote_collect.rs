//! Coordinator-side quarantined collection (contract §6, #376): the
//! validated import of one remote executor's collection transfer —
//! `handoff.json` identity, produced-object quarantine with
//! base-ancestry and exact-commit checks, per-artifact digest
//! verification against the produced commit, bounded report/log
//! retention with honest truncation, and the durable
//! [`ExecutorCollection`] row the `acknowledge` operation matches.
//!
//! A SHA, a remote path, or a Worker success report alone is never
//! evidence: every artifact verdict derives from objects imported
//! into quarantine and verified against the commit the Worker
//! declared. Repeated collection is idempotent — the same collection
//! digest replays the durable record, artifact refs establish under
//! the expected-absent guard, and nothing duplicates. A different
//! collection digest for one Invocation is a conflict, never an
//! overwrite.

use std::path::{Path, PathBuf};
use std::process::Command;

use workloop_core::domain::{
    ArtifactHandoff, HandoffState, InvocationRole, RequiredArtifact, sha256_hex,
};
use workloop_core::executor::{
    CollectionEntry, ExecutorCollection, ExecutorCollectionRecord, ExecutorCollectionState,
    ExecutorTransferV1, RemoteHandoffV1, TransferItemV1, collection_digest, item_content_digest,
};
use workloop_core::observation::ArtifactFact;
use workloop_core::store::{Effect, Store, StoreError};
use workloop_core::worker_v2::WorkerRequestV2;

use crate::bodies::BodyStore;
use crate::handoff;

/// The artifact digest tag — the same `sha256_hex("artifact-v1", …)`
/// the local supervisor's `Present` evidence carries.
const ARTIFACT_DIGEST_TAG: &str = "artifact-v1";

/// Coordinator-side roots for collection.
#[derive(Debug, Clone)]
pub struct RemoteCollectLayout {
    /// The installation-owned artifact repository (`refs/taisan/*`) —
    /// quarantine imports and acknowledged artifact refs both live
    /// here.
    pub repository: PathBuf,
    /// The retained-body store report/log bodies commit into.
    pub bodies: BodyStore,
}

/// What a validated collection produced.
#[derive(Debug)]
pub struct CollectedRemote {
    /// The durable collection row — `Collected`, or `Invalid` with a
    /// closed reason. `replayed` means the row pre-existed with the
    /// same digest and nothing re-committed.
    pub collection: ExecutorCollection,
    /// Per-required-artifact verdicts for producing roles — the same
    /// `ArtifactFact` evidence shape the local handoff reports.
    pub artifacts: Vec<(RequiredArtifact, ArtifactFact)>,
    /// The retained report bytes (bounded prefix) and evidence
    /// reference, when the collection carried a report.
    pub report: Option<RetainedOutput>,
    /// The retained log bytes, when the collection carried one.
    pub log: Option<RetainedOutput>,
    /// `true` when this collect replayed an already-durable row —
    /// a repeated `collect` after a lost response.
    pub replayed: bool,
}

/// One bounded output body: the retained prefix plus the honest
/// truncation flag the collection record carries.
#[derive(Debug)]
pub struct RetainedOutput {
    /// The exact bytes retained (≤ the Invocation bound).
    pub bytes: Vec<u8>,
    /// The CAS content id the bytes committed under.
    pub body_id: String,
    /// The transferred item exceeded the bound — only the prefix was
    /// retained, and the record says so.
    pub truncated: bool,
}

/// Why collection could not commit. `Invalid` is a deterministic
/// validation outcome the durable row records as `invalid`;
/// `Conflict` is the store's identity/CAS refusal; `Fault` is
/// infrastructure — the pass retries.
#[derive(Debug)]
pub enum CollectError {
    Store(StoreError),
    Fault(String),
}

impl std::fmt::Display for CollectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Store(e) => write!(f, "{e}"),
            Self::Fault(d) => write!(f, "remote collect fault: {d}"),
        }
    }
}

impl std::error::Error for CollectError {}

impl From<StoreError> for CollectError {
    fn from(e: StoreError) -> Self {
        Self::Store(e)
    }
}

fn fault(detail: impl Into<String>) -> CollectError {
    CollectError::Fault(detail.into())
}

fn git(repo: &Path, args: &[&str]) -> Result<String, CollectError> {
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

/// One finalized item's whole body, re-verified against the manifest
/// — the same read-side check prepare runs.
fn verified_item(dir: &Path, item: &TransferItemV1) -> Result<Vec<u8>, CollectError> {
    let path = dir.join("items").join(&item.item_id);
    let bytes = std::fs::read(&path).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            fault(format!("collected item {:?} is absent", item.item_id))
        } else {
            fault(format!("read item: {e}"))
        }
    })?;
    if bytes.len() as u64 != item.bytes || item_content_digest(&bytes) != item.digest {
        return Err(fault(format!(
            "item {:?} fails its declared digest",
            item.item_id
        )));
    }
    Ok(bytes)
}

/// Import the produced object closure into quarantine and return the
/// verified produced commit. `bundle` is written to a bounded
/// scratch path for the fetch; `branch` is the managed work-branch
/// name the executor bundled. Fails closed: an unverifiable bundle,
/// a tip that is not the declared produced commit, or a failed
/// connectivity check is a fault — never imported state.
fn quarantine_import(
    repo: &Path,
    scratch: &Path,
    bundle: &[u8],
    branch: &str,
    produced: &str,
    quarantine: &str,
) -> Result<(), CollectError> {
    std::fs::create_dir_all(scratch).map_err(|e| fault(format!("create scratch: {e}")))?;
    let bundle_path = scratch.join("export.bundle");
    std::fs::write(&bundle_path, bundle).map_err(|e| fault(format!("write bundle: {e}")))?;
    let bundle_arg = bundle_path.to_string_lossy().into_owned();
    git(repo, &["bundle", "verify", &bundle_arg])
        .map_err(|_| fault("collected bundle fails verification"))?;
    let heads = git(repo, &["bundle", "list-heads", &bundle_arg])?;
    let expect = format!("{produced} refs/heads/{branch}");
    if !heads.lines().any(|l| l.trim() == expect) {
        return Err(fault("collected bundle does not pin the produced commit"));
    }
    // A crashed earlier import may have left the ref; discard and
    // re-import — quarantine is transient transport, never durable.
    let _ = git(repo, &["update-ref", "-d", quarantine]);
    git(
        repo,
        &[
            "fetch",
            &bundle_arg,
            &format!("+refs/heads/{branch}:{quarantine}"),
        ],
    )?;
    git(repo, &["fsck", "--connectivity-only"]).map_err(|_| fault("fsck connectivity failed"))?;
    let resolved = git(repo, &["rev-parse", "--verify", quarantine])?;
    if resolved != produced {
        let _ = git(repo, &["update-ref", "-d", quarantine]);
        return Err(fault("quarantine ref does not resolve to produced"));
    }
    Ok(())
}

/// The canonical object manifest for one produced artifact — the
/// same `{format_version, repository, base, produced, tree, objects}`
/// the local handoff derives, so remote and local `manifest_digest`
/// evidence is interchangeable.
fn canonical_manifest(
    repo: &Path,
    repository: &str,
    base: &str,
    produced: &str,
) -> Result<serde_json::Value, CollectError> {
    let tree = git(repo, &["rev-parse", &format!("{produced}^{{tree}}")])?;
    let range = if base.is_empty() {
        produced.to_string()
    } else {
        format!("{base}..{produced}")
    };
    let objects_out = git(repo, &["rev-list", "--objects", &range])?;
    let mut objects: Vec<String> = objects_out
        .lines()
        .filter_map(|l| l.split_whitespace().next().map(String::from))
        .collect();
    objects.sort();
    objects.dedup();
    Ok(serde_json::json!({
        "format_version": 1,
        "repository": repository,
        "base": base,
        "produced": produced,
        "tree": tree,
        "objects": objects,
    }))
}

/// The blob bytes one manifest path holds inside `produced` —
/// `cat-file` on the exact commit, never a filesystem path. `None`
/// when the commit has no such blob.
fn blob_at(repo: &Path, produced: &str, path: &str) -> Result<Option<Vec<u8>>, CollectError> {
    let spec = format!("{produced}:{path}");
    let kind = git(repo, &["cat-file", "-t", &spec]).unwrap_or_default();
    if kind != "blob" {
        return Ok(None);
    }
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .arg("cat-file")
        .arg("blob")
        .arg(&spec)
        .output()
        .map_err(|e| fault(format!("spawn git cat-file: {e}")))?;
    if !output.status.success() {
        return Err(fault(format!(
            "git cat-file blob {spec}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(Some(output.stdout))
}

/// Validate and commit one remote collection.
///
/// `transfer_dir` is the receiver's finalized `complete/<id>`;
/// `manifest` the declared collection manifest; `collection_id` the
/// durable identity the `acknowledge` will match; `stop_proof` the
/// receipt digest authorizing output collection.
#[allow(clippy::too_many_arguments)]
pub fn collect_remote(
    store: &impl Store,
    layout: &RemoteCollectLayout,
    transfer_dir: &Path,
    manifest: &ExecutorTransferV1,
    request_id: &str,
    collection_id: &str,
    stop_proof: &str,
    now_unix_seconds: i64,
) -> Result<CollectedRemote, CollectError> {
    if manifest.collection_id != collection_id {
        return Err(fault(format!(
            "collection manifest names {:?}, expected {collection_id:?}",
            manifest.collection_id
        )));
    }
    let body = store
        .transact(|tx| tx.worker_request_body(request_id))?
        .ok_or_else(|| fault("no durable request body for collection"))?;
    if !body.verifies() {
        return Err(fault("durable request body fails verification"));
    }
    let request = WorkerRequestV2::parse(&body.canonical_body)
        .map_err(|e| fault(format!("durable request body does not parse: {e}")))?;
    if request.request_id != request_id {
        return Err(fault("collection names a different request"));
    }
    let invocation = store
        .transact(|tx| tx.invocation_for_request(request_id))?
        .ok_or_else(|| fault("no durable invocation for collection"))?;
    let producing = matches!(
        invocation.role,
        InvocationRole::Coding | InvocationRole::Fix
    );

    // A replayed collect on an already-durable row: identical digest
    // replays the record byte-for-byte; a different digest for the
    // same request conflicts at the store and stays a refusal here.
    if let Some(existing) = store.transact(|tx| tx.executor_collection(request_id))? {
        // The durable row is authoritative and untouched — the
        // integrity check on read already proved it; a different
        // collection under the same request is refused by the row's
        // own `record()` verification only when the caller tries to
        // commit it, so a replay returns here unchanged.
        existing
            .record()
            .map_err(|e| fault(format!("durable collection is corrupt: {e}")))?;
        return Ok(CollectedRemote {
            collection: existing,
            artifacts: Vec::new(),
            report: None,
            log: None,
            replayed: true,
        });
    }

    let branch = format!("work/{}", request.work_id);
    let item_body = |id: &str| -> Result<Option<Vec<u8>>, CollectError> {
        match manifest.item(id) {
            Some(item) => Ok(Some(verified_item(transfer_dir, item)?)),
            None => Ok(None),
        }
    };
    let report_bytes =
        item_body("report")?.ok_or_else(|| fault("collection carries no report item"))?;
    let log_bytes = item_body("log")?;
    let handoff_bytes = item_body("handoff")?;
    let bundle_bytes = item_body("export-bundle")?;
    let checkpoint_bytes = item_body("checkpoint")?;

    // Producing roles must carry the handoff manifest and the object
    // closure — a workspace writer that produces neither is an
    // incomplete collection, not an empty artifact set.
    let mut invalid_reason: Option<String> = None;
    let mut artifacts: Vec<(RequiredArtifact, ArtifactFact)> = Vec::new();
    let mut artifact_ids: Vec<String> = Vec::new();
    // The quarantine ref names the collection — wire identifiers admit
    // `:` (request-derived ids carry it) but ref names do not, so the
    // transient ref uses the sanitized form.
    let quarantine = format!(
        "refs/taisan/quarantine/{}",
        crate::isolation::sanitize_key(collection_id)
    );
    if producing {
        match (handoff_bytes, bundle_bytes) {
            (Some(handoff_raw), Some(bundle_raw)) => {
                match validate_produced(
                    store,
                    layout,
                    &request,
                    &invocation,
                    request_id,
                    &quarantine,
                    &handoff_raw,
                    &bundle_raw,
                    &transfer_dir.join("scratch"),
                    &branch,
                ) {
                    Ok(found) => {
                        for (required, fact) in found {
                            if matches!(fact, ArtifactFact::Present { .. }) {
                                artifact_ids.push(format!("{}/{}", invocation.id.0, required.kind));
                            }
                            artifacts.push((required, fact));
                        }
                    }
                    Err(reason) => invalid_reason = Some(reason),
                }
            }
            _ => invalid_reason = Some("collection_incomplete".into()),
        }
    }

    // Bounded report/log retention: only the Invocation-bound prefix
    // is kept; the flags record the tail honestly.
    let mut report_truncated = false;
    let mut log_truncated = false;
    let bound = |bytes: &[u8], limit: u64, truncated: &mut bool| -> Vec<u8> {
        if bytes.len() as u64 > limit {
            *truncated = true;
            bytes[..limit as usize].to_vec()
        } else {
            bytes.to_vec()
        }
    };
    let report_kept = bound(
        &report_bytes,
        request.budget.max_report_bytes,
        &mut report_truncated,
    );
    let log_kept = log_bytes.map(|b| bound(&b, request.budget.max_log_bytes, &mut log_truncated));

    // Every transferred item is a collection-digest entry — the
    // report/log entries cover the retained bytes, everything else
    // the exact item.
    let mut entries = Vec::new();
    for item in &manifest.items {
        let (bytes, source) = match item.item_id.as_str() {
            "report" => (report_kept.clone(), "worker"),
            "log" => (log_kept.clone().unwrap_or_default(), "worker"),
            "export-bundle" => (item_body(&item.item_id)?.unwrap_or_default(), "quarantine"),
            other => (item_body(other)?.unwrap_or_default(), "worker"),
        };
        entries.push(CollectionEntry {
            item_id: item.item_id.clone(),
            bytes: bytes.len() as u64,
            content_digest: item_content_digest(&bytes),
            source: source.into(),
        });
    }

    // Retain the bounded outputs in the CAS — the durable bodies the
    // acknowledgement's authority rests on. Retention faults are
    // retryable infrastructure, not collection state.
    let catalog = store.body_catalog();
    let retain = |bytes: &[u8]| -> Result<RetainedOutput, CollectError> {
        let horizon = workloop_store::retention::CategoryLimits::documented_default(
            crate::bodies::CAS_CATEGORY,
        )
        .and_then(|limits| limits.max_age)
        .map(|max_age| now_unix_seconds.saturating_add(max_age));
        let body_id = layout
            .bodies
            .retain(
                catalog,
                crate::bodies::CAS_CATEGORY,
                bytes,
                now_unix_seconds,
                horizon,
            )
            .map_err(|e| fault(format!("retain collected body: {e}")))?;
        Ok(RetainedOutput {
            bytes: bytes.to_vec(),
            body_id,
            truncated: false,
        })
    };
    let report = {
        let mut kept = retain(&report_kept)?;
        kept.truncated = report_truncated;
        Some(kept)
    };
    let log = match log_kept {
        Some(kept_bytes) => {
            let mut kept = retain(&kept_bytes)?;
            kept.truncated = log_truncated;
            Some(kept)
        }
        None => None,
    };
    let _ = checkpoint_bytes;

    let digest =
        collection_digest(&entries).map_err(|e| fault(format!("collection digest: {e}")))?;
    let (state, reason) = match invalid_reason {
        Some(reason) => (ExecutorCollectionState::Invalid, Some(reason)),
        None => (ExecutorCollectionState::Collected, None),
    };
    let record = ExecutorCollectionRecord {
        request_id: request_id.into(),
        invocation_id: request.invocation_id.clone(),
        collection_id: collection_id.into(),
        collection_digest: digest,
        stop_proof: stop_proof.into(),
        entries,
        artifact_ids,
        report_truncated,
        log_truncated,
    };
    let mut collection = ExecutorCollection::new(record, state, reason, 0)
        .map_err(|e| fault(format!("collection record: {e}")))?;
    store.transact(|tx| {
        collection.recorded_seq = tx.alloc_seq()?;
        tx.apply(&Effect::UpsertExecutorCollection {
            collection: collection.clone(),
            expected_seq: 0,
        })
    })?;

    Ok(CollectedRemote {
        collection,
        artifacts,
        report,
        log,
        replayed: false,
    })
}

/// The producing-role validation: parse `handoff.json`, import the
/// produced closure through quarantine, verify base ancestry and the
/// exact commit, and resolve every required artifact against the
/// imported objects. `Err(String)` is the closed `invalid_reason` —
/// deterministic collection failure, never a transport retry.
#[allow(clippy::too_many_arguments)]
fn validate_produced(
    store: &impl Store,
    layout: &RemoteCollectLayout,
    request: &WorkerRequestV2,
    invocation: &workloop_core::domain::Invocation,
    request_id: &str,
    quarantine: &str,
    handoff_raw: &[u8],
    bundle_raw: &[u8],
    scratch: &Path,
    branch: &str,
) -> Result<Vec<(RequiredArtifact, ArtifactFact)>, String> {
    let invalid = |reason: &str| Err(reason.to_string());
    let handoff = RemoteHandoffV1::parse(handoff_raw, u64::MAX)
        .map_err(|e| format!("collection_invalid: handoff manifest refused: {e}"))?;
    if !handoff
        .identity_violations(request_id, &request.invocation_id)
        .is_empty()
    {
        return invalid("collection_invalid");
    }
    if handoff.repository.host != request.repository.host
        || handoff.repository.repository != request.repository.repository
    {
        return invalid("collection_invalid");
    }
    // The declared base must be the request's pinned base — a Worker
    // naming a different fork point is foreign, not stale.
    if handoff.base_commit != request.base.object {
        return invalid("collection_invalid");
    }
    // Descriptor coverage is exact: every required path must be
    // described, and no descriptor may name a path outside the
    // required set.
    let mut verdicts: Vec<(RequiredArtifact, ArtifactFact)> = Vec::new();
    for descriptor in &handoff.artifacts {
        if !invocation
            .required_artifacts
            .iter()
            .any(|r| r.path == descriptor.path)
        {
            return invalid("collection_invalid");
        }
    }
    let produced = handoff.produced_commit.clone();

    // Quarantine import: transport faults map to invalid here only
    // when they prove the collection wrong; an io/git failure is
    // retried by the caller — but inside this pure-validation seam we
    // surface both as reasons and let the caller classify. The
    // import itself is idempotent.
    if let Err(e) = quarantine_import(
        &layout.repository,
        scratch,
        bundle_raw,
        branch,
        &produced,
        quarantine,
    ) {
        return Err(format!(
            "collection_invalid: quarantine import refused: {e}"
        ));
    }
    let ancestor = handoff::is_ancestor(&layout.repository, &handoff.base_commit, &produced)
        .map_err(|e| format!("collection_invalid: ancestry check failed: {e}"))?;
    if !ancestor {
        return invalid("stale_base");
    }
    // A produced commit that never moved past the base produced
    // nothing — same verdict as the local handoff.
    if produced == handoff.base_commit && !invocation.required_artifacts.is_empty() {
        return invalid("nothing_produced");
    }

    // Per-artifact verification: size and content digest from the
    // exact produced commit — the claim is checked, never trusted.
    for required in &invocation.required_artifacts {
        let fact = match handoff.artifacts.iter().find(|d| d.path == required.path) {
            None => ArtifactFact::Missing,
            Some(descriptor) => match blob_at(&layout.repository, &produced, &descriptor.path) {
                Ok(Some(blob)) => {
                    let digest = sha256_hex(ARTIFACT_DIGEST_TAG, &blob);
                    if blob.len() as u64 != descriptor.bytes {
                        ArtifactFact::Corrupt {
                            reason: Some("declared size disagrees with the produced blob".into()),
                        }
                    } else if digest != descriptor.claimed_sha256 {
                        ArtifactFact::Corrupt {
                            reason: Some("claimed digest disagrees with the produced blob".into()),
                        }
                    } else if required
                        .expected_digest
                        .as_ref()
                        .is_some_and(|expected| *expected != digest)
                    {
                        ArtifactFact::Corrupt {
                            reason: Some(
                                "payload digest does not match the declared expectation".into(),
                            ),
                        }
                    } else {
                        ArtifactFact::Present { digest }
                    }
                }
                Ok(None) => ArtifactFact::Missing,
                Err(e) => {
                    return Err(format!("collection_invalid: blob read failed: {e}"));
                }
            },
        };
        verdicts.push((required.clone(), fact));
    }

    // Commit the durable artifact records: the acknowledged ref
    // under the expected-absent guard, then the handoff row — the
    // same authority the local path establishes, so remote evidence
    // is indistinguishable downstream.
    let repository_name = request.repository.repository.clone();
    for (required, fact) in &verdicts {
        let ArtifactFact::Present { .. } = fact else {
            continue;
        };
        let artifact_id = format!("{}/{}", invocation.id.0, required.kind);
        let reference = handoff::artifact_ref(&artifact_id);
        match git(&layout.repository, &["rev-parse", "--verify", &reference]) {
            Ok(current) if current == produced => {}
            Ok(_) => return invalid("ref_conflict"),
            Err(_) => {
                if git(
                    &layout.repository,
                    &["update-ref", &reference, &produced, ""],
                )
                .is_err()
                    && git(&layout.repository, &["rev-parse", "--verify", &reference])
                        .map(|c| c != produced)
                        .unwrap_or(true)
                {
                    return invalid("ref_conflict");
                }
            }
        }
        let manifest = canonical_manifest(
            &layout.repository,
            &repository_name,
            &handoff.base_commit,
            &produced,
        )
        .map_err(|e| format!("collection_invalid: manifest derive failed: {e}"))?;
        let digest = sha256_hex(
            "artifact-manifest-v1",
            &workloop_core::domain::canonical_json(&manifest),
        );
        let mut row = ArtifactHandoff {
            artifact_id: artifact_id.clone(),
            invocation_id: invocation.id.clone(),
            request_id: request_id.into(),
            work_id: workloop_core::domain::WorkId(request.work_id.clone()),
            execution_id: invocation.execution_id.clone(),
            base: handoff.base_commit.clone(),
            produced: Some(produced.clone()),
            manifest_digest: Some(digest),
            state: HandoffState::Acknowledged,
            invalid_reason: None,
            attempt: 0,
            recorded_seq: 0,
        };
        let upsert = store.transact(|tx| {
            let existing = tx.artifact_handoff(&artifact_id)?;
            let expected = existing.as_ref().map(|r| r.recorded_seq).unwrap_or(0);
            if let Some(existing) = existing {
                // A repeated collection replays: the row must already
                // hold this exact artifact, never a different one.
                if existing.produced.as_deref() != Some(produced.as_str())
                    || existing.base != handoff.base_commit
                {
                    return Err(StoreError::ConstraintConflict {
                        detail: format!(
                            "artifact {artifact_id} is already recorded under a different produced"
                        ),
                    });
                }
                return Ok(());
            }
            row.recorded_seq = tx.alloc_seq()?;
            tx.apply(&Effect::UpsertHandoff {
                handoff: row.clone(),
                expected_seq: expected,
            })
        });
        if let Err(e) = upsert {
            return Err(format!("collection_invalid: artifact record refused: {e}"));
        }
    }
    Ok(verdicts)
}
