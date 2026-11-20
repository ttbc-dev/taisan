//! Content-addressed retained bodies (`docs/retention-policy.md` CAS
//! objects, `docs/v0.3.md` §4): the filesystem half of body retention.
//! Bytes live under a digest-addressed root and the durable
//! `body_catalog` row (through the [`BodyCatalog`] port) carries the
//! metadata and lifecycle state the retention sweep owns. This module
//! never deletes: reclamation is the sweep's job, and a body that left
//! `present` resolves as absent.
//!
//! Two identity shapes coexist:
//!
//! - **Content-addressed** — `retain` stores bytes at
//!   `bodies/<content_id>`; the catalogue `body_id` is the digest and
//!   the path the same digest's `bodies/` location, so the file
//!   location *is* the integrity check.
//! - **Lookup-addressed** — `retain_at` stores bytes at a deterministic
//!   catalogue path (issue snapshots resolve by the spec-bound
//!   `(issue, source_revision, content_digest)` identity) while
//!   `body_id` stays the content digest, so integrity verification is
//!   unchanged.
//!
//! Resolution is fail-closed at every step: an unregistered id, a
//! non-`present` state, a missing file, a size mismatch, or bytes whose
//! recomputed id disagrees are all distinct refusals the dispatch path
//! turns into typed `request.*` reasons — never a partial read and
//! never a fetch.

use std::path::{Path, PathBuf};

use workloop_core::domain::sha256_hex;
use workloop_core::store::{BodyCatalog, BodyCatalogEntry, NewCatalogBody, StoreError};

/// The CAS domain tag: `content_id` is `sha256_hex(BODY_CAS_TAG, bytes)`.
/// Content identity is deliberately type-agnostic here — the *wire*
/// digests on `WorkerRequestV2` body refs are tagged by each body's own
/// type and computed by the request builder, which knows the type.
const BODY_CAS_TAG: &str = "retained-body-cas-v1";

/// The retention category every dispatch-retained body registers
/// under — the closed set `docs/retention-policy.md` approves; the
/// body's own format travels in its envelope and lookup path, never in
/// the category.
pub const CAS_CATEGORY: &str = "cas";

/// The content address of one retained body.
pub fn content_id(bytes: &[u8]) -> String {
    sha256_hex(BODY_CAS_TAG, bytes)
}

/// A body resolved from the catalogue and verified against its CAS
/// identity: the exact retained bytes plus the catalogue metadata the
/// caller may need for size/state decisions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedBody {
    pub body_id: String,
    pub category: String,
    pub bytes: Vec<u8>,
}

/// Why a body operation failed. `Missing`/`Unavailable`/`Corrupt` are
/// dispatch-refusal material; `Io`/`Catalog` are infrastructure faults
/// the caller surfaces rather than converts into a request reason.
#[derive(Debug)]
pub enum BodyError {
    /// No catalogue row exists for the id or lookup path.
    Missing { detail: String },
    /// The row exists but the retention sweep moved it out of `present`
    /// (`reclaiming`/`reclaimed`/`unavailable`) or the file is gone.
    Unavailable { body_id: String, state: String },
    /// The file's bytes do not recompute to the catalogue identity —
    /// size mismatch or digest mismatch. Corrupt content is never
    /// dispatched.
    Corrupt { body_id: String, detail: String },
    /// The catalogue path escapes the bodies root, names the root
    /// itself, or is already bound to different content — a tampered
    /// row is refused before any filesystem cost.
    UnsafePath { detail: String },
    /// A filesystem or catalogue fault.
    Fault(String),
}

impl std::fmt::Display for BodyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Missing { detail } => write!(f, "body missing: {detail}"),
            Self::Unavailable { body_id, state } => {
                write!(f, "body {body_id}: catalogue state {state}")
            }
            Self::Corrupt { body_id, detail } => {
                write!(f, "body {body_id}: corrupt ({detail})")
            }
            Self::UnsafePath { detail } => write!(f, "unsafe body path: {detail}"),
            Self::Fault(detail) => write!(f, "body store fault: {detail}"),
        }
    }
}

impl std::error::Error for BodyError {}

impl From<StoreError> for BodyError {
    fn from(e: StoreError) -> Self {
        Self::Fault(e.to_string())
    }
}

/// fsync one just-written file so its bytes survive a crash before
/// the rename that publishes it.
fn sync_file(path: &Path) -> Result<(), BodyError> {
    std::fs::File::open(path)
        .and_then(|f| f.sync_all())
        .map_err(|e| BodyError::Fault(format!("sync {}: {e}", path.display())))
}

/// fsync a directory so a rename into it survives a crash: file data
/// without the directory entry is unreachable, and the entry without
/// file data is a corrupt body the next resolve refuses on.
pub(crate) fn fsync_dir(dir: &Path) -> Result<(), BodyError> {
    std::fs::File::open(dir)
        .and_then(|f| f.sync_all())
        .map_err(|e| BodyError::Fault(format!("sync dir {}: {e}", dir.display())))
}

/// The installation state root: catalogue paths resolve under it —
/// the same rule the retention sweep enforces — so a managed body
/// records a `bodies/…` path and its file lives at
/// `<state_root>/bodies/…`. One instance per installation state root.
#[derive(Debug, Clone)]
pub struct BodyStore {
    root: PathBuf,
}

impl BodyStore {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The managed file location for a catalogue `path` entry. The
    /// recorded path is installation-relative: no `..`/root components,
    /// resolved inside the state root — the same rule the retention
    /// sweep enforces, so a row the sweep would refuse is never read
    /// either.
    fn managed(&self, recorded: &str) -> Result<PathBuf, BodyError> {
        use std::path::Component;
        let raw = Path::new(recorded);
        if recorded.is_empty() {
            return Err(BodyError::UnsafePath {
                detail: "empty path".into(),
            });
        }
        if raw.components().any(|c| {
            matches!(
                c,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        }) {
            return Err(BodyError::UnsafePath {
                detail: format!("path {recorded:?} escapes the bodies root"),
            });
        }
        let candidate = self.root.join(raw);
        if candidate == self.root {
            return Err(BodyError::UnsafePath {
                detail: "path names the bodies root itself".into(),
            });
        }
        Ok(candidate)
    }

    /// Write `bytes` at `path` under the bodies root, then register the
    /// catalogue row binding `body_id = content_id(bytes)` to it.
    ///
    /// Order is file-then-catalog: a crash between them leaves an
    /// unregistered file the sweep's unknown-file scan reclaims, never
    /// a catalogue row pointing at nothing. An existing file is verified
    /// against the supplied bytes before reuse — different bytes at one
    /// path is a fault, never silently overwritten, because the path is
    /// supposed to be identity-derived.
    fn write_at(
        &self,
        catalog: &dyn BodyCatalog,
        category: &str,
        path: &str,
        bytes: &[u8],
        now_unix_seconds: i64,
        eligible_after_unix_seconds: Option<i64>,
    ) -> Result<String, BodyError> {
        let id = content_id(bytes);
        let file = self.managed(path)?;
        // A lookup path is single-tenant: if the catalogue already binds
        // this `(category, path)` to different content, the deterministic
        // identity collided or the row was tampered with — refuse before
        // any filesystem cost rather than leave bytes at a path another
        // body's row resolves through.
        if let Some(existing) = catalog.body_entry_at(category, path)?
            && existing.body_id != id
        {
            return Err(BodyError::UnsafePath {
                detail: format!(
                    "catalogue path {path:?} is already bound to body {}",
                    existing.body_id
                ),
            });
        }
        if let Some(parent) = file.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                BodyError::Fault(format!("create bodies dir {}: {e}", parent.display()))
            })?;
        }
        match std::fs::read(&file) {
            Ok(existing) if existing == bytes => {}
            Ok(_) => {
                return Err(BodyError::Fault(format!(
                    "body file {} exists with different bytes under its identity path",
                    file.display()
                )));
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                // Write-then-rename so a concurrent reader never sees a
                // partial body; a same-bytes racer renames over an
                // identical file harmlessly. The file is fsynced
                // before the rename and the directory after it, so a
                // crash cannot leave the catalogue pointing at bytes
                // the disk never saw.
                let tmp = file.with_extension("tmp");
                std::fs::write(&tmp, bytes)
                    .map_err(|e| BodyError::Fault(format!("write {}: {e}", tmp.display())))?;
                sync_file(&tmp)?;
                if let Err(e) = std::fs::rename(&tmp, &file) {
                    let _ = std::fs::remove_file(&tmp);
                    // A same-content racer may have won the rename;
                    // verify before reporting the fault.
                    match std::fs::read(&file) {
                        Ok(existing) if existing == bytes => {}
                        _ => {
                            return Err(BodyError::Fault(format!(
                                "rename {}: {e}",
                                file.display()
                            )));
                        }
                    }
                }
            }
            Err(e) => {
                return Err(BodyError::Fault(format!("read {}: {e}", file.display())));
            }
        }
        // The rename is only durable once the containing directory is
        // synced — an existing-file reuse needed no rename, but the
        // sync is harmless and keeps the publish path uniform.
        if let Some(parent) = file.parent() {
            fsync_dir(parent)?;
        }
        catalog.register_body(&NewCatalogBody {
            body_id: id.clone(),
            category: category.to_string(),
            path: path.to_string(),
            bytes: bytes.len() as u64,
            registered_unix_seconds: now_unix_seconds,
            eligible_after_unix_seconds,
            pinned: false,
        })?;
        Ok(id)
    }

    /// Retain `bytes` under their content address — the file lives at
    /// `bodies/<content_id>` and re-retaining identical bytes is a
    /// replay. `eligible_after_unix_seconds` is the sweep horizon the
    /// caller assigns (the retention category's age limit from now);
    /// `None` retains the body indefinitely. Re-registration extends a
    /// bounded horizon, never shortens it.
    pub fn retain(
        &self,
        catalog: &dyn BodyCatalog,
        category: &str,
        bytes: &[u8],
        now_unix_seconds: i64,
        eligible_after_unix_seconds: Option<i64>,
    ) -> Result<String, BodyError> {
        let id = content_id(bytes);
        // The same content may already be catalogued under another
        // logical path — e.g. a materialized input a worker echoes
        // into its own report. That is an alias of the same content
        // address, not a conflict: the bytes resolve by `body_id`
        // identically either way, so re-observe the existing row to
        // extend its eligibility horizon rather than force a second
        // binding that must fail. A byte-count mismatch under one
        // content address is a corrupt catalogue row and stays a
        // fault, and a non-`present` row keeps the normal path —
        // re-writing is the only way the bytes come back.
        if let Some(existing) = catalog.body_entry(&id)?
            && existing.state == "present"
        {
            if existing.bytes != bytes.len() as u64 {
                return Err(BodyError::Corrupt {
                    body_id: id,
                    detail: format!(
                        "catalogue records {} bytes under the content identity, body holds {}",
                        existing.bytes,
                        bytes.len()
                    ),
                });
            }
            catalog.register_body(&NewCatalogBody {
                body_id: id.clone(),
                category: existing.category,
                path: existing.path,
                bytes: existing.bytes,
                registered_unix_seconds: now_unix_seconds,
                eligible_after_unix_seconds,
                pinned: existing.pinned,
            })?;
            return Ok(id);
        }
        self.write_at(
            catalog,
            category,
            &format!("bodies/{id}"),
            bytes,
            now_unix_seconds,
            eligible_after_unix_seconds,
        )
    }

    /// Retain `bytes` at a deterministic installation-relative lookup
    /// `path` (e.g. an issue snapshot's spec-bound `bodies/…` identity
    /// path). The catalogue `body_id`
    /// remains the content digest, so [`Self::resolve`] verifies the
    /// bytes identically; [`Self::resolve_at`] finds the row by path.
    /// `eligible_after_unix_seconds` follows [`Self::retain`].
    pub fn retain_at(
        &self,
        catalog: &dyn BodyCatalog,
        category: &str,
        path: &str,
        bytes: &[u8],
        now_unix_seconds: i64,
        eligible_after_unix_seconds: Option<i64>,
    ) -> Result<String, BodyError> {
        self.write_at(
            catalog,
            category,
            path,
            bytes,
            now_unix_seconds,
            eligible_after_unix_seconds,
        )
    }

    /// Resolve the catalogue entry to verified bytes. Every failure is
    /// typed so the caller can refuse dispatch with the retained
    /// reason; `Ok(None)` means no row exists at all.
    fn resolve_entry(&self, entry: BodyCatalogEntry) -> Result<Option<ResolvedBody>, BodyError> {
        if entry.state != "present" {
            return Err(BodyError::Unavailable {
                body_id: entry.body_id.clone(),
                state: entry.state,
            });
        }
        let file = self.managed(&entry.path)?;
        let bytes = match std::fs::read(&file) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(BodyError::Unavailable {
                    body_id: entry.body_id.clone(),
                    state: "missing-file".into(),
                });
            }
            Err(e) => {
                return Err(BodyError::Fault(format!("read {}: {e}", file.display())));
            }
        };
        if bytes.len() as u64 != entry.bytes {
            return Err(BodyError::Corrupt {
                body_id: entry.body_id.clone(),
                detail: format!(
                    "catalog records {} bytes, file holds {}",
                    entry.bytes,
                    bytes.len()
                ),
            });
        }
        if content_id(&bytes) != entry.body_id {
            return Err(BodyError::Corrupt {
                body_id: entry.body_id.clone(),
                detail: "bytes do not recompute to the catalogue identity".into(),
            });
        }
        Ok(Some(ResolvedBody {
            body_id: entry.body_id,
            category: entry.category,
            bytes,
        }))
    }

    /// Resolve one body by content address.
    pub fn resolve(
        &self,
        catalog: &dyn BodyCatalog,
        body_id: &str,
    ) -> Result<Option<ResolvedBody>, BodyError> {
        let Some(entry) = catalog.body_entry(body_id)? else {
            return Ok(None);
        };
        self.resolve_entry(entry)
    }

    /// Resolve one body by its deterministic `(category, path)` lookup —
    /// the issue-snapshot dispatch path. `Ok(None)` means nothing is
    /// retained at that identity.
    pub fn resolve_at(
        &self,
        catalog: &dyn BodyCatalog,
        category: &str,
        path: &str,
    ) -> Result<Option<ResolvedBody>, BodyError> {
        let Some(entry) = catalog.body_entry_at(category, path)? else {
            return Ok(None);
        };
        self.resolve_entry(entry)
    }

    /// Materialize one resolved body as a read-only file inside a
    /// per-Invocation input directory. The file is written, its bytes
    /// re-verified against the resolved body, then chmod'd read-only —
    /// the Worker sees immutable input it cannot rewrite, and the
    /// directory lives outside the managed source workspace so it can
    /// never enter the delivered diff.
    pub fn materialize_input(
        &self,
        input_dir: &Path,
        name: &str,
        resolved: &ResolvedBody,
    ) -> Result<PathBuf, BodyError> {
        use std::os::unix::fs::PermissionsExt;
        // `name` is a single path component chosen by the caller —
        // refuse anything that could traverse.
        if name.is_empty()
            || name == "."
            || name == ".."
            || name.contains('/')
            || name.contains('\\')
        {
            return Err(BodyError::UnsafePath {
                detail: format!("input name {name:?} is not a single component"),
            });
        }
        std::fs::create_dir_all(input_dir).map_err(|e| {
            BodyError::Fault(format!("create input dir {}: {e}", input_dir.display()))
        })?;
        let dest = input_dir.join(name);
        let tmp = dest.with_extension("tmp");
        std::fs::write(&tmp, &resolved.bytes)
            .map_err(|e| BodyError::Fault(format!("write input {}: {e}", tmp.display())))?;
        // Re-verify what was written before it becomes visible: the
        // materialized bytes must be exactly the resolved body.
        let written = std::fs::read(&tmp)
            .map_err(|e| BodyError::Fault(format!("read input {}: {e}", tmp.display())))?;
        if written != resolved.bytes {
            let _ = std::fs::remove_file(&tmp);
            return Err(BodyError::Corrupt {
                body_id: resolved.body_id.clone(),
                detail: "materialized bytes differ from the resolved body".into(),
            });
        }
        sync_file(&tmp)?;
        std::fs::rename(&tmp, &dest).map_err(|e| {
            let _ = std::fs::remove_file(&tmp);
            BodyError::Fault(format!("rename input {}: {e}", dest.display()))
        })?;
        let mut perms = std::fs::metadata(&dest)
            .map_err(|e| BodyError::Fault(format!("stat {}: {e}", dest.display())))?
            .permissions();
        perms.set_mode(0o444);
        std::fs::set_permissions(&dest, perms)
            .map_err(|e| BodyError::Fault(format!("chmod input {}: {e}", dest.display())))?;
        if let Some(parent) = dest.parent() {
            fsync_dir(parent)?;
        }
        Ok(dest)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex;

    /// `(category, path, bytes, state, pinned)` — the catalogue row
    /// shape the in-memory fake stores.
    type MemRow = (String, String, u64, String, bool);

    /// In-memory catalogue for unit tests: the same contract the SQLite
    /// implementation honors — replay on identical metadata, conflict
    /// on disagreement, `present` until pinned-state changes.
    #[derive(Default)]
    struct MemCatalog {
        rows: Mutex<HashMap<String, MemRow>>,
        by_path: Mutex<HashMap<(String, String), String>>,
    }

    impl BodyCatalog for MemCatalog {
        fn body_entry(&self, body_id: &str) -> Result<Option<BodyCatalogEntry>, StoreError> {
            Ok(self.rows.lock().unwrap().get(body_id).map(
                |(category, path, bytes, state, pinned)| BodyCatalogEntry {
                    body_id: body_id.to_string(),
                    category: category.clone(),
                    path: path.clone(),
                    bytes: *bytes,
                    state: state.clone(),
                    pinned: *pinned,
                },
            ))
        }
        fn body_entry_at(
            &self,
            category: &str,
            path: &str,
        ) -> Result<Option<BodyCatalogEntry>, StoreError> {
            let id = self
                .by_path
                .lock()
                .unwrap()
                .get(&(category.to_string(), path.to_string()))
                .cloned();
            match id {
                Some(id) => self.body_entry(&id),
                None => Ok(None),
            }
        }
        fn register_body(&self, body: &NewCatalogBody) -> Result<(), StoreError> {
            let mut rows = self.rows.lock().unwrap();
            match rows.get(&body.body_id) {
                Some((category, path, bytes, _, _))
                    if *category == body.category && *path == body.path && *bytes == body.bytes =>
                {
                    Ok(())
                }
                Some(_) => Err(StoreError::ConstraintConflict {
                    detail: "conflicting body metadata".into(),
                }),
                None => {
                    rows.insert(
                        body.body_id.clone(),
                        (
                            body.category.clone(),
                            body.path.clone(),
                            body.bytes,
                            "present".to_string(),
                            body.pinned,
                        ),
                    );
                    self.by_path.lock().unwrap().insert(
                        (body.category.clone(), body.path.clone()),
                        body.body_id.clone(),
                    );
                    Ok(())
                }
            }
        }
        fn set_body_pin(
            &self,
            body_id: &str,
            _dependency: &str,
            pinned: bool,
        ) -> Result<(), StoreError> {
            if let Some(row) = self.rows.lock().unwrap().get_mut(body_id) {
                row.4 = pinned;
            }
            Ok(())
        }
    }

    fn tmp(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("workloop-bodies-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn retain_and_resolve_round_trips_verified_bytes() {
        let root = tmp("roundtrip");
        let store = BodyStore::new(root.clone());
        let catalog = MemCatalog::default();
        let bytes = b"{\"hello\":\"world\"}".to_vec();
        let id = store
            .retain(&catalog, CAS_CATEGORY, &bytes, 100, None)
            .expect("retain");
        assert_eq!(id, content_id(&bytes));
        let resolved = store
            .resolve(&catalog, &id)
            .expect("resolve")
            .expect("present");
        assert_eq!(resolved.bytes, bytes);
        assert_eq!(resolved.category, CAS_CATEGORY);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn retain_at_resolves_by_lookup_path_with_content_integrity() {
        let root = tmp("lookup");
        let store = BodyStore::new(root.clone());
        let catalog = MemCatalog::default();
        let bytes = b"snapshot-envelope".to_vec();
        let id = store
            .retain_at(
                &catalog,
                CAS_CATEGORY,
                "bodies/issue-snapshot/abc",
                &bytes,
                100,
                None,
            )
            .expect("retain_at");
        assert_eq!(id, content_id(&bytes));
        // Path lookup finds the same verified body.
        let resolved = store
            .resolve_at(&catalog, CAS_CATEGORY, "bodies/issue-snapshot/abc")
            .expect("resolve_at")
            .expect("present");
        assert_eq!(resolved.body_id, id);
        assert_eq!(resolved.bytes, bytes);
        // The same bytes under a second lookup path would give one
        // content address two locations — the catalogue refuses the
        // ambiguity rather than silently dual-registering.
        assert!(
            store
                .retain_at(
                    &catalog,
                    CAS_CATEGORY,
                    "bodies/issue-snapshot/def",
                    &bytes,
                    100,
                    None
                )
                .is_err()
        );
        // Binding the same path to different content is refused.
        assert!(matches!(
            store.retain_at(
                &catalog,
                CAS_CATEGORY,
                "bodies/issue-snapshot/abc",
                b"other",
                100,
                None
            ),
            Err(BodyError::UnsafePath { .. }) | Err(BodyError::Fault(_))
        ));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn retain_aliases_content_already_registered_under_another_path() {
        let root = tmp("alias");
        let store = BodyStore::new(root.clone());
        let catalog = MemCatalog::default();
        let bytes = b"goal-contents-a-worker-can-echo".to_vec();
        // The materialized input owns the first catalogue binding for
        // this content — a different category and a lookup path.
        let id = store
            .retain_at(&catalog, "snapshot", "inputs/goal.md", &bytes, 100, None)
            .expect("input retain");
        // Worker-controlled evidence holding the same bytes is an
        // alias of the same content address — a re-observation, not a
        // conflicting registration that would fault the finish path.
        let aliased = store
            .retain(&catalog, CAS_CATEGORY, &bytes, 200, None)
            .expect("evidence retain aliases the registered body");
        assert_eq!(aliased, id);
        let resolved = store
            .resolve(&catalog, &id)
            .expect("resolve")
            .expect("present");
        assert_eq!(resolved.bytes, bytes);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn write_at_refuses_a_bound_lookup_path_before_touching_the_file() {
        let root = tmp("bound-refuse");
        let store = BodyStore::new(root.clone());
        let catalog = MemCatalog::default();
        let bytes = b"snapshot-envelope".to_vec();
        store
            .retain_at(
                &catalog,
                CAS_CATEGORY,
                "bodies/issue-snapshot/abc",
                &bytes,
                100,
                None,
            )
            .expect("retain_at");
        // The row survives but its file is gone — the state a sweep
        // leaves behind. Writing different bytes here before the
        // single-tenant check would leave them at a path another
        // body's row resolves through.
        let file = root.join("bodies/issue-snapshot/abc");
        std::fs::remove_file(&file).unwrap();
        assert!(matches!(
            store.retain_at(
                &catalog,
                CAS_CATEGORY,
                "bodies/issue-snapshot/abc",
                b"other",
                100,
                None
            ),
            Err(BodyError::UnsafePath { .. })
        ));
        assert!(
            !file.exists(),
            "the refused write left no bytes at the bound path"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn resolve_reports_missing_and_unavailable_distinctly() {
        let root = tmp("states");
        let store = BodyStore::new(root.clone());
        let catalog = MemCatalog::default();
        assert!(matches!(store.resolve(&catalog, "nope"), Ok(None)));
        let bytes = b"data".to_vec();
        let id = store
            .retain(&catalog, CAS_CATEGORY, &bytes, 100, None)
            .unwrap();
        // Simulate reclamation: the row leaves `present`.
        catalog.rows.lock().unwrap().get_mut(&id).unwrap().3 = "reclaimed".to_string();
        assert!(matches!(
            store.resolve(&catalog, &id),
            Err(BodyError::Unavailable { .. })
        ));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn resolve_detects_corrupt_bytes() {
        let root = tmp("corrupt");
        let store = BodyStore::new(root.clone());
        let catalog = MemCatalog::default();
        let id = store
            .retain(&catalog, CAS_CATEGORY, b"good", 100, None)
            .unwrap();
        std::fs::write(root.join("bodies").join(&id), b"tampered").unwrap();
        assert!(matches!(
            store.resolve(&catalog, &id),
            Err(BodyError::Corrupt { .. }) | Err(BodyError::Fault(_))
        ));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn materialize_input_writes_read_only_verified_file() {
        use std::os::unix::fs::PermissionsExt;
        let root = tmp("materialize");
        let store = BodyStore::new(root.clone());
        let catalog = MemCatalog::default();
        let bytes = b"input-body".to_vec();
        let id = store
            .retain(&catalog, CAS_CATEGORY, &bytes, 100, None)
            .unwrap();
        let resolved = store.resolve(&catalog, &id).unwrap().unwrap();
        let dir = root.join("inv-1").join("input");
        let path = store
            .materialize_input(&dir, "goal.md", &resolved)
            .expect("materialize");
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o222, 0, "input must be read-only");
        assert!(matches!(
            store.materialize_input(&dir, "../escape", &resolved),
            Err(BodyError::UnsafePath { .. })
        ));
        let _ = std::fs::remove_dir_all(&root);
    }
}
