# Per-change security review — PR #504 (security baseline docs)

- **Scope**: branch `issue-491-agent`, docs-only diff —
  `AGENTS.md` (Trust boundary, Invariants, Out of scope, Security review
  sections), `docs/security/audit-checklist.md`,
  `docs/security/review-policy.md`, `docs/security/audit-v0.3.md`,
  `docs/security/reviews/` (this record).
- **Required tier**: `AGENTS.md`'s Trust boundary / Invariants / Out of
  scope (added by this PR and listed as a required surface in
  `../review-policy.md`).
- **Verdict**: no unresolved findings against the change itself. The PR
  introduces the audit and review framework; every defect it records is a
  pre-existing code defect tracked for follow-up, not introduced here.
  `audit-v0.3.md` is still unpublished (it lands with this PR), so
  corrections to it are made in place rather than deferred to the next
  audit pass.
- **Review trail**: the change was produced and corrected through the
  PR's shadow-review converge loop — each posted finding was verified
  against the code and either fixed in-change or rebutted, with an
  independent manifest cross-check per round.

## Findings and dispositions

| Finding | Disposition | Evidence |
| --- | --- | --- |
| Invariants section omitted audit-recorded defects, so the false-positive filter was incomplete | Resolved in-change — every violated/partially-upheld invariant bullet now names its audit finding (F2–F6, F8–F10; F7 was reclassified to Accepted risks and F12 has no matching bullet) | `AGENTS.md` Invariants; `docs/security/audit-v0.3.md` "Invariants verified" table |
| F8 claimed every operation refuses old-incarnation requests, but collect/acknowledge bind to the stored record via `bound_request`, not the live incarnation | Resolved in-change — corrected to record-binding semantics and the collect/acknowledge consequence | `audit-v0.3.md` F8; `service.rs:447-491`, `:797-803`; `runner.rs:1126-1133` |
| F9's scenario rode on the uncontained-deployment accepted risk | Resolved in-change — re-grounded on the capability-honesty contract violation observable with benign actors under an approved registry profile | `audit-v0.3.md` F9; `service.rs:~36` (`BASE_CAPABILITIES`); `runner.rs:711-719` |
| F3's scenario was unreachable — a crash mid-stage strands the request on a dead incarnation, so no stop proof, collection, or reclaim can follow | Resolved in-change — rewritten to the in-incarnation retried-delivery write failure that survives both removal points | `audit-v0.3.md` F3; `runner.rs:472-500`; `service.rs:789`,`:848`,`:1175` |
| F6's blast radius overstated — `Runner::observe` still enforces per-request | Resolved in-change — scope narrowed to the autonomous sweep | `audit-v0.3.md` F6; `runner.rs:975`,`:1001`,`:1051-1054` |
| `rust.yml` enforcement described as "every push and PR" though it triggers only on PRs and pushes to `main` | Resolved in-change | `docs/security/review-policy.md` Automated scanning; `.github/workflows/rust.yml:3-6` |
| Review-required surface list omitted `AGENTS.md`'s canonical sections | Resolved in-change — added to the required list, together with `docs/identity-storage.md` and `docs/u3.6e-authorization.md`, the remaining claimed-invariant sources named by checklist Step 1 | `docs/security/review-policy.md` Review required; `docs/security/audit-checklist.md` Step 1 |

## Residual risk

The code defects the audit still records as defects (F2–F6, F8–F10)
remain open by design — they are follow-up issues, not this PR's
regressions. F1, F7, and F11 were reclassified into `audit-v0.3.md`'s
Accepted risks, and F12 is correct as written (a future-edit hardening
note), so none of those carry an open defect to track. One inconclusive
review candidate was withheld by the reviewer without an inline anchor;
nothing actionable was received.
