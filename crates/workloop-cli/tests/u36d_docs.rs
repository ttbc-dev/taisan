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
//! - `u36d_github_cells_are_all_live_evidenced` — every `github`
//!   adapter cell cites its retained authorized U3.6e live record;
//!   #119's cases are complete, so a regression back to
//!   `requires_live`/`unexercised` fails.
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
//! fixture row citing a revision no cited record carries, a row
//! omitting a cited record's own tested revision, a base SHA
//! transcribed as a tested revision, a record-index entry whose
//! claim_class, tested revision, configuration, command/scenario or
//! result disagrees with the cited record's own, a `live_evidenced`
//! index entry missing its authorization fields, a consolidated row
//! whose claim_class or result disagrees with its cited index
//! entries, and a record-index config naming no installation host
//! instance.

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
        // The row's tested_revision cell must equal the set of
        // tested_revision values the cited records declare — a base
        // SHA, an invented SHA, or a revision lifted from a sibling
        // section of the same file fails, and a cited record's own
        // revision cannot be omitted from the cell.
        if !unexercised {
            let mut record_shas: BTreeSet<String> = BTreeSet::new();
            for reference in &refs {
                let (file, label) = split_ref(reference)?;
                let text = read_repo(root, file)?;
                for record in section_records(&text, label)? {
                    if let Some(sha) = record.get("tested_revision").and_then(|v| v.as_str()) {
                        record_shas.insert(sha.to_string());
                    }
                }
            }
            for sha in &shas {
                if !record_shas.contains(sha) {
                    return Err(format!(
                        "tested_revision {sha} of {failure_mode}/{adapter} appears in no cited record"
                    ));
                }
            }
            for sha in &record_shas {
                if !shas.contains(sha) {
                    return Err(format!(
                        "cited record tested_revision {sha} of {failure_mode}/{adapter} \
                         is missing from the row's tested_revision cell"
                    ));
                }
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

/// Parse the retained record JSON blocks inside the labelled
/// section of a cited evidence file. Each scenario section carries
/// one fenced `json` block — the retained `EvidenceRecord`.
fn section_records(text: &str, label: &str) -> Result<Vec<serde_json::Value>, String> {
    let lines: Vec<&str> = text.lines().collect();
    let start = lines
        .iter()
        .position(|line| {
            let t = line.trim_start();
            t.starts_with('#') && t.contains(label)
        })
        .ok_or_else(|| format!("no evidence section carries label {label:?}"))?;
    let mut end = lines.len();
    for (i, line) in lines.iter().enumerate().skip(start + 1) {
        if line.trim_start().starts_with('#') {
            end = i;
            break;
        }
    }
    let mut records = Vec::new();
    let mut in_json = false;
    let mut buf = String::new();
    for line in &lines[start..end] {
        let t = line.trim();
        if t == "```json" {
            in_json = true;
            buf.clear();
            continue;
        }
        if t == "```" && in_json {
            in_json = false;
            records
                .push(serde_json::from_str(&buf).map_err(|e| {
                    format!("record JSON in section {label:?} does not parse: {e}")
                })?);
            continue;
        }
        if in_json {
            buf.push_str(line);
            buf.push('\n');
        }
    }
    if records.is_empty() {
        return Err(format!("section {label:?} carries no record JSON"));
    }
    Ok(records)
}

/// One labeled value inside a record-index `config_identity` cell:
/// the first token after `key` whose characters all satisfy `keep`.
fn config_token<'a>(config: &'a str, key: &str, keep: impl Fn(char) -> bool) -> Option<&'a str> {
    config
        .split_once(key)
        .map(|(_, rest)| rest)
        .and_then(|rest| rest.split(|c: char| !keep(c)).find(|t| !t.is_empty()))
}

fn host_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_')
}

fn repo_char(c: char) -> bool {
    host_char(c) || c == '/'
}

/// The fields one record-index row claims its cited record carries.
/// Every value is compared verbatim against the record JSON — a
/// substring present anywhere else in the evidence file cannot
/// satisfy the row.
struct IndexEntry<'a> {
    reference: &'a str,
    tested_revision: &'a str,
    host_instance: &'a str,
    repository: &'a str,
    adapter: &'a str,
    contract_version: &'a str,
    config_digest: &'a str,
    command_scenario: &'a str,
    claim_class: &'a str,
    result: &'a str,
}

fn json_scalar(value: &serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::String(s) => Some(s.clone()),
        serde_json::Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

/// A dotted-path read into the record JSON returning the scalar as
/// text — `contract_version` serializes as a number, the rest as
/// strings.
fn record_text(record: &serde_json::Value, path: &[&str]) -> Option<String> {
    let mut current = record;
    for key in path {
        current = current.get(*key)?;
    }
    json_scalar(current)
}

/// Every retained record in the cited section must carry exactly the
/// fields the index row transcribes — tested revision, installation
/// configuration, command/scenario, claim class and result — so a
/// value lifted from a sibling section or a different field of the
/// same record cannot satisfy the row. Fixture evidence can never be
/// indexed as `live_evidenced`, and a live record cannot be indexed
/// as `fixture_contract`; a `live_evidenced` record must additionally
/// carry the `authorization_reference` and `isolation_scope` the
/// evidence schema mandates.
fn check_record_entry(records: &[serde_json::Value], entry: &IndexEntry<'_>) -> Result<(), String> {
    for record in records {
        let expect = |path: &[&str], claimed: &str, label: &str| -> Result<(), String> {
            let actual = record_text(record, path);
            if actual.as_deref() != Some(claimed) {
                return Err(format!(
                    "record-index {label} {claimed:?} of {} does not match the cited \
                     record's {label} {actual:?}",
                    entry.reference
                ));
            }
            Ok(())
        };
        expect(
            &["tested_revision"],
            entry.tested_revision,
            "tested_revision",
        )?;
        expect(
            &["config_identity", "install_identity", "host_instance"],
            entry.host_instance,
            "config_identity.host_instance",
        )?;
        expect(
            &["config_identity", "install_identity", "repository"],
            entry.repository,
            "config_identity.repository",
        )?;
        expect(
            &["config_identity", "install_identity", "adapter"],
            entry.adapter,
            "config_identity.adapter",
        )?;
        expect(
            &["config_identity", "install_identity", "contract_version"],
            entry.contract_version,
            "config_identity.contract_version",
        )?;
        expect(
            &["config_identity", "config_digest"],
            entry.config_digest,
            "config_identity.config_digest",
        )?;
        expect(
            &["command_or_scenario"],
            entry.command_scenario,
            "command_or_scenario",
        )?;
        expect(&["claim_class"], entry.claim_class, "claim_class")?;
        expect(&["result"], entry.result, "result")?;
        if entry.claim_class != "live_evidenced" {
            continue;
        }
        let authorized = record
            .get("authorization_reference")
            .and_then(|a| a.as_str())
            .is_some_and(|a| !a.trim().is_empty());
        let scope = record.get("isolation_scope");
        let scoped = scope
            .and_then(|s| s.get("installation_id"))
            .and_then(|v| v.as_str())
            .is_some_and(|v| !v.trim().is_empty())
            && scope
                .and_then(|s| s.get("repository"))
                .and_then(|v| v.as_str())
                .is_some_and(|v| !v.trim().is_empty());
        if !authorized || !scoped {
            return Err(format!(
                "live_evidenced record {} must carry authorization_reference \
                 and isolation_scope",
                entry.reference
            ));
        }
    }
    Ok(())
}

/// The record index: every `scenario_record_ref` of the table
/// resolves to exactly one index row carrying the retained record's
/// tested revision, configuration, command/scenario, claim class and
/// result — each transcribed verbatim against the cited record.
fn index_text(root: &Path, record: &str) -> Result<(), String> {
    let index = parse_table(record, &INDEX_HEADER)?;
    validate_shape(&index, INDEX_HEADER.len())?;
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut index_claims: BTreeMap<String, String> = BTreeMap::new();
    let mut index_results: BTreeMap<String, String> = BTreeMap::new();
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
        let config = row[2].trim();
        if config.chars().count() < MIN_CONFIG {
            return Err(format!(
                "record-index config_identity must carry at least {MIN_CONFIG} characters: {config}"
            ));
        }
        // The configuration names the installation the cited record
        // ran under; every component is compared verbatim against the
        // record JSON below — no instance literal is hardcoded, and a
        // value present elsewhere in the evidence file cannot satisfy
        // the row.
        let host = config_token(config, "host_instance", host_char).ok_or_else(|| {
            format!(
                "record-index config_identity must name the installation host instance \
                 (host_instance <instance>): {config}"
            )
        })?;
        let repository = config_token(config, "repository", repo_char).ok_or_else(|| {
            format!("record-index config_identity must name the repository: {config}")
        })?;
        let adapter = config_token(config, "adapter", host_char).ok_or_else(|| {
            format!("record-index config_identity must name the adapter: {config}")
        })?;
        let contract_version =
            config_token(config, "contract version", |c: char| c.is_ascii_digit()).ok_or_else(
                || format!("record-index config_identity must name the contract version: {config}"),
            )?;
        let config_digest = config
            .split_once("sha256:")
            .and_then(|(_, rest)| {
                let token = rest
                    .split(|c: char| !c.is_ascii_hexdigit())
                    .next()
                    .unwrap_or("");
                (token.len() == 64).then(|| format!("sha256:{token}"))
            })
            .ok_or_else(|| {
                format!(
                    "record-index config_identity must carry config_digest sha256:<64 hex>: {config}"
                )
            })?;
        let scenario = row[3].trim();
        if scenario.chars().count() < 10 {
            return Err(format!(
                "record-index command_scenario must name the command/scenario: {scenario}"
            ));
        }
        let claim_class = row[4].trim();
        if !INDEX_CLAIM_CLASSES.contains(&claim_class) {
            return Err(format!(
                "record-index claim_class must be one of {INDEX_CLAIM_CLASSES:?}: {claim_class}"
            ));
        }
        let result = row[5].trim();
        let result_first = result.split_whitespace().next().unwrap_or("");
        if !RESULTS.contains(&result_first) {
            return Err(format!(
                "record-index result must start with pass|fail|unexercised: {result}"
            ));
        }
        if result_first != "pass" && result.trim().chars().count() < MIN_REASON + result_first.len()
        {
            return Err(format!(
                "record-index result {result_first} must carry a retained reason: {result}"
            ));
        }
        // The cited record's own fields must equal the index entry's
        // — substring presence anywhere in the file cannot keep a
        // fixture record under a live_evidenced row, nor a sibling
        // section's values under this row.
        check_record_entry(
            &section_records(&text, label)?,
            &IndexEntry {
                reference,
                tested_revision: shas[0].as_str(),
                host_instance: host,
                repository,
                adapter,
                contract_version,
                config_digest: config_digest.as_str(),
                command_scenario: scenario,
                claim_class,
                result: result_first,
            },
        )?;
        index_claims.insert(reference.to_string(), claim_class.to_string());
        index_results.insert(reference.to_string(), result_first.to_string());
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

    // Claim and result agreement: every ref a row cites must carry
    // the row's claim class and result in the index — ref-set
    // agreement alone cannot keep a live_evidenced row backed by
    // fixture_contract records, nor a pass row backed by a failed
    // record.
    for row in &table {
        let table_claim = row[3].trim();
        let table_result = row[6].trim();
        let (_, refs) = parse_ref_cell(root, &row[2])?;
        for reference in refs {
            let index_claim = index_claims
                .get(&reference)
                .map(String::as_str)
                .unwrap_or("");
            if index_claim != table_claim {
                return Err(format!(
                    "claim_class {table_claim} of {}×{} does not match the record-index \
                     claim_class {index_claim} of {reference}",
                    row[0].trim(),
                    row[1].trim()
                ));
            }
            let index_result = index_results
                .get(&reference)
                .map(String::as_str)
                .unwrap_or("");
            if index_result != table_result {
                return Err(format!(
                    "result {table_result} of {}×{} does not match the record-index \
                     result {index_result} of {reference}",
                    row[0].trim(),
                    row[1].trim()
                ));
            }
        }
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

const IDX_BASE_SHA: &str = "88a2367bfdbf971522ffa49558d9f3af606fc1c9";
const IDX_SIBLING_DIGEST: &str =
    "sha256:c7b73234b63dc9bd11d30231f093b219b4fb40c879aa4562b4ff2195b792f1fa";
const IDX_SIBLING_SCENARIO: &str =
    "u36a-read-path::duplicate_delivery_deduplicates_to_a_single_effect";

/// A record JSON carrying every field an index entry transcribes —
/// the values of the real `u3.6a.md` fixture-github stale-head
/// record the `U36A_INDEX` row cites.
fn fixture_index_record() -> serde_json::Value {
    serde_json::json!({
        "tested_revision": SIBLING_U36A,
        "base_revision": IDX_BASE_SHA,
        "config_identity": {
            "install_identity": {
                "host_instance": "test-host",
                "repository": "local-repo",
                "adapter": "fixture-github",
                "contract_version": 1
            },
            "config_digest": "sha256:fa78ae14d9ef35b83489ed640ccc20cae01bceeec67fe7b4a2372f6c18cfe0b8"
        },
        "command_or_scenario": "u36a-read-path::stale_head_observation_never_rolls_back",
        "result": "pass",
        "claim_class": "fixture_contract"
    })
}

fn fixture_index_entry<'a>(claim_class: &'a str, result: &'a str) -> IndexEntry<'a> {
    IndexEntry {
        reference: U36A_REF,
        tested_revision: SIBLING_U36A,
        host_instance: "test-host",
        repository: "local-repo",
        adapter: "fixture-github",
        contract_version: "1",
        config_digest: "sha256:fa78ae14d9ef35b83489ed640ccc20cae01bceeec67fe7b4a2372f6c18cfe0b8",
        command_scenario: "u36a-read-path::stale_head_observation_never_rolls_back",
        claim_class,
        result,
    }
}

#[test]
fn u36d_index_entry_fields_must_match_the_cited_record() {
    // A record matching every transcribed field passes.
    check_record_entry(
        &[fixture_index_record()],
        &fixture_index_entry("fixture_contract", "pass"),
    )
    .expect("a record matching every index field passes");

    // Each transcribed field is pinned to the cited record's own
    // value: the record's base_revision, a sibling section's
    // installation values and a fabricated result all appear inside
    // the same evidence file, so only field equality rejects them.
    let mismatches: [(IndexEntry<'static>, &str); 9] = [
        (
            IndexEntry {
                tested_revision: IDX_BASE_SHA,
                ..fixture_index_entry("fixture_contract", "pass")
            },
            "tested_revision",
        ),
        (
            IndexEntry {
                host_instance: "sparse-host",
                ..fixture_index_entry("fixture_contract", "pass")
            },
            "config_identity.host_instance",
        ),
        (
            IndexEntry {
                repository: "other/repo",
                ..fixture_index_entry("fixture_contract", "pass")
            },
            "config_identity.repository",
        ),
        (
            IndexEntry {
                adapter: "sparse-host",
                ..fixture_index_entry("fixture_contract", "pass")
            },
            "config_identity.adapter",
        ),
        (
            IndexEntry {
                contract_version: "2",
                ..fixture_index_entry("fixture_contract", "pass")
            },
            "config_identity.contract_version",
        ),
        (
            IndexEntry {
                config_digest: IDX_SIBLING_DIGEST,
                ..fixture_index_entry("fixture_contract", "pass")
            },
            "config_identity.config_digest",
        ),
        (
            IndexEntry {
                command_scenario: IDX_SIBLING_SCENARIO,
                ..fixture_index_entry("fixture_contract", "pass")
            },
            "command_or_scenario",
        ),
        (
            IndexEntry {
                claim_class: "live_evidenced",
                ..fixture_index_entry("live_evidenced", "pass")
            },
            "claim_class",
        ),
        (
            IndexEntry {
                result: "fail",
                ..fixture_index_entry("fixture_contract", "fail")
            },
            "result",
        ),
    ];
    for (entry, label) in mismatches {
        let err = check_record_entry(&[fixture_index_record()], &entry)
            .expect_err("a field the cited record does not carry must fail");
        assert!(
            err.contains(label),
            "the failure names the mismatched field {label}: {err}"
        );
    }

    // A fixture_contract record can never satisfy a live_evidenced
    // index entry — the boundary the substring check alone missed.
    let err = check_record_entry(
        &[fixture_index_record()],
        &fixture_index_entry("live_evidenced", "pass"),
    )
    .expect_err("a fixture record cannot be indexed as live_evidenced");
    assert!(
        err.contains("claim_class"),
        "the failure names the claim-class mismatch: {err}"
    );

    // A live record cannot be indexed as fixture_contract either.
    let mut live_record = fixture_index_record();
    live_record["claim_class"] = serde_json::json!("live_evidenced");
    live_record["authorization_reference"] = serde_json::json!("https://example.test/auth");
    live_record["isolation_scope"] =
        serde_json::json!({"installation_id": "install-1", "repository": "o/r"});
    let err = check_record_entry(
        std::slice::from_ref(&live_record),
        &fixture_index_entry("fixture_contract", "pass"),
    )
    .expect_err("a live record cannot be indexed as fixture_contract");
    assert!(
        err.contains("claim_class"),
        "the failure names the claim-class mismatch: {err}"
    );

    // A live_evidenced record missing its authorization reference or
    // isolation scope fails — the evidence schema's requirement is
    // enforced at the index boundary too.
    for missing in ["authorization_reference", "isolation_scope"] {
        let mut record = live_record.clone();
        record.as_object_mut().unwrap().remove(missing);
        let err = check_record_entry(&[record], &fixture_index_entry("live_evidenced", "pass"))
            .expect_err("a live_evidenced record without its authorization fields must fail");
        assert!(
            err.contains("authorization_reference") && err.contains("isolation_scope"),
            "the failure names the missing authorization fields: {err}"
        );
    }

    // A complete live_evidenced record passes.
    check_record_entry(
        std::slice::from_ref(&live_record),
        &fixture_index_entry("live_evidenced", "pass"),
    )
    .expect("a complete live_evidenced record passes the index check");
}

#[test]
fn u36d_table_claim_class_must_match_the_index() {
    // The consolidated row's claim_class must equal the record-index
    // entry's for every ref it cites — ref-set agreement alone let a
    // live_evidenced row sit on fixture_contract records.
    let mut rows = full_table();
    let pos = rows
        .iter()
        .position(|r| r.contains("| stale_head | fixture |"))
        .expect("the fixture stale_head row exists");
    rows[pos] = row([
        "stale_head",
        "fixture",
        &format!("[\"{U36A_REF}\"]"),
        "live_evidenced",
        SIBLING_U36A,
        "installation test-host/local-repo contract version 1, adapters fixture-github and sparse-host",
        "pass",
        "retained fixture-class records only",
    ]);
    let record = synthetic_record(&rows, U36A_INDEX);
    let err = schema_text(&repo_root(), &record)
        .expect_err("a live_evidenced row backed by fixture_contract records must fail");
    assert!(
        err.contains("does not match the record-index"),
        "the failure names the table/index claim-class mismatch: {err}"
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
fn u36d_schema_rejects_a_row_omitting_a_cited_records_revision() {
    // A row citing records tested at different revisions must
    // transcribe every one of them — dropping a cited record's own
    // tested_revision falsifies the row's revision cell.
    let u36b_ref = "docs/evidence/u3.6b.md#fixture-github — stale_head_guarded_write_rejected";
    let mut rows = full_table();
    let pos = rows
        .iter()
        .position(|r| r.contains("| stale_head | fixture |"))
        .expect("the fixture stale_head row exists");
    rows[pos] = row([
        "stale_head",
        "fixture",
        &format!("[\"{U36A_REF}\", \"{u36b_ref}\"]"),
        "fixture_contract",
        SIBLING_U36A,
        "installation test-host/local-repo contract version 1, adapters fixture-github and sparse-host",
        "pass",
        "retained fixture-class records only",
    ]);
    let record = synthetic_record(&rows, U36A_INDEX);
    let err = schema_text(&repo_root(), &record)
        .expect_err("a row omitting a cited record's tested revision must fail");
    assert!(
        err.contains("missing from the row's tested_revision cell"),
        "the failure names the omitted revision: {err}"
    );
}

#[test]
fn u36d_schema_rejects_a_base_revision_as_tested_revision() {
    // The cited record's own base_revision is transcribed inside the
    // same record JSON — file-wide presence used to accept it as the
    // row's tested revision; the field pin rejects it.
    let base = "88a2367bfdbf971522ffa49558d9f3af606fc1c9";
    let u36a_text =
        read_repo(&repo_root(), "docs/evidence/u3.6a.md").expect("the u3.6a record is retained");
    assert!(
        u36a_text.contains(base),
        "the base revision is transcribed inside the cited record"
    );
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
        base,
        "installation test-host/local-repo contract version 1, adapters fixture-github and sparse-host",
        "pass",
        "retained fixture-class records only",
    ]);
    let record = synthetic_record(&rows, U36A_INDEX);
    let err = schema_text(&repo_root(), &record)
        .expect_err("a base revision transcribed as tested_revision must fail");
    assert!(
        err.contains("appears in no cited record"),
        "the failure names the untranscribed revision: {err}"
    );
}

#[test]
fn u36d_index_fields_must_match_the_cited_record() {
    let root = repo_root();
    // Every index field is pinned to the cited section's record JSON:
    // values that appear elsewhere in the same evidence file — the
    // record's own base_revision, a sibling record's scenario, another
    // installation's digest — cannot satisfy the row.
    let cases: [(&str, &str, &str); 3] = [
        (SIBLING_U36A, IDX_BASE_SHA, "tested_revision"),
        (
            "u36a-read-path::stale_head_observation_never_rolls_back",
            IDX_SIBLING_SCENARIO,
            "command_or_scenario",
        ),
        (
            "| fixture_contract | pass |",
            "| fixture_contract | fail — retained failure reason |",
            "result",
        ),
    ];
    for (from, to, label) in cases {
        let index = U36A_INDEX.replace(from, to);
        let record = synthetic_record(&[fixture_row("stale_head")], &index);
        let err = schema_text(&root, &record)
            .expect_err("an index field the cited record does not carry must fail");
        assert!(
            err.contains(label),
            "the failure names the mismatched field {label}: {err}"
        );
    }
}

#[test]
fn u36d_index_config_must_match_the_cited_record() {
    let root = repo_root();
    // Each config component is pinned to the cited record's own
    // config_identity — a value that appears elsewhere in the same
    // evidence file (the sparse-host sibling record's instance,
    // adapter and digest) cannot satisfy the row.
    let cases: [(&str, &str, &str); 5] = [
        (
            "host_instance test-host",
            "host_instance sparse-host",
            "host_instance",
        ),
        (
            "repository local-repo",
            "repository other/repo",
            "repository",
        ),
        ("adapter fixture-github", "adapter sparse-host", "adapter"),
        (
            "contract version 1",
            "contract version 2",
            "contract_version",
        ),
        (
            "fa78ae14d9ef35b83489ed640ccc20cae01bceeec67fe7b4a2372f6c18cfe0b8",
            "c7b73234b63dc9bd11d30231f093b219b4fb40c879aa4562b4ff2195b792f1fa",
            "config_digest",
        ),
    ];
    for (from, to, label) in cases {
        let index = U36A_INDEX.replace(from, to);
        let record = synthetic_record(&[fixture_row("stale_head")], &index);
        let err = schema_text(&root, &record)
            .expect_err("a config component the cited record does not carry must fail");
        assert!(
            err.contains(label),
            "the failure names the mismatched component {label}: {err}"
        );
    }

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
fn u36d_github_cells_are_all_live_evidenced() {
    let root = repo_root();
    let record = read_repo(&root, RECORD_PATH).expect("the consolidated record is retained");
    let rows = parse_table(&record, &TABLE_HEADER).expect("the consolidated table exists");
    let mut github_rows = 0;
    for row in &rows {
        if row[1].trim() != "github" {
            continue;
        }
        github_rows += 1;
        // #119's authorized live cases are complete: every github
        // cell cites its retained live record — a regression back to
        // `requires_live`/`unexercised` would falsify the record's
        // claim while leaving the schema checks green.
        assert_eq!(
            row[3].trim(),
            "live_evidenced",
            "github cell {} must stay live_evidenced once its authorized run is retained",
            row[0]
        );
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
        github_live, 5,
        "every github cell is live-evidenced — #119's authorized cases are complete"
    );
    assert_eq!(
        github_open, 0,
        "no github cell remains an unexercised finding"
    );
}
