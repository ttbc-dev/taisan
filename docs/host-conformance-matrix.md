# Host conformance matrix

The U9.1 contract matrix for repository-host adapters. Every row states
the host-neutral required semantics from `docs/repository-host-contract.md`
and names the scenario or test that proves it. The shared suite
(`crates/workloop-github/tests/host_conformance.rs`) runs each scenario
against every fixture whose declared capability set covers the row's
requirements; a fixture that cannot cover a row is skipped for that row,
never substituted. Coverage claims are fixture-level modeled behavior —
they do not claim production-host support (see the contract's release
scope).

## Parameterization

The suite parameterizes over two axes:

- **Fixture implementation.** `FixtureGithub` (full baseline vocabulary)
  and `SparseHost` (baseline reads plus `push_ref` and
  `change_request_create` only) are independent implementations with
  different remote models — flat maps vs. a repository-scoped journal.
  A row is covered for a fixture only by running the unchanged scenario
  against it.
- **Declared capability set.** Each scenario lists the capabilities it
  requires. A fixture runs the scenario only when its declared set
  covers all of them, matching the contract rule that an optional
  capability may be absent but, once declared and required, may not be
  skipped or weakened.

## Baseline operations

| Row | Required semantics | Scenario / coverage |
| --- | --- | --- |
| HC-BASE-01 | `push_ref` applies only from a durable recorded intent and its receipt carries the normalized `RefPublished` fact (observed = produced, original repository binding). | `baseline_effects_apply` in the shared suite; `publish_writes_only_from_a_recorded_intent_and_reports_applied` (fixture adapter). |
| HC-BASE-02 | `open_change_request` produces an `applied` receipt with `ChangeOpened` facts: scoped change-request identity, head revision, source/target branches — not a bare provider URL. | `baseline_effects_apply` (shared suite, both fixtures). |
| HC-BASE-03 | Head-bound writes (`update_change_request`) re-read the head immediately before writing; a moved head aborts with a `failed` receipt, surfaces the fresh head, and lands no write. | `expected_head_mismatch_aborts_write` (fixtures declaring `change_request_update`); `head_mismatch_aborts_the_write_and_surfaces_the_fresh_head`. |
| HC-BASE-04 | `post_comment` binds to the exact recorded head and marker; a mismatched head aborts with a `failed` receipt and lands no write. | `expected_head_mismatch_aborts_write` exercises both head-bound write kinds in the same scenario (fixtures declaring `comment_create`). |
| HC-PART-01 | An `applied` effect alongside an unresolved one keeps separate receipts — no combined-success claim; an `unknown` intent is never retried blindly and stays reconcilable by action ID. | `partial_receipts_stay_separate` (both fixtures); `unknown_result_stays_outstanding_until_reconciled_by_action_id`. |

## Support vs. permission

| Row | Required semantics | Scenario / coverage |
| --- | --- | --- |
| HC-PERM-01 | Denied mutation authorization leaves intents `pending` — it is not a capability verdict and must not produce a write or a fabricated failure. | `denied_permission_is_not_unsupported` (both fixtures); `host_write_denied_leaves_the_outbox_pending`, `execution_denial_is_not_overridden_by_installation_permissions`. |
| HC-CAP-01 | A required capability that is missing or undeclared rejects admission explicitly (`unsupported_capability`) before Worker start or remote writes — never deferred silently. | `unsupported_capability_stays_explicit` (both fixtures). |
| HC-CAP-02 | Unsupported operations stay explicit at the effect boundary: a fixture that does not implement an operation reports a `failed` receipt naming it — never `applied`. | `sparse_host_unsupported_operations_stay_explicit` (`SparseHost` refuses `update_change_request` and `post_comment`). |
| HC-CAP-03 | `approve_change` / `mark_ready` remain explicit-unsupported: they are not in the `HostAction` vocabulary, so no fixture can emit an `applied` receipt for them, and admission of a profile requiring them cannot weaken them into a comment. | Uncovered by scenario — no conforming operation exists in the current vocabulary. The admission gate (HC-CAP-01) is the enforcement point; adding these operations is tracked by the optional-capability issues. |

## Identity, snapshots, and binding

| Row | Required semantics | Scenario / coverage |
| --- | --- | --- |
| HC-SCOPE-01 | Identical branch names and display numbers on different repositories never collide: each receipt keeps its own recorded repository binding. | `scoped_identities_do_not_collide` (both fixtures); `open_receipt_for_a_different_target_branch_is_unknown`. |
| HC-SNAP-01 | Completion reads immutable revision snapshots: checks or review evidence for a superseded head cannot complete the Work; only evidence for the current head counts. | `stale_head_evidence_cannot_complete` in the shared suite (both fixtures); `head_bound_evidence_drives_success_only_for_the_current_head`, `superseded_check_evidence_does_not_complete_the_work`, `delayed_check_success_cannot_complete_the_work`, `delayed_review_on_a_superseded_target_does_not_complete_the_work` (fixture adapter). |
| HC-SWAP-01 | A configuration change after an intent is recorded cannot retarget it: the intent and its receipt keep the original host/repository binding, and reconciliation continues under it. | `config_swap_keeps_recorded_binding` (both fixtures); `terminal_execution_still_authorizes_outstanding_effect_reconciliation`. |

## Delivery and lifecycle

| Row | Required semantics | Scenario / coverage |
| --- | --- | --- |
| HC-DELIV-01 | Duplicate delivery of the same fact is inert: no second write, no second intent. | `repeated_artifact_evidence_does_not_queue_a_second_publish` (fixture adapter). |
| HC-LIFE-01 | Cancellation defers terminal settlement until outstanding outbox effects reconcile — nothing is settled over an unresolved write. | `cancel_defers_terminal_settlement_until_the_outbox_reconciles` (fixture adapter). |
| HC-LIFE-02 | Budget failure likewise waits for the outstanding outbox effect. | `budget_failure_waits_for_the_outstanding_outbox_effect` (fixture adapter). |
| HC-READ-01 | Issue content/comments/thread reads. | Partial: `issue_read` is enforced at admission (HC-CAP-01). Typed read-model operations for issue content, comments, and threads are not yet in the contract vocabulary — uncovered, tracked by the read-model issues. |
| HC-LIFE-03 | Host-side rename, actor changes, and restart reconciliation. | Partial: restart durability is exercised by the durable-store tests; rename and mid-Work actor changes are uncovered — tracked by later conformance issues. |

## What this slice does not claim

- Fixture passes are modeled contract evidence only. The real GitHub
  adapter and the live spike are owned by their own issues.
- `SparseHost` proves the suite is parameterized, not that a second
  production host exists. Adapter exchangeability claims require the
  unchanged suite against independently implemented adapters plus the
  narrow live spike described in the contract.
- Rows marked uncovered are deliberate gaps recorded here, not silent
  skips inside the suite: the runner reports covered and skipped rows
  per fixture.
