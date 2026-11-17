//! The U2.4 setup-preflight contract: `init` persists an install
//! config carrying the allowlist, credential references, and
//! identities; `check` runs its 8 stages in the documented order and
//! stops at the first failing stage with a stage-scoped diagnostic;
//! success requires persisted observed-identity evidence; the probe is
//! opt-in, read-only, and never a lasting grant
//! (`docs/installation-config.md` setup contract).

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::path::{Path, PathBuf};

use workloop_config::preflight::{
    CheckOptions, CheckReport, CheckStage, Diagnostic, InitHost, InitObservation, InitRepository,
    InitRole, InitSpec, ProbeMode, ProbeOperationKind, StageStatus, check, init,
};
use workloop_config::profile::parse_profile;
use workloop_core::ports::{
    CapabilityGrant, CapabilityObservation, CapabilityRead, CapabilityStatus, CapabilitySupport,
    IdentityRead, ObservedIdentity, ProbePort,
};

const ENDPOINT: &str = "https://***.github.com";
const PUBLICATION_REPO: &str = "0123456789";
const SOURCE_REPO: &str = "5566778899";
const PUBLISHER: &str = "taisan-publisher";
const REVIEWER: &str = "taisan-reviewer";
const TOKEN: &str = "seeded-secret-value";
const TOKEN_ENV: &str = "TAISAN_TEST_TOKEN";

fn temp_root(tag: &str) -> PathBuf {
    let dir =
        std::env::temp_dir().join(format!("workloop-preflight-{}-{}", tag, std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A complete two-role install spec: one publication and one review
/// role with `env:` / `file:` credential references, both observed.
fn spec(dir: &Path, observed_unix_seconds: i64) -> InitSpec {
    InitSpec {
        hosts: vec![InitHost {
            name: "h1".into(),
            adapter: "github".into(),
            endpoint: ENDPOINT.into(),
            contract_version: 1,
            repositories: vec![
                InitRepository {
                    id: PUBLICATION_REPO.into(),
                    display: Some("team/tool".into()),
                    use_: "publication".into(),
                },
                InitRepository {
                    id: SOURCE_REPO.into(),
                    display: None,
                    use_: "source".into(),
                },
            ],
        }],
        roles: vec![
            InitRole {
                role: "publication".into(),
                host: "h1".into(),
                actor: PUBLISHER.into(),
                credential_ref: format!("env:{TOKEN_ENV}"),
            },
            InitRole {
                role: "review".into(),
                host: "h1".into(),
                actor: REVIEWER.into(),
                credential_ref: format!("file:{}", dir.join("review.token").display()),
            },
        ],
        egress_allowlist: vec!["crates.io".into(), "static.crates.io".into()],
        observations: vec![
            InitObservation {
                role: "publication".into(),
                login: PUBLISHER.into(),
                host_instance: ENDPOINT.into(),
                repository: PUBLICATION_REPO.into(),
            },
            InitObservation {
                role: "review".into(),
                login: REVIEWER.into(),
                host_instance: ENDPOINT.into(),
                repository: PUBLICATION_REPO.into(),
            },
        ],
        observed_unix_seconds,
    }
}

/// The `file:` credential slot the spec binds review to, at 0600.
fn review_token(dir: &Path) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let path = dir.join("review.token");
    std::fs::write(&path, "seeded-review-secret").unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    path
}

struct Setup {
    dir: PathBuf,
    config: PathBuf,
    state: PathBuf,
}

/// A clean setup: the credential files exist and `init` published a
/// complete config at the given observation time.
fn clean_setup(tag: &str, observed_unix_seconds: i64) -> Setup {
    let dir = temp_root(tag);
    review_token(&dir);
    let config = dir.join("config.toml");
    let state = dir.join("state");
    init(&config, &spec(&dir, observed_unix_seconds)).expect("init publishes a clean setup");
    Setup { dir, config, state }
}

/// An environment carrying the publication credential slot.
fn env_present(name: &str) -> Option<OsString> {
    (name == TOKEN_ENV).then(|| OsString::from(TOKEN))
}

/// An environment with the publication credential slot revoked.
fn env_missing(_name: &str) -> Option<OsString> {
    None
}

/// A scripted probe: identity answers (or failures) per role, a full
/// support/grant answer per requested capability unless the scenario
/// overrides it, and a call log the tests inspect for the read-only
/// contract.
struct ScriptedProbe {
    logins: BTreeMap<String, ObservedIdentity>,
    login_errors: BTreeMap<String, String>,
    capability_errors: BTreeSet<String>,
    denied: BTreeSet<(String, String)>,
    unsupported: BTreeSet<(String, String)>,
    unanswered: BTreeSet<(String, String)>,
    calls: Vec<String>,
}

impl ScriptedProbe {
    /// Answers that match the clean setup's bound identities exactly.
    fn matching() -> Self {
        let mut logins = BTreeMap::new();
        for (role, actor) in [("publication", PUBLISHER), ("review", REVIEWER)] {
            logins.insert(
                role.to_string(),
                ObservedIdentity {
                    login: actor.into(),
                    host_instance: ENDPOINT.into(),
                    repository: PUBLICATION_REPO.into(),
                },
            );
        }
        Self {
            logins,
            login_errors: BTreeMap::new(),
            capability_errors: BTreeSet::new(),
            denied: BTreeSet::new(),
            unsupported: BTreeSet::new(),
            unanswered: BTreeSet::new(),
            calls: Vec::new(),
        }
    }
}

impl ProbePort for ScriptedProbe {
    fn read_actor_identity(&mut self, read: &IdentityRead) -> Result<ObservedIdentity, String> {
        self.calls.push(format!("identity:{}", read.role));
        if let Some(error) = self.login_errors.get(&read.role) {
            return Err(error.clone());
        }
        self.logins
            .get(&read.role)
            .cloned()
            .ok_or_else(|| format!("no scripted identity for {}", read.role))
    }

    fn read_capabilities(
        &mut self,
        read: &CapabilityRead,
    ) -> Result<CapabilityObservation, String> {
        self.calls.push(format!("capabilities:{}", read.role));
        if self.capability_errors.contains(&read.role) {
            return Err("probe timed out".into());
        }
        Ok(CapabilityObservation {
            statuses: read
                .capabilities
                .iter()
                .map(|capability| {
                    let key = (read.role.clone(), capability.clone());
                    let mut status = CapabilityStatus {
                        capability: capability.clone(),
                        support: if self.unsupported.contains(&key) {
                            CapabilitySupport::Unsupported
                        } else {
                            CapabilitySupport::Supported
                        },
                        grant: if self.denied.contains(&key) {
                            CapabilityGrant::Denied
                        } else {
                            CapabilityGrant::Allowed
                        },
                    };
                    if self.unanswered.contains(&key) {
                        // Simulate the probe omitting this entry: the
                        // caller must treat it as `unknown`, never pass.
                        status.capability = format!("omitted-{capability}");
                    }
                    status
                })
                .collect(),
        })
    }
}

fn failed(report: &CheckReport, stage: CheckStage) -> &Vec<Diagnostic> {
    let entry = report
        .stages
        .iter()
        .find(|entry| entry.stage == stage)
        .unwrap_or_else(|| panic!("stage {} missing from the report", stage.id()));
    assert_eq!(entry.status, StageStatus::Fail, "{}", stage.id());
    assert!(
        !entry.diagnostics.is_empty(),
        "{} carries a diagnostic",
        stage.id()
    );
    &entry.diagnostics
}

fn assert_stage(report: &CheckReport, stage: CheckStage, status: StageStatus) {
    let entry = report
        .stages
        .iter()
        .find(|entry| entry.stage == stage)
        .unwrap_or_else(|| panic!("stage {} missing from the report", stage.id()));
    assert_eq!(entry.status, status, "{}", stage.id());
}

// ---------------------------------------------------------------------------
// Acceptance criteria
// ---------------------------------------------------------------------------

/// AC1: `init` persists the install config carrying allowlist,
/// credential references, and identities — a named test runs `init`
/// and asserts the written config contains all three.
#[test]
fn init_persists_allowlist_credential_references_and_identities() {
    let dir = temp_root("ac1");
    review_token(&dir);
    let config = dir.join("config.toml");
    let outcome = init(&config, &spec(&dir, 1_763_000_000)).expect("init succeeds");

    let text = std::fs::read_to_string(&config).expect("init wrote a config");
    let profile = parse_profile(&text).expect("the written config parses under the loader");

    // Allowlist.
    assert_eq!(
        profile.verification.egress.allowlist,
        vec!["crates.io", "static.crates.io"],
        "the written config carries the egress allowlist"
    );
    assert!(
        text.contains("crates.io"),
        "allowlist present in the file text"
    );

    // Credential references (shape only — refs, never values).
    assert_eq!(
        profile.roles["publication"].credential_ref.as_deref(),
        Some(format!("env:{TOKEN_ENV}").as_str())
    );
    let review_ref = profile.roles["review"].credential_ref.as_deref().unwrap();
    assert!(
        review_ref.starts_with("file:"),
        "review credential ref: {review_ref}"
    );
    assert!(
        text.contains(TOKEN_ENV),
        "credential ref present in the file text"
    );
    assert!(
        !text.contains("seeded-review-secret"),
        "init never embeds a credential value"
    );

    // Identities: bound role actors plus the observed-identity evidence.
    assert_eq!(
        profile.roles["publication"].actor.as_deref(),
        Some(PUBLISHER)
    );
    assert_eq!(profile.roles["review"].actor.as_deref(), Some(REVIEWER));
    let publication_evidence = &profile.identity_evidence["publication"];
    assert_eq!(publication_evidence.login, PUBLISHER);
    assert_eq!(publication_evidence.host_instance, ENDPOINT);
    assert_eq!(publication_evidence.repository, PUBLICATION_REPO);
    assert_eq!(publication_evidence.observed_unix_seconds, 1_763_000_000);
    let review_evidence = &profile.identity_evidence["review"];
    assert_eq!(review_evidence.login, REVIEWER);

    // The outcome mirrors the file that actually landed.
    assert_eq!(
        outcome.roles,
        vec!["publication".to_string(), "review".to_string()]
    );
    assert_eq!(outcome.identity_evidence.len(), 2);
    assert_eq!(
        outcome.egress_allowlist,
        vec!["crates.io", "static.crates.io"]
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// AC2: `check` runs its steps in the documented order and stops at
/// the first failing step — a named test supplies an unparseable
/// config and asserts no later step ran.
#[test]
fn check_stops_at_the_first_failing_stage_and_runs_no_later_step() {
    let dir = temp_root("ac2");
    let config = dir.join("config.toml");
    // Unparseable TOML: a header with no closing bracket.
    std::fs::write(&config, "schema_version = 1\n[hosts.h1\nbroken =\n").unwrap();

    let mut probe = ScriptedProbe::matching();
    let report = {
        let mut options = CheckOptions::new(dir.join("state"));
        // The probe is opted in — and still never reached.
        options.probe = ProbeMode::Enabled(&mut probe);
        check(&config, &mut options)
    };

    assert!(!report.success, "an unparseable config can never succeed");
    // The report lists all 8 stages in the documented order …
    let order: Vec<CheckStage> = report.stages.iter().map(|entry| entry.stage).collect();
    assert_eq!(
        order,
        workloop_config::preflight::STAGE_ORDER.to_vec(),
        "stages are reported in the documented order"
    );
    // … but only the first one ran.
    assert_stage(&report, CheckStage::ConfigParse, StageStatus::Fail);
    let diagnostics = failed(&report, CheckStage::ConfigParse);
    assert_eq!(diagnostics[0].code, "config.invalid_toml");
    assert_eq!(diagnostics[0].key, config.display().to_string());
    for stage in workloop_config::preflight::STAGE_ORDER.iter().skip(1) {
        assert_stage(&report, *stage, StageStatus::NotRun);
    }
    // The opted-in probe was never touched: no operation, no calls.
    assert!(report.probe_operations.is_empty());
    assert_eq!(report.remote_writes, 0);
    assert!(
        probe.calls.is_empty(),
        "no later step ran, including the probe"
    );

    // An absent config stops at stage 1 the same way.
    let absent = dir.join("absent.toml");
    let report = {
        let mut options = CheckOptions::new(dir.join("state"));
        check(&absent, &mut options)
    };
    assert!(!report.success);
    let diagnostics = failed(&report, CheckStage::ConfigParse);
    assert_eq!(diagnostics[0].code, "config.absent");
    for stage in workloop_config::preflight::STAGE_ORDER.iter().skip(1) {
        assert_stage(&report, *stage, StageStatus::NotRun);
    }

    // Every stage-1 refusal class is distinct and still gated: unknown
    // key, unsupported schema version, empty file, unreadable path.
    for (name, content, expected) in [
        (
            "unknown-key",
            "schema_version = 1\n[hosts.h1]\nadapter = \"github\"\nendpoint = \"https://***.github.com\"\ncontract_version = 1\nunexpected = 1\n",
            "config.invalid_key",
        ),
        ("schema", "schema_version = 2\n", "config.schema_version"),
        ("empty", "", "config.invalid"),
    ] {
        let path = dir.join(format!("stage1-{name}.toml"));
        std::fs::write(&path, content).unwrap();
        let report = {
            let mut options = CheckOptions::new(dir.join("state"));
            check(&path, &mut options)
        };
        assert!(!report.success, "{name}");
        let diagnostics = failed(&report, CheckStage::ConfigParse);
        assert_eq!(diagnostics[0].code, expected, "{name}");
        for stage in workloop_config::preflight::STAGE_ORDER.iter().skip(1) {
            assert_stage(&report, *stage, StageStatus::NotRun);
        }
    }
    // A path that cannot be read at all — a directory standing where
    // the config should be — is `config.unreadable`.
    let path = dir.join("unreadable.toml");
    std::fs::create_dir(&path).unwrap();
    let report = {
        let mut options = CheckOptions::new(dir.join("state"));
        check(&path, &mut options)
    };
    assert!(!report.success);
    let diagnostics = failed(&report, CheckStage::ConfigParse);
    assert_eq!(diagnostics[0].code, "config.unreadable");

    let _ = std::fs::remove_dir_all(&dir);
}

/// AC3: `check` reports success only after an observed identity read —
/// a named test without identity evidence asserts no success claim.
#[test]
fn check_without_observed_identity_evidence_never_claims_success() {
    let dir = temp_root("ac3");
    // A structurally valid profile: hosts, roles, credentials — but no
    // `identity_evidence` at all (the observed identity never happened).
    let config = dir.join("config.toml");
    std::fs::write(
        &config,
        format!(
            r#"schema_version = 1

[hosts.h1]
adapter = "github"
endpoint = "{ENDPOINT}"
contract_version = 1

[[hosts.h1.repositories]]
id = "{PUBLICATION_REPO}"
use = "publication"

[roles.publication]
host = "h1"
actor = "{PUBLISHER}"
credential_ref = "env:{TOKEN_ENV}"
"#
        ),
    )
    .unwrap();

    let report = {
        let mut options = CheckOptions::new(dir.join("state"));
        options.env_lookup = &env_present;
        check(&config, &mut options)
    };

    assert!(
        !report.success,
        "a config that never carried an observed identity read must not be reported successful"
    );
    assert_stage(&report, CheckStage::ConfigParse, StageStatus::Pass);
    assert_stage(&report, CheckStage::IdentityTuples, StageStatus::Fail);
    let diagnostics = failed(&report, CheckStage::IdentityTuples);
    assert_eq!(diagnostics[0].code, "identity.evidence_absent");
    assert_eq!(diagnostics[0].key, "identity_evidence.publication");
    assert!(
        diagnostics[0].message.contains("observed identity"),
        "the diagnostic names the missing observed identity: {}",
        diagnostics[0].message
    );
    // Nothing after the failing stage ran.
    for stage in [
        CheckStage::CredentialRefs,
        CheckStage::Paths,
        CheckStage::WorkerRuntime,
        CheckStage::IsolationPrimitives,
        CheckStage::Store,
        CheckStage::ProbeCapabilities,
    ] {
        assert_stage(&report, stage, StageStatus::NotRun);
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// AC4: a missing credential reference produces a step-scoped
/// diagnostic at its stage — a named test removes the credential slot
/// and asserts `check` fails there with an explicit diagnostic and
/// performs no remote write.
#[test]
fn a_missing_credential_slot_fails_the_credential_stage_with_a_scoped_diagnostic_and_no_remote_write()
 {
    let setup = clean_setup("ac4", 1_763_000_000);
    let before = std::fs::read(&setup.config).unwrap();

    // The credential slot is removed: the `env:` variable is gone.
    let mut probe = ScriptedProbe::matching();
    let report = {
        let mut options = CheckOptions::new(&setup.state);
        options.env_lookup = &env_missing;
        options.probe = ProbeMode::Enabled(&mut probe);
        check(&setup.config, &mut options)
    };

    assert!(!report.success);
    assert_stage(&report, CheckStage::ConfigParse, StageStatus::Pass);
    assert_stage(&report, CheckStage::IdentityTuples, StageStatus::Pass);
    let diagnostics = failed(&report, CheckStage::CredentialRefs);
    assert_eq!(diagnostics[0].code, "credential.missing");
    assert_eq!(diagnostics[0].key, "roles.publication.credential_ref");
    assert!(
        diagnostics[0].message.contains(TOKEN_ENV),
        "the diagnostic names the missing slot: {}",
        diagnostics[0].message
    );
    // Later stages never ran.
    for stage in [
        CheckStage::Paths,
        CheckStage::WorkerRuntime,
        CheckStage::IsolationPrimitives,
        CheckStage::Store,
        CheckStage::ProbeCapabilities,
    ] {
        assert_stage(&report, stage, StageStatus::NotRun);
    }
    // No remote write: the probe was never reached, nothing logged,
    // and the local config is byte-identical (the failing check wrote
    // nothing anywhere).
    assert!(report.probe_operations.is_empty());
    assert_eq!(report.remote_writes, 0);
    assert!(probe.calls.is_empty(), "the probe stage never ran");
    assert_eq!(
        std::fs::read(&setup.config).unwrap(),
        before,
        "check wrote nothing"
    );
    let _ = std::fs::remove_dir_all(&setup.dir);
}

/// AC5: `--probe` is opt-in and read-only — a named test asserts a
/// plain `check` run does not invoke it and that a probe's operation
/// log contains only reads, zero remote writes.
#[test]
fn plain_check_never_invokes_the_probe_and_probe_operations_are_reads_only() {
    let setup = clean_setup("ac5", 1_763_000_000);

    // Plain check: stage 8 is not run, the operation log is empty, and
    // no probe backend is even handed to the run.
    let untouched_probe = ScriptedProbe::matching();
    let plain = {
        let mut options = CheckOptions::new(&setup.state);
        options.env_lookup = &env_present;
        check(&setup.config, &mut options)
    };
    assert!(plain.success, "the clean setup passes a plain check");
    assert_stage(&plain, CheckStage::ProbeCapabilities, StageStatus::NotRun);
    assert!(
        plain.probe_operations.is_empty(),
        "a plain check invokes no probe"
    );
    assert_eq!(plain.remote_writes, 0);
    assert!(untouched_probe.calls.is_empty());

    // `check --probe`: every logged operation is a read, the port only
    // ever receives identity/capability reads, zero remote writes.
    let mut probe = ScriptedProbe::matching();
    let probed = {
        let mut options = CheckOptions::new(&setup.state);
        options.env_lookup = &env_present;
        options.now_unix_seconds = 2_000;
        options.probe = ProbeMode::Enabled(&mut probe);
        check(&setup.config, &mut options)
    };
    assert!(probed.success, "the probed clean setup passes");
    assert!(
        !probed.probe_operations.is_empty(),
        "the probe actually ran"
    );
    assert!(
        probed
            .probe_operations
            .iter()
            .all(|operation| matches!(operation.kind, ProbeOperationKind::Read)),
        "every probe operation is a read: {:?}",
        probed.probe_operations
    );
    assert_eq!(
        probed.remote_writes, 0,
        "a probe run performs zero remote writes"
    );
    for role in ["publication", "review"] {
        assert!(
            probe
                .calls
                .iter()
                .any(|call| call == &format!("identity:{role}")),
            "probe read the identity of {role}"
        );
        assert!(
            probe
                .calls
                .iter()
                .any(|call| call == &format!("capabilities:{role}")),
            "probe read the capabilities of {role}"
        );
    }
    assert!(
        probe
            .calls
            .iter()
            .all(|call| { call.starts_with("identity:") || call.starts_with("capabilities:") }),
        "the port received reads only: {:?}",
        probe.calls
    );
    let _ = std::fs::remove_dir_all(&setup.dir);
}

/// AC6: setup success is never cached as a lasting grant — a named
/// test revokes the credential after a clean setup and asserts a
/// subsequent `check` fails (at credential resolution).
#[test]
fn a_revoked_credential_after_clean_setup_fails_the_next_check() {
    let setup = clean_setup("ac6", 1_763_000_000);
    let config_before = std::fs::read(&setup.config).unwrap();

    // Clean setup: the check passes, and passing writes nothing —
    // no grant, no stamp, no cache. The report is per-run only.
    let first = {
        let mut options = CheckOptions::new(&setup.state);
        options.env_lookup = &env_present;
        check(&setup.config, &mut options)
    };
    assert!(first.success, "the clean setup passes its first check");
    assert_eq!(
        std::fs::read(&setup.config).unwrap(),
        config_before,
        "a passing plain check persists no grant anywhere"
    );
    assert_eq!(first.remote_writes, 0);

    // The credential is revoked after the successful setup.
    let second = {
        let mut options = CheckOptions::new(&setup.state);
        options.env_lookup = &env_missing;
        check(&setup.config, &mut options)
    };
    assert!(
        !second.success,
        "the earlier success must not be cached — the revoked credential fails the next check"
    );
    assert_stage(&second, CheckStage::ConfigParse, StageStatus::Pass);
    assert_stage(&second, CheckStage::IdentityTuples, StageStatus::Pass);
    let diagnostics = failed(&second, CheckStage::CredentialRefs);
    assert_eq!(diagnostics[0].code, "credential.missing");
    assert_eq!(diagnostics[0].key, "roles.publication.credential_ref");
    // Neither run ever carried the secret value.
    let rendered = format!("{first:?}\n{second:?}");
    assert!(!rendered.contains(TOKEN), "no secret value in any report");
    assert!(
        !rendered.contains("seeded-review-secret"),
        "no file secret in any report"
    );
    assert_eq!(
        std::fs::read(&setup.config).unwrap(),
        config_before,
        "the failed re-check also persists nothing"
    );
    let _ = std::fs::remove_dir_all(&setup.dir);
}

// ---------------------------------------------------------------------------
// Stage ordering, diagnostics, and probe semantics beyond the six criteria
// ---------------------------------------------------------------------------

/// `init` is non-destructive: an existing config is never rewritten.
#[test]
fn init_refuses_to_rewrite_an_existing_config() {
    let setup = clean_setup("init-existing", 1_763_000_000);
    let before = std::fs::read(&setup.config).unwrap();
    let error = init(&setup.config, &spec(&setup.dir, 2_000)).unwrap_err();
    assert!(
        error.contains("config.exists"),
        "names the refusal: {error}"
    );
    assert_eq!(
        std::fs::read(&setup.config).unwrap(),
        before,
        "file untouched"
    );
    let _ = std::fs::remove_dir_all(&setup.dir);
}

/// A partial init leaves no config: without observed identity for a
/// bound role, `init` publishes nothing and never fabricates identity.
#[test]
fn init_without_observed_identity_publishes_nothing() {
    let dir = temp_root("init-no-observe");
    review_token(&dir);
    let config = dir.join("config.toml");
    let mut without_observation = spec(&dir, 1_763_000_000);
    without_observation.observations.clear();
    let error = init(&config, &without_observation).unwrap_err();
    assert!(
        error.contains("never fabricates"),
        "names the rule: {error}"
    );
    assert!(!config.exists(), "no partial config was published");
    // An observed login that is not the bound actor is refused too.
    let mut mismatched = spec(&dir, 1_763_000_000);
    mismatched.observations[0].login = "someone-else".into();
    let error = init(&config, &mismatched).unwrap_err();
    assert!(error.contains("does not match the bound actor"), "{error}");
    assert!(!config.exists(), "still nothing published");

    // Every incoherent pointer is refused by name, publishing nothing.
    type InvalidSpecCase = (&'static str, Box<dyn FnOnce(&mut InitSpec)>, &'static str);
    let cases: Vec<InvalidSpecCase> = vec![
        (
            "endpoint mismatch",
            Box::new(|spec: &mut InitSpec| {
                spec.observations[0].host_instance = "https://other.example".into();
            }),
            "does not match its declared endpoint",
        ),
        (
            "undeclared repository",
            Box::new(|spec: &mut InitSpec| {
                spec.observations[0].repository = "9999999999".into();
            }),
            "is not declared under host",
        ),
        (
            "observation for an unbound role",
            Box::new(|spec: &mut InitSpec| {
                spec.observations.push(InitObservation {
                    role: "discussion".into(),
                    login: "taisan-discussant".into(),
                    host_instance: ENDPOINT.into(),
                    repository: PUBLICATION_REPO.into(),
                });
            }),
            "names a role that is not bound",
        ),
        (
            "duplicate observation",
            Box::new(|spec: &mut InitSpec| {
                spec.observations.push(InitObservation {
                    role: "publication".into(),
                    login: PUBLISHER.into(),
                    host_instance: ENDPOINT.into(),
                    repository: PUBLICATION_REPO.into(),
                });
            }),
            "duplicate observation",
        ),
        (
            "negative observation time",
            Box::new(|spec: &mut InitSpec| spec.observed_unix_seconds = -1),
            "negative observation time",
        ),
        (
            "unknown role name",
            Box::new(|spec: &mut InitSpec| spec.roles[0].role = "manager".into()),
            "unknown role",
        ),
        (
            "malformed credential reference",
            Box::new(|spec: &mut InitSpec| {
                spec.roles[0].credential_ref = "pasted-secret".into();
            }),
            "credential_ref",
        ),
        (
            "host without repositories",
            Box::new(|spec: &mut InitSpec| spec.hosts[0].repositories.clear()),
            "declares no repository",
        ),
    ];
    for (name, mutate, expected) in cases {
        let mut invalid = spec(&dir, 1_763_000_000);
        mutate(&mut invalid);
        let error = init(&config, &invalid).unwrap_err();
        assert!(
            error.contains(expected),
            "{name}: expected '{expected}' in: {error}"
        );
        assert!(!config.exists(), "{name}: nothing was published");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// A passing `check --probe` refreshes the persisted observed-identity
/// evidence (the "refreshed by --probe" path) — and only ever the
/// observation, never a permission.
#[test]
fn probe_refresh_persists_observed_identity_evidence() {
    let setup = clean_setup("probe-refresh", 1_763_000_000);
    let mut probe = ScriptedProbe::matching();
    let report = {
        let mut options = CheckOptions::new(&setup.state);
        options.env_lookup = &env_present;
        options.now_unix_seconds = 2_222_222_222;
        options.probe = ProbeMode::Enabled(&mut probe);
        check(&setup.config, &mut options)
    };
    assert!(report.success, "the probed run passes");
    let profile = parse_profile(&std::fs::read_to_string(&setup.config).unwrap()).unwrap();
    assert_eq!(
        profile.identity_evidence["publication"].observed_unix_seconds, 2_222_222_222,
        "the fresh observation time is persisted"
    );
    assert_eq!(
        profile.identity_evidence["review"].observed_unix_seconds,
        2_222_222_222
    );
    // The refreshed values are still the observed identities.
    assert_eq!(profile.identity_evidence["publication"].login, PUBLISHER);

    // Re-probing with the observation time already recorded is a no-op:
    // the config is not rewritten when nothing changed.
    let before = std::fs::read(&setup.config).unwrap();
    let mut probe = ScriptedProbe::matching();
    let report = {
        let mut options = CheckOptions::new(&setup.state);
        options.env_lookup = &env_present;
        options.now_unix_seconds = 2_222_222_222;
        options.probe = ProbeMode::Enabled(&mut probe);
        check(&setup.config, &mut options)
    };
    assert!(report.success);
    assert_eq!(
        std::fs::read(&setup.config).unwrap(),
        before,
        "an unchanged observation is not rewritten"
    );
    let _ = std::fs::remove_dir_all(&setup.dir);
}

/// An observed identity that differs from the bound actor fails the
/// probe stage with `credential.actor_mismatch` — the binding is never
/// retargeted, and nothing is persisted.
#[test]
fn probe_identity_mismatch_fails_the_probe_stage_without_rebinding() {
    let setup = clean_setup("probe-mismatch", 1_763_000_000);
    let before = std::fs::read(&setup.config).unwrap();
    let mut probe = ScriptedProbe::matching();
    probe.logins.get_mut("publication").unwrap().login = "intruder".into();
    let report = {
        let mut options = CheckOptions::new(&setup.state);
        options.env_lookup = &env_present;
        options.now_unix_seconds = 2_000;
        options.probe = ProbeMode::Enabled(&mut probe);
        check(&setup.config, &mut options)
    };
    assert!(!report.success);
    let diagnostics = failed(&report, CheckStage::ProbeCapabilities);
    assert_eq!(diagnostics[0].code, "credential.actor_mismatch");
    assert_eq!(diagnostics[0].key, "identity_evidence.publication.login");
    assert_eq!(
        std::fs::read(&setup.config).unwrap(),
        before,
        "a failing probe persists no refreshed evidence"
    );

    // A probe answering outside the pinned context fails closed too —
    // stage-scoped, config untouched, no rebinding.
    let mut probe = ScriptedProbe::matching();
    probe.logins.get_mut("review").unwrap().host_instance = "https://other.example".into();
    let report = {
        let mut options = CheckOptions::new(&setup.state);
        options.env_lookup = &env_present;
        options.now_unix_seconds = 2_000;
        options.probe = ProbeMode::Enabled(&mut probe);
        check(&setup.config, &mut options)
    };
    assert!(!report.success);
    let diagnostics = failed(&report, CheckStage::ProbeCapabilities);
    assert_eq!(diagnostics[0].code, "identity.context_mismatch");
    assert_eq!(diagnostics[0].key, "identity_evidence.review");
    assert_eq!(std::fs::read(&setup.config).unwrap(), before);
    let _ = std::fs::remove_dir_all(&setup.dir);
}

/// Capability outcomes are evaluated separately: `denied` refuses with
/// `capability.denied`, `unsupported` with `capability.unsupported`,
/// an unreachable probe with `capability.unknown` — never a silent pass.
#[test]
fn probe_capability_outcomes_are_typed_and_never_silent_passes() {
    for (name, prepare, expected) in [
        (
            "denied",
            {
                fn deny(probe: &mut ScriptedProbe) {
                    probe
                        .denied
                        .insert(("publication".into(), "push_ref".into()));
                }
                deny as fn(&mut ScriptedProbe)
            },
            "capability.denied",
        ),
        (
            "unsupported",
            {
                fn unsupported(probe: &mut ScriptedProbe) {
                    probe
                        .unsupported
                        .insert(("publication".into(), "push_ref".into()));
                }
                unsupported as fn(&mut ScriptedProbe)
            },
            "capability.unsupported",
        ),
        (
            "unanswered",
            {
                fn unanswered(probe: &mut ScriptedProbe) {
                    probe
                        .unanswered
                        .insert(("publication".into(), "push_ref".into()));
                }
                unanswered as fn(&mut ScriptedProbe)
            },
            "capability.unknown",
        ),
        (
            "unreachable",
            {
                fn unreachable(probe: &mut ScriptedProbe) {
                    probe.capability_errors.insert("publication".into());
                }
                unreachable as fn(&mut ScriptedProbe)
            },
            "capability.unknown",
        ),
    ] {
        let setup = clean_setup(&format!("probe-cap-{name}"), 1_763_000_000);
        let mut probe = ScriptedProbe::matching();
        prepare(&mut probe);
        let report = {
            let mut options = CheckOptions::new(&setup.state);
            options.env_lookup = &env_present;
            options.probe = ProbeMode::Enabled(&mut probe);
            check(&setup.config, &mut options)
        };
        assert!(!report.success, "{name} must fail the probe stage");
        let diagnostics = failed(&report, CheckStage::ProbeCapabilities);
        assert_eq!(diagnostics[0].code, expected, "{name}");
        assert_eq!(
            diagnostics[0].key, "capabilities.required.publication",
            "{name}"
        );
        let _ = std::fs::remove_dir_all(&setup.dir);
    }
}

/// `--probe` with no wired backend fails closed at the probe stage:
/// `capability.unknown`, an empty operation log, zero writes.
#[test]
fn probe_without_a_backend_fails_closed_without_any_remote_call() {
    let setup = clean_setup("probe-unavailable", 1_763_000_000);
    let report = {
        let mut options = CheckOptions::new(&setup.state);
        options.env_lookup = &env_present;
        options.probe = ProbeMode::Unavailable;
        check(&setup.config, &mut options)
    };
    assert!(!report.success);
    let diagnostics = failed(&report, CheckStage::ProbeCapabilities);
    assert_eq!(diagnostics[0].code, "capability.unknown");
    assert!(
        diagnostics[0].message.contains("no adapter probe backend"),
        "{}",
        diagnostics[0].message
    );
    assert!(report.probe_operations.is_empty());
    assert_eq!(report.remote_writes, 0);
    let _ = std::fs::remove_dir_all(&setup.dir);
}

/// Stage (4): a state root the installation cannot write fails the
/// paths stage with the path named; later stages do not run.
#[test]
fn an_unwritable_state_root_fails_the_paths_stage() {
    use std::os::unix::fs::PermissionsExt;
    let setup = clean_setup("paths-unwritable", 1_763_000_000);
    std::fs::create_dir_all(&setup.state).unwrap();
    std::fs::set_permissions(&setup.state, std::fs::Permissions::from_mode(0o555)).unwrap();

    let report = {
        let mut options = CheckOptions::new(&setup.state);
        options.env_lookup = &env_present;
        check(&setup.config, &mut options)
    };
    // Restore before cleanup.
    std::fs::set_permissions(&setup.state, std::fs::Permissions::from_mode(0o755)).unwrap();

    assert!(!report.success);
    assert_stage(&report, CheckStage::Paths, StageStatus::Fail);
    let diagnostics = failed(&report, CheckStage::Paths);
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "paths.unwritable"),
        "names the unwritable root: {diagnostics:?}"
    );
    assert_stage(&report, CheckStage::WorkerRuntime, StageStatus::NotRun);
    let _ = std::fs::remove_dir_all(&setup.dir);
}

/// Stage (4): a state root nested inside the workspace root (or a
/// workspace outside the documented layout) is a separation failure.
#[test]
fn a_root_separation_violation_fails_the_paths_stage() {
    let setup = clean_setup("paths-nested", 1_763_000_000);
    // Workspace = the parent of the state root: the state sits inside
    // a managed workspace, which setup refuses.
    let report = {
        let mut options = CheckOptions::new(&setup.state);
        options.workspace_root = Some(setup.dir.clone());
        options.env_lookup = &env_present;
        check(&setup.config, &mut options)
    };
    assert!(!report.success);
    assert_stage(&report, CheckStage::Paths, StageStatus::Fail);
    let diagnostics = failed(&report, CheckStage::Paths);
    assert_eq!(diagnostics[0].code, "paths.nested", "{diagnostics:?}");
    let _ = std::fs::remove_dir_all(&setup.dir);
}

/// Stage (5): a `worker.argv` that does not resolve is `runtime.missing`
/// with the argv named, and the isolation stage never runs.
#[test]
fn a_missing_worker_runtime_fails_the_worker_stage() {
    let setup = clean_setup("runtime-missing", 1_763_000_000);
    let mut text = std::fs::read_to_string(&setup.config).unwrap();
    text.push_str("\n[worker]\nargv = [\"/definitely/missing/taisan-worker\"]\n");
    std::fs::write(&setup.config, &text).unwrap();

    let report = {
        let mut options = CheckOptions::new(&setup.state);
        options.env_lookup = &env_present;
        check(&setup.config, &mut options)
    };
    assert!(!report.success);
    assert_stage(&report, CheckStage::WorkerRuntime, StageStatus::Fail);
    let diagnostics = failed(&report, CheckStage::WorkerRuntime);
    assert_eq!(diagnostics[0].code, "runtime.missing");
    assert_eq!(diagnostics[0].key, "worker.argv");
    assert_stage(
        &report,
        CheckStage::IsolationPrimitives,
        StageStatus::NotRun,
    );
    let _ = std::fs::remove_dir_all(&setup.dir);
}

/// Stage (6): a declared reference-profile launch on a host missing
/// `bwrap` fails with the isolation primitive named (the documented
/// `runtime.missing` case), while the runtime itself resolved.
#[test]
fn a_missing_isolation_primitive_fails_the_isolation_stage() {
    let setup = clean_setup("isolation-missing", 1_763_000_000);
    let mut text = std::fs::read_to_string(&setup.config).unwrap();
    text.push_str(
        "\n[worker]\nargv = [\"/usr/bin/true\"]\nisolation_profile = \"taisan-reference-v0.1\"\n",
    );
    std::fs::write(&setup.config, &text).unwrap();

    let report = {
        let mut options = CheckOptions::new(&setup.state);
        options.env_lookup = &env_present;
        options.which = &|_| None; // no `bwrap` on this lookup
        check(&setup.config, &mut options)
    };
    assert!(!report.success);
    assert_stage(&report, CheckStage::WorkerRuntime, StageStatus::Pass);
    assert_stage(&report, CheckStage::IsolationPrimitives, StageStatus::Fail);
    let diagnostics = failed(&report, CheckStage::IsolationPrimitives);
    assert_eq!(diagnostics[0].code, "runtime.missing");
    assert!(
        diagnostics[0].message.contains("bwrap"),
        "the primitive is named: {}",
        diagnostics[0].message
    );
    assert_stage(&report, CheckStage::Store, StageStatus::NotRun);

    // With the primitive present on the same lookup, the stage passes
    // and the plain check succeeds end to end.
    let report = {
        let mut options = CheckOptions::new(&setup.state);
        options.env_lookup = &env_present;
        options.which = &|name| (name == "bwrap").then(|| PathBuf::from("/usr/bin/bwrap"));
        check(&setup.config, &mut options)
    };
    assert!(report.success, "{report:?}");
    let _ = std::fs::remove_dir_all(&setup.dir);
}

/// Stage (5): a pinned runtime version is checked against the observed
/// binary — a satisfied pin passes, a violated pin is
/// `runtime.version_mismatch`, and the pin's own form is bounded.
#[test]
fn runtime_version_pins_are_checked_against_the_observed_binary() {
    let setup = clean_setup("runtime-pin", 1_763_000_000);
    let base = std::fs::read_to_string(&setup.config).unwrap();

    // `/bin/echo 9.9.9 --version` prints a parsable observed version.
    let mut satisfied = base.clone();
    satisfied.push_str(
        "\n[worker]\nargv = [\"/bin/echo\", \"9.9.9\"]\nruntime_version = \">=9.0.0\"\nisolation_profile = \"fixture-worker-v0.1\"\n",
    );
    std::fs::write(&setup.config, &satisfied).unwrap();
    let report = {
        let mut options = CheckOptions::new(&setup.state);
        options.env_lookup = &env_present;
        check(&setup.config, &mut options)
    };
    assert!(
        report.success,
        "an observed version satisfying the pin passes"
    );

    let mut violated = base.clone();
    violated.push_str(
        "\n[worker]\nargv = [\"/bin/echo\", \"9.9.9\"]\nruntime_version = \">=99.0.0\"\nisolation_profile = \"fixture-worker-v0.1\"\n",
    );
    std::fs::write(&setup.config, &violated).unwrap();
    let report = {
        let mut options = CheckOptions::new(&setup.state);
        options.env_lookup = &env_present;
        check(&setup.config, &mut options)
    };
    assert!(!report.success);
    let diagnostics = failed(&report, CheckStage::WorkerRuntime);
    assert_eq!(diagnostics[0].code, "runtime.version_mismatch");
    assert_eq!(diagnostics[0].key, "worker.runtime_version");

    let mut unsupported = base;
    unsupported.push_str(
        "\n[worker]\nargv = [\"/bin/echo\", \"9.9.9\"]\nruntime_version = \"^9.0.0\"\nisolation_profile = \"fixture-worker-v0.1\"\n",
    );
    std::fs::write(&setup.config, &unsupported).unwrap();
    let report = {
        let mut options = CheckOptions::new(&setup.state);
        options.env_lookup = &env_present;
        check(&setup.config, &mut options)
    };
    assert!(!report.success);
    let diagnostics = failed(&report, CheckStage::WorkerRuntime);
    assert_eq!(diagnostics[0].code, "runtime.version_pin");
    let _ = std::fs::remove_dir_all(&setup.dir);
}

/// Stage (7): a corrupt store file fails closed with the path named;
/// an absent store passes (the first open bootstraps it).
#[test]
fn a_corrupt_store_file_fails_the_store_stage() {
    let setup = clean_setup("store-corrupt", 1_763_000_000);
    std::fs::create_dir_all(&setup.state).unwrap();
    std::fs::write(setup.state.join("store.db"), "not a database").unwrap();

    let report = {
        let mut options = CheckOptions::new(&setup.state);
        options.env_lookup = &env_present;
        check(&setup.config, &mut options)
    };
    assert!(!report.success);
    assert_stage(&report, CheckStage::Store, StageStatus::Fail);
    let diagnostics = failed(&report, CheckStage::Store);
    assert_eq!(diagnostics[0].code, "store.corrupt");
    assert!(diagnostics[0].key.ends_with("store.db"));
    assert_stage(&report, CheckStage::ProbeCapabilities, StageStatus::NotRun);
    let _ = std::fs::remove_dir_all(&setup.dir);
}

/// Stage (3): a `file:` slot with group/world-readable permissions is
/// `credential.unresolvable` — checked from metadata, so the file's
/// content never enters the report.
#[test]
fn a_world_readable_credential_file_fails_the_credential_stage_without_reading_it() {
    use std::os::unix::fs::PermissionsExt;
    let setup = clean_setup("file-weak-mode", 1_763_000_000);
    let token = setup.dir.join("review.token");
    std::fs::set_permissions(&token, std::fs::Permissions::from_mode(0o644)).unwrap();

    let report = {
        let mut options = CheckOptions::new(&setup.state);
        options.env_lookup = &env_present;
        check(&setup.config, &mut options)
    };
    // Restore for cleanup.
    std::fs::set_permissions(&token, std::fs::Permissions::from_mode(0o600)).unwrap();

    assert!(!report.success);
    let diagnostics = failed(&report, CheckStage::CredentialRefs);
    let review = diagnostics
        .iter()
        .find(|diagnostic| diagnostic.key == "roles.review.credential_ref")
        .expect("the review slot's failure is reported");
    assert_eq!(review.code, "credential.unresolvable");
    assert!(
        review.message.contains("owner-only"),
        "names the permission rule: {}",
        review.message
    );
    let rendered = format!("{report:?}");
    assert!(
        !rendered.contains("seeded-review-secret"),
        "the file's content never enters the report"
    );
    let _ = std::fs::remove_dir_all(&setup.dir);
}

/// Stage (3): a `command:` provider resolves by exit status — exit
/// zero passes, a non-zero exit is `credential.unresolvable`.
#[test]
fn command_provider_slots_resolve_by_exit_status() {
    let dir = temp_root("command-provider");
    review_token(&dir);
    let config = dir.join("config.toml");
    let mut spec = spec(&dir, 1_763_000_000);
    spec.roles.push(InitRole {
        role: "discussion".into(),
        host: "h1".into(),
        actor: "taisan-discussant".into(),
        credential_ref: "command:/usr/bin/true".into(),
    });
    spec.observations.push(InitObservation {
        role: "discussion".into(),
        login: "taisan-discussant".into(),
        host_instance: ENDPOINT.into(),
        repository: PUBLICATION_REPO.into(),
    });
    init(&config, &spec).expect("init with a command: slot");
    let state = dir.join("state");

    let report = {
        let mut options = CheckOptions::new(&state);
        options.env_lookup = &env_present;
        check(&config, &mut options)
    };
    assert!(report.success, "exit zero resolves the slot: {report:?}");

    let text = std::fs::read_to_string(&config).unwrap();
    std::fs::write(
        &config,
        text.replace("command:/usr/bin/true", "command:/usr/bin/false"),
    )
    .unwrap();
    let report = {
        let mut options = CheckOptions::new(&state);
        options.env_lookup = &env_present;
        check(&config, &mut options)
    };
    assert!(!report.success);
    let diagnostics = failed(&report, CheckStage::CredentialRefs);
    assert_eq!(diagnostics[0].code, "credential.unresolvable");
    assert!(
        diagnostics[0].message.contains("non-zero"),
        "{}",
        diagnostics[0].message
    );

    // A provider that cannot even be started is unresolvable too —
    // argv contents are never echoed into the refusal.
    let text = std::fs::read_to_string(&config).unwrap();
    std::fs::write(
        &config,
        text.replace(
            "command:/usr/bin/false",
            "command:/definitely/missing-taisan-prog",
        ),
    )
    .unwrap();
    let report = {
        let mut options = CheckOptions::new(&state);
        options.env_lookup = &env_present;
        check(&config, &mut options)
    };
    assert!(!report.success);
    let diagnostics = failed(&report, CheckStage::CredentialRefs);
    assert_eq!(diagnostics[0].code, "credential.unresolvable");
    assert!(
        diagnostics[0].message.contains("could not be started"),
        "{}",
        diagnostics[0].message
    );
    assert!(
        !format!("{diagnostics:?}").contains("missing-taisan-prog"),
        "the refusal never echoes the provider argv: {diagnostics:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Stage (2): a profile with no bound roles fails identity
/// completeness; publication targets and the bound publication role
/// must be coherent in both directions.
#[test]
fn identity_completeness_and_allowlist_coherence_are_enforced() {
    // No roles at all.
    let dir = temp_root("identity-empty");
    let config = dir.join("config.toml");
    std::fs::write(
        &config,
        format!(
            r#"schema_version = 1

[hosts.h1]
adapter = "github"
endpoint = "{ENDPOINT}"
contract_version = 1

[[hosts.h1.repositories]]
id = "{PUBLICATION_REPO}"
use = "publication"
"#
        ),
    )
    .unwrap();
    let report = {
        let mut options = CheckOptions::new(dir.join("state"));
        check(&config, &mut options)
    };
    assert!(!report.success);
    let diagnostics = failed(&report, CheckStage::IdentityTuples);
    assert_eq!(diagnostics[0].code, "identity.roles_missing");
    let _ = std::fs::remove_dir_all(&dir);

    // A publication role whose host only declares source repositories.
    let dir = temp_root("identity-source-only");
    review_token(&dir);
    let config = dir.join("config.toml");
    std::fs::write(
        &config,
        format!(
            r#"schema_version = 1

[hosts.h1]
adapter = "github"
endpoint = "{ENDPOINT}"
contract_version = 1

[[hosts.h1.repositories]]
id = "{SOURCE_REPO}"
use = "source"

[roles.publication]
host = "h1"
actor = "{PUBLISHER}"
credential_ref = "env:{TOKEN_ENV}"

[identity_evidence.publication]
login = "{PUBLISHER}"
host_instance = "{ENDPOINT}"
repository = "{SOURCE_REPO}"
observed_unix_seconds = 1763000000
"#
        ),
    )
    .unwrap();
    let report = {
        let mut options = CheckOptions::new(dir.join("state"));
        options.env_lookup = &env_present;
        check(&config, &mut options)
    };
    assert!(!report.success);
    let diagnostics = failed(&report, CheckStage::IdentityTuples);
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "identity.allowlist_incoherent"),
        "a publication role with no publication target is incoherent: {diagnostics:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);

    // A host that declares no repository can never host a bound role.
    let dir = temp_root("identity-no-repo");
    let config = dir.join("config.toml");
    std::fs::write(
        &config,
        format!(
            r#"schema_version = 1

[hosts.h1]
adapter = "github"
endpoint = "{ENDPOINT}"
contract_version = 1

[roles.publication]
host = "h1"
actor = "{PUBLISHER}"
credential_ref = "env:{TOKEN_ENV}"

[identity_evidence.publication]
login = "{PUBLISHER}"
host_instance = "{ENDPOINT}"
repository = "{PUBLICATION_REPO}"
observed_unix_seconds = 1763000000
"#
        ),
    )
    .unwrap();
    let report = {
        let mut options = CheckOptions::new(dir.join("state"));
        options.env_lookup = &env_present;
        check(&config, &mut options)
    };
    assert!(!report.success);
    let diagnostics = failed(&report, CheckStage::IdentityTuples);
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "identity.host_no_repositories"),
        "a repository-less host is named: {diagnostics:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);

    // A publication target whose host binds no publication role is an
    // incoherent allowlist entry (review binds to the same host).
    let dir = temp_root("identity-target-without-role");
    let config = dir.join("config.toml");
    std::fs::write(
        &config,
        format!(
            r#"schema_version = 1

[hosts.h1]
adapter = "github"
endpoint = "{ENDPOINT}"
contract_version = 1

[[hosts.h1.repositories]]
id = "{PUBLICATION_REPO}"
use = "publication"

[roles.review]
host = "h1"
actor = "{REVIEWER}"
credential_ref = "env:{TOKEN_ENV}"

[identity_evidence.review]
login = "{REVIEWER}"
host_instance = "{ENDPOINT}"
repository = "{PUBLICATION_REPO}"
observed_unix_seconds = 1763000000
"#
        ),
    )
    .unwrap();
    let report = {
        let mut options = CheckOptions::new(dir.join("state"));
        options.env_lookup = &env_present;
        check(&config, &mut options)
    };
    assert!(!report.success);
    let diagnostics = failed(&report, CheckStage::IdentityTuples);
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "identity.allowlist_incoherent"),
        "a publication target without a bound publisher is named: {diagnostics:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);

    // Every identity-evidence field mismatch is its own stage-scoped
    // diagnostic: login, host instance, and repository.
    let dir = temp_root("identity-evidence-mismatch");
    let config = dir.join("config.toml");
    std::fs::write(
        &config,
        format!(
            r#"schema_version = 1

[hosts.h1]
adapter = "github"
endpoint = "{ENDPOINT}"
contract_version = 1

[[hosts.h1.repositories]]
id = "{PUBLICATION_REPO}"
use = "publication"

[roles.publication]
host = "h1"
actor = "{PUBLISHER}"
credential_ref = "env:{TOKEN_ENV}"

[identity_evidence.publication]
login = "stale-actor"
host_instance = "https://other.example"
repository = "9999999999"
observed_unix_seconds = 1763000000
"#
        ),
    )
    .unwrap();
    let report = {
        let mut options = CheckOptions::new(dir.join("state"));
        options.env_lookup = &env_present;
        check(&config, &mut options)
    };
    assert!(!report.success);
    let diagnostics = failed(&report, CheckStage::IdentityTuples);
    for field in ["login", "host_instance", "repository"] {
        assert!(
            diagnostics.iter().any(|diagnostic| {
                diagnostic.code == "identity.evidence_mismatch"
                    && diagnostic.key == format!("identity_evidence.publication.{field}")
            }),
            "mismatched {field} is named: {diagnostics:?}"
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// Stage (3): every `file:` metadata fault is distinct — owner
/// cannot read (`0200`), a non-regular target, an absent slot — and
/// none of them ever reads the file's content into the report.
#[test]
fn file_credential_slots_fail_closed_for_each_metadata_fault() {
    use std::os::unix::fs::PermissionsExt;
    let setup = clean_setup("file-metadata", 1_763_000_000);
    let token = setup.dir.join("review.token");

    // Owner cannot read (write-only permissions).
    std::fs::set_permissions(&token, std::fs::Permissions::from_mode(0o200)).unwrap();
    let report = {
        let mut options = CheckOptions::new(&setup.state);
        options.env_lookup = &env_present;
        check(&setup.config, &mut options)
    };
    std::fs::set_permissions(&token, std::fs::Permissions::from_mode(0o600)).unwrap();
    assert!(!report.success);
    let diagnostics = failed(&report, CheckStage::CredentialRefs);
    let review = diagnostics
        .iter()
        .find(|diagnostic| diagnostic.key == "roles.review.credential_ref")
        .expect("the review slot's failure is reported");
    assert_eq!(review.code, "credential.unresolvable");
    assert!(
        review.message.contains("owner cannot read"),
        "{}",
        review.message
    );

    // A directory standing where the slot's file should be.
    std::fs::remove_file(&token).unwrap();
    std::fs::create_dir(&token).unwrap();
    let report = {
        let mut options = CheckOptions::new(&setup.state);
        options.env_lookup = &env_present;
        check(&setup.config, &mut options)
    };
    std::fs::remove_dir(&token).unwrap();
    assert!(!report.success);
    let diagnostics = failed(&report, CheckStage::CredentialRefs);
    let review = diagnostics
        .iter()
        .find(|diagnostic| diagnostic.key == "roles.review.credential_ref")
        .unwrap();
    assert_eq!(review.code, "credential.unresolvable");
    assert!(
        review.message.contains("not a regular file"),
        "{}",
        review.message
    );

    // The slot removed entirely: `credential.missing`, not unresolvable.
    let report = {
        let mut options = CheckOptions::new(&setup.state);
        options.env_lookup = &env_present;
        check(&setup.config, &mut options)
    };
    assert!(!report.success);
    let diagnostics = failed(&report, CheckStage::CredentialRefs);
    let review = diagnostics
        .iter()
        .find(|diagnostic| diagnostic.key == "roles.review.credential_ref")
        .unwrap();
    assert_eq!(review.code, "credential.missing");

    // No report ever carried the file's content.
    assert!(
        !format!("{report:?}").contains("seeded-review-secret"),
        "content never read into the report"
    );
    let _ = std::fs::remove_dir_all(&setup.dir);
}

/// Stage (5): resolution failures are distinct — a name that does not
/// resolve on the lookup, a non-executable absolute path, and a pin
/// whose observed `--version` yields no version triple.
#[test]
fn runtime_resolution_failures_have_distinct_diagnostics() {
    use std::os::unix::fs::PermissionsExt;
    let setup = clean_setup("runtime-faults", 1_763_000_000);
    let base = std::fs::read_to_string(&setup.config).unwrap();

    // A non-absolute argv[0] with no lookup hit: runtime.missing.
    let mut unresolved = base.clone();
    unresolved.push_str(
        "\n[worker]\nargv = [\"taisan-not-installed\"]\nisolation_profile = \"fixture-worker-v0.1\"\n",
    );
    std::fs::write(&setup.config, &unresolved).unwrap();
    let report = {
        let mut options = CheckOptions::new(&setup.state);
        options.env_lookup = &env_present;
        options.which = &|_| None;
        check(&setup.config, &mut options)
    };
    assert!(!report.success);
    let diagnostics = failed(&report, CheckStage::WorkerRuntime);
    assert_eq!(diagnostics[0].code, "runtime.missing");
    assert_eq!(diagnostics[0].key, "worker.argv");

    // An absolute path that exists but is not executable.
    let not_executable = setup.dir.join("worker.sh");
    std::fs::write(&not_executable, "#!/bin/sh\nexit 0\n").unwrap();
    std::fs::set_permissions(&not_executable, std::fs::Permissions::from_mode(0o644)).unwrap();
    let mut non_exec = base.clone();
    non_exec.push_str(&format!(
        "\n[worker]\nargv = [\"{}\"]\nisolation_profile = \"fixture-worker-v0.1\"\n",
        not_executable.display()
    ));
    std::fs::write(&setup.config, &non_exec).unwrap();
    let report = {
        let mut options = CheckOptions::new(&setup.state);
        options.env_lookup = &env_present;
        check(&setup.config, &mut options)
    };
    assert!(!report.success);
    let diagnostics = failed(&report, CheckStage::WorkerRuntime);
    assert_eq!(diagnostics[0].code, "runtime.not_executable");

    // A pin whose observed output carries no X.Y.Z triple: unknown,
    // never a guess and never a mismatch.
    let mut unparsable = base;
    unparsable.push_str(
        "\n[worker]\nargv = [\"/bin/echo\", \"hello\"]\nruntime_version = \">=1.0.0\"\n\
         isolation_profile = \"fixture-worker-v0.1\"\n",
    );
    std::fs::write(&setup.config, &unparsable).unwrap();
    let report = {
        let mut options = CheckOptions::new(&setup.state);
        options.env_lookup = &env_present;
        check(&setup.config, &mut options)
    };
    assert!(!report.success);
    let diagnostics = failed(&report, CheckStage::WorkerRuntime);
    assert_eq!(diagnostics[0].code, "runtime.version_unknown");
    assert_eq!(diagnostics[0].key, "worker.runtime_version");
    let _ = std::fs::remove_dir_all(&setup.dir);
}

/// Stage (4): a relative root is refused (never the ambient working
/// directory) and a state root that exists as a plain file fails.
#[test]
fn relative_or_non_directory_roots_fail_the_paths_stage() {
    let setup = clean_setup("paths-relative", 1_763_000_000);

    let report = {
        let mut options = CheckOptions::new("relative-state-root");
        options.env_lookup = &env_present;
        check(&setup.config, &mut options)
    };
    assert!(!report.success);
    let diagnostics = failed(&report, CheckStage::Paths);
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "paths.relative"),
        "the relative root is named: {diagnostics:?}"
    );

    // The state root exists but is a file.
    std::fs::write(&setup.state, "not a directory").unwrap();
    let report = {
        let mut options = CheckOptions::new(&setup.state);
        options.env_lookup = &env_present;
        check(&setup.config, &mut options)
    };
    std::fs::remove_file(&setup.state).unwrap();
    assert!(!report.success);
    let diagnostics = failed(&report, CheckStage::Paths);
    assert!(
        diagnostics.iter().any(|diagnostic| {
            diagnostic.code == "paths.unwritable" && diagnostic.message.contains("not a directory")
        }),
        "the non-directory root is named: {diagnostics:?}"
    );
    let _ = std::fs::remove_dir_all(&setup.dir);
}

/// Stage (8): an unreachable identity read is `capability.unknown`
/// (never denied) with the role scoped — and the attempted read is
/// still logged as a read.
#[test]
fn an_unreachable_probe_identity_read_fails_with_capability_unknown() {
    let setup = clean_setup("probe-identity-unreachable", 1_763_000_000);
    let mut probe = ScriptedProbe::matching();
    probe
        .login_errors
        .insert("publication".into(), "adapter timed out".into());
    let report = {
        let mut options = CheckOptions::new(&setup.state);
        options.env_lookup = &env_present;
        options.probe = ProbeMode::Enabled(&mut probe);
        check(&setup.config, &mut options)
    };
    assert!(!report.success);
    let diagnostics = failed(&report, CheckStage::ProbeCapabilities);
    assert_eq!(diagnostics[0].code, "capability.unknown");
    assert_eq!(diagnostics[0].key, "identity_evidence.publication");
    // The attempted read is logged; nothing after it ran remotely.
    assert_eq!(report.probe_operations.len(), 1);
    assert_eq!(report.probe_operations[0].kind, ProbeOperationKind::Read);
    assert_eq!(report.probe_operations[0].call, "identity");
    assert_eq!(report.remote_writes, 0);
    let _ = std::fs::remove_dir_all(&setup.dir);
}

/// Stage (7): the store header is classified without opening the
/// database — fresh files pass, unversioned and newer formats fail
/// closed with the path named.
#[test]
fn store_header_classification_covers_fresh_unversioned_and_newer_formats() {
    let setup = clean_setup("store-headers", 1_763_000_000);
    std::fs::create_dir_all(&setup.state).unwrap();
    let store = setup.state.join("store.db");
    let header = |pages: u32, version: u32| -> Vec<u8> {
        let mut bytes = vec![0u8; 100];
        bytes[..16].copy_from_slice(b"SQLite format 3\0");
        bytes[28..32].copy_from_slice(&pages.to_be_bytes());
        bytes[60..64].copy_from_slice(&version.to_be_bytes());
        bytes
    };
    let cases: Vec<(&str, Vec<u8>, bool, Option<&str>)> = vec![
        ("zero length", Vec::new(), true, None),
        (
            "foreign bytes",
            vec![b'x'; 200],
            false,
            Some("store.corrupt"),
        ),
        ("fresh header", header(0, 0), true, None),
        (
            "unversioned with pages",
            header(1, 0),
            false,
            Some("store.unversioned"),
        ),
        (
            "newer format",
            header(1, 9),
            false,
            Some("store.unsupported"),
        ),
        ("recognized format", header(1, 8), true, None),
    ];
    for (name, bytes, should_pass, code) in cases {
        std::fs::write(&store, &bytes).unwrap();
        let report = {
            let mut options = CheckOptions::new(&setup.state);
            options.env_lookup = &env_present;
            check(&setup.config, &mut options)
        };
        if should_pass {
            assert!(report.success, "{name}: {report:?}");
        } else {
            assert!(!report.success, "{name}");
            let diagnostics = failed(&report, CheckStage::Store);
            assert_eq!(diagnostics[0].code, code.unwrap(), "{name}");
            assert!(diagnostics[0].key.ends_with("store.db"), "{name}");
        }
    }
    let _ = std::fs::remove_dir_all(&setup.dir);
}

/// Stage (7): a live WAL-mode store — initialized but not yet
/// checkpointed — is classified from its committed WAL prefix, never
/// misread as an unversioned archive. `check` still writes nothing:
/// the store connection stays open and no store file is created,
/// journaled, or migrated by the inspection.
#[test]
fn a_live_wal_store_is_classified_from_its_committed_frames() {
    let setup = clean_setup("store-wal", 1_763_000_000);
    std::fs::create_dir_all(&setup.state).unwrap();
    let store_path = setup.state.join("store.db");
    // Open and initialize the real store, then keep the connection
    // alive: the bootstrap commit sits in the WAL and the main-file
    // header still reads version 0 until the first checkpoint.
    let store =
        workloop_store::sqlite::SqliteStore::open(&store_path, 1).expect("the store initializes");
    let wal_path = setup.state.join("store.db-wal");
    assert!(
        wal_path.exists(),
        "an open WAL store keeps its write-ahead log"
    );

    let report = {
        let mut options = CheckOptions::new(&setup.state);
        options.env_lookup = &env_present;
        options.store_path = Some(store_path.clone());
        check(&setup.config, &mut options)
    };
    assert!(
        report.success,
        "a live WAL store must not read as unversioned: {report:?}"
    );

    // The inspection itself wrote nothing: the WAL is still there and
    // the main file was never checkpointed by `check`.
    assert!(wal_path.exists(), "check never checkpoints the store");
    drop(store);
    let _ = std::fs::remove_dir_all(&setup.dir);
}

/// Stage (5): a `--version` command that prints a satisfying version
/// but exits non-zero cannot satisfy the pin — the failed probe is
/// `runtime.version_unknown`, never a pass.
#[test]
fn a_failed_version_command_cannot_satisfy_the_pin() {
    use std::os::unix::fs::PermissionsExt;
    let setup = clean_setup("runtime-exit-fail", 1_763_000_000);
    let script = setup.dir.join("version-fail.sh");
    std::fs::write(&script, "#!/bin/sh\necho 9.9.9\nexit 1\n").unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let mut text = std::fs::read_to_string(&setup.config).unwrap();
    text.push_str(&format!(
        "\n[worker]\nargv = [\"{}\"]\nruntime_version = \">=9.0.0\"\n\
         isolation_profile = \"fixture-worker-v0.1\"\n",
        script.display()
    ));
    std::fs::write(&setup.config, &text).unwrap();

    let report = {
        let mut options = CheckOptions::new(&setup.state);
        options.env_lookup = &env_present;
        check(&setup.config, &mut options)
    };
    assert!(!report.success);
    let diagnostics = failed(&report, CheckStage::WorkerRuntime);
    assert_eq!(diagnostics[0].code, "runtime.version_unknown");
    assert_eq!(diagnostics[0].key, "worker.runtime_version");
    let _ = std::fs::remove_dir_all(&setup.dir);
}

/// Stage (4): writability and searchability are judged by the
/// process's effective access, not the directory owner's mode bits —
/// a directory this process cannot search and write fails even when
/// its owner bits look permissive.
#[test]
fn directories_without_effective_access_fail_the_paths_stage() {
    use std::os::unix::fs::PermissionsExt;
    let setup = clean_setup("paths-effective", 1_763_000_000);

    // An existing state root the process cannot search (0666: read and
    // write for all, execute for none) is unwritable for our purposes.
    std::fs::create_dir_all(&setup.state).unwrap();
    std::fs::set_permissions(&setup.state, std::fs::Permissions::from_mode(0o666)).unwrap();
    let report = {
        let mut options = CheckOptions::new(&setup.state);
        options.env_lookup = &env_present;
        check(&setup.config, &mut options)
    };
    std::fs::set_permissions(&setup.state, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert!(!report.success);
    let diagnostics = failed(&report, CheckStage::Paths);
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "paths.unwritable"),
        "a non-searchable root is unwritable: {diagnostics:?}"
    );

    // A missing state root beneath a non-searchable parent is not
    // creatable — owner mode bits cannot grant what the kernel denies.
    let parent = setup.dir.join("noexec");
    std::fs::create_dir(&parent).unwrap();
    std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o666)).unwrap();
    let state = parent.join("state");
    let report = {
        let mut options = CheckOptions::new(&state);
        options.env_lookup = &env_present;
        check(&setup.config, &mut options)
    };
    std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert!(!report.success);
    let diagnostics = failed(&report, CheckStage::Paths);
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "paths.not_creatable"),
        "a missing root under a non-searchable parent is not creatable: {diagnostics:?}"
    );
    let _ = std::fs::remove_dir_all(&setup.dir);
}
