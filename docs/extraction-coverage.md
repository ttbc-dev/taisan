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
