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
import hashlib
import json
import re
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
REQUIRED_CAPABILITIES = (
    "repository_read",
    "branch_read",
    "change_request_read",
    "push_ref",
    "change_request_create",
    "change_request_update",
    "comment_write",
    "issue_read",
    "checks_read",
    "review_findings_read",
)
REQUIRED_CHECKS = ("verify",)
BUDGET_KEYS = ("max_invocations", "max_review_rounds", "runtime_limit_seconds")
PLACEHOLDER_MARKERS = (
    "recorded-from",
    "recorded-at",
    "recorded from",
    "recorded at",
    "to-be-recorded",
    "placeholder",
    "changeme",
    "tbd",
    "todo",
    "unresolved",
    "pending",
    "unknown",
    "unset",
    "not-set",
)
PLACEHOLDER_TOKENS = {
    "a", "app", "available", "bar", "baz", "be", "bound", "change",
    "changeme", "config", "configuration", "decide", "decided", "dummy",
    "empty", "example", "fake", "fill", "fixture", "foo", "from", "host",
    "id", "identifier", "identity", "installation", "later", "listing",
    "me", "missing", "n", "na", "name", "nil", "none", "not", "null",
    "omitted", "open", "owner", "pending", "placeholder", "preflight",
    "provided", "r", "read", "recorded", "redacted", "repo", "replace",
    "replacement", "repository", "required", "sample", "set", "spec",
    "stable", "store", "tbd", "test", "to", "todo", "unassigned",
    "undecided", "unknown", "unresolved", "unset", "uuid", "value", "x",
    "xxx",
}
REVISION_KEYS = (
    "materials_revision",
    "taisan_core_revision",
    "taisan_cli_revision",
    "github_adapter_revision",
    "worker_adapter_revision",
    "configuration_identity",
    "contract_version",
)
SHA_REVISION_KEYS = (
    "taisan_core_revision",
    "taisan_cli_revision",
    "github_adapter_revision",
    "worker_adapter_revision",
)
EVIDENCE_DOCUMENT = "docs/evidence/u8.1.md"
INLINE_DIGEST_PATTERN = re.compile(
    r"fixture content digest[ \t]+`?([0-9a-f]{64})`?", re.IGNORECASE
)
TABLE_DIGEST_PATTERN = re.compile(
    r"fixture content digest[ \t]*\|[ \t]*`?([0-9a-f]{64})`?", re.IGNORECASE
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


def _is_placeholder(value: str) -> bool:
    """Return true for any value that reads as placeholder text.

    A resolved binding identity is an opaque machine value, not a prose
    description of one. Besides the explicit marker phrases, a value made
    up entirely of generic identity words and separators (``app-id``,
    ``N/A``, ``replace_me``, ``not available``) is placeholder text even
    when it carries no known marker.
    """
    lowered = value.strip().lower()
    if not lowered:
        return True
    if "<" in lowered or ">" in lowered:
        return True
    if any(marker in lowered for marker in PLACEHOLDER_MARKERS):
        return True
    tokens = [token for token in re.split(r"[^a-z0-9]+", lowered) if token]
    if not tokens:
        return True
    return all(token in PLACEHOLDER_TOKENS for token in tokens)


def _identity_state(value) -> tuple[str, str]:
    """Classify a binding identity as resolved, unresolved, or invalid.

    An unresolved identity is a declared reference with an explicit
    resolution step, not a binding value. It keeps the preflight honest:
    preparation can carry it, but the installation is never certified
    complete until the live preflight records the real value.
    """
    if isinstance(value, dict):
        if (
            value.get("status") == "unresolved"
            and _nonempty(value.get("resolution"))
            and _nonempty(value.get("resolved_by"))
        ):
            return "unresolved", f"{value['resolution']} ({value['resolved_by']})"
        return (
            "invalid",
            "unresolved identities require status 'unresolved', resolution, and resolved_by",
        )
    if not isinstance(value, str) or not value.strip():
        return "invalid", "required"
    if _is_placeholder(value):
        return (
            "invalid",
            "must be a resolved identity or an explicit unresolved reference; "
            "placeholder text is not an identity",
        )
    return "resolved", value


def install_spec_unresolved(spec: dict) -> list[str]:
    """Return the binding items the live preflight must still resolve."""
    unresolved: list[str] = []
    repository = spec.get("repository")
    reviewer = spec.get("reviewer_identity")
    config = spec.get("configuration_identity")
    for prefix, value in (
        ("repository.stable_id", repository.get("stable_id") if isinstance(repository, dict) else None),
        ("reviewer_identity.app_id", reviewer.get("app_id") if isinstance(reviewer, dict) else None),
        ("configuration_identity.store_id", config.get("store_id") if isinstance(config, dict) else None),
    ):
        state, detail = _identity_state(value)
        if state == "unresolved":
            unresolved.append(f"{prefix}: {detail}")

    status = spec.get("implementation_status")
    target = status.get("target_profile") if isinstance(status, dict) else None
    if not isinstance(target, dict):
        unresolved.append(
            "implementation_status.target_profile: missing; target availability is unverified"
        )
        return unresolved
    if target.get("adapter_available") is not True:
        unresolved.append(
            "implementation_status.target_profile.adapter: target adapter is not available in this tree"
        )
    if target.get("reviewer_app_available") is not True:
        unresolved.append(
            "implementation_status.target_profile.reviewer_app: target reviewer App does not exist yet"
        )
    return unresolved


def fixture_content_digest(root: Path) -> str:
    """Return the sha256 over the fixture files in lexical path order."""
    digest = hashlib.sha256()
    for relative in sorted(FIXTURE_FILES.values()):
        digest.update((root / relative).read_bytes())
    return digest.hexdigest()


def configuration_identity_label(spec: dict) -> str:
    """Return the exact pre-execution configuration identity.

    The label binds the concrete declared installation fields plus a
    canonical digest, so a capability, check, effect, or budget change
    cannot leave a checklist row pointing at stale config. The store
    identity is deliberately not pinned: it is a late-bound binding
    resolved by the live preflight and recorded in execution evidence,
    and embedding its unresolved reference here would make every
    prepared checklist row stale as soon as the store is resolved.
    """
    config = spec.get("configuration_identity")
    if not isinstance(config, dict):
        return ""
    declared = {key: value for key, value in config.items() if key != "store_id"}
    worker = spec.get("worker_runtime_identity")
    effects = worker.get("allowed_effects") if isinstance(worker, dict) else None
    if isinstance(effects, dict):
        effects_label = ",".join(
            f"{key}={str(value).lower()}" for key, value in sorted(effects.items())
        )
    else:
        effects_label = "missing"
    canonical = json.dumps(
        {"configuration_identity": declared, "allowed_effects": effects},
        sort_keys=True,
        separators=(",", ":"),
    )
    config_digest = hashlib.sha256(canonical.encode()).hexdigest()
    return (
        f"{config.get('host_instance')}/{config.get('repository')} "
        f"adapter={config.get('adapter')} "
        f"contract={config.get('contract_version')} "
        f"store=late-bound effects={effects_label} "
        f"config_sha256={config_digest}"
    )


def expected_revision_config(spec: dict) -> dict:
    """Return the exact revision/config values every checklist row must carry."""
    revisions = spec.get("revisions")
    revisions = revisions if isinstance(revisions, dict) else {}
    config = spec.get("configuration_identity")
    config = config if isinstance(config, dict) else {}
    return {
        "materials_revision": revisions.get("materials"),
        "taisan_core_revision": revisions.get("taisan_core"),
        "taisan_cli_revision": revisions.get("taisan_cli"),
        "github_adapter_revision": revisions.get("github_adapter"),
        "worker_adapter_revision": revisions.get("worker_adapter"),
        "configuration_identity": configuration_identity_label(spec),
        "contract_version": config.get("contract_version"),
    }


def validate_checklist_revision_config(checklist: dict, spec: dict) -> list[str]:
    """Check every checklist row against the install-spec revision/config."""
    errors: list[str] = []
    expected = expected_revision_config(spec)
    if not isinstance(checklist, dict):
        return errors
    if checklist.get("revision_config") != expected:
        errors.append(
            "revision_config: does not match the exact installation-spec "
            "revision/config"
        )
    rows = checklist.get("rows")
    if not isinstance(rows, list):
        return errors
    for index, row in enumerate(rows):
        if not isinstance(row, dict):
            continue
        if row.get("revision_config") != expected:
            errors.append(
                f"rows[{index}].revision_config: does not match the exact "
                "installation-spec revision/config"
            )
    return errors


def _records_rehearsal_value(text: str, key: str, value: str) -> bool:
    """Return true only when the evidence records the exact value for *key*.

    Plain substring matching is not sufficient: ``default`` occurs inside
    unrelated prose such as ``defaults``. Each key therefore requires its
    recorded form (a keyed assignment or backticked token) rather than an
    arbitrary occurrence anywhere in the document.
    """
    token = re.escape(value)
    if key == "run_tag":
        pattern = rf"`{token}`"
    elif key == "credential_reference":
        pattern = rf"credential[ _]ref(?:erence)?\s*[:=]?\s*`{token}`"
    elif key == "adapter":
        pattern = rf"adapter\s*[:=]?\s*`?{token}`?(?![A-Za-z0-9_.-])"
    else:
        pattern = rf"(?<![A-Za-z0-9_.-]){token}(?![A-Za-z0-9_.-])"
    return re.search(pattern, text) is not None


def validate_fixture_digest(root: Path) -> tuple[str, list[str]]:
    """Compute the fixture digest and check it against the evidence record."""
    digest = fixture_content_digest(root)
    document = root / EVIDENCE_DOCUMENT
    if not document.is_file():
        return digest, [
            f"{EVIDENCE_DOCUMENT}: missing required fixture content digest record"
        ]
    text = document.read_text()
    errors: list[str] = []
    for label, pattern in (
        ("inline", INLINE_DIGEST_PATTERN),
        ("table-row", TABLE_DIGEST_PATTERN),
    ):
        recorded = set(pattern.findall(text))
        if not recorded:
            errors.append(
                f"{EVIDENCE_DOCUMENT}: missing {label} fixture content digest"
            )
        elif recorded != {digest}:
            errors.append(
                f"{EVIDENCE_DOCUMENT}: recorded {label} fixture content digest "
                f"{', '.join(sorted(recorded))} does not match computed {digest}"
            )
    return digest, errors


def _contained_document(root: Path, relative: str) -> Path | None:
    """Resolve *relative* under *root*, rejecting paths that escape it.

    The install spec is reviewable input, so its evidence path must never
    make this read-only preflight stat or read a file outside the
    checkout. Absolute paths, ``..`` segments, and symlinks that resolve
    outside the root are all rejected.
    """
    candidate = Path(relative)
    if candidate.is_absolute():
        return None
    resolved_root = root.resolve()
    resolved = (resolved_root / candidate).resolve()
    if not resolved.is_relative_to(resolved_root):
        return None
    return resolved


def validate_rehearsals(root: Path, spec: dict) -> list[str]:
    """Cross-check the recorded rehearsal profile against its evidence files."""
    errors: list[str] = []
    status = spec.get("implementation_status")
    if not isinstance(status, dict):
        return ["implementation_status: missing object"]
    target = status.get("target_profile")
    if not isinstance(target, dict):
        errors.append("implementation_status.target_profile: missing object")
    rehearsals = status.get("retained_rehearsals")
    if not isinstance(rehearsals, list) or not rehearsals:
        errors.append(
            "implementation_status.retained_rehearsals: required non-empty array"
        )
        return errors
    for index, rehearsal in enumerate(rehearsals):
        prefix = f"implementation_status.retained_rehearsals[{index}]"
        if not isinstance(rehearsal, dict):
            errors.append(f"{prefix}: expected object")
            continue
        for key in (
            "evidence",
            "run_tag",
            "adapter",
            "reviewer_actor",
            "credential_reference",
            "classification",
        ):
            if not _nonempty(rehearsal.get(key)):
                errors.append(f"{prefix}.{key}: required")
        if rehearsal.get("repository_host_write") is not True:
            errors.append(f"{prefix}.repository_host_write: expected true")
        evidence = rehearsal.get("evidence")
        if not isinstance(evidence, str) or not evidence:
            continue
        document = _contained_document(root, evidence)
        if document is None:
            errors.append(
                f"{prefix}.evidence: must stay within the repository, "
                f"got {evidence!r}"
            )
            continue
        if not document.is_file():
            errors.append(f"{prefix}.evidence: missing {evidence}")
            continue
        text = document.read_text()
        for key in ("run_tag", "adapter", "reviewer_actor", "credential_reference"):
            value = rehearsal.get(key)
            if (
                isinstance(value, str)
                and value
                and not _records_rehearsal_value(text, key, value)
            ):
                errors.append(
                    f"{prefix}.{key}: {value!r} is not recorded exactly in {evidence}"
                )
        if (
            rehearsal.get("repository_host_write") is True
            and "host_write: true" not in text
        ):
            errors.append(
                f"{prefix}.repository_host_write: true is not recorded in {evidence}"
            )
    return errors


def validate_install_spec(spec: dict) -> list[str]:
    """Return every hard install-spec failure.

    An empty list means the declared fields are structurally and
    semantically valid; unresolved binding identities are reported
    separately by :func:`install_spec_unresolved` and never certify the
    installation as complete.
    """
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
        state, detail = _identity_state(repository.get("stable_id"))
        if state == "invalid":
            errors.append(f"repository.stable_id: {detail}")

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
        state, detail = _identity_state(reviewer.get("app_id"))
        if state == "invalid":
            errors.append(f"reviewer_identity.app_id: {detail}")
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
        else:
            for key, expected in (
                ("workspace_write", True),
                ("local_git_write", True),
                ("repository_host_write", False),
            ):
                if effects.get(key) is not expected:
                    errors.append(
                        "worker_runtime_identity.allowed_effects."
                        f"{key}: must be {str(expected).lower()}"
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
        for key in ("host_instance", "repository", "adapter", "contract_version"):
            if not _nonempty(config.get(key)):
                errors.append(f"configuration_identity.{key}: required")
        if config.get("repository") != S1_REPOSITORY:
            errors.append(
                f"configuration_identity.repository: expected {S1_REPOSITORY!r}"
            )
        if config.get("host_instance") != "github.com":
            errors.append("configuration_identity.host_instance: expected github.com")
        state, detail = _identity_state(config.get("store_id"))
        if state == "invalid":
            errors.append(f"configuration_identity.store_id: {detail}")

        capabilities = config.get("capabilities")
        if not isinstance(capabilities, list) or not capabilities:
            errors.append(
                "configuration_identity.capabilities: required non-empty array"
            )
        else:
            string_capabilities = [
                item
                for item in capabilities
                if isinstance(item, str) and item.strip()
            ]
            if len(string_capabilities) != len(capabilities):
                errors.append(
                    "configuration_identity.capabilities: entries must be non-empty strings"
                )
            missing = sorted(set(REQUIRED_CAPABILITIES) - set(string_capabilities))
            if missing:
                errors.append(
                    "configuration_identity.capabilities: missing required S1 "
                    f"capabilities {', '.join(missing)}"
                )

        required_checks = config.get("required_checks")
        if not isinstance(required_checks, list) or required_checks != list(
            REQUIRED_CHECKS
        ):
            errors.append(
                "configuration_identity.required_checks: expected exactly "
                f"{list(REQUIRED_CHECKS)!r}, got {required_checks!r}"
            )

        budget = config.get("budget")
        if not isinstance(budget, dict):
            errors.append("configuration_identity.budget: required object")
        else:
            for key in BUDGET_KEYS:
                value = budget.get(key)
                if isinstance(value, bool) or not isinstance(value, int) or value <= 0:
                    errors.append(
                        f"configuration_identity.budget.{key}: expected a positive "
                        f"integer, got {value!r}"
                    )

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
    title_template = seed.get("title_template")
    if (
        not isinstance(title_template, str)
        or not title_template.strip()
        or "<run-tag>" not in title_template
    ):
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
        success = variants.get("success")
        success_outcome = (
            success.get("expected_outcome") if isinstance(success, dict) else None
        )
        attention = variants.get("Attention")
        attention_outcome = (
            attention.get("expected_outcome") if isinstance(attention, dict) else None
        )
        if (
            not isinstance(success_outcome, dict)
            or success_outcome.get("work_state") != "succeeded"
        ):
            errors.append(
                "variants.success.expected_outcome.work_state: expected succeeded"
            )
        if (
            not isinstance(attention_outcome, dict)
            or attention_outcome.get("work_state") != "paused"
        ):
            errors.append(
                "variants.Attention.expected_outcome.work_state: expected paused"
            )
    return errors


def _check_variants(prefix: str, variants, errors: list[str]) -> None:
    if not isinstance(variants, dict):
        errors.append(f"{prefix}: missing object")
        return
    if set(variants) != set(VARIANTS):
        errors.append(
            f"{prefix}: expected exactly {list(VARIANTS)!r}, got {sorted(variants)!r}"
        )


def _validate_revision_config(
    prefix: str, revision_config, errors: list[str]
) -> None:
    """Validate one exact revision/config object, rejecting reference text."""
    if not isinstance(revision_config, dict) or not revision_config:
        errors.append(f"{prefix}: required")
        return
    for key in REVISION_KEYS:
        value = revision_config.get(key)
        if not _nonempty(value):
            errors.append(f"{prefix}.{key}: required")
            continue
        if key in SHA_REVISION_KEYS:
            if not isinstance(value, str) or not re.fullmatch(r"[0-9a-f]{40}", value):
                errors.append(
                    f"{prefix}.{key}: expected an exact "
                    f"40-hex commit SHA, got {value!r}"
                )
        elif key == "configuration_identity":
            if not isinstance(value, str):
                errors.append(f"{prefix}.{key}: expected a string, got {value!r}")
                continue
            lowered = value.lower()
            if "installation-spec" in lowered or "exact sha" in lowered:
                errors.append(
                    f"{prefix}.{key}: expected the exact "
                    f"configuration identity, not a reference ({value!r})"
                )
            elif "unresolved:" in lowered:
                errors.append(
                    f"{prefix}.{key}: must not pin a late-bound unresolved "
                    f"identity ({value!r})"
                )
        elif key == "contract_version":
            if isinstance(value, bool) or not isinstance(value, int) or value <= 0:
                errors.append(
                    f"{prefix}.{key}: expected a positive "
                    f"integer, got {value!r}"
                )
        elif not isinstance(value, str):
            errors.append(f"{prefix}.{key}: expected a string, got {value!r}")


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

    _validate_revision_config("revision_config", checklist.get("revision_config"), errors)

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
        if not isinstance(row_id, str) or not row_id.strip():
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
        _validate_revision_config(
            f"{prefix}.revision_config", row.get("revision_config"), errors
        )
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
    rows = checklist.get("rows")
    rows = rows if isinstance(rows, list) else []
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
    spec: dict = {}
    resolved_spec = False

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

    fixture_digest: str | None = None
    if all(path.is_file() for path in paths.values()):
        try:
            fixture_digest, digest_errors = validate_fixture_digest(root)
            errors.extend(digest_errors)
        except OSError as exc:
            errors.append(f"fixture-content-digest: unreadable ({exc})")

    try:
        spec_path = install_spec if install_spec is not None else paths["installation-spec"]
        if not spec_path.is_file():
            errors.append(f"installation-spec: missing at {spec_path}")
        else:
            spec = _load_json(spec_path)
            spec_errors = validate_install_spec(spec)
            errors.extend(f"installation-spec: {item}" for item in spec_errors)
            errors.extend(
                f"installation-spec: {item}"
                for item in validate_rehearsals(root, spec)
            )
            checked.append(str(spec_path))
            resolved_spec = True

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
        if resolved_spec:
            errors.extend(
                f"evidence-checklist: {item}"
                for item in validate_checklist_revision_config(checklist, spec)
            )
        if all((root / relative).is_file() for relative in DOC_FILES.values()):
            errors.extend(validate_markdown(root, checklist))
    except CheckFailure as exc:
        errors.extend(exc.errors)
    except (TypeError, AttributeError, KeyError) as exc:
        errors.append(f"preflight: malformed input type ({exc})")

    unresolved = install_spec_unresolved(spec) if resolved_spec else []
    status = "failed" if errors else ("prepared" if unresolved else "ok")
    return {
        "status": status,
        "installation_ready": status == "ok",
        "unresolved_identities": unresolved,
        "live_execution": "not-run (materials preflight only)",
        "fixture_content_digest": fixture_digest,
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
    except (OSError, TypeError, AttributeError, KeyError) as exc:
        report = {
            "status": "failed",
            "installation_ready": False,
            "unresolved_identities": [],
            "live_execution": "not-run (materials preflight only)",
            "fixture_content_digest": None,
            "checked_files": [],
            "errors": [str(exc)],
        }
    print(json.dumps(report, indent=2, sort_keys=True))
    return 1 if report["status"] == "failed" else 0


if __name__ == "__main__":
    raise SystemExit(main())
