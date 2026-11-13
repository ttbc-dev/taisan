//! U4.6c evidence-record citation check. The swap-window recovery
//! scenarios retain their U4.6 records under `target/`, which is
//! git-ignored; this guard keeps the committed evidence record honest:
//! it must cite the branch commit that contains the exercised slice,
//! transcribe every retained scenario record under that revision, and
//! never defer the stamp behind a placeholder. When the checkout does
//! not retain the cited commit, the guard falls back to verifying that
//! this tree still carries the exercised scenarios, so the fallback
//! cannot pass on the explanation words alone.

use std::path::Path;

const EXERCISED_SLICE_PATH: &str = "crates/workloop-exec/tests/swap_recovery.rs";

/// The named scenarios the slice must carry, cited commit or not.
const EXERCISED_SCENARIOS: [&str; 3] = [
    "u46c_restart_after_worker_argv_change_reconciles_in_flight_on_recorded_bindings",
    "u46c_interrupted_export_import_recovers_under_changed_config_without_duplicate_artifact_ids",
    "u46c_recollection_acknowledges_and_retains_git_objects_under_changed_config",
];

fn git_succeeds(root: &Path, args: &[&str]) -> bool {
    std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .map(|out| out.status.success())
        .unwrap_or(false)
}

/// One git query in `root` whose trimmed stdout, `None` on failure.
fn git_stdout(root: &Path, args: &[&str]) -> Option<String> {
    std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .ok()
        .filter(|out| out.status.success())
        .and_then(|out| String::from_utf8(out.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn assert_carries_exercised_scenarios(slice: &str, where_: &str) {
    for scenario in EXERCISED_SCENARIOS {
        assert!(
            slice.contains(scenario),
            "the exercised slice {where_} carries the named scenario {scenario}"
        );
    }
}

fn full_commit_id<'a>(row: &'a str, what: &str) -> &'a str {
    let sha = row
        .split('`')
        .nth(1)
        .unwrap_or_else(|| panic!("{what} is a backticked commit id: {row}"));
    assert_eq!(sha.len(), 40, "{what} is a full commit id: {sha}");
    assert!(
        sha.chars().all(|c| c.is_ascii_hexdigit()),
        "{what} is a hex commit id: {sha}"
    );
    sha
}

#[test]
fn u46c_evidence_record_cites_the_tested_revision() {
    let evidence = include_str!("../../../docs/evidence/u4.6c.md");
    let tested_row = evidence
        .lines()
        .find(|line| line.contains("Tested revision (slice commit"))
        .expect("evidence names a tested revision");
    let tested = full_commit_id(tested_row, "tested revision");
    let base_row = evidence
        .lines()
        .find(|line| line.contains("Base revision"))
        .expect("evidence names a base revision");
    let base = full_commit_id(base_row, "base revision");

    assert!(
        evidence.contains(EXERCISED_SLICE_PATH),
        "evidence names the exercised slice path"
    );
    assert_eq!(
        evidence
            .matches(&format!("\"tested_revision\": \"{tested}\""))
            .count(),
        3,
        "each retained scenario record is transcribed under the tested revision"
    );
    assert_eq!(
        evidence
            .matches(&format!("\"base_revision\": \"{base}\""))
            .count(),
        3,
        "each retained scenario record is transcribed under the base revision"
    );

    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let commit = format!("{tested}^{{commit}}");
    if !git_succeeds(&root, &["cat-file", "-e", &commit]) {
        // The cited commit is absent from this checkout. The record
        // must explain the omission, and this tree's slice must still
        // carry the exercised scenarios — the fallback never passes on
        // the explanation words alone.
        let lower = evidence.to_ascii_lowercase();
        assert!(
            lower.contains("branch commit") && lower.contains("shallow"),
            "an absent tested revision must be explained as a branch commit a shallow checkout may omit"
        );
        let slice_here = std::fs::read_to_string(root.join(EXERCISED_SLICE_PATH))
            .expect("the exercised slice must be readable in this tree");
        assert_carries_exercised_scenarios(&slice_here, "in this tree");
        return;
    }
    // The checkout retains the cited commit: its slice must be the
    // exercised suite, not merely an existing path, and the commit
    // must sit on the branch history leading to HEAD.
    let slice = git_stdout(
        &root,
        &["show", &format!("{tested}:{EXERCISED_SLICE_PATH}")],
    )
    .unwrap_or_else(|| panic!("tested revision {tested} must contain the exercised slice"));
    assert_carries_exercised_scenarios(&slice, &format!("at {tested}"));
    assert!(
        git_succeeds(&root, &["merge-base", "--is-ancestor", tested, "HEAD"]),
        "tested revision {tested} must be on the branch history leading to HEAD"
    );
}
