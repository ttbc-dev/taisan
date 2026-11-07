# taisan v0.1 implementation plan

Status: implementation planning for #36, reviewed with the owner on 2026-11-07. This issue changes documentation only. It does not authorize runtime implementation, operational setup, live E2E execution, or creation of follow-up issues. Implementation requires the follow-up authorization gate below.

## Scope and reviewed decisions

The target is one self-hosted installation with SQLite authority, a built-in queue, direct CLI operation, CLI-backed agent skills, replaceable Worker and repository-host adapters, and a real GitHub review/fix loop. S1 uses manual submission of one issue. The reference Selector is a separate alternate-admission check, not unattended scheduling or a prerequisite for S1. Preserve Work, Execution, Invocation, command replay, ownership, and evidence history.

The owner reviewed responsibilities and scope in the #36 working session:

- Publication ownership was already agreed: Core authorizes durable effects, the repository-host adapter executes remote writes, and Workers produce local changes or review evidence. Reuse #35 / PR #46, rather than reopen that choice or put publication back into Workers.
- Start GitHub delivery with user-supplied tokens for ease of setup. Keep user-token support when App authentication is added. App authentication remains a supported target design, not a prerequisite for the first token-based live slice.
- The CLI execution loop drives timed checks, observation collection, reconciliation, and process supervision. Core evaluates supplied facts against durable state and enforces transition, budget, stop, and release guards. Supervisor and host adapter perform and observe effects. Cross-task inconsistency and no-progress diagnosis belong to a later doctor-like operational layer, not a new Core monitoring service.
- Configurable retention is required. Separate durable history/evidence references from artifact and git-object bodies. Plan configurable age and storage bounds, safe reclamation, and observable cleanup results; do not assume unlimited local retention. Unresolved effects, active writers, paused WIP, and recovery dependencies must protect required bodies from deletion.
- CLI and agent skills are required under #47 / PR #48. TUI delivery is deferred; existing TUI code/tests stay. Its deferral cannot hide the shared finding-coverage defect in #24. MCP is optional future work.

These decisions settle the boundaries above, not the exact retention defaults, schemas, or implementation schedule. Review of this document's final matrices and authorization of follow-up work remain separate checkpoints.

## Detailed identity/storage design

[Identity, storage and admission](identity-storage.md) refines U1 under #50. The owner chose maintenance-only cutover with the old database kept for read-only reference and a fresh new-format store/new Work; executable legacy Work and old-key replay are not migrated. New-format history and replay guarantees remain. Unfinished source-Issue overlap is rejected for manual and Selector submissions. Core requires an authoritative read immediately before dispatch, performed by the host adapter and driven by the CLI loop. The owner accepts that read as the source-version boundary; detected change/unavailability holds Work for renewed approval of a new specification revision under the same Work identity, without changing host/actor/permission binding or resetting budgets. A future configured auto mode may regenerate invariants, but is not required for v0.1; #51/#55 design its protocol before activation.

## Detailed evidence and gate design

[Evidence and review gates](evidence-gates.md) refines U1/U5 under #51. The owner selected mechanical Work verification plus independently optional invariant conformance (default on) and code review, with additional GitHub CI only when explicitly configured. Conformance runs before publication; code review runs after it. Both enabled assessment loops are bounded and surface unresolved work as Attention. No native Approve setting, repository-rule import or draft/Request-changes merge barrier is added. Immutable review rounds/exact-set judgments, typed receipts and proof closure replace aggregate-check/review assumptions in the target design; current runtime and #24 remain unchanged.

## Detailed Worker protocol design

[Worker protocol, artifact handoff and isolation](worker-protocol.md) refines U2/U4 under #52. The owner selected Claude Code CLI headless behind a taisan-owned shim as the reference Worker, `resume: unsupported` in v0.1, supervisor-run mechanical verification on the handed-off commit, a handoff repository ref as the durable git-object acknowledgement, and bubblewrap isolation with a mandatory model-API egress proxy. All Worker-owned inventory targets are mapped in [worker requirements](worker-requirements.md). No runtime, sandbox or live evidence is claimed.

## Revisions and evidence baseline

Runtime inspection and checks use main `dc63df1edbceba5d2c863fcd840ca34d3b962e10`. The plan was prepared atop PR #46 at `28d09d4f914373d18a6805141c2fbf4f8ddf5186`, which includes that runtime revision and changes documentation only. During planning, #46 merged as `94061eeb5b5812c8597e71ac0950afdb3877dd88`; this branch now targets that main revision. `git diff dc63df1 28d09d4 -- crates Cargo.toml Cargo.lock scripts` is empty. The checks therefore cover the same runtime tree, not any future adapter implementation. Recheck runtime evidence after subsequent code changes.

| Required check | Observed result on 2026-11-07 | Evidence scope |
| --- | --- | --- |
| `cargo fmt --check` | PASS | Workspace formatting only |
| `cargo build --workspace -j 1` | PASS | Builds CLI, fixture worker, and existing workspace crates |
| `cargo test --workspace -j 1 -- --test-threads=1` | PASS outside the command sandbox | Existing contracts and fixtures only; no real Worker, GitHub, skills, or live S1 |
| `cargo clippy --workspace --all-targets -j 1 -- -D warnings` | PASS | Current static checks only |

Build before tests: process contracts spawn `target/debug/workloop-fixture-worker`; `cargo test` alone does not build that executable. Single-job and single-test-thread execution limit local resource use. The first sandboxed test run failed, including process-spawn `Operation not permitted`; that run is invalid evidence of a passing baseline. The unrestricted rerun passed. No runtime fix is claimed.

Local execution logs were retained by this run, but temporary local paths are not portable release evidence. Future CI evidence must retain revision, command, exit status, and output. #28 remains the existing CI delivery item. None of the local results proves deployed behavior or live integration acceptance.

### Evidence classification

| Evidence | Class and supported claim | Unsupported claim / remaining gap |
| --- | --- | --- |
| Current source and tests at the runtime revision above, with passing baseline | Valid for the specific assertions named below | File presence or a green workspace does not prove every requirement |
| Owner-controlled source audit | Valid as recorded design evidence at its inspected source revisions | Source-system live behavior was not rerun; historical issue states are stale for current status |
| #21 / merged PR #31 and `stage_contract.rs` | Valid for publication classification and static executor/reviewer validation | Invalid as proof of complete stage receipts, ready transition, live publication, or reviewer event routing |
| #23 / merged PR #29 and TUI tests | Valid for finding-number/selection regression | Invalid for same-head finding growth at Command acceptance (#24), CLI skills, or live review |
| Merged PR #46 at the pinned head | Valid as the merged design contract and dependency context; publication responsibility approved by owner | Documentation only, not implemented adapters or live portability proof |
| Checks/reviews for an earlier head, another Invocation or another repository | Invalid for the current stage/Outcome; preserve with rejection reason | Cannot satisfy current-head completion |
| Real Worker contract pair, real GitHub writes/reconciliation, clean skill install, S1 | Absent | Planned gates, never marked complete by fixture results |

## Existing work and dependency disposition

Statuses were queried from GitHub on 2026-11-07. Re-query before implementation. This issue creates no duplicate issues and changes no existing issue body or state.

| Item | Current state | Reuse, proposed revision, or exclusion |
| --- | --- | --- |
| #32 / PR #37 | Closed / merged | Reuse corrected provenance and status evidence |
| #33 / PR #38, #41 | Closed / merged | Reuse the retained/re-modeled audit; replace stale tracker-status assumptions with this snapshot |
| #34 / PR #39, #42 | Closed / merged | Reuse S1 and the separate two-Worker interchangeability gate |
| #35 / PR #46 | Closed / merged (`94061ee`), rechecked before push | Required design dependency now landed. This plan originally stacked on #46 and now targets main. Neutral host contracts require coordinated migration, not aliases around GitHub types |
| #17 | Open | Reuse for real GitHub integration. Propose aligning its old combined-publication wording with #46's separate push/create receipts and common host suite; do not edit its body here |
| #21 / PR #31 | Closed / merged (`793e357`) | Reuse classifier and matrix tests. Missing end-to-end stage assessment is follow-up work, not a duplicate of this closed design issue |
| #23 / PR #29 | Closed / merged (`d98521b`) | Reuse TUI regression; no reopening or duplicate repair |
| #24 | Open | Propose extending its acceptance-time coverage to shared Core/CLI and same-head findings. Keep TUI-specific repair deferred; the shared guard remains required |
| #28 | Open | Reuse for CI with explicit fixture-worker prebuild; no duplicate CI issue |
| #47 / PR #48 | Closed / merged (`dc63df1`) | Reuse required CLI/skills and deferred TUI scope |

Merged PR #46 additionally specifies host-neutral identities, immutable diff snapshots, per-operation/actor capabilities, typed receipts, and a GitLab semantic pressure test. Plan implementation against its merged contract; those contracts have no runtime implementation yet. GitHub stays the v0.1 production target. A narrow live GitLab spike is required before a cross-host exchangeability claim, not before GitHub S1. Full production GitLab, cross-host Work migration, and broad feature parity remain deferred.

## Retained audit requirement inventory

Each row corresponds in audit order to one retained or re-modeled classification in the owner-controlled [source workflow audit](source-workflow-audit.md). Audit requirements are assessed as a whole: partial code is `implemented+unverified` when the remaining contract is unproven; wholly absent behavior is `missing`. `implemented+verified` applies only to the stated narrow assertion on the runtime revision. `deferred` is an explicit scope decision, not proof. `U` identifies the delivery unit below. Paths are relative to `crates/` unless prefixed with `docs/`.

| ID | Audit requirement | Status | Supporting implementation/test or verification gap | Unit |
| --- | --- | --- | --- | --- |
| R01 | Pure snapshot+policy selection with stable fingerprint | missing | Selector proposal port and deterministic snapshot identity exist only in `docs/adapter-boundaries.md`; no Selector crate or suite. | U6 |
| R02 | Grouping by milestone+label-set, group as atomic batch | deferred | S1 is single-issue. Rich grouping and multi-issue identity require a separate scope decision; preserve atomic emitted groups if added. | U6 |
| R03 | Per-repo policy / allowlist | implemented+unverified | `workloop-config/src/lib.rs::Installation` has a default repository, not a verified allowlist/policy enforcement path. | U2,U6 |
| R04 | Stable request identity + content-hash dedup excluding bookkeeping | implemented+verified | `workloop-core/src/command.rs::payload_digest`; `workloop-store/tests/contract_commands.rs::submit_replays_the_same_decision` and `same_key_different_payload_is_idempotency_conflict` pass. Selector overlap dedup is separate. | U1,U6 |
| R05 | Terminal dedup never restarts | implemented+verified | `contract_commands.rs::retry_requires_confirmed_failure` and `cancel_queued_is_terminal_and_second_cancel_is_noop` pass; retry creates a new Execution. | U1 |
| R06 | Triage/ambiguity filtering upstream of admission | deferred | Ambiguity screening stays upstream; no Core triage engine or automatic issue refinement in S1. | U6 |
| R07 | Bounded scan that skips an unstartable head and advances independent work | implemented+verified | `contract_queries.rs::deferred_head_does_not_block_later_items` and `contract_commands.rs::dispatch_conflict_defers_that_item_and_later_items_still_admit` pass. | U1 |
| R08 | Unknown scan result → hold, never "free" | implemented+unverified | Uncertain process holds tested in supervisor contracts. Selector scan/read unknown and real adapter unavailable paths are missing. | U1,U3,U6 |
| R09 | Capacity as claim on a card | implemented+verified | Single-installation capacity/reservation accounting tested by `contract_queries.rs::capacity_explains_every_slot_reservation_and_deferral`; per-host or per-repository pools are deferred. | U1 |
| R10 | Dispatch = atomic (transition + reservation + saved request) | implemented+verified | `contract_commands.rs::dispatch_admits_oldest_eligible_and_reserves_atomically` and `admission_rolls_back_on_request_persistence_failure` pass. | U1 |
| R11 | Lost acknowledgement is not failure | implemented+verified | `supervisor_contract.rs::crash_near_spawn_leaves_uncertain_with_no_second_writer` and `contract_queries.rs::command_result_recovers_a_lost_response_by_id_and_replay_key` pass. Live host-effect unknowns remain U3. | U1,U3 |
| R12 | Two-layer duplicate guard (local lock + remote label) | implemented+unverified | Reservations and external claim tests cover supported entry points; OS/runtime permission enforcement and new host identity keys are not proven. | U1,U2,U4 |
| R13 | Bounded run contract (immutable base, timeouts, budgets) | implemented+unverified | Invocation/review budgets and runtime stopping tests pass; immutable real task/artifact binding and live enforcement remain absent. | U1,U4 |
| R14 | Worker report is evidence, not outcome | implemented+unverified | `supervisor_contract.rs::a_workers_done_claim_alone_releases_nothing` passes. Full WorkerReport validation and real completion evidence are missing. | U1,U4 |
| R15 | Stop semantics: identity gates, group-wide stop proof, unverified/unknown distinction | implemented+verified | `supervisor_contract.rs`: PID reuse, snapshot escape, zombie/reap, persistence refusal, and kill-9 restart tests pass. This verifies the local POSIX contract only. | U4 |
| R16 | Plan/execute split with read-only planner | deferred | Read-only planner is Worker-internal optional hardening; no separate v0.1 planner port. | U4 |
| R17 | Verification record bound to the exact patch bytes | missing | Artifact presence is modeled; exact patch/commit digest, durable git objects, and verification binding required by #46 are not implemented. | U1,U4 |
| R18 | Structured terminal classification over exit codes | implemented+unverified | Typed `ObservationFact`/`Outcome` exist; a complete real Worker result schema and validation are missing. | U1,U4 |
| R19 | High-risk manifest → independent review gate | deferred | Additional declared-risk manifest gate deferred in #46; required task checks/review/human gates remain. | U1 |
| R20 | Bounded verify remediation | implemented+unverified | `admission_never_exceeds_max_invocations` and review-round tests pass; actual verify/fix-stage orchestration and budget consumption need U5 proof. | U1,U5 |
| R21 | Single publication owner with persisted intent | implemented+unverified | `work.rs::maybe_publish` and `fixture_adapter.rs` exercise durable fixture intents; separate push/create and real receipts are missing. | U1,U3 |
| R22 | Verdict requires publication evidence (fail-closed adoption) | implemented+unverified | `workloop-core/tests/stage_contract.rs` verifies produced/pushed-head classification. Runtime receipt assembly and all stage postconditions remain missing. | U1,U3,U5 |
| R23 | Marked replies, no merge/force-push, provenance markers | implemented+unverified | Fixture records publication/comment intents; real marked replies, thread effects, ready state, and non-force publication need independent receipts. | U3,U5 |
| R24 | Bounded refusal codes as the only persisted failure detail | implemented+unverified | Typed rejection codes exist, but free-text reasons and arbitrary input/output need bounded/redacted export verification. | U2,U7 |
| R25 | Head-bound judgment: stale answers never silently rebind | implemented+unverified | Stale-head judgment/release/dispatch tests pass in `contract_commands.rs`; complete current finding-set validation is missing (#24). | U1,U7 |
| R26 | Bounded converge loop with severity-routed human halt | implemented+unverified | Review-round bounds and paused Attention exist. Real review/fix dispatch and severity routing are not established. Per-Work no-progress observation and bounded automatic recovery are required designs under #51/#55; cross-task diagnosis remains deferred. | U1,U5 |
| R27 | Judgment waits/decisions as durable ledgers surviving card replacement | implemented+unverified | SQLite judgments persist and head guards are tested. Cross-Execution decision validity and real finding identities require explicit evidence tests. | U1,U5 |
| R28 | Finding disposition ledger across attempts | missing | No complete per-finding disposition ledger with published fix evidence across attempts; event-log presence alone does not establish it. | U1,U5 |
| R29 | Operator console over versioned read contracts + typed actions | deferred | TUI delivery deferred under #47. Existing TUI/CLI use shared contracts; required CLI/skill workflow is U7, not deferred with TUI. | U7 |
| R30 | Reconciliation reads facts, never invents them | implemented+unverified | Local process reconciliation/restart tests pass; real external observations and unknown effects remain unverified. | U3,U4,U5 |
| R31 | Idempotent lifecycle ops keyed on stable identities | implemented+verified | `contract_commands.rs` replay tests cover all Command kinds and changed-payload conflicts; Queries recover saved decisions. | U1,U7 |
| R32 | Bounded, recorded recovery with permanent-refusal class | implemented+unverified | `supervisor_contract.rs::missing_artifact_runs_bounded_recovery_then_fails` passes. Missing/corrupt distinction, permanent refusal, unresolved-effect guards need follow-up. | U1,U4,U5 |
| R33 | Deterministic blocker re-evaluation from structured evidence | implemented+unverified | `work.rs::decide_resume` checks phase/head/budget and reservation reuse; generic repair prerequisites and changed-evidence proof need design/tests. | U1,U5 |
| R34 | Attribution heuristic for foreign writers (checkpoint markers) | implemented+unverified | Supported external claims/reservations exist. Unknown ownership is conservative; arbitrary processes bypassing the claim contract are outside guarantees. Permission isolation remains U2. | U1,U2,U4 |
| R35 | Attention = actionable-now decisions, derived not stored | implemented+verified | `contract_queries.rs::attention_versions_and_acknowledgement`, `uncertain_beyond_threshold_surfaces_attention`, and `acknowledge_requires_a_currently_derivable_attention_version` pass. | U1,U7 |
| R36 | Unknown-surfaced-over-hidden | implemented+unverified | `QueryResult.sources` supports unknown/unavailable, but real source availability and distinctions from empty findings are unverified. | U3,U7 |
| R37 | Declarative installation config | implemented+unverified | `workloop-config/tests/installation.rs` verifies shared actor/config behavior; credential roles, host capabilities, retention and allowlist configuration are absent. | U2 |
| R38 | Secrets never in argv/logs/evidence | missing | No real credential-reference resolver or structural redaction/isolation tests. Flags and documentation do not prevent secrets in inherited credentials, argv, logs, or evidence. | U2,U4,U7 |

All audit `omit` mechanisms stay excluded: external card/queue databases, unattended scheduler/cron, parked-selector registries, card claims/heartbeats, remote admission/SSH/container infrastructure, pinned runtime deployment, embedded remote publication pipelines, deployment-specific webhook/debounce filters, external message delivery, and source-specific pruning/install scripts. Their retained safety requirements are mapped above. Do not import those mechanisms or infer that an old incident is still unfixed. The six incident classes map to R04/R07/R10 (duplicate dispatch), R09/R14/R30 (state disagreement), R11/R15 (uncertain stops), R21–R23 (publication), R25/R27 (stale review), and R20/R26/R32/R35 (convergence and recovery). Cross-task doctor diagnosis is a later operational concern. Per-Work no-progress observation and bounded automatic recovery are v0.1 requirements; #51 defines failure and recovery accounting, and #55 defines observation, stop/reconcile/requeue and retry scheduling. The owner-approved defaults and eligible failure classes are defined in [recovery policy](recovery-policy.md).

## Dependent delivery units

These are proposed units for later issue planning, not new issues or authorization to implement. Each requires the owner to approve its concrete scope and acceptance criteria. Reuse existing issues where identified. U1–U5 use #46's merged design contracts; U0 baseline/CI and documentation preparation can proceed independently. Design schemas before implementation and move all active callers and persistence together. For the unversioned legacy store, #50 records the owner-approved read-only archive/new-store exception rather than executable data conversion. Preserve historical identities in that archive; do not maintain parallel GitHub-specific and generic Core paths.

| Unit | Scope and responsibility boundary | Dependencies | Acceptance and completion evidence |
| --- | --- | --- | --- |
| U0: reproducible baseline / CI | Reuse #28. CI runs fmt, build, test, clippy in build-before-test order; retain runtime revision and logs. No new service | None | All four checks green on the same head; fixture-worker prebuild demonstrated on a clean checkout |
| U1: remaining Core and store contracts | Migrate to #46's neutral host/repository/revision/actor identities, immutable Work binding, per-effect intents/receipts, capability guards and normalized evidence. Complete current finding coverage (#24), artifact identity/digest validity, required stage effect set, per-finding disposition, repair prerequisites and Outcome evidence references. Implement the [extracted #51 obligations](evidence-requirements.md), immutable failure/recovery decisions, and Work-wide accounting under approved policy. Keep transition rules in Core, I/O outside | #46; U0 gate before merge | Regression tests for same-head growth, missing/wrong effect facts, invalid/corrupt artifacts, target-branch-only diff movement, identity collisions/rename, archive/new-store cutover, new-format record integrity, budget/cancel guards with unresolved effects. No Outcome with empty/mismatched required evidence; source records retained |
| U2: configuration, permissions, setup and retention | Extend shared installation config with host binding/capabilities, allowlist, user-token credential references and actor roles, Worker permission isolation, and retention age/storage policy. User-token support persists after App support. Setup validates operation/actor permissions. Physical cleanup belongs to runtime tooling, guarded by authoritative references/state; no Core filesystem scanner | U1 binding/evidence model; token setup before U3 live checks | Clean isolated setup; denied/missing/expired credentials visible; no secrets in argv/log/export; worker cannot use inherited GitHub writes. Configurable retention exercised with old/young bodies, storage pressure, active/paused/recovering/unknown-effect pins, interrupted cleanup and missing bodies. Durable history/reference metadata retained; deleted bodies show explicit unavailable evidence. Exact defaults and storage-bound behavior require owner approval |
| U3: real GitHub repository-host adapter | Reuse #17, propose revised criteria without editing it here. Translate common typed contract to GitHub. Separate guarded fast-forward push, PR create/update, comments, reviews, replies/thread actions and ready effects. Reconcile action/payload/original actor binding; preserve successful partial receipts. Polling is sufficient; no mandatory webhook server | U1, U2 | Shared host suite plus isolated live checks: push succeeds/create fails, lost response, duplicate events, stale head, permission drift, actor rotation, outage, restart/config change. Read back each applied effect; unknown never authorizes blind replay. Reviewer support/eligibility validated per requested operation |
| U4: real Worker plus independent contract adapter | Translate one approved Invocation to reference Worker runtime, with durable request/artifact export, exact commit/check binding and declared capabilities. Reuse POSIX supervisor for process evidence. Add independent deterministic contract Worker, not a second production provider. Workers never publish or drive outer loops | U1, U2 | Unchanged shared Worker suite against both implementations: duplicate/lost start, unknown liveness, stop proof, missing/invalid artifacts, stale revision and denied writes. Config-only swap for new Invocations preserves Command, queue, budget and Outcome semantics. Retain workspace/git objects through verified handoff; no running-Invocation migration |
| U5: CLI execution-loop composition and safe reconciliation | `workloop-cli::run_tick` schedules observations, dispatch, supervisor, host outbox, timed sweep and artifact/unknown-effect reconciliation. Core decides permitted transitions using supplied clock/facts. Supervisor stops/inspects; adapter executes/observes remote effects. Wire bounded code → mechanical verification → enabled invariant check → publish → enabled code review → fix → reassess and current Attention | U1–U4 | Restart at ready/spawn/stop/effect boundaries; deadlines prohibit new trials, stop is verified before terminal release, partial effects retain receipts, no duplicate writer or inferred success. Verify review rounds, explicit repair conditions, retry E2 and terminal intent handling. Per-Work material-progress observation, durable bounded automatic recovery across Executions, stop/reconcile/requeue, persisted backoff and terminal retry-intent retirement required. Use the approved configurable defaults and failure classes in [recovery policy](recovery-policy.md). Cross-task doctor audits excluded |
| U6: reference Selector / alternate admission | Thin deterministic single-issue proposal from snapshot+policy; Core owns allowlist/admission/idempotency. Selector outage cannot block manual submission or existing Work. Rich grouping/automatic triage/scheduling deferred | U1 admission, U2 scope config | Same snapshot/policy yields same identity; duplicates and stale sources handled; unauthorized sources refused. Config-only alternate admission has the same queue/outcome semantics. Separate from S1 |
| U7: CLI results, skills and installation walkthrough | Reuse direct Commands/Queries; document structured accepted/rejected/error/unknown output, exit status and diagnostics. Ship a skill for a named supported coding agent, installation/discovery/invocation guidance and direct-CLI fallback. Skill retains replay identity and preconditions and never implements execution policy | U1 guard, U2 setup, U5 runtime | Every agent-interface matrix row below passes through real CLI and installed skill where applicable; malformed/config/store errors cannot appear accepted; conversation closure leaves execution queryable. No TUI/MCP requirement |
| U8: GitHub S1, Attention and recovery acceptance | Seed an isolated issue/repository; exercise real Worker, real GitHub and real review/fix, then failure branches. Run success, Attention and recovery as separately evidenced variants | U0–U5, U7; U6 alternate check separate | All S1 mappings below pass with exact revisions/config, Work/Execution/Invocation/action identities, remote PR/head/check/review receipts and stop evidence. At least one real finding and published fix; zero-finding run does not cover review/fix. Never merge/deploy as part of acceptance |
| U9: host exchangeability proof | Required by merged #46 ADR-002. Independently implemented host fixtures run common suite; narrow live GitLab contract spike before claiming GitHub/GitLab interchangeability. Full GitLab production support deferred | U1 common contract; U2 binding/config; U3 shared suite | Baseline operations, declared optional capabilities, support versus permission, scoped IDs, immutable diff snapshots, partial effects, config changes and swap semantics tested. Live guarded push/MR/comment/head and revision-bound reassessment, denial/lost-response/restart evidence. Unsupported approval/ready stays explicit. This is separate from GitHub S1 |

Critical path: #46 → U1 → U2 → U3/U4 → U5 → U7 → U8, with U0 as the merge-quality gate. U3 and U4 may be developed independently after their shared contracts/config are settled; local heavy tests remain serial on a memory-limited host. U6 and U9 have their own acceptance gates and do not turn S1 into an unattended or multi-host workflow.

### Required infrastructure and runtime setup

One POSIX host with a Rust toolchain, git, the chosen real Worker runtime, writable isolated workspace/state directories, SQLite, outbound GitHub access, and an allowlisted disposable test repository is sufficient. The CLI execution loop must be runnable independently of the conversational client. Use a user-token reference for the initial GitHub installation and configured invariant/code-review providers for enabled lanes. Native GitHub Approve is not required; separate host identity is needed only for a separately requested host capability, not automatic Work success. No cloud provisioning, containers, Kubernetes, remote SSH workers, message bus, webhook service, external Kanban, or artifact-storage service is required. Single-host retained workspace/git objects can supply the artifact handoff, subject to configured retention and dependency pins.

Installation evidence records CLI/Core/adapter revisions, config identity and sanitized policy, runtime version, host platform, allowed repository, credential role identifiers, required capabilities, verification command and limits. Never record token values. U2 defines safe storage/cleanup before U8 produces persistent local bodies. App support can be a later adapter unit without removing tokens or altering original actor bindings.

## Representative-scenario coverage

The authoritative scenario is [v0.1 S1](v0.1.md#representative-live-acceptance-scenario). Every row below names required changes, owner, dependencies via U units, verification, and completion evidence. These are absent live evidence today. Unit-level fixture checks cover only the narrow assertions in the inventory.

### S1 setup and expected path

| Criterion | Required changes / owner and dependency | Verification and completion evidence |
| --- | --- | --- |
| Isolated input, intent, constraints, verification, permissions and runtime | U2 setup; U7 submission; U4 Worker. Core binds one issue/repository, effects and task; host adapter holds publication token | Seed procedure, installation/config/revision manifest, queued request with exact verification input; no external board, no secrets, no Worker host-write credentials |
| Observed facts and stage owners | U1 typed observations; U3 host; U4 process/artifact; U5 loop | Issue, current head, named checks, findings and per-effect results retain producer, revision and original binding; owner matrix matches #46 |
| S1.1 accepted W1/E1 queued, no Invocation | U1 existing admission + U7 | CLI/skill output and store commit show one Work, one E1, no early launch |
| S1.2 atomic dispatch and coding I1 | U1 reservations + U5/U4 | Atomic slot/reservation/request transaction, stable request/I1 identity and recorded process identity; replay launches no second writer |
| S1.3 exact change and passing verification | U4 export; U1 validity | Artifact digest, reconstructable produced commit, supervisor verification argv/result bound to I1 and exact bytes, and separate enabled invariant-check Invocation with exact approved-set coverage; missing/corrupt/mismatched results cannot authorize publication |
| S1.4 push then PR creation and H1 | U1 per-effect authorization; U3 publication | Independent push and create IDs/receipts, observed target PR/head H1, no force-push or combined success hiding a failed second effect |
| S1.5 real checks and real review with findings | U3 ingestion; U5 review dispatch | Explicit configured CI and separate code-review task identities on H1, available source, at least one real finding. Zero findings means this leg is unexercised; revise input and rerun |
| S1.6 judgment, fix I2, republish H2 | U1 complete judgment (#24); U4 fix; U3 effects; U5 loop | Accepted H1/finding set, I2 patch/check evidence, guarded update H1→H2, required marked reply/thread/ready receipts as policy requires; claims are not receipts |
| S1.7 H2 reassessment | U3 observations; U1 stage gate; U5 | Passing required named checks and review for H2, no unresolved finding. H1 evidence remains history and cannot approve H2 |
| S1.8 verified succeeded Outcome | U1 evidence references; U5 settlement | Outcome retains PR/H2/check/review/artifact/stop references and stage assessments; all writers safe, no unresolved host effect, reservation/slot released once. Merge/deploy untouched |

### S1 branches and abnormal inputs

| Criterion | Required changes / owner and dependency | Verification and completion evidence |
| --- | --- | --- |
| Further failed checks or findings | U1 bounds; U5 next-stage scheduling; U4 | New Invocation under same Execution within saved limits; each finding disposition and verification linked to correct head |
| Human judgment | U1 accepted/released judgment; U5; U7 | Paused state and current Attention, head-bound decisions, explicit release, requeue without losing WIP or resetting budgets |
| Duplicate submit, identical key/payload | U1 existing replay; U7 | Original decision returned, one W/E and effect; repeat through direct CLI and skill |
| Duplicate submit key, changed payload | U1 existing conflict; U7 | Explicit `idempotency_conflict`, original retained, no second mutation |
| Duplicate Observation source event | U1 scoped event identity; U3 | Duplicate ingest receipt and single effect even with equal delivery IDs from different configured sources |
| Installation stop/restart with possibly live Invocation | U4 process identity; U5 reconciliation | Restart reconciles original request and all recorded identities; unproven writer retains ownership; no duplicate launch |
| Publication unknown after push or PR-create response lost | U1 separate intents; U3 reconciliation; U5 | Original action/payload/actor binding and partial receipts retained; matching effect adopted, ambiguity remains unknown, no blind second write |
| Stale head/review; external head change | U1 guards; U3 fresh reads; U5 reassessment | Old-head judgment rejected, current head never moves backward, new-head review requested within budget, old evidence retained |
| GitHub outage or unavailable source | U3 availability; U5 recovery; U7 query | Unknown/unavailable is distinguishable from empty findings; no false completion; recovery observations catch up on restore |
| Invocation/review/runtime budget exhaustion | U1 bounds; U5 timed sweep; U4 stop; U3 unknown-effect reconciliation | Desired failure/stop recorded, no new attempts, no terminal release before safety; limit code, WIP, stop/effect evidence retained; retry E2 can claim same workspace/branch |
| Prolonged uncertain state | U1 Attention derivation; U5 periodic supplied clock/facts | Recorded uncertainty duration/threshold, retained capacity and ownership, current next actor/action; acknowledgment does not release safety guards |
| Completion boundary and later head movement | U1 frozen Outcome + current projection; U3/U7 | Success is not merge/deploy. Later head displays unreviewed readiness without rewriting the completed Outcome; new follow-up Work required |
| Rerun | U2 seed/setup; U8 runner | Fresh issue, Work and branch per run; no identity collision or reused acceptance evidence |
| Cleanup and retention | U2 policy; U5 safe terminal handling | In-flight cancellation reconciled before release; remote PR/issues/checks/reviews retained as evidence. Configured local reclamation preserves history/refs and protects dependent bodies. Missing/reclaimed bodies explicitly unavailable |
| Absent / valid / invalid stage evidence | U1 validator; U4 export; U3 receipts | Each assessment records class and reason against Invocation/repository/head/required check. Absent triggers bounded recovery after stop, valid can satisfy only its criterion, invalid retained and never counted as success |
| Worker interchangeability, separate from S1 | U4 both adapters and U1/U5 semantics | Same shared suite against real and independent deterministic Worker; config-only new-Invocation swap, identical Command/queue/budget/Outcome behavior. Does not substitute for real S1 |

### Remaining lifecycle acceptance rows

The general [acceptance-scenario table](v0.1.md#acceptance-scenarios) also retains these conditions. The following completes its mapping; duplicate command/Observation, stale review, missing artifacts, unknown launch and adapter outage are mapped above.

| Criterion | Required changes / owner and dependency | Verification and completion evidence |
| --- | --- | --- |
| No external Kanban | U2/U5/U7/U8 | Clean setup, intake/queue/dispatch/Outcome through built-in store and CLI only |
| Capacity unavailable | U1 admission; U5 later dispatch | Accepted queued E, no early request; one admission when capacity frees |
| Concurrent CLI/skill retry | U1 transaction/version; U7 | Exactly one new Execution, other result explicit/replayed/rejected; no duplicate writer |
| Queued cancel versus dispatch | U1 ordering; U5/U4 | Either no request ever launches or existing request enters safe stopping/uncertain path; no late launch treated as stopped |
| Same-head findings grow after inspection | U1 coverage guard (#24); U7 | Add a finding between inspect and submit/release; incomplete judgment explicitly rejected or presented for renewed decision |
| Stale external view | U1 preconditions; U7 | Rejection retains requested version/head and exposes current state; skill does not silently rewrite request |
| Process disappears or heartbeat missed | U4 inspect; U5 reconcile | Liveness unknown is not death; replacement waits for stop/prevention proof, no slot release from heartbeat loss |
| Escaped child / unreaped zombie | U4 existing supervisor + U8 injection | All snapshot identities inspected; survivor holds uncertainty, zombie stopped, owned child reaped |
| Conflicting managed writers | U1 reservations; U5 | Retry/claim conflict identifies holder; dispatch defers only affected queued E |
| Manual writer / expired external claim | U1 claim protocol; U3 unexpected-head facts | Active claim conflicts; TTL expiry alone is not stop proof. Arbitrary claim-bypassing processes remain outside managed guarantees |
| Paused WIP | U1 reservations; U2 cleanup; U5 | Capacity released but resource reservation retained; cleanup cannot remove WIP needed for resume |
| Accepted command response lost | U1 existing result query; U7 | Original identity/key recovers decision; no fresh-key duplicate on unknown |
| Separate installation | U2 identity/paths; U7 | Distinct store/config/credentials cannot mix Work, events or evidence |

## CLI and skill acceptance coverage

Every row of [agent-interface acceptance](agent-interface.md#acceptance-and-evidence) is covered here. Its workflow is required even though no skill exists today.

| Criterion | Required changes / owner and dependency | Verification and completion evidence |
| --- | --- | --- |
| Clean setup and invocation | U2 installation; U7 packaging | Named supported agent discovers actual shipped skill using real documented paths; direct CLI uses same installation; no TUI/MCP prerequisite |
| Submit | U7 → U1 | Direct and skill runs each correlate accepted output/key with exactly one queued E; never describe acceptance as completion |
| Inspect | U7 → projections/U3 | Work list/detail, evidence, Attention and capacity match persisted facts; unavailable source/blocker visible |
| Eligible interventions | U7 → U1/U5/U4 | Retry/resume/cancel/acknowledge in eligible and ineligible states through both paths; correlate E identities, stop proof and Attention versions |
| Review judgment safety | U1 #24 + U7 | Stale head and same-head finding growth rejected at acceptance; no reliance on TUI draft validation |
| Duplicate/concurrent commands | U1 + U7 | Replay, changed-payload conflict and concurrent interfaces create no duplicate E or writer |
| Lost response | U7 → saved command result | Original command/replay identity recovered; recovery unavailable remains unknown without fresh request |
| Explicit failure | U7 parser/config/store boundary; U2/U3 | Documented distinguishable domain rejection, invalid input/config, store error and unavailable evidence; diagnostics do not corrupt structured output; process exit zero never alone means accepted |
| Conversation ends | U5 independent loop; U7 | Close agent session, inspect from CLI, prove no cancellation/duplicate start/authority loss inferred from session closure |

## Six design aspects and remaining decisions

| Aspect | Applicability and rationale | Settled boundary | Open question and gate |
| --- | --- | --- | --- |
| State transitions | Real waits and restart can leave external facts ahead of store state | Core evaluates guards; CLI loop drives checks/reconciliation; adapters perform effects | U1 must design new receipt/repair/finding schema and U5 scheduling using #46's merged contract before implementation |
| Failure semantics | Unknown/partial effect is different from definite failure; silence is not stop proof | Reconcile original action; retain ownership and successful partial receipts; bounded recovery does not erase unknown effects | Choose operational response and user-visible repair steps for unavailable evidence, without inventing terminal success or unlimited retries |
| Trust boundaries | Worker text and source issue/review content are untrusted evidence | Host adapter alone writes remotely; token-first auth with retained user-token option; scoped actor/operation validation | U2 proposes concrete credential isolation and supported runtime; owner reviews tradeoffs before implementation. Independent reviewer identity/setup must be fixed before U8 |
| Identity invariants | Retries, rename, provider collisions and moving diff bases can invalidate shortcuts | Stable W/E/I/action plus #46 host/repository/actor binding, exact revision and immutable diff context | U1 uses #50's concrete identities and archive/new-store cutover; #51 completes durable finding/evidence identities. Multi-issue group identity remains outside S1 |
| Information preservation | Local history/artifact growth already harms operation; deleting recovery evidence is unsafe | Configurable body retention required; preserve durable history/refs, pin bodies needed for active/paused/unknown/recovery paths; mark reclaimed evidence unavailable | Owner must choose default age/storage limits, protected-data pressure behavior and compact-history policy before U2. No unapproved numbers or unconditional pruning |
| Resource bounds | Invocation, review, runtime and artifact recovery have distinct budgets | CLI loop supplies clock/observations; Core guards limits and safe release; single-installation capacity | Per-repository/multi-host pools deferred. Per-Work no-progress and bounded automatic recovery are in scope under #51/#55. The CLI loop supplies observations and schedules checks; Core guards transitions and durable limits. Cross-task diagnosis stays deferred. Thresholds, eligible failures, Work-wide budgets, and safe capacity release follow the approved [recovery policy](recovery-policy.md) |

## Human review and follow-up authorization

The owner reviewed and confirmed publication responsibility, token-first authentication with continued token support, configurable retention, and the CLI-loop/Core/supervisor/doctor division in the working session on 2026-11-07. The owner also previously approved #47's CLI/skills scope. These are recorded scope/responsibility decisions, not proof of acceptance or permission to execute the runtime plan.

1. Review this planning PR's complete R01–R38 inventory, S1 and interface mappings, dependency graph and non-goals. The owner accepted the overall planning document by merging PR #49. Detailed design issues #50–#56 still require their own owner review; planning approval does not check off live acceptance.
2. Resolve implementation-shaping choices with the owner: retention defaults/storage pressure and history growth policy; concrete Worker runtime/isolation; reviewer identity and exact required profile; remaining evidence/approval schemas and operational cutover details. #50 specifies identity/storage and the archive/new-store choice. Preserve questions rather than silently choose defaults. #46's additional host-contract requirements are merged design inputs, not runtime completion evidence.
3. After planning approval, propose concrete follow-up issue scopes/acceptance/dependencies using U0–U9 and reuse #17/#24/#28. Obtain explicit authorization before creating/updating issues or beginning implementation. This issue itself creates no follow-up issues and changes no existing issue bodies/states.
4. After implementation, independently review required contract/CLI/setup evidence before authorizing isolated live runs. A later S1 report must separate success, Attention, failure/recovery, Worker swap and any host-portability proof. Plan completion, implementation completion and live E2E completion are different milestones.

Non-goals: migrating external Kanban history, source-system changes, unattended scheduling, hosted tenancy/cloud infrastructure, auto-merge/deploy, TUI delivery, MCP, dynamic plugins, live adapter handoff, production GitLab or broad multi-host parity. Remaining evidence gaps are the absent live/installation/adapter/skill checks above, incomplete implemented Core contracts, and owner review of the remaining detailed designs. Retention may remove bodies under policy but must never turn missing evidence into a fabricated verified result.

## Approved policy and later milestones

Implement the [recovery policy](recovery-policy.md) under U1, U2, and U5. #51 owns evidence, equivalent-failure and progress classification, and counters; #53 owns configuration validation and complete agent-usable reference documentation; #55 owns Commands, observation scheduling, and safe human-wait capacity release. Existing per-Execution bounds remain additional limits. The [later milestone scope](post-v0.1-milestones.md) retains all seven approved deferrals; it is registered in the linked post-v0.1 milestones but does not authorize implementation or create follow-up issues.

The [40 reconciled identity/admission obligations](identity-storage.md#extracted-identity-and-admission-obligations) specify all previously planned #50 targets, each with a covering criterion and target proof scenario. Other design targets remain planned; the full design and implementation-start gate remain incomplete. Final review of this concrete design precedes #50 closure. Conflict detection/human handling, closed-source holds, verified rename versus approved transfer, and deferred external-result adoption are owner-approved; #51-#55 own their detailed protocols.
