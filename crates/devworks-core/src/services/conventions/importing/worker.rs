use super::{
    commands::{Job, invalid, validate},
    *,
};
use crate::{
    ports::{IdKind, Preparation},
    protocol::{
        EventId, provider::ArtifactRef, records::ConventionRecordResult, results::OperationResult,
    },
    services::{
        artifacts::host::HostStream,
        conventions::{model, parent_path},
        sessions::Failure,
    },
};
use serde_json::json;
#[derive(Debug)]
pub enum ImportOutcome {
    Completed(OperationResult),
    Failed(String),
    NotApplied(String),
    Unknown(String),
}
impl ImportService {
    pub fn execute(&self, operation: &OperationRecord) -> ImportOutcome {
        match self.run(operation) {
            Ok(result) => ImportOutcome::Completed(result),
            Err(StorageError::Busy) => self.busy_outcome(operation),
            Err(StorageError::Cancelled) => {
                ImportOutcome::NotApplied("Import cancelled or host closing".into())
            }
            Err(error) => {
                let cancelled = Preparation::new(self.store.as_ref(), None, None)
                    .read(|r| {
                        Ok(r.conventions()
                            .convention_import(&operation.id)?
                            .is_some_and(|d| d.phase == "Cancelled"))
                    })
                    .unwrap_or(false);
                if cancelled {
                    return ImportOutcome::NotApplied("Import cancelled before publication".into());
                }
                if self.is_stopped() {
                    return ImportOutcome::NotApplied("Host import owner stopped".into());
                }
                if matches!(error, StorageError::CommitUnknown) {
                    return ImportOutcome::Unknown("Import commit must be observed".into());
                }
                // Staged rows remain private. A refusal never repeats a partially published document.
                let op = operation.id.clone();
                let reason = error.to_string();
                let saved = reason.clone();
                let _ = self.write(move |tx| {
                    if let Some(d) = tx.conventions().convention_import(&op)?
                        && d.phase != "Published"
                        && !matches!(d.phase.as_str(), "Failed" | "Cancelled")
                    {
                        tx.conventions().set_convention_import_phase(
                            &op,
                            d.revision,
                            "Failed",
                            None,
                            Some(&saved),
                            None,
                        )?;
                    }
                    Ok(())
                });
                ImportOutcome::Failed(reason)
            }
        }
    }
    // Recheck the shared execution guard under the SAME journal owner before classifying Busy.
    // A temporary control hold is pending observation, never a four-attempt not-applied retry.
    fn busy_outcome(&self, operation: &OperationRecord) -> ImportOutcome {
        let job: Job = match serde_json::from_value(operation.payload.clone()) {
            Ok(job) => job,
            Err(_) => return ImportOutcome::Failed("Invalid import journal".into()),
        };
        let op = operation.clone();
        match self.write(move|tx|{
            match validate(tx,&op,&job) {
                Ok(_)=>Ok(false),
                Err(StorageError::Busy)=>{
                    let current=tx.operations().operation(&op.id)?.ok_or_else(||invalid("import journal missing"))?;
                    let attempt=current.latest_attempt().ok_or_else(||invalid("import attempt missing"))?;
                    if !attempt.observations.iter().any(|v|v.get("conventionImportWaitingControls").is_some()) {
                        let number=attempt.attempt_no;
                        tx.operations().observe(&op.id,number,json!({"conventionImportWaitingControls":{"kind":"ExecutionControls","controls":job.controls,"assignmentGeneration":job.assignment_generation}}))?;
                    }
                    Ok(true)
                },
                Err(error)=>Err(error),
            }
        }) {
            Ok(true)=>ImportOutcome::Unknown("Import is waiting for its current execution controls; the original journal and private draft are retained.".into()),
            Ok(false)=>ImportOutcome::NotApplied("Private import staging raced current blob metadata; recheck required".into()),
            Err(StorageError::ScopeDenied|StorageError::InvalidRecord(_))=>ImportOutcome::Failed("Import source or captured work ownership changed.".into()),
            Err(_)=>ImportOutcome::Unknown("Import controls/storage could not be observed; the original journal and private draft are retained.".into()),
        }
    }
    pub fn observe(&self, operation: &OperationRecord) -> ImportOutcome {
        self.execute(operation)
    }
    fn run(&self, operation: &OperationRecord) -> Result<OperationResult, StorageError> {
        let job: Job = serde_json::from_value(operation.payload.clone())
            .map_err(|_| invalid("invalid import job"))?;
        let prior = Preparation::new(self.store.as_ref(), None, None)
            .read(|r| r.conventions().convention_import(&operation.id))?;
        if let Some(prior) = prior.filter(|d| d.phase == "Published") {
            let ctx = crate::services::approvals::restore_source_context(
                &self.store.info(),
                &job.actor,
                super::commands::principal_kind(&job.actor_kind)?,
                &job.source_context,
            )?;
            return self
                .result(&prior, &ctx)
                .map_err(|_| StorageError::CommitUnknown);
        }
        let op = operation.id.clone();
        let captured = operation.clone();
        let j = job.clone();
        let artifacts = self.artifacts.clone();
        let (ctx, draft) = self.write(move |tx| {
            let (c, d) = validate(tx, &captured, &j)?;
            let r: ArtifactRef = serde_json::from_value(d.source_ref.clone())
                .map_err(|_| invalid("import source ref"))?;
            let version = tx
                .artifacts()
                .version(&r.version_id)?
                .ok_or(StorageError::ScopeDenied)?;
            if version.blob_generation != j.source_generation {
                return Err(StorageError::ScopeDenied);
            }
            artifacts
                .validate_import_source(tx, &c, &r, &version)
                .map_err(failure)?;
            Ok((c, d))
        })?;
        if draft.phase == "Published" {
            return self.result(&draft, &ctx);
        }
        if draft.phase == "Failed" {
            return Err(invalid(
                "Import failed; submit a new request after correcting the source",
            ));
        }
        if draft.phase == "Cancelled" {
            return Err(StorageError::Cancelled);
        }
        let reference: ArtifactRef = serde_json::from_value(draft.source_ref.clone())
            .map_err(|_| invalid("import source ref"))?;
        let env = Preparation::new(self.store.as_ref(), None, None);
        let lease = env.read(|read| {
            self.artifacts
                .import_source(read, &ctx, &reference)
                .map_err(failure)
        })?;
        let closed = self.closed.clone();
        let store = self.store.clone();
        let delivery = job.delivery.clone();
        let stopped: Arc<dyn Fn() -> bool + Send + Sync> = Arc::new(move || {
            closed.is_closed()
                || Preparation::new(store.as_ref(), None, None)
                    .read(|r| {
                        Ok(r.queue()
                            .entry(&delivery)?
                            .is_none_or(|q| q.cancel_requested))
                    })
                    .unwrap_or(true)
        });
        let max_resource = env
            .read(|r| {
                self.artifacts
                    .storage_status(
                        r,
                        &draft
                            .scope
                            .project()
                            .cloned()
                            .map_or(ArtifactScope::Workspace, ArtifactScope::Project),
                    )
                    .map_err(|_| StorageError::ScopeDenied)
            })?
            .max_file_bytes;
        let max_resource = if max_resource > 0 {
            max_resource as u64
        } else {
            i64::MAX as u64
        };
        if lease.version.byte_count as u64 > max_resource {
            return Err(invalid(
                "portable manifest exceeds current artifact file limit",
            ));
        }
        let mut reader = lease.import_reader(stopped.clone());
        let mut checkpoint: parser::Checkpoint = serde_json::from_value(draft.checkpoint.clone())
            .map_err(|_| invalid("import checkpoint"))?;
        loop {
            if stopped() {
                return Err(StorageError::Cancelled);
            }
            let step =
                parser::Parser::new(reader.as_mut(), &checkpoint, i64::MAX as u64, &*stopped)
                    .and_then(|mut p| p.next(checkpoint.clone()))
                    .map_err(|e| invalid(&e.to_string()))?;
            match step {
                parser::Step::Entry {
                    entry,
                    checkpoint: next,
                } => {
                    if entry.payload.byte_length > max_resource {
                        return Err(invalid("import entry exceeds current file limit"));
                    }
                    let source = json!({"kind":"ConventionImportSource","operationId":op,"conventionId":draft.convention_id,"ordinal":entry.ordinal,"entryKind":entry.kind,"resourceKey":entry.key});
                    let mut payload = parser::PayloadReader::new(
                        reader.as_mut(),
                        entry.payload.clone(),
                        &*stopped,
                    )
                    .map_err(|e| invalid(&e.to_string()))?;
                    let stage_phase = self.closed.enter()?;
                    let staged = self
                        .artifacts
                        .stage_host_reader(
                            self.store.as_ref(),
                            HostStream {
                                reader: &mut payload,
                                digest: &entry.payload.sha256,
                                byte_count: entry.payload.byte_length,
                                source,
                            },
                            &mut || stopped(),
                        )?
                        .map_err(|_| invalid("import entry staging failed"))?;
                    let (captured, j, artifacts, reference, next_value, file, entry) = (
                        operation.clone(),
                        job.clone(),
                        self.artifacts.clone(),
                        reference.clone(),
                        serde_json::to_value(&next).map_err(|_| invalid("checkpoint"))?,
                        staged.clone(),
                        entry,
                    );
                    let outcome = self.write(move |tx| {
                        let (c, d) = validate(tx, &captured, &j)?;
                        let version = tx
                            .artifacts()
                            .version(&reference.version_id)?
                            .ok_or(StorageError::ScopeDenied)?;
                        if version.blob_generation != j.source_generation {
                            return Err(StorageError::ScopeDenied);
                        }
                        artifacts
                            .validate_import_source(tx, &c, &reference, &version)
                            .map_err(failure)?;
                        artifacts.bind_import_capture(tx, &d, &entry, &file)?;
                        let d = tx
                            .conventions()
                            .convention_import(&d.operation_id)?
                            .ok_or_else(|| invalid("import disappeared"))?;
                        tx.conventions().set_convention_import_phase(
                            &d.operation_id,
                            d.revision,
                            "Parsing",
                            None,
                            None,
                            Some(&next_value),
                        )?;
                        Ok(())
                    });
                    self.artifacts
                        .settle_host_bytes(self.store.as_ref(), &staged, outcome.is_ok());
                    drop(stage_phase);
                    outcome?;
                    checkpoint = next;
                }
                parser::Step::Complete {
                    metadata,
                    resources,
                } => return self.publish(operation, &job, metadata, resources),
            }
        }
    }
    fn publish(
        &self,
        operation: &OperationRecord,
        job: &Job,
        metadata: parser::Metadata,
        resources: i64,
    ) -> Result<OperationResult, StorageError> {
        let (op, j, artifacts, index) = (
            operation.clone(),
            job.clone(),
            self.artifacts.clone(),
            self.index
                .clone()
                .ok_or_else(|| invalid("import index owner unavailable"))?,
        );
        self.write(move |tx| {
            let (ctx, d) = validate(tx, &op, &j)?;
            let reference: ArtifactRef =
                serde_json::from_value(d.source_ref.clone()).map_err(|_| invalid("source ref"))?;
            let v = tx
                .artifacts()
                .version(&reference.version_id)?
                .ok_or(StorageError::ScopeDenied)?;
            if v.blob_generation != j.source_generation {
                return Err(StorageError::ScopeDenied);
            }
            artifacts
                .validate_import_source(tx, &ctx, &reference, &v)
                .map_err(failure)?;
            let parent = parent_path(tx, &d.scope, metadata.parent_path.as_deref().unwrap_or(&[]))
                .map_err(failure)?;
            let documents =
                tx.conventions()
                    .convention_import_artifacts(&d.operation_id, None, 2)?;
            let document_ref = |kind: &str| {
                documents
                    .iter()
                    .find(|row| row.kind == kind)
                    .map(|row| {
                        serde_json::to_value(crate::services::artifacts::version_ref(&row.version))
                            .map_err(|_| invalid("private document ref"))
                    })
                    .transpose()
            };
            let content_ref = document_ref("Markdown")?;
            let raw_content_ref = document_ref("Raw")?;
            let now = tx.now_ms();
            let sort_order = match metadata.sort_order {
                Some(order) => order,
                None => {
                    let maximum = tx
                        .conventions()
                        .convention_max_sort_order(&d.scope, parent.as_ref().map(|p| &p.id))?
                        .unwrap_or(-1);
                    let maximum = i32::try_from(maximum).map_err(|_| {
                        invalid("stored sibling sort order exceeds retained i32 range")
                    })?;
                    // Frozen WikiService increments an int in the default unchecked context;
                    // MySqlWikiStore uses -1 for no siblings. Retain MAX -> MIN explicitly.
                    maximum.wrapping_add(1)
                }
            };
            let proto = StoredConvention {
                id: d.convention_id.clone(),
                scope: d.scope.clone(),
                revision: 1,
                slug: metadata.slug.ok_or_else(|| invalid("missing slug"))?,
                title: metadata.title.ok_or_else(|| invalid("missing title"))?,
                parent_id: parent.as_ref().map(|p| p.id.clone()),
                sort_order: i64::from(sort_order),
                lifecycle: metadata
                    .lifecycle
                    .ok_or_else(|| invalid("missing lifecycle"))?,
                content_ref,
                raw_content_ref,
                applicability: metadata.applicability.unwrap_or_default(),
                provenance: serde_json::to_value(
                    crate::protocol::records::ConventionProvenance::Imported {
                        actor_id: d.author_id.clone(),
                        source: reference,
                        schema: j.schema.clone(),
                        imported_at_ms: crate::services::sessions::counter(now),
                    },
                )
                .map_err(|_| invalid("provenance"))?,
                author_id: d.author_id.clone(),
                created_at_ms: d.created_at_ms,
                updated_at_ms: now,
                legacy_approved: false,
            };
            tx.conventions().set_convention_import_metadata(
                &d.operation_id,
                &proto,
                parent.as_ref().map(|p| p.revision),
            )?;
            let d = tx
                .conventions()
                .convention_import(&d.operation_id)?
                .ok_or_else(|| invalid("import missing"))?;
            let d = tx.conventions().set_convention_import_phase(
                &d.operation_id,
                d.revision,
                "Ready",
                Some(resources),
                None,
                None,
            )?;
            artifacts.check_import_quota(tx, &d)?;
            let row = tx.conventions().publish_convention_import(
                &d.operation_id,
                d.revision,
                d.expected_scope_revision,
                resources,
            )?;
            index.record_changed(tx, &ctx, &row)?;
            let id = EventId(tx.new_id(IdKind::Event));
            tx.history().append(NewEvent {
                id,
                actor: ctx.principal().clone(),
                project_id: d.scope.project().cloned(),
                root_id: ctx.root_id().cloned(),
                operation_id: Some(d.operation_id.clone()),
                aggregate_kind: "convention".into(),
                aggregate_id: row.id.as_str().into(),
                aggregate_revision: 1,
                kind: "ConventionImported".into(),
                payload: json!({"resourceCount":resources}),
            })?;
            let result = OperationResult::ConventionsImport {
                value: ConventionRecordResult::Current(model::record(
                    &row,
                    index.status_in(tx, &row)?,
                )?),
            };
            // Publication and its exact result share one transaction. Later edits, index
            // progress and scope moves must not change an interrupted import's outcome.
            let attempt = op
                .latest_attempt()
                .ok_or_else(|| invalid("import attempt missing"))?;
            let saved =
                serde_json::to_value(&result).map_err(|_| invalid("import publication result"))?;
            tx.operations().observe(
                &d.operation_id,
                attempt.attempt_no,
                json!({"conventionImportPublished":saved}),
            )?;
            Ok(result)
        })
    }
    fn result(
        &self,
        draft: &StoredConventionImport,
        ctx: &crate::context::InvocationContext,
    ) -> Result<OperationResult, StorageError> {
        let id = draft.convention_id.clone();
        let scope = draft.scope.clone();
        Preparation::new(self.store.as_ref(), None, None).read(|read| {
            super::super::access::scope(read, ctx, Some(&model::scope(&scope)), false)
                .map_err(failure)?;
            let operation = read
                .operations()
                .operation(&draft.operation_id)?
                .ok_or_else(|| invalid("import journal missing"))?;
            let saved = operation
                .attempts
                .iter()
                .flat_map(|a| &a.observations)
                .find_map(|value| value.get("conventionImportPublished"))
                .ok_or(StorageError::CommitUnknown)?;
            let result: OperationResult = serde_json::from_value(saved.clone())
                .map_err(|_| invalid("invalid import publication result"))?;
            let OperationResult::ConventionsImport {
                value: ConventionRecordResult::Current(record),
            } = &result
            else {
                return Err(invalid("import publication result kind"));
            };
            let expected_scope = serde_json::to_value(model::scope(&scope))
                .map_err(|_| invalid("import publication scope"))?;
            if record.id != id
                || record.revision.get() != 1
                || serde_json::to_value(&record.scope)
                    .map_err(|_| invalid("import publication scope"))?
                    != expected_scope
            {
                return Err(invalid("import publication result identity"));
            }
            Ok(result)
        })
    }
    pub fn recover_pending(&self) -> Result<usize, StorageError> {
        let mut after = None;
        let mut count = 0;
        loop {
            if self.is_stopped() {
                break;
            }
            let rows = Preparation::new(self.store.as_ref(), None, None).read(|r| {
                r.conventions()
                    .convention_imports_pending(after.as_ref(), 32)
            })?;
            if rows.is_empty() {
                break;
            }
            after = rows.last().map(|r| r.operation_id.clone());
            for row in rows {
                let id = row.operation_id.clone();
                let row = self.write(move |tx| {
                    let d = tx
                        .conventions()
                        .convention_import(&id)?
                        .ok_or_else(|| invalid("private import missing"))?;
                    if matches!(d.phase.as_str(), "Queued" | "Parsing" | "Ready") {
                        let op = tx
                            .operations()
                            .operation(&id)?
                            .ok_or_else(|| invalid("owning import missing"))?;
                        let phase = match op.state {
                            crate::protocol::OperationState::CancelledBeforeDispatch => {
                                Some("Cancelled")
                            }
                            crate::protocol::OperationState::Failed => Some("Failed"),
                            _ => None,
                        };
                        if let Some(phase) = phase {
                            return tx.conventions().set_convention_import_phase(
                                &id,
                                d.revision,
                                phase,
                                None,
                                Some("Owning import operation ended before publication"),
                                None,
                            );
                        }
                    }
                    Ok(d)
                })?;
                if matches!(row.phase.as_str(), "Cancelled" | "Failed") {
                    loop {
                        if self.is_stopped() {
                            return Ok(count);
                        }
                        let id = row.operation_id.clone();
                        let finished = self.write(move |tx| {
                            tx.conventions()
                                .discard_convention_import_artifacts(&id, 32)
                        })?;
                        if finished {
                            break;
                        }
                    }
                    count += 1;
                }
            }
        }
        Ok(count)
    }
}
fn failure(error: Failure) -> StorageError {
    match error {
        Failure::Storage(error) => error,
        _ => StorageError::ScopeDenied,
    }
}
