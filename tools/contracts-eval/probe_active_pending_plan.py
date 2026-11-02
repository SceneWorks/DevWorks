"""AR-02 real SQLite mechanics fixture, not a production plan service."""
import argparse
import hashlib
import json
import sqlite3
import tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
SCHEMA = ROOT / 'docs/architecture/persistence-schema.sql'
parser = argparse.ArgumentParser()
parser.add_argument('--output', type=Path, required=True)
args = parser.parse_args()
checks = []

with tempfile.TemporaryDirectory(prefix='devworks-plan-fixture-') as temporary:
    path = Path(temporary) / 'workspace.sqlite'
    db = sqlite3.connect(path, isolation_level=None)
    db.executescript(SCHEMA.read_text())
    db.execute("INSERT INTO actors VALUES('human','user','Fixture user')")
    db.execute("INSERT INTO projects VALUES('p','Project','Active',1,1,1)")
    db.execute("INSERT INTO work_items VALUES('f','p','feature',NULL,NULL,NULL,'Feature',1,0,1,1,1,'{}')")
    db.execute("INSERT INTO root_controls VALUES('f','Enabled',1,1,NULL)")
    db.execute("INSERT INTO feature_plans(feature_id,revision,approved_plan_json,approved_by,approved_at) VALUES('f',1,?, 'human',1)", ('{"scope":"original"}',))

    def active():
        return db.execute("SELECT approved_plan_json FROM feature_plans WHERE feature_id='f'").fetchone()[0]

    original = active()
    db.execute("UPDATE feature_plans SET pending_plan_json=?,pending_revision=2,revision=revision+1 WHERE feature_id='f'", ('{"scope":"pending"}',))
    assert active() == original
    checks.append('saving pending content preserves the approved plan')
    db.close()
    db = sqlite3.connect(path, isolation_level=None)
    db.execute('PRAGMA foreign_keys=ON')
    assert active() == original and db.execute('SELECT pending_revision FROM feature_plans').fetchone()[0] == 2
    checks.append('reopen retains approved and pending content separately')

    def proposal(identity, revision):
        db.execute("INSERT INTO operations VALUES(?, 'f', NULL, 'feature-plan',1,?, '{}','IntentRecorded',1,1)", (identity, str(revision)))
        db.execute("INSERT INTO approvals VALUES(?,?,?,?,'Pending',NULL,NULL,NULL)", (identity, identity, revision, str(revision)))
        db.execute("UPDATE feature_plans SET pending_proposal_id=? WHERE feature_id='f'", (identity,))

    proposal('proposal-2', 2)
    assert active() == original
    checks.append('requesting approval does not invalidate the original approval')
    db.execute("UPDATE approvals SET state='Rejected',decided_by='human',decided_at=2 WHERE id='proposal-2'")
    assert active() == original
    checks.append('rejection preserves the approved plan')
    db.execute("UPDATE feature_plans SET pending_plan_json=?,pending_revision=3,pending_proposal_id=NULL,revision=revision+1 WHERE feature_id='f'", ('{"scope":"latest"}',))
    stale = db.execute("UPDATE feature_plans SET approved_plan_json=pending_plan_json WHERE feature_id='f' AND pending_revision=2 AND pending_proposal_id='proposal-2'")
    assert stale.rowcount == 0 and active() == original
    checks.append('stale draft approval cannot replace the current plan')
    proposal('proposal-3', 3)

    def promote():
        db.execute("UPDATE approvals SET state='Approved',decided_by='human',decided_at=3 WHERE id='proposal-3' AND state='Pending'")
        result = db.execute("UPDATE feature_plans SET approved_plan_json=pending_plan_json,approved_by='human',approved_at=3,pending_plan_json=NULL,pending_revision=NULL,pending_proposal_id=NULL,revision=revision+1 WHERE feature_id='f' AND pending_revision=3 AND pending_proposal_id='proposal-3'")
        assert result.rowcount == 1
        db.execute("INSERT INTO events(id,actor_id,project_id,root_id,aggregate_kind,aggregate_id,aggregate_revision,kind,created_at,payload_json) VALUES('adopted','human','p','f','feature-plan','f',4,'PlanApproved',3,'{}')")

    db.execute('BEGIN IMMEDIATE')
    promote()
    db.execute('ROLLBACK')
    assert active() == original
    assert db.execute("SELECT state FROM approvals WHERE id='proposal-3'").fetchone()[0] == 'Pending'
    assert db.execute("SELECT count(*) FROM events WHERE id='adopted'").fetchone()[0] == 0
    checks.append('interrupted promotion rolls back plan, approval and event together')
    db.execute("UPDATE root_controls SET desired_state='Paused',revision=2 WHERE root_id='f'")
    db.execute('BEGIN IMMEDIATE')
    promote()
    db.execute('COMMIT')
    assert json.loads(active())['scope'] == 'latest'
    assert db.execute('SELECT pending_plan_json,pending_revision,pending_proposal_id FROM feature_plans').fetchone() == (None, None, None)
    checks.append('approval atomically replaces the plan and clears the one pending slot')
    assert db.execute("SELECT desired_state FROM root_controls WHERE root_id='f'").fetchone()[0] == 'Paused'
    checks.append('plan approval does not clear a manual pause')
    assert db.execute('SELECT count(*) FROM feature_plans').fetchone()[0] == 1
    checks.append('feature owns one plan-state row rather than a plan-version catalog')
    assert not list(db.execute('PRAGMA foreign_key_check'))
    db.close()

report = {'date': '2026-11-02', 'scope': 'SQLite design-fixture transaction mechanics only; human authority, actual plan adoption, work eligibility and native services remain implementation acceptance',
          'checks': checks, 'passed': True, 'noInference': True,
          'schemaSha256': hashlib.sha256(SCHEMA.read_bytes()).hexdigest()}
args.output.parent.mkdir(parents=True, exist_ok=True)
args.output.write_text(json.dumps(report, indent=2) + '\n')
print(json.dumps({'checks': len(checks), 'passed': True}))
