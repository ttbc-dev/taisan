//! U4.6d consolidated-record checks (parent issue #102).
//!
//! The consolidated U4.6 evidence record in `docs/evidence/u4.6d.md`
//! maps every #102 acceptance criterion to concrete retained records.
//! Four named checks guard it:
//!
//! - `u46d_completeness_every_102_criterion_has_a_row` — every #102
//!   acceptance criterion has exactly one row; a missing or duplicate
//!   row fails, an `absent{reason}` row (or a non-`pass` result) is
//!   permitted but reported as a gap, never counted as a pass.
//! - `u46d_consolidated_rows_conform_to_the_schema` — every mapping
//!   row and every record-index entry carries the specified columns,
//!   `record_refs[]` resolves into retained records, and each index
//!   entry transcribes the cited record's tested revision,
//!   configuration, command/scenario and result verbatim.
//! - `u46d_issue_102_references_the_consolidated_record` — #102's
//!   owner document links the consolidated record inside its U4.6
//!   evidence-record section, the record cites #102 and retains the
//!   prepared issue-side append payload; when `gh` can read #102 the
//!   live owner-document link, the `#124` child reference and the
//!   transcribed acceptance criteria are asserted too, bounded to a
//!   single read with a hermetic fallback.
//! - `u46d_evidence_record_cites_the_tested_revision_or_the_explicit_gap`
//!   — the record's own tested-revision cell is either a full commit
//!   SHA that contains this exercised slice on the branch history to
//!   `HEAD`, or the explicit `absent{reason}` gap; a base SHA never
//!   satisfies it.
//!
//! Five failure-path tests exercise the rejections: a missing #102
//! criterion row, a duplicated or foreign row, an absent row that
//! claims a pass, a row missing a column, and a base SHA transcribed
//! in no cited record.
//!
//! The #102 criterion texts are transcribed with
//! `gh issue view 102 -R shimpeiws/taisan --json body` (2026-11-13);
//! the parent issue is not part of this checkout, so the transcription
//! is the completeness check's source of truth.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const RECORD_PATH: &str = "docs/evidence/u4.6d.md";
const PROTOCOL_PATH: &str = "docs/worker-protocol.md";
const SLICE_CHECK_PATH: &str = "crates/workloop-exec/tests/u46d_evidence.rs";

const ISSUE_102_OWNER_DOC_URL: &str =
    "https://github.com/shimpeiws/taisan/blob/main/docs/worker-protocol.md";
const ISSUE_102_CHILD_REF: &str = "#124";
const ISSUE_102_RECORD_TITLE: &str = "Consolidated U4.6 evidence record";

/// The acceptance criteria of issue #102, verbatim.
const ISSUE_102_CRITERIA: [&str; 2] = [
    "Commands, queue, budgets and Outcomes are unchanged by selection; running Invocations are not migrated; artifact recollection and real-runtime safe stop have retained evidence.",
    "Retain verification evidence for this slice with the tested revision, configuration, command/scenario and result. The tested revision is the commit on the PR branch that contains the exercised slice tree: the slice must be committed to its branch before the evidence record is finalized, and the record must cite that commit SHA — a base SHA that does not contain the slice does not satisfy this criterion; a retained patch digest may supplement but cannot replace it. Fixture results support contract claims only; live claims require separately authorized live evidence.",
];

const MAPPING_HEADER: [&str; 5] = [
    "criterion",
    "record_refs[]",
    "tested_revision",
    "config_identity",
    "result",
];
const INDEX_HEADER: [&str; 5] = [
    "record_ref",
    "tested_revision",
    "config_identity",
    "command_scenario",
    "result",
];

const RESULTS: [&str; 4] = ["pass", "fail", "skipped", "inconclusive"];
const MIN_REASON: usize = 20;
const MIN_CONFIG: usize = 20;

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn read_repo(root: &Path, relative: &str) -> Result<String, String> {
    std::fs::read_to_string(root.join(relative)).map_err(|e| format!("cannot read {relative}: {e}"))
}

fn normalize_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn first_backticked(line: &str) -> Option<&str> {
    line.split('`').nth(1)
}

fn is_full_commit_id(token: &str) -> bool {
    token.len() == 40 && token.chars().all(|c| c.is_ascii_hexdigit())
}

/// True when the text carries any hex run of at least 40 characters.
fn carries_commit_id(text: &str) -> bool {
    let mut run = 0usize;
    for c in text.chars() {
        if c.is_ascii_hexdigit() {
            run += 1;
            if run >= 40 {
                return true;
            }
        } else {
            run = 0;
        }
    }
    false
}

/// `absent{reason}` with a retained reason, `None` when the cell is
/// not an absent marker, `Err` when the marker is malformed — a
/// malformed absent marker fails closed.
fn parse_absent(cell: &str) -> Result<Option<String>, String> {
    let Some(rest) = cell.strip_prefix("absent{") else {
        return Ok(None);
    };
    let Some(reason) = rest.strip_suffix('}') else {
        return Err(format!("absent marker must close with '}}': {cell}"));
    };
    if reason.trim().len() < MIN_REASON {
        return Err(format!(
            "absent{{reason}} must carry a reason of at least {MIN_REASON} characters: {cell}"
        ));
    }
    Ok(Some(reason.trim().to_string()))
}

fn split_row(line: &str) -> Result<Vec<String>, String> {
    let line = line.trim();
    if !(line.starts_with('|') && line.ends_with('|')) {
        return Err(format!("not a table row: {line}"));
    }
    Ok(line[1..line.len() - 1]
        .split('|')
        .map(|c| c.trim().to_string())
        .collect())
}

fn is_separator(cell: &str) -> bool {
    !cell.is_empty() && cell.chars().all(|c| c == '-' || c == ':')
}

fn parse_table(text: &str, header: &[&str]) -> Result<Vec<Vec<String>>, String> {
    let wanted: Vec<String> = header.iter().map(|s| (*s).to_string()).collect();
    let lines: Vec<&str> = text.lines().collect();
    let mut start = None;
    for (i, line) in lines.iter().enumerate() {
        if let Ok(cells) = split_row(line)
            && cells == wanted
        {
            start = Some(i + 1);
            break;
        }
    }
    let start =
        start.ok_or_else(|| format!("table header not found: | {} |", wanted.join(" | ")))?;
    let mut rows = Vec::new();
    for line in &lines[start..] {
        if !line.trim().starts_with('|') {
            break;
        }
        let cells = split_row(line)?;
        if cells.iter().all(|c| is_separator(c)) {
            continue;
        }
        rows.push(cells);
    }
    Ok(rows)
}

fn validate_shape(rows: &[Vec<String>], expected: usize) -> Result<(), String> {
    for (i, row) in rows.iter().enumerate() {
        if row.len() != expected {
            return Err(format!(
                "table row {} has {} cells, expected {expected}: {:?}",
                i + 1,
                row.len(),
                row
            ));
        }
    }
    Ok(())
}

fn split_ref(reference: &str) -> Result<(&str, &str), String> {
    let (file, label) = reference
        .split_once('#')
        .ok_or_else(|| format!("record ref must be path#label: {reference}"))?;
    if !file.starts_with("docs/evidence/") || !file.ends_with(".md") {
        return Err(format!(
            "record ref must name a docs/evidence markdown record: {reference}"
        ));
    }
    if label.trim().is_empty() {
        return Err(format!("record ref must carry a label: {reference}"));
    }
    Ok((file, label))
}

/// Returns `(absent reason, refs)`; every ref resolves into a retained
/// record whose label is present in the cited record.
fn parse_refs(root: &Path, cell: &str) -> Result<(Option<String>, Vec<String>), String> {
    if let Some(reason) = parse_absent(cell)? {
        return Ok((Some(reason), Vec::new()));
    }
    let refs: Vec<String> = serde_json::from_str(cell).map_err(|_| {
        format!("record_refs cell must be a JSON array or the literal absent{{reason}}: {cell}")
    })?;
    if refs.is_empty() {
        return Err("record_refs[] must not be empty".to_string());
    }
    for reference in &refs {
        let (file, label) = split_ref(reference)?;
        let text = read_repo(root, file)?;
        if !text.contains(label) {
            return Err(format!(
                "record ref {reference} does not resolve: {file} carries no {label:?}"
            ));
        }
    }
    Ok((None, refs))
}

fn parse_tested_revisions(cell: &str) -> Result<Vec<String>, String> {
    if parse_absent(cell)?.is_some() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for token in cell.split(|c: char| c == ',' || c.is_whitespace()) {
        if token.is_empty() {
            continue;
        }
        if !is_full_commit_id(token) {
            return Err(format!(
                "tested_revision token is not a full 40-hex commit id: {token}"
            ));
        }
        out.push(token.to_string());
    }
    if out.is_empty() {
        return Err(format!("tested_revision cell must not be empty: {cell}"));
    }
    Ok(out)
}

fn validate_result(cell: &str, must_not_pass: bool) -> Result<(), String> {
    let mut parts = cell.splitn(2, '—');
    let word = parts.next().unwrap_or("").trim();
    if !RESULTS.contains(&word) {
        return Err(format!(
            "result must start with pass|fail|skipped|inconclusive: {cell}"
        ));
    }
    let rest = parts.next().map(str::trim).unwrap_or("");
    if word == "pass" {
        if must_not_pass {
            return Err(format!("an absent/gap entry must not claim a pass: {cell}"));
        }
        return Ok(());
    }
    let reason = rest.trim_start_matches(['—', '-', ' ']).trim();
    if reason.len() < MIN_REASON {
        return Err(format!(
            "result {word} must carry a retained reason of at least {MIN_REASON} characters: {cell}"
        ));
    }
    Ok(())
}

#[derive(Debug)]
struct Coverage {
    rows: usize,
    gaps: Vec<String>,
}

/// Completeness: every #102 acceptance criterion has exactly one row;
/// a missing or foreign row fails. An absent row or a non-`pass`
/// result is reported as a gap — permitted, never a pass.
fn completeness(rows: &[Vec<String>]) -> Result<Coverage, String> {
    validate_shape(rows, MAPPING_HEADER.len())?;
    let mut gaps = Vec::new();
    for criterion in ISSUE_102_CRITERIA {
        let hits: Vec<&Vec<String>> = rows
            .iter()
            .filter(|r| normalize_ws(&r[0]) == normalize_ws(criterion))
            .collect();
        if hits.is_empty() {
            let head: String = criterion.chars().take(80).collect();
            return Err(format!(
                "missing consolidated row for #102 acceptance criterion: {head}…"
            ));
        }
        if hits.len() > 1 {
            return Err(format!(
                "duplicate consolidated row for #102 acceptance criterion: {criterion}"
            ));
        }
        let row = hits[0];
        let absent = parse_absent(&row[1])?;
        let first_word = row[4].split_whitespace().next().unwrap_or("");
        if absent.is_some() || first_word != "pass" {
            gaps.push((*criterion).to_string());
        }
    }
    for row in rows {
        if !ISSUE_102_CRITERIA
            .iter()
            .any(|c| normalize_ws(c) == normalize_ws(&row[0]))
        {
            let head: String = row[0].chars().take(80).collect();
            return Err(format!(
                "consolidated table carries a row that is no #102 acceptance criterion: {head}"
            ));
        }
    }
    Ok(Coverage {
        rows: rows.len(),
        gaps,
    })
}

struct IndexEntry {
    shas: Vec<String>,
    absent: bool,
}

/// Schema: every mapping row and every record-index entry conforms to
/// the specified columns, and the two tables agree.
fn schema_text(root: &Path, record: &str) -> Result<(), String> {
    let mapping = parse_table(record, &MAPPING_HEADER)?;
    validate_shape(&mapping, MAPPING_HEADER.len())?;

    let mut rows: Vec<(Vec<String>, Vec<String>, bool)> = Vec::new();
    for row in &mapping {
        if row[0].trim().is_empty() {
            return Err("criterion cell must not be empty".to_string());
        }
        let (absent, refs) = parse_refs(root, &row[1])?;
        let shas = parse_tested_revisions(&row[2])?;
        if row[3].trim().chars().count() < MIN_CONFIG {
            return Err(format!(
                "config_identity must carry at least {MIN_CONFIG} characters: {}",
                row[3]
            ));
        }
        validate_result(&row[4], absent.is_some())?;
        for sha in &shas {
            let transcribed = refs.iter().any(|reference| {
                let file = reference.split('#').next().unwrap_or("");
                read_repo(root, file)
                    .map(|text| text.contains(sha))
                    .unwrap_or(false)
            });
            if !transcribed {
                return Err(format!(
                    "tested_revision {sha} of criterion row {:?} appears in no cited record",
                    row[0].chars().take(60).collect::<String>()
                ));
            }
        }
        rows.push((refs, shas, absent.is_some()));
    }

    let index = parse_table(record, &INDEX_HEADER)?;
    validate_shape(&index, INDEX_HEADER.len())?;
    let mut index_map: BTreeMap<String, IndexEntry> = BTreeMap::new();
    for row in &index {
        let (file, label) = split_ref(&row[0])?;
        let text = read_repo(root, file)?;
        if !text.contains(label) {
            return Err(format!(
                "record-index ref {} does not resolve: {file} carries no {label:?}",
                row[0]
            ));
        }
        if index_map.contains_key(&row[0]) {
            return Err(format!("duplicate record-index entry: {}", row[0]));
        }
        let absent = parse_absent(&row[1])?.is_some();
        let shas = parse_tested_revisions(&row[1])?;
        if absent && !shas.is_empty() {
            return Err(format!(
                "record-index entry {} must not mix absent{{reason}} with a commit id",
                row[0]
            ));
        }
        if !absent && shas.len() != 1 {
            return Err(format!(
                "record-index entry {} must name exactly one tested revision, got {}",
                row[0],
                shas.len()
            ));
        }
        for sha in &shas {
            if !text.contains(sha) {
                return Err(format!(
                    "tested revision {sha} is not transcribed in {file} (record-index entry {})",
                    row[0]
                ));
            }
        }
        let config = row[2].trim();
        if config.chars().count() < MIN_CONFIG {
            return Err(format!(
                "record-index config_identity must carry at least {MIN_CONFIG} characters: {config}"
            ));
        }
        let hosts: Vec<&str> = config
            .split(|c: char| !(c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'))
            .filter(|t| t.ends_with("-host"))
            .collect();
        if hosts.is_empty() {
            return Err(format!(
                "record-index config_identity must name the installation host instance (<id>-host): {config}"
            ));
        }
        for host in &hosts {
            if !text.contains(host) {
                return Err(format!(
                    "installation host {host} of {file} does not appear in the cited record"
                ));
            }
        }
        let scenario = row[3].trim();
        if scenario.chars().count() < 10 {
            return Err(format!(
                "record-index command_scenario must name the command/scenario: {scenario}"
            ));
        }
        if !text.contains(scenario) {
            return Err(format!(
                "command_scenario {scenario:?} is not transcribed in {file}"
            ));
        }
        validate_result(&row[4], absent)?;
        index_map.insert(row[0].clone(), IndexEntry { shas, absent });
    }

    let union: BTreeSet<&String> = rows.iter().flat_map(|(refs, _, _)| refs.iter()).collect();
    let indexed: BTreeSet<&String> = index_map.keys().collect();
    if let Some(reference) = union.difference(&indexed).next() {
        return Err(format!(
            "record_refs entry {reference} has no record-index entry"
        ));
    }
    if let Some(reference) = indexed.difference(&union).next() {
        return Err(format!(
            "record-index entry {reference} is never referenced by a criterion row"
        ));
    }

    for (row, (refs, shas, absent)) in mapping.iter().zip(&rows) {
        if *absent {
            continue;
        }
        let mut expected: BTreeSet<String> = BTreeSet::new();
        let mut has_gap_entry = false;
        for reference in refs {
            let entry = &index_map[reference];
            if entry.absent {
                has_gap_entry = true;
            }
            expected.extend(entry.shas.iter().cloned());
        }
        let actual: BTreeSet<String> = shas.iter().cloned().collect();
        if actual != expected {
            return Err(format!(
                "row tested_revision set does not match the indexed tested revisions of its record_refs: {:?} vs {:?}",
                actual, expected
            ));
        }
        if has_gap_entry {
            validate_result(&row[4], true)?;
        }
    }

    let own_row = record
        .lines()
        .find(|line| line.contains("Tested revision (slice commit"))
        .ok_or("the record carries no Tested revision (slice commit…) row")?;
    let own_cell = first_backticked(own_row).ok_or("the tested revision cell is backticked")?;
    let own_absent = parse_absent(own_cell)?.is_some();
    for (reference, entry) in &index_map {
        if !reference.starts_with(RECORD_PATH) {
            continue;
        }
        if entry.absent != own_absent {
            return Err(format!(
                "record-index entry {reference} disagrees with the record's own tested-revision cell"
            ));
        }
        if !entry.absent && entry.shas[0] != own_cell {
            return Err(format!(
                "record-index entry {reference} must cite the record's own tested revision {own_cell}"
            ));
        }
    }
    Ok(())
}

fn git_succeeds(root: &Path, args: &[&str]) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .map(|out| out.status.success())
        .unwrap_or(false)
}

/// The record's own tested-revision stamp: either the explicit
/// `absent{reason}` gap (with base revision and digests retained as
/// supplements) or a full commit id that contains the exercised slice
/// on the branch history to `HEAD`.
fn slice_evidence(root: &Path, record: &str) -> Result<(), String> {
    let row = record
        .lines()
        .find(|line| line.contains("Tested revision (slice commit"))
        .ok_or("the record carries no Tested revision (slice commit…) row")?;
    let cell = first_backticked(row).ok_or("the tested revision cell is backticked")?;

    if let Some(reason) = parse_absent(cell)? {
        if carries_commit_id(cell) {
            return Err("an absent tested-revision cell must not carry any commit id".to_string());
        }
        if !reason.contains("commit") {
            return Err(format!(
                "the tested-revision gap reason must explain the missing commit: {cell}"
            ));
        }
        let base_row = record
            .lines()
            .find(|line| line.contains("Base revision"))
            .ok_or("the record carries no base revision row")?;
        let base = first_backticked(base_row).ok_or("the base revision cell is backticked")?;
        if !is_full_commit_id(base) {
            return Err(format!("base revision must be a full commit id: {base}"));
        }
        for supplement in [SLICE_CHECK_PATH, PROTOCOL_PATH] {
            let digested = record.lines().any(|line| {
                line.contains(supplement)
                    && line
                        .split('`')
                        .any(|tok| tok.len() == 64 && tok.chars().all(|c| c.is_ascii_hexdigit()))
            });
            if !digested {
                return Err(format!(
                    "the exercised slice must retain a sha256 digest for {supplement}"
                ));
            }
        }
        if !record.contains("## Commands and results") || !record.contains("u46d_evidence") {
            return Err("the record must retain the slice's commands and results".to_string());
        }
        if !record.contains("Toolchain") || !record.contains("Host platform") {
            return Err(
                "the record must retain the configuration of this slice's verification run"
                    .to_string(),
            );
        }
        println!(
            "GAP reported (not a pass): tested-revision stamp for {RECORD_PATH} is {cell} — the PR-branch commit SHA must be stamped before this record is finalized"
        );
        return Ok(());
    }

    if !is_full_commit_id(cell) {
        return Err(format!(
            "tested revision must be a full 40-hex commit id or the explicit absent{{reason}} gap: {cell}"
        ));
    }
    let commit = format!("{cell}^{{commit}}");
    if !git_succeeds(root, &["cat-file", "-e", &commit]) {
        let lower = record.to_ascii_lowercase();
        if !(lower.contains("branch commit") && lower.contains("shallow")) {
            return Err(format!(
                "tested revision {cell} does not resolve and the record does not explain it as a branch commit a shallow checkout may omit"
            ));
        }
        return Ok(());
    }
    let slice = format!("{cell}:{SLICE_CHECK_PATH}");
    if !git_succeeds(root, &["cat-file", "-e", &slice]) {
        return Err(format!(
            "tested revision {cell} must contain the exercised slice {SLICE_CHECK_PATH}"
        ));
    }
    if !git_succeeds(root, &["merge-base", "--is-ancestor", cell, "HEAD"]) {
        return Err(format!(
            "tested revision {cell} must be on the branch history leading to HEAD"
        ));
    }
    Ok(())
}

/// Bounded read of #102's body; `None` when `gh` cannot answer (no
/// binary, no credentials, no network) so the hermetic assertions of
/// the retained transcription take over.
fn issue_102_body() -> Option<String> {
    let mut child = Command::new("gh")
        .args([
            "issue",
            "view",
            "102",
            "-R",
            "shimpeiws/taisan",
            "--json",
            "body",
            "-q",
            ".body",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                if !status.success() {
                    return None;
                }
                use std::io::Read;
                let mut out = String::new();
                child.stdout.as_mut()?.read_to_string(&mut out).ok()?;
                return Some(out);
            }
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(_) => return None,
        }
    }
}

/// The `### U4.6 evidence record` section of the owner document, up
/// to the next `## ` heading.
fn protocol_section(protocol: &str) -> Result<String, String> {
    let heading = "### U4.6 evidence record";
    let start = protocol
        .lines()
        .position(|line| line.trim() == heading)
        .ok_or_else(|| format!("{PROTOCOL_PATH} carries no {heading:?} section"))?;
    let mut collected = Vec::new();
    for line in protocol.lines().skip(start + 1) {
        if line.starts_with("## ") {
            break;
        }
        collected.push(line);
    }
    Ok(collected.join("\n"))
}

#[test]
fn u46d_completeness_every_102_criterion_has_a_row() {
    let root = repo_root();
    let record = read_repo(&root, RECORD_PATH).expect("the consolidated record is retained");
    let rows = parse_table(&record, &MAPPING_HEADER).expect("the consolidated table exists");
    let coverage = completeness(&rows).expect("every #102 acceptance criterion has a row");
    assert_eq!(
        coverage.rows,
        ISSUE_102_CRITERIA.len(),
        "one row per #102 acceptance criterion"
    );
    for gap in &coverage.gaps {
        println!("GAP reported (not a pass): #102 criterion without satisfied evidence: {gap}");
        assert!(
            ISSUE_102_CRITERIA.contains(&gap.as_str()),
            "gaps are reported against #102 criteria"
        );
    }
}

#[test]
fn u46d_completeness_fails_when_a_102_criterion_has_no_row() {
    let table = mapping_table(&[(
        ISSUE_102_CRITERIA[0],
        "[\"docs/evidence/u4.6a.md#WS-20\"]",
        "aa2fa49911a7c267b3b7382032ae79857aadcd59",
        "swap-host/swap-repo fixture installation, contract version 1",
        "pass — fixture_supported",
    )]);
    let rows = parse_table(&table, &MAPPING_HEADER).expect("synthetic table parses");
    let err = completeness(&rows).expect_err("a missing criterion row must fail");
    assert!(
        err.contains("missing consolidated row"),
        "the failure names the missing row: {err}"
    );
}

#[test]
fn u46d_completeness_fails_on_duplicate_and_foreign_rows() {
    let shared = (
        "swap-host/swap-repo fixture installation, contract version 1",
        "aa2fa49911a7c267b3b7382032ae79857aadcd59",
    );
    let duplicate = mapping_table(&[
        (
            ISSUE_102_CRITERIA[0],
            "[\"docs/evidence/u4.6a.md#WS-20\"]",
            shared.1,
            shared.0,
            "pass — fixture_supported",
        ),
        (
            ISSUE_102_CRITERIA[0],
            "[\"docs/evidence/u4.6a.md#WS-20\"]",
            shared.1,
            shared.0,
            "pass — fixture_supported",
        ),
    ]);
    let rows = parse_table(&duplicate, &MAPPING_HEADER).expect("synthetic table parses");
    let err = completeness(&rows).expect_err("a duplicated criterion row must fail");
    assert!(
        err.contains("duplicate consolidated row"),
        "the failure names the duplicate row: {err}"
    );

    let foreign = mapping_table(&[
        (
            ISSUE_102_CRITERIA[0],
            "[\"docs/evidence/u4.6a.md#WS-20\"]",
            shared.1,
            shared.0,
            "pass — fixture_supported",
        ),
        (
            ISSUE_102_CRITERIA[1],
            "[\"docs/evidence/u4.6a.md#WS-20\"]",
            shared.1,
            shared.0,
            "inconclusive — isolated here only to exercise the foreign-row rejection",
        ),
        (
            "A criterion of some other parent issue",
            "[\"docs/evidence/u4.6a.md#WS-20\"]",
            shared.1,
            shared.0,
            "pass — fixture_supported",
        ),
    ]);
    let rows = parse_table(&foreign, &MAPPING_HEADER).expect("synthetic table parses");
    let err = completeness(&rows).expect_err("a foreign row must fail");
    assert!(
        err.contains("no #102 acceptance criterion"),
        "the failure names the foreign row: {err}"
    );
}

#[test]
fn u46d_completeness_reports_an_absent_row_as_a_gap_not_a_pass() {
    let table = mapping_table(&[
        (
            ISSUE_102_CRITERIA[0],
            "[\"docs/evidence/u4.6a.md#WS-20\"]",
            "aa2fa49911a7c267b3b7382032ae79857aadcd59",
            "swap-host/swap-repo fixture installation, contract version 1",
            "pass — fixture_supported",
        ),
        (
            ISSUE_102_CRITERIA[1],
            "absent{no retained record qualifies — the evidence would have to be live and no live scenario is authorized}",
            "absent{no record carries a stamped revision for this criterion}",
            "swap-host/swap-repo fixture installation, contract version 1",
            "inconclusive — recorded as an explicit gap, never a pass",
        ),
    ]);
    let rows = parse_table(&table, &MAPPING_HEADER).expect("synthetic table parses");
    let coverage = completeness(&rows).expect("an absent row is permitted");
    assert_eq!(
        coverage.gaps,
        vec![ISSUE_102_CRITERIA[1].to_string()],
        "the absent row is reported as a gap, not counted as a pass"
    );
    assert_eq!(
        rows[1][4].split_whitespace().next(),
        Some("inconclusive"),
        "an absent row never carries a pass result"
    );

    let passing_gap = mapping_table(&[
        (
            ISSUE_102_CRITERIA[0],
            "[\"docs/evidence/u4.6a.md#WS-20\"]",
            "aa2fa49911a7c267b3b7382032ae79857aadcd59",
            "swap-host/swap-repo fixture installation, contract version 1",
            "pass — fixture_supported",
        ),
        (
            ISSUE_102_CRITERIA[1],
            "absent{no retained record qualifies — the evidence would have to be live}",
            "absent{no record carries a stamped revision for this criterion}",
            "swap-host/swap-repo fixture installation, contract version 1",
            "pass — fixture_supported",
        ),
    ]);
    let err = schema_text(&repo_root(), &passing_gap)
        .expect_err("an absent row must never be reported as a pass");
    assert!(
        err.contains("must not claim a pass"),
        "the schema failure names the forbidden pass: {err}"
    );
}

#[test]
fn u46d_consolidated_rows_conform_to_the_schema() {
    let root = repo_root();
    let record = read_repo(&root, RECORD_PATH).expect("the consolidated record is retained");
    schema_text(&root, &record).expect("every row and index entry conforms to the schema");
}

#[test]
fn u46d_schema_rejects_a_row_missing_a_column() {
    let table = concat!(
        "| criterion | record_refs[] | tested_revision | config_identity | result |\n",
        "| --- | --- | --- | --- | --- |\n",
        "| short row | [docs/evidence/u4.6a.md#WS-20] | aa2fa49911a7c267b3b7382032ae79857aadcd59 | swap-host/swap-repo fixture installation |\n",
    );
    let err = schema_text(&repo_root(), table)
        .expect_err("a row without the result column must fail the schema check");
    assert!(
        err.contains("cells, expected 5"),
        "failure names the shape: {err}"
    );
}

#[test]
fn u46d_schema_rejects_a_base_sha_transcribed_in_no_cited_record() {
    let table = mapping_table(&[(
        ISSUE_102_CRITERIA[0],
        "[\"docs/evidence/u4.6a.md#WS-20\"]",
        "457cd3b4345f7b95cf84cb2202a8f4fc6c2e3168",
        "swap-host/swap-repo fixture installation, contract version 1",
        "pass — fixture_supported",
    )]);
    let err = schema_text(&repo_root(), &table)
        .expect_err("a base SHA that no cited record carries must fail");
    assert!(
        err.contains("appears in no cited record"),
        "failure names the untranscribed revision: {err}"
    );
}

#[test]
fn u46d_issue_102_references_the_consolidated_record() {
    let root = repo_root();
    let record = read_repo(&root, RECORD_PATH).expect("the consolidated record is retained");
    let protocol = read_repo(&root, PROTOCOL_PATH).expect("the owner document is retained");

    // The record retains #102's references and names the parent issue.
    for needle in [
        ISSUE_102_OWNER_DOC_URL,
        ISSUE_102_CHILD_REF,
        ISSUE_102_RECORD_TITLE,
    ] {
        assert!(
            record.contains(needle),
            "the record must retain #102's reference context: {needle}"
        );
    }

    // #102's owner document links the consolidated record — the
    // in-checkout hop of the reference chain from #102 to the record.
    let section = protocol_section(&protocol).expect("the U4.6 evidence record section exists");
    assert!(
        section.contains("evidence/u4.6d.md"),
        "the owner document linked from #102 must link the consolidated record inside its U4.6 evidence record section"
    );

    // The prepared issue-side append payload names the record path.
    let payload_start = record
        .find("### Prepared append payload")
        .expect("the record retains the prepared #102 append payload");
    let fence = record[payload_start..]
        .find("```markdown")
        .expect("the payload is fenced markdown")
        + payload_start;
    let after = &record[fence + "```markdown".len()..];
    let end = after.find("\n```").expect("the payload fence closes");
    let payload = &after[..end];
    assert!(
        payload.contains("docs/evidence/u4.6d.md"),
        "the prepared #102 payload must reference the consolidated record"
    );

    // Live hop: when `gh` can read #102, assert its owner-document
    // link, its child reference and the acceptance-criterion texts the
    // completeness check transcribes against the live issue body.
    match issue_102_body() {
        Some(body) => {
            for needle in [
                ISSUE_102_OWNER_DOC_URL,
                ISSUE_102_CHILD_REF,
                ISSUE_102_RECORD_TITLE,
            ] {
                assert!(
                    body.contains(needle),
                    "live #102 must reference the consolidated record through {needle}"
                );
            }
            let start = body
                .find("## Acceptance criteria")
                .expect("live #102 carries an Acceptance criteria section");
            let rest = &body[start..];
            let end = rest[2..].find("\n## ").map(|i| i + 2).unwrap_or(rest.len());
            let live: Vec<String> = rest[..end]
                .lines()
                .filter_map(|line| line.strip_prefix("- [ ] "))
                .map(str::to_string)
                .collect();
            let transcribed: Vec<String> = ISSUE_102_CRITERIA
                .iter()
                .map(|s| (*s).to_string())
                .collect();
            assert_eq!(
                live, transcribed,
                "the completeness check's transcription must match the live #102 acceptance criteria"
            );
            println!(
                "live #102 read asserted the owner-document link, the #124 record reference and both acceptance criteria"
            );
        }
        None => println!(
            "u46d: live #102 read unavailable — asserted the retained #102 transcription and the owner-document chain instead"
        ),
    }
}

#[test]
fn u46d_evidence_record_cites_the_tested_revision_or_the_explicit_gap() {
    let root = repo_root();
    let record = read_repo(&root, RECORD_PATH).expect("the consolidated record is retained");
    slice_evidence(&root, &record)
        .expect("the record cites a containing commit or the explicit tested-revision gap");
}

fn mapping_table(rows: &[(&str, &str, &str, &str, &str)]) -> String {
    let mut text = String::from(
        "| criterion | record_refs[] | tested_revision | config_identity | result |\n| --- | --- | --- | --- | --- |\n",
    );
    for row in rows {
        text.push_str(&format!(
            "| {} | {} | {} | {} | {} |\n",
            row.0, row.1, row.2, row.3, row.4
        ));
    }
    text
}
