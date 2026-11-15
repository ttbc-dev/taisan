//! U7.4 named matrix proof: every applicable Command/Query row of the
//! [agent-interface](../../../docs/agent-interface.md) workflow — the
//! CLI contract's command surface as narrowed by the U7.4 spec
//! clarification — is exercised twice against identically seeded
//! installations: once through the **installed** shipped
//! `skills/taisan/SKILL.md` (arguments parsed out of the installed
//! document) and once through the **direct CLI** (arguments built
//! literally by the test). Each row asserts an identical
//! status/diagnostic envelope (exit code, `schema_version`, `status`,
//! `command.kind`, `reason.code`, addressed `work_id`, `reason.message`,
//! and the `result` payload — with the per-row root directory
//! normalized) on both paths plus the row's documented expectation.
//! The row set is enumerated explicitly in
//! `APPLICABLE_ROWS`, so no applicable row can be silently skipped;
//! `run*` driver rows belong to the walkthrough's recovery leg and TUI/
//! MCP remain excluded.
//!
//! Claims boundary: fixture/contract results only — the real `taisan`
//! binary driving real Commands and Queries against a real SqliteStore,
//! with prior-history states the Command surface cannot reach on its
//! own (a published change request, a sealed review round, a paused or
//! failed Execution) seeded through the same store the binary opens.
//! The clean-install test installs the shipped skill file to the
//! documented `npx skills add` project destination (`.agents/skills/`)
//! in an environment with no prior taisan config or state and invokes
//! it once end-to-end; the live `npx skills add` registry install
//! transcript is captured in `docs/skill-walkthrough.md`, not re-run
//! here. No live host/Worker/E2E scenario is claimed.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use workloop_config::{Installation, load_or_create};
use workloop_core::domain::{ExecutionId, ExecutionState, RepairPrerequisite, WorkId};
use workloop_core::identity::{
    ActorRole, ChangeRequestId, ChangeRequestRef, CommitRevision, DiffTarget, HostActorId,
    HostActorRef, HostInstanceId, RepositoryId, RepositoryRef,
};
use workloop_core::observation::{
    Completeness, HostActionStatus, NormalizedEffectFact, Observation, ObservationFact,
    OccurrenceInput,
};
use workloop_core::store::{Effect, HostAction, HostActionIntent, Store};
use workloop_core::work::{self, WorkloopConfig};
use workloop_store::sqlite::SqliteStore;

const NOW: i64 = 1_700_000_000;

/// The applicable matrix rows — every Command/Query row of the CLI
/// contract needed by the agent-interface workflow table, per the U7.4
/// spec clarification: the setup rows where the walkthrough exercises
/// them, `submit`, the inspect set, the intervention set, the judgment
/// set, and `command-result`. The `run*` driver rows are exercised by
/// the walkthrough's recovery leg, not this matrix; TUI/MCP are
/// excluded. This list is the contract: the test iterates it and
/// asserts every entry ran.
const APPLICABLE_ROWS: &[&str] = &[
    "init",
    "check",
    "check --probe",
    "submit",
    "work-list",
    "work-detail",
    "attention",
    "capacity",
    "retry",
    "resume",
    "cancel",
    "acknowledge",
    "judge-findings",
    "release-judgment",
    "command-result",
];

/// Documented expectation per row: exit code, envelope `status`,
/// `command.kind` (`""` where the envelope carries no command identity),
/// and `reason.code` (`""` when there is no reason).
struct Expected {
    row: &'static str,
    exit: i32,
    status: &'static str,
    kind: &'static str,
    reason: &'static str,
}

const EXPECTED: &[Expected] = &[
    Expected {
        row: "init",
        exit: 0,
        status: "accepted",
        kind: "",
        reason: "",
    },
    Expected {
        row: "check",
        exit: 0,
        status: "accepted",
        kind: "",
        reason: "",
    },
    Expected {
        row: "check --probe",
        exit: 3,
        status: "error",
        kind: "",
        reason: "config",
    },
    Expected {
        row: "submit",
        exit: 0,
        status: "accepted",
        kind: "submit",
        reason: "",
    },
    Expected {
        row: "work-list",
        exit: 0,
        status: "accepted",
        kind: "work_list",
        reason: "",
    },
    Expected {
        row: "work-detail",
        exit: 0,
        status: "accepted",
        kind: "work_detail",
        reason: "",
    },
    Expected {
        row: "attention",
        exit: 0,
        status: "accepted",
        kind: "attention",
        reason: "",
    },
    Expected {
        row: "capacity",
        exit: 0,
        status: "accepted",
        kind: "capacity",
        reason: "",
    },
    Expected {
        row: "retry",
        exit: 0,
        status: "accepted",
        kind: "retry",
        reason: "",
    },
    Expected {
        row: "resume",
        exit: 0,
        status: "accepted",
        kind: "resume",
        reason: "",
    },
    Expected {
        row: "cancel",
        exit: 0,
        status: "accepted",
        kind: "cancel",
        reason: "",
    },
    Expected {
        row: "acknowledge",
        exit: 0,
        status: "accepted",
        kind: "acknowledge",
        reason: "",
    },
    Expected {
        row: "judge-findings",
        exit: 0,
        status: "accepted",
        kind: "judge_findings",
        reason: "",
    },
    Expected {
        row: "release-judgment",
        exit: 0,
        status: "accepted",
        kind: "release_judgment",
        reason: "",
    },
    Expected {
        row: "command-result",
        exit: 0,
        status: "accepted",
        kind: "command_result",
        reason: "",
    },
];

fn skill_md() -> &'static str {
    include_str!("../../../skills/taisan/SKILL.md")
}

fn shipped_skill() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../skills/taisan/SKILL.md")
}

fn bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_taisan"))
}

fn root(tag: &str) -> PathBuf {
    let sanitized: String = tag
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let dir =
        std::env::temp_dir().join(format!("workloop-u74-{}-{}", sanitized, std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn output_at(root: &Path, args: &[&str]) -> Output {
    let db = root.join("workloop.db");
    let config = root.join("config.json");
    let mut full = vec![
        "--db".into(),
        db.to_string_lossy().into_owned(),
        "--config".into(),
        config.to_string_lossy().into_owned(),
    ];
    full.extend(args.iter().map(|s| s.to_string()));
    Command::new(bin())
        .args(&full)
        .output()
        .expect("taisan runs")
}

/// Setup rows resolve their profile and state through the fixed
/// flag → env → XDG precedence (never the ambient working directory).
fn setup_output(root: &Path, args: &[String]) -> Output {
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    Command::new(bin())
        .args(&refs)
        .env("TAISAN_CONFIG", root.join("config.toml"))
        .env("TAISAN_STATE_DIR", root.join("state"))
        .env("TAISAN_TEST_TOKEN", "matrix-credential-value")
        .output()
        .expect("taisan runs")
}

fn run_at(root: &Path, args: &[&str]) -> serde_json::Value {
    let output = output_at(root, args);
    assert!(
        output.status.success(),
        "taisan {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    envelope(&output)
}

fn envelope(output: &Output) -> serde_json::Value {
    let stdout = String::from_utf8_lossy(&output.stdout);
    serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("stdout is one JSON object ({e}): {stdout}"))
}

fn submit_at(root: &Path, key: &str, intent: &str) -> serde_json::Value {
    run_at(
        root,
        &["submit", "--intent", intent, "--idempotency-key", key],
    )
}

fn work_detail(root: &Path, work_id: &str) -> serde_json::Value {
    run_at(root, &["work-detail", "--work", work_id])
}

// ---- Fixture history the CLI cannot reach on its own ----

fn installation(root: &Path) -> Installation {
    load_or_create(&root.join("config.json")).expect("installation loads")
}

fn workloop_config(root: &Path) -> WorkloopConfig {
    installation(root)
        .workloop_config()
        .expect("installation config is valid")
}

fn store(root: &Path) -> SqliteStore {
    SqliteStore::open(&root.join("workloop.db"), installation(root).capacity).expect("store opens")
}

fn host(root: &Path) -> HostInstanceId {
    HostInstanceId::new(installation(root).host_instance).unwrap()
}

fn repository(root: &Path) -> RepositoryRef {
    RepositoryRef::new(
        host(root),
        RepositoryId::new(installation(root).repository).unwrap(),
    )
}

fn publisher(root: &Path) -> HostActorRef {
    let installation = installation(root);
    HostActorRef {
        host: host(root),
        actor: HostActorId::new(installation.host_actors["publication"].clone()).unwrap(),
    }
}

fn change(root: &Path) -> ChangeRequestRef {
    ChangeRequestRef {
        repository: repository(root),
        change_request: ChangeRequestId::trusted("cr-1"),
        display_number: Some(1),
    }
}

fn rev(root: &Path, object: &str) -> CommitRevision {
    CommitRevision::new(repository(root), object).unwrap()
}

fn apply_effects(store: &SqliteStore, effects: &[Effect]) {
    store
        .transact(|tx| {
            for e in effects {
                tx.apply(e)?;
            }
            Ok(())
        })
        .expect("fixture history applies");
}

fn read_execution(store: &SqliteStore, id: &ExecutionId) -> workloop_core::domain::Execution {
    store
        .transact(|tx| tx.execution(id))
        .expect("read")
        .expect("execution exists")
}

fn force_state(store: &SqliteStore, id: &ExecutionId, to: ExecutionState) {
    let from = read_execution(store, id).state;
    let mut e = read_execution(store, id);
    e.state = to.clone();
    if to.is_terminal() {
        e.terminal_code = Some("fixture".into());
        e.outcome = Some(workloop_core::observation::Outcome {
            state: to.clone(),
            code: Some("fixture".into()),
            detail: None,
            evidence_refs: vec![],
            recorded_seq: 0,
        });
    }
    apply_effects(
        store,
        &[Effect::UpdateExecution {
            execution: e,
            expected_state: from,
        }],
    );
}

fn pause_for_judgment(store: &SqliteStore, id: &ExecutionId) {
    let mut e = read_execution(store, id);
    let from = e.state.clone();
    e.state = ExecutionState::Paused;
    e.pause_reason = Some("review findings require human disposition".into());
    e.repair_prerequisites = vec![RepairPrerequisite::OperatorDecision];
    e.paused_since_unix_seconds = Some(NOW);
    apply_effects(
        store,
        &[Effect::UpdateExecution {
            execution: e,
            expected_state: from,
        }],
    );
}

fn open_change(root: &Path, work_id: &str, head: &str) {
    let store = store(root);
    let work_id = WorkId(work_id.to_string());
    let created_seq = store.transact(|tx| tx.alloc_seq()).expect("seq allocates");
    apply_effects(
        &store,
        &[
            Effect::InsertHostActionIntent {
                intent: HostActionIntent {
                    action_id: "act-open".into(),
                    work_id: work_id.clone(),
                    host: host(root),
                    repository: repository(root),
                    role: ActorRole::Publication,
                    actor: publisher(root),
                    action: HostAction::OpenChangeRequest {
                        source_branch: format!("work/{}", work_id.0),
                        target_branch: "main".into(),
                        title: "fixture".into(),
                        body: String::new(),
                    },
                    payload_digest: "fixture".into(),
                    status: HostActionStatus::Applied,
                    authenticated_actor: None,
                    effects: vec![],
                    created_seq,
                },
            },
            Effect::InsertObservation {
                observation: Observation {
                    delivery: None,
                    subject_work_id: Some(work_id.clone()),
                    subject_execution_id: None,
                    fact: ObservationFact::HostActionResult {
                        action_id: "act-open".into(),
                        authenticated_actor: Some(publisher(root)),
                        status: HostActionStatus::Applied,
                        effects: vec![NormalizedEffectFact::ChangeOpened {
                            change: change(root),
                            head: rev(root, head),
                            source_branch: format!("work/{}", work_id.0),
                            target_branch: "main".into(),
                        }],
                    },
                    observed_unix_seconds: NOW,
                },
            },
            Effect::InsertObservation {
                observation: Observation {
                    delivery: None,
                    subject_work_id: Some(work_id.clone()),
                    subject_execution_id: None,
                    fact: ObservationFact::ChangeHead {
                        change: change(root),
                        head: rev(root, head),
                    },
                    observed_unix_seconds: NOW,
                },
            },
        ],
    );
}

fn seal_review(root: &Path, work_id: &str, head: &str, source_keys: &[&str]) {
    let obs = Observation {
        delivery: None,
        subject_work_id: Some(WorkId(work_id.to_string())),
        subject_execution_id: None,
        fact: ObservationFact::Review {
            target: DiffTarget {
                change: change(root),
                head: rev(root, head),
                base: None,
                target_revision: None,
            },
            source: "reviewer".into(),
            source_revision: None,
            completeness: Completeness::Complete,
            occurrences: source_keys
                .iter()
                .map(|key| OccurrenceInput {
                    source_key: key.to_string(),
                    content_digest: None,
                    severity: None,
                    position: None,
                    thread_id: None,
                })
                .collect(),
        },
        observed_unix_seconds: NOW,
    };
    work::handle_observation(&store(root), &workloop_config(root), &obs, NOW).unwrap();
}

fn displayed_occurrences(detail: &serde_json::Value) -> Vec<String> {
    detail["result"]["findings"]
        .as_array()
        .expect("detail carries a findings list")
        .iter()
        .map(|f| f["occurrence_id"].as_str().unwrap().to_string())
        .collect()
}

// ---- Installed-skill document parsing ----

/// Install the shipped skill into a fresh agent environment at the
/// documented `npx skills add` project destination
/// (`.agents/skills/taisan/SKILL.md`) and return the installed
/// document, asserted byte-identical to the shipped file.
fn install_skill_into_fresh_agent_environment() -> String {
    let env_root = root("installed-skill");
    let dest = env_root.join(".agents/skills/taisan/SKILL.md");
    std::fs::create_dir_all(dest.parent().expect("skill dir")).unwrap();
    std::fs::copy(shipped_skill(), &dest).expect("shipped skill copies");
    let installed = std::fs::read_to_string(&dest).expect("installed skill reads");
    assert_eq!(
        installed,
        skill_md(),
        "the installed file is the shipped skills/taisan/SKILL.md"
    );
    installed
}

/// Fenced code blocks whose content invokes `taisan`, as argv token
/// vectors with placeholders intact.
fn fenced_invocations(md: &str) -> Vec<Vec<String>> {
    let mut blocks = Vec::new();
    let mut in_fence = false;
    let mut current = String::new();
    for line in md.lines() {
        if line.trim_start().starts_with("```") {
            if in_fence {
                let block = current.trim().to_string();
                if block.contains("taisan ") {
                    blocks.push(
                        block
                            .split_whitespace()
                            .map(|s| s.to_string())
                            .collect::<Vec<String>>(),
                    );
                }
                current.clear();
            }
            in_fence = !in_fence;
            continue;
        }
        if in_fence {
            current.push_str(line);
            current.push(' ');
        }
    }
    blocks
}

/// The installed-skill invocation for `op` selected by `pick`, with
/// every `<PLACEHOLDER>` substituted from `subs`.
fn skill_args(
    md: &str,
    op: &str,
    pick: impl Fn(&[String]) -> bool,
    subs: &[(&str, &str)],
) -> Vec<String> {
    fenced_invocations(md)
        .into_iter()
        .find(|argv| {
            argv.first().map(|s| s == "taisan").unwrap_or(false)
                && argv.get(1).map(|s| s == op).unwrap_or(false)
                && pick(argv)
        })
        .unwrap_or_else(|| panic!("installed SKILL.md documents a `taisan {op}` invocation"))
        .into_iter()
        .skip(1)
        .map(|token| {
            let mut out = token;
            for (placeholder, value) in subs {
                out = out.replace(placeholder, value);
            }
            out
        })
        .collect()
}

fn as_refs(args: &[String]) -> Vec<&str> {
    args.iter().map(String::as_str).collect()
}

/// The status/diagnostic surface of an envelope: what both paths must
/// agree on. Volatile identities (command id, timestamps, config
/// paths) are intentionally excluded — the per-row root directory is
/// normalized so path-valued fields (e.g. `init`'s `result.path`)
/// compare equal across the two independently rooted installations —
/// but the addressed scope (`work_id`), the diagnostic message, and
/// the query/mutation `result` are part of the surface: two paths
/// that query different scopes must not compare equal.
fn status_surface(
    root: &Path,
    output: &Output,
) -> (i32, String, String, String, String, String, String, String) {
    let code = output.status.code().unwrap_or(-1);
    let env = envelope(output);
    let root_str = root.display().to_string();
    let normalize = |v: &serde_json::Value| v.to_string().replace(&root_str, "<ROOT>");
    (
        code,
        env["schema_version"].as_str().unwrap_or("").to_string(),
        env["status"].as_str().unwrap_or("").to_string(),
        env["command"]["kind"].as_str().unwrap_or("").to_string(),
        env["reason"]["code"].as_str().unwrap_or("").to_string(),
        env["work_id"].as_str().unwrap_or("").to_string(),
        env["reason"]["message"].as_str().unwrap_or("").to_string(),
        normalize(&env["result"]),
    )
}

#[derive(Clone, Copy, PartialEq)]
enum Path_ {
    Skill,
    Direct,
}

// ---- One scenario per applicable row, both paths ----

/// Setup rows: the profile scaffold and the ordered preflight. The
/// skill path parses the invocation out of the installed document; the
/// direct path builds it literally — two independent constructions of
/// the same setup command against identical fresh environments.
fn scenario_setup(row: &str, root: &Path, path: Path_, md: &str) -> Output {
    if row == "init" {
        let init_subs = [
            ("<ENDPOINT>", "https://github.com"),
            ("<REPO_ID:USE>", "0123456789:publication"),
            (
                "<ROLE:ACTOR:CREDENTIAL_REF>",
                "publication:matrix-publisher:env:TAISAN_TEST_TOKEN",
            ),
            (
                "<ROLE:LOGIN@HOST/REPO>",
                "publication:matrix-publisher@https://github.com/0123456789",
            ),
            ("<EGRESS_HOST>", "crates.io"),
        ];
        let args = match path {
            Path_::Skill => skill_args(
                md,
                "init",
                |a| a.iter().any(|t| t == "--endpoint"),
                &init_subs,
            ),
            Path_::Direct => vec![
                "init".to_string(),
                "--endpoint".to_string(),
                "https://github.com".to_string(),
                "--repo".to_string(),
                "0123456789:publication".to_string(),
                "--role".to_string(),
                "publication:matrix-publisher:env:TAISAN_TEST_TOKEN".to_string(),
                "--observe".to_string(),
                "publication:matrix-publisher@https://github.com/0123456789".to_string(),
                "--egress".to_string(),
                "crates.io".to_string(),
            ],
        };
        return setup_output(root, &args);
    }
    let probe = row == "check --probe";
    // The walkthrough's setup sequence: `check` runs against the
    // profile `init` published — seed it identically for both paths.
    let init = vec![
        "init".to_string(),
        "--endpoint".to_string(),
        "https://github.com".to_string(),
        "--repo".to_string(),
        "0123456789:publication".to_string(),
        "--role".to_string(),
        "publication:matrix-publisher:env:TAISAN_TEST_TOKEN".to_string(),
        "--observe".to_string(),
        "publication:matrix-publisher@https://github.com/0123456789".to_string(),
        "--egress".to_string(),
        "crates.io".to_string(),
    ];
    let seeded = setup_output(root, &init);
    assert_eq!(
        seeded.status.code(),
        Some(0),
        "setup seed init succeeds: {}",
        String::from_utf8_lossy(&seeded.stderr)
    );
    let args = match path {
        Path_::Skill => skill_args(
            md,
            "check",
            |a| a.iter().any(|t| t == "--probe") == probe,
            &[],
        ),
        Path_::Direct => {
            let mut args = vec!["check".to_string()];
            if probe {
                args.push("--probe".into());
            }
            args
        }
    };
    setup_output(root, &args)
}

fn scenario(row: &str, root: &Path, path: Path_, md: &str) -> Output {
    match row {
        "init" | "check" | "check --probe" => scenario_setup(row, root, path, md),
        "submit" => {
            let args = match path {
                Path_::Skill => skill_args(
                    md,
                    "submit",
                    |a| a.iter().any(|t| t == "--intent"),
                    &[
                        ("<INTENT>", "matrix fixture intent"),
                        ("<KEY>", "matrix-submit"),
                    ],
                ),
                Path_::Direct => vec![
                    "submit".into(),
                    "--intent".into(),
                    "matrix fixture intent".into(),
                    "--idempotency-key".into(),
                    "matrix-submit".into(),
                ],
            };
            output_at(root, &as_refs(&args))
        }
        "work-list" => {
            submit_at(root, "matrix-list", "matrix list work");
            let args = match path {
                Path_::Skill => skill_args(md, "work-list", |_| true, &[]),
                Path_::Direct => vec!["work-list".into()],
            };
            output_at(root, &as_refs(&args))
        }
        "work-detail" => {
            let env = submit_at(root, "matrix-detail", "matrix detail work");
            let work_id = env["work_id"].as_str().unwrap().to_string();
            let args = match path {
                Path_::Skill => skill_args(md, "work-detail", |_| true, &[("<WORK_ID>", &work_id)]),
                Path_::Direct => vec!["work-detail".into(), "--work".into(), work_id],
            };
            output_at(root, &as_refs(&args))
        }
        "attention" => {
            let env = submit_at(root, "matrix-attn", "matrix attention work");
            let work_id = env["work_id"].as_str().unwrap().to_string();
            let execution_id = env["execution_id"].as_str().unwrap().to_string();
            pause_for_judgment(&store(root), &ExecutionId(execution_id));
            let args = match path {
                Path_::Skill => skill_args(md, "attention", |_| true, &[("<WORK_ID>", &work_id)]),
                Path_::Direct => vec!["attention".into(), "--work".into(), work_id],
            };
            let out = output_at(root, &as_refs(&args));
            let env = envelope(&out);
            assert_eq!(
                env["result"].as_array().map(|a| a.len()),
                Some(1),
                "the work-scoped Attention of the paused judgment is derived, not empty: {env}"
            );
            out
        }
        "capacity" => {
            submit_at(root, "matrix-cap", "matrix capacity work");
            let args = match path {
                Path_::Skill => skill_args(md, "capacity", |_| true, &[]),
                Path_::Direct => vec!["capacity".into()],
            };
            output_at(root, &as_refs(&args))
        }
        "retry" => {
            let env = submit_at(root, "matrix-retry-setup", "matrix retry work");
            let work_id = env["work_id"].as_str().unwrap().to_string();
            let execution_id = env["execution_id"].as_str().unwrap().to_string();
            force_state(
                &store(root),
                &ExecutionId(execution_id.clone()),
                ExecutionState::Failed,
            );
            let args = match path {
                Path_::Skill => skill_args(
                    md,
                    "retry",
                    |_| true,
                    &[
                        ("<WORK_ID>", &work_id),
                        ("<N>", "1"),
                        ("<EXECUTION_ID>", &execution_id),
                        ("<KEY>", "matrix-retry"),
                    ],
                ),
                Path_::Direct => vec![
                    "retry".into(),
                    "--work".into(),
                    work_id,
                    "--expected-version".into(),
                    "1".into(),
                    "--expected-execution".into(),
                    execution_id,
                    "--idempotency-key".into(),
                    "matrix-retry".into(),
                ],
            };
            output_at(root, &as_refs(&args))
        }
        "resume" => {
            let env = submit_at(root, "matrix-resume-setup", "matrix resume work");
            let work_id = env["work_id"].as_str().unwrap().to_string();
            let execution_id = env["execution_id"].as_str().unwrap().to_string();
            pause_for_judgment(&store(root), &ExecutionId(execution_id.clone()));
            let args = match path {
                Path_::Skill => skill_args(
                    md,
                    "resume",
                    |_| true,
                    &[
                        ("<WORK_ID>", &work_id),
                        ("<N>", "1"),
                        ("<EXECUTION_ID>", &execution_id),
                        ("<KEY>", "matrix-resume"),
                    ],
                ),
                Path_::Direct => vec![
                    "resume".into(),
                    "--work".into(),
                    work_id,
                    "--expected-version".into(),
                    "1".into(),
                    "--execution".into(),
                    execution_id,
                    "--idempotency-key".into(),
                    "matrix-resume".into(),
                ],
            };
            output_at(root, &as_refs(&args))
        }
        "cancel" => {
            let env = submit_at(root, "matrix-cancel-setup", "matrix cancel work");
            let work_id = env["work_id"].as_str().unwrap().to_string();
            let args = match path {
                Path_::Skill => skill_args(
                    md,
                    "cancel",
                    |_| true,
                    &[
                        ("<WORK_ID>", &work_id),
                        ("<N>", "1"),
                        ("<KEY>", "matrix-cancel"),
                    ],
                ),
                Path_::Direct => vec![
                    "cancel".into(),
                    "--work".into(),
                    work_id,
                    "--expected-version".into(),
                    "1".into(),
                    "--idempotency-key".into(),
                    "matrix-cancel".into(),
                ],
            };
            output_at(root, &as_refs(&args))
        }
        "acknowledge" => {
            let env = submit_at(root, "matrix-ack-setup", "matrix ack work");
            let work_id = env["work_id"].as_str().unwrap().to_string();
            let execution_id = env["execution_id"].as_str().unwrap().to_string();
            pause_for_judgment(&store(root), &ExecutionId(execution_id));
            let attention = run_at(root, &["attention", "--work", &work_id]);
            let attention_id = attention["result"][0]["attention_id"]
                .as_str()
                .expect("paused judgment derives an Attention version")
                .to_string();
            let args = match path {
                Path_::Skill => skill_args(
                    md,
                    "acknowledge",
                    |_| true,
                    &[
                        ("<WORK_ID>", &work_id),
                        ("<N>", "1"),
                        ("<ATTENTION_ID>", &attention_id),
                        ("<KEY>", "matrix-ack"),
                    ],
                ),
                Path_::Direct => vec![
                    "acknowledge".into(),
                    "--work".into(),
                    work_id,
                    "--expected-version".into(),
                    "1".into(),
                    "--attention".into(),
                    attention_id,
                    "--idempotency-key".into(),
                    "matrix-ack".into(),
                ],
            };
            output_at(root, &as_refs(&args))
        }
        "judge-findings" => {
            let env = submit_at(root, "matrix-judge-setup", "matrix judge work");
            let work_id = env["work_id"].as_str().unwrap().to_string();
            open_change(root, &work_id, "h-matrix");
            seal_review(root, &work_id, "h-matrix", &["a", "b"]);
            let det = work_detail(root, &work_id);
            let occurrences = displayed_occurrences(&det);
            let version = det["result"]["version"].as_u64().unwrap().to_string();
            let args = match path {
                Path_::Skill => {
                    let mut args = skill_args(
                        md,
                        "judge-findings",
                        |a| a.iter().any(|t| t == "--decision"),
                        &[
                            ("<WORK_ID>", &work_id),
                            ("<N>", &version),
                            ("<HEAD>", "h-matrix"),
                            ("<OCCURRENCE_ID>", &occurrences[0]),
                            ("<KEY>", "matrix-judge"),
                        ],
                    );
                    // `--decision` is documented repeatable; the minimal
                    // block carries one, the exact-set judgment needs
                    // every occurrence.
                    args.push("--decision".into());
                    args.push(format!("{}:fix", occurrences[1]));
                    args
                }
                Path_::Direct => vec![
                    "judge-findings".into(),
                    "--work".into(),
                    work_id,
                    "--expected-version".into(),
                    version,
                    "--head".into(),
                    "h-matrix".into(),
                    "--decision".into(),
                    format!("{}:fix", occurrences[0]),
                    "--decision".into(),
                    format!("{}:fix", occurrences[1]),
                    "--idempotency-key".into(),
                    "matrix-judge".into(),
                ],
            };
            output_at(root, &as_refs(&args))
        }
        "release-judgment" => {
            let env = submit_at(root, "matrix-release-setup", "matrix release work");
            let work_id = env["work_id"].as_str().unwrap().to_string();
            let execution_id = env["execution_id"].as_str().unwrap().to_string();
            open_change(root, &work_id, "h-matrix");
            seal_review(root, &work_id, "h-matrix", &["a"]);
            let det = work_detail(root, &work_id);
            let occurrences = displayed_occurrences(&det);
            let version = det["result"]["version"].as_u64().unwrap().to_string();
            run_at(
                root,
                &[
                    "judge-findings",
                    "--work",
                    &work_id,
                    "--expected-version",
                    &version,
                    "--head",
                    "h-matrix",
                    "--decision",
                    &format!("{}:fix", occurrences[0]),
                    "--idempotency-key",
                    "matrix-release-judge",
                ],
            );
            pause_for_judgment(&store(root), &ExecutionId(execution_id));
            let version = work_detail(root, &work_id)["result"]["version"]
                .as_u64()
                .unwrap()
                .to_string();
            let args = match path {
                Path_::Skill => skill_args(
                    md,
                    "release-judgment",
                    |_| true,
                    &[
                        ("<WORK_ID>", &work_id),
                        ("<N>", &version),
                        ("<HEAD>", "h-matrix"),
                        ("<KEY>", "matrix-release"),
                    ],
                ),
                Path_::Direct => vec![
                    "release-judgment".into(),
                    "--work".into(),
                    work_id,
                    "--expected-version".into(),
                    version,
                    "--head".into(),
                    "h-matrix".into(),
                    "--idempotency-key".into(),
                    "matrix-release".into(),
                ],
            };
            output_at(root, &as_refs(&args))
        }
        "command-result" => {
            submit_at(root, "matrix-recover", "matrix recover work");
            let args = match path {
                Path_::Skill => skill_args(
                    md,
                    "command-result",
                    |a| a.iter().any(|t| t == "--origin"),
                    &[("<ORIGIN>", "cli"), ("<KEY>", "matrix-recover")],
                ),
                Path_::Direct => vec![
                    "command-result".into(),
                    "--origin".into(),
                    "cli".into(),
                    "--replay-key".into(),
                    "matrix-recover".into(),
                ],
            };
            output_at(root, &as_refs(&args))
        }
        other => panic!("row {other} is not in APPLICABLE_ROWS"),
    }
}

// ---- The named matrix test ----

#[test]
fn every_applicable_matrix_row_passes_through_the_installed_skill_and_the_direct_cli() {
    let installed_md = install_skill_into_fresh_agent_environment();
    let mut exercised: Vec<&str> = Vec::new();
    for &row in APPLICABLE_ROWS {
        let expected = EXPECTED
            .iter()
            .find(|e| e.row == row)
            .unwrap_or_else(|| panic!("row {row} has no documented expectation"));
        let skill_root = root(&format!("{row}-skill"));
        let direct_root = root(&format!("{row}-direct"));
        let skill = scenario(row, &skill_root, Path_::Skill, &installed_md);
        let direct = scenario(row, &direct_root, Path_::Direct, &installed_md);
        let skill_surface = status_surface(&skill_root, &skill);
        let direct_surface = status_surface(&direct_root, &direct);
        assert_eq!(
            skill_surface, direct_surface,
            "{row}: installed-skill and direct-CLI envelopes differ\nskill: {skill_surface:?}\ndirect: {direct_surface:?}"
        );
        assert_eq!(
            (
                skill_surface.0,
                skill_surface.2.as_str(),
                skill_surface.3.as_str(),
                skill_surface.4.as_str()
            ),
            (
                expected.exit,
                expected.status,
                expected.kind,
                expected.reason
            ),
            "{row}: envelope does not match the documented expectation"
        );
        exercised.push(row);
    }
    assert_eq!(
        exercised, APPLICABLE_ROWS,
        "every applicable matrix row was exercised — no applicable row is uncovered"
    );
}

// ---- Clean skill install (U7.4 spec clarification) ----

/// "Clean" = the shipped `skills/taisan/SKILL.md` installed via the
/// documented path (the `npx skills add` project destination
/// `.agents/skills/taisan/SKILL.md`; the live `npx skills add`
/// transcript is captured in docs/skill-walkthrough.md) into an
/// environment with no prior taisan config or state, then invoked once
/// end-to-end: the installed document's own `submit` block, run against
/// the pristine environment, where the installation record and store
/// are created implicitly on first use.
#[test]
fn a_clean_skill_install_into_a_fresh_environment_invokes_once_end_to_end() {
    let env_root = root("clean-install");
    assert!(
        !env_root.join("config.json").exists() && !env_root.join("workloop.db").exists(),
        "the environment starts with no taisan config or state"
    );

    // Install: the documented project destination, byte-identical to
    // the shipped file.
    let dest = env_root.join(".agents/skills/taisan/SKILL.md");
    std::fs::create_dir_all(dest.parent().expect("skill dir")).unwrap();
    std::fs::copy(shipped_skill(), &dest).expect("shipped skill copies");
    let installed = std::fs::read_to_string(&dest).expect("installed skill reads");
    assert_eq!(installed, skill_md(), "installed skill is the shipped file");

    // Discover + invoke once end-to-end from the installed document.
    let args = skill_args(
        &installed,
        "submit",
        |a| a.iter().any(|t| t == "--intent"),
        &[
            ("<INTENT>", "clean-install end-to-end invocation"),
            ("<KEY>", "clean-install-1"),
        ],
    );
    let db = env_root.join("workloop.db");
    let output = Command::new(bin())
        .arg("--db")
        .arg(&db)
        .args(as_refs(&args))
        .output()
        .expect("taisan runs");
    assert!(
        output.status.success(),
        "end-to-end invocation succeeds: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let env = envelope(&output);
    assert_eq!(env["schema_version"], "1");
    assert_eq!(env["status"], "accepted", "{env}");
    assert_eq!(env["command"]["kind"], "submit", "{env}");
    assert!(
        env["work_id"].as_str().is_some_and(|w| !w.is_empty()),
        "the invocation queued durable Work: {env}"
    );

    // First use created the installation record and the store beside
    // the db — no config or state existed before the invocation.
    assert!(
        env_root.join("workloop.config.json").exists(),
        "implicit installation recorded beside the store"
    );
    assert!(db.exists(), "the store exists after the invocation");
}

// ---- Walkthrough step-6 fresh-key claim ----

/// The walkthrough's recovery leg claims a fresh idempotency key for
/// the same payload is a **new** Command that queues a second Work —
/// never a recovery of the first — and that each retained key
/// recovers its own recorded command through `command-result`.
#[test]
fn a_fresh_key_for_the_same_payload_queues_a_second_work_not_a_recovery() {
    let root = root("fresh-key");
    let first = submit_at(&root, "fresh-key-1", "fresh key payload");
    let second = submit_at(&root, "fresh-key-2", "fresh key payload");
    let first_work = first["work_id"]
        .as_str()
        .expect("first work id")
        .to_string();
    let second_work = second["work_id"]
        .as_str()
        .expect("second work id")
        .to_string();
    assert_ne!(
        first_work, second_work,
        "the fresh key queued a second Work, not a replay: {second}"
    );
    assert_ne!(
        first["command"]["id"], second["command"]["id"],
        "the fresh key is a new Command: {second}"
    );

    // Each retained key recovers its own recorded decision — the
    // original key recovers the original Work, the fresh key the
    // second one.
    for (key, work) in [("fresh-key-1", &first_work), ("fresh-key-2", &second_work)] {
        let recovered = run_at(
            &root,
            &["command-result", "--origin", "cli", "--replay-key", key],
        );
        assert_eq!(recovered["status"], "accepted", "{key}: {recovered}");
        assert_eq!(
            recovered["result"]["work_id"].as_str(),
            Some(work.as_str()),
            "{key} recovers its own queued Work: {recovered}"
        );
    }

    let list = run_at(&root, &["work-list"]);
    assert_eq!(
        list["result"]["items"].as_array().map(|a| a.len()),
        Some(2),
        "two distinct Works are recorded: {list}"
    );
}
