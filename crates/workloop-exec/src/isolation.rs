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
//! `~/.git-credentials`, `~/.gitconfig` all resolve inside it).
//! Git is pinned through the environment because repository-local
//! `.git/config` outranks `GIT_CONFIG_GLOBAL`: `GIT_ALLOW_PROTOCOL=file`
//! denies every transport except the local one no matter which
//! `protocol.<name>` a workspace plants, and `GIT_CONFIG_COUNT` sets an
//! empty `credential.helper` at command scope — the highest precedence —
//! so a planted helper is cleared before it can run. The generated
//! `GIT_CONFIG_GLOBAL` file carries the same rules as a second layer
//! (and as what `git config --global` reads), and `GIT_TERMINAL_PROMPT=0`
//! turns a remaining prompt into a failure.
//!
//! This is the hygiene layer the worker protocol calls "required, not
//! sufficient": same-UID filesystem reads outside the env boundary are
//! blocked by the namespace machinery the separate U2.4/U4 slices own.
//! Preparation is fail-closed — a scratch directory or gitconfig write
//! that fails refuses the launch; there is no degrade-to-inherited-env
//! path.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use workloop_core::ports::WorkerEnv;
use workloop_core::worker::IsolationProfile;

/// `GIT_CONFIG_GLOBAL` inside the launch: credential helpers emptied,
/// every transport denied except `file` — the local transport the
/// handoff transport legitimately uses. `protocol.file.allow` is the
/// more specific key and overrides the blanket `never`. This file sits
/// at global scope, which repository-local `.git/config` outranks, so
/// [`prepare`] additionally pins `GIT_ALLOW_PROTOCOL=file` and an
/// empty `credential.helper` at command scope.
const GENERATED_GITCONFIG: &str = "[credential]\n\thelper =\n[protocol]\n\tallow = never\n[protocol \"file\"]\n\tallow = always\n";

/// Build the environment for one launch or probe under `scratch`
/// (`<workspace_root>/.isolation/<key>/`): creates `home/` and `tmp/`
/// and writes the generated `gitconfig`, then returns the complete
/// child environment. Errors name the failing path; callers must
/// refuse the launch, never fall back to inherited variables.
///
/// A scratch tree left by an earlier use of this key — a re-probe, a
/// failed release, a crashed pass — is swept first, so reuse starts
/// from an empty tree or not at all; a non-directory occupier survives
/// the sweep and fails the creation below. A refused preparation
/// reclaims its own partial tree before returning the error.
pub fn prepare(profile: &IsolationProfile, scratch: &Path) -> Result<WorkerEnv, String> {
    let _ = std::fs::remove_dir_all(scratch);
    match build(profile, scratch) {
        Ok(env) => Ok(env),
        Err(why) => {
            let _ = std::fs::remove_dir_all(scratch);
            Err(why)
        }
    }
}

fn build(profile: &IsolationProfile, scratch: &Path) -> Result<WorkerEnv, String> {
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
    // Repository-local config outranks `GIT_CONFIG_GLOBAL`, so the two
    // rules a workspace could otherwise override are also pinned at
    // command scope: `GIT_ALLOW_PROTOCOL` beats every `protocol.*`
    // config key (a planted `protocol.<name>.allow = always` cannot
    // reopen a transport), and an empty `credential.helper` at
    // `GIT_CONFIG_COUNT` clears every helper a lower scope accumulated.
    vars.push(("GIT_ALLOW_PROTOCOL".to_string(), "file".to_string()));
    vars.push(("GIT_CONFIG_COUNT".to_string(), "1".to_string()));
    vars.push((
        "GIT_CONFIG_KEY_0".to_string(),
        "credential.helper".to_string(),
    ));
    vars.push(("GIT_CONFIG_VALUE_0".to_string(), String::new()));
    Ok(WorkerEnv { vars })
}

/// Best-effort reclamation of a scratch tree once its probe or
/// Invocation can no longer use it — after the probe process returns
/// and after the writer is proven stopped or refused before it
/// started. Never fails a caller: reclamation is not part of the
/// launch's fail-closed contract, and a leftover tree is swept the
/// next time the same key is prepared.
pub fn release(scratch: &Path) {
    let _ = std::fs::remove_dir_all(scratch);
}

/// Reclaim every scratch tree under `scratch_root` that `keep` does
/// not name — the per-pass bound on `.isolation` growth. `keep`
/// carries the scratch directories unresolved Invocations still own
/// (a live or unproven writer's `HOME`/`TMPDIR` is never touched);
/// every other `inv-*` entry is a leftover of a resolved or crashed
/// launch, every `probe-*` entry is a leftover of an interrupted
/// declaration — a probe holds its tree only for the synchronous
/// `describe` call, so one found on disk is already dead — and every
/// `check-*` entry is a leftover of a resolved or crashed invariant
/// check whose best-effort release failed. Entries outside the owned
/// prefixes are foreign and never touched.
pub fn sweep_orphans(scratch_root: &Path, keep: &HashSet<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(scratch_root) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if !(name.starts_with("inv-") || name.starts_with("probe-") || name.starts_with("check-")) {
            continue;
        }
        if keep.contains(&entry.path()) {
            continue;
        }
        release(&entry.path());
    }
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
        // Command-scope pins: they outrank repository-local config, so
        // a workspace's own `.git/config` cannot reopen a transport or
        // restore a credential helper.
        assert_eq!(map["GIT_ALLOW_PROTOCOL"], "file");
        assert_eq!(map["GIT_CONFIG_COUNT"], "1");
        assert_eq!(map["GIT_CONFIG_KEY_0"], "credential.helper");
        assert!(map.contains_key("GIT_CONFIG_VALUE_0"));
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
        // is refused rather than retried under inherited env, and the
        // occupier itself is never swept away by the cleanup.
        let root = tmp("blocked");
        let scratch = root.join("env");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(&scratch, "not a dir").unwrap();
        let err = prepare(&default_isolation_profile(), &scratch).unwrap_err();
        assert!(err.contains("isolation scratch"), "{err}");
        assert_eq!(std::fs::read_to_string(&scratch).unwrap(), "not a dir");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn prepare_starts_a_reused_key_from_an_empty_tree() {
        // A scratch tree left behind by an earlier use of the same key
        // never leaks into the next environment.
        let scratch = tmp("stale").join("env");
        std::fs::create_dir_all(scratch.join("home")).unwrap();
        std::fs::write(scratch.join("home").join("stale"), "old").unwrap();
        prepare(&default_isolation_profile(), &scratch).unwrap();
        assert!(!scratch.join("home").join("stale").exists());
        let _ = std::fs::remove_dir_all(scratch.parent().unwrap());
    }

    #[test]
    fn release_reclaims_a_scratch_tree_and_ignores_non_trees() {
        let root = tmp("release");
        let scratch = root.join("env");
        prepare(&default_isolation_profile(), &scratch).unwrap();
        assert!(scratch.join("home").exists());
        release(&scratch);
        assert!(!scratch.exists());
        release(&scratch); // absent is already reclaimed
        // A non-directory occupier is not a scratch tree: best-effort
        // reclamation never deletes it.
        let occupier = root.join("occupied");
        std::fs::write(&occupier, "keep").unwrap();
        release(&occupier);
        assert_eq!(std::fs::read_to_string(&occupier).unwrap(), "keep");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn sanitize_key_strips_unsafe_characters() {
        assert_eq!(sanitize_key("exec-1:inv:0"), "exec-1-inv-0");
        assert_eq!(sanitize_key("../escape"), "---escape");
        assert_eq!(sanitize_key(".."), "--");
    }

    #[test]
    fn sweep_orphans_reclaims_only_unowned_scratch_prefixes() {
        let root = tmp("sweep").join("env");
        let keep = root.join("inv-inv-1");
        prepare(&default_isolation_profile(), &keep).unwrap();
        let orphan_inv = root.join("inv-inv-2");
        let orphan_probe = root.join("probe-req-1");
        let orphan_check = root.join("check-inv-3");
        let kept_check = root.join("check-inv-4");
        for dir in [&orphan_inv, &orphan_probe, &orphan_check, &kept_check] {
            std::fs::create_dir_all(dir.join("home")).unwrap();
        }
        let foreign = root.join("foreign");
        std::fs::create_dir_all(&foreign).unwrap();

        let keep_set = std::collections::HashSet::from([keep.clone(), kept_check.clone()]);
        sweep_orphans(&root, &keep_set);

        assert!(keep.exists(), "a kept scratch tree survives");
        assert!(
            kept_check.exists(),
            "a kept check snapshot survives the sweep"
        );
        assert!(!orphan_inv.exists(), "an unowned inv-* tree is reclaimed");
        assert!(
            !orphan_probe.exists(),
            "an unowned probe-* tree is reclaimed"
        );
        assert!(
            !orphan_check.exists(),
            "an unowned check-* snapshot is reclaimed"
        );
        assert!(foreign.exists(), "entries outside the prefixes are kept");
        let _ = std::fs::remove_dir_all(root.parent().unwrap());
    }
}
