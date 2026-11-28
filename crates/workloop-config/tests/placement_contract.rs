//! The executor-placement configuration contract (#379,
//! `docs/remote-executor.md` §2.2): the section is owned by
//! `workloop-config`, carries an explicit `schema_version`, and is
//! fail-closed at the `executor_placement` boundary — an unknown
//! version, unknown key, or malformed endpoint is a hard configuration
//! error, never a silent default. An absent section is the only
//! exception: it resolves to the implicit all-local v0.3 policy, so a
//! local-only installation needs zero new fields. The block rides the
//! installation record as raw JSON and its validation outcome rides
//! `WorkloopConfig.placement` as a `Result`, so a malformed section
//! fails the run path only — never `load`, `submit`, or any other
//! command.

use workloop_config::Installation;
use workloop_core::domain::InvocationRole;
use workloop_core::placement::{PLACEMENT_SCHEMA_VERSION, PlacementTarget};

/// The minimal installation record every scenario builds from —
/// required identity fields plus the declarations `workloop_config`
/// refuses without (capabilities, a bound actor, the contract
/// version); the rest default.
fn installation(extra: serde_json::Value) -> Installation {
    let mut value = serde_json::json!({
        "actor_id": "actor-1",
        "actor_kind": "human",
        "contract_version": 1,
        "capabilities": ["repository_read"],
        "host_actors": { "publication": "pub-agent" },
        "worker_argv": ["worker", "run"],
    });
    let base = value.as_object_mut().unwrap();
    if let serde_json::Value::Object(map) = extra {
        for (k, v) in map {
            base.insert(k, v);
        }
    }
    serde_json::from_value(value).expect("installation parses")
}

/// The one static endpoint descriptor a valid remote policy names.
fn endpoint_json() -> serde_json::Value {
    serde_json::json!({
        "installation_id": "inst-1",
        "endpoint_id": "edge-1",
        "kind": "ssh",
        "host": "edge-1.internal",
        "port": 2222,
        "expected_identity_digest": "ab".repeat(32),
        "endpoint_credential_ref": "env:TAISAN_ENDPOINT_KEY",
        "coordinator_credential_ref": "env:TAISAN_COORDINATOR_KEY",
    })
}

/// A `worker_dispatch` section — a remote target requires the
/// complete bound-request transport.
fn worker_dispatch_json() -> serde_json::Value {
    serde_json::json!({
        "source_path": "/srv/checkout",
        "budget": {
            "max_turns": 8,
            "max_report_bytes": 1048576,
            "max_log_bytes": 33554432,
            "max_verification_commands": 4,
            "max_continuation_refs": 4
        },
        "capabilities": ["artifact_json", "process_control"]
    })
}

/// Absent means the implicit all-local policy — the v0.3 behavior
/// with no new mandatory configuration.
#[test]
fn absent_placement_resolves_the_all_local_policy() {
    let inst = installation(serde_json::json!({}));
    let policy = inst.executor_placement().unwrap();
    assert_eq!(policy.schema_version, 0);
    for role in [
        InvocationRole::Coding,
        InvocationRole::Fix,
        InvocationRole::InvariantCheck,
        InvocationRole::Review,
    ] {
        let binding = policy.binding_for(&role);
        assert_eq!(binding.target, PlacementTarget::Local);
        assert_eq!(binding.executor, "local");
        assert!(binding.endpoint.is_none());
    }
    assert_eq!(policy.verification, PlacementTarget::Local);
}

/// The block is raw JSON on the record: a structurally malformed
/// section still loads — `load`/`submit` and every other command are
/// unaffected — and fails only at the run-path boundary.
#[test]
fn malformed_placement_fails_at_the_boundary_not_at_load() {
    let inst = installation(serde_json::json!({ "placement": "garbage" }));
    let err = inst.executor_placement().unwrap_err();
    assert!(
        err.contains("installation placement"),
        "unexpected error: {err}"
    );
}

/// `schema_version` is explicit and required — serde refuses the
/// whole section when it is absent.
#[test]
fn missing_schema_version_is_a_hard_error() {
    let inst = installation(serde_json::json!({
        "placement": { "roles": {} }
    }));
    let err = inst.executor_placement().unwrap_err();
    assert!(err.contains("schema_version"), "unexpected error: {err}");
}

/// An unknown version refuses closed — never a silent default.
#[test]
fn unknown_schema_version_refuses() {
    let inst = installation(serde_json::json!({
        "placement": { "schema_version": PLACEMENT_SCHEMA_VERSION + 9 }
    }));
    let err = inst.executor_placement().unwrap_err();
    assert!(err.contains("unsupported"), "unexpected error: {err}");
}

/// Unknown keys are refused: a misspelled role or endpoint field can
/// never silently bind nothing.
#[test]
fn unknown_keys_refuse() {
    let inst = installation(serde_json::json!({
        "placement": {
            "schema_version": PLACEMENT_SCHEMA_VERSION,
            "roles": { "reivew": "remote" }
        }
    }));
    assert!(inst.executor_placement().is_err());

    let inst = installation(serde_json::json!({
        "placement": {
            "schema_version": PLACEMENT_SCHEMA_VERSION,
            "endpoint": {
                "installation_id": "inst-1",
                "endpoint_id": "edge-1",
                "kind": "ssh",
                "host": "edge-1.internal",
                "port": 2222,
                "expected_identity_digest": "ab".repeat(32),
                "endpoint_credential_ref": "env:K",
                "coordinator_credential_ref": "env:C",
                "typosquat": true
            }
        }
    }));
    assert!(inst.executor_placement().is_err());
}

/// A remote target without the endpoint descriptor is a
/// configuration error — the one static endpoint is all the policy
/// may name.
#[test]
fn remote_target_requires_the_endpoint() {
    let inst = installation(serde_json::json!({
        "placement": {
            "schema_version": PLACEMENT_SCHEMA_VERSION,
            "roles": { "review": "remote" }
        },
        "worker_dispatch": worker_dispatch_json()
    }));
    let err = inst.executor_placement().unwrap_err();
    assert!(err.contains("endpoint"), "unexpected error: {err}");
}

/// A remote target without `worker_dispatch` refuses: the complete
/// bound-request transport is the only remote dispatch path.
#[test]
fn remote_target_requires_worker_dispatch() {
    let inst = installation(serde_json::json!({
        "placement": {
            "schema_version": PLACEMENT_SCHEMA_VERSION,
            "endpoint": endpoint_json(),
            "roles": { "review": "remote" }
        }
    }));
    let err = inst.executor_placement().unwrap_err();
    assert!(err.contains("worker_dispatch"), "unexpected error: {err}");
}

/// Every endpoint field is checked at the boundary: a malformed value
/// is a hard error, never a partially bound endpoint.
#[test]
fn endpoint_fields_validate() {
    let cases: Vec<(serde_json::Value, &'static str)> = vec![
        (serde_json::json!({ "kind": "https" }), "unknown transport"),
        (
            serde_json::json!({ "host": "edge-1.internal/path" }),
            "bare host name",
        ),
        (serde_json::json!({ "host": "::1" }), "bare host name"),
        (serde_json::json!({ "port": 0 }), "not a port"),
        (
            serde_json::json!({ "expected_identity_digest": "AB".repeat(32) }),
            "lowercase hex",
        ),
        (
            serde_json::json!({ "expected_identity_digest": "ab" }),
            "lowercase hex",
        ),
        (
            serde_json::json!({ "endpoint_credential_ref": "literal-secret-value" }),
            "credential_ref",
        ),
        (
            serde_json::json!({ "coordinator_credential_ref": "file:relative/path" }),
            "credential_ref",
        ),
        (
            serde_json::json!({ "allowed_protocols": [] }),
            "at least one",
        ),
    ];
    for (patch, needle) in cases {
        let mut endpoint = endpoint_json();
        let map = endpoint.as_object_mut().unwrap();
        for (k, v) in patch.as_object().unwrap() {
            map.insert(k.clone(), v.clone());
        }
        let inst = installation(serde_json::json!({
            "placement": {
                "schema_version": PLACEMENT_SCHEMA_VERSION,
                "endpoint": endpoint,
                "roles": { "review": "remote" }
            },
            "worker_dispatch": worker_dispatch_json()
        }));
        let err = inst.executor_placement().unwrap_err();
        assert!(
            err.contains(needle),
            "patch {patch:?}: expected '{needle}' in error: {err}"
        );
    }
}

/// Review-first mixed mode: Review + InvariantCheck remote, Coding +
/// Fix local — config-driven, no role mapping hardcoded.
#[test]
fn review_first_mixed_mode_binds_per_role() {
    let inst = installation(serde_json::json!({
        "placement": {
            "schema_version": PLACEMENT_SCHEMA_VERSION,
            "endpoint": endpoint_json(),
            "roles": {
                "review": "remote",
                "invariant_check": "remote"
            }
        },
        "worker_dispatch": worker_dispatch_json()
    }));
    let policy = inst.executor_placement().unwrap();
    assert_eq!(policy.schema_version, PLACEMENT_SCHEMA_VERSION);
    assert!(!policy.config_digest.is_empty());
    assert_eq!(policy.review, PlacementTarget::Remote);
    assert_eq!(policy.invariant_check, PlacementTarget::Remote);
    assert_eq!(policy.coding, PlacementTarget::Local);
    assert_eq!(policy.fix, PlacementTarget::Local);
    assert_eq!(policy.verification, PlacementTarget::Local);
    assert!(policy.uses_remote());

    let review = policy.binding_for(&InvocationRole::Review);
    assert_eq!(review.target, PlacementTarget::Remote);
    assert_eq!(review.executor, "edge-1");
    assert_eq!(review.endpoint.as_ref().unwrap().host, "edge-1.internal");
    assert_eq!(review.schema_version, PLACEMENT_SCHEMA_VERSION);
    assert_eq!(review.config_digest, policy.config_digest);

    let coding = policy.binding_for(&InvocationRole::Coding);
    assert_eq!(coding.target, PlacementTarget::Local);
    assert_eq!(coding.executor, "local");
    assert!(coding.endpoint.is_none());
}

/// One all-remote endpoint: every role target plus verification bind
/// to the same descriptor.
#[test]
fn all_remote_binds_every_role_to_the_one_endpoint() {
    let inst = installation(serde_json::json!({
        "placement": {
            "schema_version": PLACEMENT_SCHEMA_VERSION,
            "endpoint": endpoint_json(),
            "roles": {
                "coding": "remote",
                "fix": "remote",
                "invariant_check": "remote",
                "review": "remote"
            },
            "verification": "remote"
        },
        "worker_dispatch": worker_dispatch_json()
    }));
    let policy = inst.executor_placement().unwrap();
    assert_eq!(policy.verification, PlacementTarget::Remote);
    for role in [
        InvocationRole::Coding,
        InvocationRole::Fix,
        InvocationRole::InvariantCheck,
        InvocationRole::Review,
    ] {
        let binding = policy.binding_for(&role);
        assert_eq!(binding.target, PlacementTarget::Remote);
        assert_eq!(binding.executor, "edge-1");
        assert_eq!(binding.endpoint.as_ref().unwrap().endpoint_id, "edge-1");
    }
}

/// `workloop_config` carries the validation outcome — the same
/// surface admission stamps bindings from.
#[test]
fn workloop_config_wires_the_validated_policy() {
    let inst = installation(serde_json::json!({
        "placement": {
            "schema_version": PLACEMENT_SCHEMA_VERSION,
            "endpoint": endpoint_json(),
            "roles": { "review": "remote" }
        },
        "worker_dispatch": worker_dispatch_json()
    }));
    let policy = inst.executor_placement().unwrap();
    let config = inst.workloop_config().unwrap();
    assert_eq!(config.placement, Ok(policy));
}

/// The malformed section's bounded error rides the config instead of
/// failing it: `workloop_config` resolves for submit/status/inspect
/// and every other command, and only the dispatch paths that consume
/// `config.placement` fail closed.
#[test]
fn malformed_placement_defers_to_the_dispatch_boundary() {
    let bad = installation(serde_json::json!({
        "placement": { "schema_version": PLACEMENT_SCHEMA_VERSION + 5 }
    }));
    let config = bad.workloop_config().unwrap();
    let err = config
        .placement
        .expect_err("the section's bounded error rides the config");
    assert!(err.contains("unsupported"), "unexpected error: {err}");
}
