"""Author candidate shared-message fixtures; no application execution or inference."""
from copy import deepcopy
import json
from pathlib import Path
ROOT=Path(__file__).resolve().parents[2]
OUT=ROOT/'docs/architecture/contracts/fixtures.json'
P='1.0-draft.1'
CASES=[]
def add(name,entry,value,typed=True,schema=None):
    CASES.append({'id':name,'entry':entry,'raw':value if isinstance(value,str) else json.dumps(value,ensure_ascii=False,separators=(',',':')),'typedValid':typed,'schemaValid':typed if schema is None else schema})
def mutated(name,entry,value,change,typed=False,schema=None):
    v=deepcopy(value);change(v);add(name,entry,v,typed,schema)
root={'rootId':'root-a','revision':'2','desiredState':'Enabled'}
activation={'executionId':'exec-a','generation':'1','phase':'Pending'}
hold={'holdId':'hold-a','revision':'1'}
blocker={**hold,'subject':{'kind':'root','id':'root-a'},'cause':'Approval'}
empty={'items':[],'hasMore':False}
start={'protocol':P,'requestKey':'key-start','command':{'kind':'runs.start','rootId':'root-a','expectedWorkRevision':'1','expectedControlRevision':'1'}}
pause={'protocol':P,'requestKey':'key-pause','command':{'kind':'runs.pause','rootId':'root-a','expectedControlRevision':'1'}}
cont={'protocol':P,'requestKey':'key-continue','command':{'kind':'runs.continue','rootId':'root-a','expectedControlRevision':'1','coveredHolds':[hold]}}
for name,value in [('start',start),('pause',pause),('continue',cont)]:add(name,'command-request',value)
for field in ['actorId','policyRevision','approved','resumeGrant','scopeKey']:
 mutated('reject-caller-'+field,'command-request',cont,lambda v,f=field:v.update({f:'forged'}))
mutated('reject-body-authority','command-request',cont,lambda v:v['command'].update({'actorId':'user'}))
mutated('reject-child-control-variant','command-request',cont,lambda v:v['command'].update({'kind':'assignments.continue'}))
mutated('reject-zero-revision','command-request',start,lambda v:v['command'].update({'expectedControlRevision':'0'}))
mutated('reject-numeric-revision','command-request',start,lambda v:v['command'].update({'expectedControlRevision':1}))
mutated('reject-leading-zero','command-request',start,lambda v:v['command'].update({'expectedControlRevision':'01'}))
mutated('reject-signed64-overflow','command-request',start,lambda v:v['command'].update({'expectedControlRevision':'9223372036854775808'}),schema=True)
mutated('accept-signed64-max','command-request',start,lambda v:v['command'].update({'expectedControlRevision':'9223372036854775807'}),typed=True)
mutated('reject-unknown-protocol','command-request',start,lambda v:v.update({'protocol':'2.0'}))
mutated('reject-empty-covered-set','command-request',cont,lambda v:v['command'].update({'coveredHolds':[]}))
mutated('reject-duplicate-hold-id','command-request',cont,lambda v:v['command']['coveredHolds'].append({**hold,'revision':'2'}),schema=True)
mutated('normalize-hold-set-order','command-request',cont,lambda v:v['command'].update({'coveredHolds':[{'holdId':'hold-z','revision':'2'},hold]}),typed=True)
mutated('reject-path-as-id','command-request',start,lambda v:v['command'].update({'rootId':'../root-a'}))
mutated('reject-overlong-key','command-request',start,lambda v:v.update({'requestKey':'x'*129}))
raw=json.dumps(start,separators=(',',':')).replace('"rootId":"root-a"','"rootId":"root-a","rootId":"root-b"')
add('reject-duplicate-json-member','command-request',raw,False,False)
receipt={'commandId':'cmd-a','committedSequence':'9','acceptedAtMs':'1793570000000','requestDigest':'a'*64,'decision':{'kind':'Accepted','root':root,'activation':activation,'clearedHolds':[hold],'remaining':{'items':[blocker],'hasMore':False}}}
reply={'kind':'Recorded','protocol':P,'receipt':receipt,'replayed':False}
add('accepted-does-not-mean-unblocked','command-reply',reply)
mutated('receipt-replayed','command-reply',reply,lambda v:v.update({'replayed':True}),typed=True)
for name,decision in [
 ('already-active',{'kind':'AlreadyActive','root':root,'activation':activation}),
 ('continue-required',{'kind':'ContinueRequired','rootId':'root-a','holds':[hold]}),
 ('blocked',{'kind':'Blocked','rootId':'root-a','blockers':{'items':[blocker],'hasMore':False}}),
 ('revision-conflict',{'kind':'RevisionConflict','subject':{'kind':'root','id':'root-a'},'revisionKind':'Control','expected':'1','current':'2'}),
 ('archived',{'kind':'Archived','rootId':'root-a'})]:
 v=deepcopy(reply);v['receipt']['decision']=decision;add(name,'command-reply',v)
add('command-status-unknown','command-reply',{'kind':'CommandStatusUnknown','protocol':P,'requestKey':'key-a','correlationId':'corr-a'})
add('idempotency-conflict','command-reply',{'kind':'Rejected','protocol':P,'requestKey':'key-a','code':'IdempotencyConflict','retry':'Never','correlationId':'corr-a'})
mutated('reject-malformed-digest','command-reply',reply,lambda v:v['receipt'].update({'requestDigest':'not-a-digest'}))
mutated('reject-unknown-reply-variant','command-reply',reply,lambda v:v.update({'kind':'Success'}))
query={'protocol':P,'queryId':'query-a','query':{'kind':'runs.eligibility','target':{'kind':'assignment','id':'child-a'},'purpose':'Implementation'}}
add('advisory-child-eligibility','query-request',query)
for name,q in [('root-query',{'kind':'runs.inspect','rootId':'root-a'}),('operation-query',{'kind':'history.operation_status','operationId':'op-a'}),('receipt-query',{'kind':'history.command_receipt','requestKey':'key-a'}),('event-query',{'kind':'history.events','rootId':'root-a','after':'opaque-cursor','limit':100})]:
 add(name,'query-request',{'protocol':P,'queryId':'query-a','query':q})
mutated('reject-query-mutation','query-request',query,lambda v:v['query'].update({'clearBlockers':True}))
eventquery={'protocol':P,'queryId':'query-a','query':{'kind':'history.events','rootId':'root-a','limit':1}}
mutated('reject-unbounded-page','query-request',eventquery,lambda v:v['query'].update({'limit':501}))
mutated('reject-oversized-cursor','query-request',eventquery,lambda v:v['query'].update({'after':'x'*4097}))
mutated('null-optional-normalizes','query-request',eventquery,lambda v:v['query'].update({'after':None}),typed=True)
event={'protocol':P,'eventId':'event-a','sequence':'8','aggregate':{'kind':'root','id':'root-a'},'aggregateRevision':'2','aggregateRevisionKind':'Control','actorId':'user-a','occurredAtMs':'1793570000000','commandId':'cmd-a','rootId':'root-a','event':{'kind':'RootControlChanged','root':root,'reason':'Continued'}}
add('root-event','event',event)
for name,payload,aggregate,revision in [
 ('decision-event',{'kind':'CommandDecided','commandId':'cmd-a'},{'kind':'root','id':'root-a'},'2'),
 ('activation-event',{'kind':'ActivationRequested','rootId':'root-a','executionId':'exec-a','generation':'1'},{'kind':'root','id':'root-a'},'2'),
 ('blocker-event',{'kind':'BlockerChanged','blocker':blocker,'change':'Raised'},{'kind':'blocker','id':'hold-a'},'1'),
 ('stale-observation-event',{'kind':'OperationObserved','operationId':'op-a','attemptId':'attempt-a','ownerGeneration':'1','observation':'StaleOwnerEvidenceOnly','state':'OutcomeUnknown'},{'kind':'operation','id':'op-a'},'2')]:
 v=deepcopy(event);v.update({'event':payload,'aggregate':aggregate,'aggregateRevision':revision,'aggregateRevisionKind':'Blocker' if name=='blocker-event' else 'Operation' if name=='stale-observation-event' else 'Control'});add(name,'event',v)
mutated('reject-mismatched-event-root','event',event,lambda v:v.update({'rootId':'root-b'}),schema=True)
mutated('reject-mismatched-event-revision','event',event,lambda v:v.update({'aggregateRevision':'3'}),schema=True)
mutated('reject-unknown-event-schema','event',event,lambda v:v['event'].update({'kind':'UnknownNewMutation'}))
stamp={'storeId':'store-a','restoreEpoch':'0','sequence':'9','policyRevision':'1','scopeRevision':'1'}
queryreply={'kind':'Result','protocol':P,'queryId':'query-a','stamp':stamp,'result':{'kind':'Eligibility','target':{'kind':'assignment','id':'child-a'},'purpose':'Implementation','eligible':False,'blockers':{'items':[blocker],'hasMore':False}}}
add('blocked-eligibility-result','query-reply',queryreply)
mutated('reject-eligible-with-blocker','query-reply',queryreply,lambda v:v['result'].update({'eligible':True}),schema=True)
for name,result in [
 ('root-result',{'kind':'Root','root':root,'blockers':empty}),
 ('unknown-effect-result',{'kind':'Operation','operationId':'op-a','revision':'1','state':'OutcomeUnknown','retry':'ReconcileOperation'}),
 ('receipt-result',{'kind':'Receipt','receipt':receipt}),
 ('missing-receipt-result',{'kind':'Receipt'}),
 ('event-page-result',{'kind':'Events','events':[event],'nextCursor':'tail-cursor','hasMore':False})]:
 v=deepcopy(queryreply);v['result']=result;add(name,'query-reply',v)
 v2=deepcopy(v)
 if name=='unknown-effect-result':v2['result']['retry']='SameCommand';add('reject-replay-unknown-effect','query-reply',v2,False,True)
 if name=='event-page-result':v2['stamp']['sequence']='7';add('reject-page-beyond-watermark','query-reply',v2,False,True)
add('cursor-expired-result','query-reply',{'kind':'Rejected','protocol':P,'queryId':'query-a','code':'CursorExpired','retry':'RefreshAndNewRequest','correlationId':'corr-a'})
OUT.write_text(json.dumps(CASES,ensure_ascii=False,indent=2)+'\n')
print(json.dumps({'fixtures':len(CASES),'output':str(OUT)}))
