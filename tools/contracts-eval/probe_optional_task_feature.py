"""AR-01 SQLite design-fixture checks; no production service or inference."""
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
db = sqlite3.connect(':memory:')
db.executescript(SCHEMA.read_text())
db.execute("INSERT INTO projects VALUES('project-a','A','Active',1,1,1)")
db.execute("INSERT INTO projects VALUES('project-b','B','Active',1,1,1)")
checks = []

def insert(identity, kind, feature=None, project='project-a'):
    db.execute('INSERT INTO work_items VALUES(?,?,?,?,NULL,NULL,?,1,0,1,1,1,?)',
               (identity, project, kind, feature, identity, '{}'))

def rejects(name, identity, kind, feature=None, project='project-a'):
    db.execute('SAVEPOINT reject_case')
    try:
        insert(identity, kind, feature, project)
    except sqlite3.IntegrityError:
        checks.append(name)
    else:
        raise AssertionError(name)
    finally:
        db.execute('ROLLBACK TO reject_case')
        db.execute('RELEASE reject_case')

insert('feature-a', 'feature')
insert('task-a', 'task', 'feature-a')
checks.append('feature-linked task accepted')
insert('standalone-a', 'task')
checks.append('task without a feature accepted')
insert('issue-a', 'issue')
checks.append('standalone issue accepted')
rejects('project is still required', 'no-project', 'task', project=None)
rejects('unknown feature refused', 'missing-feature', 'task', 'missing')
rejects('cross-project feature refused', 'wrong-project', 'task', 'feature-a', 'project-b')
rejects('feature cannot become a child task', 'feature-child', 'feature', 'feature-a')
rejects('duplicate stable work identity refused', 'standalone-a', 'issue')
assert not list(db.execute('PRAGMA foreign_key_check'))
report = {
    'date': '2026-11-02',
    'scope': 'Real SQLite against the architecture DDL; host membership-kind, queue, '
             'authority, delegation and delivery policies are not implemented by this probe',
    'schemaSha256': hashlib.sha256(SCHEMA.read_bytes()).hexdigest(),
    'checks': checks,
    'passed': True,
    'noInference': True,
}
args.output.parent.mkdir(parents=True, exist_ok=True)
args.output.write_text(json.dumps(report, indent=2) + '\n')
print(json.dumps({'checks': len(checks), 'passed': True}))
