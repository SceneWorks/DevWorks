"""Author graph/retrieval/artifact/delivery candidates and negative contract cases."""
from copy import deepcopy as cp
import json
from pathlib import Path
ROOT=Path(__file__).resolve().parents[2];OUT=ROOT/'docs/architecture/contracts';P='1.0-draft.1';A='a'*64
CASES=[]
def add(name,entry,value,typed=True,schema=None):CASES.append({'id':name,'entry':entry,'raw':json.dumps(value,separators=(',',':')),'typedValid':typed,'schemaValid':typed if schema is None else schema})
def mut(name,entry,value,fn,typed=False,schema=None):v=cp(value);fn(v);add(name,entry,v,typed,schema)
artifact={'artifactId':'artifact-a','versionId':'version-a','sha256':A,'byteLength':'8','mediaType':'text/plain'}
base={'format':'Sha1','hex':'a'*40};head={'format':'Sha1','hex':'b'*40}
snapshot={'repositoryId':'repo-a','base':base,'head':head,'worktreeId':'work-a','captureDigest':A}
coverage={'state':'Complete','gaps':[],'moreGaps':False}
view={'viewId':'view-a','snapshot':cp(snapshot),'sourceRevision':'1','baseline':{'generationId':'g-base','manifestDigest':A},'branch':{'generationId':'g-branch','manifestDigest':A,'parentGenerationId':'g-base'},'worktree':{'generationId':'g-capture','manifestDigest':A,'parentGenerationId':'g-branch'},'coverage':cp(coverage)}
stamp={'storeId':'store-a','restoreEpoch':'0','sequence':'1','policyRevision':'1','scopeRevision':'1'}
resolved={'pinId':'pin-a','stamp':stamp,'expiresAtMs':'100','views':[view]}
add('immutable-captured-view','resolved-view-set',resolved)
mut('reject-hidden-worktree-fallback','resolved-view-set',resolved,lambda v:v['views'][0].pop('worktree'),schema=True)
mut('reject-branch-parent-mismatch','resolved-view-set',resolved,lambda v:v['views'][0]['branch'].update({'parentGenerationId':'other'}),schema=True)
mut('reject-duplicate-view-source','resolved-view-set',resolved,lambda v:v['views'].append(cp(v['views'][0])),schema=True)
mut('reject-complete-with-gaps','resolved-view-set',resolved,lambda v:v['views'][0]['coverage'].update({'gaps':['missing-unit']}),schema=True)
mut('reject-mismatched-git-hash-format','resolved-view-set',resolved,lambda v:v['views'][0]['snapshot']['head'].update({'format':'Sha256'}),schema=True)
schema_view={'pinId':'pin-schema','stamp':stamp,'expiresAtMs':'100','views':[],'schemaViews':[{'connectionId':'connection-a','snapshotId':'schema-snapshot','generationId':'schema-generation','schemaDigest':A,'accessRevision':'1','coverage':coverage}]}
add('metadata-only-schema-view','resolved-view-set',schema_view)
mut('reject-source-values-in-schema-snapshot','resolved-view-set',schema_view,lambda v:v['schemaViews'][0].update({'sampleRecords':[{'password':'forbidden'}]}))
limits={'maxResults':100,'maxVisited':10000,'deadlineMs':2000}
graph={'protocol':P,'queryId':'query-a','scope':{'kind':'Project','projectId':'project-a'},'view':{'kind':'Exact','viewIds':['view-a']},'query':{'kind':'Overview'},'limits':limits}
queries={
 'Search':{'text':'Widget','nodeKinds':[]},'Overview':{},'Schema':{},'Node':{'nodeId':'node-a'},'Source':{'nodeId':'node-a','maxLines':100},
 'Traverse':{'startIds':['node-a'],'direction':'Forward','edgeKinds':[],'maxDepth':5},'Path':{'fromId':'node-a','toId':'node-b','maxDepth':8},
 'NodeImpact':{'nodeId':'node-a'},'FileImpact':{'repositoryId':'repo-a','unitKey':'unit-a'},'NodesByFile':{'repositoryId':'repo-a','unitKey':'unit-a'},'RepositoryDependencies':{'repositoryId':'repo-a'},
 'Consumers':{'nodeId':'node-a'},'Publishers':{'nodeId':'node-a'},'DataLineage':{'nodeId':'node-a'},'ScopeStatus':{},
}
for kind,data in queries.items():v=cp(graph);v['query']={'kind':kind,**data};add('graph-'+kind,'graph-read-request',v)
mut('reject-authorized-source-claim','graph-read-request',graph,lambda v:v.update({'authorized':True}))
mut('reject-graph-unbounded-work','graph-read-request',graph,lambda v:v['limits'].update({'maxVisited':1000001}))
mut('reject-empty-exact-view','graph-read-request',graph,lambda v:v['view'].update({'viewIds':[]}))
mut('reject-raw-sql-query','graph-read-request',graph,lambda v:v.update({'sql':'select private_data'}))
v=cp(graph);v['view']={'kind':'SchemaPublished','connectionIds':['connection-a']};add('schema-topology-read','graph-read-request',v)
def code_citation():return {'source':{'kind':'Repository','snapshot':cp(snapshot),'unitKey':'unit-a','sourceDigest':A},'blockId':'block-a','contentDigest':A,'locator':{'kind':'Code','snapshot':cp(snapshot),'unitKey':'unit-a','firstLine':1,'lastLine':4}}
citation={'source':{'kind':'Managed','version':cp(artifact),'extractionRevision':'1'},'blockId':'block-a','contentDigest':A,'locator':{'kind':'Text','firstLine':1,'lastLine':3}}
node={'nodeId':'node-a','unitKey':'unit-a','kind':'function','name':'Widget','citation':code_citation()}
graphpage={'protocol':P,'queryId':'query-a','view':cp(resolved),'completion':'Complete','result':{'kind':'Nodes','nodes':[node]}}
add('graph-page-pinned-source','graph-page',graphpage)
def foreign_citation(v):
 for key in ['source','locator']:v['result']['nodes'][0]['citation'][key]['snapshot']['repositoryId']='repo-other'
mut('reject-graph-citation-outside-view','graph-page',graphpage,foreign_citation,schema=True)
v=cp(graphpage);v['result']={'kind':'Source','citation':code_citation(),'text':'fn Widget() {}'};add('source-page-pinned-source','graph-page',v)
def foreign_source(v):
 for key in ['source','locator']:v['result']['citation'][key]['snapshot']['repositoryId']='repo-other'
mut('reject-source-citation-outside-view','graph-page',v,foreign_source,schema=True)
v=cp(graphpage);v['result']={'kind':'Graph','nodes':[cp(node)],'edges':[{'edgeId':'edge-a','sourceId':'node-a','targetId':'node-missing','kind':'calls','producingUnit':'unit-a'}]};add('reject-dangling-graph-result','graph-page',v,False,True)
space={'spaceId':'space-a','modelDigest':A,'dimensions':2,'metric':'L2'}
retrieval={'protocol':P,'queryId':'query-rag','scope':{'kind':'Project','projectId':'project-a'},'sources':['Document','Knowledge','Code','Convention'],'versions':{'kind':'Current'},'input':{'kind':'Text','text':'transaction ownership'},'mode':'Hybrid','lexicalFallback':True,'limits':limits}
add('scoped-hybrid-retrieval','retrieval-request',retrieval)
v=cp(retrieval);v['input']={'kind':'Vector','space':space,'float32Le':cp(artifact)};v['mode']='Vector';add('exact-space-vector-query','retrieval-request',v)
mut('reject-vector-shape-mismatch','retrieval-request',v,lambda x:x['input']['float32Le'].update({'byteLength':'12'}),schema=True)
mut('exact-old-version-selection','retrieval-request',retrieval,lambda v:v.update({'versions':{'kind':'Exact','versions':[artifact]}}),True)
page={'protocol':P,'queryId':'query-rag','stamp':stamp,'generationIds':['generation-a'],'effectiveMode':'Lexical','completion':'Complete','passages':[{'chunkId':'chunk-a','citation':cp(citation),'text':'transaction ownership','rank':1}]}
add('lexical-fallback-is-visible','retrieval-page',page)
mut('reject-vector-result-without-space','retrieval-page',page,lambda v:v.update({'effectiveMode':'Vector'}),schema=True)
mut('reject-reversed-citation-range','retrieval-page',page,lambda v:v['passages'][0]['citation']['locator'].update({'firstLine':4,'lastLine':1}),schema=True)
mut('reject-duplicate-passage','retrieval-page',page,lambda v:v['passages'].append(cp(v['passages'][0])),schema=True)
v=cp(page);v['passages'][0]['citation']['locator']={'kind':'Docx','part':'body-part','paragraph':3};add('word-citation-no-invented-page','retrieval-page',v)
mut('reject-word-page-substitution','retrieval-page',v,lambda v:v['passages'][0]['citation']['locator'].update({'page':17}))
v=cp(page);v['passages'][0]['citation']['locator']={'kind':'Pdf','page':2};add('pdf-version-page-citation','retrieval-page',v)
publication={'protocol':P,'operationId':'operation-index','sourceId':'repo-a','expectedSourceRevision':'1','expectedPointerRevision':'0','view':cp(view),'sealedManifest':cp(artifact)}
add('sealed-publication-preconditions','publication-request',publication)
mut('reject-publish-other-source','publication-request',publication,lambda v:v.update({'sourceId':'repo-other'}),schema=True)
mut('reject-publish-other-revision','publication-request',publication,lambda v:v.update({'expectedSourceRevision':'2'}),schema=True)
for kind,data in [('RefreshOverlay',{'repositoryId':'repo-a','worktreeId':'work-a','expectedSourceRevision':'1'}),('Cancel',{'operationId':'operation-index'}),('SetTrust',{'repositoryId':'repo-a','nodeId':'node-a','expectedRevision':'1','viewId':'view-a','expectedContentDigest':A,'trusted':False})]:add('index-'+kind,'index-command-request',{'protocol':P,'requestKey':'index-command','command':{'kind':kind,**data}})
artifact_commands={
 'Save':{'projectId':'project-a','title':'Report','mediaType':'text/plain','source':{'kind':'Upload','ticketId':'upload-a'},'expectedDigest':A,'expectedBytes':'8'},
 'Replace':{'artifactId':'artifact-a','expectedRevision':'1','source':{'kind':'WorkOutput','fileHandle':'file-a'},'expectedDigest':A,'expectedBytes':'8'},
 'Reference':{'artifactId':'artifact-a','versionId':'version-a','subject':{'kind':'root','id':'root-a'},'expectedRevision':'1'},
 'DeleteVersion':{'artifactId':'artifact-a','versionId':'version-a','expectedRevision':'1','expectedReferenceRevision':'1'},
 'DetachProject':{'artifactId':'artifact-a','projectId':'project-a','expectedReferenceRevision':'1'},
 'Share':{'artifactId':'artifact-a','targetProjectId':'project-b','expectedScopeRevision':'1'},'Move':{'artifactId':'artifact-a','targetProjectId':'project-b','expectedScopeRevision':'1'},
 'DisableRetrieval':{'artifactId':'artifact-a','expectedRevision':'1'},'RemoveIndex':{'artifactId':'artifact-a','expectedRevision':'1'},
}
for kind,data in artifact_commands.items():add('artifact-'+kind,'artifact-command-request',{'protocol':P,'requestKey':'artifact-command','command':{'kind':kind,**data}})
for kind,data in [('DeleteVersion',artifact_commands['DeleteVersion']),('DetachProject',artifact_commands['DetachProject']),('DeleteArtifact',{'artifactId':'artifact-a','expectedRevision':'1','expectedReferenceRevision':'1','expectedVersionsDigest':A})]:add('artifact-delete-'+kind,'artifact-deletion-request',{'protocol':P,'requestKey':'artifact-delete','deletion':{'kind':kind,**data}})
v={'protocol':P,'requestKey':'artifact-command','command':{'kind':'Save',**cp(artifact_commands['Save'])}};mut('reject-forged-artifact-origin','artifact-command-request',v,lambda v:v['command'].update({'originActorId':'user'}))
mut('reject-arbitrary-artifact-path','artifact-command-request',v,lambda v:v['command'].update({'path':'/private/file'}))
for kind,data in [('List',{'projectId':'project-a'}),('Read',{'artifactId':'artifact-a','versionId':'version-a'}),('Preview',{'artifactId':'artifact-a','versionId':'version-a'}),('Export',{'artifactId':'artifact-a','versionId':'version-a'}),('ResolveCitation',{'citation':citation})]:add('artifact-query-'+kind,'artifact-query-request',{'protocol':P,'queryId':'query-artifact','query':{'kind':kind,**data},'limit':100})
receipt={'commandId':'command-a','committedSequence':'1','acceptedAtMs':'1','requestDigest':A,'decision':{'kind':'VersionUnavailable','artifactId':'artifact-a','versionId':'version-a','physicalDeletePending':True}}
add('logical-delete-before-unlink','artifact-command-reply',{'kind':'Recorded','protocol':P,'receipt':receipt,'replayed':False})
repo={'repositoryId':'repo-a','pullRequestId':'pr-a','baseRef':'refs/heads/main','expectedHead':cp(head),'observedBase':cp(base),'reviewSource':'GitHubAutomated','mergeMethod':'MergeCommit','evidence':cp(artifact),'policyRevision':'1'}
repo_b=cp(repo);repo_b['repositoryId']='repo-b';repo_b['pullRequestId']='pr-b'
barrier={'protocol':P,'deliveryId':'delivery-a','rootId':'root-a','kind':'Feature','revision':'1','approvedScopeDigest':A,'repositories':[cp(repo),repo_b],'dependencies':[{'before':'repo-a','after':'repo-b'}]}
add('all-repository-readiness-barrier','delivery-barrier',barrier)
mut('reject-cyclic-delivery-order','delivery-barrier',barrier,lambda v:v['dependencies'].append({'before':'repo-b','after':'repo-a'}),schema=True)
mut('reject-unknown-delivery-dependency','delivery-barrier',barrier,lambda v:v['dependencies'][0].update({'after':'repo-unknown'}),schema=True)
mut('reject-duplicate-delivery-repository','delivery-barrier',barrier,lambda v:v['repositories'].append(cp(v['repositories'][0])),schema=True)
mut('reject-final-delivery-to-task-branch','delivery-barrier',barrier,lambda v:v['repositories'][0].update({'baseRef':'refs/heads/codex/task'}),schema=True)
mut('reject-caller-readiness-flag','delivery-barrier',barrier,lambda v:v.update({'allChecksPassed':True}))
git_commands={
 'Commit':{'worktreeId':'work-a','expectedHead':head,'message':artifact},
 'Push':{'worktreeId':'work-a','remoteId':'remote-a','targetRef':'refs/heads/codex/task-a','expectedHead':head,'expectedRemote':base,'mode':'ForceWithLease'},
 'OpenPullRequest':{'repositoryId':'repo-a','headRef':'refs/heads/codex/task-a','baseRef':'refs/heads/codex/feature-a','expectedHead':head,'description':artifact},
 'Integrate':{'deliveryId':'delivery-a','expectedBarrierRevision':'1','repositoryId':'repo-a','expectedHead':head},
 'Cleanup':{'worktreeId':'work-a','expectedOwnershipRevision':'1','expectedHead':head},
}
for kind,data in git_commands.items():add('git-'+kind,'git-command-request',{'protocol':P,'requestKey':'git-command','command':{'kind':kind,**cp(data)}})
push={'protocol':P,'requestKey':'git-command','command':{'kind':'Push',**cp(git_commands['Push'])}}
mut('reject-main-force-push','git-command-request',push,lambda v:v['command'].update({'targetRef':'refs/heads/main'}),schema=True)
mut('reject-force-without-remote-lease','git-command-request',push,lambda v:v['command'].pop('expectedRemote'),schema=True)
mut('reject-git-ref-option-injection','git-command-request',push,lambda v:v['command'].update({'targetRef':'--delete'}),schema=True)
cleanup={'protocol':P,'requestKey':'git-command','command':{'kind':'Cleanup',**cp(git_commands['Cleanup'])}}
mut('reject-cleanup-arbitrary-path','git-command-request',cleanup,lambda v:v['command'].update({'path':'/'}))
merge={'repositoryId':'repo-a','expectedHead':cp(head),'observation':'Merged','mergeCommit':cp(head),'evidence':cp(artifact)}
status={'deliveryId':'delivery-a','revision':'1','repositories':[merge],'featureComplete':True,'cleanupPending':True,'releasePending':True,'deploymentPending':True}
add('feature-complete-separate-cleanup-release-deployment','delivery-snapshot',status)
mut('reject-queued-as-feature-complete','delivery-snapshot',status,lambda v:(v['repositories'][0].update({'observation':'Queued','requestTicketId':'merge-ticket-a'}),v['repositories'][0].pop('mergeCommit')),schema=True)
v=cp(status);v['featureComplete']=False;v['repositories'][0].update({'observation':'Queued','requestTicketId':'merge-ticket-a'});v['repositories'][0].pop('mergeCommit');add('queued-ticket-stays-incomplete','delivery-snapshot',v)
mut('reject-merged-without-merge-commit','delivery-snapshot',status,lambda v:v['repositories'][0].pop('mergeCommit'),schema=True)
release={'protocol':P,'requestKey':'release-a','action':'Prepare','repositoryId':'repo-a','expectedRevision':head,'version':'v1.2.3-rc.1+build.8','destinationId':'destination-a','artifact':artifact,'expectedPolicyRevision':'1'}
for action in ['Prepare','Publish','Deploy']:
 v=cp(release);v['action']=action;add('release-'+action,'release-command-request',v)
mut('reject-release-version-path','release-command-request',release,lambda v:v.update({'version':'../../other'}))
mut('reject-release-hash-format-mismatch','release-command-request',release,lambda v:v['expectedRevision'].update({'format':'Sha256'}),schema=True)
OUT.joinpath('data-delivery-fixtures.json').write_text(json.dumps(CASES,indent=2)+'\n')
print(json.dumps({'messageFixtures':len(CASES)}))
