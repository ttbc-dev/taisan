//! The runtime credential/egress boundary every contained launch
//! shares (`docs/v0.3.md` §14.3–§14.4, `docs/installation-config.md`
//! credential references).
//!
//! Configuration names *references*, never values: a
//! `credential_ref` provider slot, an absolute non-secret provider
//! config path, and the bare model-API host set. Resolution is lazy
//! and happens once per supervisor at the first contained launch or
//! declaration probe — `env:`/`file:`/`command:` providers resolve
//! through [`crate::credentials`], the resolved bytes are staged as a
//! single `0600` file the mount plan binds read-only at
//! `/taisan/runtime-auth`, and the provider config is validated
//! (present, readable, and free of the credential value) before it is
//! bound read-only at `/taisan/provider-config`.
//!
//! Egress is a supervisor-owned CONNECT proxy on a Unix socket,
//! started lazily the first time a contained launch needs it and
//! living until the supervisor drops. Its allowlist is exactly the
//! configured model-API hosts; an empty set starts no proxy and the
//! sandbox's `--unshare-net` leaves egress fully denied. The value
//! never enters argv, the request body, `runtime.log`, the report,
//! evidence, or an error message.
//!
//! Every failure is a typed refusal the caller records as a launch
//! repair — there is no ambient-credential or unrestricted-network
//! fallback.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use workloop_core::domain::{canonical_json, sha256_hex};

use crate::credentials::{self, ResolvedCredential};
use crate::egress::ConnectProxy;

/// Everything the boundary needs that is not itself secret. Paths are
/// absolute; `model_api_hosts` are the validated bare host names.
#[derive(Debug, Clone)]
pub struct BoundaryConfig {
    /// `env:`/`file:`/`command:` provider reference — shape-validated
    /// upstream, resolved lazily here. `None` grants no credential.
    pub credential_ref: Option<String>,
    /// Absolute path to the non-secret provider configuration (file
    /// or directory), mounted read-only.
    pub provider_config: Option<PathBuf>,
    /// Bare host names the CONNECT proxy may reach on 443. Empty
    /// denies all Worker egress — no proxy is started.
    pub model_api_hosts: Vec<String>,
    /// Supervisor-owned directory the staged credential file lives
    /// under (`<workspace_root>/.runtime`).
    pub runtime_dir: PathBuf,
    /// Supervisor-owned directory the proxy's bounded evidence log
    /// lives under (`<workspace_root>/.egress`); sockets land here
    /// when the path fits `sun_path`, else in the digest-derived
    /// owner-only fallback `egress::socket_dir` picks.
    pub egress_dir: PathBuf,
    /// The binary the in-sandbox relay runs as — the `taisan`
    /// executable carrying the `__sandbox-relay` subcommand. Resolved
    /// by the composition root (`std::env::current_exe` in production).
    pub relay_exe: PathBuf,
}

/// What one contained launch gets from the boundary. Pure data — the
/// proxy itself stays owned by the boundary until the supervisor
/// drops; the grant carries only the paths and the relay URL.
#[derive(Debug, Clone, Default)]
pub struct BoundaryGrant {
    /// The staged credential file → `/taisan/runtime-auth`.
    pub credential_file: Option<PathBuf>,
    /// For a `file:` provider, the host path the value was read from —
    /// masked inside the sandbox when a fixed system bind covers it,
    /// so the staged file is the only credential surface.
    pub credential_source: Option<PathBuf>,
    /// The validated provider config → `/taisan/provider-config`.
    pub provider_config: Option<PathBuf>,
    /// The proxy's host-side Unix socket → `/taisan/proxy.sock`.
    pub proxy_sock: Option<PathBuf>,
}

struct BoundaryReady {
    credential_file: Option<PathBuf>,
    credential_source: Option<PathBuf>,
    provider_config: Option<PathBuf>,
    /// The provider-config content identity captured at resolution —
    /// `ensure` re-checks it so a post-resolution edit re-runs the
    /// credential scan instead of riding on the cached grant.
    provider_digest: String,
    proxy: Option<ConnectProxy>,
}

enum BoundaryState {
    Unresolved,
    Ready(BoundaryReady),
}

/// The lazily-resolved runtime boundary. `ensure` is idempotent: the
/// first contained use resolves the credential, validates the
/// provider config, and starts the proxy; later uses reuse the
/// recorded state. A failure is never cached — each call retries, so
/// a credential that becomes available mid-run recovers on the next
/// launch attempt.
pub struct RuntimeBoundary {
    config: BoundaryConfig,
    state: Mutex<BoundaryState>,
}

impl RuntimeBoundary {
    pub fn new(config: BoundaryConfig) -> Self {
        Self {
            config,
            state: Mutex::new(BoundaryState::Unresolved),
        }
    }

    /// The executable the in-sandbox relay wraps launches with.
    pub fn relay_exe(&self) -> &Path {
        &self.config.relay_exe
    }

    /// The content identity the containment gate records: the
    /// credential *reference* (never its value), the provider-config
    /// content digest, and the model-API host set. A change to any of
    /// them is an identity change that re-proves the negative-probe
    /// battery (`docs/v0.3.md` §14: profile/config changes never ride
    /// on stale evidence). Provider-config content is hashed, not the
    /// credential — the secret never enters evidence.
    pub fn identity_digest(&self) -> Result<String, String> {
        let provider_digest = match &self.config.provider_config {
            None => "none".to_string(),
            Some(path) => provider_config_digest(path)
                .map_err(|e| format!("provider_config.unreadable: {e}"))?,
        };
        let mut hosts = self.config.model_api_hosts.clone();
        hosts.sort();
        let value = serde_json::json!({
            "credential_ref": self.config.credential_ref,
            "provider_config": provider_digest,
            "model_api_hosts": hosts,
        });
        Ok(sha256_hex("runtime-boundary-v1", &canonical_json(&value)))
    }

    /// Resolve the boundary for one contained launch or probe. The
    /// first call performs the credential/provider/proxy work; later
    /// calls return the recorded grant. `Err` is a typed refusal —
    /// `credential.*`, `provider_config.*`, or `egress.*` — the caller
    /// records it as a launch repair.
    pub fn ensure(&self) -> Result<BoundaryGrant, String> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if matches!(*state, BoundaryState::Unresolved) {
            *state = BoundaryState::Ready(self.resolve()?);
        }
        // The provider config is mounted read-only into the sandbox:
        // a post-resolution edit — including a same-size one that
        // embeds the resolved credential — must re-run the scan, not
        // ride on the cached grant.
        let stale = match &*state {
            BoundaryState::Ready(ready) => current_provider_digest(&self.config.provider_config)
                .map(|digest| digest != ready.provider_digest)
                .unwrap_or(true),
            BoundaryState::Unresolved => unreachable!("state resolved above"),
        };
        if stale {
            if let BoundaryState::Ready(ready) = &mut *state
                && let Some(proxy) = ready.proxy.take()
            {
                proxy.shutdown();
            }
            *state = BoundaryState::Ready(self.resolve()?);
        }
        match &*state {
            BoundaryState::Ready(ready) => Ok(BoundaryGrant {
                credential_file: ready.credential_file.clone(),
                credential_source: ready.credential_source.clone(),
                provider_config: ready.provider_config.clone(),
                proxy_sock: ready
                    .proxy
                    .as_ref()
                    .map(|proxy| proxy.sock_path().to_path_buf()),
            }),
            BoundaryState::Unresolved => unreachable!("state resolved above"),
        }
    }

    /// The one-time resolution: credential → staged file, provider
    /// config → validated, proxy → started when hosts are configured.
    fn resolve(&self) -> Result<BoundaryReady, String> {
        let mut credential_bytes: Option<Vec<u8>> = None;
        let mut credential_source: Option<PathBuf> = None;
        let credential_file = match &self.config.credential_ref {
            None => None,
            Some(reference) => {
                let resolved = credentials::resolve(reference).map_err(|e| e.to_string())?;
                credential_bytes = Some(resolved.bytes().to_vec());
                credential_source = resolved.source_path().map(Path::to_path_buf);
                Some(self.stage_credential(&resolved)?)
            }
        };
        if let Some(path) = &self.config.provider_config {
            self.validate_provider_config(path, credential_bytes.as_deref())?;
        }
        let provider_digest = current_provider_digest(&self.config.provider_config)
            .map_err(|e| format!("provider_config.unreadable: {e}"))?;
        let proxy = if self.config.model_api_hosts.is_empty() {
            None
        } else {
            let sock_dir = crate::egress::socket_dir(&self.config.egress_dir)
                .map_err(|e| format!("egress.proxy_unavailable: {e}"))?;
            let sock = sock_dir.join("worker.sock");
            let evidence = self.config.egress_dir.join("worker-egress.jsonl");
            Some(
                ConnectProxy::start(&sock, self.config.model_api_hosts.clone(), &evidence)
                    .map_err(|e| format!("egress.proxy_unavailable: {e}"))?,
            )
        };
        Ok(BoundaryReady {
            credential_file,
            credential_source,
            provider_config: self.config.provider_config.clone(),
            provider_digest,
            proxy,
        })
    }

    /// Stage the resolved credential as a single owner-only file the
    /// mount plan binds read-only. The value exists only in this file
    /// and the provider's own storage — never argv, logs, or evidence.
    fn stage_credential(&self, credential: &ResolvedCredential) -> Result<PathBuf, String> {
        std::fs::create_dir_all(&self.config.runtime_dir).map_err(|e| {
            format!(
                "credential.stage_failed: create {}: {e}",
                self.config.runtime_dir.display()
            )
        })?;
        let path = self.config.runtime_dir.join("runtime-auth");
        let tmp = self.config.runtime_dir.join("runtime-auth.tmp");
        {
            use std::io::Write;
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .open(&tmp)
                .map_err(|e| format!("credential.stage_failed: {e}"))?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                file.set_permissions(std::fs::Permissions::from_mode(0o600))
                    .map_err(|e| format!("credential.stage_failed: {e}"))?;
            }
            file.write_all(credential.bytes())
                .and_then(|()| file.sync_all())
                .map_err(|e| format!("credential.stage_failed: {e}"))?;
        }
        std::fs::rename(&tmp, &path).map_err(|e| format!("credential.stage_failed: {e}"))?;
        Ok(path)
    }

    /// The provider config must exist, be a file or directory the
    /// supervisor can read, and carry no credential value — a secret
    /// embedded there is a configuration error, never a tolerated
    /// convenience (`docs/installation-config.md`).
    fn validate_provider_config(
        &self,
        path: &Path,
        credential_bytes: Option<&[u8]>,
    ) -> Result<(), String> {
        let metadata = std::fs::symlink_metadata(path)
            .map_err(|e| format!("provider_config.unreadable: {e}"))?;
        if !metadata.is_file() && !metadata.is_dir() {
            return Err("provider_config.unreadable: not a regular file or directory".into());
        }
        // Scanning the provider config for the resolved bytes proves
        // the mount carries no secret. When no credential is
        // configured there is nothing to leak.
        if let Some(bytes) = credential_bytes
            && !bytes.is_empty()
            && credentials::provider_config_contains(path, bytes)
                .map_err(|e| format!("provider_config.unreadable: {e}"))?
        {
            return Err(
                "provider_config.contains_credential: the provider configuration embeds \
                 the resolved credential value"
                    .into(),
            );
        }
        Ok(())
    }
}

impl Drop for RuntimeBoundary {
    fn drop(&mut self) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if let BoundaryState::Ready(ready) = &mut *state
            && let Some(proxy) = ready.proxy.take()
        {
            proxy.shutdown();
        }
    }
}

/// The provider-config content identity `ensure` compares against
/// the recorded grant: `None` for an unconfigured boundary, else the
/// same digest the gate identity records.
fn current_provider_digest(path: &Option<PathBuf>) -> Result<String, std::io::Error> {
    match path {
        None => Ok("none".to_string()),
        Some(path) => provider_config_digest(path),
    }
}

/// Bounded-memory digest of one file's full content: every byte feeds
/// the digest through fixed-size chunks, so a same-size edit anywhere
/// in the file changes the identity without buffering the file.
fn file_content_digest(path: &Path) -> Result<String, std::io::Error> {
    use std::io::Read;
    let mut file = std::fs::File::open(path)?;
    let mut chunks = String::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        chunks.push_str(&sha256_hex("provider-config-chunk-v1", &buf[..n]));
    }
    Ok(sha256_hex("provider-config-file-v1", chunks.as_bytes()))
}

/// Bounded content digest of a provider-config path for the boundary
/// identity: a file hashes its full bytes; a directory hashes a
/// sorted manifest of member names, kinds, sizes, and content
/// digests — enough that any edit, including a same-size one,
/// re-proves the gate without hashing unbounded trees.
fn provider_config_digest(path: &Path) -> Result<String, std::io::Error> {
    const MAX_ENTRIES: usize = 256;
    let metadata = std::fs::symlink_metadata(path)?;
    if metadata.is_file() {
        return file_content_digest(path);
    }
    if !metadata.is_dir() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "not a file or directory",
        ));
    }
    let mut manifest = Vec::new();
    collect_manifest(path, path, &mut manifest, MAX_ENTRIES)?;
    manifest.sort();
    Ok(sha256_hex(
        "provider-config-dir-v1",
        &canonical_json(&serde_json::json!(manifest)),
    ))
}

/// Per-member read bound for [`collect_manifest`]: a member file's
/// digest covers the member's full content — the manifest stays
/// bounded by the file count cap however large the config tree is.
fn collect_manifest(
    root: &Path,
    dir: &Path,
    out: &mut Vec<String>,
    budget: usize,
) -> Result<(), std::io::Error> {
    if out.len() >= budget {
        return Ok(());
    }
    for entry in std::fs::read_dir(dir)? {
        if out.len() >= budget {
            break;
        }
        let entry = entry?;
        let path = entry.path();
        let rel = path
            .strip_prefix(root)
            .unwrap_or(&path)
            .to_string_lossy()
            .into_owned();
        // Never follow a member link: a symlink's identity is its
        // target path, and recursing or hashing through it would
        // fold an outside tree into this config's identity.
        let file_type = entry.file_type()?;
        if file_type.is_symlink() {
            let target = std::fs::read_link(&path)?.to_string_lossy().into_owned();
            out.push(format!("link:{rel}:{target}"));
        } else if file_type.is_dir() {
            out.push(format!("dir:{rel}"));
            collect_manifest(root, &path, out, budget)?;
        } else if file_type.is_file() {
            // Content, not name+size: a same-size edit is an identity
            // change the gate must re-prove.
            let digest = file_content_digest(&path)?;
            out.push(format!("file:{rel}:{}:{digest}", entry.metadata()?.len()));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn tmp(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("boundary-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn config(dir: &Path) -> BoundaryConfig {
        BoundaryConfig {
            credential_ref: None,
            provider_config: None,
            model_api_hosts: vec![],
            runtime_dir: dir.join("runtime"),
            egress_dir: dir.join("egress"),
            relay_exe: PathBuf::from("/bin/taisan"),
        }
    }

    #[test]
    fn an_empty_boundary_grants_nothing_and_starts_no_proxy() {
        let dir = tmp("empty");
        let boundary = RuntimeBoundary::new(config(&dir));
        let grant = boundary.ensure().unwrap();
        assert!(grant.credential_file.is_none());
        assert!(grant.provider_config.is_none());
        assert!(grant.proxy_sock.is_none());
        // Idempotent: a second ensure returns the same empty grant.
        let again = boundary.ensure().unwrap();
        assert!(again.proxy_sock.is_none());
        assert!(!dir.join("egress/worker.sock").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn env_credential_resolves_once_into_an_owner_only_file() {
        let dir = tmp("envcred");
        // SAFETY: test-only env mutation.
        unsafe { std::env::set_var("TAISAN_BOUNDARY_TEST_TOKEN", "tok-abc") };
        let mut cfg = config(&dir);
        cfg.credential_ref = Some("env:TAISAN_BOUNDARY_TEST_TOKEN".into());
        let boundary = RuntimeBoundary::new(cfg);
        let grant = boundary.ensure().unwrap();
        let path = grant.credential_file.clone().expect("credential staged");
        assert_eq!(std::fs::read(&path).unwrap(), b"tok-abc");
        #[cfg(unix)]
        {
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "staged credential must be owner-only");
        }
        // The grant never carries the value — only the file path.
        assert!(!format!("{grant:?}").contains("tok-abc"));
        // A missing provider refuses as credential.unresolvable and
        // leaves no staged file behind for the next attempt to trust.
        unsafe { std::env::remove_var("TAISAN_BOUNDARY_TEST_TOKEN") };
        let mut cfg2 = config(&dir);
        cfg2.credential_ref = Some("env:TAISAN_BOUNDARY_TEST_TOKEN".into());
        let failing = RuntimeBoundary::new(cfg2);
        let err = failing.ensure().unwrap_err();
        assert!(err.contains("credential.unresolvable"), "{err}");
        assert!(!err.contains("tok-abc"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A `file:` provider's source path flows into the grant so the
    /// containment plan can mask it when a fixed system bind would
    /// expose it — env/command providers carry no host path.
    #[test]
    fn a_file_credential_grant_reports_its_source_path() {
        let dir = tmp("filecred");
        let token = dir.join("runtime.token");
        std::fs::write(&token, "file-tok").unwrap();
        std::fs::set_permissions(&token, std::fs::Permissions::from_mode(0o600)).unwrap();
        let mut cfg = config(&dir);
        cfg.credential_ref = Some(format!("file:{}", token.display()));
        let boundary = RuntimeBoundary::new(cfg);
        let grant = boundary.ensure().unwrap();
        assert_eq!(
            grant.credential_source,
            Some(token.canonicalize().unwrap()),
            "the grant carries the credential's canonical source path"
        );
        assert!(grant.credential_file.is_some(), "the staged file exists");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn model_api_hosts_start_the_proxy_and_grant_its_socket() {
        let dir = tmp("proxy");
        let mut cfg = config(&dir);
        cfg.model_api_hosts = vec!["model.example".into()];
        let boundary = RuntimeBoundary::new(cfg);
        let grant = boundary.ensure().unwrap();
        let sock = grant.proxy_sock.expect("proxy socket granted");
        assert!(sock.ends_with("worker.sock"));
        assert!(sock.exists(), "the proxy socket is bound for the sandbox");
        // The identity digest changes with the host set — the gate
        // re-proves on any boundary change.
        let digest_a = boundary.identity_digest().unwrap();
        let mut cfg2 = config(&dir);
        cfg2.model_api_hosts = vec!["other.example".into()];
        let boundary2 = RuntimeBoundary::new(cfg2);
        assert_ne!(digest_a, boundary2.identity_digest().unwrap());
        drop(boundary);
        drop(boundary2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn provider_config_embedding_the_credential_is_refused() {
        let dir = tmp("dirtycfg");
        // SAFETY: test-only env mutation.
        unsafe { std::env::set_var("TAISAN_BOUNDARY_TEST_TOKEN2", "sekrit") };
        let dirty = dir.join("provider.toml");
        std::fs::write(&dirty, "token = \"sekrit\"\n").unwrap();
        let mut cfg = config(&dir);
        cfg.credential_ref = Some("env:TAISAN_BOUNDARY_TEST_TOKEN2".into());
        cfg.provider_config = Some(dirty);
        let boundary = RuntimeBoundary::new(cfg);
        let err = boundary.ensure().unwrap_err();
        assert!(err.contains("provider_config.contains_credential"), "{err}");
        assert!(!err.contains("sekrit"));
        unsafe { std::env::remove_var("TAISAN_BOUNDARY_TEST_TOKEN2") };
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_clean_provider_config_is_granted_read_only_path() {
        let dir = tmp("cleancfg");
        let clean = dir.join("provider.toml");
        std::fs::write(&clean, "model = \"m\"\n").unwrap();
        std::fs::set_permissions(&clean, std::fs::Permissions::from_mode(0o644)).unwrap();
        let mut cfg = config(&dir);
        cfg.provider_config = Some(clean.clone());
        let boundary = RuntimeBoundary::new(cfg);
        let grant = boundary.ensure().unwrap();
        assert_eq!(grant.provider_config, Some(clean));
        // Digest is stable for identical content and changes on edit.
        let d1 = boundary.identity_digest().unwrap();
        std::fs::write(dir.join("provider.toml"), "model = \"m2\"\n").unwrap();
        // identity_digest re-reads the file each call.
        let d2 = boundary.identity_digest().unwrap();
        assert_ne!(d1, d2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A directory provider-config's identity covers member *content*:
    /// a same-size edit is still an identity change — the gate re-proves
    /// rather than riding on stale preflight evidence.
    #[test]
    fn a_same_size_member_edit_changes_the_directory_identity() {
        let dir = tmp("dirmanifest");
        let cfg_dir = dir.join("provider.d");
        std::fs::create_dir_all(&cfg_dir).unwrap();
        std::fs::write(cfg_dir.join("a.toml"), "model = \"m1\"\n").unwrap();
        let digest_before = provider_config_digest(&cfg_dir).unwrap();
        // Same byte length, different content.
        std::fs::write(cfg_dir.join("a.toml"), "model = \"m2\"\n").unwrap();
        let digest_after = provider_config_digest(&cfg_dir).unwrap();
        assert_ne!(
            digest_before, digest_after,
            "a same-size content edit must re-prove the boundary"
        );
        // A re-pointed member link is an identity change too.
        std::fs::write(dir.join("real.toml"), "x = 1\n").unwrap();
        std::os::unix::fs::symlink(dir.join("real.toml"), cfg_dir.join("b.toml")).unwrap();
        let linked = provider_config_digest(&cfg_dir).unwrap();
        std::fs::remove_file(cfg_dir.join("b.toml")).unwrap();
        std::fs::write(dir.join("other.toml"), "x = 1\n").unwrap();
        std::os::unix::fs::symlink(dir.join("other.toml"), cfg_dir.join("b.toml")).unwrap();
        assert_ne!(linked, provider_config_digest(&cfg_dir).unwrap());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A same-size edit inside a provider-config directory changes the
    /// content identity — the manifest hashes member bytes, not only
    /// names and sizes.
    #[test]
    fn a_same_size_dir_edit_changes_the_provider_config_digest() {
        let dir = tmp("samesize");
        let cfg_dir = dir.join("cfg");
        std::fs::create_dir_all(&cfg_dir).unwrap();
        let member = cfg_dir.join("provider.toml");
        std::fs::write(&member, "token = \"aaaa\"\n").unwrap();
        let d1 = provider_config_digest(&cfg_dir).unwrap();
        std::fs::write(&member, "token = \"bbbb\"\n").unwrap();
        let d2 = provider_config_digest(&cfg_dir).unwrap();
        assert_ne!(d1, d2, "same-size content edits move the digest");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The grant cache must not carry a stale scan: embedding the
    /// resolved credential into the provider config after the first
    /// resolution — even at identical size — re-runs the
    /// `contains_credential` check on the next `ensure`.
    #[test]
    fn a_post_resolution_provider_config_edit_re_runs_the_credential_scan() {
        let dir = tmp("restage");
        // SAFETY: test-only env mutation.
        unsafe { std::env::set_var("TAISAN_BOUNDARY_TEST_TOKEN3", "sekrit3") };
        let cfg_dir = dir.join("cfg");
        std::fs::create_dir_all(&cfg_dir).unwrap();
        let member = cfg_dir.join("provider.toml");
        std::fs::write(&member, "token = \"aaaaa\"\n").unwrap();
        let mut cfg = config(&dir);
        cfg.credential_ref = Some("env:TAISAN_BOUNDARY_TEST_TOKEN3".into());
        cfg.provider_config = Some(cfg_dir);
        let boundary = RuntimeBoundary::new(cfg);
        boundary.ensure().expect("a clean config resolves");
        // A same-size edit that embeds the credential.
        std::fs::write(&member, "token = \"sekrit3\"\n").unwrap();
        let err = boundary.ensure().unwrap_err();
        assert!(err.contains("provider_config.contains_credential"), "{err}");
        assert!(!err.contains("sekrit3"));
        unsafe { std::env::remove_var("TAISAN_BOUNDARY_TEST_TOKEN3") };
        let _ = std::fs::remove_dir_all(&dir);
    }
}
