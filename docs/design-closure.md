# v0.1 design closure and implementation start gate

Status: owner-approved design closure from the #56 review. This document integrates the v0.1 design corpus, records the requirement-disposition manifest, and proposes the implementation backlog. **It authorizes nothing**: backlog publication, runtime implementation, and live tests each require separate explicit owner authorization. It claims no runtime or live completion.

## Owner decisions

| Decision | Choice | Notes |
| --- | --- | --- |
| R48 disposition | Re-modelled: the path-filtered gate mechanism is host-side CI configuration, outside taisan's control surface; the retained obligations (only declared named checks participate; superseding heads cancel stale assessment; check evidence is bounded and three-valued) are satisfied by existing clauses | Not deferred — there is no taisan mechanism to defer |
| Code-review lane default | `on` | Resolves the setup decision left open in [evidence gates](evidence-gates.md) |
| Backlog proposal | U0–U9 from the [implementation plan](implementation-plan.md#dependent-delivery-units), unchanged | With explicit authorization boundaries below |
| Open items | `recovery.retry.backoff_seconds` jitter policy, `run --phase` filter, and the `waiting` reason name are non-blocking, resolved during implementation in the owning documents | Recorded, not silently chosen here |

## Consistency review: one owner per decision

The lifecycle reads left to right; each cell names the single authoritative document for that decision. A later document cites an earlier decision but never redefines it.

| Lifecycle concern | Owning document | Key schema/state |
| --- | --- | --- |
| Identity, admission, storage, cutover | [identity-storage.md](identity-storage.md) (#50) | `works`/`executions`/`commands`/`resource_reservations`/`event_log`; `user_version=1`; archive/new-store |
| Recovery limits, failure classes, progress | [recovery-policy.md](recovery-policy.md) (#50-approved) | Configurable defaults; equivalent-failure classifier; Work-wide budgets |
| Evidence validity, assessment, review, judgment, accounting | [evidence-gates.md](evidence-gates.md) + [evidence-requirements.md](evidence-requirements.md) (#51) | Immutable rounds/occurrences; typed receipts; `assessment_subjects`; recovery ledger |
| Worker contract, artifact handoff, isolation | [worker-protocol.md](worker-protocol.md) + [worker-requirements.md](worker-requirements.md) (#52) | `WorkerTask`/`WorkerReport`; `artifact_handoffs`; `refs/taisan/artifacts/*`; sandboxed bundle |
| Installation config, credentials, roles, setup | [installation-config.md](installation-config.md) + [installation-requirements.md](installation-requirements.md) (#53) | `config.toml`/`schema_version`; `env:`/`file:`/`command:` providers; three roles; `init`/`check` |
| Retention, cleanup, CAS, history | [retention-policy.md](retention-policy.md) + [retention-requirements.md](retention-requirements.md) (#54) | `body_catalog`; `bodies/` CAS; two-phase reclaim; protective pins |
| Loop, CLI contract, recovery scheduling, skill | [execution-loop.md](execution-loop.md) + [loop-requirements.md](loop-requirements.md) (#55) | `run`/`--once`; always-JSON envelope; exit 0–6; `recovery_intents`; SKILL.md |
| Host adapter semantics | [repository-host-contract.md](repository-host-contract.md) + [adapter-boundaries.md](adapter-boundaries.md) (#46) | Host-neutral identities; `HostActionIntent`; supported×allowed |
| Module wiring | [module-contracts.md](module-contracts.md) (#46) | Ports; store constraint list; target tables |
| Acceptance scenario | [v0.1.md](v0.1.md) (#47) | S1; lifecycle acceptance rows |
| Deferral scope | [post-v0.1-milestones.md](post-v0.1-milestones.md) | Seven approved deferrals |

Cross-cutting invariants hold identically in every document: Core decides, the store enforces atomically, the CLI loop drives I/O/timing, adapters execute effects, Workers produce local evidence; `unknown`/`unavailable` are first-class and never coerce to success or emptiness; configuration cannot weaken ownership/approval/evidence/dedup guards.

## Requirement-disposition manifest

The extraction inventory carries every audited requirement to one of four dispositions. The [coverage checker](../scripts/check-extraction-coverage.py) regenerates this accounting and is part of the closure evidence.

| Disposition | Meaning | Where recorded |
| --- | --- | --- |
| retain | The requirement's obligation maps to a normative clause in a target document | The four `*-requirements.md` mapping docs + `identity-storage.md`'s extracted-obligations table |
| re-model | The source mechanism is omitted; the retained safety obligation maps to a target mechanism | Each requirement's mapping entry names the substitute |
| omit | The mechanism is excluded and nothing retained maps further | This document's omissions registry |
| defer | Owner-approved deferral to a named milestone or later unit | [post-v0.1-milestones.md](post-v0.1-milestones.md) and the deferral registry below |

Every target in `docs/extraction-coverage.json` is `specified` — the checker reports zero unmapped #50–#56 targets at closure. `status` fields (`missing`, `implemented+unverified`, `deferred`, `implemented+verified`) describe implementation status, not design coverage; no status is upgraded by this document.

### Recorded omissions (owner-approved, no deferral)

External card/queue databases; a scheduler service/cron; console/projection binaries; notice ledgers and push delivery; webhook ingress; tunnels; installer convergence; remote operator control; parked-selector registries; card claims/heartbeats; remote admission/SSH/container infrastructure; pinned runtime deployment; embedded remote-publication pipelines; manifest self-repair sessions; bundle-as-durable-artifact; in-place database compaction. Each mechanism's retained obligations are recorded in the owning issue's `*-requirements.md` entries.

### Recorded deferrals (owner-approved)

| Deferral | Destination |
| --- | --- |
| Planner machinery (`worker-B004`, `worker-B008`, `TGT-B07`) and converge repair planning (`worker-B042`) | [Planning and bounded replanning](post-v0.1-milestones.md#planning-and-bounded-replanning) |
| Evaluation machinery (`worker-B061`, `worker-B065`, `worker-B067`, `worker-B129`) | [Evaluation and independent adjudication](post-v0.1-milestones.md#evaluation-and-independent-adjudication) |
| Automatic PR-conflict repair; dedicated external completed-fix adoption | Planning milestone, follow-up capabilities |
| TUI delivery; MCP; GitHub comment command adapter; Projects projection; second control interface | v0.1 release scope ([agent-interface](agent-interface.md), [v0.1](v0.1.md)) |
| Cross-task doctor diagnosis; unattended scheduling; automatic merge/deploy; production GitLab; rich Selector grouping/triage | Scope exclusions recorded across the corpus; no milestone promises |

Per-requirement deferred-status rows (R02, R06, R16, R19, R49, R230, R268, R272, R274, R327, R331 and planner-linked R286/R288/R307) cite their destination in their mapping entries.

## Acceptance plan

R01–R38 plus the S1/general/agent-interface criteria resolve to the contracts below. Evidence classification is uniform: **absent** holds; **valid** satisfies only the exact schema/identity/revision/criterion; **invalid** (malformed/stale/mismatched) is retained with reason and never counted; fixtures prove contract behavior, never live acceptance.

| Acceptance area | Governing contract | Unit | Planned proof |
| --- | --- | --- | --- |
| R01–R38 evidence/invariant/admission obligations | [evidence-requirements](evidence-requirements.md) + [identity-storage](identity-storage.md#extracted-identity-and-admission-obligations) | U1 | Contract tests: same-head growth, missing/wrong facts, invalid artifacts, rename/collision, cutover, budget guards |
| S1 live scenario | [v0.1 S1](v0.1.md#representative-live-acceptance-scenario) + [S1 mapping](implementation-plan.md#s1-setup-and-expected-path) | U8 | Isolated repo run with exact revisions/identities/receipts; separate success/Attention/recovery variants |
| Lifecycle acceptance rows | [implementation-plan acceptance](implementation-plan.md#remaining-lifecycle-acceptance-rows) | U1/U5/U7 | Fault injection per row; duplicate/concurrent/stale/lost/unknown |
| Agent-interface matrix | [agent-interface](agent-interface.md) + [execution-loop CLI contract](execution-loop.md#cli-contract) | U7 | Every row through direct CLI and installed skill; malformed/config/store errors distinguishable |
| Worker interchangeability | [worker-protocol](worker-protocol.md) shared suite | U4 | Same suite against real and deterministic Workers; config-only swap |
| Host interchangeability | [repository-host-contract](repository-host-contract.md) + U9 | U9 | Independent host fixtures + narrow live GitLab contract spike |
| Per-Work bounded recovery | [execution-loop recovery](execution-loop.md#bounded-automatic-recovery) | U5 | LP-19–LP-27 fault injection; release requires these, not a happy path |

## Recovery acceptance matrix

Release evidence requires every branch — reconciliation, recollection, safe stop, and re-execution — not a green path plus manual retry.

| Branch | Scenario | Contract proof (fixture) | Live evidence (later, authorized) |
| --- | --- | --- | --- |
| Reconciliation | Lost publication response; partial effect | LP-15 (F6/F7) | U8 isolated-repo lost-response variant |
| Recollection | Missing/corrupt artifact | LP-14 (F5) | U4 shared suite against both Workers |
| Safe stop | Uncertain stop; orphan/escaped writer | LP-12, LP-13 (F3/F4) | U4 supervisor fault injection on the real runtime |
| Re-execution | Deadline/budget exhaustion; equivalent failure; terminal-intent retirement | LP-20–LP-27 (F11–F15) | U8 recovery variant with persisted intents across restart |
| Escalation | No-progress; prolonged uncertainty; equivalent-failure limit | LP-19, LP-22, LP-26 | U8 Attention visibility check |

No branch may be substituted by a transition to `failed` plus a manual retry command.

## Implementation start gate

### Proposed backlog

U0–U9 from the [implementation plan](implementation-plan.md#dependent-delivery-units), unchanged, reusing #28 (U0 CI), #24 (U1 finding coverage), and #17 (U3 host adapter). Critical path: U0 → U1 → U2 → U3/U4 → U5 → U7 → U8; U6 and U9 hold their own acceptance gates.

### Authorization boundaries

| Action | Required authorization |
| --- | --- |
| Publish the backlog as tracker issues | Explicit owner approval of this closure's proposed scopes |
| Start implementation of any unit | Explicit per-unit owner approval, against the approved contracts |
| Run live host/E2E evidence | Explicit owner approval per scenario, in isolated installation/repository scope |

Design closure — including this document — authorizes none of the three.

### Gate blocking conditions

The gate stays blocked while any retained requirement is unmapped (currently zero — the checker reports all targets `specified`) or any recovery policy/schema decision is unresolved. The code-review lane default is resolved (`on`); the recorded open items below are non-blocking refinements with named owning documents.

## Six design aspects

| Aspect | Closure assessment |
| --- | --- |
| State transitions | Each transition has exactly one deciding document and one enforcing mechanism (store transaction); loop phases only supply facts |
| Failure semantics | Every failure row across the corpus names detection evidence, durable transition, bounds, and escalation; `unknown`/`unavailable` never coerce |
| Trust boundaries | Core/adapters/Worker/skill separation is uniform; secrets resolve only at the adapter boundary; Workers hold no host mutation credentials |
| Identity invariants | Work/Execution/Invocation/request/intent/Command/body identities are durable and digest-bound; requeue keeps Execution identity, retry mints new; retirement is versioned |
| Information preservation | Metadata never reclaimed; bodies two-phase with protective pins; invalid evidence retained with reason; retirement reasons recorded |
| Resource bounds | Every layer composes bounded budgets — per-Invocation, per-tick, per-pass, Work-wide, retention caps — none replenishes another |

## Open items (non-blocking)

| Item | Owning document | Resolution point |
| --- | --- | --- |
| `recovery.retry.backoff_seconds` jitter policy | [installation-config](installation-config.md) | U2 config reference completion |
| `run --phase` filter | [execution-loop](execution-loop.md) | U5; default remains no |
| `waiting` reason name for export/verification progress | [worker-protocol](worker-protocol.md)/[execution-loop](execution-loop.md) | U4/U5 vocabulary finalization |

## Implementation-plan mapping

This document is the #56 deliverable: it closes the design phase, feeds the [implementation plan](implementation-plan.md)'s human-review section, and blocks nothing that is already specified. Implementation starts only after the authorization boundaries above are met.
