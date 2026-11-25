//! Contract coverage for [`GhCliApi`] — the `gh` CLI transport — driven
//! entirely by stub `gh` executables. No test here touches the network:
//! each stub is a shell script that answers `gh api` argv with canned
//! JSON, canned HTTP diagnostics, or transport-level failures, which is
//! what makes the failure-mode surface (refusal, lost response, stale
//! guard, spawn failure, timeout) deterministically testable — the
//! property the live evidence run depends on.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use workloop_core::identity::{
    ChangeRequestId, CredentialRef, HostInstanceId, IssueId, IssueRef, RepositoryId, RepositoryRef,
};
use workloop_core::observation::{CheckConclusion, IssueState};
use workloop_github::adapter::GitHubAdapter;
use workloop_github::api::{ApiError, GitHubApi};
use workloop_github::credential::{OsProviders, resolve_credential};
use workloop_github::gh_cli::GhCliApi;

fn repository() -> RepositoryRef {
    RepositoryRef::new(
        HostInstanceId::trusted("install-u36e"),
        RepositoryId::trusted("owner/repo"),
    )
}

fn change_id() -> ChangeRequestId {
    ChangeRequestId::trusted("PR_node_1")
}

/// Write an executable stub `gh` script into `dir` and return its
/// path. `body` is the script's case logic; the harness prepends the
/// argv/stdin log lines every stub shares.
fn stub_gh(dir: &Path, name: &str, body: &str) -> PathBuf {
    let path = dir.join(name);
    let dir_display = dir.display();
    let script = format!(
        "#!/bin/sh\nlog=\"{dir_display}/calls.log\"\nstdin_file=\"{dir_display}/stdin.json\"\necho \"$@\" >> \"$log\"\ncat > \"$stdin_file\" 2>/dev/null || true\n{body}"
    );
    // Write-then-rename: executing a file whose inode was just opened
    // for writing fails ETXTBUSY on Linux; the renamed inode never was.
    let tmp = dir.join(format!("{name}.tmp"));
    fs::write(&tmp, script).unwrap();
    let mut perms = fs::metadata(&tmp).unwrap().permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&tmp, perms).unwrap();
    fs::rename(&tmp, &path).unwrap();
    path
}

fn tempdir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("gh-cli-test-{}-{}", tag, std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn calls(dir: &Path) -> String {
    fs::read_to_string(dir.join("calls.log")).unwrap_or_default()
}

fn stdin_json(dir: &Path) -> String {
    fs::read_to_string(dir.join("stdin.json")).unwrap_or_default()
}

/// Write an executable stub `git` into `dir`: argv lines go to
/// `git-calls.log` and the child's environment to `git-env.log`, so
/// tests assert both the push shape and the credential hygiene.
fn stub_git(dir: &Path, body: &str) -> PathBuf {
    let path = dir.join("git");
    let dir_display = dir.display();
    let script = format!(
        "#!/bin/sh\nlog=\"{dir_display}/git-calls.log\"\nenv_file=\"{dir_display}/git-env.log\"\necho \"$@\" >> \"$log\"\nenv > \"$env_file\"\n{body}"
    );
    let tmp = dir.join("git.tmp");
    fs::write(&tmp, script).unwrap();
    let mut perms = fs::metadata(&tmp).unwrap().permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&tmp, perms).unwrap();
    fs::rename(&tmp, &path).unwrap();
    path
}

fn git_calls(dir: &Path) -> String {
    fs::read_to_string(dir.join("git-calls.log")).unwrap_or_default()
}

fn git_env(dir: &Path) -> String {
    fs::read_to_string(dir.join("git-env.log")).unwrap_or_default()
}

#[test]
fn the_adapter_composes_over_the_transport() {
    // Compile-level proof the seam holds: the real adapter runs
    // unchanged over the gh transport.
    let _adapter: GitHubAdapter<GhCliApi> = GitHubAdapter::new(GhCliApi::new());
}

#[test]
fn read_branch_returns_the_head_and_none_on_404() {
    let dir = tempdir("read-branch");
    let gh = stub_gh(
        &dir,
        "gh",
        r#"
case "$2" in
  repos/owner/repo/git/ref/heads/b) echo '{"ref":"refs/heads/b","object":{"sha":"aaa111","type":"commit"}}' ;;
  repos/owner/repo/git/ref/heads/absent) echo 'gh: Not Found (HTTP 404)' >&2; exit 1 ;;
esac
"#,
    );
    let mut api = GhCliApi::with_gh(gh);
    let repo = repository();
    assert_eq!(
        api.read_branch(&repo, "b").unwrap(),
        Some("aaa111".to_string())
    );
    assert_eq!(api.read_branch(&repo, "absent").unwrap(), None);
}

#[test]
fn read_repository_follows_the_default_branch() {
    let dir = tempdir("read-repo");
    let gh = stub_gh(
        &dir,
        "gh",
        r#"
case "$2" in
  repos/owner/repo) echo '{"default_branch":"trunk"}' ;;
  repos/owner/repo/git/ref/heads/trunk) echo '{"object":{"sha":"def456"}}' ;;
esac
"#,
    );
    let mut api = GhCliApi::with_gh(gh);
    assert_eq!(
        api.read_repository(&repository()).unwrap(),
        Some("def456".to_string())
    );
}

#[test]
fn write_ref_refuses_a_stale_head_before_any_write() {
    let dir = tempdir("stale-head");
    let gh = stub_gh(
        &dir,
        "gh",
        r#"
case "$2" in
  repos/owner/repo/git/ref/heads/b) echo '{"object":{"sha":"moved999"}}' ;;
esac
exit 0
"#,
    );
    let mut api = GhCliApi::with_gh(gh);
    let err = api
        .write_ref(&repository(), "b", "new000", Some("expected111"))
        .unwrap_err();
    assert_eq!(
        err,
        ApiError::Refused {
            reason: "stale_head".into()
        },
        "the guarded write must refuse with the stale_head reason the adapter matches"
    );
    assert!(
        !calls(&dir).contains("PATCH") && !calls(&dir).contains("POST"),
        "no write may be issued after the guard fails: {}",
        calls(&dir)
    );
}

#[test]
fn write_ref_patches_with_force_false_when_the_guard_holds() {
    let dir = tempdir("guarded-write");
    let gh = stub_gh(
        &dir,
        "gh",
        r#"
case "$2" in
  repos/owner/repo/git/ref/heads/b)
    if [ "$4" = "PATCH" ]; then echo '{"object":{"sha":"new000"}}'; else echo '{"object":{"sha":"expected111"}}'; fi ;;
  repos/owner/repo/git/refs/heads/b) echo '{"object":{"sha":"new000"}}' ;;
esac
"#,
    );
    let mut api = GhCliApi::with_gh(gh);
    let head = api
        .write_ref(&repository(), "b", "new000", Some("expected111"))
        .unwrap();
    assert_eq!(head, "new000");
    let body = stdin_json(&dir);
    assert!(
        body.contains("\"force\":false") && body.contains("\"sha\":\"new000\""),
        "the PATCH must carry the new sha with server-side ff enforcement: {body}"
    );
}

#[test]
fn write_ref_creates_the_ref_when_the_branch_is_absent() {
    let dir = tempdir("create-ref");
    let gh = stub_gh(
        &dir,
        "gh",
        r#"
case "$2" in
  repos/owner/repo/git/ref/heads/new-branch) echo 'gh: Not Found (HTTP 404)' >&2; exit 1 ;;
  repos/owner/repo/git/refs) echo '{"ref":"refs/heads/new-branch","object":{"sha":"seed000"}}' ;;
esac
"#,
    );
    let mut api = GhCliApi::with_gh(gh);
    let head = api
        .write_ref(&repository(), "new-branch", "seed000", None)
        .unwrap();
    assert_eq!(head, "seed000");
    let log = calls(&dir);
    assert!(
        log.contains("git/refs"),
        "create path posts git/refs: {log}"
    );
    assert!(
        stdin_json(&dir).contains("refs/heads/new-branch"),
        "the create body names the full ref"
    );
}

#[test]
fn write_ref_maps_4xx_to_refused_and_5xx_to_unknown() {
    let dir = tempdir("write-classes");
    // The stub reads its failure mode from a file the test rewrites —
    // no process-env mutation, so parallel tests cannot race it.
    fs::write(dir.join("mode.txt"), "422").unwrap();
    let gh = stub_gh(
        &dir,
        "gh",
        r#"
case "$2" in
  repos/owner/repo/git/ref/heads/b) echo '{"object":{"sha":"expected111"}}' ;;
  repos/owner/repo/git/refs/heads/b)
    if [ "$4" = "PATCH" ]; then
      mode=$(cat "MODE_FILE" 2>/dev/null || echo 422)
      case "$mode" in
        422) echo 'gh: Validation Failed (HTTP 422)' >&2; exit 1 ;;
        500) echo 'gh: Server Error (HTTP 500)' >&2; exit 1 ;;
      esac
    fi ;;
esac
exit 0
"#,
    );
    // Rewrite the placeholder with the real mode-file path.
    let script = fs::read_to_string(&gh)
        .unwrap()
        .replace("MODE_FILE", &format!("{}", dir.join("mode.txt").display()));
    fs::write(&gh, script).unwrap();
    // 422 → definite refusal (head still matches, so not stale_head).
    let mut api = GhCliApi::with_gh(&gh);
    let err = api
        .write_ref(&repository(), "b", "new000", Some("expected111"))
        .unwrap_err();
    assert!(
        matches!(err, ApiError::Refused { .. }),
        "a 422 is a definite refusal: {err:?}"
    );
    assert_ne!(
        err,
        ApiError::Refused {
            reason: "stale_head".into()
        },
        "a non-guard 422 must not masquerade as stale_head"
    );
    // 500 → unknown: the write may have landed.
    fs::write(dir.join("mode.txt"), "500").unwrap();
    let mut api = GhCliApi::with_gh(&gh);
    let err = api
        .write_ref(&repository(), "b", "new000", Some("expected111"))
        .unwrap_err();
    assert!(
        matches!(err, ApiError::Unknown { .. }),
        "a 5xx is unknown, never a failed claim: {err:?}"
    );
}

#[test]
fn transport_failures_classify_honestly() {
    let dir = tempdir("failure-classes");
    // Spawn failure: the binary does not exist — definite no-effect.
    let mut api = GhCliApi::with_gh(dir.join("no-such-gh"));
    let err = api.read_branch(&repository(), "b").unwrap_err();
    assert!(
        matches!(err, ApiError::Refused { .. }),
        "a spawn failure never reached the host: {err:?}"
    );

    // Transport-level failure with no HTTP status — the request may
    // have been issued: unknown.
    let gh = stub_gh(
        &dir,
        "gh",
        "echo 'dial tcp: connection reset by peer' >&2\nexit 1\n",
    );
    let mut api = GhCliApi::with_gh(gh);
    let err = api.read_branch(&repository(), "b").unwrap_err();
    assert!(
        matches!(err, ApiError::Unknown { .. }),
        "a lost transport response is unknown: {err:?}"
    );

    // A garbled success body is unknown — the answer cannot be trusted.
    let gh = stub_gh(&dir, "gh2", "echo 'this is not json'\nexit 0\n");
    let mut api = GhCliApi::with_gh(gh);
    let err = api.read_branch(&repository(), "b").unwrap_err();
    assert!(
        matches!(err, ApiError::Unknown { .. }),
        "an unparseable response is unknown: {err:?}"
    );
}

#[test]
fn a_hung_transport_times_out_as_unknown() {
    let dir = tempdir("timeout");
    let gh = stub_gh(&dir, "gh", "sleep 30\nexit 0\n");
    let mut api = GhCliApi::with_gh(gh).with_timeout(std::time::Duration::from_millis(200));
    let err = api.read_branch(&repository(), "b").unwrap_err();
    assert!(
        matches!(err, ApiError::Unknown { .. }),
        "a timed-out call is unknown, not a hang: {err:?}"
    );
}

#[test]
fn no_credential_value_reaches_argv() {
    let dir = tempdir("secret-hygiene");
    let gh = stub_gh(
        &dir,
        "gh",
        r#"
case "$2" in
  repos/owner/repo/git/ref/heads/b) echo '{"object":{"sha":"aaa111"}}' ;;
esac
"#,
    );
    // SAFETY: the variable is set and removed within this test only;
    // no other test reads GH_TOKEN, and the child inherits the
    // environment snapshot at spawn.
    unsafe { std::env::set_var("GH_TOKEN", "ghp_supersecretvalue123") };
    let mut api = GhCliApi::with_gh(gh);
    api.read_branch(&repository(), "b").unwrap();
    unsafe { std::env::remove_var("GH_TOKEN") };
    let log = calls(&dir);
    assert!(
        !log.contains("ghp_supersecretvalue123"),
        "the token must never appear in argv: {log}"
    );
}

#[test]
fn a_non_slug_repository_is_refused_before_any_subprocess() {
    let dir = tempdir("bad-slug");
    let gh = stub_gh(&dir, "gh", "exit 0\n");
    let mut api = GhCliApi::with_gh(gh);
    let repo = RepositoryRef::new(
        HostInstanceId::trusted("install-u36e"),
        RepositoryId::trusted("not-a-slug"),
    );
    let err = api.read_branch(&repo, "b").unwrap_err();
    assert!(matches!(err, ApiError::Refused { .. }));
    assert!(calls(&dir).is_empty(), "no subprocess for a bad slug");
}

#[test]
fn unsafe_names_are_refused_before_any_subprocess() {
    let dir = tempdir("bad-names");
    let gh = stub_gh(&dir, "gh", "exit 0\n");
    let mut api = GhCliApi::with_gh(gh);
    let repo = repository();

    // Branch names that could reach a request path or query.
    for branch in ["a?b", "a&state=closed", "a#frag", "a%2e%2e", "a b", ""] {
        for result in [
            api.read_branch(&repo, branch).map(|_| ()),
            api.write_ref(&repo, branch, "new000", None).map(|_| ()),
            api.list_pulls_on_branch(&repo, branch, "want1").map(|_| ()),
        ] {
            assert!(
                matches!(result, Err(ApiError::Refused { .. })),
                "branch {branch:?} must refuse before any subprocess"
            );
        }
    }
    // Revisions interpolated into request paths.
    for revision in ["a?b", "a/b?c", "a#x", "a&x=1", ""] {
        assert!(matches!(
            api.check_runs(&repo, revision),
            Err(ApiError::Refused { .. })
        ));
        assert!(matches!(
            api.contains_revision(&repo, revision, "ok1"),
            Err(ApiError::Refused { .. })
        ));
        assert!(matches!(
            api.write_ref(&repo, "b", revision, None),
            Err(ApiError::Refused { .. })
        ));
    }
    // Markers that could break out of the HTML-comment envelope.
    assert!(matches!(
        api.comment_on_pull(&repo, &change_id(), "x-->", "b"),
        Err(ApiError::Refused { .. })
    ));
    assert!(calls(&dir).is_empty(), "no subprocess for unsafe input");
}

#[test]
fn open_pull_maps_the_remote_pull_identity() {
    let dir = tempdir("open-pull");
    let gh = stub_gh(
        &dir,
        "gh",
        r#"
case "$2" in
  repos/owner/repo/pulls) echo '{"node_id":"PR_node_9","number":9,"state":"open","merged":false,"title":"t","body":"b","head":{"sha":"h1","ref":"feature/x"},"base":{"ref":"main"}}' ;;
esac
"#,
    );
    let mut api = GhCliApi::with_gh(&gh);
    let pull = api
        .open_pull(&repository(), "feature/x", "main", "t", "b")
        .unwrap();
    assert_eq!(pull.id, "PR_node_9", "the stable node id is the identity");
    assert_eq!(pull.display_number, 9);
    assert_eq!(pull.head_sha, "h1");
    assert_eq!(pull.source_branch, "feature/x");
    assert_eq!(pull.target_branch, "main");
    // The request body carried the PR fields, never a credential.
    let body = stdin_json(&dir);
    assert!(body.contains("feature/x") && body.contains("main"));
}

#[test]
fn comment_marker_round_trips_through_find_comment() {
    let dir = tempdir("comment-marker");
    let gh = stub_gh(
        &dir,
        "gh",
        r#"
case "$2" in
  graphql) echo '{"data":{"node":{"number":7,"repository":{"owner":{"login":"owner"},"name":"repo"}}}}' ;;
  repos/owner/repo/issues/7/comments)
    if echo "$@" | grep -q -- '--paginate'; then
      # printf, not echo: BSD echo would expand the \n escapes into
      # raw control characters and corrupt the JSON.
      printf '%s\n' '[[{"id":41,"body":"hello\n\n<!-- workloop:m1 -->"},{"id":42,"body":"other"}]]'
    else
      echo '{"id":41,"body":"echoed"}'
    fi ;;
esac
"#,
    );
    let mut api = GhCliApi::with_gh(gh);
    let repo = repository();
    let change = change_id();

    // Post: the marker rides inside the body as an HTML comment.
    api.comment_on_pull(&repo, &change, "workloop:m1", "hello")
        .unwrap();
    let posted = stdin_json(&dir);
    assert!(
        posted.contains("<!-- workloop:m1 -->"),
        "the posted body carries the marker envelope: {posted}"
    );

    // Find: the envelope locates the stored comment after a lost
    // response.
    let found = api.find_comment(&repo, &change, "workloop:m1").unwrap();
    let found = found.expect("the marked comment reconciles by marker");
    assert_eq!(found.comment_id, "41");
    assert!(found.body.contains("<!-- workloop:m1 -->"));

    // An absent marker finds nothing — never an adoption.
    assert!(
        api.find_comment(&repo, &change, "workloop:other")
            .unwrap()
            .is_none()
    );
}

#[test]
fn a_node_id_from_another_repository_is_refused() {
    let dir = tempdir("foreign-node");
    let gh = stub_gh(
        &dir,
        "gh",
        r#"
case "$1 $2" in
  "api graphql") echo '{"data":{"node":{"number":7,"repository":{"owner":{"login":"someone"},"name":"else"}}}}' ;;
esac
"#,
    );
    let mut api = GhCliApi::with_gh(gh);
    let err = api.read_pull(&repository(), &change_id()).unwrap_err();
    assert!(
        matches!(err, ApiError::Refused { .. }),
        "a foreign node id is a definite refusal, never a wrong-repo read: {err:?}"
    );
}

#[test]
fn reviews_and_findings_join_the_review_commit() {
    let dir = tempdir("reviews");
    let gh = stub_gh(
        &dir,
        "gh",
        r#"
case "$2" in
  graphql) echo '{"data":{"node":{"number":7,"repository":{"owner":{"login":"owner"},"name":"repo"}}}}' ;;
  repos/owner/repo/pulls/7/reviews)
    if echo "$@" | grep -q -- '--paginate'; then
      echo '[[{"id":100,"commit_id":"rev-a","state":"CHANGES_REQUESTED","body":"fix it","user":{"login":"rev1"}}]]'
    fi ;;
  repos/owner/repo/pulls/7/comments)
    if echo "$@" | grep -q -- '--paginate'; then
      echo '[[{"id":55,"pull_request_review_id":100,"body":"inline","original_commit_id":"rev-x"}]]'
    fi ;;
esac
"#,
    );
    let mut api = GhCliApi::with_gh(gh);
    let repo = repository();
    let change = change_id();

    let reviews = api.reviews(&repo, &change).unwrap();
    assert_eq!(reviews.len(), 1);
    assert_eq!(reviews[0].commit_sha, "rev-a");
    assert!(reviews[0].changes_requested);
    assert_eq!(reviews[0].author, "rev1");

    let findings = api.review_findings(&repo, &change).unwrap();
    assert_eq!(findings.len(), 1);
    assert_eq!(findings[0].finding_id, "55");
    assert_eq!(findings[0].review_id, "100");
    // The finding's commit is its review's examined revision — not
    // the comment's own original_commit_id fallback.
    assert_eq!(findings[0].commit_sha, "rev-a");
}

#[test]
fn check_runs_map_status_and_conclusion() {
    let dir = tempdir("checks");
    let gh = stub_gh(
        &dir,
        "gh",
        r#"
case "$2" in
  repos/owner/repo/commits/head1/check-runs*)
    if echo "$@" | grep -q -- '--paginate'; then
      echo '[{"total_count":3,"check_runs":[{"id":1,"name":"ci","head_sha":"head1","status":"completed","conclusion":"success","app":{"slug":"github-actions"}},{"id":2,"name":"lint","head_sha":"head1","status":"completed","conclusion":"failure","app":{"slug":"github-actions"}},{"id":3,"name":"pending","head_sha":"head1","status":"in_progress","conclusion":null,"app":{"slug":"other"}}]}]'
    fi ;;
esac
"#,
    );
    let mut api = GhCliApi::with_gh(gh);
    let runs = api.check_runs(&repository(), "head1").unwrap();
    assert_eq!(runs.len(), 3);
    assert_eq!(runs[0].conclusion, CheckConclusion::Success);
    assert_eq!(runs[0].producer, "github-actions");
    assert_eq!(runs[1].conclusion, CheckConclusion::Failure);
    assert_eq!(runs[2].conclusion, CheckConclusion::Pending);
    assert!(runs.iter().all(|r| r.head_sha == "head1"));
}

#[test]
fn contains_revision_maps_the_compare_status() {
    let dir = tempdir("compare");
    let gh = stub_gh(
        &dir,
        "gh",
        r#"
case "$2" in
  repos/owner/repo/compare/a...b) echo '{"status":"ahead"}' ;;
  repos/owner/repo/compare/a...c) echo '{"status":"diverged"}' ;;
esac
"#,
    );
    let mut api = GhCliApi::with_gh(gh);
    let repo = repository();
    assert!(api.contains_revision(&repo, "a", "b").unwrap());
    assert!(!api.contains_revision(&repo, "a", "c").unwrap());
}

/// Issue #450: a produced commit that is not pushed yet makes the
/// compare read 404 — "commit unknown to the remote" is a definite
/// not-contained, so a divergent republish reconciles to `Absent` and
/// the leased write proceeds. Only the read itself failing may error.
#[test]
fn contains_revision_treats_an_unknown_revision_as_absent_not_failed() {
    let dir = tempdir("compare-404");
    let gh = stub_gh(
        &dir,
        "gh",
        r#"
case "$2" in
  repos/owner/repo/compare/new1...head9) echo 'gh: Not Found (HTTP 404)' >&2; exit 1 ;;
  repos/owner/repo/compare/a...broken) echo 'gh: Server Error (HTTP 500)' >&2; exit 1 ;;
  repos/owner/repo/compare/a...denied) echo 'gh: Forbidden (HTTP 403)' >&2; exit 1 ;;
  repos/owner/repo/compare/a...lost) echo 'gh: connection reset' >&2; exit 1 ;;
esac
"#,
    );
    let mut api = GhCliApi::with_gh(gh);
    let repo = repository();
    assert!(
        !api.contains_revision(&repo, "new1", "head9").unwrap(),
        "a revision the remote does not know is a definite absence"
    );
    assert!(
        matches!(
            api.contains_revision(&repo, "a", "broken"),
            Err(ApiError::Unknown { .. })
        ),
        "a server-side failure stays an unreadable read"
    );
    assert!(
        matches!(
            api.contains_revision(&repo, "a", "denied"),
            Err(ApiError::Refused { .. })
        ),
        "a non-404 refusal is not an absence"
    );
    assert!(
        matches!(
            api.contains_revision(&repo, "a", "lost"),
            Err(ApiError::Unknown { .. })
        ),
        "a lost response is never an absence"
    );
}

#[test]
fn list_pulls_on_branch_filters_by_head_sha() {
    let dir = tempdir("list-pulls");
    let gh = stub_gh(
        &dir,
        "gh",
        r#"
case "$2" in
  repos/owner/repo/pulls)
    if echo "$@" | grep -q -- '--paginate'; then
      echo '[[{"node_id":"PR_a","number":1,"state":"open","merged":false,"title":"t","body":"b","head":{"sha":"want1","ref":"feature/x"},"base":{"ref":"main"}},{"node_id":"PR_b","number":2,"state":"open","merged":false,"title":"t","body":"b","head":{"sha":"other9","ref":"feature/x"},"base":{"ref":"main"}}]]'
    fi ;;
esac
"#,
    );
    let mut api = GhCliApi::with_gh(gh);
    let pulls = api
        .list_pulls_on_branch(&repository(), "feature/x", "want1")
        .unwrap();
    assert_eq!(pulls.len(), 1, "only the recorded head revision matches");
    assert_eq!(pulls[0].id, "PR_a");
    // The branch filter rides `-f` fields, never a hand-joined query.
    let log = calls(&dir);
    assert!(
        log.contains("-f head=owner:feature/x") && log.contains("-f state=all"),
        "pull filters must be encoded fields: {log}"
    );
}

#[test]
fn read_pull_resolves_the_node_and_reads_the_pull() {
    let dir = tempdir("read-pull");
    let gh = stub_gh(
        &dir,
        "gh",
        r#"
case "$1 $2" in
  "api graphql") echo '{"data":{"node":{"number":7,"repository":{"owner":{"login":"owner"},"name":"repo"}}}}' ;;
esac
case "$2" in
  repos/owner/repo/pulls/7) echo '{"node_id":"PR_node_1","number":7,"state":"open","merged":false,"title":"t","body":"b","head":{"sha":"h9","ref":"feature/x"},"base":{"ref":"main"}}' ;;
esac
"#,
    );
    let mut api = GhCliApi::with_gh(gh);
    let pull = api.read_pull(&repository(), &change_id()).unwrap();
    assert_eq!(pull.id, "PR_node_1");
    assert_eq!(pull.head_sha, "h9");
    // The resolution is cached: a second read issues no GraphQL call.
    api.read_pull(&repository(), &change_id()).unwrap();
    let log = calls(&dir);
    assert_eq!(
        log.matches("graphql").count(),
        1,
        "node→number resolves once: {log}"
    );
}

#[test]
fn read_branch_maps_a_definite_4xx_to_refused() {
    let dir = tempdir("read-branch-4xx");
    let gh = stub_gh(
        &dir,
        "gh",
        r#"
case "$2" in
  repos/owner/repo/git/ref/heads/b) echo 'gh: Forbidden (HTTP 403)' >&2; exit 1 ;;
esac
"#,
    );
    let mut api = GhCliApi::with_gh(gh);
    let err = api.read_branch(&repository(), "b").unwrap_err();
    assert!(
        matches!(err, ApiError::Refused { .. }),
        "a permission-denied read is a definite refusal: {err:?}"
    );
}

#[test]
fn a_response_larger_than_the_pipe_buffer_still_completes() {
    let dir = tempdir("large-output");
    // ~300 KiB of JSON — far past the OS pipe buffer. A transport
    // that only drains stdout after the child exits deadlocks here
    // until the timeout; the reader threads must keep up.
    let gh = stub_gh(
        &dir,
        "gh",
        r#"
case "$2" in
  repos/owner/repo)
    printf '{"default_branch":"main","pad":"'
    awk 'BEGIN{for(i=0;i<30000;i++)printf "xxxxxxxxxx"}'
    printf '"}'
    ;;
  repos/owner/repo/git/ref/heads/main) echo '{"object":{"sha":"abc123"}}' ;;
esac
"#,
    );
    let mut api = GhCliApi::with_gh(gh).with_timeout(std::time::Duration::from_secs(30));
    assert_eq!(
        api.read_repository(&repository()).unwrap(),
        Some("abc123".to_string())
    );
}

#[test]
fn read_issue_returns_validated_state_revision_and_digest() {
    let dir = tempdir("read-issue");
    let gh = stub_gh(
        &dir,
        "gh",
        r#"
case "$2" in
  repos/owner/repo/issues/7) echo '{"number":7,"state":"open","title":"hello","body":"world","updated_at":"2026-01-01T00:00:00Z"}' ;;
  repos/owner/repo/issues/8) echo '{"number":8,"state":"closed","title":"t","body":null,"updated_at":"r2"}' ;;
  repos/owner/repo/issues/9) echo '{"number":10,"state":"open","title":"t","body":"b","updated_at":"r3"}' ;;
  repos/owner/repo/issues/11) echo '{"number":11,"state":"weird","title":"t","body":"b","updated_at":"r4"}' ;;
  repos/owner/repo/issues/12) echo '{"number":12,"state":"open","title":"t","body":"b"}' ;;
  repos/owner/repo/issues/13) echo '{"number":13,"state":"open","title":"t","body":["a","b"],"updated_at":"r5"}' ;;
  repos/owner/repo/issues/14) echo '{"number":14,"state":"open","title":"t","body":42,"updated_at":"r6"}' ;;
esac
"#,
    );
    let mut api = GhCliApi::with_gh(gh);
    let make = |number: u64| IssueRef {
        repository: repository(),
        issue: IssueId::trusted(number.to_string()),
        display_number: Some(number),
    };
    let open = api.read_issue(&make(7)).unwrap();
    assert_eq!(open.source_revision, "2026-01-01T00:00:00Z");
    assert_eq!(open.state, IssueState::Open);
    assert_eq!(
        open.content_digest,
        workloop_selector::issue_content_digest("hello", "world"),
        "the digest is the shared selector convention"
    );
    let closed = api.read_issue(&make(8)).unwrap();
    assert_eq!(closed.state, IssueState::Closed);
    assert_eq!(
        closed.content_digest,
        workloop_selector::issue_content_digest("t", ""),
        "a null body digests as empty, matching the selector convention"
    );
    for (number, why) in [
        (9, "a mismatched response number"),
        (11, "an unrecognised state"),
        (12, "a missing updated_at"),
        (13, "an array body"),
        (14, "a numeric body"),
    ] {
        assert!(
            matches!(api.read_issue(&make(number)), Err(ApiError::Refused { .. })),
            "{why} refuses: issue {number}"
        );
    }
    let log = calls(&dir);
    assert!(log.contains("repos/owner/repo/issues/7"), "endpoint: {log}");
    assert!(!log.contains("issues/$2"), "no unvalidated endpoint: {log}");
}

#[test]
fn read_issue_refuses_malformed_identities_before_any_request() {
    let dir = tempdir("read-issue-bad-id");
    let gh = stub_gh(&dir, "gh", "echo '{}' ");
    let mut api = GhCliApi::with_gh(gh);
    for id in ["abc", "0", "-1", "1;rm", "1/../2"] {
        let issue = IssueRef {
            repository: repository(),
            issue: IssueId::trusted(id),
            display_number: None,
        };
        assert!(
            matches!(api.read_issue(&issue), Err(ApiError::Refused { .. })),
            "identity {id:?} refuses"
        );
    }
    let bad_repo = IssueRef {
        repository: RepositoryRef::new(
            HostInstanceId::trusted("install-u36e"),
            RepositoryId::trusted("not-a-slug"),
        ),
        issue: IssueId::trusted("7"),
        display_number: Some(7),
    };
    assert!(matches!(
        api.read_issue(&bad_repo),
        Err(ApiError::Refused { .. })
    ));
    assert!(
        calls(&dir).is_empty(),
        "no request is issued for refused identities"
    );
}

#[test]
fn a_pinned_hostname_survives_an_environment_retarget() {
    if std::env::var("GH_HOST").ok().as_deref() != Some("evil.example.com") {
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "a_pinned_hostname_survives_an_environment_retarget",
                "--exact",
                "--nocapture",
            ])
            .env("GH_HOST", "evil.example.com")
            .status()
            .unwrap();
        assert!(status.success(), "the hostile-GH_HOST child run failed");
        return;
    }
    let dir = tempdir("hostname-pin");
    let gh = stub_gh(
        &dir,
        "gh",
        r#"
echo "GH_HOST=$GH_HOST" >> "$log.env"
case "$2" in
  repos/owner/repo/issues/7) echo '{"number":7,"state":"open","title":"t","body":"b","updated_at":"r1"}' ;;
esac
"#,
    );
    let mut api = GhCliApi::with_gh(gh).with_hostname("github.com");
    let issue = IssueRef {
        repository: repository(),
        issue: IssueId::trusted("7"),
        display_number: Some(7),
    };
    let remote = api.read_issue(&issue).unwrap();
    assert_eq!(remote.state, IssueState::Open);
    let log = calls(&dir);
    assert!(
        log.contains("--hostname github.com"),
        "the endpoint is pinned explicitly: {log}"
    );
    let env_log = fs::read_to_string(dir.join("calls.log.env")).unwrap_or_default();
    assert_eq!(
        env_log.trim(),
        "GH_HOST=",
        "GH_HOST never reaches the child"
    );
}

#[test]
fn successive_calls_share_one_absolute_deadline() {
    let dir = tempdir("read-deadline-shared");
    let body = format!(
        r#"
n_file="{dir}/n"
n=$(( $(cat "$n_file" 2>/dev/null || echo 0) + 1 )); echo "$n" > "$n_file"
case "$n" in
  1) echo '{{"number":7,"state":"open","title":"t","body":"b","updated_at":"r1"}}' ;;
  *) sleep 30 ;;
esac
exit 0
"#,
        dir = dir.display()
    );
    let gh = stub_gh(&dir, "gh", &body);
    let mut api = GhCliApi::with_gh(gh);
    let issue = IssueRef {
        repository: repository(),
        issue: IssueId::trusted("7"),
        display_number: Some(7),
    };
    api.set_read_deadline(Some(
        std::time::Instant::now() + std::time::Duration::from_secs(3),
    ));
    assert!(api.read_issue(&issue).is_ok(), "the first call answers");
    assert!(
        matches!(api.read_issue(&issue), Err(ApiError::Unknown { .. })),
        "the second call dies at the shared deadline, not a fresh budget"
    );
    assert!(
        matches!(api.read_issue(&issue), Err(ApiError::Unknown { .. })),
        "a later call never gets a fresh per-call budget"
    );
    assert_eq!(
        calls(&dir).lines().count(),
        2,
        "the deadline-expired third call never reached the stub"
    );
}

#[test]
fn an_elapsed_read_deadline_refuses_before_spawning() {
    let dir = tempdir("read-deadline");
    let gh = stub_gh(
        &dir,
        "gh",
        r#"
case "$2" in
  repos/owner/repo/issues/7) echo '{"number":7,"state":"open","title":"t","body":"b","updated_at":"r1"}' ;;
esac
"#,
    );
    let mut api = GhCliApi::with_gh(gh);
    let issue = IssueRef {
        repository: repository(),
        issue: IssueId::trusted("7"),
        display_number: Some(7),
    };
    assert!(api.read_issue(&issue).is_ok(), "baseline read answers");
    api.set_read_deadline(Some(
        std::time::Instant::now() - std::time::Duration::from_secs(1),
    ));
    assert!(
        matches!(api.read_issue(&issue), Err(ApiError::Unknown { .. })),
        "an elapsed absolute deadline refuses rather than respawning"
    );
    assert_eq!(
        calls(&dir).lines().count(),
        1,
        "the expired call never reached the stub"
    );
}

// ----------------------------------------------------------------------
// git object transport (push_ref through real `git push`)
// ----------------------------------------------------------------------

/// A `gh` stub answering the ref read and `gh auth token` — the
/// ambient-slot credential the git transport resolves through.
fn stub_gh_for_push(dir: &Path) -> PathBuf {
    stub_gh(
        dir,
        "gh",
        r#"
if [ "$1" = "auth" ] && [ "$2" = "token" ]; then echo 'ghp_ambient-token-1'; exit 0; fi
case "$2" in
  repos/owner/repo/git/ref/heads/b) echo '{"object":{"sha":"expected111"}}' ;;
  repos/owner/repo/git/ref/heads/new-branch) echo 'gh: Not Found (HTTP 404)' >&2; exit 1 ;;
esac
"#,
    )
}

#[test]
fn write_ref_pushes_objects_through_git_transport() {
    let dir = tempdir("git-push");
    let gh = stub_gh_for_push(&dir);
    let git = stub_git(&dir, "exit 0\n");
    let store = dir.join("handoff-repo.git");
    fs::create_dir_all(&store).unwrap();
    let mut api = GhCliApi::with_gh(gh)
        .with_git(git)
        .with_git_transport(&store);
    let head = api
        .write_ref(&repository(), "b", "new000", Some("expected111"))
        .unwrap();
    assert_eq!(head, "new000");
    let argv = git_calls(&dir);
    assert!(
        argv.contains(&format!("-C {}", store.display())),
        "the push runs from the object source: {argv}"
    );
    assert!(
        argv.contains("merge-base --is-ancestor expected111 new000"),
        "a guarded write proves the produced head descends from the prior: {argv}"
    );
    assert!(
        argv.contains("--force-with-lease=refs/heads/b:expected111"),
        "the lease carries the guarded prior head: {argv}"
    );
    assert!(
        argv.contains("https://github.com/owner/repo.git new000:refs/heads/b"),
        "the refspec uploads produced objects and moves the remote ref: {argv}"
    );
    let env = git_env(&dir);
    // `x-access-token` is GitHub's documented username for token auth
    // on the smart-HTTP endpoint — the literal payload decodes as
    // `x-access-token:ghp_ambient-token-1` (#420: Bearer is refused).
    assert!(
        env.contains(
            "GIT_CONFIG_VALUE_1=Authorization: Basic eC1hY2Nlc3MtdG9rZW46Z2hwX2FtYmllbnQtdG9rZW4tMQ=="
        ),
        "the ambient gh token authenticates the push as Basic x-access-token via env config: {env}"
    );
    for key in ["GH_TOKEN", "GITHUB_TOKEN", "HOME", "GIT_SSH_COMMAND"] {
        assert!(
            !env.contains(&format!("{key}=")),
            "the scrubbed env must not carry {key}: {env}"
        );
    }
    assert!(
        env.contains("GIT_CONFIG_VALUE_0="),
        "the credential helper list is emptied: {env}"
    );
    assert!(
        !calls(&dir).contains("PATCH") && !calls(&dir).contains("git/refs --method POST"),
        "no refs-API write may be issued once git transport is active: {}",
        calls(&dir)
    );
}

#[test]
fn write_ref_creates_via_git_transport_with_an_empty_lease() {
    let dir = tempdir("git-push-create");
    let gh = stub_gh_for_push(&dir);
    let git = stub_git(&dir, "exit 0\n");
    let store = dir.join("handoff-repo.git");
    fs::create_dir_all(&store).unwrap();
    let mut api = GhCliApi::with_gh(gh)
        .with_git(git)
        .with_git_transport(&store);
    let head = api
        .write_ref(&repository(), "new-branch", "seed000", None)
        .unwrap();
    assert_eq!(head, "seed000");
    let argv = git_calls(&dir);
    assert!(
        argv.contains("--force-with-lease=refs/heads/new-branch:"),
        "an absent expectation is an empty lease — the ref must not exist: {argv}"
    );
    assert!(argv.contains("seed000:refs/heads/new-branch"), "{argv}");
    assert!(
        !argv.contains("merge-base"),
        "a new ref has no prior — no ancestry check is possible or needed: {argv}"
    );
}

#[test]
fn git_transport_refuses_a_non_descendant_head() {
    // `--force-with-lease` guards only the ref's position: without
    // the local ancestry check the push would happily rewrite the
    // branch to a non-descendant history, where the refs-API
    // `force: false` refused it server-side.
    let dir = tempdir("git-push-nonff");
    let gh = stub_gh_for_push(&dir);
    let git = stub_git(
        &dir,
        "if [ \"$3\" = \"merge-base\" ]; then exit 1; fi\nexit 0\n",
    );
    let store = dir.join("handoff-repo.git");
    fs::create_dir_all(&store).unwrap();
    let mut api = GhCliApi::with_gh(gh)
        .with_git(git)
        .with_git_transport(&store);
    let err = api
        .write_ref(&repository(), "b", "new000", Some("expected111"))
        .unwrap_err();
    assert_eq!(
        err,
        ApiError::Refused {
            reason: "non_fast_forward".into()
        },
        "a non-descendant produced head is a definite refusal"
    );
    assert!(
        !git_calls(&dir).lines().any(|l| l.contains(" push ")),
        "the push is never issued: {}",
        git_calls(&dir)
    );
}

#[test]
fn git_transport_refuses_when_ancestry_cannot_be_determined() {
    // A `merge-base` that cannot answer (missing object, corrupt
    // store) is not a verdict — the write refuses rather than
    // pushing blind. Definite refusal, not Unknown: nothing reached
    // the remote.
    let dir = tempdir("git-push-nobase");
    let gh = stub_gh_for_push(&dir);
    let git = stub_git(
        &dir,
        "if [ \"$3\" = \"merge-base\" ]; then echo 'fatal: Not a valid object name expected111' >&2; exit 128; fi\nexit 0\n",
    );
    let store = dir.join("handoff-repo.git");
    fs::create_dir_all(&store).unwrap();
    let mut api = GhCliApi::with_gh(gh)
        .with_git(git)
        .with_git_transport(&store);
    let err = api
        .write_ref(&repository(), "b", "new000", Some("expected111"))
        .unwrap_err();
    assert!(
        matches!(err, ApiError::Refused { .. }),
        "an undeterminable ancestry refuses, never pushes blind: {err:?}"
    );
    assert!(
        !git_calls(&dir).lines().any(|l| l.contains(" push ")),
        "the push is never issued: {}",
        git_calls(&dir)
    );
}

#[test]
fn git_transport_bound_credential_reaches_env_not_argv() {
    let dir = tempdir("git-push-cred");
    let gh = stub_gh_for_push(&dir);
    let git = stub_git(&dir, "exit 0\n");
    let store = dir.join("handoff-repo.git");
    fs::create_dir_all(&store).unwrap();
    let secret_file = dir.join("push-token");
    fs::write(&secret_file, "bound-secret-value-9").unwrap();
    // The file: provider demands owner-only permissions.
    let mut perms = fs::metadata(&secret_file).unwrap().permissions();
    perms.set_mode(0o600);
    fs::set_permissions(&secret_file, perms).unwrap();
    let credential = resolve_credential(
        Some(&CredentialRef::trusted(
            format!("file:{}", secret_file.display()).as_str(),
        )),
        &OsProviders,
    )
    .unwrap();
    let mut api = GhCliApi::with_gh(gh)
        .with_git(git)
        .with_git_transport(&store);
    api.set_credential(Some(credential));
    api.write_ref(&repository(), "b", "new000", Some("expected111"))
        .unwrap();
    let argv = git_calls(&dir);
    assert!(
        !argv.contains("bound-secret-value-9"),
        "the credential never enters argv: {argv}"
    );
    let env = git_env(&dir);
    assert!(
        env.contains("Authorization: Basic eC1hY2Nlc3MtdG9rZW46Ym91bmQtc2VjcmV0LXZhbHVlLTk="),
        "the bound credential authenticates the push as Basic x-access-token: {env}"
    );
    assert!(
        !calls(&dir).contains("auth token"),
        "a bound credential never falls back to the ambient slot: {}",
        calls(&dir)
    );
}

#[test]
fn git_transport_lease_failure_reports_stale_head() {
    let dir = tempdir("git-push-stale");
    let gh = stub_gh_for_push(&dir);
    let git = stub_git(
        &dir,
        "if [ \"$3\" = \"merge-base\" ]; then exit 0; fi\necho '! refs/heads/b:refs/heads/b [rejected] (stale info)'\nexit 1\n",
    );
    let store = dir.join("handoff-repo.git");
    fs::create_dir_all(&store).unwrap();
    let mut api = GhCliApi::with_gh(gh)
        .with_git(git)
        .with_git_transport(&store);
    let err = api
        .write_ref(&repository(), "b", "new000", Some("expected111"))
        .unwrap_err();
    assert_eq!(
        err,
        ApiError::Refused {
            reason: "stale_head".into()
        },
        "a lease rejection maps to the adapter's stale_head reason"
    );
}

#[test]
fn git_transport_rejection_is_a_definite_refusal() {
    let dir = tempdir("git-push-rejected");
    let gh = stub_gh_for_push(&dir);
    let git = stub_git(
        &dir,
        "if [ \"$3\" = \"merge-base\" ]; then exit 0; fi\necho '! refs/heads/b:refs/heads/b [rejected] (non-fast-forward)'\nexit 1\n",
    );
    let store = dir.join("handoff-repo.git");
    fs::create_dir_all(&store).unwrap();
    let mut api = GhCliApi::with_gh(gh)
        .with_git(git)
        .with_git_transport(&store);
    let err = api
        .write_ref(&repository(), "b", "new000", Some("expected111"))
        .unwrap_err();
    assert!(
        matches!(err, ApiError::Refused { .. }),
        "a server-side rejection is a definite no-effect refusal: {err:?}"
    );
    assert_ne!(
        err,
        ApiError::Refused {
            reason: "stale_head".into()
        },
        "a non-guard rejection must not masquerade as stale_head"
    );
}

#[test]
fn git_transport_lost_connection_is_unknown() {
    let dir = tempdir("git-push-lost");
    let gh = stub_gh_for_push(&dir);
    let git = stub_git(
        &dir,
        "if [ \"$3\" = \"merge-base\" ]; then exit 0; fi\necho 'fatal: unable to access https://github.com/owner/repo.git/: Could not resolve host' >&2\nexit 128\n",
    );
    let store = dir.join("handoff-repo.git");
    fs::create_dir_all(&store).unwrap();
    let mut api = GhCliApi::with_gh(gh)
        .with_git(git)
        .with_git_transport(&store);
    let err = api
        .write_ref(&repository(), "b", "new000", Some("expected111"))
        .unwrap_err();
    assert!(
        matches!(err, ApiError::Unknown { .. }),
        "a lost transport response is unknown, never a failed claim: {err:?}"
    );
}

#[test]
fn git_transport_refuses_when_no_credential_resolves() {
    let dir = tempdir("git-push-nocred");
    let gh = stub_gh(
        &dir,
        "gh",
        r#"
if [ "$1" = "auth" ] && [ "$2" = "token" ]; then
  echo 'gh: not logged in' >&2; exit 1
fi
case "$2" in
  repos/owner/repo/git/ref/heads/b) echo '{"object":{"sha":"expected111"}}' ;;
esac
"#,
    );
    let git = stub_git(&dir, "exit 0\n");
    let store = dir.join("handoff-repo.git");
    fs::create_dir_all(&store).unwrap();
    let mut api = GhCliApi::with_gh(gh)
        .with_git(git)
        .with_git_transport(&store);
    let err = api
        .write_ref(&repository(), "b", "new000", Some("expected111"))
        .unwrap_err();
    assert!(
        matches!(err, ApiError::Refused { .. }),
        "no credential is a definite refusal, never an anonymous push: {err:?}"
    );
    assert!(
        git_calls(&dir).is_empty(),
        "git is never spawned without a credential: {}",
        git_calls(&dir)
    );
}

#[test]
fn git_transport_refuses_an_ambient_token_that_cannot_form_a_header() {
    // The ambient slot does not pass the credential boundary's
    // whitespace check — a `gh auth token` answer with interior
    // whitespace could only corrupt the Authorization header into a
    // bare remote 401, so the write refuses fast with an explicit
    // reason and git is never spawned.
    let dir = tempdir("git-push-badtok");
    let gh = stub_gh(
        &dir,
        "gh",
        r#"
if [ "$1" = "auth" ] && [ "$2" = "token" ]; then
  printf 'gho_fragment\nextra-line\n'; exit 0
fi
case "$2" in
  repos/owner/repo/git/ref/heads/b) echo '{"object":{"sha":"expected111"}}' ;;
esac
"#,
    );
    let git = stub_git(&dir, "exit 0\n");
    let store = dir.join("handoff-repo.git");
    fs::create_dir_all(&store).unwrap();
    let mut api = GhCliApi::with_gh(gh)
        .with_git(git)
        .with_git_transport(&store);
    let err = api
        .write_ref(&repository(), "b", "new000", Some("expected111"))
        .unwrap_err();
    assert!(
        matches!(err, ApiError::Refused { .. }),
        "an unencodable credential is a definite refusal: {err:?}"
    );
    assert!(
        git_calls(&dir).is_empty(),
        "git is never spawned with an unencodable credential: {}",
        git_calls(&dir)
    );
}

#[test]
fn git_transport_spawn_failure_is_a_definite_refusal() {
    let dir = tempdir("git-push-nospawn");
    let gh = stub_gh_for_push(&dir);
    let store = dir.join("handoff-repo.git");
    fs::create_dir_all(&store).unwrap();
    let mut api = GhCliApi::with_gh(gh)
        .with_git(dir.join("no-such-git"))
        .with_git_transport(&store);
    let err = api
        .write_ref(&repository(), "b", "new000", Some("expected111"))
        .unwrap_err();
    assert!(
        matches!(err, ApiError::Refused { .. }),
        "a spawn failure never reached the host: {err:?}"
    );
}
