//! The shared installation config contract: one schema and one loader
//! for every adapter. The actor identity persists across relaunches,
//! concurrent first writers converge on one identity, a reader never
//! observes a partial write, and content the loader did not publish
//! errors instead of being deleted or overwritten.

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

/// A file the loader did not publish can only be foreign — this loader
/// never produces empty or partial config, since a first write stages
/// complete content and publishes it atomically. Such a file errors
/// rather than being deleted or overwritten: unlinking it could race
/// a concurrent publisher and remove a live config.
#[test]
fn foreign_empty_or_malformed_config_errors() {
    let dir = root("install-foreign");
    let path = dir.join("config.json");

    // A crashed foreign write left an empty file — reported, not
    // reclaimed: the operator removes it to reinitialize.
    std::fs::write(&path, "").unwrap();
    let err = load_or_create(&path).unwrap_err();
    assert!(
        err.contains("empty"),
        "expected the empty-file error: {err}"
    );

    // Malformed nonempty content errors rather than being overwritten.
    std::fs::write(&path, "{ not json").unwrap();
    assert!(load_or_create(&path).is_err());

    let _ = std::fs::remove_dir_all(&dir);
}

/// An installation file written before capability/actor declarations
/// existed still parses — serde defaults fill empty values — but it
/// must be refused at the boundary to `WorkloopConfig`, not silently
/// turned into a config that rejects every Submit.
#[test]
fn legacy_config_without_declarations_is_refused() {
    let dir = root("install-legacy");
    let path = dir.join("config.json");
    std::fs::write(
        &path,
        r#"{
            "actor_id": "actor-legacy",
            "actor_kind": "human",
            "host_instance": "host-local",
            "repository": "local-repo",
            "adapter": "fixture"
        }"#,
    )
    .unwrap();
    let installation = load_or_create(&path).unwrap();
    let err = installation.workloop_config().unwrap_err();
    assert!(
        err.contains("capabilities"),
        "expected the declaration error: {err}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Identity fields are external input: empty, oversized, or
/// control-character values are rejected at the boundary instead of
/// silently binding Work to a misconfigured host.
#[test]
fn invalid_identity_fields_are_refused() {
    let dir = root("install-invalid-id");
    let path = dir.join("config.json");
    let installation = load_or_create(&path).unwrap();

    for (field, value) in [
        ("host_instance", ""),
        ("repository", "bad\nrepo"),
        ("adapter", "\t"),
    ] {
        let mut i = installation.clone();
        match field {
            "host_instance" => i.host_instance = value.into(),
            "repository" => i.repository = value.into(),
            _ => i.adapter = value.into(),
        }
        let err = i.workloop_config().unwrap_err();
        assert!(err.contains(field), "expected {field} error: {err}");
    }

    // An unknown role name is a hard error too, never a silently
    // dropped binding.
    let mut i = installation.clone();
    i.host_actors.insert("bogus".into(), "agent".into());
    let err = i.workloop_config().unwrap_err();
    assert!(err.contains("bogus"), "expected role error: {err}");

    let _ = std::fs::remove_dir_all(&dir);
}

/// The negotiated contract version is binding data: a fresh
/// installation records version 1, and a versionless file is refused
/// at the boundary rather than producing versionless host bindings.
#[test]
fn contract_version_zero_is_refused() {
    let dir = root("install-version");
    let path = dir.join("config.json");
    let installation = load_or_create(&path).unwrap();
    assert_eq!(installation.contract_version, 1, "fresh default is v1");
    assert!(installation.workloop_config().is_ok());

    let mut legacy = installation.clone();
    legacy.contract_version = 0;
    let err = legacy.workloop_config().unwrap_err();
    assert!(
        err.contains("contract_version"),
        "expected the contract_version error: {err}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// The installation's `worker_isolation_profile` resolves fail-closed
/// against the approved registry (U2.3): absent names the reference
/// profile, an approved name resolves, and an unknown name is an error
/// at this boundary — never a silent downgrade to unprofiled launches.
#[test]
fn worker_isolation_profile_is_fail_closed() {
    let dir = root("install-isolation");
    let path = dir.join("config.json");
    let installation = load_or_create(&path).unwrap();
    let profile = installation.worker_isolation_profile().unwrap();
    assert_eq!(profile.id, "taisan-reference-v0.1");

    let mut named = installation.clone();
    named.worker_isolation_profile = Some("fixture-worker-v0.1".into());
    assert_eq!(
        named.worker_isolation_profile().unwrap().id,
        "fixture-worker-v0.1"
    );

    let mut invalid = installation;
    invalid.worker_isolation_profile = Some("made-up".into());
    let err = invalid.worker_isolation_profile().unwrap_err();
    assert!(
        err.contains("worker_isolation_profile") && err.contains("not an approved profile"),
        "expected the registry refusal: {err}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}
