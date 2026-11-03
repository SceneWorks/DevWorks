"""Bind common candidate schemas to every operation in the retained catalog."""
import argparse
import datetime
import hashlib
import json
from pathlib import Path
from domain_spec import OPS, LEGACY_RESULTS, FAMILY_RULES, REVISIONS
ROOT=Path(__file__).resolve().parents[2]
p=argparse.ArgumentParser();p.add_argument('--date',default=datetime.date.today().isoformat(),help='Report date (default: the run date)');DATE=p.parse_args().date
D=ROOT/'docs/architecture'
CATALOG=json.loads((D/'devworks-capability-catalog.json').read_text())
BINDINGS={
 'runs.start':('command-request','runs.start'),
 'runs.pause':('command-request','runs.pause'),
 'runs.continue':('command-request','runs.continue'),
 'runs.inspect':('query-request','runs.inspect'),
 'runs.eligibility':('query-request','runs.eligibility'),
 'history.operation_status':('query-request','history.operation_status'),
 'history.command_receipt':('query-request','history.command_receipt'),
 'history.events':('query-request','history.events'),
 'sessions.append_message':('session-command-request',None),
 'sessions.transcript':('session-query-request',None),
 'addons.install':('addon-command-request','Install'),
 'addons.update':('addon-command-request','Update'),
 'addons.enable':('addon-command-request','Enable'),
 'addons.disable':('addon-command-request','Disable'),
 'addons.rollback':('addon-command-request','Rollback'),
 'addons.uninstall':('addon-command-request','Uninstall'),
}
for operation,variant in {
 'search':'Search','architecture':'Overview','dependency':'Traverse','impact':'NodeImpact',
 'source':'Source','traverse':'Traverse','graph_scope_status':'ScopeStatus','overview':'Overview',
 'nodes_by_file':'NodesByFile','node_detail':'Node','file_impact':'FileImpact','graph_schema':'Schema',
 'call_path':'Path','data_lineage':'DataLineage','consumers':'Consumers','publishers':'Publishers',
 'snippet':'Source','repository_dependencies':'RepositoryDependencies',
}.items(): BINDINGS['code.'+operation]=('graph-read-request',variant)
for operation,variant in {'code.set_trust':'SetTrust','indexing.refresh_overlay':'RefreshOverlay','indexing.cancel':'Cancel'}.items():
    BINDINGS[operation]=('index-command-request',variant)
for operation,variant in {'documents.share':'Share','documents.move':'Move','documents.disable_retrieval':'DisableRetrieval','documents.remove_index':'RemoveIndex','artifacts.save':'Save','artifacts.reference':'Reference'}.items():
    BINDINGS[operation]=('artifact-command-request',variant)
for operation,variant in {'documents.read':'Read','artifacts.list':'List','artifacts.read':'Read','artifacts.preview':'Preview','artifacts.export':'Export'}.items():
    BINDINGS[operation]=('artifact-query-request',variant)
BINDINGS['documents.search']=('retrieval-request',None)
BINDINGS['artifacts.delete']=('artifact-deletion-request',None)
for operation,variant in {'commit':'Commit','push':'Push','pull_request':'OpenPullRequest','integrate':'Integrate','cleanup':'Cleanup'}.items():
    BINDINGS['git.'+operation]=('git-command-request',variant)
for operation,variant in {'prepare':'Prepare','publish':'Publish','deploy':'Deploy'}.items():
    BINDINGS['releases.'+operation]=('release-command-request',variant)
LEGACY_BINDINGS=dict(BINDINGS)
assert not set(OPS).intersection(BINDINGS)
for operation in OPS: BINDINGS[operation]=('domain-request',operation)
LEGACY_QUERIES={o for o,(schema,_) in LEGACY_BINDINGS.items() if schema in {'query-request','graph-read-request','artifact-query-request','retrieval-request','session-query-request'}}

def pointer(schema):
    if schema=='domain-request': return '/body/kind'
    if schema in {'query-request','graph-read-request','artifact-query-request'}: return '/query/kind'
    if schema=='release-command-request': return '/action'
    return '/command/kind'
def revision_doc(base,revision):
    # The schema document of `base` at `revision`: the latest `<base>.rK` with K <= revision.
    for k in range(revision,1,-1):
        if (D/'contracts'/f'{base}.r{k}.schema.json').exists(): return f'{base}.r{k}.schema.json'
    return base+'.schema.json'
def revisions(name,schema,modes):
    return [{'revision':r,'inputSchema':revision_doc(schema,r),'resultSchema':revision_doc('operation-result',r),
             'queryReplySchema':revision_doc('domain-query-reply',r) if 'Query' in modes else None,
             'commandReplySchema':revision_doc('domain-command-reply',r) if 'Command' in modes else None,
             'completionSchema':revision_doc('operation-completion',r) if 'Command' in modes else None}
            for r in REVISIONS.get(name,[1])]
entries=[]
for f in CATALOG['families']:
    ops=[]
    for name in f['operations']:
        qualified=f['family']+'.'+name; binding=BINDINGS.get(qualified)
        descriptor=OPS.get(qualified)
        mode=descriptor['mode'] if descriptor else ('Query' if qualified in LEGACY_QUERIES else 'Command')
        modes=['Query','Command'] if mode=='Hybrid' else [mode]
        ops.append({'operation':qualified,'inputSchema':binding[0]+'.schema.json' if binding else None,'variant':binding[1] if binding else None,'selectionConstraint':{'pointer':pointer(binding[0]),'equals':binding[1]} if binding and binding[1] else None,
            'requestModes':modes,'modePolicies':[{'mode':m,'authority':'ScopedRead' if m=='Query' else descriptor['authority'] if descriptor else 'Existing host command authority','effect':'ReadOnly or declared external read' if m=='Query' else descriptor['effect'] if descriptor else 'Existing journaled command contract'} for m in modes],'resultSchema':'operation-result.schema.json','resultSelectionConstraint':{'pointer':'/kind','equals':qualified},'resultValueType':descriptor['result'] if descriptor else LEGACY_RESULTS[qualified],
            'queryReplySchema':'domain-query-reply.schema.json' if 'Query' in modes else None,'commandReplySchema':'domain-command-reply.schema.json' if 'Command' in modes else None,
            'completionSchema':'operation-completion.schema.json' if 'Command' in modes else None,
            'schemaRevisions':revisions(qualified,binding[0],modes),
            'authority':descriptor['authority'] if descriptor else 'Existing host authority contract',
            'effect':descriptor['effect'] if descriptor else 'Existing exact-view/immutable-read contract' if 'Query' in modes else 'Existing journaled command contract',
            'semanticRules':descriptor['rules'] if descriptor else 'Preserve existing concrete input/authority/receipt/recovery contract; selected result kind must match operation.',
            'familyRules':FAMILY_RULES[f['family']],
            'admission':'Validate protocol, strict fields and selected operation/mode first; derive host principal, current scope/role/grants, then apply command receipt and dispatch policy. Payload IDs are references, not authority.',
            'errorSchema':'DomainFault in domain-query-reply or domain-command-reply; shared pre-admission rejections and legacy specialized control replies remain available',
            'recovery':'Read uses snapshot/cursor revalidation; local mutations replay receipt. Journaled/external/inference commands reconcile uncertain outcomes before retry. A retry hint never clears Pause/Continue/approval/budget holds.',
            'status':'Concrete input/result candidate; actual service/authority behavior not implemented'})
    entries.append({'family':f['family'],'owner':f['owner'],'effectAndAuthority':f['effectAndAuthority'],'operations':ops})
all_names=[o['operation'] for f in entries for o in f['operations']]
assert len(all_names)==len(set(all_names));assert set(BINDINGS)<=set(all_names)
(D/'contracts/family-coverage.json').write_text(json.dumps({'status':'All retained operation inputs and result tags have concrete candidates; not a public API freeze or production runtime coverage','date':DATE,'catalogSha256':hashlib.sha256((D/'devworks-capability-catalog.json').read_bytes()).hexdigest(),'familyCount':len(entries),'operationCount':len(all_names),'concreteInputCandidates':len(BINDINGS),'domainInputsPending':len(all_names)-len(BINDINGS),'families':entries},indent=2)+'\n')
files=[{'file':p.name,'sha256':hashlib.sha256(p.read_bytes()).hexdigest()} for p in sorted((D/'contracts').glob('*.schema.json'))]
(D/'contracts/manifest.json').write_text(json.dumps({'contract':'1.0-draft.1','date':DATE,'status':'Adopted architecture candidate; not public SDK v1, production service or platform proof','schemaSource':'../../../tools/contracts-eval/src/lib.rs','schemaSources':['../../../tools/contracts-eval/src/lib.rs','../../../tools/contracts-eval/src/provider.rs','../../../tools/contracts-eval/src/package.rs','../../../tools/contracts-eval/src/data_delivery.rs','../../../tools/contracts-eval/src/domain_generated.rs','../../../tools/contracts-eval/src/domain_support.rs','../../../tools/contracts-eval/src/domain_rules.rs','../../../tools/contracts-eval/domain_spec.py','../../../tools/contracts-eval/domain-spec.json'],'schemas':files,'fixtures':'fixtures.json','fixtureSets':['fixtures.json','provider-package-fixtures.json','data-delivery-fixtures.json','domain-fixtures.json'],'scenarioSets':['provider-package-scenarios.json'],'operationCoverage':'family-coverage.json','notes':['Schema checks structural constraints; typed decoding checks signed-64 range and cross-field consistency.','Authority, relationships, current eligibility and receipt/dispatch transitions remain application service obligations.','All registered operations now have concrete input/result candidates and explicit mode, authority, effect and recovery metadata. New query/command/completion envelopes coexist with earlier specialized candidate replies; no published SDK compatibility or production behavior claimed. Selection constraints must be applied before dispatch and before result publication.','Each operation lists the schema revisions it negotiates (schemaRevisions). A request names one with the optional envelope member schemaRevision (absent means 1); <name>.rN.schema.json documents the shape at revision N and later and base documents are revision 1. Members introduced at N carry x-since N.']},indent=2)+'\n')
print(json.dumps({'families':len(entries),'catalogOperations':len(all_names),'concreteInputs':len(BINDINGS),'pendingDomainInputs':len(all_names)-len(BINDINGS),'schemas':len(files)}))
