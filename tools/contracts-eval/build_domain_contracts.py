"""Generate architecture DTO source from explicit domain_spec; never app code."""
from pathlib import Path
import json,re,subprocess
from domain_spec import TYPES,OPS,LEGACY_RESULTS,FAMILY_RULES
from versioned_result import generate as gen_versioned
ROOT=Path(__file__).resolve().parents[2]; OUT=ROOT/'tools/contracts-eval/src/domain_generated.rs'
def pascal(s): return ''.join(p[:1].upper()+p[1:] for p in re.split('[._-]',s))
def rust(t):
 if t.startswith('?'): return 'Option<'+rust(t[1:])+'>'
 return t
ATTR='#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]\n'
REC=ATTR+'#[serde(rename_all = "camelCase", deny_unknown_fields)]\n'
TAG=ATTR+'#[serde(tag = "kind", rename_all_fields = "camelCase", deny_unknown_fields)]\n'
def camel(s):return re.sub(r'_([a-z])',lambda m:m[1].upper(),s)
def literal(v):return json.dumps(v)
def fields(fs,indent='    ',since=None,one=(),schema=None):
 # `one`: members of an exactly-one choice (rendered without null); `schema`: extra keywords.
 lines=[];schema=schema or {}
 for n,t in fs.items():
  extra=''.join(f', {literal(k)} = {literal(v)}' for k,v in schema.get(n,{}).items())
  if since and n in since:lines += [indent+'#[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "gated_member")]',indent+f'#[schemars(with = "{rust(t[1:])}", extend("x-since" = {since[n]}{extra}))]']
  elif t.startswith('?'):
   reader=', deserialize_with = "gated_member"' if n in one else ''
   lines.append(indent+'#[serde(default, skip_serializing_if = "Option::is_none"'+reader+')]')
   if n in one:lines.append(indent+f'#[schemars(with = "{rust(t[1:])}")]')
  lines.append(indent+f'pub {n}: {rust(t)},')
 return '\n'.join(lines)
def enumfields(fs,since=None,one=(),schema=None):return fields(fs,'        ',since,one,schema).replace('pub ','')
def choice(fs,one,since,kind):
 """`oneOf` of an exactly-one member choice: each alternative lists the full required set (in
 field order) with that member; one introduced later carries its `x-since`."""
 alts=[]
 for m in one:
  req=(['kind'] if kind else [])+[camel(n) for n,t in fs.items() if not t.startswith('?') or n==m]
  alts.append({'required':req,**({'x-since':since[m]} if m in since else {})})
 return f'#[schemars(extend("oneOf" = {literal(alts)}))]'
def record_attrs(d):
 out=[]
 if d.get('schemaName'):out.append(f'#[schemars(rename = "{d["schemaName"]}")]')
 if d.get('earlierName'):out.append(f'#[schemars(extend("x-earlier" = {literal({"name":d["earlierName"],"before":d.get("earlierBefore",min(d["since"].values()))})}))]')
 return ''.join(a+'\n' for a in out)
def tokens(t):return set(re.findall(r'[A-Za-z_][A-Za-z0-9_]*',t))
# Members introduced in a later operation schema revision ("since") and the types that carry one.
RC={n for n,d in TYPES.items() if d.get('since') or d.get('variantSince') or d['kind']=='versioned'}
while True:
 grown=set(RC)
 for n,d in TYPES.items():
  members=d['fields'].values() if d['kind']=='record' else [t for fs in d['variants'].values() for t in fs.values()] if d['kind']=='union' else []
  if any(tokens(t)&RC for t in members):grown.add(n)
 if grown==RC:break
 RC=grown
def retain(fs,since,call,assign=None):
 assign=call if assign is None else assign;out=[]
 for n,t in fs.items():
  if n in since:out.append(f'if revision<{since[n]} {{{assign}{n}=None;}}'+(f' else {{{call}{n}.retain_revision(revision);}}' if tokens(t)&RC else ''))
  elif tokens(t)&RC:out.append(f'{call}{n}.retain_revision(revision);')
 return out
def checkfields(fs,base='self.',indent='        '):
 return '\n'.join(indent+f'{base}{n}.check()?;' for n in fs)
lines=['//! Generated architecture DTOs. Edit domain_spec.py and regenerate; no services.',
 'use super::domain_support::*;',
 'use crate::{Id,Revision,Counter,ProjectId,RootId,ActorId,AssignmentId,OperationId,QueryId,RequestKey,CommandId,EventId,StoreId,ResourceId,CorrelationId,Protocol,ReadStamp,OperationState,RootControl,Activation,Blockers,EligibilityTarget,Purpose,Event,CommandReply};',
 'use crate::provider::{Digest,ArtifactRef,SchemaRef,TranscriptPage};',
 'use crate::data_delivery::{DataScope,ViewSelection,Coverage,GitOid,GraphPage,RetrievalPage,DeliveryBarrier,DeliverySnapshot,ArtifactTags};',
 'use serde::{Serialize,Deserialize}; use schemars::JsonSchema;', '']
for name,d in TYPES.items():
 if d['kind']=='record':
  lines += [REC+record_attrs(d)+f'pub struct {name} {{',fields(d['fields'],since=d.get('since')),'}',f'impl Check for {name} {{ fn check(&self)->Result<(),String>{{',checkfields(d['fields']),f'    super::domain_rules::record("{name}", &serde_json::to_value(self).map_err(|e|e.to_string())?)?;', '    Ok(()) } }','']
  if name in RC:lines += [f'impl Revisioned for {name} {{fn retain_revision(&mut self,revision:u32){{',*retain(d['fields'],d.get('since',{}),'self.'),'}}','']
 elif d['kind']=='enum':
  lines += [ATTR+f'pub enum {name} {{',*['    '+pascal(v)+',' if v[0].isupper() else '    #[serde(rename = "'+v+'")] '+pascal(v)+',' for v in d['values']],'}',f'impl Check for {name} {{fn check(&self)->Result<(),String>{{Ok(())}}}}','']
 elif d['kind']=='versioned':
  lines += [gen_versioned(name,d,TYPES),'']
 else:
  lines += [TAG+f'pub enum {name} {{']
  vs=d.get('variantSince',{})
  for v,fs in d['variants'].items(): lines += ['    '+v+' {',enumfields(fs,vs.get(v)),'    },']
  lines += ['}',f'impl Check for {name} {{fn check(&self)->Result<(),String>{{match self {{']
  for v,fs in d['variants'].items():
   lines += [f' Self::{v} {{'+','.join(fs)+'} => {',checkfields(fs,'','    '),' },']
  lines += ['} Ok(()) }}','']
  if name in RC:
   gated=lambda v,fs:{n:t for n,t in fs.items() if tokens(t)&RC or n in vs.get(v,{})}
   arms=[f' Self::{v} {{'+','.join(gated(v,fs))+',..} => {'+''.join(retain(gated(v,fs),vs.get(v,{}),'','*'))+'},' for v,fs in d['variants'].items() if gated(v,fs)]
   lines += [f'impl Revisioned for {name} {{fn retain_revision(&mut self,revision:u32){{match self {{',*arms,*([' _ => {}'] if len(arms)<len(d['variants']) else []),'}}}','']

def included(d,mode): return d['mode'] in [mode,'Hybrid']
def opfields(d,mode): return d[mode.lower()+'Fields'] if d['mode']=='Hybrid' else d['fields']
def opsince(d,mode): return {n:r for n,r in d.get('since',{}).items() if n in opfields(d,mode)}
for mode in ['Query','Command']:
 lines += [TAG+f'pub enum Domain{mode} {{']
 for n,d in OPS.items():
  if included(d,mode):
   one=d.get('exactlyOne',[]);fs=opfields(d,mode)
   lines += [*( [f'    #[schemars(extend("x-since" = {d["introducedRevision"]}))]'] if d.get('introducedRevision',1)>1 else []), '    #[serde(rename = "'+n+'")] '+(choice(fs,one,opsince(d,mode),True) if one else '')+pascal(n)+' {',enumfields(fs,opsince(d,mode),one,d.get('fieldSchema')),'    },']
 lines += ['}',f'impl Domain{mode} {{pub fn operation(&self)->&\'static str {{match self {{']
 for n,d in OPS.items():
  if included(d,mode):lines.append(f' Self::{pascal(n)} {{..}} => "{n}",')
 lines += ['}}}', f'impl Check for Domain{mode} {{fn check(&self)->Result<(),String>{{match self {{']
 for n,d in OPS.items():
  if included(d,mode):
   fs=opfields(d,mode)
   lines += [f' Self::{pascal(n)} {{'+','.join(fs)+'} => {',checkfields(fs,'','    '),' },']
 lines += [f'}} super::domain_rules::input(self.operation(),"{mode}",&serde_json::to_value(self).map_err(|e|e.to_string())?) }} }}','']
 arms=[]
 for n,d in OPS.items():
  if included(d,mode):
   fs=opfields(d,mode);since=opsince(d,mode);gated={k:t for k,t in fs.items() if k in since or tokens(t)&RC}
   if gated:arms.append(f' Self::{pascal(n)} {{'+','.join(gated)+',..} => {'+''.join(retain(gated,since,'','*'))+'},')
 introduced=[(n,d['introducedRevision']) for n,d in OPS.items() if included(d,mode) and d.get('introducedRevision',1)>1]
 lines += [f'impl Revisioned for Domain{mode} {{fn retain_revision(&mut self,{"revision" if arms else "_"}:u32){{',*([ 'match self {',*arms,' _ => {}','}'] if arms else []),'}', f'fn check_revision(&self,{"revision" if introduced else "_revision"}:u32)->Result<(),String>{{match self{{',*[f'Self::{pascal(n)} {{..}} if revision<{r}=>Err("operation predates its introduction".into()),' for n,r in introduced],'_=>Ok(())}}','}','']
lines += [ATTR+'#[serde(tag="mode", rename_all_fields="camelCase", deny_unknown_fields)]',
 'pub enum DomainRequest { Query {protocol:Protocol, query_id:QueryId, body:DomainQuery}, Command {protocol:Protocol, request_key:RequestKey, body:DomainCommand} }',
 'impl DomainRequest {pub fn validate(&self)->Result<(),String>{match self{Self::Query{body,..}=>body.check(),Self::Command{body,..}=>body.check()}} pub fn operation(&self)->&\'static str{match self{Self::Query{body,..}=>body.operation(),Self::Command{body,..}=>body.operation()}}}',
 'impl Revisioned for DomainRequest {fn retain_revision(&mut self,revision:u32){match self{Self::Query{body,..}=>body.retain_revision(revision),Self::Command{body,..}=>body.retain_revision(revision)}} fn check_revision(&self,revision:u32)->Result<(),String>{match self{Self::Query{body,..}=>body.check_revision(revision),Self::Command{body,..}=>body.check_revision(revision)}}}', '']
results={n:d['result'] for n,d in OPS.items()}|LEGACY_RESULTS
lines += [TAG+'pub enum OperationResult {']
for n,t in results.items(): lines += [*([f'    #[schemars(extend("x-since" = {OPS[n]["introducedRevision"]}))]'] if OPS.get(n,{}).get('introducedRevision',1)>1 else []),f'    #[serde(rename="{n}")] {pascal(n)} {{value:{rust(t)}}},']
lines += ['}', 'impl OperationResult {pub fn operation(&self)->&\'static str{match self{']
for n in results: lines += [f' Self::{pascal(n)} {{..}}=>"{n}",']
lines += ['}}}', 'impl Check for OperationResult {fn check(&self)->Result<(),String>{match self{']
for n in results:lines += [f' Self::{pascal(n)} {{value}}=>value.check()?,']
lines += ['} super::domain_rules::result(self.operation(), &serde_json::to_value(self).map_err(|e|e.to_string())?) }}', '']
# HistoryPage is a support type with its own revision-2 member (the tail checkpoint).
projected=[n for n,t in results.items() if tokens(t)&(RC|{'HistoryPage'})]
lines += ['impl Revisioned for OperationResult {fn retain_revision(&mut self,revision:u32){match self{',*[f' Self::{pascal(n)} {{value}}=>value.retain_revision(revision),' for n in projected],*([' _ => {}'] if len(projected)<len(results) else []),'}}', 'fn check_revision(&self,revision:u32)->Result<(),String>{match self{',*[f'Self::{pascal(n)} {{..}} if revision<{d["introducedRevision"]}=>Err("operation result predates its introduction".into()),' for n,d in OPS.items() if d.get('introducedRevision',1)>1],*[f'Self::{pascal(n)} {{value}}=>value.check_revision(revision),' for n,t in results.items() if TYPES.get(t,{}).get('kind')=='versioned'],'_=>Ok(())}}','}','']
# Generic receipts preserve admitted/rejected/unknown command distinctions. Outcome
# variants cannot mint human authority or make an external uncertainty successful.
lines += [TAG+'''pub enum DomainDecision {
 Applied { result:OperationResult },
 Accepted { operation_id:OperationId },
 NoChange { result:OperationResult },
 Refused { fault:DomainFault },
}''',
 'pub type DomainCommandReply = CommandReply<DomainDecision>;',
 TAG+'''pub enum DomainQueryReply {
 Result { protocol:Protocol, query_id:QueryId, stamp:ReadStamp, result:OperationResult },
 Rejected { protocol:Protocol, query_id:QueryId, fault:DomainFault },
}''',
 TAG+'''pub enum OperationCompletion {
 Succeeded { protocol:Protocol, operation_id:OperationId, revision:Revision, result:OperationResult },
 Failed { protocol:Protocol, operation_id:OperationId, revision:Revision, fault:DomainFault },
 OutcomeUnknown { protocol:Protocol, operation_id:OperationId, revision:Revision, correlation_id:CorrelationId },
}''',
 '''impl DomainQueryReply {pub fn validate(&self)->Result<(),String>{match self{Self::Result{result,..}=>result.check(),Self::Rejected{fault,..}=>fault.check()}}}
impl OperationCompletion {pub fn validate(&self)->Result<(),String>{match self{Self::Succeeded{result,..}=>result.check(),Self::Failed{fault,..}=>fault.check(),Self::OutcomeUnknown{..}=>Ok(())}}}
pub fn validate_command_reply(v:&DomainCommandReply)->Result<(),String>{crate::validate_domain_reply(v).map_err(str::to_owned)?; if let CommandReply::Recorded{receipt,..}=v{match &receipt.decision{DomainDecision::Applied{result}|DomainDecision::NoChange{result}=>result.check()?,DomainDecision::Refused{fault}=>{fault.check()?; if matches!(fault.code,super::domain_support::DomainErrorCode::OutcomeUnknown)&&receipt.operation_id.is_none(){return Err("unknown effect needs durable operation identity".into())}},_=>{}} if let DomainDecision::Accepted{operation_id}=&receipt.decision{if receipt.operation_id.as_ref()!=Some(operation_id){return Err("accepted operation does not match receipt".into())}}} Ok(())}''']
OUT.write_text('\n'.join(lines)+'\n')
subprocess.run(['rustfmt','--edition','2024',str(OUT)],check=True)
print(json.dumps({'types':len(TYPES),'remainingInputs':len(OPS),'resultVariants':len(results)}))
