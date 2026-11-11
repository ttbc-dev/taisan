#!/usr/bin/env python3
"""Check docs/host-exchangeability.md against docs/host-conformance-matrix.md.

Fail-closed assertions for the U9.5 exchangeability evidence report:

1. Every HC-* row in the conformance matrix appears in the coverage
   matrix (the comparison applies the same matrix to both sides).
2. Every live-spike cell of the coverage matrix is marked incomplete
   unless a dedicated live-spike record
   (docs/evidence/u9.4-live-spike.md) carries an authorized run tag — a
   non-incomplete live cell is only legal with that authorized-spike
   record. The fixture-only U9.4 record is never treated as live proof.
3. Every coverage row in which at least one fixture cell is not
   `covered` has its rows listed (with reasons) in the remaining
   limitations section.
4. The exchangeability claim names only operations with a binding in
   the claim binding table, every bound row has both full-baseline
   fixtures `covered`, and the claim stays fixture-scoped (no live claim
   while the spike is unrun); structurally unsupported operations may
   never appear in the claim.
"""

import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
MATRIX = ROOT / 'docs/host-conformance-matrix.md'
REPORT = ROOT / 'docs/host-exchangeability.md'
SPIKE_EVIDENCE = ROOT / 'docs/evidence/u9.4-live-spike.md'
NEGATED_AUTH = re.compile(
    r'\b(?:(?:not|never|no|un)[ _-]+(?:authori[sz]ed|authori[sz]ation))\b')

MATRIX_ROW = re.compile(r'^\|\s*(HC-[A-Z]+-\d+)\s*\|', re.MULTILINE)


def matrix_rows():
    return sorted(set(MATRIX_ROW.findall(MATRIX.read_text())))


def row_ids(cell):
    ids = re.findall(r'HC-[A-Z]+-\d+', cell)
    expanded = []
    for prefix in re.findall(r'(HC-[A-Z]+)-(\d+)(?:/(\d+))?', cell):
        head, a, b = prefix
        expanded.append(f'{head}-{a}')
        if b:
            expanded.append(f'{head}-{b}')
    return sorted(set(expanded)) or sorted(set(ids))


def coverage_rows(text):
    rows = []
    for match in re.finditer(r'^\|\s*(HC-[^\|]+?)\s*\|((?:[^\n|]*\|){4})\s*$', text, re.MULTILINE):
        label, body = match.groups()
        cells = [c.strip() for c in body.split('|')][:4]
        cells = [c.strip() for c in cells]
        if len(cells) != 4:
            raise AssertionError(f'coverage row {label!r} must have exactly 4 cells, got {len(cells)}')
        rows.append((label.strip(), row_ids(label), cells))
    return rows


def section(text, heading):
    match = re.search(rf'^#{{2,4}}\s*{re.escape(heading)}\s*$(.*?)(?=^#{{2,}} |\Z)', text, re.MULTILINE | re.DOTALL)
    if not match:
        raise AssertionError(f'section {heading!r} not found')
    return match.group(1)


def main():
    matrix = set(matrix_rows())
    report = REPORT.read_text()
    rows = coverage_rows(report)
    if not rows:
        raise AssertionError('no coverage matrix rows found in the report')

    compared = set()
    for label, ids, cells in rows:
        compared.update(ids)
    missing = matrix - compared
    if missing:
        raise AssertionError(f'matrix rows missing from the coverage matrix: {sorted(missing)}')

    spike_text = SPIKE_EVIDENCE.read_text().lower() if SPIKE_EVIDENCE.exists() else ''
    # A live cell may leave `incomplete` only when a dedicated live-spike
    # record (never the fixture-only U9.4 record) carries a run tag and a
    # positive authorization assertion. Negations are matched as phrases
    # with either separator, so `not_authorized` fails as closed as
    # `not authorized`, while a record that merely mentions a past
    # unauthorized attempt is not a false negative.
    spike_ok = bool(spike_text) and bool(re.search(r'u94-[0-9t]+z?-r[0-9]+', spike_text)) \
        and bool(re.search(r'\bauthorized\b', spike_text)) \
        and not NEGATED_AUTH.search(spike_text)
    for label, ids, cells in rows:
        live = cells[3].lower()
        if re.match(r'^incomplete\s+(—|-)\s+spike not run', live):
            continue
        if not spike_ok:
            raise AssertionError(
                f'coverage row {label!r}: live spike cell must be `incomplete — spike not run` '
                f'(or carry an authorized U9.4 spike record with a run tag), got {cells[3]!r}')

    limitations = section(report, 'Remaining limitations')
    uncovered = []
    for label, ids, cells in rows:
        fixture_cells = cells[:3]
        if any(not c.lower().startswith('covered') for c in fixture_cells):
            uncovered.extend(ids)
    live_limitation = re.search(r'spike.*(not run|unproven)|live claim', limitations, re.IGNORECASE | re.DOTALL)
    if not live_limitation:
        raise AssertionError('limitations must record the missing live spike evidence with its reason')
    for row in sorted(set(uncovered)):
        if row not in limitations:
            raise AssertionError(f'row {row} lacking full fixture coverage is not in the limitations list')
    for marker in ('HC-READ-01', 'HC-LIFE-03'):
        if marker not in limitations:
            raise AssertionError(f'known incomplete row {marker} missing from limitations')

    claim = section(report, 'Exchangeability claim')
    binding = section(report, 'Claim binding (fixture evidence only — spike not run)')
    claimed = re.findall(r'`([a-z_]+)`', claim)
    if not claimed:
        raise AssertionError('exchangeability claim names no operations')
    unbacked = []
    bindings = []
    for op in sorted(set(claimed)):
        if op in ('approve_change', 'mark_ready', 'resolve_thread'):
            raise AssertionError(f'{op} is structurally unsupported and may not appear in the exchangeability claim')
        match = re.search(rf'^\|\s*`{re.escape(op)}`\s*\|([^|]+)\|', binding, re.MULTILINE)
        if not match:
            raise AssertionError(f'claimed operation `{op}` has no binding row in the claim binding table')
        bindings.append((f'`{op}`', match.group(1)))
    # The observations category is named in the claim as plain prose, not
    # a backticked operation; it must be bound and validated like the rest.
    observations = re.search(r'read\s*/\s*check\s*/\s*review\s+observations', claim)
    if observations:
        match = re.search(r'^\|\s*read\s*/\s*check\s*/\s*review observations\s*\|([^|]+)\|',
                          binding, re.MULTILINE)
        if not match:
            raise AssertionError('claimed read/check/review observations have no binding row')
        bindings.append(('read/check/review observations', match.group(1)))
    for op, cell in bindings:
        bound = row_ids(cell)
        if not bound:
            raise AssertionError(f'binding row for {op} names no HC rows')
        backed = []
        for row_id in bound:
            candidate = next((cells for label, ids, cells in rows if row_id in ids), None)
            if candidate is None:
                raise AssertionError(f'binding row for {op} names {row_id}, absent from the coverage matrix')
            if all(c.lower().startswith('covered') for c in candidate[:2]):
                backed.append(row_id)
        if len(backed) != len(bound):
            unbacked.append(f'{op} bound to {bound}, but not all are fixture-`covered` on both '
                            f'full-baseline fixtures: {backed}')
    if unbacked:
        raise AssertionError('; '.join(unbacked))
    for marker in ('fixture-level exchangeability', 'only'):
        if marker not in claim:
            raise AssertionError(f'exchangeability claim must stay fixture-scoped ({marker!r} missing)')
    if re.search(r'live (spike )?(pass|succeed|evidence (is )?(valid|recorded))', claim, re.IGNORECASE):
        raise AssertionError('exchangeability claim asserts live spike evidence that does not exist')

    print('check_exchangeability: all assertions passed '
          f'({len(rows)} coverage rows, {len(matrix)} matrix rows)')


if __name__ == '__main__':
    try:
        main()
    except AssertionError as e:
        print(f'check_exchangeability: FAIL: {e}', file=sys.stderr)
        sys.exit(1)
