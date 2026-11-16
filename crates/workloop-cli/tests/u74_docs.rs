//! U7.4 documentation and evidence checks:
//!
//! - the walkthrough doc check asserts the walkthrough carries one
//!   section per step (setup → install → submit → inspect →
//!   Attention/decision → recovery) and that every step section holds
//!   verbatim captured CLI output (a fenced transcript with the
//!   command echo and the JSON envelope);
//! - the evidence-label check asserts every matrix result row in the
//!   U7.4 evidence record is labeled `fixture` or `live` — no
//!   unlabeled result exists — and that the record records the
//!   exercised versions/config and distinguishes fixture evidence from
//!   live U8 evidence;
//! - the evidence-revision check asserts the record cites a commit
//!   that contains the exercised slice tree (slice files byte-
//!   identical to the exercised working tree, on the branch history
//!   to `HEAD`), or the explicit `absent{reason}` gap — and the gap
//!   form is rejected as soon as such a containing commit exists, so
//!   the commit stamp becomes mandatory the moment the slice lands;
//!   base revision and slice digests are retained as supplements.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// The walkthrough sequence mandated by the U7.4 scope: one section
/// per step, each carrying captured CLI output.
const STEPS: &[&str] = &[
    "setup",
    "install",
    "submit",
    "inspect",
    "attention/decision",
    "recovery",
];

/// The applicable matrix rows, mirrored from `u74_matrix.rs`'s
/// `APPLICABLE_ROWS`: every Command/Query row of the CLI contract
/// needed by the agent-interface workflow table. The evidence record
/// must carry exactly this set — twice each (installed skill and
/// direct CLI) — and every row labeled.
const MATRIX_ROWS: &[&str] = &[
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

const EXERCISED_SLICE_PATHS: &[&str] = &[
    "docs/skill-walkthrough.md",
    "skills/taisan/SKILL.md",
    "crates/workloop-cli/tests/u74_matrix.rs",
    "crates/workloop-cli/tests/u74_docs.rs",
    "crates/workloop-cli/tests/skill_mapping.rs",
];

/// `## `-level sections (not `###`): (heading text, body text).
fn sections(doc: &str) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    let mut current: Option<(String, String)> = None;
    for line in doc.lines() {
        if line.starts_with("## ") && !line.starts_with("### ") {
            if let Some(done) = current.take() {
                out.push(done);
            }
            current = Some((line[3..].trim().to_string(), String::new()));
        } else if let Some(entry) = current.as_mut() {
            entry.1.push_str(line);
            entry.1.push('\n');
        }
    }
    if let Some(done) = current {
        out.push(done);
    }
    out
}

/// Fenced code blocks inside `body`.
fn fenced_blocks(body: &str) -> Vec<String> {
    let mut blocks = Vec::new();
    let mut in_fence = false;
    let mut current = String::new();
    for line in body.lines() {
        if line.trim_start().starts_with("```") {
            if in_fence {
                blocks.push(current.clone());
                current.clear();
            }
            in_fence = !in_fence;
            continue;
        }
        if in_fence {
            current.push_str(line);
            current.push('\n');
        }
    }
    blocks
}

/// A captured transcript block: the `$ …` command echo of a `taisan`
/// invocation and the verbatim JSON envelope it printed on stdout.
fn is_captured_cli_output(block: &str) -> bool {
    block.contains("\"schema_version\"")
        && block
            .lines()
            .any(|line| line.starts_with("$ ") && line.contains("taisan"))
}

/// Content pins per walkthrough step: each group of substrings must
/// co-occur in at least one fenced transcript block of that step's
/// section. A section that merely carries *some* captured output (the
/// recovery leg's `run --once`, for example) does not satisfy a pin
/// whose transcripts were deleted, and a group of markers inside a
/// single block catches an envelope whose fields were rewritten —
/// presence of the markers somewhere in the section is not enough.
const STEP_TRANSCRIPT_PINS: &[(&str, &[&[&str]])] = &[
    (
        "setup",
        &[
            &[
                "$ taisan init --endpoint",
                "\"kind\": \"init\"",
                "\"status\": \"accepted\"",
            ],
            &[
                "$ taisan check",
                "\"status\": \"not_run\"",
                "\"success\": true",
            ],
            &[
                "$ taisan check --probe",
                "\"stage\": \"probe_capabilities\"",
                "\"status\": \"fail\"",
                "\"code\": \"config\"",
            ],
        ],
    ),
    (
        "install",
        &[
            &[
                "npx -y skills add",
                "\"status\": \"installed\"",
                "\"scope\": \"project\"",
                ".agents/skills/taisan",
            ],
            &["$ ls -la", "SKILL.md"],
            &[
                "--idempotency-key clean-install-1",
                "\"kind\": \"submit\"",
                "\"work_id\": \"w-1\"",
            ],
        ],
    ),
    (
        "submit",
        &[
            &[
                "--idempotency-key walkthrough-submit-1",
                "\"kind\": \"submit\"",
                "\"work_created\"",
            ],
            &[
                "--idempotency-key walkthrough-submit-1",
                "\"id\": \"cmd-1\"",
                "\"work_id\": \"w-1\"",
            ],
        ],
    ),
    (
        "inspect",
        &[
            &["work-list", "\"kind\": \"work_list\"", "\"items\""],
            &[
                "work-detail --work w-1",
                "\"kind\": \"work_detail\"",
                "\"work_id\": \"w-1\"",
            ],
            &["attention", "\"kind\": \"attention\"", "\"result\": []"],
            &["capacity", "\"kind\": \"capacity\"", "\"limit\": 1"],
        ],
    ),
    (
        "attention/decision",
        &[
            &["attention --work w-1", "\"work_id\": \"w-1\""],
            &[
                "cancel --work w-1 --expected-version 99",
                "\"status\": \"rejected\"",
                "\"code\": \"stale_work_version\"",
            ],
            &[
                "acknowledge --work w-1",
                "\"status\": \"rejected\"",
                "\"code\": \"ineligible\"",
            ],
            &[
                "judge-findings --work w-1",
                "\"status\": \"rejected\"",
                "\"code\": \"source_unavailable\"",
            ],
        ],
    ),
    (
        "recovery",
        &[
            &[
                "submit --intent \"Walkthrough: recovery leg\" --idempotency-key walkthrough-recover-1 > /dev/null",
            ],
            &[
                "command-result --origin cli --replay-key walkthrough-recover-1",
                "\"command_id\": \"cmd-5\"",
                "\"work_id\": \"w-2\"",
            ],
            &[
                "submit --intent \"Walkthrough: recovery leg\" --idempotency-key walkthrough-recover-1",
                "\"id\": \"cmd-5\"",
                "\"work_id\": \"w-2\"",
            ],
            &[
                "--idempotency-key walkthrough-recover-2",
                "\"id\": \"cmd-6\"",
                "\"work_id\": \"w-3\"",
            ],
            &["run --once", "\"kind\": \"run\""],
        ],
    ),
];

/// Assert one step section carries its required transcripts: at least
/// one captured `taisan` output block, and every content pin of the
/// step satisfied by a single fenced block carrying all of the pin's
/// markers together.
fn assert_step_section_carries_its_transcripts(step: &str, heading: &str, body: &str) {
    let blocks = fenced_blocks(body);
    let captured: Vec<&String> = blocks
        .iter()
        .filter(|b| is_captured_cli_output(b))
        .collect();
    assert!(
        !captured.is_empty(),
        "step section {heading:?} carries no verbatim captured CLI output \
         (a fenced block with the `$ taisan` echo and the JSON envelope)"
    );
    let pins = STEP_TRANSCRIPT_PINS
        .iter()
        .find(|(s, _)| *s == step)
        .unwrap_or_else(|| panic!("step {step:?} declares content pins"))
        .1;
    for group in pins {
        assert!(
            blocks.iter().any(|b| group.iter().all(|m| b.contains(m))),
            "step section {heading:?} carries no single fenced block with all of \
             the required transcript markers {group:?} — the transcript was \
             deleted or rewritten"
        );
    }
}

// ---- Walkthrough doc check ----

#[test]
fn walkthrough_doc_check_every_step_section_carries_verbatim_captured_cli_output() {
    let doc = include_str!("../../../docs/skill-walkthrough.md");
    let secs = sections(doc);
    for step in STEPS {
        let needle = step.to_ascii_lowercase();
        let hits: Vec<&(String, String)> = secs
            .iter()
            .filter(|(heading, _)| heading.to_ascii_lowercase().contains(&needle))
            .collect();
        assert_eq!(
            hits.len(),
            1,
            "walkthrough has exactly one `##` section for step {step:?}; found headings: {:?}",
            secs.iter().map(|(h, _)| h).collect::<Vec<_>>()
        );
        let (heading, body) = hits[0];
        assert_step_section_carries_its_transcripts(step, heading, body);
    }
    assert!(
        doc.contains("verbatim"),
        "the walkthrough states its output is captured verbatim"
    );
}

/// The finding scenario, as a fixture: a recovery section whose
/// fresh-key and `command-result` transcripts were deleted (leaving
/// only the `run --once` transcript) or whose recovered `work_id` was
/// rewritten must be rejected by the content pins — the real document
/// passes them, a doctored one does not.
#[test]
fn walkthrough_content_pins_reject_deleted_or_rewritten_recovery_transcripts() {
    let doc = include_str!("../../../docs/skill-walkthrough.md");
    let secs = sections(doc);
    let (heading, intact) = secs
        .iter()
        .find(|(h, _)| h.to_ascii_lowercase().contains("recovery"))
        .expect("walkthrough has a recovery section")
        .clone();
    assert_step_section_carries_its_transcripts("recovery", &heading, &intact);

    // Delete the fresh-key block and the command-result block, keeping
    // the `run --once` transcript — the section still carries captured
    // CLI output, but the pins must reject it.
    let mut doctored = intact.clone();
    for start in [
        "command-result --origin cli --replay-key walkthrough-recover-1",
        "--idempotency-key walkthrough-recover-2",
    ] {
        let begin = doctored
            .find(start)
            .expect("the intact section carries the transcript");
        let fence = doctored[..begin]
            .rfind("```")
            .expect("the transcript sits in a fenced block");
        let end = doctored[begin..]
            .find("```")
            .map(|i| begin + i + 3)
            .expect("the fenced block closes");
        doctored.replace_range(fence..end, "");
    }
    let doctored = doctored;
    std::panic::catch_unwind(|| {
        assert_step_section_carries_its_transcripts("recovery", &heading, &doctored)
    })
    .expect_err("deleted recovery transcripts must fail the content pins");

    // Rewrite the recovered `work_id` in the identical-replay
    // transcript — the `submit` echo that is directly followed by an
    // envelope (the lost-response echo of the same key is redirected
    // to /dev/null, and the command-result echo is a different
    // command) — every marker still exists somewhere in the section,
    // but no single block carries the replay group any more.
    let mut rewritten = intact.clone();
    let replay = rewritten
        .find("submit --intent \"Walkthrough: recovery leg\" --idempotency-key walkthrough-recover-1\n{")
        .expect("the intact section carries the replay transcript");
    let work = rewritten[replay..]
        .find("\"work_id\": \"w-2\"")
        .map(|i| replay + i)
        .expect("the replay envelope carries the recovered work id");
    rewritten.replace_range(
        work..work + "\"work_id\": \"w-2\"".len(),
        "\"work_id\": \"w-9\"",
    );
    let rewritten = rewritten;
    std::panic::catch_unwind(|| {
        assert_step_section_carries_its_transcripts("recovery", &heading, &rewritten)
    })
    .expect_err("a rewritten recovered work_id must fail the content pins");
}

// ---- Evidence label check ----

/// Data rows of the `## Matrix results` table: (row, path, class,
/// result) cells, header and separator skipped.
fn matrix_result_rows(doc: &str) -> Vec<Vec<String>> {
    let mut rows = Vec::new();
    let mut in_table = false;
    let mut seen_heading = false;
    for line in doc.lines() {
        if line.starts_with("## ") && !line.starts_with("### ") {
            if seen_heading {
                break;
            }
            seen_heading = line.contains("Matrix results");
            continue;
        }
        if !seen_heading {
            continue;
        }
        if !line.starts_with('|') {
            if in_table && !line.trim().is_empty() {
                break;
            }
            continue;
        }
        let cells: Vec<String> = line
            .trim()
            .trim_start_matches('|')
            .trim_end_matches('|')
            .split('|')
            .map(|c| c.trim().to_string())
            .collect();
        if !in_table {
            in_table = true;
            continue; // header row
        }
        if cells
            .first()
            .is_some_and(|c| c.chars().all(|ch| ch == '-' || ch == ' ' || ch == ':'))
        {
            continue; // separator row
        }
        rows.push(cells);
    }
    rows
}

#[test]
fn evidence_labels_every_matrix_result_fixture_or_live() {
    let doc = include_str!("../../../docs/evidence/u7.4.md");

    let rows = matrix_result_rows(doc);
    assert_eq!(
        rows.len(),
        MATRIX_ROWS.len() * 2,
        "the matrix-results table carries one row per applicable matrix row per path"
    );

    let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
    for cells in &rows {
        assert_eq!(
            cells.len(),
            4,
            "matrix result rows are `| row | path | class | result |`: {cells:?}"
        );
        let (row, path, class) = (cells[0].as_str(), cells[1].as_str(), cells[2].as_str());
        assert!(
            MATRIX_ROWS.contains(&row),
            "matrix result row {row:?} is not an applicable matrix row"
        );
        assert!(
            matches!(path, "installed skill" | "direct CLI"),
            "matrix result row {row:?} must name its path (installed skill | direct CLI), got {path:?}"
        );
        assert!(
            matches!(class, "fixture" | "live"),
            "matrix result row {row:?} ({path}) is unlabeled: class must be `fixture` or `live`, got {class:?}"
        );
        *counts.entry(row).or_insert(0) += 1;
    }
    for row in MATRIX_ROWS {
        assert_eq!(
            counts.get(row),
            Some(&2),
            "matrix row {row:?} must appear exactly twice (installed skill and direct CLI)"
        );
    }

    // Exercised versions/config recorded, and fixture evidence
    // distinguished from live U8 evidence (no live claim here).
    assert!(
        doc.contains("Tested revision"),
        "the evidence record names the exercised versions"
    );
    assert!(
        doc.contains("Configuration"),
        "the evidence record names the exercised configuration"
    );
    let lower = doc.to_ascii_lowercase();
    assert!(
        lower.contains("fixture") && lower.contains("live") && lower.contains("u8"),
        "the evidence record distinguishes fixture evidence from live U8 evidence"
    );
}

// ---- Evidence revision check ----

fn first_backticked(row: &str) -> Option<&str> {
    row.split('`').nth(1)
}

fn is_full_commit_id(token: &str) -> bool {
    token.len() == 40 && token.chars().all(|c| c.is_ascii_hexdigit())
}

fn is_sha256(token: &str) -> bool {
    token.len() == 64 && token.chars().all(|c| c.is_ascii_hexdigit())
}

fn git_succeeds(root: &Path, args: &[&str]) -> bool {
    std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .map(|out| out.status.success())
        .unwrap_or(false)
}

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// The exercised slice tree of `rev`: every exercised slice file
/// present at `rev` and byte-identical to the working tree — the tree
/// the evidence record's digests describe and the tests exercise.
fn slice_content_matches_worktree(root: &Path, rev: &str) -> bool {
    let mut args = vec!["diff", "--quiet", rev, "--"];
    args.extend(EXERCISED_SLICE_PATHS.iter().copied());
    git_succeeds(root, &args)
}

/// The newest commit on the branch history to `HEAD` whose exercised
/// slice files are byte-identical to the working tree — a commit that
/// contains the exercised slice tree — or `None` while no such commit
/// exists. Candidates are the commits that touch any exercised slice
/// path: if some ancestor's slice content matches the working tree,
/// the latest toucher across the five paths matches too, so the
/// path-touching candidates are complete.
fn commit_containing_exercised_slice(root: &Path) -> Option<String> {
    let mut cmd = std::process::Command::new("git");
    cmd.arg("-C")
        .arg(root)
        .args(["log", "--format=%H", "HEAD", "--"])
        .args(EXERCISED_SLICE_PATHS);
    let out = cmd.output().ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .find(|candidate| slice_content_matches_worktree(root, candidate))
        .map(str::to_string)
}

/// The record's own tested-revision stamp: either a full commit id
/// that contains the exercised slice tree (slice files byte-identical
/// to the exercised working tree) on the branch history to `HEAD`, or
/// the explicit `absent{reason}` gap — and the gap is accepted only
/// while no commit on that history contains the exercised slice tree.
/// Once the slice lands, the containing-commit form becomes
/// mandatory: a base SHA that does not contain the slice never
/// satisfies the stamp, and the retained sha256 digests supplement
/// but cannot replace the commit stamp.
fn slice_evidence(root: &Path, record: &str) -> Result<(), String> {
    let row = record
        .lines()
        .find(|line| line.contains("Tested revision (slice commit"))
        .ok_or("the record carries no Tested revision (slice commit…) row")?;
    let cell = first_backticked(row).ok_or("the tested revision cell is backticked")?;

    if cell.starts_with("absent{") && cell.ends_with('}') {
        let reason = &cell["absent{".len()..cell.len() - 1];
        if !reason.contains("commit") {
            return Err(format!(
                "the tested-revision gap reason must explain the missing commit: {cell}"
            ));
        }
        if cell.split_whitespace().any(is_full_commit_id) {
            return Err("an absent tested-revision cell must not carry any commit id".to_string());
        }
        let base_row = record
            .lines()
            .find(|line| line.contains("Base revision"))
            .ok_or("the record carries a base revision row")?;
        let base = first_backticked(base_row).ok_or("the base revision cell is backticked")?;
        if !is_full_commit_id(base) {
            return Err(format!("base revision must be a full commit id: {base}"));
        }
        for slice in EXERCISED_SLICE_PATHS {
            let digested = record
                .lines()
                .any(|line| line.contains(slice) && line.split('`').any(is_sha256));
            if !digested {
                return Err(format!(
                    "the exercised slice must retain a sha256 digest for {slice}"
                ));
            }
        }
        // The gap is a pre-landing form only: the moment a commit on
        // the branch history contains the exercised slice tree, the
        // record must cite that commit instead — an unstamped record
        // must not stay green after the slice lands.
        if let Some(commit) = commit_containing_exercised_slice(root) {
            return Err(format!(
                "commit {commit} on the branch history to HEAD contains the exercised slice tree; \
                 the tested-revision cell must cite that commit instead of the absent{{…}} gap"
            ));
        }
        return Ok(());
    }

    if !is_full_commit_id(cell) {
        return Err(format!(
            "the tested revision must be a full 40-hex commit id or the explicit absent{{…}} gap: {cell}"
        ));
    }
    let commit = format!("{cell}^{{commit}}");
    if !git_succeeds(root, &["cat-file", "-e", &commit]) {
        return Err(format!(
            "tested revision {cell} is not present in this checkout's object database"
        ));
    }
    for slice in EXERCISED_SLICE_PATHS {
        let spec = format!("{cell}:{slice}");
        if !git_succeeds(root, &["cat-file", "-e", &spec]) {
            return Err(format!(
                "tested revision {cell} must contain the exercised slice file {slice}"
            ));
        }
    }
    if !slice_content_matches_worktree(root, cell) {
        return Err(format!(
            "tested revision {cell} does not contain the exercised slice tree: its slice files \
             must be byte-identical to the exercised working tree"
        ));
    }
    if !git_succeeds(root, &["merge-base", "--is-ancestor", cell, "HEAD"]) {
        return Err(format!(
            "tested revision {cell} must be on the branch history leading to HEAD"
        ));
    }
    Ok(())
}

#[test]
fn evidence_record_cites_the_tested_revision_or_the_explicit_gap() {
    let record = include_str!("../../../docs/evidence/u7.4.md");
    let root = repo_root();
    slice_evidence(&root, record)
        .expect("the record cites a containing commit or the explicit tested-revision gap");
}

// ---- The gap form is pre-landing only (PRR-82d852524d) ----

fn git_in(repo: &Path, args: &[&str]) {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .expect("git runs");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// A disposable git repository whose committed slice tree is the
/// working tree: the five exercised slice paths, written and committed.
fn slice_repo(tag: &str) -> PathBuf {
    let repo =
        std::env::temp_dir().join(format!("workloop-u74-slice-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&repo);
    for (i, path) in EXERCISED_SLICE_PATHS.iter().enumerate() {
        let full = repo.join(path);
        std::fs::create_dir_all(full.parent().expect("slice dir")).unwrap();
        std::fs::write(&full, format!("exercised slice content {i}\n")).unwrap();
    }
    git_in(&repo, &["init", "-q"]);
    git_in(&repo, &["config", "user.email", "u74@example.invalid"]);
    git_in(&repo, &["config", "user.name", "u74 test"]);
    git_in(&repo, &["add", "-A"]);
    git_in(&repo, &["commit", "-q", "-m", "exercised slice"]);
    repo
}

/// The pre-landing gap form of a record over `repo`: the
/// `absent{…}` cell, a base revision row, and a digest row per
/// exercised slice path.
fn gap_record() -> String {
    let mut record = String::from(
        "| Tested revision (slice commit on the PR branch) | \
         `absent{no branch commit containing the slice tree exists yet}` |\n\
         | Base revision (branch point on origin/main) | \
         `0000000000000000000000000000000000000000` |\n",
    );
    for slice in EXERCISED_SLICE_PATHS {
        record.push_str(&format!(
            "| Slice digest — {slice} | `{}` |\n",
            "a".repeat(64)
        ));
    }
    record
}

fn record_citing(commit: &str) -> String {
    format!(
        "| Tested revision (slice commit on the PR branch) | `{commit}` |\n\
         | Base revision (branch point on origin/main) | \
         `0000000000000000000000000000000000000000` |\n"
    )
}

fn head_commit(repo: &Path) -> String {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["rev-parse", "HEAD"])
        .output()
        .expect("git runs");
    assert!(out.status.success());
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// The finding scenario: the slice is committed (the commit contains
/// all five exercised slice paths with the exercised content) while
/// the record's tested-revision cell still reads `absent{…}` — the
/// named check must fail, not stay green.
#[test]
fn tested_revision_gap_is_rejected_once_a_commit_contains_the_exercised_slice_tree() {
    let repo = slice_repo("gap-closed");
    let err = slice_evidence(&repo, &gap_record())
        .expect_err("an unstamped record must not pass once a containing commit exists");
    assert!(
        err.contains("contains the exercised slice tree"),
        "the failure names the containing commit: {err}"
    );
}

/// The complementary controls: before the slice lands (the working
/// tree no longer matches any commit) the gap stays valid, and a
/// cited commit whose slice content has drifted from the exercised
/// working tree is rejected — only the containing commit passes.
#[test]
fn tested_revision_gap_stays_valid_before_the_slice_lands_and_a_stale_stamp_fails() {
    let repo = slice_repo("gap-open");
    let drift = repo.join(EXERCISED_SLICE_PATHS[0]);
    let original = std::fs::read_to_string(&drift).expect("slice file reads");
    std::fs::write(&drift, "uncommitted drift after the recorded tree\n").unwrap();

    slice_evidence(&repo, &gap_record())
        .expect("the gap stays valid while no commit contains the exercised slice tree");

    let commit = head_commit(&repo);
    let err = slice_evidence(&repo, &record_citing(&commit))
        .expect_err("a cited commit without the exercised slice tree must fail");
    assert!(
        err.contains("does not contain the exercised slice tree"),
        "the failure names the content mismatch: {err}"
    );

    std::fs::write(&drift, original).unwrap();
    slice_evidence(&repo, &record_citing(&commit))
        .expect("the containing commit satisfies the stamp once cited");
    let err = slice_evidence(&repo, &gap_record())
        .expect_err("once the containing commit exists, the gap must not pass");
    assert!(err.contains("contains the exercised slice tree"), "{err}");
}
