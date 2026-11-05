"""Authoring consistency checks; never invokes native services or model inference."""
from pathlib import Path
import argparse
import hashlib
import json
import subprocess
from docx import Document
from pypdf import PdfReader

ROOT = Path(__file__).resolve().parents[2]
def read(path): return json.loads((ROOT / path).read_text())
def digest(path): return hashlib.sha256((ROOT / path).read_bytes()).hexdigest()
args = argparse.ArgumentParser()
args.add_argument('--codegraph-snapshot', type=Path)
opts = args.parse_args()
plan = read('docs/planning/auto-routing-plan.json')
assert plan['status'].startswith('Approved')
requirements_path = 'docs/planning/requirements-content.json'
requirements = read(requirements_path)
reqs = {b[1]:b for s in requirements for b in s['blocks'] if b[0]=='req'}
assert len(reqs) == 374
prior = json.loads(subprocess.check_output(['git','show',plan['source_commit']+':'+requirements_path],cwd=ROOT))
prior_reqs = {b[1]:b for s in prior for b in s['blocks'] if b[0]=='req'}
assert set(reqs)-set(prior_reqs) == {'ROUTE-21','ROUTE-22','ROUTE-23','ROUTE-24'}
assert all(reqs[k] == v for k,v in prior_reqs.items() if k!='ROUTE-19')
paragraphs = {' '.join(p.text.split()) for p in Document(ROOT/'docs/planning/devworks-requirements.docx').paragraphs}
for _, rid, status, text in reqs.values():
    assert ' '.join((rid+' '+status+'. '+text).split()) in paragraphs, rid
for delta in plan['requirements_delta']:
    b = reqs[delta['source_requirement']]
    assert delta['after'] == f'{b[1]} [{b[2]}]: {b[3]}'
implementation = read('docs/architecture/implementation-plan.json')
for name, expected in implementation['sourceSha256'].items():
    assert digest('docs/architecture/'+name) == expected, name
bykey = {s['key']:s for s in implementation['slices']}
assert bykey['session_auto_routing']['dependsOnKeys'] == ['chat','tool_turn','systemone']
assert 'session_auto_routing' in bykey['acceptance']['dependsOnKeys']
assert set(bykey['session_auto_routing']['primaryRequirements']) == {'ROUTE-19','ROUTE-22','ROUTE-23','ROUTE-24'}
assert 'ROUTE-21' in bykey['systemone']['primaryRequirements']
assert not bykey['session_auto_routing']['ownedOperations']
trace = read('docs/architecture/devworks-requirements-traceability.json')
assert trace['requirementsInputSha256'] == digest(requirements_path)
report = {'date':'2026-11-05','scope':'Approved planning publication only; no runtime or inference acceptance',
          'requirementsVerified':len(reqs),'unrelatedRequirementsPreserved':len(prior_reqs)-1,
          'counts':implementation['counts'],'word':{'pages':len(PdfReader(str(ROOT/'.planning-build/auto-routing/word/devworks-requirements.pdf')).pages),
          'sourceParagraphsVerified':len(reqs),'visualReview':'All72 pages inspected in contact sheets; routing pages18,19,28 inspected at full size. No clipping or overlap observed.',
          'sha256':digest('docs/planning/devworks-requirements.docx')},
          'readers':read('.planning-build/auto-routing/readers/reader-checks.json')}
assert report['readers']['passed']
if opts.codegraph_snapshot:
    snapshot = json.loads(opts.codegraph_snapshot.read_text())
    feature = {r['id']:r['text'] for r in snapshot['feature']['requirements']}
    for delta in plan['requirements_delta']:
        assert feature[delta['feature_requirement']] == delta['after']
    assert snapshot['auto']['status'] == 'ready'
    assert {b['key'] for b in snapshot['auto']['blocked_by']} == {'DW-16','DW-20','DW-22'}
    assert 'DW-107' in {b['key'] for b in snapshot['acceptance']['blocked_by']}
    for name,key in [('systemone','existing_item_change'),('auto','new_item')]:
        assert [a['text'] for a in snapshot[name]['acceptance_criteria']] == [a['text'] for a in plan['implementation'][key]['acceptance_criteria']]
    report['codegraphVersions'] = {s['key']:s['version'] for s in snapshot.values() if isinstance(s,dict) and 'version' in s}
report['passed'] = True
out = ROOT/'docs/architecture/validation/auto-routing-plan-review.json'
out.write_text(json.dumps(report,indent=2)+'\n')
print(json.dumps(report))
