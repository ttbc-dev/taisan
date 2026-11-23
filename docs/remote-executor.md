# Remote executor boundary and review-first acceptance (v0.4)

Status: design specification for issue #372 under milestone v0.4
("Pluggable remote execution, review first"). This document defines the
executor-ownership boundary, the two supported execution modes, the
custody contract between one coordinator and one static remote
endpoint, and the acceptance rows the dependent issues (#373–#386
plus the #416 manifest-custody decision) implement and verify against. It is a specification design, not
implementation: it authorizes no runtime change, credential change,
transport, store migration, live execution, publication, or Work
completion, and it changes no v0.3 guard. Dependent implementation
units start `needs human`, recheck current `main` and overlapping PRs,
and retain their own tested revisions, configuration, named scenarios,
and results.

**Evidence boundary.** The pinned-baseline inventory below is derived
from source reads and tracker queries at the recorded revisions, plus
the public locators produced by the first live run (Work `w-2` /
Execution `e-6` → `succeeded` at head `35ce3cb`, PR
`shimpeiws/coding-worker-smoke#62`, 2026-11-23). That run approximated
a remote boundary by operator-driven manual transport, a
bridge-assisted push, and containment workarounds; its facts are
cited through the issues it produced (#410, #411, #412) and are
evidence for the gaps, not for any remote-execution claim. Fixture and
contract evidence never prove a live remote run.

## 1. Pinned baseline and reusable surface

### 1.1 Inspected revisions

| Surface | Revision | Locator |
| --- | --- | --- |
| Repository `main` (all components below) | `63905c6bd08f646289876ebb1dfeedd4409ca7f6` | `git rev-parse origin/main`, merge of #408; also the revision the first live run exercised |
| Executor transport contract review baseline | `ca45ac8fbf5647df7dbdcac59185b8442c9e4890` | `git rev-parse origin/main` after #417; current review baseline for Core, supervisor, Worker, host adapter, and the public #366/#367 reference implementations |
| Manifest-handoff PR | `bfa0412` (merge of branch head `c1d3343`) | PR #413, addressing #410/#411/#412 — merged into `main` during this baseline review; the baseline main pin above predates it |
| Public v0.3 reference implementations | same tree | `public-issue-selector` and `taisan-worker-claude` shipped under #366; clean-install documentation alignment tracked by #367 |
| Design baselines cited by prior contracts | `51f0b34daf33a1975a39b7614f5ceeac7dabf431`, `7d9ea3d` | [v0.3](v0.3.md) planning/recheck baselines, kept as historical audit |

Every component lives in this repository, so one SHA pins all of them:
Core (`workloop-core`), the `execution` supervisor (`workloop-exec`),
the reference Worker (`taisan-worker-claude`), the repository host
adapter (`workloop-github`), the reference Selector
(`workloop-selector`), and the CLI composition root (`workloop-cli`).
Crate packages carry no independent release versions; the commit SHA
is the baseline identity.

### 1.2 Reusable ports

The remote boundary reuses these existing contracts unchanged. A row
marked *reused* means the remote path must satisfy the same semantics,
not a weakened remote variant.

| Port / mechanism | Where specified | Reuse for remote execution |
| --- | --- | --- |
| Worker wire contract `taisan.worker.request/2`, `taisan.worker.report/2`, `taisan.worker.declaration/1`; `argv-v2` transport (`describe`, `run --request <path> --out <dir>`) | [v0.3](v0.3.md#4-executable-request-taisanworkerrequest2) | The remote executor hosts the same argv surface and the same mount contract. The request/report envelopes and their digests are identical on either side of the transport; the executor transport (#374) carries them, it does not redefine them |
| Request/invocation identity and first-binding rule | [v0.3](v0.3.md#32-identities), [worker protocol](worker-protocol.md#start-observe-stop) | `request_id` `<execution_id>:inv:<sequence>`, `invocation_id`, `request_digest`, and the immutable adapter declaration binding are allocated and recorded coordinator-side before any remote send; the executor never mints, rewrites, or reuses them |
| Report classes `success`/`negative`/`unavailable`/`failure`/`invalid`/`absent` and closed reason codes | [v0.3](v0.3.md#6-report-taisanworkerreport2) | Ingested identically; a transport fault is not a report class — it surfaces through the executor-operation vocabulary (typed refusal, `unknown`, unreachable) and resolves to `unavailable`/`absent` evidence plus Attention (§4.2), never a clean result |
| Artifact handoff: sandboxed export bundle → quarantine import → `refs/taisan/artifacts/<artifact_id>` → `acknowledged` row | [worker protocol](worker-protocol.md#artifact-handoff) | The remote analogue is `collect` + `acknowledge`: produced objects return through quarantined import with the same base-ancestry/exact-commit checks; the durable acknowledgement stays the coordinator store row plus the verified ref, never a remote claim |
| Supervisor stop proof: durable stop intent, descendant snapshot, identity re-verification, state-not-presence inspection | [worker protocol](worker-protocol.md#process-descendants-and-stop-proof), [decisions](decisions.md) | Remote stop authority follows the same rule: persisted intent plus inspected remote process identities plus executor incarnation evidence. A closed transport session, a missed heartbeat, or an expired lease never proves stop |
| Verification runner: independent clean worktree of the produced commit, exact argv/commit/environment binding, `Inconclusive` on timeout/signal | [worker protocol](worker-protocol.md#verification-runner), [v0.3 §14.5](v0.3.md#145-verification-toolchain-dependency-setup-and-host-allowlist) | Relocatable to the executor as a supervisor-authorized operation distinct from any Worker Invocation, keeping the same input/result/evidence shapes (#383) |
| Containment profile: bwrap mount/PID/network namespaces, credential hygiene, mandatory model-API-only CONNECT egress | [worker protocol](worker-protocol.md#selected-profile), [v0.3 §14.3–14.4](v0.3.md#143-runtime-mount-credential-and-provider-config-delivery) | The executor host applies the same approved profile — including the CA trust and commit identity the first live run needed — with the same negative preflight. No remote profile is weaker than the local one |
| Repository host port (`RepositoryHostPort`, `HostActionIntent`, per-effect receipts, reconcile-by-read-back) | [repository-host-contract](repository-host-contract.md), [v0.3 §14.7](v0.3.md#147-production-host-driver-binding-and-composition) | Untouched. Every repository-host read/write stays with the coordinator's in-process host driver; the executor holds no repository-host credential and performs no host effect |
| Lifecycle, capacity slots, writer-resource reservations, external claims | [lifecycle](lifecycle.md), [glossary](glossary.md#execution-ownership) | A remote Invocation holds the same slot and reservations as a local one; `queued → ready` and terminal release rules are unchanged |
| Stage contract: `ConvergeRecord` classification, executor × reviewer orthogonality | [architecture](architecture.md#stage-contracts-are-executor-independent), `workloop-core/src/stage.rs` | The executor axis already exists in the contract: placement selects *where* a stage runs, never *what it may skip*. A remote fix that cannot show a published produced head classifies `UnpublishedFailure` exactly as a local one |
| Retention pins and `body_catalog` availability states | [retention policy](retention-policy.md) | Remote WIP/output is pinned until durable acknowledgement; coordinator pin bookkeeping is unchanged and remote bodies are tracked through transfer/acknowledgement records rather than shared filesystem state |

### 1.3 Concrete gaps the design must close

| Gap | Evidence | Consequence for the remote contract |
| --- | --- | --- |
| Reference Worker did not produce the workspace-root `artifact.json` required-artifact evidence | #410; operator scaffolding synthesized it during the first live run; addressed by PR #413, merged into `main` as `bfa0412` during this baseline review | The remote custody contract cannot rely on implicit workspace state: produced artifacts cross the transport only as validated descriptors (manifest + object closure), and the manifest shape is a contract input, not an afterthought — manifest placement/lifecycle is decided by #416 and bound through #374 |
| `push_ref` wrote only `git/refs` — no git-object transport, remote 422 | #411; worked around via Git Database API staging plus a bridge push; addressed by PR #413 (pack upload from the handoff repository under `--force-with-lease`), merged as `bfa0412` | Object transfer is the hard primitive everywhere: coordinator→executor inputs and executor→coordinator produced objects both travel as real git transport (bundle/pack), never as ref updates against objects that do not exist |
| Contained sandbox lacked a CA trust store and a git identity | #412; worked around via `--runtime-arg` bind and `GIT_*` env injection; addressed by PR #413 (`ca_trust_paths`, `commit_identity` profile fields), merged as `bfa0412` | A remote executor host needs the same declared environment; it is a preflight requirement of the executor profile, not per-run surgery |
| No executor transport or placement port | `ProcessControlPort` (`workloop-core/src/ports.rs`) is POSIX-local; `Supervisor` spawns, inspects, and signals on the coordinator host only | The coordinator-to-executor operation set (§4) and versioned wire contract in [executor transport contract](executor-transport-contract.md) are new; placement is bound per request like the adapter declaration, never inferred mid-Invocation |
| Workspace and `source_path` are local filesystem identities | [v0.3 installation](v0.3-installation.md#v03-installation-record-keys): `worker_dispatch.source_path` is an absolute coordinator path | A local path is never a transferable remote artifact identity; inputs cross as content-addressed bodies plus a git object closure (§4.1) |
| No remote durable receipt store, incarnation identity, or remote stop evidence | first live run reconciled everything by hand | Executor-side durable receipts and a boot/incarnation identity are required before any remote start; the coordinator reconciles by recorded identities after any restart (§4.3) |

The gaps also confirm the boundary lesson the milestone bakes in: the
first live run's publication only completed when an operator staged
objects and pushed by hand — exactly the custody path this spec makes
a first-class, transport-carried operation.

## 2. Boundary: one coordinator, one static remote endpoint

### 2.1 Components

| Component | Definition | Owns | Never does |
| --- | --- | --- | --- |
| **Coordinator** | The existing single installation: Core (`work` decisions, `projection`, store), the `execution` supervisor, the CLI/`taisan run` loop, the intake scheduler, and the in-process host driver | Authorization, capacity, budgets, retry/finding decisions, repository-host effects, Outcome, durable identity allocation | Run Worker process trees on the remote endpoint directly; delegate a decision |
| **Executor endpoint** | One static, configured remote Linux host. Contents: an executor helper exposing a fixed operation set, the approved containment profile, a locally installed conforming Worker adapter, and executor-local durable state (receipts, WIP, staging) | Bounded execution of exactly one dispatched request at a time per slot it is granted, process/artifact custody on its own filesystem, honest capability reporting | Choose Work, dispatch a next stage, reset budgets, read a coordinator store, or hold a repository-host credential |
| **Executor adapter** | Coordinator-side port implementation that speaks the executor transport to the configured endpoint | Framing, authentication, retries within recovery policy, translation between supervisor operations and endpoint operations | Decide outcomes, infer success, or silently fall back to local execution |
| **Worker** | Any conforming adapter behind `worker_argv` on the endpoint — the same `taisan-worker-claude` reference or an independent contract Worker | One bounded Invocation: translate the request, run the runtime, emit the report | Publish, retry outer attempts, or change Work state — unchanged from [worker protocol](worker-protocol.md#roles-and-authority) |

The endpoint is static and declared: one configured address plus its
authentication binding in the installation record. There is no
discovery, pooling, balancing, or autoscaling. The reference executor
runs at most one execution slot; capacity admission stays entirely
coordinator-side.

### 2.2 Two independent pluggability axes

| Axis | Selected by | Binding rule |
| --- | --- | --- |
| **Worker implementation** | `worker_argv` plus the `describe` declaration, as today | First binding wins per request; a running Invocation never migrates adapters |
| **Execution placement** | Per-role placement in configuration (`local` or the configured remote endpoint), plus a separate verification placement | Recorded on the durable request before dispatch, like the adapter binding; a configuration reload affects only requests not yet bound and never retargets a running or unproven one |

The two axes are orthogonal by construction — the same rule the stage
contract already enforces for the reviewer axis
(`SUPPORTED_MATRIX_CELLS`, `validate_review_matrix`). An unsupported
combination is refused at configuration validation, never silently
narrowed: a remote request to a Worker that does not declare the
required role/capability fails closed, and an unavailable endpoint
yields `unavailable` evidence plus Attention — never a local
re-dispatch under the same request identity.

### 2.3 What Core retains — unchanged authority

Everything in [the ownership matrix](adapter-boundaries.md#ownership-matrix)
and [one owner per field](v0.3.md#8-one-owner-per-field-and-per-decision)
stays where it is. For the remote boundary specifically, the
coordinator keeps sole authority over:

- **Admission and dispatch**: Command decisions, dispatch
  preconditions, request identity, spec/base/head binding, budget
  construction.
- **Capacity and reservations**: the Execution slot, workspace and
  `(repository, branch)` reservations, external claims. A remote
  writer is one reservation holder exactly as a local writer is.
- **Budgets**: `max_invocations`, `max_review_rounds`, runtime and
  Work-wide limits, artifact-recovery bounds, transport retry policy.
  A remote Invocation consumes the same counters; a retry or
  continuation is a new request/invocation minted only by Core.
- **Retry and finding decisions**: `retry`/`resume`/`cancel`,
  `judge_findings`, `release_judgment`, finding dispositions,
  equivalent-failure escalation.
- **Repository-host effects**: the complete `HostActionIntent` outbox
  and its receipts — push, change-request create/update, comments,
  review feedback, thread actions, ready. No remote path publishes.
- **Outcome**: assessment, completion, and current Attention. Remote
  reports and collected artifacts are evidence only.

The executor's authority ends at faithfully executing one bound
request and preserving its evidence. It selects placement of nothing,
sequences nothing, and declares nothing complete.

## 3. Two execution modes on existing ship/converge postconditions

Stage postconditions are executor-independent: a stage command defines
its outcome identically in every executor cell
([architecture](architecture.md#stage-contracts-are-executor-independent)).
Both modes below reuse the existing lifecycle — `submit` (or intake)
→ Coding → supervisor verification → publication → review round →
judgment → bounded Fix → republish → reassessment → Outcome — with no
new verbs, no wrapper pipeline, and no second orchestrator on the
endpoint. `ship` names the new-Issue-to-PR path; `converge` names the
bounded review/fix-to-exact-head path, including admission of an
existing PR where supported (#382). Placement changes which
Invocations run remotely; it changes nothing about what must be true
when the stage finishes.

### 3.1 Mode A: review-first mixed mode

| Role / stage effect | Placement | Custody needed |
| --- | --- | --- |
| Coding, Fix | local | existing local handoff |
| InvariantCheck | local initially; remotely placeable under Mode B's contract once #381 lands | — |
| Review | remote | target object closure + materialized `review_source` out; bounded report back — no produced objects |
| Verification | local | unchanged |
| Repository-host effects | coordinator | unchanged |

Why Review goes first:

- It is the read-only role — `workspace.read_only`, no writable source
  tree, no produced commits to collect — so it exercises transport,
  binding, stop, and report custody without the producing-artifact
  risk of remote Coding.
- Its inputs are exactly the immutable assessment set the v0.3 wire
  contract already pins (`target` = change/base/head/diff digest,
  `review_source` = retained snapshot), so remote execution adds no
  new freshness semantics: the coordinator materializes the same
  bodies it would mount locally.
- It delivers milestone value earliest: review latency/queue relief
  moves off the coordinator host while writers — the riskiest custody
  — stay on the proven local path (#380).

### 3.2 Mode B: all-remote compute

| Role / stage effect | Placement | Custody needed |
| --- | --- | --- |
| Coding, Fix, InvariantCheck, Review | remote | full input transfer; produced-object collect + acknowledge for writers; report retrieval for all roles |
| Verification (independent, supervisor-authorized) | remote | produced-commit worktree built from collected/validated objects; argv/environment binding unchanged (#383) |
| Repository-host effects | coordinator | unchanged — publication, comments, reviews, ready all drain the coordinator outbox |

Mode B reuses the identical stage graph. A remote Coding Invocation's
produced commit becomes publishable only after collection into the
coordinator's quarantined import and the existing verification/gate
chain — the remote writer cannot shorten the path to publication, and
the `ConvergeRecord` rule still classifies a claimed-but-unpublished
fix as failure.

### 3.3 What neither mode may do

- Wrap or re-implement a standalone ship/converge pipeline on the
  endpoint. The endpoint never runs a next-stage decision; every
  transition is a coordinator-authored request.
- Add remote-specific verbs, Commands, report statuses, or Outcome
  kinds. Parity is proven by running the unchanged stage postconditions
  in each placement cell.
- Admit a foreign PR or mutate a Work's target by remote evidence.
  Existing-PR admission binds provenance coordinator-side (#382)
  before any remote review or Fix is dispatched.
- Fall back to local execution when the remote endpoint is
  unavailable. Placement is part of the bound request; an unavailable
  endpoint is typed evidence plus Attention, and continuing the Work
  requires a Core decision (new request), not a silent retry elsewhere.

## 4. Custody: transfer, retention, and authority

The executor contract has six operations. This section fixes their
required semantics. The versioned envelopes, identities, digests,
refusal vocabulary, wire examples, and fault matrix are defined in the
[executor transport contract](executor-transport-contract.md) for
#374. Wire types in #375 implement that contract. Restricted SSH plus
bounded framed messages remains the starting transport candidate. The
reference transport is selected after #373's environment decision;
every conforming transport must provide the same semantics.

| Operation | Coordinator issues | Required semantics |
| --- | --- | --- |
| `prepare` | input transfer + staging | Delivers the canonical request body, all referenced immutable bodies, and the git object closure (§4.1). The executor validates digests and reconstructs its own workspace; a body that fails validation refuses before any process exists |
| `start` | launch one bound request | Durable start under recorded identities; the answer distinguishes `started`, `known_not_started`, and `unknown`. A lost or ambiguous response reconciles by request identity — never a second start while the first is unknown |
| `observe` | poll remote state | Returns remote process evidence, bounded progress, and receipt identities for the recorded Invocation. Silence is `unknown`, not `dead`; reconciliation after reboot reports both the bound and responding incarnation |
| `request_stop` | stop one Invocation | Persisted remote stop intent, bounded SIGTERM/grace/SIGKILL escalation, inspected remote stop proof covering the descendant snapshot and the executor incarnation. A closed session or expired heartbeat is never stop evidence |
| `collect` | retrieve outputs | Bounded, digest-verified retrieval of the report, retained log, and — for producing roles — the produced object closure through quarantine, with base-ancestry and exact-commit checks before coordinator import |
| `acknowledge` | confirm durable receipt | The executor retains remote WIP/output until the acknowledgement command is durable. If that command never arrives, it retains the bodies. If the command commits but its response is lost, it may clean up under retention policy and replays its durable tombstone without recollection |

### 4.1 Input and artifact transfer

- The transfer unit set for one request is: the canonical
  `taisan.worker.request/2` body; every immutable body it references
  (goal, ordered Issue snapshots, spec, `review_source`,
  `invariant_set`, predecessor artifact descriptors, required
  continuation bodies); and the git object closure the request needs
  (base commit closure for writers; the exact target commits for
  review/invariant roles). Bodies are content-addressed and carry the
  digests the request already binds.
- Trusted source acquisition is coordinator→executor only (#374).
  The executor never fetches the repository host, the coordinator's
  filesystem, or any location a path names: a local filesystem path
  is never a transferable artifact identity, and no `source_path`
  value crosses the wire.
- The executor reconstructs the managed workspace from validated
  objects — the `ws-<work_id>` checkout on branch `work/<work_id>` at
  the pinned base — into its own state root. There is no shared working tree, shared object store, network
  filesystem, or ambient home mount between coordinator and executor.
- Produced output returns the same way it leaves locally: for
  writers, the sandboxed export (`base..produced` object closure plus
  manifest) collected into coordinator quarantine; for all roles, the
  bounded report and log. A SHA, a remote path, or a Worker success
  report alone never establishes the artifact — the same rule the
  local handoff enforces.
- Transfer is bounded and restartable: bounded chunks, safe paths,
  partial-transfer staging, checksum validation, atomic finalization.
  Interruption, corruption, symlink/traversal attempts, and foreign
  request bodies are typed refusals, never repairs (#376).

### 4.2 Output retention and acknowledgement

- Until `acknowledge` lands durably on the coordinator, the executor
  retains the Invocation's workspace, out directory, staging area,
  and receipts. This is the remote mirror of the coordinator's
  protective pins — an unacknowledged handoff pins its workspace and
  out directory ([retention policy](retention-policy.md#dependency-pins)).
- After acknowledgement, remote cleanup is scoped to the acknowledged
  Invocation, bounded, and retryable; it follows the same
  two-phase-mark discipline so an interrupted cleanup can be resumed
  without losing evidence. Remote retention is executor-local policy
  informed by the coordinator's acknowledgement — it never requires
  remote read access to the coordinator store.
- Repeated collection of an already-acknowledged Invocation is
  idempotent and produces no duplicate artifacts; acknowledgement of
  an Invocation whose collection never completed is refused rather
  than recorded.
- The executor durably records an acknowledgement before it removes any
  remote body. If the acknowledgement command never arrives, the executor
  retains the body. If the acknowledgement commits but its response is lost,
  the executor may clean up under retention policy and replays the stored
  acknowledgement tombstone without requiring recollection.
- Coordinator-side availability states are unchanged: a remote body
  that cannot be collected within the artifact-recovery bound resolves
  to `unavailable`/`absent` evidence with its reason — never to a
  fabricated or empty result.

### 4.3 Authority at start, stop, and reconnect

- **Identity before send.** The coordinator persists the endpoint
  binding, request/invocation identities, launch configuration digest,
  and command identity before transmitting any operation; duplicate
  replay is idempotent, mismatched bytes are a conflict (#375).
- **Start authority.** Only the coordinator may authorize a start, and
  only for a bound request. `known_not_started` requires the endpoint
  to prove no incarnation ever ran it; `unknown` holds the slot and
  blocks replacement — the remote mirror of the local `Unknown`
  launch rule.
- **Stop authority.** Stop intent is durable on both sides before
  signals. Remote stop proof requires inspected remote process
  identities plus the executor incarnation that ran them; an executor
  reboot/incarnation change makes prior process evidence
  uninspectable and is handled as a typed class, not assumed death.
  Lease expiry, heartbeat loss, session closure, and log silence are
  never stop proof (#378).
- **Reconnect.** A transport disconnect neither kills work nor
  completes it. Reconnect re-attaches to the same recorded Invocation,
  its immutable binding, and bounded progress/output; a coordinator
  restart reconciles against durable receipts, and an executor restart
  surfaces through its incarnation identity.
- **Fencing.** The replacement-writer rule is unchanged: no new writer
  starts while an old one is unproven. The remote analogue adds one
  fence — a request bound to endpoint incarnation *i* cannot be
  adopted by incarnation *j* or by a different endpoint; ownership
  transfers only through the coordinator's normal
  stop-proof-then-new-request path.
- **Budgets carry absolute time.** The request's deadline travels as
  the absolute `deadline_unix_seconds`; clock skew between the hosts
  may consume budget but can never extend authorized runtime.

### 4.4 Hard constraints

| Constraint | Consequence |
| --- | --- |
| No remote SQLite/store access | The executor has no store handle and no query surface into coordinator state; everything it needs arrives in the bound request and its bodies |
| No shared working tree | Independent filesystems are a hard requirement of the reference environment, not an optimization: no shared workspace, no shared CAS directory, no home mount. The #373 feasibility proof runs over real transport on separate roots |
| No remote repository-host mutation credentials | The endpoint receives at most the Worker's runtime credential, delivered through the same `/taisan/runtime-auth` mechanism; repository-host credentials, helpers, agent sockets, and keyrings are absent — the same containment rule as the local profile, plus the endpoint-side bootstrap keeps host-management credentials unreachable from the Worker |

## 5. How remote execution inherits the v0.3 guarantees

| Guarantee | Inheritance rule |
| --- | --- |
| Exact-head binding | The review/invariant/fix target stays the immutable triple the request binds. A head or base move observed by the coordinator invalidates remote evidence exactly as local evidence: a remote review of a superseded head is retained history and cannot settle the current target |
| Budgets | Request budget fields are immutable and travel verbatim; remote execution consumes the same Invocation/review-round/fix and runtime counters; nothing about a remote path resets, refunds, or replenishes a bound |
| Retention | Coordinator pins and `body_catalog` states are unchanged; remote bodies exist only as transfer/acknowledgement subjects (§4.2) and are reclaimed under the coordinator's own policy once collected |
| Stop-proof | The same intent→snapshot→signal→inspect order, with remote incarnation evidence added to the inspection set; unreachable is `uncertain`, never `stopped` |
| Publication guards | All repository-host effects drain the coordinator outbox under the original actor/credential binding with per-effect receipts and read-back reconciliation. The remote path can produce at most objects for collection — never a ref update, comment, review, or ready transition |
| Reviewer axis | Remote Review consumes the same sealed round/judgment machinery; the executor axis cannot narrow the reviewer axis, exactly as `validate_review_matrix` already requires |
| v0.3 local behavior | A local-only installation keeps identical semantics — no new mandatory configuration, no changed request/report meanings, and placement defaults preserve today's launches |

## 6. Acceptance rows

These rows are the v0.4 acceptance contract. Each names the evidence a
run must retain and the claim class it may support. Fixture/contract
rows are proven by the dependent issues' suites (#375–#384); a
`live` row additionally requires the separately authorized owner-run
acceptance (#386), with reference-environment feasibility decisions
scoped to #373 and listed in §7.3 — never solved here.

| ID | Scenario | Mode | Required evidence (all retained with identities) | Claim class |
| --- | --- | --- | --- | --- |
| RA-01 | Remote review on an exact PR head | A | Materialized exact `target` + `review_source`; remote `review` Invocation bound per §3.1; complete findings ingest into the sealed round; a head move before ingestion invalidates the round and cannot settle the newer head | contract + live |
| RA-02 | New Issue → PR (ship) | B | Selector/manual admission → remote Coding → collect → coordinator import → verification → coordinator `push_ref` + open → remote Review → judgment → remote Fix → republish → verified Outcome. Each stage's postcondition is the local one; the produced head reaching the PR is proven by coordinator receipts, not remote claims | contract + live |
| RA-03 | Review / fix / republish (converge) | A+B | Judged findings → remote Fix bound to exact predecessor/round/judgment → collected commit → verification → guarded republish → `ConvergeClassification::Fixed` classification against the live head; a claimed-but-unpublished fix is `UnpublishedFailure` | contract + live |
| RA-04 | Attention surfaces | A+B | Transport loss mid-Invocation, unreachable endpoint at collect, `unknown` start, executor incapacity — each produces typed evidence and a current Attention item; none silently retries, falls back local, or completes | contract |
| RA-05 | Outage and partition | A+B | Partition at prepare/start/observe/request_stop/collect/acknowledge: reconnect recovers the same Invocation and binding; no duplicate writer, no lost report, no premature remote cleanup; outage is `unavailable`, never an empty result | contract + live (where repeatable) |
| RA-06 | Coordinator and executor restart | A+B | Coordinator restart reconciles every in-flight remote Invocation by recorded identity; executor reboot surfaces a new incarnation, prior process evidence becomes uninspectable as a typed class, and no replacement writer starts until remote stop is proven or the request is abandoned under the normal path | contract + live |
| RA-07 | Negative boundary probes | A+B | Remote Worker attempts repository-host access, coordinator store access, or a write outside its grant — denied by the containment/endpoint boundary; executor holds no repository-host credential; preflight failure refuses dispatch | live negative |
| RA-08 | Interchange | A+B | The same contract suite runs an independent deterministic Worker and the reference Worker across local and remote placement where capabilities permit; configuration-only swap of Worker and of executor endpoint binding; no adapter-name branches in the common suite | contract |

## 7. Open decisions and downstream dependencies

### 7.1 Fixed here versus decided downstream

This document fixes the boundary, the two modes, the custody
semantics, the hard constraints, and the acceptance rows. The
following remain open for their owning issues — deliberately, because
the milestone splits contract, mechanism, and environment:

| Open item | Owner | Why left open |
| --- | --- | --- |
| Reference environment (native Linux vs minimal guest on Mac, memory footprint, on-demand start/stop) | #373 | Measurement must precede selection; this spec constrains only the shape (one static Linux endpoint, independent filesystems, no mandatory Docker) |
| Executor transport and wire envelopes (`taisan.executor.*` command/receipt/observation/artifact types, digest tags, refusal vocabulary) | #374 defines the contract; #375 implements its wire types | Transport selection follows the environment; restricted SSH plus bounded framed messages is the recorded starting candidate |
| Executor helper implementation shape (reuse of supervisor components, durable receipt store, process incarnations) | #377 | Mechanism, not contract; must satisfy §4 semantics and the minimal-executable constraint |
| Input/object transfer mechanics (chunking, staging, quarantine layout) | #376 | Implementation of §4.1–4.2 |
| Fault reconciliation test surface | #378 | Implementation of §4.3 |
| Existing-PR admission entry path | #382 | A separate minimal entry-path gap; this spec requires only that admission bind provenance before remote dispatch |
| Remote verification environment (toolchain/cache staging, placement binding) | #383 | Extends §5's verification relocation; local verification remains a supported configuration |
| Interchange suite rows and packaging/quickstart | #384/#385 | Compose the proven slices |
| Automatic event-driven review arming (#309–#311, #322) | parallel milestone items | Remote review must not depend on webhook ingress; polling and the bounded reconciler supply arming facts. The milestone's automatic-review acceptance rides on those issues, not on this boundary |

#416 is not an open item in this table because it is already decided: the remote handoff manifest is a fresh per-Invocation out-directory record (`handoff.json`) beside `report.json`. The [executor transport contract](executor-transport-contract.md) defines its schema, size and path bounds, collection digest, whitelist, and acknowledgement rules. This spec consumes the decision; it does not treat the workspace-root `artifact.json` as a remote authority.

### 7.2 Intentionally open design points

Recorded so downstream issues can depend on the answer existing, not
on this document having guessed it:

- **Executor incarnation identity format** — #374 defines a fresh
  unpredictable identifier per executor process start, bound to the
  authenticated endpoint identity. #377 selects the durable receipt
  store and verifies that the identifier changes on reboot.
- **Remote workspace retention** — §4.1 fixes construction: the
  executor builds the `ws-<work_id>` workspace on branch
  `work/<work_id>` from validated objects inside its own state root. Whether the executor may then retain and reuse that
  constructed workspace across Invocations of one Execution (the
  local persistence model) or must discard and rebuild it is
  #376/#377's trade-off — a retention/optimization question over the
  fixed construction rule, not an alternative to it; §4.2 requires
  WIP retention until acknowledgement either way.
- **Bounded progress surface of `observe`** — the contract returns
  process facts, a monotonically increasing progress sequence, and
  receipt identities. A log tail is optional bounded diagnostic data,
  never a liveness substitute.
- **Clock skew treatment** — the contract carries absolute deadlines.
  Skew may consume budget but never extend it. The installation pins a
  maximum clock offset; the executor refuses new starts when measured
  offset exceeds that limit.

### 7.3 Feasibility decisions referenced, not made (#373 scope)

The public reference environment must demonstrate, on a measured
host: independent coordinator and executor roots with real transport;
no shared store/workspace/home mount; bounded incremental resident
memory with honest accounting; on-demand start and safe stop that
respects live Invocation ownership and retained WIP; and a pinned
single recipe with tested platform versions. Docker is optional, never
required. Those are environment measurements and packaging decisions
— this spec constrains the contract they must satisfy, nothing more.

## 8. Non-goals

- Cloud provisioning, autoscaling, multi-tenancy, endpoint discovery,
  more than one remote endpoint, or a second coordinator.
- Auto-merge, deploy, or any repository-host write from the executor.
- Native macOS containment or native remote control planes; "Mac/Linux"
  in the milestone names where the *reference environment* runs.
- Remote admission of unmanaged writers, shared credentials, or a
  remote operator surface. The v0.1-era omissions (remote
  admission/SSH/container infrastructure, embedded remote-publication
  pipelines — [design closure](design-closure.md)) stay omitted in
  spirit: v0.4 re-models exactly one bounded remote executor, it does
  not re-import the source systems' infrastructure.
- Provider coupling: no GitHub-, SSH-implementation-, or
  infrastructure-provider-specific type enters Core; the transport
  carries the neutral contract.

## 9. Non-claims

This document does not claim that an executor transport, a remote
endpoint, remote containment, remote artifact collection, remote
verification, placement configuration, or any live remote run exists.
It does not claim the first live run's operator workarounds are the
design — they are evidence for the gaps the design closes. It
authorizes no implementation, credential, endpoint, live model call,
host write, publication, or Work completion. Fixture and contract
suites prove modeled behavior only; live acceptance for any row in
§6 requires the separately authorized owner-run evidence defined by
#386.
