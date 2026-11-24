//! Executor-side collection custody (contract §6, #376): how the
//! remote executor assembles the bounded collection a `collect`
//! operation carries back, and how it pins remote WIP/output until
//! the coordinator's acknowledgement is durable.
//!
//! Assembly produces the same wire shapes the coordinator imports —
//! a `git bundle` of the managed work branch's full object closure,
//! the out-directory whitelist (`report.json`, `runtime.log`,
//! `checkpoint.json`, `handoff.json`), and the transfer manifest —
//! plus the [`CollectionEntry`] set the collection digest covers.
//!
//! After assembly, custody holds: the workspace, out directory,
//! staged transfer bodies, and receipts stay retained until an
//! `acknowledge` matches the collection's durable ID and digest. A
//! lost acknowledgement response never requires another collection —
//! the [`AckTombstone`] replays the same receipt after the bodies are
//! gone. Cleanup after acknowledgement is scoped (only paths derived
//! from the request identity), bounded (a journaled attempt count),
//! and retryable (a crash mid-cleanup resumes the journal).

use std::path::{Path, PathBuf};
use std::process::Command;

use workloop_core::executor::{
    CollectionEntry, ExecutorTransferV1, TransferItemV1, collection_digest, item_content_digest,
};
use workloop_core::worker_v2::WorkerRequestV2;

use crate::remote_transfer::TransferStore;

/// The durable acknowledgement tombstone: everything an
/// acknowledgement replay needs after the collection bodies are
/// reclaimed — the collection identity and digest, the incarnation
/// pair the receipt names, and the receipt body itself.
/// Acknowledgement tombstones are durable metadata and are not
/// reclaimed while they may be replayed.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AckTombstone {
    pub request_id: String,
    pub invocation_id: String,
    pub collection_id: String,
    /// The contract collection digest the `acknowledge` matched.
    pub collection_digest: String,
    /// The incarnation the request was bound to.
    pub bound_incarnation_id: String,
    /// The incarnation answering this acknowledgement (equals the
    /// bound one unless the endpoint restarted and adopted the
    /// tombstone).
    pub responder_incarnation_id: String,
    /// The stop-proof receipt digest output collection was
    /// authorized under.
    pub stop_proof: String,
    /// The exact receipt body the `acknowledge` operation produced —
    /// a replayed acknowledge returns these bytes unchanged.
    pub receipt_body: Vec<u8>,
}

/// One assembled collection item: transfer identity, materialized
/// path, and whole body.
#[derive(Debug, Clone)]
pub struct CollectionItem {
    pub item_id: String,
    pub path: String,
    pub body: Vec<u8>,
    /// The `source` the collection-digest entry records.
    pub source: String,
}

/// The executor-side collection: the transfer manifest plus every
/// item's body, and the contract collection digest.
#[derive(Debug)]
pub struct AssembledCollection {
    pub manifest: ExecutorTransferV1,
    pub items: Vec<CollectionItem>,
    /// The commit the managed work branch produced — the executor's
    /// own measured tip, never the Worker's claim.
    pub produced: String,
    /// `collection_digest` over the assembled entries.
    pub digest: String,
}

/// Executor-side roots for collection custody.
#[derive(Debug, Clone)]
pub struct CustodyLayout {
    /// `ws-<work_id>` managed workspaces live under here.
    pub workspace_root: PathBuf,
    /// Tombstones (`ack/`), cleanup journals (`cleanup/`), and the
    /// bounded assembly scratch (`scratch/`).
    pub custody_root: PathBuf,
    /// The transfer area holding this executor's staged bodies.
    pub transfers: TransferStore,
}

/// Why a custody step refused.
#[derive(Debug)]
pub enum CustodyError {
    /// Assembly input is missing, malformed, or inconsistent —
    /// `collection_incomplete` / `collection_invalid` material.
    Invalid { detail: String },
    /// The acknowledgement tombstone conflicts with the durable
    /// record — `acknowledgement_conflict`.
    Conflict { detail: String },
    /// Cleanup exceeded its bounded attempt count — kept for
    /// operator attention, never silently abandoned.
    Exhausted { detail: String },
    /// A filesystem or git fault — the step retries.
    Fault(String),
}

impl std::fmt::Display for CustodyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Invalid { detail } => write!(f, "collection invalid: {detail}"),
            Self::Conflict { detail } => write!(f, "acknowledgement_conflict: {detail}"),
            Self::Exhausted { detail } => write!(f, "cleanup exhausted: {detail}"),
            Self::Fault(d) => write!(f, "custody fault: {d}"),
        }
    }
}

impl std::error::Error for CustodyError {}

impl From<std::io::Error> for CustodyError {
    fn from(e: std::io::Error) -> Self {
        Self::Fault(e.to_string())
    }
}

fn fault(detail: impl Into<String>) -> CustodyError {
    CustodyError::Fault(detail.into())
}

fn fsync_dir(path: &Path) -> Result<(), CustodyError> {
    std::fs::File::open(path)
        .and_then(|f| f.sync_all())
        .map_err(|e| fault(format!("sync dir {}: {e}", path.display())))
}

fn git(dir: &Path, args: &[&str]) -> Result<String, CustodyError> {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
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

/// The remote out-directory whitelist (contract §6): `report.json`,
/// `runtime.log`, `checkpoint.json` keep their v0.3 meanings and
/// `handoff.json` is the transfer-metadata member. Anything else in
/// `out/` never becomes a collection item.
const OUT_WHITELIST: &[(&str, &str)] = &[
    ("report", "report.json"),
    ("log", "runtime.log"),
    ("handoff", "handoff.json"),
    ("checkpoint", "checkpoint.json"),
];

/// Assemble the collection transfer for one finished remote
/// Invocation: the work-branch object closure plus the out-directory
/// whitelist. The bundle carries the branch's full history so the
/// coordinator's quarantine verifies and imports it without
/// prerequisites — exactly the local handoff's transport shape.
/// `io_dir` is the prepared per-Invocation tree (`out/` inside it).
pub fn assemble_collection(
    layout: &CustodyLayout,
    request: &WorkerRequestV2,
    io_dir: &Path,
    transfer_id: &str,
    collection_id: &str,
    chunk_size: u64,
) -> Result<AssembledCollection, CustodyError> {
    let workspace = layout
        .workspace_root
        .join(format!("ws-{}", request.work_id));
    let branch = format!("work/{}", request.work_id);
    let produced = git(&workspace, &["rev-parse", "--verify", &branch]).map_err(|_| {
        CustodyError::Invalid {
            detail: "the managed work branch does not resolve".into(),
        }
    })?;

    let scratch = layout.custody_root.join("scratch").join(collection_id);
    if scratch.exists() {
        std::fs::remove_dir_all(&scratch).map_err(|e| fault(format!("clear scratch: {e}")))?;
    }
    std::fs::create_dir_all(&scratch).map_err(|e| fault(format!("create scratch: {e}")))?;
    let bundle = scratch.join("export.bundle");
    let bundle_arg = bundle.to_string_lossy().into_owned();
    git(&workspace, &["bundle", "create", &bundle_arg, &branch]).map_err(|_| {
        CustodyError::Invalid {
            detail: "the work branch could not be bundled".into(),
        }
    })?;

    let mut items = vec![CollectionItem {
        item_id: "export-bundle".into(),
        path: "export.bundle".into(),
        body: std::fs::read(&bundle).map_err(|e| fault(format!("read bundle: {e}")))?,
        source: "quarantine".into(),
    }];
    let out_dir = io_dir.join("out");
    for (item_id, name) in OUT_WHITELIST {
        let path = out_dir.join(name);
        // A whitelist member is only collected when it exists as a
        // regular file — a symlink at an out path is refused, never
        // followed (the out dir is executor-managed but the Worker
        // wrote inside it).
        match std::fs::symlink_metadata(&path) {
            Ok(m) if m.file_type().is_symlink() => {
                return Err(CustodyError::Invalid {
                    detail: format!("out member {name:?} is a symlink"),
                });
            }
            Ok(m) if m.is_file() => {
                items.push(CollectionItem {
                    item_id: (*item_id).into(),
                    path: (*name).into(),
                    body: std::fs::read(&path).map_err(|e| fault(format!("read {name}: {e}")))?,
                    source: "worker".into(),
                });
            }
            Ok(_) => {
                return Err(CustodyError::Invalid {
                    detail: format!("out member {name:?} is not a regular file"),
                });
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
    }

    let entries: Vec<CollectionEntry> = items
        .iter()
        .map(|i| CollectionEntry {
            item_id: i.item_id.clone(),
            bytes: i.body.len() as u64,
            content_digest: item_content_digest(&i.body),
            source: i.source.clone(),
        })
        .collect();
    let digest = collection_digest(&entries).map_err(|e| fault(format!("digest: {e}")))?;
    let manifest = ExecutorTransferV1::declared(
        transfer_id,
        collection_id,
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
    .map_err(|e| CustodyError::Invalid {
        detail: format!("collection manifest refused: {e}"),
    })?;
    Ok(AssembledCollection {
        manifest,
        items,
        produced,
        digest,
    })
}

/// The tombstone path one collection's acknowledgement occupies.
fn tombstone_path(root: &Path, collection_id: &str) -> PathBuf {
    root.join("ack").join(format!("{collection_id}.json"))
}

/// Commit the acknowledgement tombstone — write, fsync, rename. An
/// existing tombstone must hold identical identity *and* receipt
/// bytes: a different acknowledgement for one collection is a
/// conflict, never an overwrite.
pub fn commit_tombstone(
    layout: &CustodyLayout,
    tombstone: &AckTombstone,
) -> Result<(), CustodyError> {
    let path = tombstone_path(&layout.custody_root, &tombstone.collection_id);
    if let Some(existing) = read_tombstone(layout, &tombstone.collection_id)? {
        if existing == *tombstone {
            return Ok(());
        }
        return Err(CustodyError::Conflict {
            detail: format!(
                "collection {} is already acknowledged under a different record",
                tombstone.collection_id
            ),
        });
    }
    std::fs::create_dir_all(path.parent().expect("ack dir"))
        .map_err(|e| fault(format!("create ack dir: {e}")))?;
    let tmp = path.with_extension("tmp");
    std::fs::write(
        &tmp,
        serde_json::to_vec(tombstone).expect("tombstone serializes"),
    )
    .map_err(|e| fault(format!("write tombstone: {e}")))?;
    std::fs::File::open(&tmp)
        .and_then(|f| f.sync_all())
        .map_err(|e| fault(format!("sync tombstone: {e}")))?;
    std::fs::rename(&tmp, &path).map_err(|e| fault(format!("rename tombstone: {e}")))?;
    fsync_dir(path.parent().expect("ack dir"))?;
    Ok(())
}

/// The recorded tombstone for one collection, if any — the replay a
/// repeated acknowledge or a post-restart first-ack returns.
pub fn read_tombstone(
    layout: &CustodyLayout,
    collection_id: &str,
) -> Result<Option<AckTombstone>, CustodyError> {
    let path = tombstone_path(&layout.custody_root, collection_id);
    match std::fs::read(&path) {
        Ok(raw) => serde_json::from_slice(&raw)
            .map(Some)
            .map_err(|e| fault(format!("tombstone {} unreadable: {e}", path.display()))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// The cleanup journal: the scoped path set one request's
/// post-acknowledgement reclaim still owes, plus the bounded attempt
/// counter. Scoped means enumerated — every path here is derived
/// from the request identity at commit time, never globbed at
/// reclaim time.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct CleanupJournal {
    request_id: String,
    collection_id: String,
    /// Absolute paths under the custody/workspace roots, each
    /// removal attempted independently.
    pending: Vec<PathBuf>,
    attempts: u32,
}

fn journal_path(root: &Path, collection_id: &str) -> PathBuf {
    root.join("cleanup").join(format!("{collection_id}.json"))
}

fn write_journal(root: &Path, journal: &CleanupJournal) -> Result<(), CustodyError> {
    let path = journal_path(root, &journal.collection_id);
    std::fs::create_dir_all(path.parent().expect("cleanup dir"))
        .map_err(|e| fault(format!("create cleanup dir: {e}")))?;
    let tmp = path.with_extension("tmp");
    std::fs::write(
        &tmp,
        serde_json::to_vec(journal).expect("journal serializes"),
    )
    .map_err(|e| fault(format!("write journal: {e}")))?;
    std::fs::rename(&tmp, &path).map_err(|e| fault(format!("rename journal: {e}")))?;
    fsync_dir(path.parent().expect("cleanup dir"))?;
    Ok(())
}

/// What one cleanup pass resolved.
#[derive(Debug, PartialEq, Eq)]
pub enum CleanupOutcome {
    /// Every scoped path is gone; the journal is closed.
    Complete,
    /// Some removals failed this pass — the journal records the
    /// remaining set and the pass retries.
    Retryable { pending: usize },
}

/// The bounded attempt count after which cleanup stops retrying —
/// the journal stays so an operator can inspect what wedged.
pub const MAX_CLEANUP_ATTEMPTS: u32 = 5;

/// Begin or continue the post-acknowledgement cleanup for one
/// collection. `paths` enumerates the managed locations the
/// collection's custody pinned — the workspace, the io tree, staged
/// transfer bodies, scratch — and only those paths are ever removed.
/// Each pass attempts every pending path; a crash resumes the
/// journal; `MAX_CLEANUP_ATTEMPTS` bounds total passes.
pub fn cleanup_after_ack(
    layout: &CustodyLayout,
    request_id: &str,
    collection_id: &str,
    paths: &[PathBuf],
) -> Result<CleanupOutcome, CustodyError> {
    let journal_file = journal_path(&layout.custody_root, collection_id);
    let mut journal = match std::fs::read(&journal_file) {
        Ok(raw) => serde_json::from_slice::<CleanupJournal>(&raw)
            .map_err(|e| fault(format!("cleanup journal unreadable: {e}")))?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => CleanupJournal {
            request_id: request_id.into(),
            collection_id: collection_id.into(),
            pending: paths.to_vec(),
            attempts: 0,
        },
        Err(e) => return Err(e.into()),
    };
    if journal.request_id != request_id || journal.collection_id != collection_id {
        return Err(CustodyError::Conflict {
            detail: "cleanup journal belongs to another collection".into(),
        });
    }
    if journal.attempts >= MAX_CLEANUP_ATTEMPTS {
        return Err(CustodyError::Exhausted {
            detail: format!(
                "{} managed paths remain after {} attempts",
                journal.pending.len(),
                journal.attempts
            ),
        });
    }
    journal.attempts += 1;
    // Every pending path must resolve inside the custody or workspace
    // root — the scope check is the only thing standing between a
    // recorded list and arbitrary removal.
    let roots = [layout.custody_root.clone(), layout.workspace_root.clone()];
    let mut still_pending = Vec::new();
    for path in &journal.pending {
        let under_root = roots.iter().any(|root| path.starts_with(root));
        if !under_root || path.as_os_str().is_empty() {
            return Err(CustodyError::Conflict {
                detail: format!("cleanup path {:?} is outside the managed roots", path),
            });
        }
        match std::fs::symlink_metadata(path) {
            Ok(_) => {
                if std::fs::remove_dir_all(path).is_err() {
                    let _ = std::fs::remove_file(path);
                }
                if path.exists() {
                    still_pending.push(path.clone());
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => still_pending.push(path.clone()),
        }
    }
    journal.pending = still_pending;
    if journal.pending.is_empty() {
        let _ = std::fs::remove_file(&journal_file);
        // A first-pass completion never wrote the journal — the
        // cleanup dir may not exist to fsync; the fsync matters only
        // when a removal was committed against a recorded journal.
        let cleanup_dir = journal_file.parent().expect("cleanup dir");
        if cleanup_dir.exists() {
            fsync_dir(cleanup_dir)?;
        }
        return Ok(CleanupOutcome::Complete);
    }
    write_journal(&layout.custody_root, &journal)?;
    Ok(CleanupOutcome::Retryable {
        pending: journal.pending.len(),
    })
}
