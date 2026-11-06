"""Readable field reference from the exact operation register; no separate specs."""
from pathlib import Path
import json,re
from domain_spec import TYPES,OPS
ROOT=Path(__file__).resolve().parents[2];D=ROOT/'docs/architecture'
reg=json.loads((D/'contracts/family-coverage.json').read_text())
def fields(fs):return '<br>'.join('`'+re.sub(r'_([a-z])',lambda m:m[1].upper(),n)+'`: `'+t+'`' for n,t in fs.items()) or 'No caller fields'
lines=['# Domain operation field reference','','2 November 2026 · Candidate schemas, not production service acceptance. This reference is generated from the same explicit source as the native DTOs and coverage register. Read [the normative contracts](domain-operation-contracts.html) for scope, admission, results, faults and recovery. Optional fields use `?T`; all ordinary records/actions reject unknown fields. `Items`, `NonEmpty`, `Rows`, `Page`, text, counters, money and descriptor-bound JSON are defined there.','',f'All {sum(len(family["operations"]) for family in reg["families"])} qualified operations have typed input/result candidates. Existing input selectors retain their earlier schemas. The host must apply selected operation/mode, current authority and semantic rules; schema validity alone cannot authorize or complete work.','','## Operation families','']
for family in reg['families']:
 title=family['family'].replace('_',' ').title()
 lines += [f'### {title}', '', '**Owner:** '+family['owner']+'. '+family['operations'][0]['familyRules'],'', '| Operation | Mode and input fields | Typed result value |', '| --- | --- | --- |']
 for o in family['operations']:
  d=OPS.get(o['operation'])
  if d:
   modes=o['requestModes'];parts=[]
   for mode in modes:
    fs=d[mode.lower()+'Fields'] if d['mode']=='Hybrid' else d['fields']
    parts.append('**'+mode+'**<br>'+fields(fs))
   payload='<br><br>'.join(parts)
  else:payload='**'+o['requestModes'][0]+'**<br>[Existing input schema](contracts/'+o['inputSchema']+')<br>'+str(o['selectionConstraint'])
  revs=[r['revision'] for r in o['schemaRevisions']]
  if len(revs)>1 or revs[0]!=1:payload+='<br><br>**Schema revisions** '+', '.join(map(str,revs))+' ([revision '+str(revs[-1])+' input](contracts/'+o['schemaRevisions'][-1]['inputSchema']+'))'
  lines.append('| `'+o['operation']+'` | '+payload+' | `'+o['resultValueType']+'` |')
 lines += ['']
lines += ['## Domain types','','Reference names reused from earlier shared/provider/data contracts retain their existing schema definitions; they are not redeclared here. Other types below are closed records, enumerations or `kind`-tagged unions. Generic lists/pages and scalar bounds are native checked support types. A field marked *revision N+* was introduced at schema revision N: callers that negotiated an earlier revision never receive it and may not send it.','']
for name,d in TYPES.items():
 lines += ['### '+name,'']
 if d['kind']=='record':lines += ['| Field | Type |','| --- | --- |',*['| `'+re.sub(r'_([a-z])',lambda m:m[1].upper(),n)+'` | `'+t+'`'+(f" · *revision {d['since'][n]}+*" if n in d.get('since',{}) else '')+' |' for n,t in d['fields'].items()]]
 elif d['kind']=='enum':lines += [', '.join('`'+x+'`' for x in d['values'])+'.']
 elif d['kind']=='versioned':
  projection='Available Help backed by real published artifact references projects to its legacy topic/page; unavailable or inline-only views never fabricate an artifact.' if d.get('projection','').startswith('help_') else 'Successful selection outcomes project to the legacy claim; an outcome without a claim cannot be represented by a fabricated legacy record.'
  lines += [f'Before revision {d["revision"]}: exact legacy `{d["legacy"]}`. Revision {d["revision"]} and later: `{d["current"]}`. '+projection]
 else:
  lines += ['| Kind | Fields |','| --- | --- |',*['| `'+kind+'` | '+fields(fs)+' |' for kind,fs in d['variants'].items()]]
 lines += ['']
lines += ['## Machine sources','','- [Input/result/mode/authority/effect/recovery bindings](contracts/family-coverage.json)\n- [Schema manifest](contracts/manifest.json)\n- [Native/domain schema verification](validation/domain-contracts/schema-model-results.json)\n- [Frozen source bindings](validation/domain-contracts/source-binding-results.json)\n- [Local implementation ownership](implementation-plan.json)','']
(D/'domain-operation-reference.md').write_text('\n'.join(lines))
print(json.dumps({'families':reg['familyCount'],'operations':reg['operationCount'],'domainTypes':len(TYPES)}))
