//! Installation config for the console. Same file and schema the CLI
//! uses: the actor identity persists here so it is stable across adapter
//! restarts; the (origin, actor, key) idempotency scope keeps the TUI's
//! `origin: "tui"` requests distinct from the CLI's under one operator
//! actor. Fields the console does not need (worker argv, capacity)
//! pass through untouched so a file either adapter wrote stays valid.

use std::path::Path;

use workloop_core::domain::{Actor, ActorKind, AllowedEffects, ExecutionBudget};
use workloop_core::work::WorkloopConfig;

#[derive(serde::Serialize, serde::Deserialize)]
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

/// Read the file once it is complete, retrying past a first writer's
/// brief partial view — a file observed between `create_new` and the
/// completed write is empty or half-written, never authoritative.
/// `Ok(None)` means the path is absent or was reclaimed from a crashed
/// first write (an empty file carries nothing, so it is removed for the
/// create path to claim). A nonempty file that stays unparseable is a
/// malformed config and errors.
fn read_installation(path: &Path) -> Result<Option<Installation>, String> {
    for _ in 0..100 {
        match std::fs::read_to_string(path) {
            Ok(text) if text.trim().is_empty() => {
                std::thread::sleep(std::time::Duration::from_millis(5))
            }
            Ok(text) => match serde_json::from_str(&text) {
                Ok(installation) => return Ok(Some(installation)),
                Err(_) => std::thread::sleep(std::time::Duration::from_millis(5)),
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(format!("config read: {e}")),
        }
    }
    match std::fs::read_to_string(path) {
        Ok(text) if text.trim().is_empty() => {
            let _ = std::fs::remove_file(path);
            Ok(None)
        }
        Ok(text) => match serde_json::from_str::<Installation>(&text) {
            Err(e) => Err(format!("config: {e}")),
            Ok(installation) => Ok(Some(installation)),
        },
        Err(e) => Err(format!("config read: {e}")),
    }
}

/// Read the installation config, creating it with a fresh stable actor
/// id on first use — identical shape and defaults to the CLI's loader so
/// either adapter can be the first writer. Creation is an exclusive
/// `create_new`: when CLI and TUI start concurrently before the file
/// exists, exactly one write lands and every loser adopts the file that
/// did, so both adapters converge on one actor identity instead of each
/// keeping its own generated id.
pub fn load_or_create(path: &Path) -> Result<Installation, String> {
    for _ in 0..3 {
        if let Some(installation) = read_installation(path)? {
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
        let text =
            serde_json::to_string_pretty(&installation).map_err(|e| format!("config: {e}"))?;
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
        {
            Ok(mut file) => {
                use std::io::Write;
                file.write_all(text.as_bytes())
                    .and_then(|()| file.sync_all())
                    .map_err(|e| format!("config write: {e}"))?;
                return Ok(installation);
            }
            // Lost the create race: adopt the winner's file — the next
            // pass reads it, retrying past its in-flight write.
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(format!("config create: {e}")),
        }
    }
    Err("config: first writer did not finish".into())
}

impl Installation {
    pub fn actor(&self) -> Actor {
        Actor {
            kind: match self.actor_kind.as_str() {
                "automation" => ActorKind::Automation,
                _ => ActorKind::Human,
            },
            external_id: self.actor_id.clone(),
        }
    }

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
