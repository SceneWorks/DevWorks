"""AR-04..08 SQLite/reference-model probes, not production service acceptance. No inference."""
import argparse
import hashlib
import json
import sqlite3
import tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
p = argparse.ArgumentParser()
p.add_argument('--output', type=Path, required=True)
args = p.parse_args()
checks = []

def ok(name, condition):
    assert condition, name
    checks.append(name)

with tempfile.TemporaryDirectory(prefix='devworks-interview-') as tmp:
    path = Path(tmp) / 'fixture.sqlite'
    db = sqlite3.connect(path, isolation_level=None)
    db.executescript((ROOT / 'docs/architecture/persistence-schema.sql').read_text())
    db.execute("INSERT INTO projects VALUES('p','Product','Active',1,1,0)")
    db.execute("INSERT INTO repositories VALUES('repo','fixture',1)")
    for identity, kind in [('f1','feature'),('f2','feature'),('i','issue')]:
        db.execute('INSERT INTO work_items VALUES(?,?,?,NULL,NULL,NULL,?,0,0,1,1,0,?)',
                   (identity,'p',kind,identity,'{}'))
    db.execute("INSERT INTO root_controls VALUES('i','Paused',1,1,NULL)")
    # Reference mutation exercises the actual authoritative column and revision CAS.
    def member(feature, expected, clear=False):
        db.execute('BEGIN IMMEDIATE')
        try:
            changed = db.execute('UPDATE work_items SET parent_feature_id=?,revision=revision+1 WHERE id=? AND revision=?',
                                 (feature,'i',expected)).rowcount
            if not changed:
                raise ValueError('stale membership')
            if clear:
                db.execute("UPDATE root_controls SET desired_state='Paused',revision=revision+1 WHERE root_id='i'")
            db.execute('COMMIT')
        except Exception:
            db.execute('ROLLBACK')
            raise
    member('f1',1)
    ok('linked issue retains identity and preexisting pause', db.execute("SELECT id,parent_feature_id FROM work_items WHERE id='i'").fetchone()==('i','f1') and db.execute("SELECT desired_state FROM root_controls WHERE root_id='i'").fetchone()[0]=='Paused')
    member('f2',2)
    ok('direct transfer retains existing pause and one work identity', db.execute("SELECT count(*) FROM work_items WHERE id='i'").fetchone()[0]==1 and db.execute("SELECT parent_feature_id FROM work_items WHERE id='i'").fetchone()[0]=='f2')
    try:
        member('f1',2)
    except ValueError:
        ok('stale membership cannot overwrite current owner',db.execute("SELECT parent_feature_id FROM work_items WHERE id='i'").fetchone()[0]=='f2')
    else:
        raise AssertionError('stale membership accepted')
    member(None,3,clear=True)
    member('f1',4)
    ok('unlink pause survives relinking',db.execute("SELECT desired_state FROM root_controls WHERE root_id='i'").fetchone()[0]=='Paused')
    try:
        db.execute("UPDATE work_items SET associated_feature_id='f2' WHERE id='i'")
    except sqlite3.IntegrityError:
        checks.append('obsolete secondary association cannot compete with authoritative membership')
    else:
        raise AssertionError('second feature authority accepted')
    db.execute("INSERT INTO github_issue_intake VALUES('repo',1,1,'Queued',NULL,NULL,'{}',NULL)")
    ok('incoming shared-repository report can wait without a projectless task',db.execute('SELECT project_id,work_id FROM github_issue_intake').fetchone()==(None,None))
    try:
        db.execute("INSERT INTO github_issue_intake VALUES('repo',1,1,'Queued',NULL,NULL,'{}',NULL)")
    except sqlite3.IntegrityError:
        checks.append('duplicate GitHub observation cannot create a second intake identity')
    else:
        raise AssertionError('duplicate intake accepted')
    db.execute('INSERT INTO github_intake_clock(singleton,revision,next_admission_at) VALUES(1,1,0)')
    other = sqlite3.connect(path,isolation_level=None)
    def admit(connection,now):
        return connection.execute('UPDATE github_intake_clock SET revision=revision+1,last_admitted_at=?,next_admission_at=?+interval_seconds*1000 WHERE singleton=1 AND next_admission_at<=?',(now,now,now)).rowcount==1
    ok('default triage interval is thirty minutes',db.execute('SELECT interval_seconds FROM github_intake_clock').fetchone()[0]==1800)
    ok('competing app-wide consumers admit exactly one issue',admit(db,100) and not admit(other,100))
    ok('another repository/project cannot bypass clock',not admit(other,1800099))
    ok('next interval admits one issue without accumulated credits',admit(db,10000000) and not admit(other,10000000))
    other.close(); db.close()
    db=sqlite3.connect(path,isolation_level=None)
    ok('restart retains pacing deadline',not admit(db,10000001))
    db.execute('UPDATE github_intake_clock SET interval_seconds=60,next_admission_at=last_admitted_at+60000 WHERE singleton=1')
    ok('interval change permits one admission and backward clock permits none',admit(db,10060000) and not admit(db,10060000) and not admit(db,1))
    db.execute("INSERT INTO actors VALUES('host','Host','Host')")
    db.execute("INSERT INTO events(id,actor_id,aggregate_kind,aggregate_id,aggregate_revision,kind,created_at,payload_json) VALUES('event','host','Package','pkg',1,'Changed',0,'{}')")
    db.execute("INSERT INTO package_handler_state VALUES('pkg',1,1,'{\"n\":0}')")
    db.execute("INSERT INTO consumer_progress VALUES('pkg:sub','partition',0,1)")
    def commit_handler(digest='digest',fail=None):
        db.execute('BEGIN IMMEDIATE')
        try:
            replay=db.execute("SELECT payload_digest,receipt_id FROM package_handler_commits WHERE package_id='pkg' AND subscription_id='sub' AND event_id='event'").fetchone()
            if replay:
                if replay[0]!=digest:raise ValueError('changed replay')
                db.execute('COMMIT'); return replay[1]
            assert db.execute("SELECT revision FROM package_handler_state WHERE package_id='pkg'").fetchone()[0]==1
            assert db.execute("SELECT applied_seq FROM consumer_progress WHERE consumer='pkg:sub'").fetchone()[0]==0
            db.execute("UPDATE package_handler_state SET revision=2,state_json='{\"n\":1}' WHERE package_id='pkg' AND revision=1")
            if fail=='state':raise RuntimeError('crash after state proposal')
            db.execute("INSERT INTO operations VALUES('effect',NULL,NULL,'fixture.action',1,'args','{}','Queued',1,0)")
            if fail=='intent':raise RuntimeError('crash after effect intent')
            db.execute("INSERT INTO package_handler_commits VALUES('pkg','sub','event','receipt',?,1,2,1,'[\"effect\"]')",(digest,))
            db.execute("UPDATE consumer_progress SET applied_seq=1,revision=revision+1 WHERE consumer='pkg:sub'")
            if fail=='checkpoint':raise RuntimeError('crash before commit')
            db.execute('COMMIT')
        except Exception:
            db.execute('ROLLBACK'); raise
        if fail=='reply':raise RuntimeError('lost reply after commit')
        return 'receipt'
    for point in ['state','intent','checkpoint']:
        try:commit_handler(fail=point)
        except RuntimeError:pass
        ok('atomic rollback at '+point,db.execute('SELECT revision FROM package_handler_state').fetchone()[0]==1 and db.execute('SELECT count(*) FROM operations').fetchone()[0]==0 and db.execute('SELECT count(*) FROM package_handler_commits').fetchone()[0]==0 and db.execute('SELECT applied_seq FROM consumer_progress').fetchone()[0]==0)
    try:commit_handler(fail='reply')
    except RuntimeError:pass
    ok('lost reply reuses committed receipt and one effect intent',commit_handler()=='receipt' and db.execute('SELECT count(*) FROM operations').fetchone()[0]==1 and db.execute('SELECT revision FROM package_handler_state').fetchone()[0]==2)
    try:commit_handler(digest='changed')
    except ValueError:checks.append('changed event replay conflicts without altering checkpoint')
    else:raise AssertionError('changed replay accepted')
    ok('checkpoint and receipt remain aligned',db.execute('SELECT applied_seq FROM consumer_progress').fetchone()[0]==1 and db.execute('SELECT checkpoint FROM package_handler_commits').fetchone()[0]==1)
    db.execute("INSERT INTO review_source_evidence VALUES('repo',1,'head-a',1,0,'source-a','run-a','Failed','effect','{}')")
    try:
        db.execute("INSERT INTO review_source_evidence VALUES('repo',1,'head-a',1,0,'source-b','run-b','TimedOut','effect','{}')")
    except sqlite3.IntegrityError:
        checks.append('duplicate observations cannot admit a second fallback for the same head/policy/cycle')
    else:
        raise AssertionError('duplicate fallback accepted')
    db.execute("INSERT INTO review_source_evidence VALUES('repo',1,'head-b',1,0,'source-a','run-c','Requested',NULL,'{}')")
    ok('changed-head review keeps old evidence separate',db.execute('SELECT count(*) FROM review_source_evidence').fetchone()[0]==2)
    ok('fixture foreign keys remain valid',not list(db.execute('PRAGMA foreign_key_check')))
    db.close()
# Declarative resolution examples: product-contract model only, not a native resolver.
def resolve(repo,path,sources,user=None,enforced=None):
    effective={'test':'role-default'}
    for directory in ['']+['/'.join(path.split('/')[:i]) for i in range(1,len(path.split('/')))]:
        for filename in ['CLAUDE.md','AGENTS.md']:
            effective.update(sources.get((repo,directory,filename),{}))
    effective.update(user or {})
    effective.update(enforced or {})
    return effective
sources={('a','','AGENTS.md'):{'test':'root-agents','style':'root'},('a','','CLAUDE.md'):{'test':'root-claude','extra':'keep'},('a','src','CLAUDE.md'):{'test':'deep-claude'},('a','src','AGENTS.md'):{'style':'deep-agents'},('b','','AGENTS.md'):{'test':'other-repo'}}
ok('same directory AGENTS wins while CLAUDE additions survive',resolve('a','README.md',sources)=={'test':'root-agents','style':'root','extra':'keep'})
ok('nearer CLAUDE overrides ancestor AGENTS within subtree',resolve('a','src/main.rs',sources)['test']=='deep-claude')
ok('nested AGENTS retains nonconflicting ancestor guidance',resolve('a','src/main.rs',sources)['style']=='deep-agents' and resolve('a','src/main.rs',sources)['extra']=='keep')
ok('sibling subtree does not receive nested rules',resolve('a','tests/check.rs',sources)['test']=='root-agents')
ok('repository bundles remain isolated',resolve('b','src/main.rs',sources)['test']=='other-repo')
ok('user directions and enforced permissions outrank repository/default rules',resolve('a','src/main.rs',sources,{'test':'user'},{'writes':'denied'})['test']=='user' and resolve('a','src/main.rs',sources,{'writes':'allowed'},{'writes':'denied'})['writes']=='denied')
report={'date':'2026-11-02','allPassed':True,'checks':checks,'count':len(checks),'noInference':True,'scope':'Real SQLite fixture and scripted reference transitions plus declarative instruction examples; not native services, actual provider/process crashes or full implementation acceptance.','schemaSha256':hashlib.sha256((ROOT/'docs/architecture/persistence-schema.sql').read_bytes()).hexdigest()}
args.output.mkdir(parents=True,exist_ok=True)
(args.output/'sqlite-reference-model-results.json').write_text(json.dumps(report,indent=2)+'\n')
print(json.dumps({'checks':len(checks),'allPassed':True,'noInference':True}))
