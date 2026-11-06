use super::{*,commands::{Job,validate,invalid}};
use crate::{ports::{Preparation,IdKind},protocol::{OperationId,EventId,provider::ArtifactRef,results::OperationResult,records::ConventionRecordResult},services::{artifacts::host::HostStream,conventions::{model,parent_path},sessions::Failure}};
use serde_json::json;
#[derive(Debug)]
pub enum ImportOutcome {Completed(OperationResult),NotApplied(String),Unknown(String)}
impl ImportService {
 pub fn execute(&self,operation:&OperationRecord)->ImportOutcome {
  match self.run(operation) {Ok(result)=>ImportOutcome::Completed(result),Err(StorageError::Cancelled)=>ImportOutcome::NotApplied("Import cancelled or host closing".into()),Err(error)=>{
   // Staged rows remain private. A refusal never repeats a partially published document.
   let op=operation.id.clone();let reason=error.to_string();let saved=reason.clone();
   let _=self.write(move|tx|{if let Some(d)=tx.conventions().convention_import(&op)? {if d.phase!="Published" && !matches!(d.phase.as_str(),"Failed"|"Cancelled") {tx.conventions().set_convention_import_phase(&op,d.revision,"Failed",None,Some(&saved),None)?;}}Ok(())});
   ImportOutcome::NotApplied(reason)
  }}
 }
 pub fn observe(&self,operation:&OperationRecord)->ImportOutcome {self.execute(operation)}
 fn run(&self,operation:&OperationRecord)->Result<OperationResult,StorageError> {
  let job:Job=serde_json::from_value(operation.payload.clone()).map_err(|_|invalid("invalid import job"))?;
  let op=operation.id.clone();let captured=operation.clone();let j=job.clone();let artifacts=self.artifacts.clone();
  let (ctx,draft)=self.write(move|tx| {let(c,d)=validate(tx,&captured,&j)?;let r:ArtifactRef=serde_json::from_value(d.source_ref.clone()).map_err(|_|invalid("import source ref"))?;
    let version=tx.artifacts().version(&r.version_id)?.ok_or(StorageError::ScopeDenied)?;
    if version.blob_generation!=j.source_generation {return Err(StorageError::ScopeDenied)}
    artifacts.validate_import_source(tx,&c,&r,&version).map_err(failure)?;Ok((c,d))})?;
  if draft.phase=="Published" {return self.result(&draft,&ctx)}
  if matches!(draft.phase.as_str(),"Cancelled"|"Failed"){return Err(StorageError::Cancelled)}
  let reference:ArtifactRef=serde_json::from_value(draft.source_ref.clone()).map_err(|_|invalid("import source ref"))?;
  let env=Preparation::new(self.store.as_ref(),None,None);
  let lease=env.read(|read| self.artifacts.import_source(read,&ctx,&reference).map_err(failure))?;
  let closed=self.closed.clone();let stopped:Arc<dyn Fn()->bool+Send+Sync>=Arc::new(move||closed.is_closed());
  let mut reader=lease.reader(stopped.clone());
  let mut checkpoint:parser::Checkpoint=serde_json::from_value(draft.checkpoint.clone()).map_err(|_|invalid("import checkpoint"))?;
  loop {
   if stopped(){return Err(StorageError::Cancelled)}
   let step=parser::Parser::new(reader.as_mut(),&checkpoint,i64::MAX as u64,&*stopped).and_then(|mut p|p.next(checkpoint.clone())).map_err(|e|invalid(&e.to_string()))?;
   match step {
    parser::Step::Entry{entry,checkpoint:next}=> {
     let source=json!({"kind":"ConventionImportSource","operationId":op,"conventionId":draft.convention_id,"ordinal":entry.ordinal,"entryKind":entry.kind,"resourceKey":entry.key});
     let mut payload=parser::PayloadReader::new(reader.as_mut(),entry.payload.clone(),&*stopped).map_err(|e|invalid(&e.to_string()))?;
     let staged=self.artifacts.stage_host_reader(self.store.as_ref(),HostStream{reader:&mut payload,digest:&entry.payload.sha256,byte_count:entry.payload.byte_length,source},&mut ||stopped())?.map_err(|_|invalid("import entry staging failed"))?;
     let(captured,j,artifacts,reference,next_value,file,entry)=(operation.clone(),job.clone(),self.artifacts.clone(),reference.clone(),serde_json::to_value(&next).map_err(|_|invalid("checkpoint"))?,staged.clone(),entry);
     let outcome=self.write(move|tx| {let(c,d)=validate(tx,&captured,&j)?;let version=tx.artifacts().version(&reference.version_id)?.ok_or(StorageError::ScopeDenied)?;
      if version.blob_generation!=j.source_generation{return Err(StorageError::ScopeDenied)}
      artifacts.validate_import_source(tx,&c,&reference,&version).map_err(failure)?;
      artifacts.bind_import_capture(tx,&d,&entry,&file)?;
      let d=tx.conventions().convention_import(&d.operation_id)?.ok_or_else(||invalid("import disappeared"))?;
      tx.conventions().set_convention_import_phase(&d.operation_id,d.revision,"Parsing",None,None,Some(&next_value))?;Ok(())});
     self.artifacts.settle_host_bytes(self.store.as_ref(),&staged,outcome.is_ok());outcome?;checkpoint=next;
    }
    parser::Step::Complete{metadata,resources}=>return self.publish(operation,&job,metadata,resources),
   }
  }
 }
 fn publish(&self,operation:&OperationRecord,job:&Job,metadata:parser::Metadata,resources:i64)->Result<OperationResult,StorageError>{
  let(op,j,artifacts,index)=(operation.clone(),job.clone(),self.artifacts.clone(),self.index.clone().ok_or_else(||invalid("import index owner unavailable"))?);
  self.write(move|tx| {let(ctx,d)=validate(tx,&op,&j)?;
   let reference:ArtifactRef=serde_json::from_value(d.source_ref.clone()).map_err(|_|invalid("source ref"))?;let v=tx.artifacts().version(&reference.version_id)?.ok_or(StorageError::ScopeDenied)?;
   if v.blob_generation!=j.source_generation{return Err(StorageError::ScopeDenied)} artifacts.validate_import_source(tx,&ctx,&reference,&v).map_err(failure)?;
   let parent=parent_path(tx,&d.scope,metadata.parent_path.as_deref().unwrap_or(&[])).map_err(failure)?;
   let now=tx.now_ms();let proto=StoredConvention{id:d.convention_id.clone(),scope:d.scope.clone(),revision:1,slug:metadata.slug.ok_or_else(||invalid("missing slug"))?,title:metadata.title.ok_or_else(||invalid("missing title"))?,parent_id:parent.as_ref().map(|p|p.id.clone()),sort_order:i64::from(metadata.sort_order.ok_or_else(||invalid("missing sort"))?),lifecycle:metadata.lifecycle.ok_or_else(||invalid("missing lifecycle"))?,content_ref:None,raw_content_ref:None,applicability:metadata.applicability.unwrap_or_default(),provenance:json!({"kind":"Imported","operationId":d.operation_id,"source":reference}),author_id:d.author_id.clone(),created_at_ms:now,updated_at_ms:now,legacy_approved:false};
   tx.conventions().set_convention_import_metadata(&d.operation_id,&proto,parent.as_ref().map(|p|p.revision))?;
   let d=tx.conventions().convention_import(&d.operation_id)?.ok_or_else(||invalid("import missing"))?;
   let d=tx.conventions().set_convention_import_phase(&d.operation_id,d.revision,"Ready",Some(resources),None,None)?;
   artifacts.check_import_quota(tx,&d)?;
   let row=tx.conventions().publish_convention_import(&d.operation_id,d.revision,d.expected_scope_revision,resources)?;
   index.record_changed(tx,&ctx,&row)?;
   let id=EventId(tx.new_id(IdKind::Event));tx.history().append(NewEvent{id,actor:ctx.principal().clone(),project_id:d.scope.project().cloned(),root_id:ctx.root_id().cloned(),operation_id:Some(d.operation_id),aggregate_kind:"convention".into(),aggregate_id:row.id.as_str().into(),aggregate_revision:1,kind:"ConventionImported".into(),payload:json!({"resourceCount":resources})})?;
   Ok(OperationResult::ConventionsImport{value:ConventionRecordResult::Current(model::record(&row,index.status_in(tx,&row)?)?)})
  })
 }
 fn result(&self,draft:&StoredConventionImport,ctx:&crate::context::InvocationContext)->Result<OperationResult,StorageError>{
  let id=draft.convention_id.clone();let scope=draft.scope.clone();let index=self.index.as_ref().ok_or_else(||invalid("index unavailable"))?;
  Preparation::new(self.store.as_ref(),None,None).read(|read| {super::super::access::scope(read,ctx,Some(&model::scope(&scope)),false).map_err(failure)?;
   let row=read.conventions().convention(&id,None,&[scope])?.ok_or(StorageError::ScopeDenied)?;
   Ok(OperationResult::ConventionsImport{value:ConventionRecordResult::Current(model::record(&row,index.status(read,&row)?)?)})})
 }
 pub fn recover_pending(&self)->Result<usize,StorageError>{
  let mut after=None;let mut count=0;
  loop {if self.is_stopped(){break}let rows=Preparation::new(self.store.as_ref(),None,None).read(|r|r.conventions().convention_imports_pending(after.as_ref(),32))?;if rows.is_empty(){break} after=rows.last().map(|r|r.operation_id.clone());
   for row in rows {if matches!(row.phase.as_str(),"Cancelled"|"Failed") {let id=row.operation_id.clone();self.write(move|tx|tx.conventions().discard_convention_import_artifacts(&id,32))?;count+=1;}}
  }Ok(count)
 }
}
fn failure(error:Failure)->StorageError {match error{Failure::Storage(error)=>error,_=>StorageError::ScopeDenied}}
