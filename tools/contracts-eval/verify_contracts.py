"""Cross-check generated JSON schemas/native DTO fixtures and canonical fingerprints.
Requires authoring-only jsonschema 4.26.0 and rfc8785 0.1.4. No product runtime dependency.
"""
import argparse
import hashlib
import importlib.metadata
import json
from pathlib import Path
import platform
import subprocess
import tempfile
import jsonschema
import rfc8785
ROOT=Path(__file__).resolve().parents[2]
parser=argparse.ArgumentParser()
parser.add_argument('--binary',type=Path,required=True)
parser.add_argument('--output',type=Path,required=True)
args=parser.parse_args();args.output.mkdir(parents=True,exist_ok=True)
contracts=ROOT/'docs/architecture/contracts'
run=subprocess.run([str(args.binary.resolve()),'check',str(contracts/'fixtures.json')],text=True,capture_output=True,check=True)
native=json.loads(run.stdout);cases=json.loads((contracts/'fixtures.json').read_text())
assert native['allPassed']
def no_duplicates(pairs):
    out={}
    for key,value in pairs:
        if key in out:raise ValueError('Duplicate JSON member')
        out[key]=value
    return out
validators={}
for p in contracts.glob('*.schema.json'):
    schema=json.loads(p.read_text());jsonschema.Draft202012Validator.check_schema(schema)
    validators[p.name.removesuffix('.schema.json')]=jsonschema.Draft202012Validator(schema)
results=[]
for c,n in zip(cases,native['cases'],strict=True):
    try:
        value=json.loads(c['raw'],object_pairs_hook=no_duplicates,parse_constant=lambda _:(_ for _ in ()).throw(ValueError('non-finite')))
        valid=validators[c['entry']].is_valid(value)
    except ValueError:valid=False
    assert valid==c['schemaValid'],c['id']
    if n['typedValid']:validators[c['entry']].validate(n['normalized'])
    results.append({'id':c['id'],'schemaPassed':True,'typedPassed':n['passed'],'schemaValid':valid,'typedValid':n['typedValid']})
normalized={c['id']:c['normalized'] for c in native['cases'] if c['typedValid']}
def digest(request):
    return hashlib.sha256(rfc8785.dumps({'protocol':request['protocol'],'command':request['command']})).hexdigest()
start=normalized['start']; reordered=json.loads(json.dumps(start,sort_keys=True,indent=4));reordered['requestKey']='a-different-key'
assert digest(start)==digest(reordered)
changed=json.loads(json.dumps(start));changed['command']['expectedControlRevision']='2';assert digest(start)!=digest(changed)
changed['command']['rootId']='root-b';assert digest(start)!=digest(changed)
assert [h['holdId'] for h in normalized['normalize-hold-set-order']['command']['coveredHolds']]==['hold-a','hold-z']
# Large counters are decimal strings: JCS preserves their exact spelling without float conversion.
assert b'9223372036854775807' in rfc8785.dumps(normalized['accept-signed64-max'])
with tempfile.TemporaryDirectory(prefix='devworks-frame-fixture-') as tmp:
    oversized=[{'id':'oversized-control-frame','entry':'command-request','raw':' '*(1024*1024)+json.dumps(start),'typedValid':False,'schemaValid':True}]
    fixture=Path(tmp)/'frame.json';fixture.write_text(json.dumps(oversized))
    result=subprocess.run([str(args.binary.resolve()),'check',str(fixture)],text=True,capture_output=True,check=True)
    frame=json.loads(result.stdout);assert frame['allPassed'] and frame['cases'][0]['error']=='frame too large'
report={'status':'Passed candidate schema/DTO checks only; not production gateway or native JCS conformance','date':'2026-11-01','host':platform.platform(),'authoringPackages':{p:importlib.metadata.version(p) for p in ['jsonschema','rfc8785']},'schemaFiles':len(validators),'fixtureCount':len(results),'oversizedFrameRejectedBeforeTypedParsing':True,'allPassed':True,'fingerprintChecks':['object order/whitespace invariant','request key excluded','changed expected revision differs','changed root differs','hold set normalized','large decimal counter preserved'],'cases':results,'nativeResultScope':native['scope'],'inputSha256':{str(p.relative_to(ROOT)):hashlib.sha256(p.read_bytes()).hexdigest() for p in [contracts/'fixtures.json',ROOT/'tools/contracts-eval/src/lib.rs',ROOT/'tools/contracts-eval/Cargo.lock',*sorted(contracts.glob('*.schema.json'))]}}
(args.output/'schema-dto-results.json').write_text(json.dumps(report,indent=2)+'\n')
(args.output/'native-results.json').write_text(run.stdout+'\n')
print(json.dumps({'schemas':len(validators),'fixtures':len(results),'fingerprintChecks':len(report['fingerprintChecks']),'allPassed':True}))
