"""Owned-process fault probes. No inference; never touches user databases."""
from pathlib import Path
import argparse,datetime,hashlib,json,os,signal,subprocess,time

def main():
 p=argparse.ArgumentParser();p.add_argument('--fixture',type=Path,required=True);p.add_argument('--work-dir',type=Path,required=True);p.add_argument('--timeout',type=float,default=120);p.add_argument('--engines',default='overgraph,grafeo,helix,sqlite');a=p.parse_args()
 root=Path(__file__).resolve().parent
 out=a.work_dir/datetime.datetime.now(datetime.timezone.utc).strftime('%Y%m%dT%H%M%S%fZ');out.mkdir(parents=True)
 manifest={'inference':False,'fixture_sha256':hashlib.sha256(a.fixture.read_bytes()).hexdigest(),'cases':[]}
 for engine in a.engines.split(','):
  assert engine in ['overgraph','grafeo','helix','sqlite','graphqlite']
  for action,check in [('recovery_writer','recovery_check'),('interrupted_writer','interrupted_open')]:
   case=out/f'{engine}-{action}';case.mkdir();marker=case/'ack';output=case/'result.json';exe=root/'target/release'/f'eval-{engine}'
   config={'fixture':str(a.fixture.resolve()),'database':str((case/'database').resolve()),'output':str(output.resolve()),'limit':1000 if action=='recovery_writer' else 2**63-1,'action':action,'signal':str(marker.resolve())}
   cfg=case/'config.json';cfg.write_text(json.dumps(config,indent=2));env=os.environ.copy();env['RAYON_NUM_THREADS']='4'
   if engine=='graphqlite':env['GRAPHQLITE_EXTENSION_DIR']=str((case/'extension').resolve())
   entry={'engine':engine,'action':action,'binary_sha256':hashlib.sha256(exe.read_bytes()).hexdigest(),'case':str(case)}
   with (case/'writer.stdout').open('w') as stdout,(case/'writer.stderr').open('w') as stderr:
    child=subprocess.Popen([str(exe),'--workload',str(cfg)],stdout=stdout,stderr=stderr,env=env,start_new_session=True);deadline=time.monotonic()+a.timeout
    while not marker.exists() and child.poll() is None and time.monotonic()<deadline:time.sleep(.01)
    entry['acknowledgment_seen']=marker.exists()
    if marker.exists() and action=='interrupted_writer':time.sleep(.05)
    entry['import_finished_before_kill']=marker.with_suffix('.finished').exists()
    if child.poll() is None:os.killpg(child.pid,signal.SIGKILL)
    child.wait();entry['writer_exit']=child.returncode
   if marker.exists():
    config['action']=check;verify=case/'verify.json';verify.write_text(json.dumps(config,indent=2));t=time.monotonic()
    with (case/'verify.stdout').open('w')as stdout,(case/'verify.stderr').open('w')as stderr:
     child=subprocess.Popen([str(exe),'--workload',str(verify)],stdout=stdout,stderr=stderr,env=env,start_new_session=True)
     try:entry['verify_exit']=child.wait(timeout=a.timeout)
     except subprocess.TimeoutExpired:os.killpg(child.pid,signal.SIGKILL);child.wait();entry['verify_exit']=child.returncode;entry['timeout']=True
    entry['verify_wall_seconds']=time.monotonic()-t
    if output.exists():entry['result']=json.loads(output.read_text())
    else:entry['error_tail']=(case/'verify.stderr').read_text()[-1500:]
   else:entry['error_tail']=(case/'writer.stderr').read_text()[-1500:]
   manifest['cases'].append(entry);(out/'manifest.json').write_text(json.dumps(manifest,indent=2));print(json.dumps(entry),flush=True)
 print(json.dumps({'manifest':str(out/'manifest.json')}))
if __name__=='__main__':main()
