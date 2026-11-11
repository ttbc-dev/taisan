#!/usr/bin/env python3
"""Check docs/s1/acceptance-report.md against the S1 checklist.

Fail-closed assertions for the U8.5 acceptance report:

1. Every S1 criterion × variant row of the checklist appears exactly
   once, with a status cell that is `valid`, `absent` or `invalid`;
   absent/invalid cells carry a reason; valid cells reference a claim.
2. Every referenced claim exists, is complete (revision SHA, config
   digest, Work/Execution/Invocation/action identities, remote receipts
   PR/head/check/review, stop proof), and any null field is paired with
   an explicit reason — never silently absent.
3. The report contains no U6 (alternate admission) or U9 (host
   exchangeability) evidence rows or claims; references to them appear
   only in the prose boundary notes, never in the table or the claims.
"""

import json
import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
REPORT = ROOT / 'docs/s1/acceptance-report.md'
CHECKLIST = ROOT / 'docs/s1/evidence-checklist.md'
CHECKLIST_ROW = re.compile(r'^\|\s*(S-\d+a?|A-\d+|R-\d+)\s*\|', re.MULTILINE)

ROW = re.compile(r'^\|\s*(S-\d+a?|A-\d+|R-\d+)\s*\|([^|]+)\|([^|]+)\|([^|]+)\|\s*$', re.MULTILINE)
CLAIM_HEADING = re.compile(r'^###\s*(C-\d+)\s*\(([^,]+),\s*([a-z]+)\)\s*$', re.MULTILINE)
VALID = re.compile(r'^valid\s*[—-]\s*(C-\d+)\s*$')
MARKER = re.compile(r'^(absent|invalid)\s*[—-]\s*(.{10,})$', re.DOTALL)


def fail(msg):
    print(f'check_s1_acceptance: FAIL: {msg}', file=sys.stderr)
    sys.exit(1)


def expected_rows():
    # The checklist is the source of truth: a criterion added to it must
    # also appear in the report (and vice versa). Never hardcode the set,
    # or both sides could omit a new row without failing.
    try:
        ids = CHECKLIST_ROW.findall(CHECKLIST.read_text())
    except FileNotFoundError:
        fail(f'checklist {CHECKLIST} not found')
    seen = []
    for row_id in ids:
        if row_id not in seen:
            seen.append(row_id)
    if not seen:
        fail(f'checklist {CHECKLIST} defines no S1 rows')
    return seen


def main():
    text = REPORT.read_text()
    expected = expected_rows()

    rows = {}
    for match in ROW.finditer(text):
        row_id, criterion, variant, result = (g.strip() for g in match.groups())
        if row_id in rows:
            fail(f'duplicate row {row_id} — each S1 criterion must appear exactly once')
        rows[row_id] = (criterion, variant, result)
    for row_id in expected:
        if row_id not in rows:
            fail(f'missing S1 row {row_id}')
    extra = sorted(set(rows) - set(expected))
    if extra:
        fail(f'unexpected rows in the report table: {extra}')

    for row_id, (criterion, variant, result) in rows.items():
        if row_id.startswith('S') and variant != 'success':
            fail(f'{row_id}: variant {variant!r} != success')
        if row_id.startswith('A') and variant != 'attention':
            fail(f'{row_id}: variant {variant!r} != attention')
        if row_id.startswith('R') and variant != 'recovery':
            fail(f'{row_id}: variant {variant!r} != recovery')
        valid = VALID.match(result)
        if valid:
            continue
        marker = MARKER.match(result)
        if not marker:
            fail(f'{row_id}: result cell must be `valid — C-nn` or `absent|invalid — <reason ≥ 10 chars>`, got {result!r}')

    headings = {}
    for match in CLAIM_HEADING.finditer(text):
        claim_id, _criterion, variant = match.groups()
        headings[claim_id] = variant

    referenced = set()
    row_to_claims = {}
    for row_id, (_c, _v, result) in rows.items():
        valid = VALID.match(result)
        if valid:
            referenced.add(valid.group(1))
            row_to_claims[row_id] = valid.group(1)
    for claim_id in sorted(referenced):
        if claim_id not in headings:
            fail(f'referenced claim {claim_id} has no claim block')
    for claim_id in sorted(set(headings) - referenced):
        fail(f'claim {claim_id} is never referenced by a valid cell')

    fences = re.findall(r'###\s*C-\d+[^\n]*\n```json\n(.*?)```', text, re.DOTALL)
    if len(fences) != len(headings):
        fail(f'expected {len(headings)} claim json fences, found {len(fences)}')
    claims = [json.loads(f) for f in fences]

    for claim in claims:
        cid = claim.get('id', '?')
        for key in ('id', 'criterion', 'variant', 'status', 'source', 'revision', 'config_digest'):
            if not isinstance(claim.get(key), str) or not claim[key].strip():
                fail(f'{cid}: missing or empty required key {key!r}')
        row = rows.get(claim['criterion'])
        if not row:
            fail(f'{cid}: criterion {claim["criterion"]!r} not in the report table')
        if row_to_claims.get(claim['criterion']) != cid:
            fail(f'{cid}: criterion {claim["criterion"]!r} is not the citing row of this claim '
                 f'(the cell must cite the claim for its own row: {row_to_claims.get(claim["criterion"])!r})')
        if row[1] != claim['variant']:
            fail(f'{cid}: variant {claim["variant"]!r} does not match the row variant {row[1]!r}')
        if claim['status'] != 'valid':
            fail(f'{cid}: claims exist only for valid cells, status {claim["status"]!r}')
        if not re.fullmatch(r'[0-9a-f]{7,40}', claim['revision']):
            fail(f'{cid}: revision {claim["revision"]!r} is not a commit SHA')
        if not re.fullmatch(r'sha256:[0-9a-f]{64}', claim['config_digest']):
            fail(f'{cid}: config_digest {claim["config_digest"]!r} is not sha256:<64 hex>')
        src = ROOT / claim['source'].split('#')[0]
        if not src.exists():
            fail(f'{cid}: source {claim["source"]!r} does not exist in the repo')
        fragment = claim['source'].split('#', 1)[1] if '#' in claim['source'] else ''
        if fragment:
            def slug(heading):
                s = heading.strip().lower()
                s = re.sub(r'[^\w\s-]', '', s, flags=re.UNICODE)
                return re.sub(r'\s+', '-', s)
            headings = [slug(l[3:]) for l in src.read_text().splitlines() if l.startswith('## ') or l.startswith('### ')]
            if fragment not in headings:
                fail(f'{cid}: source fragment #{fragment} matches no heading in {src.relative_to(ROOT)} '
                     f'(headings: {headings})')

        ids = claim.get('identities') or {}
        if not isinstance(ids.get('work'), str) or not ids['work'].strip():
            fail(f'{cid}: identities.work must be non-empty')
        for key in ('executions', 'invocations', 'actions', 'commands'):
            if not isinstance(ids.get(key), list):
                fail(f'{cid}: identities.{key} must be an array')
        if not ids['executions']:
            fail(f'{cid}: identities.executions must be non-empty')

        receipts = claim.get('remote_receipts') or {}
        for key in ('pr', 'head', 'checks', 'review'):
            if key not in receipts:
                fail(f'{cid}: remote_receipts.{key} missing entirely (absence must be explicit null + reason)')
        pr = receipts.get('pr')
        if pr is None:
            reason = receipts.get('pr_reason')
            if not isinstance(reason, str) or len(reason.strip()) < 20:
                fail(f'{cid}: remote_receipts.pr is null without a pr_reason of ≥ 20 chars')
        elif not isinstance(pr, str) or '#' not in pr:
            fail(f'{cid}: remote_receipts.pr {pr!r} is not a PR reference')

        def resolve(container, key, label):
            value = container.get(key)
            if value is not None:
                return value
            reason = container.get(f'{key}_reason')
            if not isinstance(reason, str) or len(reason.strip()) < 20:
                fail(f'{cid}: {label} is null without a reason of ≥ 20 chars')
            return None

        head = resolve(receipts, 'head', 'remote_receipts.head')
        if head is not None:
            if not isinstance(head, str) or not re.fullmatch(r'[0-9a-f]{40}', head):
                fail(f'{cid}: remote_receipts.head must be a full git SHA, got {head!r}')

        review = resolve(receipts, 'review', 'remote_receipts.review')
        if review is not None:
            if not isinstance(review, dict):
                fail(f'{cid}: remote_receipts.review must be an object or null with a reason')
            for key in ('id', 'author', 'state'):
                if not isinstance(review.get(key), (str, int)) or str(review[key]).strip() == '':
                    fail(f'{cid}: remote_receipts.review.{key} missing or empty')

        checks = resolve(receipts, 'checks', 'remote_receipts.checks')
        if checks is not None:
            if not isinstance(checks, list):
                fail(f'{cid}: remote_receipts.checks must be an array or null with checks_reason')
            if not checks:
                reason = receipts.get('checks_reason')
                if not isinstance(reason, str) or len(reason.strip()) < 20:
                    fail(f'{cid}: empty remote_receipts.checks without a checks_reason of ≥ 20 chars')
            else:
                for i, check in enumerate(checks):
                    if not isinstance(check, dict):
                        fail(f'{cid}: remote_receipts.checks[{i}] must be an object')
                    for key in ('name', 'id', 'conclusion'):
                        if not isinstance(check.get(key), str) or not check[key].strip():
                            fail(f'{cid}: remote_receipts.checks[{i}].{key} missing or empty')
        stop_proof = resolve(claim, 'stop_proof', 'stop_proof')
        if stop_proof is not None:
            if not isinstance(stop_proof, str) or len(stop_proof.strip()) < 20:
                fail(f'{cid}: stop_proof must be a non-empty string of ≥ 20 chars when present')

    prose_and_table = '\n'.join(re.findall(r'^\|.*$', text, re.MULTILINE))
    claims_text = '\n'.join(fences)
    for match in re.finditer(r'\bU[69]\b', prose_and_table):
        fail('the boundary table contains a U6/U9 reference')
    for match in re.finditer(r'\bU[69]\b', claims_text):
        fail('a claim contains a U6/U9 reference (alternate admission / host proof are out of scope)')

    valid = sum(1 for _r in referenced)
    print(f'check_s1_acceptance: all assertions passed '
          f'({len(rows)} S1 rows, {len(claims)} valid claims, {len(rows) - valid} explicitly absent/invalid)')


if __name__ == '__main__':
    main()
