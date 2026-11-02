"""Reconcile current architecture documents and record scoped QA evidence."""
from pathlib import Path
import hashlib
import json
from docx import Document
from pypdf import PdfReader

ROOT = Path(__file__).resolve().parents[2]
DOCS = ROOT / 'docs'
ARCH = DOCS / 'architecture'
EVIDENCE = ARCH / 'validation/architecture-gates'

def read(path):
    return json.loads(path.read_text())

def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()

trace = read(ARCH / 'devworks-requirements-traceability.json')
plan = read(ARCH / 'implementation-plan.json')
spec = read(ROOT / 'tools/contracts-eval/domain-spec.json')
requirements = read(DOCS / 'planning/requirements-content.json')
assert trace['architectureVersion'] == '1.0'
assert trace['requirementsInputSha256'] == digest(DOCS / 'planning/requirements-content.json')
assert plan['counts']['slices'] == 61
assert plan['counts']['requirements'] == 345 and plan['counts']['operations'] == 347
assert len(plan['dependencyOrder']) == 61 and 'DW-038' not in plan['dependencyOrder']
for name, expected in plan['sourceSha256'].items():
    assert digest(ARCH / name) == expected, name
assert spec['types']['DbEngine']['values'] == ['Sqlite', 'MariaDb', 'MySql', 'Postgres', 'SqlServer']
assert {'AnthropicMessages', 'ClaudeSubscriptionMessages'} <= set(spec['types']['EndpointKind']['values'])
assert 'SubscriptionRisk' in spec['types']['RouterConfigAction']['variants']
assert read(DOCS / 'planning/requirements-interview.json')['pending_questions'] == []
reqs = [b for section in requirements for b in section['blocks'] if b[0] == 'req']
paragraphs = [' '.join(p.text.split()) for p in Document(DOCS / 'planning/devworks-requirements.docx').paragraphs]
assert len(reqs) == 345
for _, rid, status, text in reqs:
    expected = ' '.join((rid + ' ' + status + '. ' + text).split())
    assert expected in paragraphs, (rid, 'Word/source mismatch')

json_files = list(DOCS.rglob('*.json'))
for path in json_files:
    read(path)
readers = read(EVIDENCE / 'reader-checks.json')
links = read(EVIDENCE / 'all-document-links.json')
models = read(EVIDENCE / 'schema-model-results.json')
source = read(EVIDENCE / 'source-binding-results.json')
prototype = read(EVIDENCE / 'prototype-checks.json')
assert len(readers['readers']) == 12 and not readers['pageErrors']
assert links['passed'] and models['allPassed'] and prototype['passed']
files = []
for path in sorted(DOCS.rglob('*')):
    if not path.is_file() or path.suffix.lower() not in ['.md', '.html', '.docx', '.json', '.sql', '.mmd', '.svg', '.txt']:
        continue
    if path == EVIDENCE / 'document-review.json':
        continue
    role = 'Historical or scoped raw evidence' if 'validation' in path.parts else 'Current source or generated reader'
    if path.parent.name == 'architecture-diagrams':
        role = 'Editable or rendered architecture diagram'
    if path.suffix == '.docx':
        role = 'Synchronized Word requirements'
    files.append({'path': str(path.relative_to(ROOT)), 'bytes': path.stat().st_size,
                  'sha256': digest(path), 'role': role})
record = {
    'date': '2026-11-02',
    'status': 'Architecture baseline 1.0 reconciled; production acceptance remains open',
    'scope': 'All document families and current design/requirements/prototype/contract/plan sources; generated mirrors, structural bindings, reader QA and evidence classification. Not a rerun of historical benchmarks or source-method semantic parity.',
    'counts': plan['counts'], 'architectureDecisions': 75, 'diagrams': 43,
    'htmlDocumentsChecked': len(links['documents']), 'localTargetsChecked': links['localTargetsChecked'],
    'imagesChecked': links['images'], 'schemaFixtureChecks': models['allMessageFixtures'],
    'frozenSourceFilesVerified': source['sourceFilesVerified'],
    'jsonFilesParsed': len(json_files), 'invalidJsonFiles': [],
    'word': {
        'pages': len(PdfReader(str(ROOT / '.planning-build/architecture-gates/word-delivery/devworks-requirements.pdf')).pages),
        'sourceParagraphsVerified': 345,
        'visualReview': 'Page contact sheets and full-size changed pages reviewed; unchanged pages across final iterations compared byte-for-byte. Final diagram page inspected separately.',
        'sha256': digest(DOCS / 'planning/devworks-requirements.docx')},
    'userDecisions': [
        'Optional direct simple-router Claude subscription at user risk, plus separate Anthropic API-key access. No external Claude CLI.',
        'External HelixDB reader removed; DW-038 retired; internal indexes remain SQLite/FTS5/sqlite-vec.'],
    'remaining': 'G01–G12 actual application/adapter/SDK/provider/platform/correctness/recovery/parity/performance/installer acceptance. No real inference or broad application implementation run.',
    'documents': files,
}
(EVIDENCE / 'document-review.json').write_text(json.dumps(record, indent=2, ensure_ascii=False) + '\n')
print(json.dumps({k: record[k] for k in ['status', 'htmlDocumentsChecked', 'localTargetsChecked',
                                      'imagesChecked', 'schemaFixtureChecks', 'jsonFilesParsed', 'invalidJsonFiles']}))
