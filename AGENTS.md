# AGENTS.md

## Source references

- Publication-facing docs and code describe the private source
  repositories this project was distilled from by role ("the ops repo",
  "the worker repo", "the selector repo", "the infra repo" — see
  `docs/source-workflow-audit.md` for the role legend). Issue numbers
  and abbreviated revision SHAs may be cited as owner-verifiable
  evidence locators.
- By owner decision (#438) the private repository names and the owner's
  identity are acceptable to disclose: where a source or scenario name
  does appear — including the private acceptance repository
  `shimpeiws/coding-worker-smoke` — it is retained verbatim, and links
  or `#N` citations to private repositories resolve only for the
  owner. Documents carrying such references mark them as
  private/owner-only rather than presenting them as working links.
- `docs/source-workflow-audit.md` is the owner-controlled audit record:
  it retains source-internal names, issue/PR numbers, and revision SHAs
  so each design mapping can be re-verified against the sources. It
  stays in the published tree as the provenance record.
- `docs/release-audit.md` is the disclosure audit for issue #368: it
  maps every leak location by file:line or commit reference, classified
  by leak class, and records in §7 the owner disposition under which
  this tree is published.

## Pull requests

- PR titles and descriptions are written in English.

## Verification

```bash
cargo fmt --check
cargo build --workspace
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

`cargo test` contract tests spawn `target/debug/workloop-fixture-worker`, so
run `cargo build --workspace` first — `cargo test` alone does not produce the
fixture-worker binary.
