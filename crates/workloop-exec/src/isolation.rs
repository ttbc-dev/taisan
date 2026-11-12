//! The launch half of the Worker isolation profile's permission
//! boundary (U2.3): the environment every spawned writer and `describe`
//! probe runs under. It is *built*, never inherited — `env_clear` plus
//! the profile's passthrough names plus generated entries — so a host
//! credential cannot reach the worker through `GH_TOKEN`,
//! `SSH_AUTH_SOCK`, `HOME`, `XDG_CONFIG_HOME`, `GIT_CONFIG_*`, or any
//! other ambient variable the owner happens to export.
//!
//! Generated entries replace the credential-bearing filesystem a
//! plain `env_clear` cannot fix: `HOME` and `TMPDIR` point at a fresh
//! installation-owned scratch (`~/.config/gh`, `~/.ssh`,
//! `~/.git-credentials`, `~/.gitconfig` all resolve inside it), and
//! `GIT_CONFIG_GLOBAL` names a generated config that empties every
//! credential helper and denies every transport protocol except
//! `file`, so a workspace's own repo config cannot route git traffic
//! through a credential helper or an ssh/https remote.
//!
//! This is the hygiene layer the worker protocol calls "required, not
//! sufficient": same-UID filesystem reads outside the env boundary are
//! blocked by the namespace machinery the separate U2.4/U4 slices own.
//! Preparation is fail-closed — a scratch directory or gitconfig write
//! that fails refuses the launch; there is no degrade-to-inherited-env
//! path.

use std::path::{Path, PathBuf};

use workloop_core::ports::WorkerEnv;
use workloop_core::worker::IsolationProfile;

/// `GIT_CONFIG_GLOBAL` inside the launch: credential helpers emptied,
/// every transport denied except `file` — the local transport the
/// handoff transport legitimately uses. `protocol.file.allow` is the
/// more specific key and overrides the blanket `never`.
const GENERATED_GITCONFIG: &str = "[credential]\n\thelper =\n[protocol]\n\tallow = never\n[protocol \"file\"]\n\tallow = always\n";

/// Build the environment for one launch or probe under `scratch`
/// (`<workspace_root>/.isolation/<key>/`): creates `home/` and `tmp/`
/// and writes the generated `gitconfig`, then returns the complete
/// child environment. Errors name the failing path; callers must
/// refuse the launch, never fall back to inherited variables.
pub fn prepare(profile: &IsolationProfile, scratch: &Path) -> Result<WorkerEnv, String> {
    let home = scratch.join("home");
    let tmp = scratch.join("tmp");
    for dir in [&home, &tmp] {
        std::fs::create_dir_all(dir)
            .map_err(|e| format!("isolation scratch {}: {e}", dir.display()))?;
    }
    let gitconfig = scratch.join("gitconfig");
    std::fs::write(&gitconfig, GENERATED_GITCONFIG)
        .map_err(|e| format!("isolation gitconfig {}: {e}", gitconfig.display()))?;

    let mut vars: Vec<(String, String)> = Vec::new();
    for name in profile.env_passthrough {
        if let Ok(value) = std::env::var(name) {
            vars.push((name.to_string(), value));
        }
    }
    vars.push(("HOME".to_string(), home.to_string_lossy().into_owned()));
    vars.push(("TMPDIR".to_string(), tmp.to_string_lossy().into_owned()));
    vars.push(("GIT_CONFIG_NOSYSTEM".to_string(), "1".to_string()));
    vars.push((
        "GIT_CONFIG_GLOBAL".to_string(),
        gitconfig.to_string_lossy().into_owned(),
    ));
    vars.push(("GIT_TERMINAL_PROMPT".to_string(), "0".to_string()));
    Ok(WorkerEnv { vars })
}

/// The scratch root every launch and probe builds its environment
/// under — a sibling of the `ws-*` workspaces inside the managed
/// workspace root, outside any git workspace so the worker's own
/// `git add -A` cannot sweep it into an artifact.
pub fn scratch_root(workspace_root: &Path) -> PathBuf {
    workspace_root.join(".isolation")
}

/// A filename-safe key for a scratch directory: keeps letters, digits,
/// `_`, `-`; anything else — including `.`, so a `..` component can
/// never form — collapses to `-`.
pub fn sanitize_key(id: &str) -> String {
    id.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '_' | '-') {
                c
            } else {
                '-'
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use workloop_core::worker::default_isolation_profile;

    fn tmp(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("workloop-isolation-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn prepare_builds_a_scrubbed_environment() {
        let scratch = tmp("prepare").join("env");
        let env = prepare(&default_isolation_profile(), &scratch).unwrap();
        let map: std::collections::BTreeMap<_, _> = env.vars.iter().cloned().collect();

        assert_eq!(map["HOME"], scratch.join("home").to_string_lossy());
        assert_eq!(map["TMPDIR"], scratch.join("tmp").to_string_lossy());
        assert_eq!(map["GIT_CONFIG_NOSYSTEM"], "1");
        assert_eq!(map["GIT_TERMINAL_PROMPT"], "0");
        assert_eq!(
            map["GIT_CONFIG_GLOBAL"],
            scratch.join("gitconfig").to_string_lossy()
        );
        assert!(scratch.join("gitconfig").exists());
        for denied in [
            "GH_TOKEN",
            "GITHUB_TOKEN",
            "SSH_AUTH_SOCK",
            "GIT_SSH_COMMAND",
            "GIT_ASKPASS",
            "XDG_CONFIG_HOME",
            "GH_CONFIG_DIR",
            "TAISAN_CONFIG",
        ] {
            assert!(!map.contains_key(denied), "{denied} must not leak");
        }
        let _ = std::fs::remove_dir_all(&scratch);
    }

    #[test]
    fn prepare_fails_closed_when_scratch_is_unwritable() {
        // A file where the scratch root must be a directory: the launch
        // is refused rather than retried under inherited env.
        let root = tmp("blocked");
        let scratch = root.join("env");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(&scratch, "not a dir").unwrap();
        let err = prepare(&default_isolation_profile(), &scratch).unwrap_err();
        assert!(err.contains("isolation scratch"), "{err}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn sanitize_key_strips_unsafe_characters() {
        assert_eq!(sanitize_key("exec-1:inv:0"), "exec-1-inv-0");
        assert_eq!(sanitize_key("../escape"), "---escape");
        assert_eq!(sanitize_key(".."), "--");
    }
}
