//! U3.6d consolidated-record checks (parent issue #107, slice issue
//! #118).
//!
//! The consolidated U3.6 evidence record in `docs/evidence/u3.6d.md`
//! indexes the retained scenario records of the U3.6a/U3.6b/U3.6c
//! siblings and maps every #107 failure mode to its record — or to
//! the literal `unexercised` — across the adapter domain {fixture,
//! github}. Named checks guard it:
//!
//! - `u36d_completeness_every_failure_mode_adapter_cell_has_one_row`
//!   — every (failure_mode × adapter) cell of the 5×2 Cartesian
//!   product has exactly one row; a missing, duplicated or foreign
//!   row fails.
//! - `u36d_consolidated_rows_conform_to_the_schema` — every row
//!   carries the specified columns with closed-set values, every
//!   `scenario_record_ref` resolves into a retained record, and the
//!   record index agrees with the table and transcribes each cited
//!   record's tested revision, configuration, command/scenario,
//!   claim class and result.
//! - `u36d_doc_check_no_entry_lacks_a_claim_class` — no consolidated
//!   or index entry lacks a `claim_class`; `unexercised` rows cite
//!   why they were not run, `requires_live` rows stay `unexercised`
//!   until their separately authorized live case runs, and
//!   `live_evidenced` rows cite the retained authorized live record.
//! - `u36d_github_cells_are_live_evidenced_or_pending_119` — every
//!   `github` adapter cell is either `live_evidenced` (citing its
//!   U3.6e record) or an explicit `requires_live`/`unexercised`
//!   finding pending #119's remaining authorized case.
//! - `u36d_evidence_record_cites_the_tested_revision` — the record's
//!   own tested-revision cell is a full commit id that contains the
//!   exercised slice files (object presence) on the branch history
//!   to `HEAD` — the presence-and-ancestry citation rule the
//!   sibling doc checks follow; the check deliberately does not pin
//!   the slice files to the cited commit's blobs, so a later
//!   unrelated edit to a shared slice file cannot invalidate a
//!   retained record. This resolves non-head revisions, so CI needs
//!   a full-history checkout (`fetch-depth: 0` in
//!   `.github/workflows/rust.yml`).
//!
//! Failure-path tests exercise the rejections: a missing cell, a
//! duplicated cell, a foreign adapter, a row missing a column, an
//! `unexercised` row without a reason, a `requires_live` row that
//! claims a pass, a `live_evidenced` row that stays unexercised, a
//! fixture row citing a revision no cited record carries, and a
//! record-index config naming a host instance its cited record does
//! not transcribe.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Command;

const RECORD_PATH: &str = "docs/evidence/u3.6d.md";
const CONTRACT_PATH: &str = "docs/repository-host-contract.md";
const WORKFLOW_PATH: &str = ".github/workflows/rust.yml";
const SLICE_CHECK_PATH: &str = "crates/workloop-cli/tests/u36d_docs.rs";

/// The exercised slice tree the tested-revision stamp must contain
/// at the cited commit (object presence): the named checks, the
/// owner-document link and the CI full-history checkout the revision
/// resolution depends on. The consolidated record itself is excluded
/// — it is stamped by the commit that cites the slice commit, so it
/// cannot be part of the cited tree.
const EXERCISED_SLICE_PATHS: &[&str] = &[SLICE_CHECK_PATH, CONTRACT_PATH, WORKFLOW_PATH];

/// #107's failure modes, verbatim.
const FAILURE_MODES: [&str; 5] = [
    "stale_head",
    "duplicate_events",
    "permission_drift",
    "partial_effects",
    "lost_responses",
];

/// The adapter domain of the consolidated record: `fixture` is the
/// fixture-class adapter evidence retained by U3.6a–c (the
/// `fixture-github` and `sparse-host` implementations); `github` is
/// the real GitHub adapter, whose U3.6 failure-mode evidence is the
/// separately authorized isolated live cases of #119.
const ADAPTERS: [&str; 2] = ["fixture", "github"];

const CLAIM_CLASSES: [&str; 3] = ["fixture_contract", "requires_live", "live_evidenced"];
const INDEX_CLAIM_CLASSES: [&str; 2] = ["fixture_contract", "live_evidenced"];
const RESULTS: [&str; 3] = ["pass", "fail", "unexercised"];

const MIN_REASON: usize = 20;
const MIN_CONFIG: usize = 20;

const TABLE_HEADER: [&str; 8] = [
    "failure_mode",
    "adapter",
    "scenario_record_ref",
    "claim_class",
    "tested_revision",
    "config_identity",
    "result",
    "notes",
];
const INDEX_HEADER: [&str; 6] = [
    "record_ref",
    "tested_revision",
    "config_identity",
    "command_scenario",
    "claim_class",
    "result",
];

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

/// A `scenario_record_ref` cell is either the literal `unexercised`
/// or a JSON array of retained-record references; every reference
/// resolves into a `docs/evidence` record carrying its label.
fn parse_ref_cell(root: &Path, cell: &str) -> Result<(bool, Vec<String>), String> {
    if cell.trim() == "unexercised" {
        return Ok((true, Vec::new()));
    }
    let refs: Vec<String> = serde_json::from_str(cell).map_err(|_| {
        format!("scenario_record_ref must be a JSON array or the literal unexercised: {cell}")
    })?;
    if refs.is_empty() {
        return Err("scenario_record_ref[] must not be empty".to_string());
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
    Ok((false, refs))
}

fn parse_sha_list(cell: &str) -> Result<Vec<String>, String> {
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

/// The record's own tested-revision stamp: a full commit id on the
/// branch history to `HEAD` containing the exercised slice tree.
fn own_tested_revision(record: &str) -> Result<String, String> {
    let row = record
        .lines()
        .find(|line| line.contains("Tested revision (slice commit"))
        .ok_or("the record carries no Tested revision (slice commit…) row")?;
    let cell = first_backticked(row).ok_or("the Tested revision row carries no backticked cell")?;
    if !is_full_commit_id(cell) {
        return Err(format!(
            "the record's tested revision must be a full 40-hex commit id: {cell}"
        ));
    }
    Ok(cell.to_string())
}

/// Per-row grammar and cross-field rules. Returns the parsed rows as
/// `(failure_mode, adapter, unexercised, refs, claim_class, shas,
/// result)` for the completeness and index checks.
#[allow(clippy::type_complexity)]
fn validate_rows(
    root: &Path,
    record: &str,
    rows: &[Vec<String>],
) -> Result<
    Vec<(
        String,
        String,
        bool,
        Vec<String>,
        String,
        Vec<String>,
        String,
    )>,
    String,
> {
    validate_shape(rows, TABLE_HEADER.len())?;
    let own_sha = own_tested_revision(record)?;
    let mut out = Vec::new();
    for row in rows {
        let failure_mode = row[0].trim();
        if !FAILURE_MODES.contains(&failure_mode) {
            return Err(format!(
                "failure_mode must be one of {FAILURE_MODES:?}: {failure_mode}"
            ));
        }
        let adapter = row[1].trim();
        if !ADAPTERS.contains(&adapter) {
            return Err(format!("adapter must be one of {ADAPTERS:?}: {adapter}"));
        }
        let (unexercised, refs) = parse_ref_cell(root, &row[2])?;
        let claim_class = row[3].trim();
        if !CLAIM_CLASSES.contains(&claim_class) {
            return Err(format!(
                "claim_class must be one of {CLAIM_CLASSES:?}: {claim_class}"
            ));
        }
        let shas = parse_sha_list(&row[4])?;
        if row[5].trim().chars().count() < MIN_CONFIG {
            return Err(format!(
                "config_identity must carry at least {MIN_CONFIG} characters: {}",
                row[5]
            ));
        }
        let result = row[6].trim();
        if !RESULTS.contains(&result) {
            return Err(format!("result must be one of {RESULTS:?}: {result}"));
        }
        let notes = row[7].trim();

        if claim_class == "requires_live" {
            if result != "unexercised" || !unexercised {
                return Err(format!(
                    "a requires_live row stays unexercised until the separately authorized live run: {failure_mode}/{adapter}"
                ));
            }
            if !notes.contains("#119") {
                return Err(format!(
                    "a requires_live row must cite the pending authorized scope #119: {failure_mode}/{adapter}"
                ));
            }
        }
        if claim_class == "live_evidenced" && (unexercised || result == "unexercised") {
            return Err(format!(
                "a live_evidenced row cites a retained authorized live record — it cannot stay unexercised: {failure_mode}/{adapter}"
            ));
        }
        if unexercised && !(shas.len() == 1 && shas[0] == own_sha) {
            return Err(format!(
                "an unexercised row's tested_revision must be the consolidation slice commit {own_sha}: {failure_mode}/{adapter}"
            ));
        }
        match result {
            "pass" => {
                if refs.is_empty() {
                    return Err(format!(
                        "a pass row must cite retained scenario records: {failure_mode}/{adapter}"
                    ));
                }
            }
            "fail" | "unexercised" => {
                // A `live_evidenced` fail row legitimately cites its
                // retained record — a failed authorized live run is
                // exercised evidence of the failure, not a claim
                // dressed as a pass.
                let exercised_live_fail =
                    result == "fail" && claim_class == "live_evidenced" && !refs.is_empty();
                if !unexercised && !exercised_live_fail {
                    return Err(format!(
                        "a {result} row must not cite retained records as if exercised: {failure_mode}/{adapter}"
                    ));
                }
                if notes.chars().count() < MIN_REASON {
                    return Err(format!(
                        "an unexercised/failed row must cite why in notes (at least {MIN_REASON} characters): {failure_mode}/{adapter}"
                    ));
                }
            }
            _ => unreachable!(),
        }
        // Every cited revision is transcribed in at least one cited
        // record — a base SHA or invented SHA fails.
        for sha in &shas {
            if unexercised {
                continue;
            }
            let transcribed = refs.iter().any(|reference| {
                let file = reference.split('#').next().unwrap_or("");
                read_repo(root, file)
                    .map(|text| text.contains(sha))
                    .unwrap_or(false)
            });
            if !transcribed {
                return Err(format!(
                    "tested_revision {sha} of {failure_mode}/{adapter} appears in no cited record"
                ));
            }
        }
        out.push((
            failure_mode.to_string(),
            adapter.to_string(),
            unexercised,
            refs,
            claim_class.to_string(),
            shas,
            result.to_string(),
        ));
    }
    Ok(out)
}

/// Completeness: every (failure_mode × adapter) cell has exactly one
/// row; a missing, duplicated or foreign row fails.
fn completeness(rows: &[Vec<String>]) -> Result<BTreeMap<(String, String), usize>, String> {
    validate_shape(rows, TABLE_HEADER.len())?;
    let mut cells: BTreeMap<(String, String), usize> = BTreeMap::new();
    for row in rows {
        let failure_mode = normalize_ws(&row[0]);
        let adapter = normalize_ws(&row[1]);
        if !FAILURE_MODES.contains(&failure_mode.as_str()) {
            return Err(format!(
                "consolidated table carries a row with a foreign failure_mode: {failure_mode}"
            ));
        }
        if !ADAPTERS.contains(&adapter.as_str()) {
            return Err(format!(
                "consolidated table carries a row with a foreign adapter: {adapter}"
            ));
        }
        *cells.entry((failure_mode, adapter)).or_insert(0) += 1;
    }
    for mode in FAILURE_MODES {
        for adapter in ADAPTERS {
            let key = (mode.to_string(), adapter.to_string());
            match cells.get(&key) {
                None => {
                    return Err(format!(
                        "missing consolidated row for cell {mode} × {adapter}"
                    ));
                }
                Some(1) => {}
                Some(n) => {
                    return Err(format!(
                        "duplicate consolidated rows for cell {mode} × {adapter}: {n}"
                    ));
                }
            }
        }
    }
    if cells.len() != FAILURE_MODES.len() * ADAPTERS.len() {
        return Err(format!(
            "consolidated table carries {} cells, expected exactly {}",
            cells.len(),
            FAILURE_MODES.len() * ADAPTERS.len()
        ));
    }
    Ok(cells)
}

/// The record index: every `scenario_record_ref` of the table
/// resolves to exactly one index row carrying the retained record's
/// tested revision, configuration, command/scenario, claim class and
/// result — each transcribed verbatim against the cited record.
fn index_text(root: &Path, record: &str) -> Result<(), String> {
    let index = parse_table(record, &INDEX_HEADER)?;
    validate_shape(&index, INDEX_HEADER.len())?;
    let mut seen: BTreeSet<String> = BTreeSet::new();
    for row in &index {
        let reference = row[0].trim();
        if !seen.insert(reference.to_string()) {
            return Err(format!("duplicate record-index entry: {reference}"));
        }
        let (file, label) = split_ref(reference)?;
        let text = read_repo(root, file)?;
        if !text.contains(label) {
            return Err(format!(
                "record-index ref {reference} does not resolve: {file} carries no {label:?}"
            ));
        }
        let shas = parse_sha_list(&row[1])?;
        if shas.len() != 1 {
            return Err(format!(
                "record-index entry {reference} must name exactly one tested revision"
            ));
        }
        if !text.contains(&shas[0]) {
            return Err(format!(
                "tested revision {} is not transcribed in {file} (record-index entry {reference})",
                shas[0]
            ));
        }
        let config = row[2].trim();
        if config.chars().count() < MIN_CONFIG {
            return Err(format!(
                "record-index config_identity must carry at least {MIN_CONFIG} characters: {config}"
            ));
        }
        // The configuration names its installation host instance; the
        // instance must be transcribed in the cited record. No single
        // instance literal is hardcoded — fixture records name
        // `test-host`, the documented live extension names the real
        // host (`host_instance github.com` under #119) — so an index
        // entry is accepted whenever its cited record carries the
        // instance it claims.
        let host = config
            .split_once("host_instance")
            .map(|(_, rest)| rest)
            .and_then(|rest| {
                rest.split(|c: char| {
                    !(c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_')
                })
                .find(|token| !token.is_empty())
            })
            .unwrap_or("");
        if host.is_empty() {
            return Err(format!(
                "record-index config_identity must name the installation host instance \
                 (host_instance <instance>): {config}"
            ));
        }
        if !text.contains(host) {
            return Err(format!(
                "host_instance {host} of {reference} is not transcribed in {file}"
            ));
        }
        if let Some((_, digest)) = config.split_once("sha256:") {
            let token = digest
                .split(|c: char| !c.is_ascii_hexdigit())
                .next()
                .unwrap_or("");
            if token.len() == 64 && !text.contains(token) {
                return Err(format!(
                    "config digest {token} of {reference} is not transcribed in {file}"
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
        let claim_class = row[4].trim();
        if !INDEX_CLAIM_CLASSES.contains(&claim_class) {
            return Err(format!(
                "record-index claim_class must be one of {INDEX_CLAIM_CLASSES:?}: {claim_class}"
            ));
        }
        if !text.contains(claim_class) {
            return Err(format!(
                "claim_class {claim_class} of {reference} is not transcribed in {file}"
            ));
        }
        let result = row[5].trim();
        let first = result.split_whitespace().next().unwrap_or("");
        if !RESULTS.contains(&first) {
            return Err(format!(
                "record-index result must start with pass|fail|unexercised: {result}"
            ));
        }
        if first != "pass" && result.trim().chars().count() < MIN_REASON + first.len() {
            return Err(format!(
                "record-index result {first} must carry a retained reason: {result}"
            ));
        }
    }

    // Set agreement: the index covers exactly the refs the table cites.
    let table = parse_table(record, &TABLE_HEADER)?;
    let mut union: BTreeSet<String> = BTreeSet::new();
    for row in &table {
        let (_, refs) = parse_ref_cell(root, &row[2])?;
        union.extend(refs);
    }
    if let Some(reference) = union.difference(&seen).next() {
        return Err(format!(
            "scenario_record_ref {reference} has no record-index entry"
        ));
    }
    if let Some(reference) = seen.difference(&union).next() {
        return Err(format!(
            "record-index entry {reference} is never referenced by the consolidated table"
        ));
    }
    Ok(())
}

/// Schema: the consolidated table and the record index both conform,
/// and their ref sets agree.
fn schema_text(root: &Path, record: &str) -> Result<(), String> {
    let table = parse_table(record, &TABLE_HEADER)?;
    validate_rows(root, record, &table)?;
    index_text(root, record)
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

/// The record's own tested-revision citation: a full commit id that
/// contains every exercised slice file and lies on the branch
/// history to `HEAD` — the object-presence and ancestry rule the
/// sibling doc checks follow. The check deliberately does not pin
/// the slice files to the cited commit's blobs: a later unrelated
/// edit to a shared slice file (the owner document, the workflow)
/// must not invalidate a retained record, and the record's own
/// stamp is a later commit that does not modify the slice tree.
/// Non-head revisions are resolved through the object database, so
/// a depth-1 checkout cannot satisfy this — the workflow pins
/// `fetch-depth: 0`.
fn slice_evidence(root: &Path, record: &str) -> Result<(), String> {
    let cell = own_tested_revision(record)?;
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
    if !git_succeeds(root, &["merge-base", "--is-ancestor", &cell, "HEAD"]) {
        return Err(format!(
            "tested revision {cell} must be on the branch history leading to HEAD"
        ));
    }
    Ok(())
}

// ---- Synthetic record builders for the failure-path tests ----

const OWN_SHA: &str = "1111111111111111111111111111111111111111";
const SIBLING_U36A: &str = "fb5b0adabb610d97e8c74765ca5be5a6ac86637a";
const U36A_REF: &str = "docs/evidence/u3.6a.md#fixture-github — stale-head observation";
const U36A_INDEX: &str = "| record_ref | tested_revision | config_identity | command_scenario | claim_class | result |\n\
     | --- | --- | --- | --- | --- | --- |\n\
     | docs/evidence/u3.6a.md#fixture-github — stale-head observation | fb5b0adabb610d97e8c74765ca5be5a6ac86637a | installation host_instance test-host, repository local-repo, adapter fixture-github, contract version 1; config_digest sha256:fa78ae14d9ef35b83489ed640ccc20cae01bceeec67fe7b4a2372f6c18cfe0b8 | u36a-read-path::stale_head_observation_never_rolls_back | fixture_contract | pass |\n";

fn row(cells: [&str; 8]) -> String {
    let [mode, adapter, refs, claim, shas, config, result, notes] = cells;
    format!("| {mode} | {adapter} | {refs} | {claim} | {shas} | {config} | {result} | {notes} |")
}

fn fixture_row(mode: &str) -> String {
    row([
        mode,
        "fixture",
        &format!("[\"{U36A_REF}\"]"),
        "fixture_contract",
        SIBLING_U36A,
        "installation test-host/local-repo contract version 1, adapters fixture-github and sparse-host",
        "pass",
        "retained fixture-class records only",
    ])
}

fn github_row(mode: &str) -> String {
    row([
        mode,
        "github",
        "unexercised",
        "requires_live",
        OWN_SHA,
        "unexercised — no live GitHub configuration exercised",
        "unexercised",
        "live GitHub evidence is #119's separately authorized scope — not run",
    ])
}

fn synthetic_record(main_rows: &[String], index_rows: &str) -> String {
    let mut out = String::from(
        "# synthetic\n\n\
         | Tested revision (slice commit on the PR branch) | `1111111111111111111111111111111111111111` |\n\n\
         | failure_mode | adapter | scenario_record_ref | claim_class | tested_revision | config_identity | result | notes |\n\
         | --- | --- | --- | --- | --- | --- | --- | --- |\n",
    );
    for r in main_rows {
        out.push_str(r);
        out.push('\n');
    }
    out.push_str("\n## Record index\n\n");
    out.push_str(index_rows);
    out
}

fn full_table() -> Vec<String> {
    FAILURE_MODES
        .iter()
        .flat_map(|mode| [fixture_row(mode), github_row(mode)])
        .collect()
}

// ---- Named checks ----

#[test]
fn u36d_completeness_every_failure_mode_adapter_cell_has_one_row() {
    let root = repo_root();
    let record = read_repo(&root, RECORD_PATH).expect("the consolidated record is retained");
    let rows = parse_table(&record, &TABLE_HEADER).expect("the consolidated table exists");
    let cells = completeness(&rows).expect("every (failure_mode × adapter) cell has one row");
    assert_eq!(
        cells.len(),
        FAILURE_MODES.len() * ADAPTERS.len(),
        "the 5×2 cell set is complete"
    );
}

#[test]
fn u36d_completeness_fails_when_a_cell_has_no_row() {
    let rows: Vec<String> = full_table()
        .into_iter()
        .filter(|r| !r.contains("| lost_responses | github |"))
        .collect();
    let record = synthetic_record(&rows, U36A_INDEX);
    let parsed = parse_table(&record, &TABLE_HEADER).expect("synthetic table parses");
    let err = completeness(&parsed).expect_err("a missing cell must fail");
    assert!(
        err.contains("missing consolidated row"),
        "the failure names the missing cell: {err}"
    );
}

#[test]
fn u36d_completeness_fails_on_duplicate_and_foreign_rows() {
    let mut rows = full_table();
    rows.push(github_row("stale_head"));
    let record = synthetic_record(&rows, U36A_INDEX);
    let parsed = parse_table(&record, &TABLE_HEADER).expect("synthetic table parses");
    let err = completeness(&parsed).expect_err("a duplicated cell must fail");
    assert!(
        err.contains("duplicate consolidated rows"),
        "the failure names the duplicate cell: {err}"
    );

    let mut rows = full_table();
    rows.push(row([
        "stale_head",
        "gitlab",
        "unexercised",
        "requires_live",
        OWN_SHA,
        "unexercised — foreign adapter probe",
        "unexercised",
        "live evidence pending #119 authorization",
    ]));
    let record = synthetic_record(&rows, U36A_INDEX);
    let parsed = parse_table(&record, &TABLE_HEADER).expect("synthetic table parses");
    let err = completeness(&parsed).expect_err("a foreign adapter must fail");
    assert!(
        err.contains("foreign adapter"),
        "the failure names the foreign adapter: {err}"
    );

    let mut rows = full_table();
    rows.push(row([
        "unknown_mode",
        "fixture",
        "unexercised",
        "fixture_contract",
        OWN_SHA,
        "unexercised — foreign failure-mode probe",
        "unexercised",
        "no retained record exercises this mode",
    ]));
    let record = synthetic_record(&rows, U36A_INDEX);
    let parsed = parse_table(&record, &TABLE_HEADER).expect("synthetic table parses");
    let err = completeness(&parsed).expect_err("a foreign failure mode must fail");
    assert!(
        err.contains("foreign failure_mode"),
        "the failure names the foreign failure mode: {err}"
    );
}

#[test]
fn u36d_consolidated_rows_conform_to_the_schema() {
    let root = repo_root();
    let record = read_repo(&root, RECORD_PATH).expect("the consolidated record is retained");
    schema_text(&root, &record).expect("every row and index entry conforms to the schema");
}

#[test]
fn u36d_schema_rejects_a_row_missing_a_column() {
    let table = concat!(
        "| failure_mode | adapter | scenario_record_ref | claim_class | tested_revision | config_identity | result | notes |\n",
        "| --- | --- | --- | --- | --- | --- | --- | --- |\n",
        "| stale_head | fixture | [\"docs/evidence/u3.6a.md#fixture-github — stale-head observation\"] | fixture_contract | fb5b0adabb610d97e8c74765ca5be5a6ac86637a | installation test-host/local-repo contract version 1 |\n",
    );
    let record = format!(
        "# synthetic\n\n| Tested revision (slice commit on the PR branch) | `{OWN_SHA}` |\n\n{table}\n{U36A_INDEX}"
    );
    let err = schema_text(&repo_root(), &record)
        .expect_err("a row without the notes column must fail the schema check");
    assert!(
        err.contains("cells, expected 8"),
        "failure names the shape: {err}"
    );
}

#[test]
fn u36d_schema_rejects_an_unexercised_row_without_a_reason() {
    let mut rows = full_table();
    let pos = rows
        .iter()
        .position(|r| r.contains("| lost_responses | github |"))
        .expect("the github lost_responses row exists");
    rows[pos] = row([
        "lost_responses",
        "github",
        "unexercised",
        "requires_live",
        OWN_SHA,
        "unexercised — no live GitHub configuration exercised",
        "unexercised",
        "#119 pending",
    ]);
    let record = synthetic_record(&rows, U36A_INDEX);
    let err = schema_text(&repo_root(), &record)
        .expect_err("an unexercised row must cite why it was not run");
    assert!(
        err.contains("must cite why"),
        "the failure names the missing reason: {err}"
    );
}

#[test]
fn u36d_schema_rejects_a_requires_live_row_claiming_a_pass() {
    let mut rows = full_table();
    let pos = rows
        .iter()
        .position(|r| r.contains("| stale_head | github |"))
        .expect("the github stale_head row exists");
    rows[pos] = row([
        "stale_head",
        "github",
        "unexercised",
        "requires_live",
        OWN_SHA,
        "unexercised — no live GitHub configuration exercised",
        "pass",
        "live GitHub evidence is #119's separately authorized scope",
    ]);
    let record = synthetic_record(&rows, U36A_INDEX);
    let err =
        schema_text(&repo_root(), &record).expect_err("a requires_live row must stay unexercised");
    assert!(
        err.contains("requires_live"),
        "the failure names the claim class: {err}"
    );
}

#[test]
fn u36d_schema_rejects_a_live_evidenced_row_staying_unexercised() {
    let mut rows = full_table();
    let pos = rows
        .iter()
        .position(|r| r.contains("| stale_head | github |"))
        .expect("the github stale_head row exists");
    rows[pos] = row([
        "stale_head",
        "github",
        "unexercised",
        "live_evidenced",
        OWN_SHA,
        "unexercised — live claim without a retained record",
        "unexercised",
        "live GitHub evidence is #119's separately authorized scope",
    ]);
    let record = synthetic_record(&rows, U36A_INDEX);
    let err = schema_text(&repo_root(), &record)
        .expect_err("a live_evidenced row cannot stay unexercised");
    assert!(
        err.contains("live_evidenced"),
        "the failure names the claim class: {err}"
    );
}

#[test]
fn u36d_schema_rejects_a_revision_no_cited_record_carries() {
    let mut rows = full_table();
    let pos = rows
        .iter()
        .position(|r| r.contains("| stale_head | fixture |"))
        .expect("the fixture stale_head row exists");
    rows[pos] = row([
        "stale_head",
        "fixture",
        &format!("[\"{U36A_REF}\"]"),
        "fixture_contract",
        "9999999999999999999999999999999999999999",
        "installation test-host/local-repo contract version 1, adapters fixture-github and sparse-host",
        "pass",
        "retained fixture-class records only",
    ]);
    let record = synthetic_record(&rows, U36A_INDEX);
    let err = schema_text(&repo_root(), &record)
        .expect_err("a tested revision transcribed in no cited record must fail");
    assert!(
        err.contains("appears in no cited record"),
        "the failure names the untranscribed revision: {err}"
    );
}

#[test]
fn u36d_index_host_instance_is_transcribed_not_hardcoded() {
    let root = repo_root();
    // The record-index host instance binds to the cited record rather
    // than to a hardcoded literal: any instance the cited record
    // carries is accepted, so the documented live extension
    // (`host_instance github.com` under #119) will not be rejected
    // here, while an instance the record does not carry still fails.
    let accepted = U36A_INDEX.replace("host_instance test-host,", "host_instance sparse-host,");
    let record = synthetic_record(&[fixture_row("stale_head")], &accepted);
    schema_text(&root, &record).expect("an instance transcribed by the cited record is accepted");

    let untranscribed = U36A_INDEX.replace("host_instance test-host,", "host_instance ghost-host,");
    let record = synthetic_record(&[fixture_row("stale_head")], &untranscribed);
    let err = schema_text(&root, &record)
        .expect_err("an instance the cited record does not carry must fail");
    assert!(
        err.contains("ghost-host") && err.contains("not transcribed"),
        "the failure names the untranscribed host instance: {err}"
    );

    let unnamed = U36A_INDEX.replace(
        "installation host_instance test-host,",
        "installation of the",
    );
    let record = synthetic_record(&[fixture_row("stale_head")], &unnamed);
    let err = schema_text(&root, &record).expect_err("a config naming no host instance must fail");
    assert!(
        err.contains("host instance"),
        "the failure names the missing host instance: {err}"
    );
}

#[test]
fn u36d_doc_check_no_entry_lacks_a_claim_class() {
    let root = repo_root();
    let record = read_repo(&root, RECORD_PATH).expect("the consolidated record is retained");

    let table = parse_table(&record, &TABLE_HEADER).expect("the consolidated table exists");
    validate_shape(&table, TABLE_HEADER.len()).expect("row shape");
    for row in &table {
        let claim = row[3].trim();
        assert!(
            !claim.is_empty(),
            "consolidated entry {}×{} lacks a claim_class",
            row[0],
            row[1]
        );
        assert!(
            CLAIM_CLASSES.contains(&claim),
            "claim_class {claim} is outside the closed set {CLAIM_CLASSES:?}"
        );
    }

    let index = parse_table(&record, &INDEX_HEADER).expect("the record index exists");
    validate_shape(&index, INDEX_HEADER.len()).expect("index row shape");
    for row in &index {
        let claim = row[4].trim();
        assert!(
            !claim.is_empty(),
            "record-index entry {} lacks a claim_class",
            row[0]
        );
        assert!(
            INDEX_CLAIM_CLASSES.contains(&claim),
            "index claim_class {claim} is outside the closed set {INDEX_CLAIM_CLASSES:?}"
        );
    }
}

#[test]
fn u36d_github_cells_are_live_evidenced_or_pending_119() {
    let root = repo_root();
    let record = read_repo(&root, RECORD_PATH).expect("the consolidated record is retained");
    let rows = parse_table(&record, &TABLE_HEADER).expect("the consolidated table exists");
    let mut github_rows = 0;
    for row in &rows {
        if row[1].trim() != "github" {
            continue;
        }
        github_rows += 1;
        match row[3].trim() {
            "live_evidenced" => {
                assert_ne!(
                    row[2].trim(),
                    "unexercised",
                    "a live_evidenced github cell {} must cite its retained record",
                    row[0]
                );
                assert!(
                    matches!(row[6].trim(), "pass" | "fail"),
                    "a live_evidenced github cell {} reports the live run's result",
                    row[0]
                );
            }
            "requires_live" => {
                assert_eq!(
                    row[2].trim(),
                    "unexercised",
                    "a pending github cell {} stays unexercised until its authorized live run",
                    row[0]
                );
                assert_eq!(
                    row[6].trim(),
                    "unexercised",
                    "a pending github cell {} result stays unexercised",
                    row[0]
                );
                assert!(
                    row[7].contains("#119"),
                    "a pending github cell {} must cite the authorized scope #119",
                    row[0]
                );
            }
            other => panic!(
                "github cell {} carries a foreign claim_class {other}",
                row[0]
            ),
        }
    }
    assert_eq!(
        github_rows,
        FAILURE_MODES.len(),
        "one github cell per failure mode"
    );
}

#[test]
fn u36d_owner_document_links_the_consolidated_record() {
    let root = repo_root();
    let contract = read_repo(&root, CONTRACT_PATH).expect("the owner document is retained");
    let section = {
        let heading = "## U3.6 evidence record";
        let start = contract
            .lines()
            .position(|line| line.trim() == heading)
            .expect("the U3.6 evidence record section exists");
        let mut collected = Vec::new();
        for line in contract.lines().skip(start + 1) {
            if line.starts_with("## ") {
                break;
            }
            collected.push(line);
        }
        collected.join("\n")
    };
    assert!(
        section.contains("evidence/u3.6d.md"),
        "the owner document linked from #118 must link the consolidated record inside its U3.6 evidence record section"
    );
}

#[test]
fn u36d_evidence_record_cites_the_tested_revision() {
    let root = repo_root();
    let record = read_repo(&root, RECORD_PATH).expect("the consolidated record is retained");
    slice_evidence(&root, &record)
        .expect("the record cites a commit containing the exercised slice tree");
}

#[test]
fn u36d_completeness_and_schema_pass_on_the_real_record() {
    let root = repo_root();
    let record = read_repo(&root, RECORD_PATH).expect("the consolidated record is retained");
    let rows = parse_table(&record, &TABLE_HEADER).expect("the consolidated table exists");
    completeness(&rows).expect("every cell has exactly one row");
    let parsed = validate_rows(&root, &record, &rows).expect("rows conform");
    assert_eq!(parsed.len(), 10, "ten (failure_mode × adapter) rows");
    let fixture_pass = parsed
        .iter()
        .filter(|r| r.1 == "fixture" && r.6 == "pass")
        .count();
    assert_eq!(
        fixture_pass, 5,
        "every fixture cell passes on retained records"
    );
    let github_open = parsed
        .iter()
        .filter(|r| r.1 == "github" && r.2 && r.6 == "unexercised")
        .count();
    let github_live = parsed
        .iter()
        .filter(|r| r.1 == "github" && r.4 == "live_evidenced")
        .count();
    assert_eq!(
        github_open + github_live,
        5,
        "every github cell is live-evidenced or an explicit unexercised finding"
    );
}
