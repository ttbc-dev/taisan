//! The versioned executable Worker contract (`docs/v0.3.md`):
//! `taisan.worker.request/2`, `taisan.worker.report/2`, and the
//! `taisan.worker.declaration/1` surface they negotiate against.
//!
//! These are the wire types only. Everything here is a closed
//! envelope: unknown fields are refused, a `format_version` other than
//! the exact string is refused, and readers never guess, downgrade, or
//! partially parse. Digests are SHA-256 over the tag, one ASCII colon,
//! and the canonical bytes of the object with its own digest member
//! omitted — nothing else is omitted or normalized.
//!
//! The durable v1 export (`worker.rs`) stays untouched: a request
//! recorded under `worker-request-v1` is never re-digested or read as
//! v2, and the v2 body is a separate durable row keyed by the dispatch
//! `request_id`.

use serde::{Deserialize, Serialize};

use crate::domain::{InvocationRole, canonical_json, sha256_hex};

/// The only request format this contract reads or writes.
pub const WORKER_REQUEST_V2_FORMAT: &str = "taisan.worker.request/2";
/// The only report format this contract reads or writes.
pub const WORKER_REPORT_V2_FORMAT: &str = "taisan.worker.report/2";
/// The only declaration format this contract reads or writes.
pub const WORKER_DECLARATION_V1_FORMAT: &str = "taisan.worker.declaration/1";

/// Digest domain tags (`docs/v0.3.md` §3.1 / OD-3).
const REQUEST_DIGEST_TAG: &str = "worker-request-v2";
const REPORT_DIGEST_TAG: &str = "worker-report-v2";

/// A repository identity on the wire: flat `host` + `repository`
/// strings, exactly as the request/report bodies carry them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WireRepository {
    pub host: String,
    pub repository: String,
}

/// An ordered Issue snapshot group entry: the group's `order` is
/// significant and readers never sort it (`docs/v0.3.md` OD-8).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GroupedIssue {
    pub order: u64,
    pub issue: WireIssue,
    /// The source revision the snapshot was taken at — a string, not a
    /// number, exactly as the source reports it.
    pub source_revision: String,
    /// Digest of the retained snapshot body for this exact revision.
    pub snapshot_digest: String,
    /// The retained immutable body reference.
    pub body: IssueBodyRef,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WireIssue {
    pub host: String,
    pub repository: String,
    /// The issue identity as a string (`"17"`), never a number.
    pub issue: String,
}

/// A content-addressed body reference: `digest` is SHA-256 over the
/// exact retained bytes tagged by the body's own type.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IssueBodyRef {
    /// The retained body's storage identity.
    #[serde(rename = "ref")]
    pub body_ref: String,
    pub bytes: u64,
    pub digest: String,
}

/// The fix/coding goal: a retained immutable body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GoalRef {
    #[serde(rename = "ref")]
    pub body_ref: String,
    pub media_type: String,
    pub bytes: u64,
    pub digest: String,
}

/// An immutable artifact reference: the acknowledged handoff record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactRef {
    pub artifact_id: String,
    pub manifest_digest: String,
    /// The produced commit (40 lowercase hex).
    pub produced: String,
}

/// The exact review target a CodeReview or review-origin Fix binds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DiffTarget {
    pub change: String,
    /// 40 lowercase hex.
    pub base: String,
    /// 40 lowercase hex.
    pub head: String,
    pub diff_digest: String,
}

/// One sealed review-finding occurrence selected for a review-origin
/// Fix — bound to the exact judgment and snapshot it came from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FindingOccurrenceRef {
    pub occurrence: String,
    pub source: String,
    pub digest: String,
    pub order: u64,
}

/// The failed mechanical evidence a pre-publication Fix repairs. It
/// never fabricates a change-request identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FailedEvidence {
    pub evidence_id: String,
    pub kind: String,
    pub digest: String,
}

/// What produced the Fix request: an exact sealed review judgment, or
/// failed verification/invariant evidence for an unpublished artifact.
/// Member sets are checked against the closed lists below — internally
/// tagged enums cannot carry `deny_unknown_fields`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum FixOrigin {
    Review {
        target: DiffTarget,
        /// The immutable review round; never released.
        round: String,
        /// The sealed judgment the findings were selected from.
        judgment: String,
        findings: Vec<FindingOccurrenceRef>,
    },
    Prepublication {
        failed_evidence: Vec<FailedEvidence>,
    },
}

/// The executable task body — `role` and `task.kind` must agree:
/// `coding`→`coding`, `fix`→`fix`, `invariant_check`→`invariant_check`,
/// `code_review`→`review`. Member sets are checked against the closed
/// lists below — internally tagged enums cannot carry
/// `deny_unknown_fields`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TaskBody {
    Coding {
        goal: GoalRef,
        /// Ordered grouped Issue snapshots; order is significant.
        issues: Vec<GroupedIssue>,
        constraints: Vec<String>,
        verification_argv: Vec<Vec<String>>,
    },
    Fix {
        goal: GoalRef,
        issues: Vec<GroupedIssue>,
        /// The predecessor artifact being corrected.
        predecessor: ArtifactRef,
        origin: Box<FixOrigin>,
        constraints: Vec<String>,
        verification_argv: Vec<Vec<String>>,
    },
    InvariantCheck {
        /// The exact artifact under check.
        artifact: ArtifactRef,
        invariant_set: InvariantSetRef,
    },
    CodeReview {
        target: DiffTarget,
        /// The retained snapshot the supervisor materialized; the
        /// Worker never fetches remote review input itself.
        review_source: ReviewSourceRef,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InvariantSetRef {
    pub id: String,
    pub revision: u64,
    pub digest: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewSourceRef {
    pub source: String,
    pub bytes: u64,
    pub digest: String,
    pub source_revision: String,
}

/// The workspace grant for one Invocation. `read_only` is required for
/// `invariant_check` and `review` and forbidden for `coding`/`fix`
/// (`docs/v0.3.md` §4.1, OD-7).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceGrant {
    pub workspace_id: String,
    pub mount: String,
    pub read_only: bool,
}

/// The dispatch budget. Every bound is integer; the supervisor stops
/// the process tree at the deadline and refuses oversize bodies before
/// paying for them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RequestBudget {
    /// Integer Unix seconds.
    pub deadline_unix_seconds: i64,
    pub max_runtime_seconds: u64,
    pub max_turns: u64,
    pub max_report_bytes: u64,
    pub max_log_bytes: u64,
    pub max_verification_commands: u64,
    pub max_continuation_refs: u64,
}

/// The declaration surface the request requires (§5.2): the declared
/// `protocol_versions` must contain both `/2` versions, `roles` must
/// contain this role, `capabilities` must cover this set, and `resume`
/// must be `unsupported`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RequiredCapabilities {
    /// The role the adapter must declare — equals the request `role`.
    pub role: InvocationRole,
    pub protocol_versions: Vec<String>,
    pub capabilities: Vec<String>,
    pub resume: String,
    pub verification_egress_hosts: Vec<String>,
}

/// A retained body the new attempt continues from — never a runtime
/// session (`resume` is unsupported; OD-9 bounds the list at
/// `max_continuation_refs`). Member sets are checked against the closed
/// lists below — internally tagged enums cannot carry
/// `deny_unknown_fields`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ContinuationRef {
    WorkspaceCommit {
        id: String,
        /// 40 lowercase hex.
        commit: String,
        required: bool,
    },
    Artifact {
        id: String,
        digest: String,
        required: bool,
    },
    Report {
        id: String,
        digest: String,
        required: bool,
    },
}

/// The executable request `taisan.worker.request/2`. `request_digest`
/// is SHA-256 over `worker-request-v2`, one colon, and the canonical
/// bytes of the request object with `request_digest` omitted.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerRequestV2 {
    pub format_version: String,
    /// Durable dispatch identity `<execution_id>:inv:<sequence>`.
    pub request_id: String,
    pub invocation_id: String,
    pub request_digest: String,
    pub store_id: String,
    pub work_id: String,
    pub execution_id: String,
    pub role: InvocationRole,
    pub repository: WireRepository,
    pub base: RequestBase,
    pub spec: SpecRef,
    pub workspace: WorkspaceGrant,
    pub budget: RequestBudget,
    pub required_capabilities: RequiredCapabilities,
    pub continuation: Vec<ContinuationRef>,
    pub task: TaskBody,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RequestBase {
    pub repository: WireRepository,
    /// 40 lowercase hex.
    pub object: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SpecRef {
    pub revision: u64,
    pub digest: String,
}

/// Why a request body is not executable. Codes are the closed set the
/// dispatch record carries (`docs/v0.3.md` §4.3, §9.2); provider prose
/// never enters a code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RequestInvalid {
    /// The body could not be parsed as the closed schema at all —
    /// malformed JSON, a missing member, or an unknown field.
    Malformed { detail: String },
    /// `format_version` is not the exact string this contract reads.
    VersionUnknown { declared: String },
    /// Internally inconsistent: role/task disagreement, a capability
    /// requirement naming another role, a wrong `read_only` for the
    /// role, a bound exceeded, or a duplicate group entry.
    Invalid {
        reason: &'static str,
        detail: String,
    },
    /// The declared `request_digest` does not match the body's own
    /// canonical digest — the record and the bytes disagree.
    DigestMismatch,
}

impl std::fmt::Display for RequestInvalid {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Malformed { detail } => write!(f, "request.malformed: {detail}"),
            Self::VersionUnknown { declared } => {
                write!(f, "request.version_unknown: {declared}")
            }
            Self::Invalid { reason, detail } => write!(f, "{reason}: {detail}"),
            Self::DigestMismatch => write!(f, "request.digest_mismatch"),
        }
    }
}

impl std::error::Error for RequestInvalid {}

/// The store's durable record of one canonical v2 request body: the
/// verbatim bytes replayed to the worker across restarts. The body is
/// never reconstructed from live configuration; a restart re-reads this
/// row. A second insert under the same `request_id` is a replay when
/// the bytes match and a refused conflict when they do not.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkerRequestBody {
    pub request_id: String,
    /// The recorded wire format — `"taisan.worker.request/2"` today.
    /// An [`InvocationRequest`] row with no body row is a legacy v1
    /// dispatch and is never silently made executable under v2.
    pub format_version: String,
    /// The exact canonical bytes written to the worker — digest member
    /// included, exactly as `WorkerRequestV2::to_canonical_bytes`
    /// produced them.
    pub canonical_body: Vec<u8>,
    /// `request_v2_digest` over the body minus `request_digest`,
    /// re-checked on every read so a tampered row reads as corrupt.
    pub body_digest: String,
}

impl WorkerRequestBody {
    /// The recorded format and digest re-checked over the stored body
    /// — `false` marks a corrupt or non-v2 row the reader must refuse
    /// to launch from.
    pub fn verifies(&self) -> bool {
        if self.format_version != WORKER_REQUEST_V2_FORMAT {
            return false;
        }
        let Ok(value) = serde_json::from_slice::<serde_json::Value>(&self.canonical_body) else {
            return false;
        };
        let digest = request_v2_digest(&canonical_minus(&value, "request_digest"));
        // Identity members are covered by the digest, so post-insert
        // tampering already fails it — but a row written with a
        // mismatched key/member pair verifies consistently. The gate
        // therefore re-checks the embedded identity against the row's
        // own fields, and the self-referential `request_digest`
        // member, which is excluded from its own digest, against the
        // recomputed value.
        self.body_digest == digest
            && value.get("request_id").and_then(serde_json::Value::as_str)
                == Some(self.request_id.as_str())
            && value
                .get("format_version")
                .and_then(serde_json::Value::as_str)
                == Some(self.format_version.as_str())
            && value
                .get("request_digest")
                .and_then(serde_json::Value::as_str)
                == Some(digest.as_str())
    }
}

/// SHA-256 over the canonical request bytes with `request_digest`
/// omitted — the digest the supervisor stores and validates before
/// writing the request file.
pub fn request_v2_digest(canonical_body: &[u8]) -> String {
    sha256_hex(REQUEST_DIGEST_TAG, canonical_body)
}

/// SHA-256 over the canonical report bytes with `report_digest`
/// omitted.
pub fn report_v2_digest(canonical_body: &[u8]) -> String {
    sha256_hex(REPORT_DIGEST_TAG, canonical_body)
}

/// Canonical bytes of a raw `taisan.worker.report/2` value with
/// `report_digest` omitted — the exact input `report_v2_digest`
/// covers. Digest verification must run on the transmitted value,
/// never on a typed re-serialization: `Option` members serialize
/// `None` as `null`, inventing members a valid wire report may omit.
pub fn report_canonical_body(value: &serde_json::Value) -> Vec<u8> {
    canonical_minus(value, "report_digest")
}

/// Canonical bytes of a JSON value minus one top-level member — the
/// exact bytes a request/report digest covers.
fn canonical_minus(value: &serde_json::Value, member: &str) -> Vec<u8> {
    let mut value = value.clone();
    if let Some(map) = value.as_object_mut() {
        map.remove(member);
    }
    canonical_json(&value)
}

/// The closed member set of each internally tagged variant — serde
/// cannot `deny_unknown_fields` on a `tag` enum, so `parse` checks the
/// keys of every tagged object against these lists instead.
fn check_members(
    object: &serde_json::Value,
    allowed: &[&str],
    where_in: &str,
) -> Result<(), RequestInvalid> {
    let Some(map) = object.as_object() else {
        return Err(RequestInvalid::Malformed {
            detail: format!("{where_in} is not an object"),
        });
    };
    for key in map.keys() {
        if !allowed.contains(&key.as_str()) {
            return Err(RequestInvalid::Malformed {
                detail: format!("unknown field {key:?} in {where_in}"),
            });
        }
    }
    Ok(())
}

/// Member-set validation for the tagged bodies a serde tag enum cannot
/// guard itself: `task`, `task.origin`, and every `continuation` entry.
fn check_tagged_members(request: &serde_json::Value) -> Result<(), RequestInvalid> {
    let Some(task) = request.get("task") else {
        return Err(RequestInvalid::Malformed {
            detail: "missing task".into(),
        });
    };
    let kind = task.get("kind").and_then(|k| k.as_str()).unwrap_or("");
    let allowed: &[&str] = match kind {
        "coding" => &["kind", "goal", "issues", "constraints", "verification_argv"],
        "fix" => &[
            "kind",
            "goal",
            "issues",
            "predecessor",
            "origin",
            "constraints",
            "verification_argv",
        ],
        "invariant_check" => &["kind", "artifact", "invariant_set"],
        "code_review" => &["kind", "target", "review_source"],
        _ => {
            return Err(RequestInvalid::Malformed {
                detail: format!("unknown task.kind {kind:?}"),
            });
        }
    };
    check_members(task, allowed, "task")?;
    if kind == "fix"
        && let Some(origin) = task.get("origin")
    {
        let okind = origin.get("kind").and_then(|k| k.as_str()).unwrap_or("");
        let oallowed: &[&str] = match okind {
            "review" => &["kind", "target", "round", "judgment", "findings"],
            "prepublication" => &["kind", "failed_evidence"],
            _ => {
                return Err(RequestInvalid::Malformed {
                    detail: format!("unknown origin.kind {okind:?}"),
                });
            }
        };
        check_members(origin, oallowed, "task.origin")?;
    }
    if let Some(continuation) = request.get("continuation").and_then(|c| c.as_array()) {
        for entry in continuation {
            let ckind = entry.get("kind").and_then(|k| k.as_str()).unwrap_or("");
            let callowed: &[&str] = match ckind {
                "workspace_commit" => &["kind", "id", "commit", "required"],
                "artifact" | "report" => &["kind", "id", "digest", "required"],
                _ => {
                    return Err(RequestInvalid::Malformed {
                        detail: format!("unknown continuation.kind {ckind:?}"),
                    });
                }
            };
            check_members(entry, callowed, "continuation")?;
        }
    }
    Ok(())
}

impl WorkerRequestV2 {
    /// Canonical body bytes with `request_digest` omitted — the exact
    /// bytes [`Self::digest`] covers. These are not the stored body:
    /// the durable row and the worker's wire body are
    /// [`Self::to_canonical_bytes`], which carry the digest member.
    pub fn canonical_body(&self) -> Vec<u8> {
        canonical_minus(
            &serde_json::to_value(self).expect("request serializes"),
            "request_digest",
        )
    }

    /// The full canonical request bytes — `request_digest` member
    /// included — exactly what the durable body row stores and what
    /// [`Self::parse`] accepts on verbatim replay.
    pub fn to_canonical_bytes(&self) -> Vec<u8> {
        canonical_json(&serde_json::to_value(self).expect("request serializes"))
    }

    /// The digest of this request's canonical body.
    pub fn digest(&self) -> String {
        request_v2_digest(&self.canonical_body())
    }

    /// Parse a request body and validate it: exact format version,
    /// closed envelope, role/task agreement, capability role echo,
    /// `read_only` by role, continuation/verification bounds, ordered
    /// group uniqueness, base/repository identity, and the declared
    /// digest. A passing request is dispatch-shaped; the dispatch
    /// preconditions of §4.3 (body freshness, spec/spec revision, head
    /// checks) are the supervisor's and evaluated separately.
    pub fn parse(raw: &[u8]) -> Result<Self, RequestInvalid> {
        let value: serde_json::Value =
            serde_json::from_slice(raw).map_err(|e| RequestInvalid::Malformed {
                detail: format!("not JSON: {e}"),
            })?;
        let declared = value
            .get("format_version")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        if declared != WORKER_REQUEST_V2_FORMAT {
            return Err(RequestInvalid::VersionUnknown { declared });
        }
        check_tagged_members(&value)?;
        // The digest is verified over the raw body minus its digest
        // member — never over a re-serialization, whose optional-field
        // normalization could invent a mismatch.
        let body_digest = request_v2_digest(&canonical_minus(&value, "request_digest"));
        let request: Self = serde_json::from_value(value).map_err(|e| {
            let detail = e.to_string();
            RequestInvalid::Malformed { detail }
        })?;
        request.validate()?;
        if request.request_digest != body_digest {
            return Err(RequestInvalid::DigestMismatch);
        }
        Ok(request)
    }

    /// The internal consistency rules of §4.2/§5.2: every check fails
    /// the whole request before any process exists.
    pub fn validate(&self) -> Result<(), RequestInvalid> {
        let invalid =
            |reason: &'static str, detail: String| RequestInvalid::Invalid { reason, detail };
        // `role` and `task.kind` must agree (§4.2).
        let task_role = match &self.task {
            TaskBody::Coding { .. } => InvocationRole::Coding,
            TaskBody::Fix { .. } => InvocationRole::Fix,
            TaskBody::InvariantCheck { .. } => InvocationRole::InvariantCheck,
            TaskBody::CodeReview { .. } => InvocationRole::Review,
        };
        if task_role != self.role {
            return Err(invalid(
                "request.invalid",
                format!(
                    "role {:?} but task.kind agrees with {task_role:?}",
                    self.role
                ),
            ));
        }
        // The capability echo names the same role (§5.2); a request
        // that asks the adapter for another role is inconsistent.
        if self.required_capabilities.role != self.role {
            return Err(invalid(
                "request.invalid",
                format!(
                    "required_capabilities.role {:?} disagrees with role {:?}",
                    self.required_capabilities.role, self.role
                ),
            ));
        }
        // `resume` is unsupported for any v2 dispatch (§5.2): the
        // request must not demand a resume mode the contract does not
        // define — continuation refs are the only carry-over.
        if self.required_capabilities.resume != "unsupported" {
            return Err(invalid(
                "request.invalid",
                format!(
                    "required_capabilities.resume {:?} — v2 resume is unsupported",
                    self.required_capabilities.resume
                ),
            ));
        }
        // The workspace is read-only exactly for the read roles
        // (§4.1, OD-7); a read-write grant for a read role is invalid.
        let read_only = matches!(
            self.role,
            InvocationRole::InvariantCheck | InvocationRole::Review
        );
        if self.workspace.read_only != read_only {
            return Err(invalid(
                "request.invalid",
                format!(
                    "role {:?} requires workspace.read_only={read_only}, request grants {}",
                    self.role, self.workspace.read_only
                ),
            ));
        }
        // The base commits to the same repository the request names.
        if self.base.repository != self.repository {
            return Err(invalid(
                "request.invalid",
                "base.repository names a foreign repository".into(),
            ));
        }
        // Bounds (§4.2, OD-9).
        if self.continuation.len() as u64 > self.budget.max_continuation_refs {
            return Err(invalid(
                "request.invalid",
                format!(
                    "continuation has {} refs, budget allows {}",
                    self.continuation.len(),
                    self.budget.max_continuation_refs
                ),
            ));
        }
        let verification_len = |argv: &Vec<Vec<String>>| argv.len() as u64;
        let task_verify = match &self.task {
            TaskBody::Coding {
                verification_argv, ..
            }
            | TaskBody::Fix {
                verification_argv, ..
            } => verification_len(verification_argv),
            _ => 0,
        };
        if task_verify > self.budget.max_verification_commands {
            return Err(invalid(
                "request.invalid",
                format!(
                    "verification_argv has {task_verify} commands, budget allows {}",
                    self.budget.max_verification_commands
                ),
            ));
        }
        // Ordered groups are never sorted or deduplicated, but a
        // duplicate issue identity in one request is invalid (OD-8).
        let issues = match &self.task {
            TaskBody::Coding { issues, .. } | TaskBody::Fix { issues, .. } => issues.as_slice(),
            _ => &[],
        };
        for (i, entry) in issues.iter().enumerate() {
            if issues[..i].iter().any(|prior| prior.issue == entry.issue) {
                return Err(invalid(
                    "request.invalid",
                    format!(
                        "duplicate issue identity {} in group order",
                        entry.issue.issue
                    ),
                ));
            }
        }
        Ok(())
    }
}

/// The v2-era adapter declaration `taisan.worker.declaration/1`: the
/// exact surface the capability digest and the v2 negotiation read
/// (`docs/v0.3.md` §5.1). Still effect-free and fail-closed — this
/// type only models what a conforming `describe` prints.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerDeclarationV1 {
    pub format_version: String,
    pub adapter: String,
    pub adapter_version: String,
    pub runtime: DeclarationRuntime,
    pub protocol_versions: Vec<String>,
    pub roles: Vec<String>,
    pub capabilities: Vec<String>,
    /// Must equal `unsupported` for any v2 dispatch.
    pub resume: String,
    pub checkpoints: String,
    /// Free-form adapter detail; `null` for the reference adapter.
    pub runtime_artifact_store: serde_json::Value,
    pub local_retries: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeclarationRuntime {
    pub product: String,
    pub version: String,
}

/// Why a v2 request may not start under a declaration. The codes are
/// the negotiation surface of §5.2 plus the cutover version refusal of
/// §9.2.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NegotiationRefusal {
    /// The declaration does not offer both `/2` wire versions — or is
    /// not a declaration this binary can read.
    VersionUnsupported { detail: String },
    /// The declaration's `roles` lacks the request role.
    RoleMismatch { role: String },
    /// A required capability is absent from the declared set.
    CapabilityMissing { capability: String },
}

impl std::fmt::Display for NegotiationRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::VersionUnsupported { detail } => {
                write!(f, "request.version_unsupported: {detail}")
            }
            Self::RoleMismatch { role } => {
                write!(f, "adapter.role_mismatch: declaration lacks role {role}")
            }
            Self::CapabilityMissing { capability } => {
                write!(f, "capability.missing: {capability}")
            }
        }
    }
}

impl std::error::Error for NegotiationRefusal {}

impl WorkerDeclarationV1 {
    /// Parse a `describe` body: exact format version, closed envelope.
    /// Everything else (spawn refused, non-zero exit, timeout,
    /// missing adapter id) stays `adapter.declaration_unavailable` at
    /// the probe boundary — this is the parse step only.
    pub fn parse(raw: &[u8]) -> Result<Self, NegotiationRefusal> {
        let value: serde_json::Value =
            serde_json::from_slice(raw).map_err(|e| NegotiationRefusal::VersionUnsupported {
                detail: format!("not JSON: {e}"),
            })?;
        let declared = value
            .get("format_version")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        if declared != WORKER_DECLARATION_V1_FORMAT {
            return Err(NegotiationRefusal::VersionUnsupported {
                detail: format!("declared format_version {declared:?}"),
            });
        }
        serde_json::from_value(value).map_err(|e| NegotiationRefusal::VersionUnsupported {
            detail: e.to_string(),
        })
    }

    /// The exact v2 negotiation of §5.2: both `/2` wire versions, the
    /// request role, the required capability set, and
    /// `resume == unsupported`. The first failure refuses the start;
    /// nothing is negotiated best-effort.
    pub fn negotiate_v2(&self, request: &WorkerRequestV2) -> Result<(), NegotiationRefusal> {
        for version in [WORKER_REQUEST_V2_FORMAT, WORKER_REPORT_V2_FORMAT] {
            if !self.protocol_versions.iter().any(|v| v == version) {
                return Err(NegotiationRefusal::VersionUnsupported {
                    detail: format!("declaration lacks protocol {version}"),
                });
            }
        }
        let role = serde_json::to_value(&request.role)
            .ok()
            .and_then(|v| v.as_str().map(String::from))
            .unwrap_or_default();
        if !self.roles.contains(&role) {
            return Err(NegotiationRefusal::RoleMismatch { role });
        }
        for capability in &request.required_capabilities.capabilities {
            if !self.capabilities.contains(capability) {
                return Err(NegotiationRefusal::CapabilityMissing {
                    capability: capability.clone(),
                });
            }
        }
        if self.resume != "unsupported" {
            return Err(NegotiationRefusal::CapabilityMissing {
                capability: format!("resume:unsupported (declared {:?})", self.resume),
            });
        }
        Ok(())
    }
}

/// The Worker-authored report status — the closed set of §6.2.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerReportStatus {
    Success,
    Negative,
    Unavailable,
    Refused,
    LimitReached,
    Failed,
}

impl WorkerReportStatus {
    /// Whether `code` is in this status's allowed reason set (§6.2).
    pub fn allows_reason(&self, code: &str) -> bool {
        let allowed: &[&str] = match self {
            Self::Success => &["completed"],
            Self::Negative => &["checks_failed", "invariant_violated", "findings_reported"],
            Self::Unavailable => &[
                "provider_unavailable",
                "auth_unavailable",
                "missing_required_input",
            ],
            Self::Refused => &["provider_refused"],
            Self::LimitReached => &[
                "deadline_reached",
                "max_turns_reached",
                "output_limit_reached",
            ],
            Self::Failed => &["invalid_request", "unsupported_role", "internal_error"],
        };
        allowed.contains(&code)
    }
}

/// The report's typed failure reason — provider prose stays in the
/// bounded `runtime.log`, never here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReportReason {
    pub code: String,
}

/// One declared check the Worker claims to have run. A claim only — a
/// supervisor `VerificationResult` on the exact artifact/argv/commit
/// is what satisfies a mechanical requirement.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClaimedCheck {
    pub argv: Vec<String>,
    pub exit_code: i64,
    pub output_digest: String,
    pub status: ClaimedCheckStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClaimedCheckStatus {
    Passed,
    Failed,
}

/// The bounded runtime-usage claim. Every metric is `Option`: an
/// absent value is *unknown*, never zero — a worker that measured
/// nothing must not read as one that measured zero. `runtime_product`
/// records the actual runtime/model the worker ran under.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeUsage {
    pub turns: Option<u64>,
    pub elapsed_ms: Option<u64>,
    pub runtime_product: String,
}

/// What the new attempt may continue from (§6.3): the produced
/// workspace commit plus optional report references. All are
/// references into retained bodies, never a runtime session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReportContinuation {
    pub workspace_commit: Option<String>,
    pub report_refs: Vec<serde_json::Value>,
}

/// The bounded log body descriptor; a truncated log is retained and
/// marked, its missing tail never reconstructed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReportLog {
    pub path: String,
    pub bytes: u64,
    pub truncated: bool,
    pub digest: String,
}

/// The Worker report `taisan.worker.report/2`. `findings` and
/// `invariant_items` are Worker claims whose member shape belongs to
/// the evidence gates, not this envelope — they are retained verbatim
/// so no member is dropped or re-digested.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerReportV2 {
    pub format_version: String,
    pub request_id: String,
    pub invocation_id: String,
    pub request_digest: String,
    pub report_digest: String,
    pub role: InvocationRole,
    pub status: WorkerReportStatus,
    pub reason: ReportReason,
    /// A claim only; the handoff decides the produced artifact.
    pub claimed_head: Option<String>,
    pub claimed_checks: Vec<ClaimedCheck>,
    /// Claims bound to the exact review source/target digests.
    pub findings: Vec<serde_json::Value>,
    /// Claims for the exact invariant set.
    pub invariant_items: Vec<serde_json::Value>,
    pub continuation: ReportContinuation,
    pub runtime_usage: Option<RuntimeUsage>,
    pub log: Option<ReportLog>,
}

/// The binding a report must echo: the exact request it answers.
pub struct ReportBinding<'a> {
    pub request_id: &'a str,
    pub invocation_id: &'a str,
    pub request_digest: &'a str,
    pub role: InvocationRole,
}

/// The supervisor-owned report classification (§6.1): what the
/// validated envelope means, never what the Worker claims.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReportClassification {
    /// A valid positive claim for this exact request — still only
    /// evidence; handoff and verification stand between it and any
    /// Outcome.
    Success,
    /// A valid negative result (failed checks, invariant violations,
    /// findings) — never success and never a publication.
    Negative,
    /// The Worker or a required input was unavailable — distinct from
    /// a negative result and from absence of findings.
    Unavailable,
    /// A typed Worker failure (`refused`, `limit_reached`, `failed`):
    /// no automatic outer retry is authorized by the report.
    Failure { code: String },
    /// The report exists but is malformed, oversized, wrong
    /// version/identity/role/digest, or carries unknown
    /// fields/reasons. `reasons` retains every violated binding (a
    /// report can be wrong on several axes at once, §7.4); never
    /// parsed leniently.
    Invalid { reasons: Vec<String> },
    /// No report after confirmed exit — never an empty success and
    /// never "zero findings".
    Absent,
}

/// The report plus its classification: the envelope is only retained
/// for the positive classes; an `invalid` report's raw bytes are
/// evidence but never a parsed claim.
#[derive(Debug)]
pub struct ClassifiedReport {
    pub classification: ReportClassification,
    pub report: Option<WorkerReportV2>,
}

/// Read a report body against the binding of the exact request it
/// answers. `max_bytes` applies before parsing — an oversize body is
/// `invalid` (`report.oversize`) and never parsed. Identity checks
/// collect every mismatch (§7.4), so `invalid` names each broken
/// binding. `absent` is produced by the caller (no file after
/// confirmed exit), not here.
pub fn classify_report(
    raw: &[u8],
    binding: &ReportBinding<'_>,
    max_bytes: u64,
) -> ClassifiedReport {
    let invalid = |reasons: Vec<String>| ClassifiedReport {
        classification: ReportClassification::Invalid { reasons },
        report: None,
    };
    if raw.len() as u64 > max_bytes {
        return invalid(vec!["report.oversize".into()]);
    }
    let value: serde_json::Value = match serde_json::from_slice(raw) {
        Ok(v) => v,
        Err(e) => return invalid(vec![format!("report.malformed: {e}")]),
    };
    // Structural faults stop classification before identity checks:
    // a body that is not the closed envelope at all cannot echo a
    // meaningful identity either way.
    let format = value
        .get("format_version")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    if format != WORKER_REPORT_V2_FORMAT {
        return invalid(vec!["report.wrong_version".into()]);
    }
    let report: WorkerReportV2 = match serde_json::from_value(value.clone()) {
        Ok(r) => r,
        Err(e) => {
            let detail = e.to_string();
            let reason = if detail.contains("unknown field") {
                format!("report.unknown_field: {detail}")
            } else {
                format!("report.malformed: {detail}")
            };
            return invalid(vec![reason]);
        }
    };
    // The declared digest is verified over the raw body minus its
    // digest member, never a re-serialization.
    let body_digest = report_v2_digest(&report_canonical_body(&value));
    let mut reasons: Vec<String> = Vec::new();
    if report.request_id != binding.request_id {
        reasons.push("report.wrong_request".into());
    }
    if report.invocation_id != binding.invocation_id {
        reasons.push("report.wrong_invocation".into());
    }
    if report.role != binding.role {
        reasons.push("report.wrong_role".into());
    }
    if report.request_digest != binding.request_digest {
        reasons.push("report.wrong_request".into());
    }
    if report.report_digest != body_digest {
        reasons.push("report.digest_mismatch".into());
    }
    if !report.status.allows_reason(&report.reason.code) {
        reasons.push(format!(
            "report.malformed: reason {:?} not allowed for status {:?}",
            report.reason.code, report.status
        ));
    }
    if !reasons.is_empty() {
        reasons.dedup();
        return invalid(reasons);
    }
    let classification = match report.status {
        WorkerReportStatus::Success => ReportClassification::Success,
        WorkerReportStatus::Negative => ReportClassification::Negative,
        WorkerReportStatus::Unavailable => ReportClassification::Unavailable,
        WorkerReportStatus::Refused
        | WorkerReportStatus::LimitReached
        | WorkerReportStatus::Failed => ReportClassification::Failure {
            code: report.reason.code.clone(),
        },
    };
    ClassifiedReport {
        classification,
        report: Some(report),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The valid Coding request of `docs/v0.3.md` §7.2 — canonical,
    /// with a real verifying `request_digest`.
    const DOC_REQUEST: &str = r#"{"base":{"object":"0123456789abcdef0123456789abcdef01234567","repository":{"host":"host-1","repository":"repo-a"}},"budget":{"deadline_unix_seconds":1800000600,"max_continuation_refs":4,"max_log_bytes":33554432,"max_report_bytes":1048576,"max_runtime_seconds":1800,"max_turns":200,"max_verification_commands":8},"continuation":[],"execution_id":"e-7","format_version":"taisan.worker.request/2","invocation_id":"inv-41","repository":{"host":"host-1","repository":"repo-a"},"request_digest":"f6b30b3d524d1d883b46ae87ecf575965077695ad296a4ef9fbc9146974bc489","request_id":"e-7:inv:2","required_capabilities":{"capabilities":["invocation_transport.argv-v2","report_json","workspace_write","process_control"],"protocol_versions":["taisan.worker.request/2"],"resume":"unsupported","role":"coding","verification_egress_hosts":[]},"role":"coding","spec":{"digest":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","revision":3},"store_id":"store-1","task":{"constraints":["Do not change public API names"],"goal":{"bytes":812,"digest":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb","media_type":"text/markdown","ref":"goal-7"},"issues":[{"body":{"bytes":2048,"digest":"cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc","ref":"issue-17-v9"},"issue":{"host":"host-1","issue":"17","repository":"repo-a"},"order":0,"snapshot_digest":"dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd","source_revision":"9"}],"kind":"coding","verification_argv":[["cargo","test","-p","workloop-core"]]},"work_id":"w-7","workspace":{"mount":"/workspace","read_only":false,"workspace_id":"ws-w-7"}}"#;

    /// The valid negative Coding report of §7.3 — echoes §7.2's
    /// identity, `report_digest` verifies.
    const DOC_REPORT: &str = r#"{"claimed_checks":[{"argv":["cargo","test","-p","workloop-core"],"exit_code":101,"output_digest":"eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee","status":"failed"}],"claimed_head":null,"continuation":{"report_refs":[],"workspace_commit":null},"findings":[],"format_version":"taisan.worker.report/2","invariant_items":[],"invocation_id":"inv-41","log":{"bytes":800,"digest":"ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff","path":"runtime.log","truncated":false},"reason":{"code":"checks_failed"},"report_digest":"f057b455539282e75af552dc5fd8da03ac827157a0ef2ec4415484019032ff2d","request_digest":"f6b30b3d524d1d883b46ae87ecf575965077695ad296a4ef9fbc9146974bc489","request_id":"e-7:inv:2","role":"coding","runtime_usage":{"elapsed_ms":4120,"runtime_product":"example-runtime","turns":8},"status":"negative"}"#;

    /// The invalid report of §7.4: digests verify, only
    /// `invocation_id` and `role` differ.
    const DOC_INVALID_REPORT: &str = r#"{"format_version":"taisan.worker.report/2","request_id":"e-7:inv:2","invocation_id":"inv-99","request_digest":"f6b30b3d524d1d883b46ae87ecf575965077695ad296a4ef9fbc9146974bc489","report_digest":"19f95ca07284740384a3c452a307df06fa59cf5bb34e83ec15ca1e1dfecb8ea8","role":"review","status":"success","reason":{"code":"completed"},"claimed_head":null,"claimed_checks":[],"findings":[],"invariant_items":[],"continuation":{"workspace_commit":null,"report_refs":[]},"runtime_usage":null,"log":{"path":"runtime.log","bytes":10,"truncated":false,"digest":"0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"}}"#;

    fn doc_binding() -> ReportBinding<'static> {
        ReportBinding {
            request_id: "e-7:inv:2",
            invocation_id: "inv-41",
            request_digest: "f6b30b3d524d1d883b46ae87ecf575965077695ad296a4ef9fbc9146974bc489",
            role: InvocationRole::Coding,
        }
    }

    #[test]
    fn the_documented_request_digest_verifies() {
        let request = WorkerRequestV2::parse(DOC_REQUEST.as_bytes()).expect("valid request");
        assert_eq!(request.request_id, "e-7:inv:2");
        assert_eq!(request.role, InvocationRole::Coding);
        assert_eq!(
            request.digest(),
            "f6b30b3d524d1d883b46ae87ecf575965077695ad296a4ef9fbc9146974bc489"
        );
    }

    #[test]
    fn every_role_task_round_trips() {
        let request = WorkerRequestV2::parse(DOC_REQUEST.as_bytes()).expect("base request");
        for (kind, task) in [
            (
                "fix",
                TaskBody::Fix {
                    goal: GoalRef {
                        body_ref: "goal-7-fix-1".into(),
                        media_type: "text/markdown".into(),
                        bytes: 640,
                        digest: "b".repeat(64),
                    },
                    issues: vec![],
                    predecessor: ArtifactRef {
                        artifact_id: "inv-39/artifact.json".into(),
                        manifest_digest: "c".repeat(64),
                        produced: "0123456789abcdef0123456789abcdef01234567".into(),
                    },
                    origin: Box::new(FixOrigin::Review {
                        target: DiffTarget {
                            change: "change-5".into(),
                            base: "0123456789abcdef0123456789abcdef01234567".into(),
                            head: "89abcdef0123456789abcdef0123456789abcdef".into(),
                            diff_digest: "d".repeat(64),
                        },
                        round: "round-3".into(),
                        judgment: "judgment-9".into(),
                        findings: vec![FindingOccurrenceRef {
                            occurrence: "occ-2".into(),
                            source: "review-source-3".into(),
                            digest: "e".repeat(64),
                            order: 0,
                        }],
                    }),
                    constraints: vec!["Address only the selected finding".into()],
                    verification_argv: vec![vec!["cargo".into(), "test".into()]],
                },
            ),
            (
                "invariant_check",
                TaskBody::InvariantCheck {
                    artifact: ArtifactRef {
                        artifact_id: "inv-39/artifact.json".into(),
                        manifest_digest: "c".repeat(64),
                        produced: "0123456789abcdef0123456789abcdef01234567".into(),
                    },
                    invariant_set: InvariantSetRef {
                        id: "invariants-core".into(),
                        revision: 4,
                        digest: "f".repeat(64),
                    },
                },
            ),
            (
                "code_review",
                TaskBody::CodeReview {
                    target: DiffTarget {
                        change: "change-5".into(),
                        base: "0123456789abcdef0123456789abcdef01234567".into(),
                        head: "89abcdef0123456789abcdef0123456789abcdef".into(),
                        diff_digest: "d".repeat(64),
                    },
                    review_source: ReviewSourceRef {
                        source: "review-source-3".into(),
                        bytes: 4096,
                        digest: "a".repeat(64),
                        source_revision: "3".into(),
                    },
                },
            ),
        ] {
            let mut draft = request.clone();
            draft.task = task;
            draft.role = match kind {
                "fix" => InvocationRole::Fix,
                "invariant_check" => InvocationRole::InvariantCheck,
                _ => InvocationRole::Review,
            };
            draft.required_capabilities.role = draft.role.clone();
            draft.workspace.read_only = matches!(
                draft.role,
                InvocationRole::InvariantCheck | InvocationRole::Review
            );
            draft.request_digest = draft.digest();
            let bytes = draft.canonical_body();
            let mut value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            value["request_digest"] = serde_json::Value::String(draft.request_digest.clone());
            let wire = canonical_json(&value);
            let parsed = WorkerRequestV2::parse(&wire)
                .unwrap_or_else(|e| panic!("{kind} round-trip failed: {e}"));
            assert_eq!(parsed.task, draft.task, "{kind}");
        }
    }

    /// The durable body stores `to_canonical_bytes` — digest member
    /// included — and verbatim replay parses it; `WorkerRequestBody`
    /// verification covers both the digest and the format version.
    #[test]
    fn the_durable_body_round_trips_with_its_digest() {
        let mut request = WorkerRequestV2::parse(DOC_REQUEST.as_bytes()).expect("valid request");
        request.request_digest = request.digest();
        let bytes = request.to_canonical_bytes();
        let parsed = WorkerRequestV2::parse(&bytes).expect("stored bytes parse on replay");
        assert_eq!(parsed.request_digest, request.request_digest);

        let body = WorkerRequestBody {
            request_id: request.request_id.clone(),
            format_version: WORKER_REQUEST_V2_FORMAT.into(),
            canonical_body: bytes,
            body_digest: request.digest(),
        };
        assert!(body.verifies());

        // The embedded `request_digest` member is excluded from its
        // own digest, so altering that member alone must still read
        // as corrupt — the gate re-checks it explicitly.
        let mut member_tampered = body.clone();
        let mut value: serde_json::Value =
            serde_json::from_slice(&member_tampered.canonical_body).unwrap();
        value["request_digest"] = serde_json::Value::String("0".repeat(64));
        member_tampered.canonical_body = canonical_json(&value);
        assert!(
            !member_tampered.verifies(),
            "a tampered digest member reads as corrupt"
        );

        let mut tampered = body.clone();
        tampered.canonical_body = b"{}".to_vec();
        assert!(!tampered.verifies(), "a tampered body reads as corrupt");
        let mut foreign = body.clone();
        foreign.format_version = "taisan.worker.request/9".into();
        assert!(!foreign.verifies(), "a non-v2 row reads as corrupt");

        // A row whose embedded `request_id` names a different
        // dispatch verifies consistently — the digest covers whatever
        // content was stored — so the gate re-checks the embedded
        // identity against the row's own key.
        let mut foreign_id = body.clone();
        let mut value: serde_json::Value =
            serde_json::from_slice(&foreign_id.canonical_body).unwrap();
        value["request_id"] = serde_json::Value::String("e-9:inv:9".into());
        foreign_id.body_digest = request_v2_digest(&canonical_minus(&value, "request_digest"));
        foreign_id.canonical_body = canonical_json(&value);
        assert!(
            !foreign_id.verifies(),
            "a mismatched request_id reads as corrupt"
        );
    }

    #[test]
    fn an_unknown_version_is_refused() {
        let body = DOC_REQUEST.replace("taisan.worker.request/2", "taisan.worker.request/9");
        // The digest no longer verifies either, but version refusal
        // must fire first — never a fallback parse.
        match WorkerRequestV2::parse(body.as_bytes()) {
            Err(RequestInvalid::VersionUnknown { declared }) => {
                assert_eq!(declared, "taisan.worker.request/9")
            }
            other => panic!("expected version_unknown, got {other:?}"),
        }
    }

    /// The valid request body minus nothing, plus one foreign member —
    /// with its digest corrected so only the envelope check can refuse
    /// it.
    fn doc_request_with(extra: &str) -> Vec<u8> {
        let mut value: serde_json::Value = serde_json::from_str(DOC_REQUEST).unwrap();
        let patch: serde_json::Value = serde_json::from_str(extra).unwrap();
        for (k, v) in patch.as_object().unwrap() {
            value[k.clone()] = v.clone();
        }
        value["request_digest"] = serde_json::Value::String(request_v2_digest(&canonical_minus(
            &value,
            "request_digest",
        )));
        canonical_json(&value)
    }

    #[test]
    fn a_non_json_request_is_malformed() {
        match WorkerRequestV2::parse(b"not json") {
            Err(RequestInvalid::Malformed { detail }) => {
                assert!(detail.contains("not JSON"), "{detail}")
            }
            other => panic!("expected malformed, got {other:?}"),
        }
    }

    #[test]
    fn an_unknown_envelope_member_is_refused() {
        let body = doc_request_with(r#"{"operator_hint":"skip checks"}"#);
        match WorkerRequestV2::parse(&body) {
            Err(RequestInvalid::Malformed { detail }) => {
                assert!(detail.contains("unknown field"), "{detail}")
            }
            other => panic!("expected malformed, got {other:?}"),
        }
    }

    #[test]
    fn a_member_foreign_to_the_task_variant_is_refused() {
        // `predecessor` exists on `fix`; on a `coding` task it is an
        // unknown member — the closed set is per variant, and serde's
        // internal tag cannot enforce it, so the explicit check must.
        let body = doc_request_with(
            r#"{"task":{"kind":"coding","goal":{"ref":"goal-7","media_type":"text/markdown","bytes":812,"digest":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"},"issues":[],"constraints":[],"verification_argv":[],"predecessor":{"artifact_id":"x","manifest_digest":"d","produced":"p"}}}"#,
        );
        match WorkerRequestV2::parse(&body) {
            Err(RequestInvalid::Malformed { detail }) => {
                assert!(detail.contains("unknown field"), "{detail}")
            }
            other => panic!("expected malformed, got {other:?}"),
        }
    }

    #[test]
    fn an_unknown_continuation_kind_is_refused() {
        let body = doc_request_with(
            r#"{"continuation":[{"kind":"live_session","id":"s-1","required":false}]}"#,
        );
        match WorkerRequestV2::parse(&body) {
            Err(RequestInvalid::Malformed { detail }) => {
                assert!(detail.contains("unknown continuation.kind"), "{detail}")
            }
            other => panic!("expected malformed, got {other:?}"),
        }
    }

    #[test]
    fn a_digest_over_wrong_bytes_is_refused() {
        let mut value: serde_json::Value = serde_json::from_str(DOC_REQUEST).unwrap();
        value["store_id"] = serde_json::Value::String("store-2".into());
        // The declared digest is stale — it covered other bytes.
        match WorkerRequestV2::parse(&canonical_json(&value)) {
            Err(RequestInvalid::DigestMismatch) => {}
            other => panic!("expected digest_mismatch, got {other:?}"),
        }
    }

    #[test]
    fn an_over_budget_continuation_is_invalid() {
        let mut request = WorkerRequestV2::parse(DOC_REQUEST.as_bytes()).unwrap();
        request.continuation = vec![
            ContinuationRef::Artifact {
                id: "a-1".into(),
                digest: "a".repeat(64),
                required: false,
            };
            request.budget.max_continuation_refs as usize + 1
        ];
        match request.validate() {
            Err(RequestInvalid::Invalid { detail, .. }) => {
                assert!(detail.contains("continuation"), "{detail}")
            }
            other => panic!("expected invalid, got {other:?}"),
        }
    }

    #[test]
    fn an_over_budget_verification_argv_is_invalid() {
        let mut request = WorkerRequestV2::parse(DOC_REQUEST.as_bytes()).unwrap();
        let TaskBody::Coding {
            verification_argv, ..
        } = &mut request.task
        else {
            unreachable!()
        };
        *verification_argv =
            vec![vec!["true".into()]; request.budget.max_verification_commands as usize + 1];
        match request.validate() {
            Err(RequestInvalid::Invalid { detail, .. }) => {
                assert!(detail.contains("verification_argv"), "{detail}")
            }
            other => panic!("expected invalid, got {other:?}"),
        }
    }

    #[test]
    fn role_task_disagreement_is_invalid() {
        let mut request = WorkerRequestV2::parse(DOC_REQUEST.as_bytes()).unwrap();
        request.role = InvocationRole::Review;
        request.required_capabilities.role = InvocationRole::Review;
        request.workspace.read_only = true;
        request.request_digest = request.digest();
        match request.validate() {
            Err(RequestInvalid::Invalid { reason, .. }) => {
                assert_eq!(reason, "request.invalid")
            }
            other => panic!("expected invalid, got {other:?}"),
        }
    }

    #[test]
    fn a_read_role_cannot_claim_a_writable_workspace() {
        // The §7.5 invalid request: review role, read_only false.
        let mut request = WorkerRequestV2::parse(DOC_REQUEST.as_bytes()).unwrap();
        request.role = InvocationRole::Review;
        request.required_capabilities.role = InvocationRole::Review;
        request.task = TaskBody::CodeReview {
            target: DiffTarget {
                change: "change-5".into(),
                base: "0".repeat(40),
                head: "1".repeat(40),
                diff_digest: "d".repeat(64),
            },
            review_source: ReviewSourceRef {
                source: "review-source-3".into(),
                bytes: 4096,
                digest: "a".repeat(64),
                source_revision: "3".into(),
            },
        };
        match request.validate() {
            Err(RequestInvalid::Invalid { detail, .. }) => {
                assert!(detail.contains("read_only"), "{detail}")
            }
            other => panic!("expected invalid, got {other:?}"),
        }
    }

    #[test]
    fn a_request_demanding_resume_is_invalid() {
        let mut request = WorkerRequestV2::parse(DOC_REQUEST.as_bytes()).unwrap();
        request.required_capabilities.resume = "session".into();
        match request.validate() {
            Err(RequestInvalid::Invalid { detail, .. }) => {
                assert!(detail.contains("resume"), "{detail}")
            }
            other => panic!("expected invalid, got {other:?}"),
        }
    }

    #[test]
    fn duplicate_group_entries_are_invalid() {
        let mut request = WorkerRequestV2::parse(DOC_REQUEST.as_bytes()).unwrap();
        let TaskBody::Coding { issues, .. } = &mut request.task else {
            unreachable!()
        };
        issues.push(issues[0].clone());
        issues[1].order = 1;
        match request.validate() {
            Err(RequestInvalid::Invalid { detail, .. }) => {
                assert!(detail.contains("duplicate issue identity"), "{detail}")
            }
            other => panic!("expected invalid, got {other:?}"),
        }
    }

    #[test]
    fn the_documented_report_digest_verifies() {
        let classified = classify_report(DOC_REPORT.as_bytes(), &doc_binding(), 1_048_576);
        assert_eq!(classified.classification, ReportClassification::Negative);
        let report = classified.report.expect("negative report parses");
        // The digest is verified over the transmitted value, never a
        // typed re-serialization — `Option` members would invent
        // `null`s the wire body may not carry.
        let value: serde_json::Value = serde_json::from_slice(DOC_REPORT.as_bytes()).unwrap();
        assert_eq!(
            report_v2_digest(&report_canonical_body(&value)),
            "f057b455539282e75af552dc5fd8da03ac827157a0ef2ec4415484019032ff2d"
        );
        assert_eq!(report.reason.code, "checks_failed");
        assert_eq!(report.runtime_usage.as_ref().unwrap().turns, Some(8));
    }

    /// `Option` members may be absent on the wire; the digest covers
    /// the transmitted value, so omission — not an invented `null` —
    /// is what the Worker hashed.
    #[test]
    fn a_report_omitting_optional_members_still_verifies() {
        let mut value: serde_json::Value = serde_json::from_slice(DOC_REPORT.as_bytes()).unwrap();
        let map = value.as_object_mut().unwrap();
        for member in ["claimed_head", "runtime_usage", "log"] {
            map.remove(member);
        }
        value["continuation"]
            .as_object_mut()
            .unwrap()
            .remove("workspace_commit");
        value["report_digest"] =
            serde_json::Value::String(report_v2_digest(&report_canonical_body(&value)));
        let raw = canonical_json(&value);
        let classified = classify_report(&raw, &doc_binding(), 1_048_576);
        assert_eq!(
            classified.classification,
            ReportClassification::Negative,
            "an omitted-member report is not a digest mismatch"
        );
    }

    #[test]
    fn the_documented_invalid_report_names_every_broken_binding() {
        let classified = classify_report(DOC_INVALID_REPORT.as_bytes(), &doc_binding(), 1_048_576);
        match classified.classification {
            ReportClassification::Invalid { reasons } => {
                assert!(
                    reasons.iter().any(|r| r == "report.wrong_invocation"),
                    "{reasons:?}"
                );
                assert!(
                    reasons.iter().any(|r| r == "report.wrong_role"),
                    "{reasons:?}"
                );
                assert!(
                    !reasons.iter().any(|r| r == "report.wrong_request"),
                    "{reasons:?}"
                );
                assert!(
                    !reasons
                        .iter()
                        .any(|r| r.starts_with("report.digest_mismatch")),
                    "{reasons:?}"
                );
            }
            other => panic!("expected invalid, got {other:?}"),
        }
        assert!(classified.report.is_none());
    }

    #[test]
    fn an_oversize_report_is_invalid_before_parsing() {
        let raw = vec![b' '; 2_000_000];
        let classified = classify_report(&raw, &doc_binding(), 1_048_576);
        match classified.classification {
            ReportClassification::Invalid { reasons } => {
                assert!(
                    reasons.iter().any(|r| r == "report.oversize"),
                    "{reasons:?}"
                )
            }
            other => panic!("expected invalid, got {other:?}"),
        }
    }

    #[test]
    fn absent_usage_is_never_zero() {
        // A report omitting usage fields parses them as unknown,
        // never as measured zero.
        let report: RuntimeUsage = serde_json::from_str(r#"{"runtime_product":"x"}"#).unwrap();
        assert_eq!(report.turns, None);
        assert_eq!(report.elapsed_ms, None);
    }

    #[test]
    fn an_unknown_reason_code_is_invalid() {
        let body = DOC_REPORT.replace("checks_failed", "vendor_said_no");
        let classified = classify_report(body.as_bytes(), &doc_binding(), 1_048_576);
        match classified.classification {
            ReportClassification::Invalid { reasons } => {
                assert!(
                    reasons
                        .iter()
                        .any(|r| r.contains("report.digest_mismatch")
                            || r.contains("report.malformed")),
                    "{reasons:?}"
                )
            }
            other => panic!("expected invalid, got {other:?}"),
        }
    }

    /// The §5.1 declaration shape, offering the full v2 surface.
    fn declaration() -> WorkerDeclarationV1 {
        WorkerDeclarationV1 {
            format_version: WORKER_DECLARATION_V1_FORMAT.into(),
            adapter: "example-adapter".into(),
            adapter_version: "1.0.0".into(),
            runtime: DeclarationRuntime {
                product: "example-runtime".into(),
                version: "4.2.0".into(),
            },
            protocol_versions: vec![
                WORKER_REQUEST_V2_FORMAT.into(),
                WORKER_REPORT_V2_FORMAT.into(),
            ],
            roles: vec![
                "coding".into(),
                "fix".into(),
                "invariant_check".into(),
                "review".into(),
            ],
            capabilities: vec![
                "invocation_transport.argv-v2".into(),
                "report_json".into(),
                "workspace_write".into(),
                "process_control".into(),
            ],
            resume: "unsupported".into(),
            checkpoints: "workspace_commits".into(),
            runtime_artifact_store: serde_json::Value::Null,
            local_retries: "runtime_internal".into(),
        }
    }

    #[test]
    fn a_conforming_declaration_negotiates() {
        let request = WorkerRequestV2::parse(DOC_REQUEST.as_bytes()).unwrap();
        declaration().negotiate_v2(&request).unwrap();
    }

    #[test]
    fn a_declaration_without_the_report_protocol_is_refused() {
        let request = WorkerRequestV2::parse(DOC_REQUEST.as_bytes()).unwrap();
        let mut d = declaration();
        d.protocol_versions.retain(|v| v != WORKER_REPORT_V2_FORMAT);
        match d.negotiate_v2(&request) {
            Err(NegotiationRefusal::VersionUnsupported { detail }) => {
                assert!(detail.contains(WORKER_REPORT_V2_FORMAT), "{detail}")
            }
            other => panic!("expected version_unsupported, got {other:?}"),
        }
    }

    #[test]
    fn a_declaration_lacking_the_role_is_refused() {
        let request = WorkerRequestV2::parse(DOC_REQUEST.as_bytes()).unwrap();
        let mut d = declaration();
        d.roles.retain(|r| r != "coding");
        match d.negotiate_v2(&request) {
            Err(NegotiationRefusal::RoleMismatch { role }) => assert_eq!(role, "coding"),
            other => panic!("expected role_mismatch, got {other:?}"),
        }
    }

    #[test]
    fn a_declaration_lacking_a_capability_is_refused() {
        let request = WorkerRequestV2::parse(DOC_REQUEST.as_bytes()).unwrap();
        let mut d = declaration();
        d.capabilities.retain(|c| c != "workspace_write");
        match d.negotiate_v2(&request) {
            Err(NegotiationRefusal::CapabilityMissing { capability }) => {
                assert_eq!(capability, "workspace_write")
            }
            other => panic!("expected capability_missing, got {other:?}"),
        }
    }

    #[test]
    fn a_declaration_offering_resume_is_refused() {
        // §5.2: continuation is a reference set, never a runtime
        // session — a declaration offering `resume` cannot serve v2.
        let request = WorkerRequestV2::parse(DOC_REQUEST.as_bytes()).unwrap();
        let mut d = declaration();
        d.resume = "session".into();
        match d.negotiate_v2(&request) {
            Err(NegotiationRefusal::CapabilityMissing { capability }) => {
                assert!(capability.contains("resume"), "{capability}")
            }
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    #[test]
    fn a_malformed_declaration_is_refused() {
        match WorkerDeclarationV1::parse(b"not json") {
            Err(NegotiationRefusal::VersionUnsupported { detail }) => {
                assert!(detail.contains("not JSON"), "{detail}")
            }
            other => panic!("expected version_unsupported, got {other:?}"),
        }
    }

    #[test]
    fn a_declaration_of_an_unknown_version_is_refused() {
        let mut d = declaration();
        d.format_version = "taisan.worker.declaration/9".into();
        let raw = serde_json::to_vec(&d).unwrap();
        match WorkerDeclarationV1::parse(&raw) {
            Err(NegotiationRefusal::VersionUnsupported { detail }) => {
                assert!(detail.contains("declaration/9"), "{detail}")
            }
            other => panic!("expected version_unsupported, got {other:?}"),
        }
    }

    #[test]
    fn a_declaration_with_an_unknown_member_is_refused() {
        let mut value = serde_json::to_value(declaration()).unwrap();
        value["vendor_extension"] = serde_json::Value::Bool(true);
        let raw = serde_json::to_vec(&value).unwrap();
        match WorkerDeclarationV1::parse(&raw) {
            Err(NegotiationRefusal::VersionUnsupported { detail }) => {
                assert!(detail.contains("unknown field"), "{detail}")
            }
            other => panic!("expected version_unsupported, got {other:?}"),
        }
    }

    #[test]
    fn a_wrong_version_report_is_invalid() {
        let body = DOC_REPORT.replace("taisan.worker.report/2", "taisan.worker.report/9");
        let classified = classify_report(body.as_bytes(), &doc_binding(), 1_048_576);
        match classified.classification {
            ReportClassification::Invalid { reasons } => {
                assert!(
                    reasons.iter().any(|r| r == "report.wrong_version"),
                    "{reasons:?}"
                )
            }
            other => panic!("expected invalid, got {other:?}"),
        }
    }

    #[test]
    fn a_malformed_report_is_invalid() {
        let classified = classify_report(b"not json", &doc_binding(), 1_048_576);
        match classified.classification {
            ReportClassification::Invalid { reasons } => {
                assert!(
                    reasons.iter().any(|r| r.starts_with("report.malformed")),
                    "{reasons:?}"
                )
            }
            other => panic!("expected invalid, got {other:?}"),
        }
    }
}
