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
//!   that contains the exercised slice (or the explicit tested-
//!   revision gap, with base revision and slice digests retained as
//!   supplements), following the repository's evidence-record
//!   precedent.

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
    }
    assert!(
        doc.contains("verbatim"),
        "the walkthrough states its output is captured verbatim"
    );
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

/// The record's own tested-revision stamp: either a full commit id
/// that contains every exercised slice file on the branch history to
/// `HEAD`, or the explicit `absent{reason}` gap (explaining the
/// missing commit, with the base revision and sha256 digests of the
/// exercised slice retained as supplements). A base SHA that does not
/// contain the slice never satisfies the stamp on its own.
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
