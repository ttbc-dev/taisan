//! Bounded authenticated chunk staging (contract §6, #376): the
//! filesystem mechanics one transfer session's receiver runs — both
//! directions share them (a `prepare` transfer and a `collect`
//! transfer differ only in which side declares the manifest).
//!
//! A declared manifest lands in `staging/<transfer_id>/manifest.json`
//! before any data frame is accepted. Each chunk writes into
//! `staging/<transfer_id>/items/` under the item's *encoded* name at
//! its declared offset and advances a small per-item offset journal
//! only after the body bytes are durable — a crash mid-write resumes
//! at the journaled offset, never at a torn file length. A chunk
//! replayed with the same transfer, collection, item, offset, and
//! digest is idempotent; a different digest at an occupied offset is
//! `conflicting_replay`. Finalization re-verifies every item's whole
//! body against the declared content digest and publishes
//! `complete/<transfer_id>` by rename — the Worker only ever sees a
//! fully validated set.

use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use workloop_core::executor::{
    ExecutorChunkV1, ExecutorTransferV1, item_content_digest, transfer_id_is_valid,
};

/// Outcome of one accepted chunk frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChunkOutcome {
    /// The chunk body appended at the expected offset; `next_offset`
    /// is the resume position a `pending` receipt reports.
    Accepted { next_offset: u64 },
    /// An identical chunk at an already-occupied offset — the durable
    /// idempotent replay; nothing was rewritten.
    Replayed { next_offset: u64 },
}

/// Why a transfer step refused. `ConflictingReplay`/`DigestMismatch`/
/// `OutOfOrder` are contract-visible conditions; `Foreign` refuses a
/// chunk that names another session; `Fault` is local io.
#[derive(Debug)]
pub enum TransferError {
    /// The chunk's transfer or collection id does not match the
    /// declared session, or the manifest re-declared differently
    /// under a live transfer id.
    Foreign { detail: String },
    /// The item the chunk names is not declared.
    UnknownItem { item_id: String },
    /// A chunk at an occupied offset carries a different digest, or a
    /// manifest re-declares under a live transfer id with different
    /// canonical bytes.
    ConflictingReplay { detail: String },
    /// A chunk body or a finalized item failed its declared digest.
    DigestMismatch { detail: String },
    /// The chunk declares a gap: bytes arrive only at the journaled
    /// offset. The receipt layer turns this into `pending`.
    OutOfOrder { next_offset: u64 },
    /// A declared byte/count bound was exceeded.
    LimitExceeded { detail: String },
    /// Finalize was called before every item was whole.
    Incomplete { detail: String },
    /// Staging state is corrupt — an offset journal disagrees with
    /// the staged file, or a staged path is not a regular file.
    Corrupt { detail: String },
    /// A filesystem fault.
    Fault(String),
}

impl std::fmt::Display for TransferError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Foreign { detail } => write!(f, "foreign transfer: {detail}"),
            Self::UnknownItem { item_id } => write!(f, "undeclared item {item_id:?}"),
            Self::ConflictingReplay { detail } => write!(f, "conflicting_replay: {detail}"),
            Self::DigestMismatch { detail } => write!(f, "digest mismatch: {detail}"),
            Self::OutOfOrder { next_offset } => {
                write!(f, "chunk gap; next offset is {next_offset}")
            }
            Self::LimitExceeded { detail } => write!(f, "limit exceeded: {detail}"),
            Self::Incomplete { detail } => write!(f, "incomplete transfer: {detail}"),
            Self::Corrupt { detail } => write!(f, "corrupt staging: {detail}"),
            Self::Fault(detail) => write!(f, "transfer io: {detail}"),
        }
    }
}

impl std::error::Error for TransferError {}

impl From<std::io::Error> for TransferError {
    fn from(e: std::io::Error) -> Self {
        Self::Fault(e.to_string())
    }
}

fn fault(detail: impl Into<String>) -> TransferError {
    TransferError::Fault(detail.into())
}

fn fsync_dir(path: &Path) -> Result<(), TransferError> {
    std::fs::File::open(path)
        .and_then(|f| f.sync_all())
        .map_err(|e| fault(format!("sync dir {}: {e}", path.display())))
}

/// The on-disk name one item's staged body occupies — a fixed
/// 64-hex digest of the item_id, never the raw identifier. A valid
/// identifier may contain '.' and run to 256 characters, so a
/// verbatim name can collide with another item's `<id>.offset`
/// journal (item `a.offset` versus the journal of item `a`) or
/// overflow the filename limit; a bounded dot-free digest keeps the
/// body name and its `<name>.offset` journal unambiguous for every
/// legal id.
fn item_file_name(item_id: &str) -> String {
    workloop_core::domain::sha256_hex("taisan.executor.item-name.v1", item_id.as_bytes())
}

/// The path one item's staged body occupies inside a transfer dir —
/// the encoded name the receiver writes and the importers re-read.
pub fn item_path(dir: &Path, item_id: &str) -> PathBuf {
    dir.join("items").join(item_file_name(item_id))
}

/// The per-item offset journal: one 8-byte big-endian counter
/// naming the next expected offset — the durable resume position.
fn journal_path(dir: &Path, item_id: &str) -> PathBuf {
    dir.join("items")
        .join(format!("{}.offset", item_file_name(item_id)))
}

/// One transfer area: `staging/` holds in-flight sessions,
/// `complete/` holds finalized ones. The transport never exposes a
/// staging directory to the Worker.
#[derive(Debug, Clone)]
pub struct TransferStore {
    root: PathBuf,
}

impl TransferStore {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    /// The in-flight staging directory — custody pins and reclaims
    /// it, never the transport.
    pub fn staging_dir(&self, transfer_id: &str) -> PathBuf {
        self.root.join("staging").join(transfer_id)
    }

    /// The finalized directory a completed session published under.
    pub fn complete_dir(&self, transfer_id: &str) -> PathBuf {
        self.root.join("complete").join(transfer_id)
    }

    /// The finalized item directory if this transfer already
    /// completed; `None` for an in-flight or absent session.
    pub fn completed(&self, transfer_id: &str) -> Option<PathBuf> {
        let dir = self.complete_dir(transfer_id);
        if dir.is_dir() { Some(dir) } else { None }
    }

    /// The completed directory when this transfer finalized under a
    /// byte-identical manifest — the idempotent finalize replay the
    /// contract requires. A completed transfer under a different
    /// manifest is `conflicting_replay`, mirroring `declare`.
    fn completed_match(
        &self,
        transfer_id: &str,
        canonical: &[u8],
    ) -> Result<Option<PathBuf>, TransferError> {
        let Some(done) = self.completed(transfer_id) else {
            return Ok(None);
        };
        let recorded = self.completed_manifest(transfer_id)?;
        if recorded == canonical {
            Ok(Some(done))
        } else {
            Err(TransferError::ConflictingReplay {
                detail: format!(
                    "transfer {transfer_id:?} already completed under a different manifest"
                ),
            })
        }
    }

    /// The journaled resume position: the offset file if present and
    /// consistent with the staged body, else 0 for a not-yet-started
    /// item. A journal naming more than the staged file holds is
    /// corrupt — the file would have to have lost durable bytes.
    fn next_offset(dir: &Path, item_id: &str, declared_bytes: u64) -> Result<u64, TransferError> {
        let body_path = item_path(dir, item_id);
        let body_len = match std::fs::symlink_metadata(&body_path) {
            Ok(m) if m.is_file() => m.len(),
            Ok(_) => {
                return Err(TransferError::Corrupt {
                    detail: format!("staged item {item_id:?} is not a regular file"),
                });
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => 0,
            Err(e) => return Err(e.into()),
        };
        let journal = journal_path(dir, item_id);
        let offset = match std::fs::read(&journal) {
            Ok(raw) if raw.len() == 8 => u64::from_be_bytes(raw.try_into().expect("8 bytes")),
            Ok(_) => {
                return Err(TransferError::Corrupt {
                    detail: format!("offset journal for {item_id:?} is malformed"),
                });
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => 0,
            Err(e) => return Err(e.into()),
        };
        if offset > body_len || offset > declared_bytes {
            return Err(TransferError::Corrupt {
                detail: format!(
                    "offset journal for {item_id:?} names {offset}, staged body holds {body_len}"
                ),
            });
        }
        Ok(offset)
    }

    /// Read one staged manifest's canonical bytes — the identity a
    /// resumed session re-declares byte-identically.
    fn staged_manifest(&self, transfer_id: &str) -> Result<Option<Vec<u8>>, TransferError> {
        match std::fs::read(self.staging_dir(transfer_id).join("manifest.json")) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// The declared manifest for a live or completed session — the
    /// receipt layer re-reads it to bind chunks and results.
    pub fn manifest(&self, transfer_id: &str) -> Result<ExecutorTransferV1, TransferError> {
        let raw = match self.staged_manifest(transfer_id)? {
            Some(raw) => raw,
            None => match self.completed_manifest(transfer_id) {
                Ok(raw) => raw,
                Err(_) => {
                    return Err(TransferError::Foreign {
                        detail: format!("no transfer {transfer_id:?} is declared"),
                    });
                }
            },
        };
        ExecutorTransferV1::parse(&raw).map_err(|e| TransferError::Corrupt {
            detail: format!("staged manifest for {transfer_id:?} does not parse: {e}"),
        })
    }

    fn completed_manifest(&self, transfer_id: &str) -> Result<Vec<u8>, TransferError> {
        std::fs::read(self.complete_dir(transfer_id).join("manifest.json"))
            .map_err(|e| fault(format!("read completed manifest: {e}")))
    }

    /// Declare (or resume) a transfer session. The first declare
    /// writes the canonical manifest; a replayed declare with
    /// byte-identical canonical bytes is the contract's resume — a
    /// different manifest under a live transfer id conflicts.
    /// Returns the per-item resume positions the `pending` receipt
    /// carries.
    pub fn declare(
        &self,
        manifest: &ExecutorTransferV1,
    ) -> Result<Vec<(String, u64)>, TransferError> {
        if !transfer_id_is_valid(&manifest.transfer_id) {
            return Err(TransferError::Foreign {
                detail: "transfer_id is not a safe identifier".into(),
            });
        }
        let canonical = manifest
            .to_canonical_bytes()
            .map_err(|e| fault(format!("manifest canonicalization: {e}")))?;

        // A completed session replays as already done: the receipt
        // layer reports the immutable final state rather than
        // reopening staging — under a different manifest it is
        // `conflicting_replay`.
        if self
            .completed_match(&manifest.transfer_id, &canonical)?
            .is_some()
        {
            return Ok(Vec::new());
        }

        let dir = self.staging_dir(&manifest.transfer_id);
        match self.staged_manifest(&manifest.transfer_id)? {
            Some(existing) if existing != canonical => {
                return Err(TransferError::ConflictingReplay {
                    detail: format!(
                        "transfer {:?} re-declared under a different manifest",
                        manifest.transfer_id
                    ),
                });
            }
            Some(_) => {}
            None => {
                let items = dir.join("items");
                std::fs::create_dir_all(&items)
                    .map_err(|e| fault(format!("create {}: {e}", items.display())))?;
                // Manifest lands before any data frame is accepted:
                // write-then-fsync-then-rename so a crash can never
                // leave a torn manifest a chunk would validate against.
                let tmp = dir.join("manifest.json.tmp");
                std::fs::write(&tmp, &canonical)
                    .map_err(|e| fault(format!("write manifest: {e}")))?;
                std::fs::File::open(&tmp)
                    .and_then(|f| f.sync_all())
                    .map_err(|e| fault(format!("sync manifest: {e}")))?;
                std::fs::rename(&tmp, dir.join("manifest.json"))
                    .map_err(|e| fault(format!("rename manifest: {e}")))?;
                fsync_dir(&dir)?;
            }
        }
        self.progress(manifest)
    }

    /// The per-item resume positions for a declared transfer — the
    /// `pending` receipt's `next_offset` set.
    pub fn progress(
        &self,
        manifest: &ExecutorTransferV1,
    ) -> Result<Vec<(String, u64)>, TransferError> {
        let dir = self.staging_dir(&manifest.transfer_id);
        let mut offsets = Vec::with_capacity(manifest.items.len());
        for item in &manifest.items {
            offsets.push((
                item.item_id.clone(),
                Self::next_offset(&dir, &item.item_id, item.bytes)?,
            ));
        }
        Ok(offsets)
    }

    /// Accept one chunk frame. The header must match the declared
    /// session and item, the body must satisfy `bytes` +
    /// `chunk_digest`, and the write lands at the journaled offset —
    /// an identical replay at an occupied offset is idempotent, a
    /// different digest there is `conflicting_replay`, a gap is
    /// `pending`.
    pub fn accept_chunk(
        &self,
        manifest: &ExecutorTransferV1,
        chunk: &ExecutorChunkV1,
        body: &[u8],
    ) -> Result<ChunkOutcome, TransferError> {
        if chunk.transfer_id != manifest.transfer_id
            || chunk.collection_id != manifest.collection_id
        {
            return Err(TransferError::Foreign {
                detail: format!(
                    "chunk names transfer {:?}/collection {:?}, this session is {:?}/{:?}",
                    chunk.transfer_id,
                    chunk.collection_id,
                    manifest.transfer_id,
                    manifest.collection_id
                ),
            });
        }
        let Some(item) = manifest.item(&chunk.item_id) else {
            return Err(TransferError::UnknownItem {
                item_id: chunk.item_id.clone(),
            });
        };
        if body.len() as u64 != chunk.bytes {
            return Err(TransferError::LimitExceeded {
                detail: format!(
                    "chunk body holds {} bytes, header declares {}",
                    body.len(),
                    chunk.bytes
                ),
            });
        }
        if chunk.bytes == 0 || chunk.bytes > manifest.chunk_size {
            return Err(TransferError::LimitExceeded {
                detail: format!(
                    "chunk of {} bytes outside 1..={}",
                    chunk.bytes, manifest.chunk_size
                ),
            });
        }
        if chunk
            .offset
            .checked_add(chunk.bytes)
            .is_none_or(|end| end > item.bytes)
        {
            return Err(TransferError::LimitExceeded {
                detail: format!(
                    "chunk {}+{} overruns item {:?} ({} bytes)",
                    chunk.offset, chunk.bytes, chunk.item_id, item.bytes
                ),
            });
        }
        if !chunk.verifies_body(body) {
            return Err(TransferError::DigestMismatch {
                detail: "chunk body fails its declared digest".into(),
            });
        }

        let dir = self.staging_dir(&manifest.transfer_id);
        let staged = Self::next_offset(&dir, &chunk.item_id, item.bytes)?;
        if chunk.offset > staged {
            return Err(TransferError::OutOfOrder {
                next_offset: staged,
            });
        }
        let end = (chunk.offset + chunk.bytes) as usize;
        if end <= staged as usize {
            // Fully occupied range: the replay is idempotent only when
            // the staged bytes re-digest to the declared chunk digest —
            // the receiver's own bytes are the comparison, never the
            // sender's claim alone.
            let mut file = std::fs::File::open(item_path(&dir, &chunk.item_id))
                .map_err(|e| fault(format!("open staged item: {e}")))?;
            file.seek(SeekFrom::Start(chunk.offset))
                .map_err(|e| fault(format!("seek staged item: {e}")))?;
            let mut held = vec![0u8; chunk.bytes as usize];
            file.read_exact(&mut held)
                .map_err(|e| fault(format!("read staged item: {e}")))?;
            if workloop_core::executor::chunk_body_digest(&held) != chunk.chunk_digest {
                return Err(TransferError::ConflictingReplay {
                    detail: format!(
                        "offset {} in item {:?} is occupied by different bytes",
                        chunk.offset, chunk.item_id
                    ),
                });
            }
            return Ok(ChunkOutcome::Replayed {
                next_offset: staged,
            });
        }
        // Partially occupied range: the already-staged prefix must
        // match, and only the tail beyond `staged` appends.
        if chunk.offset < staged {
            let mut file = std::fs::File::open(item_path(&dir, &chunk.item_id))
                .map_err(|e| fault(format!("open staged item: {e}")))?;
            file.seek(SeekFrom::Start(chunk.offset))
                .map_err(|e| fault(format!("seek staged item: {e}")))?;
            let mut held = vec![0u8; (staged - chunk.offset) as usize];
            file.read_exact(&mut held)
                .map_err(|e| fault(format!("read staged item: {e}")))?;
            let prefix_len = (staged - chunk.offset) as usize;
            if held != body[..prefix_len] {
                return Err(TransferError::ConflictingReplay {
                    detail: format!(
                        "offset {} in item {:?} is occupied by different bytes",
                        chunk.offset, chunk.item_id
                    ),
                });
            }
        }

        // Append the uncovered suffix at the journaled offset, fsync
        // the body, then advance the journal — a crash between them
        // resumes at the old offset and the rewritten suffix is an
        // identical replay.
        let body_path = item_path(&dir, &chunk.item_id);
        std::fs::create_dir_all(body_path.parent().expect("items dir"))
            .map_err(|e| fault(format!("create items dir: {e}")))?;
        let skip = (staged - chunk.offset) as usize;
        {
            let mut file = std::fs::OpenOptions::new()
                .create(true)
                .truncate(false)
                .write(true)
                .open(&body_path)
                .map_err(|e| fault(format!("open {}: {e}", body_path.display())))?;
            file.seek(SeekFrom::Start(staged))
                .map_err(|e| fault(format!("seek {}: {e}", body_path.display())))?;
            file.write_all(&body[skip..])
                .map_err(|e| fault(format!("write {}: {e}", body_path.display())))?;
            file.sync_all()
                .map_err(|e| fault(format!("sync {}: {e}", body_path.display())))?;
        }
        let journal = journal_path(&dir, &chunk.item_id);
        let next = staged + (body.len() - skip) as u64;
        std::fs::write(&journal, next.to_be_bytes())
            .map_err(|e| fault(format!("write journal: {e}")))?;
        std::fs::File::open(&journal)
            .and_then(|f| f.sync_all())
            .map_err(|e| fault(format!("sync journal: {e}")))?;
        fsync_dir(&dir.join("items"))?;
        Ok(ChunkOutcome::Accepted { next_offset: next })
    }

    /// Finalize a session: every declared item must be whole and
    /// re-digest to its manifest identity, then the staged directory
    /// publishes under `complete/` by rename — atomic, so a finalized
    /// set is never partially visible. Idempotent: an already-completed
    /// transfer under the same manifest returns its directory, so a
    /// retried finalize after a lost response is the replay, never an
    /// `Incomplete` or a fault.
    pub fn finalize(&self, manifest: &ExecutorTransferV1) -> Result<PathBuf, TransferError> {
        let canonical = manifest
            .to_canonical_bytes()
            .map_err(|e| fault(format!("manifest canonicalization: {e}")))?;
        // The completed set is the durable answer: an identical
        // manifest replays it, a different one conflicts.
        if let Some(done) = self.completed_match(&manifest.transfer_id, &canonical)? {
            return Ok(done);
        }
        let dir = self.staging_dir(&manifest.transfer_id);
        for item in &manifest.items {
            let staged = Self::next_offset(&dir, &item.item_id, item.bytes)?;
            if staged != item.bytes {
                return Err(TransferError::Incomplete {
                    detail: format!(
                        "item {:?} holds {staged} of {} declared bytes",
                        item.item_id, item.bytes
                    ),
                });
            }
            let path = item_path(&dir, &item.item_id);
            // A zero-byte item emits no chunk frame and `accept_chunk`
            // refuses a 0-byte body, so no staged file ever exists for
            // it: the whole body is the empty file, materialized here
            // at finalize so the completed set holds every declared
            // item.
            if item.bytes == 0
                && matches!(
                    std::fs::symlink_metadata(&path),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound
                )
            {
                std::fs::write(&path, b"").map_err(|e| fault(format!("create empty item: {e}")))?;
                std::fs::File::open(&path)
                    .and_then(|f| f.sync_all())
                    .map_err(|e| fault(format!("sync empty item: {e}")))?;
            }
            let bytes =
                std::fs::read(&path).map_err(|e| fault(format!("read finalized item: {e}")))?;
            if bytes.len() as u64 != item.bytes || item_content_digest(&bytes) != item.digest {
                return Err(TransferError::DigestMismatch {
                    detail: format!("item {:?} fails its declared digest", item.item_id),
                });
            }
        }
        let target = self.complete_dir(&manifest.transfer_id);
        std::fs::create_dir_all(target.parent().expect("complete root"))
            .map_err(|e| fault(format!("create complete root: {e}")))?;
        // Strip the journals — the completed set is items + manifest
        // only, and a leftover journal is not part of the materialized
        // surface.
        let items_dir = dir.join("items");
        if let Ok(entries) = std::fs::read_dir(&items_dir) {
            for entry in entries.flatten() {
                if entry.file_name().to_string_lossy().ends_with(".offset") {
                    let _ = std::fs::remove_file(entry.path());
                }
            }
        }
        fsync_dir(&items_dir)?;
        fsync_dir(&dir)?;
        if let Err(e) = std::fs::rename(&dir, &target) {
            // A racing finalizer may have won the rename: when the
            // completed set matches this manifest the race resolved
            // to the same answer — return it rather than a fault.
            if let Some(done) = self.completed_match(&manifest.transfer_id, &canonical)? {
                return Ok(done);
            }
            return Err(fault(format!("rename {}: {e}", dir.display())));
        }
        fsync_dir(target.parent().expect("complete root"))?;
        Ok(target)
    }
}

/// The `(header, body)` frames one declared item produces, in order —
/// the send side of the same bound. `body` is the item's whole bytes;
/// each frame borrows a `chunk_size` slice.
pub fn item_chunks<'a>(
    manifest: &'a ExecutorTransferV1,
    item_id: &str,
    body: &'a [u8],
) -> Vec<(ExecutorChunkV1, &'a [u8])> {
    let mut frames = Vec::new();
    let mut offset = 0u64;
    for slice in body.chunks(manifest.chunk_size as usize) {
        if slice.is_empty() {
            break;
        }
        frames.push((
            ExecutorChunkV1::new(
                manifest.transfer_id.clone(),
                manifest.collection_id.clone(),
                item_id,
                offset,
                slice,
            ),
            slice,
        ));
        offset += slice.len() as u64;
    }
    frames
}

#[cfg(test)]
mod tests {
    use super::*;
    use workloop_core::executor::TransferItemV1;

    fn root(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "remote-transfer-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn item(id: &str, path: &str, body: &[u8]) -> TransferItemV1 {
        TransferItemV1 {
            item_id: id.into(),
            path: path.into(),
            bytes: body.len() as u64,
            digest: item_content_digest(body),
        }
    }

    fn manifest(
        transfer_id: &str,
        chunk_size: u64,
        items: Vec<TransferItemV1>,
    ) -> ExecutorTransferV1 {
        ExecutorTransferV1::declared(transfer_id, "col-1", chunk_size, items).unwrap()
    }

    #[test]
    fn a_full_item_transfers_and_finalizes() {
        let store = TransferStore::new(root("full"));
        let body = b"hello world".repeat(100);
        let m = manifest("t-1", 64, vec![item("a", "a.bin", &body)]);
        store.declare(&m).unwrap();
        for (chunk, bytes) in item_chunks(&m, "a", &body) {
            store.accept_chunk(&m, &chunk, bytes).unwrap();
        }
        let done = store.finalize(&m).unwrap();
        assert_eq!(
            std::fs::read(item_path(&done, "a")).unwrap(),
            body,
            "the finalized item is the exact body"
        );
    }

    /// An item_id ending in `.offset` is legal per the wire grammar,
    /// so on-disk names must be encoded: item `a.offset`'s body must
    /// never collide with item `a`'s `<id>.offset` journal.
    #[test]
    fn an_item_named_like_a_journal_does_not_collide() {
        let store = TransferStore::new(root("collision"));
        let body_a = b"aaaa".repeat(30);
        let body_j = b"journal-looking".repeat(10);
        let m = manifest(
            "t-1",
            60,
            vec![
                item("a", "a.bin", &body_a),
                item("a.offset", "a-offset.bin", &body_j),
            ],
        );
        store.declare(&m).unwrap();
        // Interleave so `a`'s journal exists while `a.offset` is
        // mid-flight — the journal must not read as `a.offset`'s
        // body nor be clobbered by its chunks.
        let fa = item_chunks(&m, "a", &body_a);
        let fj = item_chunks(&m, "a.offset", &body_j);
        store.accept_chunk(&m, &fa[0].0, fa[0].1).unwrap();
        store.accept_chunk(&m, &fj[0].0, fj[0].1).unwrap();
        let progress = store.progress(&m).unwrap();
        assert_eq!(
            progress,
            vec![("a".to_string(), 60), ("a.offset".to_string(), 60)]
        );
        for (chunk, bytes) in &fa[1..] {
            store.accept_chunk(&m, chunk, bytes).unwrap();
        }
        for (chunk, bytes) in &fj[1..] {
            store.accept_chunk(&m, chunk, bytes).unwrap();
        }
        let done = store.finalize(&m).unwrap();
        assert_eq!(std::fs::read(item_path(&done, "a")).unwrap(), body_a);
        assert_eq!(std::fs::read(item_path(&done, "a.offset")).unwrap(), body_j);
    }

    /// `finalize` is contracted idempotent: after the staging rename
    /// wins, a retried finalize — the lost-response replay — returns
    /// the completed directory, never `Incomplete` or a rename fault.
    /// A different manifest under the same transfer id conflicts.
    #[test]
    fn finalize_replays_the_completed_result() {
        let store = TransferStore::new(root("finalize-idem"));
        let body = b"payload".repeat(20);
        let m = manifest("t-1", 40, vec![item("a", "a.bin", &body)]);
        store.declare(&m).unwrap();
        for (chunk, bytes) in item_chunks(&m, "a", &body) {
            store.accept_chunk(&m, &chunk, bytes).unwrap();
        }
        let done = store.finalize(&m).unwrap();
        assert!(!store.staging_dir("t-1").exists());
        let replayed = store.finalize(&m).unwrap();
        assert_eq!(replayed, done);
        let other = manifest("t-1", 40, vec![item("a", "a.bin", b"different!")]);
        let err = store.finalize(&other).unwrap_err();
        assert!(
            matches!(err, TransferError::ConflictingReplay { .. }),
            "a different manifest under a completed transfer id conflicts, got {err:?}"
        );
    }

    #[test]
    fn an_interrupted_item_resumes_at_the_journaled_offset() {
        let store = TransferStore::new(root("resume"));
        let body = b"abcdef".repeat(50);
        let m = manifest("t-1", 60, vec![item("a", "a.bin", &body)]);
        store.declare(&m).unwrap();
        let frames = item_chunks(&m, "a", &body);
        // Two chunks in, then an interruption — resume reports the
        // journaled offset and accepts only from there.
        store.accept_chunk(&m, &frames[0].0, frames[0].1).unwrap();
        store.accept_chunk(&m, &frames[1].0, frames[1].1).unwrap();
        let progress = store.progress(&m).unwrap();
        assert_eq!(progress, vec![("a".to_string(), 120)]);
        let gap = store
            .accept_chunk(&m, &frames[4].0, frames[4].1)
            .unwrap_err();
        assert!(
            matches!(gap, TransferError::OutOfOrder { next_offset: 120 }),
            "a gap reports the resume offset, got {gap:?}"
        );
        // Re-declaring resumes the same session and reports progress.
        let resumed = store.declare(&m).unwrap();
        assert_eq!(resumed, vec![("a".to_string(), 120)]);
        for (chunk, bytes) in &frames[2..] {
            store.accept_chunk(&m, chunk, bytes).unwrap();
        }
        store.finalize(&m).unwrap();
    }

    #[test]
    fn a_replayed_chunk_is_idempotent_and_a_conflict_refuses() {
        let store = TransferStore::new(root("replay"));
        let body = b"xy".repeat(40);
        let m = manifest("t-1", 40, vec![item("a", "a.bin", &body)]);
        store.declare(&m).unwrap();
        let frames = item_chunks(&m, "a", &body);
        store.accept_chunk(&m, &frames[0].0, frames[0].1).unwrap();
        // Identical replay: nothing moves.
        let replayed = store.accept_chunk(&m, &frames[0].0, frames[0].1).unwrap();
        assert!(matches!(
            replayed,
            ChunkOutcome::Replayed { next_offset: 40 }
        ));
        // A different body at the occupied offset conflicts.
        let mut forged = frames[0].0.clone();
        let bad = vec![b'z'; 40];
        forged.chunk_digest = workloop_core::executor::chunk_body_digest(&bad);
        let err = store.accept_chunk(&m, &forged, &bad).unwrap_err();
        assert!(
            matches!(err, TransferError::ConflictingReplay { .. }),
            "a different digest at an occupied offset is conflicting_replay, got {err:?}"
        );
    }

    #[test]
    fn finalize_refuses_a_missing_or_corrupt_item() {
        let dir = root("missing");
        let store = TransferStore::new(dir.clone());
        let body = b"data".repeat(10);
        let m = manifest(
            "t-1",
            40,
            vec![item("a", "a.bin", &body), item("b", "b.bin", b"other")],
        );
        store.declare(&m).unwrap();
        for (chunk, bytes) in item_chunks(&m, "a", &body) {
            store.accept_chunk(&m, &chunk, bytes).unwrap();
        }
        let err = store.finalize(&m).unwrap_err();
        assert!(matches!(err, TransferError::Incomplete { .. }));
        // Whole staged bytes with no journal still read as 0 — a torn
        // file without its journal is never trusted.
        let staging = dir.join("staging/t-1");
        let staged = item_path(&staging, "b");
        std::fs::write(&staged, b"other").unwrap();
        let err = store.finalize(&m).unwrap_err();
        assert!(matches!(err, TransferError::Incomplete { .. }));
        // Forging the journal to claim completeness hits the whole-body
        // digest re-check instead.
        std::fs::write(&staged, b"WRONG").unwrap();
        std::fs::write(journal_path(&staging, "b"), 5u64.to_be_bytes()).unwrap();
        let err = store.finalize(&m).unwrap_err();
        assert!(matches!(err, TransferError::DigestMismatch { .. }));
    }

    /// A declared zero-byte item emits no chunk frame — `item_chunks`
    /// yields nothing and `accept_chunk` would refuse a 0-byte body —
    /// so finalize materializes the empty file directly: the declared
    /// set is whole without a single frame.
    #[test]
    fn a_zero_byte_item_finalizes_without_a_chunk() {
        let store = TransferStore::new(root("empty-item"));
        let m = manifest(
            "t-1",
            64,
            vec![
                item("empty", "empty.bin", b""),
                item("full", "full.bin", b"body"),
            ],
        );
        store.declare(&m).unwrap();
        assert!(
            item_chunks(&m, "empty", b"").is_empty(),
            "an empty item produces no frames"
        );
        for (chunk, bytes) in item_chunks(&m, "full", b"body") {
            store.accept_chunk(&m, &chunk, bytes).unwrap();
        }
        let done = store.finalize(&m).unwrap();
        assert_eq!(
            std::fs::read(item_path(&done, "empty")).unwrap(),
            b"",
            "the empty item materializes as an empty file"
        );
        assert_eq!(std::fs::read(item_path(&done, "full")).unwrap(), b"body");
    }

    #[test]
    fn a_redeclared_different_manifest_conflicts() {
        let store = TransferStore::new(root("redeclare"));
        let body = b"q".repeat(10);
        let m1 = manifest("t-1", 40, vec![item("a", "a.bin", &body)]);
        store.declare(&m1).unwrap();
        let m2 = manifest("t-1", 40, vec![item("a", "a.bin", b"different!")]);
        let err = store.declare(&m2).unwrap_err();
        assert!(matches!(err, TransferError::ConflictingReplay { .. }));
    }

    #[test]
    fn a_chunk_for_another_session_or_item_refuses() {
        let store = TransferStore::new(root("foreign"));
        let body = b"n".repeat(20);
        let m = manifest("t-1", 40, vec![item("a", "a.bin", &body)]);
        store.declare(&m).unwrap();
        let frames = item_chunks(&m, "a", &body);
        let mut foreign = frames[0].0.clone();
        foreign.transfer_id = "t-other".into();
        let err = store.accept_chunk(&m, &foreign, frames[0].1).unwrap_err();
        assert!(matches!(err, TransferError::Foreign { .. }));
        let mut unknown = frames[0].0.clone();
        unknown.item_id = "nope".into();
        let err = store.accept_chunk(&m, &unknown, frames[0].1).unwrap_err();
        assert!(matches!(err, TransferError::UnknownItem { .. }));
    }
}
