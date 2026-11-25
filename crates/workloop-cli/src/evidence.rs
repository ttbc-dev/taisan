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

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use super::envelope::{MESSAGE_BOUND, bounded};
use workloop_evidence_protocol::{
    ArtifactRequest, ArtifactResult, CommandKind, ErrorCode, ExportError, ExportRequest,
    MAX_RECORDS, ProtocolError, ProtocolVersion, QueryCommand, QueryEnvelope, QuerySource,
    QueryStatus, SchemaVersion, SourceAvailability,
};
use workloop_store::artifact_class::ArtifactClass;
use workloop_store::evidence;
use workloop_store::export_profile::{ExportProfile, ExportedFile, bundle_files, export_profiled};
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
            // The frozen envelope validator bounds reason.message at
            // 240 chars; a long store or decode error must truncate,
            // never overflow the contract surface.
            message: bounded(&message.into(), MESSAGE_BOUND),
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

/// The `evidence export` request surface.
pub struct ExportOptions {
    pub after: Option<String>,
    pub cursor: Option<String>,
    pub limit: u16,
    pub profile: String,
    pub include_classes: Vec<String>,
    pub output: Option<PathBuf>,
    pub manifest: Option<PathBuf>,
}

/// `taisan evidence export [--after C | --cursor C] --limit N
/// [--profile NAME] [--include-class CLASS]... [--output DIR]
/// [--manifest PATH]`.
pub fn export(store: &SqliteStore, actor_label: &str, options: ExportOptions) -> i32 {
    let ExportOptions {
        after,
        cursor,
        limit,
        profile: profile_name,
        include_classes: includes,
        output,
        manifest: manifest_path,
    } = options;
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
    let Some(mut profile) = ExportProfile::named(&profile_name) else {
        return usage(
            CommandKind::EvidenceExport,
            actor_label,
            &format!("unknown export profile '{profile_name}'"),
        );
    };
    for include in &includes {
        let Some(class) = ArtifactClass::parse(include) else {
            return usage(
                CommandKind::EvidenceExport,
                actor_label,
                &format!("unknown artifact class '{include}'"),
            );
        };
        profile.widen(class);
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
    let profiled = match export_profiled(&request, &snapshot, now(), &profile) {
        Ok(export) => export,
        Err(e) => return export_error(&e, cmd()),
    };
    let page = profiled.page.clone();
    if let Some(dir) = output {
        let files = match bundle_files(&profiled) {
            Ok(files) => files,
            Err(e) => {
                return error(
                    Some(cmd()),
                    ErrorCode::Store,
                    format!("cannot render the export bundle: {e}"),
                );
            }
        };
        if let Err(e) = write_bundle(&dir, &files) {
            return error(Some(cmd()), ErrorCode::Usage, e);
        }
    }
    if let Some(path) = manifest_path {
        let bytes = match serde_json::to_vec_pretty(&profiled.manifest) {
            Ok(bytes) => bytes,
            Err(e) => {
                return error(
                    Some(cmd()),
                    ErrorCode::Store,
                    format!("cannot render the export manifest: {e}"),
                );
            }
        };
        if let Err(e) = write_file(&path, &bytes) {
            return error(Some(cmd()), ErrorCode::Usage, e);
        }
    }
    emit(&envelope_for(
        Some(cmd()),
        Some(page.clone()),
        Some(page.snapshot_seq),
        None,
    ))
}

fn write_file(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent)
        .map_err(|e| format!("cannot create the export directory: {e}"))?;
    let name = path
        .file_name()
        .ok_or_else(|| "the export path has no file name".to_string())?;
    let tmp = parent.join(format!(
        ".{}.tmp-{}",
        name.to_string_lossy(),
        std::process::id()
    ));
    use std::io::Write;
    let result = (|| -> Result<(), String> {
        let mut file = std::fs::File::create_new(&tmp)
            .map_err(|e| format!("cannot create a temporary export file: {e}"))?;
        file.write_all(bytes)
            .and_then(|()| file.sync_all())
            .map_err(|e| format!("cannot write the export file: {e}"))?;
        drop(file);
        std::fs::rename(&tmp, path).map_err(|e| format!("cannot publish the export file: {e}"))
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

fn write_bundle(dir: &Path, files: &[ExportedFile]) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("cannot create the export directory: {e}"))?;
    // A manifest left by an earlier run must not survive a failed
    // rewrite and claim the new bundle is complete. `bundle_files`
    // emits the manifest last, so clearing it first makes the manifest
    // the single completion marker.
    let manifest = dir.join("manifest.json");
    if manifest.is_file() {
        std::fs::remove_file(&manifest)
            .map_err(|e| format!("cannot clear the previous export manifest: {e}"))?;
    }
    // A narrower re-run into the same directory must not leave content
    // files an earlier wider profile wrote. Only the bundle-managed
    // content subtrees are swept; `page.json` and `manifest.json` are
    // rewritten, and files outside the bundle's own layout are not ours
    // to delete.
    let keep: BTreeSet<PathBuf> = files.iter().map(|file| dir.join(&file.path)).collect();
    for subtree in ["artifacts", "previews"] {
        sweep_stale_files(&dir.join(subtree), &keep)?;
    }
    for file in files {
        write_file(&dir.join(&file.path), &file.bytes)?;
    }
    Ok(())
}

/// Remove files under `path` that the current bundle does not emit.
/// Bundle content lives in the flat `artifacts/` and `previews/`
/// subtrees; empty descendants are pruned so a narrower export leaves no
/// stale tree behind. Symlinks are unlinked, never followed.
fn sweep_stale_files(path: &Path, keep: &BTreeSet<PathBuf>) -> Result<(), String> {
    // The subtree root itself is checked without following it: a
    // symlinked `artifacts/` or `previews/` must be unlinked, never
    // descended — `read_dir` resolves the link and the sweep would
    // delete regular files under an attacker-controlled target.
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(format!("cannot inspect the export directory: {e}")),
    };
    if metadata.file_type().is_symlink() {
        return std::fs::remove_file(path)
            .map_err(|e| format!("cannot clear a stale export link: {e}"));
    }
    let entries = match std::fs::read_dir(path) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(format!("cannot inspect the export directory: {e}")),
    };
    for entry in entries {
        let entry = entry.map_err(|e| format!("cannot inspect the export directory: {e}"))?;
        let child = entry.path();
        let file_type = entry
            .file_type()
            .map_err(|e| format!("cannot inspect the export directory: {e}"))?;
        if file_type.is_dir() {
            sweep_stale_files(&child, keep)?;
            let _ = std::fs::remove_dir(&child);
        } else if !keep.contains(&child) {
            std::fs::remove_file(&child)
                .map_err(|e| format!("cannot clear a stale export file: {e}"))?;
        }
    }
    Ok(())
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
    profile_name: &str,
) -> i32 {
    let cmd = || command(CommandKind::EvidenceArtifact, actor_label);
    let Some(profile) = ExportProfile::named(profile_name) else {
        return usage(
            CommandKind::EvidenceArtifact,
            actor_label,
            &format!("unknown export profile '{profile_name}'"),
        );
    };
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
    let registered = match store.artifact_class(&id) {
        Ok(class) => class,
        Err(e) => return error(Some(cmd()), ErrorCode::Store, e.to_string()),
    };
    let class = workloop_store::artifact_class::classify_artifact(
        &id,
        Some(payload.as_bytes()),
        registered,
    );
    if profile.excludes(class) {
        return error(
            Some(cmd()),
            ErrorCode::ArtifactUnavailable,
            format!(
                "artifact class {class} is excluded by the {} export profile",
                profile.name()
            ),
        );
    }
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
