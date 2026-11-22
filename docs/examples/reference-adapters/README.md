# Example: public reference Selector + Worker, composed

A complete, standalone configuration binding the two public reference
adapters — `public-issue-selector` for intake and
`taisan-worker-claude` (driving the Claude Code CLI) for every Worker
role — through the advertised workflow. Everything here is
provider-neutral placeholder configuration; nothing in this directory
proves live behavior.

## Evidence boundary

| What this example establishes | What it does not establish |
| --- | --- |
| Both adapters are bound by configuration alone (`argv` in the profile, `worker_argv` in the installation record) and neither requires a Core change to swap | A live model run, a live `gh` source read, or any live host effect |
| The composed surfaces parse (`taisan check` against the profile; the installation record loads for store-backed commands) | Real credentials, a real `claude` binary, or sandbox enforcement on this host |
| Every stage below is reachable with these bindings on a real installation | That the stages were executed here — they are the operator's runbook, not a transcript |

Live execution is intentionally out of scope: it requires real
credentials, a real runtime, and an isolated repository, and is
covered by the owner-run acceptance activity, not this example.

## Files

- `config.toml` — the versioned profile. `[worker]` declares the
  `taisan-worker-claude` argv `taisan check` validates; `[intake]`
  enables scheduled intake through `public-issue-selector` with an
  explicit `--github-endpoint` scope and an all-of `--label` filter.
- `workloop.config.json` — the installation record, the actual launch
  binding: `worker_argv`, isolation profile, runtime credential
  *reference*, provider-config path, model API egress allowlist, and
  the v2 `worker_dispatch` budget/invariant-set.
- `worker-provider.json` — the supervisor-validated provider config
  staged at `/taisan/provider-config` (model and endpoint only; the
  credential is staged separately at `/taisan/runtime-auth`).

## The composed workflow

With a real installation of this profile and record:

1. **Real Issue intake.** `taisan run` ticks the intake scheduler;
   `public-issue-selector` reads open Issues in the declared scope via
   ambient `gh` auth, filters them through the configured `--label`
   set and the scheduler's unfinished-Work exclusions, and returns
   single-Issue proposals with the configured task defaults.
2. **Work.** Core admits each proposal through normal submit —
   deduplicated, capability-checked, queue-bounded. The Selector
   proposed; Core decided.
3. **Coding.** A Coding dispatch materializes the immutable
   `taisan.worker.request/2` (goal, Issue snapshots, constraints,
   verification argv, budget) and launches
   `taisan-worker-claude run` under the declared isolation profile.
   The shim drives `claude -p` inside the granted workspace; the
   produced change is a local commit the shim reports as
   `claimed_head`.
4. **Verification.** The shim reports the declared
   `verification_argv` results as claims; the *supervisor* then runs
   its own verification against the produced commit — the Worker's
   claims never satisfy a mechanical requirement by themselves.
5. **Publication.** With handoff confirmed and gates passed, the host
   adapter — the only component holding publication credentials —
   pushes the branch and opens the change request. The Worker never
   held a GitHub credential.
6. **Review finding.** A `code_review` dispatch runs the same adapter
   in the `review` role on a read-only workspace: it assesses the
   sealed diff target and reports typed `findings` against the exact
   review source.
7. **Fix.** An accepted finding produces a `fix` dispatch whose
   request binds the predecessor artifact and the sealed finding
   occurrences; the adapter repairs the work as a new commit.
8. **Republish.** The fixed artifact re-passes verification and the
   host adapter updates the change request — again without the Worker
   holding any publication authority.
9. **Reassessment.** Re-review and invariant-check dispatches run the
   reader roles on read-only workspaces; verdicts enter the evidence
   gates that decide what Core may conclude.

Every adapter boundary in that walkthrough is the configuration
surface shown here — no step requires a private adapter or a Core
change.

## Trying the pieces without a live environment

- `public-issue-selector select --sources <file>` runs the reference
  selection policy against a scripted source file — see
  [`selector-protocol`](../selector-protocol/) for request/response
  examples.
- `taisan-worker-claude` runs against any executable behind
  `--runtime`; the crate's wire-contract test suite drives it with a
  fake runtime script and no credentials (`cargo test -p
  taisan-worker-claude`).
