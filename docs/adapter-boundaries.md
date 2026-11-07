# Selector and worker adapter boundaries

Status: design contract for the adapter contracts, including the publication decision in ADR-001 below. This is not a claim of implemented real integrations. This document refines [architecture](architecture.md), [module contracts](module-contracts.md), and [lifecycle](lifecycle.md). The Worker role is exercised through the execution supervisor and a configurable `worker_argv` (the fixture worker in slice 0); the Work Selector port and the real GitHub adapter are not yet implemented. See [provenance](provenance.md) for the implementation audit.

## Goal

taisan is a **control plane**, not a work planner and not a coding agent. The lifecycle is derived from explicit invariants and failure scenarios. The core defines its own state model and dispatch rules.

Two independently replaceable ports surround the core:

- **Work Selector** decides *what to propose*: which issues belong together and why. Issue selection and grouping are adapter responsibilities, not core dependencies.
- **Worker** decides *how to perform one authorized invocation*: coding, review, or fix. Claude Code, a worker implementation, or another runner may implement the port. Their internal orchestration is not the taisan authority.

Neither adapter writes core state, reserves capacity, creates Executions, retries Work, or declares a Work completed. Core decisions remain deterministic given persisted facts and policy.

The repository-host integration is also replaceable under [ADR-002](repository-host-contract.md): a conforming adapter satisfying a Work's required capabilities is selected through configuration without Core changes. The GitHub-specific names and authentication examples in ADR-001 describe the first adapter; the neutral target messages and capability rules are normative for other hosts. No production GitLab adapter is implemented by these documents.

## Ownership matrix

| Responsibility | Selector | taisan Core | Worker | GitHub integration |
| --- | --- | --- | --- | --- |
| Discover candidate issues / grouping | Owns | Validates submitted proposal | No | Supplies issue facts |
| Accept Work, identity, dedup, queue | Proposes | Owns | No | No |
| Capacity, resource ownership, launch permission | No | Owns | Obeys | No |
| Plan, code, test, review as requested | No | Requests stage | Owns execution | Observes external facts |
| Retry/resume/cancel and stop proof | No | Owns decisions and supervision | Supports stop/observe | No |
| PR head, checks, reviews | No | Evaluates evidence | May produce artifacts | Owns observation/publication |
| Completion, human Attention | No | Owns | Reports evidence only | Supplies facts |

### Selector contract

A selector receives an explicit repository allowlist, selection policy, a snapshot/cursor of available source facts, and a bounded request (e.g. maximum candidates). It returns **proposals**, not commands:

- stable proposal key and source revision/cursor;
- one or more issue references; goal/intent, constraints, optional verification requirements;
- rationale and provenance; optional grouping key;
- declared required permissions/resources; no hidden expansion beyond allowlist.

Core validates proposal shape, repository scope, source freshness, idempotency and overlap/admission policy. Acceptance creates Work and its first queued Execution through the normal Command path. A selector may return zero proposals or fail without stopping existing Work. A proposal does not reserve a slot or authorize a worker.

**Reference selector:** list eligible GitHub issues from a configured allowlist, propose **one issue per Work**, no semantic grouping, deterministic order and stable proposal keys. This is intentionally simple. More sophisticated grouping belongs in any third-party adapter satisfying the contract.

The [identity/storage contract](identity-storage.md) settles same-scoped-Issue overlap: reject a new Work while unfinished Work owns that source Issue, for both manual and Selector admission. Stable proposal keys replay their original decision; safe terminal release permits a new explicit submission. Manual `submit` remains supported without a selector. Richer grouping and unattended selection remain deferred.

### Worker contract

A Worker Adapter receives a durable, uniquely identified **Invocation request** with:

- execution/invocation/request IDs, role (coding/review/fix), immutable goal and constraints;
- workspace and permitted resources, base/head revision when relevant;
- verification instructions, budget/deadline, evidence and continuation references;
- explicit permissions and credential scope.

It provides:

- `start(request)`: acknowledge a stable request identity; distinguish known not-started, started, and **unknown** outcomes;
- `observe(request_id)`: report process identity/liveness, checkpoint, artifacts, completion/failure and evidence provenance;
- `request_stop(request_id)`: request cancellation; **not** proof that writers stopped;
- optional `resume` capability declared by the adapter; absence means taisan cannot assume an internal session can be resumed.

Worker-internal mechanics stay inside the adapter: how a process is launched, how requests and results are translated to and from the worker's own formats, authentication to the worker runtime, internal session state, and any resume implementation. None of these may surface in Core types, state transitions, or Outcome decisions — the Core contract above is the whole boundary. Capabilities are either required by this contract or explicitly declared optional; an adapter must never silently emulate a capability it does not support, or compensate by altering Command, queue, budget, or Outcome semantics.

The core supervisor persists launch intent, owns resource reservations, and establishes stop proof before authorizing a conflicting writer. Worker exit code, self-reported success, PR URL or a model's summary are evidence, not Work completion. Results must be tied to the Invocation and, for review, the reviewed PR head. Replays must not silently start a second worker.

**Adapter selection and interchange.** The Worker adapter is chosen through installation config — the `worker_argv` entry point today; the exact selection key is an implementation detail — never by changing Core. Any implementation conforming to this contract is interchangeable for *new* Invocation requests. Interchange does not transfer a running Invocation to a different worker, and it requires no migration of adapter-internal state.

The Worker contract test suite below is shared: it runs unchanged against every adapter implementation, covering failure behavior (lost start response, duplicate request delivery, unobservable liveness, stop proof, missing artifacts, stale-head evidence) as well as the happy path. A second, independently implemented adapter that replays scripted results is sufficient to prove interchangeability — it does not need to be a real coding agent.

**Reference worker:** a thin Claude Code CLI process adapter that launches one bounded request in an isolated workspace, captures exit/process identity and output artifacts, and exposes cancellation and observation. No built-in planning pipeline, review framework or proprietary Claude Code-specific state in Core. Other worker adapters are valid implementations; their internal orchestration must not supersede core retry or settlement authority.

## Separation of control surfaces and adapters

The CLI is the required **control interface**, with CLI-backed agent skills planned for v0.1. The existing TUI is deferred; GitHub comments and a future MCP adapter are optional control interfaces. Skills invoke the CLI and interpret its results. None owns execution state or replaces a Selector or Worker. GitHub issue/PR observation and publication is an **integration adapter**. Keep these ports separate even if a single binary hosts them in v0.1.

The repository host adapter implements a single `RepositoryHostPort`; [its contract](repository-host-contract.md) defines mandatory semantics, optional capabilities, binding, and host validation. This replaces the integration boundary rather than wrapping a GitHub-only Core. Worker/Selector exchangeability and host exchangeability are separate acceptance items.

## Required contract tests

1. Selector returns duplicate or reordered proposals: stable identities, no duplicate accepted Work.
2. Selector attempts out-of-allowlist or stale issue: rejected with evidence; existing queue continues.
3. Selector unavailable: manual submit and already queued Work remain usable.
4. Worker start response lost: same request reconciled; no second start.
5. Worker reports success without required artifacts/checks: no successful Work outcome.
6. Worker crashes, escapes process group, or fails stop proof: ownership stays uncertain and replacement is blocked.
7. Review result refers to an old head: cannot settle a newer head.
8. Swap two conforming worker implementations: taisan command, queue, budget and outcome semantics remain unchanged.
9. An adapter attempts to request retry or change a Work state directly: rejected; only Commands through Core may do so.
10. The same Invocation request is delivered twice: reconciled by request identity; no duplicate start or duplicate effect.

## Non-goals

No external Kanban migration or synchronization, no general plugin marketplace or dynamic loader, no automatic semantic grouping in the reference selector, and no model-specific orchestration inside the core. Adapter interchange excludes live handoff of a running Invocation to a different Worker, migration of adapter-internal saved state, and any mechanism for switching workers mid-Invocation.

## ADR-001: Core authorizes publication; GitHub integration performs it

ADR-002 generalizes the integration owner to a repository host adapter without changing these authority, evidence, or recovery rules. The GitHub operation names below are reference-adapter terminology. Target `HostActionIntent` replaces `PrActionIntent`; `open_change_request` replaces PR-only `publish_pr`, and `update_change_request` replaces metadata-only `update_pr`. Review feedback, approval, and requesting changes have separate target effects. The neutral `HostActionIntent` names are defined in [module contracts](module-contracts.md#type-sketch). GitHub App/user token handling is not imposed on other hosts.

Decision for issue #35: retain executor-independent stage postconditions, and route every GitHub mutation through Core-approved, durable `PrActionIntent`s. A stage is a unit of assessed behavior, not permission for its worker to publish. The coordinator is the existing `work` decision logic and `execution` supervisor, not a new workflow engine.

### Alternatives and rationale

| Model | Benefit | Cost and decision |
| --- | --- | --- |
| Worker or stage runner pushes, replies, and marks ready, then reports to Core | A standalone stage can perform all its postconditions in one runtime, including remote execution | Requires GitHub write credentials in the Worker and durable recovery of its private effect ledger. Conflicts with `AllowedEffects.github_write: false` and duplicates the outbox authority. Rejected for managed v0.1 Invocations. |
| Core approves effects; GitHub integration publishes and reports facts | One durable decision path for cancellation, budgets, publication, and recovery; reuses the existing outbox | Requires explicit per-effect receipts and stage assessment in Core. Selected. The real adapter and expanded effect payloads remain implementation work. |

The incident behind #21 requires that a fix reach the PR before it is called fixed. It does not require that the coding process possess the publication credential. The chosen model retains that requirement and the independent reviewer axis without importing the worker repo's entire pipeline.

### Stage responsibility and permission matrix

The persistence owner below names who records facts in the installation store. GitHub remains authoritative for its external effects. Workers may retain internal checkpoints, but those are evidence sources, never a second Work ledger.

| Stage | Decision maker | Executor | Persistence owner | Permissions | Evidence verifier |
| --- | --- | --- | --- | --- | --- |
| Candidate selection | Selector chooses proposals; Core accepts admission | Selector reads allowlisted sources | Core records admission and stable proposal identity | Source read only | Core checks scope, freshness, identity, and overlap policy |
| Queue, dispatch, retry, resume, cancel | Core | `execution` consumes approved requests | Core records decisions/reservations; `execution` records Invocation identity | Store access; supervised process control | Core and supervisor check ownership and request identity |
| Coding or fix preparation | Core selects bounded task, head, and budget | Worker through stage runner | `execution` records reports and evidence refs | Isolated workspace and local git writes; no GitHub mutation credential | Core checks artifact schema, digest, Invocation, base/head, and declared verification |
| Review | Core selects target and reviewer policy; human judges findings when gated | Review Worker or configured review source | `execution` or GitHub integration records head-bound Observations | Read target revision; no publication credential in Review Worker | Core matches reviewed revision, findings, source availability, and judgment |
| Push and PR create/update | Core approves exact artifact and target | GitHub integration | Core records intent; integration records result and observed head | Git transport and PR write scoped to allowlisted repository and managed branch | Core assesses integration's independent reads against artifact and intended PR |
| Review submission, marked replies, thread actions, draft-to-ready | Core approves required effects after their prerequisites | GitHub integration | Core records one intent per effect; integration records receipts | Scoped PR/review/comment mutation only | Core checks action identity, actor, target head, review/comment/thread receipts, and ready state |
| Stop and recovery | Core decides desired action and recovery bounds | Supervisor inspects/stops writers and collects artifacts; integration reconciles its effects | `execution` records stop/recovery evidence; integration records external facts | Process control and bounded artifact read; integration retains its own credential | Supervisor establishes stop proof; Core checks evidence and unresolved effects |
| Stage assessment and final Outcome | Core | Core decision logic; projection presents it | Core records assessment, Outcome, and current Attention evidence | Store writes only; no implicit merge or deployment | Core checks the complete gate against the current PR head |

The stage runner only translates a single approved task into Worker input, supervises its bounded execution, and returns evidence through `execution`. It cannot launch a next stage, reset budgets, retry a Work, publish, or settle an Outcome independently. Infrastructure supplies process identity and stop/observe primitives; it cannot infer success from exit status.

### Worker pipeline disposition

| Existing worker responsibility | Disposition in managed Invocations | Rationale |
| --- | --- | --- |
| Planning within the task, code changes, local commits, declared checks, artifact/checkpoint export | Retain inside the Worker adapter | These implement one bounded Invocation. Check results must cover the exported change. |
| Runtime authentication, session formats, optional continuation, internal model/tool calls | Retain inside the adapter | These are implementation details, subject to the Invocation deadline and permissions. |
| Verification evidence and review findings | Separate production from acceptance | Worker reports facts; Core verifies their identity and decides their significance. |
| Push, PR creation/update, review submission, replies, thread mutation, ready transition | Separate into GitHub integration | A single external-write authority can reconcile partial effects after a crash. Disable the worker's standalone publication path for managed Invocations. |
| Work selection, persistent execution claims, outer review/fix loop, autonomous retry/resume, terminal verdict | Remove from the managed entry point | These duplicate Selector or Core authority. Existing standalone tools outside taisan are not changed by this decision. |

This does not require backward compatibility with a worker's standalone pipeline or introduce a compatibility wrapper. Existing historical evidence is retained. No running Invocation is moved to a different adapter. There is no live integration to cut over in this repository; external systems remain untouched.

### Durable effects and partial success

1. After the producing writer is proven stopped, Core validates the immutable change artifact and verification evidence. The versioned artifact identifies Work, Execution, request/Invocation, task digest, base revision, produced commit, and content digest. It contains or references durable git objects sufficient to reconstruct that exact commit, not just a SHA or a path inside an ephemeral runtime. The integration verifies the exported objects and declared checks against that commit. A local retained workspace is sufficient in single-host v0.1; no remote artifact service is required. Do not discard the producing workspace or exported objects until durable handoff is confirmed and retention policy permits it. Core records each authorized effect before any external write. `push_ref` updates only the branch; `publish_pr` creates only the PR after its referenced push action is verified applied. PR metadata updates do not implicitly push. Each operation has a separate durable action ID and result, so a failed PR creation never overwrites a successful push receipt.
2. Core records the required effect set for the assessed stage before execution, including the selected finding/thread IDs and reply-content digests. This set derives from accepted head-bound judgments and policy, never from whichever receipts the Worker happens to return. An inapplicable effect carries a reason, not an unexplained omission. Push/PR publication precedes replies claiming a fix was published. Each marked reply or thread action has its own stable action ID and exact target. Draft-to-ready is a separate intent after required publication, replies, and head-bound gates. If the intended PR is already ready, a fresh observation of that PR/head satisfies the state postcondition without another mutation. A stage with no required replies records an empty required set with its policy/judgment basis. An unchanged-head stage still assesses any applicable reply and readiness requirements. A thread claim is not proof of a reply or of thread resolution.
3. GitHub integration checks the intended repository, branch, artifact, and expected head before writing. Existing-PR mutations require a fresh head read. A mismatch records `failed` and a new head Observation; Core reassesses rather than retargeting the old action. A `push_ref` authorizes an absent branch or one exact prior SHA and rejects a mismatch. Existing-branch publication also requires a fast-forward descendant, so neither concurrent changes nor divergent history can be overwritten.
4. Every completed effect retains its external receipt. If push succeeds but PR creation, a reply, or ready fails, the successful push is not rolled back or repeated. Core retains the partial record and withholds full stage success. Failure of one effect does not authorize replay of another.
5. An interrupted write stays `unknown`. Reconciliation uses the recorded action ID, payload digest, branch/PR identity, produced SHA, reply marker and returned comment identity, or ready-state observation, as appropriate. A matching effect may be adopted. Multiple matches, insufficient attribution, or unavailable reads retain `unknown`; they never authorize a second blind write. Initial PR creation reconciles the repository, head branch, base, published head, and matching PR existence because it has no prior PR head. The referenced push remains independently applied even if PR creation fails.
6. Durable action identity prevents duplicate local scheduling; it does not make a network write and SQLite commit atomic. GitHub operations without an idempotency primitive require read-back reconciliation. An ambiguous reply or ready transition is not an exactly-once guarantee. Re-reading the head before a comment also cannot make that write atomic with a concurrent head change; record its target and observe again, never use it to settle a newer head.
7. A definitely failed action is retained. A new action requires a new Core decision with fresh evidence and satisfied prerequisites. Unknown effects block terminal release; cancellation does not erase or undo an already applied effect. Budgets and cancellation prohibit new stage effects while outstanding ones are reconciled.

Core assembles stage evidence from the producing Invocation plus these independently observed effects. `head_after` is the produced commit; `pushed_sha` is populated from verified publication evidence, never copied from Worker prose. `review-fixed` requires `pushed_sha == head_after` and that commit equal to, or an ancestor of, the current PR head. Ancestry proves publication only: a newer live head needs its own checks and review. Full `converge` success additionally requires all requested marked replies, thread effects, and the ready-state postcondition. An unchanged head is `nothing-to-fix`, never a published fix; it does not waive outstanding replies or readiness gates.

### Evidence validity and lifecycle decisions

Core classifies each evidence item against the specific stage, preserving rejected evidence and its reason. A valid schema alone is insufficient: digest, Work/Execution/Invocation identity, base and produced revision, exact reviewed head, and all required checks must match the accepted task. Required checks cover the exact change; credentials and unbounded remote text are excluded from exports.

| Evidence class | Worker artifact | Publication evidence | Core decision and transition |
| --- | --- | --- | --- |
| Absent | No usable report/artifact recorded | No receipt or independently observed matching effect | Never success. If a writer may live, remain `running` or `uncertain` and block replacement. After confirmed exit, recover artifacts within the existing bounded `waiting` policy. Reconcile unknown effects without resending; exhaustion cannot release a reservation while an effect remains unresolved. |
| Valid | Schema, digest, identities, revisions, and required verification match | Exact produced commit reached the intended PR; each required side effect has matching evidence | May authorize publication or the next bounded step, not automatically an Outcome. Only complete current-head gates, stop proof, and reconciled effects permit `succeeded`. |
| Invalid | Corrupt content/digest, wrong Invocation, mismatched revision, or missing/failed required verification in a claimed complete result | Unpublished fix claim, wrong PR/branch/SHA, or unverified side-effect claim | Preserve the rejected record; do not publish from it or count it as fixed. A stale review remains history and requests reassessment of the current head within budget. A confirmed bad artifact or unpublished claim records a failure cause; terminal `failed` waits for stop proof and effect reconciliation. If writers/effects may act, use `stopping` or `uncertain` with desired `fail`. |

Missing required evidence is distinct from a check that ran and failed. Recoverable absence uses 3 attempts within 30 minutes by default after confirmed exit; exhaustion records `artifact_recovery_exhausted` with WIP retained once terminal guards hold. Invalid evidence does not gain validity by retrying collection indefinitely. Repairable configuration failures may pause only with a recorded repair prerequisite and safe writers/effects. A new Invocation or retry is always a Core decision and consumes the existing budget rules.

### Permissions, reviewers, and managed bot events

The Selector needs allowlisted issue reads. A Worker needs workspace/local-git writes and its own runtime authentication, supplied by credential reference rather than secret values in argv or evidence. It receives no GitHub mutation credential, inherited git credential helper, SSH agent, or unrestricted network route that bypasses this boundary. GitHub integration alone receives credentials for required repository reads, git publication, PR/comments, and review observations. Permission enforcement and the exact credential mechanism must be implemented and verified in #36's follow-up units; an `AllowedEffects` flag alone is not a sandbox.

Unsupported runtime/reviewer combinations fail configuration validation before dispatch. A supported combination preserves the configured reviewer axis. Bot filtering exempts the installation's managed identities together with its managed PR/head set; it does not admit all bots. Neither an executor change nor bot authorship can silently remove a required review. A declared-risk review gate is deferred as an additional policy; the task's required verification, review, and human gates still apply.

### GitHub authentication belongs to the integration

An installation selects either GitHub App installation authentication or a user-supplied token for GitHub operations. Both implement the same integration contract. Worker runtime authentication is separate: swapping a Worker does not replace GitHub credentials or grant it remote-write access. Review Workers return findings and proposed review text; Core validates the target revision and authorizes a `submit_pr_review` intent before integration posts it.

Configure credential roles for publication, review, and discussion. These roles may resolve to the same credential when policy permits, or to different GitHub identities. Every intent binds its selected configured actor and credential reference before execution; result evidence records the observed actor. Token values are never persisted in an intent or evidence. Reconciliation keeps the original actor identity even if its token is renewed; credential rotation or switching from App to user must not silently reattribute or replay an unresolved action. App token creation/refresh and user-token expiry remain integration concerns. An unavailable credential blocks its effects and surfaces a repair path without changing the Worker contract.

For App or fine-grained token permissions, git push needs Contents write, PR creation/review submission needs Pull requests write, and issue comments need Issues write; PR comments can use Issues or Pull requests write. Permissions and repository policy can still refuse an operation. The adapter validates supported operations before dispatch; exact repository grants and enforcement are follow-up setup work. See GitHub's [App permissions](https://docs.github.com/en/apps/creating-github-apps/registering-a-github-app/choosing-permissions-for-a-github-app), [PR API](https://docs.github.com/en/rest/pulls/pulls), [review API](https://docs.github.com/en/rest/pulls/reviews), and [comment API](https://docs.github.com/en/rest/issues/comments).

A published review is not automatically an independent approval. GitHub prohibits PR authors from approving their own PRs. A policy requiring independent approval therefore selects an eligible reviewer identity or waits for an external reviewer; it cannot fall back silently to a comment or count the author's own analysis as approval. See [GitHub approval rules](https://docs.github.com/en/pull-requests/how-tos/review-pull-requests/approving-a-pull-request-with-required-reviews). Merge and deployment remain outside this decision.

### Impact and implementation evidence

The current Rust `PublishPr`, `UpdatePr`, and `PostPrComment` variants do not implement the separate target `push_ref` or review/thread/ready actions. In the target sketch, `publish_pr` means PR creation only and `update_pr` means metadata only; #36 must migrate their callers and tests together rather than retain an ambiguous combined-publication path.

At base revision `9804fa1`, `crates/workloop-core/src/stage.rs` and `tests/stage_contract.rs` implement/test the publication classifier and static executor/reviewer matrix from #21 / merged PR #31. `ConvergeRecord::classify` checks the produced/pushed head and supplied ancestry fact; it retains reply/thread IDs but does not verify their effects or draft readiness, and it is not the complete stage/Outcome gate. Retain that evidence and the regression tests. The real coordinator must assemble verified receipts before using it; no new compatibility layer or change to that Rust API is implemented by this ADR.

`work.rs::maybe_publish` already records fixture outbox intents, but this is not proof of the real publication protocol. The current artifact recovery path treats missing and corrupt states together; the validity distinctions above require follow-up implementation. Module contract sketches add the required artifact envelope, configured actor, head-bound review/thread/ready actions and effect-result evidence below; concrete Rust payloads, enforcement, and tests remain future work.

#17 must implement real publication/observation and test partial push/PR creation, duplicate delivery, ambiguous reply/ready results, stale-head races, and restart reconciliation against the same port. #21 / PR #31 remain valid evidence for their narrow classifier invariant, not proof that real publication, all converge postconditions, or bot event filtering work. #36 maps these gaps and the two Worker implementations into implementation units and live S1 evidence. This decision authorizes no runtime implementation or E2E execution by itself.

### Design aspects and remaining work

| Aspect | Applicability and rationale | Remaining implementation question or verification |
| --- | --- | --- |
| State transitions | Core alone changes state; stage assessment combines process, artifact, and effect facts to prevent false success | Map the new receipts to existing transition guards and test invalid-artifact/partial-effect paths in #36. |
| Failure semantics | Lost responses and partial publication are independent of worker exit; unknown never means not applied | Prove reconciliation for every real GitHub operation; bounded investigation may surface Attention but cannot erase an unknown effect. |
| Trust boundaries | One GitHub writer avoids competing worker/outbox publication and repeated loops | Select concrete scoped credentials and enforce Worker isolation; verify inherited credential/network denial. |
| Identity invariants | Request/Invocation IDs identify attempts; action IDs and payload digests identify effects; heads identify review targets | Define durable comment markers and receipt schema; selector overlap/group identity remains separate from publication. |
| Information preservation | Preserve partial results, invalid claims, stop evidence, and WIP so restart cannot invent success | Implement evidence schema/digest validation and safe export fixtures; broader retention/export policy remains a spike. |
| Resource bounds | Core budgets bound all stages; artifact recovery and external reconciliation remain distinct | Verify deadlines and stop behavior during partial effects. Multi-host/per-repository capacity and advanced stall policy remain deferred. |
