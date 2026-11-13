//! U4.6c evidence-record citation check. The swap-window recovery
//! scenarios retain their U4.6 records under `target/`, which is
//! git-ignored; this guard keeps the committed evidence record honest:
//! it must cite the branch commit that contains the exercised slice,
//! transcribe every retained scenario record under that revision, and
//! never defer the stamp behind a placeholder.

use std::path::Path;

const EXERCISED_SLICE_PATH: &str = "crates/workloop-exec/tests/swap_recovery.rs";

fn git_succeeds(root: &Path, args: &[&str]) -> bool {
    std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .map(|out| out.status.success())
        .unwrap_or(false)
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
        let lower = evidence.to_ascii_lowercase();
        assert!(
            lower.contains("branch commit") && lower.contains("shallow"),
            "an absent tested revision must be explained as a branch commit a shallow checkout may omit"
        );
        return;
    }
    let slice = format!("{tested}:{EXERCISED_SLICE_PATH}");
    assert!(
        git_succeeds(&root, &["cat-file", "-e", &slice]),
        "tested revision {tested} must contain the exercised slice"
    );
    assert!(
        git_succeeds(&root, &["merge-base", "--is-ancestor", tested, "HEAD"]),
        "tested revision {tested} must be on the branch history leading to HEAD"
    );
}
