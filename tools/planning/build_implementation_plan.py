"""Local architecture planning only. Fails closed on missing/ambiguous coverage."""
from pathlib import Path
from collections import Counter
from fnmatch import fnmatchcase
import hashlib
import json

ROOT = Path(__file__).resolve().parents[2]
BASE = ROOT / 'docs/architecture'
SOURCES = ['implementation-slices.json', 'devworks-capability-catalog.json',
           'devworks-requirements-traceability.json', 'codegraph-capability-inventory.json',
           'capability-validation-contracts.json', 'validation-workloads.json',
           'contracts/family-coverage.json', 'mcp-publication-candidates.json', 'codegraph-work-map.json']
def read(name): return json.loads((BASE / name).read_text())
def write(name, value): (BASE / name).write_text(json.dumps(value, indent=2) + '\n')

slice_source = read('implementation-slices.json')
slices = slice_source['slices'] + slice_source.get('nativeExtensions', [])
bykey = {s['key']: s for s in slices}
assert len(bykey) == len(slices), 'Duplicate slice key'
# Each approved feature retains its terminal acceptance scope; the native baseline does not
# silently acquire dependencies on the separately approved EVAL feature.
scopes = read('implementation-slices.json').get('acceptanceScopes', {
    'acceptance': [s['key'] for s in slices if s['key'] != 'acceptance']})
for terminal, members in scopes.items():
    assert terminal in bykey and terminal not in members and len(set(members)) == len(members)
    bykey[terminal]['dependsOnKeys'] = members
assert set(scopes) | {member for members in scopes.values() for member in members} == set(bykey), ('Unscoped implementation slice', set(bykey) - set(scopes) - {member for members in scopes.values() for member in members})
for s in slices:
    assert set(s['dependsOnKeys']) <= bykey.keys(), ('Unknown dependency', s['key'])
order, visiting, visited = [], set(), set()
def visit(key):
    assert key not in visiting, ('Dependency cycle', key)
    if key in visited: return
    visiting.add(key)
    for dep in bykey[key]['dependsOnKeys']: visit(dep)
    visiting.remove(key); visited.add(key); order.append(key)
for key in bykey: visit(key)

catalog = read('devworks-capability-catalog.json')
inputs = {o['operation']: o for f in read('contracts/family-coverage.json')['families'] for o in f['operations']}
operations = []
for f in catalog['families']:
    for short in f['operations']:
        name = f['family'] + '.' + short
        owners = [s['key'] for s in slices if any(fnmatchcase(name, p) for p in s['operations'])]
        assert len(owners) == 1, ('Missing/ambiguous operation coverage', name, owners)
        operations.append({'operation': name, 'family': f['family'], 'serviceOwner': f['owner'],
            'primarySlice': owners[0], 'inputSchema': inputs[name]['inputSchema'],
            'schemaStatus': inputs[name]['status'], 'resultSchema':inputs[name].get('resultSchema'), 'resultValueType':inputs[name].get('resultValueType'), 'requestModes':inputs[name].get('requestModes'),
            'acceptanceStatus': 'Planned; no production operation acceptance claimed'})
assert len({x['operation'] for x in operations}) == len(operations)
opowner = {x['operation']: x['primarySlice'] for x in operations}

# First key is accountable primary; remaining keys are explicit integration obligations.
reqowners = {
 'ADDON':'packages sdk authority', 'AGENT':'delegation tool_turn controls',
 'ARCH':'contracts store shell sdk', 'ART':'files documents authority',
 'CACHE':'tool_turn transports usage sessions', 'CODE':'graph structural analysis',
 'CODEREVIEW':'reviews graph', 'CONTAINER':'containers authority process_owner',
 'CONTEXT':'tool_turn sessions knowledge graph documents', 'CONTROL':'controls approvals delegation jobs',
 'CONV':'conventions knowledge', 'COORD':'delegation workflow git_delivery tool_turn',
 'DB':'schema_sources database_tools', 'DELIVERY':'git_delivery releases reviews',
 'DOC':'documents files', 'DREAM':'dreaming knowledge approvals', 'EXT':'sdk packages instructions',
 'EVAL':'eval_recorded',
 'GH':'github git_tasks reviews', 'GIT':'git_tasks git_delivery process_owner',
 'GRAPH':'graph structural knowledge_graph', 'HEALTH':'analysis diagnostics', 'HELP':'help repo_docs',
 'HISTORY':'sessions files', 'IMPORT':'imports files', 'JOB':'jobs usage controls',
 'KNOW':'knowledge knowledge_graph dreaming', 'LOG':'diagnostics usage', 'MCP':'mcp authority',
 'MODEL':'work projects git_tasks', 'NOTIFY':'approvals shell', 'OP':'contracts authority controls',
 'OPS':'maintenance backup diagnostics', 'PERF':'performance distribution',
 'PLAT':'distribution shell desktop', 'POWER':'controls queue process_owner',
 'RAG':'documents knowledge graph transports', 'RES':'process_owner process_tools containers',
 'REVIEW':'reviews approvals', 'ROUTE':'router transports credentials usage systemone',
 'SCHEMA':'schema_sources database_tools', 'SEC':'authority credentials process_owner distribution',
 'SELF':'updates packages sdk approvals', 'SET':'settings shell projects',
 'SKILL':'instructions sdk authority', 'STORE':'store files queue backup',
 'TEST':'acceptance contracts performance', 'TOOL':'tool_turn process_tools browser desktop mcp',
 'UI':'shell chat work settings', 'UIQA':'browser desktop reviews', 'UPDATE':'updates backup distribution',
 'VIS':'shell graph schema_sources', 'WORK':'work workflow controls delegation'
}
overrides = {**{f'MCP-{n:02}':'mcp_server mcp_server_access authority graph work knowledge' for n in range(7,12)}, 'ROUTE-18':'systemone router credentials usage',
 'ROUTE-19':'session_auto_routing router chat settings', 'ROUTE-20':'router chat transports',
 'ROUTE-21':'systemone router credentials usage',
 'ROUTE-22':'session_auto_routing systemone router chat settings delegation',
 'ROUTE-23':'session_auto_routing systemone transports usage authority',
 'ROUTE-24':'session_auto_routing router sessions tool_turn chat delegation',
 'ROUTE-25':'model_latest credentials router settings',
 'ROUTE-26':'model_latest systemone credentials router settings',
 'ROUTE-27':'model_latest session_auto_routing router chat jobs settings',
 'ROUTE-16':'usage router', 'ROUTE-17':'usage router',
 **{r:'delegation work workflow controls approvals' for r in ['AGENT-12','AGENT-18','AGENT-19','AGENT-20']},
 'UI-10':'chat router systemone settings',
 **{r:'work workflow controls delegation work_item_metadata' for r in ['WORK-02','WORK-03','WORK-04','WORK-10','WORK-11','WORK-12']},
 'EVAL-01':'eval_suites eval_package_suites eval_acceptance',
 'EVAL-02':'eval_recorded eval_mcp_records eval_acceptance',
 'EVAL-03':'eval_judges eval_acceptance',
 'EVAL-04':'eval_trials eval_acceptance',
 'EVAL-05':'eval_comparisons eval_gates eval_acceptance',
 'EVAL-06':'eval_suites eval_package_suites eval_acceptance',
 'EVAL-07':'eval_adoption eval_acceptance',
 'EVAL-08':'eval_adoption eval_acceptance',
 'EVAL-09':'eval_comparisons eval_acceptance',
 'EVAL-10':'eval_triggers eval_acceptance',
 'EVAL-11':'eval_ui eval_acceptance',
 'EVAL-12':'eval_ui eval_acceptance',
 'EVAL-13':'eval_suites eval_ui eval_acceptance',
 'EVAL-14':'eval_recorded eval_comparisons eval_mcp_records eval_acceptance'}
requirements = []
for r in read('devworks-requirements-traceability.json')['requirements']:
    keys = overrides.get(r['requirement'], reqowners[r['requirement'].rsplit('-', 1)[0]]).split()
    assert set(keys) <= bykey.keys()
    requirements.append({**r, 'primarySlice': keys[0], 'supportingSlices': keys[1:],
        'acceptanceStatus': 'Planned AT case; requirement status is preserved, not promoted to approved'})

capowners = {
 'GRAPH':'graph', 'SOURCE':'graph files', 'REPOSITORIES':'projects',
 'INDEXING':'structural', 'LANGUAGES':'structural dotnet typescript swift rust sdk',
 'LINKING':'graph structural dotnet typescript swift rust', 'OVERLAY':'structural graph',
 'ANALYSIS':'analysis', 'KNOWLEDGE':'knowledge knowledge_graph', 'CONVENTIONS':'conventions',
 'DREAMING':'dreaming', 'WORK':'work workflow delegation', 'SESSIONS':'sessions chat tool_turn',
 'HISTORY':'sessions files', 'RESOURCES':'process_owner process_tools',
 'DECISIONS':'approvals systemone', 'ROLES':'instructions', 'ROUTER':'router transports systemone credentials',
 'USAGE':'usage', 'DATABASE':'schema_sources database_tools', 'MCP':'mcp',
 'GITHUB':'github git_tasks git_delivery', 'REVIEWS':'reviews', 'JOBS':'jobs', 'QUEUE':'queue',
 'DOCS':'help repo_docs', 'IMPORT':'imports', 'MAINTENANCE':'maintenance',
 'DIAGNOSTICS':'diagnostics', 'NAVIGATION':'shell chat', 'SETTINGS':'settings',
 'SERVER_REPLACEMENT':'contracts store queue shell distribution', 'EXCLUDED':'acceptance'
}
capcases = []
for case in read('capability-validation-contracts.json')['cases']:
    keys = capowners[case['id'].removeprefix('CAP-')].split()
    assert set(keys) <= bykey.keys()
    capcases.append({**case, 'primarySlice': keys[0], 'supportingSlices': keys[1:],
        'auditOnly': case['id'] == 'CAP-EXCLUDED'})
source = read('codegraph-capability-inventory.json')
entries = []
for e in source['entries']:
    matches = sorted({op for p in e['targetOperations'] for op in opowner if fnmatchcase(op, p)})
    assert e['disposition'] == 'excluded' or all(any(fnmatchcase(op, p) for op in opowner) for p in e['targetOperations']), ('Unknown selector', e['id'])
    keys = sorted({opowner[op] for op in matches})
    excluded = e['disposition'] == 'excluded'
    if excluded: keys = []
    elif not keys: keys = capowners[e['group']].split()
    assert excluded or keys, ('Unowned retained entry', e['id'])
    entries.append({**e, 'resolvedOperations': matches, 'implementationSlices': keys,
        'coverageKind': 'Explicit user-approved exclusion; acceptance audit only' if excluded else
            ('Exact catalog operation binding' if matches else 'Explicit supporting capability obligation'),
        'sourceRevision': source['sourceRevision'], 'acceptanceStatus': 'Not executed against production services'})

workowners = {
 'W01':'distribution shell', 'W02':'work sessions', 'W03':'chat shell',
 'W04':'structural dotnet typescript swift rust', 'W05':'graph', 'W06':'structural graph',
 'W07':'documents', 'W08':'queue', 'W09':'process_owner router controls',
 'W10':'store backup', 'W11':'sdk packages mcp', 'W12':'distribution browser desktop containers'
}
workloads = [{**w, 'measurementSlice':'performance', 'domainSlices':workowners[w['id']].split()}
             for w in read('validation-workloads.json')['workloads']]
for s in slices:
    key = s['key']
    # Native extensions retain their explicitly tracked requirement/capability obligations
    # without changing the frozen retained-source ownership or fabricating source entries.
    extra_requirements = s.get('additionalPrimaryRequirements', [])
    extra_cases = s.get('additionalCapabilityCases', [])
    assert set(extra_requirements) <= {r['requirement'] for r in requirements}
    assert set(extra_cases) <= {c['id'] for c in capcases}
    s['dependsOn'] = [bykey[k]['id'] for k in s['dependsOnKeys']]
    s['ownedOperations'] = [o['operation'] for o in operations if o['primarySlice'] == key]
    s['primaryRequirements'] = list(dict.fromkeys([r['requirement'] for r in requirements if r['primarySlice'] == key] + extra_requirements))
    s['supportingRequirements'] = [r['requirement'] for r in requirements if key in r['supportingSlices']]
    s['capabilityCases'] = list(dict.fromkeys([c['id'] for c in capcases if key in [c['primarySlice'],*c['supportingSlices']]] + extra_cases))
    s['sourceEntryIds'] = [e['id'] for e in entries if key in e['implementationSlices']]
    s['workloads'] = [w['id'] for w in workloads if key in [w['measurementSlice'],*w['domainSlices']]]
    s['pendingInputSchemas'] = [o['operation'] for o in operations if o['primarySlice'] == key and not o['inputSchema']]
    s['commonAcceptance'] = ('Implement the recorded strict input/result/error schemas and selected-operation/mode binding, '
        'current authority, durable receipt/effect/cancellation/recovery behavior, and meaningful native success/fault cases. '
        'Expose the same domain service to native UI and applicable agent tools; no parallel business implementation. '
        'Preserve source arguments/results/quirks traced here. Do not advertise unqualified targets or infer product acceptance from fixture tests.')

counts = {'slices':len(slices), 'operationFamilies':len(catalog['families']), 'operations':len(operations),
 'requirements':len(requirements), 'requirementsByStatus':dict(Counter(r['requirementStatus'] for r in requirements)),
 'inputSchemaCandidates':sum(bool(o['inputSchema']) for o in operations),
 'inputSchemasPending':sum(not o['inputSchema'] for o in operations),
 'sourceEntries':len(entries), 'retainedSourceEntries':sum(e['disposition']!='excluded' for e in entries),
 'excludedSourceEntries':sum(e['disposition']=='excluded' for e in entries),
 'capabilityAcceptanceGroups':len(capcases), 'workloadSpecifications':len(workloads),
 'unownedOperations':0, 'unownedRequirements':0, 'unownedRetainedEntries':0}
result = {'title':'DevWorks local implementation plan',
 'status':'Architecture baseline 1.1 and approved EVAL plan metadata; execution and acceptance are tracked in CodeGraph',
 'retiredSlices':read('implementation-slices.json').get('retiredSlices',[]),
 'sourceSha256':{n:hashlib.sha256((BASE/n).read_bytes()).hexdigest() for n in SOURCES},
 'counts':counts, 'dependencyOrder':[bykey[k]['id'] for k in order],
 'slices':slices, 'requirements':requirements, 'operations':operations, 'sourceEntries':entries,
 'capabilityCases':capcases, 'workloads':workloads,
 'mcpPublication':{'counts':read('mcp-publication-candidates.json')['counts'],
 'queryAdapterSlice':'mcp_server','commandAndAccessSlice':'mcp_server_access',
 'manifest':'mcp-publication-candidates.json','acceptanceStatus':'Candidate mapping; actual MCP transport, authority and client acceptance unexecuted'}}
write('implementation-plan.json', result)

lines = ['# Local implementation plan', '',
 'Status: architecture baseline 1.1. This is the complete ownership and dependency breakdown of the captured scope, '
 'not implementation authorization or a frozen public API. '
 f'Requirements remain {counts["requirementsByStatus"].get("Confirmed", 0)} Confirmed and {counts["requirementsByStatus"].get("Proposed", 0)} Proposed; '
 'a planned slice does not approve a Proposed requirement. CodeGraph remains the operational source of truth.', '',
 '## Success and remaining commitments', '',
 f'The register assigns all {counts["requirements"]} requirements, {counts["operations"]} operations across '
 f'{counts["operationFamilies"]} families, {counts["retainedSourceEntries"]} retained source entries, '
 f'{counts["capabilityAcceptanceGroups"]} acceptance groups and {counts["workloadSpecifications"]} workloads. '
 f'The {counts["excludedSourceEntries"]} excluded source entries retain their reasons and an exclusion audit. '
 f'There are {counts["slices"]} local slices and no unowned operations, requirements or retained entries.', '',
 'All 351 operation inputs now have candidate schema bindings, with typed qualified result tags and fault/receipt/completion contracts; no catalog input remains unbound. '
 'Actual adoption of those contracts, host authority/SDK/provider behavior, confinement, full native UI '
 'integration and all six OS/CPU builds remain implementation acceptance below. The four original architecture feasibility questions are resolved in the final review. AD76–78 adds incoming MCP support in DW-063/064; actual protocol/authentication and two-client acceptance remain explicit. '
 'A material failure returns to architecture review with alternatives, without dropping scope or expanding authority.', '',
 '[Machine-readable register](implementation-plan.json) preserves each requirement statement/status, every frozen source span/digest, '
 'resolved operation binding, acceptance text and workload specification. '
 '[Editable slice source](implementation-slices.json) owns sequencing and concrete outcomes. '
 '[Handoff gates](architecture-handoff.md) distinguish interface decisions from later production acceptance.', '',
 '## Execution and dependency rules', '',
 'Complete concrete contracts before exposing their operations. Build vertical UI/service/tool behavior in its owning slice, '
 'using the shared shell and authority boundary. Foundation slices do not create a second UI-only data model. '
 'Language/semantic packs and all eight SDK contribution types remain retained scope. '
 'The native baseline and EVAL feature each retain their approved terminal acceptance dependencies. Optional dependencies must fail visibly and locally without making a server, '
 'VM or preinstalled general-purpose runtime a core requirement.', '',
 'The dependency order is a valid topological order, not a timeline or resource estimate. It allows independent work once prerequisites '
 'are satisfied. Integration qualification may hold a particular capability without falsely declaring the product ready. '
 'Routine engineering decisions remain delegated; no further product question arose from the native probes.', '',
 'Unit and integration suites use deterministic fixtures without inference compute. Real inference compatibility and end-to-end runs '
 'retain their separate, later authorized entry point. Workload IDs describe unexecuted production measurements.', '',
 f'DW-038 (conditional external HelixDB reader) was removed by explicit user decision on 2 November 2026. Its ID is retired, not reassigned; {counts["slices"]} planned slices are now represented, including the separately approved EVAL feature. No HelixDB source adapter or engine is an app dependency.', '',
 '![Coverage and qualification gates](architecture-diagrams/41-implementation-coverage.svg)', '',
 '## Dependency order', '',
 ' → '.join(result['dependencyOrder']), '',
 '## Slice register', '',
 '| Slice | Outcome | Prerequisites | Gates |', '| --- | --- | --- | --- |']
for s in slices:
    lines.append(f'| [{s["id"]}](#{s["id"].lower()}) | {s["title"]} | {", ".join(s["dependsOn"]) or "None"} | {", ".join(s["gates"])} |')
lines += ['', '## Concrete outcomes and acceptance', '']
for s in slices:
    lines += [f'### {s["id"]}', '', f'**{s["title"]}** · {s["type"]} · {s["status"]}.', '',
        s['acceptance'], '', *([f'**Tracked work:** {s["codegraphItem"]} in {s["codegraphFeature"]}.', ''] if s.get('codegraphItem') else []), *([s['readinessAcceptance'], ''] if s.get('readinessAcceptance') else []), s['commonAcceptance'], '',
        '**Operations:** ' + (', '.join('`'+o+'`' for o in s['ownedOperations']) or
          s.get('operationContractOwner', 'Cross-cutting implementation/qualification; no separate public operation.')) + '', '',
        '**Primary requirement cases:** ' + (', '.join('AT-'+r for r in s['primaryRequirements']) or 'See supporting and integration obligations.') + '', '',
        '**Supporting requirements:** ' + (', '.join(s['supportingRequirements']) or 'None assigned.') + '', '',
        '**Behavior/workload obligations:** ' + (', '.join(s['capabilityCases']+s['workloads']) or 'Slice-specific acceptance above.') +
          f'. Frozen source entries bound: {len(s["sourceEntryIds"])}. Pending input schemas: {len(s["pendingInputSchemas"])}.', '']
lines += ['## Acceptance group ownership', '', '| Group | Primary slice | Supporting slices | Evidence status |', '| --- | --- | --- | --- |']
for c in capcases:
    lines.append(f'| {c["id"]} · {c["title"]} | {bykey[c["primarySlice"]]["id"]} | {", ".join(bykey[k]["id"] for k in c["supportingSlices"]) or "None"} | {c["fixtureStatus"]} |')
lines += ['', '## Production workload ownership', '', '| Workload | Measurement | Domain slices | Status |', '| --- | --- | --- | --- |']
for w in workloads:
    lines.append(f'| {w["id"]} · {w["title"]} | {bykey[w["measurementSlice"]]["id"]} | {", ".join(bykey[k]["id"] for k in w["domainSlices"])} | {w["executionStatus"]} |')
lines += ['', '## Reproduction and review', '',
 'Run `python3 tools/planning/build_implementation_plan.py` after regenerating the architecture and family schema coverage. '
 'The builder rejects dependency cycles, missing or ambiguous operation ownership, unknown retained selectors and missing requirement/group/workload assignments. '
 'Source hashes detect planning input drift; they do not audit current CodeGraph revisions or prove behavioral parity. '
 'Review those source changes before execution and re-run native acceptance against the implemented services.', '']
(BASE/'implementation-plan.md').write_text('\n'.join(lines))
print(json.dumps(counts))
