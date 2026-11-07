# Extraction coverage

The extraction inventory records which pinned source entries were reviewed and how their behaviors map to the target design. The owner-controlled [audit](source-workflow-audit.md) contains source locators and detailed review evidence. The public manifest uses role, file, behavior, and requirement IDs.

## Completion rules

Every file in each pinned tree must have one manifest entry. `reviewed` requires a linked review record describing the bodies, definitions, tests, or regions inspected. `excluded` requires a reason. Accessible material that has not been reviewed remains `pending`. `inaccessible` records a source that could not be inspected. Both states block extraction completeness.

Each behavior has one disposition: retain, re-model, omit, or defer. Retained and re-modeled behaviors link to requirements. A deferral needs an explicit owner decision. Requirements name their design owner, acceptance criterion, normative section, implementation unit, and planned proof. R01–R38 remain individually enumerable even when later evidence changes their implementation status.

A source implementation, a documented intention, a known defect, a historical repair, and an unverified operational claim are different evidence classes. A mechanism with a defect can establish why a safety requirement is needed. It does not establish that the target should copy that mechanism or claim the safety requirement is already implemented.

## Run the check

Run from the repository root:

```sh
python3 scripts/check-extraction-coverage.py
```

The strict check fails on invalid mappings, pending source reviews, unresolved dispositions, and unfinished target specifications. During the ordered design issues, this command permits explicitly planned target sections:

```sh
python3 scripts/check-extraction-coverage.py --allow-pending-design
```

This option still fails on pending or inaccessible source reviews. The output keeps `complete` false while any design gap remains. It must not be used as the final implementation-start gate.

An owner with the pinned repositories available locally can also check the catalog against their Git trees:

```sh
python3 scripts/check-extraction-coverage.py --verify-source-trees
```

This mode performs local reads only. It does not fetch, check out, or modify source repositories. Missing pinned objects remain an explicit gap.

Run the checker tests with:

```sh
python3 -m unittest discover -s scripts/tests -p 'test_*.py'
```

## Proof limits

A passing check establishes internal mapping completeness against the recorded catalog. It cannot prove that a reviewer understood every source body, that a cited test was executed, that an acceptance snapshot is still current on GitHub, or that the target works at runtime. Independent source review, acceptance comparison, and behavior verification remain separate obligations.

The source tree pins describe the audited baseline. Changes to the design after that baseline do not silently change the source inventory. Updating a pin requires rebuilding its file catalog and reviewing changed entries and affected behavior mappings.

## Approved deferrals

The seven original deferred behaviors and the subsequently approved dedicated external-result adoption deferral have explicit owner approval and retained scope in [post-v0.1 milestones](post-v0.1-milestones.md). Their `owner_disposition` fields record that decision. Planned target specifications remain gaps; approval of a deferral does not turn mapping completeness into runtime verification or close #50 automatically.

## Identity/admission mapping review

All 40 initially planned #50 targets now link to [normative identity/admission obligations](identity-storage.md#extracted-identity-and-admission-obligations), with target-specific proof scenarios. This does not upgrade implementation status or complete the other design issues. The mapping has no unresolved owner dispositions.

## Evidence and recovery mapping review

All 125 initially planned #51 targets now link to [normative evidence and recovery obligations](evidence-requirements.md), with requirement-specific proof scenarios and the shared absent, valid, and invalid evidence contract. Together with #50, this leaves 239 planned target links under #52-#56. Run the checker to regenerate these counts. `complete` remains false; no runtime implementation status is upgraded.

## Worker protocol and handoff mapping review

All 33 initially planned #52 targets now link to [normative Worker contract obligations](worker-requirements.md), each naming its governing clause and shared-suite coverage or an explicit approved deferral. Four targets (R16, R286, R288, R307) are deferred under the approved planner-machinery milestone scope; the re-modelled targets (including R189, R204, R253, R262, R287, R308, R315, R317) record mechanism substitutions — sandbox enforcement in place of server-side audit or self-repair sessions — reviewed in the 2026-11-08 #52 session. This leaves 206 planned target links under #53-#56. Run the checker to regenerate these counts. `complete` remains false; no runtime implementation status is upgraded.

## Retention and cleanup mapping review

All 18 initially planned #54 targets now link to [normative retention and cleanup obligations](retention-requirements.md), each naming its governing clause and retention-suite coverage. Source pruning mechanisms — the ops repo's personal cleanup service, container quarantine verbs, and directory-layout gc — are explicitly omitted while their preservation requirements (durable WIP, unknown-writer protection, evidence-bound retirement classification, generation-rechecked claim release, terminal-state-only checkout gc) are retained through protective-dependency pins and the eligibility matrix. This leaves 188 planned target links under #53, #55, and #56. Run the checker to regenerate these counts. `complete` remains false; no runtime implementation status is upgraded.

## Installation and authentication mapping review

All 55 initially planned #53 targets now link to [normative installation and authentication obligations](installation-requirements.md), each naming its governing clause and configuration-suite coverage. Source automation mechanisms — webhook intake routes, cron/launchd convergence, tunnel ingress, installer automation, remote operator control — are explicitly omitted while their retained obligations (deny-by-default admission, deterministic resolution, secret hygiene, fail-closed preflight, ownership-scoped mutation, atomic publication) are re-modelled onto configuration validation and the `check` contract; no new deferrals were created. This leaves 133 planned target links under #55 and #56. Run the checker to regenerate these counts. `complete` remains false; no runtime implementation status is upgraded.
