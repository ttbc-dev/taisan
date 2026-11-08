# Design-closure requirement mappings

Normative mapping of the four requirements assigned to #56 by the extraction inventory onto [design closure](design-closure.md) clauses and the target documents they reference. Each `## R###` section names the #56-owned obligation — the part the acceptance plan and closure review must satisfy — not the requirement's entire source mechanism.

## R48

Path-filtered PR verification gate with supersede cancellation and bounded checks — the mechanism (condition-gated CI trigger) is host-side workflow configuration, outside taisan's control surface. The retained obligations map to existing clauses: only declared named checks participate in assessment, a superseding head invalidates stale evidence rather than cancel-then-inherit, and check evidence is bounded and three-valued.

Clause: [design closure](design-closure.md#owner-decisions) (R48 disposition); [evidence gates](evidence-gates.md) named requirement identities; [execution loop](execution-loop.md#failure-scenario-matrix) F9 stale head. Suite: LP-17.

Planned proof for I56-AC2: a head change mid-run — the old head's check state cannot satisfy the new head; only a fresh named-check result under the current head counts.

## R70

Projection never fabricates unknowns — mechanism (the source's projection service) omitted. Retained: the CLI Query never converts an unreadable or intermediate record into a settled-looking answer; non-terminal states stay visible as such.

Clause: [execution loop](execution-loop.md#envelope) (three-valued queries, `unknown` status); [evidence classes](execution-loop.md#evidence-classes). Suite: LP-07.

Planned proof for I56-AC2: corrupt a record and query — the result is `unknown`/`unavailable` with reason, never a fabricated settled view.

## R102

Attention next-actor axis derives from closed stop/verdict vocabulary and fails closed to `unknown` — retained onto Query-only Attention: every item carries version, reason, and required next action from the closed vocabulary; indeterminate derivation yields `unknown`, never a guessed actor.

Clause: [execution loop](execution-loop.md#owner-decisions) Attention delivery; phase 8 [Attention derivation](execution-loop.md#tick-phases-in-order). Suite: LP-22.

Planned proof for I56-AC2: withhold the derivation basis — the item reports `unknown` next actor, never an inferred one.

## R214

Presented facts only, with visibly distinct unknown/unobserved/known-absent — console mechanism omitted; retained onto the CLI Query: counts never fabricate zeros over absent data, and the three absence classes stay distinct.

Clause: [execution loop](execution-loop.md#envelope) queries; [command surface](execution-loop.md#command-surface). Suite: LP-07.

Planned proof for I56-AC2: absent data renders `unknown`/null-with-reason — never a synthesized `0` or `none`.
