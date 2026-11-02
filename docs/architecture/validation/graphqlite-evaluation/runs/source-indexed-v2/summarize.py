from pathlib import Path
import argparse,json,statistics
p=argparse.ArgumentParser();p.add_argument('manifest',type=Path);p.add_argument('--output',type=Path,required=True);a=p.parse_args();m=json.loads(a.manifest.read_text())
def agg(values):
 return {'median':statistics.median(values),'min':min(values),'max':max(values),'runs':len(values)} if values else None
out={'date':'2026-11-01','classification':'Local native evaluation, not production selection or platform acceptance','manifest':str(a.manifest),'input':{k:v for k,v in m.items() if k!='runs'},'profiles':{},'failures':[]}
for profile in ['small','source','overlay']:
 out['profiles'][profile]={}
 for engine in sorted({v['engine']for v in m['runs']}):
  entries=[v for v in m['runs']if v['engine']==engine and v['profile']==profile]
  rs=[json.loads((Path(v['case'])/'result.json').read_text()) for v in entries if (Path(v['case'])/'result.json').exists()]
  if not entries:continue
  d={'runs_attempted':len(entries),'runs_with_results':len(rs),'process_failures':sum(v['exit_code']!=0 for v in entries),'correctness_failures':sum(bool(v.get('case_errors'))for v in entries),'metrics':{}}
  for k in ['nodes','edges','import_and_index_ready_ns','open_ns','close_ns','reopen_ns','baseline_rss_kib','imported_rss_kib','post_query_rss_kib','database_logical_bytes','correctness_checks']:
   d['metrics'][k]=agg([r[k]for r in rs])
  for k in ['binary_bytes','process_peak_rss_bytes']:d['metrics'][k]=agg([e[k]for e in entries if k in e])
  for k in sorted({k for r in rs for k in r['summary']}):
   for stat in ['median_ns','p95_ns','p99_ns']:
    values=[r['summary'][k][stat]for r in rs if r['summary'][k].get(stat)is not None]
    if values:d['metrics'][k+'__'+stat]=agg(values)
  d['vector_recall']={}
  for case in ['knn_project_view_scope','knn_graph_scope']:
   qs=[v for r in rs for v in r['vector_quality']if v['case']==case]
   d['vector_recall'][case]={'mean':statistics.mean(q['recall']for q in qs),'minimum':min(q['recall']for q in qs),'queries':len(qs),'underfilled_queries':sum(q['expected']!=q['returned']for q in qs)} if qs else None
  d['case_errors']=sorted(set(err for r in rs for err in r['case_errors']));out['profiles'][profile][engine]=d
  out['failures'] += [v for v in entries if v['exit_code']!=0 or v.get('case_errors')]
a.output.write_text(json.dumps(out,indent=2)+'\n');print(json.dumps({'runs':len(m['runs']),'profiles':list(out['profiles']),'failure_cases':len(out['failures'])}))
