"""Deterministic provider/session/package candidates. No live provider or app effects."""
from copy import deepcopy as cp
import json
from pathlib import Path
ROOT=Path(__file__).resolve().parents[2];OUT=ROOT/'docs/architecture/contracts';P='1.0-draft.1';A='a'*64
CASES=[]
def add(name,entry,value,typed=True,schema=None):CASES.append({'id':name,'entry':entry,'raw':value if isinstance(value,str) else json.dumps(value,ensure_ascii=False,separators=(',',':')),'typedValid':typed,'schemaValid':typed if schema is None else schema})
def mut(name,entry,value,fn,typed=False,schema=None):v=cp(value);fn(v);add(name,entry,v,typed,schema)
artifact={'artifactId':'artifact-a','versionId':'version-a','sha256':A,'byteLength':'2','mediaType':'application/json'}
schema={'schemaId':'core/object','revision':'1','sha256':A}
binding={'connectionId':'connection-a','accountRevision':'1','endpointProfile':'OpenAiResponses','modelId':'model-a','contextEpoch':'0'}
call={'call':{'attemptId':'attempt-a','nativeCallId':'call:a','ordinal':'0'},'capabilityId':'core/read','inputSchema':schema,'arguments':artifact,'nativeItem':artifact}
options={'selection':{'kind':'Auto'},'deadlineMs':60000}
spec={'options':options,'input':{'kind':'Generation','sessionId':'session-a','context':artifact,'contextEpoch':'0','tools':[{'capabilityId':'core/read','wireName':'read_file','inputSchema':schema}],'responseHeadroomTokens':'100','requiresImages':False,'requiresParallelTools':True}}
request={'protocol':P,'requestId':'request-a','purpose':'Implementation','attribution':{'kind':'Feature','projectId':'project-a','rootId':'root-a','jobRunId':'job-a'},'authorityRevision':'1','controlGeneration':'1','instructionRevision':'1','cancellationRevision':'1',**spec}
add('generation-spec','inference-spec',spec);add('host-owned-feature-attribution','inference-request',request)
mut('reject-broker-attribution-injection','inference-spec',spec,lambda v:v.update({'attribution':{'kind':'Job','runId':'job-a'}}))
mut('reject-provider-secret-input','inference-request',request,lambda v:v.update({'apiKey':'forged'}))
mut('reject-ambiguous-tool-alias','inference-request',request,lambda v:v['input']['tools'].append(cp(v['input']['tools'][0])),schema=True)
for purpose in ['Conversation','Planning','Review','Dreaming','Compaction','Title','Vision','Semantic','Package']:mut('purpose-'+purpose,'inference-request',request,lambda v,p=purpose:v.update({'purpose':p}),True)
embedding={'protocol':P,'requestId':'embed-a','purpose':'Embedding','attribution':{'kind':'Service','operationId':'op-a'},'options':options,'authorityRevision':'1','controlGeneration':'1','instructionRevision':'1','cancellationRevision':'1','input':{'kind':'Embedding','embeddingSpaceId':'space-a','dimensions':384,'inputs':[artifact]}}
add('embedding-no-fake-session','inference-request',embedding)
mut('reject-embedding-purpose-mismatch','inference-request',embedding,lambda v:v.update({'purpose':'Conversation'}),schema=True)
mut('reject-empty-embedding-batch','inference-request',embedding,lambda v:v['input'].update({'inputs':[]}))
event={'protocol':P,'requestId':'request-a','attemptId':'attempt-a','binding':binding,'sequence':'1','event':{'kind':'ToolCallComplete','toolCall':call}}
add('complete-tool-observation','provider-event',event)
for name,body in [('text',{'kind':'TextDelta','blockIndex':'0','text':'Hello'}),('native',{'kind':'NativeContinuation','binding':binding,'artifact':artifact}),('terminal',{'kind':'Terminal','outcome':'ToolCallsReady','nativeStopReason':'tool_calls'}),('heartbeat',{'kind':'Heartbeat'})]:
 v=cp(event);v['event']=body;add('provider-'+name,'provider-event',v)
for name,cost in [('reported-zero',{'kind':'Reported','amount':'0','currency':'USD'}),('subscription-estimate',{'kind':'Estimated','amount':'0.025','currency':'USD','priceSnapshotId':'price-a','subscriptionEquivalent':True}),('local-free',{'kind':'Free','basis':'LocalProvider'}),('untracked',{'kind':'Untracked','reason':'no-price'}),('unknown',{'kind':'Unknown','reason':'interrupted'})]:
 v=cp(event);v['event']={'kind':'Usage','observation':{'inputTokens':'0','cost':cost}};add('usage-'+name,'provider-event',v)
mut('reject-foreign-call-attempt','provider-event',event,lambda v:v['event']['toolCall']['call'].update({'attemptId':'attempt-other'}),schema=True)
v=cp(event);v['event']={'kind':'NativeContinuation','binding':cp(binding),'artifact':artifact}
mut('reject-cross-connection-opaque-replay','provider-event',v,lambda x:x['event']['binding'].update({'connectionId':'connection-other'}),schema=True)
codec={'requestId':'request-a','attemptId':'attempt-a','sequence':'1','event':{'kind':'Usage','observation':{'reportedCost':{'amount':'0','currency':'USD'}}}}
add('codec-reported-zero','provider-codec-event',codec)
mut('reject-codec-free-cost-authority','provider-codec-event',codec,lambda v:v['event']['observation'].update({'cost':{'kind':'Free','basis':'LocalProvider'}}))
mut('reject-codec-billing-owner','provider-codec-event',codec,lambda v:v.update({'attribution':{'kind':'Job','runId':'job-a'}}))
msg={'protocol':P,'requestKey':'message-a','sessionId':'session-a','expectedSessionRevision':'1','content':[{'kind':'Text','text':'Continue this work'},{'kind':'Attachment','artifact':artifact}]}
add('session-user-message','session-command-request',msg)
for field in ['role','actorId','authorityReceipt','hostNotice']:mut('reject-message-'+field,'session-command-request',msg,lambda v,f=field:v.update({f:'forged'}))
add('transcript-query','session-query-request',{'protocol':P,'queryId':'q-a','sessionId':'session-a','limit':100})
add('transcript-query-too-large','session-query-request',{'protocol':P,'queryId':'q-a','sessionId':'session-a','limit':501},False)
def batch(bid,entries,expected='1',epoch='0'):return {'protocol':P,'batchId':bid,'sessionId':'session-a','contextEpoch':epoch,'expectedNextSequence':expected,'entries':entries}
def entry(eid,seq,body):return {'entryId':eid,'sequence':str(seq),'body':body}
entries=[entry('entry-call',1,{'kind':'ToolCallRecorded','toolCall':call}),entry('entry-response',2,{'kind':'ToolResponseRecorded','call':call['call'],'outcome':{'kind':'ApprovalRequired','approvalId':'approval-a','operationId':'op-a'}})]
b=batch('batch-a',entries);add('durable-call-and-wait-response','transcript-batch',b)
mut('reject-gapped-transcript-batch','transcript-batch',b,lambda v:v['entries'][1].update({'sequence':'3'}),schema=True)
mut('reject-duplicate-entry-id','transcript-batch',b,lambda v:v['entries'][1].update({'entryId':'entry-call'}),schema=True)
receipt={'commandId':'cmd-a','committedSequence':'3','acceptedAtMs':'1','requestDigest':A,'decision':{'kind':'MessageSaved','sessionId':'session-a','sessionRevision':'2','entryId':'entry-user','sequence':'3'}}
add('session-command-receipt','session-command-reply',{'kind':'Recorded','protocol':P,'receipt':receipt,'replayed':False})
page={'protocol':P,'queryId':'q-a','stamp':{'storeId':'store-a','restoreEpoch':'0','sequence':'9','policyRevision':'1','scopeRevision':'1'},'sessionId':'session-a','entries':entries,'nextCursor':'cursor-a','hasMore':False}
add('transcript-page','transcript-page',page)
mut('reject-unordered-transcript-page','transcript-page',page,lambda v:v['entries'].reverse(),schema=True)
def meta(name):return {'id':'fixture/'+name,'inputSchema':schema,'outputSchema':schema,'helpTopic':'fixture-guide','required':True,'cancellation':'Cooperative'}
contribs=[
 {'kind':'Semantic','meta':meta('semantic'),'languages':['rust'],'format':'Scip'},
 {'kind':'Tool','meta':meta('tool'),'requestedEffect':'ReadCandidate'},
 {'kind':'Workflow','meta':meta('workflow'),'role':'VerificationGate','evidenceSchema':schema},
 {'kind':'EventHandler','meta':meta('event'),'eventSchemas':[schema]},
 {'kind':'ContextHook','meta':meta('context'),'phase':'Compact','priority':0},
 {'kind':'Content','meta':meta('content'),'contentKind':'Skill','artifact':artifact},
 {'kind':'Provider','meta':meta('provider'),'profiles':['OpenAiResponses'],'endpointIds':['endpoint-a']},
 {'kind':'NativeUi','meta':meta('ui'),'view':{'kind':'Group','children':[{'kind':'Text','text':'Package report'},{'kind':'Field','fieldId':'name','label':'Name','fieldKind':'Text','readOnly':False},{'kind':'Action','actionId':'fixture/tool','label':'Inspect'},{'kind':'Table','columns':['name'],'dataSchema':schema},{'kind':'Image','artifact':artifact}]}},
]
manifest={'packageId':'fixture','version':'1.2.3','publisherId':'publisher-a','protocolMin':{'major':1,'minor':0},'protocolMax':{'major':1,'minor':0},'minimumHostVersion':'1.0.0','dependencies':[],'runtimes':[],'schemaAssets':[],'requestedLimits':{'maxConcurrent':4,'queueCapacity':32,'maxMemoryBytes':'268435456','streamCreditBytes':262144},'targets':[{'targetTriple':'aarch64-apple-darwin','entrypoint':'bin/helper'}],'files':[{'path':'bin/helper','sha256':A,'byteLength':'100'},{'path':'help.md','sha256':A,'byteLength':'20'}],'helpFile':'help.md','stateSchema':schema,'configurationSchema':schema,'requestedPermissions':[{'kind':'WorktreeRead'},{'kind':'Inference'},{'kind':'StateWrite'},{'kind':'UiContribution'}],'contributions':contribs}
add('all-eight-contribution-categories','package-manifest',manifest)
for i,c in enumerate(contribs):v=cp(manifest);v['contributions']=[c];add('contribution-'+c['kind'],'package-manifest',v)
mut('reject-contribution-namespace-hijack','package-manifest',manifest,lambda v:v['contributions'][0]['meta'].update({'id':'core/semantic'}),schema=True)
mut('reject-aliased-package-files','package-manifest',manifest,lambda v:v['files'].append({'path':'BIN/helper','sha256':A,'byteLength':'1'}),schema=True)
mut('reject-package-traversal','package-manifest',manifest,lambda v:v['files'][0].update({'path':'../helper'}),schema=True)
mut('reject-missing-entrypoint','package-manifest',manifest,lambda v:v['targets'][0].update({'entrypoint':'bin/absent'}),schema=True)
mut('reject-wrong-entrypoint-case','package-manifest',manifest,lambda v:v['targets'][0].update({'entrypoint':'BIN/helper'}),schema=True)
mut('reject-bad-semver','package-manifest',manifest,lambda v:v.update({'version':'latest'}),schema=True)
mut('reject-self-dependency','package-manifest',manifest,lambda v:v['dependencies'].append({'packageId':'fixture','versionRequirement':'^1','manifestDigest':A,'required':True}),schema=True)
mut('reject-supplied-trust-receipt','package-manifest',manifest,lambda v:v.update({'trustedPublisher':True}))
version={'major':1,'minor':0}
initialize={'jsonrpc':'2.0','id':'session-a:h:1','method':'host.initialize','params':{'processSessionId':'session-a','offeredMin':version,'offeredMax':version,'expectedManifestDigest':A,'requiredFeatures':['cancel','credits'],'maxFrameBytes':1048576}}
activate={'jsonrpc':'2.0','id':'session-a:h:2','method':'host.activate','params':{'processSessionId':'session-a','registryRevision':'1','stateGeneration':'1','configurationRevision':'1'}}
invoke={'jsonrpc':'2.0','id':'session-a:h:3','method':'host.invoke','params':{'processSessionId':'session-a','invocationId':'invocation-a','invocationToken':'token-a','contributionId':'fixture/tool','scope':{'kind':'Root','projectId':'project-a','rootId':'root-a'},'allowedOperations':['core/read'],'registryRevision':'1','stateGeneration':'1','authorityRevision':'1','controlGeneration':'1','operationId':'op-a','deadlineMs':1000,'input':artifact}}
cancel={'jsonrpc':'2.0','id':'session-a:h:4','method':'host.cancel','params':{'processSessionId':'session-a','invocationId':'invocation-a','cancellationRevision':'1'}}
shutdown={'jsonrpc':'2.0','id':'session-a:h:5','method':'host.shutdown','params':{'processSessionId':'session-a','deadlineMs':1000}}
for name,v in [('initialize',initialize),('activate',activate),('invoke',invoke),('cancel',cancel),('shutdown',shutdown)]:add('rpc-'+name,'package-host-request',v)
mut('reject-host-unknown-major','package-host-request',initialize,lambda v:v['params'].update({'offeredMax':{'major':2,'minor':0}}),schema=True)
mut('reject-host-credential-argument','package-host-request',invoke,lambda v:v['params'].update({'credential':'forged'}))
broker={'jsonrpc':'2.0','id':'session-a:p:1','method':'broker.call','params':{'processSessionId':'session-a','invocationId':'invocation-a','invocationToken':'token-a','actionKey':'read-input','call':{'kind':'ArtifactRead','artifact':artifact}}}
add('broker-artifact-read','package-request',broker)
for kind,callbody in [('application',{'kind':'Application','operation':'core/read','inputSchema':schema,'arguments':artifact}),('inference',{'kind':'Inference','inputSchema':schema,'request':artifact}),('http',{'kind':'Http','connectionHandle':'connection-handle','endpointId':'endpoint-a','httpMethod':'Post','relativePath':'/v1/messages','body':artifact}),('write-artifact',{'kind':'ArtifactWrite','proposal':artifact})]:v=cp(broker);v['params']['call']=callbody;add('broker-'+kind,'package-request',v)
mut('reject-broker-scope-injection','package-request',broker,lambda v:v['params'].update({'rootId':'root-other'}))
v=cp(broker);v['params']['call']={'kind':'Http','connectionHandle':'connection-handle','endpointId':'endpoint-a','httpMethod':'Get','relativePath':'https://other.invalid/'};add('reject-broker-absolute-url','package-request',v,False,True)
for action in [{'kind':'Read'},{'kind':'CompareAndSet','expectedRevision':'1','proposal':artifact}]:add('broker-state-'+action['kind'],'package-request',{'jsonrpc':'2.0','id':'session-a:p:2','method':'broker.state','params':{'processSessionId':'session-a','invocationId':'invocation-a','invocationToken':'token-a','actionKey':'state-a','stateGeneration':'1','action':action}})
notification={'jsonrpc':'2.0','method':'package.progress','params':{'processSessionId':'session-a','invocationId':'invocation-a','invocationToken':'token-a','sequence':'1','text':'Working','droppedCount':'0'}}
commit={'jsonrpc':'2.0','id':'session-a:p:3','method':'broker.commit_event','params':{'processSessionId':'session-a','invocationId':'invocation-a','invocationToken':'token-a','subscriptionId':'subscription-a','eventId':'event-a','expectedCheckpoint':'0','stateGeneration':'1','expectedStateRevision':'1','stateProposal':artifact,'intents':[{'actionKey':'follow-up','call':{'kind':'ArtifactWrite','proposal':artifact}}]}}
add('event-commit-state-intents-checkpoint','package-request',commit)
mut('reject-duplicate-event-intent','package-request',commit,lambda v:v['params']['intents'].append(cp(v['params']['intents'][0])),schema=True)
mut('event-commit-checkpoint-only','package-request',commit,lambda v:(v['params'].pop('stateProposal'),v['params'].update(intents=[])),True)
mut('reject-forged-event-processing-receipt','package-request',commit,lambda v:v['params'].update(processingReceipt='forged'))
mut('reject-missing-event-checkpoint','package-request',commit,lambda v:v['params'].pop('expectedCheckpoint'))
v=cp(commit);v['params']['intents'][0]['call']={'kind':'Http','connectionHandle':'connection-a','endpointId':'endpoint-a','httpMethod':'Post','relativePath':'https://other.invalid/'}
add('reject-event-intent-absolute-url','package-request',v,False,True)
add('event-commit-host-accepted','package-response',{'jsonrpc':'2.0','id':'session-a:p:3','result':{'kind':'EventCommitAccepted','processingReceipt':'receipt-a','checkpoint':'1','stateRevision':'2','operationIds':['operation-a']}})
add('progress-not-terminal','package-notification',notification)
mut('reject-notification-request-id','package-notification',notification,lambda v:v.update({'id':'not-a-notification'}))
credit={'jsonrpc':'2.0','method':'broker.streamCredit','params':{'processSessionId':'session-a','invocationId':'invocation-a','invocationToken':'token-a','streamId':'stream-a','sequence':'1','grantBytes':262144}}
add('bounded-stream-credit','package-notification',credit)
mut('reject-unbounded-credit','package-notification',credit,lambda v:v['params'].update({'grantBytes':262145}))
for name,body in [('codec',{'kind':'Provider','event':codec}),('artifact',{'kind':'ArtifactProduced','artifact':artifact}),('semantic',{'kind':'SemanticIndex','sourceSnapshot':artifact,'index':artifact,'format':'Scip'}),('context',{'kind':'ContextProposal','inputDigest':A,'proposal':artifact}),('verification',{'kind':'Verification','passed':True,'evidence':[artifact]}),('view',{'kind':'ViewUpdate','revision':'1','data':artifact}),('event',{'kind':'EventProcessed','subscriptionId':'sub-a','eventId':'event-a','processingReceipt':'receipt-a'})]:
 add('package-event-'+name,'package-notification',{'jsonrpc':'2.0','method':'package.event','params':{'processSessionId':'session-a','invocationId':'invocation-a','invocationToken':'token-a','sequence':'1','event':body}})
results=[{'kind':'Initialized','selected':version,'manifestDigest':A,'contributionIds':['fixture/tool'],'features':['cancel','credits']},{'kind':'Activated','registryRevision':'1','stateGeneration':'1'},{'kind':'InvocationFinished','invocationId':'invocation-a','outcome':{'kind':'Completed','artifacts':[artifact]}},{'kind':'InvocationFinished','invocationId':'invocation-a','outcome':{'kind':'OutcomeUnknown','operationId':'op-a'}},{'kind':'CancelAcknowledged','invocationId':'invocation-a'},{'kind':'ShutdownAcknowledged'},{'kind':'BrokerCompleted','artifact':artifact},{'kind':'StateSnapshot','stateGeneration':'1','revision':'1','artifact':artifact}]
for i,result in enumerate(results):add('rpc-result-'+str(i),'package-response',{'jsonrpc':'2.0','id':'session-a:h:1','result':result})
error={'jsonrpc':'2.0','id':'session-a:h:1','error':{'code':-32020,'message':'Unavailable','data':{'class':'Unavailable'}}};add('rpc-error','package-response',error)
mut('rpc-parse-error-null-id','package-response',error,lambda v:(v.update({'id':None}),v['error'].update({'code':-32700})),True)
mut('reject-both-result-and-error','package-response',error,lambda v:v.update({'result':results[0]}))
mut('reject-inappropriate-null-id','package-response',error,lambda v:v.update({'id':None}),schema=True)
for kind,command in [('Install',{'bundle':artifact,'expectedRegistryRevision':'1'}),('Update',{'packageId':'fixture','bundle':artifact,'expectedRegistryRevision':'1'}),('Enable',{'packageId':'fixture','expectedRegistryRevision':'1'}),('Disable',{'packageId':'fixture','expectedRegistryRevision':'1'}),('Rollback',{'packageId':'fixture','targetManifestDigest':A,'expectedRegistryRevision':'1','expectedStateGeneration':'1'}),('Uninstall',{'packageId':'fixture','expectedRegistryRevision':'1'})]:
 add('addon-'+kind,'addon-command-request',{'protocol':P,'requestKey':'addon-a','command':{'kind':kind,**command}})
receipt2=cp(receipt);receipt2['decision']={'kind':'Staged','packageId':'fixture','manifestDigest':A,'waits':['ExactVersionApproval','Quiescence']}
add('addon-staged-not-activated','addon-command-reply',{'kind':'Recorded','protocol':P,'receipt':receipt2,'replayed':False})
route={'requestId':'request-a','attemptId':'attempt-a','attemptNumber':'1','binding':binding,'accessMode':'Subscription','connectionRevision':'1','credentialRevision':'1','capabilityRevision':'1','catalogRevision':'1','policyRevision':'1','pinRevision':'1','reason':'SessionPin','capacityGrantId':'capacity-a','priceBasis':{'kind':'RateSnapshot','snapshotId':'price-a'}}
add('selected-route-with-revisions','route-decision',route)
envelope={'protocol':P,'route':route,'options':options,'requestArtifact':artifact,'contextDigest':A,'cacheEpoch':'0','nativeContinuation':[{'binding':cp(binding),'artifact':artifact}]}
add('durable-attempt-with-native-binding','attempt-envelope',envelope)
mut('reject-foreign-attempt-native-item','attempt-envelope',envelope,lambda v:v['nativeContinuation'][0]['binding'].update({'accountRevision':'2'}),schema=True)
mut('reject-persisted-authorization-header','attempt-envelope',envelope,lambda v:v.update({'authorization':'Bearer forbidden'}))
add('capability-unknown-is-explicit','capability-snapshot',{'binding':binding,'revision':'1','tools':'Supported','parallelTools':'Unknown','inputImages':'Supported','structuredOutput':'Unknown','embeddings':'Unsupported','cancellation':'Unknown','requestObservation':'Unsupported','cacheMetrics':'Unknown','contextTokens':'128000','outputTokens':'16000'})
OUT.joinpath('provider-package-fixtures.json').write_text(json.dumps(CASES,ensure_ascii=False,indent=2)+'\n')
# Native state-model traces, including failure atomicity and unchanged prior state after rejection.
SCENARIOS=[]
def stream(name,bodies,accept,ready):
 events=[]
 for i,body in enumerate(bodies,1):v=cp(event);v['sequence']=str(i);v['event']=body;events.append(v)
 SCENARIOS.append({'kind':'Stream','id':name,'events':events,'expectedAccept':accept,'expectedReady':ready})
call2=cp(call);call2['call'].update({'nativeCallId':'call:b','ordinal':'1'})
stream('parallel-calls-ready-only-at-terminal',[{'kind':'ToolCallComplete','toolCall':call2},event['event'],{'kind':'Terminal','outcome':'ToolCallsReady'}],[True]*3,[False,False,True])
stream('eof-with-call-is-not-ready',[event['event']],[True],[False])
stream('incomplete-turn-retains-unready-call',[event['event'],{'kind':'Terminal','outcome':'Incomplete'}],[True,True],[False,False])
stream('duplicate-call-does-not-advance-stream',[event['event'],event['event']],[True,False],[False,False])
stream('text-completion',[{'kind':'TextDelta','blockIndex':'0','text':'Hello'},{'kind':'Terminal','outcome':'Completed'}],[True,True],[False,False])
stream('event-after-terminal-rejected',[{'kind':'Terminal','outcome':'Completed'},{'kind':'Heartbeat'}],[True,False],[False,False])
stream('foreign-opaque-preserved-as-invalid',[{'kind':'NativeContinuation','binding':{**binding,'connectionId':'other'},'artifact':artifact}],[False],[False])
# A replacement attempt cannot append its output to the old stream model.
stream('attempt-change-fenced',[{'kind':'TextDelta','blockIndex':'0','text':'Partial'},{'kind':'Heartbeat'}],[True,False],[False,False]);SCENARIOS[-1]['events'][1]['attemptId']='attempt-b'
def transcript(name,batches,expected,nexts):SCENARIOS.append({'kind':'Transcript','id':name,'batches':batches,'expected':expected,'expectedNext':nexts})
full=[entry('c-a',1,{'kind':'ToolCallRecorded','toolCall':call}),entry('c-b',2,{'kind':'ToolCallRecorded','toolCall':call2}),entry('r-b',3,{'kind':'ToolResponseRecorded','call':call2['call'],'outcome':{'kind':'ApprovalRequired','approvalId':'approval-a','operationId':'op-b'}}),entry('r-a',4,{'kind':'ToolResponseRecorded','call':call['call'],'outcome':{'kind':'Completed','artifacts':[artifact]}})]
first=batch('batch-full',full);duplicate_response=batch('batch-duplicate-response',[entry('r-b-again',5,{'kind':'ToolResponseRecorded','call':call2['call'],'outcome':{'kind':'Completed','artifacts':[]}})],'5')
update=batch('batch-update',[entry('operation-update',5,{'kind':'OperationUpdate','operationId':'op-b','revision':'2','artifact':artifact})],'5')
transcript('parallel-wait-replay-and-later-operation-event',[first,first,duplicate_response,update],['Accepted','Replayed','Rejected','Accepted'],[5,5,5,6])
bad=batch('batch-bad',[entry('u-a',1,{'kind':'UserMessage','content':[{'kind':'Text','text':'Hello'}]}),entry('orphan',2,{'kind':'ToolResponseRecorded','call':call['call'],'outcome':{'kind':'Failed','code':'failed'}})])
good=batch('batch-good',[entry('u-b',1,{'kind':'UserMessage','content':[{'kind':'Text','text':'Hello'}]})])
transcript('invalid-tail-does-not-persist-prefix',[bad,good],['Rejected','Accepted'],[1,2])
changed=cp(good);changed['entries'][0]['body']['content'][0]['text']='Changed';transcript('batch-key-payload-conflict',[good,changed],['Accepted','Rejected'],[2,2])
switch=batch('batch-switch',[entry('epoch-change',1,{'kind':'ContextEpochChanged','previousEpoch':'0','nextEpoch':'1','reason':'Compaction','context':artifact})],epoch='1')
old_context=batch('batch-old-context',[entry('old-observation',2,{'kind':'NativeItem','binding':binding,'artifact':artifact})],'2')
record_old_as_history=cp(old_context);record_old_as_history['batchId']='history-current-context';record_old_as_history['contextEpoch']='1'
transcript('epoch-boundary-and-old-native-evidence',[switch,old_context,record_old_as_history],['Accepted','Rejected','Accepted'],[2,2,3])
OUT.joinpath('provider-package-scenarios.json').write_text(json.dumps(SCENARIOS,ensure_ascii=False,indent=2)+'\n')
OUT.joinpath('helper-fixture.json').write_text(json.dumps({'initialize':initialize,'activate':activate,'invoke':invoke,'cancel':cancel,'shutdown':shutdown,'artifact':artifact},indent=2)+'\n')
print(json.dumps({'messageFixtures':len(CASES),'scenarios':len(SCENARIOS),'steps':sum(len(x.get('events',x.get('batches',[]))) for x in SCENARIOS)}))
