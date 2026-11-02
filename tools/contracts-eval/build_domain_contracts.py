"""Generate architecture DTO source from explicit domain_spec; never app code."""
from pathlib import Path
import json,re,subprocess
from domain_spec import TYPES,OPS,LEGACY_RESULTS,FAMILY_RULES
ROOT=Path(__file__).resolve().parents[2]; OUT=ROOT/'tools/contracts-eval/src/domain_generated.rs'
def pascal(s): return ''.join(p[:1].upper()+p[1:] for p in re.split('[._-]',s))
def rust(t):
 if t.startswith('?'): return 'Option<'+rust(t[1:])+'>'
 return t
ATTR='#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]\n'
REC=ATTR+'#[serde(rename_all = "camelCase", deny_unknown_fields)]\n'
TAG=ATTR+'#[serde(tag = "kind", rename_all_fields = "camelCase", deny_unknown_fields)]\n'
def fields(fs,indent='    '):
 lines=[]
 for n,t in fs.items():
  if t.startswith('?'):lines.append(indent+'#[serde(default, skip_serializing_if = "Option::is_none")]')
  lines.append(indent+f'pub {n}: {rust(t)},')
 return '\n'.join(lines)
def enumfields(fs):return fields(fs,'        ').replace('pub ','')
def checkfields(fs,base='self.',indent='        '):
 return '\n'.join(indent+f'{base}{n}.check()?;' for n in fs)
lines=['//! Generated architecture DTOs. Edit domain_spec.py and regenerate; no services.',
 'use super::domain_support::*;',
 'use crate::{Id,Revision,Counter,ProjectId,RootId,ActorId,AssignmentId,OperationId,QueryId,RequestKey,CommandId,EventId,StoreId,ResourceId,CorrelationId,Protocol,ReadStamp,OperationState,RootControl,Activation,Blockers,EligibilityTarget,Purpose,Event,CommandReply};',
 'use crate::provider::{Digest,ArtifactRef,SchemaRef,TranscriptPage};',
 'use crate::data_delivery::{DataScope,ViewSelection,Coverage,GitOid,GraphPage,RetrievalPage,DeliveryBarrier,DeliverySnapshot};',
 'use serde::{Serialize,Deserialize}; use schemars::JsonSchema;', '']
for name,d in TYPES.items():
 if d['kind']=='record':
  lines += [REC+f'pub struct {name} {{',fields(d['fields']),'}',f'impl Check for {name} {{ fn check(&self)->Result<(),String>{{',checkfields(d['fields']),f'    super::domain_rules::record("{name}", &serde_json::to_value(self).map_err(|e|e.to_string())?)?;', '    Ok(()) } }','']
 elif d['kind']=='enum':
  lines += [ATTR+f'pub enum {name} {{',*['    '+pascal(v)+',' if v[0].isupper() else '    #[serde(rename = "'+v+'")] '+pascal(v)+',' for v in d['values']],'}',f'impl Check for {name} {{fn check(&self)->Result<(),String>{{Ok(())}}}}','']
 else:
  lines += [TAG+f'pub enum {name} {{']
  for v,fs in d['variants'].items(): lines += ['    '+v+' {',enumfields(fs),'    },']
  lines += ['}',f'impl Check for {name} {{fn check(&self)->Result<(),String>{{match self {{']
  for v,fs in d['variants'].items():
   lines += [f' Self::{v} {{'+','.join(fs)+'} => {',checkfields(fs,'','    '),' },']
  lines += ['} Ok(()) }}','']

def included(d,mode): return d['mode'] in [mode,'Hybrid']
def opfields(d,mode): return d[mode.lower()+'Fields'] if d['mode']=='Hybrid' else d['fields']
for mode in ['Query','Command']:
 lines += [TAG+f'pub enum Domain{mode} {{']
 for n,d in OPS.items():
  if included(d,mode):lines += ['    #[serde(rename = "'+n+'")] '+pascal(n)+' {',enumfields(opfields(d,mode)),'    },']
 lines += ['}',f'impl Domain{mode} {{pub fn operation(&self)->&\'static str {{match self {{']
 for n,d in OPS.items():
  if included(d,mode):lines.append(f' Self::{pascal(n)} {{..}} => "{n}",')
 lines += ['}}}', f'impl Check for Domain{mode} {{fn check(&self)->Result<(),String>{{match self {{']
 for n,d in OPS.items():
  if included(d,mode):
   fs=opfields(d,mode)
   lines += [f' Self::{pascal(n)} {{'+','.join(fs)+'} => {',checkfields(fs,'','    '),' },']
 lines += [f'}} super::domain_rules::input(self.operation(),"{mode}",&serde_json::to_value(self).map_err(|e|e.to_string())?) }} }}','']
lines += [ATTR+'#[serde(tag="mode", rename_all_fields="camelCase", deny_unknown_fields)]',
 'pub enum DomainRequest { Query {protocol:Protocol, query_id:QueryId, body:DomainQuery}, Command {protocol:Protocol, request_key:RequestKey, body:DomainCommand} }',
 'impl DomainRequest {pub fn validate(&self)->Result<(),String>{match self{Self::Query{body,..}=>body.check(),Self::Command{body,..}=>body.check()}} pub fn operation(&self)->&\'static str{match self{Self::Query{body,..}=>body.operation(),Self::Command{body,..}=>body.operation()}}}', '']
results={n:d['result'] for n,d in OPS.items()}|LEGACY_RESULTS
lines += [TAG+'pub enum OperationResult {']
for n,t in results.items(): lines += [f'    #[serde(rename="{n}")] {pascal(n)} {{value:{rust(t)}}},']
lines += ['}', 'impl OperationResult {pub fn operation(&self)->&\'static str{match self{']
for n in results: lines += [f' Self::{pascal(n)} {{..}}=>"{n}",']
lines += ['}}}', 'impl Check for OperationResult {fn check(&self)->Result<(),String>{match self{']
for n in results:lines += [f' Self::{pascal(n)} {{value}}=>value.check()?,']
lines += ['} super::domain_rules::result(self.operation(), &serde_json::to_value(self).map_err(|e|e.to_string())?) }}', '']
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
