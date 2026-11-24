//! The endpoint's durable state: local receipt files under one state
//! root — no database server, broker, or second service. Every record
//! is canonical JSON written atomically (tmp + rename) and digested on
//! the way out so a torn or tampered record reads as corrupt, never as
//! absent. Receipts and acknowledgement tombstones are retained for
//! the installation's life; WIP bodies are retained until a matching
//! durable acknowledgement permits cleanup.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use workloop_core::domain::sha256_hex;
use workloop_core::executor::ExecutorLimits;
use workloop_exec::isolation::sanitize_key;

/// Records under the state root carry a self-check digest: the tagged
/// SHA-256 of the canonical body with the digest member's value
/// emptied — the same re-verify-on-read discipline the coordinator
/// store uses (#375).
fn record_digest(body: &serde_json::Value) -> String {
    let mut covered = body.clone();
    covered["record_digest"] = serde_json::Value::String(String::new());
    sha256_hex(
        "taisan.executor.endpoint-record.v1",
        workloop_core::domain::canonical_json(&covered).as_slice(),
    )
}

fn seal<T: Serialize>(value: &T) -> serde_json::Value {
    let mut body = serde_json::to_value(value).expect("record serializes");
    body["record_digest"] = serde_json::Value::String(record_digest(&body));
    body
}

fn verify(body: &serde_json::Value) -> Result<(), StateError> {
    let declared = body
        .get("record_digest")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    if declared.is_empty() || declared != record_digest(body) {
        return Err(StateError::Corrupt {
            detail: "record digest mismatch".into(),
        });
    }
    Ok(())
}

#[derive(Debug)]
pub enum StateError {
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    Corrupt {
        detail: String,
    },
}

impl std::fmt::Display for StateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io { path, source } => write!(f, "{}: {source}", path.display()),
            Self::Corrupt { detail } => write!(f, "corrupt: {detail}"),
        }
    }
}

impl std::error::Error for StateError {}

fn io(path: &Path) -> impl FnOnce(std::io::Error) -> StateError + '_ {
    move |source| StateError::Io {
        path: path.to_path_buf(),
        source,
    }
}

/// Write `bytes` to `path` atomically: same-directory tmp, fsync,
/// rename. A torn write can never read as a record.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), StateError> {
    let tmp = path.with_extension("tmp-partial");
    std::fs::write(&tmp, bytes).map_err(io(&tmp))?;
    let file = std::fs::File::open(&tmp).map_err(io(&tmp))?;
    file.sync_all().map_err(io(&tmp))?;
    std::fs::rename(&tmp, path).map_err(io(path))?;
    Ok(())
}

/// `write_atomic` for secret bodies: the tmp file is created owner-
/// only (mode 0600) and a pre-existing tmp is clamped before any
/// secret byte lands, so the content is never world-readable — and a
/// permission failure is an error, never silently ignored (contract
/// §7 requires mode 0600 for the staged runtime credential).
pub fn write_atomic_private(path: &Path, bytes: &[u8]) -> Result<(), StateError> {
    use std::io::Write as _;
    let tmp = path.with_extension("tmp-partial");
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&tmp).map_err(io(&tmp))?;
    // `mode` applies only at creation — clamp a pre-existing tmp
    // before any byte lands in it.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600)).map_err(io(&tmp))?;
    }
    file.write_all(bytes).map_err(io(&tmp))?;
    file.sync_all().map_err(io(&tmp))?;
    std::fs::rename(&tmp, path).map_err(io(path))?;
    Ok(())
}

fn write_record(path: &Path, value: &serde_json::Value) -> Result<(), StateError> {
    let bytes = workloop_core::domain::canonical_json(value);
    write_atomic(path, &bytes)
}

fn read_record(path: &Path) -> Result<Option<serde_json::Value>, StateError> {
    match std::fs::read(path) {
        Ok(bytes) => {
            let value: serde_json::Value =
                serde_json::from_slice(&bytes).map_err(|e| StateError::Corrupt {
                    detail: format!("{} is not JSON: {e}", path.display()),
                })?;
            verify(&value).map_err(|e| match e {
                StateError::Corrupt { detail } => StateError::Corrupt {
                    detail: format!("{}: {detail}", path.display()),
                },
                other => other,
            })?;
            Ok(Some(value))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(io(path)(e)),
    }
}

fn read_typed<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<Option<T>, StateError> {
    match read_record(path)? {
        Some(value) => {
            let mut covered = value.clone();
            if let Some(map) = covered.as_object_mut() {
                map.remove("record_digest");
            }
            serde_json::from_value(covered)
                .map(Some)
                .map_err(|e| StateError::Corrupt {
                    detail: format!("{} does not decode: {e}", path.display()),
                })
        }
        None => Ok(None),
    }
}

// ---------------------------------------------------------------------
// Service boot record: the executor incarnation.
// ---------------------------------------------------------------------

/// The durable service record. `incarnation_id` is a fresh,
/// unpredictable identifier minted at executor process start
/// (contract §2); it does not survive a restart, and receipts bound
/// to a previous incarnation keep naming it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServiceRecord {
    pub incarnation_id: String,
    pub started_at_unix_seconds: i64,
    /// Fresh nonce → `inc-<uuid>`; unpredictable per contract §2.
    pub boot_seq: u64,
}

// ---------------------------------------------------------------------
// Per-request records.
// ---------------------------------------------------------------------

/// The endpoint-side request record, written at `prepare`: the
/// immutable binding fields plus the launch configuration the start
/// path re-verifies. Serialized verbatim; `record_digest` is added by
/// `seal`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RequestRecord {
    pub installation_id: String,
    pub endpoint_id: String,
    pub endpoint_identity_digest: String,
    pub bound_incarnation_id: String,
    pub request_id: String,
    pub invocation_id: String,
    pub request_digest: String,
    /// Deterministic prepared identity the `prepared` receipt names.
    pub prepared_id: String,
    /// The canonical launch configuration bytes the coordinator
    /// committed — start re-digests and compares.
    pub launch_config_body: serde_json::Value,
    pub launch_config_digest: String,
    /// Digest over the sorted staged-item descriptor list.
    pub input_manifest_digest: String,
    /// `writable` | `read_only`.
    pub workspace_grant: String,
    pub work_id: String,
    /// Absolute coordinator deadline; enforcement compares against
    /// the measured conservative local cutoff.
    pub deadline_unix_seconds: i64,
    pub limits: ExecutorLimits,
    /// Milliseconds of stop grace before escalation (contract §8).
    pub stop_grace_ms: u64,
    /// Capabilities the launch requires the endpoint to enforce.
    pub required_capabilities: Vec<String>,
    /// Whether the Worker gets the staged runtime credential at
    /// `/taisan/runtime-auth`.
    pub runtime_auth_mount: bool,
    /// `prepared` | `started` | `stopped` | `collected` | `acknowledged`.
    pub state: String,
}

impl RequestRecord {
    /// The state values a durable record may carry; anything else is
    /// corrupt.
    pub fn state_is_known(&self) -> bool {
        matches!(
            self.state.as_str(),
            "prepared" | "started" | "stopped" | "collected" | "acknowledged" | "live_unknown"
        )
    }

    /// Whether the Invocation may still be executing remotely —
    /// occupies the single execution slot.
    pub fn is_live(&self) -> bool {
        matches!(self.state.as_str(), "started" | "live_unknown")
    }
}

/// One durable executor-side operation record (contract §4): the full
/// canonical command bytes and, once answered, the canonical receipt
/// bytes. Keyed by `(request_id, operation_id)`; replay requires the
/// identical command digest.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OpRecord {
    pub operation_id: String,
    pub operation_kind: String,
    /// Base64url canonical command bytes — stored verbatim for replay.
    pub command_body_b64: String,
    pub command_digest: String,
    /// Set once the receipt is recorded.
    #[serde(default)]
    pub receipt_body_b64: Option<String>,
    #[serde(default)]
    pub receipt_digest: Option<String>,
}

/// The durable start record: the spawned leader's `pid + lstart`
/// identity, its process group, the descendant snapshot taken at
/// start, and the opaque evidence reference receipts name.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProcessRecord {
    pub evidence_ref: String,
    /// Tagged digest over the recorded identity set — the
    /// `identity_digest` receipts carry.
    pub identity_digest: String,
    pub leader: workloop_core::domain::ProcessIdentity,
    /// Write-capable descendants captured at start and refreshed at
    /// stop intent.
    pub descendants: Vec<workloop_core::domain::ProcessIdentity>,
    /// Tagged digest over the snapshot — part of stop proof.
    pub snapshot_digest: String,
    /// `Some` once the leader is observed exited; the proven status.
    #[serde(default)]
    pub leader_exit_code: Option<Option<i32>>,
    /// The conservative local cutoff (unix ms) the bound deadline was
    /// converted to at start — endpoint-side enforcement compares
    /// against this, so skew consumes but never extends the budget.
    #[serde(default)]
    pub local_deadline_unix_ms: i64,
}

/// Durable stop intent (contract §4.3): persisted before any signal;
/// once it exists a delayed `start` for the Invocation is refused.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StopIntent {
    pub operation_id: String,
    pub recorded_at_unix_seconds: i64,
    /// `requested` | `proven`.
    pub state: String,
}

/// The durable stop proof (contract §4, §5): every recorded identity
/// inspected gone, bound to the incarnation that ran them. `manner`
/// is `signaled` (request_stop path) or `exit` (natural exit proved
/// by inspection).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StopProof {
    pub manner: String,
    pub leader_identity_digest: String,
    pub snapshot_digest: String,
    pub incarnation_id: String,
    pub proven_at_unix_seconds: i64,
    /// The operation whose receipt carries this proof (`observe` for
    /// natural exit, `request_stop` for signaled).
    pub proving_operation_id: String,
    /// Digest of this record — the `proof.proof_digest` member.
    pub proof_digest: String,
}

/// The bounded observation sequence for one Invocation: monotonically
/// increasing, durable across reconnects.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ObservationRecord {
    pub sequence: u64,
}

/// The durable collection record (contract §6): the items the
/// endpoint retained for acknowledgement, with content digests.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CollectionRecord {
    pub collection_id: String,
    pub collection_digest: String,
    /// Item descriptors `{item_id, bytes, content_digest, source}` —
    /// bodies stay under `out/` until acknowledgement.
    pub items: Vec<CollectionItemRecord>,
    pub collected_at_unix_seconds: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CollectionItemRecord {
    pub item_id: String,
    pub bytes: u64,
    pub content_digest: String,
    /// Where the body lives relative to the request dir
    /// (`out/report.json`, `out/handoff.json`, `objects.bundle`, …).
    pub source: String,
}

/// The acknowledgement tombstone: durable before any body cleanup; a
/// lost acknowledgement answer replays this record without
/// recollection.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AckTombstone {
    pub collection_id: String,
    pub collection_digest: String,
    pub acknowledged_at_unix_seconds: i64,
    /// `present` | `reclaimed` — bodies may be cleaned after this.
    pub bodies: String,
}

// ---------------------------------------------------------------------
// The state root.
// ---------------------------------------------------------------------

/// The endpoint's durable root. All mutation goes through
/// `write_record`/`write_atomic`; all reads re-verify digests.
#[derive(Debug, Clone)]
pub struct EndpointState {
    root: PathBuf,
}

impl EndpointState {
    pub fn open(root: PathBuf) -> Result<Self, StateError> {
        std::fs::create_dir_all(&root).map_err(io(&root))?;
        std::fs::create_dir_all(root.join("requests")).map_err(io(&root))?;
        Ok(Self { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Mint-and-persist a fresh incarnation for this service boot.
    /// The previous boot's record is replaced; receipts bound to it
    /// remain in their request records.
    pub fn boot_incarnation(&self, now_unix_seconds: i64) -> Result<ServiceRecord, StateError> {
        let record = ServiceRecord {
            incarnation_id: format!("inc-{}", uuid::Uuid::new_v4().simple()),
            started_at_unix_seconds: now_unix_seconds,
            boot_seq: now_unix_seconds.max(0) as u64,
        };
        write_record(&self.root.join("service.json"), &seal(&record))?;
        Ok(record)
    }

    /// The incarnation this boot of the service runs under. `None`
    /// only before `boot_incarnation` has ever run.
    pub fn incarnation(&self) -> Result<Option<ServiceRecord>, StateError> {
        read_typed(&self.root.join("service.json"))
    }

    /// The per-request directory key: human-traceable and
    /// collision-free.
    fn request_dir(&self, request_id: &str) -> PathBuf {
        let tag = &sha256_hex("taisan.executor.request-dir.v1", request_id.as_bytes())[..12];
        self.root
            .join("requests")
            .join(format!("{}-{tag}", sanitize_key(request_id)))
    }

    pub fn request_path(&self, request_id: &str) -> PathBuf {
        self.request_dir(request_id)
    }

    /// Enumerate every request record — reboot reconciliation reads
    /// this set, never the process table.
    pub fn requests(&self) -> Result<Vec<RequestRecord>, StateError> {
        let mut out = Vec::new();
        let dir = self.root.join("requests");
        if !dir.is_dir() {
            return Ok(out);
        }
        for entry in std::fs::read_dir(&dir).map_err(io(&dir))? {
            let entry = entry.map_err(io(&dir))?;
            let binding = entry.path().join("binding.json");
            if binding.is_file()
                && let Some(record) = read_typed::<RequestRecord>(&binding)?
            {
                out.push(record);
            }
        }
        Ok(out)
    }

    pub fn request(&self, request_id: &str) -> Result<Option<RequestRecord>, StateError> {
        read_typed(&self.request_dir(request_id).join("binding.json"))
    }

    pub fn put_request(&self, record: &RequestRecord) -> Result<(), StateError> {
        let dir = self.request_dir(&record.request_id);
        std::fs::create_dir_all(dir.join("ops")).map_err(io(&dir))?;
        write_record(&dir.join("binding.json"), &seal(record))
    }

    // --- operation ledger ------------------------------------------------

    /// The per-operation ledger filename: `sanitize_key` alone is
    /// lossy (`a:b` and `a-b` both become `a-b`), so the file name
    /// carries the same content tag the request directory does — a
    /// distinct operation id can never overwrite another's record.
    fn op_path(&self, request_id: &str, operation_id: &str) -> PathBuf {
        let tag = &sha256_hex("taisan.executor.operation-file.v1", operation_id.as_bytes())[..12];
        self.request_dir(request_id)
            .join("ops")
            .join(format!("{}-{tag}.json", sanitize_key(operation_id)))
    }

    pub fn operation(
        &self,
        request_id: &str,
        operation_id: &str,
    ) -> Result<Option<OpRecord>, StateError> {
        read_typed(&self.op_path(request_id, operation_id))
    }

    pub fn put_operation(&self, request_id: &str, op: &OpRecord) -> Result<(), StateError> {
        // The operation ledger may be committed before the request
        // binding exists — the prepare op lands first — so the ops
        // directory is created here, not just under `put_request`.
        let dir = self.request_dir(request_id).join("ops");
        std::fs::create_dir_all(&dir).map_err(io(&dir))?;
        write_record(&self.op_path(request_id, &op.operation_id), &seal(op))
    }

    /// Every recorded operation for the request — the auditable
    /// sequence the contract preserves (observation receipts are
    /// durable too).
    pub fn operations_for(&self, request_id: &str) -> Result<Vec<OpRecord>, StateError> {
        let mut out = Vec::new();
        let dir = self.request_dir(request_id).join("ops");
        if dir.is_dir() {
            for entry in std::fs::read_dir(&dir).map_err(io(&dir))? {
                let entry = entry.map_err(io(&dir))?;
                if let Some(op) = read_typed::<OpRecord>(&entry.path())? {
                    out.push(op);
                }
            }
        }
        out.sort_by(|a, b| a.operation_id.cmp(&b.operation_id));
        Ok(out)
    }

    // --- process / stop / observation records ----------------------------

    pub fn process(&self, request_id: &str) -> Result<Option<ProcessRecord>, StateError> {
        read_typed(&self.request_dir(request_id).join("process.json"))
    }

    pub fn put_process(&self, request_id: &str, record: &ProcessRecord) -> Result<(), StateError> {
        write_record(
            &self.request_dir(request_id).join("process.json"),
            &seal(record),
        )
    }

    pub fn stop_intent(&self, request_id: &str) -> Result<Option<StopIntent>, StateError> {
        read_typed(&self.request_dir(request_id).join("stop_intent.json"))
    }

    pub fn put_stop_intent(&self, request_id: &str, intent: &StopIntent) -> Result<(), StateError> {
        write_record(
            &self.request_dir(request_id).join("stop_intent.json"),
            &seal(intent),
        )
    }

    pub fn stop_proof(&self, request_id: &str) -> Result<Option<StopProof>, StateError> {
        read_typed(&self.request_dir(request_id).join("stop_proof.json"))
    }

    pub fn put_stop_proof(&self, request_id: &str, proof: &StopProof) -> Result<(), StateError> {
        write_record(
            &self.request_dir(request_id).join("stop_proof.json"),
            &seal(proof),
        )
    }

    pub fn observation(&self, request_id: &str) -> Result<ObservationRecord, StateError> {
        Ok(
            read_typed(&self.request_dir(request_id).join("observations.json"))?
                .unwrap_or(ObservationRecord { sequence: 0 }),
        )
    }

    pub fn put_observation(
        &self,
        request_id: &str,
        record: &ObservationRecord,
    ) -> Result<(), StateError> {
        write_record(
            &self.request_dir(request_id).join("observations.json"),
            &seal(record),
        )
    }

    // --- collection / acknowledgement ------------------------------------

    pub fn collection(&self, request_id: &str) -> Result<Option<CollectionRecord>, StateError> {
        read_typed(&self.request_dir(request_id).join("collection.json"))
    }

    pub fn put_collection(
        &self,
        request_id: &str,
        record: &CollectionRecord,
    ) -> Result<(), StateError> {
        write_record(
            &self.request_dir(request_id).join("collection.json"),
            &seal(record),
        )
    }

    pub fn acknowledgement(&self, request_id: &str) -> Result<Option<AckTombstone>, StateError> {
        read_typed(&self.request_dir(request_id).join("ack.json"))
    }

    pub fn put_acknowledgement(
        &self,
        request_id: &str,
        tombstone: &AckTombstone,
    ) -> Result<(), StateError> {
        write_record(
            &self.request_dir(request_id).join("ack.json"),
            &seal(tombstone),
        )
    }

    /// Remove the retained WIP bodies of an acknowledged Invocation —
    /// scoped to this request dir, keeping metadata (binding, ops,
    /// tombstone, records). Restartable: it re-runs cleanly because
    /// every removal is idempotent.
    pub fn reclaim_bodies(&self, request_id: &str) -> Result<(), StateError> {
        let dir = self.request_dir(request_id);
        for name in [
            "staging",
            "prepared",
            "workspace",
            "out",
            "objects.bundle",
            "runtime-auth",
        ] {
            let path = dir.join(name);
            if path.is_dir() {
                std::fs::remove_dir_all(&path).map_err(io(&path))?;
            } else if path.exists() {
                std::fs::remove_file(&path).map_err(io(&path))?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn operation_ids_that_sanitize_alike_never_share_a_ledger_file() {
        // `a:b` and `a-b` both sanitize to `a-b`; the digest tag keeps
        // their ledger records distinct so replay reads its own
        // receipt, never the colliding operation's.
        let dir = std::env::temp_dir().join(format!("state-test-{}", std::process::id()));
        let state = EndpointState::open(dir.clone()).expect("state");
        let record = |id: &str| OpRecord {
            operation_id: id.into(),
            operation_kind: "observe".into(),
            command_body_b64: String::new(),
            command_digest: format!("digest-{id}"),
            receipt_body_b64: None,
            receipt_digest: None,
        };
        state.put_operation("req-1", &record("a:b")).expect("put");
        state.put_operation("req-1", &record("a-b")).expect("put");
        assert_eq!(
            state
                .operation("req-1", "a:b")
                .expect("read")
                .expect("exists")
                .command_digest,
            "digest-a:b"
        );
        assert_eq!(
            state
                .operation("req-1", "a-b")
                .expect("read")
                .expect("exists")
                .command_digest,
            "digest-a-b"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn private_writes_are_owner_only_from_the_start() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("state-priv-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        let path = dir.join("runtime-auth");
        write_atomic_private(&path, b"secret").expect("write");
        assert_eq!(
            std::fs::metadata(&path).expect("meta").permissions().mode() & 0o777,
            0o600
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
