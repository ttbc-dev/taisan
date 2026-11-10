//! The `credential_ref` provider grammar shared by the loader and the
//! adapter boundary. A `CredentialRef` value is opaque to Core; parsing
//! one into a `CredentialProvider` names the resolution slot (an
//! `env` variable, an absolute file path, or an argv) — still never a
//! secret value.
//!
//! Shape rules follow `docs/installation-config.md`: `env:` takes an
//! environment identifier, `file:` takes an absolute path (`~` is never
//! expanded), and `command:` takes a non-empty argv split on ASCII
//! whitespace (no shell). Refusals name the provider rule only — the
//! reference itself may be a pasted secret and is never echoed.

use std::path::PathBuf;

use crate::identity::CredentialRef;

/// Which provider resolves a `credential_ref`, plus the slot it names.
/// The stored reference is the opaque `kind:name` string; this is that
/// same shape decomposed for the adapter that resolves it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CredentialProvider {
    /// `env:NAME` — read the variable from the adapter process
    /// environment.
    Env { name: String },
    /// `file:/absolute/path` — read the file's exact bytes after an
    /// owner-only ownership/permission check.
    File { path: PathBuf },
    /// `command:argv…` — run argv with no shell and capture stdout.
    Command { argv: Vec<String> },
}

impl CredentialProvider {
    /// The provider keyword as it appears in the `kind:name` form.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Env { .. } => "env",
            Self::File { .. } => "file",
            Self::Command { .. } => "command",
        }
    }

    /// Decompose a stored reference into its provider and slot. Pure
    /// shape validation — no value is resolved and no environment,
    /// file, or command is touched.
    pub fn parse(reference: &CredentialRef) -> Result<Self, String> {
        Self::parse_str(reference.as_str())
    }

    /// Shape-check a raw `kind:name` string. Errors name the provider
    /// rule but never echo the reference: an operator may have pasted a
    /// live token into `credential_ref`, and a refusal must not carry
    /// it into logs or evidence.
    pub fn parse_str(text: &str) -> Result<Self, String> {
        if let Some(name) = text.strip_prefix("env:") {
            let valid = !name.is_empty()
                && name
                    .chars()
                    .next()
                    .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
                && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
            if !valid {
                return Err("env: reference must be a non-empty environment identifier".into());
            }
            return Ok(Self::Env { name: name.into() });
        }
        if let Some(path) = text.strip_prefix("file:") {
            let path = PathBuf::from(path);
            if !path.is_absolute() {
                return Err("file: requires an absolute path — '~' is not expanded".into());
            }
            return Ok(Self::File { path });
        }
        if let Some(argv) = text.strip_prefix("command:") {
            let argv: Vec<String> = argv.split_ascii_whitespace().map(str::to_string).collect();
            if argv.is_empty() {
                return Err("command: requires a non-empty argv".into());
            }
            return Ok(Self::Command { argv });
        }
        Err("unknown provider — expected env:, file:, or command:".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The three provider shapes decompose into slot names; refusal
    /// reasons name the rule and never echo the reference itself.
    #[test]
    fn provider_parsing_decomposes_shape_without_echoing_refs() {
        let env = CredentialProvider::parse_str("env:TAISAN_TOKEN").unwrap();
        assert_eq!(
            env,
            CredentialProvider::Env {
                name: "TAISAN_TOKEN".into()
            }
        );
        let file = CredentialProvider::parse_str("file:/etc/secrets/tok").unwrap();
        assert_eq!(
            file,
            CredentialProvider::File {
                path: PathBuf::from("/etc/secrets/tok")
            }
        );
        let command = CredentialProvider::parse_str("command:secret-mgr get taisan").unwrap();
        assert_eq!(
            command,
            CredentialProvider::Command {
                argv: vec!["secret-mgr".into(), "get".into(), "taisan".into()]
            }
        );

        // A pasted secret is refused at the shape check — and the
        // refusal must not echo it into logs or evidence.
        let secret_like = "env:ghp_pasted-secret-value";
        for bad in [
            "bogus:value",
            "env:1BAD",
            "file:~/tok",
            "command:  ",
            secret_like,
        ] {
            let err = CredentialProvider::parse_str(bad).unwrap_err();
            let echoed = bad
                .strip_prefix("env:")
                .or_else(|| bad.strip_prefix("file:"))
                .or_else(|| bad.strip_prefix("command:"))
                .unwrap_or(bad)
                .trim();
            if echoed.is_empty() {
                continue;
            }
            assert!(!err.contains(echoed), "refusal echoed input: {err}");
        }
        // A well-formed env: ref is a slot name, never a resolved value.
        let parsed = CredentialProvider::parse_str("env:GH_PASTED_TOKEN_SLOT").unwrap();
        assert_eq!(parsed.kind(), "env");
    }
}
