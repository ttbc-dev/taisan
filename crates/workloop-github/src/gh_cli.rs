//! The `gh` CLI-backed [`GitHubApi`] transport — the live transport for
//! the current stage. Every trait operation maps onto `gh api` REST
//! calls (plus one small GraphQL node lookup for change-request id
//! resolution); authentication is entirely ambient — `gh` reads
//! `GH_TOKEN`/`GITHUB_TOKEN` or its own stored login, so no credential
//! value ever appears in this process's argv, logs, or records.
//!
//! Failure classification follows the trait contract:
//! - a spawn failure or a definite HTTP 4xx is [`ApiError::Refused`] —
//!   the host (or the transport itself) refused before any effect;
//! - a 5xx, a lost/garbled response, or an unclassifiable non-zero exit
//!   is [`ApiError::Unknown`] — the effect may have landed;
//! - `write_ref` re-reads the ref and enforces `expected_prior`
//!   before issuing the write, returning `Refused { "stale_head" }` on
//!   mismatch — the reason string the adapter's guarded-write path
//!   matches on.
//!
//! Known transport limitation, recorded for the runbook: GitHub's git
//! refs API has no compare-and-swap on the prior value. The guard is
//! read-then-write plus the server-side fast-forward check
//! (`force: false`); a ref moved by a third party in the narrow window
//! between the read and the PATCH can still be overwritten when the
//! new head remains a descendant. The `git push
//! --force-with-lease` form would close that window but requires a
//! local object store this transport does not have; the v0.2 HTTPS
//! transport inherits the same constraint unless it adds one.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use workloop_core::identity::{
    ChangeRequestId, HostActorId, HostActorRef, HostInstanceId, IssueRef, RepositoryRef,
};
use workloop_core::observation::{CheckConclusion, IssueState};

use crate::api::{
    ApiError, GitHubApi, RemoteCheckRun, RemoteComment, RemoteIssue, RemotePull, RemoteReview,
    RemoteReviewFinding, RepositoryPermissions,
};
use crate::credential::{CredentialError, ResolvedCredential};

/// Default per-call ceiling for one `gh` subprocess. A hung transport
/// must not stall the adapter loop forever.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(120);

/// Ceiling on one subprocess's captured output. `--paginate --slurp`
/// reads every page into one response; an unbounded read would let a
/// pathological host exhaust memory. Excess is truncated, which fails
/// JSON parsing as `Unknown` — never a fabricated success.
const MAX_OUTPUT: u64 = 64 * 1024 * 1024;

/// Branch names interpolated into request paths or query fields:
/// the conservative allowlist `[A-Za-z0-9._/-]` with git-ref shape
/// rules. Anything else — whitespace, URL-significant characters,
/// `..`, edge slashes — is refused before a subprocess is spawned;
/// over-rejection is a definite `Refused`, never a mangled request.
fn valid_branch(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 255
        && !name.starts_with('/')
        && !name.ends_with(['/', '.'])
        && !name.contains("..")
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '/'))
}

/// Revision strings interpolated into request paths (check-run heads,
/// compare endpoints, written ref targets): the same allowlist —
/// SHAs are hex and branch-shaped refs fit, while `?`, `#`, `%`, `&`,
/// `=` and whitespace can never reach the endpoint.
fn valid_revision(revision: &str) -> bool {
    !revision.is_empty()
        && revision.len() <= 255
        && revision
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '/'))
}

/// Comment markers embedded in `<!-- -->` envelopes: the marker must
/// never be able to close the envelope early.
fn valid_marker(marker: &str) -> bool {
    !marker.is_empty() && !marker.contains("-->")
}

/// The raw result of one `gh` invocation, before trait-level
/// classification. `http_status` is parsed from `gh`'s stderr
/// diagnostic and is `None` for transport-level failures (spawn,
/// DNS, reset, TLS) where no HTTP answer exists.
struct GhOutput {
    success: bool,
    http_status: Option<u16>,
    stdout: String,
    stderr: String,
}

/// The `gh`-CLI transport. Construct with [`GhCliApi::new`] for the
/// `gh` on `PATH`, or [`GhCliApi::with_gh`] to pin an executable —
/// the test suite uses the latter to drive stub binaries.
#[derive(Debug)]
pub struct GhCliApi {
    gh: PathBuf,
    timeout: Duration,
    /// `change_request_id` (node id) → `(pull number, owner/name)`.
    /// Numbers never change for a pull request; the slug is retained
    /// so a node id from another repository can never be read through
    /// this repository's number namespace.
    pull_numbers: HashMap<String, (u64, String)>,
    hostname: Option<String>,
    /// The bound credential the adapter resolved for the current
    /// intent, injected as `GH_TOKEN` on each `gh` child so the call
    /// authenticates under the declared credential rather than `gh`'s
    /// ambient login. `None` keeps the ambient identity.
    credential: Option<ResolvedCredential>,
    read_deadline: Option<Instant>,
}

impl GhCliApi {
    /// The transport driving `gh` from `PATH`.
    pub fn new() -> Self {
        Self {
            gh: PathBuf::from("gh"),
            timeout: DEFAULT_TIMEOUT,
            pull_numbers: HashMap::new(),
            hostname: None,
            credential: None,
            read_deadline: None,
        }
    }

    /// The transport driving a specific `gh` executable — the test
    /// seam that lets stub binaries stand in for the CLI.
    pub fn with_gh(gh: impl Into<PathBuf>) -> Self {
        Self {
            gh: gh.into(),
            timeout: DEFAULT_TIMEOUT,
            pull_numbers: HashMap::new(),
            hostname: None,
            credential: None,
            read_deadline: None,
        }
    }

    /// Override the per-call subprocess ceiling.
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    pub fn with_hostname(mut self, hostname: impl Into<String>) -> Self {
        self.hostname = Some(hostname.into());
        self
    }

    /// `owner/name` for one repository reference. The `RepositoryId`
    /// string is the GitHub slug; the host instance is installation
    /// scope the transport does not need for addressing.
    fn slug(repository: &RepositoryRef) -> Result<String, ApiError> {
        let slug = repository.repository.as_str();
        let valid = slug.split('/').count() == 2
            && slug.split('/').all(|part| {
                !part.is_empty()
                    && part
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
            });
        if valid {
            Ok(slug.to_string())
        } else {
            Err(ApiError::Refused {
                reason: format!("repository id is not a GitHub owner/name slug: {slug}"),
            })
        }
    }

    /// Spawn `gh` with the given arguments, pipe `body` as JSON on
    /// stdin when present, and collect the result under the timeout.
    /// Spawn failure is a definite no-effect [`ApiError::Refused`]; a
    /// timeout is [`ApiError::Unknown`] — the request may have been
    /// issued before the ceiling hit.
    fn run(&self, args: &[String], body: Option<&serde_json::Value>) -> Result<GhOutput, ApiError> {
        self.run_as(args, body, None)
    }

    /// `run` under an explicit credential — the authentication probe
    /// passes the candidate it is verifying rather than the bound
    /// credential; `None` defers to the bound one.
    fn run_as(
        &self,
        args: &[String],
        body: Option<&serde_json::Value>,
        token_override: Option<&ResolvedCredential>,
    ) -> Result<GhOutput, ApiError> {
        let effective = match self.read_deadline {
            Some(deadline) => {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    return Err(ApiError::Unknown {
                        reason: format!("gh api {}: read deadline elapsed", args.join(" ")),
                    });
                }
                remaining.min(self.timeout)
            }
            None => self.timeout,
        };
        let mut command = Command::new(&self.gh);
        command.arg("api").args(args);
        if let Some(hostname) = &self.hostname {
            command.arg("--hostname").arg(hostname);
            command.env_remove("GH_HOST");
        }
        // The bound credential rides as `GH_TOKEN` on this one child —
        // `gh` prefers it over its own stored login, so the call
        // authenticates as the declared credential's actor. The value
        // never enters argv (process lists) and the child's own
        // environment only.
        let credential = token_override.or(self.credential.as_ref());
        if let Some(credential) = credential {
            command.env("GH_TOKEN", credential.expose());
        }
        if body.is_some() {
            command.arg("--input").arg("-");
        }
        let mut child = command
            .stdin(if body.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| ApiError::Refused {
                reason: format!("gh spawn failed: {e}"),
            })?;
        // The stdin payload write runs on its own thread: a child
        // that never reads stdin cannot hold this call open — the
        // timeout path kills it and the write fails with EPIPE.
        let stdin_pipe = child.stdin.take();
        let payload = body.and_then(|b| serde_json::to_vec(b).ok());
        let _stdin_writer = stdin_pipe.map(|mut stdin| {
            std::thread::spawn(move || {
                if let Some(payload) = payload {
                    // A broken pipe means `gh` exited before reading —
                    // the wait below reports the real outcome.
                    let _ = stdin.write_all(&payload);
                }
            })
        });
        // Drain both pipes on reader threads into shared buffers
        // while the child runs: a response larger than the OS pipe
        // buffer would otherwise block `gh` mid-write. The caller
        // never blocks on join — a descendant surviving the kill can
        // hold a pipe open, so readers get only a short flush grace.
        let stdout_buf = Arc::new(Mutex::new(Vec::new()));
        let stderr_buf = Arc::new(Mutex::new(Vec::new()));
        let stdout_reader = spawn_drain(child.stdout.take(), MAX_OUTPUT, &stdout_buf);
        let stderr_reader = spawn_drain(child.stderr.take(), MAX_OUTPUT, &stderr_buf);
        let deadline = Instant::now() + effective;
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(25));
                }
                Ok(None) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    flush_readers(&[&stdout_reader, &stderr_reader]);
                    return Err(ApiError::Unknown {
                        reason: format!("gh api {} timed out", args.join(" ")),
                    });
                }
                Err(e) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    flush_readers(&[&stdout_reader, &stderr_reader]);
                    return Err(ApiError::Unknown {
                        reason: format!("gh wait failed: {e}"),
                    });
                }
            }
        };
        flush_readers(&[&stdout_reader, &stderr_reader]);
        let stdout = buffer_string(&stdout_buf);
        let stderr = buffer_string(&stderr_buf);
        Ok(GhOutput {
            success: status.success(),
            http_status: http_status(stderr.trim()),
            stdout: stdout.trim().to_string(),
            stderr: stderr.trim().to_string(),
        })
    }

    /// One JSON REST call: `gh api <endpoint> --method <METHOD>`.
    fn api(
        &self,
        method: &str,
        endpoint: &str,
        body: Option<&serde_json::Value>,
    ) -> Result<serde_json::Value, ApiError> {
        self.api_as(method, endpoint, body, None)
    }

    /// `api` under an explicit credential — the authentication probe
    /// runs the candidate token rather than the bound one.
    fn api_as(
        &self,
        method: &str,
        endpoint: &str,
        body: Option<&serde_json::Value>,
        token_override: Option<&ResolvedCredential>,
    ) -> Result<serde_json::Value, ApiError> {
        let args = vec![
            endpoint.to_string(),
            "--method".to_string(),
            method.to_string(),
        ];
        let output = self.run_as(&args, body, token_override)?;
        if output.success {
            return serde_json::from_str(&output.stdout).map_err(|e| ApiError::Unknown {
                reason: format!("gh api {endpoint}: unparseable response: {e}"),
            });
        }
        Err(self.classify_err(endpoint, &output))
    }

    /// A JSON REST call that surfaces the HTTP status on failure —
    /// `read_branch` maps 404 to `None`. `Ok(Err(status))` carries the
    /// status; transport-level failures stay `Err(ApiError)`.
    fn api_status(
        &self,
        method: &str,
        endpoint: &str,
    ) -> Result<Result<serde_json::Value, u16>, ApiError> {
        let args = vec![
            endpoint.to_string(),
            "--method".to_string(),
            method.to_string(),
        ];
        let output = self.run(&args, None)?;
        if output.success {
            let value = serde_json::from_str(&output.stdout).map_err(|e| ApiError::Unknown {
                reason: format!("gh api {endpoint}: unparseable response: {e}"),
            })?;
            return Ok(Ok(value));
        }
        match output.http_status {
            Some(status) => Ok(Err(status)),
            None => Err(self.classify_err(endpoint, &output)),
        }
    }

    /// A list endpoint's items across all pages. `gh api --paginate
    /// --slurp` wraps each page body in an array, so the payload is an
    /// array of page bodies; each page body is itself the list (or, for
    /// `check-runs`, an object whose `check_runs` member is the list —
    /// callers unwrap that shape). `fields` become `-f key=value`
    /// arguments — `gh` encodes them into the query string, so values
    /// never need hand-rolled escaping.
    fn api_pages(
        &self,
        endpoint: &str,
        fields: &[(&str, &str)],
    ) -> Result<Vec<serde_json::Value>, ApiError> {
        // `--method GET` is mandatory once `-f` fields are present:
        // `gh api` defaults to POST when parameters are added, which
        // would turn a list read into a write.
        let mut args = vec![
            endpoint.to_string(),
            "--method".to_string(),
            "GET".to_string(),
            "--paginate".to_string(),
            "--slurp".to_string(),
        ];
        for (key, value) in fields {
            args.push("-f".to_string());
            args.push(format!("{key}={value}"));
        }
        let output = self.run(&args, None)?;
        if !output.success {
            return Err(self.classify_err(endpoint, &output));
        }
        let value: serde_json::Value =
            serde_json::from_str(&output.stdout).map_err(|e| ApiError::Unknown {
                reason: format!("gh api {endpoint}: unparseable response: {e}"),
            })?;
        let mut items = Vec::new();
        match value {
            serde_json::Value::Array(pages) => {
                for page in pages {
                    match page {
                        serde_json::Value::Array(list) => items.extend(list),
                        other => items.push(other),
                    }
                }
            }
            other => items.push(other),
        }
        Ok(items)
    }

    fn classify_err(&self, endpoint: &str, output: &GhOutput) -> ApiError {
        let detail = if output.stderr.is_empty() {
            output.stdout.clone()
        } else {
            output.stderr.clone()
        };
        match output.http_status {
            Some(status) if (400..500).contains(&status) => ApiError::Refused {
                reason: format!("gh api {endpoint}: {detail}"),
            },
            Some(status) => ApiError::Unknown {
                reason: format!("gh api {endpoint}: HTTP {status}: {detail}"),
            },
            None => ApiError::Unknown {
                reason: format!("gh api {endpoint}: {detail}"),
            },
        }
    }

    /// `gh api graphql` — the one GraphQL call the REST surface cannot
    /// express: resolving a change-request node id to its pull number
    /// *and* confirming the node lives in this repository.
    fn graphql_node_pull(&self, change_id: &ChangeRequestId, slug: &str) -> Result<u64, ApiError> {
        let query = "query($id: ID!) { node(id: $id) { ... on PullRequest { number repository { owner { login } name } } } }";
        let args = vec![
            "graphql".to_string(),
            "-f".to_string(),
            format!("query={query}"),
            "-F".to_string(),
            format!("id={}", change_id.as_str()),
        ];
        let output = self.run(&args, None)?;
        if !output.success {
            return Err(self.classify_err("graphql", &output));
        }
        let value: serde_json::Value =
            serde_json::from_str(&output.stdout).map_err(|e| ApiError::Unknown {
                reason: format!("gh api graphql: unparseable response: {e}"),
            })?;
        let node = value.pointer("/data/node");
        let Some(node) = node.filter(|n| !n.is_null()) else {
            return Err(ApiError::Refused {
                reason: format!("change request {} not found", change_id.as_str()),
            });
        };
        let owner = node
            .pointer("/repository/owner/login")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        let name = node
            .pointer("/repository/name")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        if format!("{owner}/{name}") != slug {
            // A node id from another repository must never be read
            // through this repository's number namespace.
            return Err(ApiError::Refused {
                reason: format!(
                    "change request {} belongs to {owner}/{name}, not {slug}",
                    change_id.as_str()
                ),
            });
        }
        node.pointer("/number")
            .and_then(|v| v.as_u64())
            .ok_or_else(|| ApiError::Unknown {
                reason: "pull node missing number".into(),
            })
    }

    /// Resolve a change-request node id to `(pull number, slug)` —
    /// cached, since numbers never change for a pull request.
    fn pull_number(
        &mut self,
        repository: &RepositoryRef,
        change_id: &ChangeRequestId,
    ) -> Result<u64, ApiError> {
        let slug = Self::slug(repository)?;
        if let Some((number, cached_slug)) = self.pull_numbers.get(change_id.as_str())
            && *cached_slug == slug
        {
            return Ok(*number);
        }
        let number = self.graphql_node_pull(change_id, &slug)?;
        self.pull_numbers
            .insert(change_id.as_str().to_string(), (number, slug));
        Ok(number)
    }

    /// Every issue comment on the pull request behind `change_id`,
    /// paginated through the same read `find_comment` uses — a
    /// uniqueness check needs the whole comment stream, not the
    /// first marker hit. Returns the raw comment payloads; callers
    /// read `body`/`id` themselves.
    pub fn issue_comments(
        &mut self,
        repository: &RepositoryRef,
        change_id: &ChangeRequestId,
    ) -> Result<Vec<serde_json::Value>, ApiError> {
        let slug = Self::slug(repository)?;
        let number = self.pull_number(repository, change_id)?;
        self.api_pages(&format!("repos/{slug}/issues/{number}/comments"), &[])
    }

    fn remote_pull(value: &serde_json::Value) -> Result<RemotePull, ApiError> {
        let str_field = |key: &str| {
            value
                .get(key)
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string()
        };
        let node_id = str_field("node_id");
        if node_id.is_empty() {
            return Err(ApiError::Unknown {
                reason: "pull response missing node_id".into(),
            });
        }
        let number = value
            .get("number")
            .and_then(|v| v.as_u64())
            .ok_or_else(|| ApiError::Unknown {
                reason: "pull response missing number".into(),
            })?;
        let head = value.get("head").cloned().unwrap_or_default();
        let base = value.get("base").cloned().unwrap_or_default();
        let head_str = |key: &str| {
            head.get(key)
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string()
        };
        let base_str = |key: &str| {
            base.get(key)
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string()
        };
        Ok(RemotePull {
            id: node_id,
            display_number: number,
            head_sha: head_str("sha"),
            source_branch: head_str("ref"),
            target_branch: base_str("ref"),
            title: str_field("title"),
            body: str_field("body"),
            merged: value
                .get("merged")
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
                || value.get("merged_at").is_some_and(|v| v.is_string()),
        })
    }
}

impl Default for GhCliApi {
    fn default() -> Self {
        Self::new()
    }
}

/// Spawn a reader that drains `pipe` to EOF into `buf`, keeping at
/// most `cap` bytes. The reader never stops consuming — a child
/// writing past the cap cannot block — while the retained prefix
/// stays small enough that an oversized response fails JSON parsing
/// as `Unknown` instead of exhausting memory.
fn spawn_drain(
    pipe: Option<impl Read + Send + 'static>,
    cap: u64,
    buf: &Arc<Mutex<Vec<u8>>>,
) -> JoinHandle<()> {
    let buf = Arc::clone(buf);
    std::thread::spawn(move || {
        let Some(mut pipe) = pipe else { return };
        let mut chunk = [0u8; 16 * 1024];
        let mut kept = 0u64;
        loop {
            match pipe.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if kept < cap {
                        let take = ((cap - kept) as usize).min(n);
                        if let Ok(mut guard) = buf.lock() {
                            guard.extend_from_slice(&chunk[..take]);
                        }
                        kept += take as u64;
                    }
                }
            }
        }
    })
}

/// Give pipe readers a short flush window after the child exits —
/// long enough for the final bytes, short enough that a descendant
/// holding a pipe open cannot stall the call past the deadline.
/// Unfinished readers are detached; whatever they already drained
/// stays in the shared buffer.
fn flush_readers(readers: &[&JoinHandle<()>]) {
    let grace = Instant::now() + Duration::from_secs(2);
    while readers.iter().any(|r| !r.is_finished()) && Instant::now() < grace {
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn buffer_string(buf: &Mutex<Vec<u8>>) -> String {
    let guard = buf.lock().unwrap_or_else(|e| e.into_inner());
    String::from_utf8_lossy(&guard).into_owned()
}

/// Extract `HTTP NNN` from `gh`'s stderr diagnostic (`gh: ... (HTTP
/// 422)`). Absent for transport-level failures.
fn http_status(detail: &str) -> Option<u16> {
    let marker = "HTTP ";
    let start = detail.find(marker)? + marker.len();
    let digits: String = detail[start..]
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect();
    digits.parse().ok()
}

/// The comment body actually posted: the logical body plus the
/// reconciliation marker as an HTML comment — invisible when rendered,
/// exact-matchable as text.
fn comment_body(body: &str, marker: &str) -> String {
    format!("{body}\n\n<!-- {marker} -->")
}

fn marker_envelope(marker: &str) -> String {
    format!("<!-- {marker} -->")
}

impl GitHubApi for GhCliApi {
    fn viewer(&mut self) -> Result<Option<String>, ApiError> {
        let user = self.api("GET", "user", None)?;
        match user.get("login").and_then(|v| v.as_str()) {
            Some(login) => Ok(Some(login.to_string())),
            None => Err(ApiError::Unknown {
                reason: "gh api user: response missing login".into(),
            }),
        }
    }

    fn set_credential(&mut self, credential: Option<ResolvedCredential>) {
        self.credential = credential;
    }

    fn authenticate(
        &mut self,
        credential: &ResolvedCredential,
    ) -> Result<HostActorRef, CredentialError> {
        // `gh api user` under the candidate token — never the bound
        // credential and never `gh`'s ambient login — answers which
        // actor the resolved value acts as.
        let user = self
            .api_as("GET", "user", None, Some(credential))
            .map_err(|_| {
                CredentialError::unresolvable("the host denies or cannot verify the credential")
            })?;
        let login = user
            .get("login")
            .and_then(|v| v.as_str())
            .ok_or_else(|| CredentialError::unresolvable("gh api user: response missing login"))?;
        let actor = HostActorId::new(login)
            .map_err(|_| CredentialError::unresolvable("gh api user: unparseable login"))?;
        Ok(HostActorRef {
            host: HostInstanceId::trusted(self.hostname.as_deref().unwrap_or("github.com")),
            actor,
        })
    }

    fn read_repository(&mut self, repository: &RepositoryRef) -> Result<Option<String>, ApiError> {
        let slug = Self::slug(repository)?;
        let repo = self.api("GET", &format!("repos/{slug}"), None)?;
        let default_branch = repo
            .get("default_branch")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ApiError::Unknown {
                reason: "repository response missing default_branch".into(),
            })?;
        self.read_branch(repository, default_branch)
    }

    fn read_permissions(
        &mut self,
        repository: &RepositoryRef,
    ) -> Result<RepositoryPermissions, ApiError> {
        let slug = Self::slug(repository)?;
        let repo = self.api("GET", &format!("repos/{slug}"), None)?;
        // Absent bits are not granted: only an explicit `true` in the
        // host's own permissions object authorizes a capability.
        let bit = |name: &str| {
            repo.pointer(&format!("/permissions/{name}"))
                .and_then(|v| v.as_bool())
                == Some(true)
        };
        Ok(RepositoryPermissions {
            push: bit("push"),
            pull: bit("pull"),
        })
    }

    fn read_branch(
        &mut self,
        repository: &RepositoryRef,
        branch: &str,
    ) -> Result<Option<String>, ApiError> {
        let slug = Self::slug(repository)?;
        if !valid_branch(branch) {
            return Err(ApiError::Refused {
                reason: format!("invalid branch name: {branch:?}"),
            });
        }
        let endpoint = format!("repos/{slug}/git/ref/heads/{branch}");
        match self.api_status("GET", &endpoint)? {
            Ok(value) => match value.pointer("/object/sha").and_then(|v| v.as_str()) {
                Some(sha) => Ok(Some(sha.to_string())),
                // A 200 without the ref object is a malformed
                // success, never proof the branch is absent.
                None => Err(ApiError::Unknown {
                    reason: format!("gh api {endpoint}: response missing object.sha"),
                }),
            },
            Err(404) => Ok(None),
            // A definite 4xx is a refusal, same as `classify_err`
            // applies everywhere else — only 404 means "absent".
            Err(status) if (400..500).contains(&status) => Err(ApiError::Refused {
                reason: format!("gh api {endpoint}: HTTP {status}"),
            }),
            Err(status) => Err(ApiError::Unknown {
                reason: format!("gh api {endpoint}: HTTP {status}"),
            }),
        }
    }

    fn write_ref(
        &mut self,
        repository: &RepositoryRef,
        branch: &str,
        new_head: &str,
        expected_prior: Option<&str>,
    ) -> Result<String, ApiError> {
        let slug = Self::slug(repository)?;
        if !valid_branch(branch) {
            return Err(ApiError::Refused {
                reason: format!("invalid branch name: {branch:?}"),
            });
        }
        if !valid_revision(new_head) {
            return Err(ApiError::Refused {
                reason: format!("invalid revision: {new_head:?}"),
            });
        }
        // The exact prior-ref guard: the current head must still equal
        // `expected_prior` before any write is issued.
        let actual = self.read_branch(repository, branch)?;
        if actual.as_deref() != expected_prior {
            return Err(ApiError::Refused {
                reason: "stale_head".into(),
            });
        }
        let result = if actual.is_some() {
            self.api(
                "PATCH",
                &format!("repos/{slug}/git/refs/heads/{branch}"),
                Some(&serde_json::json!({
                    "sha": new_head,
                    // Server-side fast-forward enforcement: a head
                    // moved to a non-ancestor refuses here even if it
                    // raced our read.
                    "force": false,
                })),
            )
        } else {
            self.api(
                "POST",
                &format!("repos/{slug}/git/refs"),
                Some(&serde_json::json!({
                    "ref": format!("refs/heads/{branch}"),
                    "sha": new_head,
                })),
            )
        };
        match result {
            Ok(_) => Ok(new_head.to_string()),
            Err(ApiError::Refused { reason }) => {
                // A refused write can still mean the head moved under
                // us — re-read and report `stale_head` when the guard
                // is what actually failed.
                match self.read_branch(repository, branch) {
                    Ok(now) if now.as_deref() != expected_prior => Err(ApiError::Refused {
                        reason: "stale_head".into(),
                    }),
                    _ => Err(ApiError::Refused { reason }),
                }
            }
            Err(other) => Err(other),
        }
    }

    fn read_pull(
        &mut self,
        repository: &RepositoryRef,
        change_id: &ChangeRequestId,
    ) -> Result<RemotePull, ApiError> {
        let slug = Self::slug(repository)?;
        let number = self.pull_number(repository, change_id)?;
        let value = self.api("GET", &format!("repos/{slug}/pulls/{number}"), None)?;
        Self::remote_pull(&value)
    }

    fn open_pull(
        &mut self,
        repository: &RepositoryRef,
        source_branch: &str,
        target_branch: &str,
        title: &str,
        body: &str,
    ) -> Result<RemotePull, ApiError> {
        let slug = Self::slug(repository)?;
        let value = self.api(
            "POST",
            &format!("repos/{slug}/pulls"),
            Some(&serde_json::json!({
                "title": title,
                // Same-repository head: the branch name alone.
                "head": source_branch,
                "base": target_branch,
                "body": body,
            })),
        )?;
        let pull = Self::remote_pull(&value)?;
        self.pull_numbers
            .insert(pull.id.clone(), (pull.display_number, slug));
        Ok(pull)
    }

    fn update_pull(
        &mut self,
        repository: &RepositoryRef,
        change_id: &ChangeRequestId,
        title: Option<&str>,
        body: Option<&str>,
    ) -> Result<RemotePull, ApiError> {
        let slug = Self::slug(repository)?;
        let number = self.pull_number(repository, change_id)?;
        let mut patch = serde_json::Map::new();
        if let Some(title) = title {
            patch.insert("title".into(), serde_json::json!(title));
        }
        if let Some(body) = body {
            patch.insert("body".into(), serde_json::json!(body));
        }
        let value = self.api(
            "PATCH",
            &format!("repos/{slug}/pulls/{number}"),
            Some(&serde_json::Value::Object(patch)),
        )?;
        Self::remote_pull(&value)
    }

    fn comment_on_pull(
        &mut self,
        repository: &RepositoryRef,
        change_id: &ChangeRequestId,
        marker: &str,
        body: &str,
    ) -> Result<RemoteComment, ApiError> {
        let slug = Self::slug(repository)?;
        if !valid_marker(marker) {
            return Err(ApiError::Refused {
                reason: format!("invalid comment marker: {marker:?}"),
            });
        }
        let number = self.pull_number(repository, change_id)?;
        let value = self.api(
            "POST",
            &format!("repos/{slug}/issues/{number}/comments"),
            Some(&serde_json::json!({
                "body": comment_body(body, marker),
            })),
        )?;
        let comment_id =
            value
                .get("id")
                .map(|v| v.to_string())
                .ok_or_else(|| ApiError::Unknown {
                    reason: "comment response missing id".into(),
                })?;
        Ok(RemoteComment {
            comment_id,
            body: value
                .get("body")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string(),
            marker: marker.to_string(),
        })
    }

    fn find_comment(
        &mut self,
        repository: &RepositoryRef,
        change_id: &ChangeRequestId,
        marker: &str,
    ) -> Result<Option<RemoteComment>, ApiError> {
        let slug = Self::slug(repository)?;
        if !valid_marker(marker) {
            return Err(ApiError::Refused {
                reason: format!("invalid comment marker: {marker:?}"),
            });
        }
        let number = self.pull_number(repository, change_id)?;
        let envelope = marker_envelope(marker);
        let comments = self.api_pages(&format!("repos/{slug}/issues/{number}/comments"), &[])?;
        Ok(comments
            .iter()
            .find(|comment| {
                comment
                    .get("body")
                    .and_then(|v| v.as_str())
                    .is_some_and(|body| body.contains(&envelope))
            })
            .map(|comment| RemoteComment {
                comment_id: comment.get("id").map(|v| v.to_string()).unwrap_or_default(),
                body: comment
                    .get("body")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string(),
                marker: marker.to_string(),
            }))
    }

    fn list_pulls_on_branch(
        &mut self,
        repository: &RepositoryRef,
        source_branch: &str,
        head_sha: &str,
    ) -> Result<Vec<RemotePull>, ApiError> {
        let slug = Self::slug(repository)?;
        if !valid_branch(source_branch) {
            return Err(ApiError::Refused {
                reason: format!("invalid branch name: {source_branch:?}"),
            });
        }
        if !valid_revision(head_sha) {
            return Err(ApiError::Refused {
                reason: format!("invalid revision: {head_sha:?}"),
            });
        }
        let owner = slug.split('/').next().unwrap_or_default();
        // Query fields ride `-f` so `gh` encodes them — the branch
        // name never hand-joins a query string.
        let head_filter = format!("{owner}:{source_branch}");
        let pulls = self.api_pages(
            &format!("repos/{slug}/pulls"),
            &[
                ("head", head_filter.as_str()),
                ("state", "all"),
                ("per_page", "100"),
            ],
        )?;
        pulls
            .iter()
            .filter(|pull| {
                pull.pointer("/head/sha")
                    .and_then(|v| v.as_str())
                    .is_some_and(|sha| sha == head_sha)
            })
            .map(Self::remote_pull)
            .collect()
    }

    fn check_runs(
        &mut self,
        repository: &RepositoryRef,
        head_sha: &str,
    ) -> Result<Vec<RemoteCheckRun>, ApiError> {
        let slug = Self::slug(repository)?;
        if !valid_revision(head_sha) {
            return Err(ApiError::Refused {
                reason: format!("invalid revision: {head_sha:?}"),
            });
        }
        let pages = self.api_pages(
            &format!("repos/{slug}/commits/{head_sha}/check-runs?per_page=100"),
            &[],
        )?;
        // `check-runs` answers `{total_count, check_runs}` per page —
        // collect the member arrays.
        let mut runs = Vec::new();
        for page in &pages {
            if let Some(list) = page.get("check_runs").and_then(|v| v.as_array()) {
                runs.extend(list.iter().cloned());
            }
        }
        runs.iter()
            .map(|run| {
                let conclusion = match run.get("status").and_then(|v| v.as_str()) {
                    Some("completed") => match run.get("conclusion").and_then(|v| v.as_str()) {
                        Some("success" | "neutral" | "skipped") => CheckConclusion::Success,
                        _ => CheckConclusion::Failure,
                    },
                    _ => CheckConclusion::Pending,
                };
                Ok(RemoteCheckRun {
                    check_run_id: run.get("id").map(|v| v.to_string()).unwrap_or_default(),
                    name: run
                        .get("name")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_string(),
                    head_sha: run
                        .get("head_sha")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_string(),
                    conclusion,
                    producer: run
                        .pointer("/app/slug")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_string(),
                })
            })
            .collect()
    }

    fn reviews(
        &mut self,
        repository: &RepositoryRef,
        change_id: &ChangeRequestId,
    ) -> Result<Vec<RemoteReview>, ApiError> {
        let slug = Self::slug(repository)?;
        let number = self.pull_number(repository, change_id)?;
        let reviews = self.api_pages(&format!("repos/{slug}/pulls/{number}/reviews"), &[])?;
        reviews
            .iter()
            .map(|review| {
                Ok(RemoteReview {
                    review_id: review.get("id").map(|v| v.to_string()).unwrap_or_default(),
                    commit_sha: review
                        .get("commit_id")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_string(),
                    author: review
                        .pointer("/user/login")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_string(),
                    body: review
                        .get("body")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_string(),
                    changes_requested: review
                        .get("state")
                        .and_then(|v| v.as_str())
                        .is_some_and(|state| state == "CHANGES_REQUESTED"),
                })
            })
            .collect()
    }

    fn review_findings(
        &mut self,
        repository: &RepositoryRef,
        change_id: &ChangeRequestId,
    ) -> Result<Vec<RemoteReviewFinding>, ApiError> {
        let slug = Self::slug(repository)?;
        let number = self.pull_number(repository, change_id)?;
        // The commit a finding examined is its review's `commit_id` —
        // join through `pull_request_review_id`, the same binding the
        // in-memory contract double records.
        let reviews = self.api_pages(&format!("repos/{slug}/pulls/{number}/reviews"), &[])?;
        let review_commits: HashMap<String, String> = reviews
            .iter()
            .filter_map(|review| {
                let id = review.get("id")?.to_string();
                let commit = review.get("commit_id")?.as_str()?.to_string();
                Some((id, commit))
            })
            .collect();
        let comments = self.api_pages(&format!("repos/{slug}/pulls/{number}/comments"), &[])?;
        comments
            .iter()
            .map(|comment| {
                let review_id = comment
                    .get("pull_request_review_id")
                    .map(|v| v.to_string())
                    .unwrap_or_default();
                Ok(RemoteReviewFinding {
                    finding_id: comment.get("id").map(|v| v.to_string()).unwrap_or_default(),
                    review_id: review_id.clone(),
                    commit_sha: review_commits
                        .get(&review_id)
                        .cloned()
                        .or_else(|| {
                            comment
                                .get("original_commit_id")
                                .and_then(|v| v.as_str())
                                .map(str::to_string)
                        })
                        .unwrap_or_default(),
                    body: comment
                        .get("body")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_string(),
                })
            })
            .collect()
    }

    fn contains_revision(
        &mut self,
        repository: &RepositoryRef,
        ancestor: &str,
        descendant: &str,
    ) -> Result<bool, ApiError> {
        let slug = Self::slug(repository)?;
        if !valid_revision(ancestor) {
            return Err(ApiError::Refused {
                reason: format!("invalid revision: {ancestor:?}"),
            });
        }
        if !valid_revision(descendant) {
            return Err(ApiError::Refused {
                reason: format!("invalid revision: {descendant:?}"),
            });
        }
        let value = self.api(
            "GET",
            &format!("repos/{slug}/compare/{ancestor}...{descendant}"),
            None,
        )?;
        Ok(matches!(
            value.get("status").and_then(|v| v.as_str()),
            Some("ahead" | "identical")
        ))
    }

    fn read_issue(&mut self, issue: &IssueRef) -> Result<RemoteIssue, ApiError> {
        let slug = Self::slug(&issue.repository)?;
        let id = issue.issue.as_str();
        let number = id
            .parse::<u64>()
            .ok()
            .filter(|number| *number > 0 && number.to_string() == id)
            .ok_or_else(|| ApiError::Refused {
                reason: format!("issue id is not a positive decimal number: {id}"),
            })?;
        let endpoint = format!("repos/{slug}/issues/{number}");
        let value = self.api("GET", &endpoint, None)?;
        let malformed = |reason: &str| ApiError::Refused {
            reason: format!("gh api {endpoint}: malformed issue response: {reason}"),
        };
        let reported = value
            .get("number")
            .and_then(|v| v.as_u64())
            .ok_or_else(|| malformed("missing or non-numeric number"))?;
        if reported != number {
            return Err(malformed(
                "response number does not match the requested issue",
            ));
        }
        let state = match value.get("state").and_then(|v| v.as_str()) {
            Some("open") => IssueState::Open,
            Some("closed") => IssueState::Closed,
            _ => return Err(malformed("state is not open or closed")),
        };
        let title = value
            .get("title")
            .and_then(|v| v.as_str())
            .ok_or_else(|| malformed("title is not a string"))?;
        let body = match value.get("body") {
            None | Some(serde_json::Value::Null) => "",
            Some(v) => v
                .as_str()
                .ok_or_else(|| malformed("body is not a string"))?,
        };
        let source_revision = value
            .get("updated_at")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| malformed("missing or empty updated_at"))?;
        Ok(RemoteIssue {
            source_revision: source_revision.to_string(),
            content_digest: workloop_selector::issue_content_digest(title, body),
            state,
            title: title.to_string(),
            body: body.to_string(),
        })
    }

    fn set_read_deadline(&mut self, deadline: Option<Instant>) {
        self.read_deadline = deadline;
    }
}
