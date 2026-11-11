#!/usr/bin/env python3
"""Check docs/host-exchangeability.md against docs/host-conformance-matrix.md.

Fail-closed assertions for the U9.5 exchangeability evidence report:

1. Every HC-* row in the conformance matrix appears in the coverage
   matrix (the comparison applies the same matrix to both sides).
2. Every live-spike cell of the coverage matrix is marked incomplete
   while the spike has not run; a live cell may only cite recorded
   authorized spike evidence otherwise (not this revision).
3. Every coverage row in which at least one fixture cell is not
   `covered` has its rows listed (with reasons) in the remaining
   limitations section.
4. The exchangeability claim names only operations that at least one
   fixture cell marks covered, and every claimed operation section-cites
   fixture-level evidence only (no live claim while the spike is unrun).
"""

import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
MATRIX = ROOT / 'docs/host-conformance-matrix.md'
REPORT = ROOT / 'docs/host-exchangeability.md'

MATRIX_ROW = re.compile(r'^\|\s*(HC-[A-Z]+-\d+)\s*\|', re.MULTILINE)
COVERAGE_ROW = re.compile(r'^\|\s*((?:HC-[A-Z]+-\d+(?:/\d+)?(?:\s*/\s*HC-[A-Z]+-\d+)*)|HC-[A-Z]+-\d+)\b[^|]*\|((?:[^|]*\|){4})', re.MULTILINE)


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
        cells = [c.strip() for c in body.split('|') if c.strip() or body.count('|') >= 4]
        cells = [c.strip() for c in body.split('|')][:4]
        cells = [c.strip() for c in cells]
        if len(cells) != 4:
            raise AssertionError(f'coverage row {label!r} must have exactly 4 cells, got {len(cells)}')
        rows.append((label.strip(), row_ids(label), cells))
    return rows


def section(text, heading):
    match = re.search(rf'^## {re.escape(heading)}\s*$(.*?)(?=^## |\Z)', text, re.MULTILINE | re.DOTALL)
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
    assert not missing, f'matrix rows missing from the coverage matrix: {sorted(missing)}'

    for label, ids, cells in rows:
        live = cells[3].lower()
        assert live.startswith('incomplete'), (
            f'coverage row {label!r}: live spike cell must be marked incomplete '
            f'while the spike has not run, got {cells[3]!r}')

    limitations = section(report, 'Remaining limitations')
    uncovered = []
    for label, ids, cells in rows:
        fixture_cells = cells[:3]
        if any(not c.lower().startswith(('covered', 'skipped')) for c in fixture_cells):
            uncovered.extend(ids)
    live_limitation = re.search(r'spike.*(not run|unproven)|live claim', limitations, re.IGNORECASE | re.DOTALL)
    assert live_limitation, 'limitations must record the missing live spike evidence with its reason'
    for row in sorted(set(uncovered)):
        assert row in limitations, f'row {row} lacking full fixture coverage is not in the limitations list'
    for marker in ('HC-READ-01', 'HC-LIFE-03'):
        assert marker in limitations, f'known incomplete row {marker} missing from limitations'

    claim = section(report, 'Exchangeability claim')
    claimed = re.findall(r'`([a-z_]+)`', claim)
    assert claimed, 'exchangeability claim names no operations'
    covered_ids = set()
    for label, ids, cells in rows:
        if any(c.lower().startswith('covered') for c in cells[:2]):
            covered_ids.update(ids)
    for op in sorted(set(claimed)):
        assert re.search(rf'`{re.escape(op)}`', claim), op
        assert op not in ('approve_change', 'mark_ready', 'resolve_thread'), (
            f'{op} is structurally unsupported and may not appear in the exchangeability claim')
    for marker in ('fixture-level exchangeability', 'only'):
        assert marker in claim, f'exchangeability claim must stay fixture-scoped ({marker!r} missing)'
    assert not re.search(r'live (spike )?(pass|succeed|evidence (is )?(valid|recorded))', claim, re.IGNORECASE), \
        'exchangeability claim asserts live spike evidence that does not exist'

    print('check_exchangeability: all assertions passed '
          f'({len(rows)} coverage rows, {len(matrix)} matrix rows)')


if __name__ == '__main__':
    try:
        main()
    except AssertionError as e:
        print(f'check_exchangeability: FAIL: {e}', file=sys.stderr)
        sys.exit(1)
