//! The installation config file every adapter shares: actor identity,
//! repository, capacity, worker argv, workspace root, allowed effects,
//! and budget. One schema and one loader live here so the CLI and the
//! TUI can never drift on the file they must agree on — whichever
//! adapter creates it, the other adopts it.

use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use workloop_core::domain::{Actor, ActorKind, AllowedEffects, ExecutionBudget};
use workloop_core::work::WorkloopConfig;

/// The on-disk installation record. Fields an adapter does not use
/// pass through untouched, so a file either adapter wrote stays valid
/// for both.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Installation {
    pub actor_id: String,
    pub actor_kind: String,
    #[serde(default = "default_repository")]
    pub repository: String,
    #[serde(default = "default_capacity")]
    pub capacity: u32,
    #[serde(default)]
    pub worker_argv: Vec<String>,
    #[serde(default = "default_workspace_root")]
    pub workspace_root: String,
    #[serde(default)]
    pub allowed_effects: Option<AllowedEffects>,
    #[serde(default)]
    pub budget: Option<ExecutionBudget>,
}

fn default_repository() -> String {
    "local/repo".into()
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

/// Make a freshly published directory entry durable. Renaming or
/// linking a file only updates the directory; without syncing it, a
/// power loss can leave the old directory contents even though the
/// file's own data survived.
fn sync_parent(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        let parent = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        std::fs::File::open(parent)?.sync_all()
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(())
    }
}

/// Publish the complete scratch file at `path`. A hard link is the
/// preferred atomic publish: `AlreadyExists` means a concurrent first
/// writer won, so this writer adopts that complete file. Filesystems
/// without hard links (some FAT/exFAT, SMB/CIFS, or FUSE mounts) fall
/// back to a same-directory rename, which is still atomic and still
/// publishes only complete content; a winner that appeared before the
/// rename is adopted rather than overwritten. Either way the parent
/// directory is synced so the new name survives a crash.
fn publish(path: &Path, scratch: &Path) -> Result<(), String> {
    publish_with(path, scratch, |from, to| std::fs::hard_link(from, to))
}

fn publish_with(
    path: &Path,
    scratch: &Path,
    link: impl FnOnce(&Path, &Path) -> std::io::Result<()>,
) -> Result<(), String> {
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
/// live writer's file, and a crashed writer leaves only a scratch
/// file, never a broken config. On filesystems without hard links the
/// same atomic publish falls back to a rename (see `publish`).
pub fn load_or_create(path: &Path) -> Result<Installation, String> {
    if let Some(installation) = read(path)? {
        return Ok(installation);
    }
    let installation = Installation {
        actor_id: format!("actor-{}", uuidish()),
        actor_kind: "human".into(),
        repository: default_repository(),
        capacity: default_capacity(),
        worker_argv: vec![],
        workspace_root: default_workspace_root(),
        allowed_effects: None,
        budget: None,
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
    publish(path, &scratch)?;
    // Return the file that actually won the publish, not necessarily the
    // record this process staged: concurrent first writers must all
    // report the one identity the installation now holds.
    read(path)?.ok_or_else(|| "config: first writer's file vanished".into())
}

impl Installation {
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
    pub fn workloop_config(&self) -> WorkloopConfig {
        WorkloopConfig {
            default_repository: self.repository.clone(),
            allowed_effects: self.allowed_effects.clone().unwrap_or(AllowedEffects {
                workspace_write: true,
                github_write: false,
                network: false,
            }),
            budget: self.budget.clone().unwrap_or_default(),
        }
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
}
