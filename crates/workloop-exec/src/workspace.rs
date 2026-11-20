use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use workloop_core::domain::*;
use workloop_core::identity::RepositoryRef;
use workloop_core::ports::WorkerEnv;
use workloop_core::store::{Store, StoreError, TxView};
use workloop_core::worker_v2::{TaskBody, WorkerRequestV2};

use crate::bounded::{self, ExchangeBounds, ExchangeError};
use crate::handoff::{self, HandoffLayout};
use crate::isolation;

#[derive(Debug, Clone)]
pub struct RepositorySource {
    pub repository: RepositoryRef,
    pub path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedWorkspace {
    pub path: PathBuf,
    pub branch: String,
    pub base: String,
    pub head: String,
    pub request_digest: String,
}

#[derive(Debug)]
pub enum PreparationError {
    Store(StoreError),
    InvalidRequest { detail: String },
    RepositoryMismatch { detail: String },
    MissingBase { detail: String },
    InvalidPredecessor { detail: String },
    ReservationConflict { detail: String },
    ActiveWriter { detail: String },
    WorkspaceConflict { detail: String },
    Io(std::io::Error),
    Git { detail: String },
}

impl std::fmt::Display for PreparationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Store(e) => write!(f, "{e}"),
            Self::InvalidRequest { detail } => write!(f, "invalid request: {detail}"),
            Self::RepositoryMismatch { detail } => {
                write!(f, "repository mismatch: {detail}")
            }
            Self::MissingBase { detail } => write!(f, "missing base: {detail}"),
            Self::InvalidPredecessor { detail } => {
                write!(f, "invalid predecessor: {detail}")
            }
            Self::ReservationConflict { detail } => {
                write!(f, "reservation conflict: {detail}")
            }
            Self::ActiveWriter { detail } => write!(f, "active writer: {detail}"),
            Self::WorkspaceConflict { detail } => {
                write!(f, "workspace conflict: {detail}")
            }
            Self::Io(e) => write!(f, "{e}"),
            Self::Git { detail } => write!(f, "git failure: {detail}"),
        }
    }
}

impl std::error::Error for PreparationError {}

impl From<StoreError> for PreparationError {
    fn from(e: StoreError) -> Self {
        Self::Store(e)
    }
}

impl From<std::io::Error> for PreparationError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

const GIT_TIMEOUT: Duration = Duration::from_secs(120);

const GIT_BOUNDS: ExchangeBounds = ExchangeBounds {
    max_stdin: 0,
    max_stdout: 4 * 1024 * 1024,
    max_stderr: 16 * 1024,
};

fn git_env() -> WorkerEnv {
    WorkerEnv {
        vars: vec![
            ("PATH".into(), "/usr/bin:/bin".into()),
            ("GIT_CONFIG_NOSYSTEM".into(), "1".into()),
            ("GIT_CONFIG_GLOBAL".into(), "/dev/null".into()),
            ("GIT_TERMINAL_PROMPT".into(), "0".into()),
            ("GIT_ALLOW_PROTOCOL".into(), "file".into()),
            ("GIT_NO_REPLACE_OBJECTS".into(), "1".into()),
            ("GIT_CONFIG_COUNT".into(), "2".into()),
            ("GIT_CONFIG_KEY_0".into(), "credential.helper".into()),
            ("GIT_CONFIG_VALUE_0".into(), String::new()),
            (
                "GIT_CONFIG_KEY_1".into(),
                "uploadpack.allowAnySHA1InWant".into(),
            ),
            ("GIT_CONFIG_VALUE_1".into(), "true".into()),
        ],
    }
}

const GIT_CONFIG_ARGS: &[&str] = &[
    "core.hooksPath=/dev/null",
    "core.fsmonitor=false",
    "credential.helper=",
    "protocol.allow=never",
    "protocol.file.allow=always",
    "core.alternateRefsCommand=/bin/false",
];

fn bounded_run(
    argv: &[String],
    cwd: &Path,
    timeout: Duration,
) -> Result<(String, Option<i32>), PreparationError> {
    let detail = |reason: &str| PreparationError::Git {
        detail: format!("git invocation: {reason}"),
    };
    match bounded::exchange(argv, cwd, &git_env(), &[], &GIT_BOUNDS, timeout) {
        Ok(out) => Ok((
            String::from_utf8_lossy(&out.stdout).trim().to_string(),
            out.exit_code,
        )),
        Err(ExchangeError::Exit { exit_code }) => Ok((String::new(), exit_code)),
        Err(ExchangeError::Launch { .. }) => Err(detail("could not launch")),
        Err(ExchangeError::Deadline { .. }) => Err(detail("timed out")),
        Err(ExchangeError::OutputOverflow) => Err(detail("output overflow")),
        Err(ExchangeError::StderrOverflow) => Err(detail("stderr overflow")),
        Err(ExchangeError::Unknown { .. }) => Err(detail("uncertain outcome")),
    }
}

fn git_run(dir: &Path, args: &[&str]) -> Result<(String, Option<i32>), PreparationError> {
    let mut argv: Vec<String> = vec![
        "git".into(),
        "-C".into(),
        dir.to_string_lossy().into_owned(),
    ];
    for kv in GIT_CONFIG_ARGS {
        argv.push("-c".into());
        argv.push((*kv).into());
    }
    argv.extend(args.iter().map(|a| (*a).to_string()));
    bounded_run(&argv, dir, GIT_TIMEOUT)
}

fn git(dir: &Path, args: &[&str]) -> Result<String, PreparationError> {
    let (stdout, code) = git_run(dir, args)?;
    if code != Some(0) {
        return Err(PreparationError::Git {
            detail: format!("git {} failed", args.first().copied().unwrap_or("")),
        });
    }
    Ok(stdout)
}

fn is_ancestor(dir: &Path, base: &str, produced: &str) -> Result<bool, PreparationError> {
    let (_, code) = git_run(dir, &["merge-base", "--is-ancestor", base, produced])?;
    match code {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => Err(PreparationError::Git {
            detail: "merge-base --is-ancestor failed".into(),
        }),
    }
}

/// The source repository's current `HEAD` commit — the base a fresh
/// Coding workspace pins at dispatch-materialization time. Read under
/// the same scrubbed git environment every other supervisor-side git
/// call uses; the result is a commit id the durable request body then
/// freezes, never a live re-read at launch.
pub(crate) fn git_head(dir: &Path) -> Result<String, PreparationError> {
    git(dir, &["rev-parse", "HEAD"])
}

fn is_hex_sha1(s: &str) -> bool {
    s.len() == 40
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn valid_component(s: &str) -> bool {
    !s.is_empty()
        && !s.starts_with('-')
        && !s.starts_with('.')
        && !s.ends_with(".lock")
        && !s.contains("..")
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
}

fn valid_ref_components(s: &str) -> bool {
    !s.is_empty() && !s.starts_with('-') && s.split('/').all(valid_component)
}

fn check_ref(dir: &Path, reference: &str) -> Result<(), PreparationError> {
    let (_, code) = git_run(dir, &["check-ref-format", reference])?;
    if code == Some(0) {
        Ok(())
    } else {
        Err(PreparationError::InvalidRequest {
            detail: "identifier does not form a valid git ref".into(),
        })
    }
}

fn control_area(workspace_root: &Path) -> PathBuf {
    workspace_root.join(".prepare")
}

fn lock_path(control: &Path, work_id: &str) -> PathBuf {
    control.join(format!("{}.lock", isolation::sanitize_key(work_id)))
}

fn receipt_path(control: &Path, work_id: &str) -> PathBuf {
    control.join(format!("{}.receipt.json", isolation::sanitize_key(work_id)))
}

struct Receipt {
    request_digest: String,
    work_id: String,
    host: String,
    repository: String,
    branch: String,
    base: String,
    head: String,
}

impl Receipt {
    fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "version": 1,
            "request_digest": self.request_digest,
            "work_id": self.work_id,
            "repository": {"host": self.host, "repository": self.repository},
            "branch": self.branch,
            "base": self.base,
            "head": self.head,
        })
    }

    fn parse(raw: &str) -> Option<Self> {
        let v: serde_json::Value = serde_json::from_str(raw).ok()?;
        let get = |k: &str| v.get(k).and_then(|x| x.as_str()).map(str::to_string);
        let repo = |k: &str| {
            v.get("repository")
                .and_then(|r| r.get(k))
                .and_then(|x| x.as_str())
                .map(str::to_string)
        };
        Some(Self {
            request_digest: get("request_digest")?,
            work_id: get("work_id")?,
            host: repo("host")?,
            repository: repo("repository")?,
            branch: get("branch")?,
            base: get("base")?,
            head: get("head")?,
        })
    }
}

fn write_receipt(control: &Path, work_id: &str, receipt: &Receipt) -> Result<(), PreparationError> {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let tmp = control.join(format!(
        ".receipt-{}-{}.tmp",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&tmp)
            .map_err(PreparationError::Io)?;
        f.write_all(receipt.to_json().to_string().as_bytes())
            .map_err(PreparationError::Io)?;
        f.sync_all().map_err(PreparationError::Io)?;
    }
    std::fs::rename(&tmp, receipt_path(control, work_id)).map_err(PreparationError::Io)?;
    fsync_dir(control)
}

fn rename_noreplace(from: &Path, to: &Path) -> Result<(), PreparationError> {
    #[cfg(target_os = "macos")]
    {
        use std::os::unix::ffi::OsStrExt;
        let src = std::ffi::CString::new(from.as_os_str().as_bytes())
            .map_err(|_| PreparationError::Io(std::io::Error::other("invalid path")))?;
        let dst = std::ffi::CString::new(to.as_os_str().as_bytes())
            .map_err(|_| PreparationError::Io(std::io::Error::other("invalid path")))?;
        let rc = unsafe { libc::renamex_np(src.as_ptr(), dst.as_ptr(), libc::RENAME_EXCL) };
        if rc != 0 {
            let e = std::io::Error::last_os_error();
            return Err(
                if e.kind() == std::io::ErrorKind::AlreadyExists
                    || e.raw_os_error() == Some(libc::EEXIST)
                {
                    PreparationError::WorkspaceConflict {
                        detail: "workspace path appeared during preparation".into(),
                    }
                } else {
                    PreparationError::Io(e)
                },
            );
        }
        Ok(())
    }
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::ffi::OsStrExt;
        let src = std::ffi::CString::new(from.as_os_str().as_bytes())
            .map_err(|_| PreparationError::Io(std::io::Error::other("invalid path")))?;
        let dst = std::ffi::CString::new(to.as_os_str().as_bytes())
            .map_err(|_| PreparationError::Io(std::io::Error::other("invalid path")))?;
        let rc = unsafe {
            libc::renameat2(
                libc::AT_FDCWD,
                src.as_ptr(),
                libc::AT_FDCWD,
                dst.as_ptr(),
                libc::RENAME_NOREPLACE,
            )
        };
        if rc != 0 {
            let e = std::io::Error::last_os_error();
            return Err(if e.raw_os_error() == Some(libc::EEXIST) {
                PreparationError::WorkspaceConflict {
                    detail: "workspace path appeared during preparation".into(),
                }
            } else {
                PreparationError::Io(e)
            });
        }
        Ok(())
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        if to.symlink_metadata().is_ok() {
            return Err(PreparationError::WorkspaceConflict {
                detail: "workspace path appeared during preparation".into(),
            });
        }
        std::fs::rename(from, to).map_err(PreparationError::Io)
    }
}

fn fsync_dir(path: &Path) -> Result<(), PreparationError> {
    std::fs::File::open(path)
        .and_then(|f| f.sync_all())
        .map_err(PreparationError::Io)
}

fn read_receipt(control: &Path, work_id: &str) -> Result<Option<Receipt>, PreparationError> {
    let path = receipt_path(control, work_id);
    let meta = match std::fs::symlink_metadata(&path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(PreparationError::Io(e)),
    };
    if meta.file_type().is_symlink() {
        return Err(PreparationError::WorkspaceConflict {
            detail: "receipt path is a symlink".into(),
        });
    }
    if !meta.is_file() || meta.len() > 64 * 1024 {
        return Err(PreparationError::WorkspaceConflict {
            detail: "receipt is not a bounded regular file".into(),
        });
    }
    let raw = std::fs::read_to_string(&path).map_err(PreparationError::Io)?;
    Receipt::parse(&raw)
        .map(Some)
        .ok_or_else(|| PreparationError::WorkspaceConflict {
            detail: "receipt is unreadable".into(),
        })
}

fn acquire_flock(path: &Path) -> Result<std::fs::File, PreparationError> {
    match std::fs::symlink_metadata(path) {
        Ok(m) if m.file_type().is_symlink() => {
            return Err(PreparationError::WorkspaceConflict {
                detail: "lock path is a symlink".into(),
            });
        }
        _ => {}
    }
    let file = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .map_err(|e| {
            if e.raw_os_error() == Some(libc::ELOOP) {
                PreparationError::WorkspaceConflict {
                    detail: "lock path is a symlink".into(),
                }
            } else {
                PreparationError::Io(e)
            }
        })?;
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc != 0 {
        let e = std::io::Error::last_os_error();
        return Err(if e.raw_os_error() == Some(libc::EWOULDBLOCK) {
            PreparationError::WorkspaceConflict {
                detail: "another preparer holds the workspace lock".into(),
            }
        } else {
            PreparationError::Io(e)
        });
    }
    Ok(file)
}

const GIT_CONFIG_ALLOWLIST: &[&str] = &[
    "core.repositoryformatversion",
    "core.filemode",
    "core.bare",
    "core.logallrefupdates",
    "core.ignorecase",
    "core.precomposeunicode",
    "user.name",
    "user.email",
];

fn no_symlink_tree(dir: &Path, depth: u32) -> Result<bool, PreparationError> {
    if depth > 32 {
        return Ok(false);
    }
    for entry in std::fs::read_dir(dir).map_err(PreparationError::Io)? {
        let entry = entry.map_err(PreparationError::Io)?;
        let ft = std::fs::symlink_metadata(entry.path())
            .map_err(PreparationError::Io)?
            .file_type();
        if ft.is_symlink() || !(ft.is_dir() || ft.is_file()) {
            return Ok(false);
        }
        if ft.is_dir() && !no_symlink_tree(&entry.path(), depth + 1)? {
            return Ok(false);
        }
    }
    Ok(true)
}

fn verify_git_dir_safe(git_dir: &Path) -> Result<(), PreparationError> {
    let conflict = |detail: &str| PreparationError::WorkspaceConflict {
        detail: detail.to_string(),
    };
    if !no_symlink_tree(git_dir, 0)? {
        return Err(conflict(".git contains a symlink"));
    }
    for name in ["commondir", "gitdir"] {
        if git_dir.join(name).exists() {
            return Err(conflict(".git carries control redirection"));
        }
    }
    if git_dir.join("objects/info/alternates").exists() {
        return Err(conflict("object alternates present"));
    }
    let config = git_dir.join("config");
    match std::fs::symlink_metadata(&config) {
        Ok(m) => {
            if !m.is_file() || m.len() > 64 * 1024 {
                return Err(conflict(".git config is not a bounded regular file"));
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(PreparationError::Io(e)),
    }
    let listed = git(
        git_dir.parent().unwrap_or(git_dir),
        &[
            "config",
            "--file",
            &config.to_string_lossy(),
            "--no-includes",
            "--null",
            "--list",
        ],
    )
    .map_err(|_| conflict(".git config could not be parsed safely"))?;
    for entry in listed.split('\0') {
        if entry.is_empty() {
            continue;
        }
        let key = entry.split('\n').next().unwrap_or("").to_lowercase();
        if !GIT_CONFIG_ALLOWLIST.contains(&key.as_str()) {
            return Err(conflict(
                ".git config carries keys outside the inert allowlist",
            ));
        }
    }
    Ok(())
}

fn verify_existing(
    final_path: &Path,
    branch: &str,
    base: &str,
    head: &str,
) -> Result<(), PreparationError> {
    let conflict = |detail: &str| PreparationError::WorkspaceConflict {
        detail: detail.to_string(),
    };
    let meta = std::fs::symlink_metadata(final_path).map_err(PreparationError::Io)?;
    if meta.file_type().is_symlink() {
        return Err(conflict("workspace path is a symlink"));
    }
    if !meta.is_dir() {
        return Err(conflict("workspace path is not a directory"));
    }
    let git_dir = final_path.join(".git");
    let gmeta = std::fs::symlink_metadata(&git_dir).map_err(PreparationError::Io)?;
    if gmeta.file_type().is_symlink() {
        return Err(conflict(".git is a symlink"));
    }
    if !gmeta.is_dir() {
        return Err(conflict(".git is not a standalone directory"));
    }
    verify_git_dir_safe(&git_dir)?;
    if git(final_path, &["symbolic-ref", "--short", "HEAD"])? != branch {
        return Err(conflict("checked-out branch differs"));
    }
    if git(final_path, &["rev-parse", "--verify", "HEAD"])? != head {
        return Err(conflict("HEAD differs from the recorded pin"));
    }
    if git(
        final_path,
        &["rev-parse", "--verify", "refs/heads/taisan-base"],
    )? != base
    {
        return Err(conflict("base anchor differs"));
    }
    if !git(final_path, &["status", "--porcelain"])?.is_empty() {
        return Err(conflict("workspace is not clean"));
    }
    Ok(())
}

struct Gate {
    request: WorkerRequestV2,
    work: Work,
    branch: String,
    workspace_id: String,
    base: String,
    head: String,
    predecessor: Option<(String, PathBuf)>,
}

fn gates(
    tx: &mut dyn TxView,
    request_id: &str,
    source: &RepositorySource,
    handoff: Option<&HandoffLayout>,
    now_unix_seconds: i64,
) -> Result<Gate, PreparationError> {
    let invalid = |detail: String| PreparationError::InvalidRequest { detail };
    let row = tx
        .request(request_id)?
        .ok_or_else(|| invalid("no durable request row".into()))?;
    let body = tx
        .worker_request_body(request_id)?
        .ok_or_else(|| invalid("no durable v2 request body".into()))?;
    if !body.verifies() {
        return Err(invalid("durable request body fails verification".into()));
    }
    let request =
        WorkerRequestV2::parse(&body.canonical_body).map_err(|e| invalid(e.to_string()))?;
    if request.request_id != request_id {
        return Err(invalid("body request_id disagrees with the row key".into()));
    }
    let store_id = tx.store_id()?;
    if request.store_id != store_id.as_str() {
        return Err(PreparationError::RepositoryMismatch {
            detail: "request store_id names another installation".into(),
        });
    }
    if request.work_id != row.work_id.0 || request.execution_id != row.execution_id.0 {
        return Err(invalid(
            "wire identity disagrees with the durable request".into(),
        ));
    }
    if request.role != row.role {
        return Err(invalid(
            "wire role disagrees with the durable request".into(),
        ));
    }
    if request.spec.revision != row.spec_revision {
        return Err(invalid(
            "wire spec revision disagrees with the durable request".into(),
        ));
    }
    let work = tx
        .work(&row.work_id)?
        .ok_or_else(|| invalid("request names no work".into()))?;
    let execution = tx
        .execution(&row.execution_id)?
        .ok_or_else(|| invalid("request names no execution".into()))?;
    if execution.work_id != work.id {
        return Err(invalid("execution belongs to another work".into()));
    }
    if execution.state != ExecutionState::Ready {
        return Err(invalid("execution is not ready".into()));
    }
    if request.budget.deadline_unix_seconds <= now_unix_seconds {
        return Err(invalid("request deadline has elapsed".into()));
    }
    let spec = tx
        .spec(&work.id, Some(row.spec_revision))?
        .ok_or_else(|| invalid("request spec revision is not recorded".into()))?;
    if request.spec.digest != spec.digest {
        return Err(invalid(
            "wire spec digest disagrees with the recorded revision".into(),
        ));
    }
    let wire_repo = RepositoryRef {
        host: workloop_core::identity::HostInstanceId::trusted(request.repository.host.clone()),
        repository: workloop_core::identity::RepositoryId::trusted(
            request.repository.repository.clone(),
        ),
    };
    if wire_repo != work.binding.repository {
        return Err(PreparationError::RepositoryMismatch {
            detail: "wire repository names a repository the work is not bound to".into(),
        });
    }
    if source.repository != work.binding.repository {
        return Err(PreparationError::RepositoryMismatch {
            detail: "source identity disagrees with the work binding".into(),
        });
    }
    if request.base.repository != request.repository {
        return Err(PreparationError::RepositoryMismatch {
            detail: "base names a foreign repository".into(),
        });
    }
    if !is_hex_sha1(&request.base.object) {
        return Err(invalid("base object is not a 40-lowerhex object id".into()));
    }
    let workspace_id = format!("ws-{}", work.id.0);
    if request.workspace.workspace_id != workspace_id {
        return Err(invalid(
            "wire workspace_id disagrees with the managed id".into(),
        ));
    }
    let branch = format!("work/{}", work.id.0);
    if !valid_component(&work.id.0) {
        return Err(invalid("work id is not a safe path/ref component".into()));
    }
    for res in workloop_core::work::work_resources(&work) {
        let held = match tx.reservation(&res.key())? {
            Some(r) => {
                matches!(
                    &r.holder,
                    ReservationHolder::Execution { execution_id } if *execution_id == execution.id
                ) && !r
                    .expires_at_unix_seconds
                    .is_some_and(|e| e <= now_unix_seconds)
            }
            None => false,
        };
        if !held {
            return Err(PreparationError::ReservationConflict {
                detail: format!("{} is not held by this execution", res.key()),
            });
        }
    }
    for inv in tx.unresolved_invocations()? {
        // The Invocation this preparation exists to launch was recorded
        // by the same consume commit that stored the durable request
        // body these gates validate — it is the expected writer, not a
        // competing one. Any OTHER unresolved writer for the work
        // still refuses.
        if inv.request_id == request_id {
            continue;
        }
        if let Some(owner) = tx.execution(&inv.execution_id)?
            && owner.work_id == work.id
        {
            return Err(PreparationError::ActiveWriter {
                detail: "an unresolved writer exists for this work".into(),
            });
        }
    }
    let (head, predecessor) = match &request.task {
        TaskBody::Coding { .. } => (request.base.object.clone(), None),
        TaskBody::Fix { predecessor, .. } => {
            let layout = handoff.ok_or_else(|| PreparationError::InvalidPredecessor {
                detail: "no handoff layout is configured".into(),
            })?;
            if !valid_ref_components(&predecessor.artifact_id) {
                return Err(invalid("artifact id is not a safe ref name".into()));
            }
            if !is_hex_sha1(&predecessor.produced) {
                return Err(invalid(
                    "predecessor produced is not a 40-lowerhex object id".into(),
                ));
            }
            let row = tx
                .artifact_handoff(&predecessor.artifact_id)?
                .ok_or_else(|| PreparationError::InvalidPredecessor {
                    detail: "no artifact handoff row".into(),
                })?;
            if row.state != HandoffState::Acknowledged {
                return Err(PreparationError::InvalidPredecessor {
                    detail: "handoff is not acknowledged".into(),
                });
            }
            if row.work_id != work.id {
                return Err(PreparationError::InvalidPredecessor {
                    detail: "handoff belongs to another work".into(),
                });
            }
            if row.produced.as_deref() != Some(predecessor.produced.as_str()) {
                return Err(PreparationError::InvalidPredecessor {
                    detail: "handoff produced disagrees".into(),
                });
            }
            if row.manifest_digest.as_deref() != Some(predecessor.manifest_digest.as_str()) {
                return Err(PreparationError::InvalidPredecessor {
                    detail: "handoff manifest digest disagrees".into(),
                });
            }
            if row.base != request.base.object {
                return Err(PreparationError::InvalidPredecessor {
                    detail: "handoff base disagrees".into(),
                });
            }
            (
                predecessor.produced.clone(),
                Some((predecessor.artifact_id.clone(), layout.repository.clone())),
            )
        }
        _ => {
            return Err(invalid(
                "task kind is not a preparable workspace write role".into(),
            ));
        }
    };
    Ok(Gate {
        base: request.base.object.clone(),
        request,
        work,
        branch,
        workspace_id,
        head,
        predecessor,
    })
}

fn validate_git_gate(gate: &Gate, source: &RepositorySource) -> Result<(), PreparationError> {
    if !source.path.is_dir() {
        return Err(PreparationError::MissingBase {
            detail: "repository source path is unavailable".into(),
        });
    }
    if git(&source.path, &["rev-parse", "--git-dir"]).is_err() {
        return Err(PreparationError::MissingBase {
            detail: "repository source is not a git repository".into(),
        });
    }
    check_ref(&source.path, &gate.branch)?;
    match &gate.predecessor {
        None => {
            let object = git(&source.path, &["cat-file", "-t", &gate.base]).unwrap_or_default();
            if object != "commit" {
                return Err(PreparationError::MissingBase {
                    detail: "base object is not present in the source".into(),
                });
            }
        }
        Some((artifact_id, handoff_repo)) => {
            let reference = handoff::artifact_ref(artifact_id);
            check_ref(&source.path, &reference)?;
            let resolved =
                git(handoff_repo, &["rev-parse", "--verify", &reference]).map_err(|_| {
                    PreparationError::InvalidPredecessor {
                        detail: "artifact ref does not resolve".into(),
                    }
                })?;
            if resolved != gate.head {
                return Err(PreparationError::InvalidPredecessor {
                    detail: "artifact ref does not resolve to produced".into(),
                });
            }
            if !is_ancestor(handoff_repo, &gate.base, &gate.head)? {
                return Err(PreparationError::InvalidPredecessor {
                    detail: "produced does not descend from the request base".into(),
                });
            }
        }
    }
    Ok(())
}

fn same_gate(a: &Gate, b: &Gate) -> bool {
    a.request.request_digest == b.request.request_digest
        && a.work.id.0 == b.work.id.0
        && a.branch == b.branch
        && a.workspace_id == b.workspace_id
        && a.base == b.base
        && a.head == b.head
        && a.predecessor == b.predecessor
}

fn revalidate(
    store: &impl Store,
    request_id: &str,
    source: &RepositorySource,
    handoff: Option<&HandoffLayout>,
    now_unix_seconds: i64,
    snapshot: &Gate,
    finalize: impl FnOnce() -> Result<(), PreparationError>,
) -> Result<(), PreparationError> {
    let inner: Result<Result<(), PreparationError>, StoreError> = store.transact(|tx| {
        Ok(
            gates(tx, request_id, source, handoff, now_unix_seconds).and_then(|fresh| {
                if !same_gate(&fresh, snapshot) {
                    return Err(PreparationError::InvalidRequest {
                        detail: "the durable request changed during preparation".into(),
                    });
                }
                finalize()
            }),
        )
    });
    match inner {
        Ok(result) => result,
        Err(e) => Err(PreparationError::Store(e)),
    }
}

fn unique_staging(control: &Path, work_id: &str) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    control.join(format!(
        "staging-{}-{}-{}-{}",
        isolation::sanitize_key(work_id),
        std::process::id(),
        nanos,
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ))
}

fn build_staging(
    staging: &Path,
    base: &str,
    head: &str,
    branch: &str,
    predecessor: &Option<(String, PathBuf)>,
    source: &RepositorySource,
) -> Result<(), PreparationError> {
    git(staging, &["init"])?;
    match predecessor {
        None => {
            let src = source.path.to_string_lossy().into_owned();
            git(
                staging,
                &[
                    "fetch",
                    "--no-tags",
                    "--no-write-fetch-head",
                    "--",
                    &src,
                    base,
                ],
            )
            .map_err(|_| PreparationError::MissingBase {
                detail: "base object could not be imported".into(),
            })?;
            if git(staging, &["cat-file", "-t", base])? != "commit" {
                return Err(PreparationError::MissingBase {
                    detail: "base object is not present in the source".into(),
                });
            }
        }
        Some((artifact_id, handoff_repo)) => {
            let src = handoff_repo.to_string_lossy().into_owned();
            let reference = handoff::artifact_ref(artifact_id);
            git(
                staging,
                &[
                    "fetch",
                    "--no-tags",
                    "--no-write-fetch-head",
                    "--",
                    &src,
                    &format!("+{reference}"),
                ],
            )
            .map_err(|_| PreparationError::InvalidPredecessor {
                detail: "predecessor objects could not be imported".into(),
            })?;
            if git(staging, &["cat-file", "-t", head])? != "commit" {
                return Err(PreparationError::InvalidPredecessor {
                    detail: "produced object is not present after import".into(),
                });
            }
            if git(staging, &["cat-file", "-t", base])? != "commit" {
                return Err(PreparationError::InvalidPredecessor {
                    detail: "request base is not reachable from the predecessor".into(),
                });
            }
        }
    }
    git(staging, &["branch", "taisan-base", base])?;
    git(staging, &["checkout", "-b", branch, head])?;
    if git(staging, &["rev-parse", "--verify", "HEAD"])? != head {
        return Err(PreparationError::Git {
            detail: "checkout did not land on the pinned head".into(),
        });
    }
    if !git(staging, &["status", "--porcelain"])?.is_empty() {
        return Err(PreparationError::Git {
            detail: "staging is not clean after checkout".into(),
        });
    }
    if staging.join(".git/objects/info/alternates").exists() {
        return Err(PreparationError::Git {
            detail: "staging carries object alternates".into(),
        });
    }
    if !git(staging, &["remote"])?.is_empty() {
        return Err(PreparationError::Git {
            detail: "staging carries remotes".into(),
        });
    }
    Ok(())
}

fn prepare_locked(
    store: &impl Store,
    workspace_root: &Path,
    handoff: Option<&HandoffLayout>,
    request_id: &str,
    source: &RepositorySource,
    now_unix_seconds: i64,
    gate: Gate,
) -> Result<PreparedWorkspace, PreparationError> {
    match std::fs::symlink_metadata(workspace_root) {
        Ok(m) if m.file_type().is_symlink() => {
            return Err(PreparationError::WorkspaceConflict {
                detail: "workspace root is a symlink".into(),
            });
        }
        Ok(m) if !m.is_dir() => {
            return Err(PreparationError::WorkspaceConflict {
                detail: "workspace root is not a directory".into(),
            });
        }
        Ok(_) => {}
        Err(e) => return Err(PreparationError::Io(e)),
    }
    let workspace_root = workspace_root
        .canonicalize()
        .map_err(PreparationError::Io)?;
    let final_path = workspace_root.join(&gate.workspace_id);
    let control = control_area(&workspace_root);
    match std::fs::symlink_metadata(&control) {
        Ok(m) => {
            if m.file_type().is_symlink() || !m.is_dir() {
                return Err(PreparationError::WorkspaceConflict {
                    detail: "control area is a symlink".into(),
                });
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            std::fs::create_dir(&control).map_err(PreparationError::Io)?;
        }
        Err(e) => return Err(PreparationError::Io(e)),
    }
    let _flock = acquire_flock(&lock_path(&control, &gate.work.id.0))?;
    let receipt = read_receipt(&control, &gate.work.id.0)?;
    let same_pin = |r: &Receipt| {
        r.work_id == gate.work.id.0
            && r.host == gate.request.repository.host
            && r.repository == gate.request.repository.repository
            && r.branch == gate.branch
            && r.base == gate.base
    };
    match std::fs::symlink_metadata(&final_path) {
        Ok(meta) => {
            if meta.file_type().is_symlink() {
                return Err(PreparationError::WorkspaceConflict {
                    detail: "workspace path is a symlink".into(),
                });
            }
            let receipt = receipt.ok_or_else(|| PreparationError::WorkspaceConflict {
                detail: "workspace exists without a receipt".into(),
            })?;
            if !same_pin(&receipt) {
                return Err(PreparationError::WorkspaceConflict {
                    detail: "workspace exists under a different pin".into(),
                });
            }
            if receipt.request_digest == gate.request.request_digest {
                if receipt.head != gate.head {
                    return Err(PreparationError::WorkspaceConflict {
                        detail: "receipt head disagrees with the request pin".into(),
                    });
                }
                verify_existing(&final_path, &gate.branch, &gate.base, &gate.head)?;
                revalidate(
                    store,
                    request_id,
                    source,
                    handoff,
                    now_unix_seconds,
                    &gate,
                    || Ok(()),
                )?;
            } else {
                // A different durable request of the same Work may take
                // over the managed workspace — a retried Execution's
                // fresh Coding request or a Fix continuing from an
                // acknowledged predecessor — only after the live tree
                // proves it still matches this request's pins; the
                // receipt is then re-bound to the new digest. A dirty
                // or advanced tree still refuses and preserves WIP.
                verify_existing(&final_path, &gate.branch, &gate.base, &gate.head)?;
                let control = control.clone();
                let work_id = gate.work.id.0.clone();
                let request_digest = gate.request.request_digest.clone();
                let head = gate.head.clone();
                revalidate(
                    store,
                    request_id,
                    source,
                    handoff,
                    now_unix_seconds,
                    &gate,
                    move || {
                        write_receipt(
                            &control,
                            &work_id,
                            &Receipt {
                                request_digest,
                                head,
                                ..receipt
                            },
                        )
                    },
                )?;
            }
            return Ok(PreparedWorkspace {
                path: final_path,
                branch: gate.branch,
                base: gate.base,
                head: gate.head,
                request_digest: gate.request.request_digest,
            });
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(PreparationError::Io(e)),
    }
    if let Some(receipt) = &receipt {
        if !same_pin(receipt) {
            return Err(PreparationError::WorkspaceConflict {
                detail: "receipt belongs to a different pin".into(),
            });
        }
        if receipt.request_digest == gate.request.request_digest && receipt.head != gate.head {
            return Err(PreparationError::WorkspaceConflict {
                detail: "receipt head disagrees with the request pin".into(),
            });
        }
        // A different digest under the same pin rebuilds at this
        // request's pins: the tree is gone, so there is nothing to
        // preserve and `build_staging` lands exactly on the gate.
    }
    let staging = unique_staging(&control, &gate.work.id.0);
    std::fs::create_dir(&staging).map_err(PreparationError::Io)?;
    let staged: Result<(), PreparationError> = (|| {
        build_staging(
            &staging,
            &gate.base,
            &gate.head,
            &gate.branch,
            &gate.predecessor,
            source,
        )?;
        verify_git_dir_safe(&staging.join(".git"))?;
        revalidate(
            store,
            request_id,
            source,
            handoff,
            now_unix_seconds,
            &gate,
            || {
                write_receipt(
                    &control,
                    &gate.work.id.0,
                    &Receipt {
                        request_digest: gate.request.request_digest.clone(),
                        work_id: gate.work.id.0.clone(),
                        host: gate.request.repository.host.clone(),
                        repository: gate.request.repository.repository.clone(),
                        branch: gate.branch.clone(),
                        base: gate.base.clone(),
                        head: gate.head.clone(),
                    },
                )?;
                rename_noreplace(&staging, &final_path)?;
                fsync_dir(&workspace_root)
            },
        )
    })();
    if staged.is_err() {
        let _ = std::fs::remove_dir_all(&staging);
    }
    staged?;
    Ok(PreparedWorkspace {
        path: final_path,
        branch: gate.branch,
        base: gate.base,
        head: gate.head,
        request_digest: gate.request.request_digest,
    })
}

pub fn prepare(
    store: &impl Store,
    workspace_root: &Path,
    handoff: Option<&HandoffLayout>,
    request_id: &str,
    source: &RepositorySource,
    now_unix_seconds: i64,
) -> Result<PreparedWorkspace, PreparationError> {
    let gated: Result<Result<Gate, PreparationError>, StoreError> =
        store.transact_deferred(|tx| Ok(gates(tx, request_id, source, handoff, now_unix_seconds)));
    let gate = match gated {
        Ok(result) => result?,
        Err(e) => return Err(PreparationError::Store(e)),
    };
    validate_git_gate(&gate, source)?;
    prepare_locked(
        store,
        workspace_root,
        handoff,
        request_id,
        source,
        now_unix_seconds,
        gate,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(command: &str) -> Vec<String> {
        vec!["/bin/sh".into(), "-c".into(), command.into()]
    }

    #[test]
    fn oversized_output_is_a_bounded_typed_error_not_a_deadlock() {
        let dir = std::env::temp_dir();
        let err = bounded_run(
            &argv("head -c 6000000 /dev/zero"),
            &dir,
            Duration::from_secs(10),
        )
        .unwrap_err();
        assert!(matches!(err, PreparationError::Git { .. }), "{err:?}");
    }

    #[test]
    fn rename_noreplace_refuses_an_existing_destination() {
        let dir = std::env::temp_dir().join(format!("rename-noreplace-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("stage")).unwrap();
        std::fs::create_dir_all(dir.join("final")).unwrap();
        std::fs::write(dir.join("stage/x"), "s").unwrap();
        let err = rename_noreplace(&dir.join("stage"), &dir.join("final")).unwrap_err();
        assert!(
            matches!(err, PreparationError::WorkspaceConflict { .. }),
            "{err:?}"
        );
        assert!(dir.join("stage/x").exists());
        assert!(dir.join("final").is_dir());
        assert!(!dir.join("final/x").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_overrun_deadline_is_a_typed_error() {
        let dir = std::env::temp_dir();
        let err = bounded_run(&argv("sleep 30"), &dir, Duration::from_millis(200)).unwrap_err();
        assert!(matches!(err, PreparationError::Git { .. }), "{err:?}");
    }
}
