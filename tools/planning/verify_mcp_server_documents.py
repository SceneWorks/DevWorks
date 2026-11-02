"""Verify the baseline 1.1 amendment against its actual requirement/contract sources."""
from pathlib import Path
import hashlib
import json
from docx import Document
from pypdf import PdfReader

ROOT = Path(__file__).resolve().parents[2]
ARCH = ROOT / 'docs/architecture'
PLANNING = ROOT / 'docs/planning'
EVIDENCE = ARCH / 'validation/mcp-server-amendment'
def read(p):
    return json.loads(p.read_text())
def digest(p):
    return hashlib.sha256(p.read_bytes()).hexdigest()

trace = read(ARCH / 'devworks-requirements-traceability.json')
plan = read(ARCH / 'implementation-plan.json')
requirements = read(PLANNING / 'requirements-content.json')
reqs = [b for section in requirements for b in section['blocks'] if b[0] == 'req']
assert trace['architectureVersion'] == '1.1'
assert trace['requirementsInputSha256'] == digest(PLANNING / 'requirements-content.json')
assert plan['counts']['requirements'] == len(reqs) == 350
assert plan['counts']['requirementsByStatus'] == {'Confirmed': 209, 'Proposed': 141}
assert plan['counts']['operations'] == 351 and plan['counts']['operationFamilies'] == 48
assert plan['counts']['slices'] == len(plan['dependencyOrder']) == 63
assert 'DW-038' not in plan['dependencyOrder']
assert {'DW-063', 'DW-064'} <= set(plan['dependencyOrder'])
assert all(plan['counts'][k] == 0 for k in ['unownedRequirements', 'unownedOperations', 'unownedRetainedEntries'])
for name, expected in plan['sourceSha256'].items():
    assert digest(ARCH / name) == expected, name
acceptance = next(s for s in plan['slices'] if s['key'] == 'acceptance')
assert {'mcp_server', 'mcp_server_access'} <= set(acceptance['dependsOnKeys'])
for n in range(7, 12):
    r = next(r for r in plan['requirements'] if r['requirement'] == f'MCP-{n:02}')
    assert r['primarySlice'] == 'mcp_server' and 'mcp_server_access' in r['supportingSlices']

interview = read(PLANNING / 'requirements-interview.json')
assert interview['pending_questions'] == []
def objects(v):
    if isinstance(v, dict):
        yield v
        for x in v.values():
            yield from objects(x)
    elif isinstance(v, list):
        for x in v:
            yield from objects(x)
decisions = {x['id']: x for x in objects(interview) if 'id' in x}
assert decisions['mcp_client_only']['status'] == 'Superseded'
assert decisions['mcp_server_for_other_harnesses']['status'] == 'Current'

doc = Document(PLANNING / 'devworks-requirements.docx')
paragraphs = [' '.join(p.text.split()) for p in doc.paragraphs]
for _, rid, status, text in reqs:
    assert ' '.join((rid + ' ' + status + '. ' + text).split()) in paragraphs, rid
table_text = '\n'.join(c.text for t in doc.tables for row in t.rows for c in row.cells)
assert 'incoming DevWorks server' in table_text and 'per-client project/worktree scopes' in table_text
assert 'MCP server protocol and native adapter' in table_text
assert '350 requirements' in table_text and '351 operations/48 families' in table_text

publication = read(ARCH / 'mcp-publication-candidates.json')
coverage = read(ARCH / 'contracts/family-coverage.json')
assert publication['sourceSha256'] == digest(ARCH / publication['source'])
expected_modes = {(o['operation'], m) for f in coverage['families'] for o in f['operations'] for m in o['requestModes']}
assert {(d['operation'], d['mode']) for d in publication['dispositions']} == expected_modes
assert len(publication['dispositions']) == len(expected_modes)
assert len({t['name'] for t in publication['tools']}) == len(publication['tools'])
assert all(not t['defaultForNewRegistration'] for t in publication['tools'])
assert all(t['requiresApplicationIdempotencyKey'] == (t['mode'] == 'Command') for t in publication['tools'])
assert all(t['authority'] not in ['Configuration', 'HumanConfiguration'] for t in publication['tools'])
public = {t['operation'] for t in publication['tools']}
assert {'code.search', 'code.architecture', 'code.source', 'features.get', 'tasks.list',
        'history.work_lineage', 'knowledge.search', 'knowledge.entity_bundle', 'documents.search',
        'conventions.search', 'features.create', 'knowledge.record'} <= public
assert not any(n.startswith(('mcp.', 'mcp_server.', 'process.', 'desktop.', 'approvals.')) for n in public)

readers = read(EVIDENCE / 'reader-checks.json')
links = read(EVIDENCE / 'all-document-links.json')
model = read(EVIDENCE / 'schema-model-results.json')
source = read(EVIDENCE / 'source-binding-results.json')
prototype = read(EVIDENCE / 'prototype-checks.json')
assert len(readers['readers']) == 13 and not readers['pageErrors']
assert links['passed'] and model['allPassed'] and prototype['passed'] and source['allPassed']
assert model['catalogOperations'] == model['inputCandidates'] == model['qualifiedResultTags'] == 351
assert model['allMessageFixtures'] == 1049
for relative, expected in model['sourceSha256'].items():
    assert digest(ROOT / relative) == expected, ('Schema evidence source drift', relative)
json_files = list((ROOT / 'docs').rglob('*.json'))
for p in json_files:
    read(p)
pdf = ROOT / '.planning-build/mcp-server-amendment/word-delivery/devworks-requirements.pdf'
report = {
    'date': '2026-11-02', 'architectureVersion': '1.1', 'passed': True,
    'status': 'Requirements, publication candidates, ownership, generated documents and scoped QA reconciled; native MCP server unimplemented',
    'counts': plan['counts'], 'publication': publication['counts'],
    'architectureDecisions': 78, 'diagrams': 44, 'htmlReaders': len(readers['readers']),
    'htmlDocumentsChecked': len(links['documents']), 'localTargetsChecked': links['localTargetsChecked'],
    'imagesChecked': links['images'], 'jsonFilesParsed': len(json_files),
    'schemaFixtureChecks': model['allMessageFixtures'], 'wordRequirementsVerified': len(reqs),
    'wordPages': len(PdfReader(str(pdf)).pages), 'wordSha256': digest(PLANNING / 'devworks-requirements.docx'),
    'wordVisualReview': 'All pages reviewed in contact sheets; affected MCP, component/settings, acceptance and reference pages reviewed at full size after final rendering.',
    'remaining': 'DW-063/064 actual native transport, credentials/IPC, complete projection, two-client, command authority/recovery and supported-target acceptance; all prior G01–G12 production obligations remain.',
    'sources': {str(p.relative_to(ROOT)): digest(p) for p in [
        ARCH / 'devworks-architecture.md', ARCH / 'mcp-server-contracts.md',
        ARCH / 'mcp-publication-candidates.json', ARCH / 'implementation-plan.json',
        PLANNING / 'requirements-content.json', PLANNING / 'requirements-interview.json',
        PLANNING / 'devworks-requirements.docx', ROOT / 'tools/planning/build_mcp_publication.py']},
}
(EVIDENCE / 'document-review.json').write_text(json.dumps(report, indent=2) + '\n')
print(json.dumps({k: report[k] for k in ['passed', 'htmlDocumentsChecked', 'localTargetsChecked', 'schemaFixtureChecks', 'wordRequirementsVerified', 'wordPages', 'jsonFilesParsed']}))
