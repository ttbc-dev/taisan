//! `evidence export` / `evidence artifact` — the `rui-evidence/1`
//! query surface (proposal T3).
//!
//! Both commands reuse the existing JSON result envelope and exit
//! classes: one envelope on stdout, diagnostics on stderr, 0 ok /
//! 1 rejected / 2 usage / 3 config / 4 store. The wire body inside
//! `result` is the frozen contract in `workloop-evidence-protocol`;
//! `reason.code` names the frozen error vocabulary. The commands are
//! read-only: they replay recorded history and never invoke a worker,
//! refresh a host or mutate orchestration state.

use std::path::{Path, PathBuf};

use workloop_evidence_protocol::{
    ArtifactRequest, ArtifactResult, CommandKind, ErrorCode, ExportError, ExportRequest,
    MAX_RECORDS, ProtocolError, ProtocolVersion, QueryCommand, QueryEnvelope, QuerySource,
    QueryStatus, SchemaVersion, SourceAvailability,
};
use workloop_store::evidence;
use workloop_store::sqlite::SqliteStore;

const EXIT_OK: i32 = 0;
const EXIT_REJECTED: i32 = 1;
const EXIT_USAGE: i32 = 2;
const EXIT_CONFIG: i32 = 3;
const EXIT_STORE: i32 = 4;
const EXIT_SOURCE: i32 = 5;

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn command(kind: CommandKind, actor: &str) -> QueryCommand {
    // The frozen query identity carries no idempotency key — the
    // contract validator refuses one — so the CLI's own key is never
    // propagated onto this envelope.
    QueryCommand {
        id: None,
        kind,
        actor: actor.into(),
        idempotency_key: None,
    }
}

fn envelope_for<T: serde::Serialize>(
    cmd: Option<QueryCommand>,
    result: Option<T>,
    as_of: Option<u64>,
    reason: Option<ProtocolError>,
) -> QueryEnvelope<serde_json::Value> {
    let failed = reason.is_some();
    QueryEnvelope {
        schema_version: SchemaVersion::V1,
        status: if failed {
            QueryStatus::Error
        } else {
            QueryStatus::Accepted
        },
        command: cmd,
        work_id: None,
        execution_id: None,
        result: result.and_then(|r| serde_json::to_value(r).ok()),
        reason,
        evidence_refs: vec![],
        sources: if failed || as_of.is_none() {
            None
        } else {
            Some(vec![QuerySource {
                source: "event_log".into(),
                availability: SourceAvailability::Known,
                observed_unix_seconds: None,
            }])
        },
        as_of,
    }
}

/// Emit the one envelope for a result and hand back the exit class a
/// reason code maps to. stdout carries the object; a bounded one-line
/// diagnostic mirrors `reason` on stderr.
fn emit(env: &QueryEnvelope<serde_json::Value>) -> i32 {
    if let Some(r) = &env.reason {
        eprintln!(
            "taisan: {}: {}",
            serde_json::to_value(r.code)
                .ok()
                .and_then(|v| v.as_str().map(str::to_string))
                .unwrap_or_else(|| format!("{:?}", r.code)),
            r.message
        );
    }
    println!(
        "{}",
        serde_json::to_string_pretty(env).expect("output serializes")
    );
    // Exit classes per `docs/rui-evidence-v1.md` §Query failures — the
    // frozen mapping, not a producer choice.
    match env.reason.as_ref().map(|r| r.code) {
        None => EXIT_OK,
        Some(
            ErrorCode::Usage
            | ErrorCode::InvalidRequest
            | ErrorCode::InvalidCursor
            | ErrorCode::UnsupportedProtocol
            | ErrorCode::InstallationMismatch
            | ErrorCode::EpochMismatch,
        ) => EXIT_USAGE,
        Some(ErrorCode::Config) => EXIT_CONFIG,
        Some(ErrorCode::Store | ErrorCode::StoreUnavailable) => EXIT_STORE,
        Some(
            ErrorCode::SnapshotExpired
            | ErrorCode::CheckpointExpired
            | ErrorCode::RecordTooLarge
            | ErrorCode::ArtifactUnavailable
            | ErrorCode::ArtifactTooLarge
            | ErrorCode::DigestMismatch,
        ) => EXIT_SOURCE,
        Some(ErrorCode::Forbidden) => EXIT_REJECTED,
    }
}

fn error(cmd: Option<QueryCommand>, code: ErrorCode, message: impl Into<String>) -> i32 {
    emit(&envelope_for::<serde_json::Value>(
        cmd,
        None,
        None,
        Some(ProtocolError {
            code,
            message: message.into(),
        }),
    ))
}

fn usage(kind: CommandKind, actor: &str, message: &str) -> i32 {
    error(
        Some(command(kind, actor)),
        ErrorCode::Usage,
        message.to_string(),
    )
}

fn export_error(e: &ExportError, cmd: QueryCommand) -> i32 {
    error(Some(cmd), e.code, e.message.clone())
}

/// `taisan evidence export [--after C | --cursor C] --limit N`.
pub fn export(
    store: &SqliteStore,
    actor_label: &str,
    after: Option<String>,
    cursor: Option<String>,
    limit: u16,
) -> i32 {
    let cmd = || command(CommandKind::EvidenceExport, actor_label);
    if after.is_some() && cursor.is_some() {
        return usage(
            CommandKind::EvidenceExport,
            actor_label,
            "--after and --cursor are mutually exclusive",
        );
    }
    if limit == 0 || usize::from(limit) > MAX_RECORDS {
        return usage(
            CommandKind::EvidenceExport,
            actor_label,
            &format!("--limit must be within 1..={MAX_RECORDS}"),
        );
    }
    let request = ExportRequest {
        protocol_version: ProtocolVersion::V1,
        after,
        cursor,
        limit,
    };
    let snapshot = match store.evidence_history() {
        Ok(s) => s,
        Err(e) => {
            return error(Some(cmd()), ErrorCode::Store, e.to_string());
        }
    };
    match evidence::export_page(&request, &snapshot, now()) {
        Ok(page) => emit(&envelope_for(
            Some(cmd()),
            Some(page.clone()),
            Some(page.snapshot_seq),
            None,
        )),
        Err(e) => export_error(&e, cmd()),
    }
}

/// `taisan evidence artifact --id ID --sha256 DIGEST --output PATH`.
/// Exact retained bytes only: the digest is verified before publish,
/// the destination must not exist, and the write lands through a
/// temporary file so no partial or wrong bytes ever sit at `--output`.
pub fn artifact(
    store: &SqliteStore,
    actor_label: &str,
    id: String,
    sha256_digest: String,
    output: PathBuf,
) -> i32 {
    let cmd = || command(CommandKind::EvidenceArtifact, actor_label);
    if id.is_empty() {
        return usage(
            CommandKind::EvidenceArtifact,
            actor_label,
            "--id must not be empty",
        );
    }
    if sha256_digest.len() != 64
        || !sha256_digest
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
    {
        return usage(
            CommandKind::EvidenceArtifact,
            actor_label,
            "--sha256 must be 64 lowercase hex characters",
        );
    }
    if output.exists() {
        return usage(
            CommandKind::EvidenceArtifact,
            actor_label,
            "the --output destination already exists",
        );
    }
    let Some(seq) = evidence::artifact_seq(&id) else {
        return error(
            Some(cmd()),
            ErrorCode::ArtifactUnavailable,
            "artifact id is not in the retained namespace",
        );
    };
    let (installation_id, store_epoch) = match store.evidence_identity() {
        Ok(i) => i,
        Err(e) => return error(Some(cmd()), ErrorCode::Store, e.to_string()),
    };
    let payload = match store.evidence_event_payload(seq) {
        Ok(Some(p)) => p,
        Ok(None) => {
            return error(
                Some(cmd()),
                ErrorCode::ArtifactUnavailable,
                "artifact is not retained",
            );
        }
        Err(e) => return error(Some(cmd()), ErrorCode::Store, e.to_string()),
    };
    let request = ArtifactRequest {
        protocol_version: ProtocolVersion::V1,
        artifact_id: id,
        sha256: sha256_digest,
    };
    let (descriptor, bytes) = match evidence::artifact_from_payload(
        &request,
        &installation_id,
        &store_epoch,
        seq,
        &payload,
    ) {
        Ok(v) => v,
        Err(e) => return export_error(&e, cmd()),
    };
    if let Err(code) = publish(&output, &bytes) {
        return error(Some(cmd()), ErrorCode::Usage, code);
    }
    emit(&envelope_for(
        Some(cmd()),
        Some(ArtifactResult {
            protocol_version: ProtocolVersion::V1,
            descriptor,
            bytes_written: bytes.len() as u64,
        }),
        None,
        None,
    ))
}

/// Write `bytes` to `path` through a sibling temporary file, linking
/// it into place only after the full content is durably on disk. The
/// destination is created fresh — it was checked absent at the
/// boundary — and no partial content is ever left under the caller's
/// path.
fn publish(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let name = path
        .file_name()
        .ok_or_else(|| "the --output path has no file name".to_string())?;
    let tmp = parent.join(format!(
        ".{}.tmp-{}",
        name.to_string_lossy(),
        std::process::id()
    ));
    let write = || -> Result<(), String> {
        use std::io::Write;
        let mut f = std::fs::File::create_new(&tmp)
            .map_err(|e| format!("cannot create a temporary output file: {e}"))?;
        f.write_all(bytes)
            .and_then(|()| f.sync_all())
            .map_err(|e| format!("cannot write the artifact bytes: {e}"))?;
        drop(f);
        // link(2) fails with EEXIST when the destination name is
        // taken, so the no-overwrite guarantee is atomic — a
        // check-then-rename pair would let a concurrent creator slip
        // between the check and the rename.
        std::fs::hard_link(&tmp, path).map_err(|e| {
            if e.kind() == std::io::ErrorKind::AlreadyExists {
                "the --output destination already exists".to_string()
            } else {
                format!("cannot publish the artifact: {e}")
            }
        })?;
        // The destination already holds the complete bytes; dropping
        // the temp name is housekeeping.
        let _ = std::fs::remove_file(&tmp);
        Ok(())
    };
    let result = write();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}
