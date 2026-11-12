//! Worker adapter selection and binding. One durable request is bound to
//! exactly one adapter declaration: the first binding recorded on the
//! request is the stored declaration, immutable for the request's
//! life. A later `worker_argv` change applies only to requests not yet
//! bound. At launch the bound argv is probed again; a
//! declaration that no longer matches the request's stored binding is a
//! configuration repair that authorizes no start, and a request refused
//! on that repair is terminal for its binding — the operator abandons it
//! and re-issues the work under a new request, which binds the current
//! declaration.
//!
//! `describe` is fail-closed: when the probe does not answer with a
//! declaration (spawn refused, bound elapsed, non-zero exit, unparseable
//! body, or no adapter identity), the adapter is not selectable — no
//! synthetic declaration is compared or dispatched on, so a transient
//! probe failure can never masquerade as a version mismatch or an empty
//! capability surface.

use std::path::Path;
use std::time::Duration;

use workloop_core::domain::{WorkerAdapterBinding, canonical_json, sha256_hex};
use workloop_core::identity::AdapterId;
use workloop_core::ports::{ProbeOutcome, ProcessControlPort, SpawnError, WorkerEnv};

/// The effect-free declaration an adapter reports through `describe`.
/// `capabilities` and `roles` are part of the request binding's
/// capability digest; the runtime version is compared against the
/// request's stored binding before a bound request may start a worker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkerDeclaration {
    pub adapter: AdapterId,
    pub adapter_version: String,
    /// The declared runtime product identity (`runtime.name` or
    /// `runtime.product`). A product swap under the same version string
    /// is a different worker, so the product digests with the
    /// capability surface while the version compares on its own.
    pub runtime_product: String,
    pub runtime_version: String,
    pub protocol_versions: Vec<String>,
    pub roles: Vec<String>,
    pub capabilities: Vec<String>,
    /// The declared `WorkerCapabilities` fields the supervisor does not
    /// model individually — `resume`, `checkpoints`,
    /// `runtime_artifact_store`, `local_retries` — kept verbatim so a
    /// drift confined to any of them still shifts the capability
    /// digest into a configuration repair instead of an invisible
    /// launch.
    pub declared_surface: serde_json::Value,
}

impl WorkerDeclaration {
    /// SHA-256 over the canonical declaration fields that define the
    /// adapter's contract surface. The runtime version is deliberately
    /// excluded: a version change is reported separately as
    /// `runtime.version_mismatch`, not folded into a capability change.
    /// The reported lists are sets, so the digest sorts them — an
    /// adapter reporting the same surface in a different order hashes
    /// identically.
    pub fn capability_digest(&self) -> String {
        let sorted = |items: &[String]| -> Vec<String> {
            let mut v = items.to_vec();
            v.sort();
            v
        };
        let payload = serde_json::json!({
            "adapter": self.adapter.as_str(),
            "adapter_version": self.adapter_version,
            "runtime_product": self.runtime_product,
            "protocol_versions": sorted(&self.protocol_versions),
            "roles": sorted(&self.roles),
            "capabilities": sorted(&self.capabilities),
            "declared_surface": self.declared_surface,
        });
        sha256_hex(
            "worker-capability-declaration-v1",
            &canonical_json(&payload),
        )
    }

    /// The immutable request binding for this declaration and argv.
    pub fn binding(&self, argv: &[String]) -> WorkerAdapterBinding {
        WorkerAdapterBinding {
            adapter: self.adapter.clone(),
            adapter_version: self.adapter_version.clone(),
            runtime_version: self.runtime_version.clone(),
            capability_digest: self.capability_digest(),
            argv: argv.to_vec(),
        }
    }
}

/// Why a selected adapter may not start a request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AdapterSelection {
    Ready(WorkerAdapterBinding),
    /// The freshly observed declaration no longer matches the request's
    /// stored binding. No spawn is authorized, and the request is
    /// terminal for its binding: it is abandoned and the work is
    /// re-issued under a new request, never resumed under the old one.
    ConfigurationRepair {
        reason: String,
    },
}

/// Verify a fresh `describe` observation against the request's stored
/// binding. Every differing field is named with its own reason code —
/// a capability change is never reported as a runtime-version repair.
pub fn select_adapter(
    observed: &WorkerDeclaration,
    stored: &WorkerAdapterBinding,
) -> AdapterSelection {
    let mut mismatches: Vec<String> = Vec::new();
    if observed.adapter != stored.adapter {
        mismatches.push(format!(
            "adapter.identity_mismatch: adapter {} declared, stored binding is {}",
            observed.adapter, stored.adapter
        ));
    }
    if observed.adapter_version != stored.adapter_version {
        mismatches.push(format!(
            "adapter.version_mismatch: adapter {} declared version {}, stored binding is {}",
            observed.adapter, observed.adapter_version, stored.adapter_version
        ));
    }
    if observed.runtime_version != stored.runtime_version {
        mismatches.push(format!(
            "runtime.version_mismatch: adapter {} declared runtime {}, stored binding is {}",
            observed.adapter, observed.runtime_version, stored.runtime_version
        ));
    }
    let observed_digest = observed.capability_digest();
    if observed_digest != stored.capability_digest {
        mismatches.push(format!(
            "capability.digest_mismatch: adapter {} declared capability {}, stored binding is {}",
            observed.adapter, observed_digest, stored.capability_digest
        ));
    }
    if mismatches.is_empty() {
        AdapterSelection::Ready(stored.clone())
    } else {
        AdapterSelection::ConfigurationRepair {
            reason: mismatches.join("; "),
        }
    }
}

/// Run the effect-free `describe` operation through the process-control
/// port (own process group, bounded wait, group signalled on timeout —
/// the same containment a writer gets) and parse the declaration. Every
/// failure is `Err`: no probe answer means no declaration and no
/// selection, so a failed `describe` never produces a synthetic
/// declaration that is then compared or dispatched on.
/// `env` is the allowlisted environment the adapter runs under — the
/// same profile environment a writer launch gets, so an untrusted
/// adapter binary sees no host credential during declaration.
pub fn observe_worker_declaration(
    port: &impl ProcessControlPort,
    argv: &[String],
    cwd: &Path,
    env: &WorkerEnv,
) -> Result<WorkerDeclaration, String> {
    observe_worker_declaration_within(port, argv, cwd, env, Duration::from_secs(5))
}

fn observe_worker_declaration_within(
    port: &impl ProcessControlPort,
    argv: &[String],
    cwd: &Path,
    env: &WorkerEnv,
    timeout: Duration,
) -> Result<WorkerDeclaration, String> {
    let mut probe_argv = argv.to_vec();
    probe_argv.push("describe".into());
    let outcome = port
        .run_probe(&probe_argv, cwd, env, timeout)
        .map_err(|e| match e {
            SpawnError::Refused { reason } => format!("describe spawn refused: {reason}"),
            SpawnError::Unknown { reason } => format!("describe spawn unknown: {reason}"),
        })?;
    let stdout = match outcome {
        ProbeOutcome::Exited {
            stdout,
            success: true,
        } => stdout,
        ProbeOutcome::Exited { success: false, .. } => {
            return Err("describe exited non-zero".into());
        }
        ProbeOutcome::Bounded => {
            return Err(format!("describe did not answer within {timeout:?}"));
        }
    };
    let value = serde_json::from_slice::<serde_json::Value>(&stdout)
        .map_err(|e| format!("describe output is not JSON: {e}"))?;
    let adapter = value
        .get("adapter")
        .or_else(|| value.get("adapter_id"))
        .and_then(|v| v.as_str())
        .filter(|v| !v.is_empty())
        .ok_or_else(|| "describe omitted the adapter identity".to_string())?;
    // The reported identity is untrusted worker output and gets bound
    // verbatim: it must pass the same `check_opaque` rules that gate
    // `AdapterId::new` and deserialization — an oversized or
    // control-character id stored now would fail every pending-request
    // read back (#152).
    let adapter = AdapterId::new(adapter)
        .map_err(|why| format!("describe declared an invalid adapter identity: {why}"))?;
    let strings = |key: &str| -> Vec<String> {
        value
            .get(key)
            .and_then(|v| v.as_array())
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| item.as_str().map(ToString::to_string))
                    .collect()
            })
            .unwrap_or_default()
    };
    let runtime = value.get("runtime");
    let declared = |key: &str| value.get(key).cloned().unwrap_or(serde_json::Value::Null);
    Ok(WorkerDeclaration {
        adapter,
        adapter_version: value
            .get("adapter_version")
            .and_then(|v| v.as_str())
            .unwrap_or("undeclared")
            .to_string(),
        runtime_product: runtime
            .and_then(|r| r.get("name").or_else(|| r.get("product")))
            .and_then(|v| v.as_str())
            .unwrap_or("undeclared")
            .to_string(),
        runtime_version: value
            .get("runtime_version")
            .or_else(|| runtime.and_then(|runtime| runtime.get("version")))
            .and_then(|v| v.as_str())
            .unwrap_or("undeclared")
            .to_string(),
        protocol_versions: strings("protocol_versions"),
        roles: strings("roles"),
        capabilities: strings("capabilities"),
        declared_surface: serde_json::json!({
            "resume": declared("resume"),
            "checkpoints": declared("checkpoints"),
            "runtime_artifact_store": declared("runtime_artifact_store"),
            "local_retries": declared("local_retries"),
        }),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use workloop_core::domain::ProcessIdentity;
    use workloop_core::ports::{ProcessEvidence, SignalOutcome};

    fn declaration(adapter: &str, runtime: &str) -> WorkerDeclaration {
        WorkerDeclaration {
            adapter: AdapterId::trusted(adapter),
            adapter_version: "1.0.0".into(),
            runtime_product: "fixture-runtime".into(),
            runtime_version: runtime.into(),
            protocol_versions: vec!["taisan.worker/1".into()],
            roles: vec!["coding".into()],
            capabilities: vec!["workspace_write".into()],
            declared_surface: serde_json::json!({
                "resume": "unsupported",
                "checkpoints": "workspace_commits",
                "runtime_artifact_store": null,
                "local_retries": "runtime_internal",
            }),
        }
    }

    #[test]
    fn a_same_adapter_runtime_change_is_a_configuration_repair() {
        let argv = vec!["worker".into()];
        let stored = declaration("adapter-a", "1.0.0").binding(&argv);
        match select_adapter(&declaration("adapter-a", "2.0.0"), &stored) {
            AdapterSelection::ConfigurationRepair { reason } => {
                assert!(reason.contains("runtime.version_mismatch"), "{reason}");
            }
            other => panic!("expected configuration repair, got {other:?}"),
        }
    }

    #[test]
    fn mismatch_reasons_name_the_differing_field() {
        let argv = vec!["worker".into()];
        let stored = declaration("adapter-a", "1.0.0").binding(&argv);
        let mut changed = declaration("adapter-a", "1.0.0");
        changed.adapter_version = "2.0.0".into();
        changed.capabilities = vec!["workspace_write".into(), "artifact_json".into()];
        match select_adapter(&changed, &stored) {
            AdapterSelection::ConfigurationRepair { reason } => {
                assert!(reason.contains("adapter.version_mismatch"), "{reason}");
                assert!(reason.contains("capability.digest_mismatch"), "{reason}");
                assert!(!reason.contains("runtime.version_mismatch"), "{reason}");
            }
            other => panic!("expected configuration repair, got {other:?}"),
        }
    }

    /// A drift confined to the unmodelled `WorkerCapabilities` fields
    /// still shifts the capability digest, so it selects a
    /// configuration repair rather than an invisible launch.
    #[test]
    fn a_declared_surface_drift_is_a_capability_repair() {
        let argv = vec!["worker".into()];
        let stored = declaration("adapter-a", "1.0.0").binding(&argv);
        let mut changed = declaration("adapter-a", "1.0.0");
        changed.declared_surface["resume"] = serde_json::json!("supported");
        match select_adapter(&changed, &stored) {
            AdapterSelection::ConfigurationRepair { reason } => {
                assert!(reason.contains("capability.digest_mismatch"), "{reason}");
            }
            other => panic!("expected configuration repair, got {other:?}"),
        }
    }

    #[test]
    fn a_matching_declaration_reuses_the_stored_binding() {
        let argv = vec!["worker".into()];
        let stored = declaration("adapter-a", "1.0.0").binding(&argv);
        match select_adapter(&declaration("adapter-a", "1.0.0"), &stored) {
            AdapterSelection::Ready(binding) => assert_eq!(binding, stored),
            other => panic!("expected ready, got {other:?}"),
        }
    }

    #[test]
    fn the_capability_digest_is_order_insensitive() {
        let mut reordered = declaration("adapter-a", "1.0.0");
        reordered.protocol_versions = vec!["taisan.worker/2".into(), "taisan.worker/1".into()];
        reordered.roles = vec!["review".into(), "coding".into()];
        reordered.capabilities = vec!["process_control".into(), "workspace_write".into()];
        let mut ordered = declaration("adapter-a", "1.0.0");
        ordered.protocol_versions = vec!["taisan.worker/1".into(), "taisan.worker/2".into()];
        ordered.roles = vec!["coding".into(), "review".into()];
        ordered.capabilities = vec!["workspace_write".into(), "process_control".into()];
        assert_eq!(ordered.capability_digest(), reordered.capability_digest());
    }

    struct Declaring {
        outcome: Result<ProbeOutcome, SpawnError>,
    }

    impl ProcessControlPort for Declaring {
        fn spawn(
            &self,
            _: &[String],
            _: &Path,
            _: &WorkerEnv,
        ) -> Result<ProcessIdentity, SpawnError> {
            unimplemented!()
        }
        fn snapshot_descendants(&self, _: i32) -> Result<Vec<ProcessIdentity>, String> {
            unimplemented!()
        }
        fn group_members(&self, _: i32) -> Result<Vec<ProcessIdentity>, String> {
            unimplemented!()
        }
        fn inspect(&self, _: &ProcessIdentity) -> ProcessEvidence {
            unimplemented!()
        }
        fn signal_verified(&self, _: &ProcessIdentity, _: i32) -> SignalOutcome {
            unimplemented!()
        }
        fn signal_group(&self, _: i32, _: i32) -> SignalOutcome {
            unimplemented!()
        }
        fn reap(&self, _: i32) {}
        fn run_probe(
            &self,
            _: &[String],
            _: &Path,
            _: &WorkerEnv,
            _: Duration,
        ) -> Result<ProbeOutcome, SpawnError> {
            self.outcome.clone()
        }
    }

    fn declaring(stdout: &str) -> Declaring {
        Declaring {
            outcome: Ok(ProbeOutcome::Exited {
                stdout: stdout.as_bytes().to_vec(),
                success: true,
            }),
        }
    }

    #[test]
    fn a_describe_answer_parses_into_the_declaration() {
        let port = declaring(
            r#"{"adapter":"adapter-a","adapter_version":"1.0.0",
               "runtime_version":"2.0.0","capabilities":["workspace_write"]}"#,
        );
        let observed = observe_worker_declaration(
            &port,
            &["worker".into()],
            Path::new("/tmp"),
            &WorkerEnv::default(),
        )
        .expect("declaration");
        assert_eq!(observed.adapter.as_str(), "adapter-a");
        assert_eq!(observed.runtime_version, "2.0.0");
    }

    #[test]
    fn a_failed_describe_is_not_selectable() {
        for port in [
            Declaring {
                outcome: Ok(ProbeOutcome::Exited {
                    stdout: b"{}".to_vec(),
                    success: false,
                }),
            },
            Declaring {
                outcome: Ok(ProbeOutcome::Bounded),
            },
            Declaring {
                outcome: Err(SpawnError::Refused {
                    reason: "missing binary".into(),
                }),
            },
            declaring("not json"),
            declaring(r#"{"adapter_version":"1.0.0"}"#),
        ] {
            assert!(
                observe_worker_declaration(
                    &port,
                    &["worker".into()],
                    Path::new("/tmp"),
                    &WorkerEnv::default()
                )
                .is_err()
            );
        }
    }

    /// A malformed adapter identity is refused at the describe
    /// boundary, never bound: the same `check_opaque` rules that gate
    /// `AdapterId::new` and deserialization apply to worker output —
    /// storing one would break every pending-request read back (#152).
    #[test]
    fn a_malformed_adapter_identity_is_refused() {
        for body in [
            format!(r#"{{"adapter":"{}"}}"#, "a".repeat(600)),
            r#"{"adapter":"has\ncontrol"}"#.to_string(),
        ] {
            let why = observe_worker_declaration(
                &declaring(&body),
                &["worker".into()],
                Path::new("/tmp"),
                &WorkerEnv::default(),
            )
            .expect_err("a malformed adapter identity must be refused");
            assert!(why.contains("invalid adapter identity"), "{why}");
        }
    }

    #[test]
    fn a_wedged_describe_is_bounded_and_not_selectable() {
        let argv = vec!["/bin/sh".into(), "-c".into(), "sleep 5".into()];
        let started = std::time::Instant::now();
        let result = observe_worker_declaration_within(
            &crate::supervisor::PosixProcessControl,
            &argv,
            Path::new("/tmp"),
            &WorkerEnv::default(),
            Duration::from_millis(50),
        );
        assert!(started.elapsed() < Duration::from_secs(4));
        assert!(result.is_err());
    }
}
