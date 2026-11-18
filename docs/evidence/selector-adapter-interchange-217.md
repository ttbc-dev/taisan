# Selector adapter interchange verification

Issue: #217. This record covers the shared external-adapter contract
suite and the intake-path interchange checks only. It is **fixture
evidence**: `workloop-fixture-selector` is a test fixture that reads
scripted source files. Nothing here claims production selection
quality, live-source coverage, or Worker-execution proof for either
adapter, and no owner-run live scenario was exercised.

## Tested material and configuration

The tested runtime baseline is the `issue-217-adapter-swap` branch;
the commit containing this record identifies the reviewed revision.
The contract is [protocol version 1](../selector-protocol.md) with
capability `grouped_sources_v1`, exercised through two independently
implemented executables:

| Adapter | Executable | Selector id | Implementation |
| --- | --- | --- | --- |
| Reference | `public-issue-selector` (`crates/workloop-selector`) | `public-issue-selector` | Packaged reference; `gh api` or scripted `--sources` file backend |
| Fixture | `workloop-fixture-selector` (`crates/workloop-fixture-selector`) | `workloop-fixture-selector` | Independent crate: own request parser, file backend, selection policy, response writer, canonical-JSON digest. No dependency on `workloop-selector`; never forwards to the reference executable |

Both were driven with selector version `1`, policy revision `7`,
argv of the form `[<exe>, "select", "--sources", <file.json>,
"--label", <name>]`, a scripted sources file, and the protocol
bounds (`max_candidates` 32 in contract scenarios, member bound 32,
total sources 1024, request 1 MiB, response 4 MiB). The intake-path
checks used the profile `[intake.selector]` binding and
`run_intake` with the same scripted sources.

## Shared contract suite results

`crates/workloop-selector/tests/adapter_contract.rs` runs every
scenario below unchanged against both executables through
`tests/common::ADAPTERS`; assertions never branch on adapter name.
`contract_suite_covers_both_adapters` fails if a third adapter is
added without extending the suite.

| Scenario (test) | Reference | Fixture |
| --- | --- | --- |
| Rank order and skip accounting | Passed | Passed |
| Exclusions, bounds, truncation | Passed | Passed |
| Exclusion by display number | Passed | Passed |
| Cross-repository rank by scoped identity | Passed | Passed |
| Typed failures for source errors | Passed | Passed |
| Deadline exceeded is a typed failure | Passed | Passed |
| Malformed and wrong-version requests rejected | Passed | Passed |
| Control-bearing request ids omitted from failures | Passed | Passed |
| Bound violations reported as `input_limit` | Passed | Passed |
| Paged sources served in order | Passed | Passed |
| Proposal identity stable across runs | Passed | Passed |
| Proposal keys identical across adapters | Passed | Passed |
| No-label configuration proposes all open issues | Passed | Passed |
| Non-conforming titles skipped | Passed | Passed |
| Zero display numbers skipped | Passed | Passed |
| Out-of-scope source facts rejected | Passed | Passed |
| Contradictory facts fail closed | Passed | Passed |
| Duplicate facts across pages deduplicate | Passed | Passed |
| Empty page list is a source failure | Passed | Passed |
| Oversized response rejected identically | Passed | Passed |

`contract_proposal_keys_match_across_adapters` recomputes the
`proposal-v1` digest for every candidate on both executables and
requires byte-identical keys — digest interoperability, not just
shape compatibility. Every fixture response additionally passes the
same `protocol::validate_response` the intake coordinator runs.

`contract_oversized_response_is_rejected_identically` pins behavior
at the response bound: an adversarial-but-valid request (maximum
constraints echoed by 32 candidates) makes **both** executables emit
the same over-limit `status:"ok"` envelope, which the launcher's
`validate_response` rejects wholesale as `response_too_large`. The
response bound is launcher-enforced by protocol design — neither
adapter self-truncates — and the scenario proves the two
implementations stay identical even there.

## Fixture-only grouping results

`crates/workloop-selector/tests/fixture_selector.rs` exercises the
`group` extension only the fixture implements (the reference reader
ignores the extra key):

| Scenario (test) | Result |
| --- | --- |
| Grouped facts emit one multi-source candidate | Passed |
| A group counts as one candidate slot | Passed |
| Groups contain only eligible members | Passed |
| Groups span repositories | Passed |
| Oversized groups fail `member_limit` | Passed |
| Group intent overflow fails `member_limit` | Passed |
| Bad argv is a bounded usage refusal | Passed |

## Intake-path interchange results

`crates/workloop-intake/tests/adapter_interchange.rs` drives both
executables through the real `run_intake` coordinator against real
stores — identical scripted sources, identical task defaults, only
the selector binding differing:

| Scenario (test) | Result |
| --- | --- |
| Either adapter argv drives the same intake contract (identical intents, identical claimed issues) | Passed |
| A grouped candidate admits as one Work: one candidate row, one submit, member order preserved in `spec.sources`, one queue position | Passed |
| A grouped submit rejects atomically on member conflict — no partial Work, unconflicted member stays unclaimed | Passed |

`crates/workloop-cli/tests/intake_cli.rs::
swapping_only_the_profile_selector_selects_the_fixture_adapter`
rewrites only `[intake.selector]`'s `adapter`/`argv` in the TOML
profile between two `taisan intake` invocations on one store: the
durable run records name `workloop-fixture-selector` then
`public-issue-selector`, exactly one Work exists afterward, and the
second run admits nothing because the first run's Work owns the
issue.

## Repository verification

| Command or check | Actual result |
| --- | --- |
| `cargo fmt --check` | Passed |
| `cargo build --workspace` | Passed |
| `cargo test --workspace` | Passed — 105 test targets, 1040 tests, 0 failures |
| `cargo clippy --workspace --all-targets -- -D warnings` | Passed |

## Scope of the claim

This record evidences **contract interchange**: two independent
implementations satisfy the same wire protocol, selection semantics,
proposal identity, and intake admission path. It does not evidence
that `workloop-fixture-selector` is suitable for production source
selection — its backend serves scripted files — and it does not
evidence Worker execution, live source reads, or any real
installation running either adapter.
