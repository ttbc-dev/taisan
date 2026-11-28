# Packaging example: a managed Worker runtime

A concrete layout for installing a managed Worker so the fixed
runtime binding surface
([runtime-bindings.md](../../runtime-bindings.md)) carries everything
it needs — the Worker executable, its Node/runtime dependencies,
package files, and the OpenCode runtime payload — through read-only
binds only.

## Layout

Install the package under a path beneath the fixed system prefixes so
the wholesale read-only system binds cover it:

```
/usr/local/lib/taisan-managed-worker/
  bin/managed-worker            # the Worker executable (see layout/bin/)
  package.json                  # package manifest
  dist/main.js                  # bundled adapter code
  node_modules/pkg/index.js     # pinned runtime dependency
  runtime/opencode/bin/opencode # OpenCode runtime payload the Worker spawns
```

`layout/` in this directory mirrors that tree file-for-file. The
installation record then names the executable and the bindings:

```json
{
  "worker_argv": ["/usr/local/lib/taisan-managed-worker/bin/managed-worker"],
  "worker_isolation_profile": "taisan-reference-v0.1",
  "worker_credential_ref": "env:TAISAN_WORKER_TOKEN",
  "worker_provider_config": "/home/operator/.config/taisan/worker-bindings.json",
  "worker_model_api_hosts": ["api.example-runtime.example"]
}
```

`worker-bindings.json` beside this README is the `taisan.worker.bindings/1`
contract document the operator installs at the path the record's
`worker_provider_config` names — in this example
`/home/operator/.config/taisan/worker-bindings.json`: `model`,
`endpoint`, no credential. The supervisor validates it before every
dispatch and binds it read-only at `/taisan/provider-config`; the
staged credential reaches the Worker only at `/taisan/runtime-auth`.

## What the Worker sees

Inside the sandbox the package paths are identical to the host — each
is a read-only bind at its own path — so a runtime that resolves its
dependencies relative to its executable works unchanged:

| Need | Where |
| --- | --- |
| Provider/model settings | `/taisan/provider-config` (read-only) |
| Runtime credential | `/taisan/runtime-auth` (read-only, this Invocation only) |
| Model API | `HTTPS_PROXY`/`https_proxy` → `/taisan/proxy.sock` relay (only when `worker_model_api_hosts` is non-empty) |
| Canonical request | `/taisan/request.json` (read-only) |
| Report, runtime.log, scratch output | `/taisan/out` (read-write, fresh per Invocation) |
| Caches, `$HOME`-relative state | `HOME` (tmpfs — nothing persists across Invocations) |
| Temp files | `TMPDIR` → `/tmp` (tmpfs) |
| Source tree | `/workspace` (read-write for `coding`/`fix`, read-only for `invariant_check`/`review`) |

Nothing under the package root is writable: the worker must not write
to its own install tree. Host `/`, container sockets, and the owner's
home directory itself (or an ancestor of it) are never bind sources —
paths strictly beneath the home may still be bound.
