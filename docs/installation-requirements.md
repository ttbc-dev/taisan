# Installation and authentication requirement mapping

Status: normative target obligations for Issue #53, derived from the complete pinned inventory in [extraction coverage](extraction-coverage.md). The schema, credential, role, rotation, setup, and failure clauses are specified in [installation configuration](installation-config.md). This is documentation only. No requirement below claims runtime implementation, installed credentials, or live checks.

## Reading and mapping rules

Each R heading is a precise target for the #53-owned inventory link. The scope is that requirement's configuration/authentication/setup obligation — not its entire source mechanism. Obligations owned by identity (#50), evidence (#51), the Worker contract (#52), retention (#54), or the loop (#55) are cited, not duplicated.

Many source requirements describe ops/infra-repo machinery — webhook intake routes, cron/launchd convergence, tunnel ingress, installer automation, remote operator control — that has no counterpart in the target installation. Per owner decision, every such requirement is re-modelled: the mechanism is omitted and its retained safety obligation maps onto configuration validation, the `check` preflight, fail-closed defaults, or secret hygiene. No new deferrals were created.

Every mapping names the governing clause and the configuration contract suite (CF) cases that prove it. The proof paragraphs are planned contract scenarios run through future public Command/Query/setup paths; structural checks supplement them and cannot establish live behavior. This document's 51 requirement headings cover all 51 #53 inventory targets at this revision.

## R03

Repository policy evidence — configuration slice: the allowlist, repository identities, actor roles, capability floor, and effective policy are declared in `config.toml`, validated at load, and saved with each accepted Work. Missing or unverifiable policy evidence holds dependent operations; changing installation defaults cannot silently alter a saved binding.

Clause: [configuration schema](installation-config.md#configuration-schema); [rotation, renewal, and binding preservation](installation-config.md#rotation-renewal-and-binding-preservation). Suite: CF-06, CF-07.

Planned proof for AC1: Load a policy whose repository display name matches another host's and reject it; change defaults after acceptance and show saved Work still requires its original policy.

## R04

Stable request identity — configuration slice: installation and config content participate in provenance (the saved effective policy fingerprints what was accepted), but config edits can never mint a new request identity or bypass saved-decision replay. The dedup contract itself is #50-owned.

Clause: [configuration schema](installation-config.md#configuration-schema) (edits apply to new Work); [identity storage](identity-storage.md#serialization-and-command-replay). Suite: CF-11.

Planned proof for AC5: Edit config between a rejected and an identical retried command — replay still returns the saved decision.

## R08

Unknown observations and repair prerequisites — configuration slice: an unknown capability report, an unresolvable credential, or an unreachable source is `unknown`, never denied/supported. Resume requires the failed prerequisite to acquire fresh valid evidence; unchanged evidence cannot resume.

Clause: [failure and repair semantics](installation-config.md#failure-and-repair-semantics); [roles and permission matrix](installation-config.md#roles-and-permission-matrix). Suite: CF-06, CF-15.

Planned proof for AC5: Inject a capability probe outage and an unchanged broken credential; neither creates a replacement grant, and resume is refused until fresh validated evidence exists.

## R18

Typed failure classification — configuration slice: configuration, credential, capability, runtime, and binding failures have closed typed codes distinct from process exit status; each maps to a specific allowed repair action.

Clause: [failure and repair semantics](installation-config.md#failure-and-repair-semantics). Suite: CF-01, CF-03.

Planned proof for AC5: Produce a malformed-config failure and a denied-capability failure — distinct codes, distinct repair prerequisites, neither becomes a blind retry.

## R24

Bounded reasons and retained diagnostics — configuration slice: every failure code is closed and display-bounded; credential values can never appear in a failure reason, projection, or export. An old repaired refusal never overrides a newer authoritative result.

Clause: [credential references and secret hygiene](installation-config.md#credential-references-and-secret-hygiene); [failure and repair semantics](installation-config.md#failure-and-repair-semantics). Suite: CF-03, CF-04.

Planned proof for AC1: Seed a secret in an error pathway and inspect argv/log/report output — zero occurrences; supersede an auth failure only through fresh validated repair evidence.

## R27

Durable judgment history — configuration slice: saved bindings, effective policy snapshots, and actor provenance are durable metadata that config edits cannot rewrite. A `check`/reload cannot erase or reinterpret what a Work was authorized under.

Clause: [rotation, renewal, and binding preservation](installation-config.md#rotation-renewal-and-binding-preservation). Suite: CF-09, CF-11.

Planned proof for AC5: Change the default reviewer actor after Work acceptance — the saved binding and judgment history still name the original actor.

## R29

Operator console over versioned read contracts and typed actions — configuration slice: operator actions are typed Commands bound to the installation actor identity supplied by configuration; the config supplies actor identity, never authority by itself. Console/TUI delivery stays deferred under #47; the shared contract requirements are unchanged.

Clause: [configuration schema](installation-config.md#configuration-schema) (`[installation]` actor); [roles and permission matrix](installation-config.md#roles-and-permission-matrix). Suite: CF-16.

Planned proof for AC2: An operator command submitted under a stale WorkVersion is refused with the typed conflict — the configured actor does not bypass version guards.

## R30

Read-only reconciliation — configuration slice: reconciliation uses the intent's original actor/binding and read operations only. Lost read permission keeps effects `unknown`; no credential rotation may reattribute or replay.

Clause: [roles and permission matrix](installation-config.md#roles-and-permission-matrix) (reconciliation column); [rotation, renewal, and binding preservation](installation-config.md#rotation-renewal-and-binding-preservation). Suite: CF-10.

Planned proof for AC5: Revoke then replace a token after a lost push response — reconciliation runs under the original binding and a different actor's token cannot reattribute the effect.

## R33

Changed-evidence resume — configuration slice: every auth/config repair names the failed prerequisite; resume requires fresh validated evidence of that prerequisite changing. A generic "fixed" message or unchanged evidence cannot resume.

Clause: [failure and repair semantics](installation-config.md#failure-and-repair-semantics). Suite: CF-15.

Planned proof for AC5: Retry resume with identical expired-credential evidence — refused; update the credential, pass `check` stage 3, and resume under unchanged consumed usage.

## R34

Foreign-writer attribution — configuration slice: managed roots, role-bound credentials, and workspace isolation define what counts as a managed writer. Credentials never leave the adapter boundary, so an unmanaged process cannot impersonate a managed writer through leaked configuration; bodies outside managed identity stay foreign to retention as well.

Clause: [installation layout](installation-config.md#installation-layout); [credential references and secret hygiene](installation-config.md#credential-references-and-secret-hygiene). Suite: CF-17, CF-18.

Planned proof for AC2: A process outside the managed root attempts a managed-path write and a managed-identity read — the path hierarchy and credential boundary refuse both; nothing records it as a managed writer.

## R35

Actionable Attention evidence — configuration slice: a config/auth failure produces an Attention item naming the failed prerequisite and the required repair action; acknowledgement changes presentation only and cannot repair evidence.

Clause: [failure and repair semantics](installation-config.md#failure-and-repair-semantics). Suite: CF-13, CF-15.

Planned proof for AC5: Acknowledge a denied-capability Attention — the capability stays denied until fresh allowed evidence exists.

## R36

Visible unknown sources — configuration slice: capability `unknown`, credential `unresolvable`, and source `unavailable` surface as distinct states with retained reasons; none collapses into denied/absent/supported.

Clause: [evidence validity for configuration items](installation-config.md#evidence-validity-for-configuration-items). Suite: CF-06.

Planned proof for AC5: Return a probe timeout for one capability and a clean denial for another — both hold admission, reported distinctly.

## R37

Effective configuration evidence — configuration slice: assessment and Outcome refer to the saved effective policy, actor, and required capability identities. `check` success proves validated configuration, not a standing runtime grant; installing files or changing defaults alone proves no capability.

Clause: [configuration schema](installation-config.md#configuration-schema); [setup contract](installation-config.md#setup-contract). Suite: CF-12, CF-14, CF-23.

Planned proof for AC1: Pass `check`, then dispatch rechecks capabilities anyway; a config written but never validated cannot justify any operation.

## R38

Secret-safe evidence — configuration slice: credential values never persist in config, intents, evidence, argv, logs, or exports; only opaque refs are stored. Resolution happens at the adapter boundary in-process only.

Clause: [credential references and secret hygiene](installation-config.md#credential-references-and-secret-hygiene). Suite: CF-03, CF-04.

Planned proof for AC1: Seed credentials in env/file/command providers and inspect every retained surface; fail resolution and require repair without anonymous fallback.

## R49

Public tunnel ingress — mechanism omitted: v0.1 installs no public ingress at all. The retained obligation — deny-by-default admission of only declared traffic — transfers to configuration: no listener exists unless explicitly declared, and undeclared endpoint/route keys are refused at load.

Clause: [configuration schema](installation-config.md#configuration-schema) (unknown keys refused). Suite: CF-01.

Planned proof for AC5: A config carrying an undeclared ingress/listener key fails loading; the installation exposes no route.

## R50

Dependabot-style automated merge/close — mechanism omitted: v0.1 has no bot-merge automation. The retained obligation — external-effect mutations require an explicit named command plus fresh state re-fetch — is already the intent contract: every host effect needs a durable authorized `HostActionIntent` and a fresh precondition read at apply.

Clause: [roles and permission matrix](installation-config.md#roles-and-permission-matrix) (effect apply). Suite: CF-07.

Planned proof for AC5: A head change between intent save and apply aborts the write with a fresh observation; no automatic mutation exists to inherit.

## R56

Ordered model-provider fallback — config slice: `worker.model` and runtime pins are declared explicitly; an ordered provider fallback chain, if configured, lists each provider with its own pinned endpoint — never an ambient default. Malformed provider config fails the whole worker section.

Clause: [configuration schema](installation-config.md#configuration-schema) (`[worker]`). Suite: CF-19.

Planned proof for AC5: A provider entry missing its endpoint fails load; a pinned order is honored exactly as declared.

## R57

Refinement mutations rate-capped — mechanism omitted: v0.1 performs no source-Issue mutation (no refinement automation, no label edits). The retained obligation — automated source mutation is bounded and deny-by-default — transfers to config: source-mutation capabilities are undeclared in v0.1 and any future declaration is explicit and rate-limited.

Clause: [configuration schema](installation-config.md#configuration-schema). Suite: CF-01.

Planned proof for AC5: A config declaring a source-mutation capability not in the closed set fails loading.

## R58

Loopback webhook identity — mechanism omitted: v0.1 has no event webhook. The retained obligation — machine-scoped identity with a file-held secret, never argv — is satisfied by the credential-reference providers: machine-scoped secret material lives behind `file:`/`command:` refs.

Clause: [credential references and secret hygiene](installation-config.md#credential-references-and-secret-hygiene). Suite: CF-03, CF-04.

Planned proof for AC5: Any future event-ingress secret is declared as a `credential_ref`, never argv; the pattern is exercised by CF-03.

## R60

Maintenance-automation rollout gates — mechanism omitted: no automated issue creation or scheduled handoff exists. The retained obligation — automation stays disabled until proven by dry-run — transfers to the general rule: every automation feature is off unless explicitly declared, and `check`/`--probe` exercises it read-only before any effect.

Clause: [setup contract](installation-config.md#setup-contract). Suite: CF-14.

Planned proof for AC5: An unconfigured feature contributes zero effects; `check` exercises it read-only without producing writes.

## R61

External-agent delegation — re-modelled: delegation to a Worker already requires a durable Invocation identity and bounded declared capabilities (with #52). Dangerous operations require the same human/Core approval path as any effect; there is no ambient delegation channel.

Clause: [roles and permission matrix](installation-config.md#roles-and-permission-matrix); [worker protocol](worker-protocol.md#roles-and-authority). Suite: CF-18.

Planned proof for AC5: A Worker request carrying an undeclared write scope is refused; only the bounded declared capability executes.

## R62

Loopback bearer auth shell-equivalent — mechanism omitted: the CLI is already shell-equivalent for the local user; no remote control surface exists. The retained obligation — any control surface is treated as shell-equivalent authority — transfers to the rule that installation authority is the local user's; a future remote surface must be explicitly declared and authenticated.

Clause: [credential references and secret hygiene](installation-config.md#credential-references-and-secret-hygiene). Suite: CF-01.

Planned proof for AC5: A config declaring a remote control listener without declared authentication fails load.

## R69

Coordinator config is the sole source of remote verify profiles — retained: `config.toml` is the single source for verification/egress profiles; a malformed mapping fails the whole affected section closed, never a partial fallback.

Clause: [configuration schema](installation-config.md#configuration-schema) (`[verification.egress]`). Suite: CF-01, CF-05.

Planned proof for AC5: A malformed egress mapping fails load rather than silently defaulting to open egress.

## R72

Per-run scoped grants — retained: a request's declared scope widens access for exactly that request; forbidden patterns always win over any grant, per the closed capability model.

Clause: [roles and permission matrix](installation-config.md#roles-and-permission-matrix). Suite: CF-18.

Planned proof for AC5: A request declaring both an allowed and a forbidden effect is refused; no grant bypasses the forbidden set.

## R73

Staged merge re-proving preconditions — retained: dispatch re-validates admission preconditions under the store transaction before request commit; a staged approval cannot bypass the commit-time recheck.

Clause: [roles and permission matrix](installation-config.md#roles-and-permission-matrix) (dispatch). Suite: CF-07.

Planned proof for AC5: Change capability evidence between admission and dispatch — the commit-time recheck defers with diagnostic.

## R80

Scheduled credential mint — mechanism omitted: no scheduled secret rewriting exists. The retained obligation — credential materialization is atomic and never leaks to argv — transfers to the `file:` provider's atomicity expectations and the argv-free resolution boundary.

Clause: [credential references and secret hygiene](installation-config.md#credential-references-and-secret-hygiene). Suite: CF-03, CF-04.

Planned proof for AC5: A credential file swapped atomically mid-Work resolves cleanly; a torn read reports `credential.unresolvable`, never a partial secret.

## R90

Persisted triage verdicts bound to fingerprints — mechanism omitted: v0.1 has no triage verdict cache. The retained obligation — persisted assessments bind input and config fingerprints and are invalidated by change — matches the saved-decision/replay rules: a decision binds its exact payload and effective policy.

Clause: [configuration schema](installation-config.md#configuration-schema); [identity storage](identity-storage.md#serialization-and-command-replay). Suite: CF-11.

Planned proof for AC5: A changed effective policy cannot reuse a prior acceptance; the payload/fingerprint mismatch is a recorded conflict.

## R94

Remote operator control — mechanism omitted: no remote control surface exists. The retained obligation — operator actions re-verify fresh state and identity before mutation — is the existing Command precondition model (WorkVersion, binding, fresh reads).

Clause: [roles and permission matrix](installation-config.md#roles-and-permission-matrix). Suite: CF-07.

Planned proof for AC5: A stale operator read cannot drive a mutation; the Command path re-verifies version and preconditions.

## R97

Deterministic interpreter resolution — retained: `worker.argv` and tool resolution are deterministic; executable lookup follows declared paths or pinned lookup, never ambient PATH surprise; explicit environment overrides are the only override channel.

Clause: [configuration schema](installation-config.md#configuration-schema) (`[worker]`); [setup contract](installation-config.md#setup-contract) (stage 5). Suite: CF-22.

Planned proof for AC5: Shadow the runtime name in a prepended PATH entry — resolution still yields the declared binary or fails `runtime.missing`.

## R116

Two-phase bounded worktree reclamation — retention slice belongs to #54; the #53-owned obligation is the operator-repair Command: a human removal decision binds to an exact evidenced plan and revalidates every criterion under the maintenance lock before executing.

Clause: [retention policy](retention-policy.md#cleanup-eligibility-and-failure-matrix) (foreign bodies row); [setup contract](installation-config.md#setup-contract). Suite: CF-14.

Planned proof for AC5: An operator repair Command rechecks the candidate's attribution and pins at commit; a body that gained a pin is refused.

## R130

Webhook route filtering — mechanism omitted: no webhook intake. Retained obligation maps to fail-closed config: undeclared intake routes cannot exist.

Clause: [configuration schema](installation-config.md#configuration-schema). Suite: CF-01.

Planned proof for AC5: An undeclared webhook/route key fails loading; nothing listens.

## R156

Installer mutation of shared user config — retained: `init`/`check` write only installation-owned files; shared user configuration is never mutated. A repair requiring an external file change is documented for the operator, never executed silently.

Clause: [setup contract](installation-config.md#setup-contract). Suite: CF-12, CF-14.

Planned proof for AC5: Run `init` — only `config.toml` and installation-root files are written; `~/.gitconfig` and shell files are byte-identical.

## R194

Tiered stale-worktree reclamation — the fail-closed scan discipline belongs to the retention policy: only managed-root candidates are sweep-eligible; foreign paths are never scanned for deletion.

Clause: [retention policy](retention-policy.md#cleanup-eligibility-and-failure-matrix). Suite: CF-17 (path boundary), RT-06.

Planned proof for AC5: A stale-lookalike directory outside the managed root is never a sweep candidate.

## R195

Owner-private preservation before manual removal — retained: an operator repair Command removing a workspace with unpublished WIP requires preservation evidence first (the durable handoff/bundle records under the retention policy); removal without preservation evidence is refused.

Clause: [retention policy](retention-policy.md#cleanup-eligibility-and-failure-matrix) (foreign bodies row). Suite: CF-14.

Planned proof for AC5: Attempt removal of a workspace holding unpushed WIP — refused until the preserved-object evidence exists.

## R196

CI-failure webhook route — mechanism omitted: no webhook intake. Retained deny-by-default obligation maps to fail-closed configuration.

Clause: [configuration schema](installation-config.md#configuration-schema). Suite: CF-01.

Planned proof for AC5: Undeclared intake keys fail load; no route exists.

## R197

Activation only after deployed copy proves inputs — retained: a configuration becomes usable only after `check` validates it; installing bytes is not activation. Dispatch still revalidates.

Clause: [setup contract](installation-config.md#setup-contract). Suite: CF-12, CF-13.

Planned proof for AC5: A config file written but never checked cannot authorize dispatch; `check` failures block use.

## R198

Deny-guard proven to fire — retained: denied operations are exercised by contract tests, not merely declared: a `use = "source"` publication attempt, a forbidden capability, and a foreign-body sweep each must demonstrably fail.

Clause: [roles and permission matrix](installation-config.md#roles-and-permission-matrix). Suite: CF-06, CF-18.

Planned proof for AC5: Drive a denied operation through each boundary and observe the typed refusal with zero adapter writes.

## R199

Convergence preserving operator intent — mechanism omitted: no cron reconcile. The retained obligation — an operator's enable/disable decision survives re-staging — transfers to the rule that `check`/`init` never silently re-enable or rewrite what the operator set; durable intent markers in config are honored.

Clause: [setup contract](installation-config.md#setup-contract). Suite: CF-12.

Planned proof for AC5: Re-run `init` over an existing config — it does not rewrite operator-set fields; a disabled feature stays disabled.

## R200

Atomic console promote — retained: config publication is atomic (the existing hard-link/rename publish); a staged candidate never partially activates; the previous complete config is never silently destroyed.

Clause: [configuration schema](installation-config.md#configuration-schema); [setup contract](installation-config.md#setup-contract). Suite: CF-24.

Planned proof for AC5: Interrupt an init publish — either no config or a complete config exists; scratch residue is never loaded.

## R201

Sync-safe convergence — retained: a durable removal/disable marker (e.g., a removed adapter) blocks re-binding for existing Work; up-to-date state is proven read-only before any staged change.

Clause: [rotation, renewal, and binding preservation](installation-config.md#rotation-renewal-and-binding-preservation). Suite: CF-11, CF-14.

Planned proof for AC5: Mark a host removed — existing Work keeps its binding and new admission cannot bind it.

## R203

Secret provisioning off argv — retained: any captured secret is stored only behind a `credential_ref`; subscribe/provision output containing secrets is captured to a protected file/ref, never placed on argv or in config text.

Clause: [credential references and secret hygiene](installation-config.md#credential-references-and-secret-hygiene). Suite: CF-03, CF-04.

Planned proof for AC5: Simulate a provisioning output containing a token — it is persisted only as a ref target with mode `0600`, never in config text or logs.

## R207

Untrusted spec-evidence quotes — mechanism omitted (selector-side machinery). The retained obligation — untrusted quoted content is sanitized, bounded, and re-proven against the source before it may drive anything — is already the evidence-validity contract: quoted text is evidence, re-proven against the exact source revision, never authority.

Clause: [evidence gates](evidence-gates.md#evidence-schema-and-validity); [configuration schema](installation-config.md#configuration-schema). Suite: CF-06.

Planned proof for AC5: A quoted spec excerpt that no longer matches the observed source fails revalidation.

## R222

Packaged-console manifest agreement — mechanism omitted: no packaged console exists. The retained obligation — a deployed artifact is proven by a deterministic manifest agreeing with the published contract — transfers to the config contract: the loaded schema_version and effective policy digest are the manifest bound to evidence.

Clause: [configuration schema](installation-config.md#configuration-schema). Suite: CF-23.

Planned proof for AC5: A config whose declared schema_version disagrees with its structure is refused, not reinterpreted.

## R229

Health-gated intake deployment — mechanism omitted: no deployed intake. The retained obligation — a component is enabled only after end-to-end health is proven — is the `check` contract: ordered read-only stages gate use, and `--probe` proves adapter health before reliance.

Clause: [setup contract](installation-config.md#setup-contract). Suite: CF-13, CF-14.

Planned proof for AC5: A failing stage prevents activation claims; later stages report as not-run, not passed.

## R230

Scheduled E2E route filter — mechanism omitted. Retained deny-by-default obligation maps to fail-closed configuration.

Clause: [configuration schema](installation-config.md#configuration-schema). Suite: CF-01.

Planned proof for AC5: Undeclared intake keys fail load.

## R232

Scheduled run-output sweep — the proven-terminal sweep belongs to the retention policy: only proven-terminal, retention-expired outputs are reclaimed, with whole-tree inventory accounting.

Clause: [retention policy](retention-policy.md#cleanup-eligibility-and-failure-matrix). Suite: RT-03, CF-17.

Planned proof for AC5: A nonterminal run's outputs are never eligible; terminal-aged outputs reclaim under the bounded pass.

## R233

CI-failure investigation — mechanism omitted: no automated CI-failure investigation lane. The retained obligation — diagnostic reads are read-only and bounded to the delivered run — maps to `check`'s read-only discipline and bounded diagnostic reads in the module contracts.

Clause: [setup contract](installation-config.md#setup-contract). Suite: CF-14.

Planned proof for AC5: `check` performs zero mutations; untrusted content read during diagnostics is bounded and classified, never executed.

## R241

Backend-directory fixed precedence — retained: installation paths resolve through one fixed precedence — flag, environment override, XDG default — and the same precedence applies at every use site.

Clause: [installation layout](installation-config.md#installation-layout). Suite: CF-16.

Planned proof for AC5: Conflicting env/cwd hints cannot redirect state or config resolution; precedence order is identical across entry points.

## R245

Ordered read-only fail-closed preflight — retained directly: `check` is ordered, read-only, and fail-closed; a failing stage gates everything after it.

Clause: [setup contract](installation-config.md#setup-contract). Suite: CF-13, CF-14.

Planned proof for AC5: Inject a failure at stage 3 — stages 4+ report not-run; nothing proceeds to dispatch.

## R254

Profile init scaffolds only from observed evidence — retained directly: `init` validates from observed facts and never fabricates a profile; an unresolvable identity fails rather than guessing.

Clause: [setup contract](installation-config.md#setup-contract). Suite: CF-12.

Planned proof for AC5: Point `init` at an unresolvable repository — it fails with the observed evidence; no invented identity is written.

## R256

Model source pinning — config slice: `worker.model` and `runtime_version` are pinned at load; the pin recorded with the request is re-checked after dispatch and over persisted evidence; a reported model differing from the pin is recorded, not silently adopted.

Clause: [configuration schema](installation-config.md#configuration-schema) (`[worker]`). Suite: CF-19.

Planned proof for AC5: A runtime reporting a different model version after dispatch records the mismatch; the pinned expectation persists in evidence.

## R258

Dual identities splitting author and reviewer write surfaces — config slice: role bindings may assign publication and review to distinct actors; the recommended profile does so; actor slugs are verified at probe/dispatch, never assumed from config text.

Clause: [roles and permission matrix](installation-config.md#roles-and-permission-matrix). Suite: CF-05, CF-20.

Planned proof for AC5: A reviewer ref resolving to the publisher's actor is flagged by `credential.actor_mismatch` unless policy explicitly permits sharing.

## R276

Reviewer posting identity resolves per-repo — retained: the review role's credential resolves for the bound repository context; a reviewer identity is verified against the host, never silently inherited from another repo or role.

Clause: [roles and permission matrix](installation-config.md#roles-and-permission-matrix). Suite: CF-05, CF-20.

Planned proof for AC5: A review intent for repository B under repository A's binding is refused structurally.

## R289

Live-dispatch readiness gate — retained: declared checks and required capabilities are verified at the pinned base before dispatch; unenforceable requirements refuse admission rather than degrade.

Clause: [roles and permission matrix](installation-config.md#roles-and-permission-matrix) (dispatch). Suite: CF-07.

Planned proof for AC5: A required check that cannot be verified at the pinned base holds dispatch with diagnostic.

## R330

Local resource attribution positively proven — retained: only resources positively attributed to the installation are managed; a non-local URL never inherits local listeners, and unattributed bodies are foreign to both retention and authority.

Clause: [installation layout](installation-config.md#installation-layout); [retention policy](retention-policy.md#cleanup-eligibility-and-failure-matrix). Suite: CF-17.

Planned proof for AC5: A config pointing at a non-local endpoint cannot claim local managed resources; unattributed paths stay foreign.
