"""SQLite architecture contract model, NOT a production gateway or authority adapter.
Uses the initial SQL fixture and scripted host contexts. No effects or inference.
"""
from concurrent.futures import ThreadPoolExecutor
from contextlib import contextmanager
from copy import deepcopy
import hashlib
import json
from pathlib import Path
import sqlite3
import tempfile
import threading
import uuid
import rfc8785

ROOT=Path(__file__).resolve().parents[2]
SCHEMA=ROOT/'docs/architecture/persistence-schema.sql'
P='1.0-draft.1'
RESULTS=[]
def check(name,condition):
    if not condition:raise AssertionError(name)
    RESULTS.append(name)
def request(kind,key,revision='1',holds=None):
    body={'kind':'runs.'+kind,'rootId':'root-a','expectedControlRevision':revision}
    if kind=='start':body['expectedWorkRevision']='1'
    if holds is not None:body['coveredHolds']=holds
    return {'protocol':P,'requestKey':key,'command':body}
def fingerprint(req):
    body=deepcopy(req['command'])
    if 'coveredHolds' in body:body['coveredHolds'].sort(key=lambda h:h['holdId'])
    return hashlib.sha256(rfc8785.dumps({'protocol':req['protocol'],'command':body})).hexdigest()
class Model:
    def __init__(self,path,enabled):
        self.path=path
        with self.db() as d:
            d.executescript(SCHEMA.read_text())
            d.executescript("""
            INSERT INTO store_meta VALUES(1,'store-a',1,1,1,0);
            INSERT INTO app_boots VALUES('boot-a',1,NULL,'Open');
            INSERT INTO actors VALUES('user','user','Fixture user'),('agent','agent','Fixture agent');
            INSERT INTO projects VALUES('project-a','Fixture','Active',1,1,1);
            INSERT INTO work_items VALUES('root-a','project-a','feature',NULL,NULL,NULL,'Fixture',1,0,1,1,1,'{}');
            INSERT INTO plan_revisions VALUES('plan-a','root-a',1,'scope','acceptance','{}','user',1);
            INSERT INTO root_controls VALUES('root-a','Enabled',1,0,NULL);
            """)
            if not enabled:
                d.execute("UPDATE root_controls SET desired_state='Paused'")
                self.add_hold(d,'hold-human','HumanPause')
    def db(self):
        d=sqlite3.connect(self.path,isolation_level=None,timeout=5)
        d.row_factory=sqlite3.Row;d.execute('PRAGMA foreign_keys=ON');d.execute('PRAGMA journal_mode=WAL');d.execute('PRAGMA synchronous=FULL')
        return d
    def add_hold(self,d,hid,cause,assignment=None):
        d.execute("INSERT INTO blockers VALUES(?,'root-a',?,NULL,?,'fixture-origin',1,'Active',1,'{}')",(hid,assignment,cause))
    def holds(self,d):
        return list(d.execute("SELECT * FROM blockers WHERE root_id='root-a' AND state='Active' AND assignment_id IS NULL AND operation_id IS NULL ORDER BY id"))
    def summary(self,d):
        return {'items':[{'holdId':h['id'],'revision':str(h['revision']),'subject':{'kind':'root','id':'root-a'},'cause':h['cause']} for h in self.holds(d)],'hasMore':False}
    def control(self,d):
        r=d.execute("SELECT * FROM root_controls WHERE root_id='root-a'").fetchone()
        return {'rootId':'root-a','revision':str(r['revision']),'desiredState':r['desired_state']}
    def emit(self,d,key,kind,actor='user',delivery=False):
        eid='event-'+uuid.uuid4().hex
        rev=d.execute("SELECT revision FROM root_controls WHERE root_id='root-a'").fetchone()[0]
        seq=d.execute("INSERT INTO events VALUES(NULL,?,?,'project-a','root-a',NULL,'root','root-a',?,?,1,?)",(eid,actor,rev,kind,json.dumps({'commandKey':key,'protocol':P}))).lastrowid
        if delivery:d.execute("INSERT INTO queue_entries VALUES(?,'supervisor',?,'root-a',NULL,1,0,'Ready',NULL,NULL,0,0,0,'{}')",('delivery-'+eid,eid))
        return seq
    def grant(self,req):
        gid='grant-'+uuid.uuid4().hex
        binding={'requestKey':req['requestKey'],'rootId':'root-a','coveredHolds':req['command']['coveredHolds'],'allowedCauses':['HumanPause']}
        with self.db() as d:d.execute("INSERT INTO resume_grants VALUES(?,'agent','root-a',1,'Active','fixture-explicit-user-instruction',?,1000,1)",(gid,json.dumps(binding)))
        return gid
    def apply(self,req,actor='user',grant_id=None,epoch=0,fail=False,lose_ack=False):
        d=self.db(); key=req['requestKey'];body=req['command'];digest=fingerprint(req)
        try:
            d.execute('BEGIN IMMEDIATE')
            current_epoch=d.execute('SELECT restore_epoch FROM store_meta').fetchone()[0]
            if actor not in ['user','agent'] or epoch!=current_epoch:
                d.execute('ROLLBACK');return {'kind':'Rejected','code':'PermissionDenied'}
            scope=f'store-a:{epoch}:fixture-ingress'
            old=d.execute('SELECT * FROM command_receipts WHERE actor_id=? AND scope_key=? AND idempotency_key=?',(actor,scope,key)).fetchone()
            if old:
                d.execute('ROLLBACK')
                if old['payload_digest']!=digest:return {'kind':'Rejected','code':'IdempotencyConflict'}
                return {'kind':'Recorded','receipt':json.loads(old['result_json']),'replayed':True}
            control=self.control(d);expected=body['expectedControlRevision'];decision=None;activation=None;cleared=[]
            if expected!=control['revision']:
                decision={'kind':'RevisionConflict','subject':{'kind':'root','id':'root-a'},'revisionKind':'Control','expected':expected,'current':control['revision']}
            elif body['kind']=='runs.start' and body['expectedWorkRevision'] != str(d.execute("SELECT revision FROM work_items WHERE id='root-a'").fetchone()[0]):
                decision={'kind':'RevisionConflict','subject':{'kind':'root','id':'root-a'},'revisionKind':'Work','expected':body['expectedWorkRevision'],'current':str(d.execute("SELECT revision FROM work_items WHERE id='root-a'").fetchone()[0])}
            elif body['kind']=='runs.start':
                held=[h for h in self.holds(d) if h['cause'] in ['HumanPause','RestartContinue','BudgetContinue']]
                live=d.execute("SELECT * FROM executions WHERE root_id='root-a' AND phase IN ('Pending','Claimed','Running','Stopping')").fetchone()
                if held or control['desiredState']=='Paused':decision={'kind':'ContinueRequired','rootId':'root-a','holds':[{'holdId':h['id'],'revision':str(h['revision'])} for h in held]}
                elif live:decision={'kind':'AlreadyActive','root':control,'activation':{'executionId':live['id'],'generation':str(live['generation']),'phase':live['phase']}}
                elif self.holds(d):decision={'kind':'Blocked','rootId':'root-a','blockers':self.summary(d)}
                else:
                    gen=d.execute("SELECT current_generation+1 FROM root_controls WHERE root_id='root-a'").fetchone()[0]
                    xid='exec-'+uuid.uuid4().hex
                    d.execute("UPDATE root_controls SET current_generation=? WHERE root_id='root-a'",(gen,))
                    d.execute("INSERT INTO executions VALUES(?,'root-a','boot-a',?,'Pending',NULL,NULL,1)",(xid,gen))
                    activation={'executionId':xid,'generation':str(gen),'phase':'Pending'}
                    self.emit(d,key,'ActivationRequested',actor,True)
            elif body['kind']=='runs.pause':
                d.execute("UPDATE root_controls SET desired_state='Paused',revision=revision+1 WHERE root_id='root-a'")
                old_pause=d.execute("SELECT id FROM blockers WHERE root_id='root-a' AND cause='HumanPause' AND state='Active'").fetchone()
                if old_pause:d.execute('UPDATE blockers SET revision=revision+1 WHERE id=?',(old_pause['id'],))
                else:self.add_hold(d,'hold-'+uuid.uuid4().hex,'HumanPause')
                self.emit(d,key,'RootControlChanged',actor,True)
            elif body['kind']=='runs.continue':
                wanted=body['coveredHolds']; valid=[]; authorized=actor=='user'
                if actor=='agent' and grant_id:
                    g=d.execute('SELECT * FROM resume_grants WHERE id=?',(grant_id,)).fetchone()
                    authorized=bool(g and g['actor_id']==actor and g['state']=='Active' and g['uses_remaining']>0 and g['expires_at']>100 and json.loads(g['binding_json'])=={'requestKey':key,'rootId':'root-a','coveredHolds':wanted,'allowedCauses':['HumanPause']})
                if not authorized:d.execute('ROLLBACK');return {'kind':'Rejected','code':'PermissionDenied'}
                for ref in wanted:
                    h=d.execute("SELECT * FROM blockers WHERE id=? AND root_id='root-a' AND state='Active' AND assignment_id IS NULL AND operation_id IS NULL",(ref['holdId'],)).fetchone()
                    allowed=['HumanPause','RestartContinue','BudgetContinue'] if actor=='user' else ['HumanPause']
                    if not h or h['cause'] not in allowed:
                        d.execute('ROLLBACK');return {'kind':'Rejected','code':'InvalidTarget'}
                    if h['revision']!=int(ref['revision']):
                        decision={'kind':'RevisionConflict','subject':{'kind':'blocker','id':h['id']},'revisionKind':'Blocker','expected':ref['revision'],'current':str(h['revision'])};break
                    valid.append(h)
                if decision is None:
                    for h in valid:d.execute("UPDATE blockers SET state='Cleared',revision=revision+1 WHERE id=?",(h['id'],))
                    cleared=wanted
                    d.execute("UPDATE root_controls SET desired_state='Enabled',revision=revision+1 WHERE root_id='root-a'")
                    if actor=='agent':d.execute("UPDATE resume_grants SET uses_remaining=uses_remaining-1,state='Consumed' WHERE id=?",(grant_id,))
                    self.emit(d,key,'RootControlChanged',actor,True)
            else:raise AssertionError('unsupported model command')
            if decision is None:
                decision={'kind':'Accepted','root':self.control(d),'clearedHolds':cleared,'remaining':self.summary(d)}
                if activation:decision['activation']=activation
            if fail:raise RuntimeError('injected precommit interruption')
            seq=self.emit(d,key,'CommandDecided',actor)
            receipt={'commandId':'cmd-'+uuid.uuid4().hex,'committedSequence':str(seq),'acceptedAtMs':'1','requestDigest':digest,'decision':decision}
            d.execute('INSERT INTO command_receipts VALUES(?,?,?,?,?,NULL,?,1)',(actor,scope,key,body['kind'],digest,json.dumps(receipt)))
            d.execute('COMMIT')
            if lose_ack:return {'kind':'CommandStatusUnknown','requestKey':key}
            return {'kind':'Recorded','receipt':receipt,'replayed':False}
        except RuntimeError:
            d.execute('ROLLBACK');return {'kind':'Rejected','code':'StorageUnavailable'}
        finally:d.close()
    def counts(self):
        with self.db() as d:return tuple(d.execute('SELECT count(*) FROM '+table).fetchone()[0] for table in ['command_receipts','events','queue_entries','executions'])
    def eligible(self,child=None):
        with self.db() as d:
            enabled=self.control(d)['desiredState']=='Enabled'
            held=d.execute("SELECT count(*) FROM blockers WHERE root_id='root-a' AND state='Active' AND (assignment_id IS NULL OR assignment_id=?)",(child,)).fetchone()[0]
            return enabled and held==0
@contextmanager
def fixture(enabled=False):
    with tempfile.TemporaryDirectory(prefix='devworks-contract-model-') as tmp:yield Model(Path(tmp)/'workspace.sqlite',enabled)

def run():
    with fixture(True) as m:
        req=request('start','start');a=m.apply(req,lose_ack=True);before=m.counts();b=m.apply(req)
        check('lost_commit_ack_replays_one_receipt_without_new_activation',a['kind']=='CommandStatusUnknown' and b['replayed'] and m.counts()==before and before[3]==1)
        changed=deepcopy(req);changed['command']['expectedControlRevision']='2'
        check('same_key_changed_payload_is_conflict',m.apply(changed)['code']=='IdempotencyConflict' and m.counts()==before)
        pause=m.apply(request('pause','pause'));after=m.counts();old=m.apply(req)
        check('old_start_receipt_does_not_resume_new_pause',old['replayed'] and m.counts()==after and not m.eligible())
        check('revoked_caller_cannot_read_old_receipt',m.apply(req,actor='revoked')['code']=='PermissionDenied')
    with fixture(True) as m:
        barrier=threading.Barrier(2)
        def contender(key):barrier.wait();return m.apply(request('start',key))['receipt']['decision']['kind']
        with ThreadPoolExecutor(max_workers=2) as pool:out=list(pool.map(contender,['one','two']))
        check('two_connections_start_one_activation',sorted(out)==['Accepted','AlreadyActive'] and m.counts()[3]==1)
    with fixture(True) as m:
        req=request('start','stale-work');req['command']['expectedWorkRevision']='2'
        out=m.apply(req)
        check('work_revision_conflict_is_distinct_from_control',out['receipt']['decision']['revisionKind']=='Work' and m.counts()[3]==0)
    with fixture() as m:
        req=request('start','paused-start');out=m.apply(req,actor='agent')
        check('start_does_not_clear_manual_pause',out['receipt']['decision']['kind']=='ContinueRequired' and not m.eligible())
        with m.db() as d:m.add_hold(d,'hold-approval','Approval')
        out=m.apply(request('continue','continue',holds=[{'holdId':'hold-human','revision':'1'}]))
        check('continue_accepted_while_independent_approval_stays',out['receipt']['decision']['kind']=='Accepted' and not m.eligible() and out['receipt']['decision']['root']['desiredState']=='Enabled')
        before=m.counts();check('continue_duplicate_does_not_reemit_wakeup',m.apply(request('continue','continue',holds=[{'holdId':'hold-human','revision':'1'}]))['replayed'] and m.counts()==before)
    with fixture() as m:
        old=request('continue','old-continue',holds=[{'holdId':'hold-human','revision':'1'}])
        m.apply(request('pause','renew-pause'));before=m.counts()
        out=m.apply(old)
        check('renewed_pause_fences_prior_continue',out['receipt']['decision']['kind']=='RevisionConflict' and not m.eligible())
        hold_rev=None
        with m.db() as d:hold_rev=d.execute("SELECT revision FROM blockers WHERE id='hold-human'").fetchone()[0]
        m.apply(request('pause','renew-pause'))
        with m.db() as d:check('same_pause_key_does_not_renew_twice',d.execute("SELECT revision FROM blockers WHERE id='hold-human'").fetchone()[0]==hold_rev)
        stale_hold=request('continue','stale-hold-current-control','2',holds=[{'holdId':'hold-human','revision':'1'}]);out=m.apply(stale_hold)
        check('stale_hold_conflict_names_blocker_dimension',out['receipt']['decision']['kind']=='RevisionConflict' and out['receipt']['decision']['revisionKind']=='Blocker' and not m.eligible())
    with fixture() as m:
        with m.db() as d:m.add_hold(d,'hold-approval','Approval')
        out=m.apply(request('continue','mixed-holds',holds=[{'holdId':'hold-human','revision':'1'},{'holdId':'hold-approval','revision':'1'}]))
        with m.db() as d:check('invalid_covered_hold_rolls_back_entire_continue',out['code']=='InvalidTarget' and self_state(d,'hold-human')=='Active' and m.control(d)['revision']=='1')
    with fixture() as m:
        req=request('continue','agent-continue',holds=[{'holdId':'hold-human','revision':'1'}])
        check('agent_requires_exact_explicit_host_grant',m.apply(req,actor='agent')['code']=='PermissionDenied')
        grant=m.grant(req);before=m.counts();out=m.apply(req,actor='agent',grant_id=grant,fail=True)
        with m.db() as d:check('precommit_failure_preserves_domain_grant_and_outbox',out['code']=='StorageUnavailable' and m.counts()==before and self_state(d,'hold-human')=='Active' and d.execute('SELECT uses_remaining FROM resume_grants').fetchone()[0]==1)
        out=m.apply(req,actor='agent',grant_id=grant)
        with m.db() as d:check('explicit_grant_consumed_with_control_and_receipt',out['receipt']['decision']['kind']=='Accepted' and d.execute('SELECT uses_remaining FROM resume_grants').fetchone()[0]==0 and m.eligible())
        before=m.counts();check('consumed_grant_can_replay_receipt_without_reuse',m.apply(req,actor='agent',grant_id=grant)['replayed'] and m.counts()==before)
    with fixture() as m:
        with m.db() as d:
            d.execute("INSERT INTO work_items VALUES('child-a','project-a','task','root-a',NULL,NULL,'Child A',1,0,1,1,1,'{}')")
            d.execute("INSERT INTO work_items VALUES('child-b','project-a','task','root-a',NULL,NULL,'Child B',1,0,1,1,1,'{}')")
            d.execute("INSERT INTO conversations VALUES('conversation-a','root-a',1)")
            d.execute("INSERT INTO sessions VALUES('session-a','conversation-a','agent','root-a',0,1,NULL)")
            for child in ['child-a','child-b']:d.execute("INSERT INTO assignments VALUES(?,'root-a',?,NULL,'session-a',NULL,'Waiting',1,0,1,NULL)",(child,child))
            m.add_hold(d,'hold-child-approval','Approval','child-a')
        check('paused_root_blocks_both_children',not m.eligible('child-a') and not m.eligible('child-b'))
        m.apply(request('continue','parent-continue',holds=[{'holdId':'hold-human','revision':'1'}]))
        check('continue_enables_only_eligible_descendant',not m.eligible('child-a') and m.eligible('child-b'))
        with m.db() as d:d.execute("UPDATE blockers SET state='Cleared',revision=revision+1 WHERE id='hold-child-approval'")
        check('cleared_child_blocker_needs_no_child_continue',m.eligible('child-a') and m.eligible('child-b'))
    with fixture() as m:
        with m.db() as d:m.add_hold(d,'hold-restart','RestartContinue')
        req=request('continue','hold-set',holds=[{'holdId':'hold-restart','revision':'1'},{'holdId':'hold-human','revision':'1'}]);m.apply(req);before=m.counts()
        req['command']['coveredHolds'].reverse()
        check('reordered_hold_set_is_same_intent',m.apply(req)['replayed'] and m.counts()==before)
    with fixture(True) as m:
        m.apply(request('start','start'))
        with m.db() as d:
            d.execute('UPDATE store_meta SET restore_epoch=1')
            d.execute("UPDATE root_controls SET desired_state='Paused',revision=2")
            m.add_hold(d,'hold-restart','RestartContinue')
        check('old_epoch_invocation_refused',m.apply(request('start','start'))['code']=='PermissionDenied')
        out=m.apply(request('start','start','2'),epoch=1)
        check('new_epoch_start_cannot_reuse_old_receipt_to_resume',not out['replayed'] and out['receipt']['decision']['kind']=='ContinueRequired' and not m.eligible())
    report={'date':'2026-11-01','status':'Passed deterministic SQLite contract-model checks; production Rust gateway/policy/process control not implemented','sqliteVersion':sqlite3.sqlite_version,'schemaSha256':hashlib.sha256(SCHEMA.read_bytes()).hexdigest(),'checks':RESULTS,'passed':len(RESULTS),'noInference':True,'noExternalEffects':True}
    out=ROOT/'docs/architecture/validation/operation-contracts/receipt-model-results.json';out.write_text(json.dumps(report,indent=2)+'\n');print(json.dumps({'passed':len(RESULTS),'status':report['status']}))
def self_state(d,hid):return d.execute('SELECT state FROM blockers WHERE id=?',(hid,)).fetchone()[0]
if __name__=='__main__':run()
