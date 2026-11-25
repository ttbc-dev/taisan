# Name migration to taisan

taisan is the public project name. The orchestration model, adapter contracts, and stored data formats are unchanged.

| Name | Current behavior |
| --- | --- |
| `taisan` | Primary CLI command, built by the `workloop-cli` package. |
| `taisan-tui` | Existing management console, built by the `workloop-tui` package; deferred from v0.1 requirements. |
| `workloop-cli`, `workloop-tui` | Retained executable aliases with identical behavior. Help, errors, and the console prompt use taisan. |
| `workloop-*` packages and crate directories | Retained Cargo package names, Rust import paths, and source paths. This includes the test-only `workloop-fixture-worker` executable. |
| `WorkloopConfig` | Retained Rust API type. |
| `workloop.db` | Unchanged default SQLite store path, relative to the working directory. |
| `workloop.config.json` | Unchanged default installation config path, derived from the database path. `--config` still overrides it. |
| Config keys, command origins, and idempotency keys | Unchanged. Both command names share the same storage and command semantics. |
| `ttbc-dev/taisan` | Current GitHub repository and clone URL. Previous slugs `shimpeiws/agent-workloop` and `shimpeiws/taisan` both redirect here. |

## Update existing installations

Build the workspace or install the new commands with the commands in the [README](../README.md#build-and-run). Replace `workloop-cli` with `taisan` and `workloop-tui` with `taisan-tui` in scripts when convenient. Existing executable names remain available when you build the workspace or install all binaries of the corresponding package.

Use the same working directory, `--db`, and `--config` values as before. No database or config file rename is required. Do not run a second coordinator against the same store while switching executable names.

Cargo package selection remains `-p workloop-cli` or `-p workloop-tui`. Use `--bin taisan` or `--bin taisan-tui` to select the new commands explicitly. A package-only `cargo run` defaults to the new command. Do not use `cargo install taisan` to install this checkout.

Generic terms such as work loop remain technical descriptions. Historical audit records and exact code references may retain workloop names.

## Update Git remotes

The GitHub repository moved from `shimpeiws/agent-workloop` to `ttbc-dev/taisan` via the intermediate slug `shimpeiws/taisan` — clones still pointing at either previous slug are redirected by GitHub. Update each existing clone:

```sh
git remote set-url origin git@github.com:ttbc-dev/taisan.git
```

For HTTPS clones, use `https://github.com/ttbc-dev/taisan.git`. Issue and pull request numbers remain unchanged, including existing open pull requests.

GitHub redirects old repository links and Git clone, fetch, and push operations. Do not reuse the old repository name, because that removes the redirects. See [GitHub repository rename behavior](https://docs.github.com/en/repositories/creating-and-managing-repositories/renaming-a-repository).

No Actions workflows, published action metadata, or GitHub Pages site were found during the rename audit. Update repository names in any external automation or local configuration you maintain. The repository description and topics are unchanged.
