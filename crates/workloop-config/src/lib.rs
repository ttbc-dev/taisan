//! The installation config file every adapter shares: actor identity,
//! repository, capacity, worker argv, workspace root, allowed effects,
//! and budget. One schema and one loader live here so the CLI and the
//! TUI can never drift on the file they must agree on — whichever
//! adapter creates it, the other adopts it.

pub mod profile;

use std::collections::BTreeSet;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use workloop_core::domain::{
    Actor, ActorKind, AllowedEffects, ExecutionBudget, LanePolicy, WorkPolicy,
};
use workloop_core::identity::{
    ActorRole, AdapterId, CredentialRef, HostActorId, HostActorRef, HostCapability, HostInstanceId,
    IssueId, IssueRef, RepositoryId, RepositoryRef, RoleBinding,
};
use workloop_core::work::{HostTemplate, WorkloopConfig};
use workloop_selector::SelectionPolicy;

/// Loop-driver settings (owner-approved defaults from the execution
/// loop design). `None` in the file means "use these defaults".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LoopConfig {
    #[serde(default = "default_loop_interval")]
    pub interval_seconds: u64,
    #[serde(default = "default_tick_deadline")]
    pub tick_deadline_seconds: u64,
    #[serde(default = "default_source_refresh_timeout")]
    pub source_refresh_timeout_seconds: u64,
}

fn default_loop_interval() -> u64 {
    30
}
fn default_tick_deadline() -> u64 {
    120
}
fn default_source_refresh_timeout() -> u64 {
    30
}

impl Default for LoopConfig {
    fn default() -> Self {
        Self {
            interval_seconds: default_loop_interval(),
            tick_deadline_seconds: default_tick_deadline(),
            source_refresh_timeout_seconds: default_source_refresh_timeout(),
        }
    }
}

/// Retention sweep settings (owner-approved defaults from the retention
/// policy). `None` in the file means "use these defaults".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RetentionConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_retention_interval")]
    pub interval_seconds: i64,
    #[serde(default = "default_max_bodies")]
    pub max_bodies: usize,
    #[serde(default = "default_max_bytes")]
    pub max_bytes: u64,
    #[serde(default = "default_delete_error_limit")]
    pub delete_error_limit: usize,
}

fn default_true() -> bool {
    true
}
fn default_retention_interval() -> i64 {
    3600
}
fn default_max_bodies() -> usize {
    256
}
fn default_max_bytes() -> u64 {
    1 << 30
}
fn default_delete_error_limit() -> usize {
    3
}

impl Default for RetentionConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            interval_seconds: default_retention_interval(),
            max_bodies: default_max_bodies(),
            max_bytes: default_max_bytes(),
            delete_error_limit: default_delete_error_limit(),
        }
    }
}

/// The on-disk installation record. Fields an adapter does not use
/// pass through untouched, so a file either adapter wrote stays valid
/// for both.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Installation {
    pub actor_id: String,
    pub actor_kind: String,
    /// The bound host instance id (opaque; never a display alias).
    #[serde(default = "default_host_instance")]
    pub host_instance: String,
    /// The bound repository's stable opaque id.
    #[serde(default = "default_repository")]
    pub repository: String,
    /// The host adapter identity (e.g. `github`).
    #[serde(default = "default_adapter")]
    pub adapter: String,
    /// The adapter contract version this installation speaks.
    #[serde(default)]
    pub contract_version: u32,
    /// Capabilities this installation declares as supported and
    /// authorized for its bound actors.
    #[serde(default)]
    pub capabilities: Vec<String>,
    /// Host actors bound per role; `publication` is required for any
    /// publishable Work.
    #[serde(default)]
    pub host_actors: std::collections::BTreeMap<String, String>,
    /// Named host checks the completion gate requires.
    #[serde(default)]
    pub required_checks: Vec<String>,
    #[serde(default = "default_capacity")]
    pub capacity: u32,
    #[serde(default)]
    pub worker_argv: Vec<String>,
    /// Name of the approved isolation profile the WorkerRequest export
    /// records and the launch binds (U4.2). Absent names the approved
    /// reference profile. Enforcement of the profile's egress/namespace
    /// settings is the U2.3/U2.4 slice; absent means no override, not
    /// no profile.
    #[serde(default)]
    pub worker_isolation_profile: Option<String>,
    #[serde(default = "default_workspace_root")]
    pub workspace_root: String,
    #[serde(default)]
    pub allowed_effects: Option<AllowedEffects>,
    #[serde(default)]
    pub budget: Option<ExecutionBudget>,
    #[serde(default)]
    pub lanes: Option<LanePolicy>,
    #[serde(default)]
    pub policy: Option<WorkPolicy>,
    /// Loop-driver bounds; absent means the approved defaults.
    #[serde(default, rename = "loop")]
    pub loop_config: Option<LoopConfig>,
    /// Retention sweep policy; absent means the approved defaults.
    #[serde(default)]
    pub retention: Option<RetentionConfig>,
    /// Alternate admission: the selector policy the `propose` path is
    /// bound to, held as the raw JSON it was written as. The shape is
    /// parsed at the selector boundary (`selection_policy`), never
    /// during load: a structurally malformed section must fail that
    /// path with a bounded config error, not fail every command —
    /// including manual `submit` — at deserialization. Absent means
    /// manual submission only — the selector holds no default
    /// authority and its absence blocks nothing.
    #[serde(default)]
    pub selector: Option<serde_json::Value>,
}

/// The selector binding for alternate admission. Presence selects the
/// alternate path. The section is deserialized from the installation
/// file's raw JSON inside `selection_policy` — a structurally malformed
/// block (missing or mistyped field, unknown key, non-array list,
/// out-of-range number) is a bounded config error for a selector run
/// only, never a load failure that could block manual submission.
/// Identity values are validated at the same boundary. Unknown keys are
/// refused rather than defaulted: a misspelled key must surface as the
/// bounded error, never silently degrade the policy (an ignored
/// `allowlist` typo would otherwise yield an accepted zero-proposal
/// run).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SelectorSection {
    /// Selector adapter identity stamped on every proposal.
    pub adapter: String,
    /// Revision of the policy document — part of proposal identity.
    #[serde(default)]
    pub policy_revision: u64,
    /// Repository ids (on this installation's host) the selector may
    /// propose from.
    #[serde(default)]
    pub allowlist: Vec<String>,
    /// Issues excluded from selection regardless of eligibility, in
    /// `repo:number` form like `submit --issue`. The number matches a
    /// fact's opaque IssueId or its repository-scoped display number,
    /// so an exclusion holds even when the adapter's IssueId is not
    /// the decimal number.
    #[serde(default)]
    pub excluded: Vec<String>,
    /// Bound on emitted proposals per selector run.
    #[serde(default = "default_max_proposals")]
    pub max_proposals: usize,
    /// Constraints every proposal's task input carries.
    #[serde(default)]
    pub task_constraints: Vec<String>,
    /// The verification argv every proposal's task input carries.
    #[serde(default)]
    pub verification_argv: Vec<String>,
    /// Permissions proposed Work will need — declared on the proposal
    /// (identity/policy provenance), never expanded, and not enforced
    /// or recorded by admission; capability coverage is Core's check.
    #[serde(default)]
    pub required_permissions: Vec<String>,
}

fn default_max_proposals() -> usize {
    10
}

fn default_host_instance() -> String {
    "host-local".into()
}
fn default_repository() -> String {
    "local-repo".into()
}
fn default_adapter() -> String {
    "fixture".into()
}
fn default_capacity() -> u32 {
    1
}
fn default_workspace_root() -> String {
    "workspaces".into()
}

fn uuidish() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{nanos:x}-{}", std::process::id())
}

/// The unique scratch name the complete content is staged under before
/// it is published. Uniqueness matters: a crashed writer's leftover
/// scratch must never block a later first write. The pid distinguishes
/// concurrent processes, the counter distinguishes threads sharing one
/// pid (equal nanos collide), and nanos distinguishes a crashed
/// writer's leftover from a later attempt.
fn scratch_path(path: &Path) -> PathBuf {
    static SCRATCH_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = SCRATCH_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    path.with_file_name(format!(
        ".{name}.{}.{seq}.{nanos:x}.tmp",
        std::process::id()
    ))
}

/// Read the config at `path`. `Ok(None)` means the name is absent.
/// Any present file this loader did not produce is reported, never
/// deleted: an empty or malformed file can only be foreign (a first
/// write publishes only complete content), and reclaiming it by
/// unlinking could race a concurrent publisher and remove a live
/// config. The operator removes such a file to reinitialize.
fn read(path: &Path) -> Result<Option<Installation>, String> {
    match std::fs::read_to_string(path) {
        Ok(text) if text.trim().is_empty() => Err(format!(
            "config: {} exists but is empty — remove it to reinitialize",
            path.display()
        )),
        Ok(text) => serde_json::from_str(&text)
            .map(Some)
            .map_err(|e| format!("config: {e}")),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("config read: {e}")),
    }
}

/// Directory fsync is not portable: not every platform or filesystem
/// implements it (macOS returns `EINVAL` for `fsync(2)` on a directory).
/// The file itself is already published, so only the crash durability
/// of the directory entry is lost, never the config.
fn dir_sync_unsupported(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        std::io::ErrorKind::InvalidInput | std::io::ErrorKind::Unsupported
    )
}

/// Make a freshly published directory entry durable. Renaming or
/// linking a file only updates the directory; without syncing it, a
/// power loss can leave the old directory contents even though the
/// file's own data survived. A platform that cannot sync directories
/// is not a publish failure (see `dir_sync_unsupported`).
fn sync_parent(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        let parent = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        match std::fs::File::open(parent).and_then(|dir| dir.sync_all()) {
            Err(e) if dir_sync_unsupported(&e) => Ok(()),
            outcome => outcome,
        }
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(())
    }
}

/// The advisory lock every publisher of `path` takes. Creation is
/// harmless-if-present: the file is a rendezvous point, not state, so a
/// leftover from a crashed writer never blocks a later first write.
fn lock_path(path: &Path) -> PathBuf {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    path.with_file_name(format!(".{name}.lock"))
}

/// Serialize publishers across threads and processes. `flock`-style
/// locks are released by the OS when the holder exits, so a crash cannot
/// leave the lock held; the lock file itself is deliberately kept
/// because unlinking it while another writer waits would let a third
/// writer create a different inode and bypass the lock.
fn lock_publish(path: &Path) -> Result<std::fs::File, String> {
    let lock_path = lock_path(path);
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)
        .map_err(|e| format!("config lock: {e}"))?;
    file.lock().map_err(|e| format!("config lock: {e}"))?;
    Ok(file)
}

/// Publish the complete scratch file at `path`. Publishers serialize on
/// a sibling lock file, so only one can attempt the publish at a time
/// and the no-hard-link fallback can safely check for a winner before
/// renaming without a concurrent publisher landing in between. A hard
/// link is the preferred atomic publish: `AlreadyExists` means a
/// concurrent first writer won, so this writer adopts that complete
/// file. Filesystems without hard links (some FAT/exFAT, SMB/CIFS, or
/// FUSE mounts) fall back to a same-directory rename, which is still
/// atomic and still publishes only complete content; a winner that
/// appeared before the rename is adopted rather than overwritten.
/// Either way the parent directory is synced so the new name survives a
/// crash.
fn publish_with(
    path: &Path,
    scratch: &Path,
    link: impl Fn(&Path, &Path) -> std::io::Result<()>,
) -> Result<(), String> {
    let _publish_lock = lock_publish(path)?;
    match link(scratch, path) {
        Ok(()) => {
            let _ = std::fs::remove_file(scratch);
            sync_parent(path).map_err(|e| format!("config dir sync: {e}"))
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            // Lost the create race: the winner's file is complete by the
            // same publish rule, so adopt it.
            let _ = std::fs::remove_file(scratch);
            Ok(())
        }
        Err(link_err) => match read(path) {
            Ok(Some(_)) => {
                let _ = std::fs::remove_file(scratch);
                Ok(())
            }
            Ok(None) => {
                std::fs::rename(scratch, path).map_err(|e| {
                    let _ = std::fs::remove_file(scratch);
                    format!("config publish: hard link failed ({link_err}); rename failed: {e}")
                })?;
                sync_parent(path).map_err(|e| format!("config dir sync: {e}"))
            }
            Err(e) => {
                let _ = std::fs::remove_file(scratch);
                Err(e)
            }
        },
    }
}

/// Read the installation config, creating it with a fresh stable actor
/// id on first use. Creation is publish-by-link: the complete content
/// is written to a scratch file and then atomically hard-linked into
/// place. `hard_link` fails `AlreadyExists` when a concurrent first
/// writer won, so exactly one complete file lands and every loser
/// adopts it — CLI and TUI converge on one actor identity. A reader
/// only ever observes the absent name or a complete file: a partial or
/// empty view of a live write is impossible, so no reader may unlink a
/// live writer's file, and a crashed writer leaves only a scratch file
/// (plus the inert publish lock), never a broken config. On filesystems
/// without hard links the same atomic publish falls back to a rename
/// (see `publish_with`), still serialized by the publish lock.
pub fn load_or_create(path: &Path) -> Result<Installation, String> {
    load_or_create_with(path, |from, to| std::fs::hard_link(from, to))
}

fn load_or_create_with(
    path: &Path,
    link: impl Fn(&Path, &Path) -> std::io::Result<()>,
) -> Result<Installation, String> {
    if let Some(installation) = read(path)? {
        return Ok(installation);
    }
    let installation = Installation {
        actor_id: format!("actor-{}", uuidish()),
        actor_kind: "human".into(),
        host_instance: default_host_instance(),
        repository: default_repository(),
        adapter: default_adapter(),
        // The adapter contract version this installation negotiates;
        // version 0 is never a valid binding (docs/installation-config).
        contract_version: 1,
        // The default fixture adapter supports reads plus the publication
        // writes a recorded intent may need; operators narrow this list to
        // least privilege for a real installation.
        capabilities: vec![
            "repository_read".into(),
            "push_ref".into(),
            "change_request_create".into(),
            "change_request_read".into(),
            "change_request_update".into(),
            "comment_write".into(),
            "checks_read".into(),
            "review_findings_read".into(),
            "issue_read".into(),
        ],
        // A binding requires at least one role: the default installation
        // publishes through a single fixture actor.
        host_actors: [("publication".into(), "fixture-agent".into())]
            .into_iter()
            .collect(),
        required_checks: vec![],
        capacity: default_capacity(),
        worker_argv: vec![],
        worker_isolation_profile: None,
        workspace_root: default_workspace_root(),
        allowed_effects: None,
        budget: None,
        lanes: None,
        policy: None,
        loop_config: None,
        retention: None,
        selector: None,
    };
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("config dir: {e}"))?;
    }
    let text = serde_json::to_string_pretty(&installation).map_err(|e| format!("config: {e}"))?;
    let scratch = scratch_path(path);
    {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&scratch)
            .map_err(|e| format!("config scratch: {e}"))?;
        file.write_all(text.as_bytes())
            .and_then(|()| file.sync_all())
            .map_err(|e| format!("config scratch write: {e}"))?;
    }
    publish_with(path, &scratch, link)?;
    // Return the file that actually won the publish, not necessarily the
    // record this process staged: concurrent first writers must all
    // report the one identity the installation now holds.
    read(path)?.ok_or_else(|| "config: first writer's file vanished".into())
}

impl Installation {
    /// The loop-driver settings, falling back to the approved defaults
    /// when the file carries no `loop` section.
    pub fn loop_settings(&self) -> LoopConfig {
        self.loop_config.clone().unwrap_or_default()
    }

    /// The retention sweep settings, falling back to the approved
    /// defaults when the file carries no `retention` section.
    pub fn retention_settings(&self) -> RetentionConfig {
        self.retention.clone().unwrap_or_default()
    }

    /// The stable actor identity every adapter's requests carry.
    pub fn actor(&self) -> Actor {
        Actor {
            kind: match self.actor_kind.as_str() {
                "automation" => ActorKind::Automation,
                _ => ActorKind::Human,
            },
            external_id: self.actor_id.clone(),
        }
    }

    /// The `work`-level config derived from the installation record.
    /// The installation file is external input: every identity string,
    /// role name, and capability name is validated at this boundary —
    /// an unparseable value is a hard error, never a silently degraded
    /// or mis-bound configuration.
    pub fn workloop_config(&self) -> Result<WorkloopConfig, String> {
        let host = HostInstanceId::new(self.host_instance.clone())
            .map_err(|e| format!("installation host_instance: {e}"))?;
        let repository_id = RepositoryId::new(self.repository.clone())
            .map_err(|e| format!("installation repository: {e}"))?;
        let repository = RepositoryRef::new(host.clone(), repository_id);
        let adapter = AdapterId::new(self.adapter.clone())
            .map_err(|e| format!("installation adapter: {e}"))?;
        let mut actors = Vec::with_capacity(self.host_actors.len());
        for (role, actor) in &self.host_actors {
            let role_name = role.clone();
            let role = ActorRole::from_name(&role_name)
                .ok_or_else(|| format!("installation host_actors: unknown role '{role_name}'"))?;
            actors.push(RoleBinding {
                role,
                actor: HostActorRef {
                    host: host.clone(),
                    actor: HostActorId::new(actor.clone())
                        .map_err(|e| format!("installation host_actors[{role_name}]: {e}"))?,
                },
                // The credential source is installation configuration;
                // the record's value names it, never a secret value.
                credential_ref: CredentialRef::new("default")
                    .map_err(|e| format!("installation credential_ref: {e}"))?,
            });
        }
        let mut declared_capabilities = BTreeSet::new();
        for c in &self.capabilities {
            declared_capabilities.insert(
                HostCapability::from_name(c)
                    .ok_or_else(|| format!("installation capabilities: unknown '{c}'"))?,
            );
        }
        // An installation file that pre-dates capability/actor
        // declarations deserializes to empty sets via serde defaults;
        // refusing at this boundary beats every Submit failing with an
        // unexplained capability rejection later.
        if declared_capabilities.is_empty() {
            return Err(
                "installation declares no capabilities — regenerate the config \
                 or declare the supported capability set"
                    .into(),
            );
        }
        if actors.is_empty() {
            return Err("installation binds no host actors — regenerate the config \
                 or bind a host actor to each required role"
                .into());
        }
        // A versionless binding can never be recorded: the installation
        // must declare which adapter contract it negotiated.
        if self.contract_version == 0 {
            return Err(
                "installation contract_version is 0 — regenerate the config or \
                 declare the negotiated contract version"
                    .into(),
            );
        }
        Ok(WorkloopConfig {
            host: HostTemplate {
                adapter,
                repository,
                contract_version: self.contract_version,
                actors,
            },
            declared_capabilities,
            lanes: self.lanes.clone().unwrap_or_default(),
            required_checks: self.required_checks.clone(),
            allowed_effects: self.allowed_effects.clone().unwrap_or(AllowedEffects {
                workspace_write: true,
                host_write: false,
                network: false,
            }),
            budget: self.budget.clone().unwrap_or_default(),
            policy: self.policy.clone().unwrap_or_default(),
        })
    }

    /// The selector adapter identity and policy for the alternate
    /// admission path, or `None` when the installation runs manual
    /// submission only. The selector section is external input: its
    /// structure and every identity string are validated here — a
    /// structurally malformed block, an unparseable adapter,
    /// repository, or issue reference is an error rather than a
    /// silently mis-bound policy. Validation deliberately happens at
    /// this boundary (a selector run), not at config load: a malformed
    /// section must never take manual submission down with it.
    pub fn selection_policy(&self) -> Result<Option<(AdapterId, SelectionPolicy)>, String> {
        let Some(raw) = &self.selector else {
            return Ok(None);
        };
        let section: SelectorSection = serde_json::from_value(raw.clone())
            .map_err(|e| format!("installation selector: {e}"))?;
        let host = HostInstanceId::new(self.host_instance.clone())
            .map_err(|e| format!("installation host_instance: {e}"))?;
        let adapter = AdapterId::new(section.adapter.clone())
            .map_err(|e| format!("installation selector.adapter: {e}"))?;
        let mut allowlist = BTreeSet::new();
        for repo_id in &section.allowlist {
            allowlist.insert(RepositoryRef::new(
                host.clone(),
                RepositoryId::new(repo_id.clone())
                    .map_err(|e| format!("installation selector.allowlist[{repo_id}]: {e}"))?,
            ));
        }
        let mut excluded = BTreeSet::new();
        for entry in &section.excluded {
            let (repo, number) = entry.rsplit_once(':').ok_or_else(|| {
                format!("installation selector.excluded[{entry}]: expected repo:number")
            })?;
            let display_number: u64 = number
                .parse()
                .map_err(|_| format!("installation selector.excluded[{entry}]: bad number"))?;
            // The entry carries the configured number as both the
            // opaque id and the display number: the selector matches
            // either, so an exclusion holds for adapters whose
            // IssueId is not the decimal number.
            excluded.insert(IssueRef {
                repository: RepositoryRef::new(
                    host.clone(),
                    RepositoryId::new(repo)
                        .map_err(|e| format!("installation selector.excluded[{entry}]: {e}"))?,
                ),
                issue: IssueId::trusted(number),
                display_number: Some(display_number),
            });
        }
        Ok(Some((
            adapter,
            SelectionPolicy {
                revision: section.policy_revision,
                allowlist,
                excluded,
                max_proposals: section.max_proposals,
                task_constraints: section.task_constraints.iter().cloned().collect(),
                verification_argv: section.verification_argv.clone(),
                required_permissions: section.required_permissions.iter().cloned().collect(),
            },
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io;

    fn temp_root(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("workloop-config-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A filesystem without hard links still publishes the complete
    /// staged config atomically by renaming it into place.
    #[test]
    fn publish_falls_back_to_rename_without_hard_links() {
        let dir = temp_root("publish-fallback");
        let path = dir.join("config.json");
        let scratch = dir.join("scratch.tmp");
        std::fs::write(
            &scratch,
            "{\"actor_id\":\"actor-fallback\",\"actor_kind\":\"human\"}",
        )
        .unwrap();

        publish_with(&path, &scratch, |_, _| {
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "hard links unsupported",
            ))
        })
        .unwrap();

        assert_eq!(
            read(&path).unwrap().unwrap().actor_id,
            "actor-fallback",
            "the staged config was not published"
        );
        assert!(!scratch.exists(), "the scratch file was left behind");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// If a concurrent writer publishes before the fallback rename, the
    /// loser adopts that complete file instead of overwriting it.
    #[test]
    fn publish_fallback_adopts_a_concurrent_winner() {
        let dir = temp_root("publish-winner");
        let path = dir.join("config.json");
        let scratch = dir.join("scratch.tmp");
        std::fs::write(
            &scratch,
            "{\"actor_id\":\"actor-loser\",\"actor_kind\":\"human\"}",
        )
        .unwrap();
        let winner = "{\"actor_id\":\"actor-winner\",\"actor_kind\":\"human\"}";

        publish_with(&path, &scratch, move |_, target| {
            std::fs::write(target, winner).unwrap();
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "hard links unsupported",
            ))
        })
        .unwrap();

        assert_eq!(
            read(&path).unwrap().unwrap().actor_id,
            "actor-winner",
            "the loser overwrote the concurrent winner"
        );
        assert!(!scratch.exists(), "the scratch file was left behind");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Platforms that reject directory fsync (macOS returns `EINVAL`)
    /// must not fail the publish after the file has landed.
    #[test]
    fn directory_fsync_rejection_is_not_a_publish_failure() {
        let invalid = io::Error::from(io::ErrorKind::InvalidInput);
        let unsupported = io::Error::from(io::ErrorKind::Unsupported);
        let real_fault = io::Error::from(io::ErrorKind::PermissionDenied);

        assert!(dir_sync_unsupported(&invalid));
        assert!(dir_sync_unsupported(&unsupported));
        assert!(!dir_sync_unsupported(&real_fault));
    }

    /// Concurrent first writers on a filesystem without hard links
    /// serialize on the publish lock: exactly one rename lands and every
    /// loser adopts that complete file rather than clobbering it.
    #[test]
    fn concurrent_fallback_writers_share_one_actor() {
        let dir = temp_root("publish-fallback-race");
        let path = std::sync::Arc::new(dir.join("config.json"));
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(8));
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let p = path.clone();
                let b = barrier.clone();
                std::thread::spawn(move || {
                    b.wait();
                    load_or_create_with(&p, |_, _| {
                        Err(io::Error::new(
                            io::ErrorKind::Unsupported,
                            "hard links unsupported",
                        ))
                    })
                    .map(|installation| installation.actor_id)
                })
            })
            .collect();
        let ids: std::collections::HashSet<String> = handles
            .into_iter()
            .map(|h| h.join().unwrap().unwrap())
            .collect();

        assert_eq!(
            ids.len(),
            1,
            "concurrent fallback writers diverged: {ids:?}"
        );
        assert_eq!(
            read(&path).unwrap().unwrap().actor_id,
            *ids.iter().next().unwrap(),
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
