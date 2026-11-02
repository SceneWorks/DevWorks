"""Deterministic fixtures for the narrow Rust DOCX architecture probe; no inference."""
import argparse
import hashlib
import json
from pathlib import Path
import platform
import subprocess
import zipfile

parser = argparse.ArgumentParser()
parser.add_argument('--binary', type=Path, required=True)
parser.add_argument('--output', type=Path, required=True)
parser.add_argument('--document', type=Path)
args = parser.parse_args()
args.output.mkdir(parents=True, exist_ok=True)
W = 'http://schemas.openxmlformats.org/wordprocessingml/2006/main'
def wrap(body):
    return f'<w:document xmlns:w="{W}"><w:body>{body}</w:body></w:document>'
def package(name, xml, extra=(), main=True):
    path = args.output / name
    with zipfile.ZipFile(path, 'w', zipfile.ZIP_DEFLATED) as z:
        z.writestr('[Content_Types].xml', '<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"/>')
        if main:
            z.writestr('word/document.xml', xml)
        for key, value in extra:
            z.writestr(key, value)
    return path
valid = wrap('<w:p><w:r><w:t>A &amp; B — café</w:t></w:r><w:r><w:t> &#x3bb;</w:t><w:tab/><w:t>end</w:t></w:r></w:p><w:tbl><w:tr><w:tc><w:p><w:r><w:t>Cell</w:t></w:r></w:p></w:tc></w:tr></w:tbl>')
fixtures = [
    ('body-runs-unicode-entities-table.docx', valid, (), True, None),
    ('missing-main.docx', '', (), False, 'MissingRequiredPart'),
    ('malformed.docx', '<w:document><w:body></w:document>', (), True, 'MalformedXml'),
    ('dtd.docx', '<!DOCTYPE x [<!ENTITY x "bad">]>' + valid, (), True, 'DtdForbidden'),
    ('unsafe-path.docx', valid, (('../outside.xml', 'x'),), True, 'UnsafePartPath'),
    ('duplicate-case.docx', valid, (('WORD/document.xml', 'x'),), True, 'DuplicatePart'),
    ('main-too-large.docx', wrap('<w:p><w:r><w:t>' + 'x' * (9*1024*1024) + '</w:t></w:r></w:p>'), (), True, 'MainPartTooLarge'),
    ('deep-xml.docx', wrap('<x>'*130 + '</x>'*130), (), True, 'XmlDepthExceeded'),
    ('legacy.doc', valid, (), True, 'UnsupportedFormat'),
    ('external-reference-inert.docx', valid, (('word/_rels/document.xml.rels', '<Relationships><Relationship TargetMode="External" Target="https://invalid.example/not-fetched"/></Relationships>'),), True, None),
]
results = []
for name, xml, extra, main, rejection in fixtures:
    path = package(name, xml, extra, main)
    run = subprocess.run([str(args.binary.resolve()), str(path)], capture_output=True, text=True, timeout=10)
    value = json.loads(run.stdout)
    if rejection:
        passed = run.returncode == 2 and value['reason'].startswith(rejection)
    else:
        passed = run.returncode == 0 and [x['text'] for x in value['paragraphs']] == ['A & B — café λ\tend', 'Cell'] and value['tableCells'] == 1 and [x['locator'] for x in value['paragraphs']] == ['word/document.xml#paragraph/1', 'word/document.xml#paragraph/2']
    results.append({'case': name, 'passed': passed, 'expectedRejection': rejection, 'output': value})
if args.document:
    run = subprocess.run([str(args.binary.resolve()), str(args.document.resolve())], capture_output=True, text=True, timeout=10)
    value = json.loads(run.stdout)
    results.append({'case': 'requirements-document-body', 'passed': run.returncode == 0 and len(value.get('paragraphs', [])) > 100,
                    'sha256': hashlib.sha256(args.document.read_bytes()).hexdigest(), 'paragraphs': len(value.get('paragraphs', [])), 'tableCells': value.get('tableCells')})
report = {'status': 'Narrow main-body feasibility only; not production extraction/security/platform acceptance', 'host': platform.platform(), 'cases': results, 'allPassed': all(x['passed'] for x in results)}
(args.output / 'results.json').write_text(json.dumps(report, ensure_ascii=False, indent=2) + '\n')
print(json.dumps({'allPassed': report['allPassed'], 'cases': len(results), 'failures': [x['case'] for x in results if not x['passed']]}))
raise SystemExit(0 if report['allPassed'] else 1)
