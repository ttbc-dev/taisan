# v0.3 documentation evidence captures

Sanitized, non-live CLI captures retained for
[docs/v0.3-installation.md](../../docs/v0.3-installation.md) (#269).
Every file was produced by a real `taisan` invocation against:

- the **fixture host adapter** (`"adapter": "fixture"` in
  `store.config.json`) — no live host reads or writes;
- the **deterministic fixture Worker** `workloop-fixture-worker`
  (`clean-exit --wire v2 --git-commit --claim-head HEAD`) — no live
  model calls;
- a throwaway state directory (`store.db`, `bodies/`, `workspaces/`)
  that is not retained here.

## Provenance

| File | Command that produced it |
| --- | --- |
| `profile/config.toml` | `taisan init` output, then hand-edited `worker.argv`/`isolation_profile`; placeholder identities only. The isolation profile names `taisan-reference-v0.1`, the only profile `taisan run` accepts — `run` rejects the contract-harness profile by design |
| `store.config.json` | The installation record the store-backed commands loaded (published beside `store.db`) |
| `out-check-pass.json` | `taisan --config profile/config.toml check` — all stages pass, `probe_capabilities: not_run` |
| `out-check-probe.json` / `.err` | `taisan --config profile/config.toml check --probe` — stages 1–7 pass, `probe_capabilities` fails closed `capability.unknown`, exit 3 |
| `out-check-neg.json` / `.err` | Same `check` with `TAISAN_GH_TOKEN` unset — fails at `credential_refs` (`credential.missing`), exit 3 |
| `out-check-neg3.json` / `.err` | Same `check` with `isolation_profile = "taisan-reference-v0.1"` on a host without `bwrap` — fails at `isolation_primitives` (`runtime.missing`), exit 3 |
| `out-submit.json` | `taisan submit --intent … --idempotency-key doc-submit-1` |
| `out-work-list.json` | `taisan work-list` after the run |
| `out-work-detail_--work_w-1.json` | `taisan work-detail --work w-1` — two Done Invocations (`inv-1` coding, `inv-2` invariant_check), artifact `Present`, Execution `waiting` |
| `out-attention.json`, `out-attention_--work_w-1.json` | `taisan attention` — empty; no human-gate items |
| `out-capacity.json` | `taisan capacity` — one occupied slot, workspace/branch reservations held |
| `out-command-result_--origin_cli_--replay-key_doc-submit-1.json` | `taisan command-result --origin cli --replay-key doc-submit-1` — idempotent replay returns the same decision |
| `out-run.json`, `out-run2.json`, `out-run2.err` | `taisan run --ticks 8` transcript (`out-run2`) and a `--ticks 6` transcript (`out-run`) — Worker-produced commits (`fe4c03a`, `1944df5`) and per-pass progress lines |
| `out-tick-report.json` | The last `tick_report` event extracted from `store.db`'s `event_log` |

`<repo>` in the retained argv stands for the absolute checkout path
the captures ran under: `worker_argv[0]` must resolve
deterministically, so a relative `target/debug/...` spelling would
refuse — the published records substitute `<repo>` for the local
absolute prefix and are otherwise verbatim.

## What these captures prove — and do not prove

They prove the command surface, the durable record shapes, the
fail-closed preflight ordering, and the publish boundary: the run
ended with the Execution `waiting` and **zero**
`host_action_intents` rows, because no in-process driver emits
`CapabilityReport` evidence on this tree (tracked as
[#355](https://github.com/shimpeiws/taisan/issues/355)).

They are **automated fixture/contract evidence**. They never prove
live Worker behavior, live model calls, live host reads or writes, or
owner-run acceptance — those require the
[owner-run checklist](../../docs/v0.3-owner-acceptance-checklist.md)
under recorded owner authorization.
