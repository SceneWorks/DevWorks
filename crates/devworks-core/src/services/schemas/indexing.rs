//! Durable metadata capture. Target I/O is outside the writer; every local phase rechecks
//! the captured source, authority and operation attempt before publishing an immutable view.
use super::{SchemaService, access};
use crate::{
    context::InvocationContext,
    gateway::AdmittedCommand,
    ports::{AuthorityFence, IdKind, database::*, storage::*},
    protocol::{
        ActorId, Check, EventId, Id, OperationId, OperationState, RootId, StoreId, data::DataScope,
        records::*, results::DomainDecision,
    },
    services::{
        approvals,
        artifacts::ArtifactService,
        conventions::control,
        sessions::{Failure, Refusal},
    },
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};

pub const EFFECT_CONSUMER: &str = "schema_metadata";
pub const JOB_KIND: &str = "SchemaMetadataV1";
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SchemaJob {
    pub kind: String,
    pub connection_id: Id,
    pub source_revision: i64,
    pub scope: DataScope,
    pub test_only: bool,
    pub source_context: Value,
    pub actor: ActorId,
    pub actor_kind: String,
    pub store_id: StoreId,
    pub restore_epoch: i64,
    pub fences: Vec<AuthorityFence>,
    pub delivery_id: Id,
    pub control_roots: Vec<(RootId, i64)>,
    pub assignment_generation: Option<i64>,
}
pub(super) struct Runtime {
    pub store: Arc<dyn ApplicationStore>,
    reader: Arc<dyn MetadataReader>,
    artifacts: Arc<ArtifactService>,
    closed: Arc<crate::services::conventions::indexing::lifecycle::PublicationFence>,
    active: Mutex<BTreeMap<String, Cancellation>>,
}
#[derive(Debug)]
pub enum SchemaOutcome {
    Completed(Value),
    Failed(String),
}
fn invalid() -> StorageError {
    StorageError::InvalidRecord("invalid schema metadata journal".into())
}
fn identity(prefix: &str, key: &str) -> Id {
    Id::new(format!(
        "{prefix}{}",
        crate::fingerprint::sha256_hex(key.as_bytes())
    ))
    .expect("digest identifier")
}
impl SchemaService {
    pub fn with_runtime(
        mut self,
        store: Arc<dyn ApplicationStore>,
        reader: Arc<dyn MetadataReader>,
        artifacts: Arc<ArtifactService>,
    ) -> Self {
        self.runtime = Some(Arc::new(Runtime {
            store,
            reader,
            artifacts,
            closed: Arc::default(),
            active: Mutex::default(),
        }));
        self
    }
    pub fn is_stopped(&self) -> bool {
        self.runtime.as_ref().is_none_or(|r| r.closed.is_closed())
    }
    pub fn stop(&self) {
        if let Some(r) = &self.runtime {
            r.closed.begin_close();
            for token in r.active.lock().unwrap_or_else(|p| p.into_inner()).values() {
                token.cancel();
            }
            r.closed.drain();
        }
    }
    pub(super) fn decide_index(
        &self,
        tx: &mut dyn WriteTransaction,
        ctx: &InvocationContext,
        command: &AdmittedCommand,
        id: &Id,
        expected: &crate::protocol::Revision,
        test_only: bool,
    ) -> Result<DomainDecision, Failure> {
        if self.is_stopped() {
            return Err(StorageError::Cancelled.into());
        }
        let scope = ctx
            .project_id()
            .map_or(DataScope::Workspace, |p| DataScope::Project {
                project_id: p.clone(),
            });
        let reach = crate::services::knowledge::reach(tx.authority(), ctx)?;
        crate::services::knowledge::resolve_scope_with(&reach, ctx, &scope, true, |p| {
            tx.artifacts().project_exists(p)
        })?;
        let mut source = tx.schemas().schema_source(id)?.ok_or_else(|| {
            Refusal::new(
                crate::protocol::DomainErrorCode::NotFound,
                "schema_connection",
                Some(id.clone()),
            )
        })?;
        let visibility = SchemaVisibility {
            actor_id: ctx.principal().clone(),
            workspace: ctx.project_id().is_none(),
            project_id: ctx.project_id().map(|p| p.0.clone()),
            exposed_only: !matches!(
                ctx.principal_kind(),
                crate::context::PrincipalKind::Human | crate::context::PrincipalKind::Host
            ),
        };
        if !access::sees(&source, &visibility)
            || source
                .connection
                .config
                .as_ref()
                .is_none_or(|c| c.enabled == Some(false))
        {
            return Err(StorageError::ScopeDenied.into());
        }
        if &source.connection.revision != expected {
            return Err(StorageError::conflict(
                crate::protocol::RevisionDimension::Connection,
                "schema_connection",
                id.clone(),
                expected.clone(),
                source.connection.revision,
            )
            .into());
        }
        if source.connection.active_operation_id.is_some() {
            return Err(StorageError::Busy.into());
        }
        let op = OperationId(tx.new_id(IdKind::Operation));
        let delivery = tx.new_id(IdKind::QueueEntry);
        let event = EventId(tx.new_id(IdKind::Event));
        let controls = control::capture(tx, ctx)?;
        let revision = expected.checked_next().ok_or_else(invalid)?;
        let job = SchemaJob {
            kind: JOB_KIND.into(),
            connection_id: id.clone(),
            source_revision: revision.get(),
            scope,
            test_only,
            source_context: approvals::save_source_context(ctx),
            actor: ctx.principal().clone(),
            actor_kind: format!("{:?}", ctx.principal_kind()),
            store_id: ctx.store_id().clone(),
            restore_epoch: ctx.restore_epoch().get(),
            fences: command.authorization().fences().to_vec(),
            delivery_id: delivery.clone(),
            control_roots: controls.roots,
            assignment_generation: controls.assignment_generation,
        };
        source.connection.revision = revision;
        source.connection.active_operation_id = Some(op.0.clone());
        source.updated_at_ms = tx.now_ms();
        source.connection.last_error = None;
        tx.schemas().revise_schema_source(&source, expected.get())?;
        tx.operations().record_intent(NewOperation {
            id: op.clone(),
            root_id: ctx.root_id().cloned(),
            assignment_id: ctx.assignment_id().cloned(),
            capability: command.input().operation().into(),
            schema_version: 1,
            args_digest: command.fingerprint().as_str().into(),
            payload: serde_json::to_value(&job).map_err(|_| invalid())?,
        })?;
        tx.history().append(NewEvent {
            id: event.clone(),
            actor: ctx.principal().clone(),
            project_id: ctx.project_id().cloned(),
            root_id: ctx.root_id().cloned(),
            operation_id: Some(op.clone()),
            aggregate_kind: "schema_connection".into(),
            aggregate_id: id.as_str().into(),
            aggregate_revision: job.source_revision,
            kind: "SchemaMetadataRequested".into(),
            payload: json!({"connectionId":id,"testOnly":test_only,"metadataOnly":true}),
        })?;
        let ready = tx.now_ms();
        tx.queue().enqueue(NewQueueEntry {
            id: delivery,
            consumer: EFFECT_CONSUMER.into(),
            event_id: event,
            root_id: ctx.root_id().cloned(),
            operation_id: Some(op.clone()),
            partition: None,
            priority: 0,
            ready_at_ms: ready,
            payload: json!({"kind":JOB_KIND}),
        })?;
        Ok(DomainDecision::Accepted { operation_id: op })
    }
    fn metadata_write<T: Send + 'static>(
        &self,
        op: &OperationRecord,
        job: &SchemaJob,
        cancel: &Cancellation,
        work: impl FnOnce(
            &mut dyn WriteTransaction,
            StoredSchemaSource,
            InvocationContext,
        ) -> Result<T, StorageError>
        + Send
        + 'static,
    ) -> Result<T, StorageError> {
        let runtime = self.runtime.as_ref().ok_or(StorageError::Cancelled)?;
        let _phase = runtime.closed.enter()?;
        let (op, job, closed, token) = (
            op.clone(),
            job.clone(),
            runtime.closed.clone(),
            cancel.clone(),
        );
        run_write(
            runtime.store.as_ref(),
            WriteOptions {
                cancellation: Some(cancel.clone()),
                ..Default::default()
            },
            move |tx| {
                if closed.is_closed() || token.is_cancelled() {
                    return Err(StorageError::Cancelled);
                }
                let (source, ctx) = current(tx, &op, &job)?;
                work(tx, source, ctx)
            },
        )
    }
    /// Settle only this attempt's source status. This never publishes metadata or grants
    /// authority, and can therefore clear cancelled/revoked work without replaying its read.
    pub fn fail_metadata(
        &self,
        op: &OperationRecord,
        reason: MetadataFailure,
    ) -> Result<(), StorageError> {
        let runtime = self.runtime.as_ref().ok_or(StorageError::Cancelled)?;
        let _phase = runtime.closed.enter()?;
        let closed = runtime.closed.clone();
        let op = op.clone();
        let job: SchemaJob = serde_json::from_value(op.payload.clone()).map_err(|_| invalid())?;
        run_write(runtime.store.as_ref(), WriteOptions::default(), move |tx| {
            if closed.is_closed() {
                return Err(StorageError::Cancelled);
            }
            if tx.store().store_id != job.store_id || tx.store().restore_epoch != job.restore_epoch
            {
                return Err(StorageError::ScopeDenied);
            }
            if tx.schemas().schema_publication(&op.id)?.is_some() {
                return Ok(());
            }
            let actual = tx.operations().operation(&op.id)?.ok_or_else(invalid)?;
            if actual.payload != op.payload
                || actual.latest_attempt().map(|a| a.fence())
                    != op.latest_attempt().map(|a| a.fence())
            {
                return Err(StorageError::LeaseLost {
                    kind: "schema_metadata",
                    id: op.id.0.as_str().into(),
                });
            }
            let Some(mut source) = tx.schemas().schema_source(&job.connection_id)? else {
                return Ok(());
            };
            if source.connection.active_operation_id.as_ref() != Some(&op.id.0) {
                return Ok(());
            }
            let expected = source.connection.revision.get();
            source.connection.revision = source
                .connection
                .revision
                .checked_next()
                .ok_or_else(invalid)?;
            source.connection.active_operation_id = None;
            source.connection.last_error = Some(crate::protocol::support::BoundedText(format!(
                "Metadata{reason:?}"
            )));
            source.updated_at_ms = tx.now_ms();
            tx.schemas().revise_schema_source(&source, expected)
        })
    }
    /// Observing repeats only metadata reads, never caller SQL or source writes. The immutable
    /// publication witness is checked before current-source eligibility, including after edits.
    pub async fn run_metadata(
        &self,
        op: &OperationRecord,
        cancel: &Cancellation,
    ) -> Result<SchemaOutcome, StorageError> {
        let runtime = self.runtime.as_ref().ok_or(StorageError::Cancelled)?;
        let job: SchemaJob = serde_json::from_value(op.payload.clone()).map_err(|_| invalid())?;
        if job.kind != JOB_KIND
            || op.schema_version != 1
            || !matches!(op.capability.as_str(), "schemas.index" | "schemas.refresh")
        {
            return Err(invalid());
        }
        let completed = run_write(runtime.store.as_ref(), WriteOptions::default(), {
            let id = op.id.clone();
            move |tx| tx.schemas().schema_publication(&id)
        })?;
        if let Some(done) = completed {
            return Ok(SchemaOutcome::Completed(json!(done.result)));
        }
        {
            let mut active = runtime.active.lock().unwrap_or_else(|p| p.into_inner());
            if runtime.closed.is_closed() || cancel.is_cancelled() {
                return Err(StorageError::Cancelled);
            }
            if active.contains_key(op.id.0.as_str()) {
                return Err(StorageError::Busy);
            }
            active.insert(op.id.0.as_str().to_owned(), cancel.clone());
        }
        let _active = Active {
            runtime,
            id: op.id.0.as_str().to_owned(),
        };
        let (source, ctx) =
            self.metadata_write(op, &job, cancel, |_, source, ctx| Ok((source, ctx)))?;
        let config = source.connection.config.clone().ok_or_else(invalid)?;
        let password = if let Some(handle) = &config.credential_ref {
            let owner = self.credentials.as_ref().ok_or(StorageError::ScopeDenied)?;
            owner
                .password(handle)
                .map_err(|_| StorageError::ScopeDenied)?
        } else {
            None
        };
        let ca_certificate = config
            .ca_certificate
            .as_ref()
            .map(|reference| {
                runtime
                    .artifacts
                    .read_exact(runtime.store.as_ref(), &ctx, reference, 1024 * 1024)
                    .map(|v| v.0)
                    .map_err(|_| StorageError::ScopeDenied)
            })
            .transpose()?;
        let request = MetadataRequest {
            connection_id: job.connection_id.clone(),
            config,
            password,
            ca_certificate,
            test_only: job.test_only,
            limits: MetadataLimits::default(),
            cancellation: cancel.clone(),
        };
        let snapshot = match runtime.reader.read(request).await {
            Ok(value) => value,
            Err(failure) => {
                let reason = format!("Metadata{failure:?}");
                self.fail_metadata(op, failure)?;
                return Ok(SchemaOutcome::Failed(reason));
            }
        };
        snapshot.profile.check().map_err(|_| invalid())?;
        snapshot.coverage.check().map_err(|_| invalid())?;
        if snapshot.objects.len() > 100_000 || snapshot.relationships.len() > 500_000 {
            return Err(invalid());
        }
        let view = self.ids.next_id(IdKind::View);
        let key = |k: &str| {
            identity(
                "schema_object_",
                &format!("{}:{k}", job.connection_id.as_str()),
            )
        };
        let ids: BTreeMap<_, _> = snapshot
            .objects
            .iter()
            .map(|o| (o.key.clone(), key(&o.key)))
            .collect();
        if ids.len() != snapshot.objects.len() {
            return Err(invalid());
        }
        let mut objects = Vec::with_capacity(snapshot.objects.len());
        for o in snapshot.objects {
            let object = SchemaObject {
                id: ids[&o.key].clone(),
                view_id: view.clone(),
                kind: serde_json::from_value(json!(o.kind)).map_err(|_| invalid())?,
                qualified_name: crate::protocol::support::BoundedText(o.qualified_name),
                definition: None,
                parent_id: o
                    .parent_key
                    .map(|p| ids.get(&p).cloned().ok_or_else(invalid))
                    .transpose()?,
                coverage: o.coverage,
                metadata: Some(o.metadata),
            };
            object.check().map_err(|_| invalid())?;
            objects.push(object);
        }
        let mut relations = Vec::with_capacity(snapshot.relationships.len());
        for r in snapshot.relationships {
            let row = SchemaRelation {
                id: identity(
                    "schema_relation_",
                    &format!("{}:{}", job.connection_id.as_str(), r.key),
                ),
                view_id: view.clone(),
                from_object_id: ids.get(&r.from_key).cloned().ok_or_else(invalid)?,
                to_object_id: ids.get(&r.to_key).cloned().ok_or_else(invalid)?,
                relation: crate::protocol::support::BoundedText(r.kind),
                inferred: r.inferred,
            };
            row.check().map_err(|_| invalid())?;
            relations.push(row);
        }
        objects.sort_by(|a, b| a.id.as_str().cmp(b.id.as_str()));
        relations.sort_by(|a, b| a.id.as_str().cmp(b.id.as_str()));
        let bytes =
            serde_json::to_vec(&(&objects, &relations, &snapshot.profile, &snapshot.coverage))
                .map_err(|_| invalid())?;
        if bytes.len() > 64 * 1024 * 1024 {
            return Err(invalid());
        }
        let digest = crate::fingerprint::sha256_hex(&bytes);
        drop(bytes);
        if !job.test_only {
            loop {
                let connection = job.connection_id.clone();
                if self.metadata_write(op, &job, cancel, move |tx, _, _| {
                    tx.schemas().discard_schema_staging(&connection, 500)
                })? {
                    break;
                }
            }
            let header = StoredSchemaView {
                id: view.clone(),
                connection_id: job.connection_id.clone(),
                source_revision: job.source_revision,
                operation_id: op.id.clone(),
                digest,
                sealed: false,
                created_at_ms: self.clock.now_ms().get(),
            };
            self.metadata_write(op, &job, cancel, move |tx, _, _| {
                tx.schemas().create_schema_view(&header)
            })?;
            for chunk in objects.chunks(500) {
                let (view, rows) = (view.clone(), chunk.to_vec());
                self.metadata_write(op, &job, cancel, move |tx, _, _| {
                    tx.schemas().append_schema_rows(&view, &rows, &[])
                })?;
            }
            for chunk in relations.chunks(500) {
                let (view, rows) = (view.clone(), chunk.to_vec());
                self.metadata_write(op, &job, cancel, move |tx, _, _| {
                    tx.schemas().append_schema_rows(&view, &[], &rows)
                })?;
            }
        }
        let total = objects.len();
        let (operation, job2) = (op.clone(), job.clone());
        self.metadata_write(op,&job,cancel,move|tx,mut source,_|{
            // Re-reading the witness inside the serialized final writer prevents an uncertain
            // acknowledgement from manufacturing a second publication.
            if let Some(done)=tx.schemas().schema_publication(&operation.id)?{return Ok(SchemaOutcome::Completed(json!(done.result)));}
            let expected=source.connection.revision.get();
            if !job2.test_only {tx.schemas().seal_schema_view(&view)?;source.connection.current_view_id=Some(view.clone());source.connection.last_synced_at_ms=Some(crate::protocol::Counter::new(tx.now_ms()).map_err(|_|invalid())?);source.connection.coverage=Some(snapshot.coverage);}
            source.connection.profile=Some(snapshot.profile);source.connection.last_error=None;source.connection.active_operation_id=None;source.connection.revision=source.connection.revision.checked_next().ok_or_else(invalid)?;source.updated_at_ms=tx.now_ms();
            tx.schemas().revise_schema_source(&source,expected)?;
            let result:IndexRunRecord=serde_json::from_value(json!({"id":operation.id.0,"revision":"1","repositoryIds":[],"stage":if job2.test_only{"ConnectionTest"}else{"MetadataPublished"},"state":"ObservedComplete","completedUnits":total.to_string(),"totalUnits":total.to_string(),"outputViews":if job2.test_only{vec![]}else{vec![view]},"diagnostics":[]})).map_err(|_|invalid())?;
            result.check().map_err(|_|invalid())?;
            let published_at=tx.now_ms();
            tx.schemas().record_schema_publication(&StoredSchemaPublication{operation_id:operation.id,connection_id:job2.connection_id,result:result.clone(),created_at_ms:published_at})?;
            Ok(SchemaOutcome::Completed(json!(result)))
        })
    }
}
struct Active<'a> {
    runtime: &'a Runtime,
    id: String,
}
impl Drop for Active<'_> {
    fn drop(&mut self) {
        self.runtime
            .active
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(&self.id);
    }
}
fn current(
    tx: &mut dyn WriteTransaction,
    op: &OperationRecord,
    job: &SchemaJob,
) -> Result<(StoredSchemaSource, InvocationContext), StorageError> {
    if tx.store().store_id != job.store_id
        || tx.store().restore_epoch != job.restore_epoch
        || !crate::authority::fences_hold(tx, &job.fences)?
    {
        return Err(StorageError::ScopeDenied);
    }
    let kind = tx
        .actors()
        .actor_kind(&job.actor)?
        .ok_or(StorageError::ScopeDenied)?;
    if format!("{kind:?}") != job.actor_kind {
        return Err(StorageError::ScopeDenied);
    }
    let ctx = approvals::restore_source_context(tx.store(), &job.actor, kind, &job.source_context)?;
    control::check(
        tx,
        &ctx,
        &op.id,
        &job.control_roots,
        job.assignment_generation,
    )?;
    let actual = tx.operations().operation(&op.id)?.ok_or_else(invalid)?;
    if actual.payload != op.payload
        || actual.latest_attempt().map(|a| a.fence()) != op.latest_attempt().map(|a| a.fence())
        || !matches!(
            actual.state,
            OperationState::Dispatching | OperationState::OutcomeUnknown
        )
    {
        return Err(StorageError::LeaseLost {
            kind: "schema_metadata",
            id: op.id.0.as_str().into(),
        });
    }
    let entry = tx.queue().entry(&job.delivery_id)?.ok_or_else(invalid)?;
    if entry.consumer != EFFECT_CONSUMER || entry.operation_id.as_ref() != Some(&op.id) {
        return Err(StorageError::ScopeDenied);
    }
    if entry.cancel_requested {
        return Err(StorageError::Cancelled);
    }
    let source = tx
        .schemas()
        .schema_source(&job.connection_id)?
        .ok_or(StorageError::ScopeDenied)?;
    let visibility = SchemaVisibility {
        actor_id: ctx.principal().clone(),
        workspace: ctx.project_id().is_none(),
        project_id: ctx.project_id().map(|p| p.0.clone()),
        exposed_only: !matches!(
            kind,
            crate::context::PrincipalKind::Human | crate::context::PrincipalKind::Host
        ),
    };
    let reach = crate::services::knowledge::reach(tx.authority(), &ctx)
        .map_err(|_| StorageError::ScopeDenied)?;
    crate::services::knowledge::resolve_scope_with(&reach, &ctx, &job.scope, true, |p| {
        tx.artifacts().project_exists(p)
    })
    .map_err(|_| StorageError::ScopeDenied)?;
    if !access::sees(&source, &visibility)
        || source.connection.revision.get() != job.source_revision
        || source.connection.active_operation_id.as_ref() != Some(&op.id.0)
        || source
            .connection
            .config
            .as_ref()
            .is_none_or(|c| c.enabled == Some(false))
    {
        return Err(StorageError::ScopeDenied);
    }
    Ok((source, ctx))
}

/// An owner may revoke or remove a source while target I/O is pending. Queue cancellation and
/// the source revision change are in the same writer; the old worker cannot publish afterward.
pub(super) fn cancel_active(
    tx: &mut dyn WriteTransaction,
    source: &StoredSchemaSource,
) -> Result<(), StorageError> {
    let Some(id) = source.connection.active_operation_id.as_ref() else {
        return Ok(());
    };
    let operation = tx
        .operations()
        .operation(&OperationId(id.clone()))?
        .ok_or_else(invalid)?;
    if is_terminal(operation.state) {
        return Ok(());
    }
    let job: SchemaJob = serde_json::from_value(operation.payload).map_err(|_| invalid())?;
    if job.kind != JOB_KIND || job.connection_id != source.connection.id {
        return Err(StorageError::ScopeDenied);
    }
    let entry = tx.queue().entry(&job.delivery_id)?.ok_or_else(invalid)?;
    if entry.consumer != EFFECT_CONSUMER
        || entry.operation_id.as_ref() != Some(&OperationId(id.clone()))
    {
        return Err(StorageError::ScopeDenied);
    }
    if !entry.cancel_requested {
        tx.queue().cancel(&job.delivery_id)?;
    }
    Ok(())
}
