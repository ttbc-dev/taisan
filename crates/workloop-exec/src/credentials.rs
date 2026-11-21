//! Runtime credential resolution at the supervisor boundary
//! (`docs/v0.3.md` §14.3, `docs/installation-config.md` credential
//! references).
//!
//! A `credential_ref` names a provider slot — `env:NAME`,
//! `file:/absolute/path`, or `command:argv…` — never a secret value.
//! Resolution happens here, once per supervisor use, and the resolved
//! bytes are written to a per-Invocation file the containment mount
//! plan binds read-only at `/taisan/runtime-auth`. The value never
//! enters argv, the request body, `runtime.log`, the report, evidence,
//! or an error message: every failure names the provider kind and the
//! failure class only.
//!
//! Failure classes follow the installation-config contract:
//! `credential.missing` (the slot itself is undefined — no reference
//! configured where one is required is the caller's refusal, not this
//! module's), `credential.unresolvable` (the provider failed — unset
//! variable, unreadable or weakly-permissioned file, nonzero or silent
//! command). `credential.expired` has no provider that reports it yet
//! and is never fabricated.

use std::io::Read;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use workloop_core::credential::CredentialProvider;

/// Bound on one `command:` provider run — a secret lookup is a local
/// operation; a wedged provider is `unresolvable`, never a hang.
const COMMAND_TIMEOUT: Duration = Duration::from_secs(30);
/// Bound on resolved credential bytes: a runtime credential is a
/// token, not a payload. Larger output is `unresolvable`.
const MAX_CREDENTIAL_BYTES: u64 = 64 * 1024;
/// Bound on provider-config scanning when checking it carries no
/// credential value.
const MAX_SCAN_FILE_BYTES: u64 = 1024 * 1024;
const MAX_SCAN_FILES: usize = 64;
const MAX_SCAN_DEPTH: u32 = 8;

/// The resolved credential bytes. `Debug`/`Display` are deliberately
/// absent — the value must never be formattable into a log, an error,
/// or evidence.
#[derive(Clone)]
pub struct ResolvedCredential {
    bytes: Vec<u8>,
    /// For a `file:` provider, the canonicalized host path the value
    /// was read from. The containment plan masks it inside the
    /// sandbox when a fixed system bind would expose it — the staged
    /// `/taisan/runtime-auth` file is the only credential surface.
    source: Option<PathBuf>,
}

impl ResolvedCredential {
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// The host path a `file:` provider read, when one exists.
    pub fn source_path(&self) -> Option<&Path> {
        self.source.as_deref()
    }
}

/// Debug is redacted by construction: the resolved value must never
/// reach a log, an error chain, or evidence through formatting.
impl std::fmt::Debug for ResolvedCredential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResolvedCredential")
            .field("bytes", &"<redacted>")
            .finish()
    }
}

/// Why resolution failed. The `class` is the stable evidence token
/// (`credential.unresolvable`); `detail` names the provider kind and
/// the rule that failed — never the reference text (it may be a pasted
/// secret) and never a resolved value.
#[derive(Debug)]
pub struct CredentialError {
    pub class: &'static str,
    pub detail: String,
}

impl std::fmt::Display for CredentialError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.class, self.detail)
    }
}

impl std::error::Error for CredentialError {}

fn unresolvable(detail: impl Into<String>) -> CredentialError {
    CredentialError {
        class: "credential.unresolvable",
        detail: detail.into(),
    }
}

/// Resolve one provider reference to its bytes. Pure shape validation
/// happens in [`CredentialProvider::parse_str`]; this function touches
/// the environment, the filesystem, or spawns the command provider.
pub fn resolve(reference: &str) -> Result<ResolvedCredential, CredentialError> {
    let provider = CredentialProvider::parse_str(reference)
        .map_err(|rule| unresolvable(format!("{} provider: {rule}", provider_kind(reference))))?;
    match provider {
        CredentialProvider::Env { name } => resolve_env(&name),
        CredentialProvider::File { path } => resolve_file(&path),
        CredentialProvider::Command { argv } => resolve_command(&argv),
    }
}

/// The provider keyword for error detail — derived from the reference
/// prefix only, never echoing the slot value.
fn provider_kind(reference: &str) -> &'static str {
    if reference.starts_with("env:") {
        "env"
    } else if reference.starts_with("file:") {
        "file"
    } else if reference.starts_with("command:") {
        "command"
    } else {
        "unknown"
    }
}

fn resolve_env(name: &str) -> Result<ResolvedCredential, CredentialError> {
    match std::env::var_os(name) {
        None => Err(unresolvable(format!(
            "env provider: variable '{name}' is not set"
        ))),
        Some(value) => {
            let bytes = value.as_encoded_bytes().to_vec();
            if bytes.is_empty() {
                return Err(unresolvable(format!(
                    "env provider: variable '{name}' is empty"
                )));
            }
            if bytes.len() as u64 > MAX_CREDENTIAL_BYTES {
                return Err(unresolvable(format!(
                    "env provider: variable '{name}' exceeds the credential bound"
                )));
            }
            Ok(ResolvedCredential {
                bytes,
                source: None,
            })
        }
    }
}

/// `file:` provider rules (`docs/installation-config.md`): absolute
/// path (shape-checked at parse), regular file, owner-only permissions
/// (`0600`/`0400`), symlinks resolved and rechecked, owner-checked.
fn resolve_file(path: &Path) -> Result<ResolvedCredential, CredentialError> {
    let resolved = path
        .canonicalize()
        .map_err(|_| unresolvable("file provider: path does not resolve to an existing file"))?;
    let metadata = std::fs::metadata(&resolved)
        .map_err(|_| unresolvable("file provider: metadata unreadable"))?;
    if !metadata.is_file() {
        return Err(unresolvable("file provider: path is not a regular file"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.mode() & 0o077 != 0 {
            return Err(unresolvable(
                "file provider: permissions must be owner-only (0600 or 0400)",
            ));
        }
        // Owner-check: the file must belong to the effective user —
        // a same-mode file owned by another account is still refused.
        let euid = unsafe { libc::geteuid() };
        if metadata.uid() != euid {
            return Err(unresolvable(
                "file provider: file is not owned by the effective user",
            ));
        }
    }
    if metadata.len() == 0 {
        return Err(unresolvable("file provider: file is empty"));
    }
    if metadata.len() > MAX_CREDENTIAL_BYTES {
        return Err(unresolvable(
            "file provider: file exceeds the credential bound",
        ));
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    std::fs::File::open(&resolved)
        .and_then(|mut f| f.read_to_end(&mut bytes))
        .map_err(|_| unresolvable("file provider: read failed"))?;
    Ok(ResolvedCredential {
        bytes,
        source: Some(resolved),
    })
}

/// `command:` provider: argv split on ASCII whitespace at parse, run
/// without a shell, bounded wall clock, stdout is the credential. The
/// child inherits the supervisor's environment — a secret-manager CLI
/// authenticates to its own backend the same way an operator's shell
/// would; the resolved value still never enters argv, logs, or
/// evidence. Stdout is drained on a reader thread so a provider that
/// fills the pipe cannot deadlock the bounded wait.
fn resolve_command(argv: &[String]) -> Result<ResolvedCredential, CredentialError> {
    resolve_command_bounded(argv, COMMAND_TIMEOUT)
}

fn resolve_command_bounded(
    argv: &[String],
    bound: Duration,
) -> Result<ResolvedCredential, CredentialError> {
    let program = argv.first().expect("parse guarantees a non-empty argv");
    let mut child = std::process::Command::new(program)
        .args(&argv[1..])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        // Its own process group: a provider that forks a descendant
        // holding the stdout pipe is reaped as a unit on timeout — the
        // reader thread and the fd never leak on a dead child's pipe.
        .process_group(0)
        .spawn()
        .map_err(|_| unresolvable("command provider: failed to start"))?;
    let kill_group = |child: &std::process::Child| {
        // SAFETY: the child spawned in its own process group, so the
        // negative pid targets exactly that group.
        unsafe { libc::kill(-(child.id() as i32), libc::SIGKILL) };
    };
    // The reader reports through a channel rather than a join: a
    // provider that exits but leaves a forked descendant holding the
    // stdout pipe open would otherwise wedge `join` forever — the
    // bounded lookup must never hang on a pipe the dead child no
    // longer owns.
    let (output_tx, output_rx) = std::sync::mpsc::channel();
    let reader = child.stdout.take().map(|mut stdout| {
        std::thread::spawn(move || {
            let mut bytes = Vec::new();
            let _ = stdout
                .by_ref()
                .take(MAX_CREDENTIAL_BYTES + 1)
                .read_to_end(&mut bytes);
            let _ = output_tx.send(bytes);
        })
    });
    // Bounded wait: poll try_wait like the verification runner — a
    // provider that wedges past the bound is killed and refused.
    let deadline = std::time::Instant::now() + bound;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if std::time::Instant::now() >= deadline => {
                kill_group(&child);
                let _ = child.wait();
                return Err(unresolvable(
                    "command provider: did not answer within the bound",
                ));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(unresolvable("command provider: wait failed"));
            }
        }
    };
    if !status.success() {
        // A non-zero exit still owes the bound: a descendant can
        // outlive the child holding the stdout pipe open, so reap
        // the group and drain the reader before refusing — the
        // same cleanup the timeout and output paths perform.
        kill_group(&child);
        let _ = output_rx.recv_timeout(bound);
        return Err(unresolvable(format!(
            "command provider: exited with {}",
            status
                .code()
                .map(|c| c.to_string())
                .unwrap_or_else(|| "a signal".into())
        )));
    }
    let bytes = match reader {
        Some(_) => match output_rx.recv_timeout(bound) {
            Ok(bytes) => bytes,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                kill_group(&child);
                return Err(unresolvable(
                    "command provider: output did not close within the bound",
                ));
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => Vec::new(),
        },
        None => Vec::new(),
    };
    if bytes.is_empty() {
        return Err(unresolvable("command provider: produced no output"));
    }
    if bytes.len() as u64 > MAX_CREDENTIAL_BYTES {
        return Err(unresolvable(
            "command provider: output exceeds the credential bound",
        ));
    }
    Ok(ResolvedCredential {
        bytes,
        source: None,
    })
}

/// Whether `haystack` (a file's bytes) contains the credential value.
/// Used to prove a provider configuration file carries no secret —
/// a config that embeds the resolved credential is a configuration
/// error, never a tolerated convenience. `needle` empty never matches.
pub fn contains_secret(haystack: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty() && haystack.windows(needle.len()).any(|w| w == needle)
}

/// Bounded recursive scan of a provider-config path for an embedded
/// credential value. A regular file is scanned up to
/// [`MAX_SCAN_FILE_BYTES`]; a directory is walked to any nesting —
/// [`MAX_SCAN_FILES`] regular files total and [`MAX_SCAN_DEPTH`]
/// directories deep. Exceeding a bound is an error: a config the scan
/// cannot cover cannot be proven credential-free, and the "no
/// credential value" rule is a configuration contract, not an
/// advisory check. Symlinks are never followed — the mount carries
/// only what the directory itself holds.
pub fn provider_config_contains(path: &Path, needle: &[u8]) -> Result<bool, std::io::Error> {
    if needle.is_empty() {
        return Ok(false);
    }
    let metadata = std::fs::symlink_metadata(path)?;
    if metadata.is_file() {
        return scan_file(path, needle);
    }
    if metadata.is_dir() {
        let mut scanned = 0usize;
        return scan_dir(path, needle, &mut scanned, 0);
    }
    Ok(false)
}

fn scan_dir(
    dir: &Path,
    needle: &[u8],
    scanned: &mut usize,
    depth: u32,
) -> Result<bool, std::io::Error> {
    if depth > MAX_SCAN_DEPTH {
        return Err(scan_bound());
    }
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            if scan_dir(&entry.path(), needle, scanned, depth + 1)? {
                return Ok(true);
            }
            continue;
        }
        if !file_type.is_file() {
            continue;
        }
        *scanned += 1;
        if *scanned > MAX_SCAN_FILES {
            return Err(scan_bound());
        }
        if scan_file(&entry.path(), needle)? {
            return Ok(true);
        }
    }
    Ok(false)
}

fn scan_bound() -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        "scan bound exceeded: the provider config cannot be proven credential-free",
    )
}

fn scan_file(path: &Path, needle: &[u8]) -> Result<bool, std::io::Error> {
    let mut file = std::fs::File::open(path)?;
    // A file larger than the read bound cannot be proven
    // credential-free — refuse rather than scan a prefix and pass.
    if file.metadata()?.len() > MAX_SCAN_FILE_BYTES {
        return Err(scan_bound());
    }
    let mut buf = Vec::new();
    file.read_to_end(&mut buf)?;
    Ok(contains_secret(&buf, needle))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn env_provider_resolves_and_reports_missing_without_value() {
        // SAFETY: test-only env mutation in a single-threaded test.
        unsafe { std::env::set_var("TAISAN_TEST_CRED_ENV", "s3cr3t-value") };
        let resolved = resolve("env:TAISAN_TEST_CRED_ENV").unwrap();
        assert_eq!(resolved.bytes(), b"s3cr3t-value");
        let err = resolve("env:TAISAN_TEST_CRED_MISSING").unwrap_err();
        assert_eq!(err.class, "credential.unresolvable");
        assert!(err.to_string().contains("TAISAN_TEST_CRED_MISSING"));
        assert!(!err.to_string().contains("s3cr3t"));
    }

    #[test]
    fn file_provider_enforces_owner_only_permissions() {
        let dir = std::env::temp_dir().join(format!("cred-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let good = dir.join("good.token");
        std::fs::write(&good, "file-secret").unwrap();
        std::fs::set_permissions(&good, std::fs::Permissions::from_mode(0o600)).unwrap();
        let resolved = resolve(&format!("file:{}", good.display())).unwrap();
        assert_eq!(resolved.bytes(), b"file-secret");

        let weak = dir.join("weak.token");
        std::fs::write(&weak, "weak-secret").unwrap();
        std::fs::set_permissions(&weak, std::fs::Permissions::from_mode(0o644)).unwrap();
        let err = resolve(&format!("file:{}", weak.display())).unwrap_err();
        assert_eq!(err.class, "credential.unresolvable");
        assert!(!err.to_string().contains("weak-secret"));

        let missing = dir.join("absent.token");
        let err = resolve(&format!("file:{}", missing.display())).unwrap_err();
        assert_eq!(err.class, "credential.unresolvable");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn command_provider_captures_stdout_and_refuses_failure() {
        let ok = resolve("command:printf cmd-secret").unwrap();
        assert_eq!(ok.bytes(), b"cmd-secret");
        let failed = resolve("command:false").unwrap_err();
        assert_eq!(failed.class, "credential.unresolvable");
        let silent = resolve("command:true").unwrap_err();
        assert_eq!(silent.class, "credential.unresolvable");
    }

    /// A provider that exits but leaves a forked descendant holding
    /// the stdout pipe open must refuse within the bound — the dead
    /// child's pipe staying open is unresolvable evidence, never a
    /// hang.
    #[test]
    fn command_provider_output_held_open_past_the_bound_refuses() {
        let argv = vec![
            "sh".to_string(),
            "-c".to_string(),
            "sleep 5 & printf leaked".to_string(),
        ];
        let started = std::time::Instant::now();
        let err = match resolve_command_bounded(&argv, Duration::from_millis(300)) {
            Ok(_) => panic!("a pipe held open past the bound must not resolve"),
            Err(e) => e,
        };
        assert_eq!(err.class, "credential.unresolvable");
        assert!(err.to_string().contains("did not close"), "{err}");
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "the lookup hung on the inherited pipe"
        );
    }

    #[test]
    fn parse_shape_errors_never_echo_the_reference() {
        let err = resolve("env:1BAD-NAME").unwrap_err();
        assert_eq!(err.class, "credential.unresolvable");
        assert!(!err.to_string().contains("1BAD-NAME"));
    }

    #[test]
    fn provider_config_scan_finds_an_embedded_secret() {
        let dir = std::env::temp_dir().join(format!("cred-scan-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let clean = dir.join("clean.toml");
        std::fs::write(&clean, "model = \"m\"\n").unwrap();
        assert!(!provider_config_contains(&clean, b"needle").unwrap());
        let dirty = dir.join("dirty.toml");
        std::fs::write(&dirty, "token = \"xxneedlexx\"\n").unwrap();
        assert!(provider_config_contains(&dirty, b"needle").unwrap());
        assert!(!contains_secret(b"", b"needle"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn provider_config_scan_reaches_nested_directories() {
        let dir = std::env::temp_dir().join(format!("cred-scan-nested-{}", std::process::id()));
        let nested = dir.join("models").join("providers");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(nested.join("deep.conf"), "token = \"xxneedlexx\"\n").unwrap();
        // A credential embedded at any depth is still detected —
        // top-level-only scanning let nested secrets escape the check.
        assert!(provider_config_contains(&dir, b"needle").unwrap());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn provider_config_scan_fails_closed_at_its_bounds() {
        let dir = std::env::temp_dir().join(format!("cred-scan-bound-{}", std::process::id()));
        // Nesting past MAX_SCAN_DEPTH cannot be proven credential-free.
        let mut deep = dir.clone();
        for i in 0..=(MAX_SCAN_DEPTH + 1) {
            deep = deep.join(format!("d{i}"));
        }
        std::fs::create_dir_all(&deep).unwrap();
        std::fs::write(deep.join("deep.conf"), "token = \"x\"\n").unwrap();
        let err = provider_config_contains(&dir, b"needle").unwrap_err();
        assert!(err.to_string().contains("scan bound"), "{err}");

        // A file past the byte bound cannot be proven either — a
        // prefix scan would pass a credential at a deeper offset.
        let dir2 = std::env::temp_dir().join(format!("cred-scan-big-{}", std::process::id()));
        std::fs::create_dir_all(&dir2).unwrap();
        let big = dir2.join("big.conf");
        std::fs::write(&big, vec![b'x'; (MAX_SCAN_FILE_BYTES + 1) as usize]).unwrap();
        let err = provider_config_contains(&big, b"needle").unwrap_err();
        assert!(err.to_string().contains("scan bound"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&dir2);
    }

    #[test]
    fn file_provider_reports_its_source_path() {
        let dir = std::env::temp_dir().join(format!("cred-source-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let token = dir.join("token");
        std::fs::write(&token, "source-secret").unwrap();
        std::fs::set_permissions(&token, std::fs::Permissions::from_mode(0o600)).unwrap();
        let resolved = resolve(&format!("file:{}", token.display())).unwrap();
        assert_eq!(
            resolved.source_path().map(Path::to_path_buf),
            Some(token.canonicalize().unwrap()),
            "the file provider reports the canonical path it read"
        );
        // SAFETY: test-only env mutation in a single-threaded test.
        unsafe { std::env::set_var("TAISAN_TEST_CRED_SOURCE", "env-secret") };
        assert_eq!(
            resolve("env:TAISAN_TEST_CRED_SOURCE")
                .unwrap()
                .source_path(),
            None
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
