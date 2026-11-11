//! U9.4 — the narrow live GitLab spike harness.
//!
//! "Narrow" means exactly four operations — guarded push, merge-request
//! create, comment post, head read — and nothing else. The harness is
//! the codeable deliverable of the spike: isolated-scope configuration,
//! drivers for the four operations, and receipt/evidence capture into a
//! durable journal. Live rows additionally require a recorded
//! [`LiveAuthorization`]; without one the harness reports
//! `not_authorized` and the remote is never touched.
//!
//! `GitlabSpikeRemote` is a GitLab-shaped fixture remote — per-project
//! IID numbering, a change-request head derived from its source branch —
//! with the denial and lost-response injection the named fixture runs
//! drive. It is not a GitLab adapter: fixture results support contract
//! claims only, and any live claim still requires separately authorized
//! live evidence under the implementation start gate.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use workloop_core::identity::{
    ChangeRequestId, ChangeRequestRef, CommitRevision, HostActorRef, HostInstanceId, RepositoryId,
    RepositoryRef,
};

/// Host write operations in the contract vocabulary that the narrow
/// spike does not drive. Every journal records this list so unsupported
/// stays explicit — these operations are reachable nowhere in the
/// harness.
pub const UNSUPPORTED_OPERATIONS: &[&str] = &[
    "update_change_request",
    "post_inline_comment",
    "publish_review_feedback",
    "approve_change",
    "request_changes",
    "resolve_thread",
    "mark_ready",
];

/// The isolated scope a spike run binds: one host instance, one
/// repository, one actor, one branch namespace prefix. Every write the
/// harness emits stays under the prefix — the spike never writes a ref
/// outside its isolated scope.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpikeScope {
    pub host: HostInstanceId,
    pub repository: RepositoryId,
    /// The branch namespace the run may write, e.g. `spike/u94/`.
    pub branch_prefix: String,
    pub actor: HostActorRef,
}

impl SpikeScope {
    /// The repository the scope binds.
    pub fn repository(&self) -> RepositoryRef {
        RepositoryRef::new(self.host.clone(), self.repository.clone())
    }

    /// Deterministic identity string a [`LiveAuthorization`] binds to.
    /// Not a hash — the scope is small and a readable binding is better
    /// evidence than an opaque digest. Every field is escaped so a
    /// separator inside an opaque id cannot merge two scopes, and the
    /// actor's host instance is part of the binding: `(host, actor)` is
    /// the contract's actor identity.
    pub fn binding(&self) -> String {
        format!(
            "{}/{}/{}/{}/{}",
            escape_binding_field(&self.host.to_string()),
            escape_binding_field(&self.repository.to_string()),
            escape_binding_field(&self.branch_prefix),
            escape_binding_field(&self.actor.host.to_string()),
            escape_binding_field(&self.actor.actor.to_string()),
        )
    }

    /// The isolation rule: `branch` must live under the scope prefix.
    fn check_branch(&self, branch: &str) -> Result<(), String> {
        if branch.starts_with(&self.branch_prefix) {
            Ok(())
        } else {
            Err(format!(
                "scope: branch `{branch}` is outside spike namespace `{}`",
                self.branch_prefix
            ))
        }
    }
}

/// Escape one binding field so joining with `/` stays injective:
/// backslash and slash are both escaped, so `a/b` and `a` + `/b` cannot
/// serialize to the same component sequence.
fn escape_binding_field(field: &str) -> String {
    let mut escaped = String::with_capacity(field.len());
    for ch in field.chars() {
        match ch {
            '\\' => escaped.push_str("\\\\"),
            '/' => escaped.push_str("\\/"),
            _ => escaped.push(ch),
        }
    }
    escaped
}

/// A recorded live authorization — the gate on every `RowLane::Live`
/// row. `binding` names the scope the approval covers; an authorization
/// recorded for a different scope does not open the gate here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LiveAuthorization {
    /// Owner-verifiable locator for the recorded approval (an issue or
    /// comment reference under the implementation start gate).
    pub record: String,
    /// `SpikeScope::binding()` of the scope the approval covers.
    pub binding: String,
}

impl LiveAuthorization {
    /// Whether this record authorizes live rows under `scope`.
    pub fn covers(&self, scope: &SpikeScope) -> bool {
        self.binding == scope.binding()
    }
}

/// Which lane a spike row executes in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RowLane {
    /// Runs against the fixture remote — contract evidence only.
    Fixture,
    /// Runs only with a recorded live authorization covering the
    /// harness scope; otherwise the row reports `not_authorized`.
    Live,
}

/// The four operations the narrow spike may drive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SpikeOpKind {
    GuardedPush,
    MrCreate,
    CommentPost,
    HeadRead,
}

impl SpikeOpKind {
    /// The complete reachable set — the static-check anchor: any host
    /// operation outside these four is unreachable from the harness.
    pub const ALL: [Self; 4] = [
        Self::GuardedPush,
        Self::MrCreate,
        Self::CommentPost,
        Self::HeadRead,
    ];

    pub fn name(&self) -> &'static str {
        match self {
            Self::GuardedPush => "guarded_push",
            Self::MrCreate => "mr_create",
            Self::CommentPost => "comment_post",
            Self::HeadRead => "head_read",
        }
    }
}

/// What a head read targets. Both forms are head reads — no other read
/// type is reachable through the spike.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "target")]
pub enum HeadTarget {
    /// The current head of `branch` — reassesses a push receipt and is
    /// the reconcile read for an unresolved push.
    Branch { branch: String },
    /// The identity and current head of the change request named by its
    /// host-scoped IID — reassesses a create/comment receipt and is the
    /// reconcile read for an unresolved create or comment.
    Change { change_request: ChangeRequestId },
}

/// One operation a spike row carries — the closed set of four.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "op")]
pub enum SpikeOp {
    /// Push `produced` to `branch`, requiring the remote head to be
    /// `expected_prior` (`None` = the branch must be absent). The guard
    /// holds at the write boundary — a pre-read is not concurrency
    /// control.
    GuardedPush {
        branch: String,
        expected_prior: Option<String>,
        produced: String,
    },
    /// Open a change request `source_branch` → `target_branch`.
    MrCreate {
        source_branch: String,
        target_branch: String,
    },
    /// Post one ordinary note on the change request named by
    /// `change_request`, bound to `expected_head`. `source_branch` is
    /// the IID's expected source branch, checked by the remote so a
    /// stale plan cannot comment on the wrong change request.
    CommentPost {
        change_request: ChangeRequestId,
        source_branch: String,
        expected_head: String,
        body: String,
    },
    /// A fresh head read.
    HeadRead { target: HeadTarget },
}

impl SpikeOp {
    pub fn kind(&self) -> SpikeOpKind {
        match self {
            Self::GuardedPush { .. } => SpikeOpKind::GuardedPush,
            Self::MrCreate { .. } => SpikeOpKind::MrCreate,
            Self::CommentPost { .. } => SpikeOpKind::CommentPost,
            Self::HeadRead { .. } => SpikeOpKind::HeadRead,
        }
    }

    /// The branch a write operates through, for the scope check. A
    /// change request opened from a spike branch stays spike-scoped, so
    /// the comment write binds the same branch.
    fn write_branch(&self) -> Option<&str> {
        match self {
            Self::GuardedPush { branch, .. }
            | Self::MrCreate {
                source_branch: branch,
                ..
            }
            | Self::CommentPost {
                source_branch: branch,
                ..
            } => Some(branch),
            Self::HeadRead { .. } => None,
        }
    }
}

/// A single row in a spike plan.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SpikeRow {
    pub id: String,
    pub lane: RowLane,
    pub op: SpikeOp,
}

/// The host's report for one attempted write.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "status")]
pub enum WriteReceipt {
    /// The effect landed; `revision` is the revision the remote binds
    /// to it — the pushed head, the opened change-request head, or the
    /// head a comment bound to.
    Applied {
        revision: CommitRevision,
        fact: Box<EffectFact>,
    },
    /// Rejected at the write boundary (guard mismatch, absent source).
    Failed { reason: String },
    /// The operation is supported but this actor's permission was
    /// denied — distinct from unsupported and from failure.
    Denied { reason: String },
    /// The response was lost; the effect may or may not have landed.
    /// Never retried blindly — reconcile by fresh read.
    Unknown { reason: String },
}

/// The normalized fact an applied receipt carries.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "fact")]
pub enum EffectFact {
    RefPublished {
        branch: String,
        produced: CommitRevision,
    },
    ChangeOpened {
        change: ChangeRequestRef,
        head: CommitRevision,
    },
    CommentPosted {
        change: ChangeRequestRef,
        note_id: String,
    },
}

/// A completed fresh head read.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HeadObservation {
    /// The change request the read resolved, when the target named one.
    pub change: Option<ChangeRequestRef>,
    /// The observed head; `None` = the target does not exist remotely.
    pub head: Option<CommitRevision>,
}

/// What a row produced.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "outcome")]
pub enum RowOutcome {
    /// A write receipt.
    Receipt(WriteReceipt),
    /// A completed head read.
    Observed(HeadObservation),
    /// A live row refused: no recorded live authorization covers the
    /// harness scope — the remote was never touched.
    NotAuthorized,
}

/// Revision-bound reassessment: the fresh head read taken immediately
/// after an applied write, compared against the receipt's revision.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Reassessment {
    /// The revision the applied receipt reported.
    pub receipt: CommitRevision,
    /// The revision the fresh head read observed.
    pub observed: Option<CommitRevision>,
    /// `observed == Some(receipt)` — recorded at run time, not derived
    /// by the reader of the evidence.
    pub bound: bool,
}

/// What a reconcile read establishes about an `unknown` receipt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReconciledEffect {
    /// The remote shows the effect landed.
    Present,
    /// The remote shows it did not.
    Absent,
    /// A head read cannot settle it — e.g. a lost comment response,
    /// whose establishment needs a comment read the narrow spike does
    /// not drive.
    Indeterminate,
}

/// Facts a fresh read established about an `unknown` receipt. The
/// receipt itself stays `unknown` — reconciliation establishes facts
/// about the effect; it does not upgrade the receipt or write again.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Reconciliation {
    /// The change request the reconcile read resolved, if any.
    pub change: Option<ChangeRequestRef>,
    /// The head observed at reconcile time.
    pub observed: Option<CommitRevision>,
    pub effect: ReconciledEffect,
}

/// One row's retained evidence: its outcome, the reassessment bound to
/// an applied write, and any reconciliation of an unknown receipt.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RowRecord {
    pub row_id: String,
    pub lane: RowLane,
    pub op: SpikeOp,
    pub outcome: RowOutcome,
    pub reassessment: Option<Reassessment>,
    pub reconciliation: Option<Reconciliation>,
}

/// A lifecycle event on the journal itself.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "event")]
pub enum JournalEvent {
    /// The run resumed from a retained journal after restart. The
    /// recorded binding wins over the installation default: a default
    /// that moved under the run is noted, never retargeted.
    Resumed {
        recorded_binding: String,
        default_binding: String,
        binding_diverged: bool,
    },
}

/// The retained evidence record of one spike run: the scope binding,
/// the explicit unsupported list, every row's outcome/reassessment/
/// reconciliation, and lifecycle events. Serializes so a run survives
/// restart — a resumed run reconciles under the recorded binding.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SpikeJournal {
    /// The scope recorded when the run started.
    pub scope: SpikeScope,
    /// Contract write operations outside the narrow set — explicit,
    /// never silently absent.
    pub unsupported_operations: Vec<String>,
    pub records: Vec<RowRecord>,
    pub events: Vec<JournalEvent>,
}

/// The port the spike drives — exactly the four narrow operations; no
/// other host operation is reachable through it.
pub trait SpikeRemote {
    /// Push `produced` to `branch` iff its remote head is
    /// `expected_prior` (`None`: the branch must be absent).
    fn guarded_push(
        &mut self,
        repository: &RepositoryRef,
        branch: &str,
        expected_prior: Option<&str>,
        produced: &str,
    ) -> WriteReceipt;
    /// Open a change request `source_branch` → `target_branch`.
    fn mr_create(
        &mut self,
        repository: &RepositoryRef,
        source_branch: &str,
        target_branch: &str,
    ) -> WriteReceipt;
    /// Post one ordinary note on change request `change_request`, bound
    /// to `expected_head`; `source_branch` is its expected source.
    fn comment_post(
        &mut self,
        repository: &RepositoryRef,
        change_request: &ChangeRequestId,
        source_branch: &str,
        expected_head: &str,
        body: &str,
    ) -> WriteReceipt;
    /// A fresh head read — never a write.
    fn head_read(&self, repository: &RepositoryRef, target: &HeadTarget) -> HeadObservation;
}

/// The head-read target reassessing an applied write of `op`. A create's
/// identity comes from its applied receipt; a comment already names the
/// IID it wrote to.
fn reassess_target(op: &SpikeOp, fact: &EffectFact) -> HeadTarget {
    match op {
        SpikeOp::GuardedPush { branch, .. } => HeadTarget::Branch {
            branch: branch.clone(),
        },
        SpikeOp::MrCreate { .. } => match fact {
            EffectFact::ChangeOpened { change, .. } => HeadTarget::Change {
                change_request: change.change_request.clone(),
            },
            _ => unreachable!("an applied create receipt carries its change identity"),
        },
        SpikeOp::CommentPost { change_request, .. } => HeadTarget::Change {
            change_request: change_request.clone(),
        },
        SpikeOp::HeadRead { .. } => unreachable!("a read has no write receipt to reassess"),
    }
}

/// The spike driver. Executes rows against a [`SpikeRemote`], binds each
/// applied write to a fresh head read, gates live rows on the recorded
/// authorization, and keeps the durable journal — the retained evidence
/// record — resumable across restart.
pub struct SpikeHarness<'r, R: SpikeRemote> {
    scope: SpikeScope,
    authorization: Option<LiveAuthorization>,
    remote: &'r mut R,
    journal: SpikeJournal,
}

impl<'r, R: SpikeRemote> SpikeHarness<'r, R> {
    /// A fresh run under `scope`.
    pub fn new(
        scope: SpikeScope,
        remote: &'r mut R,
        authorization: Option<LiveAuthorization>,
    ) -> Self {
        Self {
            scope: scope.clone(),
            authorization,
            remote,
            journal: SpikeJournal {
                scope,
                unsupported_operations: UNSUPPORTED_OPERATIONS
                    .iter()
                    .map(|s| s.to_string())
                    .collect(),
                records: vec![],
                events: vec![],
            },
        }
    }

    /// Resume a run from its retained journal after restart.
    /// `default_scope` is the installation's *current* default, which
    /// may have moved under the run — the run continues and reconciles
    /// under the journal's recorded binding, never the moved default.
    pub fn resume(
        remote: &'r mut R,
        authorization: Option<LiveAuthorization>,
        mut journal: SpikeJournal,
        default_scope: &SpikeScope,
    ) -> Self {
        journal.events.push(JournalEvent::Resumed {
            recorded_binding: journal.scope.binding(),
            default_binding: default_scope.binding(),
            binding_diverged: default_scope.binding() != journal.scope.binding(),
        });
        Self {
            scope: journal.scope.clone(),
            authorization,
            remote,
            journal,
        }
    }

    pub fn journal(&self) -> &SpikeJournal {
        &self.journal
    }

    /// Consume the harness, returning the retained evidence record.
    pub fn into_journal(self) -> SpikeJournal {
        self.journal
    }

    /// The remote, for arming faults between run segments in fixture
    /// runs.
    pub fn remote_mut(&mut self) -> &mut R {
        self.remote
    }

    /// Execute the plan rows that have no recorded outcome yet. Rows
    /// already recorded — including `unknown` ones — are not re-driven:
    /// an unknown effect is never retried blindly; [`Self::reconcile`]
    /// settles it by fresh read.
    pub fn run(&mut self, plan: &[SpikeRow]) {
        for row in plan {
            if self.journal.records.iter().any(|r| r.row_id == row.id) {
                continue;
            }
            let outcome = self.execute(row);
            let mut record = RowRecord {
                row_id: row.id.clone(),
                lane: row.lane,
                op: row.op.clone(),
                outcome,
                reassessment: None,
                reconciliation: None,
            };
            // Revision-bound reassessment: an applied write is followed
            // by a fresh head read whose observed revision must equal
            // the receipt's.
            if let RowOutcome::Receipt(WriteReceipt::Applied { revision, fact }) = &record.outcome {
                let observed = self
                    .remote
                    .head_read(&self.scope.repository(), &reassess_target(&row.op, fact))
                    .head;
                record.reassessment = Some(Reassessment {
                    receipt: revision.clone(),
                    bound: observed.as_ref() == Some(revision),
                    observed,
                });
            }
            self.journal.records.push(record);
        }
    }

    /// Fresh-read reconciliation for every `unknown` receipt, under the
    /// journal's recorded binding. Reconciliation establishes facts
    /// about the effect; it does not re-write and does not upgrade the
    /// receipt.
    pub fn reconcile(&mut self) {
        for i in 0..self.journal.records.len() {
            let needs = matches!(
                self.journal.records[i].outcome,
                RowOutcome::Receipt(WriteReceipt::Unknown { .. })
            ) && self.journal.records[i].reconciliation.is_none();
            if !needs {
                continue;
            }
            let op = self.journal.records[i].op.clone();
            let repository = self.scope.repository();
            let (observed, effect) = match &op {
                SpikeOp::GuardedPush { branch, .. } => {
                    let observed = self.remote.head_read(
                        &repository,
                        &HeadTarget::Branch {
                            branch: branch.clone(),
                        },
                    );
                    let effect = reconcile_effect(&op, &repository, &observed);
                    (observed, effect)
                }
                SpikeOp::CommentPost { change_request, .. } => {
                    let observed = self.remote.head_read(
                        &repository,
                        &HeadTarget::Change {
                            change_request: change_request.clone(),
                        },
                    );
                    let effect = reconcile_effect(&op, &repository, &observed);
                    (observed, effect)
                }
                // A lost create response leaves no IID to read. A head
                // read can show the source branch, but not which change
                // request (if any) this create made — the effect stays
                // indeterminate rather than borrowing another MR's
                // identity because it shares a source branch.
                SpikeOp::MrCreate { source_branch, .. } => {
                    let observed = self.remote.head_read(
                        &repository,
                        &HeadTarget::Branch {
                            branch: source_branch.clone(),
                        },
                    );
                    (observed, ReconciledEffect::Indeterminate)
                }
                SpikeOp::HeadRead { .. } => unreachable!("a read receipt is never unknown"),
            };
            self.journal.records[i].reconciliation = Some(Reconciliation {
                change: observed.change,
                observed: observed.head,
                effect,
            });
        }
    }

    fn execute(&mut self, row: &SpikeRow) -> RowOutcome {
        // The live gate: a recorded, scope-matching authorization or the
        // row does not execute — the remote is never touched.
        if row.lane == RowLane::Live {
            let allowed = self
                .authorization
                .as_ref()
                .is_some_and(|a| a.covers(&self.scope));
            if !allowed {
                return RowOutcome::NotAuthorized;
            }
        }
        // Isolation: writes stay under the scope's branch namespace.
        if let Some(branch) = row.op.write_branch()
            && let Err(reason) = self.scope.check_branch(branch)
        {
            return RowOutcome::Receipt(WriteReceipt::Failed { reason });
        }
        let repository = self.scope.repository();
        match &row.op {
            SpikeOp::GuardedPush {
                branch,
                expected_prior,
                produced,
            } => RowOutcome::Receipt(self.remote.guarded_push(
                &repository,
                branch,
                expected_prior.as_deref(),
                produced,
            )),
            SpikeOp::MrCreate {
                source_branch,
                target_branch,
            } => RowOutcome::Receipt(self.remote.mr_create(
                &repository,
                source_branch,
                target_branch,
            )),
            SpikeOp::CommentPost {
                change_request,
                source_branch,
                expected_head,
                body,
            } => RowOutcome::Receipt(self.remote.comment_post(
                &repository,
                change_request,
                source_branch,
                expected_head,
                body,
            )),
            SpikeOp::HeadRead { target } => {
                RowOutcome::Observed(self.remote.head_read(&repository, target))
            }
        }
    }
}

/// What the reconcile read establishes about the lost effect of `op`.
fn reconcile_effect(
    op: &SpikeOp,
    repository: &RepositoryRef,
    observed: &HeadObservation,
) -> ReconciledEffect {
    match op {
        SpikeOp::GuardedPush {
            expected_prior,
            produced,
            ..
        } => {
            let produced = CommitRevision::new(repository.clone(), produced).ok();
            let prior = expected_prior
                .as_ref()
                .and_then(|p| CommitRevision::new(repository.clone(), p).ok());
            if observed.head.is_some() && observed.head == produced {
                ReconciledEffect::Present
            } else if observed.head == prior {
                ReconciledEffect::Absent
            } else {
                ReconciledEffect::Indeterminate
            }
        }
        // A lost create response leaves no IID to compare, so a head read
        // cannot establish which change request (if any) the create
        // produced. Never borrow another MR's identity from the branch.
        SpikeOp::MrCreate { .. } => ReconciledEffect::Indeterminate,
        // A head read cannot see a note: if the change request itself is
        // gone the effect is absent, otherwise indeterminate — a comment
        // read the narrow spike does not drive would be needed.
        SpikeOp::CommentPost { .. } => {
            if observed.change.is_some() {
                ReconciledEffect::Indeterminate
            } else {
                ReconciledEffect::Absent
            }
        }
        SpikeOp::HeadRead { .. } => unreachable!("a read receipt is never unknown"),
    }
}

/// One project on the fixture remote: an independent numbering, branch,
/// and merge-request namespace.
#[derive(Debug, Default)]
struct Project {
    /// Per-project merge-request counter — the IID namespace.
    next_iid: u64,
    /// Per-project note counter.
    next_note: u64,
    /// `branch` → head object id.
    branches: BTreeMap<String, String>,
    /// `change_request` IID string → merge request. No stored head: the
    /// head always derives from the source branch.
    merge_requests: BTreeMap<String, MergeRequest>,
}

/// A merge request tracks its source branch; the reported head follows
/// the branch rather than a snapshot taken at open time.
#[derive(Debug)]
struct MergeRequest {
    iid: u64,
    source_branch: String,
    /// Ordinary notes posted to this merge request.
    notes: Vec<String>,
}

/// A GitLab-shaped fixture remote for the spike: per-project IID
/// numbering, a change-request head derived from its source branch —
/// plus the denial and lost-response injection the named runs drive.
/// The guarded push enforces the absent/exact prior-ref condition at
/// the write boundary; ancestry (fast-forward) checking is a live-check
/// item the fixture cannot model on opaque object ids.
#[derive(Debug, Default)]
pub struct GitlabSpikeRemote {
    /// `repository` (host-scoped identity string) → project state.
    projects: BTreeMap<String, Project>,
    /// Operations whose writes the actor is denied — permission denial,
    /// not absence of support: reads and other operations are
    /// unaffected.
    denied: BTreeSet<SpikeOpKind>,
    /// Drop the next write response: the effect lands, the receipt is
    /// `unknown` — a lost response, not a failure.
    lose_response: bool,
    /// The write history, for assertions that an effect landed.
    applied: Vec<String>,
}

impl GitlabSpikeRemote {
    pub fn new() -> Self {
        Self::default()
    }

    /// Deny the actor's permission for `op`'s writes — injected denial.
    pub fn deny(&mut self, op: SpikeOpKind) {
        self.denied.insert(op);
    }

    /// The next write lands remotely but its response is lost: the
    /// receipt reports `unknown`.
    pub fn lose_next_response(&mut self) {
        self.lose_response = true;
    }

    /// The write history, for test assertions.
    pub fn applied(&self) -> &[String] {
        &self.applied
    }

    /// The head the remote records for `(repository, branch)`.
    pub fn branch_head(&self, repository: &RepositoryRef, branch: &str) -> Option<String> {
        self.projects
            .get(&repository.to_string())
            .and_then(|p| p.branches.get(branch).cloned())
    }

    /// A receipt the remote reports after possibly dropping it.
    fn report(&mut self, revision: CommitRevision, fact: EffectFact) -> WriteReceipt {
        if self.lose_response {
            self.lose_response = false;
            return WriteReceipt::Unknown {
                reason: "response lost".into(),
            };
        }
        WriteReceipt::Applied {
            revision,
            fact: Box::new(fact),
        }
    }
}

impl SpikeRemote for GitlabSpikeRemote {
    fn guarded_push(
        &mut self,
        repository: &RepositoryRef,
        branch: &str,
        expected_prior: Option<&str>,
        produced: &str,
    ) -> WriteReceipt {
        if self.denied.contains(&SpikeOpKind::GuardedPush) {
            return WriteReceipt::Denied {
                reason: "permission denied".into(),
            };
        }
        let project = self.projects.entry(repository.to_string()).or_default();
        let current = project.branches.get(branch);
        let guard_ok = match (expected_prior, current) {
            (Some(expected), Some(current)) => expected == current,
            (None, None) => true,
            _ => false,
        };
        if !guard_ok {
            return WriteReceipt::Failed {
                reason: format!(
                    "guard rejected: expected prior {expected_prior:?}, remote has {current:?}"
                ),
            };
        }
        project
            .branches
            .insert(branch.to_string(), produced.to_string());
        self.applied
            .push(format!("guarded_push:{repository}:{branch}:{produced}"));
        let revision = CommitRevision::new(repository.clone(), produced)
            .expect("spike object ids are non-empty");
        self.report(
            revision.clone(),
            EffectFact::RefPublished {
                branch: branch.to_string(),
                produced: revision,
            },
        )
    }

    fn mr_create(
        &mut self,
        repository: &RepositoryRef,
        source_branch: &str,
        target_branch: &str,
    ) -> WriteReceipt {
        if self.denied.contains(&SpikeOpKind::MrCreate) {
            return WriteReceipt::Denied {
                reason: "permission denied".into(),
            };
        }
        let project = self.projects.entry(repository.to_string()).or_default();
        // A merge request opens only against a branch that exists on
        // the remote — an absent branch is a failed receipt, never an
        // invented head.
        let Some(head_object) = project.branches.get(source_branch).cloned() else {
            return WriteReceipt::Failed {
                reason: "source branch not found".into(),
            };
        };
        project.next_iid += 1;
        let iid = project.next_iid;
        let change = ChangeRequestRef {
            repository: repository.clone(),
            change_request: ChangeRequestId::trusted(format!("!{iid}")),
            display_number: Some(iid),
        };
        project.merge_requests.insert(
            change.change_request.as_str().to_string(),
            MergeRequest {
                iid,
                source_branch: source_branch.to_string(),
                notes: vec![],
            },
        );
        self.applied
            .push(format!("mr_create:{repository}:{change}:{target_branch}"));
        let head = CommitRevision::new(repository.clone(), head_object)
            .expect("spike object ids are non-empty");
        self.report(head.clone(), EffectFact::ChangeOpened { change, head })
    }

    fn comment_post(
        &mut self,
        repository: &RepositoryRef,
        change_request: &ChangeRequestId,
        source_branch: &str,
        expected_head: &str,
        body: &str,
    ) -> WriteReceipt {
        if self.denied.contains(&SpikeOpKind::CommentPost) {
            return WriteReceipt::Denied {
                reason: "permission denied".into(),
            };
        }
        let project = self.projects.entry(repository.to_string()).or_default();
        let Some(mr) = project.merge_requests.get_mut(change_request.as_str()) else {
            return WriteReceipt::Failed {
                reason: format!("no change request {change_request}"),
            };
        };
        if mr.source_branch != source_branch {
            return WriteReceipt::Failed {
                reason: format!(
                    "change request {change_request} is from {}, not {source_branch}",
                    mr.source_branch
                ),
            };
        }
        let head_object = project.branches.get(&mr.source_branch).cloned();
        if head_object.as_deref() != Some(expected_head) {
            return WriteReceipt::Failed {
                reason: "expected head moved".into(),
            };
        }
        project.next_note += 1;
        let note_id = format!("note-{}-{}", mr.iid, project.next_note);
        mr.notes.push(body.to_string());
        let change = ChangeRequestRef {
            repository: repository.clone(),
            change_request: change_request.clone(),
            display_number: Some(mr.iid),
        };
        self.applied
            .push(format!("comment_post:{repository}:{change}:{note_id}"));
        let revision = CommitRevision::new(repository.clone(), expected_head)
            .expect("spike object ids are non-empty");
        self.report(revision, EffectFact::CommentPosted { change, note_id })
    }

    fn head_read(&self, repository: &RepositoryRef, target: &HeadTarget) -> HeadObservation {
        let head = |object: Option<&String>| {
            object.and_then(|o| CommitRevision::new(repository.clone(), o).ok())
        };
        let project = self.projects.get(&repository.to_string());
        match target {
            HeadTarget::Branch { branch } => HeadObservation {
                change: None,
                head: head(project.and_then(|p| p.branches.get(branch))),
            },
            HeadTarget::Change { change_request } => {
                let mr = project.and_then(|p| p.merge_requests.get(change_request.as_str()));
                let head_object =
                    mr.and_then(|m| project.and_then(|p| p.branches.get(m.source_branch.as_str())));
                HeadObservation {
                    change: mr.map(|m| ChangeRequestRef {
                        repository: repository.clone(),
                        change_request: change_request.clone(),
                        display_number: Some(m.iid),
                    }),
                    head: head(head_object),
                }
            }
        }
    }
}
