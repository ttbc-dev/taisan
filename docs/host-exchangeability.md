# Host exchangeability evidence (U9.5)

Comparison of the independently implemented host fixtures — and the
live-spike column — against the unchanged U9.1 conformance matrix
(`docs/host-conformance-matrix.md`, scenarios in
`crates/workloop-github/tests/host_conformance.rs`).

**Claims boundary.** Every cell below is a *fixture* result: modeled
contract behavior produced by local tests. It supports the claim that
the shared suite is host-neutral and that the contract expresses the
fixtures' differences. It does **not** claim production-host support.
Live GitLab evidence requires the separately authorized spike tracked
by U9.4 (#80); that spike has not run, so every live row is marked
incomplete. This proof is independent of GitHub S1 — S1 is measured
against the real GitHub adapter, not these fixtures.

## Fixture differences the contract expresses

| Axis | `FixtureGithub` | `GitlabFixture` | `SparseHost` |
| --- | --- | --- | --- |
| Host IDs | Globally unique change-request IDs (`cr-N`); display numbers per repository | Per-project IID (`!N`) as both identity and display number — the same `!1` on two repositories names different change requests; only the scoped pair is stable | Globally unique change-request IDs; display numbers per repository |
| Remote head model | Head snapshotted at open; moves only by direct remote edit | Head derived from the source branch — a push to the branch moves the change-request head (`push_to_an_open_merge_requests_source_branch_surfaces_the_new_head`) | Head snapshotted at open |
| Declared capabilities | Full baseline (10) | Baseline minus `issue_read` (9) — a source-bound Work is rejected `unsupported_capability` | Reads + `push_ref` + `change_request_create` (7) |
| Effect behavior | Absent source branch yields a synthesized head | Absent source branch is a `failed` receipt (`gitlab_fixture_open_without_source_branch_is_a_failed_receipt`) | Head-bound writes are `failed(unsupported)` receipts |
| Host binding | Optional per-instance binding (`for_host`) | Always bound to one host instance; drains only its own intents | Single-instance model |

## Coverage matrix

Rows run wherever the fixture's declared capability set covers the
scenario's requirement list; a fixture that cannot cover a row is
skipped for that row, never substituted. Suite runner:
`run_suite` in `host_conformance.rs`.

| Matrix row | FixtureGithub | GitlabFixture | SparseHost | Live GitLab spike |
| --- | --- | --- | --- | --- |
| HC-BASE-01/02 baseline effects | covered | covered | covered | incomplete — spike not run |
| HC-BASE-03/04 head-bound writes | covered — update applies against the recorded head, then aborts on a moved head | covered — same applied-then-aborted update | skipped (no `change_request_update`/`comment_create`) | incomplete — spike not run |
| HC-PART-01 partial receipts | covered | covered | covered | incomplete — spike not run |
| HC-SNAP-01 stale-head evidence | covered | covered | covered | incomplete — spike not run |
| HC-PERM-01 denied vs unsupported | covered | covered | covered | incomplete — spike not run |
| HC-SCOPE-01 scoped identities | covered | covered — plus `gitlab_fixture_reuses_iid_across_projects_without_aliasing` (real `!1` reuse across projects) | covered | incomplete — spike not run |
| HC-SWAP-01 binding survives config | covered | covered | covered | incomplete — spike not run |
| HC-CAP-01 admission gate | covered | covered — plus `gitlab_fixture_undeclared_issue_read_rejects_source_bound_work` | covered | incomplete — spike not run |
| HC-CAP-02 explicit unsupported | not exercised — implements every declared operation | not exercised — implements every declared operation | covered (`failed` receipts) | incomplete — spike not run |
| HC-CAP-03 approval/ready explicit | covered — `approval_and_ready_stay_explicitly_unsupported` | covered — same fixture-independent, type-level check | covered — same fixture-independent, type-level check | incomplete — spike not run |
| HC-DELIV-01 duplicate delivery | covered (fixture adapter suite) | not exercised | not exercised — the adapter suite instantiates `FixtureGithub` only | incomplete — spike not run |
| HC-LIFE-01/02 deferred settlement | covered (fixture adapter suite) | not exercised | not exercised — the adapter suite instantiates `FixtureGithub` only | incomplete — spike not run |
| HC-READ-01 typed reads | incomplete — read-model ops not in vocabulary; fixture side (spike not run) | incomplete — same | incomplete — same | incomplete — spike not run |
| HC-LIFE-03 rename/actor/restart | partial — restart via durable-store tests; rename and actor change uncovered | partial — same | partial — same | incomplete — spike not run |

HC-CAP-02 measures a fixture's own explicit-unsupported behavior; a
fixture implementing every declared operation has nothing to exercise
there, so only `SparseHost` covers the row. HC-CAP-03 is a single
fixture-independent, type-level check — `approve`/`mark_ready` exist as
capability names but in no `HostAction` variant or deserialization
path, so no applied receipt is constructible — one test covers all
fixtures and any future host implementation.

Cross-fixture selection and non-migration under a configuration swap
are exercised separately by
`host_selection.rs::config_swap_selects_host_for_new_work_and_keeps_existing_bindings`:
Work accepted before the swap keeps its binding, its unresolved effect
reconciles under the original (host, repository) pair, and intents
recorded for one host instance are never drained by an adapter bound
to another.

## Exchangeability claim

For the operations both full-baseline fixtures implement (`push_ref`,
`open_change_request`, `update_change_request`, `post_comment`, and the
read/check/review observations the suite drives), the unchanged shared
suite passes on `FixtureGithub` and `GitlabFixture` with no
Core/Command change — fixture-level exchangeability for those
operations only, with valid scenario evidence cited above. `SparseHost`
proves the runner's declared-set skip path rather than full coverage.

### Claim binding (fixture evidence only — spike not run)

Every operation named in the claim is bound below to the matrix rows
that back it on the fixture side. The live-spike side of every one of
those rows is `incomplete — spike not run`; nothing in this table
constitutes, or may be read as, live evidence.

| Claimed operation | Backing rows (fixture side) | Live gate |
| --- | --- | --- |
| `push_ref` | HC-BASE-01/02 (`baseline_effects_apply`), HC-PART-01 (`partial_receipts_stay_separate`), HC-SWAP-01 (`config_swap_keeps_recorded_binding`) | not run — incomplete |
| `open_change_request` | HC-BASE-02 (`baseline_effects_apply`), HC-PART-01 (`partial_receipts_stay_separate`) | not run — incomplete |
| `update_change_request` | HC-BASE-03 (`expected_head_mismatch_aborts_write`) | not run — incomplete |
| `post_comment` | HC-BASE-04 (`expected_head_mismatch_aborts_write`) | not run — incomplete |
| read/check/review observations | HC-SNAP-01 (`stale_head_evidence_cannot_complete`), HC-CAP-01 (`unsupported_capability_stays_explicit`) | not run — incomplete |

## Remaining limitations

- `HC-CAP-02` is exercised only by `SparseHost` — both
  full-baseline fixtures implement every declared operation, so there is
  nothing for them to refuse (by design, not an omission), but that
  means the row carries no `FixtureGithub`/`GitlabFixture` evidence.
- `HC-DELIV-01`, `HC-LIFE-01` and `HC-LIFE-02` are exercised only by
  `FixtureGithub` (the fixture adapter suite instantiates it only);
  `GitlabFixture` and `SparseHost` have no delivery-idempotency or
  deferred-settlement evidence.
- `HC-READ-01` — no fixture evidence at all (read-model ops are not in
  the vocabulary).
- `HC-LIFE-03` is partial on every side — restart durability is
  exercised via durable-store tests; rename and mid-Work actor change
  are uncovered.
- No live claim: U9.4 (#80) has not run; approval/readiness semantics,
  actor eligibility, denied-permission injection, and restart
  reconciliation on a real host remain unproven — every live cell in
  the coverage matrix is `incomplete — spike not run`, with this reason.
- `approve_change` / `mark_ready` / thread operations are absent from
  the `HostAction` vocabulary — structurally unsupported, not yet
  implementable (HC-CAP-03, covered by a fixture-independent type-level
  check on all fixtures).
- Typed read-model operations for issue content, comments, and threads
  are not in the contract vocabulary (HC-READ-01).
- Rename, mid-Work actor change, and cross-instance delivery-ID
  deduplication are unexercised (HC-LIFE-03).
- `GitlabFixture` models IID scoping and branch-following heads; a real
  GitLab adapter may still diverge in transport, pagination, diff
  positions, and approval endpoints.
