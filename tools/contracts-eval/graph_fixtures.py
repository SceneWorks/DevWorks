"""DW-28 concrete exact graph messages and counterexamples; no graph/runtime execution."""
import copy,json

def append_graph_cases(case,sample,base):
 cp=copy.deepcopy
 page=cp(base['GraphPage']);view=page['view'];repo=view['views'][0]['snapshot']['repositoryId'];vid=view['views'][0]['viewId'];snapshot=cp(view['views'][0]['snapshot']);citation=cp(page['result']['nodes'][0]['citation']);unit='src/main.rs'
 base['RepositorySnapshot']=cp(snapshot);base['Citation']=cp(citation)
 source={'snapshot':snapshot,'unitKey':unit,'sourceDigest':'a'*64,'path':unit,'startLine':'1','endLine':'4','language':'Rust'}
 node={'nodeId':'node-a','repositoryId':repo,'viewId':vid,'unitKey':unit,'nodeKind':'Function','name':'Widget','qualifiedName':'module.Widget','source':source,'properties':[],'propertiesComplete':True,'doNotTrust':False,'confidence':'high','extractionSource':'structural','trustRevision':'1','contentIdentity':'git-oid-sha1-opaque','contentDigest':'a'*64,'viewDigest':'b'*64}
 node2=cp(node);node2['nodeId']='node-b'
 edge={'edgeId':'edge-a','sourceId':'node-a','targetId':'node-b','sourceRepositoryId':repo,'targetRepositoryId':repo,'sourceUnitKey':unit,'targetUnitKey':unit,'producingUnit':unit,'edgeKind':'CALLS','resolution':'Resolved','confidence':1.0,'confidenceBand':'high','properties':[],'propertiesComplete':True}
 result_types={'code.search':('CodeSearch','CodeSearchResult'),'repositories.nodes':('CodeNodes','CodeNodesResult'),'code.nodes_by_file':('CodeNodes','CodeNodesResult'),'code.dependency':('CodeGraph','CodeGraphResult'),'code.traverse':('CodeGraph','CodeGraphResult'),'code.node_detail':('NodeDetail','CodeNodeDetail'),'code.source':('SourceText','CodeSourceText'),'code.snippet':('SourceText','CodeSourceText'),'code.overview':('RepositoryOverview','CodeOverview'),'code.architecture':('Architecture','CodeArchitecture'),'code.impact':('Impact','CodeImpact'),'code.file_impact':('Impact','CodeImpact'),'code.call_path':('CallTrace','CodeCallTrace'),'code.data_lineage':('Lineage','CodeLineage'),'code.consumers':('Related','CodeRelated'),'code.publishers':('Related','CodeRelated'),'code.repository_dependencies':('Dependencies','CodeDependencies'),'code.graph_schema':('SchemaDetails','CodeSchema'),'code.graph_scope_status':('ScopeDetails','CodeScopeStatus')}
 def output(op):
  kind,ty=result_types[op];value=sample(ty)
  if ty=='CodeSearchResult':value.update(mode='RankedNodes',items=[{'kind':'Node','node':cp(node)}],total='1')
  if ty=='CodeNodesResult':value.update(nodes=[cp(node)],total='1',selectedNodeId='node-a')
  if ty=='CodeGraphResult':value.update(nodes=[cp(node),cp(node2)],edges=[cp(edge)])
  if ty=='CodeNodeDetail':value.update(node=cp(node),neighbors=[cp(node2)],outgoing=[cp(edge)],relationshipsComplete=True)
  if ty=='CodeSourceText':
   text='fn Widget() {}\n';value.update(source=cp(source),nodeId='node-a',text=text,startByte='0',endByte=str(len(text.encode())),totalBytes=str(len(text.encode())),firstLine='1',lastLine='1',nodeStartLine='1',nodeEndLine='1')
  if ty=='CodeOverview':value.update(stamp=cp(view['stamp']),graphCoverage=cp(base['Coverage']))
  if ty=='CodeArchitecture':value.update(repositoryId=repo,summary='Exact current repository summary',confidence='high')
  if ty=='CodeImpact':value.update(found=False,blastRadius='None',path=unit,language='Rust',languageIndexed=True)
  if ty=='CodeCallTrace':value.update(start=cp(node),entries=[{'node':cp(node2),'depth':'1','parentId':'node-a','edgeKind':'CALLS','edgeId':'edge-a'}])
  if ty=='CodeLineage':value.update(modelName='Widget',model=cp(node),consumers=[{'node':cp(node2),'depth':'1','parentId':'node-a','edgeKind':'USES_TYPE'}])
  if ty=='CodeRelated':value.update(nodes=[{'node':cp(node),'edgeKind':'CONSUMES'if op=='code.consumers'else'PUBLISHES','lowConfidence':False}])
  if ty=='CodeDependencies':value.update(repositoryId=repo);value['rustSemantic'].update(status='not_analyzed')
  if ty=='CodeSchema':value.update(origin='Declared',stamp=cp(view['stamp']),nodeKinds=['Function','Module'],edgeKinds=['CALLS'],properties=[{'ownerKind':'CALLS','name':'confidence','description':'Recorded relation confidence'}],notes=['No inference'])
  if ty=='CodeScopeStatus':value.update(stamp=cp(view['stamp']),current=True)
  p=cp(page);p['scopeCoverage']={'coverage':cp(base['Coverage']),'unavailableRepositories':[]};p['result']={'kind':kind,'value':value};return {'kind':op,'value':p}
 queries={
  'code.search':{'kind':'Search','selection':{'mode':'RankedNodes','text':'Widget','repositoryId':repo,'internalModule':'core'}},
  'code.architecture':{'kind':'Overview','repositoryId':repo},'code.overview':{'kind':'Overview'},
  'code.dependency':{'kind':'Traverse','startIds':['node-a'],'direction':'Both','edgeKinds':[],'maxDepth':3},
  'code.traverse':{'kind':'Traverse','startIds':['node-a'],'direction':'Both','edgeKinds':[],'maxDepth':3},
  'code.impact':{'kind':'NodeImpact','selection':{'target':{'kind':'Name','name':'module.Widget','repositoryId':repo},'depth':3}},
  'code.source':{'kind':'Source','selection':{'kind':'Node','nodeId':'node-a','mode':'NodeWindow'}},
  'code.snippet':{'kind':'Source','selection':{'kind':'File','repositoryId':repo,'path':unit,'startLine':'0','endLine':'0'}},
  'code.graph_scope_status':{'kind':'ScopeStatus'},'code.graph_schema':{'kind':'Schema'},
  'code.nodes_by_file':{'kind':'NodesByFile','selection':{'repositoryId':repo,'path':unit,'line':'3'}},
  'code.node_detail':{'kind':'Node','nodeId':'node-a'},
  'code.file_impact':{'kind':'FileImpact','selection':{'repositoryId':repo,'path':unit,'depth':3}},
  'code.call_path':{'kind':'Path','selection':{'target':{'kind':'Name','name':'Widget','repositoryId':repo},'direction':'Both','depth':3}},
  'code.data_lineage':{'kind':'DataLineage','selection':{'kind':'Name','name':'Widget','repositoryId':repo}},
  'code.consumers':{'kind':'Consumers','selection':{'kind':'Name','name':'Widget','repositoryId':repo}},
  'code.publishers':{'kind':'Publishers','selection':{'kind':'Name','name':'Widget','repositoryId':repo}},
  'code.repository_dependencies':{'kind':'RepositoryDependencies','repositoryId':repo}}
 inputs={}
 for op,q in queries.items():
  frame={'protocol':'1.0-draft.1','schemaRevision':3,'queryId':'query-a','scope':{'kind':'Project','projectId':'project-a'},'current':True,'query':q,'limits':{'maxResults':100,'maxVisited':10000,'deadlineMs':2000}}
  inputs[op]=frame;case('dw28-input-'+op,'graph-read-request.r3',frame)
  case('dw28-result-'+op,'operation-result.r3',output(op))
 repo_input={'mode':'Query','protocol':'1.0-draft.1','schemaRevision':3,'queryId':'query-a','body':{'kind':'repositories.nodes','repositoryId':repo,'current':True,'page':{'limit':20},'nodeKind':'Function','internalModule':'core','scope':{'kind':'Project','projectId':'project-a'}}}
 case('dw28-input-repositories.nodes','domain-request.r3',repo_input);case('dw28-result-repositories.nodes','operation-result.r3',output('repositories.nodes'))
 trust={'protocol':'1.0-draft.1','schemaRevision':3,'requestKey':'trust-a','command':{'scope':{'kind':'Project','projectId':'project-a'},'kind':'SetTrust','repositoryId':repo,'nodeId':'node-a','unitKey':unit,'viewId':vid,'expectedContentDigest':'a'*64,'expectedViewDigest':'b'*64,'expectedRevision':'1','trusted':False}}
 case('dw28-input-code.set_trust','index-command-request.r3',trust)
 case('dw28-result-code.set_trust','operation-result.r3',{'kind':'code.set_trust','value':sample('MutationSummary')})
 def mutated(id,entry,value,fn,typed=False,schema=False):v=cp(value);fn(v);case('dw28-'+id,entry,v,typed,schema)
 for op in ['code.search','code.source','code.call_path','code.impact','code.data_lineage','code.consumers','code.publishers','code.nodes_by_file','code.file_impact']:
  mutated('reject-null-selection-'+op,'graph-read-request.r3',inputs[op],lambda v:v['query'].update(selection=None))
  mutated('reject-current-before3-'+op,'graph-read-request',inputs[op],lambda v:v.update(schemaRevision=1))
 for field in ['unitKey','expectedViewDigest']:
  mutated('reject-null-trust-'+field,'index-command-request.r3',trust,lambda v:v['command'].update({field:None}))
  mutated('reject-missing-trust-'+field,'index-command-request.r3',trust,lambda v:v['command'].pop(field))
 mutated('reject-both-view-choices','graph-read-request.r3',inputs['code.search'],lambda v:v.update(view={'kind':'Exact','viewIds':[vid]}))
 mutated('reject-false-current','graph-read-request.r3',inputs['code.search'],lambda v:v.update(current=False))
 mutated('reject-null-current','graph-read-request.r3',inputs['code.search'],lambda v:v.update(current=None))
 mutated('reject-null-explicit-view','graph-read-request.r3',inputs['code.search'],lambda v:(v.pop('current'),v.update(view=None)))
 mutated('reject-both-search-targets','graph-read-request.r3',inputs['code.search'],lambda v:v['query'].update(text='Widget',nodeKinds=[]))
 mutated('reject-partial-legacy-path','graph-read-request.r3',inputs['code.call_path'],lambda v:(v['query'].pop('selection'),v['query'].update(fromId='node-a',maxDepth=3)),False,False)
 mutated('search-unified-empty','graph-read-request.r3',inputs['code.search'],lambda v:v['query']['selection'].update(mode='Unified',text=''),True,True)
 mutated('search-paged-node-pattern','graph-read-request.r3',inputs['code.search'],lambda v:v['query']['selection'].update(mode='NodeList',text='%',nodeKind='Function'),True,True)
 mutated('whole-file-source','graph-read-request.r3',inputs['code.source'],lambda v:v['query']['selection'].update(mode='WholeFile'),True,True)
 mutated('reject-null-repo-current','domain-request.r3',repo_input,lambda v:v['body'].update(current=None))
 mutated('reject-both-repo-views','domain-request.r3',repo_input,lambda v:v['body'].update(views={'kind':'Exact','viewIds':[vid]}))
 unavailable=output('code.graph_scope_status');unavailable['value'].pop('view');unavailable['value']['scopeCoverage']={'coverage':{'state':'Unavailable','gaps':['missing-view'],'moreGaps':False},'unavailableRepositories':[]};unavailable['value']['result']['value'].update(unavailableReason='own_worktree_not_indexed',coverage={'state':'Unavailable','gaps':['missing-view'],'moreGaps':False})
 case('dw28-unavailable-own-view-no-fake-pin','operation-result.r3',unavailable)
 mutated('reject-missing-view-reason','operation-result.r3',unavailable,lambda v:v['value']['result']['value'].pop('unavailableReason'),False,False)
 mutated('reject-viewless-real-nodes','operation-result.r3',output('repositories.nodes'),lambda v:v['value'].pop('view'),False,False)
 mutated('reject-foreign-view-node','operation-result.r3',output('repositories.nodes'),lambda v:v['value']['result']['value']['nodes'][0].update(viewId='foreign'),False,True)
 mutated('reject-foreign-source-snapshot','operation-result.r3',output('code.source'),lambda v:v['value']['result']['value']['source']['snapshot'].update(repositoryId='foreign'),False,True)
 mutated('reject-edge-endpoint-unit','operation-result.r3',output('code.traverse'),lambda v:v['value']['result']['value']['edges'][0].update(targetUnitKey='other.rs'),False,True)
 external=output('repositories.nodes');n=external['value']['result']['value']['nodes'][0];n.pop('source');n.update(nodeKind='ExternalSymbol',name='External',qualifiedName='external:External')
 case('dw28-source-less-external-node','operation-result.r3',external)
 long_source=output('code.source');v=long_source['value']['result']['value'];v.update(text='x'*65536,startByte='65536',endByte='131072',totalBytes='200000',firstLine='1',lastLine='1');long_source['value'].update(completion='ResultLimit',nextCursor='source-byte-131072')
 case('dw28-long-single-line-source-fragment','operation-result.r3',long_source)
 mutated('reject-source-byte-loss','operation-result.r3',long_source,lambda v:v['value']['result']['value'].update(endByte='131071'),False,True)
 prop=output('repositories.nodes');n=prop['value']['result']['value']['nodes'][0];n['properties']=[{'name':'large','jsonText':'"'+'x'*65535,'offset':'0','totalBytes':'100002','nextOffset':'65536'}];n['propertiesComplete']=False;prop['value'].update(completion='ResultLimit',nextCursor='property-65536')
 case('dw28-large-property-json-fragment','operation-result.r3',prop)
 mutated('reject-property-gap','operation-result.r3',prop,lambda v:v['value']['result']['value']['nodes'][0]['properties'][0].update(nextOffset='65537'),False,True)
 for op in result_types:
  mutated('reject-current-result-before3-'+op,'operation-result',output(op),lambda v:None,False,False)
 wrong=output('code.architecture');wrong['kind']='code.search';case('dw28-reject-selected-result-substitution','operation-result.r3',wrong,False,True)
 for name,text,first,last in [('empty-file','',0,0),('verbatim-nul-source','a\0b',1,1)]:
  source_case=output('code.source');v=source_case['value']['result']['value'];v.update(text=text,startByte='0',endByte=str(len(text.encode())),totalBytes=str(len(text.encode())),firstLine=str(first),lastLine=str(last));v.pop('nodeStartLine',None);v.pop('nodeEndLine',None)
  case('dw28-'+name,'operation-result.r3',source_case)
 mutated('reject-partial-mixed-path','graph-read-request.r3',inputs['code.call_path'],lambda v:v['query'].update(fromId='node-a'))
 mutated('reject-partial-mixed-source','graph-read-request.r3',inputs['code.source'],lambda v:v['query'].update(maxLines=100))
 mutated('reject-partial-mixed-search','graph-read-request.r3',inputs['code.search'],lambda v:v['query'].update(text='Widget'))
 mutated('reject-partial-mixed-file','graph-read-request.r3',inputs['code.nodes_by_file'],lambda v:v['query'].update(repositoryId=repo))
 # Public declared schema and registered-repository metadata do not fabricate an indexed view.
 schema_only=output('code.graph_schema');schema_only['value'].pop('view');schema_only['value']['scopeCoverage']={'coverage':{'state':'Unavailable','gaps':['missing-view'],'moreGaps':False},'unavailableRepositories':[]};schema_only['value']['result']['value']['unavailableReason']='no_indexed_view'
 case('dw28-static-schema-empty-application','operation-result.r3',schema_only)
 mutated('reject-viewless-observed-schema','operation-result.r3',schema_only,lambda v:v['value']['result']['value'].update(origin='Observed'))
 mutated('reject-viewless-schema-without-reason','operation-result.r3',schema_only,lambda v:v['value']['result']['value'].pop('unavailableReason'))
 metadata=output('code.overview');metadata['value'].pop('view');metadata['value']['scopeCoverage']={'coverage':{'state':'Unavailable','gaps':['missing-view'],'moreGaps':False},'unavailableRepositories':[]};v=metadata['value']['result']['value'];v.update(graphCoverage={'state':'Unavailable','gaps':['not-indexed'],'moreGaps':False},unavailableReason='not_indexed',repositories=[{'repositoryId':repo,'name':'Registered unindexed repository','foundational':False}])
 case('dw28-overview-metadata-with-unknown-graph-totals','operation-result.r3',metadata)
 mutated('reject-metadata-overview-complete-graph','operation-result.r3',metadata,lambda v:v['value']['result']['value'].update(graphCoverage=cp(base['Coverage'])))
 mutated('reject-metadata-overview-fake-zero-totals','operation-result.r3',metadata,lambda v:v['value']['result']['value'].update(graphTotals={'nodeCount':'0','edgeCount':'0'}))
 mutated('reject-metadata-overview-fallback-dependencies','operation-result.r3',metadata,lambda v:v['value']['result']['value'].update(dependencies=[{'sourceRepositoryId':repo,'targetRepositoryId':'other','edgeKind':'CALLS','count':'1'}]))
 fake=cp(metadata);fake['value']['view']={'pinId':'fake-pin','stamp':cp(view['stamp']),'expiresAtMs':'1','views':[]}
 case('dw28-reject-empty-fabricated-view','operation-result.r3',fake,False,True)
 # Cross-repository answers pin both actual snapshots, never today's default branch.
 cross=output('code.traverse');other=cp(cross['value']['view']['views'][0]);other['viewId']='view-b';other['snapshot']['repositoryId']='repo-b';cross['value']['view']['views'].append(other)
 remote=cross['value']['result']['value']['nodes'][1];remote.update(repositoryId='repo-b',viewId='view-b');remote['source']['snapshot']['repositoryId']='repo-b';cross['value']['result']['value']['edges'][0]['targetRepositoryId']='repo-b'
 case('dw28-cross-repository-pinned-graph','operation-result.r3',cross)
 directory=output('repositories.nodes');n=directory['value']['result']['value']['nodes'][0];n.update(unitKey='/src',nodeKind='Folder',contentIdentity='directory-sha256-opaque');n.pop('source');n.pop('contentDigest',None)
 case('dw28-directory-unit-opaque-identity','operation-result.r3',directory)
 opaque=cp(trust);opaque['command'].update(unitKey='/src',expectedContentIdentity='directory-sha256-opaque');opaque['command'].pop('expectedContentDigest')
 case('dw28-directory-opaque-trust','index-command-request.r3',opaque)
 mutated('reject-mixed-trust-content-witnesses','index-command-request.r3',opaque,lambda v:v['command'].update(expectedContentDigest='a'*64))
 mutated('reject-null-opaque-trust','index-command-request.r3',opaque,lambda v:v['command'].update(expectedContentIdentity=None))
 # Required aggregate facts can exceed the returned row count without page-local totals.
 architecture=output('code.architecture');v=architecture['value']['result']['value'];v.update(nodeCounts=[{'name':'Function','count':'100'}],hotspots=[{'name':'Widget','qualifiedName':'module.Widget','fanIn':'20','reason':'High fan-in method'}],inbound=[{'sourceRepositoryId':'repo-b','targetRepositoryId':repo,'edgeKind':'REFERENCES_PACKAGE','count':'3','confidenceBand':'medium'}],packages=[{'name':'pkg','ecosystem':'cargo','version':'1.0','dependentCount':'2'}],packageCount='50',packageCountsByEcosystem=[{'name':'cargo','count':'50'}],supertypes=[{'name':'Base','qualifiedName':'module.Base','nodeKind':'Interface','subtypeCount':'5','external':True}]);architecture['value'].update(completion='ResultLimit',nextCursor='architecture-packages-1')
 overview=output('code.overview');overview['value']['result']['value']['repositories']=[{'repositoryId':repo,'name':'Actual registered repository','foundational':False}];overview['value']['result']['value']['repositories'][0]['readmeSource']=cp(output('code.source')['value']['result']['value']['source'])
 case('dw28-overview-captured-readme','operation-result.r3',overview)
 unavailable=cp(overview);unavailable['value'].pop('view');unavailable['value']['result']['value'].update(graphCoverage={'state':'Unavailable','gaps':[],'moreGaps':False},unavailableReason='not_indexed');unavailable['value']['result']['value'].pop('graphTotals',None)
 case('dw28-reject-viewless-overview-readme','operation-result.r3',unavailable,False,True)
 mutated('reject-null-overview-readme','operation-result.r3',overview,lambda v:v['value']['result']['value']['repositories'][0].update(readmeSource=None))
 mutated('reject-cross-repository-overview-readme','operation-result.r3',overview,lambda v:v['value']['result']['value']['repositories'][0]['readmeSource']['snapshot'].update(repositoryId='wrong-repo'),False,True)
 mutated('reject-other-view-overview-readme','operation-result.r3',overview,lambda v:v['value']['result']['value']['repositories'][0]['readmeSource']['snapshot']['head'].update(hex='c'*40),False,True)
 case('dw28-full-architecture-aggregate','operation-result.r3',architecture)
 incomplete=cp(architecture);incomplete['value']['completion']='WorkLimit';incomplete['value'].pop('nextCursor',None);incomplete['value']['result']['value'].pop('packageCount');incomplete['value']['result']['value']['packageCountsByEcosystem']=[]
 case('dw28-work-limited-architecture-unknown-package-count','operation-result.r3',incomplete)
 mutated('reject-null-architecture-package-count','operation-result.r3',incomplete,lambda v:v['value']['result']['value'].update(packageCount=None))
 impacts=output('code.impact');v=impacts['value']['result']['value'];v.update(found=True,blastRadius='High',changed=[{'node':cp(node),'depth':'0','edgeKind':'Changed','risk':'Low','riskFactors':[],'lowConfidence':False}],affected=[{'node':cp(node2),'depth':'1','edgeKind':'CALLS','risk':'High','riskFactors':['external contract'],'lowConfidence':False}],summary={'totalAffected':'10','crossRepositoryCount':'0','criticalCount':'0','highCount':'10','mediumCount':'0','lowCount':'0','affectedRepositoryIds':[repo],'lowConfidenceCount':'0'});impacts['value'].update(completion='ResultLimit',nextCursor='impact-1')
 case('dw28-risk-summary-not-page-local','operation-result.r3',impacts)
 old_at3={'kind':'code.search','value':cp(base['GraphPage'])}
 case('dw28-reject-legacy-result-at3','operation-result.r3',old_at3,False,False)
 # Mixed Current scope is explicitly partial; successful available rows do not claim complete totals.
 partial=output('code.search');partial['value']['completion']='Partial';partial['value']['scopeCoverage']={'coverage':{'state':'Partial','gaps':['repo-b'],'moreGaps':False},'unavailableRepositories':[{'repositoryId':'repo-b','reason':'not_indexed'}]};partial['value']['result']['value'].pop('total',None)
 case('dw28-current-mixed-index-partial','operation-result.r3',partial)
 mutated('reject-partial-as-complete','operation-result.r3',partial,lambda v:v['value'].update(completion='Complete'),False,True)
 mutated('reject-partial-with-complete-coverage','operation-result.r3',partial,lambda v:v['value']['scopeCoverage'].update(coverage=cp(base['Coverage'])),False,True)
 case('dw28-reject-partial-before3','operation-result',partial,False,False)
 missing_scope=cp(repo_input);missing_scope['body'].pop('scope')
 case('dw28-reject-missing-repository-scope3','domain-request.r3',missing_scope,False,False)
 mutated('reject-null-repository-scope3','domain-request.r3',repo_input,lambda v:v['body'].update(scope=None))
 legacy_scope=cp(repo_input);legacy_scope['schemaRevision']=1;legacy_scope['body']={'kind':'repositories.nodes','repositoryId':repo,'views':{'kind':'Exact','viewIds':[vid]},'page':{'limit':20}}
 case('dw28-legacy-repository-scope-from-host','domain-request',legacy_scope)
 ref={'repositoryId':repo,'viewId':vid,'unitKey':unit,'nodeId':'node-a'}
 def drill(kind,count=None):
  v={'nodeKind':kind,'nodes':[],'hasMore':False}
  if count is not None:v['count']=str(count)
  return v
 module={'module':{'repositoryId':repo,'moduleName':'Core','indexedNode':ref},'state':'NotAnalyzed','reason':'no_recorded_analysis','observedNodeCounts':[{'name':'Function','count':'7'}],'observedCoverage':cp(base['Coverage']),'endpoints':drill('Route',0),'services':drill('Service',0),'tables':drill('Table',0)}
 modules=output('code.architecture');modules['value']['result']['value']['internalModules']=[cp(module)]
 case('dw28-module-not-analyzed-indexed-facts','operation-result.r3',modules)
 mutated('reject-module-invented-recorded-state','operation-result.r3',modules,lambda v:v['value']['result']['value']['internalModules'][0].update(state='Recorded'),False,True)
 recorded=cp(modules);v=recorded['value']['result']['value']['internalModules'][0];v.update(state='Recorded',recorded={'summary':'Exact persisted module summary','confidence':'high','updatedAtMs':'100','modelUsed':'recorded-model','endpoints':[{'route':'/orders','httpMethod':'GET','description':'Retained endpoint','responseModel':'Orders'}],'services':[{'name':'Orders','description':'Retained service','interfaceName':'IOrders','lifetime':'Scoped'}],'externalDependencies':['package-a'],'databaseTables':['Orders']});v.pop('reason')
 case('dw28-recorded-module-full-source-fields','operation-result.r3',recorded)
 unknown=cp(module);unknown.update(state='Unavailable',reason='index_unavailable',observedCoverage={'state':'Unavailable','gaps':['module-index'],'moreGaps':False},observedNodeCounts=[],endpoints=drill('Route'),services=drill('Service'),tables=drill('Table'))
 unavailable_modules=output('code.architecture');unavailable_modules['value']['result']['value']['internalModules']=[unknown]
 case('dw28-unavailable-module-counts-unknown','operation-result.r3',unavailable_modules)
 mutated('reject-unavailable-module-fake-zero','operation-result.r3',unavailable_modules,lambda v:v['value']['result']['value']['internalModules'][0]['endpoints'].update(count='0'),False,True)
 mutated('reject-module-cross-scope-drilldown','operation-result.r3',modules,lambda v:v['value']['result']['value']['internalModules'][0]['module']['indexedNode'].update(viewId='foreign'),False,True)
 mutated('reject-metadata-only-module-fallback','operation-result.r3',metadata,lambda v:v['value']['result']['value'].update(internalModules=[module]))
 with_pin=cp(trust);with_pin['command']['pinId']='pin-a'
 case('dw28-trust-explicit-retained-pin','index-command-request.r3',with_pin)
 mutated('reject-trust-null-pin','index-command-request.r3',with_pin,lambda v:v['command'].update(pinId=None))
 mutated('reject-trust-missing-scope3','index-command-request.r3',trust,lambda v:v['command'].pop('scope'))
 mutated('reject-trust-null-scope3','index-command-request.r3',trust,lambda v:v['command'].update(scope=None))
 # A real legacy-representable unit/citation projects; native tests also exercise the adapter.
 legacy=output('repositories.nodes');n=legacy['value']['result']['value']['nodes'][0];n['unitKey']='unit-a';n['source'].update(unitKey='unit-a',path='unit-a',legacyCitation=citation)
 case('dw28-real-legacy-citation-current','operation-result.r3',legacy)
