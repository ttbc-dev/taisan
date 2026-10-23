# Module contracts

This document specifies the messages crossing every edge of the [module map](architecture.md#module-map), then checks them against four interactions. It is a design proposal, not an implemented API or a choice of programming language. The type notation is illustrative; names such as `judge_findings` and the exact state codes remain open. The terms and lifetimes come from the [glossary](glossary.md).

## Caller view

The CLI, TUI, and GitHub comment adapter use the same Command and Query contracts. The TUI lists the built-in queue with `work_list`, then queries `work_detail(W1)`, asks `work` for a read-only preflight of a judgment, and submits that Command with the displayed Work version and PR head. If the submit response disappears, it asks for the result by Command ID or replays the same key and payload. An accepted result means the decision is durable; it does not mean a worker or GitHub write has finished.

## Type sketch

IDs are distinct opaque types even if an implementation stores each as a string. `WorkVersion` is a version of one Work item. `EventSequence` identifies a committed position in one installation's history. `PrHeadSha` identifies the revision used for a review or check.

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
type PrHeadSha = OpaqueId<"pr_head_sha">;
type EvidenceRef = { id: string; digest: string; subject_revision?: string };
type GitHubIssueRef = { repository: string; number: number };
type GitHubPrRef = { repository: string; number: number };
type SubjectRef =
	| { kind: "work"; work_id: WorkId; execution_id?: ExecutionId;
	    invocation_id?: InvocationId; pr?: GitHubPrRef }
	| { kind: "candidate"; issue: GitHubIssueRef };
type FindingRef = { finding_id: string; reviewed_revision: string;
	summary: string; location?: { path: string; line?: number };
	evidence: EvidenceRef[] };
type CheckResult = "pending" | "passed" | "failed" | "unknown";
type RejectionCode = "invalid_request" | "unauthorized" | "stale_work_version"
	| "stale_pr_head" | "ineligible" | "ownership_uncertain"
	| "capacity_exhausted" | "source_unavailable" | "idempotency_conflict";

type Actor = { kind: "human" | "automation"; external_id: string };
type CommandMeta = { actor: Actor; origin: string; idempotency_key: string };
type ExistingWork = { work_id: WorkId; expected_work_version: WorkVersion };
type FindingDecision = { finding_id: string; choice: "fix" | "dismiss" | "accept" | "wait" };
type CommandEffect =
	| { kind: "work_created" }
	| { kind: "execution_created"; execution_id: ExecutionId }
	| { kind: "state_changed" };

type CommandRequest = CommandMeta & (
	| { kind: "submit"; intent: string; issue_refs: GitHubIssueRef[];
	    constraints: string[]; verification_argv: string[][] }
	| (ExistingWork & (
		| { kind: "retry"; expected_execution_id: ExecutionId }
		| { kind: "resume"; execution_id: ExecutionId }
		| { kind: "cancel"; execution_id?: ExecutionId }
		| { kind: "acknowledge"; attention_id: string }
		| { kind: "judge_findings"; expected_pr_head: PrHeadSha; decisions: FindingDecision[] }
		| { kind: "release_judgment"; expected_pr_head: PrHeadSha }
	))
);

type CommandDecision =
	| { kind: "accepted"; command_id: CommandId; work_id: WorkId;
	    work_version: WorkVersion; effect: CommandEffect;
	    committed_at: EventSequence }
	| { kind: "rejected"; command_id: CommandId; reason: RejectionCode;
	    current_work_id?: WorkId; current_version?: WorkVersion;
	    evidence: EvidenceRef[]; committed_at: EventSequence };

type Query =
	| { kind: "work_list"; repository?: string;
	    phase?: WorkPhase; cursor?: string }
	| { kind: "work_detail"; work_id: WorkId }
	| { kind: "attention"; work_id?: WorkId }
	| { kind: "command_result"; lookup:
	    | { kind: "id"; command_id: CommandId }
	    | { kind: "replay_key"; origin: string; actor: Actor; idempotency_key: string } };

type QueryResult<T> =
	| { kind: "found"; value: T; as_of: EventSequence;
	    sources: { source: string; observed_at?: string;
	               availability: "known" | "unknown" | "unavailable" }[] }
	| { kind: "not_found" | "forbidden" | "store_unavailable" };

type ActionPreview = { availability: "available" | "blocked" | "unknown";
	observed_work_version?: WorkVersion; observed_pr_head?: PrHeadSha;
	reasons: RejectionCode[]; evidence: EvidenceRef[] };
type AttentionView = { attention_id: string; work_id: WorkId;
	reason: string; evidence: EvidenceRef[] };
type WorkPhase = "queued" | "implementing" | "reviewing"
	| "converging" | "waiting" | "completed" | "stopped" | "unknown";
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
	activity: "active" | "settled" | "uncertain";
	ownership: "confirmed" | "uncertain";
	invocation_ids: InvocationId[]; outcome?: OutcomeView };
type WorkDetail = { work_id: WorkId; version: WorkVersion;
	executions: ExecutionView[]; current_pr?: { ref: GitHubPrRef; head?: PrHeadSha };
	findings: FindingRef[]; outcome?: OutcomeView;
	attention: AttentionView[]; evidence: EvidenceRef[] };
```

`submit` has no Work ID: `work` allocates it only after accepting the Command. An existing-Work Command must carry the version the caller saw. A review judgment also carries its expected PR head. The server scopes the idempotency key to the installation, origin, and actor, and stores a digest of the canonical payload. The same key and payload return the saved `CommandDecision`; the same key with a different payload yields `idempotency_conflict`. A transport timeout has no `CommandDecision` yet from the caller's point of view, so the caller queries or replays rather than assuming rejection.

`work.preview(CommandRequest) → ActionPreview` uses saved facts and is advisory. It reserves neither capacity nor execution ownership. A Query never refreshes GitHub or starts a worker. `projection` exposes current Attention but does not evaluate Command eligibility. A Command handler may obtain a new Observation before deciding. The final Command decision always rechecks preconditions.

`WorkList` is the Kanban-style management view; its `queued` items form the pending Work queue. Its phase, Outcome, and Attention count are derived from saved Work, Execution, and Observation records; there is no separate card status or card ID. `waiting` means an external check or automatic step is pending, `stopped` has a recorded failed, paused, needs-action, or cancelled Outcome, and `unknown` means evidence is insufficient. A stopped Work does not return to `queued` without an accepted Command. Pagination uses a stable order over accepted Work and the snapshot sequence. `submit` can accept Work into `queued` before capacity is available. An internal dispatch decision later creates its Execution, reserves capacity, and records an Invocation request in one commit. `WorkDetail.findings` contains findings actionable for the current review target, which is the PR head when a PR exists. Findings for earlier revisions remain in evidence history. An empty list with an unavailable review source is not evidence that review found no issues; `QueryResult.sources` carries that distinction.

```ts
type ObservationBase = { id: ObservationId;
	source_event_id?: string; subject: SubjectRef; observed_at: string;
	evidence: EvidenceRef[] };
type UnknownFact = { kind: "unknown"; reason: string };
type Observation = ObservationBase & (
	| { source: "github"; fact: GitHubFact | UnknownFact }
	| { source: "execution"; fact: InvocationFact | UnknownFact }
);

type GitHubFact =
	| { kind: "issue"; issue: GitHubIssueRef; source_revision: string }
	| { kind: "pr_head"; pr: GitHubPrRef; head: PrHeadSha }
	| { kind: "check"; pr: GitHubPrRef; head: PrHeadSha; conclusion: CheckResult }
	| { kind: "review"; pr: GitHubPrRef; head: PrHeadSha; findings: FindingRef[] }
	| { kind: "pr_action_result"; action_id: ActionId; status: "applied" | "failed" | "unknown" };

type InvocationRequest = { request_id: string; work_id: WorkId;
	execution_id: ExecutionId; input_revision: string; budget: Budget } & (
	| { role: "coding"; task: CodingTask }
	| { role: "review"; task: ReviewTask }
);
type WorkerTask = InvocationRequest & { invocation_id: InvocationId;
	workspace: WorkspaceRef; allowed_effects: AllowedEffects };
type WorkerStartReceipt =
	| { kind: "started"; process_identity: string }
	| { kind: "rejected"; reason: string }
	| { kind: "unknown"; reason: string };
type TaskBase = { goal: string; input_artifacts: EvidenceRef[];
	acceptance_criteria: string[]; verification_argv: string[][] };
type CodingTask = TaskBase & { kind: "coding"; issue_refs: GitHubIssueRef[] };
type ReviewTask = TaskBase & { kind: "review"; reviewed_revision: string };
type Budget = { deadline?: string; max_invocations?: number;
	max_review_rounds?: number };
type ExecutionSpec = { execution_id: ExecutionId; work_id: WorkId;
	input_revision: string; intent_digest: string; budget: Budget };
type WorkspaceRef = { workspace_id: string; path: string };
type AllowedEffects = { workspace_write: boolean; local_git_write: boolean;
	github_write: false };
type WorkerReport =
	| { kind: "coding"; invocation_id: InvocationId;
	    status: "completed" | "failed" | "waiting";
	    artifacts: EvidenceRef[]; checks: EvidenceRef[] }
	| { kind: "review"; invocation_id: InvocationId; reviewed_revision: string;
	    status: "completed" | "failed" | "waiting";
	    findings: FindingRef[]; evidence: EvidenceRef[] };
type ProcessEvidence =
	| { state: "alive"; process_identity: string; observed_at: string }
	| { state: "stopped"; process_identity: string; observed_at: string;
	    exit_code?: number }
	| { state: "unknown"; reason: string; observed_at: string };
type InvocationFact = { kind: "invocation"; execution_id: ExecutionId;
	invocation_id: InvocationId; process: ProcessEvidence;
	worker_report?: WorkerReport };

type PrActionIntent = { action_id: ActionId; work_id: WorkId;
	repository: string; payload_digest: string } & (
	| { operation: { kind: "publish_pr"; branch: string; base: string;
	    change_artifact: EvidenceRef } }
	| { pr: GitHubPrRef; expected_head: PrHeadSha;
	    operation: { kind: "update_pr"; change_artifact: EvidenceRef }
	      | { kind: "post_pr_comment"; body: EvidenceRef } }
);

type WorkChange =
	| { kind: "none" }
	| { kind: "transition"; outcome?: OutcomeView;
	    attention: AttentionView[]; new_execution?: ExecutionSpec;
	    judgments?: { head: PrHeadSha; decisions: FindingDecision[] };
	    capacity_delta: number; requests: InvocationRequest[];
	    pr_actions: PrActionIntent[] };
type WorkCommit =
	| { kind: "work"; work_id: WorkId; expected_version?: WorkVersion;
	    cause:
	      | { kind: "command"; request: CommandRequest;
	          decision: { kind: "accepted"; effect: CommandEffect }
	            | { kind: "rejected"; reason: RejectionCode } }
	      | { kind: "observation"; observation: Observation }
	      | { kind: "dispatch"; request_id: string };
	    change: WorkChange }
	| { kind: "intake_rejection"; request: CommandRequest & { kind: "submit" };
	    reason: RejectionCode; change: { kind: "none" } };
type PendingWork = { work_id: WorkId; expected_work_version: WorkVersion;
	accepted_at: EventSequence };
type PendingWorkPage = { items: PendingWork[]; next_cursor?: string;
	as_of: EventSequence };
type CommitReceipt =
	| { kind: "committed_work"; work_version: WorkVersion;
	    as_of: EventSequence; command_decision?: CommandDecision }
	| { kind: "recorded_rejection"; as_of: EventSequence;
	    command_decision: Extract<CommandDecision, { kind: "rejected" }> }
	| { kind: "version_conflict"; current_version: WorkVersion }
	| { kind: "constraint_conflict"; constraint: "active_execution" | "capacity";
	    current_version: WorkVersion };

type SafeOwnership = { kind: "safe_to_start"; evidence: EvidenceRef[] };
type OwnershipAssessment =
	| SafeOwnership
	| { kind: "uncertain"; reason: string; evidence: EvidenceRef[] };
type InvocationRecord = { invocation_id: InvocationId;
	request_id: string; execution_id: ExecutionId; role: "coding" | "review";
	input_revision: string; ownership: SafeOwnership };
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

`AllowedEffects` states the target boundary: workers may change their isolated workspace, but GitHub mutation goes through the recorded `PrActionIntent`. A `PrActionIntent` is saved before GitHub integration writes; an unknown external result must be reconciled before `work` treats the action as applied. Exact Work states, retry limits, and GitHub operation payloads can be refined without changing these ownership rules.

`SafeOwnership` records the evidence that permits an Invocation to start. It does not name or prescribe a Lease mechanism. An uncertain assessment cannot produce an `InvocationRecord`. If the state store reports a version or constraint conflict, `work` rereads state and evaluates the Command or Observation again; it does not retry the proposed write unchanged.

## Ports and all module connections

These are behavioral ports, not separate processes. Delivery can use a local call or a durable queue. The state store owns atomic writes and uniqueness checks; `work` and `execution` own the policy behind those writes.

| Diagram edge | Port and message | Receiver's obligation |
| --- | --- | --- |
| CLI → `work` | `submit(CommandRequest) → CommandDecision` | Validate identity, version, policy, and idempotency. |
| CLI → `projection` | `query(Query) → QueryResult<T>` | Read the queue, Work detail, and a Command result after lost response. |
| TUI → `work` | `preview(CommandRequest) → ActionPreview`; `submit(CommandRequest) → CommandDecision` | Apply the same rules as CLI; preflight gives no extra authority. |
| TUI → `projection` | `query(Query) → QueryResult<T>` | Return the queue, Work detail, Attention, Command results, and source availability. |
| GitHub comment adapter → `work` | `submit(CommandRequest) → CommandDecision` | Map external actor and delivery ID; reject duplicate effects. |
| GitHub comment adapter → `projection` | `query(Query) → QueryResult<T>` | Read the status to render; a comment is not the state store. |
| GitHub integration → `work` | `ingest(Observation) → IngestReceipt` | Deduplicate source events and assess facts against the current Work revision. |
| `work` → GitHub integration | `PrActionIntent → action result Observation` | Apply only the recorded intent; reconcile an unknown response. |
| `work` → `execution` | durable `InvocationRequest → DispatchReceipt` | Verify the request and ownership before creating an Invocation. |
| `execution` → `work` | `ingest(Observation) → IngestReceipt` | Assess worker and process facts; do not copy a worker's claimed Outcome. |
| `work` → state store | `commitWorkDecision(WorkCommit) → CommitReceipt` | Atomically enforce Work version, active Execution uniqueness, capacity reservation, Command result, and outgoing intents. Observation assessment uses the same port without inventing a Command. |
| `execution` → state store | `appendInvocation(InvocationWrite) → IngestReceipt` | Persist identity before launch; append process, checkpoint, and result evidence without overwriting history. Save the pending Work Observation with its Invocation fact. |
| State store → `work` | `listPendingWork(cursor) → PendingWorkPage` | Return pending Work in stable order with the version seen; `work` rechecks eligibility in the dispatch transaction. |
| State store → `execution` | approved `InvocationRequest` by durable request ID | Start or reconcile the approved request; never infer permission from a visible queue phase. |
| State store → `projection` | `readSnapshot(Query) → StoredSnapshot` | Return Work and pending request records with event sequence and source availability; projection only shapes the queue and other views. |
| `execution` → CodingWorker | `start(WorkerTask) → WorkerStartReceipt` | Run the bounded coding task under the permitted workspace and effects. |
| `execution` → ReviewWorker | `start(WorkerTask) → WorkerStartReceipt` | Review the specified patch or PR head under the given budget. |
| CodingWorker → `execution` | coding `WorkerReport` and evidence | Correlate by Invocation ID; report artifacts and checks, not Work completion. |
| ReviewWorker → `execution` | review `WorkerReport` and evidence | Correlate by Invocation ID and reviewed revision; report findings, not final acceptance. |

`IngestReceipt` reports recorded, duplicate, or rejected input. `DispatchReceipt` reports queued, already queued, or refused because safe ownership cannot be established. Delivery of a durable `InvocationRequest` is at least once; its request ID prevents a second Invocation for the same request. When a Command creates an Execution, the same transaction records the decision, Execution, capacity reservation, and request or outbox intent. A process crash after acceptance therefore leaves work for a dispatcher to resume. `execution` records an Invocation before launch, and a lost start response becomes `ProcessEvidence.unknown` until reconciled. GitHub writes cannot share this transaction and require reconciliation.

The internal dispatch loop reads `PendingWorkPage` from the same store; it does not consume Hermes cards. It may propose a start, but `work` decides eligibility and the store atomically reserves capacity with the new Execution and Invocation request. `work` derives the next Execution ordinal and request ID from the Work history it read; the commit checks that Work version and enforces request ID uniqueness. A conflict makes `work` reread before deciding again. `WorkCommit.cause.dispatch` records this internal decision without impersonating an operator Command. `execution` reads approved Invocation requests from the store. Re-reading after a crash must find the prior request ID before any new start. The queue view itself has no write path: a TUI action uses a typed Command, and moving a row on screen has no lifecycle effect.

The worker ports must not give a worker an unreviewed path to mutate GitHub. The target design routes PR publication and other PR actions through `PrActionIntent`; adapting a private worker that currently performs such writes will require separating that behavior.

## Built-in queue admission

1. The CLI submits a Work item. `work` records its identity, accepted intent, and Command result. The Work appears as `queued` in `work_list` even if the executor is busy.
2. The internal dispatch loop selects pending Work in a stable order and proposes a start using its observed Work version. This selection is only a proposal; a stale list does not confer execution permission.
3. `work` rechecks Work version, eligibility, capacity, and ownership evidence. One state-store transaction creates the Execution, reserves capacity, and saves the Invocation request. A competing dispatch sees the existing request or a version/capacity conflict.
4. `execution` reads that approved request and records the Invocation before launch. If launch outcome is unknown, it inspects the recorded process identity before trying another start. The Work list shows `unknown` when evidence is insufficient, rather than silently returning it to `queued`.

No Hermes card is created in this flow. The TUI's queue row and the CLI's Work detail come from the same state-store snapshot.

## TUI review judgment

The [hermes-setup console](https://github.com/shimpeiws/hermes-setup/pull/386) shows the intended operator interaction: read a Work detail, choose a decision for each finding, preflight, then submit a typed action. This repository will implement its own TUI against the workloop contracts.

1. The TUI queries the Work detail and current Attention. The Projection includes `work_id`, Work version, PR head SHA, finding IDs, evidence references, and any unavailable source. The TUI keeps selection state, but it does not classify Attention.
2. The operator chooses a decision for each finding. The TUI builds a proposed `judge_findings` Command with the displayed Work version and PR head. A read-only preflight shows whether the saved facts allow that Command. Preflight does not reserve the state or guarantee later acceptance.
3. On submit, `work` first checks whether the idempotency key already has a result. It returns that result for a replay. For a new Command, GitHub integration reads the current PR head and records an Observation. `work` compares that head and the Work version with the Command's expected values. If the GitHub read fails, `work` cannot accept a judgment tied to an unverified head.
4. If the head changed, `work` rejects the old-head judgment and returns the current version and reason. The TUI requeries the findings and clears choices tied to the old head. It does not rewrite the expected head and retry the old choices.
5. If accepted, `work` records the decisions against the finding IDs and head SHA. A later `release_judgment` Command may request more work through `execution` when policy and the recorded decisions permit it. The TUI does not create an Execution or Invocation.
6. If the response is lost, the TUI shows the outcome as unknown. It queries the Command result or replays the identical idempotency key and payload. It does not construct a new release request while the first result is unknown.

This flow requires two separate checks: preflight helps the operator understand the action, while the Command handler enforces the preconditions at acceptance time. A head change after acceptance makes that judgment stale for later decisions; it does not erase the recorded choice.

## Explicit retry after a failed Execution

Assume Work `W1` has Execution `E1` with a confirmed terminal failure. A retry is a new attempt under the same Work intent; it is not another Invocation inside `E1`.

1. The CLI or TUI queries `W1`. The Projection shows `E1`'s Outcome, evidence, current Attention, and whether process ownership is still uncertain.
2. The operator submits `retry` with `work_id=W1`, `expected_execution_id=E1`, the displayed Work version, and an idempotency key.
3. `work` checks the failure evidence, retry budget, current Work version, capacity, and execution ownership. Expiry of a claim alone does not prove that an old process has stopped. If it may still write, `work` leaves the state uncertain and does not authorize `E2`.
4. On acceptance, the state store records the Command result, creates `E2`, and reserves its capacity as one decision. `work` then requests the first Invocation through `execution`. The worker result cannot create `E2` on its own.
5. A concurrent retry with another key sees the changed Work version or active `E2` and receives a rejection. Replaying the accepted key returns the recorded `E2` result without creating `E3`.

`resume` has a different contract: it continues an eligible interrupted Execution and may create another Invocation with the same Execution ID. It never turns a confirmed failed `E1` into `E2` implicitly.

## Worker result and PR review

1. `execution` records an Invocation before starting a CodingWorker or ReviewWorker role. Its request names the Execution, input revision, task, and budget. Devin, Claude Code, or another provider can fill the role.
2. The worker returns artifacts and evidence to `execution`. `execution` records the Invocation result and reports an Observation to `work`. A worker saying "done" is evidence to assess, not a Work Outcome.
3. GitHub integration records separate Observations for the PR head, checks, and review events. Each review conclusion is bound to the head SHA it examined. `work` assesses the worker and GitHub Observations together, then records an Outcome or requests another Invocation within the same Execution.
4. A later PR-head Observation makes a prior head's review conclusion stale. A delayed webhook for the old head triggers a current GitHub read; it cannot move the recorded current head backward. `work` cannot mark the new head reviewed from the old result. `projection` shows the current head and the missing review evidence in the next Query.
5. When `work` requests a PR action, it records the outgoing intent before GitHub integration performs the write. Reconciliation records the actual GitHub result as an Observation. An accepted internal decision alone does not prove that the PR changed.

## Decisions needed before a directory layout

- Define the minimal Work and Execution states, transition reasons, and Outcome codes used by these flows.
- Define how a PR action checks its expected head and how reconciliation handles a head change between judgment acceptance and the external write.
- Define the execution-ownership mechanism that prevents an old process from writing after ownership changes. The [Lease name remains undecided](glossary.md#execution-ownership-terminology-under-study).
- Define how long idempotency keys and Command results are retained, and how each adapter keeps the same actor identity across process restarts.

Once these contracts are settled, directories can follow the ownership boundaries: `work`, `execution`, `projection`, shared persistence, and the CLI, TUI, GitHub, and worker adapters. The layout does not decide the contracts.
