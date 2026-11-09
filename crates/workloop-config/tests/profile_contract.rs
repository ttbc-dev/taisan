//! The versioned `config.toml` contract for slice U2.1: unknown keys
//! and unsupported versions are refused without mutation, the
//! documented sanitized profile loads with the documented defaults,
//! role/repository/allowlist/policy values validate against their
//! owning documents, and paths resolve through the fixed
//! flag → environment → XDG precedence with root separation enforced.

use std::path::{Path, PathBuf};

use workloop_config::profile::{
    LimitInput, ResolvedPaths, SCHEMA_VERSION, default_workspace_root, load_profile, parse_profile,
    resolve_paths, resolve_paths_from_env, validate_root_separation,
};

/// The literal sanitized example from docs/installation-config.md —
/// the profile CF-23 requires to load under this validator.
const DOCUMENTED_EXAMPLE: &str = r#"schema_version = 1

[installation]
capacity_slots = 1

[hosts.h1]
adapter = "github"                    # adapter id from the adapter registry
endpoint = "https://api.github.com"   # stable endpoint identity, not a display alias
contract_version = 1

[[hosts.h1.repositories]]
id = "0123456789"                     # adapter-resolved stable repository id
display = "team/tool"                 # display metadata only, never an equality key
use = "publication"                   # publication target

[[hosts.h1.repositories]]
id = "5566778899"
display = "org/reference-docs"
use = "source"                        # allowlisted read-only source

[roles.publication]
host = "h1"
actor = "taisan-publisher"            # verified adapter-side actor identity
credential_ref = "env:TAISAN_GH_TOKEN"

[roles.review]                        # recommended: a distinct reviewer identity
host = "h1"
actor = "taisan-reviewer"
credential_ref = "file:/home/user/.secrets/taisan-review.token"

[roles.discussion]
host = "h1"
actor = "taisan-publisher"
credential_ref = "env:TAISAN_GH_TOKEN"

[worker]
argv = ["taisan-worker-shim", "--runtime", "claude-code-cli"]
runtime_version = ">=2.0.0"           # pinned to declared range, verified at preflight
model = "claude-sonnet-4-6"           # pinned at load; re-checked after dispatch
max_turns = 200

[verification.egress]
allowlist = ["crates.io", "static.crates.io"]

[capabilities.required]
# Work requirements compose with this floor at admission; callers cannot weaken it
publication = ["repository_read", "branch_read", "push_ref",
               "change_request_read", "change_request_create",
               "change_request_update", "comment_write", "mark_ready"]
review      = ["repository_read", "branch_read", "review_feedback",
               "inline_comment", "thread_reply", "thread_resolve"]
discussion  = ["comment_read", "comment_write"]

[recovery]  # keys defined by recovery-policy.md; defaults apply when omitted
[retention]  # keys defined by retention-policy.md
"#;

/// A minimal valid profile that individual tests then break on purpose.
fn base_profile() -> String {
    r#"schema_version = 1

[hosts.h1]
adapter = "github"
endpoint = "https://api.github.com"
contract_version = 1

[[hosts.h1.repositories]]
id = "0123456789"
use = "publication"

[roles.publication]
host = "h1"
actor = "taisan-publisher"
credential_ref = "env:TAISAN_TOKEN"
"#
    .into()
}

fn root(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("workloop-profile-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn no_env(_: &str) -> Option<std::ffi::OsString> {
    None
}

/// The documented sanitized example loads under this validator and
/// carries the documented defaults for every omitted key (CF-23,
/// recovery-policy and retention-policy default tables).
#[test]
fn documented_sanitized_example_loads_with_documented_defaults() {
    let profile = parse_profile(DOCUMENTED_EXAMPLE).expect("documented example must load");

    assert_eq!(profile.schema_version, SCHEMA_VERSION);
    assert_eq!(profile.installation.capacity_slots, 1);

    let host = &profile.hosts["h1"];
    assert_eq!(host.adapter.as_deref(), Some("github"));
    assert_eq!(host.endpoint.as_deref(), Some("https://api.github.com"));
    assert_eq!(host.contract_version, Some(1));
    assert_eq!(host.repositories.len(), 2);
    assert_eq!(host.repositories[0].use_.as_deref(), Some("publication"));
    assert_eq!(host.repositories[1].use_.as_deref(), Some("source"));

    // Role references keep their exact shape; values resolve only at
    // the adapter boundary, never at load.
    assert_eq!(
        profile.roles["publication"].credential_ref.as_deref(),
        Some("env:TAISAN_GH_TOKEN")
    );
    assert_eq!(
        profile.roles["review"].credential_ref.as_deref(),
        Some("file:/home/user/.secrets/taisan-review.token")
    );
    assert_eq!(
        profile.roles["review"].actor.as_deref(),
        Some("taisan-reviewer")
    );

    // Egress allowlist parses as declared.
    assert_eq!(
        profile.verification.egress.allowlist,
        vec!["crates.io", "static.crates.io"]
    );

    // Publication targets are the opaque ids — display metadata takes
    // no part in the binding.
    assert_eq!(
        profile.publication_targets(),
        vec![("h1".to_string(), "0123456789".to_string())]
    );

    // Worker pins are carried unchanged.
    assert_eq!(profile.worker.argv.len(), 3);
    assert_eq!(profile.worker.runtime_version.as_deref(), Some(">=2.0.0"));
    assert_eq!(profile.worker.model.as_deref(), Some("claude-sonnet-4-6"));
    assert_eq!(profile.worker.max_turns, Some(200));

    // Recovery defaults (docs/recovery-policy.md).
    assert_eq!(profile.recovery.transport.max_retries, LimitInput::Int(3));
    assert_eq!(
        profile.recovery.transport.backoff_seconds,
        vec![
            LimitInput::Int(10),
            LimitInput::Int(30),
            LimitInput::Int(90)
        ]
    );
    assert_eq!(profile.recovery.transport.backoff_jitter_ratio.value(), 0.0);
    assert_eq!(
        profile.recovery.effect_reconcile_seconds,
        LimitInput::Int(600)
    );
    assert_eq!(profile.recovery.max_fix_invocations, LimitInput::Int(3));
    assert_eq!(
        profile.recovery.equivalent_failure_limit,
        LimitInput::Int(2)
    );
    assert_eq!(profile.recovery.no_progress_seconds, LimitInput::Int(1800));
    assert_eq!(
        profile.recovery.automatic_time_limit_seconds,
        LimitInput::Int(14400)
    );

    // Retention defaults (docs/retention-policy.md), including the
    // archive `unlimited` pair.
    assert_eq!(
        profile.retention.workspace.max_age_seconds,
        Some(LimitInput::Int(2592000))
    );
    assert_eq!(
        profile.retention.workspace.max_bytes,
        Some(LimitInput::Int(21474836480))
    );
    assert_eq!(
        profile.retention.worker_output.max_age_seconds,
        Some(LimitInput::Int(2592000))
    );
    assert_eq!(
        profile.retention.worker_output.max_bytes,
        Some(LimitInput::Int(10737418240))
    );
    assert_eq!(
        profile.retention.cas.max_age_seconds,
        Some(LimitInput::Int(7776000))
    );
    assert_eq!(
        profile.retention.cas.max_bytes,
        Some(LimitInput::Int(42949672960))
    );
    assert_eq!(
        profile.retention.handoff_objects.max_age_seconds,
        Some(LimitInput::Int(7776000))
    );
    assert_eq!(
        profile.retention.handoff_objects.max_bytes,
        Some(LimitInput::Int(10737418240))
    );
    assert_eq!(
        profile.retention.archive.max_age_seconds,
        Some(LimitInput::Str("unlimited".into()))
    );
    assert_eq!(
        profile.retention.archive.max_bytes,
        Some(LimitInput::Str("unlimited".into()))
    );
    assert_eq!(
        profile.retention.sweep.interval_seconds,
        LimitInput::Int(3600)
    );
    assert_eq!(profile.retention.sweep.max_bodies, LimitInput::Int(256));
    assert_eq!(
        profile.retention.sweep.max_bytes,
        LimitInput::Int(1073741824)
    );
    assert_eq!(
        profile.retention.sweep.delete_error_limit,
        LimitInput::Int(3)
    );
}

/// Unknown keys at any level fail the load with the offending key
/// named, and the file on disk is byte-identical afterwards — the
/// loader never mutates what it refuses (CF-01).
#[test]
fn unknown_keys_are_refused_naming_the_key_without_mutation() {
    let dir = root("unknown-key");

    for (tag, text, key) in [
        ("top", "schema_version = 1\nbogus_key = 1\n", "bogus_key"),
        (
            "nested",
            "schema_version = 1\n\n[installation]\ncapacity_sots = 2\n",
            "capacity_sots",
        ),
        (
            "role",
            "schema_version = 1\n\n[roles.publication]\nhost = \"h1\"\nactor = \"a\"\ncredential_ref = \"env:T\"\ntoken = \"secret\"\n",
            "token",
        ),
        (
            "table",
            "schema_version = 1\n\n[unsurprise]\nkey = 1\n",
            "unsurprise",
        ),
    ] {
        let path = dir.join(format!("{tag}.toml"));
        std::fs::write(&path, text).unwrap();
        let before = std::fs::read(&path).unwrap();

        let err = load_profile(&path).unwrap_err();
        assert!(err.contains(key), "expected key {key} in error: {err}");
        assert_eq!(
            std::fs::read(&path).unwrap(),
            before,
            "refused load mutated {tag}.toml"
        );
    }

    let _ = std::fs::remove_dir_all(&dir);
}

/// `schema_version` is required and must equal the version this
/// runtime understands: missing, corrupt, zero, and newer versions are
/// refused without migration, file untouched (CF-01).
#[test]
fn unsupported_or_missing_schema_version_is_refused_without_mutation() {
    let dir = root("schema-version");

    for (tag, text) in [
        ("missing", "[installation]\ncapacity_slots = 1\n"),
        ("newer", "schema_version = 2\n"),
        ("zero", "schema_version = 0\n"),
        ("string", "schema_version = \"1\"\n"),
    ] {
        let path = dir.join(format!("{tag}.toml"));
        std::fs::write(&path, text).unwrap();
        let before = std::fs::read(&path).unwrap();

        let err = load_profile(&path).unwrap_err();
        assert!(
            err.contains("schema_version"),
            "expected schema_version in {tag}: {err}"
        );
        assert_eq!(std::fs::read(&path).unwrap(), before, "{tag} file mutated");
    }

    let _ = std::fs::remove_dir_all(&dir);
}

/// Structurally invalid content is refused, and an absent config is
/// reported without the loader creating anything (CF-01, `config.absent`).
#[test]
fn malformed_and_absent_configs_are_refused_without_creating_files() {
    let dir = root("malformed");

    let empty = dir.join("empty.toml");
    std::fs::write(&empty, "").unwrap();
    let err = load_profile(&empty).unwrap_err();
    assert!(
        err.contains("schema_version"),
        "expected schema_version error: {err}"
    );

    let broken = dir.join("broken.toml");
    std::fs::write(&broken, "{ not toml").unwrap();
    let err = load_profile(&broken).unwrap_err();
    assert!(err.contains("config"), "expected a parse error: {err}");

    let absent = dir.join("absent.toml");
    let err = load_profile(&absent).unwrap_err();
    assert!(
        err.contains("config.absent"),
        "expected config.absent: {err}"
    );
    assert!(!absent.exists(), "a refused load must not create the file");

    let _ = std::fs::remove_dir_all(&dir);
}

/// Role references validate at load: unknown role names, missing
/// fields, actors on undeclared hosts, and non-conforming credential
/// providers each fail with the role named (CF-05, and the literal
/// `file:~/token.txt` example from installation-config.md).
#[test]
fn role_references_are_validated_with_the_role_named() {
    // Unknown role name.
    let mut text = base_profile().replace("[roles.publication]", "[roles.maintainer]");
    let err = parse_profile(&text).unwrap_err();
    assert!(err.contains("maintainer"), "expected role name: {err}");

    // Missing actor — the error must name the role, not only the field.
    text = base_profile().replace("actor = \"taisan-publisher\"\n", "");
    let err = parse_profile(&text).unwrap_err();
    assert!(
        err.contains("roles.publication"),
        "expected the role: {err}"
    );
    assert!(err.contains("actor"), "expected the field: {err}");

    // Missing credential_ref.
    text = base_profile().replace("credential_ref = \"env:TAISAN_TOKEN\"\n", "");
    let err = parse_profile(&text).unwrap_err();
    assert!(
        err.contains("roles.publication"),
        "expected the role: {err}"
    );
    assert!(err.contains("credential_ref"), "expected the field: {err}");

    // Actor bound to a host that is never declared.
    text = base_profile().replace("host = \"h1\"", "host = \"h2\"");
    let err = parse_profile(&text).unwrap_err();
    assert!(
        err.contains("roles.publication"),
        "expected the role: {err}"
    );
    assert!(err.contains("h2"), "expected the host: {err}");

    // Credential providers: shape only, values never resolved.
    for (bad, needle) in [
        ("file:~/token.txt", "absolute"),
        ("file:relative/token.txt", "absolute"),
        ("env:1BAD", "env:"),
        ("env:", "env:"),
        ("bogus:value", "unknown provider"),
        ("command:   ", "command:"),
    ] {
        text = base_profile().replace(
            "credential_ref = \"env:TAISAN_TOKEN\"",
            &format!("credential_ref = \"{bad}\""),
        );
        let err = parse_profile(&text).unwrap_err();
        assert!(
            err.contains(needle) && err.contains("roles.publication"),
            "expected {needle} + role for {bad}: {err}"
        );
    }

    // Every documented provider shape loads.
    for good in [
        "env:TAISAN_TOKEN",
        "file:/etc/secrets/token",
        "command:secret-manager get taisan",
    ] {
        text = base_profile().replace(
            "credential_ref = \"env:TAISAN_TOKEN\"",
            &format!("credential_ref = \"{good}\""),
        );
        parse_profile(&text).unwrap_or_else(|e| panic!("{good} must load: {e}"));
    }

    // Identifier values are bounded before any binding is constructed.
    let long = "a".repeat(600);
    text = base_profile().replace(
        "actor = \"taisan-publisher\"",
        &format!("actor = \"{long}\""),
    );
    let err = parse_profile(&text).unwrap_err();
    assert!(err.contains("exceeds"), "expected the size bound: {err}");
}

/// `use` selects the structural role of a repository: unknown or
/// missing values are load errors, and a `source` repository is never
/// offered as a publication target (CF-18).
#[test]
fn repository_use_is_structurally_bounded() {
    let text = base_profile().replace("use = \"publication\"", "use = \"publish\"");
    let err = parse_profile(&text).unwrap_err();
    assert!(
        err.contains("use") && err.contains("publish"),
        "expected use error: {err}"
    );

    let text = base_profile().replace("use = \"publication\"\n", "");
    let err = parse_profile(&text).unwrap_err();
    assert!(err.contains("use"), "expected the missing use: {err}");

    let text = r#"schema_version = 1

[hosts.h1]
adapter = "github"
endpoint = "https://api.github.com"
contract_version = 1

[[hosts.h1.repositories]]
id = "0123456789"
use = "source"

[roles.publication]
host = "h1"
actor = "taisan-publisher"
credential_ref = "env:TAISAN_TOKEN"
"#;
    let profile = parse_profile(text).expect("source-only profile loads");
    assert!(
        profile.publication_targets().is_empty(),
        "a source repository must never be a publication target"
    );
}

/// Limit keys follow the approved policies: `0`, negative, and
/// unit-suffixed values fail the load naming the key; `unlimited` is
/// accepted only on retention axes; recovery jitter stays in range
/// (CF-02, RT-13).
#[test]
fn limit_values_follow_the_approved_policies() {
    // Retention: 0 is a load error, never "no limit".
    let text = base_profile() + "[retention.workspace]\nmax_age_seconds = 0\n";
    let err = parse_profile(&text).unwrap_err();
    assert!(
        err.contains("retention.workspace.max_age_seconds") && err.contains("0"),
        "expected the key: {err}"
    );

    // Negative value.
    let text = base_profile() + "[retention.workspace]\nmax_bytes = -1\n";
    let err = parse_profile(&text).unwrap_err();
    assert!(
        err.contains("retention.workspace.max_bytes"),
        "expected the key: {err}"
    );

    // Unknown unit.
    let text = base_profile() + "[retention.cas]\nmax_age_seconds = \"30d\"\n";
    let err = parse_profile(&text).unwrap_err();
    assert!(
        err.contains("retention.cas.max_age_seconds") && err.contains("30d"),
        "expected the key: {err}"
    );

    // `unlimited` is accepted on a retention axis; the other axis keeps
    // its default, so the category still has one bound.
    let text = base_profile() + "[retention.cas]\nmax_age_seconds = \"unlimited\"\n";
    let profile = parse_profile(&text).expect("unlimited retention axis must load");
    assert_eq!(
        profile.retention.cas.max_age_seconds,
        Some(LimitInput::Str("unlimited".into()))
    );
    assert_eq!(
        profile.retention.cas.max_bytes,
        Some(LimitInput::Int(42949672960)),
        "the omitted axis keeps the documented default"
    );

    // Recovery: `unlimited` is not part of the recovery vocabulary.
    let text = base_profile() + "[recovery]\neffect_reconcile_seconds = \"unlimited\"\n";
    let err = parse_profile(&text).unwrap_err();
    assert!(
        err.contains("recovery.effect_reconcile_seconds"),
        "expected the key: {err}"
    );

    // A zero recovery window is refused.
    let text = base_profile() + "[recovery]\nautomatic_time_limit_seconds = 0\n";
    let err = parse_profile(&text).unwrap_err();
    assert!(
        err.contains("recovery.automatic_time_limit_seconds"),
        "expected the key: {err}"
    );

    // A `0` backoff entry is a valid immediate retry; an empty
    // schedule is not.
    let text = base_profile() + "[recovery.transport]\nbackoff_seconds = [0, 10]\n";
    parse_profile(&text).expect("zero schedule entry is a valid immediate retry");
    let text = base_profile() + "[recovery.transport]\nbackoff_seconds = []\n";
    let err = parse_profile(&text).unwrap_err();
    assert!(
        err.contains("recovery.transport.backoff_seconds"),
        "expected the key: {err}"
    );

    // Jitter ratio range.
    let text = base_profile() + "[recovery.transport]\nbackoff_jitter_ratio = 1.5\n";
    let err = parse_profile(&text).unwrap_err();
    assert!(
        err.contains("backoff_jitter_ratio"),
        "expected the key: {err}"
    );
    let text = base_profile() + "[recovery.transport]\nbackoff_jitter_ratio = 0.25\n";
    parse_profile(&text).expect("in-range jitter must load");

    // Sweep bounds are limits too.
    let text = base_profile() + "[retention.sweep]\nmax_bodies = 0\n";
    let err = parse_profile(&text).unwrap_err();
    assert!(
        err.contains("retention.sweep.max_bodies"),
        "expected the key: {err}"
    );
}

/// Required capability names are validated against the closed
/// `HostCapability` set: an unknown name fails the load, never a
/// silent no-op.
#[test]
fn capability_names_are_closed() {
    let text = base_profile() + "[capabilities.required]\npublication = [\"comment_writ\"]\n";
    let err = parse_profile(&text).unwrap_err();
    assert!(
        err.contains("capabilities.required.publication") && err.contains("comment_writ"),
        "expected the capability error: {err}"
    );

    // The owner-documented baseline names are inside the closed set.
    let text = base_profile()
        + "[capabilities.required]\ndiscussion = [\"comment_read\", \"comment_write\"]\n";
    parse_profile(&text).expect("documented capability names must load");
}

/// Host binding fields, repository ids, the egress allowlist, and the
/// installation capacity each fail closed with the offending key named.
#[test]
fn host_bindings_allowlist_and_capacity_are_validated() {
    // Missing host fields name the host.
    for (remove, needle) in [
        ("adapter = \"github\"\n", "hosts.h1: missing 'adapter'"),
        (
            "endpoint = \"https://api.github.com\"\n",
            "hosts.h1: missing 'endpoint'",
        ),
        (
            "contract_version = 1\n",
            "hosts.h1: missing 'contract_version'",
        ),
    ] {
        let text = base_profile().replace(remove, "");
        let err = parse_profile(&text).unwrap_err();
        assert!(err.contains(needle), "expected {needle}: {err}");
    }

    // A versionless binding and an empty adapter id are refused.
    let text = base_profile().replace("contract_version = 1", "contract_version = 0");
    let err = parse_profile(&text).unwrap_err();
    assert!(err.contains("contract_version"), "expected the key: {err}");
    let text = base_profile().replace("adapter = \"github\"", "adapter = \"\"");
    let err = parse_profile(&text).unwrap_err();
    assert!(err.contains("adapter"), "expected the key: {err}");

    // A repository without its stable id is refused.
    let text = base_profile().replace("id = \"0123456789\"\n", "");
    let err = parse_profile(&text).unwrap_err();
    assert!(err.contains("repositories[0]"), "expected the index: {err}");

    // Allowlist entries must be bare host names.
    for (entry, needle) in [
        ("\"\"", "must not be empty"),
        ("\"crates.io static\"", "whitespace"),
        ("\"https://crates.io\"", "not a URL"),
    ] {
        let text = base_profile() + &format!("[verification.egress]\nallowlist = [{entry}]\n");
        let err = parse_profile(&text).unwrap_err();
        assert!(
            err.contains("verification.egress.allowlist") && err.contains(needle),
            "expected allowlist {needle}: {err}"
        );
    }

    // A zero-slot installation cannot dispatch.
    let text = "schema_version = 1\n\n[installation]\ncapacity_slots = 0\n";
    let err = parse_profile(text).unwrap_err();
    assert!(err.contains("capacity_slots"), "expected the key: {err}");
}

/// The legacy and alternate spellings recorded in existing state still
/// deserialize into the same closed-set member, so the rename cannot
/// split identity across reloads.
#[test]
fn capability_aliases_deserialize_into_the_closed_set() {
    use workloop_core::identity::HostCapability;

    let legacy: HostCapability = serde_json::from_str("\"comment_create\"").unwrap();
    assert_eq!(legacy, HostCapability::CommentWrite);
    assert_eq!(legacy.name(), "comment_write");

    let ready: HostCapability = serde_json::from_str("\"ready\"").unwrap();
    assert_eq!(ready, HostCapability::MarkReady);
    assert_eq!(ready.name(), "mark_ready");
    assert_eq!(
        HostCapability::from_name("comment_create"),
        Some(HostCapability::CommentWrite)
    );
    assert_eq!(
        HostCapability::from_name("ready"),
        Some(HostCapability::MarkReady)
    );
}

/// Path precedence is fixed: explicit flag → environment override →
/// XDG default; the ambient working directory is never consulted, and
/// relative overrides are refused (CF-16).
#[test]
fn path_precedence_is_flag_env_xdg_and_never_cwd() {
    // Flag beats environment beats XDG.
    let lookup = |key: &str| -> Option<std::ffi::OsString> {
        match key {
            "TAISAN_CONFIG" => Some("/env/config.toml".into()),
            "TAISAN_STATE_DIR" => Some("/env/state".into()),
            "XDG_CONFIG_HOME" => Some("/xdg/config".into()),
            "XDG_DATA_HOME" => Some("/xdg/data".into()),
            "HOME" => Some("/home/u".into()),
            _ => None,
        }
    };
    let resolved = resolve_paths(
        Some(Path::new("/flag/config.toml")),
        Some(Path::new("/flag/state")),
        &lookup,
    )
    .unwrap();
    assert_eq!(
        resolved,
        ResolvedPaths {
            config_path: PathBuf::from("/flag/config.toml"),
            state_root: PathBuf::from("/flag/state"),
        }
    );

    // Environment beats XDG.
    let resolved = resolve_paths(None, None, &lookup).unwrap();
    assert_eq!(resolved.config_path, PathBuf::from("/env/config.toml"));
    assert_eq!(resolved.state_root, PathBuf::from("/env/state"));

    // XDG default when no override is set.
    let xdg_only = |key: &str| -> Option<std::ffi::OsString> {
        match key {
            "XDG_CONFIG_HOME" => Some("/xdg/config".into()),
            "XDG_DATA_HOME" => Some("/xdg/data".into()),
            "HOME" => Some("/home/u".into()),
            _ => None,
        }
    };
    let resolved = resolve_paths(None, None, &xdg_only).unwrap();
    assert_eq!(
        resolved.config_path,
        PathBuf::from("/xdg/config/taisan/config.toml")
    );
    assert_eq!(resolved.state_root, PathBuf::from("/xdg/data/taisan"));

    // HOME fallback under the documented XDG layout.
    let resolved =
        resolve_paths(None, None, &|key| (key == "HOME").then(|| "/home/u".into())).unwrap();
    assert_eq!(
        resolved.config_path,
        PathBuf::from("/home/u/.config/taisan/config.toml")
    );
    assert_eq!(
        resolved.state_root,
        PathBuf::from("/home/u/.local/share/taisan")
    );

    // Relative XDG values are ignored per the XDG specification.
    let resolved = resolve_paths(None, None, &|key| match key {
        "XDG_CONFIG_HOME" => Some("relative/xdg".into()),
        "HOME" => Some("/home/u".into()),
        _ => None,
    })
    .unwrap();
    assert_eq!(
        resolved.config_path,
        PathBuf::from("/home/u/.config/taisan/config.toml")
    );

    // An absolute XDG value resolves without consulting HOME.
    let resolved = resolve_paths(None, None, &|key| match key {
        "XDG_CONFIG_HOME" => Some("/xdg/config".into()),
        "XDG_DATA_HOME" => Some("/xdg/data".into()),
        _ => None,
    })
    .unwrap();
    assert_eq!(
        resolved.config_path,
        PathBuf::from("/xdg/config/taisan/config.toml")
    );
    assert_eq!(resolved.state_root, PathBuf::from("/xdg/data/taisan"));

    // Relative overrides are refused — they would resolve against the
    // ambient working directory.
    let err = resolve_paths(Some(Path::new("relative/config.toml")), None, &no_env).unwrap_err();
    assert!(
        err.contains("--config") && err.contains("absolute"),
        "expected flag error: {err}"
    );
    let err = resolve_paths(
        Some(Path::new("/abs/c.toml")),
        Some(Path::new("state")),
        &no_env,
    )
    .unwrap_err();
    assert!(
        err.contains("--state-dir"),
        "expected state flag error: {err}"
    );
    let err = resolve_paths(None, None, &|key| {
        (key == "TAISAN_CONFIG").then(|| "relative.toml".into())
    })
    .unwrap_err();
    assert!(err.contains("TAISAN_CONFIG"), "expected env error: {err}");
    let err = resolve_paths(None, None, &|key| match key {
        "TAISAN_STATE_DIR" => Some("relative/state".into()),
        "HOME" => Some("/home/u".into()),
        _ => None,
    })
    .unwrap_err();
    assert!(
        err.contains("TAISAN_STATE_DIR"),
        "expected state env error: {err}"
    );

    // A missing HOME with no override is an error, not a cwd fallback.
    let err = resolve_paths(None, None, &no_env).unwrap_err();
    assert!(err.contains("HOME"), "expected HOME error: {err}");

    // A non-absolute HOME is refused for the same reason.
    let err = resolve_paths(None, None, &|key| {
        (key == "HOME").then(|| "relative/home".into())
    })
    .unwrap_err();
    assert!(err.contains("HOME"), "expected HOME error: {err}");

    // The real-environment wrapper honours absolute flags without
    // reading any override variable.
    let resolved = resolve_paths_from_env(
        Some(Path::new("/flag/config.toml")),
        Some(Path::new("/flag/state")),
    )
    .unwrap();
    assert_eq!(resolved.config_path, PathBuf::from("/flag/config.toml"));
    assert_eq!(resolved.state_root, PathBuf::from("/flag/state"));
}

/// Root separation refuses nesting violations in either direction and
/// keeps the documented layout loadable (CF-17). Nothing on disk is
/// touched by the check.
#[test]
fn root_separation_refuses_nesting_violations() {
    let config = Path::new("/home/u/.config/taisan/config.toml");
    let state = Path::new("/home/u/.local/share/taisan");
    let workspace = default_workspace_root(state);
    assert_eq!(
        workspace,
        Path::new("/home/u/.local/share/taisan/workspaces")
    );

    // The documented layout is accepted.
    validate_root_separation(config, state, &workspace).expect("documented layout must pass");

    // A state root nested inside a workspace is refused.
    let workspace = Path::new("/home/u/projects/checkout");
    let state = Path::new("/home/u/projects/checkout/state");
    let err = validate_root_separation(config, state, workspace).unwrap_err();
    assert!(
        err.contains("nested inside workspace"),
        "expected nesting error: {err}"
    );

    // A workspace root equal to the state root is refused.
    let state = Path::new("/home/u/.local/share/taisan");
    let err = validate_root_separation(config, state, state).unwrap_err();
    assert!(
        err.contains("state root") && err.contains("workspace"),
        "expected overlap: {err}"
    );

    // A workspace root outside the state root is refused — the
    // managed tree must stay under the installation state root.
    let err = validate_root_separation(config, state, Path::new("/mnt/workspaces")).unwrap_err();
    assert!(
        err.contains("documented managed root"),
        "expected layout error: {err}"
    );

    // A workspace root inside the state root but not the documented
    // child is refused (the vice-versa direction).
    let err = validate_root_separation(
        config,
        state,
        Path::new("/home/u/.local/share/taisan/other"),
    )
    .unwrap_err();
    assert!(
        err.contains("documented managed root"),
        "expected layout error: {err}"
    );

    // The config path must stay outside the managed trees.
    let workspace = default_workspace_root(state);
    let err = validate_root_separation(
        Path::new("/home/u/.local/share/taisan/workspaces/config.toml"),
        state,
        &workspace,
    )
    .unwrap_err();
    assert!(
        err.contains("workspace root"),
        "expected config/workspace error: {err}"
    );
    let err = validate_root_separation(
        Path::new("/home/u/.local/share/taisan/config.toml"),
        state,
        &workspace,
    )
    .unwrap_err();
    assert!(
        err.contains("state root"),
        "expected config/state error: {err}"
    );

    // Relative inputs are refused rather than resolved against cwd.
    let err = validate_root_separation(Path::new("config.toml"), state, &workspace).unwrap_err();
    assert!(err.contains("absolute"), "expected absolute error: {err}");
}
