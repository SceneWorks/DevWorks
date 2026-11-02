"""Cross-check all current candidates and actually exchanged helper frames; authoring only."""
import argparse
import hashlib
import importlib.metadata
import json
from pathlib import Path
import platform
import subprocess
import tempfile
import jsonschema
ROOT=Path(__file__).resolve().parents[2];D=ROOT/'docs/architecture';C=D/'contracts'
p=argparse.ArgumentParser();p.add_argument('--binary',type=Path,required=True);p.add_argument('--output',type=Path,required=True);args=p.parse_args();args.output=args.output.resolve();args.output.mkdir(parents=True,exist_ok=True)
fixture_files=[C/'fixtures.json',C/'provider-package-fixtures.json',args.output/'actual-frame-fixtures.json']
fixtures=[x for path in fixture_files for x in json.loads(path.read_text())]
with tempfile.TemporaryDirectory(prefix='devworks-protocol-check-') as tmp:
    path=Path(tmp)/'combined.json';path.write_text(json.dumps(fixtures))
    run=subprocess.run([str(args.binary.resolve()),'check',str(path)],capture_output=True,text=True)
native=json.loads(run.stdout);assert native['allPassed'],[(c['id'],c['error']) for c in native['cases'] if not c['passed']]
validators={}
for path in C.glob('*.schema.json'):
    schema=json.loads(path.read_text());jsonschema.Draft202012Validator.check_schema(schema);validators[path.name.removesuffix('.schema.json')]=jsonschema.Draft202012Validator(schema)
def unique(pairs):
    out={}
    for k,v in pairs:
        if k in out:raise ValueError('duplicate member')
        out[k]=v
    return out
results=[]
for case,actual in zip(fixtures,native['cases'],strict=True):
    try:value=json.loads(case['raw'],object_pairs_hook=unique);valid=validators[case['entry']].is_valid(value)
    except ValueError:valid=False
    assert valid==case['schemaValid'],case['id']
    if actual['typedValid']:validators[case['entry']].validate(actual['normalized'])
    results.append({'id':case['id'],'entry':case['entry'],'schemaValid':valid,'typedValid':actual['typedValid'],'passed':actual['passed']})
run=subprocess.run([str(args.binary.resolve()),'scenario-check',str(C/'provider-package-scenarios.json')],capture_output=True,text=True,check=True);scenarios=json.loads(run.stdout);assert scenarios['allPassed']
(args.output/'scenario-results.json').write_text(run.stdout+'\n')
(args.output/'native-results.json').write_text(json.dumps(native,indent=2)+'\n')
coverage=json.loads((C/'family-coverage.json').read_text());assert coverage['familyCount']==len(coverage['families'])
bindings=[o for f in coverage['families'] for o in f['operations'] if o['inputSchema']]
# A selected capability never accepts a different command hidden in a shared union schema.
def selected(binding,value):
    selector=binding['selectionConstraint']
    if selector:
        for component in selector['pointer'].strip('/').split('/'):value=value.get(component,{})
        return value==selector['equals']
    return True
bound=next(o for o in bindings if o['operation']=='addons.disable')
install=next(json.loads(c['raw']) for c in fixtures if c['id']=='addon-Install')
disable=next(json.loads(c['raw']) for c in fixtures if c['id']=='addon-Disable')
assert selected(bound,disable) and not selected(bound,install)
start_binding=next(o for o in bindings if o['operation']=='runs.start')
cont=next(json.loads(c['raw']) for c in fixtures if c['id']=='continue')
assert not selected(start_binding,cont)
# Assert every bound variant has at least one conforming fixture under its selected entry point.
for b in bindings:
    entry=b['inputSchema'].removesuffix('.schema.json')
    assert any(c['entry']==entry and c['typedValid'] and selected(b,json.loads(c['raw'])) for c in fixtures),b['operation']
steps=sum(len(s['steps']) for s in scenarios['scenarios'])
paths=[*fixture_files,C/'provider-package-scenarios.json',ROOT/'tools/contracts-eval/Cargo.lock',*sorted((ROOT/'tools/contracts-eval/src').glob('*.rs')),*sorted(C.glob('*.schema.json'))]
report={'date':'2026-11-01','status':'Candidate DTO/schema, binding and native model checks passed; no live provider or production host/SDK qualification','host':platform.platform(),'authoringJsonschema':importlib.metadata.version('jsonschema'),'schemaFiles':len(validators),'messageFixtures':len(fixtures),'coreFixtures':len(json.loads(fixture_files[0].read_text())),'newFixtures':len(json.loads(fixture_files[1].read_text())),'actualHelperFrames':len(json.loads(fixture_files[2].read_text())),'scenarios':len(scenarios['scenarios']),'scenarioSteps':steps,'selectedOperationBindingsChecked':len(bindings),'crossOperationPayloadRejected':True,'allPassed':True,'cases':results,'inputSha256':{str(path.relative_to(ROOT)):hashlib.sha256(path.read_bytes()).hexdigest() for path in paths}}
(args.output/'schema-model-results.json').write_text(json.dumps(report,indent=2)+'\n');print(json.dumps({k:report[k] for k in ['schemaFiles','messageFixtures','newFixtures','actualHelperFrames','scenarios','scenarioSteps','selectedOperationBindingsChecked','allPassed']}))
