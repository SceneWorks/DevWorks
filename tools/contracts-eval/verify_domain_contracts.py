"""Validate every registered input/result and old candidates without executing effects."""
import argparse,copy,hashlib,json,platform,subprocess,tempfile
from pathlib import Path
import jsonschema
from domain_spec import OPS,LEGACY_RESULTS,TYPES
ROOT=Path(__file__).resolve().parents[2];D=ROOT/'docs/architecture';C=D/'contracts'
p=argparse.ArgumentParser();p.add_argument('--binary',type=Path,required=True);p.add_argument('--output',type=Path,required=True);a=p.parse_args();a.output.mkdir(parents=True,exist_ok=True)
files=[C/'fixtures.json',C/'provider-package-fixtures.json',D/'validation/provider-package-contracts/actual-frame-fixtures.json',C/'data-delivery-fixtures.json',C/'domain-fixtures.json']
fixtures=[c for f in files for c in json.loads(f.read_text())]
with tempfile.TemporaryDirectory(prefix='devworks-domain-dto-') as temp:
 f=Path(temp)/'all.json';f.write_text(json.dumps(fixtures))
 run=subprocess.run([str(a.binary.resolve()),'check',str(f)],text=True,capture_output=True)
 native=json.loads(run.stdout)
assert run.returncode==0 and native['allPassed'],[(c['id'],c['error']) for c in native['cases'] if not c['passed']]
validators={}
for path in C.glob('*.schema.json'):
 s=json.loads(path.read_text());jsonschema.Draft202012Validator.check_schema(s);validators[path.name.removesuffix('.schema.json')]=jsonschema.Draft202012Validator(s)
def unique(pairs):
 d={}
 for k,v in pairs:
  if k in d:raise ValueError('duplicate JSON member')
  d[k]=v
 return d
results=[]
for case,actual in zip(fixtures,native['cases'],strict=True):
 try:
  value=json.loads(case['raw'],object_pairs_hook=unique)
  valid=validators[case['entry']].is_valid(value)
 except ValueError:valid=False
 assert valid==case['schemaValid'],('schema expectation',case['id'],valid)
 if actual['typedValid']:validators[case['entry']].validate(actual['normalized'])
 results.append({'id':case['id'],'entry':case['entry'],'schemaValid':valid,'typedValid':actual['typedValid'],'passed':actual['passed']})
coverage=json.loads((C/'family-coverage.json').read_text());bindings=[o for f in coverage['families'] for o in f['operations']]
assert len(bindings)==len(OPS)+len(LEGACY_RESULTS) and coverage['domainInputsPending']==0
catalog={f['family']+'.'+op for f in json.loads((D/'devworks-capability-catalog.json').read_text())['families'] for op in f['operations']}
assert {b['operation'] for b in bindings}==catalog==set(OPS)|set(LEGACY_RESULTS)
def select(pointer,value):
 for k in pointer.strip('/').split('/'):
  if not isinstance(value,dict) or k not in value:return None
  value=value[k]
 return value
def selected(b,value):
 constraint=b['selectionConstraint']
 return not constraint or select(constraint['pointer'],value)==constraint['equals']
positive=[(c,json.loads(c['raw'])) for c in fixtures if c['typedValid']]
checks=[]
for b in bindings:
 entry=b['inputSchema'].removesuffix('.schema.json')
 matching=[(c,v) for c,v in positive if c['entry']==entry and selected(b,v)]
 assert matching,b['operation']
 if entry=='domain-request':assert {v['mode'] for _,v in matching}==set(b['requestModes']),b['operation']
 outputs=[(c,v) for c,v in positive if c['entry']=='operation-result' and v['kind']==b['operation']]
 assert outputs,b['operation']
 assert all(validators['operation-result'].is_valid(v) for _,v in outputs)
 other=next((v for c,v in positive if c['entry']==entry and b['selectionConstraint'] and not selected(b,v)),None)
 if b['selectionConstraint'] and entry=='domain-request':assert other is not None
 # A catalog-qualified result cannot be substituted under another selected operation.
 another=next(v for c,v in positive if c['entry']=='operation-result' and v['kind']!=b['operation'])
 assert another['kind']!=b['resultSelectionConstraint']['equals']
 checks.append({'operation':b['operation'],'inputFixtureIds':[c['id'] for c,_ in matching], 'resultFixtureIds':[c['id'] for c,_ in outputs], 'modes':b['requestModes'],'wrongOperationPayloadChecked':other is not None,'wrongResultTagRejected':True})
# Exact selector equality classes are disclosed aliases, not falsely called isolation.
aliases={}
for b in bindings:
 key=json.dumps([b['inputSchema'],b['selectionConstraint']],sort_keys=True)
 aliases.setdefault(key,[]).append(b['operation'])
aliases=[v for v in aliases.values() if len(v)>1]
# Native context/stream scenarios are rerun against this binary; helper exchanges
# above remain replayed frames, never claimed as new helper-process execution.
scenario=subprocess.run([str(a.binary.resolve()),'scenario-check',str(C/'provider-package-scenarios.json')],text=True,capture_output=True,check=True)
scenarios=json.loads(scenario.stdout);assert scenarios['allPassed']
(a.output/'scenario-results.json').write_text(scenario.stdout+'\n')
(a.output/'native-results.json').write_text(json.dumps(native,indent=2)+'\n')
sources=[*files,C/'family-coverage.json',C/'manifest.json',D/'devworks-capability-catalog.json',ROOT/'tools/contracts-eval/Cargo.lock',ROOT/'tools/contracts-eval/domain_spec.py',ROOT/'tools/contracts-eval/domain-spec.json',ROOT/'tools/contracts-eval/build_domain_contracts.py',ROOT/'tools/contracts-eval/build_register.py',ROOT/'tools/contracts-eval/build_domain_reference.py',ROOT/'tools/contracts-eval/make_domain_fixtures.py',ROOT/'tools/contracts-eval/verify_domain_contracts.py',*sorted((ROOT/'tools/contracts-eval/src').glob('*.rs')),*sorted(C.glob('*.schema.json'))]
new=json.loads(files[-1].read_text())
report={'date':'2026-11-02','status':'Candidate schema/typed/binding checks passed; no production service, authority, source behavioral parity or platform acceptance',
 'host':platform.platform(),'schemaFiles':len(validators),'domainTypeDefinitions':len(TYPES),'catalogOperations':len(bindings),'inputCandidates':len(bindings),'inputsPending':0,'qualifiedResultTags':len(bindings),
 'newOperationInputs':len(OPS),'newInputModeVariants':sum(2 if d['mode']=='Hybrid' else 1 for d in OPS.values()),'hybridOperations':[n for n,d in OPS.items() if d['mode']=='Hybrid'],
 'allMessageFixtures':len(fixtures),'newMessageFixtures':len(new),'newPositiveFixtures':sum(c['typedValid'] for c in new),'newNegativeFixtures':sum(not c['typedValid'] for c in new),
 'coreAndPriorMessagesRerun':len(fixtures)-len(new),'historicalHelperFramesReplayed':55,'newHelperExecutionClaimed':False,
 'operationBindingChecks':checks,'sharedInputSelectorAliases':aliases,'schemaCases':results,'scenarios':len(scenarios['scenarios']),'scenarioSteps':sum(len(x['steps']) for x in scenarios['scenarios']),
 'noInference':True,'allPassed':True,'sourceSha256':{str(f.relative_to(ROOT)):hashlib.sha256(f.read_bytes()).hexdigest() for f in sources},
 'limits':['Shape, scalar, selected-operation and stated pure cross-field validation only. No host principal/permission/credential/enforcement proof.',
 'Installed extension/settings/structured-decision payloads additionally require current descriptor schema and capability validation before use.',
 'Typed tags and source accounting do not prove retained argument semantics or native behavior; actual service/CAP/platform acceptance remains required.',
 'Previous native UI/PDF/CDP/read experiments are not rerun or expanded by DTO checks; all production workloads remain unexecuted.']}
(a.output/'schema-model-results.json').write_text(json.dumps(report,indent=2)+'\n')
print(json.dumps({k:report[k] for k in ['schemaFiles','catalogOperations','inputCandidates','inputsPending','qualifiedResultTags','newMessageFixtures','newNegativeFixtures','allMessageFixtures','scenarios','scenarioSteps','allPassed']}))
