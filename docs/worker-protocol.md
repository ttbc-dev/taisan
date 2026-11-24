# Worker protocol, artifact handoff, and isolation

Status: concrete design for #52, stacked on the [identity/storage design](identity-storage.md) (#50, PR #57) and the [evidence and gate design](evidence-gates.md) (#51, PR #58). Documentation only. No runtime code, configuration, sandbox, proxy, credential installation, or live E2E is added or claimed. #50's comprehensive source behavior inventory is merged in `docs/extraction-coverage.json`; all 33 Worker-owned inventory targets are mapped to clauses and shared-suite cases — or to an approved deferral — in [worker requirements](worker-requirements.md). #51's [failure classification](evidence-gates.md#failure-classification-and-recovery-decisions) and [Work-wide accounting](evidence-gates.md#work-wide-recovery-accounting) govern normalized recovery decisions; Worker reason codes alone do not authorize retries.

The [v0.3 executable request and report contract](v0.3.md) reconciles this document with the implemented narrow `WorkerRequest` export, the `execution` supervisor, and the independent contract Worker at baseline `51f0b34daf33a1975a39b7614f5ceeac7dabf431`. It inventories actual versus designed behavior, publishes provider-neutral versioned request/report schemas and wire examples, and defines the cutover that preserves historical v1 request digests, bindings, and evidence. Its [operational launch, verification, and host composition section](v0.3.md#14-operational-launch-verification-and-host-composition) selects the single-host v0.3 mount/credential/provider-config delivery, model-API egress, verification toolchain and allowlist, managed process lifecycle, in-process host driver, and compatibility rules. Where this document's v0.1 schema and the v0.3 wire contract differ, the v0.3 document is authoritative for new requests; this document remains the design of record for the v0.1 behavior and gates.

## Owner decisions

The owner chose these in the #52 working session on 2026-11-07:

| Decision | Choice | Rejected alternatives |
| --- | --- | --- |
| Sequencing | Stack on #58. Map Worker-owned behavior provisionally to R01–R38 and #51 failure classes, and record that the mapping must be redone against #50's inventory | Waiting for #50; building a separate Worker-only inventory inside #52 |
| Reference runtime and entry point | Claude Code CLI in headless mode (`claude -p`, `--output-format stream-json`), launched by a taisan-owned shim (working name `taisan-worker-claude`) that is the configured `worker_argv`. One Invocation is one process tree | A resident HTTP-API agent runtime shared across Invocations (the worker repo's approach); a generic argv contract with no named reference |
| Resume capability | The reference adapter declares `resume: unsupported` for v0.1. Continuation is always a new Invocation whose inputs reference the retained workspace, the previous artifact, and the previous report. Runtime session state is discarded and is never evidence | Declaring `--resume <session>` as an optional capability |
| Mechanical verification runner | The Worker's own check results are claims. Authoritative `VerificationResult`s come from the supervisor, which runs each declared argv against a clean worktree of the handed-off commit under the same isolation profile. This is not a Worker Invocation and does not consume `max_invocations` | Accepting Worker-reported check results after argv/commit/digest matching |
| Durable git-object handoff | After confirmed exit and stop proof, import the produced commit closure into an installation-owned bare **handoff repository** under `refs/taisan/artifacts/<artifact_id>`. Verify the closure, record a canonical manifest digest, and treat the committed store record plus the existing ref as the durable acknowledgement | A `git bundle` file as the durable body; bundle and ref together |
| Isolation | bubblewrap (mount, PID, network and IPC namespaces) plus environment/credential hygiene, plus a **mandatory** model-API allowlist egress proxy (N1) in v0.1 | Hygiene only; a separate UID; Landlock only; cgroup only; containers; a shared network (N0) |

The owner approved these refinements in the #52 session on 2026-11-08:

| Decision | Choice | Rejected alternatives |
| --- | --- | --- |
| Export transport | A sandboxed `git bundle` of `base..produced` as the transient transport: the export git invocation runs inside the sandbox with hooks and fsmonitor disabled, and trusted code reads the bundle only as data. The durable body remains the imported object closure under `refs/taisan/artifacts/<artifact_id>` plus the `acknowledged` store row | Trusted git commands against the live workspace; a direct fetch that lets the source repository's config drive pack generation; raw `.git/objects` copying; the bundle file itself as the durable body |
| Verification egress | The verification profile denies network by default. A Work may declare required verification hosts in its immutable capability requirements at admission; the effective egress set is the installation-level verification allowlist plus the Work's declared hosts, enforced by the same CONNECT-proxy mechanism. The set never contains model-API hosts and the profile never carries runtime credentials | An always-denied verification network; unrestricted egress; sharing the Worker model-API allowlist |
| Numeric defaults | Approved as proposed: `--max-turns` 200, 10 s stop grace, 1 MiB report cap, 32 MiB log cap, 30 min per-command verification timeout, 8 MiB per-command output cap, zero shim-level retries | — |

These choices shape implementation units U2 and U4 in the [implementation plan](implementation-plan.md). They do not authorize implementation, and no runtime or live behavior is claimed here.

The v0.3 executable request/report contract has its own open owner
decisions ([v0.3 §10](v0.3.md#10-open-owner-decisions-blocking-dependent-implementation)).
Those decisions block the dependent implementation; this table remains
the v0.1 design record.

## Roles and authority

| Component | Owns | Never does |
| --- | --- | --- |
| Core (`work`) | Authorizes each Invocation request, its role/task/budget, required artifacts, and the immutable capability requirements; classifies evidence; decides transitions | Launches processes, reads workspaces, parses runtime output |
| CLI loop | Drives `execution` ticks, timed sweeps, export, verification, and reconciliation | Decides which guard may be skipped |
| `execution` supervisor | Persists launch/stop intent, spawns the sandbox, records process identities, proves stop, exports and imports artifacts, runs verification, records receipts | Infers success from exit status; reads Worker prose as evidence of an effect |
| Sandbox launcher (bubblewrap and egress proxy) | Enforces filesystem, credential, network, and process-namespace limits for one Invocation | Holds repository-host credentials |
| Worker adapter (shim) | Translates one request into the runtime's input; runs the runtime as its child; translates the runtime's result into a `WorkerReport` | Pushes, publishes, comments, retries outer attempts, starts another Invocation, decides an Outcome, or hands a running Invocation to another adapter |
| Worker runtime (Claude Code) | Plans and edits inside the workspace; makes local commits; may run checks as claims | Anything outside the sandbox |
| Repository host adapter | Remote writes from the handoff repository under `HostActionIntent`s | Reads the Worker workspace directly |

Worker-runtime authentication, session handling, prompt construction, tool permissions, and stream formats remain inside the adapter. None of them appears in Core types or transitions.

## Worker protocol

### Entry point and files

The target design starts one sandbox per Invocation. Inside it, `worker_argv` runs as:

```text
<worker_argv...> run --request /taisan/request.json --out /taisan/out
<worker_argv...> describe
```

This is the target `argv-v2` transport specified in
[v0.3](v0.3.md#41-transport), not the behavior implemented at the
v0.1/v0.2 baseline. At `51f0b34` the supervisor appends `describe` to
the bound argv for the declaration probe and otherwise spawns the bound
argv with the managed workspace as cwd under the environment profile; it
starts no namespace sandbox, writes no request file, and binds no out
directory. The fixture and contract Workers are exercised
through their test modes, not through this transport.

`describe` is effect-free from the worker's perspective: it prints the adapter's `WorkerCapabilities` and runtime version, and is run at setup validation and before dispatch to bind the exact declaration into the Invocation request. It is still a spawned process, so it runs through the supervisor's process-control port in its own process group with a bounded wait; a wedged probe is killed with its group. `describe` is fail-closed: when the probe does not answer with a declaration — spawn refused, bound elapsed, non-zero exit, unparseable body, or no adapter identity — the adapter is not selectable and no synthetic declaration is compared or dispatched on. The request is refused as `adapter.declaration_unavailable`, a configuration repair that never masquerades as a version mismatch or an empty capability surface. A request whose Execution can no longer launch is never probed at all.

| Path inside the sandbox | Owner and mode | Contents |
| --- | --- | --- |
| `/taisan/request.json` | Supervisor writes before spawn; read-only bind | Canonical `WorkerRequest`; its digest is stored with the request |
| `/taisan/out/` | Fresh per-Invocation directory under the installation state root; read-write bind | `report.json`, `runtime.log` (bounded stream copy), optional `checkpoint.json`, and assessment scratch |
| `/workspace` | The Execution's managed workspace; read-write bind for `coding`/`fix`, read-only bind for `invariant_check`/`review` | Git worktree on `work/<work_id>`; the only writable source tree for a writer |
| `/taisan/runtime-auth` | Read-only bind of only the selected runtime credential file, if the credential reference resolves to a file | Never repository-host credentials |
| `/taisan/provider-config` | Supervisor-rendered, read-only bind | Non-secret model/provider configuration (model identity, endpoint host, limits, empty MCP config); no credential value and no workspace override |

Provider-config delivery, the runtime credential reference, the
model-API CONNECT proxy, the verification toolchain/cache, and the
child-runtime-server lifecycle are selected in
[v0.3 §14](v0.3.md#14-operational-launch-verification-and-host-composition).
The v1/v0.2 implementation has no mount set at all: this table is the
target profile, not the launch behavior at the rechecked baseline. The
out directory belongs to exactly one Invocation ID and is never reused.
Any file the Worker writes elsewhere does not survive the sandbox and
cannot be evidence.

### Implementation status at `51f0b34`

The implemented export is not the schema below. `WorkerRequest` in
`workloop-core/src/worker.rs` currently carries only `format_version`
(`taisan.worker.request/1`), `kind`, `invocation_id`, a derived
`request_id` (`worker:<invocation_id>`), `profile_id`, and
`workspace_root`; its digest uses tag `worker-request-v1`. The
supervisor commits that row before launch and binds it to the spawned
identity. It does not deliver the row as a request file, does not bind
an out directory, and does not read a report. Required artifacts are
discovered in the workspace and reported as present/corrupt/missing
facts; process exit and stop proof come from the process-control port.
The role/task/report shapes below are therefore **design targets**, not
compiled APIs or live behavior. The [v0.3 inventory](v0.3.md#2-inventory-actual-versus-designed-at-51f0b34)
records each exact gap and the reuse boundary. The
[v0.3 operational mapping](v0.3.md#141-existing-implementation-mapped-to-the-required-boundary)
separates what is declared, what is fixture/contract evidence, and what
is actually enforced: at the rechecked baseline the launch is hygiene
only, the handoff and POSIX stop proof are implemented, and the
namespace/egress/read-only-mount/verification-toolchain profile is not.

### Schemas

Target Rust shapes, reusing #50/#51 identities; not compiled APIs. The
provider-neutral v2 wire contract for new requests and reports is
[v0.3](v0.3.md); the shapes here describe the v0.1 design surface.

```rust
struct WorkerRequest {
    format_version: WorkerFormatVersion,          // "taisan.worker.request/1"
    store: StoreId,
    work: WorkId,
    execution: ExecutionId,
    request: RequestId,
    invocation: InvocationId,
    role: WorkerRole,                             // Coding | Fix | InvariantCheck | CodeReview
    task: WorkerTaskBody,
    repository: RepositoryRef,
    base: CommitRevision,                         // exact revision checked out before start
    workspace: WorkspaceId,
    budget: InvocationBudget,                     // deadline, max_turns, output caps
    required_capabilities: WorkerCapabilityRequirement,
                                                // includes declared verification egress hosts (default empty)
    continuation: Vec<ContinuationRef>,           // prior artifact/report refs, never runtime sessions
}
enum WorkerTaskBody {
    Coding { goal: RetainedContentRef, issues: Vec<SourceSnapshotRef>, constraints: Vec<String>,
             verification_argv: Vec<Vec<String>> },
    Fix { goal: RetainedContentRef, predecessor: ArtifactId, origin: FixOrigin,
          verification_argv: Vec<Vec<String>> },
    InvariantCheck { artifact: ArtifactId, invariant_set: InvariantSetRef, spec: SpecRevision },
    CodeReview { target: DiffTarget, review_source: ReviewTaskSourceRef },
}
enum FixOrigin {
    Prepublication { failed_evidence: NonEmptyVec<EvidenceId> },
    Review { target: DiffTarget, round: ReviewRoundId, judgment: JudgmentId,
             findings: NonEmptyVec<OccurrenceRef> },
}
struct WorkerCapabilities {
    adapter: AdapterId,
    adapter_version: String,
    runtime: RuntimeIdentity,                     // e.g. claude-code + exact version
    protocol_versions: Vec<WorkerFormatVersion>,
    roles: Vec<WorkerRole>,
    resume: Unsupported,                          // v0.1 reference value
    checkpoints: CheckpointSupport,               // WorkspaceCommits for the reference adapter
    runtime_artifact_store: Option<StoreKind>,    // None for the reference adapter
    local_retries: DeclaredRetries,
}
struct WorkerReport {
    format_version: WorkerFormatVersion,          // "taisan.worker.report/1"
    invocation: InvocationId,
    request_digest: Digest,
    role: WorkerRole,
    status: ReportStatus,                         // Completed | Failed | Refused | LimitReached
    reason: Option<BoundedReasonCode>,
    claimed_head: Option<ObjectId>,               // claim only; handoff decides
    claimed_checks: Vec<ClaimedCheck>,            // claims only
    findings: Vec<ReportedFinding>,               // CodeReview
    invariant_items: Vec<ReportedInvariantItem>,  // InvariantCheck
    runtime_usage: Option<UsageSummary>,
    runtime_log: OutRef,
}
```

`BoundedReasonCode` is a closed set (`provider_refused`, `rate_limited`, `quota_exhausted`, `auth_unavailable`, `max_turns_reached`, `deadline_reached`, `runtime_crashed`, `invalid_request`, `unsupported_role`, `internal_error`). Provider text is kept only in the bounded `runtime.log` body, never in reason fields, Command results, or Attention text.

A pre-publication correction binds the exact predecessor artifact, Work/specification, failed mechanical/invariant evidence, and approved repair goal without requiring a PR identity. Review correction additionally binds the current diff target, immutable round, exact-set judgment, and selected findings. Core validates these references and freezes the fix goal before dispatch. Adapter role/task/body mismatches are invalid; a Fix report retains the `fix` discriminator and uses the same artifact fields as coding in the mirrored module contract. The serialized role names are `coding`, `fix`, `invariant_check`, and `review` for the respective Coding, Fix, InvariantCheck, and CodeReview variants. Both correction origins consume the same Work-wide fix account. A PR-less fix must not fabricate a ChangeRequestRef or inherit an unrelated review judgment.

### Role translation for the reference adapter

| Role | Translation | Report contents |
| --- | --- | --- |
| Coding / Fix | Prompt built from goal, source snapshots or finding occurrences, constraints, and verification argv. Edits and local commits allowed in `/workspace`. Publication tools and outer loops are disabled by the shim's tool allowlist and by the absence of credentials | `claimed_head`, optional `claimed_checks`; the commit itself is handed off by the supervisor |
| InvariantCheck | The supervisor prepares a read-only checkout of the exact artifact commit. Prompt lists every invariant ID of the approved set. Edits are not allowed (workspace bind is read-only for this role) | One `ReportedInvariantItem` per invariant; #51 decides coverage and validity |
| CodeReview | Read-only checkout of the exact `DiffTarget`. Prompt covers the immutable diff. `review_source` is a retained snapshot materialized by the supervisor before dispatch — the Worker never fetches remote review input itself; an absent, stale, or undeliverable source makes the request invalid and stops dispatch | `findings` for this single requested source; #51 seals the snapshot |

The shim runs `claude -p` with `--output-format stream-json`, a bounded `--max-turns`, an explicit tool allowlist and permission mode, and `--strict-mcp-config` with an empty MCP config. Flags are hygiene and translation, not enforcement. Exact flags and the stream schema are pinned to the runtime version recorded in `WorkerCapabilities`. A runtime version that differs from the declaration is a configuration repair, not a silent upgrade.

### start, observe, stop

| Operation | Contract | Reference implementation |
| --- | --- | --- |
| `start(request)` | Idempotent by `RequestId`. Returns `Started{identity}`, `NotStarted{reason}` (definitely no process), or `Unknown{reason}` | The supervisor persists the launch intent, then spawns bwrap. `NotStarted` only when spawn returns an error before any child exists (missing binary, preflight failure, sandbox setup refused). Any identity-capture failure after spawn is `Unknown` (existing `SpawnError::Unknown`) |
| `observe(invocation)` | Process facts from the supervisor; report and out files only after confirmed exit | Existing `inspect` plus PID-namespace state. Runtime stream lines are progress, not liveness proof |
| `request_stop(invocation)` | Durable stop intent before signal; a receipt is not stop proof | Persist intent and descendant snapshot, then signal the group; the v0.3 target adds SIGTERM and a 10 s grace before the SIGKILL escalation. See [process descendants](#process-descendants-and-stop-proof) |
| `resume` | Not supported in v0.1 | Core never requests it; an adapter declaring it unsupported cannot be selected for a Work that requires it |

A replayed request with an existing launch intent never spawns again: the supervisor inspects the recorded identity or stays `uncertain`. A launch whose outcome was never recorded — a `prepared` Invocation carrying no process identity — resolves to one of four outcomes once the process table is scanned for the recorded launch signature (the bound argv): a single unrecorded signature match whose working directory lands inside the launch directory is re-bound as the writer; after the bounded absence window with no unrecorded signature match, no unexplained process in the launch directory, and no live unrecorded member of the recorded launch group — a descendant that changed both argv and working directory stays in its leader's group unless it left the group entirely — `setsid`, `setpgid`, or joining another group in the same session — and the group enumeration is the last handle on that escape — absence is proven: the writer is reported `Exited`, the Invocation is `abandoned`, and the Execution pauses for an operator decision; anything ambiguous or unreadable stays `uncertain`, since a guess never binds or abandons a writer; and a bounded count of consecutive unprovable scans — a failed absence verdict or a scan that could not run at all, such as an unreadable process table or an unmappable candidate — escalates to the distinct `absence_unprovable` outcome — the Invocation is marked `unknown` and the unprovable absence is surfaced to the operator instead of an indefinite silent hold. The escalation is recorded once and is not a conclusion: the launch stays an unresolved writer whose slot is never released, the absence scans keep running underneath, and a later pass that can prove an answer still resolves it — a re-bound writer is handled by the normal stop path under the recorded desired action, and a completed clear settles the Invocation `abandoned` and the Execution `paused`. The recorded group also verifies its owner before members count: `pgid` is the spawned leader's pid, and a live process at that slot whose start differs from the recorded leader's means the number was recycled after the leader died — the whole group is a foreign process family and contributes no member to the proof in either direction. The named residual is the escape that severs the group record itself — `setsid`, `setpgid`, or joining another group in the same session — plus a chdir and argv change: such a descendant is indistinguishable from a truly absent writer. A running Invocation is never moved to a different adapter. Swapping `worker_argv` in config affects only requests not yet bound — a request still unbound at its first binding takes the declaration current then, whenever it was created. Each request stores the adapter ID, adapter version, runtime version, capability digest, and the exact argv it was bound with.

The first binding recorded on the request — by the dispatch-admission probe, or at consume for a request no probe answered — is *the* stored declaration — immutable for the request's life. At launch the bound argv is probed again and must still declare the stored binding; a declaration that drifted under the same argv (for example a worker binary upgraded in place between binding and launch) is reported per differing field — `adapter.identity_mismatch`, `adapter.version_mismatch`, `runtime.version_mismatch`, or `capability.digest_mismatch` — as a configuration repair that authorizes no start. A request refused on that repair is **terminal for its binding**: it is abandoned and never resumed into the same repair. A legitimate runtime upgrade is accepted by issuing a **new** request, which binds the declaration current at its first binding; the operator abandons the refused request and re-issues the work — the stored declaration is never updated in place.

### Adapter-local retries

The shim performs no retry of its own: one `run` launches the runtime once. Retries inside the runtime (for example HTTP retries on transient API errors) are runtime-internal. They are declared in `WorkerCapabilities.local_retries` as `runtime_internal` and bounded by the Invocation deadline and `max_turns`. They never create new Invocations, new commits outside the workspace, or remote effects. Every outer attempt is a Core-authorized Invocation counted in `max_invocations`.

## Artifact handoff

This section describes the existing local handoff. The remote v0.4
manifest is separate transfer metadata in a per-Invocation out
directory. See [handoff manifest custody](handoff-manifest-custody.md)
for its identity, collection, and acknowledgement rules.

### Ownership

| Step | Producer | Consumer | Durable record |
| --- | --- | --- | --- |
| Local change | Worker runtime (commits on `work/<work_id>` in the workspace) | Supervisor export | Workspace HEAD and dirty state recorded at exit |
| Export | Supervisor, running `git bundle create` in a sandbox with the workspace read-only, no network, hooks and fsmonitor disabled | Supervisor import | Transient bundle in the Invocation out directory; deleted only after acknowledgement |
| Import and verify | Supervisor, outside the sandbox, fetching from the bundle into the handoff repository's quarantine | Core evidence classification | `artifact_handoffs` row and `refs/taisan/artifacts/<artifact_id>` |
| Verification | Supervisor verification runner on a clean worktree from the handoff ref | Core gates | `VerificationResult` bound to `ArtifactId` and the produced commit |
| Publication | Host adapter pushes from the handoff repository | Core assessment | `HostActionIntent` receipts (#51) |

The workspace's own git config, hooks, and attributes are never executed by trusted code. The supervisor reads only the bundle file, which is plain data, so a malicious workspace cannot run code in the supervisor.

### Steps and acknowledgement

1. Preconditions: confirmed exit of every recorded identity (stop proof), and an existing `PreparedChange` slot in Core for this Invocation.
2. Record `export_started{invocation, attempt}` durably.
3. In the export sandbox, resolve `work/<work_id>`, verify it descends from the request's `base`, and write `out/export.bundle` for `base..produced`. A dirty tree is recorded as WIP evidence; it is not part of the artifact.
4. Outside the sandbox: `git bundle verify`, then fetch the bundle into a quarantine namespace of the handoff repository. Run `git fsck --connectivity-only` from the produced commit; compute the canonical manifest `{format_version, repository, base, produced, tree, sorted object IDs of base..produced}` and its SHA-256 as `content_digest`.
5. In one store transaction: insert `artifact_handoffs{artifact_id, invocation, base, produced, manifest_digest, state: imported}`. Then update `refs/taisan/artifacts/<artifact_id>` to `produced` with an expected-absent guard, and commit `state: acknowledged` in a second transaction after reading the ref back.
6. The out-directory bundle and the workspace may be reclaimed only after `acknowledged` and when the [retention policy](retention-policy.md) permits.

Durable acknowledgement means the store row is `acknowledged` **and** the ref resolves to `produced` with a passing connectivity check. SHA-only report claims, files in `/tmp`, workspace paths, or a bundle file alone never establish handoff.

### Crash recovery

| Crash point | Observed on restart | Recovery |
| --- | --- | --- |
| Before `export_started` | Exit confirmed, no export row | Run export (first artifact-recovery attempt) |
| After `export_started`, no bundle or a truncated bundle | Bundle absent or `bundle verify` fails | Delete the partial bundle (it is not evidence), re-export from the workspace within the artifact-recovery bound |
| Bundle verified, import interrupted | Objects in quarantine, no `imported` row | Discard quarantine, re-import from the same bundle |
| `imported` row, ref absent | Row without ref | Recreate the ref from quarantine/objects after re-verifying the manifest digest; on mismatch mark the artifact invalid |
| Ref present, row still `imported` | Read back ref and digest | Commit `acknowledged` if they match; otherwise invalid with reason |
| Workspace deleted or lost before acknowledgement | No source for re-export | Absent: `artifact_recovery_exhausted` after bounds; never inferred from the claim |

Each recovery attempt counts against the existing artifact-recovery bound (3 attempts / 30 min by default), not `max_invocations`. A writer that may still be alive blocks export entirely.

### Evidence validity

| Evidence | Absent | Valid for the exact schema/identity/revision | Invalid (retain record and reason) |
| --- | --- | --- | --- |
| Request file | Not written: launch not started (`NotStarted`) | Digest equals stored request digest | Digest mismatch: refuse start, configuration repair |
| Start receipt / process identity | No identity after spawn: `Unknown`, hold ownership | `pid + start time + pgid` and PID-namespace init recorded | Identity belongs to another Invocation: `uncertain`, no replacement |
| `report.json` | Missing after confirmed exit: report-recovery attempt within the artifact bound, then failure class *unknown report* | Schema, `invocation`, `request_digest`, and role match | Malformed, oversized, or wrong Invocation: invalid report; process facts still decide stop |
| `runtime.log` | Missing: diagnostics unavailable; no gate depends on it | Bounded size, linked to Invocation | Truncated at the cap: retained, marked truncated |
| Workspace commit | Branch unchanged: `nothing_produced` for Coding/Fix | Descends from `base`, on the managed branch | Not a descendant, different branch, or history rewrite below `base`: invalid artifact |
| Export bundle | Absent: re-export within bounds | `bundle verify` passes and contains `produced` | Corrupt or contains unexpected refs: discard and re-export; repeated corruption becomes invalid |
| Handoff ref / acknowledgement | Absent: no publication, no verification | Ref equals `produced`; manifest digest matches the row | Ref differs, digest differs, or connectivity fails: invalid, never published |
| `VerificationResult` | Not yet run: requirement pending | Argv, environment profile, artifact, and commit exact; completed | Different artifact/argv/profile, or runner killed: `Inconclusive` or invalid, never passing |
| Capability declaration | `describe` fails: adapter not selectable (`adapter.declaration_unavailable`, fail-closed — no synthetic declaration is compared) | Fresh `describe` of the bound argv matches the request's stored binding field-for-field | Per-field mismatch with the stored request binding: configuration repair, no start, terminal for that binding (abandon and re-issue under a new request) |
| Isolation preflight | Not run: dispatch refused | All probes pass for this profile | Any probe fails: dispatch refused with repair prerequisite |
| Stop proof | Not established: `uncertain` | Every recorded identity stopped, namespace init gone | Contradictory inspection: `uncertain`, Attention after threshold |

The Worker's `claimed_head` must equal the handed-off `produced` commit; a mismatch is retained as an invalid claim, and the handoff (not the claim) is the artifact. Claimed checks never satisfy a mechanical requirement.

### Checkpoints

The reference adapter declares `checkpoints: WorkspaceCommits`: WIP that the runtime committed on the managed branch remains in the workspace and is the continuation input for a later Fix Invocation via `ContinuationRef`. There is no runtime artifact store (`runtime_artifact_store: None`), so artifact recovery reads only the workspace and the out directory. A checkpoint is evidence of WIP, never a produced artifact until handed off. Recovery dependency pins ([retention policy](retention-policy.md#dependency-pins)) protect: the workspace while the Execution is nonterminal or a reservation is held; the out directory until acknowledgement; and every handoff ref referenced by an unresolved effect, a current gate, or an Outcome.

## Verification runner

1. For each declared argv, create an independent clean worktree from `refs/taisan/artifacts/<artifact_id>`. Verify the exact commit and tracked input bytes before running it; no command inherits source changes from another command.
2. Run that argv in the recorded verification sandbox profile. Tracked source inputs are read-only. Build outputs and temporary files use separate per-command writable directories that cannot shadow tracked inputs. The out directory is per-command, there are no runtime credentials, and the same PID namespace stop rules apply. A command requiring source mutation is incompatible with this profile and needs an explicit validated profile decision; its result cannot silently certify the original commit.
3. Egress: denied by default. The effective allowlist for a command is the installation-level verification allowlist plus the egress hosts the Work declared in `required_capabilities` at admission — enforced through the same CONNECT-proxy mechanism, CONNECT-only on port 443 with DNS resolved by the proxy, TLS uninspected. The allowlist never contains model-API hosts, and its digest is part of the recorded environment identity. A command needing an undeclared host fails; a Work that needs hosts it did not declare is an admission gap, never a reason to open the network silently.
4. Record `VerificationResult{requirement, artifact, commit, argv, environment: profile digest, result, output}` with bounded output. A timeout or a killed runner is `Inconclusive`, never `Failed` with an invented exit code and never `Passed`.

The runner is a supervised effect of the CLI loop, not a Worker Invocation; it is bounded by its own per-command timeout and the Execution deadline.

## Isolation

### Threat assumptions

- The Worker runtime is not malicious by design but executes instructions influenced by untrusted issue, review, and repository content (prompt injection). It can run arbitrary commands available in the sandbox.
- The owner's account holds repository-host credentials (`~/.config/gh`, `~/.git-credentials`, `~/.ssh`, SSH agent, OS keyrings). A same-UID process outside a sandbox can read all of them; environment scrubbing alone does not prevent this.
- The host kernel, bubblewrap, the egress proxy, and the supervisor are trusted. Other same-UID processes outside taisan are outside the guarantee.

### Mechanism comparison

| Mechanism | Enforces | Limitations | Decision |
| --- | --- | --- | --- |
| Hygiene only (env allowlist, fresh `HOME`, `GIT_CONFIG_NOSYSTEM=1`, `GIT_CONFIG_GLOBAL=/dev/null`, no credential helper, no `SSH_AUTH_SOCK`, closed fds) | Nothing; prevents accidental inheritance | Same-UID file reads remain possible | Required, not sufficient |
| bubblewrap | Mount namespace (read-only system, tmpfs `HOME`, only workspace and out directory writable), PID namespace, network namespace, `--new-session`, `--die-with-parent` | Needs unprivileged user namespaces (distribution or AppArmor policy may restrict them) | **Selected** |
| Separate UID | File-permission separation | Root setup; the supervisor cannot signal or inspect the other UID without privilege, conflicting with the existing stop-proof design | Rejected |
| Landlock | Unprivileged path rules; TCP port rules only | Cannot distinguish destinations on port 443; does not hide the environment | Rejected for v0.1; possible hardening |
| cgroup v2 (`systemd-run --user`) | Complete descendant containment and kill | Needs a user systemd session, which headless hosts may lack; process control only | Rejected for v0.1; PID namespace gives the needed proof |
| Rootless containers | Broad isolation | Contradicts the "no containers" v0.1 setup and adds image management | Rejected |

### Selected profile

- **Filesystem.** Read-only binds of the system directories the runtime and toolchain need; tmpfs for `HOME`, `/tmp`, and `/run`; read-write binds only for `/workspace` (read-only for InvariantCheck and CodeReview) and `/taisan/out`; read-only bind of `/taisan/request.json`, the supervisor-rendered `/taisan/provider-config`, and, if needed, the single runtime credential file. The owner's home directory, other workspaces, the state store, the handoff repository, and installation config are not mounted.
- **Credentials.** The environment is constructed from an allowlist, not inherited. Runtime credentials come from a credential reference resolved by the supervisor and passed as an environment variable or a read-only file; their values never appear in argv, request files, logs, or evidence. No repository-host credential, git credential helper, SSH agent socket, or keyring socket enters the sandbox. Git inside the sandbox pins its rules through the environment, because repository-local `.git/config` outranks `GIT_CONFIG_GLOBAL`: `GIT_ALLOW_PROTOCOL=file` denies every transport except the local one regardless of any `protocol.<name>` a workspace plants, and an empty `credential.helper` at `GIT_CONFIG_COUNT` command scope clears every helper a lower scope accumulated. The generated `GIT_CONFIG_GLOBAL` config carries the same rules (`credential.helper=` and `protocol.allow=never` except `file`) as a second layer.
- **Network (N1, mandatory).** `--unshare-net` leaves only loopback. A taisan-owned CONNECT proxy outside the sandbox listens on a Unix socket bound into the sandbox and exposed to the runtime as `HTTPS_PROXY` through an in-sandbox relay. The proxy allows CONNECT only to the configured model-API host names and port 443, resolves DNS itself, logs allowed and denied destinations as bounded evidence, and denies everything else. TLS is not inspected. Coding, Fix, InvariantCheck, and CodeReview Invocations all get the same model-API-only proxy. Read-only assessment roles need model access too. The supervisor verification runner gets no proxy or runtime credentials.
- **Process.** `--unshare-pid` makes bwrap's child the PID-namespace init; `--die-with-parent` and `--new-session` prevent survival after the supervisor-spawned leader and terminal injection.

### Process descendants and stop proof

The existing supervisor contract stays: persist stop intent and a descendant snapshot, re-verify `pid + start time` before signaling, read state not presence, reap spawned children. The PID namespace adds a stronger proof: when the namespace init exits, the kernel kills every process in the namespace, including `setsid`-escaped descendants, which cannot leave the namespace. Stop proof is therefore: the recorded bwrap leader and namespace init are stopped or reaped, and every snapshotted identity inspects as stopped. If the namespace init cannot be inspected, the Execution remains `uncertain`. Ownership is never released based on the proxy connection closing or log silence.

### Preflight

Before the first dispatch with a profile, and whenever the profile, runtime version, or host kernel changes, setup runs negative probes inside the profile and records their results as installation evidence:

- writing outside `/workspace` and `/taisan/out` fails;
- reading the owner's host credential files fails, `SSH_AUTH_SOCK` is unset, and `git credential fill` returns nothing;
- TCP connect to the repository host and an arbitrary address fails, CONNECT to a non-allowlisted host is denied by the proxy, and the model-API host is reachable for each of Coding, Fix, InvariantCheck, and CodeReview;
- the verification profile denies an undeclared destination and reaches a declared one, and holds no runtime credential;
- a `setsid` child is killed when the namespace init exits.

Any failing or skipped probe refuses dispatch with a repair prerequisite. There is no fallback to hygiene-only execution, and a flag or `AllowedEffects` value never substitutes for these probes.

### Operational delivery and managed lifecycle (v0.3)

The [v0.3 operational profile](v0.3.md#14-operational-launch-verification-and-host-composition) selects the concrete single-host wiring for this profile:

- **Fixed mounts.** System directories read-only; `/workspace` read-write only for Coding/Fix and read-only for InvariantCheck/CodeReview; `/taisan/out` fresh and read-write per Invocation; `/taisan/request.json` read-only; one `/taisan/runtime-auth` file read-only; a supervisor-rendered `/taisan/provider-config` read-only. The workspace cannot supply or shadow provider configuration or credentials.
- **Credential delivery.** One `[worker].credential_ref` resolved at the supervisor boundary, injected as one allowlisted environment variable or a read-only file. No repository-host credential, helper, agent socket, or keyring enters the sandbox. A missing credential is an environment repair, never a weaker read or an anonymous run.
- **Model-API egress.** The mandatory N1 proxy of the selected profile, CONNECT-only on port 443 to the exact configured model-API hosts, for all four roles. No repository host, registry, source host, or verification host is reachable through it.
- **Child runtime servers.** A runtime-internal server (MCP, language server, or similar) is a descendant of the one Invocation process tree, not a new Invocation. It inherits the sandbox and dies with the PID namespace init. Only the shim leader's process identity and the recorded descendant snapshot are stop-proof subjects.
- **Output caps.** `report.json` is capped at 1 MiB before parsing; `runtime.log` and child-server output share the 32 MiB retained cap with truncation recorded; the profile's per-stream `stdout`/`stderr` caps are truncation points under that same shared cap, not separate budgets; a full child pipe never blocks the parent. Assessment roles receive a read-only workspace and may write only bounded scratch inside `/taisan/out`; the v1 snapshot copy remains defence-in-depth, not the read-only mount.
- **Stop evidence and restart.** The stop intent and descendant snapshot precede signal; every recorded `pid + start time` is re-verified. The v0.3 target escalates SIGTERM, a 10 s grace, then SIGKILL across the group and namespace; the rechecked baseline stop path sends SIGKILL directly without the SIGTERM stage or grace wait. The namespace init and each recorded identity must inspect as stopped. A restart re-attaches by recorded identity, never starts a second writer, never reuses an out directory, and never adopts a child server from a dead runtime. Uncertainty retains the slot, reservations, and workspace.

### Limitations

- The model-API channel remains an exfiltration path for workspace content and the runtime credential. The allowlist narrows destinations; it does not inspect content.
- Hosts without unprivileged user namespaces cannot run the v0.1 profile; setup must fail rather than degrade.
- The proxy is trusted code with its own availability; proxy failure is a runtime failure class (environment repair), never a reason to open the network.
- Runtime-internal sandbox features of Claude Code are not counted as enforcement.

## Failure mapping

Classes follow #51's taxonomy (transient/retryable, permanent, environment repair, human decision, unknown effect). Core authorizes any new Invocation; this table only fixes the facts and the class.

| Condition | Facts recorded | Class and Core consequence |
| --- | --- | --- |
| Provider refusal | Report `Refused`, `provider_refused` | Permanent for this task; failure or Attention, no automatic retry of the same request |
| Rate limit or quota | `rate_limited` / `quota_exhausted` | Transient (rate) or environment repair (quota). A new Invocation needs a Core decision within budget |
| Runtime auth unavailable | `auth_unavailable`, or preflight failure | Environment repair; `paused` with prerequisite after stop proof |
| `max_turns` or deadline | `max_turns_reached` / `deadline_reached`, or supervisor deadline stop | Resource bound; handoff of any produced commit still runs; further attempts are a Core decision |
| Definitely not started | `NotStarted` before any child exists | Counts as an Invocation attempt; `paused` (repair) or `failed` |
| Unknown launch | `Unknown` after spawn | Unknown effect for ownership: `uncertain`, no replacement start |
| Missing terminal report | Exit confirmed, no `report.json` | Bounded report recovery, then invalid/absent report; the handoff may still produce a valid artifact, but no claimed review/invariant result exists |
| Corrupt or wrong report | Report invalid | Retained invalid evidence; never parsed leniently |
| Lost finish report (exit observed, supervisor crashed before recording) | On restart: exit inspected, out files present | Re-observe from out directory; idempotent by Invocation ID |
| Checkpoint unavailable | Workspace missing or branch absent | Absent continuation input; Core cannot dispatch a Fix that depends on it |
| Interrupted artifact promotion | Crash during export/import | [Crash recovery](#crash-recovery) table; within the artifact-recovery bound |
| Isolation preflight failure | Probe results | Environment repair; dispatch refused |
| Proxy unavailable during run | Runtime network errors and proxy health | Environment repair after stop proof |

A permanent refusal class (`provider_refused`) blocks the task immediately and consumes no artifact-recovery or report-recovery attempt; the refusal record and any retained evidence survive exhaustion with their honest flags intact.

## Shared Worker contract suite

Both implementations run the same suite through the supervisor and the public Command/Query path. **Real**: the reference shim, run against a deterministic fake `claude` executable that replays scripted stream-json, plus a separate live smoke with the real runtime in U8. **Contract adapter**: an independently implemented deterministic Worker that shares no code with the shim and replays scripted results. The current fixture worker's process modes may be reused by the contract adapter's process cases. The U4.6b run executes the identical suite code twice — once with each adapter bound — and retains one [U4.6 record](#u46-evidence-record) per case per implementation; a named static check asserts the suite source contains no adapter-name branch.

| ID | Case | Expected |
| --- | --- | --- |
| WS-01 | Happy path Coding | One process, report valid, handoff acknowledged, verification by runner |
| WS-02 | Duplicate delivery of the same request | No second spawn; same Invocation |
| WS-03 | Start response lost (crash after spawn) | Restart inspects the recorded identity; no second writer |
| WS-04 | Spawn refused before child exists | `NotStarted`; attempt counted |
| WS-05 | Liveness unknown (inspection fails) | `uncertain`; slot and reservation retained |
| WS-06 | `setsid` escape and orphan writer | Killed with namespace init; stop proof covers snapshot |
| WS-07 | Stop request with SIGTERM-ignoring child | SIGKILL after grace; proof only after inspection |
| WS-08 | Missing report | Bounded recovery; no success |
| WS-09 | Corrupt / wrong-Invocation report | Invalid evidence retained |
| WS-10 | Claimed head without commit / commit without claim | Handoff decides; claim mismatch recorded |
| WS-11 | Export interrupted at each crash point | Recovery per table; no duplicate artifact IDs |
| WS-12 | Stale base (commit does not descend from base) | Invalid artifact |
| WS-13 | Corrupt bundle / digest mismatch | Invalid, never published |
| WS-14 | Write outside allowed paths | Denied by sandbox; preflight and in-run probe |
| WS-15 | Read host credential, use SSH agent or credential helper | Unavailable |
| WS-16 | All four Worker roles connect to the model API, repository host, and non-allowlisted host; verification runner attempts the same | Only the allowed model API is reachable for each Worker role. The verification runner reaches only installation-default and Work-declared hosts and holds no runtime credentials |
| WS-17 | Provider refusal, rate limit, max turns | Bounded reason codes; no outer retry by adapter |
| WS-18 | Stale-head review report | Cannot settle the newer head (#51) |
| WS-19 | Adapter attempts to change Work state or request retry | No path exists; any such output ignored and recorded |
| WS-20 | Config-only swap between adapters for new Invocations | Identical Command, queue, budget and Outcome results; running Invocation unaffected |
| WS-21 | Runtime version differs from declaration | Configuration repair; no start |
| WS-22 | Verification runner timeout or source-mutation attempt | Timeout is `Inconclusive`, not `Failed` or `Passed`. Source mutation is denied; every subsequent command starts from the exact original artifact with independent outputs |
| WS-23 | Verification egress with Work-declared hosts | Declared and installation-default hosts reachable; undeclared destination denied; no runtime credential present; recorded environment digest reflects the effective allowlist |
| WS-24 | Missing, stale, or undeliverable worker input snapshot (`review_source`, `invariant_set`, `goal`) | Dispatch refused as an invalid request; the Worker never fetches the input itself |

### U4.6 evidence record

Every U4.6 swap/recovery proof retains one record per exercised scenario. The record is JSON with these fields; values are evidence, never instructions:

| Field | Required content |
| --- | --- |
| `schema_version` | `1` |
| `tested_revision` | Exact commit SHA on the PR branch that contains the exercised slice tree. A base revision or patch digest alone never satisfies this field; a patch digest may be retained alongside it |
| `base_revision` | The worktree fork point the slice was applied to |
| `claim_class` | Closed set: `fixture_supported` (local fixture/contract evidence only) or `live_evidenced` (separately authorized live scenario). Fixture results never satisfy a live claim |
| `configuration_identity` | The installation config identity, plus the adapter selection active when the request was bound: `adapter_id`, `adapter_version`, `runtime_version`, `capability_digest`, `worker_argv` (or its digest), and the request/invocation identity |
| `command_scenario` | The named test/command and scenario row (for example `WS-20`, `WS-21`) |
| `result` | `pass`, `fail`, `skipped`, or `inconclusive`, with the retained reason when not `pass` |
| `evidence_refs` | Retained test output, store identities, or log locations that let the result be re-checked |

A binding is recorded once per request; a later `worker_argv` change affects only requests not yet bound, and a running Invocation keeps its recorded adapter ID, adapter version, runtime version, capability digest, and argv. A declaration that drifted under the bound argv is reported per differing field (`runtime.version_mismatch`, `capability.digest_mismatch`, and so on) — a configuration repair with no start, never a silent upgrade, and terminal for that request's binding: the work is re-issued under a new request that binds the current declaration.

The swap-window recovery scenarios retain one record each. A supervisor restart after a `worker_argv` change reconciles every in-flight Invocation against its recorded adapter ID, version and capability digest — the recorded binding and writer identity are unchanged, no running Invocation is migrated, and no second writer starts; only requests not yet bound select the new adapter. Interrupted export/import recovers per the [crash-recovery table](#crash-recovery) under the changed configuration, with exactly one artifact ID per Invocation across every window and across re-drives: recovery reads recorded identities, workspaces, and durable rows, never the current `worker_argv`. Real-runtime safe stop records a durable stop intent covering every recorded identity launched under the prior selection and inspects each stopped before terminal release, and artifact recollection reaches the durable acknowledgement (row plus ref plus the declared verification on the produced commit) with workspace and git objects retained through the verified handoff. Records are stamped fail-closed from the branch commit that contains the exercised slice: a checkout whose tree differs from `HEAD` runs the scenarios but retains no records, so a retained record can never cite a revision that does not contain it.

The retained scenario records are consolidated for issue #102 in the [consolidated U4.6 evidence record](evidence/u4.6d.md): one row per #102 acceptance criterion with the columns `criterion`, `record_refs[]` (or the literal `absent{reason}`), `tested_revision`, `config_identity` and `result`, where every `record_refs[]` entry resolves to a retained per-scenario record whose index entry carries that record's tested revision, configuration, command/scenario and result. A named completeness check asserts every #102 criterion has a row — a missing row fails, an `absent{reason}` row is reported as a gap, never as a pass; a named schema check asserts every row and index entry conforms to the columns above.

## Requirement mapping

Every Worker-owned requirement #50's inventory assigns to this issue — 33 targets — is mapped to a normative clause of this document plus shared-suite coverage, or to an explicit approved deferral, in [worker requirements](worker-requirements.md). Requirements whose retained safety obligation is met by a different mechanism than the source's (for example, sandbox enforcement in place of a server-side audit) record the substitution there; no target remains unmapped.

## Approved defaults

| Setting | Default |
| --- | --- |
| Reference adapter `--max-turns` | 200 per Invocation |
| Stop grace between SIGTERM and SIGKILL | 10 s |
| `report.json` size cap | 1 MiB |
| `runtime.log` retained cap | 32 MiB (truncation recorded) |
| Child runtime server stdout/stderr | Shares the 32 MiB `runtime.log` cap; truncation recorded, never a blocking pipe |
| Verification per-command timeout | 30 min, bounded by the Execution deadline |
| Verification output cap per command | 8 MiB |
| Shim-level retries | 0 (runtime-internal retries only) |

## Six design aspects

| Aspect | Applicability and rationale | Settled here | Open question |
| --- | --- | --- | --- |
| State transitions | Launch, exit, export, and verification produce facts that drive `ready`/`running`/`waiting`/`uncertain` | `NotStarted` vs `Unknown`; export only after stop proof; handoff states `imported`→`acknowledged` | Whether export/verification progress needs its own `waiting` reason name ([execution loop](execution-loop.md)) |
| Failure semantics | Runtime, sandbox, and handoff failures look alike from exit codes | Closed reason codes, class mapping, recovery table | Final taxonomy names from #51 |
| Trust boundaries | Prompt-injected runtime; host credentials on the same account | bwrap + hygiene + mandatory proxy; no workspace code runs in trusted processes | Model-API exfiltration remains |
| Identity invariants | Requests, Invocations, artifacts, and commits recur across attempts | Per-Invocation out directory; `ArtifactId` ref; request digest; capability digest bound per request | — mapped to #50 inventory |
| Information preservation | Claims, invalid reports, and WIP must survive | Invalid evidence retained; workspace and bundle kept until acknowledgement | Exact pins and reclamation in the [retention policy](retention-policy.md) |
| Resource bounds | Runtime turns, verification time, logs, recovery | Separate bounds; no shim retries | — defaults approved |

## Implementation-plan mapping and non-claims

- **U1**: `WorkerRequest`/`WorkerReport`/`WorkerCapabilities` types, `artifact_handoffs` table, evidence classification above.
- **U2**: isolation profile config, credential references for the runtime, egress proxy config and preflight probes, runtime version pinning.
- **U4**: shim, fake-runtime harness, independent contract adapter, export/import, verification runner, shared suite WS-01–WS-22.
- **U5**: CLI loop drives export, verification, and recovery ticks.
- **U8**: live smoke with the real runtime inside the profile.

At `51f0b34` the implemented Worker boundary is narrower than this
design: a durable v1 export, process supervision and stop proof, an
environment-hygiene launch, durable git-object handoff, and a
supervisor-run verification runner exist and are covered by
fixture/contract tests. The executable v2 request/report transport, four
role-specific bodies, typed report ingestion, namespace and egress
enforcement, isolation preflight, and a live reference coding-agent
runtime do not exist. Authorized live S1 evidence exercised a scenario
bridge and a deterministic scenario shim, not this v2 contract, a real
coding-agent runtime, four distinct Worker roles, or the namespace/egress
profile; it is never evidence for those claims. The
[v0.3 contract](v0.3.md) is the dependency for their implementation and
records the owner decisions that must be settled first.
