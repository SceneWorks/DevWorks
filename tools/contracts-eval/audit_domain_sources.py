"""Verify frozen inventory bytes and complete operation contract ownership, not behavioral parity."""
from pathlib import Path
import argparse,datetime,hashlib,json,subprocess
ROOT=Path(__file__).resolve().parents[2];D=ROOT/'docs/architecture'
p=argparse.ArgumentParser();p.add_argument('--repository',type=Path,required=True);p.add_argument('--output',type=Path,required=True);p.add_argument('--date',default=datetime.date.today().isoformat(),help='Report date (default: the run date)');a=p.parse_args()
inventory=json.loads((D/'codegraph-capability-inventory.json').read_text());rev=inventory['sourceRevision']; verified=[]
for source in inventory['sourceFiles']:
 raw=subprocess.check_output(['git','-C',str(a.repository),'show',rev+':'+source['path']])
 digest=hashlib.sha256(raw).hexdigest();assert digest==source['sha256'],source['path']
 verified.append({'path':source['path'],'sha256':digest})
bindings={o['operation']:o for f in json.loads((D/'contracts/family-coverage.json').read_text())['families'] for o in f['operations']}
plan=json.loads((D/'implementation-plan.json').read_text());owners={o['operation']:o['primarySlice'] for o in plan['operations']}
from fnmatch import fnmatchcase
entries=[]
for entry in inventory['entries']:
 matched=sorted({op for selector in entry['targetOperations'] for op in bindings if fnmatchcase(op,selector)})
 if entry['disposition']!='excluded':
  assert all(any(fnmatchcase(op,s) for op in bindings) for s in entry['targetOperations']),entry['id']
  assert all(bindings[op]['inputSchema'] and bindings[op]['resultSchema'] and op in owners for op in matched)
 entries.append({'sourceEntryId':entry['id'],'disposition':entry['disposition'],'acceptanceCase':entry['acceptanceCase'],'source':entry['source'],'operations':matched,'implementationSlices':sorted({owners[op] for op in matched}),'reason':entry['reason']})
report={'date':a.date,'status':'Frozen source bytes and schema/ownership bindings verified; method argument/result semantics and actual native parity remain CAP acceptance work',
 'frozenRevision':rev,'currentRepositoryHead':subprocess.check_output(['git','-C',str(a.repository),'rev-parse','HEAD'],text=True).strip(),
 'sourceFilesVerified':len(verified),'sourceEntries':len(entries),'retainedEntries':sum(e['disposition']!='excluded' for e in entries),'excludedEntries':sum(e['disposition']=='excluded' for e in entries),
 'qualifiedOperations':len(bindings),'unboundOperationInputs':sum(not b['inputSchema'] for b in bindings.values()),'unboundOperationResults':sum(not b['resultSchema'] for b in bindings.values()),
 'files':verified,'entries':entries,'inputSha256':{str(f.relative_to(ROOT)):hashlib.sha256(f.read_bytes()).hexdigest() for f in [D/'codegraph-capability-inventory.json',D/'contracts/family-coverage.json',D/'implementation-plan.json',D/'devworks-capability-catalog.json']},'noInference':True,'allPassed':True}
a.output.parent.mkdir(parents=True,exist_ok=True);a.output.write_text(json.dumps(report,indent=2)+'\n')
print(json.dumps({k:report[k] for k in ['sourceFilesVerified','sourceEntries','retainedEntries','excludedEntries','qualifiedOperations','unboundOperationInputs','unboundOperationResults','allPassed']}))
