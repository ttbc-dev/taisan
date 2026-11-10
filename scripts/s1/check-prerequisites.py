#!/usr/bin/env python3
"""Validate prepared S1 scenario materials without executing the scenario.

This is a read-only preflight for the fixtures in ``fixtures/s1``. It
checks that the isolated installation specification, the deterministic
seeded issue, and the criterion-to-evidence checklist are present and
complete. It never contacts GitHub, starts a Worker, opens a store, or
runs the live scenario.
"""
from __future__ import annotations

import argparse
import json
from pathlib import Path

S1_REPOSITORY = "shimpeiws/coding-worker-smoke"
VARIANTS = ("success", "Attention", "recovery")
IDENTITY_KEYS = ("work", "execution", "invocation", "action")
FIXTURE_FILES = {
    "installation-spec": "fixtures/s1/installation-spec.json",
    "seeded-issue": "fixtures/s1/seeded-issue.json",
    "evidence-checklist": "fixtures/s1/evidence-checklist.json",
}
DOC_FILES = {
    "installation-spec-doc": "docs/s1/installation-spec.md",
    "seeded-issue-doc": "docs/s1/seeded-issue.md",
    "evidence-checklist-doc": "docs/s1/evidence-checklist.md",
    "materials-readme": "docs/s1/README.md",
}
CREDENTIAL_VALUE_KEYS = {"value", "secret", "token", "password", "pat"}
REVISION_KEYS = (
    "materials_revision",
    "taisan_core_revision",
    "taisan_cli_revision",
    "github_adapter_revision",
    "worker_adapter_revision",
    "configuration_identity",
    "contract_version",
)


class CheckFailure(Exception):
    """A fail-closed prerequisite or material check."""

    def __init__(self, errors: list[str]):
        self.errors = errors
        super().__init__("; ".join(errors))


def _load_json(path: Path) -> dict:
    try:
        data = json.loads(path.read_text())
    except OSError as exc:
        raise CheckFailure([f"{path}: unreadable ({exc})"]) from exc
    except json.JSONDecodeError as exc:
        raise CheckFailure([f"{path}: malformed JSON ({exc})"]) from exc
    if not isinstance(data, dict):
        raise CheckFailure([f"{path}: expected a JSON object"])
    return data


def _nonempty(value) -> bool:
    if isinstance(value, str):
        return bool(value.strip())
    return bool(value)


def _check_identity_scope(prefix: str, value, errors: list[str]) -> None:
    if value != [S1_REPOSITORY]:
        errors.append(f"{prefix}: must scope to exactly [{S1_REPOSITORY!r}], got {value!r}")


def validate_install_spec(spec: dict) -> list[str]:
    """Return every install-spec failure; an empty list means complete."""
    errors: list[str] = []
    if spec.get("schema_version") != 1:
        errors.append("schema_version: expected 1")
    if spec.get("kind") != "s1-installation-spec":
        errors.append("kind: expected s1-installation-spec")
    if spec.get("live_execution_authorized") is not False:
        errors.append("live_execution_authorized: preparation must not authorize live execution")

    allowlist = spec.get("installation_allowlist")
    if not isinstance(allowlist, list) or allowlist != [S1_REPOSITORY]:
        errors.append(
            "installation_allowlist: must contain only "
            f"{S1_REPOSITORY!r}, got {allowlist!r}"
        )

    repository = spec.get("repository")
    if not isinstance(repository, dict):
        errors.append("repository: missing object")
    else:
        if repository.get("host") != "github.com":
            errors.append("repository.host: expected github.com")
        if repository.get("full_name") != S1_REPOSITORY:
            errors.append(f"repository.full_name: expected {S1_REPOSITORY!r}")
        if not _nonempty(repository.get("stable_id")):
            errors.append("repository.stable_id: required")

    credential = spec.get("credential_reference")
    if not isinstance(credential, dict) or not credential:
        errors.append("credential_reference: missing")
    else:
        if not _nonempty(credential.get("provider")):
            errors.append("credential_reference.provider: required")
        if not _nonempty(credential.get("name")):
            errors.append("credential_reference.name: required")
        if credential.get("provider") not in {"env", "file", "command"}:
            errors.append("credential_reference.provider: expected env, file, or command")
        if credential.get("role") != "publication":
            errors.append("credential_reference.role: expected publication")
        _check_identity_scope("credential_reference.scope", credential.get("scope"), errors)
        leaked = sorted(CREDENTIAL_VALUE_KEYS & set(credential))
        if leaked:
            errors.append(
                "credential_reference: must never carry a value "
                f"(unexpected keys: {', '.join(leaked)})"
            )
        permissions = credential.get("permissions")
        if not isinstance(permissions, dict):
            errors.append("credential_reference.permissions: missing")
        else:
            expected = {
                "contents": "write",
                "pull_requests": "write",
                "issues": "write",
                "checks": "read",
            }
            if permissions != expected:
                errors.append(
                    "credential_reference.permissions: expected "
                    f"{expected!r}, got {permissions!r}"
                )

    reviewer = spec.get("reviewer_identity")
    if not isinstance(reviewer, dict) or not reviewer:
        errors.append("reviewer_identity: missing")
    else:
        if reviewer.get("kind") != "github-app":
            errors.append("reviewer_identity.kind: expected github-app")
        if not _nonempty(reviewer.get("app_slug")):
            errors.append("reviewer_identity.app_slug: required")
        if not _nonempty(reviewer.get("app_id")):
            errors.append("reviewer_identity.app_id: required")
        if not _nonempty(reviewer.get("actor")):
            errors.append("reviewer_identity.actor: required")
        _check_identity_scope(
            "reviewer_identity.installation_scope",
            reviewer.get("installation_scope"),
            errors,
        )
        if reviewer.get("distinct_from_publication_actor") is not True:
            errors.append(
                "reviewer_identity.distinct_from_publication_actor: must be true"
            )

    worker = spec.get("worker_runtime_identity")
    if not isinstance(worker, dict) or not worker:
        errors.append("worker_runtime_identity: missing")
    else:
        if not _nonempty(worker.get("adapter")):
            errors.append("worker_runtime_identity.adapter: required")
        if not _nonempty(worker.get("identity")):
            errors.append("worker_runtime_identity.identity: required")
        effects = worker.get("allowed_effects")
        if not isinstance(effects, dict):
            errors.append("worker_runtime_identity.allowed_effects: missing")
        elif effects.get("repository_host_write") is not False:
            errors.append(
                "worker_runtime_identity.allowed_effects.repository_host_write: "
                "must be false"
            )

    revisions = spec.get("revisions")
    if not isinstance(revisions, dict):
        errors.append("revisions: missing")
    else:
        for key in (
            "materials",
            "materials_base_commit",
            "taisan_core",
            "taisan_cli",
            "github_adapter",
            "worker_adapter",
        ):
            if not _nonempty(revisions.get(key)):
                errors.append(f"revisions.{key}: required")

    config = spec.get("configuration_identity")
    if not isinstance(config, dict):
        errors.append("configuration_identity: missing")
    else:
        for key in (
            "store_id",
            "host_instance",
            "repository",
            "adapter",
            "contract_version",
            "capabilities",
            "required_checks",
            "budget",
        ):
            if not _nonempty(config.get(key)):
                errors.append(f"configuration_identity.{key}: required")
        if config.get("repository") != S1_REPOSITORY:
            errors.append(
                f"configuration_identity.repository: expected {S1_REPOSITORY!r}"
            )
        if config.get("host_instance") != "github.com":
            errors.append("configuration_identity.host_instance: expected github.com")

    identities = spec.get("required_identities")
    if not isinstance(identities, list):
        errors.append("required_identities: missing")
    else:
        missing = [key for key in ("work_id", "execution_id", "invocation_id", "action_id")
                   if key not in identities]
        if missing:
            errors.append(f"required_identities: missing {', '.join(missing)}")
    return errors


def validate_seeded_issue(seed: dict) -> list[str]:
    errors: list[str] = []
    if seed.get("schema_version") != 1:
        errors.append("schema_version: expected 1")
    if seed.get("kind") != "s1-seeded-issue":
        errors.append("kind: expected s1-seeded-issue")
    if seed.get("live_execution_authorized") is not False:
        errors.append("live_execution_authorized: preparation must not authorize live execution")
    if not _nonempty(seed.get("title_template")) or "<run-tag>" not in seed["title_template"]:
        errors.append("title_template: required and must carry <run-tag>")
    if not _nonempty(seed.get("body_template")):
        errors.append("body_template: required")
    if seed.get("labels") != []:
        errors.append("labels: seeded issue must carry no labels")
    if not _nonempty(seed.get("label_policy")):
        errors.append("label_policy: required")
    constraints = seed.get("constraints")
    if not isinstance(constraints, dict):
        errors.append("constraints: missing")
    else:
        if constraints.get("force_push") is not False:
            errors.append("constraints.force_push: must be false")
        if constraints.get("merge") is not False:
            errors.append("constraints.merge: must be false")
        if constraints.get("deploy") is not False:
            errors.append("constraints.deploy: must be false")
        if not constraints.get("writable_paths"):
            errors.append("constraints.writable_paths: required")
    if not isinstance(seed.get("verification_argv"), list) or not seed["verification_argv"]:
        errors.append("verification_argv: required")
    if not seed.get("expected_review_gap"):
        errors.append("expected_review_gap: required")
    if "zero" not in str(seed.get("zero_finding_policy", "")).lower():
        errors.append("zero_finding_policy: required")

    variants = seed.get("variants")
    _check_variants("variants", variants, errors)
    if isinstance(variants, dict):
        for name in VARIANTS:
            entry = variants.get(name)
            if not isinstance(entry, dict):
                continue
            if not _nonempty(entry.get("purpose")):
                errors.append(f"variants.{name}.purpose: required")
            outcome = entry.get("expected_outcome")
            if not isinstance(outcome, dict) or not _nonempty(outcome.get("work_state")):
                errors.append(f"variants.{name}.expected_outcome.work_state: required")
        success = variants.get("success", {}).get("expected_outcome", {})
        attention = variants.get("Attention", {}).get("expected_outcome", {})
        if success.get("work_state") != "succeeded":
            errors.append("variants.success.expected_outcome.work_state: expected succeeded")
        if attention.get("work_state") != "paused":
            errors.append("variants.Attention.expected_outcome.work_state: expected paused")
    return errors


def _check_variants(prefix: str, variants, errors: list[str]) -> None:
    if not isinstance(variants, dict):
        errors.append(f"{prefix}: missing object")
        return
    if set(variants) != set(VARIANTS):
        errors.append(
            f"{prefix}: expected exactly {list(VARIANTS)!r}, got {sorted(variants)!r}"
        )


def validate_checklist(checklist: dict) -> list[str]:
    errors: list[str] = []
    if checklist.get("schema_version") != 1:
        errors.append("schema_version: expected 1")
    if checklist.get("kind") != "s1-criterion-to-evidence-checklist":
        errors.append("kind: expected s1-criterion-to-evidence-checklist")
    if checklist.get("live_execution_authorized") is not False:
        errors.append("live_execution_authorized: preparation must not authorize live execution")
    _check_variants("variants", checklist.get("variants"), errors)
    variants = checklist.get("variants")
    if isinstance(variants, dict):
        for name in VARIANTS:
            entry = variants.get(name)
            if not isinstance(entry, dict):
                continue
            for key in ("definition", "checklist_section", "expected_outcome"):
                if not _nonempty(entry.get(key)):
                    errors.append(f"variants.{name}.{key}: required")

    rows = checklist.get("rows")
    if not isinstance(rows, list) or not rows:
        errors.append("rows: required non-empty array")
        return errors
    seen: set[str] = set()
    for index, row in enumerate(rows):
        prefix = f"rows[{index}]"
        if not isinstance(row, dict):
            errors.append(f"{prefix}: expected object")
            continue
        row_id = row.get("id")
        if not _nonempty(row_id):
            errors.append(f"{prefix}.id: required")
        elif row_id in seen:
            errors.append(f"{prefix}.id: duplicate {row_id!r}")
        else:
            seen.add(row_id)
            prefix = f"row {row_id}"
        for key in ("criterion", "source"):
            if not _nonempty(row.get(key)):
                errors.append(f"{prefix}.{key}: required")
        if not _nonempty(row.get("required_evidence")):
            errors.append(f"{prefix}.required_evidence: required")
        if row.get("variant") not in VARIANTS:
            errors.append(
                f"{prefix}.variant: must be one of {list(VARIANTS)!r}, "
                f"got {row.get('variant')!r}"
            )
        revision_config = row.get("revision_config")
        if not isinstance(revision_config, dict) or not revision_config:
            errors.append(f"{prefix}.revision_config: required")
        else:
            for key in REVISION_KEYS:
                if not _nonempty(revision_config.get(key)):
                    errors.append(f"{prefix}.revision_config.{key}: required")
        identities = row.get("identities")
        if not isinstance(identities, dict):
            errors.append(f"{prefix}.identities: required")
        else:
            for key in IDENTITY_KEYS:
                if key not in identities:
                    errors.append(f"{prefix}.identities.{key}: required")
                elif not isinstance(identities[key], list):
                    errors.append(f"{prefix}.identities.{key}: expected array")
    return errors


def _markdown_row_ids(path: Path) -> set[str]:
    ids: set[str] = set()
    for line in path.read_text().splitlines():
        if not line.startswith("|"):
            continue
        first = line.split("|", 2)[1].strip().strip("`")
        if first.startswith(("S-", "A-", "R-")):
            ids.add(first)
    return ids


def validate_markdown(root: Path, checklist: dict) -> list[str]:
    """Check that every documented S1 row has a variant in the fixture."""
    errors: list[str] = []
    readme = (root / DOC_FILES["materials-readme"]).read_text()
    for variant in VARIANTS:
        if variant.lower() not in readme.lower():
            errors.append(f"docs/s1/README.md: variant {variant!r} is not defined")
    checklist_doc = (root / DOC_FILES["evidence-checklist-doc"]).read_text()
    for heading in ("Success variant", "Attention variant", "Recovery variant"):
        if heading.lower() not in checklist_doc.lower():
            errors.append(f"docs/s1/evidence-checklist.md: missing {heading!r}")

    documented = _markdown_row_ids(root / DOC_FILES["evidence-checklist-doc"])
    rows = checklist.get("rows", [])
    fixture_ids = {row.get("id") for row in rows if isinstance(row, dict)}
    missing = sorted(documented - fixture_ids)
    if missing:
        errors.append(
            "docs/s1/evidence-checklist.md: rows without a fixture variant: "
            + ", ".join(missing)
        )
    unassigned = sorted(
        str(row.get("id"))
        for row in rows
        if isinstance(row, dict) and row.get("variant") not in VARIANTS
    )
    if unassigned:
        errors.append("checklist rows without a variant: " + ", ".join(unassigned))
    return errors


def verify(root: Path, install_spec: Path | None = None) -> dict:
    """Run every check and return a machine-readable report."""
    root = root.resolve()
    errors: list[str] = []
    checked: list[str] = []

    paths = {name: root / relative for name, relative in FIXTURE_FILES.items()}
    for name, path in paths.items():
        if not path.is_file():
            errors.append(f"{name}: fixture file missing at {path.relative_to(root)}")
        else:
            checked.append(str(path.relative_to(root)))
    for name, relative in DOC_FILES.items():
        path = root / relative
        if not path.is_file():
            errors.append(f"{name}: document missing at {relative}")

    try:
        spec_path = install_spec if install_spec is not None else paths["installation-spec"]
        if not spec_path.is_file():
            errors.append(f"installation-spec: missing at {spec_path}")
            spec = {}
        else:
            spec = _load_json(spec_path)
            spec_errors = validate_install_spec(spec)
            errors.extend(f"installation-spec: {item}" for item in spec_errors)
            checked.append(str(spec_path))

        seed = _load_json(paths["seeded-issue"]) if paths["seeded-issue"].is_file() else {}
        errors.extend(f"seeded-issue: {item}" for item in validate_seeded_issue(seed))

        checklist = (
            _load_json(paths["evidence-checklist"])
            if paths["evidence-checklist"].is_file()
            else {}
        )
        errors.extend(
            f"evidence-checklist: {item}" for item in validate_checklist(checklist)
        )
        if all((root / relative).is_file() for relative in DOC_FILES.values()):
            errors.extend(validate_markdown(root, checklist))
    except CheckFailure as exc:
        errors.extend(exc.errors)

    return {
        "status": "ok" if not errors else "failed",
        "live_execution": "not-run (materials preflight only)",
        "checked_files": checked,
        "errors": errors,
    }


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--root",
        type=Path,
        default=Path(__file__).resolve().parents[2],
        help="Repository root (defaults to this checkout).",
    )
    parser.add_argument(
        "--install-spec",
        type=Path,
        help="Install-spec fixture to check (defaults to fixtures/s1/installation-spec.json).",
    )
    args = parser.parse_args(argv)
    try:
        report = verify(args.root, args.install_spec)
    except OSError as exc:
        report = {
            "status": "failed",
            "live_execution": "not-run (materials preflight only)",
            "checked_files": [],
            "errors": [str(exc)],
        }
    print(json.dumps(report, indent=2, sort_keys=True))
    return 0 if report["status"] == "ok" else 1


if __name__ == "__main__":
    raise SystemExit(main())
