//! The runtime credential/egress boundary every contained launch
//! shares (`docs/v0.3.md` §14.3–§14.4, `docs/installation-config.md`
//! credential references).
//!
//! Configuration names *references*, never values: a
//! `credential_ref` provider slot, an absolute non-secret provider
//! config path, and the bare model-API host set. The credential is
//! resolved and staged fresh for every contained launch and probe —
//! `env:`/`file:`/`command:` providers resolve through
//! [`crate::credentials`] at each `ensure`, so a rotated `file:` or
//! `command:` value is what the next Invocation observes — and the
//! resolved bytes are staged as a single `0600` file the mount plan
//! binds read-only at `/taisan/runtime-auth`. The provider config is
//! validated as the `taisan.worker.bindings/1` contract document
//! ([`crate::bindings`]) — present, regular, versioned, and free of
//! the credential value — before it is bound read-only at
//! `/taisan/provider-config`.
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
    /// Absolute path to the non-secret provider configuration —
    /// exactly one regular file satisfying the
    /// `taisan.worker.bindings/1` contract — mounted read-only.
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
    provider_config: Option<PathBuf>,
    /// The staged credential files this instance created — one
    /// unique path per grant. A grant's file must survive until its
    /// launch has mounted it, and the boundary cannot observe the
    /// mount, so owned files are removed only by this instance's
    /// `Drop`: a boundary that staged nothing removes nothing, and a
    /// later grant can never rewrite or unlink a file a pending
    /// launch is about to bind.
    staged: Vec<PathBuf>,
    /// The next per-grant staging suffix — the unique name is what
    /// keeps a later grant's restage from clobbering an earlier
    /// grant's pending mount.
    next_grant: u64,
    /// The proxy persists for the boundary's life: its allowlist is
    /// static configuration, so a provider-config edit never rebuilds
    /// the listener.
    proxy: Option<ConnectProxy>,
}

enum BoundaryState {
    Unresolved,
    Ready(BoundaryReady),
}

/// The lazily-resolved runtime boundary. `ensure` re-resolves the
/// credential and re-validates the provider contract on every call —
/// the latest staged credential is what each launch and probe mounts,
/// and no validation result is ever cached past a use. Only the proxy
/// is recorded: a failure starts no proxy, and a proxy that could not
/// start retries on the next call.
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

    /// Resolve the boundary for one contained launch or probe. Every
    /// call re-resolves the credential reference and re-validates the
    /// provider contract — the latest staged credential is what the
    /// launch mounts, and a post-resolution provider-config edit can
    /// never ride on a cached validation. `Err` is a typed refusal —
    /// `credential.*`, `provider_config.*`, or `egress.*` — the caller
    /// records it as a launch repair.
    pub fn ensure(&self) -> Result<BoundaryGrant, String> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        // A refused ensure still swept foreign staging files inside
        // `grant_locked`; a failed stage removes its own partial copy.
        // It never touches an earlier grant's file — that file may be
        // a pending launch's only credential surface.
        self.grant_locked(&mut state)
    }

    /// The per-call grant path under the state mutex: sweep foreign
    /// staging files, resolve the credential reference fresh, validate
    /// the provider contract against the resolved bytes, start the
    /// CONNECT proxy once when hosts are configured, and stage the
    /// credential under a unique per-grant name. Nothing is cached
    /// across calls except the proxy itself and the set of files this
    /// instance staged.
    fn grant_locked(&self, state: &mut BoundaryState) -> Result<BoundaryGrant, String> {
        {
            let owned: &[PathBuf] = match state {
                BoundaryState::Ready(ready) => &ready.staged,
                BoundaryState::Unresolved => &[],
            };
            self.sweep_foreign_staging(owned)?;
        }
        let resolved = match &self.config.credential_ref {
            None => None,
            Some(reference) => Some(credentials::resolve(reference).map_err(|e| e.to_string())?),
        };
        self.validate_provider_config(resolved.as_ref().map(|r| r.bytes()))?;
        if matches!(*state, BoundaryState::Unresolved) {
            *state = BoundaryState::Ready(BoundaryReady {
                provider_config: self.config.provider_config.clone(),
                staged: Vec::new(),
                next_grant: 0,
                proxy: self.start_proxy()?,
            });
        }
        let BoundaryState::Ready(ready) = state else {
            unreachable!("state resolved above")
        };
        let credential_file = match &resolved {
            None => None,
            Some(resolved) => Some(self.stage_credential(ready, resolved)?),
        };
        Ok(BoundaryGrant {
            credential_file,
            credential_source: resolved
                .as_ref()
                .and_then(|r| r.source_path().map(Path::to_path_buf)),
            provider_config: ready.provider_config.clone(),
            proxy_sock: ready
                .proxy
                .as_ref()
                .map(|proxy| proxy.sock_path().to_path_buf()),
        })
    }

    /// The CONNECT proxy, started the first time a grant needs the
    /// boundary and kept for its life — the allowlist is static
    /// configuration, so nothing later rebuilds the listener. An
    /// empty host set starts no proxy and grants no egress.
    fn start_proxy(&self) -> Result<Option<ConnectProxy>, String> {
        if self.config.model_api_hosts.is_empty() {
            return Ok(None);
        }
        let sock_dir = crate::egress::socket_dir(&self.config.egress_dir)
            .map_err(|e| format!("egress.proxy_unavailable: {e}"))?;
        let sock = sock_dir.join("worker.sock");
        let evidence = self.config.egress_dir.join("worker-egress.jsonl");
        ConnectProxy::start(&sock, self.config.model_api_hosts.clone(), &evidence)
            .map(Some)
            .map_err(|e| format!("egress.proxy_unavailable: {e}"))
    }

    /// Stage the resolved credential as an owner-only file unique to
    /// this grant — the mount plan binds it read-only. The value
    /// exists only in this file and the provider's own storage, never
    /// argv, logs, or evidence. The unique name is the load-bearing
    /// property: a later grant stages a *new* file rather than
    /// rewriting the path a pending launch is about to bind, and the
    /// recorded path is what this instance's `Drop` removes.
    fn stage_credential(
        &self,
        ready: &mut BoundaryReady,
        credential: &ResolvedCredential,
    ) -> Result<PathBuf, String> {
        std::fs::create_dir_all(&self.config.runtime_dir).map_err(|e| {
            format!(
                "credential.stage_failed: create {}: {e}",
                self.config.runtime_dir.display()
            )
        })?;
        let tag = ready.next_grant;
        ready.next_grant += 1;
        let path = self.config.runtime_dir.join(format!("runtime-auth.{tag}"));
        let tmp = self
            .config
            .runtime_dir
            .join(format!("runtime-auth.{tag}.tmp"));
        match write_staged_credential(&tmp, &path, credential) {
            Ok(()) => {
                ready.staged.push(path.clone());
                Ok(path)
            }
            // The staging sibling carries the same secret bytes the
            // moment it is created: a failure past that point must
            // not leave the copy at its predictable path.
            Err(e) => {
                let _ = std::fs::remove_file(&tmp);
                Err(e)
            }
        }
    }

    /// Remove staging files in `runtime_dir` this instance does not
    /// own — a crashed supervisor's `runtime-auth*` leftovers are
    /// stale secret material and every resolve sweeps them. Owned
    /// files are skipped: a grant's file is a live credential surface
    /// until its launch binds it. Only regular files and links can
    /// hold the secret — a same-named directory is an obstruction the
    /// stage itself refuses on, not secret material. An already-absent
    /// file is not an error; a real removal failure refuses the
    /// resolve — the boundary cannot certify a clean staging dir it
    /// failed to clean.
    fn sweep_foreign_staging(&self, owned: &[PathBuf]) -> Result<(), String> {
        let entries = match std::fs::read_dir(&self.config.runtime_dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(format!("credential.unstage_failed: {e}")),
        };
        let mut failed = None;
        for entry in entries {
            let entry = entry.map_err(|e| format!("credential.unstage_failed: {e}"))?;
            if !entry
                .file_name()
                .to_string_lossy()
                .starts_with("runtime-auth")
            {
                continue;
            }
            let path = entry.path();
            if owned.contains(&path) {
                continue;
            }
            match entry.file_type() {
                Ok(t) if t.is_file() || t.is_symlink() => {}
                Ok(_) => continue,
                Err(e) => return Err(format!("credential.unstage_failed: {e}")),
            }
            match std::fs::remove_file(&path) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => {
                    if failed.is_none() {
                        failed = Some(format!("credential.unstage_failed: {e}"));
                    }
                }
            }
        }
        failed.map_or(Ok(()), Err)
    }

    /// The provider config must exist and satisfy the
    /// `taisan.worker.bindings/1` contract — a regular bounded file
    /// with the exact format version and the required non-secret
    /// members — and carry no credential value. A secret embedded
    /// there is a configuration error, never a tolerated convenience
    /// (`docs/runtime-bindings.md`, `docs/installation-config.md`).
    /// Validation runs on every `ensure`, so an edit between calls is
    /// always re-checked against the freshly resolved credential.
    fn validate_provider_config(&self, credential_bytes: Option<&[u8]>) -> Result<(), String> {
        match &self.config.provider_config {
            None => Ok(()),
            Some(path) => crate::bindings::validate(path, credential_bytes),
        }
    }
}

impl Drop for RuntimeBoundary {
    fn drop(&mut self) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let BoundaryState::Ready(ready) = &mut *state else {
            // A boundary that never resolved staged nothing and owns
            // nothing — dropping it must not unlink another
            // supervisor's credential at the shared runtime dir (a
            // `run` that loses the lease drops a boundary that was
            // constructed but never ensured).
            return;
        };
        if let Some(proxy) = ready.proxy.take() {
            proxy.shutdown();
        }
        // The staged credential's lifetime is its grant's — and only
        // this instance's grants: with the boundary gone, its own
        // staged files are stale secret material.
        for path in &ready.staged {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// Write the credential to `tmp` owner-only, fsync it, and rename it
/// over `path`. On `Err` the caller owns removing `tmp` — it may
/// already hold a partial secret copy.
fn write_staged_credential(
    tmp: &Path,
    path: &Path,
    credential: &ResolvedCredential,
) -> Result<(), String> {
    {
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(tmp)
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
    std::fs::rename(tmp, path).map_err(|e| format!("credential.stage_failed: {e}"))
}

/// Bounded digest of one file's content: at most the contract's own
/// size bound is read, through fixed-size chunks, so a same-size edit
/// anywhere in an admitted file changes the identity without
/// buffering the file. A larger source cannot be a valid contract —
/// `bindings::validate` refuses it unread — so the identity fails
/// the same way instead of hashing an unbounded file before the
/// refusal runs.
fn file_content_digest(path: &Path) -> Result<String, std::io::Error> {
    use std::io::Read;
    if std::fs::metadata(path)?.len() > crate::bindings::MAX_CONTRACT_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "provider config exceeds the contract size bound",
        ));
    }
    let mut file = std::fs::File::open(path)?;
    let mut chunks = String::new();
    let mut total = 0u64;
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        total += n as u64;
        // The metadata check and the read race: a file that grew
        // between them is still refused at the bound.
        if total > crate::bindings::MAX_CONTRACT_BYTES {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "provider config exceeds the contract size bound",
            ));
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

    /// Every `runtime-auth*` entry — staged grants and staging
    /// siblings — currently in the staging dir.
    fn staged_entries(dir: &Path) -> Vec<PathBuf> {
        let runtime = dir.join("runtime");
        let Ok(entries) = std::fs::read_dir(&runtime) else {
            return vec![];
        };
        entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .is_some_and(|n| n.to_string_lossy().starts_with("runtime-auth"))
            })
            .collect()
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
        let dirty = dir.join("provider.json");
        std::fs::write(
            &dirty,
            "{\"format_version\":\"taisan.worker.bindings/1\",\"model\":\"sekrit\"}\n",
        )
        .unwrap();
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
        let clean = dir.join("provider.json");
        std::fs::write(
            &clean,
            "{\"format_version\":\"taisan.worker.bindings/1\",\"model\":\"m\"}\n",
        )
        .unwrap();
        std::fs::set_permissions(&clean, std::fs::Permissions::from_mode(0o644)).unwrap();
        let mut cfg = config(&dir);
        cfg.provider_config = Some(clean.clone());
        let boundary = RuntimeBoundary::new(cfg);
        let grant = boundary.ensure().unwrap();
        assert_eq!(grant.provider_config, Some(clean));
        // Digest is stable for identical content and changes on edit.
        let d1 = boundary.identity_digest().unwrap();
        std::fs::write(
            dir.join("provider.json"),
            "{\"format_version\":\"taisan.worker.bindings/1\",\"model\":\"m2\"}\n",
        )
        .unwrap();
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

    /// Validation never rides on a cached scan: embedding the
    /// resolved credential into the provider config after the first
    /// grant re-runs the `contains_credential` check on the next
    /// `ensure`.
    #[test]
    fn a_post_resolution_provider_config_edit_re_runs_the_credential_scan() {
        let dir = tmp("restage");
        // SAFETY: test-only env mutation.
        unsafe { std::env::set_var("TAISAN_BOUNDARY_TEST_TOKEN3", "sekrit3") };
        let member = dir.join("provider.json");
        std::fs::write(
            &member,
            "{\"format_version\":\"taisan.worker.bindings/1\",\"model\":\"aaaaa\"}\n",
        )
        .unwrap();
        let mut cfg = config(&dir);
        cfg.credential_ref = Some("env:TAISAN_BOUNDARY_TEST_TOKEN3".into());
        cfg.provider_config = Some(member.clone());
        let boundary = RuntimeBoundary::new(cfg);
        boundary.ensure().expect("a clean config resolves");
        // An edit that embeds the credential.
        std::fs::write(
            &member,
            "{\"format_version\":\"taisan.worker.bindings/1\",\"model\":\"sekrit3\"}\n",
        )
        .unwrap();
        let err = boundary.ensure().unwrap_err();
        assert!(err.contains("provider_config.contains_credential"), "{err}");
        assert!(!err.contains("sekrit3"));
        unsafe { std::env::remove_var("TAISAN_BOUNDARY_TEST_TOKEN3") };
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A refusal after the credential was staged — here a provider
    /// config embedding the resolved value — must unlink the staged
    /// file rather than leave it at its predictable path.
    #[test]
    fn a_refused_resolve_removes_the_staged_credential() {
        let dir = tmp("refuse");
        // SAFETY: test-only env mutation.
        unsafe { std::env::set_var("TAISAN_BOUNDARY_TEST_TOKEN4", "sekrit4") };
        let dirty = dir.join("provider.json");
        std::fs::write(
            &dirty,
            "{\"format_version\":\"taisan.worker.bindings/1\",\"model\":\"sekrit4\"}\n",
        )
        .unwrap();
        let mut cfg = config(&dir);
        cfg.credential_ref = Some("env:TAISAN_BOUNDARY_TEST_TOKEN4".into());
        cfg.provider_config = Some(dirty);
        let boundary = RuntimeBoundary::new(cfg);
        let err = boundary.ensure().unwrap_err();
        assert!(err.contains("provider_config.contains_credential"), "{err}");
        assert!(
            staged_entries(&dir).is_empty(),
            "a refused resolve must not leave the staged credential"
        );
        unsafe { std::env::remove_var("TAISAN_BOUNDARY_TEST_TOKEN4") };
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A resolve carrying no `credential_ref` still owns the staging
    /// dir: a file a crashed predecessor left behind — or a staging
    /// sibling a killed stage orphaned — is stale secret material and
    /// is swept.
    #[test]
    fn a_no_credential_resolve_unlinks_an_earlier_grants_file() {
        let dir = tmp("unstage");
        // SAFETY: test-only env mutation.
        unsafe { std::env::set_var("TAISAN_BOUNDARY_TEST_TOKEN5", "sekrit5") };
        let mut cfg = config(&dir);
        cfg.credential_ref = Some("env:TAISAN_BOUNDARY_TEST_TOKEN5".into());
        let boundary = RuntimeBoundary::new(cfg);
        boundary.ensure().unwrap();
        let staged = dir.join("runtime/runtime-auth.0");
        assert!(staged.exists(), "the first grant staged the file");
        // Forget the boundary so its file survives like a crashed
        // supervisor's — Drop would remove it. A tmp orphaned
        // mid-stage is the same stale secret material.
        std::mem::forget(boundary);
        let orphan = dir.join("runtime/runtime-auth.tmp");
        std::fs::write(&orphan, "partial-secret").unwrap();
        let boundary2 = RuntimeBoundary::new(config(&dir));
        boundary2.ensure().unwrap();
        assert!(!staged.exists(), "the stale staged file is unlinked");
        assert!(!orphan.exists(), "the orphaned staging file is unlinked");
        assert!(staged_entries(&dir).is_empty());
        unsafe { std::env::remove_var("TAISAN_BOUNDARY_TEST_TOKEN5") };
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A stage that fails after the tmp file carries secret bytes —
    /// here the rename refuses because the staged name is a directory —
    /// must remove the partial copy rather than leave it at the
    /// predictable staging path.
    #[test]
    fn a_failed_stage_removes_the_tmp_credential() {
        let dir = tmp("stagefail");
        // SAFETY: test-only env mutation.
        unsafe { std::env::set_var("TAISAN_BOUNDARY_TEST_TOKEN8", "sekrit8") };
        std::fs::create_dir_all(dir.join("runtime/runtime-auth.0")).unwrap();
        let mut cfg = config(&dir);
        cfg.credential_ref = Some("env:TAISAN_BOUNDARY_TEST_TOKEN8".into());
        let boundary = RuntimeBoundary::new(cfg);
        let err = boundary.ensure().unwrap_err();
        assert!(err.contains("credential.stage_failed"), "{err}");
        assert!(
            !dir.join("runtime/runtime-auth.0.tmp").exists(),
            "a failed stage must not leave the tmp credential copy"
        );
        unsafe { std::env::remove_var("TAISAN_BOUNDARY_TEST_TOKEN8") };
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A failed stale re-resolve never dismantles an earlier grant:
    /// the first grant's file is a pending launch's credential
    /// surface until it mounts, so a refusal leaves it untouched.
    /// Once the provider config recovers, the next `ensure` resolves
    /// from scratch and hands out a new grant whose staged file
    /// actually exists.
    #[test]
    fn a_failed_reresolve_does_not_serve_a_dismantled_grant() {
        let dir = tmp("dismantle");
        // SAFETY: test-only env mutation.
        unsafe { std::env::set_var("TAISAN_BOUNDARY_TEST_TOKEN7", "sekrit7") };
        let member = dir.join("provider.json");
        std::fs::write(
            &member,
            "{\"format_version\":\"taisan.worker.bindings/1\",\"model\":\"aaaaaa\"}\n",
        )
        .unwrap();
        let mut cfg = config(&dir);
        cfg.credential_ref = Some("env:TAISAN_BOUNDARY_TEST_TOKEN7".into());
        cfg.provider_config = Some(member.clone());
        let boundary = RuntimeBoundary::new(cfg);
        let first = boundary.ensure().expect("a clean config resolves");
        let first_file = first.credential_file.expect("the first grant stages");
        // A re-validate that fails past resolution must not touch the
        // earlier grant's file — a pending launch may still bind it.
        std::fs::write(
            &member,
            "{\"format_version\":\"taisan.worker.bindings/1\",\"model\":\"sekrit7\"}\n",
        )
        .unwrap();
        let err = boundary.ensure().unwrap_err();
        assert!(err.contains("provider_config.contains_credential"), "{err}");
        assert_eq!(
            std::fs::read(&first_file).unwrap(),
            b"sekrit7",
            "a refused re-resolve leaves the earlier grant's file intact"
        );
        // Restoring the original content resolves again: the new grant
        // carries a live staged file of its own, never a dismantled
        // grant's dead path.
        std::fs::write(
            &member,
            "{\"format_version\":\"taisan.worker.bindings/1\",\"model\":\"aaaaaa\"}\n",
        )
        .unwrap();
        let grant = boundary.ensure().unwrap();
        let staged = grant.credential_file.expect("credential restaged");
        assert_ne!(staged, first_file, "each grant stages its own file");
        assert_eq!(std::fs::read(&staged).unwrap(), b"sekrit7");
        unsafe { std::env::remove_var("TAISAN_BOUNDARY_TEST_TOKEN7") };
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The staged credential's lifetime is the grant's: dropping the
    /// boundary removes the file along with the proxy.
    #[test]
    fn dropping_the_boundary_removes_the_staged_credential() {
        let dir = tmp("drop");
        // SAFETY: test-only env mutation.
        unsafe { std::env::set_var("TAISAN_BOUNDARY_TEST_TOKEN6", "sekrit6") };
        let mut cfg = config(&dir);
        cfg.credential_ref = Some("env:TAISAN_BOUNDARY_TEST_TOKEN6".into());
        let boundary = RuntimeBoundary::new(cfg);
        let grant = boundary.ensure().unwrap();
        let staged = grant.credential_file.expect("credential staged");
        assert!(staged.exists());
        drop(boundary);
        assert!(!staged.exists(), "drop unlinks the staged credential");
        unsafe { std::env::remove_var("TAISAN_BOUNDARY_TEST_TOKEN6") };
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A `run` that loses the lease drops a boundary it constructed
    /// but never resolved against the same workspace staging dir —
    /// its drop must not unlink the active supervisor's staged
    /// credential.
    #[test]
    fn a_never_resolved_boundarys_drop_keeps_the_holders_credential() {
        let dir = tmp("contender");
        // SAFETY: test-only env mutation.
        unsafe { std::env::set_var("TAISAN_BOUNDARY_TEST_TOKEN9", "sekrit9") };
        let mut cfg = config(&dir);
        cfg.credential_ref = Some("env:TAISAN_BOUNDARY_TEST_TOKEN9".into());
        let holder = RuntimeBoundary::new(cfg.clone());
        let grant = holder.ensure().unwrap();
        let staged = grant.credential_file.expect("the holder staged");
        assert!(staged.exists());
        // The lease contender shares the staging dir and drops
        // without ever resolving — as `run` does on `lock.held`.
        let contender = RuntimeBoundary::new(cfg);
        drop(contender);
        assert!(
            staged.exists(),
            "a contender's drop must not unlink the holder's staged credential"
        );
        assert_eq!(std::fs::read(&staged).unwrap(), b"sekrit9");
        unsafe { std::env::remove_var("TAISAN_BOUNDARY_TEST_TOKEN9") };
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Each grant stages its own file: a later grant — including one
    /// that observes a rotated provider value — can never rewrite the
    /// path a pending launch is about to bind, and a refused
    /// re-resolve never unlinks it either.
    #[test]
    fn a_later_grant_never_rewrites_a_pending_grants_file() {
        let dir = tmp("pergrant");
        let source = dir.join("token");
        std::fs::write(&source, "credential-v1").unwrap();
        std::fs::set_permissions(&source, std::fs::Permissions::from_mode(0o600)).unwrap();
        let mut cfg = config(&dir);
        cfg.credential_ref = Some(format!("file:{}", source.display()));
        let boundary = RuntimeBoundary::new(cfg);
        let first = boundary.ensure().unwrap();
        let first_file = first.credential_file.expect("the first grant stages");
        // The provider value rotates before the first grant's launch
        // has mounted its file: the second grant must observe v2 at a
        // NEW path, leaving the pending mount's file untouched.
        std::fs::write(&source, "credential-v2").unwrap();
        let second = boundary.ensure().unwrap();
        let second_file = second.credential_file.expect("the second grant stages");
        assert_ne!(first_file, second_file, "each grant stages its own file");
        assert_eq!(
            std::fs::read(&first_file).unwrap(),
            b"credential-v1",
            "the pending grant's file keeps the content it was granted"
        );
        assert_eq!(std::fs::read(&second_file).unwrap(), b"credential-v2");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The identity digest reads at most the contract's own size
    /// bound: an oversized provider source — file or directory
    /// member — fails the identity the same way validation refuses
    /// it, instead of being hashed unbounded before the refusal runs.
    #[test]
    fn an_oversized_provider_config_fails_the_bounded_identity_read() {
        let dir = tmp("oversize");
        let big = dir.join("provider.json");
        std::fs::write(
            &big,
            vec![b'x'; crate::bindings::MAX_CONTRACT_BYTES as usize + 1],
        )
        .unwrap();
        assert!(provider_config_digest(&big).is_err());
        let mut cfg = config(&dir);
        cfg.provider_config = Some(big);
        let boundary = RuntimeBoundary::new(cfg);
        let err = boundary.identity_digest().unwrap_err();
        assert!(err.contains("provider_config.unreadable"), "{err}");
        // An oversized member of a directory config fails the same
        // bounded read — the entry cap does not bound member bytes.
        let cfg_dir = dir.join("provider.d");
        std::fs::create_dir_all(&cfg_dir).unwrap();
        std::fs::write(
            cfg_dir.join("big.json"),
            vec![b'x'; crate::bindings::MAX_CONTRACT_BYTES as usize + 1],
        )
        .unwrap();
        assert!(provider_config_digest(&cfg_dir).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
