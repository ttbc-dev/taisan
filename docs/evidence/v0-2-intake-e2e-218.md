# v0.2 intake end-to-end acceptance matrix

Issue: #218. This record maps every v0.2 delivery issue's acceptance
criteria and the milestone completion conditions to the named
automated scenarios that cover them. It is **fixture and contract
evidence**: every scenario ran against scripted adapter executables,
file/in-memory SQLite stores, and spawned fixture processes. No live
source, real Worker, or owner-run installation was exercised; the
owner-run live acceptance criteria remain pending under #205 and are
never claimed here.

## Tested material and gates

The tested runtime baseline is the `issue-218-intake-e2e` branch; the
commit containing this record identifies the reviewed revision. All
scenarios ran on macOS against the workspace debug build.

| Gate | Result |
| --- | --- |
| `cargo fmt --check` | Passed |
| `cargo build --workspace` | Passed |
| `cargo test --workspace` | Passed — all targets, 0 failures (the fixture-selector/fixture-worker contract tests spawn `target/debug/` binaries, so the build gate runs first) |
| `cargo clippy --workspace --all-targets -- -D warnings` | Passed |

## Requirement-to-test matrix

Each row names the issue, the acceptance-criteria cluster it covers,
and the tests that exercise it. "Suite" entries name whole contract
files whose every test maps to the cluster.

### #200 — stateless external Selector protocol

| Criteria cluster | Covering tests |
| --- | --- |
| Wire schema, bounds, whole-response rejection, typed failures | `workloop-selector/tests/protocol_contract.rs`, `selector_contract.rs`, `adapter_contract.rs` (21 shared scenarios incl. malformed candidate, wrong version, duplicate identity, bound violations, oversized response rejected identically on both adapters) |
| Ordinary rejection vs contract violation | `run_intake` suite: `ordinary_rejection_is_recorded_and_later_candidates_continue`, `selector_contract_violation_fails_the_whole_response` |

### #201 — bounded external Selector processes

| Criteria cluster | Covering tests |
| --- | --- |
| Distinct structured failures; nothing reaches admission on failure | `selector_nonzero_exit_fails_the_run_without_candidates`, `selector_malformed_output_is_a_bounded_failure`, `selector_timeout_is_bounded`, `missing_adapter_binary_is_a_launch_failure`, `selector_declared_failure_maps_to_a_bounded_code` |
| argv/no-shell, env allowlist, no inherited secrets | `exchange_passes_argv_without_shell`, `exchange_environment_is_the_allowlist_only`, `selector_environment_carries_no_inherited_secrets` (`workloop-exec/tests/bounded_exchange.rs`) |
| Timeout kills process group; uncertain descendants hold ownership | `exchange_deadline_terminates_the_process_group`, `exchange_kills_same_group_descendants_holding_pipes`, `exchange_reports_uncertain_descendants_as_unknown` |
| Bounded drains, no deadlock/overflow | `exchange_stdout_overflow_terminates`, `exchange_stderr_overflow_terminates`, `exchange_stderr_is_bounded_and_counted_not_retained`, `exchange_writes_large_stdin_without_deadlock`, `exchange_rejects_oversized_stdin_before_spawn` |
| Nonblocking vs store transactions | `a_blocked_exchange_leaves_manual_submit_free`, `an_in_flight_intake_leaves_submit_run_and_queries_free` (new, this issue) |

### #202 — reference Selector source collection

| Criteria cluster | Covering tests |
| --- | --- |
| Bounded collection, typed failures, fail-closed facts | `workloop-selector/tests/collect_contract.rs` (13 tests: complete-scan-only facts, deadline between calls and after final page, typed read failures, malformed-fact repository failure, scoped identity, determinism, cursor exhaustion, call budget, bounded diagnostics, oversized page) |
| Allowlisted repositories, adapter-owned auth | `reference_selector.rs` (gh-backend scenarios), `adapter_contract.rs` exclusion/bounds scenarios |

### #203 — ordered one-shot admission coordinator

| Criteria cluster | Covering tests |
| --- | --- |
| Preflight sizing, exclusions, single call, ordered admission | `run_intake_admits_candidates_in_order_through_submit`, `run_intake_sizes_max_candidates_to_free_positions`, `run_intake_excludes_owned_issues_and_scopes_repositories`, `run_intake_with_empty_response_completes_without_candidates`, `run_intake_skips_without_launching_when_queue_is_full`, `run_intake_skips_without_launching_when_repositories_break_the_wire_bound` |
| Whole-response validation before any Work | `selector_contract_violation_fails_the_whole_response` + `adapter_contract.rs` malformed/duplicate/bound scenarios on both adapters |
| Ordinary rejection continues; grouped atomicity | `ordinary_rejection_is_recorded_and_later_candidates_continue`, `a_grouped_candidate_admits_as_one_work`, `a_grouped_submit_rejects_atomically_on_member_conflict` |
| Capacity deferral without second queue | `mid_batch_capacity_loss_defers_the_unattempted_remainder`, `a_manual_submit_taking_the_last_position_defers_the_intake_candidate` (new) |
| Replay/finished-source rules | `rejected_decisions_replay_on_a_second_run`, `a_finished_source_is_readmitted_under_a_new_proposal_key`, `the_same_proposal_key_replays_the_recorded_decision_for_a_finished_source` (new) |
| Partial commits stop mutation, retain identities | `a_submit_store_fault_halts_and_reconciles_the_candidate`, `a_batch_store_fault_aborts_the_run_instead_of_wedging`, `an_admission_store_fault_aborts_and_reconciles_candidates` |

### #204 — nonblocking intake in `taisan run`

| Criteria cluster | Covering tests |
| --- | --- |
| 60-second settings cadence inside the driver | `maybe_tick_*` unit tests in `workloop-cli/src/intake.rs`, `the_settings_tick_is_sixty_seconds` |
| Supervision/dispatch continues while selector waits | `run_once_drives_a_scheduled_startup_intake_without_blocking`, `an_in_flight_intake_leaves_submit_run_and_queries_free` (new) |
| Single-flight across scheduled/manual/restart | `concurrent_intake_invocations_are_single_flight` (new), `intake_is_refused_while_the_intake_lease_is_held`, `intake_is_refused_while_a_run_is_active`, `a_busy_lease_refuses_the_invocation`, `an_active_run_refuses_a_second_invocation` |
| Graceful shutdown / `--once` / `--work` contract | `run_driver.rs` suite (`run_once_emits_one_eight_phase_tick_report_and_persists_it`, `a_sigint_finishes_the_phase_boundary_and_exits_gracefully`, `run_work_diagnostic_scope_never_evaluates_intake_scheduling`) |
| Selector failure waits for next slot, never aborts loop | scheduler contract `a_due_slot_observed_while_active_is_skipped_not_queued`; failure-classification tests above |

### #206 — intake identity/persistence/recovery specification

| Criteria cluster | Covering tests |
| --- | --- |
| Record table, transitions, crash-point matrix | `docs/intake-identity-storage.md` normative spec; `contract_intake.rs` run/candidate transition guards (`run_state_transition_guards`, `candidate_identity_immutable_status_guarded`, `candidate_decision_links_follow_the_status_contract`, `run_record_field_invariants_are_enforced`) |
| Replay identity across ticks/restart | `rejected_decisions_replay_on_a_second_run`, `the_same_proposal_key_replays_the_recorded_decision_for_a_finished_source` (new), `replay_returns_the_recorded_decision_under_a_full_queue` |
| New-key finished source / owner exclusion | `a_finished_source_is_readmitted_under_a_new_proposal_key`, `running_and_paused_owners_still_exclude_their_sources` (new), `preflight_reports_positions_and_scoped_owners_from_one_snapshot` |
| Deferred ≠ rejected; retry on later tick | `mid_batch_capacity_loss_defers_the_unattempted_remainder`, `deferred_candidates_hold_no_decision_links`, `a_capacity_rejection_is_redecided_when_capacity_returns` |
| Unknown outcomes reconcile by command identity | `recovery_reconciles_candidates_against_the_ledger`, `the_sweep_finishes_candidates_under_an_interrupted_run` |

### #207 — durable intake records and ownership

| Criteria cluster | Covering tests |
| --- | --- |
| Round-trip of runs/candidates/links | `intake_records_survive_reopen_and_compete`, `contract_intake.rs` link/FK guard tests, `intake_runs`/`intake-detail` projection tests in `intake_cli.rs` |
| Atomic single owner; busy without second launch | `intake_lease_single_flight`, `intake_lease_is_independent_of_the_driver_lease`, `concurrent_intake_invocations_are_single_flight` (new) |
| Dead-owner reclaim gated on process evidence | `a_dead_owner_open_run_interrupts_without_a_validated_batch`, `a_live_owner_retains_the_run_untouched`, `an_uninspectable_owner_retains_the_run_untouched` |
| Upgrade/format safety | `v7_store_upgrades_additively`, `v8_stamp_without_intake_tables_is_refused`, `newer_format_is_refused`, `contract_intake_settings.rs` upgrade tests |

### #208 — interrupted-intake recovery

| Criteria cluster | Covering tests |
| --- | --- |
| Crash before/after persistence and commit; at most one Work per identity | `recovery_contract.rs` full suite (10 tests) incl. `a_dead_owner_open_run_interrupts_without_a_validated_batch`, `recovery_reconciles_candidates_against_the_ledger`, `an_admitting_run_with_all_candidates_decided_closes_completed`, `recovery_is_idempotent_and_finishes_interrupted_run_leftovers`, `concurrent_recovery_passes_do_not_report_a_store_fault` |
| No invented candidates; fresh selection only after safe ownership | `a_dead_owner_open_run_interrupts_without_a_validated_batch`, `selector_nonzero_exit_fails_the_run_without_candidates` |
| Provenance immutable across swap/reload/restart | `skipped`/terminal run guards in `contract_intake.rs`; `swapping_only_the_profile_selector_selects_the_fixture_adapter` |
| Real SQLite reopen and process boundaries | `intake_records_survive_reopen_and_compete`, recovery tests use spawned-then-reaped pids for owner liveness |

### #209 — one-shot intake CLI

| Criteria cluster | Covering tests |
| --- | --- |
| Works with automation disabled; no implicit enable | `intake_runs_with_automatic_intake_disabled`, `intake_profile_resolution_never_reads_the_json_install_config_flag` |
| Racing invocations → at most one adapter process | `concurrent_intake_invocations_are_single_flight` (new) |
| Envelope/exit taxonomy | `intake_without_a_profile_is_a_bounded_config_refusal`, `intake_with_an_invalid_profile_is_a_bounded_refusal`, `intake_runs_reports_the_enabled_profile_state` |
| Full queue records skip without launch | `run_intake_skips_without_launching_when_queue_is_full` + `intake-runs` projection assertions |

### #210 — intake history/status queries

| Criteria cluster | Covering tests |
| --- | --- |
| Run/candidate projections preserve order and identities | `intake_runs_the_coordinator_and_projects_the_run`, `intake_detail_for_an_unknown_run_is_a_completed_not_found`, `swapping_only_the_profile_selector_selects_the_fixture_adapter` (run list newest-first, per-adapter selector ids) |
| Status reports configured/active/last outcomes | `intake_runs_reports_the_enabled_profile_state`, `intake_runs_reports_unconfigured_without_a_profile`, in-flight projection in `an_in_flight_intake_leaves_submit_run_and_queries_free` (new) |

### #211 — scheduler/reload/CLI specification

Specification issue; normative content lives in `docs/execution-loop.md`,
`docs/installation-config.md`, and `docs/intake-identity-storage.md`.
Behavioral coverage is enumerated under #215/#216.

### #212 — intake settings loading

| Criteria cluster | Covering tests |
| --- | --- |
| Defaults for pre-v0.2 profiles; validation of every invalid class | `workloop-config/tests/profile_contract.rs`, `preflight_contract.rs`; `intake_with_an_invalid_profile_is_a_bounded_refusal` |
| Coherent revision; invalid reload reported independently | `settings_record_writes_are_revision_guarded`, `invalid_profile_keeps_the_last_valid_record` (driver unit test) |

### #213 — shared pending-queue limit

| Criteria cluster | Covering tests |
| --- | --- |
| Atomic limit across manual/selector submits; grouped = one position | `concurrent_submits_never_exceed_the_pending_limit`, `a_grouped_submission_consumes_exactly_one_position`, `selector_origin_submits_share_the_same_capacity_check`, `the_active_limit_is_what_submit_decides_against` |
| Queue-full structured result; lowered limit non-destructive | `a_capacity_rejection_leaves_no_partial_rows`, `lowering_the_limit_refuses_new_submits_without_cancelling_work`, `only_queued_executions_hold_a_pending_position` |
| Replay under full queue; retry/resume bypass | `replay_returns_the_recorded_decision_under_a_full_queue`, `retry_bypasses_the_pending_limit`, `resume_bypasses_the_pending_limit` |
| Read-only bounded preflight | `preflight_reports_positions_and_scoped_owners_from_one_snapshot`, `preflight_is_read_only_and_non_reserving`, `an_over_bound_preflight_scope_fails_explicitly` |
| Last-position race | `a_manual_submit_taking_the_last_position_defers_the_intake_candidate` (new) |

### #214 — packaged reference Selector

| Criteria cluster | Covering tests |
| --- | --- |
| Label-scoped open issues, exclusions honored, deterministic ranking | `adapter_contract.rs` reference column (rank order, exclusions, bounds, truncation, cross-repository identity, proposal-key stability) |
| No cursor/ledger; clean-install operation | `reference_selector.rs` file-backend and gh-backend scenarios |

### #215 — deterministic fixed-interval scheduling

| Criteria cluster | Covering tests |
| --- | --- |
| Startup/re-enable immediate; disabled never fires | `startup_is_immediately_due_and_anchors_the_grid`, `enable_after_disabled_is_a_reenable_not_a_startup`, `disabled_intake_never_fires` |
| Interval/adapter change semantics; active-slot skip; no catch-up | `the_interval_grid_fires_once_per_due_point`, `an_interval_change_reanchors_without_firing`, `adapter_and_policy_changes_preserve_the_due_time`, `a_due_slot_observed_while_active_is_skipped_not_queued`, `startup_and_reenable_dues_observed_while_active_are_skipped`, `missed_periods_collapse_to_the_next_future_grid_point`, `disable_clears_the_grid_and_reenable_restarts_it` — all clock-controlled, no real-time sleeps |

### #216 — configuration reload on scheduler ticks

| Criteria cluster | Covering tests |
| --- | --- |
| Atomic profile replacement at next tick; disable/re-enable/interval/adapter semantics | driver unit tests in `workloop-cli/src/intake.rs` (`settings_tick_persists_revision_and_limit`, `unchanged_tick_does_not_rewrite_the_record`, `edited_limit_applies_at_the_next_tick`, `manual_submit_observes_the_reloaded_queue_limit`, `absent_profile_writes_nothing`) + scheduler contract rows above |
| In-flight invocation keeps captured settings; invalid reload suppresses one tick and recovers | `invalid_profile_keeps_the_last_valid_record`, `an_invalid_tick_suppresses_the_decision_and_preserves_state` |
| Accepted Work bindings untouched by reload | settings contract tests + `swapping_only_the_profile_selector_selects_the_fixture_adapter` (existing Work unchanged across adapter swap) |

### #217 — independent adapter interchange

| Criteria cluster | Covering tests |
| --- | --- |
| Independent implementation, shared suite, config-only swap, grouped atomicity, fixture label | `adapter_interchange.rs` (3 tests), `adapter_contract.rs` + `fixture_selector.rs` + `reference_selector.rs`, `docs/evidence/selector-adapter-interchange-217.md` |

### #218 — this record

New cross-component scenarios added by this issue:

| Scenario (test) | File | Proves |
| --- | --- | --- |
| `a_manual_submit_taking_the_last_position_defers_the_intake_candidate` | `workloop-intake/tests/intake_contract.rs` | Manual/scheduled race for the last queue position: gated adapter holds the exchange open, a manual submit takes the slot preflight sized for, the candidate lands `capacity_deferred` with no Work and no decision links |
| `a_finished_source_is_readmitted_under_a_new_proposal_key` | same | Finished source (terminal Execution + released claims) re-proposed under a new key admits a fresh Work — no finished-source gate |
| `the_same_proposal_key_replays_the_recorded_decision_for_a_finished_source` | same | Same proposal identity against a finished source replays the recorded decision (`replayed`, original Work link), never a duplicate Work |
| `running_and_paused_owners_still_exclude_their_sources` | same | Non-`queued` owners (`running`, `paused`) appear in `excluded_sources` — the wire-level overlap rule |
| `a_blocked_exchange_leaves_manual_submit_free` | same | A selector blocked mid-exchange holds only the intake lease; ordinary submit proceeds and the run completes after release |
| `concurrent_intake_invocations_are_single_flight` | `workloop-cli/tests/intake_cli.rs` | Two real `taisan intake` processes: the lease refuses the loser `intake_busy` before any run record; exactly one completed run exists |
| `an_in_flight_intake_leaves_submit_run_and_queries_free` | same | While a slow selector decides: `submit` accepted, `run --once` completes its pass (scheduler skips the due slot under the active run), `intake-runs` projects the live run, and the invocation completes normally |

### #219 — installation/operation/owner-run documentation

Documentation delivered in `docs/v0.2.md`,
`docs/v0.2-owner-run-checklist.md`,
`docs/v0.2-evidence-template.md`, and the sanitized
`docs/examples/v0.2/config.toml`. The static checks in
`crates/workloop-cli/tests/v02_docs.rs` are **automated
fixture/contract evidence**: they parse the example and assert that the
walkthrough, checklist, and template carry the required steps, default
bounds, labels, and redaction/unexercised fields. They create no Issue,
enable no automation, and launch no Worker. The owner-run rows remain
pending under #205 and are not claimed here.

### #205 — owner-run public acceptance and milestone completion

**Pending — owner-run only.** No fixture or contract result substitutes
for it. The owner-run runbook is posted on the issue; live evidence
must record tested revisions, sanitized configuration, actual
profiles, command lines, observed timestamps/results, and
durable run/proposal/command/Work links with credentials redacted.

### #235 — deferred-proposal reporting

| Criteria cluster | Covering tests |
| --- | --- |
| `capacity_deferred` carries replay key, no command id; null command-result; envelope omission | `propose.rs` deferred-item assertions (`capacity_deferred` item shape at line ~770) plus the store/CLI regression tests landed with the fix |

## Milestone completion conditions

| Condition | Status |
| --- | --- |
| Public versioned contract and configuration/identity/recovery behavior specified with examples and mapped tests | Met — `docs/selector-protocol.md`, `docs/intake-identity-storage.md`, `docs/execution-loop.md`, `docs/installation-config.md`; this matrix |
| Reference and independent public test adapters pass the same contract suite and swap by configuration | Met — fixture evidence, `selector-adapter-interchange-217.md` |
| Concurrent manual/scheduled admission, restart, whole-response validation, capacity deferral and hot reload pass automated acceptance | Met — fixture evidence, this matrix |
| One-shot intake works while automatic intake is disabled; failures do not stop existing Work or manual submit | Met — fixture evidence (`intake_runs_with_automatic_intake_disabled`, `an_in_flight_intake_leaves_submit_run_and_queries_free`, failure-classification suite) |
| Owner records real public-reference scheduled intake and existing Work processing using the published checklist; fixture and live evidence remain distinct | **Pending — owner-run (#205)** |
| Existing fmt/build/test/clippy gates pass against the tested implementation revision | Met — gate table above |

## Residual risks and observed boundaries

- **Store bootstrap race (observed, pre-existing, out of scope —
  tracked as #276).**
  While building `concurrent_intake_invocations_are_single_flight`,
  two `taisan` processes opening a *fresh* store file within the same
  instant both passed the `user_version == 0` / empty-tables gate and
  raced the bootstrap transaction; the loser's `INSERT INTO
  store_meta ('store_id', …)` failed `UNIQUE constraint failed:
  store_meta.key`, surfacing as a `store` error envelope. The window
  is narrow (simultaneous first-open of a not-yet-created store) and
  unrelated to intake semantics — the intake lease itself serializes
  correctly once the store exists — but it is a real cross-process
  edge tracked as #276.
- **Timing-sensitive exchange test.** `exchange_deadline_terminates_
  the_process_group` (`workloop-exec/tests/bounded_exchange.rs`)
  flaked once under full-workspace parallel load (its 1200 ms deadline
  fired before the fixture script's first write under CPU pressure)
  and passed in isolation and on re-run; unrelated to this change.
- **Live-source and Worker-execution proof.** Every scenario here uses
  scripted adapters and fixture Workers. Real `gh`-backed collection,
  real Worker supervision during intake, and owner-run acceptance are
  explicitly out of fixture scope and tracked by #219/#205.
