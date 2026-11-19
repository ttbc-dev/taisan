# Durable identity, storage, and admission design

Status: concrete design for #50 against runtime revision `eb1c2f1`. This document specifies future Rust/SQLite behavior; it changes no runtime API, database, configuration or live process. The owner selected maintenance-only cutover, read-only legacy archive plus a fresh store/new Work, unfinished-Issue overlap rejection, and same-Work specification revisions with renewed approval after source changes. The [host contract](repository-host-contract.md) remains authoritative for adapter semantics. Evidence/approval payload details belong to #51; runtime/CLI protocol details belong to the [execution loop](execution-loop.md).

## Usage and authority

An operator supplies an installation, publication repository, source issues, intent, constraints and verification requirements. A boundary resolver converts configured display references into verified stable identities outside Core. Core receives an immutable resolved submission plus the actual source/capability facts and decides acceptance in one store transaction. A Selector returns a proposal into that same admission path; it cannot directly insert Work.

The caller receives Store/Work identity, Work version, specification revision and the saved Command decision. A Query reads these records without network resolution. Core requires an authoritative source observation for the dispatch attempt and decides whether it permits dispatch. The host adapter performs that read; the CLI loop drives the requested I/O and re-enters the decision path. CLI scheduling never decides which domain guard may be skipped; acceptance alone is never execution permission. Actions use the binding saved with Work, not the current default repository or credential actor.

The owner chose these additional policies during #50:

| Choice | Result |
| --- | --- |
| Maintenance window | Stop supported writer/dispatch entry points before cutover; no online old/new schema coexistence |
| Legacy execution compatibility | Preserve the old database for reference. Create new-format state and new Work; do not resume old Work or replay old keys into the new store |
| Same Issue, different submission keys | Reject overlap while any Work owns that scoped source Issue, including manual submissions, paused Work and uncertain effects. After safe terminal release, permit explicit new submission |
| Source edited between acceptance and first dispatch | Core requires an authoritative read immediately before dispatch. Detected change/unavailability holds execution; the matched version from that read is the fixed input boundary |
| Replanning identity | Append a new specification revision to the same Work; retain prior input/invariants/approval and require approval for the new revision. Do not change host, actor, permissions or budgets automatically |
| Future auto mode | A configurable mode may later regenerate invariants from changed input and proceed under an explicitly authorized policy. Not required for v0.1. Preserve identity/version hooks; no auto approval or invariant-generation implementation is added here |
| PR conflict | v0.1 detects and records the conflict, then requests human action; automatic conflict repair is later-milestone work. Bind the conflict episode to repository/change/head and target revision |
| Source Issue closed | Hold new dispatch and publication for an explicit human decision; do not infer cancellation. Safely stop a live worker and release its lane after stop proof; retain WIP and conflicting-resource reservations |
| Repository rename or transfer | A display rename may follow verified stable identity and permissions. An ownership transfer requires renewed explicit approval and allowlist/permission checks; a changed binding requires new Work |
| External completed-fix adoption | Defer a dedicated out-of-band result-adoption feature. v0.1 permits explicitly claimed manual workspace repair after managed writers stop, followed by normal resume and fresh verification; never import an external success claim as accepted evidence |

## Concrete identity model

Rust sketches below are target shapes with private constructors, not implemented APIs. Parsing validates bounded, nonempty opaque IDs and structural consistency at external/persistence boundaries. Core does not parse provider URL syntax or branch on provider names. Display URLs, paths and numbers never participate in equality.

```rust
struct StoreId(String);
struct WorkId(String);
struct ExecutionId(String);
struct InvocationId(String);
struct RequestId(String);
struct CommandId(String);
struct ActionId(String);
struct ObservationId(String);
struct HostInstanceId(String);
struct RepositoryId(String);
struct IssueId(String);
struct ChangeRequestId(String);
struct HostActorId(String);
struct AdapterId(String);
struct AdapterSourceId(String);
struct SourceRevision(String);
struct ObjectId(String);
struct WorkVersion(u64);
struct SpecRevision(u64);
struct EventSequence(u64);

struct WorkRef { store: StoreId, work: WorkId }
struct RepositoryRef { host: HostInstanceId, repository: RepositoryId }
struct IssueRef { repository: RepositoryRef, issue: IssueId }
struct ChangeRequestRef { repository: RepositoryRef, change: ChangeRequestId }
struct CommitRevision { repository: RepositoryRef, object: ObjectId }
struct HostActorRef { host: HostInstanceId, actor: HostActorId }
struct CommandActor { kind: ActorKind, external_id: String }

struct RoleBinding {
    role: ActorRole,
    actor: HostActorRef,
    credential_ref: CredentialRef,
}
struct HostBinding {
    repository: RepositoryRef,
    adapter: AdapterId,
    contract_version: ContractVersion,
    required_capabilities: CapabilitySet,
    actors: RoleBindings,
}
struct SourceSnapshot {
    issue: IssueRef,
    source_revision: SourceRevision,
    content_digest: Digest,
    observation: ObservationId,
}
struct SpecificationSnapshot {
    revision: SpecRevision,
    sources: Vec<SourceSnapshot>,
    intent: String,
    constraints: Vec<String>,
    verification_argv: Vec<Vec<String>>,
    invariants: InvariantSetRef,
}
struct Work {
    id: WorkId,
    version: WorkVersion,
    binding: HostBinding,
    current_spec: SpecRevision,
    approved_spec: Option<SpecRevision>,
    source_gate: SourceGate,
}
enum SourceGate { NotApplicable, NeedsCheck, Unavailable, Changed, Matches }
enum DeliveryIdentity {
    Host { instance: HostInstanceId, source: AdapterSourceId, event: String },
    Local { store: StoreId, source: LocalSourceId, event: String },
}
enum WriteResource {
    Workspace { store: StoreId, workspace_id: WorkspaceId },
    Branch { repository: RepositoryRef, branch: BranchName },
}
```

Each source observation retains the exact bounded source content or a durable content reference with its digest; a URL or digest alone cannot reconstruct the reviewed input. Retention of those bodies and invariant sets follows the [dependency pins](retention-policy.md#dependency-pins) specified in #54, and their precise evidence envelopes are completed in #51.

`CapabilitySet` and `RoleBindings` validate duplicate entries, required roles and same-host actors once; internal callers cannot supply invalid bags of strings. `InvariantSetRef` identifies the exact specification's immutable invariant-set record; #51 defines its contents, validity, and approval evidence. Absence is explicit and cannot satisfy a required gate. This design does not require generating invariants in Core.

| Identity | Equality and lifetime | Validation / serialized form |
| --- | --- | --- |
| Store | Persistent ID generated on explicit initialization; a new store is a different authority | Every exported Work reference, request/report transport and result carries StoreId; wrong-store input is refused |
| W/E/I/request/command/action/observation | Distinct opaque internally generated IDs in one store; no reuse | Use fresh UUID-based IDs for new records, with typed fields rather than executable meaning in prefixes. EventSequence orders records, not UUID ordering |
| Host instance | Stable configured endpoint identity, separate from display alias/provider name; endpoint spellings canonicalize to one host instance (`https://api.github.com` → `github.com`) so intake-bound and installation-bound identities coincide | Persist endpoint/config identity; a changed endpoint cannot reuse the ID and retarget saved Work |
| Repository | `(host_instance, stable_repository_id)` | Adapter-issued stable opaque ID; rename updates display metadata only |
| Issue / change request | `(repository, opaque_native_id)` | Display issue/PR/MR number is metadata. The adapter resolves it with provenance; no inferred identity from a mutable path |
| Git revision | `(repository, exact_object_id)` | Object algorithm/format validation belongs at the adapter boundary; identical object text on another repository is different evidence |
| Source revision | Version token plus content digest and producing observation for one Issue | If the host lacks a strong version token, adapter supplies a documented content-version token. A timestamp alone is not proof of unchanged content |
| Host actor | `(host_instance, stable_actor_id)` | Different from local CommandActor, which scopes command replay. A credential reference names a secret slot, not an actor identity |
| Delivery | Tagged host/local scope plus source ID and event ID | Host ID/source namespace isolates provider deliveries. Missing event ID means no dedup; changed content under the same delivery identity is a conflict, not replacement |
| Specification / Work version | SpecRevision identifies immutable task content; WorkVersion changes for decisions/gates/preconditions | New source/specification does not overwrite an old snapshot. Old approvals cannot approve a new revision |
| Resource | Tagged workspace identity or `(host, repository, exact_branch_name)` | SQL tuple keys; if serialized as text, use a versioned array encoding, never colon-concatenated opaque values |

Publication target and source issues are separate. Empty source issues are allowed for direct intent-only Work; its source freshness gate is NotApplicable. Cross-repository source issues require allowlist admission but grant no publication authority on those repositories. The source Issue set remains fixed for the lifetime of this Work in v0.1; changing its membership requires a new Work after safe terminal release. Specification revision updates its content, not the Issue membership or binding.

Fresh internal Work IDs also avoid reusing legacy `work/<old_id>` branch/workspace names after fresh-store initialization. A new store is not permission to reuse an archived workspace or overwrite an existing branch. Recheck the managed root and absent/exact-prior-ref guard. All local paths/request envelopes remain Store-scoped; another store cannot adopt a request because its bare ID happens to match.

## Storage shape and invariant ownership

Use SQL columns for identity, uniqueness, joins and constraints, and versioned typed payloads for operation-specific content and immutable snapshots. Avoid a generic entity graph and provider-specific parallel Core tables. SQL enforces structural constraints; Core evaluates transition, policy, budget and evidence legality.

| Table / record | Key and structural responsibility | Payload / change policy |
| --- | --- | --- |
| `store_metadata` | Exactly one StoreId; format/initialization metadata | Never rename an existing authority into another store |
| `host_instances`, `repositories` | Host PK; repository composite PK; display metadata excluded from keys | Retain old binding/config needed by existing new-format Work |
| `works`, `work_bindings` | Work PK; one binding/Work; composite binding FK for actions | Binding's repository, adapter, contract, required capability set and role bindings immutable |
| `work_actor_roles`, `work_capabilities` | `(work_id, role)` and `(work_id, capability)` unique | Minimal relational children allow actor/effect constraints; no per-provider role schema |
| `work_issue_refs` | `(work_id, ordinal)`, unique `(work_id, host, repo, issue)` | Immutable source membership; content versions live in specification snapshots |
| `work_specifications` | `(work_id, spec_revision)` unique, immutable | Versioned source/intent/constraint/check/invariant references; `works` points to current and approved revisions |
| `active_issue_claims` | `(host, repo, issue)` unique, FK to owning Work/source membership | Insert with acceptance/retry, retain through holds/pauses/uncertainty, delete only in safe terminal transaction |
| `proposal_admissions` | `(store_id, selector_id, proposal_key)` unique | Canonical semantic payload/digest/version and saved result; proposal replay cannot create new Work after terminal |
| `commands` | Existing origin/local actor/key scope plus unique CommandId within Store | Add canonical payload bytes, payload/digest codec version, saved decision bytes; exact replay returns original decision |
| `observations` | Unique ObservationId; scoped delivery partial unique indexes | Versioned typed fact, actual source/target/revision/time and digest; no global source-event uniqueness |
| `host_action_intents`, effect receipts | Unique ActionId, Work binding/actor/target FKs | Versioned exact approved payload, per-effect status and typed receipt; #51 details receipt payload/gates |
| requests/invocations/stop intents | Existing request and one-unresolved-writer uniqueness | Request binds Store, Work, specification revision, invariant-set reference and saved Execution limits; stop evidence keeps process identity |
| executions/slots/reservations | Existing one-nonterminal-E/Work, fixed slot count, unique held resource | Budgets/usage/reservations survive same-Work reapproval; specification changes never imply release or reset |
| judgments/outcomes/evidence | Work/specification/change/revision subject, not bare head text | #51 completes per-finding and evidence schemas; old-specification decisions remain history |
| `event_log` | Monotonic sequence in this Store; event format version | Preserve historical events; no claim that the current log alone is a complete event-sourced rebuild mechanism |

Illustrative identity constraints below complement the existing lifecycle constraints. This is a SQL sketch, not a schema migration script. All identity strings have nonempty/bounded parsing in typed constructors, and relevant SQL CHECKs repeat structural requirements at persistence. Foreign keys are enabled on every connection.

```sql
CREATE TABLE repositories (
    host_id TEXT NOT NULL REFERENCES host_instances(id),
    repo_id TEXT NOT NULL,
    display_path TEXT,
    PRIMARY KEY (host_id, repo_id)
);
CREATE TABLE work_bindings (
    work_id TEXT PRIMARY KEY REFERENCES works(id),
    host_id TEXT NOT NULL,
    repo_id TEXT NOT NULL,
    adapter_id TEXT NOT NULL,
    contract_version INTEGER NOT NULL CHECK (contract_version > 0),
    FOREIGN KEY (host_id, repo_id) REFERENCES repositories(host_id, repo_id),
    UNIQUE (work_id, host_id, repo_id)
);
CREATE TABLE work_actor_roles (
    work_id TEXT NOT NULL,
    host_id TEXT NOT NULL,
    repo_id TEXT NOT NULL,
    role TEXT NOT NULL,
    actor_id TEXT NOT NULL,
    credential_ref TEXT NOT NULL,
    PRIMARY KEY (work_id, role),
    UNIQUE (work_id, role, actor_id),
    FOREIGN KEY (work_id, host_id, repo_id)
        REFERENCES work_bindings(work_id, host_id, repo_id)
);
CREATE TABLE work_capabilities (
    work_id TEXT NOT NULL REFERENCES work_bindings(work_id),
    capability TEXT NOT NULL,
    PRIMARY KEY (work_id, capability)
);
CREATE TABLE work_issue_refs (
    work_id TEXT NOT NULL REFERENCES works(id),
    ordinal INTEGER NOT NULL CHECK (ordinal >= 0),
    host_id TEXT NOT NULL,
    repo_id TEXT NOT NULL,
    issue_id TEXT NOT NULL,
    PRIMARY KEY (work_id, ordinal),
    UNIQUE (work_id, host_id, repo_id, issue_id),
    FOREIGN KEY (host_id, repo_id) REFERENCES repositories(host_id, repo_id)
);
CREATE TABLE active_issue_claims (
    host_id TEXT NOT NULL,
    repo_id TEXT NOT NULL,
    issue_id TEXT NOT NULL,
    work_id TEXT NOT NULL,
    PRIMARY KEY (host_id, repo_id, issue_id),
    FOREIGN KEY (work_id, host_id, repo_id, issue_id)
        REFERENCES work_issue_refs(work_id, host_id, repo_id, issue_id)
);
CREATE TABLE work_specifications (
    work_id TEXT NOT NULL REFERENCES works(id),
    spec_revision INTEGER NOT NULL CHECK (spec_revision > 0),
    payload_version INTEGER NOT NULL CHECK (payload_version > 0),
    canonical_payload_ref TEXT NOT NULL,
    payload_digest TEXT NOT NULL,
    PRIMARY KEY (work_id, spec_revision)
);
CREATE UNIQUE INDEX host_delivery
    ON observations (host_id, adapter_source, source_event_id)
    WHERE source_kind = 'host' AND source_event_id IS NOT NULL;
CREATE UNIQUE INDEX local_delivery
    ON observations (store_id, local_source, source_event_id)
    WHERE source_kind = 'local' AND source_event_id IS NOT NULL;
```

Canonical payload references identify retained content by digest; body location/availability is a separate catalog. Immutable identity/specification metadata does not require every artifact/source body to stay inline forever. The [retention policy](retention-policy.md) compacts, deduplicates and reclaims bodies through the content-addressed catalog without changing their identity. Required replay/approval/recovery bytes remain dependency-pinned; if a proposed retention limit cannot preserve that contract, surface the tradeoff to the owner rather than delete them silently. A reclaimed non-required body is explicitly unavailable, never reconstructed from a digest.

Create SQL immutability triggers rejecting UPDATE/DELETE on accepted binding/roles/capabilities/source membership and specification snapshots. Pointer updates on Work require an existing `(work_id,spec_revision)` FK and a WorkVersion compare-and-update. Insert Work, source rows, specification, binding, initial queued E, overlap claims and Command decision together; any constraint failure rolls the whole attempt back. Child insertion order stays within that transaction. Immutable-child insertion is also sealed after acceptance: store effects and a sealed binding marker prohibit adding roles/capabilities later, not just updating existing rows. Core admits the complete required capability/role set, and seals it atomically. The migration implementation must test SQL rejection of post-seal insertion, including direct low-level store paths.

A host action has a composite FK to the Work's publication binding and selected role/actor. If `target_change` belongs to another repository or `revision.repository` differs from the target, parsing/structural validation rejects it before recording authorization. Source observations may describe an allowlisted external Issue; this never relaxes the action's publication FK. Host/local observations have disjoint required columns and CHECKs; nullable fields cannot accidentally evade delivery scoping. A duplicate delivery with a different canonical fact is recorded as a conflict with both provenance records retained.

Retain the existing partial unique indexes for one nonterminal Execution per Work and one unresolved Invocation per Execution, plus request-ID, slot and resource uniqueness. No new layout is permitted to weaken them. Actor/config renewal resolves the same saved credential slot and verified actor; replacing an actor or adapter cannot edit accepted Work rows.

## Serialization and command replay

New records use versioned tagged envelopes. Store raw canonical payload bytes as well as a digest and codec identifier. Define canonical encoding as UTF-8 JSON with fixed field names, recursively sorted object keys, integer-only counters, no insignificant whitespace, and explicit absent-versus-null handling per schema. Arrays preserve semantic order; declared sets such as capabilities are sorted/deduplicated at construction. New payload equality compares canonical bytes, not just a hash. Digest is SHA-256 of those bytes with an explicit version/domain tag. The exact field grammar and output schema are completed with #51 and the [execution loop](execution-loop.md); no hash of current serde layout is silently treated as the permanent contract.

Replay key is `(StoreId, origin, CommandActor.kind, CommandActor.external_id, idempotency_key)`. An exact replay returns saved CommandId/decision/sequence even if Work has finished, source content changed or current configuration differs. Different bytes under the same key yield a saved explicit conflict without replacing the original decision. Fresh source/config reads cannot change a saved replay result. A request rejected because its source was stale has its own saved decision; a regenerated proposal is a new request, not a mutated replay.

Queries read one snapshot and expose StoreId, WorkVersion, current/approved SpecRevision, immutable host binding, source revisions, stale/unavailable hold and provenance. They never map display paths to new identities through a network lookup. Head/check/review selection matches exact repository and change-request identity before revision; same SHA is insufficient. #51 defines the complete review/evidence acceptance rules.

## Admission, overlap, and source revisions

A reference Selector proposes one Issue per Work. It returns Selector ID/version, policy revision, sorted stable source references, their source revisions/content digests, semantic task input and bounded provenance. Proposal key is a deterministic digest of these semantic fields, excluding run timestamps, mutable running labels and bookkeeping cursors. Reordering a semantic set does not change the key. A re-run of the same proposal returns its saved result; new Issue content produces a new proposal identity. The operator can explicitly submit new Work after terminal release using a new command key. Rich grouping and automatically reprocessing an unchanged terminal proposal remain deferred.

Admission performs authoritative source/identity/capability reads through adapters outside the SQLite transaction. Core compares the supplied snapshot against the freshly observed revision/digest and local config/policy generation; the acceptance transaction rechecks that no intervening local observation/config generation superseded that basis. A remote edit can race the network read and SQLite commit, so acceptance records exactly what was checked and never claims a cross-system atomic snapshot. At first dispatch Core requires a new authoritative source observation for that attempt; the host adapter executes the read, the CLI loop drives it, and Core checks the returned facts against the approved specification. A cached status Query or a timer tick cannot satisfy that requirement. Missing/unknown source is a hold, not proof of unchanged content. No arbitrary freshness TTL is introduced here; each admission/first-dispatch attempt requires its own authoritative check, with transport timeout and scheduling bounds designed in the [execution loop](execution-loop.md).

Same scoped Issue overlap is separate from command replay and branch exclusion. Insert all overlap claims with accepted Work in the same transaction. On a conflict, return the existing Work identity; never partially accept a source set. Pause, source reapproval, missing evidence and unknown external effect keep the claim. Safe terminal settlement removes it together with applicable reservation/slot release. `retry` reacquires the old Work's full source set atomically; it is rejected if a newer Work now owns any Issue. A terminal no-op/cancel replay cannot remove another Work's claim. External manual branch claims remain a distinct write-resource mechanism.

The owner explicitly accepts the last authoritative pre-dispatch read as the source-version boundary. GitHub edits are not atomically locked with SQLite or process launch: an edit after that read may not be detected before the already-bound Invocation starts. Store the read observation and the specification/invariant reference used for the request, and recheck local WorkVersion/config/observation generations at request commit. A newer locally recorded mismatch defeats an earlier matching read. Do not claim prevention of all concurrent remote edits. Later detected changes hold subsequent uncommitted processing for reapproval; they never rewrite an existing request or silently replan a live writer. The approved source-closure and human-wait policies require an explicit safe-stop request, retained continuation, and capacity release after writer safety is proven. Detailed continuation/stop protocol is #51/[execution loop](execution-loop.md) design.

Before first dispatch, detected source change sets a versioned hold visible through Work detail/Attention and invalidates permission to dispatch the old specification. Create a new immutable specification revision containing the new source snapshot, proposed invariants and updated intent/constraints/checks as reviewed. Advance current_spec and WorkVersion; do not overwrite old payloads or carry the old approval forward. Dispatch requires `current_spec == approved_spec`, exact matching source facts and no hold, and saves that revision/invariant reference in the Invocation request atomically with slot/reservation/request creation. Approval must identify the precise new specification and invariant set. #51 and the [execution loop](execution-loop.md) define its Command protocol; this document does not invent a runnable CLI command.

Reapproval keeps Work identity, fixed source membership, host/adapter/actor/capability binding and existing Execution limits/usage. If new requirements need expanded scope/permissions or a different binding, do not silently amend this Work: surface the difference for a separately authorized new Work after safe termination. Once an Invocation request exists, its input never changes; an Issue edit cannot replace the task of a live or uncertain writer. Policy for later stages and any future auto regeneration is designed under #51 and the [execution loop](execution-loop.md) before enabling it.

Future auto mode must produce a new specification/invariant set with explicit policy authority and retained provenance, then satisfy the same revision/binding/budget guards. It is not authorization to infer approval from generated prose, silently increase budgets or bypass an unavailable source. The active v0.1 profile is manual renewed approval. A future config change cannot silently enable auto mode for existing Work; the immutable accepted policy/profile remains authoritative. Exact auto-mode policy/version fields and supported transitions are deferred, not advertised as implemented configuration.

## Maintenance cutover and legacy reference

The owner explicitly chose archive/new-store over preserving legacy runtime compatibility. Earlier plans to transform legacy current rows and replay old keys are superseded for the unversioned slice-0 store. Retaining history does not require a second executable legacy Core path.

1. Enter maintenance mode at every supported CLI/run-loop entry point; stop dispatch and external-write application. Establish exclusive installation ownership in addition to SQLite locking. Inspect old requests, workers, descendant identities, reservations and external actions. CLI exit alone is not proof that workers or remote effects stopped.
2. Establish stop/effect safety before permitting new Work to use any overlapping resource. If an old effect or writer is unresolved, report it and keep the affected cutover blocked; do not erase the row, re-key it into the new store or treat a new path as stop proof. Resolve through the old installation's established process or an explicit owner repair decision with evidence. No managed Invocation is handed to the new store.
3. Preserve a consistent old-store archive through SQLite backup facilities after quiescence, including committed WAL state. Do not copy only the main database while ignoring a live WAL. Record original schema fingerprint, archive identity/checksum, source path, captured sequence, row counts and unresolved-item report. Preserve old WIP/artifact references; the [retention policy](retention-policy.md) sets later reclamation policy, so no pruning is authorized during cutover.
4. Place archive and new active store at distinct explicit paths. Preserve archived bytes for read-only inspection using a read-only connection or a separate exported reference report. Legacy repository paths, digests, decisions, ID counters, sequences and raw payloads remain as originally recorded. Missing stable host/actor/provenance stays unavailable; do not fabricate neutral IDs or new successful receipts. Archive identity lives in an external manifest; do not alter the archived database merely to add metadata.
5. Explicitly initialize an empty new-format store at a new path, with a fresh StoreId, new internal IDs, new workspace generation and a configured host binding. All new Work is newly submitted; no legacy current row, command key, pending request, reservation or saved action is imported as executable truth. Event sequences restart in this different authority and are never compared with archive sequences without their Store/archive identity.
6. Validate the archive, new schema and config in read-only preflight before enabling the new CLI loop. Record cutover outcome and active path. A crash before new-store activation leaves maintenance mode in place. A partly initialized new store is discarded/reinitialized only if it has never accepted Work or authorized effects; otherwise inspect its committed StoreId/version and resume there. Never automatically fall back to the old executable installation after new activity.

New-store schema version is `PRAGMA user_version = 1`. Check version and recognized structure before DDL, capacity seeding or writes. New empty-store bootstrap creates tables/constraints/metadata and sets the version in one transaction; existing unversioned slice-0 stores are refused for runtime open and directed to archive/cutover. Unknown/newer/corrupt formats are refused without mutation. A v1 store from the previous release — recognized base structure present, missing only the additive tables the current release installs — is completed in place by installing those tables under the same version; that is schema completion of the recognized format, not a version migration. Explicit future version migration requires its own approved design; no generic automatic migration framework is needed for #50.

Version metadata does not prevent the current old executable from writing: it does not check user_version. Installation retirement must remove supported launch routes to that old runtime, retain the old path as archive-only, and prevent it from being pointed at the new path. Do not claim backward write compatibility or that a new PRAGMA defeats arbitrary old binaries. The deployment/access boundary and tests are specified in the [installation configuration](installation-config.md); arbitrary unsupported direct file mutation is outside managed guarantees.

Preflight accounts for consistent archive size, new-store initialization, WAL/temp space and preserved WIP. Insufficient space fails before activation; never delete evidence to make cutover fit. Archive/new-store avoids transforming a second full legacy runtime database, but archiving and future backups still consume storage. Archive/body/history defaults and disk-pressure behavior are settled in the [retention policy](retention-policy.md#configuration): the archive is `unlimited` by default and protected-over-limit pressure surfaces through Query and Attention. Before activation, rollback means remaining in maintenance and reopening the intact old setup only after explicit safety checks. After new decisions/effects, an old archive is not an automatic rollback target.

### Literal examples and limits

Values are illustrative, not fetched identities or executed migration evidence.

| Old state or new input | Required interpretation / result |
| --- | --- |
| Old `{"repository":"team/tool","number":17}` | Preserve those exact bytes in archive A0. Do not invent a stable repository/Issue ID or bind it to current config |
| Explicit new submission resolved as `{"repository":{"host":"h1","repository":"r42"},"issue":"i900"}` | New Work in Store S1, with its own command decision and source observation. Display path `team/tool` and number 17 remain labels only |
| Repository display path changes to `new/tool` | `(h1,r42)` and all existing new-format reservations/bindings remain equal |
| Same bare repo/Issue/event ID on h2 | `(h2,r42,i900)` and `(h2,source,event)` are distinct from h1; no false overlap/dedup |
| Old `branch:team/tool:fix:a` | Kept as old archive data. New resource key is structured `(h1,r42,"fix:a")`, not a string split/reconstruction from old state |
| Command K in archive A0, decision C0, digest D0 | Inspect C0 under archive identity. New-store `command_result(S1,C0)` is not a legacy result lookup; it returns wrong-store/not-found as appropriate. No legacy request codec or FNV recomputation is promised |
| New command K under S1 repeated with equal canonical bytes | Original saved decision/CommandId/sequence; Work/source changes cannot create another effect |
| New command K under S1 with changed canonical bytes | Explicit conflict; original decision retained |
| Same delivery `(h1,webhook-A,event-7)` and same canonical fact | One ingest effect; duplicate receipt. Same identity/different fact is conflict and preserved evidence, never replacement |
| W1 owns `(h1,r42,i900)` and manual/Selector submits W2 | Reject overlap with W1 identity, no partial W2/Execution creation. Pause/reapproval/unknown effects do not free it |
| W1 safely terminal, new explicit submission for i900 | New W2 can claim the Issue. Identical old proposal/command replay still returns W1's original result |
| W1 retries after W2 accepted i900 | Retry rejected for overlap; W2 claim is untouched |
| W1 revision 1 approved, pre-dispatch read detects an Issue change | Hold; revision 1 cannot authorize a new request. Append revision 2, preserve old invariants/approval; approval for exact revision 2 required |
| Issue edited after the last matching pre-dispatch read | The accepted observation/specification is the fixed request basis. Record the race limit; later detection holds subsequent uncommitted work without mutating the existing Invocation |
| Replan suggests more permissions or a different actor | Cannot mutate W1 binding or budget. Surface need for separately authorized new Work; no auto-mode bypass |
| Effect receipt for another repo/change or old spec/head | Preserve as invalid for this assessment; it cannot permit publication/Outcome for the current target |

The source archive is evidence of what the old installation recorded, not proof that it met the newer schema or current completion policy. Newly missing metadata is not retroactively fabricated. An absent archive/body, an intact archive valid for its captured revision, and a corrupt/mismatched archive are distinct report states. Keep the reason/checksum/provenance even if an archive is unavailable. New runtime never treats archive inspection as execution authority.

## Caller and persisted-data change inventory

This is a coordinated future code change map, not an implementation in this issue. Existing history remains archive-only; all active new-format callers move to the single neutral contract together.

| Current owner / stored fields | Target change | Historical treatment and planned proof |
| --- | --- | --- |
| `domain.rs` Work/GitHubIssueRef/WriteResource | Scoped neutral types, immutable binding, Work/specification revisions and claims | Legacy Work/refs unchanged in archive; test rename, cross-host collisions and source membership immutability |
| `command.rs` submit/claim/judgment/PrHeadSha/payload_digest | Neutral resolved inputs, Store/specification preconditions, versioned canonical payload plus byte equality | Legacy keys/decisions remain reference-only. Test all new Command kinds exact replay/changed-payload conflict |
| `observation.rs` source_event_id and PR facts | Tagged scoped delivery identity, exact subject/change/revision, producer and payload version | Unattributable old event IDs cannot become new dedup identities. Test duplicate/different-source/conflicting-fact cases |
| `store.rs` Effect/PrActionIntent/ReadView/TxView | Typed binding/spec/source/claim/action interfaces; no provider JSON in Core | Replace old active port and all callers together. SQL constraints enforce structural targets, Core enforces authorization |
| `work.rs` admission/work_resources/dispatch/retry | Publication from saved binding rather than first source Issue or current default; atomic overlap and version/source gates | Preserve lifecycle safety in new contracts; test concurrent submit/retry/reapproval/dispatch and rollback |
| `projection.rs` current head/findings/result queries | Store-scoped result, exact change target and current/approved specification; explicit unavailable/changed gate | New Queries do not merge old archive IDs. Same SHA on wrong change cannot produce readiness |
| SQLite `works.issue_refs`, resource strings | Normalized scope/source/claims; immutable specification snapshots | No old row transformation. Validate old bytes/counts separately and new foreign keys/tuple keys with examples |
| SQLite commands/event_log | New codec/version/canonical bytes and Store identity; existing saved-decision and event-order guarantees | Archive old digest/decision/sequence bytes unchanged; do not infer full original command bodies |
| SQLite requests/invocations/executions/stop_intents | Store/spec/invariant reference on requests; retain stable new I/request/process identities and budget guards | Old active process/action must be safe before cutover. No runtime import or cross-store request adoption |
| SQLite pr_action_intents/judgments/outcomes | Host-neutral approved actions, exact binding/role/specification/target and versioned evidence | Old combined-publication result is never renamed into separate successful push/create receipts; #51 completes payloads |
| SQLite observations/deferrals/attention_acks | Scoped event keys, exact source status and versioned actionable holds | Stale source/unavailable source remain distinct; acknowledgments never release claims or approve a new spec |
| Config and resolver | Stable host/repository/actor/config identity, minimum capability facts and isolated active/archive paths | The [installation configuration](installation-config.md) specifies credential/setup mechanics. Invalid/unknown evidence refuses/holds admission rather than guessing |
| CLI parse_issue/parse_resource/run_tick | Resolve display input outside Core, explicit publication binding even without source issues, refresh source facts before dispatch | The [execution loop](execution-loop.md#command-surface) specifies Command/result syntax and lifecycle. Legacy CLI is retired, not a second new-store path |
| Selector/proposal boundary | Stable canonical source/policy identity and common Core admission | No Selector runtime exists yet. Proposed contract checks cover manual fallback, stale proposals, overlap and terminal replay |
| Supervisor/Worker/host adapter/fixture constructors | New neutral typed requests/observations and per-effect receipts under original binding | Keep existing process/stop/budget contracts; #52 owns artifact handoff, #51 receipts, real integration remains later |
| TUI and tests / future skills | Update active constructors/shared ports together; TUI delivery stays deferred | Existing tests remain; shared same-head finding guard cannot be deferred with the TUI. No skill or live proof claimed |

## Extracted identity and admission obligations

The following reconciles the 40 extracted requirement targets initially assigned to #50 without a specification section. These are normative target obligations under the named #50 acceptance criteria, not claims of implementation. The source catalog remains historical evidence. Source mechanisms such as external cards, scheduler sweeps, key-suffix retries, and worker-owned publication do not become required features merely because their safety properties are retained.

For every row, absent evidence holds the dependent operation, exact valid evidence permits only the stated transition, and malformed, stale or mismatched evidence is retained as invalid with a reason. A definite not-started fact differs from an unknown writer/effect. Unknown facts never prove safe release or successful completion. Core owns decisions, SQLite owns atomic structural constraints, and execution/host adapters produce facts. Queries expose the saved subject, revision, policy, status and reason. Detailed evidence envelopes, isolation, configuration and CLI protocols are specified under #51, #52, the [installation configuration](installation-config.md) and the [execution loop](execution-loop.md); specifying this identity/admission slice does not mark those design targets complete.

The owner approved conflict detection with human handling, source-closure holds without automatic cancellation, verified rename versus approved ownership transfer, and deferral of external completed-result adoption. Operational thresholds and eligible handling policies remain configurable under the saved Work policy; configuration cannot bypass identity, evidence, approval or writer-safety guards. All human/repair holds follow prompt safe lane release and independent resource reservations in [recovery policy](recovery-policy.md#human-waiting-and-capacity).

| Requirement | #50 criterion | Target obligation | Planned proof |
| --- | --- | --- | --- |
| R59: Admission bounds | AC2 | The reference Selector proposes one Issue per Work. Configured workload bounds must fit the accepted time and attempt policy; absent bound evidence cannot justify a larger task. Rich grouping and unattended scheduling stay deferred. | An oversized proposal is refused or held with its bound; missing policy cannot silently increase workload. |
| R66: Archive recovery | AC3 | Recover identity from durable scoped records, not prose. Legacy archives remain read-only and cannot restore executable Work; interrupted fresh-store activation remains in maintenance until its durable state is verified. | Interrupt archive creation and activation; neither case imports legacy requests or launches a writer. |
| R68: Conflict episodes | AC2 | Identify a conflict by repository, change request, source head and target revision. Detect and request human handling in v0.1; automatic conflict repair is deferred. Repeated evidence of one episode cannot mint repeated execution requests. | Keep the source head fixed and advance the target revision: record a new episode, not a duplicate or an automatic repair. |
| R71: Rename and transfer | AC1 | Follow display renames only after stable repository identity, allowlist and permissions are verified. Ownership transfer holds dispatch/publication for explicit approval. A changed host, repository, actor or other immutable binding requires new Work. | Rename preserves identity; transfer blocks new effects until approval; a different stable repository cannot adopt existing Work. |
| R74: Event admission | AC2 | Bind external observations to managed repository/change and actual revision. Record missing or invalid provenance as unavailable or rejected; optional event delivery cannot independently dispatch, resume, or silently drop uncertain evidence. | An old-head or unbound event cannot authorize dispatch; record its disposition and reason. |
| R77: Judgment release | AC2 | Release checks current Work version, specification, head, finding decisions and original execution binding. A changed installation default cannot select a different executor or actor for an existing request. | Change defaults while a judgment waits; release preserves original binding and refuses a stale head. |
| R83: Recovery lineage | AC1 | Work identifies the accepted task lineage; Execution, Invocation and request identities identify distinct attempts. Continuations retain their Work and predecessor evidence; a deliberate new submission gets new Work. Similar paths or PIDs are not lineage proof. | A resumed Execution retains its Work; a new explicit submission receives a new Work; ambiguous predecessor identity remains unbound. |
| R87: Proof-gated retry | AC2 | An eligible retry creates a queued successor Execution with a new Command key and preserved predecessor outcome. Unknown effects or missing stop proof prohibit it. Exclusion of blocked proposals must not suppress independent eligible Work; no external-card retry-key ladder is imported. | Retry one safely failed Work once despite repeated delivery; unknown ownership creates no successor and independent queued Work remains eligible. |
| R88: Issue-split scope | AC2 | Automatic issue-split proposal and child-Issue creation are outside v0.1. Preserve the general rule that any future fan-out needs version-bound approval, per-effect durable identity and partial-effect reconciliation; admission cannot gain write authority from a proposal. | A split recommendation creates no child Issue or host action in v0.1; duplicate approval cannot become implicit publication authority. |
| R90: Decision evidence and launch guards | AC2 | Saved Command replay returns the original decision. Reusable assessments bind to source, base, policy and evidence versions; unverifiable assessments cannot authorize work. Triage caches are not required. Before managed worker launch, the actual permission boundary must be verified, not inferred from configured intent. | Changed assessment input invalidates its use for new admission; missing isolation proof prevents launch. Exact Command replay still returns its saved decision. |
| R91: Admission ownership | AC2 | The state-store transaction and scoped Issue claims prevent duplicate admission. External running labels are optional projections, never ownership proof or a requirement to import source label sweeps. Only the recorded holder can release its claim after safe settlement. | Concurrent submissions yield one owner; stale or missing labels cannot admit a second owner; an old terminal event cannot release a newer claim. |
| R122: External resume signals | AC2 | External activity is observation, not a Command or approval. Bind trusted actor and managed change to the current revision before assessment; keep unbound observations explicit. Comment-based control and webhook-driven automatic resume are not v0.1 requirements. | An unbound comment or self-produced event cannot release a human gate; an old-head review stays evidence only for that head. |
| R146: Honest admission outcomes | AC2 | Every evaluated proposal has a typed accepted, rejected, deferred or unknown result with identity and reason. A denied proposal is not no-work, and an unknown launch cannot free capacity or count as a fresh attempt. No per-repository scheduler is imported. | Exercise denial, unavailable source, duplicate request and unknown launch; each has one explicit decision and no duplicate dispatch. |
| R153: Proposal scope and display links | AC2 | Consumers verify proposal schema, source membership, policy and exclusion scope before admission. The Selector owns ranking; Core does not emulate richer grouping. User-visible links derive from verified scoped identities; invalid or cross-repository link input is unavailable, not fabricated. | A proposal leaking an excluded Issue is rejected; an invalid or foreign link cannot appear as an authoritative action target. |
| R158: Closed source and confirmed facts | AC2 | A source Issue closing holds new dispatch/publication for a human decision, not automatic cancellation. Stop active writers safely and release their lane after proof, retaining WIP and reservations. Unknown source state stays unknown. Only verified published heads may be reported as publication facts. | Close an Issue before launch and during execution: no new dependent effect, no inferred cancellation, and an independent Work runs after stop proof. An unreadable source or unverified head cannot establish a terminal fact. |
| R163: Idempotency conflicts | AC1 | Canonical semantic bytes and explicit scope define request equality. A changed payload under an existing key is a conflict; a new key cannot bypass active-Issue overlap or unsettled effects. Do not import suffix-based attempts to escape a saved decision. | Replay equal bytes once; changed bytes conflict; fresh keys cannot duplicate unfinished source-Issue ownership. |
| R164: Continuation and takeover | AC2 | A continuation requires exact Work/task/specification binding, retained usable checkpoint or WIP, writer-stop evidence, and remaining Work budget. No new card or remote-run takeover is inferred. Missing or ambiguous recovery identity holds the Work; no automatic outer-loop owner is created. | Unknown checkpoint identity or old writer blocks continuation; a valid checkpoint resumes under unchanged Work usage; exhausted budgets cannot be bypassed by a successor. |
| R174: Assessment cache scope | AC2 | Any cached assessment binds to source/dependency content, base revision and policy/model/prompt version. A mismatch requires fresh assessment; a cached refusal is never acceptance. Cache machinery and automatic intake remain outside v0.1. | Advance the base while source text stays equal: cached evidence cannot authorize admission. A fresh result records its actual revision. |
| R175: Atomic dispatch admission | AC2 | Core rechecks proposal identity, scoped Issue overlap, Work version and resources at the admission/dispatch transaction. Uniqueness conflicts or a superseding local observation prohibit launch; partial failure cannot leave an executable half-accepted Work. | Race manual and Selector submission, and change the observed base before dispatch; at most one valid owner/request commits. |
| R176: Concurrent resume | AC1 | Resume uses a fresh transactional Work-version check against reconciliation updates. A stale operator read cannot overwrite new process, effect, or policy evidence; conflicts return an explicit result requiring re-read. | Race resume with a reconciler commit; the stale version is refused and both original and new evidence survive. |
| R177: Non-destructive historical mapping | AC3 | Keep legacy records and their raw IDs unchanged in a verified read-only archive. Cutover preflight precedes explicit new-store activation. Historical display/mapping reports are not executable imports or a parallel legacy Core. | Repeat preflight without modifying the archive; interrupted cutover cannot adopt legacy pending requests into the fresh StoreId. |
| R181: Semantic request equality | AC1 | Define canonical identity fields separately from volatile provenance. Persist the schema/codec and canonical bytes; changing semantic task, repository or policy input conflicts under the same key. Fresh bookkeeping cannot rewrite the saved command payload. | Equal semantic inputs with reordered declared sets compare equally; changed task bytes conflict; raw evidence provenance remains retained separately. |
| R182: Serialized decisions | AC1 | Use SQLite transaction/version/uniqueness boundaries for one authoritative writer of Work state. Contention returns an explicit conflict and reread path. A held database lock is neither process liveness proof nor permission to force ownership. | Race state commits from two callers: no lost update, one version advances, and the loser cannot launch using stale state. |
| R184: Untrusted handoff validation | AC2 | Validate transferred proposal/task schema, digest, scope, repository and member identity before use, then independently check freshness. A bad payload is rejected as a whole; no string-based best-effort adoption or fallback to a different target. | Alter a repository, digest or member list in retained input; reject it without dispatch or mutation, while unchanged valid input still passes freshness checks. |
| R190: Configuration authority | AC1 | Require an allowlisted repository and validated adapter, actor, reviewer and permission policy. Unknown values refuse loading/admission. New defaults apply to new Work; accepted Work and unresolved effects retain their saved binding and versioned effective policy. | An unknown adapter or permission value fails explicitly; changing defaults cannot retarget accepted Work or its unresolved action. |
| R192: Durable records and corruption | AC1 | SQLite transactions preserve coherent records and versioned payloads. Missing, malformed, unsupported and valid stored records have distinct results. A corrupt request cannot be silently treated as absent or reused to start new work; retain diagnostic evidence. | Interrupt a transaction or corrupt a request payload; report its reason and do not infer that no prior writer/effect exists. |
| R240: Agent proposals and bounded diagnosis | AC2 | Agent input is bounded to the accepted task and dependencies supplied as context. Parse only the declared schema and closed decision vocabulary; free text is not approval. Operator diagnostics use bounded reads and expose unavailable facts; no cross-task diagnostic subsystem or automatic regrouping is introduced. | An unknown verdict or extra-scope task is refused; a timed-out diagnostic read reports unavailable without substituting another proposal. |
| R243: Declared scope constraints | AC2 | Admission and effect guards enforce the accepted allowed/forbidden scope and retain each failed condition. Unknown affected scope cannot grant broader effects. Source maintenance-discovery and issue-generation workflows remain outside v0.1. | A proposed effect outside accepted scope or with unverifiable scope is refused with a structured reason; an agent recommendation cannot override it. |
| R254: Profile provenance and recovery | AC1 | Validate explicit config and preserve provenance before acceptance. Optional profile scaffolding is not a v0.1 requirement and cannot invent observed facts. Profile changes cannot rewrite an accepted task or discard old evidence; explicit policy updates preserve usage, and binding changes require new Work. | Unreadable config fails before dispatch; a policy update preserves old policy/evidence and counters; a changed actor cannot edit the accepted binding. |
| R255: Prepared input and resumable frontier | AC2 | Bind source snapshot, exact base, repository and versioned effective policy to the prepared task. Persist explicit permitted overrides. Missing requirements or uncertain evidence hold the Work. Structured stop/resume retains the exact frontier and original claims; the CLI schedules backoff without new execution authority. | A mismatched source/base/policy cannot reuse a prepared request; a no-effect query does not spend a new attempt; invalid evidence cannot release claims. |
| R290: Credential references | AC1 | Keep credential references opaque in Core and resolve them at the boundary. Missing, empty, unsupported or unresolved required references explicitly fail; never dispatch anonymously as a fallback or expose secret values in output. The concrete schemes and repair protocol are specified in the [installation configuration](installation-config.md#credential-references-and-secret-hygiene). | Missing and unknown credential references refuse dependent operations with sanitized errors; successful resolution proves the saved actor before use. |
| R297: Validated configuration selection | AC1 | Resolve one explicit or unambiguous profile and validate it through the same contract used at dispatch. Refuse unknown fields, conflicting repository identities, invalid role models and ambiguous selection. Save content digest and effective policy; exact syntax is in the [installation configuration](installation-config.md#configuration-schema). | Two matching profiles require explicit selection; an invalid model or mismatched repository fails loading; changed bytes cannot silently reuse the old effective policy. |
| R300: Work-wide attempt identity | AC1 | Count automatic dispatch/recovery usage durably under Work identity across Invocation, Execution and specification revisions. Classify stopped outcomes and preserve task/policy binding; repeated invocation cannot buy an identical deterministic refusal or reset usage. | Reapprove a specification and restart after failed dispatch: consumed Work usage remains, and a deterministic refusal is not automatically repurchased. |
| R301: Bounded fresh dispatch | AC2 | Persist a charged automatic attempt before launch. At the Work cap, hold for explicit human action without launching or flapping external projections. Resume is not an exemption from remaining time, attempt, scope or stop guards. | Crash after attempt charge but before worker acknowledgment; restart counts it once and reconciles the same request; a capped Work cannot launch via resume. |
| R303: Bounded correction lineage | AC2 | A code/check failure may request a counted correction based on the exact retained produced change. Record each child Invocation and verification before promotion. Environmental failures route to repair/retry policy, not blind code repair; prior results cannot satisfy changed required checks. | Two correction attempts use their actual predecessor artifacts and preserve cumulative usage; changed checks require fresh verification; missing report triggers reconciliation rather than duplicate correction. |
| R305: Preview evidence isolation | AC1 | Preview or simulated evidence carries a distinct mode and identity and cannot authorize a live gate or overwrite real records. A dry-run capability is not authority to claim a real effect. #51 and the [execution loop](execution-loop.md#cli-contract) specify output envelopes and CLI behavior. | Copy simulated evidence into a live location; mode/identity validation still refuses it and the real check must run. |
| R325: Canonical storage and member claims | AC1 | Store/Work identity chooses one managed state/workspace binding, not mutable display names or current directory. Preserve host scope. When a fixed source set is accepted, claim all members atomically; rollback removes only this transaction's changes, never another holder's claims. Rich grouping stays deferred. | Colliding display names on different hosts remain disjoint; a conflict on one source member leaves no partial new claims and never deletes an existing holder. |
| R326: Explicit budget extension | AC1 | An authorized policy update extends the recorded bound, not a caller-supplied replacement baseline. Preserve consumed counters, findings and evidence; expanding an exhausted bound needs explicit approval. Exact Command syntax is in the [execution loop](execution-loop.md#command-surface). | Extend a recorded bound from one to two after one consumed attempt: usage stays one; a stale update conflicts; no evidence or approval history is replaced. |
| R327: Manual repair versus adoption | AC2 | Dedicated external completed-result adoption is deferred. In v0.1, a manual writer requires managed workers stopped and an external resource claim; release that claim before normal resume. New managed verification/review must bind the actual repaired artifact and revision, never an imported success assertion. | An external success report cannot clear a gate; claimed manual workspace repair followed by fresh managed verification can proceed without a second concurrent writer. |
| R333: Collision-safe invocation evidence | AC1 | Allocate distinct Invocation identities and durable evidence outside disposable checkout bodies. Enforce uniqueness atomically rather than with a path-exists guess. A continuation may reuse retained workspace only with a new Invocation and original Work lineage. | Force an ID collision and concurrent allocation: only one identity is accepted; resume uses a fresh Invocation and cannot overwrite prior evidence. |

### Manual repair ownership

A paused Execution normally retains its workspace and branch reservations, so an unrelated external claim must still conflict. For an explicitly authorized manual repair, transfer the existing reservation to the named external actor in one version-checked transaction after managed writer stop proof. Save a durable delegation record binding Store, Work, Execution, resources, actor, policy, and prior reservation. Keep the resource continuously reserved and keep source-Issue claims intact; never make it temporarily free to let a human claim it. Only the delegated actor may repair those local resources, without host-publication authority.

Resume requires evidence that the delegated writer stopped or cannot write, explicit return of the reservation, and atomic restoration to the paused Execution before queue admission. External-claim expiry alone cannot prove that the manual writer stopped. A crash or uncertain delegate retains exclusion and cannot launch a managed replacement. The [installation configuration](installation-config.md) and the [execution loop](execution-loop.md) specify authorization, the delegation/return Commands and stop evidence; the existing external-claim API alone is not proof this flow is implemented. Dedicated import of an external run's completed result remains deferred. Future tests race delegation with resume, crash before and after transfer/return, and let a claim expire while its writer lives; no case permits concurrent manual and managed writers.

No source-specific test or implementation status is promoted to target runtime proof by this mapping. The inventory checker must report zero pending targets for #50 while retaining every other unfinished design target and `complete: false`.

## Verification plan and six aspects

| Aspect | Applicability and rationale | Proposed verification / remaining boundary |
| --- | --- | --- |
| State transitions | Acceptance, retry, reapproval and terminal release compete over claims and versions | Two concurrent overlapping requests yield one accepted owner; reapproval versus dispatch cannot launch an old spec; terminal replay cannot release a new owner's claim |
| Failure semantics | Unknown read/effect and interrupted cutover cannot mean safe release | Reject malformed/newer schema without writes; disk-full/bootstrap crash leaves no runnable partial state; old uncertain writer blocks unsafe cutover; unknown source holds rather than appears unchanged |
| Trust boundaries | Display input, adapter claims and archive content are not authoritative identities | Wrong-store, out-of-allowlist, cross-repo action, wrong actor, unsupported/denied/unknown capability and malformed snapshot fixtures; no network I/O inside Core |
| Identity invariants | Rename, overlapping native IDs, changed source/spec and config must not retarget Work | Exact tuple/encoding checks, byte-level replay, same SHA on wrong target, scoped delivery collision tests, immutable binding post-seal insertion/update/delete refusal |
| Information preservation | Archive does not reconstruct omitted old metadata; new spec cannot inherit approval | Check archive checksum/counts/raw history and invalid/absent states; compare new old/new spec and approval refs; original bytes remain distinct from normalized display views |
| Resource bounds | Maintenance/archive and future payload/history growth need bounded work | Preflight capacity and bounded batches/record sizes; no cleanup to satisfy cutover. #54's [retention policy](retention-policy.md) selects body/history retention and pressure defaults; the [execution loop](execution-loop.md) schedules bounded source refresh/holds |

Required future tests are literal contract scenarios against the public Command/Query/adapter surface and SQLite structural constraints, not assertions that implementation symbols exist. Include fresh bootstrap and unknown-version refusal; wrong Store binding; all identity collision cases; same/different key replay; exact original proposal replay after terminal; manual/Selector overlap and retry after new owner; source change/unavailable/reapproval races on both sides of the final read boundary, Core-required read versus CLI scheduling; no budget reset; impossible actor/repository substitution; delayed old-head facts; immutable receipt/approval target; archive inconsistency and maintenance restart. These are planned tests, not executed runtime evidence.

Existing fmt/build/test/clippy on runtime revision `eb1c2f1` establish only the old implementation baseline. The SQL/type sketches require review and later implementation checks; none proves a real host, Worker, runtime schema migration, auto mode or live E2E. #51–#56 complete detailed evidence, configuration, retention, CLI and integrated design acceptance. Do not create implementation issues or start runtime work from this design document alone.

## Alternatives and owner review

Two independently produced shapes were compared: relational identity/role/capability constraints with versioned payloads, and a smaller immutable binding envelope with extracted SQL indexes. The relational base is retained where actual cross-record FK/uniqueness is needed; arbitrary artifact/check contents are not normalized into extra tables. This accepts a few identity tables in exchange for preventing repository/actor/claim mismatches at persistence boundaries. The envelope-only alternative is smaller but would move all cross-record actor membership guarantees into application validation; no duplicate authoritative binding JSON is added.

The owner chose archive/new-store, so both candidates' executable legacy conversion/replay paths are removed. Copy-versus-in-place transformation of old rows is no longer a decision to implement. Preserve archive/read access and safe cutover rather than adding a compatibility wrapper or reconstructing absent provenance. Transactional bootstrap/version checks apply to the new store; full online migration and generic migration engines are excluded.

The remaining detailed evidence/invariant approval schema is #51; credential/config/access enforcement is settled in the [installation configuration](installation-config.md); retention defaults and history growth limits are settled in the [retention policy](retention-policy.md); Command syntax, source-hold interaction and future automatic regeneration protocol are in the [execution loop](execution-loop.md). Reviewers must preserve the settled manual v0.1 source-change hold and future auto-mode extension rather than selecting arbitrary thresholds or enabling it silently. The final concrete sketch and its tradeoffs require owner review before #50 is complete.

## Approved recovery and configuration policy

Use the owner-approved [recovery and configuration policy](recovery-policy.md) for configurable defaults, Work-wide accounting, policy changes, and safe human-wait capacity release. The [post-v0.1 milestones](post-v0.1-milestones.md) retain the seven approved machinery deferrals without deferring v0.1 safety requirements. Both documents specify target behavior, not implemented runtime support.

## v0.2 intake records

The durable intake run/candidate records for v0.2 — identities, states, crash-point recovery, capacity deferral, store upgrade, and query shapes — are specified in [intake identity, persistence, and restart recovery](intake-identity-storage.md) (#206). They extend this document's command-ledger replay and overlap rules without changing the v0.1 admission contract; the external Selector wire contract remains [selector-protocol.md](selector-protocol.md).
