"""Authoring-only macOS arm64 reproduction. No download, inference or personal browser."""
from pathlib import Path
import argparse
import json
import platform
import shutil
import subprocess
import tempfile


def fixtures(root):
    from reportlab.pdfgen import canvas
    from reportlab.lib.pagesizes import A4
    from PIL import Image, ImageDraw
    from pypdf import PdfReader, PdfWriter
    root.mkdir()
    im = Image.new('RGB', (600, 360), 'white')
    draw = ImageDraw.Draw(im)
    draw.rectangle((4, 4, 595, 355), outline='navy', width=7)
    draw.text((45, 130), 'IMAGE ONLY PAGE', fill='black')
    im.save(root / 'scan.png')
    c = canvas.Canvas(str(root / 'mixed.pdf'), pagesize=A4, invariant=1)
    c.drawString(72, 760, 'DevWorks PDF fixture: native page-bound text.')
    c.showPage()
    c.drawImage(str(root / 'scan.png'), 90, 310, width=420, height=252)
    c.save()
    writer = PdfWriter()
    for page in PdfReader(root / 'mixed.pdf').pages:
        writer.add_page(page)
    writer.encrypt('fixture-pass')
    with (root / 'encrypted.pdf').open('wb') as f:
        writer.write(f)
    (root / 'broken.pdf').write_bytes(b'%PDF-1.7\nbroken fixture\n')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binary', type=Path, required=True)
    parser.add_argument('--pdfium-dir', type=Path, required=True)
    parser.add_argument('--chromium', type=Path, required=True)
    parser.add_argument('--output-dir', type=Path, required=True)
    args = parser.parse_args()
    if platform.system() != 'Darwin' or platform.machine() != 'arm64':
        parser.error('This disposable probe currently qualifies only macOS arm64.')
    binary, lib, chrome, out = [p.resolve() for p in (args.binary, args.pdfium_dir, args.chromium, args.output_dir)]
    if not binary.is_file() or not chrome.is_file() or not (lib / 'lib/libpdfium.dylib').is_file():
        parser.error('Explicit compiled probe, Chromium executable and PDFium library are required.')
    out.mkdir(parents=True, exist_ok=False)
    fixtures(out / 'fixtures')
    def run(kind, extra=()):
        value = json.loads(subprocess.check_output([str(binary), kind, *map(str, extra)], timeout=45))
        assert value['passed'] == len(value['checks'])
        (out / (kind + '-results.json')).write_text(json.dumps(value, indent=2) + '\n')
        return value['passed']
    counts = {kind: run(kind) for kind in ('ui', 'decision')}
    counts['pdf'] = run('pdf', (lib, out / 'fixtures', out / 'pdf-output'))
    for kind in ('guards', 'browser'):
        with tempfile.TemporaryDirectory(prefix='devworks-' + kind + '-') as temporary:
            extra = (chrome, temporary) if kind == 'browser' else (temporary,)
            counts[kind] = run(kind, extra)
            if kind == 'browser':
                shutil.copyfile(Path(temporary) / 'evidence.png', out / 'browser-evidence.png')
    print(json.dumps({'checks': counts, 'total': sum(counts.values()), 'noInference': True}))


if __name__ == '__main__':
    main()
