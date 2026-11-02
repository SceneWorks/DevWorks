"""AR-03 SQLite contract-model fixture; no native work service or inference."""
import argparse
import hashlib
import json
import sqlite3
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
SCHEMA = ROOT / 'docs/architecture/persistence-schema.sql'
parser = argparse.ArgumentParser()
parser.add_argument('--output', type=Path, required=True)
args = parser.parse_args()
db = sqlite3.connect(':memory:', isolation_level=None)
db.executescript(SCHEMA.read_text())
checks = []
for project in ['p', 'q']:
    db.execute('INSERT INTO projects VALUES(?,?,?,1,1,1)', (project, project, 'Active'))
for identity, project, kind, parent, priority, rank in [
    ('f1', 'p', 'feature', None, 0, 0), ('f2', 'p', 'feature', None, 0, 0),
    ('a', 'p', 'task', 'f1', 1, 0), ('e', 'p', 'task', 'f1', 2, 1),
    ('b', 'p', 'task', 'f2', 9, 0), ('i', 'p', 'issue', None, 5, 0),
    ('independent', 'p', 'issue', None, 1, 1), ('foreign', 'q', 'task', None, 1, 0)]:
    db.execute('INSERT INTO work_items VALUES(?,?,?,?,NULL,NULL,?,?,?,1,1,1,?)',
               (identity, project, kind, parent, identity, priority, rank, '{}'))
for identity in ['f1', 'f2', 'i', 'independent']:
    db.execute("INSERT INTO root_controls VALUES(?,'Enabled',1,1,NULL)", (identity,))

def replace_links(target, prerequisites, expected):
    db.execute('BEGIN IMMEDIATE')
    try:
        project, revision = db.execute('SELECT project_id,revision FROM work_items WHERE id=?', (target,)).fetchone()
        if revision != expected:
            raise ValueError('stale task revision')
        db.execute('DELETE FROM work_dependencies WHERE work_id=?', (target,))
        for prerequisite in prerequisites:
            found = db.execute('SELECT project_id FROM work_items WHERE id=?', (prerequisite,)).fetchone()
            if not found or found[0] != project:
                raise ValueError('wrong project or unknown prerequisite')
            db.execute("INSERT INTO work_dependencies VALUES(?,?,'VerifiedAccepted')", (target, prerequisite))
        cycle = db.execute('''WITH RECURSIVE reachable(id) AS (
            SELECT prerequisite_id FROM work_dependencies WHERE work_id=?
            UNION SELECT d.prerequisite_id FROM work_dependencies d JOIN reachable r ON d.work_id=r.id
        ) SELECT 1 FROM reachable WHERE id=? LIMIT 1''', (target, target)).fetchone()
        if cycle:
            raise ValueError('cycle')
        db.execute('UPDATE work_items SET revision=revision+1 WHERE id=?', (target,))
        db.execute('COMMIT')
    except Exception:
        db.execute('ROLLBACK')
        raise

def expect_refusal(name, target, prerequisites, revision):
    before = list(db.execute('SELECT * FROM work_dependencies ORDER BY work_id,prerequisite_id'))
    try:
        replace_links(target, prerequisites, revision)
    except (ValueError, sqlite3.IntegrityError):
        pass
    else:
        raise AssertionError(name)
    assert list(db.execute('SELECT * FROM work_dependencies ORDER BY work_id,prerequisite_id')) == before
    checks.append(name)

def eligible(identity):
    return db.execute('''SELECT w.id FROM work_items w
        LEFT JOIN root_controls c ON c.root_id=coalesce(w.parent_feature_id,w.id)
        WHERE w.id=? AND coalesce(c.desired_state,'Enabled')='Enabled'
        AND NOT EXISTS(SELECT 1 FROM work_dependencies d JOIN work_items p ON p.id=d.prerequisite_id
            WHERE d.work_id=w.id AND coalesce(json_extract(p.brief_json,'$.verifiedAccepted'),0)<>1)''', (identity,)).fetchone() is not None

replace_links('b', ['a'], 1)
replace_links('i', ['b'], 1)
checks.append('cross-feature and standalone links share one project DAG')
assert not eligible('b') and not eligible('i') and eligible('independent')
checks.append('blocked high-priority work waits while independent work stays eligible')
expect_refusal('cycle refused with prior edge set intact', 'a', ['i'], 1)
expect_refusal('self-link refused', 'a', ['a'], 1)
expect_refusal('duplicate link refused atomically', 'b', ['a', 'a'], 2)
expect_refusal('cross-project link refused atomically', 'b', ['foreign'], 2)
expect_refusal('stale dependency edit refused', 'b', [], 1)
# Two independently prepared edits against different task revisions still recheck
# the current graph under the writer; the second must not close a cycle.
replace_links('a', ['e'], 1)
expect_refusal('second concurrent-intent link cannot close a cycle', 'e', ['a'], 1)

before_control = list(db.execute('SELECT * FROM root_controls'))
db.execute('BEGIN IMMEDIATE')
owner = db.execute("UPDATE work_items SET revision=revision+1 WHERE id='f1' AND revision=1")
assert owner.rowcount == 1
for rank, identity in enumerate(['a', 'e']):
    db.execute('UPDATE work_items SET priority=7,manual_order=?,revision=revision+1 WHERE id=?', (rank, identity))
db.execute('COMMIT')
assert [r[0] for r in db.execute("SELECT id FROM work_items WHERE parent_feature_id='f1' ORDER BY priority DESC,manual_order,id")] == ['a', 'e']
assert db.execute("UPDATE work_items SET revision=revision+1 WHERE id='f1' AND revision=1").rowcount == 0
checks.append('bulk priority and manual order are fenced by one queue-owner revision')
assert list(db.execute('SELECT * FROM root_controls')) == before_control
checks.append('priority changes preserve execution controls')

db.execute("UPDATE work_items SET brief_json=? WHERE id='a'", ('{"manualDone":true}',))
assert not eligible('b')
checks.append('manual Done does not satisfy verified-acceptance dependency')
db.execute("UPDATE work_items SET brief_json=? WHERE id='a'", ('{"verifiedAccepted":true}',))
assert eligible('b') and not eligible('i')
checks.append('accepted prerequisite makes only its eligible dependents ready')
db.execute("UPDATE root_controls SET desired_state='Paused' WHERE root_id='i'")
replace_links('i', [], 2)
assert not eligible('i')
checks.append('removing a blocker does not clear manual pause')
assert not list(db.execute('PRAGMA foreign_key_check'))
report = {'date': '2026-11-02', 'scope': 'Actual SQLite with scripted queue/link/acceptance policy; not production scheduler, authority or verified evidence qualification',
          'checks': checks, 'passed': True, 'noInference': True,
          'schemaSha256': hashlib.sha256(SCHEMA.read_bytes()).hexdigest()}
args.output.parent.mkdir(parents=True, exist_ok=True)
args.output.write_text(json.dumps(report, indent=2) + '\n')
print(json.dumps({'checks': len(checks), 'passed': True}))
