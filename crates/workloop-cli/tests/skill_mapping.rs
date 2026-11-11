//! Agent-skill coverage for the shipped `skills/taisan/SKILL.md`
//! (U7.3): the skill's documented operations are enumerated from the
//! file itself, exercised through the same `taisan` binary an operator
//! invokes directly, and asserted equal on the
//! status/diagnostic surface (exit code, schema_version, status,
//! command kind, rejection reason code — field-level comparison).
//! The skill-path arguments
//! are parsed out of SKILL.md's fenced blocks with placeholder
//! substitution; the direct-path arguments are built literally by the
//! test — two independent constructions of the same invocation.
//!
//! Claims boundary: fixture/contract results only — the real binary
//! driving real Commands and Queries against a real SqliteStore, with
//! prior-history states the Command surface cannot reach on its own
//! (a published change request, a sealed review round, a paused or
//! failed Execution) seeded through the same store the binary opens.
//! `npx skills add` installability is validated structurally
//! (frontmatter, file location, runnable documented invocations); no
//! live registry install and no live host/Worker/E2E scenario was run.

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

/// The documented Command/Query surface of the skill: one entry point
/// per operation, 1:1 with the CLI's workflow operations.
const DOCUMENTED_OPS: &[&str] = &[
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

fn skill_md() -> &'static str {
    include_str!("../../../skills/taisan/SKILL.md")
}

fn bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_workloop-cli"))
}

fn root(tag: &str) -> PathBuf {
    let dir =
        std::env::temp_dir().join(format!("workloop-cli-skill-{}-{}", tag, std::process::id()));
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

fn run_at(root: &Path, args: &[&str]) -> serde_json::Value {
    let output = output_at(root, args);
    assert!(
        output.status.success(),
        "taisan {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    envelope(&output)
}

fn fail_at(root: &Path, args: &[&str]) -> (i32, serde_json::Value) {
    let output = output_at(root, args);
    let code = output.status.code().unwrap_or(-1);
    (code, envelope(&output))
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

fn work_version(root: &Path, work_id: &str) -> String {
    work_detail(root, work_id)["result"]["version"]
        .as_u64()
        .expect("detail carries a numeric version")
        .to_string()
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

// ---- SKILL.md parsing: the skill path is the file itself ----

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

/// The skill-path invocation for `op` selected by `pick`, with every
/// `<PLACEHOLDER>` substituted from `subs`.
fn skill_args(
    md: &str,
    op: &str,
    pick: fn(&[String]) -> bool,
    subs: &[(&str, &str)],
) -> Vec<String> {
    fenced_invocations(md)
        .into_iter()
        .find(|argv| {
            argv.first().map(|s| s == "taisan").unwrap_or(false)
                && argv.get(1).map(|s| s == op).unwrap_or(false)
                && pick(argv)
        })
        .unwrap_or_else(|| panic!("SKILL.md documents a `taisan {op}` invocation"))
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

/// Every `taisan <op>` token anywhere in the document. Inline mentions
/// of documented operations are allowed; the set of operations must not
/// grow beyond DOCUMENTED_OPS.
fn referenced_ops(md: &str) -> Vec<String> {
    let mut ops = Vec::new();
    let mut rest = md;
    while let Some(index) = rest.find("taisan ") {
        let after = &rest[index + "taisan ".len()..];
        let token: String = after
            .chars()
            .take_while(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == '-')
            .collect();
        if !token.is_empty() {
            ops.push(token.clone());
        }
        rest = &after[token.len()..];
    }
    ops
}

fn as_refs(args: &[String]) -> Vec<&str> {
    args.iter().map(String::as_str).collect()
}

/// The status/diagnostic surface of an envelope: what both paths must
/// agree on. Volatile identities (command id, work/execution ids,
/// timestamps) are intentionally excluded.
fn status_surface(output: &Output) -> (i32, String, String, String, String) {
    let code = output.status.code().unwrap_or(-1);
    let env = envelope(output);
    (
        code,
        env["schema_version"].as_str().unwrap_or("").to_string(),
        env["status"].as_str().unwrap_or("").to_string(),
        env["command"]["kind"].as_str().unwrap_or("").to_string(),
        env["reason"]["code"].as_str().unwrap_or("").to_string(),
    )
}

#[derive(Clone, Copy, PartialEq)]
enum Path_ {
    Skill,
    Direct,
}

/// Run `scenario` twice — once with skill-path args parsed out of
/// SKILL.md, once with hand-built direct-CLI args — against identical
/// seeded state, and assert the status/diagnostic surfaces agree.
fn assert_paths_agree(tag: &str, scenario: fn(&Path, Path_) -> Output) {
    let skill_root = root(&format!("{tag}-skill"));
    let direct_root = root(&format!("{tag}-direct"));
    let skill = scenario(&skill_root, Path_::Skill);
    let direct = scenario(&direct_root, Path_::Direct);
    let skill_surface = status_surface(&skill);
    let direct_surface = status_surface(&direct);
    assert_eq!(
        skill_surface, direct_surface,
        "{tag}: skill-path and direct-CLI envelopes differ"
    );
    assert_eq!(
        skill_surface.0, 0,
        "{tag}: both paths must exit 0; skill-path envelope: {:?}",
        skill_surface
    );
    assert_eq!(
        skill_surface.2, "accepted",
        "{tag}: these scenarios are seeded to be accepted; skill-path envelope: {:?}",
        skill_surface
    );
}

// ---- INV-001 / INV-005: static check of the shipped skill ----

#[test]
fn skill_references_only_documented_operations() {
    let md = skill_md();

    assert!(md.starts_with("---\n"), "SKILL.md opens with frontmatter");
    let frontmatter = md[4..].split_once("\n---").expect("frontmatter closes").0;
    assert!(frontmatter.contains("name: taisan"), "{frontmatter}");
    assert!(
        frontmatter.contains("description:") && frontmatter.contains("npx skills add"),
        "{frontmatter}"
    );

    let referenced = referenced_ops(md);
    assert!(
        !referenced.is_empty(),
        "SKILL.md references taisan operations"
    );
    for op in &referenced {
        assert!(
            DOCUMENTED_OPS.contains(&op.as_str()),
            "SKILL.md references undocumented operation `{op}`; documented set: {DOCUMENTED_OPS:?}"
        );
    }

    for op in DOCUMENTED_OPS {
        assert!(
            referenced.iter().any(|r| r == op),
            "SKILL.md never references documented operation `{op}`"
        );
        assert!(
            fenced_invocations(md)
                .iter()
                .any(|argv| argv.get(1).map(|s| s == op).unwrap_or(false)),
            "SKILL.md carries no runnable `taisan {op}` block"
        );
    }
}
// ---- The twelve documented operations, both paths ----

fn skill_submit(root: &Path, path: Path_, key: &str) -> Output {
    let md = skill_md();
    let args = match path {
        Path_::Skill => skill_args(
            md,
            "submit",
            |a| a.iter().any(|t| t == "--intent"),
            &[("<INTENT>", "skill fixture intent"), ("<KEY>", key)],
        ),
        Path_::Direct => vec![
            "submit".into(),
            "--intent".into(),
            "skill fixture intent".into(),
            "--idempotency-key".into(),
            key.into(),
        ],
    };
    output_at(root, &as_refs(&args))
}

fn scenario_submit(root: &Path, path: Path_) -> Output {
    skill_submit(root, path, "skill-eq-submit")
}

fn scenario_work_list(root: &Path, path: Path_) -> Output {
    submit_at(root, "skill-eq-list", "list work");
    let md = skill_md();
    let args = match path {
        Path_::Skill => skill_args(md, "work-list", |_| true, &[]),
        Path_::Direct => vec!["work-list".into()],
    };
    output_at(root, &as_refs(&args))
}

fn scenario_work_detail(root: &Path, path: Path_) -> Output {
    let env = submit_at(root, "skill-eq-detail", "detail work");
    let work_id = env["work_id"].as_str().unwrap().to_string();
    let md = skill_md();
    let args = match path {
        Path_::Skill => skill_args(md, "work-detail", |_| true, &[("<WORK_ID>", &work_id)]),
        Path_::Direct => vec!["work-detail".into(), "--work".into(), work_id],
    };
    output_at(root, &as_refs(&args))
}

fn scenario_attention(root: &Path, path: Path_) -> Output {
    let env = submit_at(root, "skill-eq-attn", "attention work");
    let work_id = env["work_id"].as_str().unwrap().to_string();
    let md = skill_md();
    let args = match path {
        Path_::Skill => skill_args(md, "attention", |_| true, &[("<WORK_ID>", &work_id)]),
        Path_::Direct => vec!["attention".into(), "--work".into(), work_id],
    };
    output_at(root, &as_refs(&args))
}

fn scenario_capacity(root: &Path, path: Path_) -> Output {
    submit_at(root, "skill-eq-cap", "capacity work");
    let md = skill_md();
    let args = match path {
        Path_::Skill => skill_args(md, "capacity", |_| true, &[]),
        Path_::Direct => vec!["capacity".into()],
    };
    output_at(root, &as_refs(&args))
}

fn scenario_retry(root: &Path, path: Path_) -> Output {
    let env = submit_at(root, "skill-eq-retry-setup", "retry work");
    let work_id = env["work_id"].as_str().unwrap().to_string();
    let execution_id = env["execution_id"].as_str().unwrap().to_string();
    force_state(
        &store(root),
        &ExecutionId(execution_id.clone()),
        ExecutionState::Failed,
    );
    let md = skill_md();
    let args = match path {
        Path_::Skill => skill_args(
            md,
            "retry",
            |_| true,
            &[
                ("<WORK_ID>", &work_id),
                ("<N>", "1"),
                ("<EXECUTION_ID>", &execution_id),
                ("<KEY>", "skill-eq-retry"),
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
            "skill-eq-retry".into(),
        ],
    };
    output_at(root, &as_refs(&args))
}

fn scenario_resume(root: &Path, path: Path_) -> Output {
    let env = submit_at(root, "skill-eq-resume-setup", "resume work");
    let work_id = env["work_id"].as_str().unwrap().to_string();
    let execution_id = env["execution_id"].as_str().unwrap().to_string();
    pause_for_judgment(&store(root), &ExecutionId(execution_id.clone()));
    let md = skill_md();
    let args = match path {
        Path_::Skill => skill_args(
            md,
            "resume",
            |_| true,
            &[
                ("<WORK_ID>", &work_id),
                ("<N>", "1"),
                ("<EXECUTION_ID>", &execution_id),
                ("<KEY>", "skill-eq-resume"),
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
            "skill-eq-resume".into(),
        ],
    };
    output_at(root, &as_refs(&args))
}

fn scenario_cancel(root: &Path, path: Path_) -> Output {
    let env = submit_at(root, "skill-eq-cancel-setup", "cancel work");
    let work_id = env["work_id"].as_str().unwrap().to_string();
    let md = skill_md();
    let args = match path {
        Path_::Skill => skill_args(
            md,
            "cancel",
            |_| true,
            &[
                ("<WORK_ID>", &work_id),
                ("<N>", "1"),
                ("<KEY>", "skill-eq-cancel"),
            ],
        ),
        Path_::Direct => vec![
            "cancel".into(),
            "--work".into(),
            work_id,
            "--expected-version".into(),
            "1".into(),
            "--idempotency-key".into(),
            "skill-eq-cancel".into(),
        ],
    };
    output_at(root, &as_refs(&args))
}

fn scenario_acknowledge(root: &Path, path: Path_) -> Output {
    let env = submit_at(root, "skill-eq-ack-setup", "ack work");
    let work_id = env["work_id"].as_str().unwrap().to_string();
    let execution_id = env["execution_id"].as_str().unwrap().to_string();
    pause_for_judgment(&store(root), &ExecutionId(execution_id.clone()));
    let attention = run_at(root, &["attention", "--work", &work_id]);
    let attention_id = attention["result"][0]["attention_id"]
        .as_str()
        .expect("paused judgment derives an Attention version")
        .to_string();
    let md = skill_md();
    let args = match path {
        Path_::Skill => skill_args(
            md,
            "acknowledge",
            |_| true,
            &[
                ("<WORK_ID>", &work_id),
                ("<N>", "1"),
                ("<ATTENTION_ID>", &attention_id),
                ("<KEY>", "skill-eq-ack"),
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
            "skill-eq-ack".into(),
        ],
    };
    output_at(root, &as_refs(&args))
}

fn scenario_judge_findings(root: &Path, path: Path_) -> Output {
    let env = submit_at(root, "skill-eq-judge-setup", "judge work");
    let work_id = env["work_id"].as_str().unwrap().to_string();
    open_change(root, &work_id, "h-eq");
    seal_review(root, &work_id, "h-eq", &["a", "b"]);
    let det = work_detail(root, &work_id);
    let occurrences = displayed_occurrences(&det);
    let version = det["result"]["version"].as_u64().unwrap().to_string();
    let md = skill_md();
    let args = match path {
        Path_::Skill => {
            let mut args = skill_args(
                md,
                "judge-findings",
                |a| a.iter().any(|t| t == "--decision"),
                &[
                    ("<WORK_ID>", &work_id),
                    ("<N>", &version),
                    ("<HEAD>", "h-eq"),
                    ("<OCCURRENCE_ID>", &occurrences[0]),
                    ("<KEY>", "skill-eq-judge"),
                ],
            );
            // `--decision` is documented repeatable; the minimal block
            // carries one, the exact-set judgment needs every occurrence.
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
            "h-eq".into(),
            "--decision".into(),
            format!("{}:fix", occurrences[0]),
            "--decision".into(),
            format!("{}:fix", occurrences[1]),
            "--idempotency-key".into(),
            "skill-eq-judge".into(),
        ],
    };
    output_at(root, &as_refs(&args))
}

fn scenario_release_judgment(root: &Path, path: Path_) -> Output {
    let env = submit_at(root, "skill-eq-release-setup", "release work");
    let work_id = env["work_id"].as_str().unwrap().to_string();
    let execution_id = env["execution_id"].as_str().unwrap().to_string();
    open_change(root, &work_id, "h-eq");
    seal_review(root, &work_id, "h-eq", &["a"]);
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
            "h-eq",
            "--decision",
            &format!("{}:fix", occurrences[0]),
            "--idempotency-key",
            "skill-eq-release-judge",
        ],
    );
    pause_for_judgment(&store(root), &ExecutionId(execution_id));
    let version = work_version(root, &work_id);
    let md = skill_md();
    let args = match path {
        Path_::Skill => skill_args(
            md,
            "release-judgment",
            |_| true,
            &[
                ("<WORK_ID>", &work_id),
                ("<N>", &version),
                ("<HEAD>", "h-eq"),
                ("<KEY>", "skill-eq-release"),
            ],
        ),
        Path_::Direct => vec![
            "release-judgment".into(),
            "--work".into(),
            work_id,
            "--expected-version".into(),
            version,
            "--head".into(),
            "h-eq".into(),
            "--idempotency-key".into(),
            "skill-eq-release".into(),
        ],
    };
    output_at(root, &as_refs(&args))
}

fn scenario_command_result(root: &Path, path: Path_) -> Output {
    submit_at(root, "skill-eq-recover", "recover work");
    let md = skill_md();
    let args = match path {
        Path_::Skill => skill_args(
            md,
            "command-result",
            |a| a.iter().any(|t| t == "--origin"),
            &[("<ORIGIN>", "cli"), ("<KEY>", "skill-eq-recover")],
        ),
        Path_::Direct => vec![
            "command-result".into(),
            "--origin".into(),
            "cli".into(),
            "--replay-key".into(),
            "skill-eq-recover".into(),
        ],
    };
    output_at(root, &as_refs(&args))
}

#[test]
fn every_documented_operation_maps_one_to_one_through_both_paths() {
    assert_paths_agree("submit", scenario_submit);
    assert_paths_agree("work-list", scenario_work_list);
    assert_paths_agree("work-detail", scenario_work_detail);
    assert_paths_agree("attention", scenario_attention);
    assert_paths_agree("capacity", scenario_capacity);
    assert_paths_agree("retry", scenario_retry);
    assert_paths_agree("resume", scenario_resume);
    assert_paths_agree("cancel", scenario_cancel);
    assert_paths_agree("acknowledge", scenario_acknowledge);
    assert_paths_agree("judge-findings", scenario_judge_findings);
    assert_paths_agree("release-judgment", scenario_release_judgment);
    assert_paths_agree("command-result", scenario_command_result);
}

// ---- INV-003: replay identity through the skill ----

#[test]
fn replay_identity_is_preserved_through_the_skill() {
    let root = root("replay");
    let md = skill_md();
    let args = skill_args(
        md,
        "submit",
        |a| a.iter().any(|t| t == "--intent"),
        &[
            ("<INTENT>", "skill replay intent"),
            ("<KEY>", "skill-replay-1"),
        ],
    );
    let first = run_at(&root, &as_refs(&args));
    let second = run_at(&root, &as_refs(&args));
    assert_eq!(first["status"], "accepted");
    // Identical replay returns the recorded decision bound to the
    // original identity — the same command id, the same Work.
    assert_eq!(second["command"]["id"], first["command"]["id"]);
    assert_eq!(second["work_id"], first["work_id"]);
    assert_eq!(second["execution_id"], first["execution_id"]);
    let list = run_at(&root, &["work-list"]);
    assert_eq!(list["result"]["items"].as_array().unwrap().len(), 1);

    // The same Invocation through the direct CLI on a fresh
    // installation produces the same request identity surface:
    // accepted, kind submit, the caller's idempotency key echoed.
    let direct_root = self::root("replay-direct");
    let direct = run_at(
        &direct_root,
        &[
            "submit",
            "--intent",
            "skill replay intent",
            "--idempotency-key",
            "skill-replay-1",
        ],
    );
    assert_eq!(direct["status"], "accepted");
    assert_eq!(direct["command"]["kind"], first["command"]["kind"]);
    assert_eq!(
        direct["command"]["idempotency_key"],
        first["command"]["idempotency_key"]
    );
    assert_eq!(direct["result"]["kind"], first["result"]["kind"]);
}

// ---- INV-004: unknown-result recovery through the skill ----

#[test]
fn a_mid_operation_crash_recovers_identically_through_the_skill() {
    let root = root("crash");
    let md = skill_md();
    let args = skill_args(
        md,
        "submit",
        |a| a.iter().any(|t| t == "--intent"),
        &[
            ("<INTENT>", "skill crash intent"),
            ("<KEY>", "skill-crash-1"),
        ],
    );

    // The operation runs but the response is lost (the unknown-result
    // case): the agent crashed or the connection dropped after the
    // store committed and before the envelope arrived. Nothing in the
    // recovery path below may depend on having read it.
    let output = output_at(&root, &as_refs(&args));

    // Recovery via the skill's documented reconciliation — the original
    // (origin, actor, idempotency_key), byte-identical payload.
    let by_key = skill_args(
        md,
        "command-result",
        |a| a.iter().any(|t| t == "--origin"),
        &[("<ORIGIN>", "cli"), ("<KEY>", "skill-crash-1")],
    );
    let recovered = run_at(&root, &as_refs(&by_key));
    assert_eq!(recovered["status"], "accepted");
    let command_id = recovered["result"]["command_id"]
        .as_str()
        .expect("recovery returns the recorded command id")
        .to_string();

    // The direct-CLI recovery of the same identity agrees.
    let direct = run_at(
        &root,
        &[
            "command-result",
            "--origin",
            "cli",
            "--replay-key",
            "skill-crash-1",
        ],
    );
    assert_eq!(direct["result"]["command_id"], command_id);
    assert_eq!(
        direct["result"]["command_id"],
        recovered["result"]["command_id"]
    );

    // Identical replay of the crashed command returns the stored
    // decision — never a second mutation.
    let replay = run_at(&root, &as_refs(&args));
    assert_eq!(replay["command"]["id"], command_id);
    let list = run_at(&root, &["work-list"]);
    assert_eq!(list["result"]["items"].as_array().unwrap().len(), 1);

    // Post-hoc ground truth from the lost bytes, decoded only after
    // recovery completed: the recovered identity is the identity that
    // was committed.
    let original = envelope(&output);
    assert_eq!(original["status"], "accepted");
    assert_eq!(original["command"]["id"], command_id);

    // An unknown result never justifies a fresh key.
    let (code, env) = fail_at(
        &root,
        &[
            "submit",
            "--intent",
            "a different intent",
            "--idempotency-key",
            "skill-crash-1",
        ],
    );
    assert_eq!(code, 1, "{env}");
    assert_eq!(env["reason"]["code"], "idempotency_conflict");
}
