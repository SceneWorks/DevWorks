"""Local evaluation orchestration only. The measured workload is native Rust.

Each case owns a fresh directory and process group. Keep every failed attempt;
never remove or reuse a database to hide a failure. No inference is invoked.
"""
from pathlib import Path
import argparse, datetime, hashlib, json, os, platform, random, re, signal, subprocess, time

def main():
    p=argparse.ArgumentParser()
    p.add_argument('--fixture',type=Path,required=True)
    p.add_argument('--work-dir',type=Path,required=True)
    p.add_argument('--phase',choices=['pilot','measured'],default='pilot')
    p.add_argument('--engines',default='overgraph,grafeo,helix,sqlite')
    p.add_argument('--repeats',type=int,default=3)
    p.add_argument('--timeout',type=int,default=900)
    a=p.parse_args();root=Path(__file__).resolve().parent
    tag=datetime.datetime.now(datetime.timezone.utc).strftime('%Y%m%dT%H%M%S%fZ')
    run=a.work_dir/tag;run.mkdir(parents=True,exist_ok=False)
    profiles=[('pilot',1000,False)] if a.phase=='pilot' else [('small',1000,False),('source',2**63-1,False),('overlay',2**63-1,True)]
    engines=a.engines.split(',');assert set(engines)<=set(['overgraph','grafeo','helix','sqlite','graphqlite'])
    fixture_digest=hashlib.sha256(a.fixture.read_bytes()).hexdigest()
    manifest={'phase':a.phase,'created_utc':tag,'fixture':str(a.fixture),'fixture_sha256':fixture_digest,'workspace_lock_sha256':hashlib.sha256((root/'Cargo.lock').read_bytes()).hexdigest(),'hardware':{'platform':platform.platform(),'machine':platform.machine(),'logical_cpus':os.cpu_count()},'inference':False,'order_seed':67321,'rayon_threads':4,'compiler':subprocess.check_output(['rustup','run','nightly-2026-10-06','rustc','-Vv'],text=True).strip(),'cache_policy':'Fresh process/database each case; OS filesystem cache not flushed; queries include first-access and warm samples','release_profile':{'opt_level':3,'lto':False,'codegen_units':16},'runs':[]}
    rng=random.Random(67321)
    for iteration in range(1 if a.phase=='pilot' else a.repeats):
        order=list(engines);rng.shuffle(order)
        for profile,limit,overlay in profiles:
            for engine in order:
                case=run/f'{profile}-{iteration+1}-{engine}';case.mkdir()
                config={'fixture':str(a.fixture.resolve()),'database':str((case/'database').resolve()),'output':str((case/'result.json').resolve()),'limit':limit,'overlay':overlay,'samples':120 if a.phase=='pilot' else 2000,'vector_samples':8 if a.phase=='pilot' else 80}
                (case/'config.json').write_text(json.dumps(config,indent=2))
                executable=root/'target/release'/f'eval-{engine}'
                native_command=[str(executable),'--workload',str(case/'config.json')]
                command=['/usr/bin/time','-l',*native_command] if platform.system()=='Darwin' else native_command
                entry={'engine':engine,'profile':profile,'iteration':iteration+1,'command':command,'case':str(case),'binary_bytes':executable.stat().st_size,'binary_sha256':hashlib.sha256(executable.read_bytes()).hexdigest()}
                print(json.dumps({'start':str(case)}),flush=True);start=time.monotonic()
                env=os.environ.copy();env['RAYON_NUM_THREADS']='4'
                if engine=='graphqlite':env['GRAPHQLITE_EXTENSION_DIR']=str((case/'extension').resolve())
                with (case/'stdout.log').open('w')as stdout,(case/'stderr.log').open('w')as stderr:
                    child=subprocess.Popen(command,stdout=stdout,stderr=stderr,env=env,start_new_session=True)
                    try:entry['exit_code']=child.wait(timeout=a.timeout)
                    except subprocess.TimeoutExpired:
                        os.killpg(child.pid,signal.SIGTERM)
                        try:child.wait(timeout=5)
                        except subprocess.TimeoutExpired:os.killpg(child.pid,signal.SIGKILL);child.wait()
                        entry['exit_code']=child.returncode;entry['timeout']=True
                entry['wall_seconds']=time.monotonic()-start
                if engine=='graphqlite':entry['extracted_extensions']=[{'name':p.name,'bytes':p.stat().st_size,'sha256':hashlib.sha256(p.read_bytes()).hexdigest()}for p in(case/'extension').glob('*')if p.is_file()]
                diagnostic=(case/'stderr.log').read_text()
                peak=re.search(r'(\d+)\s+maximum resident set size',diagnostic)
                if peak:entry['process_peak_rss_bytes']=int(peak.group(1))
                if (case/'result.json').exists():
                    r=json.loads((case/'result.json').read_text());entry.update({'nodes':r['nodes'],'edges':r['edges'],'correctness_checks':r['correctness_checks'],'case_errors':r['case_errors']})
                else:entry['error_tail']=(case/'stderr.log').read_text()[-3000:]
                manifest['runs'].append(entry);(run/'manifest.json').write_text(json.dumps(manifest,indent=2))
                print(json.dumps({'finish':str(case),'exit_code':entry['exit_code'],'errors':entry.get('case_errors',entry.get('error_tail'))}),flush=True)
    print(json.dumps({'manifest':str(run/'manifest.json')}),flush=True)

if __name__=='__main__':main()
