use super::publish::{Publication, destination, publish_record};
use super::service::{
    DeltaSelection, ImportService, artifact_ref, calculate, check_revision, human, invalid, refuse,
    run_view, scope_digest, stopped,
};
use super::source::Captured;
use super::{HistoryKind, Snapshot, SourceRecord};
use crate::context::{InvocationContext, PrincipalKind};
use crate::fingerprint::sha256_hex;
use crate::gateway::AdmittedCommand;
use crate::operations::imports as ops;
use crate::ports::storage::{
    ArtifactScope, Cancellation, NewEvent, NewOperation, NewQueueEntry, OperationRecord,
    ReadOptions, ReadSnapshot, StorageError, WorkImportConfig, WorkImportIssue, WorkImportMode,
    WorkImportRun, WriteOptions, WriteTransaction, read_snapshot, run_write,
};
use crate::ports::{AuthorityFence, IdKind, Preparation, Prepared};
use crate::protocol::results::{DomainCommandReply, DomainDecision, OperationResult};
use crate::protocol::{
    ActorId, CommandReply, DomainErrorCode, EventId, Id, OperationId, OperationState, ProjectId,
    RootId, StoreId,
};
use crate::services::artifacts::host::{HostArtifactPublication, HostStream, WORK_IMPORT_INPUT};
use crate::services::artifacts::{ArtifactService, StagedFile};
use crate::services::effect::{EffectOutcome, JournaledEffect};
use crate::services::kernel::{Failure, not_found};
use crate::services::{approvals, conventions::control, effect_job};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub const EFFECT_CONSUMER: &str = "selected_work_import";
const KIND: &str = "SelectedWorkImportV1";
const CHUNK: usize = 64;
const RETAINED_IDS: usize = 128;
const RETAINED_ISSUES: usize = 1024;
const INLINE_EVIDENCE_BYTES: usize = 64 * 1024;

struct PreparedRun {
    preview: WorkImportRun,
    source: StagedFile,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Job {
    kind: String,
    actor: ActorId,
    actor_kind: String,
    store_id: StoreId,
    restore_epoch: i64,
    source_context: Value,
    fences: Vec<AuthorityFence>,
    delivery: Id,
    controls: Vec<(RootId, i64)>,
    assignment_generation: Option<i64>,
}

impl ImportService {
    pub(super) fn prepare_apply(
        &self,
        ctx: &InvocationContext,
        env: &Preparation<'_>,
        input: &ops::ApplyInput,
    ) -> Result<Option<Prepared>, StorageError> {
        let outcome = (|| -> Result<PreparedRun, Failure> {
            human(ctx)?;
            let (settings, preview) = env.read(|read| {
                let settings = read
                    .work_imports()?
                    .import_config(&input.config_id)?
                    .filter(|row| row.owner == *ctx.principal());
                let preview = read
                    .work_imports()?
                    .import_preview(ctx.principal(), input.preview_digest.as_str())?;
                Ok((settings, preview))
            })?;
            let settings =
                settings.ok_or_else(|| not_found("import_settings", &input.config_id))?;
            check_revision(
                &settings.id,
                input.expected_revision.get(),
                settings.revision,
            )?;
            let preview =
                preview.ok_or_else(|| refuse(DomainErrorCode::SourceChanged, "import_preview"))?;
            if input.source_snapshot.as_str() != preview.source_snapshot {
                return Err(refuse(DomainErrorCode::SourceChanged, "import_preview"));
            }
            let source = self.sources.capture(&settings.settings.source, None, &|| {
                stopped(&*self.clock, env)
            })?;
            if source.snapshot.digest != preview.source_snapshot {
                return Err(refuse(DomainErrorCode::SourceChanged, "import_snapshot"));
            }
            let computed = env.read(|read| {
                Ok(calculate(
                    read,
                    ctx.principal(),
                    &source,
                    &settings.settings,
                    preview.requested_mode,
                    preview.updated_since_ms,
                    &|| stopped(&*self.clock, env),
                ))
            })??;
            if computed.binding_digest != preview.binding_digest
                || computed.scope_digest != preview.scope_digest
            {
                return Err(refuse(DomainErrorCode::SourceChanged, "import_destination"));
            }
            let source =
                self.stage_selected(&source, &settings, &mut || stopped(&*self.clock, env))?;
            Ok(PreparedRun { source, preview })
        })();
        Ok(Some(Prepared::new(outcome)))
    }

    pub(super) fn prepare_sync(
        &self,
        ctx: &InvocationContext,
        command: &AdmittedCommand,
        env: &Preparation<'_>,
        input: &ops::SyncInput,
    ) -> Result<Option<Prepared>, StorageError> {
        let outcome = (|| -> Result<PreparedRun, Failure> {
            let (settings, owner) = env.read(|read| {
                let settings = read.work_imports()?.import_config(&input.config_id)?;
                let owner = scheduled_owner(read, ctx)?;
                Ok((settings, owner))
            })?;
            let settings = settings
                .filter(|c| {
                    Some(&c.owner) == owner.as_ref() && c.approved && c.allow_scheduled_delta
                })
                .ok_or_else(|| refuse(DomainErrorCode::PermissionDenied, "import_schedule"))?;
            check_revision(
                &settings.id,
                input.expected_revision.get(),
                settings.revision,
            )?;
            let source = self.sources.capture(&settings.settings.source, None, &|| {
                stopped(&*self.clock, env)
            })?;
            if source.root_identity != settings.root_identity
                || source.snapshot.manifest.namespace != settings.source_namespace
            {
                return Err(refuse(DomainErrorCode::SourceChanged, "import_schedule"));
            }
            let preview = env.read(|read| {
                Ok(calculate(
                    read,
                    &settings.owner,
                    &source,
                    &settings.settings,
                    WorkImportMode::Delta,
                    None,
                    &|| stopped(&*self.clock, env),
                ))
            })??;
            let recorded = self.record_preview(
                ctx,
                command.authorization(),
                &source,
                &settings.owner,
                WorkImportMode::Delta,
                preview,
            )?;
            let source =
                self.stage_selected(&source, &settings, &mut || stopped(&*self.clock, env))?;
            Ok(PreparedRun {
                preview: recorded,
                source,
            })
        })();
        Ok(Some(Prepared::new(outcome)))
    }

    fn stage_selected(
        &self,
        capture: &Captured,
        settings: &WorkImportConfig,
        stopped: &mut dyn FnMut() -> bool,
    ) -> Result<StagedFile, StorageError> {
        let bytes = capture
            .snapshot
            .seal_selection(&settings.settings)
            .map_err(|_| invalid("bounded selected work snapshot"))?;
        self.artifacts.stage_host_reader(
            &*self.store,
            HostStream {
                reader: &mut std::io::Cursor::new(&bytes),
                digest: &sha256_hex(&bytes),
                byte_count: bytes.len() as u64,
                source: json!({"kind":WORK_IMPORT_INPUT,"sourceSnapshot":capture.snapshot.digest}),
            },
            stopped,
        )?.map_err(|_| StorageError::ViewUnavailable)
    }

    pub(super) fn settle_prepared_run(
        &self,
        prepared: Option<&Prepared>,
        reply: Option<&DomainCommandReply>,
    ) {
        let Some(Ok(run)) = prepared.and_then(Prepared::get::<Result<PreparedRun, Failure>>) else {
            return;
        };
        // On an unknown outcome, leave the staging intent for normal recovery rather than
        // deleting a generation the just-committed import may own.
        if matches!(reply, Some(CommandReply::CommandStatusUnknown { .. })) {
            return;
        }
        let committed = matches!(reply, Some(CommandReply::Recorded { receipt, replayed: false, .. })
            if matches!(&receipt.decision, DomainDecision::Accepted { .. }));
        self.artifacts
            .settle_host_bytes(&*self.store, &run.source, committed);
    }

    pub(super) fn apply(
        &self,
        ctx: &InvocationContext,
        command: &AdmittedCommand,
        tx: &mut dyn WriteTransaction,
        input: &ops::ApplyInput,
    ) -> Result<DomainDecision, Failure> {
        human(ctx)?;
        let config = tx
            .work_imports()?
            .import_config(&input.config_id)?
            .filter(|row| row.owner == *ctx.principal())
            .ok_or_else(|| not_found("import_settings", &input.config_id))?;
        self.admit(
            ctx,
            command,
            tx,
            config,
            input.expected_revision.get(),
            false,
        )
    }

    pub(super) fn sync(
        &self,
        ctx: &InvocationContext,
        command: &AdmittedCommand,
        tx: &mut dyn WriteTransaction,
        input: &ops::SyncInput,
    ) -> Result<DomainDecision, Failure> {
        let owner = schedule_owner(tx.jobs(), ctx)?;
        let config = tx
            .work_imports()?
            .import_config(&input.config_id)?
            .filter(|row| Some(&row.owner) == owner.as_ref() && row.allow_scheduled_delta)
            .ok_or_else(|| refuse(DomainErrorCode::PermissionDenied, "import_schedule"))?;
        self.admit(
            ctx,
            command,
            tx,
            config,
            input.expected_revision.get(),
            true,
        )
    }

    fn admit(
        &self,
        ctx: &InvocationContext,
        command: &AdmittedCommand,
        tx: &mut dyn WriteTransaction,
        config: WorkImportConfig,
        expected: i64,
        scheduled: bool,
    ) -> Result<DomainDecision, Failure> {
        check_revision(&config.id, expected, config.revision)?;
        let prepared = command
            .prepared::<Result<PreparedRun, Failure>>()
            .ok_or_else(|| invalid("missing prepared work import"))?
            .as_ref()
            .map_err(Clone::clone)?;
        let preview = &prepared.preview;
        if !config.approved
            || preview.source_namespace != config.source_namespace
            || preview.root_identity != config.root_identity
            || preview.scope_digest != scope_digest(&config.source_namespace, &config.settings)?
        {
            return Err(refuse(DomainErrorCode::SourceChanged, "import_preview"));
        }
        if let Some(active) = tx.work_imports()?.active_import(&config.source_namespace)?
            && let Some(operation) = tx.operations().operation(&OperationId(active.id))?
            && !matches!(
                operation.state,
                OperationState::ObservedComplete
                    | OperationState::Failed
                    | OperationState::CancelledBeforeDispatch
            )
        {
            return Err(refuse(DomainErrorCode::Conflict, "import_source_namespace"));
        }
        let operation_id = OperationId(tx.new_id(IdKind::Operation));
        let delivery = tx.new_id(IdKind::QueueEntry);
        let controls = control::capture(tx, ctx)?;
        let job = Job {
            kind: KIND.into(),
            actor: ctx.principal().clone(),
            actor_kind: ctx.principal_kind().as_str().into(),
            store_id: tx.store().store_id.clone(),
            restore_epoch: tx.store().restore_epoch,
            source_context: approvals::save_source_context(ctx),
            fences: command.authorization().fences().to_vec(),
            delivery: delivery.clone(),
            controls: controls.roots,
            assignment_generation: controls.assignment_generation,
        };
        let operation = if scheduled {
            "imports.sync"
        } else {
            "imports.apply"
        };
        tx.operations().record_intent(NewOperation {
            id: operation_id.clone(),
            root_id: None,
            assignment_id: None,
            capability: operation.into(),
            schema_version: 1,
            args_digest: command.fingerprint().as_str().into(),
            payload: serde_json::to_value(job).map_err(|_| invalid("work import job"))?,
        })?;
        let now = tx.now_ms();
        let mut run = preview.clone();
        run.id = operation_id.0.clone();
        run.owner = config.owner;
        run.revision = 1;
        run.mode = preview.requested_mode;
        run.config_id = Some(config.id);
        run.config_revision = Some(config.revision);
        run.state = OperationState::IntentRecorded;
        run.started_at_ms = now;
        run.completed_at_ms = None;
        run.counts = Default::default();
        let host_actor = tx.store().host_actor.clone();
        let version = self.artifacts.commit_host_bytes_in_scope(
            tx,
            &prepared.source,
            HostArtifactPublication {
                title: "Selected work import input",
                media_type: "application/x-ndjson",
                provenance: json!({"kind":WORK_IMPORT_INPUT,"operationId":operation_id,
                "sourceSnapshot":run.source_snapshot}),
                scope: ArtifactScope::Workspace,
                owner: host_actor,
            },
        )?;
        run.input = Some(artifact_ref(version));
        tx.work_imports()?.put_import_run(&run, None)?;
        let event_id = EventId(tx.new_id(IdKind::Event));
        tx.history().append(NewEvent {
            id: event_id.clone(),
            actor: ctx.principal().clone(),
            project_id: None,
            root_id: None,
            operation_id: Some(operation_id.clone()),
            aggregate_kind: "selected_work_import".into(),
            aggregate_id: run.id.as_str().into(),
            aggregate_revision: 1,
            kind: "WorkImportRequested".into(),
            payload: json!({"sourceNamespace":run.source_namespace,"sourceSnapshot":run.source_snapshot}),
        })?;
        tx.queue().enqueue(NewQueueEntry {
            id: delivery,
            consumer: EFFECT_CONSUMER.into(),
            event_id,
            root_id: None,
            operation_id: Some(operation_id.clone()),
            partition: None,
            priority: 0,
            ready_at_ms: now,
            payload: json!({"kind":KIND}),
        })?;
        Ok(DomainDecision::Accepted { operation_id })
    }

    fn execute_run(
        &self,
        operation: &OperationRecord,
        stopped: &Cancellation,
    ) -> Result<WorkImportRun, StorageError> {
        let job: Job = serde_json::from_value(operation.payload.clone())
            .map_err(|_| invalid("work import job"))?;
        let (run, config) =
            self.write_checked(operation, &job, |_, run, config, _| Ok((run, config)))?;
        if run.completed_at_ms.is_some() {
            return Ok(run);
        }
        let checked_at = std::cell::Cell::new(None::<std::time::Instant>);
        let cancelled = std::cell::Cell::new(false);
        let should_stop = || {
            if stopped.is_cancelled() || cancelled.get() {
                return true;
            }
            let now = std::time::Instant::now();
            if checked_at
                .get()
                .is_none_or(|previous| now.duration_since(previous).as_millis() >= 50)
            {
                checked_at.set(Some(now));
                cancelled.set(self.stopped(&job, stopped));
            }
            cancelled.get()
        };
        let root_identity = self.sources.root_identity(&config.settings.source)?;
        if root_identity != run.root_identity {
            return Err(invalid("import source root changed after admission"));
        }
        let input = run
            .input
            .as_ref()
            .ok_or_else(|| invalid("prepared work import input missing"))?;
        let bytes = self.artifacts.read_work_import_input(
            &*self.store,
            input,
            &operation.id,
            &run.source_snapshot,
            &mut || should_stop(),
        )?;
        let mut snapshot =
            Snapshot::parse(&bytes).map_err(|_| invalid("prepared work import input"))?;
        drop(bytes);
        if snapshot.manifest.namespace != run.source_namespace
            || snapshot.manifest.captured_at_ms != run.captured_through_ms
        {
            return Err(invalid("prepared work import source binding changed"));
        }
        snapshot.digest.clone_from(&run.source_snapshot);
        let captured = Captured {
            snapshot,
            root_identity,
        };
        let records = captured
            .snapshot
            .select(&config.settings, config.all_projects)
            .map_err(|_| invalid("import selection changed"))?;
        if records.records.len() as i64 != run.rows {
            return Err(invalid("import selection changed after admission"));
        }
        if run.cursor == 0 {
            let current = read_snapshot(&*self.store, &ReadOptions::default(), |read| {
                Ok(calculate(
                    read,
                    &run.owner,
                    &captured,
                    &config.settings,
                    run.mode,
                    run.updated_since_ms,
                    &should_stop,
                ))
            })?
            .map_err(Failure::into_storage)?;
            if current.binding_digest != run.binding_digest {
                return Err(invalid("import destination changed after preview"));
            }
        }
        let delta = std::sync::Arc::new(read_snapshot(
            &*self.store,
            &ReadOptions::default(),
            |read| {
                DeltaSelection::new(
                    read,
                    &run.owner,
                    &run.source_namespace,
                    &run.scope_digest,
                    run.config_revision,
                    run.updated_since_ms,
                    &records.records,
                )
            },
        )?);
        let mut remaining = &records.records[run.cursor..];
        while !remaining.is_empty() {
            if should_stop() {
                return Err(StorageError::Cancelled);
            }
            let count = if needs_evidence(remaining[0]) {
                1
            } else {
                remaining
                    .iter()
                    .take(CHUNK)
                    .take_while(|record| !needs_evidence(record))
                    .count()
            };
            let partition: Vec<_> = remaining[..count]
                .iter()
                .map(|record| (*record).clone())
                .collect();
            self.publish_chunk(operation, &job, partition, delta.clone())?;
            remaining = &remaining[count..];
        }
        self.finish(operation, &job, None)
    }

    fn stopped(&self, job: &Job, cancellation: &Cancellation) -> bool {
        cancellation.is_cancelled()
            || read_snapshot(&*self.store, &ReadOptions::default(), |read| {
                Ok(read
                    .queue()
                    .entry(&job.delivery)?
                    .is_none_or(|e| e.cancel_requested))
            })
            .unwrap_or(true)
    }

    fn publish_chunk(
        &self,
        operation: &OperationRecord,
        job: &Job,
        records: Vec<SourceRecord>,
        delta: std::sync::Arc<DeltaSelection>,
    ) -> Result<(), StorageError> {
        let evidence = if records.len() == 1 {
            self.stage_evidence(&records[0])?
        } else {
            None
        };
        let data = evidence.clone();
        let artifacts = self.artifacts.clone();
        let result = self.write_checked(operation, job, move |tx, mut run, _, actor| {
            let previous = run.revision;
            let mut published_artifact = false;
            for record in &records {
                let mapping = tx.work_imports()?.import_mapping(&run.source_namespace, record.kind(), record.id())?;
                if delta.outside_window(record, mapping.as_ref()) {
                    run.counts.skipped += 1;
                    continue;
                }
                let result = publish_record(
                    tx,
                    &actor,
                    &run.source_namespace,
                    record,
                    run.captured_through_ms,
                    run.repository_bindings.binary_search_by(|binding| {
                        binding.source_id.as_str().cmp(record.id().as_str())
                    }).ok().map(|index| &run.repository_bindings[index]),
                    |tx, project| {
                        let Some((staged, media_type)) = &data else { return Ok(None) };
                        let version = artifacts.commit_host_bytes_in_scope(tx, staged, HostArtifactPublication {
                            title: "Imported historical evidence",
                            media_type,
                            provenance: json!({"kind":"WorkImportEvidence","sourceIdentity":record.id()}),
                            scope: ArtifactScope::Project(project),
                            owner: run.owner.clone(),
                        })?;
                        published_artifact = true;
                        Ok(Some(artifact_ref(version)))
                    },
                )?;
                let reason = record_result(&mut run, record, result);
                if reason.is_some() || delta.retries(record) {
                    let project = ProjectId(destination(&run.source_namespace, "Project", record.project()));
                    tx.work_imports()?.put_import_retry(&run, &project, record.kind(), record.id(), reason)?;
                }
            }
            run.revision += 1;
            run.state = OperationState::Dispatching;
            run.cursor += records.len();
            tx.work_imports()?.put_import_run(&run, Some(previous))?;
            Ok(published_artifact)
        });
        if let Some((staged, _)) = &evidence {
            self.artifacts.settle_host_bytes(
                &*self.store,
                staged,
                matches!(&result, Ok(true) | Err(StorageError::CommitUnknown)),
            );
        }
        result.map(|_| ())
    }

    fn stage_evidence(
        &self,
        record: &SourceRecord,
    ) -> Result<Option<(StagedFile, &'static str)>, StorageError> {
        let SourceRecord::History {
            evidence,
            history_kind,
            ..
        } = record
        else {
            return Ok(None);
        };
        let (bytes, media_type) = if matches!(
            history_kind,
            HistoryKind::Attachment | HistoryKind::TranscriptEvidence
        ) && let Some(encoded) = evidence["bytesBase64"].as_str()
        {
            let bytes = STANDARD
                .decode(encoded)
                .map_err(|_| invalid("invalid import attachment"))?;
            if evidence["sha256"]
                .as_str()
                .is_none_or(|digest| !digest.eq_ignore_ascii_case(&sha256_hex(&bytes)))
            {
                return Err(invalid("import attachment digest mismatch"));
            }
            (bytes, "application/octet-stream")
        } else {
            let bytes =
                serde_json::to_vec(evidence).map_err(|_| invalid("import historical evidence"))?;
            if bytes.len() <= INLINE_EVIDENCE_BYTES {
                return Ok(None);
            }
            (bytes, "application/json")
        };
        self.artifacts
            .stage_host_bytes(
                &*self.store,
                &bytes,
                json!({"kind":"WorkImportEvidence","sourceIdentity":record.id()}),
            )?
            .map(|staged| Some((staged, media_type)))
            .map_err(|_| StorageError::ViewUnavailable)
    }

    fn write_checked<T, F>(
        &self,
        operation: &OperationRecord,
        job: &Job,
        perform: F,
    ) -> Result<T, StorageError>
    where
        T: Send + 'static,
        F: FnOnce(
                &mut dyn WriteTransaction,
                WorkImportRun,
                WorkImportConfig,
                InvocationContext,
            ) -> Result<T, StorageError>
            + Send
            + 'static,
    {
        let operation = operation.clone();
        let job = job.clone();
        run_write(&*self.store, WriteOptions::host_state(), move |tx| {
            effect_job::attempt_holds(tx, &operation, KIND)?;
            effect_job::delivery_holds(tx, &job.delivery, EFFECT_CONSUMER, &operation.id)?;
            if job.kind != KIND
                || !matches!(
                    operation.capability.as_str(),
                    "imports.apply" | "imports.sync"
                )
            {
                return Err(StorageError::ScopeDenied);
            }
            let (kind, context) = effect_job::restore(
                tx,
                &effect_job::JobBinding {
                    store_id: &job.store_id,
                    restore_epoch: job.restore_epoch,
                    fences: &job.fences,
                    actor: &job.actor,
                    actor_kind: &job.actor_kind,
                    source_context: &job.source_context,
                    control_roots: &job.controls,
                    assignment_generation: job.assignment_generation,
                },
                &operation.id,
            )?;
            let run = tx
                .work_imports()?
                .import_run(&operation.id.0)?
                .ok_or(StorageError::ScopeDenied)?;
            let config = tx
                .work_imports()?
                .import_config(run.config_id.as_ref().ok_or(StorageError::ScopeDenied)?)?
                .filter(|c| {
                    c.approved
                        && c.owner == run.owner
                        && Some(c.revision) == run.config_revision
                        && c.source_namespace == run.source_namespace
                        && c.root_identity == run.root_identity
                })
                .ok_or(StorageError::ScopeDenied)?;
            match kind {
                PrincipalKind::Human
                    if operation.capability == "imports.apply" && config.owner == job.actor =>
                {
                    human(&context).map_err(Failure::into_storage)?;
                }
                PrincipalKind::Host
                    if operation.capability == "imports.sync" && config.allow_scheduled_delta =>
                {
                    if schedule_owner(tx.jobs(), &context)?.as_ref() != Some(&config.owner) {
                        return Err(StorageError::ScopeDenied);
                    }
                }
                _ => return Err(StorageError::ScopeDenied),
            }
            perform(tx, run, config, context)
        })
    }

    fn finish(
        &self,
        operation: &OperationRecord,
        job: &Job,
        problem: Option<&'static str>,
    ) -> Result<WorkImportRun, StorageError> {
        let run = read_snapshot(&*self.store, &ReadOptions::default(), |read| {
            read.work_imports()?.import_run(&operation.id.0)
        })?
        .ok_or_else(|| invalid("work import run missing"))?;
        if run.completed_at_ms.is_some() {
            return Ok(run);
        }
        let bytes = serde_json::to_vec(&json!({
            "format":"DevWorksWorkImportResult","version":1,"sourceNamespace":run.source_namespace,
            "sourceSnapshot":run.source_snapshot,"previewDigest":run.preview_digest,"mode":run.mode,
            "updatedSinceMs":run.updated_since_ms,"coverageThroughMs":run.captured_through_ms,
            "processed":run.cursor,"total":run.rows,"counts":run.counts,"issues":run.issues,
            "importedIds":run.imported_ids,"problem":problem,
            "omittedIssueDetails":run.issue_count.saturating_sub(run.issues.len() as i64),
        }))
        .map_err(|_| invalid("work import result report"))?;
        let staged = self
            .artifacts
            .stage_host_bytes(&*self.store, &bytes, json!({"kind":"WorkImportResult"}))?
            .map_err(|_| StorageError::ViewUnavailable)?;
        let held = staged.clone();
        let artifacts = self.artifacts.clone();
        let result = if problem.is_none() {
            self.write_checked(operation, job, move |tx, current, _, _| {
                if current.cursor as i64 != current.rows || current.revision != run.revision {
                    return Err(invalid("work import checkpoint changed"));
                }
                finalize(tx, &artifacts, &held, current, None)
            })
        } else {
            // A failed or interrupted operation may report previously committed progress even
            // after the source grant was revoked. This transaction can write only its own report.
            let operation = operation.clone();
            run_write(&*self.store, WriteOptions::host_state(), move |tx| {
                effect_job::attempt_holds(tx, &operation, KIND)?;
                if tx
                    .work_imports()?
                    .import_run(&run.id)?
                    .is_none_or(|r| r.revision != run.revision)
                {
                    return Err(invalid("work import checkpoint changed"));
                }
                finalize(tx, &artifacts, &held, run, problem)
            })
        };
        self.artifacts.settle_host_bytes(
            &*self.store,
            &staged,
            matches!(&result, Ok(_) | Err(StorageError::CommitUnknown)),
        );
        result
    }
}

fn finalize(
    tx: &mut dyn WriteTransaction,
    artifacts: &crate::services::artifacts::ArtifactService,
    staged: &StagedFile,
    mut run: WorkImportRun,
    problem: Option<&str>,
) -> Result<WorkImportRun, StorageError> {
    let report = artifacts.commit_host_bytes_in_scope(
        tx,
        staged,
        HostArtifactPublication {
            title: "Work import result",
            media_type: "application/json",
            provenance: json!({"kind":"WorkImportResult","runId":run.id}),
            scope: ArtifactScope::Workspace,
            owner: run.owner.clone(),
        },
    )?;
    run.report = artifact_ref(report);
    if let Some(input) = run.input.take() {
        ArtifactService::retire_work_import_input(tx, &input, &OperationId(run.id.clone()))?;
    }
    run.completed_at_ms = Some(tx.now_ms());
    run.state = if problem.is_some() {
        OperationState::Failed
    } else {
        OperationState::ObservedComplete
    };
    let before = run.revision;
    run.revision += 1;
    tx.work_imports()?.put_import_run(&run, Some(before))?;
    Ok(run)
}

fn record_result(
    run: &mut WorkImportRun,
    record: &SourceRecord,
    result: Publication,
) -> Option<&'static str> {
    match result {
        Publication::Added | Publication::Changed => {
            if result == Publication::Added {
                run.counts.added += 1
            } else {
                run.counts.changed += 1
            }
            if run.imported_ids.len() < RETAINED_IDS {
                run.imported_ids.push(destination(
                    &run.source_namespace,
                    record.kind(),
                    record.id(),
                ));
            }
        }
        Publication::Unchanged => run.counts.unchanged += 1,
        Publication::MissingReference | Publication::DestinationChanged => {
            let reason = if result == Publication::MissingReference {
                run.counts.skipped += 1;
                "unresolved_source_reference"
            } else {
                run.counts.failures += 1;
                "destination_revision_changed"
            };
            run.issue_count += 1;
            if run.issues.len() < RETAINED_ISSUES {
                run.issues.push(WorkImportIssue {
                    source_id: record.id().clone(),
                    kind: record.kind().into(),
                    reason: reason.into(),
                });
            }
            return Some(reason);
        }
    }
    None
}

fn needs_evidence(record: &SourceRecord) -> bool {
    matches!(record, SourceRecord::History { evidence, .. }
        if evidence["bytesBase64"].is_string()
        || serde_json::to_vec(evidence).is_ok_and(|bytes| bytes.len() > INLINE_EVIDENCE_BYTES))
}

fn scheduled_owner(
    read: &dyn ReadSnapshot,
    ctx: &InvocationContext,
) -> Result<Option<ActorId>, StorageError> {
    schedule_owner(read.jobs(), ctx)
}

fn schedule_owner(
    jobs: &dyn crate::ports::storage::JobRead,
    ctx: &InvocationContext,
) -> Result<Option<ActorId>, StorageError> {
    if ctx.principal_kind() != PrincipalKind::Host || ctx.project_id().is_some() {
        return Ok(None);
    }
    let Some(id) = ctx.job_run_id() else {
        return Ok(None);
    };
    let Some(run) = jobs.job_run(id)? else {
        return Ok(None);
    };
    let Some(job) = jobs.job(&run.job_id)? else {
        return Ok(None);
    };
    if job.project_id.is_some()
        || !job.authorized
        || job.deleted_at_ms.is_some()
        || job.revision != run.job_revision
        || run.restore_epoch != ctx.restore_epoch().get()
        || run.cancel_requested
        || !matches!(run.status.as_str(), "Running" | "Holding")
        || !scheduled_target(&run.definition).is_some_and(|id| id == super::SYNC_OPERATION)
    {
        return Ok(None);
    }
    Ok(job.authorized_by)
}

pub(super) fn cancel(
    ctx: &InvocationContext,
    tx: &mut dyn WriteTransaction,
    input: &ops::CancelInput,
) -> Result<DomainDecision, Failure> {
    human(ctx)?;
    let mut run = tx
        .work_imports()?
        .import_run(&input.run_id)?
        .filter(|r| r.owner == *ctx.principal() && r.mode != WorkImportMode::DryRun)
        .ok_or_else(|| not_found("work_import_run", &input.run_id))?;
    let operation = tx
        .operations()
        .operation(&OperationId(run.id.clone()))?
        .ok_or_else(|| not_found("work_import_run", &input.run_id))?;
    let terminal = matches!(
        operation.state,
        OperationState::ObservedComplete
            | OperationState::Failed
            | OperationState::CancelledBeforeDispatch
    );
    let changed = if terminal || run.completed_at_ms.is_some() {
        false
    } else {
        check_revision(&run.id, input.expected_revision.get(), run.revision)?;
        let job: Job =
            serde_json::from_value(operation.payload).map_err(|_| invalid("work import job"))?;
        let delivery = tx
            .queue()
            .entry(&job.delivery)?
            .ok_or(StorageError::ScopeDenied)?;
        if delivery.operation_id.as_ref() != Some(&operation.id)
            || delivery.consumer != EFFECT_CONSUMER
        {
            return Err(StorageError::ScopeDenied.into());
        }
        if delivery.cancel_requested {
            false
        } else {
            let after = tx.queue().cancel(&job.delivery)?;
            if after.state == crate::ports::storage::QueueState::Cancelled {
                run = settle_undispatched(tx, &operation.id)?;
            }
            true
        }
    };
    let result = OperationResult::ImportsCancel {
        value: crate::protocol::records::MutationSummary {
            id: run.id,
            revision: crate::services::kernel::rev(run.revision),
            changed,
        },
    };
    Ok(if changed {
        DomainDecision::Applied { result }
    } else {
        DomainDecision::NoChange { result }
    })
}

fn settle_undispatched(
    tx: &mut dyn WriteTransaction,
    operation: &OperationId,
) -> Result<WorkImportRun, StorageError> {
    let mut run = tx
        .work_imports()?
        .import_run(&operation.0)?
        .ok_or(StorageError::ScopeDenied)?;
    let current = tx
        .operations()
        .operation(operation)?
        .ok_or(StorageError::ScopeDenied)?;
    if current.state != OperationState::CancelledBeforeDispatch || run.cursor != 0 {
        return Err(StorageError::ScopeDenied);
    }
    if run.completed_at_ms.is_none() {
        if let Some(input) = run.input.take() {
            ArtifactService::retire_work_import_input(tx, &input, operation)?;
        }
        let old = run.revision;
        run.revision += 1;
        run.state = OperationState::CancelledBeforeDispatch;
        run.completed_at_ms = Some(tx.now_ms());
        tx.work_imports()?.put_import_run(&run, Some(old))?;
    }
    Ok(run)
}

pub(crate) struct QueueHook;

impl crate::services::queue::ConsumerHook for QueueHook {
    fn consumer(&self) -> &'static str {
        EFFECT_CONSUMER
    }

    fn cancelled(
        &self,
        tx: &mut dyn WriteTransaction,
        entry: &crate::ports::storage::QueueEntry,
    ) -> Result<(), StorageError> {
        if let Some(operation) = &entry.operation_id {
            settle_undispatched(tx, operation)?;
        }
        Ok(())
    }
}

fn scheduled_target(definition: &str) -> Option<&'static str> {
    let definition =
        serde_json::from_str::<crate::protocol::records::JobDefinition>(definition).ok()?;
    match definition {
        crate::protocol::records::JobDefinition::Native { capability_id, .. }
            if capability_id == crate::services::jobs::capability_id(super::SYNC_OPERATION) =>
        {
            Some(super::SYNC_OPERATION)
        }
        _ => None,
    }
}

pub(crate) fn validate_schedule(
    tx: &mut dyn WriteTransaction,
    ctx: &InvocationContext,
    project: Option<&ProjectId>,
    input: &Value,
) -> Result<(), StorageError> {
    human(ctx).map_err(Failure::into_storage)?;
    let args: ops::SyncInput =
        serde_json::from_value(input.clone()).map_err(|_| invalid("work import schedule"))?;
    if project.is_some()
        || tx
            .work_imports()?
            .import_config(&args.config_id)?
            .is_none_or(|c| {
                c.owner != *ctx.principal()
                    || !c.approved
                    || !c.allow_scheduled_delta
                    || c.revision != args.expected_revision.get()
            })
    {
        return Err(StorageError::ScopeDenied);
    }
    Ok(())
}

pub(crate) fn cancel_scheduled(
    tx: &mut dyn WriteTransaction,
    run: &crate::ports::storage::StoredJobRun,
) -> Result<(), StorageError> {
    if scheduled_target(&run.definition).is_none() {
        return Ok(());
    }
    let Some(id) = &run.inner_operation_id else {
        return Ok(());
    };
    let op = tx
        .operations()
        .operation(id)?
        .ok_or(StorageError::ScopeDenied)?;
    let job: Job =
        serde_json::from_value(op.payload).map_err(|_| invalid("work import schedule job"))?;
    let entry = tx
        .queue()
        .entry(&job.delivery)?
        .ok_or(StorageError::ScopeDenied)?;
    if op.capability != super::SYNC_OPERATION
        || entry.operation_id.as_ref() != Some(id)
        || entry.consumer != EFFECT_CONSUMER
        || job.actor != tx.store().host_actor
        || job.source_context["invocationContext"]["jobRun"] != json!(run.id)
    {
        return Err(StorageError::ScopeDenied);
    }
    if !entry.cancel_requested {
        let after = tx.queue().cancel(&job.delivery)?;
        if after.state == crate::ports::storage::QueueState::Cancelled {
            settle_undispatched(tx, id)?;
        }
    }
    Ok(())
}

fn effect_result(operation: &OperationRecord, run: &WorkImportRun) -> EffectOutcome {
    if run.state != OperationState::ObservedComplete {
        return EffectOutcome::Failed(
            "import stopped; inspect its durable result and checkpoint".into(),
        );
    }
    let value = run_view(run);
    let result = if operation.capability == "imports.sync" {
        OperationResult::ImportsSync { value }
    } else {
        OperationResult::ImportsApply { value }
    };
    EffectOutcome::Completed(json!({"result":result}))
}

impl JournaledEffect for ImportService {
    fn resumes_after_observation(&self) -> bool {
        true
    }

    fn observes_delivery_cancellation(&self) -> bool {
        true
    }

    fn execute(&self, operation: &OperationRecord) -> EffectOutcome {
        self.execute_cancellable(operation, &Cancellation::default())
    }

    fn execute_cancellable(
        &self,
        operation: &OperationRecord,
        cancellation: &Cancellation,
    ) -> EffectOutcome {
        match self.execute_run(operation, cancellation) {
            Ok(run) => effect_result(operation, &run),
            Err(
                StorageError::CommitUnknown | StorageError::Busy | StorageError::LeaseLost { .. },
            ) => EffectOutcome::Unknown("import write requires checkpoint observation".into()),
            Err(error) if cancellation.is_cancelled() => {
                EffectOutcome::Unknown(format!("import worker abandoned: {error:?}"))
            }
            Err(error) => {
                let job = match serde_json::from_value(operation.payload.clone()) {
                    Ok(job) => job,
                    Err(_) => return EffectOutcome::Failed("invalid work import job".into()),
                };
                let problem = match error {
                    StorageError::Cancelled => "cancelled",
                    StorageError::ScopeDenied => "authority_or_configuration_changed",
                    StorageError::InvalidRecord(_) => "source_or_destination_changed",
                    _ => "source_or_storage_unavailable",
                };
                match self.finish(operation, &job, Some(problem)) {
                    Ok(run) => effect_result(operation, &run),
                    Err(_) => {
                        EffectOutcome::Unknown("import result needs checkpoint observation".into())
                    }
                }
            }
        }
    }

    fn observe(&self, operation: &OperationRecord) -> EffectOutcome {
        let Ok(job) = serde_json::from_value::<Job>(operation.payload.clone()) else {
            return EffectOutcome::Failed("invalid work import job".into());
        };
        let observed = read_snapshot(&*self.store, &ReadOptions::default(), |read| {
            let run = read.work_imports()?.import_run(&operation.id.0)?;
            let delivery = read
                .queue()
                .entry(&job.delivery)?
                .ok_or(StorageError::ScopeDenied)?;
            if delivery.consumer != EFFECT_CONSUMER
                || delivery.operation_id.as_ref() != Some(&operation.id)
            {
                return Err(StorageError::ScopeDenied);
            }
            Ok((run, delivery.cancel_requested))
        });
        match observed {
            Ok((Some(run), _)) if run.completed_at_ms.is_some() => effect_result(operation, &run),
            Ok((Some(_), true)) => match self.finish(operation, &job, Some("cancelled")) {
                Ok(run) => effect_result(operation, &run),
                Err(_) => {
                    EffectOutcome::Unknown("cancelled import awaits its checkpoint report".into())
                }
            },
            Ok((Some(run), false)) if run.input.is_some() && run.cursor as i64 <= run.rows => {
                // This observation does not publish anything. Each target row, source mapping
                // and checkpoint commit together, so only the work after the saved cursor
                // remains. The dispatcher may redeliver that same intent and saved input.
                EffectOutcome::NotApplied(format!(
                    "no unobserved records beyond the durable checkpoint of {}",
                    run.cursor,
                ))
            }
            _ => EffectOutcome::Unknown("work import checkpoint is unavailable".into()),
        }
    }
}
