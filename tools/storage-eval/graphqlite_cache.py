"""Native CSR profiles over completed disposable GraphQLite workload stores."""
from pathlib import Path
import argparse,hashlib,json,os,subprocess,signal,time
from cargo_target import release_binary
p=argparse.ArgumentParser();p.add_argument('manifest',type=Path);p.add_argument('--timeout',type=int,default=240);a=p.parse_args();root=Path(__file__).resolve().parent;exe=release_binary(root,'eval-graphqlite');out=a.manifest.parent/'cache-profiles';out.mkdir(exist_ok=False);source=json.loads(a.manifest.read_text());summary={'source_manifest':str(a.manifest),'inference':False,'binary_sha256':hashlib.sha256(exe.read_bytes()).hexdigest(),'runs':[]}
for entry in source['runs']:
 if entry['engine']!='graphqlite':continue
 case=Path(entry['case']);dest=out/case.name;dest.mkdir();cfg=json.loads((case/'config.json').read_text());cfg['output']=str((dest/'result.json').resolve());config=dest/'config.json';config.write_text(json.dumps(cfg,indent=2));env=os.environ.copy();env['RAYON_NUM_THREADS']='4';env['GRAPHQLITE_EXTENSION_DIR']=str((dest/'extension').resolve());command=[str(exe),'--cache-profile',str(config)];record={'profile':entry['profile'],'iteration':entry['iteration'],'command':command,'case':str(dest)}
 if entry['exit_code']!=0 or entry.get('case_errors'):record['skipped']='Common workload did not pass; database not a valid post-workload cache fixture'
 else:
  with (dest/'stdout.log').open('w')as stdout,(dest/'stderr.log').open('w')as stderr:
   child=subprocess.Popen(command,stdout=stdout,stderr=stderr,env=env,start_new_session=True)
   try:record['exit_code']=child.wait(timeout=a.timeout)
   except subprocess.TimeoutExpired:os.killpg(child.pid,signal.SIGKILL);child.wait();record['exit_code']=child.returncode;record['timeout']=True
  if (dest/'result.json').exists():r=json.loads((dest/'result.json').read_text());record['case_errors']=r['case_errors'];record['checks']=r['correctness_checks']
  else:record['stderr_tail']=(dest/'stderr.log').read_text()[-1500:]
 summary['runs'].append(record);(out/'manifest.json').write_text(json.dumps(summary,indent=2));print(json.dumps(record),flush=True)
print(json.dumps({'manifest':str(out/'manifest.json')}))
