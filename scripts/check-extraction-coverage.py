#!/usr/bin/env python3
"""Check extraction mapping coverage, never runtime correctness or source-review truth."""
from __future__ import annotations

import argparse
import json
import os
import subprocess
import re
from pathlib import Path

ROLES = {'ops', 'worker', 'selector', 'infra', 'target'}
FILE_STATES = {'reviewed', 'excluded', 'pending', 'inaccessible'}
DISPOSITIONS = {'retain', 're-model', 'omit', 'defer'}
AUDIT_START = '<!-- extraction-catalog:v1:start -->'
AUDIT_END = '<!-- extraction-catalog:v1:end -->'


def audit_catalog(text: str) -> dict:
    if text.count(AUDIT_START) != 1 or text.count(AUDIT_END) != 1:
        raise ValueError('expected one extraction catalog block in owner-controlled audit')
    block = text.split(AUDIT_START, 1)[1].split(AUDIT_END, 1)[0].strip()
    match = re.fullmatch(r'```json\s*\n(.*)\n```', block, re.S)
    if not match:
        raise ValueError('catalog block must contain one fenced JSON object')
    data = json.loads(match.group(1))
    if not isinstance(data, dict):
        raise ValueError('catalog must be an object')
    return data


def heading_ids(path: Path) -> set[str]:
    return {re.sub(r'[^\w\- ]', '', title.lower()).replace(' ', '-')
            for title in re.findall(r'^#+ (.*)', path.read_text(), re.M)}


def check(document: dict, catalog: dict, root: Path) -> dict:
    errors: list[dict] = []
    gaps: list[dict] = []

    def error(code: str, item: str, detail: str) -> None:
        errors.append({'code': code, 'item': item, 'detail': detail})

    def gap(code: str, item: str, detail: str) -> None:
        gaps.append({'code': code, 'item': item, 'detail': detail})

    def index(items: list, kind: str) -> dict:
        result = {}
        for entry in items:
            key = entry.get('id')
            if not isinstance(key, str) or not key:
                error('ID_MISSING', kind, 'nonempty stable id required')
            elif key in result:
                error('DUPLICATE_ID', key, kind)
            else:
                result[key] = entry
        return result

    for name, data, sections in [('manifest', document, ('sources','files','behaviors','requirements')), ('catalog', catalog, ('sources','files','behaviors','acceptance'))]:
        if not isinstance(data, dict):
            raise ValueError(name + ' must be an object')
        for section in sections:
            if not isinstance(data.get(section), list) or any(not isinstance(x, dict) for x in data[section]):
                raise ValueError(name + '.' + section + ' must be an array of objects')

    if document.get('schema_version') != 1:
        error('SCHEMA_VERSION', 'manifest', 'expected version 1')
    sources = {s.get('role'): s for s in document.get('sources', [])}
    private_sources = {s.get('role'): s for s in catalog.get('sources', [])}
    if set(sources) != ROLES or len(document.get('sources', [])) != len(ROLES):
        error('SOURCE_SET', 'sources', 'all five distinct role pins required')
    files = index(document.get('files', []), 'file')
    private_files = index(catalog.get('files', []), 'audit-file')
    behaviors = index(document.get('behaviors', []), 'behavior')
    private_behaviors = index(catalog.get('behaviors', []), 'audit-behavior')
    requirements = index(document.get('requirements', []), 'requirement')
    acceptance = index(catalog.get('acceptance', []), 'acceptance')
    if set(private_sources) != ROLES or len(catalog['sources']) != len(ROLES):
        error('AUDIT_SOURCE_SET', 'sources', 'all five distinct owner-controlled role pins required')
    totals = {role: {state: 0 for state in sorted(FILE_STATES)} for role in ROLES}

    if set(files) != set(private_files):
        error('FILE_SET', 'files', 'public and owner-controlled manifests differ')
    for role, source in sources.items():
        revision = source.get('revision', '')
        if not re.fullmatch(r'[0-9a-f]{40}', revision):
            error('REVISION', str(role), 'full immutable revision required')
        if private_sources.get(role, {}).get('revision') != revision:
            error('PIN_MISMATCH', str(role), 'audit/source revision differs')
        actual = {f['id'] for f in files.values() if f.get('role') == role}
        expected = set(source.get('file_ids', []))
        if actual != expected or len(expected) != len(source.get('file_ids', [])):
            error('ROLE_FILE_SET', str(role), 'missing/extra/duplicate file entry')
        if source.get('file_count') != len(actual):
            error('FILE_COUNT', str(role), 'pinned tree count differs from entries')

    for key, entry in files.items():
        role, state = entry.get('role'), entry.get('status')
        if role not in ROLES or state not in FILE_STATES:
            error('FILE_STATE', key, 'unknown role or unclassified entry')
            continue
        totals[role][state] += 1
        if not entry.get('rationale'):
            error('RATIONALE_MISSING', key, state)
        if entry.get('blob') != private_files.get(key, {}).get('blob'):
            error('BLOB_MISMATCH', key, 'audit blob differs')
        if role != private_files.get(key, {}).get('role'):
            error('AUDIT_FILE_ROLE', key, 'audit role differs')
        if not re.fullmatch(r'[0-9a-f]{40}', entry.get('blob', '')):
            error('BLOB_ID', key, 'immutable blob identity required')
        if any(field in entry for field in ('path', 'symbol', 'repository_path')):
            error('PRIVATE_LOCATOR', key, 'locators belong only in owner-controlled audit')
        if state in {'pending', 'inaccessible'}:
            gap('FILE_UNREVIEWED', key, state)
        if state == 'reviewed' and (entry.get('audit_evidence') != key or not private_files.get(key, {}).get('review_evidence')):
            error('REVIEW_EVIDENCE_MISSING', key, 'linked review scope/regions required in audit')
        for behavior in entry.get('behavior_ids', []):
            if behavior not in behaviors:
                error('BEHAVIOR_LINK_MISSING', key, str(behavior))

    if set(behaviors) != set(private_behaviors):
        error('BEHAVIOR_SET', 'behaviors', 'public/audit behavior sets differ')
    linked_behaviors = {behavior for entry in files.values()
                        for behavior in entry.get('behavior_ids', [])}
    required_details = ('trigger', 'preconditions', 'inputs_outputs', 'owners',
                        'durable_state', 'identities', 'transitions', 'failure_semantics',
                        'concurrency_restart', 'bounds', 'human_action', 'source_evidence',
                        'evidence_classification')
    for key, behavior in behaviors.items():
        if key not in linked_behaviors:
            error('BEHAVIOR_SOURCE_LINK_MISSING', key, 'behavior has no inventoried file link')
        detail = private_behaviors.get(key, {})
        for field in required_details:
            if field not in detail or detail[field] in (None, '', [], {}):
                error('BEHAVIOR_DETAIL_MISSING', key, field)
        if behavior.get('disposition') not in DISPOSITIONS:
            error('UNCLASSIFIED_BEHAVIOR', key, 'retain/re-model/omit/defer required')
        if not behavior.get('rationale'):
            error('RATIONALE_MISSING', key, 'behavior disposition')
        if behavior.get('disposition') == 'defer' and not behavior.get('owner_disposition'):
            gap('OWNER_DISPOSITION_PENDING', key, 'explicit deferral decision needed')
        refs = behavior.get('requirement_ids', [])
        if not refs:
            error('REQUIREMENT_LINK_MISSING', key, 'every disposition needs a requirement mapping')
        for req in refs:
            if req not in requirements:
                error('REQUIREMENT_LINK_MISSING', key, str(req))

    for required in (f'R{i:02}' for i in range(1, 39)):
        if required not in requirements:
            error('LEGACY_REQUIREMENT_MISSING', required, 'R01-R38 must remain enumerable')
    for key, requirement in requirements.items():
        if requirement.get('status') not in {'implemented+verified', 'implemented+unverified', 'missing', 'deferred'}:
            error('REQUIREMENT_STATUS', key, 'implementation/evidence status required')
        if requirement.get('status') == 'implemented+verified' and not requirement.get('current_evidence'):
            error('VERIFICATION_EVIDENCE_MISSING', key, 'revision/criterion-specific evidence required')
        if not requirement.get('planned_proof') or not requirement.get('implementation_units'):
            error('PROOF_OR_UNIT_MISSING', key, 'planned proof and implementation unit required')
        targets = requirement.get('targets', [])
        if not targets:
            error('TARGET_MISSING', key, 'specification/design owner/acceptance link required')
        for target in targets:
            path = (root / target.get('document', '')).resolve()
            if not path.is_relative_to(root.resolve()):
                error('SPECIFICATION_PATH', key, 'target document escapes repository')
                continue
            if target.get('owner_issue') not in range(50, 57):
                error('DESIGN_OWNER', key, 'design owner issue50-56 required')
            ac_id = target.get('acceptance_id')
            if ac_id not in acceptance:
                error('AC_LINK_MISSING', key, str(ac_id))
            elif acceptance[ac_id].get('issue') != target.get('owner_issue'):
                error('AC_OWNER_MISMATCH', key, str(ac_id))
            if target.get('state') == 'planned':
                gap('DESIGN_PENDING', key, str(target))
            elif target.get('state') != 'specified' or not path.is_file():
                error('SPECIFICATION_MISSING', key, str(path))
            elif target.get('anchor') not in heading_ids(path):
                error('SPECIFICATION_ANCHOR', key, str(target))
    return {'mapping_valid': not errors, 'complete': not errors and not gaps,
            'errors': errors, 'gaps': gaps, 'file_counts': totals,
            'behavior_count': len(behaviors), 'requirement_count': len(requirements),
            'scope': 'Mapping completeness only; source-review truth and runtime correctness require independent evidence.'}



def source_tree_gaps(document: dict, catalog: dict) -> list[dict]:
    """Optional owner-local verification; never fetch or alter source worktrees."""
    gaps = []
    sources = {s['role']: s for s in catalog['sources']}
    for role in sorted(ROLES):
        source = sources.get(role, {})
        revision = source.get('revision', '')
        if not re.fullmatch(r'[0-9a-f]{40}', revision):
            gaps.append({'code':'SOURCE_PIN_INVALID','item':role,'detail':'invalid immutable source pin'})
            continue
        try:
            result = subprocess.run(['git','--no-pager','-c','core.fsmonitor=false','-C',source['repository_path'],
                                     'ls-tree','-r','-z',revision], capture_output=True,
                                    env=os.environ | {'GIT_NO_LAZY_FETCH':'1','GIT_OPTIONAL_LOCKS':'0'},
                                    timeout=30, check=True)
            actual = {}
            for entry in filter(None, result.stdout.split(b'\x00')):
                meta, path = entry.split(b'\t',1)
                mode, kind, blob = meta.decode().split()
                actual[path.decode()] = (mode, kind, blob)
            expected = {f['path']:(f['mode'],f['kind'],f['blob']) for f in catalog['files'] if f['role']==role}
            if actual != expected:
                gaps.append({'code':'SOURCE_TREE_MISMATCH','item':role,'detail':'pinned tree entries differ from audit catalog'})
        except (OSError, KeyError, ValueError, subprocess.SubprocessError):
            gaps.append({'code':'SOURCE_TREE_UNAVAILABLE','item':role,'detail':'owner-local pinned source could not be read; no fetch attempted'})
    return gaps


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--manifest', default='docs/extraction-coverage.json')
    parser.add_argument('--root', type=Path, default=Path(__file__).resolve().parents[1])
    parser.add_argument('--verify-source-trees', action='store_true',
                        help='Verify owner-local pinned Git trees without network or checkout changes')
    parser.add_argument('--allow-pending-design', action='store_true',
                        help='Permit planned target specs, but never pending/inaccessible source reviews')
    args = parser.parse_args()
    try:
        document = json.loads((args.root / args.manifest).read_text())
        catalog = audit_catalog((args.root / 'docs/source-workflow-audit.md').read_text())
        result = check(document, catalog, args.root)
        if args.verify_source_trees:
            result['gaps'].extend(source_tree_gaps(document, catalog))
            result['complete'] = result['mapping_valid'] and not result['gaps']
    except (OSError, ValueError, KeyError, TypeError) as exc:
        print(json.dumps({'mapping_valid': False, 'error': str(exc)}))
        return 2
    print(json.dumps(result, ensure_ascii=False, indent=2))
    blocking = [g for g in result['gaps']
                if not args.allow_pending_design or g['code'] != 'DESIGN_PENDING']
    return 0 if not result['errors'] and not blocking else 1


if __name__ == '__main__':
    raise SystemExit(main())
