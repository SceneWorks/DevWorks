use super::*;
use crate::{context::{InvocationContext,PrincipalKind},gateway::AdmittedCommand,operations::conventions as ops,
ports::{AuthorityFence,Decided,IdKind,Preparation},protocol::{ActorId,DomainErrorCode,EventId,Id,OperationId,RootId,data::DataScope,provider::ArtifactRef,results::DomainDecision},
services::{approvals,conventions::{access,model},sessions::{Failure,Refusal}}};
use serde::{Serialize,Deserialize};
use serde_json::{Value,json};
pub const EFFECT_CONSUMER:&str="convention_importing";
pub(super) const KIND:&str="ConventionImportV1";
pub struct PreparedImport {pub(super) version:StoredVersion,pub(super) scope:KnowledgeScope,pub(super) revision:i64,pub(super) _lease:Arc<crate::services::artifacts::convention_import::ImportSourceLease>}
#[derive(Clone,Serialize,Deserialize)]
#[serde(rename_all="camelCase",deny_unknown_fields)]
pub(super) struct Job {pub kind:String,pub actor:ActorId,pub actor_kind:String,pub source_context:Value,pub fences:Vec<AuthorityFence>,pub delivery:Id,pub source_generation:String,
pub controls:Vec<(RootId,i64)>,pub assignment_generation:Option<i64>}
// StoredVersion is private storage metadata, serialized explicitly in the operation payload.
impl ImportService {
 pub fn prepare_import(&self,ctx:&InvocationContext,input:&ops::ImportInput,env:&Preparation<'_>)->Result<PreparedImport,Failure> {
  if self.is_stopped(){return Err(StorageError::Cancelled.into())}
  if serde_json::to_value(&input.schema).ok()!=serde_json::to_value(model::manifest_schema()).ok(){return Err(Refusal::new(DomainErrorCode::SchemaMismatch,"convention_manifest_schema",None).into())}
  env.read(|read|Ok((|| {let scope=access::scope(read,ctx,Some(&input.scope),true)?;let lease=self.artifacts.import_source(read,ctx,&input.source)?;
   Ok::<_,Failure>(PreparedImport{version:lease.version.clone(),revision:read.conventions().convention_scope_revision(&scope.exact)?,scope:scope.exact,_lease:Arc::new(lease)})})()))?.map_err(Into::into)
 }
 pub fn decide_import(&self,ctx:&InvocationContext,command:&AdmittedCommand,tx:&mut dyn WriteTransaction,input:&ops::ImportInput)->Result<Decided,Failure>{
  if self.is_stopped(){return Err(StorageError::Cancelled.into())}
  let prepared=command.prepared::<PreparedImport>().ok_or_else(||StorageError::InvalidRecord("missing prepared import".into()))?;
  let scoped=access::scope_in(tx,ctx,Some(&input.scope),true)?;
  if scoped.exact!=prepared.scope||tx.conventions().convention_scope_revision(&scoped.exact)?!=prepared.revision {return Err(Refusal::new(DomainErrorCode::SourceChanged,"convention_scope",None).into())}
  self.artifacts.validate_import_source(tx,ctx,&input.source,&prepared.version)?;
  let op=OperationId(tx.new_id(IdKind::Operation));let delivery=tx.new_id(IdKind::QueueEntry);let event=EventId(tx.new_id(IdKind::Event));
  let mut controls=Vec::new();if let Some(root)=ctx.root_id(){controls.push((root.clone(),tx.runs().root_control(root)?.unwrap_or_else(||StoredRootControl::initial(root.clone())).stop_epoch));
   if let Some(work)=tx.work().work(&root.0)? {if let Some(parent)=work.parent_feature_id {let parent=RootId(parent);if parent!=*root {controls.push((parent.clone(),tx.runs().root_control(&parent)?.unwrap_or_else(||StoredRootControl::initial(parent.clone())).stop_epoch));}}}}
  let assignment_generation=ctx.assignment_id().map(|id|tx.runs().assignment(id)?.map(|a|a.generation).ok_or(StorageError::ScopeDenied)).transpose()?;
  let job=Job{kind:KIND.into(),actor:ctx.principal().clone(),actor_kind:format!("{:?}",ctx.principal_kind()),source_context:approvals::save_source_context(ctx),fences:command.authorization().fences().to_vec(),delivery:delivery.clone(),source_generation:prepared.version.blob_generation.clone(),controls,assignment_generation};
  tx.operations().record_intent(NewOperation{id:op.clone(),root_id:ctx.root_id().cloned(),assignment_id:ctx.assignment_id().cloned(),capability:"conventions.import".into(),schema_version:1,args_digest:command.fingerprint().as_str().into(),payload:serde_json::to_value(job).map_err(|e|StorageError::InvalidRecord(e.to_string()))?})?;
  let now=tx.now_ms();let draft=StoredConventionImport{operation_id:op.clone(),convention_id:tx.new_id(IdKind::Convention),scope:scoped.exact,author_id:ctx.principal().clone(),source_ref:serde_json::to_value(&input.source).map_err(|e|StorageError::InvalidRecord(e.to_string()))?,expected_scope_revision:prepared.revision,revision:1,phase:"Queued".into(),checkpoint:serde_json::to_value(parser::Checkpoint::default()).expect("checkpoint"),processed_resources:0,total_resources:None,error:None,created_at_ms:now,updated_at_ms:now};
  tx.conventions().begin_convention_import(&draft)?;
  tx.history().append(NewEvent{id:event.clone(),actor:ctx.principal().clone(),project_id:draft.scope.project().cloned(),root_id:ctx.root_id().cloned(),operation_id:Some(op.clone()),aggregate_kind:"convention_import".into(),aggregate_id:op.0.as_str().into(),aggregate_revision:1,kind:"ConventionImportRequested".into(),payload:json!({"conventionId":draft.convention_id,"source":input.source})})?;
  tx.queue().enqueue(NewQueueEntry{id:delivery,consumer:EFFECT_CONSUMER.into(),event_id:event,root_id:ctx.root_id().cloned(),operation_id:Some(op.clone()),partition:None,priority:0,ready_at_ms:now,payload:json!({"kind":KIND})})?;
  Ok(Decided{decision:DomainDecision::Accepted{operation_id:op.clone()},operation_id:Some(op)})
 }
}
pub(super) fn invalid(reason:&str)->StorageError{StorageError::InvalidRecord(reason.into())}
pub(super) fn validate(tx:&mut dyn WriteTransaction,op:&OperationRecord,job:&Job)->Result<(InvocationContext,StoredConventionImport),StorageError>{
 if op.capability!="conventions.import"||job.kind!=KIND{return Err(StorageError::ScopeDenied)}
 let ctx=approvals::restore_source_context(tx.store(),&job.actor,principal_kind(&job.actor_kind)?,&job.source_context)?;
 if tx.actors().actor_kind(&job.actor)?!=Some(principal_kind(&job.actor_kind)?)||!crate::authority::fences_hold(tx,&job.fences)? {return Err(StorageError::ScopeDenied)}
 let draft=tx.conventions().convention_import(&op.id)?.ok_or_else(||invalid("missing import"))?;
 access::scope_in(tx,&ctx,Some(&model::scope(&draft.scope)),true).map_err(|_|StorageError::ScopeDenied)?;
 if tx.queue().entry(&job.delivery)?.is_none_or(|q|q.cancel_requested){return Err(StorageError::Cancelled)}
 for(root,epoch)in &job.controls {let control=tx.runs().root_control(root)?.unwrap_or_else(||StoredRootControl::initial(root.clone()));
  if control.stop_epoch!=*epoch||control.desired!=DesiredExecution::Enabled||!tx.runs().blockers(&BlockerFilter::RootLevel(root.clone()),None,1)?.is_empty(){return Err(StorageError::ScopeDenied)}}
 if let Some(id)=ctx.assignment_id(){let a=tx.runs().assignment(id)?.ok_or(StorageError::ScopeDenied)?;if Some(a.generation)!=job.assignment_generation||a.root_id!=*ctx.root_id().ok_or(StorageError::ScopeDenied)?||matches!(a.state,AssignmentState::Superseded|AssignmentState::Failed|AssignmentState::Completed)||!tx.runs().blockers(&BlockerFilter::Assignment(id.clone()),None,1)?.is_empty(){return Err(StorageError::ScopeDenied)}}
 Ok((ctx,draft))
}

pub(super) fn principal_kind(value:&str)->Result<PrincipalKind,StorageError>{Ok(match value {"Human"=>PrincipalKind::Human,"Host"=>PrincipalKind::Host,"Agent"=>PrincipalKind::Agent,"Package"=>PrincipalKind::Package,"McpClient"=>PrincipalKind::McpClient,"Schedule"=>PrincipalKind::Schedule,_=>return Err(StorageError::ScopeDenied)})}
