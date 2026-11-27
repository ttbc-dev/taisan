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
use std::sync::atomic::{AtomicU64, Ordering};

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

/// How many of the newest staged grant files a boundary keeps. A
/// grant's file is live only until its launch's mount — every caller
/// issues exactly one spawn promptly after `ensure` returns, and the
/// mount consumes the path at exec — so the set of grants whose mount
/// could still be pending is far smaller than the bound. Older files
/// are stale secret material (including rotated provider values) and
/// are unlinked at the next stage rather than at process exit; a
/// launch stalled across this many grant cycles fails closed on the
/// missing `--ro-bind` source, never on a substituted file.
const RETAINED_GRANT_FILES: usize = 8;

/// A staged grant file and the lock proving this instance owns it:
/// the boundary holds an exclusive `flock` on every file it stages
/// from before the name is published until its own `Drop` unlinks
/// the file, so any boundary sweeping the shared staging dir can
/// always prove whether the file still has a live owner — a proof
/// no pid or namespace assumption can falsify.
struct StagedFile {
    path: PathBuf,
    _lock: std::fs::File,
}

struct BoundaryReady {
    provider_config: Option<PathBuf>,
    /// The staged credential files this instance created and still
    /// owns — one unique owner-tagged path per grant, bounded to the
    /// newest [`RETAINED_GRANT_FILES`]. A later grant can never
    /// rewrite a pending grant's path, and only the retention bound
    /// unlinks owned files before this instance's `Drop`: a launch
    /// stalled across more than the bound's grant cycles loses its
    /// staged file and fails closed on the missing `--ro-bind`
    /// source. A boundary that staged nothing removes nothing.
    staged: Vec<StagedFile>,
    /// The next per-grant staging seq inside this instance's owner
    /// tag — the unique name is what keeps a later grant's restage
    /// from clobbering an earlier grant's pending mount.
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
    /// This instance's staging-name owner tag —
    /// `runtime-auth.p<pid>.i<instance>.<seq>` — so every file it
    /// stages is attributable to exactly one owner: two boundaries
    /// sharing the staging dir can never stage or drop the same name,
    /// and the tag identifies the owning process to anyone reading
    /// the dir. The `p`/`i` prefixes also keep the tagged shape out
    /// of any purely numeric name an operator might use.
    owner_tag: String,
    state: Mutex<BoundaryState>,
}

impl RuntimeBoundary {
    pub fn new(config: BoundaryConfig) -> Self {
        static INSTANCE: AtomicU64 = AtomicU64::new(0);
        Self {
            config,
            owner_tag: format!(
                "p{}.i{}",
                std::process::id(),
                INSTANCE.fetch_add(1, Ordering::Relaxed)
            ),
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
        // A refused ensure still swept stale staging residue inside
        // `grant_locked`; a failed stage removes its own partial copy.
        // It never touches an earlier grant's file — that file may be
        // a pending launch's only credential surface — a live
        // neighbour's file, nor a path the configuration itself names.
        self.grant_locked(&mut state)
    }

    /// The per-call grant path under the state mutex: sweep stale
    /// staging residue, resolve the credential reference fresh, validate
    /// the provider contract against the resolved bytes, start the
    /// CONNECT proxy once when hosts are configured, and stage the
    /// credential under a unique per-grant name. Nothing is cached
    /// across calls except the proxy itself and the set of files this
    /// instance staged.
    fn grant_locked(&self, state: &mut BoundaryState) -> Result<BoundaryGrant, String> {
        {
            let owned: Vec<&Path> = match state {
                BoundaryState::Ready(ready) => {
                    ready.staged.iter().map(|s| s.path.as_path()).collect()
                }
                BoundaryState::Unresolved => vec![],
            };
            self.sweep_foreign_staging(&owned, &self.configured_paths())?;
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

    /// The staging name one grant is published at —
    /// `runtime-auth.p<pid>.i<instance>.<seq>` — carrying this
    /// boundary's owner tag so the file is attributable to exactly
    /// one owner: no other instance can ever stage a name this
    /// boundary tracks, and the tag names the process a reader of
    /// the staging dir can hold responsible.
    fn staging_name(&self, seq: u64) -> String {
        format!("runtime-auth.{}.{}", self.owner_tag, seq)
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
        // A staging name that exists after the sweep is not ours to
        // overwrite — it is a configured input the sweep deliberately
        // kept, or a live owner's file. Owner tags are only *usually*
        // unique (two pid namespaces can mint the same `p<pid>.i<instance>`
        // tag), so existence is not proof a name is foreign: publish is
        // no-clobber (`write_staged_credential` links rather than
        // renames over an existing name) and an occupied name — whether
        // found here or raced past the check — costs a seq, never a
        // neighbour's file. The `.tmp` sibling carries the same secret
        // bytes the moment it is created, so it is checked as well —
        // and a sibling a concurrent sweep reclaimed before this
        // stage's claim was observable loses the race the same way:
        // publish then finds the `.tmp` gone — or the claim itself
        // meets the sweep's still-held transient probe lock — also a
        // seq, never a launch failure. Past a bounded run of
        // collisions the stage refuses rather than scan forever.
        let mut seq = ready.next_grant;
        loop {
            let name = self.staging_name(seq);
            let path = self.config.runtime_dir.join(&name);
            let tmp = self.config.runtime_dir.join(format!("{name}.tmp"));
            seq += 1;
            if seq - ready.next_grant > 64 {
                return Err("credential.stage_failed: no free staging name".into());
            }
            // Fast skip past an occupied name; publish is no-clobber
            // regardless, so a stale check can only cost a seq.
            if std::fs::symlink_metadata(&path).is_ok() || std::fs::symlink_metadata(&tmp).is_ok() {
                continue;
            }
            match write_staged_credential(&tmp, &path, credential) {
                Ok(lock) => {
                    ready.next_grant = seq;
                    ready.staged.push(StagedFile {
                        path: path.clone(),
                        _lock: lock,
                    });
                    // Retention: only the newest grants can still have
                    // a pending mount, so files past the bound are
                    // stale secret material. A failed unlink still
                    // drops ownership — and dropping the lock fd
                    // releases the claim, so the next ensure's sweep
                    // reclaims the entry (staging-shaped, unclaimed,
                    // and never configured).
                    while ready.staged.len() > RETAINED_GRANT_FILES {
                        let evicted = ready.staged.remove(0);
                        let _ = std::fs::remove_file(&evicted.path);
                    }
                    return Ok(path);
                }
                // Occupied means the name — or its `.tmp` sibling —
                // proved foreign, or lost the create→flock window to
                // a sweep's claim probe; any still-empty sibling left
                // behind is unclaimed residue the sweep owns, never
                // ours to unlink.
                Err(StageError::Occupied) => {}
                // The staging sibling carries the same secret bytes
                // the moment it is created: a failure past that point
                // must not leave the copy at its predictable path.
                Err(StageError::Failed(e)) => {
                    let _ = std::fs::remove_file(&tmp);
                    return Err(e);
                }
            }
        }
    }

    /// The canonical paths the configuration names — the provider
    /// config and a `file:` credential's source — which the sweep
    /// must never unlink. Either may sit inside `runtime_dir` under
    /// a `runtime-auth*` name: the prefix is not reserved, and a
    /// deleted operator file cannot be restored by retry. The `file:`
    /// source comes from the reference's shape parse — not the
    /// resolution — so the sweep can protect it before resolve runs;
    /// `resolve_file` canonicalizes the same path, so the sets agree.
    fn configured_paths(&self) -> Vec<PathBuf> {
        let source = self
            .config
            .credential_ref
            .as_deref()
            .and_then(|reference| {
                workloop_core::credential::CredentialProvider::parse_str(reference).ok()
            })
            .and_then(|provider| match provider {
                workloop_core::credential::CredentialProvider::File { path } => Some(path),
                _ => None,
            });
        [self.config.provider_config.clone(), source]
            .into_iter()
            .flatten()
            .filter_map(|path| path.canonicalize().ok())
            .collect()
    }

    /// Remove staging residue in `runtime_dir` no live boundary can
    /// still own — a crashed supervisor's `runtime-auth*` leftovers
    /// are stale secret material and every resolve sweeps them. Owned
    /// files are skipped: a grant's file is a live credential surface
    /// until its launch binds it. A staging-shaped entry is
    /// reclaimable only when no live owner claims it: a boundary
    /// holds an exclusive `flock` on every file it stages from before
    /// the name is published until its own `Drop` unlinks it, so a
    /// lock that cannot be taken proves a live owner — in any pid
    /// namespace — while an acquired lock proves unclaimed residue
    /// (a dead owner's leftovers, or an unattributed legacy shape
    /// `runtime-auth` / `runtime-auth.<seq>` a pre-owner-tag binary
    /// left). A path the resolved configuration names — the provider
    /// config, a `file:` credential's source — is never removed
    /// either: `.runtime` is operator-writable, the prefix is not
    /// reserved, and ambiguity always resolves toward keeping the
    /// file. Only regular files and links can hold the secret — a
    /// same-named directory is an obstruction the stage itself
    /// refuses on, not secret material. An already-absent file is not
    /// an error; a real removal failure refuses the resolve — the
    /// boundary cannot certify a clean staging dir it failed to
    /// clean.
    fn sweep_foreign_staging(&self, owned: &[&Path], protected: &[PathBuf]) -> Result<(), String> {
        let entries = match std::fs::read_dir(&self.config.runtime_dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(format!("credential.unstage_failed: {e}")),
        };
        let owned: std::collections::HashSet<&Path> = owned.iter().copied().collect();
        let mut failed = None;
        for entry in entries {
            let entry = entry.map_err(|e| format!("credential.unstage_failed: {e}"))?;
            if !is_staging_name(&entry.file_name().to_string_lossy()) {
                continue;
            }
            let path = entry.path();
            if owned.contains(path.as_path()) {
                continue;
            }
            if path
                .canonicalize()
                .is_ok_and(|canon| protected.contains(&canon))
            {
                continue;
            }
            match entry.file_type() {
                Ok(t) if t.is_file() || t.is_symlink() => {}
                Ok(_) => continue,
                Err(e) => return Err(format!("credential.unstage_failed: {e}")),
            }
            // A live owner holds its flock from before publication
            // until its own Drop — a claimed file is never ours to
            // remove.
            if staging_file_is_claimed(&path) {
                continue;
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
        // staged files are stale secret material. The owner tag in
        // each tracked name means the set can never name another
        // boundary's file, and dropping each lock descriptor after
        // the unlink releases the claim a neighbour's sweep probes.
        for staged in &ready.staged {
            let _ = std::fs::remove_file(&staged.path);
        }
    }
}

/// Whether a `runtime_dir` entry name is one of the boundary's
/// staging shapes — the owner-tagged
/// `runtime-auth.p<pid>.i<instance>.<seq>` and its `.tmp` sibling, or
/// the pre-owner-tag `runtime-auth` / `runtime-auth.<seq>` shapes an
/// older supervisor may have left. The `p`/`i` prefixes are
/// load-bearing: a purely numeric three-part name — a versioned
/// operator file like `runtime-auth.1.2.3` — can never match the
/// tagged shape, so it is operator content the sweep must never
/// treat as stale staging, like any other `runtime-auth*` name that
/// is not one of these shapes.
pub(crate) fn is_staging_name(name: &str) -> bool {
    let base = name.strip_suffix(".tmp").unwrap_or(name);
    if base == "runtime-auth" {
        return true;
    }
    let Some(rest) = base.strip_prefix("runtime-auth.") else {
        return false;
    };
    let all_digits = |part: &str| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit());
    let prefixed = |prefix: u8, part: &str| {
        part.len() > 1 && part.as_bytes()[0] == prefix && all_digits(&part[1..])
    };
    match rest.split('.').count() {
        // runtime-auth.<seq> — the pre-owner-tag per-grant shape.
        1 => all_digits(rest),
        // runtime-auth.p<pid>.i<instance>.<seq> — owner-tagged.
        3 => {
            let mut parts = rest.split('.');
            parts.next().is_some_and(|p| prefixed(b'p', p))
                && parts.next().is_some_and(|i| prefixed(b'i', i))
                && parts.next().is_some_and(all_digits)
        }
        _ => false,
    }
}

/// Whether a staging-shaped file is currently claimed by a live
/// owner. A boundary holds an exclusive `flock` on every file it
/// stages from before the name is published until its own `Drop`
/// unlinks it, so a lock that cannot be taken proves a live owner —
/// in any pid namespace — and an acquired lock proves unclaimed
/// residue: a dead owner's locks died with it, and unattributed
/// legacy shapes were never locked at all. The probe fails toward
/// keeping: a file that cannot be opened — foreign permissions — or
/// whose lock probe errors is never removed, and a symlink is
/// unclaimed by definition (it cannot carry our lock), matching how
/// the sweep has always treated link-shaped residue.
fn staging_file_is_claimed(path: &Path) -> bool {
    use std::os::unix::fs::OpenOptionsExt;
    use std::os::unix::io::AsRawFd;
    let file = match std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
    {
        Ok(file) => file,
        Err(e) if e.raw_os_error() == Some(libc::ELOOP) => return false,
        Err(_) => return true,
    };
    // A lock we actually took proves the file unclaimed; anything
    // else — EWOULDBLOCK from a live owner, or a probe error — keeps
    // it.
    (unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) }) != 0
}

/// Why a stage attempt did not publish: `Occupied` means the name
/// proved foreign at publish time — or its `.tmp` sibling lost the
/// create→flock window to a neighbour's sweep, whether the sweep
/// unlinked the sibling outright or its transient claim-probe still
/// held the sibling's lock when the stage tried to claim it (the
/// caller tries the next seq; nothing of ours was published or left
/// behind), `Failed` carries the refusal for a real error.
enum StageError {
    Occupied,
    Failed(String),
}

/// Take the ownership claim on the freshly-created staging sibling:
/// the lock lands before any secret byte so a sweep's claim probe
/// always finds the file owned once the claim is observable.
/// `EWOULDBLOCK` on a name `create_new` just made can come only from
/// a concurrent sweep's transient probe holding the inode's lock in
/// the create→flock window — the same lost race as a sibling the
/// sweep unlinked outright, so it is `Occupied` (a seq), never a
/// stage failure. Any other error is a real refusal: a filesystem
/// that cannot carry the claim cannot carry the ownership guarantee.
fn claim_staged_file(file: &std::fs::File, tmp: &Path) -> Result<(), StageError> {
    use std::os::unix::io::AsRawFd;
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
        return Ok(());
    }
    let err = std::io::Error::last_os_error();
    if err.raw_os_error() == Some(libc::EWOULDBLOCK) {
        return Err(StageError::Occupied);
    }
    Err(StageError::Failed(format!(
        "credential.stage_failed: flock {}: {err}",
        tmp.display()
    )))
}

/// Write the credential to `tmp` owner-only, fsync it, and publish it
/// under `path` *without clobbering*, returning the descriptor
/// holding the ownership claim: the file is `flock`-claimed before it
/// carries any secret and the caller keeps the descriptor as the
/// file's live-owner proof until this boundary's `Drop` unlinks it.
/// Publish is `link` + `unlink`, never `rename` — an existing name
/// fails `EEXIST` instead of being overwritten, so a same-tagged
/// neighbour (owner tags can collide across pid namespaces) racing
/// the publish loses a seq rather than having its live grant swapped
/// out from under it. A neighbour's sweep can also race `tmp` itself
/// in the window before this file's flock is observable — reclaiming
/// the sibling outright, or still holding its transient claim-probe
/// lock when this stage's claim tries to land — so both a vanished
/// sibling and a probe-blocked claim are the same lost race
/// (`Occupied`), never a stage failure. On `Err(Failed)` the
/// caller owns removing `tmp` — it may already hold a partial secret
/// copy. On `Err(Occupied)` the contested name is left exactly as
/// found: the sweep that probed the still-unclaimed sibling reclaims
/// the name itself — and unlinking here could instead remove a
/// sibling a neighbour recreated under the same name, which is never
/// ours — while a sweeper that dies first leaves residue the next
/// sweep reclaims.
fn write_staged_credential(
    tmp: &Path,
    path: &Path,
    credential: &ResolvedCredential,
) -> Result<std::fs::File, StageError> {
    let fail = |e: std::io::Error| StageError::Failed(format!("credential.stage_failed: {e}"));
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(tmp)
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::AlreadyExists {
                StageError::Occupied
            } else {
                fail(e)
            }
        })?;
    claim_and_write(file, tmp, path, credential)
}

/// Claim `file` and carry it from fresh sibling to published grant:
/// flock before the first secret byte, then write, fsync, and
/// publish. Split from `write_staged_credential` so a test can drive
/// the create→flock window directly. An `Occupied` claim never
/// unlinks `tmp` — the name may already belong to a recreated
/// sibling that is not ours.
fn claim_and_write(
    mut file: std::fs::File,
    tmp: &Path,
    path: &Path,
    credential: &ResolvedCredential,
) -> Result<std::fs::File, StageError> {
    let fail = |e: std::io::Error| StageError::Failed(format!("credential.stage_failed: {e}"));
    // The claim precedes the content: a neighbour's sweep may only
    // reclaim an *unclaimed* staging shape, so the lock is taken
    // before any secret byte lands.
    claim_staged_file(&file, tmp)?;
    {
        use std::io::Write;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(std::fs::Permissions::from_mode(0o600))
                .map_err(fail)?;
        }
        file.write_all(credential.bytes())
            .and_then(|()| file.sync_all())
            .map_err(fail)?;
    }
    publish_staged(tmp, path).map(|()| file)
}

/// Publish `tmp` under `path` *without clobbering*: `hard_link` gives
/// `path` a second name for the same inode atomically, failing EEXIST
/// on any existing entry — the owner's lock rides the inode's open
/// file description, so the published name is claimed from its first
/// instant. ENOENT is the same lost race from the other side: a
/// neighbour's sweep reclaimed the still-unclaimed sibling in the
/// window between `create_new` and the flock that made the claim
/// observable (a sweep that probed in the window may unlink the name
/// later), so nothing of ours was published — `Occupied`, a seq, not
/// a stage failure. `tmp` is removed on every non-`Failed` outcome;
/// on `Failed` the caller owns the removal.
fn publish_staged(tmp: &Path, path: &Path) -> Result<(), StageError> {
    match std::fs::hard_link(tmp, path) {
        Ok(()) => {
            let _ = std::fs::remove_file(tmp);
            Ok(())
        }
        Err(e)
            if e.kind() == std::io::ErrorKind::AlreadyExists
                || e.kind() == std::io::ErrorKind::NotFound =>
        {
            let _ = std::fs::remove_file(tmp);
            Err(StageError::Occupied)
        }
        Err(e) => Err(StageError::Failed(format!("credential.stage_failed: {e}"))),
    }
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

    /// Claim a file the way a staging boundary does: an exclusive
    /// `flock` held on the descriptor for the holder's life. Returns
    /// the descriptor so the caller can hold the claim.
    fn claim(file: std::fs::File) -> std::fs::File {
        use std::os::unix::io::AsRawFd;
        let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        assert_eq!(rc, 0, "the fixture must take the owner lock");
        file
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

    /// A resolve carrying no `credential_ref` still sweeps the
    /// staging dir: residue no live owner claims — an unlocked
    /// owner-tagged file, an orphaned `.tmp` sibling, or an
    /// unattributed legacy name — is stale secret material and is
    /// reclaimed, while a file a live owner still holds its flock on
    /// is another boundary's grant and is kept.
    #[test]
    fn a_no_credential_resolve_reclaims_dead_owner_residue() {
        let dir = tmp("unstage");
        let runtime = dir.join("runtime");
        std::fs::create_dir_all(&runtime).unwrap();
        // An unlocked owner-tagged file is residue: a live owner
        // holds its flock, so unlocked means its owner is gone.
        let dead = runtime.join("runtime-auth.p1.i2.3");
        std::fs::write(&dead, "stale-secret").unwrap();
        // An orphaned tmp sibling and an unattributed legacy name are
        // the same stale secret material.
        let orphan = runtime.join("runtime-auth.tmp");
        std::fs::write(&orphan, "partial-secret").unwrap();
        let legacy = runtime.join("runtime-auth.7");
        std::fs::write(&legacy, "legacy-secret").unwrap();
        // A file held under an exclusive lock stands in for a live
        // neighbour's grant — the test claims it the way a staging
        // boundary does.
        let claimed = runtime.join("runtime-auth.p4.i5.6");
        let _claimed_lock = claim(std::fs::File::create(&claimed).unwrap());
        std::fs::write(&claimed, "live-secret").unwrap();

        let boundary = RuntimeBoundary::new(config(&dir));
        boundary.ensure().unwrap();
        assert!(!dead.exists(), "a dead owner's staged file is reclaimed");
        assert!(!orphan.exists(), "the orphaned staging file is unlinked");
        assert!(!legacy.exists(), "the unattributed legacy name is unlinked");
        assert!(claimed.exists(), "a live owner's file is never swept");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A staging-shaped name that is not one of the boundary's shapes
    /// is never probed for a claim — a malformed tag or any other
    /// `runtime-auth.*` name is operator content and stays.
    #[test]
    fn a_malformed_or_foreign_staging_name_is_never_ours() {
        let dir = tmp("foreignnames");
        let runtime = dir.join("runtime");
        std::fs::create_dir_all(&runtime).unwrap();
        let malformed = runtime.join("runtime-auth.abc");
        std::fs::write(&malformed, "operator").unwrap();
        let two_part = runtime.join("runtime-auth.1.2");
        std::fs::write(&two_part, "operator").unwrap();
        // A three-part purely numeric name — an operator's versioned
        // file — is not the tagged shape either: the `p`/`i` prefixes
        // keep ordinary names out of the sweep.
        let versioned = runtime.join("runtime-auth.1.2.3");
        std::fs::write(&versioned, "operator").unwrap();
        let boundary = RuntimeBoundary::new(config(&dir));
        boundary.ensure().unwrap();
        assert!(
            malformed.exists() && two_part.exists() && versioned.exists(),
            "non-staging-shape names are never swept"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Two boundaries sharing the staging dir are different owners:
    /// the second boundary's ensure never sweeps the first's live
    /// staged credential — the file is claimed by the holder's flock
    /// — and dropping the first removes only the files it staged,
    /// never a name the second boundary published under its own tag.
    #[test]
    fn a_second_boundary_never_unlinks_a_live_owners_grant() {
        let dir = tmp("neighbours");
        // SAFETY: test-only env mutation.
        unsafe { std::env::set_var("TAISAN_BOUNDARY_TEST_TOKEN5", "sekrit10") };
        let mut cfg = config(&dir);
        cfg.credential_ref = Some("env:TAISAN_BOUNDARY_TEST_TOKEN5".into());
        let holder = RuntimeBoundary::new(cfg.clone());
        let grant = holder.ensure().unwrap();
        let held = grant.credential_file.expect("the holder staged");
        assert!(held.exists());
        // The second boundary's ensure sweeps foreign staging — the
        // holder's flock-claimed file is its live grant and survives.
        let neighbour = RuntimeBoundary::new(cfg);
        let grant2 = neighbour.ensure().unwrap();
        let neighbours = grant2.credential_file.expect("the neighbour staged");
        assert!(held.exists(), "a live owner's grant is never swept");
        assert_ne!(held, neighbours, "each boundary stages its own name");
        // Dropping the holder removes only the files it staged — the
        // neighbour's grant outlives it.
        drop(holder);
        assert!(!held.exists(), "drop reclaims the holder's own files");
        assert!(
            neighbours.exists(),
            "drop never touches a neighbour's grant"
        );
        unsafe { std::env::remove_var("TAISAN_BOUNDARY_TEST_TOKEN5") };
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Owner tags are only *usually* unique: two pid namespaces mint
    /// the same `p<pid>.i<instance>` tag, so a name collision is a
    /// real case, not a bug precondition. Publish must still be
    /// no-clobber — the contender loses a seq, never the holder's
    /// file — and neither boundary's Drop unlinks a name the other
    /// published.
    #[test]
    fn a_same_owner_tag_contender_never_clobbers_the_holders_grant() {
        let dir = tmp("tagcollision");
        // SAFETY: test-only env mutation; the name is unique to this
        // test — parallel env-driven tests must never share a variable.
        unsafe { std::env::set_var("TAISAN_BOUNDARY_TEST_TOKEN11", "sekrit11") };
        let mut cfg = config(&dir);
        cfg.credential_ref = Some("env:TAISAN_BOUNDARY_TEST_TOKEN11".into());
        // Force the tag two pid namespaces could both mint.
        let holder = RuntimeBoundary {
            config: cfg.clone(),
            owner_tag: "p9.i9".into(),
            state: Mutex::new(BoundaryState::Unresolved),
        };
        let held = holder
            .ensure()
            .unwrap()
            .credential_file
            .expect("holder staged");
        assert_eq!(
            held.file_name().unwrap().to_string_lossy(),
            "runtime-auth.p9.i9.0"
        );
        let contender = RuntimeBoundary {
            config: cfg,
            owner_tag: "p9.i9".into(),
            state: Mutex::new(BoundaryState::Unresolved),
        };
        // The contender sweeps first: the holder's file is claimed, so
        // it stays — then the contender publishes the next seq rather
        // than clobbering `runtime-auth.p9.i9.0`.
        let published = contender
            .ensure()
            .unwrap()
            .credential_file
            .expect("contender staged");
        assert_eq!(
            published.file_name().unwrap().to_string_lossy(),
            "runtime-auth.p9.i9.1",
            "an occupied same-tag name costs a seq, never the holder's file"
        );
        assert!(held.exists() && published.exists());
        assert_eq!(std::fs::read(&held).unwrap(), b"sekrit11");
        // Each boundary's Drop removes only the name it published.
        drop(contender);
        assert!(!published.exists());
        assert!(
            held.exists(),
            "a same-tagged neighbour's Drop never unlinks the holder's grant"
        );
        drop(holder);
        assert!(!held.exists());
        unsafe { std::env::remove_var("TAISAN_BOUNDARY_TEST_TOKEN11") };
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A concurrent sweep's claim probe holds a transient lock on the
    /// staging sibling in the stage's create→flock window: the
    /// stage's own claim then loses the race the same way a sibling
    /// the sweep unlinked does — `Occupied`, a seq, never the launch.
    /// The contested name is left alone: the sweep that holds the
    /// probe lock reclaims it, and an unlink here could instead
    /// remove a sibling a neighbour recreated — never ours.
    #[test]
    fn a_probe_claim_on_the_staging_sibling_costs_a_seq_not_the_launch() {
        let dir = tmp("proberace");
        let sibling = dir.join("runtime-auth.p1.i1.0.tmp");
        let published = dir.join("runtime-auth.p1.i1.0");
        // The sweep's transient probe: a second descriptor holds the
        // sibling's lock — exactly the state the stage's flock meets
        // when the probe lands in the create→flock window.
        let stager = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&sibling)
            .unwrap();
        let _probe = claim(
            std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&sibling)
                .unwrap(),
        );
        unsafe { std::env::set_var("TAISAN_BOUNDARY_TEST_PROBERACE", "secret") };
        let credential = credentials::resolve("env:TAISAN_BOUNDARY_TEST_PROBERACE").unwrap();
        assert!(
            matches!(
                claim_and_write(stager, &sibling, &published, &credential),
                Err(StageError::Occupied)
            ),
            "a probe-blocked claim is the swept race — Occupied, never a stage failure"
        );
        assert!(
            sibling.exists(),
            "the contested name is the sweeper's to reclaim — never ours to unlink"
        );
        unsafe { std::env::remove_var("TAISAN_BOUNDARY_TEST_PROBERACE") };
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The still-empty sibling an `Occupied` claim leaves behind is
    /// unclaimed staging residue: the next `ensure`'s sweep reclaims
    /// it like any dead owner's leftover, and staging proceeds past
    /// it — a seq, never the launch.
    #[test]
    fn a_sweep_reclaims_the_sibling_an_occupied_claim_left() {
        let dir = tmp("sweepresidue");
        let runtime = dir.join("runtime");
        std::fs::create_dir_all(&runtime).unwrap();
        let residue = runtime.join("runtime-auth.p9.i9.0.tmp");
        std::fs::write(&residue, b"").unwrap();
        unsafe { std::env::set_var("TAISAN_BOUNDARY_TEST_SWEEPRES", "secret") };
        let mut cfg = config(&dir);
        cfg.credential_ref = Some("env:TAISAN_BOUNDARY_TEST_SWEEPRES".into());
        let boundary = RuntimeBoundary::new(cfg);
        let grant = boundary.ensure().unwrap();
        assert!(grant.credential_file.is_some());
        assert!(
            !residue.exists(),
            "unclaimed staging residue is the sweep's to reclaim"
        );
        unsafe { std::env::remove_var("TAISAN_BOUNDARY_TEST_SWEEPRES") };
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A configured input inside the staging dir is never swept: an
    /// operator may point `worker_provider_config` or a `file:`
    /// credential at a `.runtime/runtime-auth*` path — the prefix is
    /// not reserved. A file merely sharing the `runtime-auth` prefix
    /// without the staging shape is operator content too. Deleting
    /// either fails the launch it is configured for, and retry cannot
    /// restore an operator's file.
    #[test]
    fn the_sweep_keeps_configured_inputs_and_foreign_names() {
        let dir = tmp("keepinputs");
        let runtime = dir.join("runtime");
        std::fs::create_dir_all(&runtime).unwrap();
        // A provider document under a staging-shaped name — only the
        // resolved-reference protection keeps it.
        let provider = runtime.join("runtime-auth.41");
        std::fs::write(
            &provider,
            "{\"format_version\":\"taisan.worker.bindings/1\",\"model\":\"m\"}\n",
        )
        .unwrap();
        // A `file:` credential source under a staging-shaped name.
        let source = runtime.join("runtime-auth.7");
        std::fs::write(&source, "configured-secret").unwrap();
        std::fs::set_permissions(&source, std::fs::Permissions::from_mode(0o600)).unwrap();
        // An operator file merely sharing the prefix — not ours.
        let foreign = runtime.join("runtime-auth-provider.json");
        std::fs::write(&foreign, "{}").unwrap();
        let mut cfg = config(&dir);
        cfg.credential_ref = Some(format!("file:{}", source.display()));
        cfg.provider_config = Some(provider.clone());
        let boundary = RuntimeBoundary::new(cfg);
        let grant = boundary.ensure().unwrap();
        assert!(
            provider.exists(),
            "the configured provider document survives the sweep"
        );
        assert!(
            source.exists(),
            "the configured credential source survives the sweep"
        );
        assert!(
            foreign.exists(),
            "a `runtime-auth*` file without the staging shape is never ours"
        );
        // The grant still staged its own unique file.
        let staged = grant.credential_file.expect("credential staged");
        assert_eq!(std::fs::read(&staged).unwrap(), b"configured-secret");
        assert_ne!(staged, source);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The stage's rename target is never a path the configuration
    /// named: a provider document or `file:` source sitting on a
    /// staging-shaped name survives the sweep *and* the stage, which
    /// publishes under its own owner tag instead of overwriting it.
    #[test]
    fn staging_never_overwrites_a_configured_input() {
        let dir = tmp("stageprotect");
        let runtime = dir.join("runtime");
        std::fs::create_dir_all(&runtime).unwrap();
        // The provider document takes an unattributed staging name —
        // unprotected it would be swept; configured it is kept.
        let provider = runtime.join("runtime-auth.0");
        let provider_bytes = "{\"format_version\":\"taisan.worker.bindings/1\",\"model\":\"m\"}\n";
        std::fs::write(&provider, provider_bytes).unwrap();
        // The credential source takes the second.
        let source = runtime.join("runtime-auth.1");
        std::fs::write(&source, "configured-secret").unwrap();
        std::fs::set_permissions(&source, std::fs::Permissions::from_mode(0o600)).unwrap();
        let mut cfg = config(&dir);
        cfg.credential_ref = Some(format!("file:{}", source.display()));
        cfg.provider_config = Some(provider.clone());
        let boundary = RuntimeBoundary::new(cfg);
        let grant = boundary.ensure().unwrap();
        assert_eq!(
            std::fs::read(&provider).unwrap(),
            provider_bytes.as_bytes(),
            "the stage must not overwrite the configured provider document"
        );
        assert_eq!(
            std::fs::read(&source).unwrap(),
            b"configured-secret",
            "the stage must not overwrite the configured credential source"
        );
        assert_eq!(
            grant.credential_file.expect("credential staged"),
            runtime.join(boundary.staging_name(0)),
            "the grant stages under this boundary's own owner tag"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A staging name that exists after the sweep is never rewritten
    /// — here a same-named directory at this boundary's own tag,
    /// which the sweep deliberately leaves — so the stage advances to
    /// the next free tag.
    #[test]
    fn an_occupied_staging_name_is_skipped_not_rewritten() {
        let dir = tmp("stageskip");
        let runtime = dir.join("runtime");
        // SAFETY: test-only env mutation.
        unsafe { std::env::set_var("TAISAN_BOUNDARY_TEST_TOKEN8", "sekrit8") };
        let mut cfg = config(&dir);
        cfg.credential_ref = Some("env:TAISAN_BOUNDARY_TEST_TOKEN8".into());
        let boundary = RuntimeBoundary::new(cfg);
        std::fs::create_dir_all(runtime.join(boundary.staging_name(0))).unwrap();
        let grant = boundary.ensure().unwrap();
        unsafe { std::env::remove_var("TAISAN_BOUNDARY_TEST_TOKEN8") };
        assert!(
            runtime.join(boundary.staging_name(0)).is_dir(),
            "an existing staging-named entry is never rewritten"
        );
        assert_eq!(
            grant.credential_file.expect("credential staged"),
            runtime.join(boundary.staging_name(1)),
            "the stage advances to a free staging name"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A neighbour's sweep can reclaim the staging sibling in the
    /// window between `create_new` and the flock that makes the
    /// stage's claim observable — publish then finds the `.tmp` gone.
    /// That lost race is the same collision class as an occupied
    /// name: it costs a seq (`Occupied`), never fails the launch, and
    /// the swept inode dies with the stager's descriptor. The
    /// `Occupied` → next-seq advance is pinned end-to-end by
    /// `a_same_owner_tag_contender_never_clobbers_the_holders_grant`.
    #[test]
    fn a_swept_staging_sibling_costs_a_seq() {
        let dir = tmp("sweptsib");
        let runtime = dir.join("runtime");
        std::fs::create_dir_all(&runtime).unwrap();
        let tmp = runtime.join("runtime-auth.p3.i3.0.tmp");
        let path = runtime.join("runtime-auth.p3.i3.0");
        // The sibling a neighbour's sweep reclaimed mid-stage:
        // publish reports the lost race, not a stage failure.
        assert!(matches!(
            publish_staged(&tmp, &path),
            Err(StageError::Occupied)
        ));
        assert!(!path.exists(), "a swept publish never creates the name");
        // An occupied publish name is the same class — and still
        // never clobbers the file that took the name.
        std::fs::write(&tmp, b"staged").unwrap();
        std::fs::write(&path, b"neighbour").unwrap();
        assert!(matches!(
            publish_staged(&tmp, &path),
            Err(StageError::Occupied)
        ));
        assert_eq!(std::fs::read(&path).unwrap(), b"neighbour");
        assert!(!tmp.exists(), "the losing sibling is removed");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The sweep runs before resolution, so a broken credential ref
    /// still clears foreign staging files — a crashed predecessor's
    /// stale secret does not linger while the config stays broken.
    #[test]
    fn a_failed_resolve_still_sweeps_foreign_staging() {
        let dir = tmp("sweepfail");
        let runtime = dir.join("runtime");
        std::fs::create_dir_all(&runtime).unwrap();
        let stale = runtime.join("runtime-auth.9");
        std::fs::write(&stale, "stale-secret").unwrap();
        let mut cfg = config(&dir);
        cfg.credential_ref = Some("env:TAISAN_BOUNDARY_TEST_MISSING".into());
        let boundary = RuntimeBoundary::new(cfg);
        let err = boundary.ensure().unwrap_err();
        assert!(err.contains("credential"), "{err}");
        assert!(
            !stale.exists(),
            "a refused ensure still swept the foreign staging file"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A stage that cannot create its tmp copy — here because the
    /// staging dir is read-only — refuses the resolve and leaves no
    /// credential bytes behind.
    #[test]
    fn a_failed_stage_removes_the_tmp_credential() {
        let dir = tmp("stagefail");
        let runtime = dir.join("runtime");
        std::fs::create_dir_all(&runtime).unwrap();
        std::fs::set_permissions(&runtime, std::fs::Permissions::from_mode(0o555)).unwrap();
        // A privileged runner ignores the mode bits — probe for that
        // and skip rather than panic the whole binary on a guarantee
        // this environment lacks.
        if std::fs::write(runtime.join(".perm-probe"), b"x").is_ok() {
            let _ = std::fs::remove_file(runtime.join(".perm-probe"));
            std::fs::set_permissions(&runtime, std::fs::Permissions::from_mode(0o755)).unwrap();
            let _ = std::fs::remove_dir_all(&dir);
            return;
        }
        // SAFETY: test-only env mutation; the name is unique to this
        // test — parallel env-driven tests must never share a variable.
        unsafe { std::env::set_var("TAISAN_BOUNDARY_TEST_TOKEN10", "sekrit10") };
        let mut cfg = config(&dir);
        cfg.credential_ref = Some("env:TAISAN_BOUNDARY_TEST_TOKEN10".into());
        let boundary = RuntimeBoundary::new(cfg);
        let err = boundary.ensure().unwrap_err();
        unsafe { std::env::remove_var("TAISAN_BOUNDARY_TEST_TOKEN10") };
        assert!(err.contains("credential.stage_failed"), "{err}");
        assert!(
            staged_entries(&dir).is_empty(),
            "a failed stage must not leave any credential copy"
        );
        std::fs::set_permissions(&runtime, std::fs::Permissions::from_mode(0o755)).unwrap();
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

    /// Staged credentials do not accumulate for the boundary's life:
    /// only the newest [`RETAINED_GRANT_FILES`] grant files are kept —
    /// the oldest files' mounts have long consumed them (or their
    /// launch failed), so they are unlinked as stale secret material
    /// at the next stage rather than at process exit.
    #[test]
    fn staged_credentials_are_retained_within_a_bound() {
        let dir = tmp("retention");
        let source = dir.join("token");
        std::fs::write(&source, "credential").unwrap();
        std::fs::set_permissions(&source, std::fs::Permissions::from_mode(0o600)).unwrap();
        let mut cfg = config(&dir);
        cfg.credential_ref = Some(format!("file:{}", source.display()));
        let boundary = RuntimeBoundary::new(cfg);
        for _ in 0..RETAINED_GRANT_FILES + 2 {
            boundary.ensure().expect("each grant stages its own file");
        }
        let entries = staged_entries(&dir);
        assert_eq!(
            entries.len(),
            RETAINED_GRANT_FILES,
            "only the newest grant files are kept: {entries:?}"
        );
        // The oldest grants' files — every mount they could serve is
        // long past — are unlinked; the newest files stay for their
        // still-pending consumers.
        let runtime = dir.join("runtime");
        assert!(!runtime.join(boundary.staging_name(0)).exists());
        assert!(!runtime.join(boundary.staging_name(1)).exists());
        for seq in 2..RETAINED_GRANT_FILES + 2 {
            assert!(
                runtime.join(boundary.staging_name(seq as u64)).exists(),
                "the newest grant files are retained: {seq}"
            );
        }
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
