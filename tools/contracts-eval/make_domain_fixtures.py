"""Structural coverage fixtures plus independent adversarial contract cases; no effects."""
from pathlib import Path
import copy,json,re
from domain_spec import TYPES,OPS,LEGACY_RESULTS
ROOT=Path(__file__).resolve().parents[2];D=ROOT/'docs/architecture/contracts'
ALL=[c for n in ['fixtures','provider-package-fixtures','data-delivery-fixtures'] for c in json.loads((D/(n+'.json')).read_text())]
def valid(entry):return json.loads(next(c['raw'] for c in ALL if c['entry']==entry and c['typedValid']))
ART={'artifactId':'artifact-a','versionId':'version-a','sha256':'a'*64,'byteLength':'8','mediaType':'text/plain'}
STAMP={'storeId':'store-a','restoreEpoch':'0','sequence':'1','policyRevision':'1','scopeRevision':'1'}
BASE={'Id':'id-a','Revision':'1','Counter':'0','ProjectId':'project-a','RootId':'root-a','ActorId':'actor-a','AssignmentId':'assignment-a','OperationId':'operation-a','QueryId':'query-a','RequestKey':'request-a','CommandId':'command-a','EventId':'event-a','StoreId':'store-a','ResourceId':'resource-a','CorrelationId':'correlation-a',
 'Label':'fixture','Text':'fixture','Content':'Fixture content','Cursor':'cursor-a','RelativePath':'src/main.rs','Url':'https://fixture.invalid/path','Money':'0','SignedCounter':'-1','PositiveCounter':'1','Finite':0.5,'Probability':0.5,'bool':False,'i32':1,'Limit':20,'Depth':2,'Port':8765,'ByteLimit':4096,'TokenBudget':1500,'ArtifactTags':['evidence'],'PathSet':['src/lib.rs'],'DynamicJson':{'input':'fixture'},
 'DataScope':{'kind':'Workspace'},'ViewSelection':{'kind':'Published','repositoryIds':['repo-a']},'Coverage':{'state':'Complete','gaps':[],'moreGaps':False},'Digest':'a'*64,'ArtifactRef':ART,'SchemaRef':{'schemaId':'fixture/object','revision':'1','sha256':'a'*64},'Attribution':{'kind':'Workspace','sessionId':'session-a'},'GitOid':{'format':'Sha1','hex':'a'*40},'ReadStamp':STAMP,'OperationState':'ObservedComplete',
 'RootControl':{'rootId':'root-a','revision':'1','desiredState':'Enabled'},'Blockers':{'items':[],'hasMore':False},'EligibilityTarget':{'kind':'root','id':'root-a'},'Purpose':'Planning',
 'DeliveryBarrier':valid('delivery-barrier'),'DeliverySnapshot':valid('delivery-snapshot'),'GraphPage':valid('graph-page'),'RetrievalPage':valid('retrieval-page'),'TranscriptPage':valid('transcript-page'),'Event':valid('event')}
def camel(s):return re.sub(r'_([a-z])',lambda m:m[1].upper(),s)
def sample(t):
 if t.startswith('?'):return None
 if t in BASE:return copy.deepcopy(BASE[t])
 if '<' in t:
  outer,inner=t.split('<',1);inner=inner[:-1]
  if outer in ['Page','HistoryPage']:return {'items':[],'hasMore':False}
  return [sample(inner)] if outer in ['NonEmpty','PlanFeatures'] else []
 d=TYPES[t]
 if d['kind']=='enum':return d['values'][0]
 if d['kind']=='union':
  k,fs=next(iter(d['variants'].items()));v={'kind':k,**{camel(n):sample(f) for n,f in fs.items() if not f.startswith('?')}}
 else:v={camel(n):sample(f) for n,f in d['fields'].items() if not f.startswith('?')}
 if t in ['ClaimDraft','KnowledgeClaim']:v['value']='A retained fact'
 if t=='KnowledgeBatch':v['claims']=[sample('ClaimDraft')]
 if t=='EnvironmentEntry':v['value']='fixture'
 if t=='DecisionQuestion':v['criteria']=[{'name':'yes'},{'name':'no'}]
 if t=='UsageItem':v['amountUsd']='0'
 if t=='ArtifactReadContent':v.update(content='fixture',nextByte='7',truncated=True)
 if t=='ProcessOutput':v['nextByte']=str(len(v['content'].encode()))
 return v

def request(name,mode):
 d=OPS[name];fs=d[mode.lower()+'Fields'] if d['mode']=='Hybrid' else d['fields']
 body={'kind':name,**{camel(n):sample(t) for n,t in fs.items() if not t.startswith('?')}}
 if d.get('exactlyOne'):m=d['exactlyOne'][0];body[camel(m)]=sample(fs[m][1:])
 if name=='git.diff':body['paths']=['src/name:part\\literal.rs']
 if name=='tasks.create':body['parentId']='feature-a'
 if name=='tasks.assign':body['clearAssignment']=True
 if name=='tasks.set_feature':body['clearFeature']=True
 if name=='workflows.reassign_and_remove':body['destinationStatusId']='status-other'
 if name in ['knowledge.entity_bundle','knowledge.entity_graph','knowledge.expand_frontier']:body['query']['entityIds']=['entity-a']
 if name=='knowledge.claim_bundle':body['query']['claimIds']=['claim-a']
 if name=='queue.reconcile':body['resolution']='Reconcile'
 if name.startswith('maintenance.'):body['previewOnly']=True
 return {'mode':mode,'protocol':'1.0-draft.1',('queryId' if mode=='Query' else 'requestKey'):'request-a','body':body}

def result(name,t):
 value=sample(t)
 if t=='GraphPage':
  if name in ['code.source','code.snippet']:value['result']={'kind':'Source','citation':copy.deepcopy(BASE['GraphPage']['result']['nodes'][0]['citation']),'text':'fixture source'}
  elif name=='code.graph_schema':value['result']={'kind':'Schema','schema':copy.deepcopy(ART)}
  elif name=='code.graph_scope_status':value['result']={'kind':'ScopeStatus','coverage':sample('Coverage')}
  else:value['result']={'kind':'Nodes','nodes':[]}
 if t=='WorkRecord':
  value['kind']={'tasks':'Task','issues':'Issue'}.get(name.split('.')[0],'Feature')
  if value['kind']=='Task':value['parentId']='feature-a'
 return {'kind':name,'value':value}

CASES=[]; POS={}
def case(id,entry,value,typed=True,schema=True):
 CASES.append({'id':id,'entry':entry,'raw':json.dumps(value,separators=(',',':')),'typedValid':typed,'schemaValid':schema})
 if typed:POS[id]=copy.deepcopy(value)
for name,d in OPS.items():
 for mode in ([d['mode']] if d['mode']!='Hybrid' else ['Query','Command']):case('input-'+name+'-'+mode,'domain-request',request(name,mode))
for name,t in ({n:d['result'] for n,d in OPS.items()}|LEGACY_RESULTS).items():
 case('result-'+name,'operation-result',result(name,t))
# Stable handwritten assertions cover wrong mode, authority/provenance injection,
# dangerous ambiguity, partial/unknown effects and cross-operation substitution.
def mutate(id,base,path,value,typed=False,schema=False,entry=None):
 obj=copy.deepcopy(POS[base]);cur=obj
 for k in path[:-1]:cur=cur[k]
 if value is DELETE:cur.pop(path[-1],None)
 else:cur[path[-1]]=value
 case(id,entry or next(c['entry'] for c in CASES if c['id']==base),obj,typed,schema)
DELETE=object()
mutate('reject-self-human','input-approvals.record_human_decision-Command',['body','human'],True)
mutate('reject-policy-grant','input-process.execute-Command',['body','grant'],{'allowAll':True})
mutate('reject-raw-credential','input-router.configure-Command',['body','apiKey'],'not-a-real-key')
mutate('reject-forged-attribution','input-router.decide-Command',['body','attribution'],{'kind':'Workspace','sessionId':'fake'})
mutate('reject-forged-price-origin','input-router.price_override-Command',['body','price','origin'],'ProviderReported')
mutate('reject-forged-setting-source','input-settings.save_revision-Command',['body','entries',0,'sourceScope'],{'kind':'Workspace'})
mutate('reject-job-cap','input-jobs.create-Command',['body','spendingLimitUsd'],'1')
mutate('reject-child-pause','input-runs.stop-Command',['body','kind'],'agents.pause')
mutate('reject-global-cap','input-agents.delegate-Command',['body','globalCap'],10)
mutate('reject-unknown-op','input-projects.get-Query',['body','kind'],'workspace.erase_drive')
mutate('reject-command-with-query-mode','input-projects.create-Command',['mode'],'Query')
mutate('reject-query-with-command-mode','input-projects.get-Query',['mode'],'Command')
mutate('reject-browser-create-query','input-browser.sessions-Command',['mode'],'Query')
mutate('reject-negative-limit','input-projects.list-Query',['body','page','limit'],-1)
mutate('reject-oversize-page','input-projects.list-Query',['body','page','limit'],501)
mutate('reject-invalid-counter','input-projects.edit-Command',['body','expectedRevision'],'01')
mutate('reject-counter-range','input-projects.edit-Command',['body','expectedRevision'],'9223372036854775808',schema=True)
mutate('reject-parent-path','input-git.diff-Query',['body','paths'],['../secret'])
mutate('reject-absolute-root','input-process.execute-Command',['body','spec','workingDirectory','relative'],'/tmp')
# A task belongs to its project; feature association and assignment parentage are separate.
mutate('accept-task-without-feature','input-tasks.create-Command',['body','parentId'],DELETE,typed=True,schema=True)
mutate('accept-task-null-feature','input-tasks.create-Command',['body','parentId'],None,typed=True,schema=True)
mutate('reject-task-without-project','input-tasks.create-Command',['body','projectId'],DELETE)
mutate('accept-issue-delegation-without-cards','input-agents.delegate-Command',['body','taskIds'],[],typed=True,schema=True)
mutate('accept-standalone-task-record','result-tasks.get',['value','parentId'],DELETE,typed=True,schema=True)
obj=copy.deepcopy(POS['result-tasks.get']);obj['value']['kind']='Issue';obj['value'].pop('parentId',None)
case('accept-issue-through-task-operation','operation-result',obj)
mutate('reject-delegation-without-root','input-agents.delegate-Command',['body','rootId'],DELETE)
mutate('reject-feature-task-only-patch','input-features.edit-Command',['body','changes','inheritsRequirementIds'],['requirement-a'])
mutate('reject-double-assignment','input-tasks.assign-Command',['body','assignmentId'],'assignment-a',schema=True)
mutate('reject-same-status-delete','input-workflows.reassign_and_remove-Command',['body','destinationStatusId'],'id-a',schema=True)
mutate('reject-replacement-without-revision','input-documents.import-Command',['body','replaceDocumentId'],'document-a',schema=True)
mutate('reject-empty-knowledge','input-knowledge.record-Command',['body','batch','claims'],[],schema=True)
mutate('reject-empty-frontier','input-knowledge.expand_frontier-Query',['body','query','entityIds'],[],schema=True)
mutate('reject-duplicate-tier-members','input-router.configure-Command',['body','action'],{'kind':'Tier','tier':'Max','expectedRevision':'1','memberIds':['a','a']},schema=True)
mutate('reject-SystemOne-in-chat','input-router.configure-Command',['body','action'],{'kind':'Model','model':{**sample('RouteModel'),'endpointProfile':'SystemOne'}},schema=True)
mutate('reject-unrecognized-question','input-router.decide-Command',['body','questions',0,'question'],{'kind':'Execute','command':'rm'})
mutate('reject-single-choice','input-router.decide-Command',['body','questions',0,'question','criteria'],[{'name':'one'}],schema=True)
mutate('reject-duplicate-choice-name','input-router.decide-Command',['body','questions',0,'question','criteria'],[{'name':'same'},{'name':'same','description':'different'}],schema=True)
mutate('reject-queue-action-substitution','input-queue.retry-Command',['body','resolution'],'Reconcile',schema=True)
mutate('reject-prune-without-preview','input-maintenance.prune_messages-Command',['body','previewOnly'],False,schema=True)
mutate('reject-page-missing-cursor','result-projects.list',['value','hasMore'],True,schema=True)
mutate('reject-work-result-kind','result-tasks.get',['value','kind'],'Feature',schema=True)
mutate('reject-graph-result-kind','result-code.search',['value','result'],{'kind':'Source','citation':copy.deepcopy(BASE['GraphPage']['result']['nodes'][0]['citation']),'text':'fixture source'},schema=True)
mutate('reject-effort-downgrade','result-router.route',['value','requestedEffort'],'High',schema=True)
# Shared task priority and directed blocking-link candidates.
mutate('accept-task-priority-edit','input-tasks.edit-Command',['body','changes'],{'priority':7},typed=True,schema=True)
mutate('accept-issue-priority-edit','input-issues.edit-Command',['body','changes'],{'priority':7},typed=True,schema=True)
mutate('reject-duplicate-task-order','input-tasks.prioritize-Command',['body','orderedTaskIds'],['task-a','task-a'],schema=True)
mutate('accept-feature-task-order','input-tasks.prioritize-Command',['body','featureId'],'feature-a',typed=True,schema=True)
mutate('reject-empty-task-order','input-tasks.prioritize-Command',['body','orderedTaskIds'],[])
obj=copy.deepcopy(POS['input-tasks.dependency_links-Command']);target=obj['body']['taskId'];obj['body']['replacement']['edges']=[{'predecessor':'blocking-task','successor':target}]
case('accept-one-task-blocks-another','domain-request',obj)
mutate('reject-self-task-dependency','accept-one-task-blocks-another',['body','replacement','edges'],[{'predecessor':target,'successor':target}],schema=True)
mutate('reject-unrelated-target-dependency','accept-one-task-blocks-another',['body','replacement','edges',0,'successor'],'other-task',schema=True)
mutate('reject-duplicate-task-dependency','accept-one-task-blocks-another',['body','replacement','edges'],obj['body']['replacement']['edges']*2,schema=True)
mutate('accept-remove-task-blockers','accept-one-task-blocks-another',['body','replacement','edges'],[],typed=True,schema=True)
# Feature owns one active approved plan and one pending replacement.
obj=copy.deepcopy(POS['result-features.get']);obj['value']['plan']=copy.deepcopy(ART)
obj['value']['approval']={'proposalId':'approved-a','planRevision':'1','planDigest':'a'*64,'state':'Approved'}
obj['value']['pendingPlan']={'revision':'2','draft':sample('PlanDraft'),'approval':{'proposalId':'pending-b','planRevision':'2','planDigest':'b'*64,'state':'Pending'}}
case('active-plan-with-pending-revision','operation-result',obj)
mutate('active-plan-with-rejected-revision','active-plan-with-pending-revision',['value','pendingPlan','approval','state'],'Rejected',typed=True,schema=True)
mutate('reject-approved-draft-left-pending','active-plan-with-pending-revision',['value','pendingPlan','approval','state'],'Approved',schema=True)
mutate('reject-stale-draft-approval','active-plan-with-pending-revision',['value','pendingPlan','approval','planRevision'],'1',schema=True)
mutate('reject-pending-plan-on-issue','active-plan-with-pending-revision',['value','kind'],'Issue',schema=True)
mutate('reject-pending-active-approval','active-plan-with-pending-revision',['value','approval','state'],'Pending',schema=True)
obj=copy.deepcopy(POS['result-features.get']);obj['value']['pendingPlan']={'revision':'1','draft':sample('PlanDraft')}
case('initial-plan-awaiting-first-approval','operation-result',obj)
# Four SystemOne forms and a descriptor-bound extension, plus entity/evidence-only writes.
for kind in ['Choice','Noul','Score','Box','Extension']:
 obj=copy.deepcopy(POS['input-router.decide-Command']);fs=TYPES['DecisionQuestion']['variants'][kind]
 question={'kind':kind,**{camel(n):sample(t) for n,t in fs.items() if not t.startswith('?')}}
 if kind=='Choice':question['criteria']=[{'name':'yes'},{'name':'no'}]
 if kind=='Score':question['criteria']=['low','high']
 obj['body']['questions']=[{'name':'fixture','question':question}];case('decision-'+kind,'domain-request',obj)
for key,t in [('entities','KnowledgeEntityDraft'),('evidence','SourceEvidence')]:
 obj=copy.deepcopy(POS['input-knowledge.record-Command']);obj['body']['batch']['claims']=[];obj['body']['batch'][key]=[sample(t)];case('knowledge-'+key+'-only','domain-request',obj)
# Explicit price semantics: reported zero is distinct from local free and untracked.
for kind,origin,amount in [('Reported','ProviderReported','0'),('Free','LocalFree','0'),('Untracked','Untracked',None),('Estimated','OfficialSnapshot','0.12')]:
 obj=copy.deepcopy(POS['result-router.usage']);item=sample('UsageItem');item.update(priceKind=kind,priceOrigin=origin)
 if amount is None:item.pop('amountUsd',None)
 else:item['amountUsd']=amount
 obj['value']['items']=[item];case('usage-'+kind,'operation-result',obj)
mutate('reject-untracked-as-priced','usage-Untracked',['value','items',0,'amountUsd'],'0',schema=True)
mutate('reject-remote-as-local-free','usage-Free',['value','items',0,'priceOrigin'],'ProviderReported',schema=True)
# Reply/receipt/completion paths retain absence-of-ack and outcome uncertainty.
FAULT={'code':'PermissionDenied','retry':'Never','correlationId':'correlation-a','blockerIds':[]}
case('query-typed-result','domain-query-reply',{'kind':'Result','protocol':'1.0-draft.1','queryId':'query-a','stamp':STAMP,'result':result('projects.get','ProjectRecord')})
case('query-permission-denied','domain-query-reply',{'kind':'Rejected','protocol':'1.0-draft.1','queryId':'query-a','fault':FAULT})
RECEIPT={'commandId':'command-a','committedSequence':'1','acceptedAtMs':'1','requestDigest':'a'*64,'operationId':'operation-a','decision':{'kind':'Accepted','operationId':'operation-a'}}
case('command-recorded-accepted','domain-command-reply',{'kind':'Recorded','protocol':'1.0-draft.1','receipt':RECEIPT,'replayed':False})
case('command-status-unknown','domain-command-reply',{'kind':'CommandStatusUnknown','protocol':'1.0-draft.1','requestKey':'request-a','correlationId':'correlation-a'})
case('command-pre-admission-rejection','domain-command-reply',{'kind':'Rejected','protocol':'1.0-draft.1','requestKey':'request-a','code':'PermissionDenied','retry':'Never','correlationId':'correlation-a'})
mutate('reject-accepted-operation-mismatch','command-recorded-accepted',['receipt','operationId'],'another',schema=True)
mutate('reject-permission-blind-retry','query-permission-denied',['fault','retry'],'SameCommand',schema=True)
conflict=copy.deepcopy(POS['query-permission-denied']);conflict['fault'].update(code='RevisionConflict',retry='RefreshAndNewRequest',targetId='project-a',targetKind='project',revisionDimension='work',expectedRevision='1',currentRevision='2')
case('conflict-identifies-revision-dimension','domain-query-reply',conflict)
mutate('reject-conflict-without-dimension','conflict-identifies-revision-dimension',['fault','revisionDimension'],DELETE,schema=True)
mutate('reject-conflict-without-target-kind','conflict-identifies-revision-dimension',['fault','targetKind'],DELETE,schema=True)

case('completion-succeeded','operation-completion',{'kind':'Succeeded','protocol':'1.0-draft.1','operationId':'operation-a','revision':'1','result':result('projects.create','ProjectRecord')})
case('completion-outcome-unknown','operation-completion',{'kind':'OutcomeUnknown','protocol':'1.0-draft.1','operationId':'operation-a','revision':'1','correlationId':'correlation-a'})
case('completion-failed','operation-completion',{'kind':'Failed','protocol':'1.0-draft.1','operationId':'operation-a','revision':'1','fault':{**FAULT,'code':'OutcomeUnknown','retry':'ReconcileOperation'}})
mutate('reject-unknown-as-retry','completion-failed',['fault','retry'],'SameCommand',schema=True)
# Dynamic payloads have explicit secondary byte/depth/node/schema checks.
obj=copy.deepcopy(POS['input-router.decide-Command']);obj['body']['state']={'body':'x'*524289};case('reject-large-dynamic-state','domain-request',obj,False,True)
obj=copy.deepcopy(POS['input-router.decide-Command']);raw=json.dumps(obj,separators=(',',':')).replace('"input":"fixture"','"input":"one","input":"two"')
CASES.append({'id':'reject-nested-duplicate-member','entry':'domain-request','raw':raw,'typedValid':False,'schemaValid':False})
# Current user decisions: separate Anthropic profiles, human risk intent and removed engine.
for profile in ['AnthropicMessages','ClaudeSubscriptionMessages']:
 obj=copy.deepcopy(POS['input-router.configure-Command']);obj['body']['action']={'kind':'Connection','name':'fixture','profile':profile,'endpoint':'https://api.anthropic.com','credentialRef':'fixture-secret-ref'};case('anthropic-profile-'+profile,'domain-request',obj)
for accepted in [True,False]:
 obj=copy.deepcopy(POS['input-router.configure-Command']);obj['body']['action']={'kind':'SubscriptionRisk','connectionId':'connection-a','expectedRevision':'1','noticeSha256':'a'*64,'accepted':accepted};case('subscription-risk-'+str(accepted),'domain-request',obj)
mutate('reject-risk-without-notice','subscription-risk-True',['body','action','noticeSha256'],DELETE)
mutate('reject-risk-forged-actor','subscription-risk-True',['body','action','actorId'],'human-a')
obj=copy.deepcopy(POS['input-schemas.configure-Command']);obj['body']['config']['engine']='Helix';case('reject-removed-helix-reader','domain-request',obj,False,False)

# Incoming MCP administration cannot embed raw credentials or assert human authority.
mutate('reject-mcp-server-raw-token','input-mcp_server.configure-Command',['body','token'],'synthetic-not-a-token')
mutate('reject-mcp-client-self-human','input-mcp_server.set_client_access-Command',['body','human'],True)
mutate('reject-mcp-client-role-injection','input-mcp_server.set_client_access-Command',['body','action','role'],'Human')
mutate('reject-mcp-server-public-bind','input-mcp_server.configure-Command',['body','bindAddress'],'0.0.0.0')
mutate('reject-mcp-admin-query-command','input-mcp_server.configure-Command',['mode'],'Query')
# Interview refinements: explicit association, app-wide pacing and exact-head review completion.
mutate('issue-create-with-feature','input-issues.create-Command',['body','featureId'],'feature-a',True,True)
obj=copy.deepcopy(POS['input-tasks.set_feature-Command']);obj['body'].update(featureId='feature-a',clearFeature=False)
case('set-task-feature','domain-request',obj)
mutate('reject-membership-set-and-clear','set-task-feature',['body','clearFeature'],True,schema=True)
mutate('reject-membership-neither','input-tasks.set_feature-Command',['body','clearFeature'],False,schema=True)
obj=copy.deepcopy(POS['input-settings.save_revision-Command'])
obj['body']['scope']={'kind':'Workspace'}
obj['body']['entries'][0].update(key='github.issueTriageIntervalSeconds')
obj['body']['entries'][0]['value']['value']=1800
case('app-wide-issue-triage-interval','domain-request',obj)
mutate('reject-project-triage-override','app-wide-issue-triage-interval',['body','scope'],{'kind':'Project','projectId':'project-a'},schema=True)
for bad in [0,-1,1.5,'1800']:
 mutate('reject-invalid-triage-interval-'+str(bad),'app-wide-issue-triage-interval',['body','entries',0,'value','value'],bad,schema=True)
review=copy.deepcopy(POS['result-reviews.inspect'])
source=sample('ReviewSourceEvidence')
source.update(state='Completed',completedAt='1',findingCount='0',zeroFindingsConfirmed=True,
              findingsCoverage={'state':'Complete','gaps':[],'moreGaps':False},headDigest=review['value']['headDigest'])
review['value'].update(state='Ready',sources=[source])
case('review-ready-explicit-zero-findings','operation-result',review)
mutate('reject-review-ready-no-source','review-ready-explicit-zero-findings',['value','sources'],[],schema=True,entry='operation-result')
mutate('reject-review-silence-as-zero-findings','review-ready-explicit-zero-findings',['value','sources',0,'zeroFindingsConfirmed'],False,schema=True,entry='operation-result')
mutate('reject-review-wrong-head','review-ready-explicit-zero-findings',['value','sources',0,'headDigest'],'b'*64,schema=True,entry='operation-result')
mutate('reject-review-partial-findings','review-ready-explicit-zero-findings',['value','sources',0,'findingsCoverage','state'],'Partial',schema=True,entry='operation-result')
mutate('review-in-app-fallback-evidence','review-ready-explicit-zero-findings',['value','sources',0,'sourceKind'],'InAppFallback',True,True,entry='operation-result')
# Schema revision 2 (artifact tags). A request negotiates it with the envelope member
# schemaRevision; `<entry>.r2` documents describe revision 2. Revision-1 shapes stay strict.
def recorded(id):return json.loads(next(c['raw'] for c in ALL if c['id']==id))
save,listing=recorded('artifact-Save'),recorded('artifact-query-List')
def r2(base,path,tags,revision=2):
 obj=copy.deepcopy(base)
 if revision is not None:obj['schemaRevision']=revision
 if tags is not DELETE:obj[path]['tags']=tags
 return obj
case('r2-save-with-tags','artifact-command-request.r2',r2(save,'command',['release','evidence']))
case('r2-save-without-tags','artifact-command-request.r2',r2(save,'command',DELETE))
case('r2-save-tag-bounds','artifact-command-request.r2',r2(save,'command',['t'+str(i).zfill(63) for i in range(32)]))
case('r2-list-by-tags','artifact-query-request.r2',r2(listing,'query',['evidence','release']))
case('r1-explicit-revision-save','artifact-command-request',r2(save,'command',DELETE,1))
case('r1-save-negotiating-revision-2','artifact-command-request',r2(save,'command',DELETE))
case('reject-r1-save-tags','artifact-command-request',r2(save,'command',['evidence'],None),False,False)
case('reject-r1-explicit-save-tags','artifact-command-request',r2(save,'command',['evidence'],1),False,False)
case('reject-r1-list-tags','artifact-query-request',r2(listing,'query',['evidence'],None),False,False)
case('reject-r2-save-not-negotiated','artifact-command-request.r2',r2(save,'command',['evidence'],None),False,False)
case('reject-r2-too-many-tags','artifact-command-request.r2',r2(save,'command',['t'+str(i) for i in range(33)]),False,False)
case('reject-r2-duplicate-tags','artifact-command-request.r2',r2(save,'command',['evidence','evidence']),False,False)
case('reject-r2-empty-tag','artifact-command-request.r2',r2(save,'command',['']),False,False)
case('reject-r2-long-tag','artifact-command-request.r2',r2(save,'command',['x'*65]),False,False)
case('reject-r2-null-tags','artifact-command-request.r2',r2(save,'command',None),False,False)
case('reject-r2-duplicate-filter-tags','artifact-query-request.r2',r2(listing,'query',['a','a']),False,False)
case('reject-revision-as-string','artifact-command-request',{**save,'schemaRevision':'2'},False,False)
case('reject-revision-zero','artifact-command-request',{**save,'schemaRevision':0},False,False)
case('reject-revision-above-range','artifact-command-request',{**save,'schemaRevision':70000},False,False)
# JSON Schema 2020-12 "integer" matches any number with a zero fractional part, so no schema can
# refuse the spelling 1.0; the typed reader refuses it like every other integer member.
case('reject-revision-float-spelling','artifact-command-request',{**save,'schemaRevision':1.0},False,True)
record=copy.deepcopy(POS['result-artifacts.save']['value'])
case('r2-result-artifacts.save','operation-result.r2',{'kind':'artifacts.save','value':{**record,'tags':['evidence','release']}})
case('r2-result-artifacts.list','operation-result.r2',{'kind':'artifacts.list','value':{'items':[{**record,'tags':['evidence']},{**record,'tags':[]}],'hasMore':False}})
case('r1-result-artifacts.list-items','operation-result',{'kind':'artifacts.list','value':{'items':[record],'hasMore':False}})
case('reject-r1-result-tags','operation-result',{'kind':'artifacts.save','value':{**record,'tags':['evidence']}},False,False)
case('reject-r1-list-result-tags','operation-result',{'kind':'artifacts.list','value':{'items':[{**record,'tags':[]}],'hasMore':False}},False,False)
# Schema revision 2 (history and sessions): tail checkpoints, the command aggregate, the project
# change feed, tool invocations, structured lineage attribution and briefing item/paths/budget.
events=recorded('event-query');events['query'].pop('after')
def at(obj,revision):
 obj=copy.deepcopy(obj)
 if revision is not None:obj['schemaRevision']=revision
 return obj
project_feed=at(events,2);project_feed['query'].pop('rootId');project_feed['query']['projectId']='project-a'
case('r2-events-root','query-request.r2',at(events,2))
case('r2-events-project','query-request.r2',project_feed)
case('r1-events-root-explicit','query-request',at(events,1))
unnegotiated=at(project_feed,None);unnegotiated.pop('schemaRevision')
case('reject-r1-events-project','query-request',unnegotiated,False,False)
case('reject-r1-explicit-events-project','query-request',at(project_feed,1),False,False)
both=copy.deepcopy(project_feed);both['query']['rootId']='root-a'
case('reject-r2-events-root-and-project','query-request.r2',both,False,False)
neither=copy.deepcopy(project_feed);neither['query'].pop('projectId')
case('reject-r2-events-no-scope','query-request.r2',neither,False,False)
null_project=copy.deepcopy(project_feed);null_project['query']['projectId']=None
case('reject-r2-events-null-project','query-request.r2',null_project,False,False)
decision=recorded('decision-event')
command_event={**decision,'aggregate':{'kind':'command','id':'cmd-a'},'aggregateRevision':'1','aggregateRevisionKind':'Command'}
work_event={**decision,'aggregate':{'kind':'root','id':'root-a'},'aggregateRevisionKind':'Work'}
def page(items,checkpoint='cursor-a',more=False):
 v={'items':items,'hasMore':more}
 if more:v['nextCursor']=checkpoint
 if checkpoint is not None:v['checkpoint']=checkpoint
 return v
case('r2-result-history.events','operation-result.r2',{'kind':'history.events','value':page([work_event,command_event])})
case('r2-result-history.events-more','operation-result.r2',{'kind':'history.events','value':page([command_event],'cursor-b',True)})
case('r2-result-history.events-empty-tail','operation-result.r2',{'kind':'history.events','value':page([])})
case('reject-r1-result-command-aggregate','operation-result',{'kind':'history.events','value':page([command_event],None)},False,False)
case('reject-r1-result-checkpoint','operation-result',{'kind':'history.events','value':page([work_event])},False,False)
mismatch=page([command_event],'cursor-b',True);mismatch['checkpoint']='cursor-c'
case('reject-r2-checkpoint-not-next-cursor','operation-result.r2',{'kind':'history.events','value':mismatch},False,True)
case('reject-r2-command-aggregate-revision','operation-result.r2',{'kind':'history.events','value':page([{**command_event,'aggregateRevision':'2'}])},False,True)
case('reject-r2-command-aggregate-other-command','operation-result.r2',{'kind':'history.events','value':page([{**command_event,'aggregate':{'kind':'command','id':'cmd-b'}}])},False,True)
case('reject-r2-command-kind-without-command','operation-result.r2',{'kind':'history.events','value':page([{**work_event,'aggregateRevisionKind':'Command'}])},False,True)
attribution={'actorId':'actor-a','occurredAtMs':'1793570000000','repositoryId':'repo-a','revision':'a'*40}
lineage=sample('LineageRecord');evidence=sample('EvidenceLink');vcs=sample('AttributedGitLink')
case('r2-result-history.work_lineage','operation-result.r2',{'kind':'history.work_lineage','value':page([{**lineage,**attribution},lineage])})
case('r2-result-history.evidence_links','operation-result.r2',{'kind':'history.evidence_links','value':page([{**evidence,'actorId':'actor-a','occurredAtMs':'1'}])})
case('r2-result-history.git_links','operation-result.r2',{'kind':'history.git_links','value':page([{**vcs,'actorId':'actor-a','occurredAtMs':'1'}])})
case('reject-r1-lineage-attribution','operation-result',{'kind':'history.work_lineage','value':page([{**lineage,**attribution}],None)},False,False)
case('reject-r1-vcs-link-actor','operation-result',{'kind':'history.git_links','value':page([{**vcs,'actorId':'actor-a'}],None)},False,False)
case('reject-r2-vcs-link-revision-member','operation-result.r2',{'kind':'history.git_links','value':page([{**vcs,'revision':'a'*40}])},False,False)
usage=sample('UsageItem')
tool={'sequence':'4','callId':'entry-call-a','sessionId':'session-a','toolName':'tasks.edit','outcome':'Failed','durationMs':'120','errorCode':'conflict','actorId':'actor-a','occurredAtMs':'1793570000000'}
waiting={'sequence':'4','callId':'entry-call-b','sessionId':'session-a','toolName':'releases.publish','outcome':'ApprovalRequired','operationId':'operation-a','actorId':'actor-a','occurredAtMs':'1793570000000'}
case('r2-result-history.tool_usage','operation-result.r2',{'kind':'history.tool_usage','value':{**page([usage]),'itemSequences':['5'],'toolInvocations':[tool,waiting]}})
case('reject-r2-usage-sequence-count','operation-result.r2',{'kind':'history.tool_usage','value':{**page([usage]),'itemSequences':[],'toolInvocations':[]}},False,True)
case('reject-r1-usage-sequences','operation-result',{'kind':'history.tool_usage','value':{**page([usage],None),'itemSequences':['5']}},False,False)
case('reject-r2-tool-invocation-without-sequence','operation-result.r2',{'kind':'history.tool_usage','value':{**page([]),'toolInvocations':[{k:v for k,v in tool.items() if k!='sequence'}]}},False,False)
case('r1-result-history.tool_usage-usage','operation-result',{'kind':'history.tool_usage','value':page([usage],None)})
case('reject-r1-tool-invocations','operation-result',{'kind':'history.tool_usage','value':{**page([usage],None),'toolInvocations':[tool]}},False,False)
case('reject-r2-tool-invocation-outcome','operation-result.r2',{'kind':'history.tool_usage','value':{**page([]),'toolInvocations':[{**tool,'outcome':'Succeeded'}]}},False,False)
case('reject-r2-usage-page-continuation','operation-result.r2',{'kind':'history.tool_usage','value':{'items':[],'hasMore':True,'checkpoint':'cursor-a'}},False,True)
def domain(name,revision=2,**extra):
 obj=request(name,'Query')
 if revision is not None:obj['schemaRevision']=revision
 obj['body'].update(extra);return obj
for name in ['history.work_lineage','history.git_links','history.evidence_links','history.tool_usage']:
 case('r2-input-'+name,'domain-request.r2',domain(name))
case('r2-input-sessions.briefing','domain-request.r2',domain('sessions.briefing',item='root-a',paths=['src/lib.rs','crates/core'],budgetTokens=1500))
case('r2-input-sessions.briefing-plain','domain-request.r2',domain('sessions.briefing'))
case('r1-input-sessions.briefing-explicit','domain-request',domain('sessions.briefing',1))
case('reject-r1-briefing-budget','domain-request',domain('sessions.briefing',None,budgetTokens=1500),False,False)
case('reject-r1-briefing-item','domain-request',domain('sessions.briefing',1,item='root-a'),False,False)
case('reject-r2-briefing-not-negotiated','domain-request.r2',domain('sessions.briefing',None,paths=['src']),False,False)
case('reject-r2-briefing-zero-budget','domain-request.r2',domain('sessions.briefing',budgetTokens=0),False,False)
case('reject-r2-briefing-budget-above-bound','domain-request.r2',domain('sessions.briefing',budgetTokens=1000001),False,False)
case('reject-r2-briefing-empty-paths','domain-request.r2',domain('sessions.briefing',paths=[]),False,False)
case('reject-r2-briefing-absolute-path','domain-request.r2',domain('sessions.briefing',paths=['/etc/passwd']),False,False)
case('reject-r2-briefing-parent-path','domain-request.r2',domain('sessions.briefing',paths=['src/../secrets']),False,False)
case('reject-r2-briefing-duplicate-paths','domain-request.r2',domain('sessions.briefing',paths=['src','src']),False,False)
for i,bad in enumerate(['C:/Windows','c:relative','C:\\Windows','\\root','\\\\server\\share','src\\..\\secrets','..\\up','a\\.\\b','a\\\\b','a/\\b','nul\u0000byte']):
 case(f'reject-r2-briefing-platform-path-{i}','domain-request.r2',domain('sessions.briefing',paths=[bad]),False,False)
case('r2-briefing-literal-separators-in-names','domain-request.r2',domain('sessions.briefing',paths=['src/name:part.rs','docs/a..b','x.']))
usage_scope=lambda **kw:domain('history.tool_usage',**kw)
def unrooted(**kw):
 obj=usage_scope(**kw);obj['body'].pop('rootId');return obj
case('r2-input-history.tool_usage-project','domain-request.r2',unrooted(projectId='project-a'))
case('r2-input-history.tool_usage-workspace','domain-request.r2',unrooted(workspace=True))
case('reject-r1-tool_usage-project','domain-request',{**unrooted(projectId='project-a'),'schemaRevision':1},False,False)
case('reject-r2-tool_usage-root-and-project','domain-request.r2',usage_scope(projectId='project-a'),False,False)
case('reject-r2-tool_usage-no-scope','domain-request.r2',unrooted(),False,False)
case('reject-r2-tool_usage-workspace-false','domain-request.r2',unrooted(workspace=False),False,False)
case('reject-r2-briefing-null-item','domain-request.r2',domain('sessions.briefing',item=None),False,False)
case('r2-result-sessions.briefing','operation-result.r2',{'kind':'sessions.briefing','value':{'items':[sample('ContextSource')],'hasMore':False}})
# Schema revision 3 (artifacts): the workspace owner scope for saves and listings, and
# artifacts.tag (a revision-1 operation) replacing an existing artifact's tag set.
def scoped(base,path,revision,workspace=True,project=DELETE):
 obj=copy.deepcopy(base)
 if revision is not None:obj['schemaRevision']=revision
 body=obj[path]
 if project is DELETE:body.pop('projectId',None)
 elif project is not None:body['projectId']=project
 if workspace is not None:body['workspace']=workspace
 return obj
case('r3-save-workspace','artifact-command-request.r3',scoped(r2(save,'command',['evidence']),'command',3))
case('r3-save-project','artifact-command-request.r3',scoped(save,'command',3,None,None))
case('r3-list-workspace','artifact-query-request.r3',scoped(r2(listing,'query',['evidence']),'query',3))
case('r3-list-project','artifact-query-request.r3',scoped(listing,'query',3,None,None))
case('reject-r2-save-workspace','artifact-command-request.r2',scoped(save,'command',2),False,False)
case('reject-r1-list-workspace','artifact-query-request',scoped(listing,'query',None),False,False)
case('reject-r3-save-project-and-workspace','artifact-command-request.r3',scoped(save,'command',3,True,'project-a'),False,False)
case('reject-r3-save-no-scope','artifact-command-request.r3',scoped(save,'command',3,None),False,False)
case('reject-r3-save-workspace-false','artifact-command-request.r3',scoped(save,'command',3,False),False,False)
case('reject-r3-list-null-workspace','artifact-query-request.r3',scoped(listing,'query',3,None)|{'query':{**scoped(listing,'query',3,None)['query'],'workspace':None}},False,False)
tag=request('artifacts.tag','Command')
case('artifacts.tag-clears','domain-request',{**tag,'body':{**tag['body'],'tags':[]}})
case('reject-artifacts.tag-duplicates','domain-request',{**tag,'body':{**tag['body'],'tags':['a','a']}},False,False)
case('reject-artifacts.tag-too-many','domain-request',{**tag,'body':{**tag['body'],'tags':['t'+str(i) for i in range(33)]}},False,False)
case('reject-artifacts.tag-missing-revision','domain-request',{**tag,'body':{k:v for k,v in tag['body'].items() if k!='expectedRevision'}},False,False)
# Schema revision 2 (resources): purpose, priority and create-time capacity/labels on acquire;
# scope, occupancy, lifecycle, queue position and ownership on the records.
def command_at(name,revision=2,**extra):
 obj=request(name,'Command')
 if revision is not None:obj['schemaRevision']=revision
 obj['body'].update(extra);return obj
acquire=dict(purpose='Run the desktop suite',priority='High',capacity=1,labels=['desktop'])
case('r2-input-resources.acquire','domain-request.r2',command_at('resources.acquire',**acquire))
case('r2-input-resources.acquire-plain','domain-request.r2',command_at('resources.acquire'))
case('r1-input-resources.acquire-explicit','domain-request',command_at('resources.acquire',1))
case('reject-r1-acquire-purpose','domain-request',command_at('resources.acquire',None,purpose='p'),False,False)
case('reject-r1-explicit-acquire-priority','domain-request',command_at('resources.acquire',1,priority='High'),False,False)
case('reject-r2-acquire-not-negotiated','domain-request.r2',command_at('resources.acquire',None,priority='Low'),False,False)
case('reject-r2-acquire-unknown-priority','domain-request.r2',command_at('resources.acquire',priority='Urgent'),False,False)
case('reject-r2-acquire-zero-capacity','domain-request.r2',command_at('resources.acquire',capacity=0),False,False)
case('reject-r2-acquire-empty-purpose','domain-request.r2',command_at('resources.acquire',purpose=''),False,False)
case('reject-r2-acquire-null-priority','domain-request.r2',command_at('resources.acquire',priority=None),False,False)
for name in ['resources.renew','resources.release']:
 case('r2-input-'+name,'domain-request.r2',command_at(name))
for name in ['resources.list','resources.holders','resources.waiters']:
 case('r2-input-'+name,'domain-request.r2',domain(name))
lease_r2={**sample('LeaseRecord'),'units':1,'purpose':'Run the desktop suite','priority':'High','queuePosition':'1','requestedAtMs':'1793570000000','projectId':'project-a','rootId':'root-a','assignmentId':'assignment-a','sessionId':'session-a'}
ended={**sample('LeaseRecord'),'state':'Released','units':1,'priority':'Medium','requestedAtMs':'1','grantedAtMs':'2','renewedAtMs':'3','endedAtMs':'4','endReason':'Expired','ownerUnreachableSinceMs':'3'}
resource_r2={**sample('ResourceRecord'),'scope':'Machine','physical':True,'labels':['desktop'],'used':'1','available':False}
case('r2-result-resources.acquire','operation-result.r2',{'kind':'resources.acquire','value':lease_r2})
case('r2-result-resources.release-ended','operation-result.r2',{'kind':'resources.release','value':ended})
case('r2-result-resources.renew','operation-result.r2',{'kind':'resources.renew','value':{**lease_r2,'state':'Held','fence':'3','grantedAtMs':'2','renewedAtMs':'5'}})
case('r2-result-resources.holders','operation-result.r2',{'kind':'resources.holders','value':{'items':[lease_r2,ended],'hasMore':False}})
case('r2-result-resources.waiters','operation-result.r2',{'kind':'resources.waiters','value':{'items':[{**lease_r2,'state':'Waiting'}],'hasMore':False}})
case('r2-result-resources.list','operation-result.r2',{'kind':'resources.list','value':{'items':[resource_r2,{**sample('ResourceRecord'),'scope':'Project','projectId':'project-a','physical':False,'labels':[],'used':'0','available':True}],'hasMore':False}})
case('reject-r1-result-lease-purpose','operation-result',{'kind':'resources.acquire','value':lease_r2},False,False)
case('reject-r1-result-resource-scope','operation-result',{'kind':'resources.list','value':{'items':[resource_r2],'hasMore':False}},False,False)
case('reject-r2-result-lease-end-reason','operation-result.r2',{'kind':'resources.release','value':{**ended,'endReason':'Stolen'}},False,False)
case('reject-r2-result-resource-scope','operation-result.r2',{'kind':'resources.list','value':{'items':[{**resource_r2,'scope':'Global'}],'hasMore':False}},False,False)
# Schema revision 2 (session times): SessionRecord carries createdAtMs and lastActivityMs for
# Chat History date grouping; every operation returning a session negotiates revision 2.
case('r2-input-sessions.list','domain-request.r2',domain('sessions.list'))
case('r2-input-sessions.get','domain-request.r2',domain('sessions.get'))
for name in ['sessions.start','sessions.end']:
 case('r2-input-'+name,'domain-request.r2',command_at(name))
case('r2-input-sessions.append_message','session-command-request',{'protocol':'1.0-draft.1','requestKey':'request-a','sessionId':'session-a','expectedSessionRevision':'1','content':[{'kind':'Text','text':'Hello'}],'schemaRevision':2})
session_r2={**sample('SessionRecord'),'createdAtMs':'1793570000000','lastActivityMs':'1793570060000'}
for name in ['sessions.get','sessions.start','sessions.end','sessions.append_message']:
 case('r2-result-'+name,'operation-result.r2',{'kind':name,'value':session_r2})
case('r2-result-sessions.list','operation-result.r2',{'kind':'sessions.list','value':{'items':[session_r2,sample('SessionRecord')],'hasMore':False}})
case('reject-r1-result-session-times','operation-result',{'kind':'sessions.get','value':session_r2},False,False)
case('reject-r2-result-session-negative-time','operation-result.r2',{'kind':'sessions.get','value':{**session_r2,'createdAtMs':'-1'}},False,False)
# Schema revision 2 (workspace diagnostics): log filters, attributed run detail on reported
# diagnostics, and health state, metrics (an unavailable value is marked, never zero) and safe
# recovery links on the records.
logs=dict(severity='Warning',outcome='Failed',operation='indexing.repository',operationId='operation-a',correlationId='correlation-a',targetId='repo-a',stage='parse',projectId='project-a',search='timeout',sinceMs='1',untilMs='2',order='Descending')
case('r2-input-workspace.logs','domain-request.r2',domain('workspace.logs',**logs))
case('r2-input-workspace.logs-plain','domain-request.r2',domain('workspace.logs'))
case('r2-input-workspace.diagnostics','domain-request.r2',domain('workspace.diagnostics'))
case('reject-r1-logs-severity','domain-request',domain('workspace.logs',None,severity='Error'),False,False)
case('reject-r2-logs-unknown-order','domain-request.r2',domain('workspace.logs',order='Sideways'),False,False)
case('reject-r2-logs-empty-search','domain-request.r2',domain('workspace.logs',search=''),False,False)
run=dict(operation='indexing.repository',operationId='operation-a',targetKind='repository',targetId='repo-a',stage='parse',sourceRevision='0123abc',attempt='2',startedAtMs='1',finishedAtMs='2',progressCompleted='3',progressTotal='5',outcome='Retryable',errorClass='Timeout',output='helper stderr tail')
case('r2-input-workspace.record_diagnostic','domain-request.r2',command_at('workspace.record_diagnostic',**run))
case('r2-input-workspace.record_diagnostic-plain','domain-request.r2',command_at('workspace.record_diagnostic'))
case('reject-r1-record-diagnostic-stage','domain-request',command_at('workspace.record_diagnostic',None,stage='parse'),False,False)
case('reject-r2-record-diagnostic-unknown-outcome','domain-request.r2',command_at('workspace.record_diagnostic',outcome='Exploded'),False,False)
case('reject-r2-record-diagnostic-unknown-class','domain-request.r2',command_at('workspace.record_diagnostic',errorClass='Gremlins'),False,False)
metric_ok={'name':'walBytes','availability':'Available','value':'4096','unit':'bytes'}
metric_na={'name':'freeBytes','availability':'Unavailable','unit':'bytes','reason':'the volume did not report free space'}
diag_r2={**sample('Diagnostic'),'sequence':'7','recordedAtMs':'1793570000000','actorId':'actor-a','origin':'Host','projectId':'project-a',**{k:v for k,v in run.items() if k!='operationId'},'operationId':'operation-a','health':'Degraded','outputOmittedBytes':'1024','redactions':'1','metrics':[metric_ok,metric_na],'recovery':[{'operation':'queue.retry','label':'Retry the failed delivery','targetKind':'queue_delivery','targetId':'delivery-a'}]}
case('r2-result-workspace.logs','operation-result.r2',{'kind':'workspace.logs','value':{'items':[diag_r2],'hasMore':False}})
case('r2-result-workspace.diagnostics','operation-result.r2',{'kind':'workspace.diagnostics','value':{'items':[{**sample('Diagnostic'),'health':'Unavailable','metrics':[metric_na]}],'hasMore':False}})
case('r2-result-workspace.record_diagnostic','operation-result.r2',{'kind':'workspace.record_diagnostic','value':{**diag_r2,'origin':'Reported'}})
case('reject-r1-result-diagnostic-health','operation-result',{'kind':'workspace.diagnostics','value':{'items':[{**sample('Diagnostic'),'health':'Healthy'}],'hasMore':False}},False,False)
case('reject-r2-result-diagnostic-unknown-health','operation-result.r2',{'kind':'workspace.diagnostics','value':{'items':[{**sample('Diagnostic'),'health':'Fine'}],'hasMore':False}},False,False)
case('r2-result-workspace.logs-tail','operation-result.r2',{'kind':'workspace.logs','value':{'items':[],'hasMore':False,'latestSequence':'7'}})
case('reject-r1-result-logs-tail','operation-result',{'kind':'workspace.logs','value':{'items':[],'hasMore':False,'latestSequence':'7'}},False,False)
case('reject-r2-result-unavailable-metric-zero','operation-result.r2',{'kind':'workspace.diagnostics','value':{'items':[{**sample('Diagnostic'),'metrics':[{**metric_na,'value':'0'}]}],'hasMore':False}},False,True)
case('reject-r2-result-available-metric-without-value','operation-result.r2',{'kind':'workspace.diagnostics','value':{'items':[{**sample('Diagnostic'),'metrics':[{k:v for k,v in metric_ok.items() if k!='value'}]}],'hasMore':False}},False,True)
case('reject-r2-result-progress-over-total','operation-result.r2',{'kind':'workspace.record_diagnostic','value':{**diag_r2,'progressCompleted':'6'}},False,True)
case('reject-r2-result-diagnostic-page-continuation','operation-result.r2',{'kind':'workspace.logs','value':{'items':[],'hasMore':True}},False,True)
case('reject-r2-result-metric-unknown-availability','operation-result.r2',{'kind':'workspace.logs','value':{'items':[{**sample('Diagnostic'),'metrics':[{**metric_ok,'availability':'Maybe'}]}],'hasMore':False}},False,False)
# Schema revision 2 (addons): effective availability apart from configured enablement, kind,
# languages, targets, prerequisites, requested/effective access, configuration, state generation,
# the pending version with its waits and proposal, the rollback predecessor, trust and failure.
prereq={'kind':'package','id':'dotnet-runtime-pack','requirement':'^1.2','required':True,'satisfied':False,'state':'Missing'}
runtime_prereq={'kind':'runtime','id':'dotnet','requirement':'>=8','required':True,'satisfied':True,'state':'Ready','detectedVersion':'8.0.204'}
offer={'version':'1.4.0','manifestDigest':'f'*64,'bundleDigest':'e'*64,'bundleBytes':'1048576','publisherId':'acme','source':'https://packs.example/devworks-index.json','hostTargetSupported':True,'newer':True}
package_r2={**sample('PackageRecord'),'installed':True,'registryRevision':'4','availability':'MissingDependency','availabilityReason':'Required package dotnet-runtime-pack is not installed','packageKind':'LanguagePack','contributions':['dotnet-pack/indexer','dotnet-pack/settings'],'languages':['csharp'],'targets':['aarch64-apple-darwin','x86_64-pc-windows-msvc'],'hostTargetSupported':True,'prerequisites':[prereq,runtime_prereq],'available':[offer],'requestedAccess':['worktree.read','process:dotnet'],'effectiveAccess':['worktree.read'],'configurationRevision':'2','configurationSchema':{'schemaId':'dotnet-pack/config','revision':'1','sha256':'b'*64},'stateGeneration':'3','pendingVersion':'1.3.0','pendingDigest':'c'*64,'pendingWaits':['ExactVersionApproval','Quiescence'],'pendingBundle':ART,'pendingProposalId':'proposal-a','predecessorVersion':'1.1.0','predecessorDigest':'d'*64,'publisherTrusted':False,'signerKey':'e'*64,'failureCode':'StartupTimeout'}
for name in ['addons.install','addons.configure','addons.enable','addons.disable','addons.update','addons.rollback','addons.uninstall']:
 case('r2-result-'+name,'operation-result.r2',{'kind':name,'value':package_r2})
case('r2-result-addons.catalog','operation-result.r2',{'kind':'addons.catalog','value':{'items':[package_r2,{**sample('PackageRecord'),'availability':'Ready'}],'hasMore':False}})
addon_art={'artifactId':'artifact-a','versionId':'version-a','sha256':'a'*64,'byteLength':'8','mediaType':'application/vnd.devworks.package'}
for kind,command in [('Install',{'bundle':addon_art,'expectedRegistryRevision':'1'}),('Update',{'packageId':'fixture','bundle':addon_art,'expectedRegistryRevision':'1'}),('Enable',{'packageId':'fixture','expectedRegistryRevision':'1'}),('Disable',{'packageId':'fixture','expectedRegistryRevision':'1'}),('Rollback',{'packageId':'fixture','targetManifestDigest':'a'*64,'expectedRegistryRevision':'1','expectedStateGeneration':'1'}),('Uninstall',{'packageId':'fixture','expectedRegistryRevision':'1'})]:
 case('r2-input-addons.'+kind.lower(),'addon-command-request',{'protocol':'1.0-draft.1','requestKey':'addon-a','command':{'kind':kind,**command},'schemaRevision':2})
case('r2-input-addons.catalog','domain-request.r2',domain('addons.catalog'))
case('r2-input-addons.configure','domain-request.r2',command_at('addons.configure'))
case('reject-r1-result-package-availability','operation-result',{'kind':'addons.catalog','value':{'items':[package_r2],'hasMore':False}},False,False)
case('reject-r2-result-package-unknown-availability','operation-result.r2',{'kind':'addons.install','value':{**package_r2,'availability':'Starting'}},False,False)
case('reject-r2-result-package-unknown-prerequisite-state','operation-result.r2',{'kind':'addons.catalog','value':{'items':[{**package_r2,'prerequisites':[{**prereq,'state':'Sort-of'}]}],'hasMore':False}},False,False)
case('reject-r2-result-package-offer-missing-digest','operation-result.r2',{'kind':'addons.catalog','value':{'items':[{**package_r2,'available':[{k:v for k,v in offer.items() if k!='manifestDigest'}]}],'hasMore':False}},False,False)
case('reject-r2-result-package-unknown-wait','operation-result.r2',{'kind':'addons.update','value':{**package_r2,'pendingWaits':['Coffee']}},False,False)
case('reject-r2-result-package-prerequisite-missing-satisfied','operation-result.r2',{'kind':'addons.install','value':{**package_r2,'prerequisites':[{k:v for k,v in prereq.items() if k!='satisfied'}]}},False,False)
# Schema revision 2 (router availability): with sessionId, the session's current pin, whether
# it is eligible for the asked intent/effort/capabilities and why it was last set.
case('r2-input-router.availability','domain-request.r2',domain('router.availability',sessionId='session-a'))
case('r2-input-router.availability-plain','domain-request.r2',domain('router.availability'))
case('reject-r1-availability-session','domain-request',domain('router.availability',None,sessionId='session-a'),False,False)
case('reject-r2-availability-null-session','domain-request.r2',domain('router.availability',sessionId=None),False,False)
pin_choice={**sample('RouteChoice'),'requestedEffort':'High','effectiveEffort':'High'}
availability_r2={**sample('RouteAvailability'),'pin':pin_choice,'pinEligible':True,'pinReason':'Failover'}
case('r2-result-router.availability','operation-result.r2',{'kind':'router.availability','value':availability_r2})
case('r2-result-router.availability-no-pin','operation-result.r2',{'kind':'router.availability','value':sample('RouteAvailability')})
case('reject-r1-result-availability-pin','operation-result',{'kind':'router.availability','value':availability_r2},False,False)
case('reject-r2-result-availability-pin-effort','operation-result.r2',{'kind':'router.availability','value':{**availability_r2,'pin':{**pin_choice,'effectiveEffort':'Low'}}},False,True)
(D/'domain-fixtures.json').write_text(json.dumps(CASES,indent=2)+'\n')
print(json.dumps({'fixtures':len(CASES),'positive':sum(c['typedValid'] for c in CASES),'negative':sum(not c['typedValid'] for c in CASES),'inputOperationModes':sum(2 if d['mode']=='Hybrid' else 1 for d in OPS.values()),'resultOperations':len(OPS)+len(LEGACY_RESULTS)}))
