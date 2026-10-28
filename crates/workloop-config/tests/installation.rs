//! The shared installation config contract: one schema and one loader
//! for every adapter. The actor identity persists across relaunches,
//! concurrent first writers converge on one identity, a reader never
//! observes a partial write, and malformed content errors instead of
//! being overwritten.

use std::path::PathBuf;

use workloop_config::load_or_create;

fn root(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("workloop-config-{}-{}", tag, std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// The actor identity persists in the shared installation config: a
/// first launch creates it, a relaunch reuses it, and the file carries
/// exactly the schema every adapter writes and reads.
#[test]
fn tui_actor_identity_is_stable_and_cli_compatible() {
    let dir = root("stable");
    let config_path = dir.join("config.json");

    let first = load_or_create(&config_path).unwrap();
    let second = load_or_create(&config_path).unwrap();
    assert_eq!(first.actor_id, second.actor_id, "actor id is stable");
    assert_eq!(first.actor_kind, second.actor_kind);

    // The file carries the shared schema: the required identity fields
    // plus the loop settings either adapter defaults.
    let written: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&config_path).unwrap()).unwrap();
    for key in [
        "actor_id",
        "actor_kind",
        "repository",
        "capacity",
        "worker_argv",
        "workspace_root",
    ] {
        assert!(written.get(key).is_some(), "config missing {key}");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// Concurrent first writers converge on one actor identity: exactly
/// one publish lands and every loser adopts that complete file.
#[test]
fn concurrent_first_writers_share_one_actor() {
    let dir = root("install-race");
    let path = std::sync::Arc::new(dir.join("config.json"));
    let handles: Vec<_> = (0..8)
        .map(|_| {
            let p = path.clone();
            std::thread::spawn(move || load_or_create(&p).map(|i| i.actor_id))
        })
        .collect();
    let ids: std::collections::HashSet<String> = handles
        .into_iter()
        .map(|h| h.join().unwrap().unwrap())
        .collect();
    assert_eq!(ids.len(), 1, "concurrent writers diverged: {ids:?}");
    let written: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(path.as_ref()).unwrap()).unwrap();
    assert_eq!(
        written["actor_id"].as_str().unwrap(),
        ids.iter().next().unwrap()
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// A config file observed mid-write can only be foreign (this loader
/// publishes complete files atomically), so an empty file is reclaimed
/// and a fresh stable identity created. A malformed nonempty file
/// still errors — it is never overwritten.
#[test]
fn install_recovers_partial_and_errors_malformed() {
    let dir = root("install-partial");
    let path = dir.join("config.json");

    // A crashed foreign write left an empty file — reclaimed, not fatal.
    std::fs::write(&path, "").unwrap();
    let created = load_or_create(&path).unwrap();
    let reread = load_or_create(&path).unwrap();
    assert_eq!(created.actor_id, reread.actor_id);

    // Malformed nonempty content errors rather than being overwritten.
    std::fs::write(&path, "{ not json").unwrap();
    assert!(load_or_create(&path).is_err());

    let _ = std::fs::remove_dir_all(&dir);
}
