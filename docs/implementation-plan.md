# Workloop re-modeling and implementation plan

Status: proposed plan, 2026-11-03. Target: credible OSS demonstration for the 2026-12-08 webinar, with slides frozen by 2026-12-01. No milestone or Issue creation is implied.

## Objective and constraints

Re-model the **loop management** proven in Hermes Setup as an independent Rust control plane. Reuse its operational lessons, not its overgrown Kanban state machine. Work selection/grouping (currently devflow) and execution (Claude Code, coding-worker, others) are **replaceable adapters**, each with a deliberately thin reference implementation. The hard engineering investment belongs in core lifecycle, execution ownership, reconciliation, bounded review/fix and human Attention.

Preserve the already documented Work / Execution / Invocation distinction, durable Command/Query, SQLite authority, queue, and evidence-driven recovery. Prefer completing and correcting the existing implementation over replacing it.

## Current repository baseline (code inspection, not test verification)

- Rust Cargo workspace exists, with `workloop-core`, SQLite `workloop-store`, `workloop-exec` POSIX supervisor, `workloop-cli`, `workloop-github` fixture, and `workloop-tui` source.
- The CLI already exposes submit/retry/resume/cancel, review judgment, queries, run loop.
- `workloop-github/src/lib.rs` exports only `fixture` at inspection time; a real GitHub adapter must be verified/implemented.
- The presence of code does **not** establish passing tests, production readiness, or completion of every lifecycle invariant. Run the full verification gate and inspect coverage before assigning implementation work.
- Existing architecture and lifecycle docs currently describe the queue and execution model; the new selector port and its contracts are not yet fully represented in the implementation.

## Source-of-truth audit: Hermes Setup → Workloop

Audit `hermes-setup`'s `devin-milestone.py` loop/lane, dispatch, reconciliation, attention, and `coding-worker` integration as **behavioral evidence**. For each behavior record: trigger, authority, persisted state, external facts, failure/recovery path, current known incident, and destination Workloop invariant. Classify as **retain**, **re-model**, or **omit**.

| Existing behavior / incident class | Target invariant | Design treatment |
| --- | --- | --- |
| Kanban card and run-record disagreement; ghost cards | One authoritative Work/Execution/Invocation model | Re-model; no second card database |
| Concurrent manual and scheduler dispatch of same issue | Stable proposal identity, admission/dedup, ownership | Re-model; test races |
| Pending retry / resume events stranded after terminal PR | Terminal reconciliation retires outstanding intents | Re-model |
| Blocked cards that never re-arm or retire | Explicit Attention, gate and transition guards | Re-model |
| Converge pending/review signal wedges | Head-bound evidence, timeouts and bounded decisions | Re-model |
| Worker exits but PR/artifacts survive | Separate process, publication and outcome facts | Retain semantics, redesign records |
| Stop/restart while old writer might still exist | Stop proof before ownership transfer | Retain invariant |
| Hermes-specific Telegram, general Kanban boards, card decoration | Not core lifecycle | Omit |
| devflow issue grouping and coding-worker internal pipelines | External responsibilities | Adapter boundary; no migration |

Do not claim all these failures are currently unresolved; check corresponding merged PRs and current code. Convert verified incident reproductions into contract tests rather than copying their fixes line-for-line.

## Design decisions to settle before coding

1. **Selector admission:** proposal identity, dedup across overlapping issue groups, source freshness, allowlist and unattended submission policy. Keep manual submit as fallback.
2. **Worker boundary:** durable invocation request, evidence schema, start/observe/stop semantics, resume capability and stage roles; separate worker report from authoritative outcome.
3. **Review/converge ownership:** Workloop controls review/fix rounds and budgets; workers perform individual stages. Define how an adapter with its own internal workflow reports intermediate evidence without taking over Workloop settlement.
4. **Recovery:** ownership across crashes, uncertain launch, resource reservations, outbox reconciliation, terminal cleanup and human intervention.
5. **Completion contract:** verified PR head, checks, review evidence, writer stop and explicit policy for human merge/approval. Avoid interpreting a green worker exit as completion.

Record each as a short decision and update `architecture.md`, `module-contracts.md` and `lifecycle.md` once agreed. Do not introduce a graph framework by default; derive complexity from the actual transitions.

## Implementation sequence and acceptance gates

### Phase A — 2026-11-03 to 11-09: baseline and contract freeze

- Run `cargo fmt --check`, `cargo build`, `cargo test`, `cargo clippy -- -D warnings`; record actual results and CI gaps.
- Inventory implemented vs specified Slice 0 behavior, including stop-proof and crash tests.
- Audit Hermes loop incidents against the table above; resolve adapter and outcome contracts.
- Deliverable: executable gap matrix, revised contracts and an agreed end-to-end scenario.

**Gate:** no ambiguity about which component can create Work, launch a worker, retry, or settle an outcome.

### Phase B — 2026-11-10 to 11-16: adapter skeletons and core correctness

- Introduce a Selector port and trivial GitHub single-issue selector; keep submission behind Core admission.
- Introduce or adapt a thin Worker port; implement the Claude Code CLI reference runner through the existing supervisor.
- Close the highest-risk Slice 0 invariants and add CI; test duplicate proposal, lost launch response and stop uncertainty.
- Deliverable: one selected Issue becomes queued Work, then a real bounded Invocation, with evidence and safe stop.

**Gate:** no duplicate launch or conflicting writer even under injected failure.

### Phase C — 2026-11-17 to 11-23: real GitHub and loop E2E

- Implement/verify GitHub PR publication and observations, head-bound checks/reviews.
- Drive coding → PR → review → bounded fix → reassessment → completion or human Attention.
- Verify restart/reconciliation, stale head, unavailable adapter, failed artifact, budget exhaustion and cancellation.
- Deliverable: repeatable, recorded end-to-end demo on an isolated test repository.

**Go/no-go on Nov 23:** if real E2E is unreliable, demo the proven core + fixture workflow and show real GitHub integration as explicitly incomplete. Do not weaken invariants to hit the talk date.

### Phase D — 2026-11-24 to 11-30: OSS readiness and talk

- README, quickstart, sample config, architecture/contract diagrams, security and permission model, failure-mode guide, reference adapter documentation.
- Verify clean installation and demo reproduction; record backup walkthrough.
- Prepare 30–35 minute presentation anchored in operational failures and the re-modeled loop.
- Deliverable: demonstrable tagged/reproducible state and finished slides by **Dec 1**.

### Phase E — 2026-12-01 to 12-08: freeze and rehearsal

Only critical fixes after slide freeze. Rehearse with recorded fallback; validate timing and Q&A.

## Scope discipline

**Must have:** independent core, explicit selector and worker contracts, minimal reference implementations, queue/ownership/recovery correctness, observable outcomes, repeatable demo.

**Can defer:** sophisticated grouping, coding-worker-specific adapter, multiple worker providers, hosted service, automatic issue prioritization, advanced TUI polish, complex Auditor automation, cross-host scheduling, broad enterprise integrations.

**Do not do:** port Hermes Kanban wholesale, make devflow or coding-worker mandatory, allow selectors/workers to mutate core state directly, hide unknown state as success, or change existing GitHub Issues/milestones during this planning phase.

## Verification matrix

| Layer | Evidence required |
| --- | --- |
| Core | State-transition and idempotency tests, queue/capacity/resource invariants |
| Selector | Stable proposal identity, allowlist, duplicate and stale-source tests |
| Worker | Start/observe/stop contract; lost response, crash, uncertain process tests |
| GitHub | PR-head binding, delayed/duplicate webhook, outbox and reconciliation tests |
| Loop | Bounded review/fix, terminal intent retirement, human Attention |
| Demo | Clean setup, isolated repository, reproducible E2E or documented fixture fallback |

## Open risks

The proposed timeline is a target, not an estimate backed by a complete code audit. The existing Rust code may require significant correctness work. Real GitHub credentials/permissions and Claude Code invocation behavior must be tested in isolation. The adapter boundary must not pretend every worker can resume or publish a PR. Revisit scope at each acceptance gate.
