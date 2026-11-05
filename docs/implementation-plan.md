# Workloop v0.1 implementation plan

Status: proposed plan through v0.1. This document defines capability milestones and acceptance criteria, not calendar commitments. No milestone or Issue creation is implied.

## Objective and constraints

Build an independent Rust control plane from explicit lifecycle invariants and failure scenarios. Work selection and grouping, and execution by coding or review tools, are **replaceable adapters** with deliberately thin reference implementations. The hard engineering investment belongs in core lifecycle, execution ownership, reconciliation, bounded review/fix and human Attention.

Preserve the already documented Work / Execution / Invocation distinction, durable Command/Query, SQLite authority, queue, and evidence-driven recovery. Prefer completing and correcting the existing implementation over replacing it.

## Current repository baseline (code inspection, not test verification)

- Rust Cargo workspace exists, with `workloop-core`, SQLite `workloop-store`, `workloop-exec` POSIX supervisor, `workloop-cli`, `workloop-config`, `workloop-github` fixture, `workloop-fixture-worker`, and `workloop-tui`.
- The CLI already exposes submit/retry/resume/cancel, review judgment, queries, run loop; the TUI implements the same Command/Query contracts including the review-judgment flow.
- `workloop-github/src/lib.rs` exports only `fixture` at inspection time; a real GitHub adapter must be verified/implemented.
- The presence of code does **not** establish passing tests, production readiness, or completion of every lifecycle invariant. Run the full verification gate and inspect coverage before assigning implementation work.
- Existing architecture and lifecycle docs currently describe the queue and execution model; the new selector port and its contracts are not yet fully represented in the implementation.

## Source-of-truth audit: prior workflow implementations → Workloop

Audit lifecycle, dispatch, reconciliation, Attention, and worker integration behavior as **design evidence**. For each behavior record: trigger, authority, persisted state, external facts, failure/recovery path, current known incident, and destination Workloop invariant. Classify as **retain**, **re-model**, or **omit**.

| Existing behavior / incident class | Target invariant | Design treatment |
| --- | --- | --- |
| Kanban card and run-record disagreement; ghost cards | One authoritative Work/Execution/Invocation model | Re-model; no second card database |
| Concurrent manual and scheduler dispatch of same issue | Stable proposal identity, admission/dedup, ownership | Re-model; test races |
| Pending retry / resume events stranded after terminal PR | Terminal reconciliation retires outstanding intents | Re-model |
| Blocked cards that never re-arm or retire | Explicit Attention, gate and transition guards | Re-model |
| Converge pending/review signal wedges | Head-bound evidence, timeouts and bounded decisions | Re-model |
| Worker exits but PR/artifacts survive | Separate process, publication and outcome facts | Retain semantics, redesign records |
| Stop/restart while old writer might still exist | Stop proof before ownership transfer | Retain invariant |
| Platform-specific messaging, general Kanban boards, card decoration | Not core lifecycle | Omit |
| Selector grouping and worker internal pipelines | External responsibilities | Adapter boundary; no migration |

Do not claim all these failures are currently unresolved; check corresponding merged PRs and current code. Convert verified incident reproductions into contract tests rather than copying their fixes line-for-line.

## Design decisions to settle before coding

1. **Selector admission:** proposal identity, dedup across overlapping issue groups, source freshness, allowlist and unattended submission policy. Keep manual submit as fallback.
2. **Worker boundary:** durable invocation request, evidence schema, start/observe/stop semantics, resume capability and stage roles; separate worker report from authoritative outcome.
3. **Review/converge ownership:** Workloop controls review/fix rounds and budgets; workers perform individual stages. Define how an adapter with its own internal workflow reports intermediate evidence without taking over Workloop settlement.
4. **Recovery:** ownership across crashes, uncertain launch, resource reservations, outbox reconciliation, terminal cleanup and human intervention.
5. **Completion contract:** verified PR head, checks, review evidence, writer stop and explicit policy for human merge/approval. Avoid interpreting a green worker exit as completion.

Record each as a short decision and update `architecture.md`, `module-contracts.md` and `lifecycle.md` once agreed. Do not introduce a graph framework by default; derive complexity from the actual transitions.

## Implementation phases through v0.1

Phases are ordered by dependencies and verified capabilities, not by dates. Each phase has an acceptance gate; implementation can proceed incrementally and parallelize independent work.

### Phase 1 — Baseline and contract alignment

- Run `cargo fmt --check`, `cargo build`, `cargo test`, `cargo clippy -- -D warnings`; record results and CI gaps.
- Inventory implemented versus specified Slice 0 behavior, including stop-proof and crash tests.
- Audit representative loop incidents against the table above; resolve selector, worker and outcome contracts.
- Deliverable: implementation gap matrix, revised contracts and one agreed end-to-end scenario.

**Gate:** ownership is unambiguous: who proposes Work, who accepts it, who launches Invocations, who retries and who settles Outcomes.

### Phase 2 — Core reliability and reference adapters

- Introduce a Selector port and a minimal GitHub single-issue selector; keep admission behind Core.
- Introduce or adapt a thin Worker port and a Claude Code CLI reference adapter using the existing supervisor.
- Add a second, independently implemented contract-verification Worker adapter (scripted/deterministic) and run the shared Worker contract suite — including the swap-invariance check — against both adapters, selected via config with no Core change.
- Close high-risk Slice 0 invariant gaps and add CI; test duplicate proposals, lost launch responses and uncertain stops.
- Deliverable: a selected Issue becomes queued Work and a bounded real Invocation with durable evidence.

**Gate:** no duplicate launch or conflicting writer under injected failure; existing Work continues if selector is unavailable; Command, queue, budget and Outcome semantics are identical under either conforming Worker adapter.

### Phase 3 — GitHub integration and bounded loop

- Implement or verify real GitHub PR publication and Observations, head-bound checks and reviews.
- Drive coding → PR → review → bounded fix → reassessment → completion or human Attention.
- Verify restart/reconciliation, stale head, unavailable adapters, missing artifacts, budget exhaustion and cancellation.
- Deliverable: repeatable, evidence-backed end-to-end execution on an isolated test repository.

**Gate:** Workloop owns all decisions; neither selector nor worker can silently retry, settle or bypass state guards.

### Phase 4 — v0.1 validation and distribution

- Document installation, quickstart, sample config, architecture, permissions, failure modes and both reference adapters.
- Verify a clean installation, contract tests, failure injection and reproducible end-to-end walkthrough.
- Record known limitations and explicitly distinguish implemented capabilities from design proposals.
- Deliverable: a usable, self-hosted v0.1 with a built-in queue, CLI, replaceable selector and worker, real GitHub integration and bounded recovery/review behavior.

**v0.1 acceptance:** one Work can be submitted manually or proposed by the reference Selector, accepted, executed by the reference Worker, observed, reviewed and either completed or surfaced as actionable Attention without an external Kanban system. Replacing either adapter must preserve Core command, queue and outcome semantics; neither adapter owns independent execution truth. For the Worker port this is proven by two conforming implementations — the reference real Worker and a second contract-verification adapter — passing the shared contract suite under config-only selection; it does not require multiple production-grade worker providers, and it does not transfer a running Invocation between workers.

## Scope discipline

**Must have:** independent core, explicit selector and worker contracts, minimal reference implementations plus one contract-verification Worker adapter with swap verification, queue/ownership/recovery correctness, observable outcomes, repeatable demo.

**Can defer:** sophisticated grouping, additional worker implementation-specific adapters beyond the contract pair, multiple production-grade worker providers, hosted service, automatic issue prioritization, advanced TUI polish, complex audit automation, cross-host scheduling, broad enterprise integrations.

**Do not do:** port external Kanban system wholesale, make a selector implementation or a worker implementation mandatory, allow selectors/workers to mutate core state directly, hide unknown state as success, or change existing GitHub Issues/milestones during this planning phase.

## Verification matrix

| Layer | Evidence required |
| --- | --- |
| Core | State-transition and idempotency tests, queue/capacity/resource invariants |
| Selector | Stable proposal identity, allowlist, duplicate and stale-source tests |
| Worker | Start/observe/stop contract; lost response, duplicate request, crash, uncertain process tests; shared suite passes on both adapters; swap preserves semantics |
| GitHub | PR-head binding, delayed/duplicate webhook, outbox and reconciliation tests |
| Loop | Bounded review/fix, terminal intent retirement, human Attention |
| v0.1 | Clean setup, isolated repository, reproducible E2E and documented limitations |

## Open risks

Phase scope and effort remain provisional until the code audit is complete. The existing Rust code may require significant correctness work. Real GitHub credentials/permissions and Claude Code invocation behavior must be tested in isolation. The adapter boundary must not pretend every worker can resume or publish a PR. Revisit scope at each acceptance gate.
