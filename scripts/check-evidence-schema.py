#!/usr/bin/env python3
"""Check the documented review SQL fragment, not the implemented store schema."""

import re
import sqlite3
from pathlib import Path


def main():
    document = Path(__file__).resolve().parents[1] / 'docs/evidence-gates.md'
    fragments = re.findall(r'```sql\n(.*?)\n```', document.read_text(), re.DOTALL)
    if len(fragments) != 1:
        raise ValueError('expected one review schema fragment')
    db = sqlite3.connect(':memory:')
    db.execute('PRAGMA foreign_keys = ON')
    db.execute('CREATE TABLE assessment_subjects (id TEXT PRIMARY KEY)')
    db.executescript(fragments[0])
    db.executemany('INSERT INTO assessment_subjects VALUES (?)', [('s1',), ('s2',)])
    db.executemany('INSERT INTO review_rounds VALUES (?, ?, ?, ?)',
                   [('r1', 's1', 'set-a', 1), ('r2', 's1', 'set-b', 1)])
    db.executemany('INSERT INTO source_snapshots VALUES (?, ?, ?, ?, ?, ?, ?)', [
        ('partial', 's1', 'review', 'rev1', 'c1', 'partial', 'body-p'),
        ('complete', 's1', 'review', 'rev1', 'c2', 'complete', 'body-a'),
        ('next', 's1', 'review', 'rev2', 'c3', 'complete', 'body-b'),
        ('foreign', 's2', 'review', 'rev1', 'c1', 'complete', 'body-x'),
    ])

    def rejects(sql, values):
        try:
            db.execute(sql, values)
        except sqlite3.IntegrityError:
            return
        raise AssertionError(f'expected constraint rejection: {sql}')

    rejects('INSERT INTO review_round_sources VALUES (?, ?, ?, ?)',
            ('r1', 's1', 'review', 'foreign'))
    rejects('INSERT INTO review_round_sources VALUES (?, ?, ?, ?)',
            ('r1', 's1', 'other-source', 'complete'))
    db.executemany('INSERT INTO review_round_sources VALUES (?, ?, ?, ?)',
                   [('r1', 's1', 'review', 'complete'),
                    ('r2', 's1', 'review', 'next')])
    db.executemany('INSERT INTO review_occurrences VALUES (?, ?, ?, ?)',
                   [('r1', 'complete', 'a', 'native-a'),
                    ('r2', 'next', 'b', 'native-b')])
    db.execute('INSERT INTO round_judgments VALUES (?, ?, ?, ?, ?)',
               ('j1', 'r1', 'set-a', 'operator', 'decision-a'))
    rejects('INSERT INTO judgment_decisions VALUES (?, ?, ?, ?)',
            ('j1', 'r2', 'b', 'dismiss'))
    rejects('INSERT INTO judgment_decisions VALUES (?, ?, ?, ?)',
            ('j1', 'r1', 'missing', 'dismiss'))
    db.execute('INSERT INTO judgment_decisions VALUES (?, ?, ?, ?)',
               ('j1', 'r1', 'a', 'dismiss'))
    rejects('INSERT INTO judgment_decisions VALUES (?, ?, ?, ?)',
            ('j1', 'r1', 'a', 'accept'))
    rejects('INSERT INTO source_snapshots VALUES (?, ?, ?, ?, ?, ?, ?)',
            ('replayed-c2', 's1', 'review', 'rev1', 'c2', 'complete', 'conflict'))
    snapshots = db.execute('SELECT collection_id, completeness FROM source_snapshots '
                           'WHERE subject_id = ? AND source_revision = ? ORDER BY collection_id',
                           ('s1', 'rev1')).fetchall()
    assert snapshots == [('c1', 'partial'), ('c2', 'complete')], snapshots
    print('Review SQL fragment: 6 constraint rejections and same-revision recollection passed.')
    print('Scope: membership and uniqueness with stub subjects; not runtime, exact-set, '
          'immutability, recovery, or live acceptance proof.')


if __name__ == '__main__':
    main()
