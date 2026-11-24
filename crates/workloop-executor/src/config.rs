//! Endpoint and coordinator-side configuration. Trusted bootstrap
//! delivers these: they are installation records, not runtime input —
//! and the credential references are files the bootstrap stages
//! mode-0600, never values on a command line.

use std::path::{Path, PathBuf};

use serde::Deserialize;
use workloop_core::executor::{EXECUTOR_PROTOCOL_V1, endpoint_identity_digest};
use workloop_core::worker::{IsolationProfile, isolation_profile};

/// The helper's static configuration (`docs/remote-executor-endpoint.md`
/// §configuration). One file configures exactly one endpoint identity.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EndpointConfig {
    /// Installation identity the endpoint serves. A challenge naming
    /// another installation is refused.
    pub installation_id: String,
    /// This endpoint's configured identity.
    pub endpoint_id: String,
    /// The helper's durable state root: incarnation record, request
    /// records, staged inputs, workspaces, retained output, receipts.
    pub state_root: PathBuf,
    /// Unix socket `serve` listens on. `connect` bridges to it.
    pub socket_path: PathBuf,
    /// Bootstrap-staged secret proving this endpoint's identity; its
    /// tagged digest is the `endpoint_identity_digest` the coordinator
    /// binds. Read from a file, never logged.
    pub endpoint_credential_file: PathBuf,
    /// Bootstrap-staged secret the coordinator proves itself with.
    pub coordinator_credential_file: PathBuf,
    /// The configured Worker adapter argv — `describe`/`run` per the
    /// unchanged Worker contract. Placement of the adapter is
    /// endpoint-local configuration, never wire data.
    pub worker_argv: Vec<String>,
    /// The approved isolation profile id this endpoint launches under.
    pub isolation_profile_id: String,
    /// Executor protocol versions this endpoint offers.
    #[serde(default = "default_protocols")]
    pub supported_protocols: Vec<String>,
    /// Tolerance for the measured coordinator↔endpoint clock offset;
    /// a measured offset beyond this refuses starts (contract §8).
    #[serde(default = "default_max_clock_offset_ms")]
    pub max_clock_offset_ms: u64,
    /// Conservative one-way transport delay deducted from deadlines.
    #[serde(default = "default_max_one_way_delay_ms")]
    pub max_one_way_delay_ms: u64,
    /// Bounded per-Invocation state-root budget in bytes: staged
    /// inputs plus produced output may not exceed it (bounded storage).
    #[serde(default = "default_max_state_bytes")]
    pub max_state_bytes: u64,
}

fn default_protocols() -> Vec<String> {
    vec![EXECUTOR_PROTOCOL_V1.to_string()]
}

fn default_max_clock_offset_ms() -> u64 {
    30_000
}

fn default_max_one_way_delay_ms() -> u64 {
    2_000
}

fn default_max_state_bytes() -> u64 {
    256 * 1024 * 1024
}

/// Errors in endpoint configuration are fatal at load — a helper that
/// cannot prove its configured identity never serves a session.
#[derive(Debug)]
pub enum ConfigError {
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    Malformed {
        detail: String,
    },
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io { path, source } => write!(f, "{}: {source}", path.display()),
            Self::Malformed { detail } => write!(f, "{detail}"),
        }
    }
}

impl std::error::Error for ConfigError {}

impl EndpointConfig {
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let text = std::fs::read_to_string(path).map_err(|e| ConfigError::Io {
            path: path.to_path_buf(),
            source: e,
        })?;
        let config: Self = toml::from_str(&text).map_err(|e| ConfigError::Malformed {
            detail: format!("endpoint config {}: {e}", path.display()),
        })?;
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<(), ConfigError> {
        let malformed = |detail: String| ConfigError::Malformed { detail };
        if self.installation_id.is_empty() {
            return Err(malformed("installation_id is empty".into()));
        }
        if self.endpoint_id.is_empty() {
            return Err(malformed("endpoint_id is empty".into()));
        }
        if self.worker_argv.is_empty() {
            return Err(malformed("worker_argv is empty".into()));
        }
        if self.supported_protocols.is_empty() {
            return Err(malformed("supported_protocols is empty".into()));
        }
        if self.profile().is_none() {
            return Err(malformed(format!(
                "isolation_profile_id {:?} is not an approved profile",
                self.isolation_profile_id
            )));
        }
        Ok(())
    }

    pub fn profile(&self) -> Option<IsolationProfile> {
        isolation_profile(&self.isolation_profile_id)
    }

    /// The configured endpoint credential bytes — read at use time,
    /// trimmed of surrounding whitespace, never written anywhere.
    pub fn endpoint_credential(&self) -> Result<Vec<u8>, ConfigError> {
        read_credential(&self.endpoint_credential_file)
    }

    pub fn coordinator_credential(&self) -> Result<Vec<u8>, ConfigError> {
        read_credential(&self.coordinator_credential_file)
    }

    /// The identity digest the coordinator binds — derived from the
    /// endpoint credential per contract §2.
    pub fn endpoint_identity_digest(&self) -> Result<String, ConfigError> {
        Ok(endpoint_identity_digest(&self.endpoint_credential()?))
    }
}

/// One bootstrap credential file's contents. A file over the bound is
/// a misconfiguration, not a credential.
pub fn read_credential(path: &Path) -> Result<Vec<u8>, ConfigError> {
    const MAX_CREDENTIAL_BYTES: u64 = 4096;
    let raw = std::fs::read(path).map_err(|e| ConfigError::Io {
        path: path.to_path_buf(),
        source: e,
    })?;
    if raw.len() as u64 > MAX_CREDENTIAL_BYTES {
        return Err(ConfigError::Malformed {
            detail: format!(
                "credential {} exceeds {MAX_CREDENTIAL_BYTES} bytes",
                path.display()
            ),
        });
    }
    let trimmed: Vec<u8> = raw
        .iter()
        .copied()
        .skip_while(|b| b.is_ascii_whitespace())
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .skip_while(|b| b.is_ascii_whitespace())
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    if trimmed.is_empty() {
        return Err(ConfigError::Malformed {
            detail: format!("credential {} is empty", path.display()),
        });
    }
    Ok(trimmed)
}

/// The coordinator's configured binding for one static endpoint
/// (`docs/executor-transport-contract.md` §2, §3): identity plus the
/// bootstrap credentials and the protocol-version range the
/// installation allows.
#[derive(Debug, Clone)]
pub struct CoordinatorEndpoint {
    pub installation_id: String,
    pub endpoint_id: String,
    /// Expected `endpoint_identity_digest` — the authenticated
    /// principal the installation pinned at configuration time.
    pub expected_identity_digest: String,
    /// Bootstrap secrets for the session-auth exchange.
    pub endpoint_credential: Vec<u8>,
    pub coordinator_credential: Vec<u8>,
    /// Executor protocol versions the installation accepts.
    pub allowed_protocols: Vec<String>,
}

impl CoordinatorEndpoint {
    pub fn validate(&self) -> Result<(), String> {
        if self.expected_identity_digest.len() != 64
            || !self
                .expected_identity_digest
                .bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
        {
            return Err("expected_identity_digest is not 64 lowercase hex".into());
        }
        if self.endpoint_credential.is_empty() || self.coordinator_credential.is_empty() {
            return Err("session credentials are empty".into());
        }
        Ok(())
    }
}
