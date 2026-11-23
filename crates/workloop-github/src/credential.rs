//! Credential resolution at the adapter boundary
//! (`docs/installation-config.md`): a `credential_ref` names a slot,
//! never a secret; the adapter resolves it to a live value at use time
//! and authenticates it against the actor the binding authorized.
//!
//! The failure classes are typed: `credential.missing` (the role
//! binding defines no slot), `credential.unresolvable` (the provider
//! failed — an unset variable, an unreadable or wrongly-permissioned
//! file, a nonzero command, a malformed reference, or a credential the
//! host does not recognize), `credential.expired` (the provider or the
//! host reports expiry), and `credential.actor_mismatch` (the resolved
//! value authenticates as a different actor than the binding
//! authorized). A secret value can never appear in a failure message.
//!
//! Secret hygiene is structural: the resolved value is held only in
//! [`ResolvedCredential`], whose `Debug`/`Display` redact. It never
//! enters argv, logs, exported environment variables, persisted
//! intents, or evidence.

use std::fmt;
use std::path::Path;

use workloop_core::credential::CredentialProvider;
use workloop_core::identity::{CredentialRef, HostActorRef};

/// A bounded tail of provider output kept in a failure reason — a noisy
/// provider cannot flood diagnostics.
const STDERR_BOUND: usize = 160;

/// The typed credential failure classes. `code()` is the stable
/// `credential.*` token diagnostics and reports carry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CredentialError {
    /// `credential.missing` — the credential slot is undefined: the
    /// role binding carries no reference to resolve.
    Missing { detail: String },
    /// `credential.unresolvable` — the provider or the host denied or
    /// failed the resolution: unset variable, unreadable file, nonzero
    /// command, malformed reference, unrecognized credential.
    Unresolvable { detail: String },
    /// `credential.expired` — the provider or host reports the resolved
    /// credential as expired.
    Expired { detail: String },
    /// `credential.actor_mismatch` — the resolved credential
    /// authenticates as a different actor than the binding authorized.
    ActorMismatch {
        bound: HostActorRef,
        authenticated: HostActorRef,
    },
}

impl CredentialError {
    /// The stable failure-class token.
    pub fn code(&self) -> &'static str {
        match self {
            Self::Missing { .. } => "credential.missing",
            Self::Unresolvable { .. } => "credential.unresolvable",
            Self::Expired { .. } => "credential.expired",
            Self::ActorMismatch { .. } => "credential.actor_mismatch",
        }
    }

    pub fn missing(detail: impl Into<String>) -> Self {
        Self::Missing {
            detail: detail.into(),
        }
    }

    pub fn unresolvable(detail: impl Into<String>) -> Self {
        Self::Unresolvable {
            detail: detail.into(),
        }
    }

    pub fn expired(detail: impl Into<String>) -> Self {
        Self::Expired {
            detail: detail.into(),
        }
    }
}

impl fmt::Display for CredentialError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Missing { detail } | Self::Unresolvable { detail } | Self::Expired { detail } => {
                write!(f, "{}: {detail}", self.code())
            }
            Self::ActorMismatch {
                bound,
                authenticated,
            } => write!(
                f,
                "{}: resolved credential authenticates as {}/{}, \
                 not the bound actor {}/{}",
                self.code(),
                authenticated.host,
                authenticated.actor,
                bound.host,
                bound.actor
            ),
        }
    }
}

impl std::error::Error for CredentialError {}

/// A resolved secret value. `Debug` and `Display` redact; the value
/// leaves this type only through [`ResolvedCredential::expose`], which
/// adapter-internal authentication machinery uses — the value is never
/// persisted, logged, placed on argv, embedded in intents or receipts,
/// or exported in evidence.
pub struct ResolvedCredential(String);

impl ResolvedCredential {
    /// The live secret, for the adapter's own authentication calls.
    /// Callers must not persist, log, or display the returned value.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for ResolvedCredential {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<redacted>")
    }
}

impl fmt::Display for ResolvedCredential {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<redacted>")
    }
}

/// The provider reads resolution dispatches on. `file` and `command`
/// have real implementations in [`read_credential_file`] and
/// [`run_credential_command`]; `env` reads whichever environment view
/// the adapter holds, which lets a fixture seat values in memory
/// without mutating the process environment.
pub trait CredentialProviders {
    /// Read one environment variable's value.
    fn env(&self, name: &str) -> Result<String, CredentialError>;
    /// Read a `file:` credential's exact bytes after the ownership and
    /// mode checks the provider demands.
    fn file(&self, path: &Path) -> Result<Vec<u8>, CredentialError>;
    /// Run a `command:` credential read and capture stdout.
    fn command(&self, argv: &[String]) -> Result<String, CredentialError>;
}

/// The adapter process's real providers: the process environment,
/// the filesystem, and child processes.
pub struct OsProviders;

impl CredentialProviders for OsProviders {
    fn env(&self, name: &str) -> Result<String, CredentialError> {
        std::env::var(name)
            .map_err(|_| CredentialError::unresolvable(format!("env: variable {name} is unset")))
    }

    fn file(&self, path: &Path) -> Result<Vec<u8>, CredentialError> {
        read_credential_file(path)
    }

    fn command(&self, argv: &[String]) -> Result<String, CredentialError> {
        run_credential_command(argv)
    }
}

/// Read a `file:` credential's exact bytes. The file must be a regular
/// file owned by the current user with owner-only permissions — `0600`
/// or stricter (e.g. `0400`), never group- or world-readable. Symlinks
/// are resolved and the target rechecked: `std::fs::metadata` follows
/// links, so ownership and mode below are the target's, checked before
/// any byte is read. A permission failure is `credential.unresolvable`,
/// never a weaker read.
#[cfg(unix)]
pub fn read_credential_file(path: &Path) -> Result<Vec<u8>, CredentialError> {
    use std::os::unix::fs::MetadataExt;
    let metadata = std::fs::metadata(path).map_err(|e| {
        CredentialError::unresolvable(format!("file: {} cannot be stat'ed: {e}", path.display()))
    })?;
    if !metadata.is_file() {
        return Err(CredentialError::unresolvable(format!(
            "file: {} is not a regular file",
            path.display()
        )));
    }
    if metadata.uid() != unsafe { libc::geteuid() } {
        return Err(CredentialError::unresolvable(format!(
            "file: {} must be owned by the current user",
            path.display()
        )));
    }
    let mode = metadata.mode() & 0o777;
    if mode & 0o077 != 0 {
        return Err(CredentialError::unresolvable(format!(
            "file: {} mode {mode:03o} grants group/other access — \
             owner-only permissions (0600 or stricter) are required",
            path.display()
        )));
    }
    std::fs::read(path).map_err(|e| {
        CredentialError::unresolvable(format!("file: {} cannot be read: {e}", path.display()))
    })
}

/// `file:` providers require the POSIX ownership/mode checks this
/// platform lacks; the ref fails closed rather than degrading to an
/// unchecked read.
#[cfg(not(unix))]
pub fn read_credential_file(path: &Path) -> Result<Vec<u8>, CredentialError> {
    let _ = path;
    Err(CredentialError::unresolvable(
        "file: provider is unsupported on this platform — \
         the ownership and mode checks it requires are POSIX",
    ))
}

/// Run a `command:` credential read: argv executes directly with no
/// shell, stdout is captured, and the exit code plus bounded stderr
/// bound the failure reason. The resolved value travels on stdout —
/// it is never an argv entry or an exported environment variable.
pub fn run_credential_command(argv: &[String]) -> Result<String, CredentialError> {
    let Some((program, args)) = argv.split_first() else {
        return Err(CredentialError::unresolvable(
            "command: requires a non-empty argv",
        ));
    };
    let output = std::process::Command::new(program)
        .args(args)
        .output()
        .map_err(|e| {
            CredentialError::unresolvable(format!("command: cannot spawn {program}: {e}"))
        })?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stderr: String = stderr.chars().take(STDERR_BOUND).collect();
        return Err(CredentialError::unresolvable(format!(
            "command: {program} exited with {}: {}",
            output.status,
            stderr.trim_end()
        )));
    }
    let text = String::from_utf8(output.stdout)
        .map_err(|_| CredentialError::unresolvable("command: stdout is not UTF-8"))?;
    Ok(text.trim_end_matches(['\r', '\n']).to_string())
}

/// Resolve one role's credential slot to a live secret. `None` models
/// the undefined slot — `credential.missing`; every provider-level
/// failure is `credential.unresolvable`. An empty resolved value is
/// unresolvable: an empty credential cannot authenticate anything.
pub fn resolve_credential(
    reference: Option<&CredentialRef>,
    providers: &dyn CredentialProviders,
) -> Result<ResolvedCredential, CredentialError> {
    let Some(reference) = reference else {
        return Err(CredentialError::missing(
            "the role binding defines no credential slot",
        ));
    };
    let provider = CredentialProvider::parse(reference)
        .map_err(|rule| CredentialError::unresolvable(format!("malformed reference: {rule}")))?;
    let kind = provider.kind();
    let raw = match provider {
        CredentialProvider::Env { name } => providers.env(&name)?,
        CredentialProvider::File { path } => {
            let bytes = providers.file(&path)?;
            String::from_utf8(bytes).map_err(|_| {
                CredentialError::unresolvable("file: credential content is not UTF-8")
            })?
        }
        CredentialProvider::Command { argv } => providers.command(&argv)?,
    };
    // Whitespace is never part of a credential value: providers
    // conventionally terminate their output — an editor-written token
    // file, `echo`-style command output — so a trailing run is
    // trimmed at the boundary. Whitespace surviving the trim is not
    // repaired into shape: the value rides `Authorization` headers
    // where an embedded space or newline corrupts the request, so the
    // resolution fails honestly as unresolvable.
    let value = raw.trim_end();
    if value.is_empty() {
        return Err(CredentialError::unresolvable(format!(
            "{kind}: resolved to an empty value"
        )));
    }
    if value.chars().any(char::is_whitespace) {
        return Err(CredentialError::unresolvable(format!(
            "{kind}: resolved value contains whitespace"
        )));
    }
    Ok(ResolvedCredential(value.to_string()))
}

/// Resolve a role's credential slot and bind it to the authorized
/// actor: the adapter's `authenticate` answers which host actor the
/// live value acts as, and a different identity is
/// `credential.actor_mismatch` — the renewed-or-replaced credential
/// never re-binds work authorized under the original actor.
pub fn bind_credential(
    reference: Option<&CredentialRef>,
    bound_actor: &HostActorRef,
    providers: &dyn CredentialProviders,
    authenticate: &dyn Fn(&ResolvedCredential) -> Result<HostActorRef, CredentialError>,
) -> Result<HostActorRef, CredentialError> {
    let resolved = resolve_credential(reference, providers)?;
    let authenticated = authenticate(&resolved)?;
    if authenticated != *bound_actor {
        return Err(CredentialError::ActorMismatch {
            bound: bound_actor.clone(),
            authenticated,
        });
    }
    Ok(authenticated)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Seats {
        env: std::collections::BTreeMap<String, String>,
    }

    impl CredentialProviders for Seats {
        fn env(&self, name: &str) -> Result<String, CredentialError> {
            self.env
                .get(name)
                .cloned()
                .ok_or_else(|| CredentialError::unresolvable("env: variable is unset"))
        }
        fn file(&self, path: &Path) -> Result<Vec<u8>, CredentialError> {
            read_credential_file(path)
        }
        fn command(&self, argv: &[String]) -> Result<String, CredentialError> {
            run_credential_command(argv)
        }
    }

    /// The redacted forms are the only display a resolved value has:
    /// `Debug`/`Display` never carry the secret.
    #[test]
    fn resolved_credentials_redact_every_display() {
        let providers = Seats {
            env: [("TAISAN_TOK".into(), "s3ntinel-value".into())]
                .into_iter()
                .collect(),
        };
        let reference = CredentialRef::trusted("env:TAISAN_TOK");
        let resolved = resolve_credential(Some(&reference), &providers).unwrap();
        assert_eq!(resolved.expose(), "s3ntinel-value");
        assert_eq!(format!("{resolved}"), "<redacted>");
        assert_eq!(format!("{resolved:?}"), "<redacted>");
    }
}
