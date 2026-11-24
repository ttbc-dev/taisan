# Module contracts

This document specifies the messages crossing every edge of the [module map](architecture.md#module-map), then checks them against four interactions. It is a design proposal, not an implemented API or a choice of programming language. ADR-002 adds a host-neutral target boundary; the existing Rust APIs remain GitHub-specific and unmodified by this document. The type notation is illustrative; the lifecycle states and guards follow [Work lifecycle](lifecycle.md). The terms and lifetimes come from the [glossary](glossary.md).

## Evidence and lane-policy refinement

[Evidence and review gates](evidence-gates.md) refines the target messages below. Mechanical Work verification remains separate from optional invariant conformance (default on), optional code review, and additional explicitly configured CI. Code review does not require native Approve or create merge/draft gates. Disabled lanes are explicit, not passing evidence. Judgment/release use exact immutable round/set identity as well as Work version and exact diff target, including same-head finding growth. These target refinements are not implemented by the current Rust fixture.

## Caller view

The required CLI and planned CLI-backed agent skills use the same Command and Query contracts. The existing TUI is deferred from v0.1; GitHub comments and MCP are optional future control adapters. Skills call the CLI rather than the state store or supervisor. The caller lists the built-in queue with `work_list`, then queries `work_detail(W1)`, may ask `work` for a read-only preflight of a judgment, and submits that Command with the displayed Work version and PR head. If the submit response disappears, it asks for the result by Command ID or replays the same key and payload. An accepted result means the decision is durable; it does not mean a worker or GitHub write has finished.

## Type sketch

The canonical target port is [RepositoryHostPort](repository-host-contract.md). Core uses scoped repository/change identities and immutable revisions without inspecting provider names or payloads. GitHub PR terminology in the example flows is a reference deployment, not a required host model. These target types replace the current GitHub-specific names when implemented, with callers, store keys, and tests migrated together. There are no parallel generic/GitHub Core paths or compatibility aliases.

The [Worker protocol](worker-protocol.md) refines `WorkerTask`, `WorkerReport`, `PreparedChange` and `AllowedEffects` below: concrete request/report/capability schemas, supervisor-owned export into a handoff repository ref, supervisor-run mechanical verification, and enforced sandbox isolation instead of flags. Target design only.

The [concrete identity/storage design](identity-storage.md) defines Rust/SQLite scope, command payload identity, source-change reapproval and the owner-approved archive/new-store cutover for legacy slice-0 data. This is target design only.

IDs are distinct opaque types even if an implementation stores each as a string. `WorkVersion` is a version of one Work item. `EventSequence` identifies a committed position in one installation's history. `CommitRevision` identifies the revision used for a review or check.

```ts
type OpaqueId<T extends string> = string & { readonly tag: T };
type OpaqueNumber<T extends string> = number & { readonly tag: T };
type WorkId = OpaqueId<"work">;
type ExecutionId = OpaqueId<"execution">;
type InvocationId = OpaqueId<"invocation">;
type CommandId = OpaqueId<"command">;
type ObservationId = OpaqueId<"observation">;
type ActionId = OpaqueId<"external_action">;
type WorkVersion = OpaqueNumber<"work_version">;
type EventSequence = OpaqueNumber<"event_sequence">;
type HostInstanceId = OpaqueId<"host_instance">;
type RepositoryRef = { host: HostInstanceId; id: string };
type CommitRevision = { repository: RepositoryRef; object_id: string };
type EvidenceRef = { id: string; digest: string; subject_revision?: CommitRevision };
type IssueRef = { repository: RepositoryRef; id: string };
type ChangeRequestRef = { repository: RepositoryRef; id: string };
type HostCapability = "repository_read" | "branch_read" | "push_ref"
	| "change_request_read" | "change_request_create" | "change_request_update"
	| "comment_read" | "comment_write" | "issue_read" | "checks_read"
	| "review_findings_read" | "inline_comment" | "review_feedback"
	| "approve" | "request_changes" | "thread_reply" | "thread_resolve" | "ready";
type HostActorRef = { host: HostInstanceId; id: string };
type HostBinding = { adapter_id: string; instance: HostInstanceId;
	repository: RepositoryRef; contract_version: 1;
	required_capabilities: HostCapability[];
	actors: { role: "publication" | "review" | "discussion";
	    actor: HostActorRef; credential_ref: string }[] };
type SubjectRef =
	| { kind: "work"; work_id: WorkId; execution_id?: ExecutionId;
	    invocation_id?: InvocationId; change_request?: ChangeRequestRef }
	| { kind: "candidate"; issue: IssueRef };
type DiffSnapshotRef = { change_request: ChangeRequestRef;
	head: CommitRevision; comparison_base: CommitRevision;
	target_revision: CommitRevision; evidence: EvidenceRef };
type DiffLocation = { snapshot: DiffSnapshotRef; path: string;
	line: number; side: "before" | "after" };
type FindingRef = { finding_id: string; reviewed_revision: CommitRevision;
	summary: string; location?: { path: string; line?: number; snapshot?: DiffSnapshotRef };
	evidence: EvidenceRef[] };
type CheckResult = "pending" | "passed" | "failed" | "unknown";
type RejectionCode = "invalid_request" | "unauthorized" | "stale_work_version"
	| "stale_head" | "ineligible" | "ownership_uncertain"
	| "capacity_exhausted" | "source_unavailable" | "idempotency_conflict"
	| "resource_conflict" | "unsupported_capability";

type Actor = { kind: "human" | "automation"; external_id: string };
type CommandMeta = { actor: Actor; origin: string; idempotency_key: string };
type ExistingWork = { work_id: WorkId; expected_work_version: WorkVersion };
type FindingDecision = { finding_id: string; choice: "fix" | "dismiss" | "accept" | "wait" };
type WriteResource =
	| { kind: "workspace"; workspace_id: string }
	| { kind: "branch"; repository: RepositoryRef; branch: string };
type ResourceReservation = { resources: WriteResource[]; holder:
	| { kind: "execution"; execution_id: ExecutionId }
	| { kind: "external"; actor_id: string };
	expires_at?: string };
type CommandEffect =
	| { kind: "work_created"; execution_id: ExecutionId }
	| { kind: "execution_created"; execution_id: ExecutionId }
	| { kind: "resources_claimed"; expires_at?: string }
	| { kind: "resources_released" }
	| { kind: "state_changed" };

type CommandRequest = CommandMeta & (
	| { kind: "submit"; intent: string; issue_refs: IssueRef[];
	    constraints: string[]; verification_argv: string[][] }
	| { kind: "claim_resources"; resources: WriteResource[]; ttl_seconds: number }
	| { kind: "release_resources"; resources: WriteResource[] }
	| (ExistingWork & (
		| { kind: "retry"; expected_execution_id: ExecutionId }
		| { kind: "resume"; execution_id: ExecutionId }
		| { kind: "cancel"; execution_id?: ExecutionId }
		| { kind: "acknowledge"; attention_id: string }
		| { kind: "judge_findings"; expected_head: CommitRevision; decisions: FindingDecision[] }
		| { kind: "release_judgment"; expected_head: CommitRevision }
	))
);

type CommandDecision =
	| { kind: "accepted"; command_id: CommandId; work_id?: WorkId;
	    work_version?: WorkVersion; effect: CommandEffect;
	    committed_at: EventSequence }
	| { kind: "rejected"; command_id: CommandId; reason: RejectionCode;
	    current_work_id?: WorkId; current_version?: WorkVersion;
	    evidence: EvidenceRef[]; committed_at: EventSequence };

type Query =
	| { kind: "work_list"; repository?: RepositoryRef;
	    phase?: WorkPhase; cursor?: string }
	| { kind: "work_detail"; work_id: WorkId }
	| { kind: "attention"; work_id?: WorkId }
	| { kind: "capacity" }
	| { kind: "anomalies"; item_limit?: number;
	    deferral_age_events?: number }
	| { kind: "command_result"; lookup:
	    | { kind: "id"; command_id: CommandId }
	    | { kind: "replay_key"; origin: string; actor: Actor; idempotency_key: string } };

type QueryResult<T> =
	| { kind: "found"; value: T; as_of: EventSequence;
	    sources: { source: string; observed_at?: string;
	               availability: "known" | "unknown" | "unavailable" }[] }
	| { kind: "not_found" | "forbidden" | "store_unavailable" };

type ActionPreview = { availability: "available" | "blocked" | "unknown";
	observed_work_version?: WorkVersion; observed_head?: CommitRevision;
	reasons: RejectionCode[]; evidence: EvidenceRef[] };
type AttentionView = { attention_id: string; work_id: WorkId;
	reason: string; evidence: EvidenceRef[];
	investigation_threshold_seconds?: number;
	next_actor?: Actor; next_action?: string };
type WorkPhase = "queued" | "implementing" | "reviewing"
	| "converging" | "waiting" | "paused" | "stopping"
	| "completed" | "stopped" | "unknown";
type WorkSummary = { work_id: WorkId; version: WorkVersion;
	phase: WorkPhase; current_execution_id?: ExecutionId;
	attention_count: number; outcome?: OutcomeView; evidence: EvidenceRef[] };
type WorkList = { items: WorkSummary[]; next_cursor?: string };
type OutcomeView = { subject:
	| { kind: "work"; id: WorkId }
	| { kind: "execution"; id: ExecutionId };
	kind: "completed" | "failed" | "paused" | "needs_action" | "cancelled";
	evidence: EvidenceRef[] };
type ExecutionView = { execution_id: ExecutionId;
	state: ExecutionStateKind;
	ownership: "confirmed" | "uncertain";
	invocation_ids: InvocationId[]; outcome?: OutcomeView };
type ExecutionState =
	| { kind: "queued"; enqueued_at: EventSequence }
	| { kind: "ready"; request_id: string }
	| { kind: "running"; invocation_id: InvocationId;
	    stage: "coding" | "review" | "fix" | "invariant_check" }
	| { kind: "waiting"; reason: string; head?: CommitRevision }
	| { kind: "paused"; reason: string; continuation: EvidenceRef }
	| { kind: "uncertain"; request_id: string; reason: string;
	    desired: "continue" | "cancel" | "fail" }
	| { kind: "stopping"; request_id: string; desired: "cancel" | "fail" }
	| { kind: "succeeded"; outcome: OutcomeView }
	| { kind: "failed"; outcome: OutcomeView }
	| { kind: "cancelled"; stop_evidence: EvidenceRef[] };
type ExecutionStateKind = ExecutionState["kind"];
type WorkDetail = { work_id: WorkId; version: WorkVersion; host_binding: HostBinding;
	current_execution_id: ExecutionId; executions: ExecutionView[];
	current_change_request?: { ref: ChangeRequestRef; head?: CommitRevision };
	findings: FindingRef[]; outcome?: OutcomeView;
	attention: AttentionView[]; evidence: EvidenceRef[] };
type CapacityView = {
	slots: { used: number; limit: number;
	    holders: { execution_id: ExecutionId; state: ExecutionStateKind;
	               since: EventSequence }[] };
	reservations: (ResourceReservation & { since: EventSequence })[];
	deferred: { execution_id: ExecutionId; reason: RejectionCode;
	            diagnostic: string; since: EventSequence }[] };
type AnomalyCensus = { as_of: EventSequence;
	judgment_waits: DetectorSection<JudgmentWaitItem>;
	released_wait_blocked: DetectorSection<ReleasedWaitBlock>;
	orphaned_findings: DetectorSection<OrphanedFinding>;
	parked_selector_starvation: DetectorSection<ParkedDeferral>;
	stale_spec_halts: DetectorSection<unknown>;
	source_gate_holds: DetectorSection<SourceGateItem> };
type DetectorSection<T> = { applicable: boolean; note?: string;
	items: T[]; total: number; truncated: boolean; limit: number;
	context?: unknown };
```

`submit` has no Work ID: `work` allocates it only after accepting the Command. An existing-Work Command must carry the version the caller saw. A review judgment also carries its expected PR head. The server scopes the idempotency key to the installation, origin, and actor, and stores a digest of the canonical payload. The same key and payload return the saved `CommandDecision`; the same key with a different payload yields `idempotency_conflict`. A transport timeout has no `CommandDecision` yet from the caller's point of view, so the caller queries or replays rather than assuming rejection. v0.1 retains command keys and decisions indefinitely — a lost response must always be recoverable — and each adapter's actor identity comes from the installation config and stays stable across restarts. This replay guarantee is scoped to the active store authority. Under #50, the unversioned legacy store is preserved for read-only reference and not imported into the fresh store; archive inspection does not resume old Work or replay old keys into new execution. An explicit StoreId distinguishes those histories.

`claim_resources` and `release_resources` let a manual or unmanaged writer register on the same reservation store that Execution admission uses; they carry no Work ID. An Execution's reservation is declared as `{workspace: <id derived from the Work ID>}` plus `{repository, branch}` with the branch fixed as `work/<work_id>`, is acquired in its `queued → ready` commit, and is held until the Execution reaches a terminal state. An external claim names only `(repository, branch)` pairs, carries a TTL renewed by repeating `claim_resources` from the same actor, and conflicts with any held row are rejected as `resource_conflict` with the current holder returned in evidence. Claim expiry forfeits the exclusion guarantee; it is not evidence that the writer stopped. `release_resources` frees the claimer's own rows; an operator force-release is recorded as an override decision.

`work.preview(CommandRequest) → ActionPreview` uses saved facts and is advisory. It reserves neither capacity nor execution ownership. A Query never refreshes the repository host or starts a worker. `projection` exposes current Attention but does not evaluate Command eligibility. A Command handler may obtain a new Observation before deciding. The final Command decision always rechecks preconditions.

The `capacity` Query returns a `CapacityView`: which Executions hold slots and why, all held resource reservations including external claims, and queued Executions deferred by a diagnostic such as a conflicting claim or an unavailable source. It is the explanation surface for queue stalls — no separate reservation bookkeeping exists for it to disagree with. `AttentionView` carries the investigation threshold that produced it and the next responsible actor/action; acknowledgement marks the displayed version as seen, and a new version is the only thing that resurfaces it.

`anomalies` returns the bounded read-only anomaly census — the structured inventory of stuck lane states that a doctor pass consumes as its Phase-1 input. It writes nothing, launches nothing, and refreshes no external source; every detector section is always present and individually capped at `item_limit` items with `truncated` reporting the cap honestly, and an empty `items` with `applicable: true` is a clean bill for that detector rather than missing coverage. `judgment_waits` reports sealed `review_rounds` lacking `round_judgments` coverage of their `set_digest`, `paused` Executions whose `paused_judgment` Attention is unacknowledged, and undecodable store records — unreadable evidence is reported, never silently dropped. `released_wait_blocked` reports nonterminal `paused`/`waiting` Executions whose wait Attention was acknowledged but which saw no accepted `resume`/`retry` Command; rejected attempts stay visible as context. `orphaned_findings` reports `finding_occurrences` on uncovered rounds with no unresolved writer Invocation, each classified `proven` (bound to this installation's host with recorded invocations), `foreign` (bound to a different host binding), or `uncertain` (missing binding or unreadable source); `uncertain` and `foreign` are report-only and no write-capable path may consume them. `parked_selector_starvation` reports `deferrals` aged past `deferral_age_events` in event-sequence distance — queue-order time, not wall clock — with the capacity-slot and reservation context that explains them. `stale_spec_halts` is `not_applicable_v0.1`: no spec-halt record exists in the v0.1 store, so the section is emitted with `applicable: false` and an explicit note rather than dropped. `source_gate_holds` covers `works.source_gate` ∈ {`changed`, `needs_check`} holds and Executions `waiting` on `human_judgment` — the spec:ambiguous / needs-human sweep records.

`WorkList` is the Kanban-style management view; its `queued` items have current Executions awaiting capacity. Phase, Outcome, and Attention count are derived from saved Work, Execution, and Observation records; there is no separate card status or card ID. `waiting` means an external check or automatic step is pending, `paused` means a safe stop awaits a decision, `stopping` means stop is not confirmed, `stopped` has a failed or cancelled Outcome, and `unknown` means evidence is insufficient. A stopped Work does not return to `queued` without an accepted Command. Pagination uses a stable order over committed enqueue sequences and the snapshot sequence. `submit` creates E1 `queued`; `retry` creates E2 `queued`; `resume` requeues the same E. Internal dispatch later reserves capacity and records an Invocation request in one commit. `WorkDetail.findings` contains findings actionable for the current review target, which is the PR head when a PR exists. Findings for earlier revisions remain in evidence history. An empty list with an unavailable review source is not evidence that review found no issues; `QueryResult.sources` carries that distinction.

```ts
type ObservationBase = { id: ObservationId;
	source_event_id?: string; subject: SubjectRef; observed_at: string;
	evidence: EvidenceRef[] };
type UnknownFact = { kind: "unknown"; reason: string };
type Observation = ObservationBase & (
	| { source: "repository_host"; instance: HostInstanceId; adapter_source: string;
	    fact: HostFact | UnknownFact }
	| { source: "execution"; fact: InvocationFact | UnknownFact }
);

type HostContext = { adapter_id: string; instance: HostInstanceId;
	repository: RepositoryRef; actor: HostActorRef; contract_version: 1 };
type CapabilityReport = { context: HostContext; observed_at: string;
	capabilities: { capability: HostCapability;
	    support: "supported" | "unsupported" | "unknown";
	    authorization: "allowed" | "denied" | "unknown";
	    reason?: string; evidence: EvidenceRef[] }[] };
type HostReadTarget = { subject: SubjectRef; context: HostContext } & (
	| { kind: "repository"; repository: RepositoryRef }
	| { kind: "issue"; issue: IssueRef }
	| { kind: "branch"; repository: RepositoryRef; branch: string }
	| { kind: "change_request"; change_request: ChangeRequestRef;
	    revision?: CommitRevision;
	    include: ("head" | "checks" | "review_findings" | "approvals" | "readiness" | "comments")[] }
	| { kind: "thread"; change_request: ChangeRequestRef; thread_id: string }
);
type HostEffectFact =
	| { kind: "ref_published"; repository: RepositoryRef; branch: string;
	    produced: CommitRevision; observed: CommitRevision;
	    relation: "equal" | "ancestor"; evidence: EvidenceRef[] }
	| { kind: "change_opened"; change_request: ChangeRequestRef;
	    source_branch: string; target_branch: string; head: CommitRevision }
	| { kind: "metadata_updated"; change_request: ChangeRequestRef; head: CommitRevision;
	    title?: string; body_digest?: string }
	| { kind: "comment_posted"; change_request: ChangeRequestRef;
	    comment_id: string; actor: HostActorRef; marker: string; body_digest: string;
	    target_revision: CommitRevision; thread_id?: string; location?: DiffLocation }
	| { kind: "feedback_published"; change_request: ChangeRequestRef; head: CommitRevision;
	    review_id: string; actor: HostActorRef; body_digest: string }
	| { kind: "review_decision"; change_request: ChangeRequestRef; head: CommitRevision;
	    actor: HostActorRef; decision: "approved" | "changes_requested";
	    eligible: boolean }
	| { kind: "thread_resolved"; change_request: ChangeRequestRef;
	    thread_id: string; head: CommitRevision; resolved: true }
	| { kind: "ready_observed"; change_request: ChangeRequestRef; head: CommitRevision; ready: true };
type HostActionReceipt = { action_id: ActionId; payload_digest: string;
	binding: HostBinding; intended_actor: HostActorRef;
	change_request?: ChangeRequestRef; revision?: CommitRevision;
	evidence: EvidenceRef[] } & (
	| { status: "applied"; authenticated_actor: HostActorRef;
	    effects: [HostEffectFact, ...HostEffectFact[]] }
	| { status: "failed" | "unknown"; authenticated_actor?: HostActorRef;
	    effects: HostEffectFact[] }
);
type HostCommentFact = { kind: "comment"; change_request: ChangeRequestRef; comment_id: string;
	actor: HostActorRef; content: EvidenceRef; body_digest: string;
	marker?: string; thread_id?: string; revision?: CommitRevision };
type HostFact =
	| { kind: "repository"; repository: RepositoryRef; default_branch: string }
	| { kind: "issue"; issue: IssueRef; source_revision: string;
	    title: string; content: EvidenceRef }
	| { kind: "branch"; repository: RepositoryRef; branch: string;
	    state: { kind: "absent" } | { kind: "at"; revision: CommitRevision } }
	| { kind: "change_head"; change_request: ChangeRequestRef; head: CommitRevision }
	| { kind: "change_merged"; change_request: ChangeRequestRef; head: CommitRevision;
	    base: CommitRevision; target_branch: string }
	| { kind: "mergeability"; change_request: ChangeRequestRef; head: CommitRevision;
	    base?: CommitRevision; target_revision?: CommitRevision;
	    state: "mergeable" | "conflicting" | "unknown";
	    conflicting_paths?: string[]; reason?: string }
	| { kind: "check"; change_request: ChangeRequestRef; head: CommitRevision;
	    check_id: string; producer: HostActorRef; conclusion: CheckResult }
	| { kind: "review"; change_request: ChangeRequestRef; head: CommitRevision;
	    reviewer: HostActorRef; findings: FindingRef[] }
	| { kind: "approval"; change_request: ChangeRequestRef; head: CommitRevision;
	    actor: HostActorRef; decision: "approved" | "changes_requested" | "withdrawn";
	    eligible: boolean }
	| { kind: "readiness"; change_request: ChangeRequestRef; head: CommitRevision; ready: boolean }
	| HostCommentFact
	| { kind: "thread"; change_request: ChangeRequestRef; thread_id: string;
	    resolved: boolean; comments: HostCommentFact[] }
	| { kind: "host_action_result"; result: HostActionReceipt };

type InvocationRequest = { request_id: string; work_id: WorkId;
	execution_id: ExecutionId; input_revision: CommitRevision; budget: Budget } & (
	| { role: "coding"; task: CodingTask }
	| { role: "fix"; task: FixTask }
	| { role: "review"; task: ReviewTask }
	| { role: "invariant_check"; task: InvariantCheckTask }
);
type WorkerTask = InvocationRequest & { invocation_id: InvocationId;
	workspace: WorkspaceRef; allowed_effects: AllowedEffects };
type WorkerStartReceipt =
	| { kind: "started"; process_identity: string }
	| { kind: "rejected"; reason: string }
	| { kind: "unknown"; reason: string };
type StopReceipt =
	| { kind: "requested"; process_identity: string }
	| { kind: "already_stopped"; evidence: EvidenceRef[] }
	| { kind: "unknown"; reason: string };
interface ProcessControlPort {
	inspect(invocation_id: InvocationId): ProcessEvidence;
	requestStop(invocation_id: InvocationId, reason: string): StopReceipt;
}
type TaskBase = { goal: string; input_artifacts: EvidenceRef[];
	acceptance_criteria: string[]; verification_argv: string[][] };
type CodingTask = TaskBase & { kind: "coding"; issue_refs: IssueRef[] };
type FixTask = TaskBase & { kind: "fix"; predecessor: EvidenceRef;
	origin: { kind: "prepublication"; failed_evidence: EvidenceRef[] }
	    | { kind: "review"; target: EvidenceRef; round: EvidenceRef;
	        judgment: EvidenceRef; findings: FindingRef[] }
	    | { kind: "conflict"; change: ChangeRequestId; source_revision: CommitRevision;
	        target_revision: CommitRevision; paths: string[] } };
type ReviewTask = TaskBase & { kind: "review"; reviewed_revision: CommitRevision };
type InvariantCheckTask = TaskBase & { kind: "invariant_check";
	artifact: EvidenceRef; invariant_set: EvidenceRef;
	specification_revision: number; assessed_revision: CommitRevision };
type Budget = { deadline?: string; max_invocations?: number;
	max_review_rounds?: number; runtime_limit_seconds?: number;
	first_admitted_at?: string };
type ExecutionSpec = { execution_id: ExecutionId; work_id: WorkId; host_binding: HostBinding;
	input_revision: CommitRevision; intent_digest: string; budget: Budget;
	initial_state: "queued" };
type ExecutionTransition = { execution_id: ExecutionId;
	expected_state: ExecutionStateKind; next_state: ExecutionState;
	reason: string; evidence: EvidenceRef[] };
type WorkspaceRef = { workspace_id: string; path: string };
type AllowedEffects = { workspace_write: boolean; local_git_write: boolean;
	repository_host_write: false };
// Target envelope for immutable changes, not an implemented Rust schema.
type PreparedChange = { schema_version: number; work_id: WorkId;
	execution_id: ExecutionId; request_id: string; invocation_id: InvocationId;
	task_digest: string; repository: RepositoryRef; base_revision: CommitRevision;
	produced_revision: CommitRevision; git_objects: EvidenceRef; checks: EvidenceRef[] };
type WorkerReport =
	| { kind: "coding" | "fix"; invocation_id: InvocationId;
	    status: "completed" | "failed" | "waiting";
	    artifacts: EvidenceRef[]; checks: EvidenceRef[] }
	| { kind: "review"; invocation_id: InvocationId; reviewed_revision: CommitRevision;
	    status: "completed" | "failed" | "waiting";
	    findings: FindingRef[]; evidence: EvidenceRef[] }
	| { kind: "invariant_check"; invocation_id: InvocationId;
	    status: "completed" | "failed" | "waiting"; artifact: EvidenceRef;
	    invariant_set: EvidenceRef; assessment: EvidenceRef; evidence: EvidenceRef[] };
// Coding and fix retain distinct report kinds with the same artifact fields.
// Invariant-check reports additionally carry the exact set/coverage and artifact
// under the typed evidence-gates.md schema; they are not a code-review verdict.
type ProcessEvidence =
	| { state: "alive"; process_identity: string; observed_at: string }
	| { state: "stopped"; process_identity: string; observed_at: string;
	    exit_code?: number }
	| { state: "unknown"; reason: string; observed_at: string };
type InvocationFact = { kind: "invocation"; execution_id: ExecutionId;
	invocation_id: InvocationId; process: ProcessEvidence;
	worker_report?: WorkerReport };

type HostActionIntent = { action_id: ActionId; work_id: WorkId; binding: HostBinding;
	repository: RepositoryRef; payload_digest: string;
	actor: { identity: HostActorRef; credential_ref: string;
	    role: "publication" | "review" | "discussion" } } & (
	| { operation: { kind: "push_ref"; branch: string;
	    expected_previous_ref: { kind: "absent" } | { kind: "at"; sha: CommitRevision };
	    produced_head: CommitRevision; change_artifact: EvidenceRef } }
	| { operation: { kind: "open_change_request"; branch: string; base: string;
	    published_head: CommitRevision; push_action_id: ActionId;
	    title: string; body: EvidenceRef } }
	| { change_request: ChangeRequestRef; expected_head: CommitRevision;
	    operation: { kind: "update_change_request"; title?: string; body?: EvidenceRef }
	      | { kind: "post_comment"; body: EvidenceRef; marker: string; thread_id?: string }
	      | { kind: "post_inline_comment"; body: EvidenceRef; marker: string;
	          location: DiffLocation }
	      | { kind: "publish_review_feedback"; body: EvidenceRef }
	      | { kind: "approve_change" }
	      | { kind: "request_changes"; reason: EvidenceRef }
	      | { kind: "resolve_thread"; thread_id: string }
	      | { kind: "mark_ready" } }
);

type WorkChange =
	| { kind: "none" }
	| { kind: "transition"; new_execution?: ExecutionSpec;
	    execution_transition?: ExecutionTransition;
	    judgments?: { head: CommitRevision; decisions: FindingDecision[] };
	    requests: InvocationRequest[];
	    host_actions: HostActionIntent[] };
type WorkCommit =
	| { kind: "work"; work_id: WorkId; expected_version?: WorkVersion;
	    cause:
	      | { kind: "command"; request: CommandRequest;
	          decision: { kind: "accepted"; effect: CommandEffect }
	            | { kind: "rejected"; reason: RejectionCode } }
	      | { kind: "observation"; observation: Observation }
	      | { kind: "dispatch"; request_id: string }
	      | { kind: "policy_limit"; execution_id: ExecutionId;
	          limit: "work_deadline" | "runtime" | "invocations" | "review_rounds";
	          observed_at: string };
	    change: WorkChange }
	| { kind: "intake_rejection"; request: CommandRequest & { kind: "submit" };
	    reason: RejectionCode; change: { kind: "none" } };
type PendingExecution = { work_id: WorkId; execution_id: ExecutionId;
	expected_work_version: WorkVersion; enqueued_at: EventSequence };
type PendingExecutionPage = { items: PendingExecution[]; next_cursor?: string;
	as_of: EventSequence };
type CommitReceipt =
	| { kind: "committed_work"; work_version: WorkVersion;
	    as_of: EventSequence; command_decision?: CommandDecision }
	| { kind: "recorded_rejection"; as_of: EventSequence;
	    command_decision: Extract<CommandDecision, { kind: "rejected" }> }
	| { kind: "version_conflict"; current_version: WorkVersion }
	| { kind: "constraint_conflict"; constraint: "active_execution" | "capacity";
	    current_version: WorkVersion };

type ClaimReceipt =
	| { kind: "claimed"; expires_at?: string; as_of: EventSequence }
	| { kind: "released"; as_of: EventSequence }
	| { kind: "conflict"; holder: ResourceReservation["holder"]; as_of: EventSequence }
	| { kind: "not_held"; as_of: EventSequence };

type SafeOwnership = { kind: "safe_to_start"; evidence: EvidenceRef[] };
type OwnershipAssessment =
	| SafeOwnership
	| { kind: "uncertain"; reason: string; evidence: EvidenceRef[] };
type InvocationRecord = { invocation_id: InvocationId;
	request_id: string; execution_id: ExecutionId; role: "coding" | "fix" | "review" | "invariant_check";
	input_revision: CommitRevision; ownership: SafeOwnership };
type InvocationWrite =
	| { kind: "created"; record: InvocationRecord }
	| { kind: "observed"; fact: InvocationFact;
	    pending_work_observation: Observation };
type StoredSnapshot = { as_of: EventSequence; work?: WorkDetail;
	work_list?: WorkList;
	attention: AttentionView[]; command_decision?: CommandDecision;
	observations: Observation[];
	sources: { source: string; observed_at?: string;
	           availability: "known" | "unknown" | "unavailable" }[] };
type IngestReceipt = { kind: "recorded" | "duplicate" | "rejected";
	as_of?: EventSequence; reason?: RejectionCode };
type DispatchReceipt = { kind: "queued" | "already_queued" | "ownership_uncertain";
	request_id: string };
```

`review` and `check` facts always include the head they examined. A delayed webhook is a signal to fetch the current PR head, not authority to replace it with the event's old head. The old event remains evidence for its own revision. `ProcessEvidence.unknown` is the execution supervisor's observation when a worker cannot be reached; the worker does not claim that status for itself. A worker report is a claim with evidence, not an Outcome. The verification commands accepted by `submit` become part of the bounded coding task; the control adapter cannot replace them when dispatching a worker.

`ProcessControlPort` is internal to `execution`: its host or worker-runtime adapter inspects and requests a stop for the recorded Invocation, then `execution` persists the resulting process Observation. A `requested` stop is not proof of termination. Inspection reads process state, not mere presence: an unreaped zombie still lists in the process table but cannot write, and `execution` reaps the processes it spawned. Stop proof covers the descendant snapshot recorded with the stop intent — a `setsid`-escaped child survives a process-group signal and becomes invisible to the process tree once its leader exits. Before signaling any recorded identity the port re-verifies its recorded start time so a reused PID is never signaled. If the runtime cannot identify and inspect the old writer, `work` keeps the Execution `uncertain` and does not grant another writer. This port is not a second lifecycle decision maker.

`AllowedEffects` states the target boundary: workers may change their isolated workspace, but repository-host mutation goes through the recorded `HostActionIntent`. A `HostActionIntent` is saved before the host adapter writes; an unknown external result must be reconciled before `work` treats the action as applied.

The individually reported baseline capabilities and required optional effects are validated under [ADR-002](repository-host-contract.md) before acceptance/dispatch; the Work binding is immutable. All revision repositories and action actors must match that binding.

A receipt with `status: applied` carries the authenticated adapter actor and nonempty effect facts. Embedded comment/review actor fields identify the external author; reading an already-ready state does not attribute its original transition to the reader. The receipt must include the operation-specific `HostEffectFact` matching the exact authorized target, actor, payload, and postcondition. Missing/mismatched normalized facts cannot establish success, even with opaque provider evidence attached. Partial facts remain retained under failed/unknown results. Core validates them without decoding provider JSON. `ref_published.relation` is an independently verified publication fact, not permission to reuse old review evidence on a newer observed head.

Each inline intent fixes the change request, head, comparison-base and target revisions in `DiffSnapshotRef`, plus path/line/side. The adapter verifies that immutable diff snapshot and its retained evidence before applying, including when the target branch changed without a source-head change. A mismatch rejects the old intent; never recompute its position against a newer diff. Ordinary external comments without a proven revision binding cannot count as a head-bound review or satisfy an applied inline effect.

The target `push_ref` performs only a managed branch update and requires the approved previous ref to be absent or exactly the recorded SHA. For an existing branch, the produced commit must also be a descendant of that SHA; reject a changed ref or non-fast-forward update. `open_change_request` creates only the PR after the referenced push action is verified applied to `published_head`; it performs no push, carries no prior PR head, and reconciles by exact repository/head branch/base and PR existence. A branch/head mismatch prevents PR creation and returns a fresh Observation to Core. `update_change_request` changes PR metadata only; later fix publication uses a new `push_ref` intent, not a metadata update.

`update_change_request`, `post_comment`, `publish_review_feedback`, `approve_change`, `request_changes`, `resolve_thread`, and `mark_ready` carry `expected_head`, and the integration re-reads the head immediately before writing; a mismatch aborts the write, records `host_action_result: failed` and a fresh `change_head` Observation, and `work` re-evaluates. v0.1 never re-issues an action automatically — a retry requires evidence that covers the new head.

The separate push/PR-create effects, prepared-change envelope, selected actor, review submission, reply marker, thread/ready operations, and result receipts above are target-contract additions from [ADR-001](adapter-boundaries.md#adr-001-core-authorizes-publication-github-integration-performs-it), not implemented Rust API claims. Each effect is separately durable and reconciled; a pre-write head read is not atomic with the host. Each action is mapped to its baseline or optional capability under ADR-002 before authorization. A thread-targeted `post_comment` additionally requires `thread_reply`. Receipts bind action/payload, producing Invocation/artifact, target PR/head, and observed effect, including published SHA or comment/thread/ready identity. A branch update also checks its prior ref to avoid overwriting a concurrent writer.

Host authentication is adapter-specific. The GitHub adapter accepts App installation or user-token authentication through configuration; credentials stay outside the Worker and the action carries only a reference. Review submission targets `expected_head` and preserves the requested review effect; missing independent-approval eligibility cannot silently downgrade it. Core builds stage evidence from those facts and never treats the existing publication classifier as the complete converge gate.

`SafeOwnership` records the evidence that permits an Invocation to start. It does not name or prescribe a Lease mechanism. An uncertain assessment cannot produce an `InvocationRecord`. If the state store reports a version or constraint conflict, `work` rereads state and evaluates the Command or Observation again; it does not retry the proposed write unchanged.

## Ports and all module connections

These are behavioral ports, not separate processes. Delivery between `work`, `execution`, and the dispatch loop is in-process in v0.1; the store itself is the durable handoff — an approved request persists until consumed, and `execution` reconciles by scanning it on wake and after restart. Wakeups are at-least-once signals, never evidence. The state store owns atomic writes and uniqueness checks; `work` and `execution` own the policy behind those writes.

| Diagram edge | Port and message | Receiver's obligation |
| --- | --- | --- |
| CLI → `work` | `submit(CommandRequest) → CommandDecision` | Validate identity, version, policy, and idempotency. |
| CLI → `projection` | `query(Query) → QueryResult<T>` | Read the queue, Work detail, and a Command result after lost response. |
| Agent skills → CLI | CLI invocation and structured results | Preserve identity, keys, preconditions, and unknown results; no separate execution authority. |
| TUI (deferred) → `work` | `preview(CommandRequest) → ActionPreview`; `submit(CommandRequest) → CommandDecision` | Apply the same rules as CLI; preflight gives no extra authority. |
| TUI (deferred) → `projection` | `query(Query) → QueryResult<T>` | Return the queue, Work detail, Attention, Command results, and source availability. |
| GitHub comment adapter (optional) → `work` | `submit(CommandRequest) → CommandDecision` | Map external actor and delivery ID; reject duplicate effects. |
| GitHub comment adapter (optional) → `projection` | `query(Query) → QueryResult<T>` | Read the status to render; a comment is not the state store. |
| repository host adapter → `work` | `ingest(Observation) → IngestReceipt` | Deduplicate source events and assess facts against the current Work revision. |
| `work` → repository host adapter | `HostActionIntent → action result Observation` | Apply only the recorded intent; reconcile an unknown response. |
| `work` → `execution` | durable `InvocationRequest → DispatchReceipt` | Verify the request and ownership before creating an Invocation. |
| `execution` → `work` | `ingest(Observation) → IngestReceipt` | Assess worker and process facts; do not copy a worker's claimed Outcome. |
| `work` → state store | `commitWorkDecision(WorkCommit) → CommitReceipt` | Atomically enforce Work version, one nonterminal Execution, reservation changes derived from state transitions, Command result, and outgoing intents. Observation assessment uses the same port without inventing a Command. |
| `work` → state store | `claimResources(ResourceReservation) → ClaimReceipt` | Enforce per-resource uniqueness across execution reservations and external claims in one write. A repeat claim from the same actor renews the expiry; a conflicting holder yields `resource_conflict`. |
| `execution` → state store | `appendInvocation(InvocationWrite) → IngestReceipt` | Persist identity before launch; append process, checkpoint, and result evidence without overwriting history. Save the pending Work Observation with its Invocation fact. |
| State store → `work` | `listQueuedExecutions(cursor) → PendingExecutionPage` | Return queued Executions in committed enqueue order with the Work version seen; `work` rechecks eligibility in the dispatch transaction. |
| State store → `execution` | approved `InvocationRequest` by durable request ID | Start or reconcile the approved request; never infer permission from a visible queue phase. |
| State store → `projection` | `readSnapshot(Query) → StoredSnapshot` | Return Work and pending request records with event sequence and source availability; projection only shapes the queue and other views. |
| `execution` → CodingWorker | `start(WorkerTask) → WorkerStartReceipt` | Run the bounded coding task under the permitted workspace and effects. |
| `execution` → ReviewWorker | `start(WorkerTask) → WorkerStartReceipt` | Review the specified patch or PR head under the given budget. |
| CodingWorker → `execution` | coding `WorkerReport` and evidence | Correlate by Invocation ID; report artifacts and checks, not Work completion. |
| ReviewWorker → `execution` | review `WorkerReport` and evidence | Correlate by Invocation ID and reviewed revision; report findings, not final acceptance. |

`IngestReceipt` reports recorded, duplicate, or rejected input. `DispatchReceipt` reports queued, already queued, or refused because safe ownership cannot be established. Delivery of a durable `InvocationRequest` is at least once; its request ID prevents a second Invocation for the same request. A Command that creates or requeues an Execution saves its decision and queue position together, without reserving capacity. The internal `queued → ready` transition reserves capacity and saves the first request in one transaction. A process crash after admission therefore leaves an approved request for `execution` to resume or reconcile. `execution` records an Invocation before launch, and a lost start response becomes `ProcessEvidence.unknown` until reconciled. Repository-host writes cannot share this transaction and require reconciliation.

The internal dispatch loop reads `PendingExecutionPage` from the same store; it does not consume external board records. It may propose a start, but `work` decides eligibility, including the [Work-wide recovery ledger](evidence-gates.md#work-wide-recovery-accounting), and the store atomically reserves capacity while moving that Execution to `ready` and saving an Invocation request. `work` derives the request ID from the Execution ID and its next Invocation sequence; the commit checks Work version and enforces request ID uniqueness. A conflict makes `work` reread before deciding again. `WorkCommit.cause.dispatch` records this internal decision without impersonating an operator Command. `policy_limit` records a budget or deadline decision without inventing a worker/GitHub Observation; the commit compares its observed time or count with the stored limit. A later coding, fix, invariant-check, or code-review request in the same reserved Execution does not reserve a second slot. `execution` reads approved Invocation requests from the store. Re-reading after a crash must find the prior request ID before any new start. The queue view itself has no write path: an operator action uses a typed Command, and rendering or selecting a row has no lifecycle effect.

The worker ports must not give a worker an unreviewed path to mutate the repository host. The target design routes PR publication and other PR actions through `HostActionIntent`; a managed Worker entry point disables those writes, its independent claims, outer retry/review loop, and final settlement. The repository host adapter performs the remote effects authorized by Core; the stage runner translates one bounded task and returns evidence. [ADR-001](adapter-boundaries.md#adr-001-core-authorizes-publication-github-integration-performs-it) defines partial effects, credential scope, and evidence validity.

## Built-in queue admission

1. The CLI submits a Work item. `work` records W1, E1 in `queued`, and the Command result in one commit. The Work appears in `work_list` even if the executor is busy.
2. The internal dispatch loop selects a queued Execution in committed order and proposes a start using its observed Work version. This selection is only a proposal; a stale list does not confer execution permission.
3. `work` rechecks Work version, eligibility, capacity, input head, and ownership evidence. One state-store transaction moves E1 to `ready`, reserves capacity, and saves the Invocation request. A competing dispatch sees the existing request or a version/capacity conflict.
4. `execution` reads that approved request and records the Invocation before launch. If launch outcome is unknown, it inspects the recorded process identity before trying another start. The Work list shows `unknown` when evidence is insufficient, rather than silently returning it to `queued`.

No external board record is created in this flow. The CLI and its planned agent skills inspect the same state-store projections. The deferred TUI also reads these projections.

## Operator review judgment

The interaction is interface-independent: read Work detail, choose a decision for each finding, optionally preflight, and submit a typed action. For v0.1 the operator uses the CLI directly or through a planned agent skill. The existing TUI implements this interaction against fixtures but is deferred from release requirements. Its tests remain useful evidence for specific cases, not a release gate for an interactive console.

1. Query Work detail and current Attention. The Projection includes Work ID, Work version, PR head SHA, finding IDs, evidence references, and source availability. Caller selection state does not classify Attention.
2. Choose exactly one supported decision for every finding in the current head-scoped set. The target contract requires `work` to reject missing, duplicate, unknown, or empty finding IDs and unsupported decisions at acceptance, even if the head and Work version have not changed. Optional preflight is read-only and reserves nothing.
3. Submit `judge_findings` with the observed Work version, PR head, and stable idempotency key. A replay returns the saved result. For a new Command the target contract requires current-head verification and validation against the current finding set; an unavailable source cannot authorize an unverified judgment.
4. A changed head or finding set requires refreshed evidence and a new operator decision. Do not silently rebind prior choices to a new head or omit newly arrived findings.
5. Acceptance records the decisions against finding IDs and head SHA. A later `release_judgment` may requeue the same paused Execution if policy permits; internal dispatch checks the head again. The caller never creates an Invocation directly.
6. A lost response remains unknown until `command_result` or identical replay settles it. Do not generate another key or a new release request to bypass the unknown result.

**Known implementation gap (#24).** These are target requirements, not a claim that all checks exist. The current TUI checks draft coverage when opening Work, but does not re-read it at submit. The core rejects an empty decision set and duplicate IDs, but does not enforce exact coverage of the current finding set. A new finding on the same head can therefore be omitted without a Work-version change. Deferring TUI does not remove this shared Command-validation requirement. Keep #24 open; #36 must assess the core fix and regression coverage through the CLI/shared handler. No fix is implemented by this documentation change.

Existing `judge_findings_flow_end_to_end`, `incomplete_draft_cannot_submit`, and `stale_head_rejection_requeries_and_clears_choices` tests in `crates/workloop-tui/tests/tui_surface.rs` cover the fixture interaction, initial incomplete drafts, and head changes. They do not establish same-head finding-growth safety. Preserve these tests and require interface-independent verification of acceptance-time coverage, stale-head rejection, and lost-response recovery.

## Explicit retry after a failed Execution

Assume Work `W1` has Execution `E1` with a confirmed terminal failure. A retry is a new attempt under the current approved immutable specification revision; it is not another Invocation inside `E1`. Retry does not edit that revision. A separately approved same-Work specification revision may update intent, constraints, and checks while preserving binding, prior snapshots, approvals as history, and consumed budgets.

1. The CLI, directly or through an agent skill, queries `W1`. The Projection shows `E1`'s Outcome, evidence, current Attention, and whether process ownership is still uncertain.
2. The operator submits `retry` with `work_id=W1`, `expected_execution_id=E1`, the displayed Work version, and an idempotency key.
3. `work` checks the confirmed failure, retry budget, current Work version, and proof that the old writer stopped or cannot write. Expiry of a claim alone is insufficient. It also checks that no other holder owns the `work/<work_id>` branch or a conflicting workspace; a held reservation rejects the retry with `resource_conflict` naming the current holder. If ownership remains uncertain, `work` does not authorize `E2`.
4. On acceptance, the state store records the Command result and creates `E2` as `queued` in one transaction. Capacity shortage does not reject the accepted retry. Internal dispatch later rechecks gates, reserves capacity, and saves E2's first Invocation request. The worker result cannot create `E2` on its own.
5. A concurrent retry with another key sees the changed Work version or nonterminal `E2` and receives a rejection. Replaying the accepted key returns the recorded `E2` result without creating `E3`.

`resume` has a different contract: it requeues an eligible paused Execution with the same Execution ID and budget. Internal dispatch may create another Invocation after capacity becomes available. It never turns a confirmed failed `E1` into `E2` implicitly.

## Worker result and PR review

1. `execution` records an Invocation before starting a coding, fix, invariant-check, or code-review role. Its request names the Execution, input revision, task, and budget. Devin, Claude Code, or another provider can fill the role.
2. The worker returns artifacts and evidence to `execution`. `execution` records the Invocation result and reports an Observation to `work`. A worker saying "done" is evidence to assess, not a Work Outcome.
3. repository host adapter records separate Observations for the PR head, checks, and review effects. Each review conclusion is bound to the head SHA it examined. `work` assesses the worker and GitHub Observations together, then records an Outcome or requests another Invocation within the same Execution.
4. A later PR-head Observation makes a prior head's review conclusion stale. A delayed webhook for the old head triggers a current GitHub read; it cannot move the recorded current head backward. `work` cannot mark the new head reviewed from the old result. `projection` shows the current head and the missing review evidence in the next Query.
5. When `work` requests a PR action, it records the outgoing intent before repository host adapter performs the write. Reconciliation records the actual host result as an Observation. An accepted internal decision alone does not prove that the PR changed.

## State store model

v0.1 uses a hybrid schema on SQLite: current-state tables that enforce structural invariants, plus an append-only `event_log` for audit and reconstruction. `EventSequence` is the log's committed position — it orders the queue, marks snapshot `as_of`, and anchors resume positions.

Target evidence/recovery extensions use immutable `assessment_subjects`, `source_snapshots`, `review_rounds`, `review_occurrences`, `round_judgments`, `judgment_decisions`, `failure_records`, `recovery_decisions`, `recovery_usage_entries`, and `recovery_time_intervals`, with Work-current pointers. Their membership, exact-set checks, attempt uniqueness, and atomic usage/request updates are specified in [evidence and gates](evidence-gates.md). These are planned tables, not implemented schema.

Tables: `works`, `executions`, `invocations`, `invocation_requests`, `commands`, `resource_reservations`, `capacity_slots`, `observations`, `pr_action_intents`, `stop_intents` (holding descendant snapshots), `attention_acks`, `event_log`.

The store enforces, atomically inside the caller's transaction:

- `(origin, actor, idempotency_key)` unique, with the payload digest recorded — a replay returns the saved decision, a changed payload yields `idempotency_conflict`.
- `request_id` and `action_id` unique; target source-event uniqueness is `(host instance, adapter source, source_event_id)` for host Observations. Current slice-0 storage uses a single-source event key and must migrate with its callers. Preserve at-most-once requests, outbox identities, and deduplicated Observations.
- `UNIQUE(work_id)` over nonterminal Executions and `UNIQUE(execution_id)` over unresolved writers — partial unique indexes; a competing admission returns `constraint_conflict`.
- `resource_reservations.resource_key` unique over a normalized key space (versioned structured arrays `["workspace", store_id, workspace_id]` and `["branch", host_id, repository_id, branch_name]` in the target contract) covering execution reservations and external claims alike.
- `capacity_slots` holds N fixed rows; claiming a free row in the same transaction reserves the slot, and no free row yields `constraint_conflict("capacity")`.
- Work version by optimistic check in the commit (`WHERE version = expected`); a mismatch returns `version_conflict`.

The store does not evaluate transition legality, budgets, or gates — that stays in `work`. Every commit records its cause and appends to `event_log` in the same transaction, so a crash cannot leave a decision recorded without its effect or vice versa.

## Implementation-facing decisions

The target contract-level choices are settled above: store shape and constraint placement, in-process delivery with the store as durable handoff, the expected-head write contract, and command retention and actor identity. The host-neutral migration in ADR-002 remains implementation work; the current Rust schema and wire names are not changed here. Slice 0 has since produced first versions of all three items below — the SQLite DDL in `workloop-store`, the `ProcessControlPort` supervisor in `workloop-exec`, and the Cargo workspace layout — so the open items are refinements toward full v0.1 rather than greenfield decisions:

- The concrete DDL that realizes the store constraints, including the transaction boundaries for the `queued → ready` commit racing with `cancel`.
- The concrete process supervisor implementing `ProcessControlPort`: descendant snapshotting, state-checked inspection including zombies, PID-reuse-safe signaling, and child reaping. The [ownership mechanism is specified without the name Lease](glossary.md#execution-ownership); whether it is one or a fence is resolved by "no replacement while unproven".
- The Rust module and crate layout itself, which follows the ownership boundaries: `work`, `execution`, `projection`, shared persistence, and the CLI, GitHub, and worker adapters.

Directories follow the ownership boundaries: `work`, `execution`, `projection`, shared persistence, and the CLI, GitHub, and worker adapters, plus the retained deferred TUI. Agent skills invoke the CLI. The layout does not decide the contracts.

## Approved recovery and configuration policy

Use the owner-approved [recovery and configuration policy](recovery-policy.md) for configurable defaults, Work-wide accounting, policy changes, and safe human-wait capacity release. The [post-v0.1 milestones](post-v0.1-milestones.md) retain the seven approved machinery deferrals without deferring v0.1 safety requirements. Both documents specify target behavior, not implemented runtime support.
