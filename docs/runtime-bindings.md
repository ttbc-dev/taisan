# Runtime bindings: the worker-neutral launch contract

Status: normative contract for managed-Worker launches. Implemented
by `crates/workloop-exec/src/bindings.rs` (validation) and
`crates/workloop-exec/src/boundary.rs` (delivery). Issue #460.

This document defines the fixed surface through which the supervisor
delivers runtime bindings — provider/model configuration, the runtime
credential, egress proxy, CA trust, and packaged runtime files — to a
managed Worker. It is worker-neutral: no member of the contract names
an adapter, runtime product, or provider, and Taisan Core never reads
an adapter-owned extension member.

## Delivery surface

The supervisor builds the sandbox; the mount set is fixed
([v0.3 §14.3](v0.3.md#143-runtime-mount-credential-and-provider-config-delivery)).
The bindings a managed Worker consumes:

| Sandbox path | Mode | Contents |
| --- | --- | --- |
| `/taisan/runtime-auth` | read-only bind of exactly one file | The staged runtime credential for this Invocation only; absent when no `worker_credential_ref` is configured |
| `/taisan/provider-config` | read-only bind of exactly one file | The `taisan.worker.bindings/1` contract document; absent when no `worker_provider_config` is configured |
| `/taisan/proxy.sock` | read-only bind | The supervisor CONNECT proxy socket; absent when `worker_model_api_hosts` is empty |
| Packaged runtime paths | read-only binds | The Worker executable and every absolute path its argv names, bound at their own path under the fixed system prefixes or as per-path read-only binds |

## The `taisan.worker.bindings/1` contract document

The provider configuration is exactly one regular file — a bounded
(≤ 64 KiB) JSON object:

| Member | Required | Rule |
| --- | --- | --- |
| `format_version` | yes | Exactly `"taisan.worker.bindings/1"`. Any other value, or absence, is a version refusal — never a tolerant parse |
| `model` | yes | Non-empty bounded string (≤ 1024 chars) naming the model the Worker runs |
| `endpoint` | no | Non-empty bounded string naming the provider endpoint the runtime calls |
| `proxy_endpoint` | no | Non-empty bounded string naming a proxy endpoint the adapter may additionally honor. Worker-owned override: the adapter may replace the supervisor-delivered `HTTPS_PROXY` pair for the runtime child with this endpoint. Under containment only loopback and the bound supervisor socket are reachable, so the override can route inside the sandbox but can never widen egress past the supervisor's CONNECT allowlist — and an endpoint that is not reachable inside the sandbox simply fails the child's egress |
| extension members | no | Adapter-owned; Core neither names nor interprets them. They never loosen the required bindings |

The document is non-secret by construction: it is validated *after*
the credential resolves, and a document containing the resolved
credential value anywhere in its bytes is refused.

### Validation and diagnostics

Validation runs at the supervisor boundary before every dispatch —
not once at load — so a post-resolution edit is always re-checked.
Every refusal is a bounded `provider_config.*` diagnostic that names
the failed rule, never the document's content:

| Diagnostic | Cause |
| --- | --- |
| `provider_config.unreadable` | Path absent or not readable — also the bounded identity digest's verdict for an oversized source, which it refuses unread before validation runs |
| `provider_config.contract_invalid` | Not a regular file, over the size bound at validation, not a JSON object, a repeated member name anywhere in the document, or a fixed member violates its shape rule (missing `model`, empty/over-long/wrong-typed value) |
| `provider_config.contract_version` | `format_version` absent or not exactly `taisan.worker.bindings/1` |
| `provider_config.contains_credential` | The resolved credential value — as a consumer reads it, trimmed and JSON-decoded — appears anywhere in the document |

An invalid or missing required binding refuses the launch; there is
no partial mount and no ambient fallback. The preflight identity hash
is bounded the same way — at most the contract size bound is read, so
an oversized source fails the gate unread instead of being hashed
before validation runs.

## Runtime credential delivery

The credential is configured only as a provider *reference*
(`env:`/`file:`/`command:` — see
[installation-config.md](installation-config.md)) and is resolved and
staged fresh at every contained launch and `describe` probe: the
latest provider value is what the next Invocation observes. Each
grant stages its own `0600` supervisor-owned file under a unique
per-grant name carrying the staging boundary's owner tag —
`runtime-auth.p<pid>.i<instance>.<seq>` — bound read-only at
`/taisan/runtime-auth` — a later grant can never rewrite the path a
pending launch is about to bind, and a refused re-resolve never
unlinks it. Retention is bounded: only the newest staged grants are
kept — a staged file is live until its launch mounts it, a window
every caller completes promptly after the grant — so older grant
files are unlinked as stale secret material rather than accumulating
for the supervisor's life — a failed unlink instead releases the
file's ownership claim, so a later sweep reclaims it as residue.
Dropping a boundary removes only the files it staged: a boundary
that was constructed but never resolved (a `run` that lost the
lease) removes nothing at the shared staging dir, and the tracked
set can never name another boundary's file: names carry the staging
boundary's owner tag, and because tags can collide across pid
namespaces a publish never overwrites an existing name — a colliding
neighbour takes the next seq — so two boundaries sharing the staging
dir can never unlink each other's live grant. Every resolve also
sweeps stale staging residue no live owner claims: a boundary holds
an exclusive `flock` on every file it stages from before the name is
published until its own drop unlinks it, so an unlocked staging
shape — a dead owner's leftovers or an unattributed legacy name
(`runtime-auth`, `runtime-auth.<seq>`) — is reclaimable, while a
file a live boundary still holds is another boundary's grant and is
never removed. A path the resolved configuration names is never
removed either, and neither is an entry that is not one of the
staging shapes — the sweep only ever unlinks a `runtime-auth`,
`runtime-auth.<seq>`, or owner-tagged
`runtime-auth.p<pid>.i<instance>.<seq>` name, or its `.tmp` sibling,
that it can prove unclaimed. A stage whose in-progress `.tmp` sibling
loses the create→flock window to a concurrent sweep — the sibling
reclaimed outright, or the sweep's transient claim probe still holding
its lock when the stage's claim tries to land — loses the race the
same way an occupied name does: it takes the next seq, never the
launch.

The credential value never enters argv, the request body,
`runtime.log`, the report, evidence, the provider contract document,
a Git object, or a GitHub credential channel. A missing or
unresolvable credential is a `credential.*` refusal.

## Environment

The environment is an allowlist assembled by the isolation profile:
the passthrough names, generated `HOME`/`TMPDIR`, the generated git
config and credential-helper pins, the declared commit identity, the
declared CA settings (`SSL_CERT_FILE`/`SSL_CERT_DIR`, only for paths
the profile declares and that exist), and — only when egress is
granted — `HTTPS_PROXY`/`https_proxy` pointing at the in-sandbox
relay. No ambient supervisor variable arrives unlisted; no credential
or socket name (`SSH_AUTH_SOCK`, repository-host tokens) is ever
generated.

## Model-API egress

Unchanged from [v0.3 §14.4](v0.3.md#144-model-api-egress):
`--unshare-net`, a supervisor-owned CONNECT proxy on
`/taisan/proxy.sock` accepting only the exact configured bare host
names on port 443, DNS resolved by the proxy, TLS uninspected, and
fail-closed behavior — an empty allowlist starts no proxy, grants no
socket bind, and exports no proxy variable. A proxy start failure is
an `egress.*` refusal, never a fallback to `--share-net`.

## CA trust and git identity

Owned by [#412](https://github.com/ttbc-dev/taisan/issues/412): the
approved `IsolationProfile` declares `ca_trust_paths` and
`commit_identity`; the profile binds declared CA paths read-only at
their own path and generates the `SSL_CERT_*`/`GIT_*` environment.
This contract adds nothing to that surface — it only requires that
the generated settings the profile produces reach the Worker
unchanged.

## Packaged runtime files and writable scratch

The packaged Worker runtime — executable, language-runtime
dependencies, package files, and runtime payload — reaches the
sandbox through the narrow read-only surface: paths under the fixed
system prefixes (`/usr`, `/bin`, `/sbin`, `/lib`, `/lib64`) ride the
wholesale read-only system binds; every other absolute path the argv
names is bound read-only at its own path. `/etc` and `/opt` are
deliberately not bound — they are host configuration and third-party
install trees, not the runtime. A Worker package installed outside
the bound prefixes therefore exposes only the paths its argv names:
the argv[0] executable and every absolute-path argv element bind at
their own path, but sibling dependencies the argv does not name stay
invisible. Packages that need a directory of dependencies must live
under a bound prefix (as in the example layout), name each required
absolute path in argv, or ship a self-contained executable. Bind
sources the contract never admits are refused by `plan_denial`
before the plan renders: host `/`, the owner's home directory or an
ancestor of it (the home is resolved from `$HOME` and the account
database, so an unset or misleading environment spelling cannot
lapse the guard; a path strictly beneath the home still binds only
the named path while the tmpfs `/home` keeps the rest invisible),
and any own-path source that is not a regular file or directory — a
host socket, device, or fifo grants an IPC endpoint, never file
content — including a directory source holding such an entry
anywhere in its tree, so an argv-named container-runtime socket
cannot be mounted. The directory scan is bounded and fails closed:
a tree that cannot be read or cannot be fully certified within the
entry bound is refused rather than admitted uninspected.
Nothing packaged is bound writable. The only writable host paths are
`/workspace` (role-dependent) and `/taisan/out`; every other writable
location inside the sandbox is ephemeral — the tmpfs the sandbox root
is assembled on (which carries only the generated mount points) and
the fresh filesystems the isolation profile mounts for the child:
`HOME` (tmpfs `/home`), `TMPDIR` (tmpfs `/tmp`), `/run` (tmpfs), and
`/dev` (the minimal device view bwrap's `--dev` builds). See
[examples/managed-worker](examples/managed-worker/README.md)
for a concrete packaged layout.

## Evidence boundary

Contract tests assert the *generated* argv, mount plan, environment,
latest-credential staging, and refusal diagnostics with dummy
credentials and fixture paths. Kernel enforcement of the binds and
namespaces — that a read-only bind denies a write, that
`--unshare-net` leaves only loopback — is proven by the containment
preflight battery on a real Linux host and is not claimed by the
fixture tests.
