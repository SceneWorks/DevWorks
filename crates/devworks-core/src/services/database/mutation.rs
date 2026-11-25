//! All target mutations use the existing operation journal and queue. A recovered attempt reads
//! its observations. Its original SQL is never sent again after possible dispatch.
use super::statement::{self, DataMode};
use super::{DataExecutor, Selected, native_storage, parameters};
use crate::authority::destructive::FileIdentity;
use crate::context::InvocationContext;
use crate::gateway::AdmittedCommand;
use crate::operations::database as ops;
use crate::ports::database::data::*;
use crate::ports::storage::*;
use crate::ports::{AuthorityFence, IdKind, Preparation, Prepared};
use crate::protocol::provider::{ArtifactRef, Digest};
use crate::protocol::records::{DatabaseMigrationSource, DbParameter, DbWriteResult};
use crate::protocol::results::{DomainDecision, OperationResult};
use crate::protocol::support::BoundedList;
use crate::protocol::{
    ActorId, DomainErrorCode, EventId, Id, OperationId, OperationState, Revision, RootId, StoreId,
};
use crate::services::approvals::{self, owner_preview::approved_binding};
use crate::services::artifacts::{StagedFile, host::HostArtifactPublication, version_ref};
use crate::services::conventions::control;
use crate::services::effect::{EffectOutcome, JournaledEffect};
use crate::services::effect_job;
use crate::services::kernel::{Failure, Refusal, counter};
use crate::services::schemas::data;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::{Duration, Instant};

pub const EFFECT_CONSUMER: &str = "DatabaseMutation";
const KIND: &str = "DatabaseMutation";
const MAX_INTENT: u64 = 1024 * 1024;
const SCHEMA_EXECUTION_REVISION: u32 = 9;

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Intent {
    sql: String,
    params: Vec<DbParameter>,
    timeout_ms: i64,
    #[serde(default)]
    schema_digest: Option<Digest>,
    #[serde(default)]
    direct_reason: Option<String>,
    #[serde(default)]
    source: Option<DatabaseMigrationSource>,
}

pub(super) struct StagedMutation {
    connection: Id,
    revision: Revision,
    mode: DataMode,
    file_identity: Option<FileIdentity>,
    proposal: Option<Id>,
    staged: StagedFile,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Job {
    kind: String,
    connection: Id,
    revision: Revision,
    configuration: i64,
    global_policy: i64,
    source_policy: i64,
    material: ArtifactRef,
    file_identity: Option<FileIdentity>,
    actor: ActorId,
    actor_kind: String,
    store_id: StoreId,
    restore_epoch: i64,
    fences: Vec<AuthorityFence>,
    source_context: Value,
    delivery_id: Id,
    control_roots: Vec<(RootId, i64)>,
    assignment_generation: Option<i64>,
}

impl Job {
    fn binding(&self) -> effect_job::JobBinding<'_> {
        effect_job::JobBinding {
            store_id: &self.store_id,
            restore_epoch: self.restore_epoch,
            fences: &self.fences,
            actor: &self.actor,
            actor_kind: &self.actor_kind,
            source_context: &self.source_context,
            control_roots: &self.control_roots,
            assignment_generation: self.assignment_generation,
        }
    }
}

fn invalid() -> StorageError {
    StorageError::InvalidRecord("invalid database mutation intent".into())
}

fn mode(operation: &str) -> Result<DataMode, StorageError> {
    match operation {
        "database.execute_write" => Ok(DataMode::Write),
        "database.apply_migration" => Ok(DataMode::Migration),
        "database.direct_schema_change" => Ok(DataMode::Schema),
        _ => Err(invalid()),
    }
}

fn target(input: &ops::Input) -> Result<(&Id, &Revision, &Option<Id>), StorageError> {
    Ok(match input {
        ops::Input::ExecuteWrite(input) => (
            &input.connection_id,
            &input.expected_connection_revision,
            &input.proposal_id,
        ),
        ops::Input::ApplyMigration(input) => (
            &input.connection_id,
            &input.expected_connection_revision,
            &input.proposal_id,
        ),
        ops::Input::DirectSchemaChange(input) => (
            &input.connection_id,
            &input.expected_connection_revision,
            &input.proposal_id,
        ),
        _ => return Err(invalid()),
    })
}

fn scoped(
    context: &InvocationContext,
    input: &ops::Input,
) -> Result<InvocationContext, StorageError> {
    let scope = match input {
        ops::Input::ExecuteWrite(input) => input.scope.as_ref(),
        ops::Input::DirectSchemaChange(input) => input.scope.as_ref(),
        ops::Input::ApplyMigration(input) => input.scope.as_ref(),
        _ => return Err(invalid()),
    };
    super::executor::scope_context(context, scope)
}

impl DataExecutor {
    fn intent(
        &self,
        context: &InvocationContext,
        input: &ops::Input,
        stop: &(dyn Fn() -> bool + Sync),
    ) -> Result<Intent, StorageError> {
        match input {
            ops::Input::ExecuteWrite(input) => Ok(Intent {
                sql: input.statement.0.clone(),
                params: input.parameters.0.clone(),
                timeout_ms: input.timeout_ms.0.parse().map_err(|_| invalid())?,
                schema_digest: None,
                direct_reason: None,
                source: None,
            }),
            ops::Input::DirectSchemaChange(input) => {
                let reason = input
                    .fallback_reason
                    .as_ref()
                    .map(|reason| reason.0.trim().to_owned())
                    .filter(|reason| !reason.is_empty())
                    .ok_or_else(invalid)?;
                let digest = input.expected_schema_digest.clone().ok_or_else(invalid)?;
                Ok(Intent {
                    sql: input.statement.0.clone(),
                    params: input.parameters.0.clone(),
                    timeout_ms: input.timeout_ms.0.parse().map_err(|_| invalid())?,
                    schema_digest: Some(digest),
                    direct_reason: Some(reason),
                    source: None,
                })
            }
            ops::Input::ApplyMigration(input) => {
                let source = input.source.as_ref().ok_or_else(invalid)?;
                let git = self.git.as_ref().ok_or(StorageError::ScopeDenied)?;
                let (artifact, _) = self
                    .artifacts
                    .read_exact(self.store.as_ref(), context, &input.migration, 65_536)
                    .map_err(|_| StorageError::ScopeDenied)?;
                let versioned = git
                    .migration_file(context, source, stop)
                    .map_err(|_| StorageError::ScopeDenied)?;
                if versioned != artifact {
                    return Err(StorageError::ScopeDenied);
                }
                Ok(Intent {
                    sql: String::from_utf8(artifact).map_err(|_| invalid())?,
                    params: Vec::new(),
                    timeout_ms: 30_000,
                    schema_digest: Some(input.expected_schema_digest.clone()),
                    direct_reason: None,
                    source: Some(source.clone()),
                })
            }
            _ => Err(invalid()),
        }
    }

    pub(super) fn prepare_mutation(
        &self,
        context: &InvocationContext,
        command: &AdmittedCommand,
        env: &Preparation<'_>,
        input: &ops::Input,
    ) -> Result<Option<Prepared>, StorageError> {
        if command.schema_revision().0 < SCHEMA_EXECUTION_REVISION
            && matches!(
                input,
                ops::Input::ApplyMigration(_) | ops::Input::DirectSchemaChange(_)
            )
        {
            return Ok(None);
        }
        let context = &scoped(context, input)?;
        let operation = command.descriptor().name;
        let mode = mode(operation)?;
        let (connection, revision, proposal) = target(input)?;
        let selected = env.read(|snapshot| {
            if snapshot.receipts().lookup(command.identity())?.is_some() {
                return Ok(None);
            }
            super::check_fences(snapshot, context, command.authorization())?;
            Selected::current(snapshot, context, connection, revision.clone(), mode).map(Some)
        })?;
        let Some(selected) = selected else {
            return Ok(None);
        };
        let capabilities = self.adapter.capabilities(&selected.config.engine);
        if (mode == DataMode::Write && !capabilities.write)
            || (mode != DataMode::Write && !capabilities.schema_change)
        {
            return Err(StorageError::ScopeDenied);
        }
        let file_identity = self.file_identity(&selected.config)?;
        let material = match self.intent(context, input, &|| env.stopped(self.clock.now_ms().get()))
        {
            Ok(material) => material,
            Err(StorageError::ScopeDenied) => {
                return Ok(Some(Prepared::new(Refusal::new(
                    DomainErrorCode::PermissionDenied,
                    "database_mutation_material",
                    Some(connection.clone()),
                ))));
            }
            Err(StorageError::InvalidRecord(_)) => {
                return Ok(Some(Prepared::new(Refusal::new(
                    DomainErrorCode::InvalidArgument,
                    "database_mutation_material",
                    Some(connection.clone()),
                ))));
            }
            Err(error) => return Err(error),
        };
        let plan = statement::check(&selected.config.engine, mode, &material.sql)
            .map_err(|_| invalid())?;
        selected.check_schema_identity(&plan)?;
        parameters::order(&selected.config.engine, &plan[0].bindings, &material.params)
            .map_err(|_| invalid())?;
        let bytes = serde_json::to_vec(&material).map_err(|_| invalid())?;
        if bytes.len() as u64 > MAX_INTENT || env.stopped(self.clock.now_ms().get()) {
            return Err(StorageError::Cancelled);
        }
        let staged = self
            .artifacts
            .stage_host_bytes(
                self.store.as_ref(),
                &bytes,
                json!({"kind":"DatabaseMutationMaterial"}),
            )?
            .map_err(|_| StorageError::ViewUnavailable)?;
        Ok(Some(Prepared::new(StagedMutation {
            connection: connection.clone(),
            revision: revision.clone(),
            mode,
            file_identity,
            proposal: proposal.clone(),
            staged,
        })))
    }

    pub(super) fn decide_mutation(
        &self,
        tx: &mut dyn WriteTransaction,
        context: &InvocationContext,
        command: &AdmittedCommand,
    ) -> Result<DomainDecision, Failure> {
        let crate::operations::OperationInput::Database(input) = command.input() else {
            return Err(invalid().into());
        };
        if command.schema_revision().0 < SCHEMA_EXECUTION_REVISION
            && matches!(
                input,
                ops::Input::ApplyMigration(_) | ops::Input::DirectSchemaChange(_)
            )
        {
            return Err(Refusal::new(
                DomainErrorCode::SchemaMismatch,
                "database_schema_execution_revision_9",
                None,
            )
            .into());
        }
        let context = &scoped(context, input)?;
        if let Some(refused) = command.prepared::<Refusal>() {
            return Err(refused.clone().into());
        }
        let prepared = command.prepared::<StagedMutation>().ok_or_else(invalid)?;
        let source = data::transaction_source(
            tx,
            context,
            &prepared.connection,
            &prepared.revision,
            prepared.mode,
        )?;
        let current = Selected::from_source(
            tx.schemas(),
            source,
            prepared.revision.clone(),
            prepared.mode,
        )?;
        let binding = approved_binding(tx, context, command)?;
        if prepared.proposal.is_some()
            && context
                .reviewed_operation()
                .is_none_or(|approval| Some(approval.approval_id()) != prepared.proposal.as_ref())
        {
            return Err(
                Refusal::new(DomainErrorCode::ApprovalRequired, "database_mutation", None).into(),
            );
        }
        if let Some(approved) = binding
            && approved
                != approval_target(&current, prepared.file_identity, &prepared.staged.digest)
        {
            return Err(
                Refusal::new(DomainErrorCode::SourceChanged, "database_mutation", None).into(),
            );
        }

        let id = OperationId(tx.new_id(IdKind::Operation));
        let owner = tx.store().host_actor.clone();
        let material = self.artifacts.commit_host_bytes_in_scope(
            tx,
            &prepared.staged,
            HostArtifactPublication {
                title: "Database operation material",
                media_type: "application/json",
                provenance: json!({"purpose":"database_intent","operationId":id}),
                scope: context
                    .project_id()
                    .map_or(ArtifactScope::Workspace, |project| {
                        ArtifactScope::Project(project.clone())
                    }),
                owner,
            },
        )?;
        let delivery = tx.new_id(IdKind::QueueEntry);
        let event = EventId(tx.new_id(IdKind::Event));
        let controls = control::capture(tx, context)?;
        let (configuration, global_policy, source_policy) = current.revisions();
        let job = Job {
            kind: KIND.into(),
            connection: current.id.clone(),
            revision: current.revision,
            configuration,
            global_policy,
            source_policy,
            material: version_ref(&material),
            file_identity: prepared.file_identity,
            actor: context.principal().clone(),
            actor_kind: context.principal_kind().as_str().into(),
            store_id: context.store_id().clone(),
            restore_epoch: context.restore_epoch().get(),
            fences: command.authorization().fences().to_vec(),
            source_context: approvals::save_source_context(context),
            delivery_id: delivery.clone(),
            control_roots: controls.roots,
            assignment_generation: controls.assignment_generation,
        };
        tx.operations().record_intent(NewOperation {
            id: id.clone(),
            root_id: context.root_id().cloned(),
            assignment_id: context.assignment_id().cloned(),
            capability: command.descriptor().name.to_owned(),
            schema_version: 1,
            args_digest: command.fingerprint().as_str().to_owned(),
            payload: serde_json::to_value(&job).map_err(|_| invalid())?,
        })?;
        tx.history().append(NewEvent {
            id: event.clone(),
            actor: context.principal().clone(),
            project_id: context.project_id().cloned(),
            root_id: context.root_id().cloned(),
            operation_id: Some(id.clone()),
            aggregate_kind: "operation".into(),
            aggregate_id: id.0.as_str().to_owned(),
            aggregate_revision: 1,
            kind: "DatabaseMutationRequested".into(),
            payload: json!({"connectionId":current.id,"connectionRevision":job.revision}),
        })?;
        let ready = tx.now_ms();
        tx.queue().enqueue(NewQueueEntry {
            id: delivery,
            consumer: EFFECT_CONSUMER.into(),
            event_id: event,
            root_id: context.root_id().cloned(),
            operation_id: Some(id.clone()),
            partition: Some(format!("database:{}", job.connection.as_str())),
            priority: 0,
            ready_at_ms: ready,
            payload: json!({"kind": KIND}),
        })?;
        Ok(DomainDecision::Accepted { operation_id: id })
    }

    pub(super) fn settle_mutation(&self, prepared: &Prepared, env: &Preparation<'_>) {
        if let Some(prepared) = prepared.get::<StagedMutation>()
            && let Ok(staging) = env.read(|snapshot| {
                snapshot
                    .operations()
                    .operation(&prepared.staged.operation_id)
            })
        {
            self.artifacts.settle_host_bytes(
                self.store.as_ref(),
                &prepared.staged,
                staging.is_some_and(|record| record.state == OperationState::ObservedComplete),
            );
        }
    }

    fn run_mutation(
        &self,
        operation: &OperationRecord,
        cancellation: &Cancellation,
    ) -> Result<DataOutcome, DataFailure> {
        let job = job(operation).map_err(native_storage)?;
        let mode = mode(&operation.capability).map_err(native_storage)?;
        let (selected, context) =
            current(self.store.as_ref(), operation, &job, mode).map_err(native_storage)?;
        let bytes = self
            .artifacts
            .read_database_intent(
                self.store.as_ref(),
                &operation.id,
                &job.material,
                MAX_INTENT,
            )
            .map_err(native_storage)?;
        let input: Intent =
            serde_json::from_slice(&bytes).map_err(|_| native_storage(invalid()))?;
        if let Some(source) = input.source.as_ref() {
            let git = self
                .git
                .as_ref()
                .ok_or_else(|| native_storage(StorageError::ScopeDenied))?;
            let current = git
                .migration_file(&context, source, &|| cancellation.is_cancelled())
                .map_err(|_| native_storage(StorageError::ScopeDenied))?;
            if current != input.sql.as_bytes() {
                return Err(DataFailure::Refused(FailureKind::StaleSchema));
            }
        }
        let statements = statement::check(&selected.config.engine, mode, &input.sql)
            .map_err(|_| DataFailure::Refused(FailureKind::UnsupportedStatement))?;
        selected
            .check_schema_identity(&statements)
            .map_err(native_storage)?;
        let order = parameters::order(
            &selected.config.engine,
            &statements[0].bindings,
            &input.params,
        )
        .map_err(|_| DataFailure::Refused(FailureKind::UnsupportedValue))?;
        let parameters = self
            .parameters(&context, &order, &input.params)
            .map_err(native_storage)?;
        let limits = DataLimits {
            deadline: Instant::now()
                + Duration::from_millis(input.timeout_ms.clamp(1, 30_000) as u64),
            rows: 0,
            bytes: 0,
            columns: 0,
        };
        let total = statements.len();
        let schema_targets = statement::schema_targets(&statements);
        let mut request = self
            .request(
                &selected,
                statements,
                parameters,
                limits,
                cancellation.clone(),
            )
            .map_err(native_storage)?;
        if request.file_authority != job.file_identity {
            return Err(DataFailure::Refused(FailureKind::InvalidTarget));
        }
        request.expected_schema_digest = input.schema_digest.map(|value| value.as_str().to_owned());
        request.schema_targets = schema_targets;
        let guard = Arc::new(MutationGuard {
            store: Arc::clone(&self.store),
            job,
            operation: operation.clone(),
            selected,
            cancellation: cancellation.clone(),
            total,
        });
        self.reader
            .wait_for_effect(self.adapter.execute(request, guard))
    }
}

fn approval_target(selected: &Selected, file: Option<FileIdentity>, digest: &str) -> Value {
    let (configuration, global_policy, source_policy) = selected.revisions();
    json!({"connectionId":selected.id,"connectionRevision":selected.revision,
        "configuration":configuration,"globalPolicy":global_policy,"sourcePolicy":source_policy,
        "physicalFile":file,"intentDigest":digest})
}

fn job(operation: &OperationRecord) -> Result<Job, StorageError> {
    let job: Job = serde_json::from_value(operation.payload.clone()).map_err(|_| invalid())?;
    if operation.schema_version != 1 || job.kind != KIND {
        return Err(invalid());
    }
    Ok(job)
}

fn current(
    store: &dyn ApplicationStore,
    operation: &OperationRecord,
    job: &Job,
    mode: DataMode,
) -> Result<(Selected, InvocationContext), StorageError> {
    let (operation, job) = (operation.clone(), job.clone());
    run_write(store, WriteOptions::settlement(), move |tx| {
        effect_job::attempt_holds(tx, &operation, EFFECT_CONSUMER)?;
        effect_job::delivery_holds(tx, &job.delivery_id, EFFECT_CONSUMER, &operation.id)?;
        let (_, context) = effect_job::restore(tx, &job.binding(), &operation.id)?;
        let source = data::transaction_source(tx, &context, &job.connection, &job.revision, mode)?;
        let selected = Selected::from_source(tx.schemas(), source, job.revision, mode)?;
        if selected.revisions() != (job.configuration, job.global_policy, job.source_policy) {
            return Err(StorageError::ScopeDenied);
        }
        Ok((selected, context))
    })
}

struct MutationGuard {
    store: Arc<dyn ApplicationStore>,
    job: Job,
    operation: OperationRecord,
    selected: Selected,
    cancellation: Cancellation,
    total: usize,
}

impl MutationGuard {
    fn gate(&self) -> Result<(), DataFailure> {
        if self.cancellation.is_cancelled() {
            return Err(DataFailure::Refused(FailureKind::Cancelled));
        }
        current(
            self.store.as_ref(),
            &self.operation,
            &self.job,
            self.selected.mode,
        )
        .map(|_| ())
        .map_err(native_storage)
    }
}

impl DispatchGuard for MutationGuard {
    fn check_columns(&self, columns: &[PhysicalColumn]) -> Result<(), DataFailure> {
        self.selected.check_columns(columns)
    }

    fn before_statement(&self, _: usize) -> Result<(), DataFailure> {
        self.gate()
    }

    fn before_commit(&self) -> Result<(), DataFailure> {
        self.gate()
    }

    fn observed_statement(
        &self,
        position: usize,
        result: MutationResult,
    ) -> Result<(), DataFailure> {
        let operation = self.operation.clone();
        let job = self.job.clone();
        let complete = position.checked_add(1) == Some(self.total);
        run_write(self.store.as_ref(), WriteOptions::settlement(), move |tx| {
            effect_job::attempt_holds(tx, &operation, EFFECT_CONSUMER)?;
            let attempt = operation.latest_attempt().ok_or_else(invalid)?.attempt_no;
            tx.operations().observe(
                &operation.id,
                attempt,
                json!({
                    "kind":"DatabaseCommitObserved",
                    "completed":complete,
                    "position":position,
                    "affectedRows":result.affected_rows,
                    "observedStatements":result.observed_statements,
                    "schemaChanged":result.schema_changed
                }),
            )?;
            if complete && result.schema_changed {
                retire_schema(tx, &job)?;
            }
            Ok(())
        })
        .map_err(native_storage)
    }
}

fn retire_schema(tx: &mut dyn WriteTransaction, job: &Job) -> Result<(), StorageError> {
    if let Some(mut source) = tx.schemas().schema_source(&job.connection)?
        && source.configuration_revision == job.configuration
        && !source.deleted
        && source.connection.current_view_id.take().is_some()
    {
        let previous = source.connection.revision.get();
        source.connection.revision = source
            .connection
            .revision
            .checked_next()
            .ok_or_else(invalid)?;
        source.updated_at_ms = tx.now_ms();
        tx.schemas().revise_schema_source(&source, previous)?;
    }
    Ok(())
}

fn retire_after_partial(
    store: &dyn ApplicationStore,
    operation: &OperationRecord,
    job: &Job,
) -> Result<(), StorageError> {
    let (operation, job) = (operation.clone(), job.clone());
    run_write(store, WriteOptions::settlement(), move |tx| {
        effect_job::attempt_holds(tx, &operation, EFFECT_CONSUMER)?;
        retire_schema(tx, &job)
    })
}

fn completed(operation: &OperationRecord, job: &Job, affected: Option<i64>) -> EffectOutcome {
    let value = DbWriteResult {
        connection_id: job.connection.clone(),
        affected_rows: affected.map(counter),
        transaction_id: Some(operation.id.0.clone()),
        state: OperationState::ObservedComplete,
        diagnostics: BoundedList(Vec::new()),
    };
    let typed = match operation.capability.as_str() {
        "database.execute_write" => OperationResult::DatabaseExecuteWrite { value },
        "database.apply_migration" => OperationResult::DatabaseApplyMigration { value },
        "database.direct_schema_change" => OperationResult::DatabaseDirectSchemaChange { value },
        _ => return EffectOutcome::Failed("InvalidDatabaseOperation".into()),
    };
    EffectOutcome::Completed(json!({"result":typed}))
}

impl JournaledEffect for DataExecutor {
    fn execute(&self, operation: &OperationRecord) -> EffectOutcome {
        self.execute_cancellable(operation, &Cancellation::new())
    }

    fn execute_cancellable(
        &self,
        operation: &OperationRecord,
        cancellation: &Cancellation,
    ) -> EffectOutcome {
        let Ok(job) = job(operation) else {
            return EffectOutcome::Failed("InvalidDatabaseOperation".into());
        };
        match self.run_mutation(operation, cancellation) {
            Ok(DataOutcome::Mutation(result)) => completed(operation, &job, result.affected_rows),
            Ok(DataOutcome::Read(_)) => EffectOutcome::Failed("UnexpectedDatabaseRead".into()),
            Err(DataFailure::Refused(reason)) => {
                EffectOutcome::Failed(format!("DatabaseRefused{reason:?}"))
            }
            Err(DataFailure::RolledBack(reason)) => {
                EffectOutcome::Failed(format!("DatabaseRolledBack{reason:?}"))
            }
            Err(DataFailure::Partial { reason, known }) => {
                if known.schema_changed
                    && retire_after_partial(self.store.as_ref(), operation, &job).is_err()
                {
                    return EffectOutcome::Unknown("Known database changes need settlement".into());
                }
                EffectOutcome::Failed(format!(
                    "DatabasePartial{reason:?}: {} statements observed",
                    known.observed_statements
                ))
            }
            Err(DataFailure::Unknown { known }) => {
                if known.schema_changed {
                    let _ = retire_after_partial(self.store.as_ref(), operation, &job);
                }
                EffectOutcome::Unknown("Database commit requires an exact observation".into())
            }
        }
    }

    fn observe(&self, operation: &OperationRecord) -> EffectOutcome {
        let Ok(job) = job(operation) else {
            return EffectOutcome::Failed("InvalidDatabaseOperation".into());
        };
        let known_change = operation.latest_attempt().is_some_and(|attempt| {
            attempt.observations.iter().any(|observation| {
                observation["kind"] == "DatabaseCommitObserved"
                    && observation["schemaChanged"] == true
            })
        });
        if known_change && retire_after_partial(self.store.as_ref(), operation, &job).is_err() {
            return EffectOutcome::Unknown("Known database changes need settlement".into());
        }
        let observed = operation.latest_attempt().and_then(|attempt| {
            attempt.observations.iter().rev().find(|observation| {
                observation["kind"] == "DatabaseCommitObserved" && observation["completed"] == true
            })
        });
        match observed {
            Some(value) => completed(operation, &job, value["affectedRows"].as_i64()),
            None => EffectOutcome::Unknown("Database commit requires an exact observation".into()),
        }
    }
}

impl crate::services::approvals::SavedOperationOwners for DataExecutor {
    fn supports(&self, operation: &str) -> bool {
        mode(operation).is_ok()
    }

    fn prepare(
        &self,
        context: &InvocationContext,
        request: &crate::admission::DecodedRequest,
        env: &Preparation<'_>,
    ) -> Result<crate::services::approvals::OwnerPreview, Failure> {
        use crate::operations::OperationInput;
        let OperationInput::Database(input) = &request.input else {
            return Err(invalid().into());
        };
        let context = &scoped(context, input)?;
        let mode = mode(request.descriptor.name)?;
        let (connection, revision, _) = target(input)?;
        let selected = env.read(|snapshot| {
            Selected::current(snapshot, context, connection, revision.clone(), mode)
        })?;
        let capabilities = self.adapter.capabilities(&selected.config.engine);
        if (mode == DataMode::Write && !capabilities.write)
            || (mode != DataMode::Write && !capabilities.schema_change)
        {
            return Err(Refusal::new(
                DomainErrorCode::UnsupportedTarget,
                "database_mutation",
                Some(connection.clone()),
            )
            .into());
        }
        let file = self.file_identity(&selected.config)?;
        let intent = self.intent(context, input, &|| env.stopped(self.clock.now_ms().get()))?;
        let sha = intent_digest(&intent)?;
        Ok(crate::services::approvals::OwnerPreview {
            scope: context.project_id().map_or(ConfigScope::Workspace, |id| {
                ConfigScope::Project(id.clone())
            }),
            target_id: connection.clone(),
            target_revision: revision.get(),
            summary: format!("Review database changes on {}", selected.config.name.0),
            reviewed: json!({
                "connectionId":connection,
                "connectionRevision":revision,
                "engine":selected.config.engine,
                "statement":intent.sql,
                "parameterNames":intent.params.iter().map(|parameter| &parameter.name).collect::<Vec<_>>(),
                "schemaDigest":intent.schema_digest,
                "migrationSource":intent.source,
                "fallbackReason":intent.direct_reason,
            }),
            binding: approval_target(&selected, file, &sha),
        })
    }

    fn validate(
        &self,
        tx: &mut dyn WriteTransaction,
        context: &InvocationContext,
        request: &crate::admission::DecodedRequest,
        preview: &crate::services::approvals::OwnerPreview,
    ) -> Result<(), Failure> {
        use crate::operations::OperationInput;
        let OperationInput::Database(input) = &request.input else {
            return Err(invalid().into());
        };
        let context = &scoped(context, input)?;
        let mode = mode(request.descriptor.name)?;
        let (connection, revision, _) = target(input)?;
        let source = data::transaction_source(tx, context, connection, revision, mode)?;
        let selected = Selected::from_source(tx.schemas(), source, revision.clone(), mode)?;
        let file = serde_json::from_value(preview.binding["physicalFile"].clone())
            .map_err(|_| invalid())?;
        let digest = preview.binding["intentDigest"]
            .as_str()
            .ok_or_else(invalid)?;
        if preview.binding != approval_target(&selected, file, digest) {
            return Err(Refusal::new(
                DomainErrorCode::SourceChanged,
                "database_mutation",
                Some(connection.clone()),
            )
            .into());
        }
        Ok(())
    }
}

fn intent_digest(intent: &Intent) -> Result<String, StorageError> {
    let bytes = serde_json::to_vec(intent).map_err(|_| invalid())?;
    Ok(crate::fingerprint::sha256_hex(&bytes))
}
