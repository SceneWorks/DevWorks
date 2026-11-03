from pathlib import Path
from html import escape
import argparse, json, re, shutil
from PIL import Image, ImageDraw, ImageFont
from docx import Document
from docx.shared import Inches, Pt, RGBColor
from docx.enum.text import WD_ALIGN_PARAGRAPH
from docx.enum.table import WD_TABLE_ALIGNMENT, WD_CELL_VERTICAL_ALIGNMENT
from docx.oxml import OxmlElement
from docx.oxml.ns import qn
from docx.opc.constants import RELATIONSHIP_TYPE as RT
from copy import deepcopy
from requirements_content import sections

parser=argparse.ArgumentParser(description='Build the DevWorks planning documents')
parser.add_argument('--export-dir',type=Path,help='Also copy generated planning artifacts to this directory')
args=parser.parse_args()
ROOT=Path(__file__).resolve().parents[2]
OUT=ROOT/'docs/planning'; OUT.mkdir(parents=True,exist_ok=True)
WORK=ROOT/'.planning-build'; WORK.mkdir(exist_ok=True)
def asset_path(path):
 if path.startswith('work/'):return WORK/path.removeprefix('work/')
 if path.startswith('outputs/'):return OUT/path.removeprefix('outputs/')
 return ROOT/path
# A simple architectural figure drawn from the proposed component model.
W,H=2100,1150
im=Image.new('RGB',(W,H),'white');d=ImageDraw.Draw(im)
font_pairs=[('/System/Library/Fonts/Supplemental/Arial.ttf','/System/Library/Fonts/Supplemental/Arial Bold.ttf'),('C:/Windows/Fonts/arial.ttf','C:/Windows/Fonts/arialbd.ttf'),('/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf','/usr/share/fonts/truetype/dejavu/DejaVuSans-Bold.ttf')]
font,bold=next(((a,b) for a,b in font_pairs if Path(a).is_file() and Path(b).is_file()),(None,None))
if font is None:raise RuntimeError('Planning diagram generation needs Arial or DejaVu Sans fonts')
f=lambda size,b=False:ImageFont.truetype(bold if b else font,size)
def box(x,y,w,h,title,lines,fill='#F1F3F5'):
 d.rounded_rectangle((x,y,x+w,y+h),radius=10,fill=fill,outline='#939DA8',width=2)
 d.text((x+w/2,y+25),title,font=f(31,True),anchor='mt',fill='#101820')
 for i,t in enumerate(lines):d.text((x+w/2,y+78+i*37),t,font=f(25),anchor='mt',fill='#35404A')
def arrow(x1,y1,x2,y2):
 d.line((x1,y1,x2,y2),fill='#687481',width=4);d.polygon([(x2,y2),(x2-9,y2-16),(x2+9,y2-16)],fill='#687481')
box(60,25,1980,130,'Native desktop application with egui',['Chat and workspaces   •   UI controls and agent tools'],fill='#E6EBF0')
arrow(1050,155,1050,198)
box(60,200,1980,130,'Shared application commands and queries',['Validation   •   policy   •   revisions   •   attribution   •   durable events'])
for x,title,lines in [(60,'Agent runtime',['Workspace and feature coordinators','Workers and execution recovery']),(730,'Inference router',['Providers and model policy','Limits streaming and usage']),(1400,'Code and knowledge',['Parsing and semantic enrichment','Graph retrieval and dreaming'])]:
 arrow(x+320,330,x+320,390);box(x,390,640,170,title,lines)
for x in (380,1050,1720):
 d.line((x,560,x,590),fill='#687481',width=4)
d.line((380,590,1720,590),fill='#687481',width=4)
for x,title,lines in [(60,'Operational repositories',['Work state transcripts and journals','SQLite adapter implementation']),(730,'Graph and retrieval traits',['Node edge tables and sqlite-vec','Shared FTS and scoped read views']),(1400,'Files and credentials',['Git worktrees and artifacts','OS credential storage'])]:
 arrow(x+320,590,x+320,620);box(x,620,640,170,title,lines)
box(60,860,1980,160,'App managed adapters and background work',['Git and local processes   •   optional semantic helpers   •   provider and Git host APIs','No separately administered application server database or message broker'],fill='#FAFAFA')
d.text((1050,1080),'Architecture baseline 1.1   •   production adapter and platform acceptance remains required',font=f(24),anchor='mt',fill='#535E69')
(WORK/'requirements-architecture.png').parent.mkdir(exist_ok=True);im.save(WORK/'requirements-architecture.png')
# Explicitly capture the rewrite boundary rather than accidentally requiring agent CLIs.
arch=next(s for s in sections if s['title'].startswith('4 '))
arch05=('req','ARCH-05','Confirmed','The core coordinator and worker harness must be implemented in the Rust application and use its router and tools directly. Running the app must not require a separately installed Claude Code, Codex CLI, or other external agent harness. Optional tool adapters must declare their own dependencies.')
arch['blocks'].insert(next((i for i,b in enumerate(arch['blocks']) if b[0]=='req' and b[1]=='ARCH-06'),len(arch['blocks'])),arch05)

reqs=[b for s in sections for b in s['blocks'] if b[0]=='req'];ids=[b[1] for b in reqs]
assert len(ids)==len(set(ids))
DOC=Document();sec=DOC.sections[0];sec.page_width=Inches(8.5);sec.page_height=Inches(11)
sec.top_margin=Inches(.68);sec.bottom_margin=Inches(.65);sec.left_margin=Inches(.72);sec.right_margin=Inches(.72)
sec.header_distance=Inches(.28);sec.footer_distance=Inches(.28)
styles=DOC.styles
for node in list(styles.element.xpath('.//w:pBdr')):node.getparent().remove(node)
for name in ['Normal','Title','Subtitle','Heading 1','Heading 2','Heading 3','Caption']:
 st=styles[name];st.font.name='Arial';st.font.color.rgb=RGBColor(0,0,0)
 for attr in ('ascii','hAnsi','eastAsia','cs'):st.element.get_or_add_rPr().get_or_add_rFonts().set(qn('w:'+attr),'Arial')
styles['Normal'].font.size=Pt(10.5);styles['Normal'].paragraph_format.space_after=Pt(6);styles['Normal'].paragraph_format.line_spacing=1.1
styles['Title'].font.size=Pt(27);styles['Title'].font.bold=True;styles['Title'].paragraph_format.space_after=Pt(10)
styles['Subtitle'].font.italic=False;styles['Subtitle'].font.size=Pt(13);styles['Subtitle'].paragraph_format.space_after=Pt(13)
for name,size in [('Heading 1',16),('Heading 2',11.5),('Heading 3',10.5)]:
 styles[name].font.size=Pt(size);styles[name].font.bold=True;styles[name].paragraph_format.space_before=Pt(15 if name=='Heading 1' else 10);styles[name].paragraph_format.space_after=Pt(7);styles[name].paragraph_format.keep_with_next=True
styles['Caption'].font.size=Pt(9);styles['Caption'].font.italic=True;styles['Caption'].paragraph_format.space_after=Pt(10)
# A restrained running title and page numbers aid navigation in this long specification.
hp=sec.header.paragraphs[0];hp.text='DevWorks requirements   |   Requirements revision 0.4';hp.style=styles['Normal'];hp.runs[0].font.size=Pt(8)
fp=sec.footer.paragraphs[0];fp.alignment=WD_ALIGN_PARAGRAPH.RIGHT
r=fp.add_run('Page ');r.font.size=Pt(8)
fld=OxmlElement('w:fldSimple');fld.set(qn('w:instr'),'PAGE');fp._p.append(fld)
DOC.core_properties.title='DevWorks requirements';DOC.core_properties.subject='Product architecture UI and coordinator behavior';DOC.core_properties.author='Codex';DOC.core_properties.keywords='requirements review draft Rust native desktop autonomous agents'
DOC.add_paragraph('DevWorks requirements','Title');DOC.add_paragraph('Product architecture UI and coordinator behavior','Subtitle')
DOC.add_paragraph('Requirements revision 0.4   |   2 November 2026')
p=DOC.add_paragraph();p.add_run('Purpose. ').bold=True;p.add_run('Capture the complete discussed product direction for review before implementation. The application is a local native desktop home for autonomous software work, with a workspace coordinator conversation as its primary interface.')
p=DOC.add_paragraph();p.add_run('Review state. ').bold=True;p.add_run('Confirmed requirements, proposed details, and open decisions are explicitly distinguished. egui is selected. Product choices are captured and architecture baseline 1.1 is complete. Proposed requirements retain their status. Actual service, source capability, provider, platform and performance acceptance remains in the local implementation plan.')
DOC.add_heading('Contents',level=2)
for s in sections:
 p=DOC.add_paragraph(s['title']);p.paragraph_format.space_after=Pt(3);p.runs[0].font.size=Pt(10)
DOC.add_page_break()

def link_github_references(paragraph):
 for run in list(paragraph.runs):
  matches=list(re.finditer(r'https://github\.com/[^\s<>]+',run.text))
  if not matches:continue
  parts=[];pos=0
  for match in matches:
   if match.start()>pos:parts.append((run.text[pos:match.start()],False))
   url=match.group().rstrip('.,;:');parts.append((url,True))
   if len(url)<len(match.group()):parts.append((match.group()[len(url):],False))
   pos=match.end()
  if pos<len(run.text):parts.append((run.text[pos:],False))
  for text,is_link in parts:
   r=OxmlElement('w:r')
   if run._r.rPr is not None:r.append(deepcopy(run._r.rPr))
   t=OxmlElement('w:t');t.set(qn('xml:space'),'preserve');t.text=text;r.append(t)
   if is_link:
    link=OxmlElement('w:hyperlink');link.set(qn('r:id'),paragraph.part.relate_to(text,RT.HYPERLINK,is_external=True));link.append(r)
    pr=r.get_or_add_rPr();color=OxmlElement('w:color');color.set(qn('w:val'),'245B9C');pr.append(color)
    underline=OxmlElement('w:u');underline.set(qn('w:val'),'single');pr.append(underline)
    run._r.addprevious(link)
   else:run._r.addprevious(r)
  run._r.getparent().remove(run._r)

def add_table(headers,rows,widths):
 t=DOC.add_table(rows=1,cols=len(headers));t.alignment=WD_TABLE_ALIGNMENT.CENTER;t.autofit=False
 widths=widths or [6.9/len(headers)]*len(headers)
 for c,w in zip(t.columns,widths):c.width=Inches(w)
 for c,w,text in zip(t.rows[0].cells,widths,headers):c.width=Inches(w);c.text=text
 trpr=t.rows[0]._tr.get_or_add_trPr();repeat=OxmlElement('w:tblHeader');trpr.append(repeat)
 for row in rows:
  cells=t.add_row().cells
  for c,w,text in zip(cells,widths,row):c.width=Inches(w);c.text=str(text)
 for i,row in enumerate(t.rows):
  trpr=row._tr.get_or_add_trPr();no=OxmlElement('w:cantSplit');trpr.append(no)
  for j,c in enumerate(row.cells):
   c.vertical_alignment=WD_CELL_VERTICAL_ALIGNMENT.CENTER
   pr=c._tc.get_or_add_tcPr();m=OxmlElement('w:tcMar')
   for edge,val in [('top','85'),('bottom','85'),('left','110'),('right','110')]:
    e=OxmlElement('w:'+edge);e.set(qn('w:w'),val);e.set(qn('w:type'),'dxa');m.append(e)
   pr.append(m);b=OxmlElement('w:tcBorders')
   for edge in ['top','left','bottom','right']:
    e=OxmlElement('w:'+edge);e.set(qn('w:val'),'single');e.set(qn('w:sz'),'4');e.set(qn('w:color'),'D9D9D9');b.append(e)
   pr.append(b);sh=OxmlElement('w:shd');sh.set(qn('w:fill'),'344251' if i==0 else 'F3F5F7' if i%2==0 else 'FFFFFF');pr.append(sh)
   for p in c.paragraphs:
    p.paragraph_format.space_after=Pt(0);p.paragraph_format.line_spacing=1.05
    for r in p.runs:r.font.size=Pt(9.25);r.font.color.rgb=RGBColor(255,255,255) if i==0 else RGBColor(0,0,0);r.bold=i==0
 for row in t.rows:
  for cell in row.cells:
   for p in cell.paragraphs:link_github_references(p)
 DOC.add_paragraph().paragraph_format.space_after=Pt(1)

htmlparts=[]
for i,s in enumerate(sections,1):
 DOC.add_heading(s['title'],level=1)
 hp=[f'<section id="s{i}"><h2>{escape(s["title"])}</h2>']
 if s['intro']:DOC.add_paragraph(s['intro']);hp.append('<p>'+escape(s['intro'])+'</p>')
 for b in s['blocks']:
  if b[0]=='p':
   p=DOC.add_paragraph(b[1]);link_github_references(p);hp.append('<p>'+escape(b[1])+'</p>')
   if b[1].startswith('Repository navigation baseline:'):
    p.paragraph_format.line_spacing=1
    p.paragraph_format.space_after=Pt(0)
    for run in p.runs:run.font.size=Pt(9)
  elif b[0]=='h2':DOC.add_heading(b[1],level=2);hp.append('<h3>'+escape(b[1])+'</h3>')
  elif b[0]=='req':
   _,rid,status,text=b;p=DOC.add_paragraph();p.paragraph_format.keep_together=True
   p.add_run(rid+'  '+status+'. ').bold=True;p.add_run(text);link_github_references(p)
   hp.append(f'<p class="requirement" id="{rid}"><strong>{rid}</strong><span class="status {status.lower()}">{status}</span>{escape(text)}</p>')
  elif b[0]=='table':
   _,headers,rows,widths=b;add_table(headers,rows,widths)
   hp.append('<div class="table-wrap"><table><thead><tr>'+''.join('<th>'+escape(str(x))+'</th>' for x in headers)+'</tr></thead><tbody>'+''.join('<tr>'+''.join('<td>'+escape(str(x))+'</td>' for x in row)+'</tr>' for row in rows)+'</tbody></table></div>')
  elif b[0]=='figure':
   _,path,caption=b;pp=DOC.add_paragraph();pp.paragraph_format.keep_with_next=True
   pp.add_run().add_picture(str(asset_path(path)),width=Inches(6.9));docpr=pp._p.xpath('.//wp:docPr')[0];docpr.set('descr',caption)
   DOC.add_paragraph(caption,'Caption')
   if path.startswith('work/'):
    (OUT/'requirements-assets').mkdir(exist_ok=True);shutil.copyfile(asset_path(path),OUT/'requirements-assets/architecture.png');src='requirements-assets/architecture.png'
   else:src=path.removeprefix('outputs/')
   hp.append(f'<figure><a href="{src}" target="_blank"><img src="{src}" alt="{escape(caption)}"></a><figcaption>{escape(caption)}</figcaption></figure>')
 hp.append('</section>');htmlparts.append(''.join(hp))
# Remove trailing empty spacer after the final table to avoid an empty final page.
while len(DOC.element.body)>1 and DOC.element.body[-2].tag==qn('w:p') and not ''.join(DOC.element.body[-2].itertext()).strip() and not DOC.element.body[-2].xpath('.//w:drawing'):
 DOC.element.body.remove(DOC.element.body[-2])
DOC.save(OUT/'devworks-requirements.docx')
nav=''.join(f'<a href="#s{i}">{escape(s["title"])}</a>' for i,s in enumerate(sections,1))
html='''<!doctype html><html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1"><title>DevWorks requirements</title><link rel="icon" href="data:,"><style>
:root{color-scheme:light}*{box-sizing:border-box}body{margin:0;background:#f6f7f9;color:#17202a;font:calc(15px + 1pt)/1.7 -apple-system,BlinkMacSystemFont,'Segoe UI',sans-serif}a{color:#245b9c}nav{position:fixed;inset:0 auto 0 0;width:272px;padding:28px 22px;background:#e9edf2;border-right:1px solid #d5dce5;overflow:auto}nav b{display:block;margin-bottom:18px;font-size:calc(14px + 1pt);color:#111}nav a{display:block;text-decoration:none;font-size:calc(12px + 1pt);line-height:1.6;padding:6px 0;color:#3b4d62}main{margin-left:272px;padding:42px 54px 80px;max-width:1300px}header{padding-bottom:22px}h1{font-size:calc(38px + 1pt);letter-spacing:-1px;line-height:1.2;margin:12px 0 15px;color:#000}h2{font-size:calc(25px + 1pt);letter-spacing:-.4px;line-height:1.35;margin:45px 0 20px;color:#000}h3{font-size:calc(17px + 1pt);margin:28px 0 12px;color:#000}section{scroll-margin-top:25px}p{margin:0 0 16px}header .meta{font-size:calc(12px + 1pt);color:#526174}.intro{font-size:calc(17px + 1pt)}.actions{display:flex;gap:24px;margin:20px 0;font-size:calc(13px + 1pt)}.requirement{background:white;border:1px solid #dce1e8;border-radius:4px;padding:15px 18px;font-size:calc(14px + 1pt)}.requirement strong{margin-right:10px;font-size:calc(12px + 1pt)}.status{display:inline-block;font-size:calc(10px + 1pt);margin-right:10px;border:1px solid #b8c7d6;padding:1px 6px;border-radius:3px;vertical-align:1px;color:#3c5268}.status.confirmed{background:#e8f1eb;border-color:#bfd1c5;color:#315a42}.status.proposed{background:#f5f1e7;border-color:#dbceb0;color:#705b30}.table-wrap{overflow:auto;margin:18px 0 25px}table{border-collapse:collapse;width:100%;font-size:calc(13px + 1pt);background:white}th,td{padding:11px 13px;border:1px solid #ccd3dd;text-align:left;vertical-align:top}th{background:#344251;color:white;font-weight:600}tbody tr:nth-child(even){background:#eef1f5}td:first-child{min-width:100px}figure{margin:25px 0}figure img{width:100%;height:auto;border:1px solid #d5dce5}figcaption{font-size:calc(12px + 1pt);color:#526174;margin-top:8px}code{font-size:calc(12px + 1pt)}a:focus-visible{outline:2px solid #245b9c;outline-offset:3px}@media(max-width:950px){nav{display:none}main{margin:0;padding:28px 22px}h1{font-size:calc(31px + 1pt)}}@media print{nav,.actions{display:none}main{margin:0;padding:0}body{background:white}.requirement{break-inside:avoid}h2,h3{break-after:avoid}table{font-size:calc(10px + 1pt)}th,td{padding:7px}section{break-inside:auto}figure{break-inside:avoid}}
</style></head><body><nav><b>DevWorks requirements</b>'''+nav+'''</nav><main><header><p class="meta">Requirements revision 0.4 · 2 November 2026</p><h1>DevWorks requirements</h1><p class="intro">Product architecture, UI, and coordinator behavior consolidated for review before implementation.</p><div class="actions"><a href="devworks-requirements.docx">Download editable Word document</a><a href="harness-workbench.html?page=chat&revision=8">Open UI prototype</a><a href="mockup-gallery.html">View screen gallery</a><a href="ui-reference.html">Approved UI reference</a></div><p>Confirmed directions and proposed implementation details are labeled throughout. Architecture resolutions and implementation acceptance are listed in Section 22.</p></header>'''+''.join(htmlparts)+'''</main></body></html>'''
def github_html_link(match):
 url=match.group().rstrip('.,;:');suffix=match.group()[len(url):]
 return '<a href="'+url+'">'+url+'</a>'+suffix
html=re.sub(r'https://github\.com/[^\s<>]+',github_html_link,html)
(OUT/'devworks-requirements.html').write_text(html)

(OUT/'requirements-content.json').write_text(json.dumps(sections,indent=2))
words=sum(len(' '.join(str(x) for x in b[1:]).split()) for s in sections for b in s['blocks'])
print(json.dumps({'sections':len(sections),'requirements':len(ids),'words_approx':words,'docx_bytes':(OUT/'devworks-requirements.docx').stat().st_size}))

if args.export_dir:
 destination=args.export_dir.expanduser().resolve()
 if destination!=OUT.resolve():shutil.copytree(OUT,destination,dirs_exist_ok=True)
 for suffix in ('html','docx'):
  shutil.copyfile(destination/('devworks-requirements.'+suffix),destination/('superharness-requirements.'+suffix))
