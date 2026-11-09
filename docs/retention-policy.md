# Retention policy, cleanup safety, and history growth limits

Status: concrete design for #54. This document specifies future Rust/SQLite/filesystem behavior; it changes no runtime, configuration, or live process, and it performs no cleanup. It consumes #50's store/identity inventory, #51's evidence envelopes, and #52's worker/artifact pins. The owner approved the category limits and defaults, the external content-addressed body store (CAS), the pressure surfacing policy, the cleanup scheduling model, protective-dependency pin semantics, and the sweep bounds recorded below.

## Owner decisions

| Choice | Result |
| --- | --- |
| Metadata versus bodies | Identity, decision, command, judgment, finding, counter, claim, and event metadata is never reclaimed. Reclaimable state is bodies: worker out directories, workspaces, handoff-repo git objects, and CAS objects holding canonical payload/event/verification bodies |
| Body storage | Bodies live in a content-addressed store under the installation state root at `bodies/<digest[:2]>/<digest>`; the database stores digest references. `body_catalog` records digest, kind, bytes, availability state, registration time, and protective dependencies. SQLite rows are never compacted in place: large payloads are CAS bodies from creation |
| Limit axes | Every category has `max_age_seconds` and `max_bytes`. An omitted key applies the default; the literal `unlimited` disables that axis for that category; `0`, negative, unknown, and mixed-invalid values are configuration load errors — never silently "no limit" |
| Category defaults | workspace and worker out directory: reclaimable Work-terminal + 30 days; handoff-repo objects, canonical payload bodies, and event/verification bodies: Work-terminal + 90 days; legacy archive: `unlimited`. Byte caps: workspaces 20 GiB, out directories 10 GiB, CAS 40 GiB, handoff repository 10 GiB |
| Pin semantics | Only protective dependencies pin a body: nonterminal Execution or held reservation, unacknowledged handoff, unresolved host effect, a current gate, a retry-eligible workspace, and a live (unexpired) external claim on affected resources. Historical evidence references do not pin; when the body is reclaimed they resolve to explicit `unavailable` references |
| Pressure behavior | Reclamation never deletes pinned bodies. If protective bodies alone exceed a configured byte cap after a sweep pass, the report exposes `protected_over_limit` per category and raises or refreshes an Attention item. New Work admission is not blocked by pressure |
| Scheduling | The CLI run loop runs a bounded retention sweep every `retention.sweep.interval_seconds`; explicit operator commands `retention report` and `retention cleanup` exist, with `cleanup` defaulting to a dry run. Physical deletion is runtime tooling driven by store-authoritative state; Core never performs filesystem I/O |
| Configuration scope | Retention configuration is installation-wide. There are no per-Work overrides; the accepted-Work policy immutability rule does not apply because pins, not policy, carry per-Work protection |

## Durable metadata versus reclaimable bodies

Two classes of state follow different rules.

Durable metadata is never reclaimed: `store_metadata`, host/repository identity rows, Work and binding rows, specification headers, commands and saved decision bytes, action intents and receipts, judgments/finding occurrences/decision history, counters and usage charges, claims and reservations, invocation/request identities, event-log headers (sequence, kind, subject, digests), importer checkpoints, and queue-drop counters. These rows are small and carry identity, authorization, and provenance. No age or size limit applies to them, and no sweep, compaction, or configuration value may delete them. Cleanup must never erase recovery counters or reset retry limits.

Reclaimable bodies are content that the metadata references by digest:

- **CAS objects**: canonical payload bodies (source snapshots, specification payloads, invariant sets), event payload bodies, and bounded verification/observation outputs. Stored under `bodies/<digest[:2]>/<digest>` and catalogued in `body_catalog(digest, kind, bytes, state, registered_at)`. Identity is the digest; location and availability are catalog state, so bodies can be reclaimed without rewriting any referencing record.
- **Worker out directories**: `report.json`, `runtime.log`, `checkpoint.json`, and the transient export bundle. Reclaimed as a unit; individual digests in the handoff record and evidence remain valid references that resolve to `unavailable`.
- **Workspaces**: managed checkouts including WIP commits on the managed branch. Reclaimed as a unit.
- **Handoff-repo git objects**: objects reachable only through deletable `refs/taisan/artifacts/<artifact_id>` refs. Reachability is git's concern; the catalog tracks refs, not objects.
- **Legacy archive**: the pre-cutover database archive. `unlimited` by default; it is evidence, not working state.

`body_catalog.state` is one of `present`, `reclaiming`, `reclaimed`, `unavailable`. `reclaimed` and `unavailable` are terminal for the body, not for the reference: the digest, kind, size, registration time, and reason remain queryable forever.

### Alternatives considered

| Alternative | Outcome and rationale |
| --- | --- |
| Age limits only | Rejected. Age alone cannot bound a burst of large bodies inside the age window — the issue's "history growth limits" concern. Both axes adopted. |
| Byte caps only | Rejected. Pure size pressure discards the oldest evidence exactly when history is densest and provides no predictable retention horizon for replay/provenance questions. |
| In-place SQLite compaction (rewriting event_log/payload rows) | Rejected. Rewriting history rows risks the durable-metadata invariant and complicates replay; CAS offload achieves reclamation without touching a single metadata byte. |
| External archive files for compacted history | Deferred to post-v0.1. Useful for very old stores but adds a second availability regime; v0.1's `unavailable` state already covers reclaimed bodies honestly. |
| Reclaim on access (lazy) | Rejected. Unbounded deletion latency makes pressure unpredictable; explicit sweep + commands keep reclamation auditable and bounded. |
| Per-Work retention overrides | Rejected for v0.1. Pins already carry per-Work protection; per-Work limits complicate accounting and invite a Work whose policy outlives its own pinned data's meaning. |

## Configuration

Target keys, not implemented options. They follow [recovery policy](recovery-policy.md#configurable-policy-and-defaults): every key has a type, unit, default, range, and explicit disable semantics; unknown keys and invalid combinations fail at load. The `retention.` prefix composes under the top-level `[retention]` TOML table — there is no `policy.` namespace.

| Target key | Default | Meaning |
| --- | --- | --- |
| `retention.workspace.max_age_seconds` | `2592000` | Age limit measured from owning Work terminal time |
| `retention.workspace.max_bytes` | `21474836480` | Total cap across managed workspaces |
| `retention.worker_output.max_age_seconds` | `2592000` | Out-directory age from owning Work terminal time |
| `retention.worker_output.max_bytes` | `10737418240` | Total cap across out directories |
| `retention.cas.max_age_seconds` | `7776000` | CAS body age from owning Work terminal time, or registration time when unattributed |
| `retention.cas.max_bytes` | `42949672960` | Total cap across CAS objects |
| `retention.handoff_objects.max_age_seconds` | `7776000` | Ref lifetime from owning Work terminal time |
| `retention.handoff_objects.max_bytes` | `10737418240` | Handoff repository size cap |
| `retention.archive.max_age_seconds` | `unlimited` | Legacy archive age; `unlimited` default |
| `retention.archive.max_bytes` | `unlimited` | Legacy archive size cap |
| `retention.sweep.interval_seconds` | `3600` | Minimum interval between automatic sweep passes |
| `retention.sweep.max_bodies` | `256` | Maximum bodies reclaimed per pass |
| `retention.sweep.max_bytes` | `1073741824` | Maximum bytes reclaimed per pass |
| `retention.sweep.delete_error_limit` | `3` | Consecutive failed passes on one body before Attention |

Semantics:

- Omitted key: the default applies. No key means "no limit".
- `unlimited`: that axis does not limit the category; the other axis still does. `unlimited` on both axes means never reclaimed by policy — the archive default.
- `0` is a load error, not immediate reclamation and not "no limit". A zero-length window cannot express intent safely.
- Negative, non-integer, unknown-unit, or unknown-key input fails configuration loading with the offending key named.
- A lowered limit applies to future eligibility decisions; it cannot un-pin protected bodies, and a raised limit cannot resurrect a `reclaimed` body.
- Reclaim ordering inside a pass is deterministic: unpinned eligible bodies oldest-first (by `registered_at`, then digest) until under the cap or the pass bound is hit. A pass that stops early leaves an honest partial result; the next pass continues.

## Dependency pins

A protective dependency is a live obligation whose loss would break a safety invariant. Bodies with any protective dependency are ineligible regardless of age or size pressure.

| Protective dependency | Bodies it pins |
| --- | --- |
| Nonterminal Execution or held resource reservation | Its workspace, out directories of its Invocations, checkpoint bodies, handoff refs of its artifacts |
| Unacknowledged artifact handoff | The out-directory bundle, the handoff ref being established, the workspace producing it |
| Recoverable missing/corrupt artifact while a recovery bound remains | Out directory, workspace, and checkpoint bodies that recollection could still read |
| Unresolved host effect | Payload bodies and handoff refs needed to reconcile the saved intent |
| Current gate (pending judgment, open Attention dependency, unsettled Outcome subject) | Bodies referenced by that gate's evidence requirements |
| Retry-eligible terminal Execution (Work nonterminal) | Its workspace and branch state — `retry`/`resume` resume into them |
| Live external claim (unexpired TTL) | The workspace carrying WIP for the claimed branch — a live claim is evidence a manual writer may still act; an expired claim removes this pin but never authorizes deletion by itself |

Historical references — an old Outcome's evidence list, a superseded judgment round, a completed command's payload ref — are not protective dependencies. They remain valid metadata references and resolve to `unavailable` once the body is reclaimed.

Pin bookkeeping lives in `body_catalog` as explicit dependency rows, not as reference counting by convention: every lifecycle site that creates a protective dependency inserts a row, and the site that discharges it deletes it. Pin creation against a body in `reclaiming`, `reclaimed`, or `unavailable` state is refused; the requesting operation sees the body as unavailable and follows ordinary missing-evidence rules. This refusal closes the race between eligibility marking and deletion.

## Cleanup eligibility and failure matrix

Eligibility is recomputed inside the marking transaction — a body qualifies only if, at commit time, it has no protective dependencies, its owning Work is terminal, and the applicable age or byte limit is exceeded. Between decision and deletion no lifecycle event may create a pin, because pin creation on a non-`present` body is refused.

| Candidate | Eligible when | Failure and edge behavior |
| --- | --- | --- |
| Active or unproven writer's workspace/out dir | Never while the writer is live or its stop is unproven | `lsof`/metadata/inspection failure means retain with a warning observation; an unproven writer is treated as live |
| Paused WIP (reservations held) | Never while `paused` — manual repair and `resume` may need the workspace | Reservations released only by lifecycle events; retention cannot release them |
| Artifact under bounded recovery | Never while the recovery bound has attempts remaining | Missing/corrupt artifact keeps the recoverable pin; bound exhaustion, not cleanup, ends recovery |
| Bodies reconciling an unknown host effect | Never while the effect is unresolved | Reconciliation window expiry escalates to human action; it does not make the effect absent or the bodies disposable |
| Bodies of a pending judgment or unsettled Outcome | Never while referenced by a current gate | Stale-for-revision evidence is retained as history; it stops pinning only when no current gate references it |
| Terminal Work bodies | When `terminal_time + max_age_seconds` elapsed, or the category exceeds `max_bytes` and the body is unpinned | Oldest-first deterministic order; partial passes leave remaining eligible bodies for the next pass |
| Expired external claim | Claim release is a metadata transaction that re-verifies the record generation under lock before commit | Expiry is not stop evidence and never authorizes body deletion; a claim refreshed mid-release wins — the release transaction fails and the claim stands |
| Handoff-repo objects | A ref is deleted only when unpinned, terminal, and aged; `git gc --prune` then reclaims unreachable objects within the pass budget | Objects shared with surviving refs persist. A gc failure is a pass error, retried under `delete_error_limit`; never a reason to force-delete a live ref |
| Foreign or unattributable bodies | Never reclaimed by the sweep | Anything not attributable to managed identity is evidence of a possible unknown writer; only an explicit operator repair decision may remove it, recorded as an auditable Command |
| Retired-legacy candidates (audit classification) | A `legacy candidate` classification is metadata about disuse evidence; it produces no removal effect | Removal requires the separate explicit eligibility gate — positive evidence plus terminal state — never audit output alone |
| Importer checkpoints, drop counters, journal heads | Never | They are durable metadata, not bodies |

Concurrent access: readers resolve a reference through `body_catalog`. A body read that races deletion returns `unavailable` with the recorded reason rather than an error; no reader may observe torn state. Deletion is single-owner: one sweep or explicit cleanup holds the installation maintenance lock for the duration of a pass, and the lock does not block ordinary Command/Query traffic — deletion happens outside Core and outside the store transaction.

Interrupted deletion: the two-phase `reclaiming` → `reclaimed` protocol makes partial deletion safe. A crash between marking and deleting leaves a `reclaiming` row; the next pass rechecks pins, deletes again if the body still exists, and commits `reclaimed` with the actual outcome. A crash after deleting but before committing resolves the same way: the body is already absent, so the commit records `reclaimed` with reason `missing`. Delete failures leave the row `reclaiming` for idempotent retry; after `delete_error_limit` consecutive failed passes for the same body the sweep records the failure and raises Attention — it does not force, rename, or truncate the body.

Restart and idempotency: a sweep pass is fully re-runnable. Re-running after completion finds no `reclaiming` rows and no newly eligible bodies; re-running after interruption resumes the marked set first. Two concurrent cleanup invocations serialize on the installation maintenance lock; the loser reports `already_running`.

## Scheduling and ownership

The CLI run loop invokes a retention pass when `retention.sweep.interval_seconds` has elapsed since the last completed pass, bounded by `max_bodies`/`max_bytes` per pass. The pass performs no lifecycle decisions: the store supplies eligibility and pin state, the pass executes deletions, and each reclaimed body lands in a `reclaimed` commit that records identity, kind, bytes freed, reason, and the sweep pass identity. Core's lifecycle and release guards are untouched — the pass cannot release reservations, settle Work, or erase counters.

Explicit commands give operators the same mechanism on demand: `retention report` is read-only and shows the accounting table below; `retention cleanup` is a dry run by default and requires `--apply` to execute, honouring the same per-pass bounds unless `--max-bodies`/`--max-bytes` override them. Both report exactly what a pass would reclaim and what pins prevented.

The sweep never runs inside the SQLite transaction that marks eligibility, never holds the store write lock during filesystem deletion, and never applies backpressure to lifecycle operations. If the store is busy, the sweep defers; if bodies are being written, its candidates were already filtered by the marking transaction.

## Accounting, pressure, and repair paths

Accounting is a read-only derived report over `body_catalog` plus protective-dependency rows and filesystem stat checks. It mutates nothing; a corrupt or unreadable filesystem region contributes `unknown`, never `0` bytes.

`retention report` output per category:

- `present` / `pinned` / `eligible` / `reclaiming` counts and bytes
- cumulative `reclaimed` and `unavailable` counts and bytes, by reason
- `pressure`: `none`, `over_limit` (reclaimable excess exists), or `protected_over_limit` (pinned bodies alone exceed the cap)
- `unknown` counts where stat or catalog reads failed, with the failed region named

Report output is bounded: per-category aggregates, not per-body listings, with an explicit `truncated` flag when a listing form would exceed the report bound.

Pressure rules:

- After each pass, if a category's pinned bytes alone exceed its `max_bytes`, the sweep writes a `protected_over_limit` pressure record and raises or refreshes an Attention item naming the category, configured cap, protected bytes, and the top protective dependencies. Protected data is retained; nothing is deleted to satisfy the cap.
- If eligible bodies remain after a bounded pass (reclamation is behind), the report shows `over_limit` without Attention; the next scheduled pass continues.
- Pressure never blocks admission, never forces Work terminal, and never loosens a pin. The operator's repair paths are: raise the limit, resolve the lifecycle state holding pins (finish/terminate Work, settle effects), or run explicit cleanup — each an auditable decision.
- Unreadable or corrupt regions surface as `unknown` in accounting and as a failed-region warning; repair is operator-driven, never automatic deletion.

## Evidence availability states

Every evidence reference resolves to one of four states in queries; the state, reason, and identity are retained permanently in metadata.

| State | Meaning | Gate effect |
| --- | --- | --- |
| `absent` | No record ever existed for the reference | Cannot satisfy a requirement; absence is never an empty success |
| `present` + valid | Body exists, digest matches, and it is valid for the exact schema/identity/revision/criterion | May satisfy the gate it was produced for |
| `present` + invalid | Body exists but is malformed, stale for the required revision, or mismatched for the required identity/criterion | Retained with its reason; cannot satisfy; may demand reassessment |
| `unavailable` | Body was reclaimed (`age_expired`, `size_pressure`), not found at deletion (`missing`), failed integrity (`corrupt`), or its region is unreadable | Retained with identity and reason; treated like absent for gating but reported distinctly — reclaimed-by-policy is different from never-existed |

A digest alone never reconstructs a body. A reference whose body is `unavailable` stays a first-class reference: queries, replay, and Attention distinguish "the evidence existed and was reclaimed under policy P" from "no evidence was recorded". Nothing about reclamation may make an unknown effect appear absent or an invalid body appear valid.

## Literal examples

Values are illustrative, not executed evidence.

| Scenario | Before | After |
| --- | --- | --- |
| W1 terminal at T0; T0+40d sweep runs | Workspace `ws-w1` present, out dir with `report.json`/`runtime.log` present, no pins | Both directories deleted; catalog rows `reclaimed`/`age_expired` with identities and bytes freed; evidence refs to `report.json` now resolve `unavailable` |
| W2 `paused` at T0+60d; workspace over age | Pin row `reservation held` exists | Nothing reclaimed; `retention report` counts it under `pinned`; resume at T0+70d still finds WIP |
| W3 failed at T0; `retry` still allowed at T0+45d | Workspace pinned (retry-eligible) | Retained. If the operator instead terminally settles W3, the pin row is discharged and the workspace becomes eligible after the age window |
| Artifact handoff ref for W4, terminal at T0, T0+95d | Ref `refs/taisan/artifacts/a-4` present, unpinned | Ref deleted; bounded `git gc --prune` removes objects reachable only through it; base objects shared with other refs survive |
| CAS body `d9` referenced by W5's superseded spec and W6's live gate | Two refs; W6's gate is protective | `d9` retained while W6's pin stands; after W6 terminal + 90d with zero protective deps, `d9` is reclaimed and both references resolve `unavailable` |
| CAS at 41 GiB of 40 GiB cap, all of it pinned | No eligible bodies | `protected_over_limit` Attention: category, cap, protected bytes, top pins; zero deletions; admission continues |
| Sweep crash after marking 12 bodies `reclaiming`, deleting 5 | 5 bodies deleted, 7 on disk, all 12 `reclaiming` | Next pass: 5 commits record `reclaimed`/`missing`, 7 delete and record `reclaimed`/`age_expired`; totals and pass identity recorded once |
| `retention cleanup` (no `--apply`) on a quiet store | 3 eligible out dirs | Report lists the 3 candidates and bytes; nothing deleted; exit indicates dry run |
| Command K replayed after its payload body reclaimed | Saved decision bytes in `commands` (metadata) | Exact replay returns the original decision; the payload-body reference resolves `unavailable`; replay never needs the body |
| Body `d7` fails digest check during accounting | Catalog `present`, bytes on disk disagree | `unavailable`/`corrupt` with identity retained; never served and never silently deleted as if absent |

## Planned verification

Named checks for the retention contract suite (RT). These are planned contract scenarios, not executed results; they run through future public Command/Query/tooling paths.

| Check | Scenario | Required result |
| --- | --- | --- |
| RT-01 | Sweep over aged store | Metadata rows (commands, judgments, counters, claims history, event headers, checkpoints, drop counters) byte-identical before and after |
| RT-02 | Out dir before handoff acknowledgement | Never reclaimed regardless of age config |
| RT-03 | Workspace while Execution nonterminal, `paused`, or retry-eligible | Pinned; reclaimable only after Work terminal + age |
| RT-04 | Handoff ref with unresolved effect vs. terminal-aged ref | First pinned; second deleted and gc prunes only unreachable objects; shared objects survive |
| RT-05 | Expired claim release | Generation re-verified in the release transaction; refreshed claim wins; zero body effects |
| RT-06 | Foreign/unattributable body next to managed ones | Sweep reclaims managed eligibles only; foreign body retained and reported |
| RT-07 | Crash between mark and delete, and between delete and commit | Resume is idempotent; `missing` vs `age_expired` reasons recorded correctly; no double-counted freed bytes |
| RT-08 | Pin creation racing a `reclaiming` mark | Pin creation refused; requester sees `unavailable` |
| RT-09 | Pinned bytes over `max_bytes` | Zero deletions; `protected_over_limit` pressure record and Attention; Query exposes it |
| RT-10 | Unreadable region during accounting | Counts report `unknown`, not `0`; warning recorded; sweep continues elsewhere |
| RT-11 | Evidence ref to reclaimed body | Query returns `unavailable` + reason, distinct from `absent` and `invalid` |
| RT-12 | Digest mismatch on catalog read | `unavailable`/`corrupt`; metadata retained; body never served |
| RT-13 | Config with `0`, negative, unknown key, `unlimited` on both axes | First three fail load naming the key; `unlimited` pair accepted and never reclaims |
| RT-14 | Eligible set larger than pass bounds | Exactly `max_bodies`/`max_bytes` reclaimed, oldest-first; remainder reported eligible for next pass |
| RT-15 | Shared CAS body, one protective + one historical ref | Survives while the protective pin stands; reclaimed after pin discharge + age |
| RT-16 | Command replay after payload-body reclaim | Original saved decision returned; body ref `unavailable` |
| RT-17 | Event payload body reclaimed | `event_log` row with sequence/digest survives; body ref `unavailable` |
| RT-18 | Importer checkpoint and drop counter present during sweep | Never reclaimed; ingestion resumes from checkpoint |
| RT-19 | Audit report calls a store region "legacy candidate" | Classification alone triggers no removal; explicit eligibility gate still required |
| RT-20 | Terminal unlinked provisional claim/record | Archived per R111, never deleted by sweep |
| RT-21 | `retention cleanup` default vs `--apply` | Default produces report only; `--apply` performs bounded reclaim; `--max-*` overrides bounds |
| RT-22 | `reclaiming` body whose pin is discharged mid-pass | Pin rows cannot appear on non-`present` bodies; the in-flight mark completes safely |
| RT-23 | Delete error three passes in a row | Third failure raises Attention with the body identity; earlier passes retried idempotently |
| RT-24 | Lower `max_age_seconds` below current ages | Future eligibility uses the new limit; no retroactive unpinning; lowered limits never resurrect `reclaimed` |

## Requirement mapping

All 18 inventory targets owned by #54 are mapped in [retention requirements](retention-requirements.md). The governing clauses above name the mechanism each requirement's retention slice binds to.

## Six design aspects

| Aspect | Risk without control | Control in this design | Open / deferred |
| --- | --- | --- | --- |
| State transitions | Reclaim could race a lifecycle event that newly needs the body | Two-phase `present`→`reclaiming`→`reclaimed`; eligibility recomputed in the marking transaction; pin creation refused on non-`present` bodies | Exact Command/Query schema for `retention report`/`cleanup` belongs to the [execution loop](execution-loop.md#command-surface) |
| Failure semantics | Crash mid-delete could lose bodies without a record, or a delete error could loop silently | `reclaiming` survives restart; outcomes recorded `missing`/`reclaimed` idempotently; `delete_error_limit` escalates to Attention | gc failure taxonomy is bounded to pass errors |
| Trust boundaries | Foreign/unproven-writer bodies could be mistaken for managed leftovers | Unattributable bodies are never sweep-eligible; only an explicit operator Command removes them; `lsof`/metadata failure means retain | Foreign-body repair Command syntax is [execution loop](execution-loop.md#command-surface) scope |
| Identity invariants | A reclaimed body could masquerade as absent evidence; a shared body could be freed early | Catalog states are terminal and reasoned; digest identity never changes; protective-dependency rows, not ref counting, gate eligibility | — |
| Information preservation | History growth limits could erase replay/provenance | Metadata never reclaimed; required bytes stay pinned; reclaimed bodies leave `unavailable` references with reasons; counters and ledgers untouchable | Post-v0.1 may add archive compaction for very old stores |
| Resource bounds | Unbounded growth, or reclamation that starves lifecycle work | Per-category age+byte caps, bounded passes, deterministic oldest-first order, pressure surfaced instead of silently deleting protected data | Pressure Attention tuning (repeat interval) is an implementation detail |

## Implementation-plan mapping and non-claims

- **U2**: `body_catalog` + CAS layout, retention config keys and validation, sweep pass tooling, `retention report`/`cleanup` commands, pressure accounting, and the RT suite.
- **U1**: protective-dependency row creation/discharge sites inside lifecycle transactions; pin-refusal checks.
- **U5**: sweep invocation from the CLI run loop and Attention surfacing for `protected_over_limit` and delete errors.

This document claims no runtime implementation, no executed cleanup, no live filesystem evidence, and no CI result. Every numbered behavior above is a target obligation verified by future contract tests. Deferred items: archive compaction tooling, cross-store body deduplication, per-Work retention overrides, and any automatic removal of foreign bodies.
