use super::statement::{CheckedStatement, DataMode};
use super::{native_storage, sensitive};
use crate::authority;
use crate::authority::destructive::{EntryKind, FilesystemResolver};
use crate::context::InvocationContext;
use crate::ports::database::data::{
    DataAdapter, DataFailure, DataFuture, DataLimits, DataOutcome, DataRequest, DispatchGuard,
    FailureKind, MutationResult, PhysicalColumn, Scalar,
};
use crate::ports::storage::{
    ApplicationStore, Cancellation, ReadOptions, ReadSnapshot, SensitivePolicy, StorageError,
    read_snapshot,
};
use crate::ports::{Authorization, Clock};
use crate::protocol::records::{ConnectionConfig, DbEngine, DbParameter, DbScalar};
use crate::protocol::{Id, Revision};
use crate::services::artifacts::ArtifactService;
use crate::services::schemas::{SchemaService, data};
use std::sync::Arc;

/// The host drives native async adapters outside the gateway snapshot, using its runtime.
pub trait DatabaseReadRunner: Send + Sync {
    fn wait(&self, action: DataFuture) -> Result<DataOutcome, DataFailure>;
    /// The effect dispatcher calls this only from its blocking pool.
    fn wait_for_effect(&self, action: DataFuture) -> Result<DataOutcome, DataFailure> {
        self.wait(action)
    }
}

pub struct DataExecutor {
    pub(super) store: Arc<dyn ApplicationStore>,
    pub(super) clock: Arc<dyn Clock>,
    pub(super) adapter: Arc<dyn DataAdapter>,
    pub(super) reader: Arc<dyn DatabaseReadRunner>,
    schemas: Arc<SchemaService>,
    pub(super) git: Option<Arc<crate::services::git::GitService>>,
    pub(super) artifacts: Arc<ArtifactService>,
    filesystem: Arc<dyn FilesystemResolver>,
}

impl DataExecutor {
    pub fn new(
        store: Arc<dyn ApplicationStore>,
        clock: Arc<dyn Clock>,
        adapter: Arc<dyn DataAdapter>,
        reader: Arc<dyn DatabaseReadRunner>,
        schemas: Arc<SchemaService>,
        artifacts: Arc<ArtifactService>,
        filesystem: Arc<dyn FilesystemResolver>,
    ) -> Self {
        Self {
            store,
            clock,
            adapter,
            reader,
            schemas,
            git: None,
            artifacts,
            filesystem,
        }
    }

    pub fn with_git(mut self, git: Arc<crate::services::git::GitService>) -> Self {
        self.git = Some(git);
        self
    }

    pub(super) fn parameters(
        &self,
        context: &InvocationContext,
        order: &[usize],
        values: &[DbParameter],
    ) -> Result<Vec<Scalar>, StorageError> {
        let mut size = 0usize;
        order
            .iter()
            .map(|&index| {
                let value = &values[index].value;
                size = size.saturating_add(match value {
                    DbScalar::Text { value } => value.0.len(),
                    DbScalar::Decimal { value } => value.0.len(),
                    _ => 32,
                });
                let value = match value {
                    DbScalar::Null {} => Scalar::Null,
                    DbScalar::Boolean { value } => Scalar::Boolean(*value),
                    DbScalar::Integer { value } => {
                        Scalar::Integer(value.0.parse().map_err(|_| {
                            StorageError::InvalidRecord("invalid database integer".into())
                        })?)
                    }
                    DbScalar::Decimal { value } => Scalar::Decimal(value.0.clone()),
                    DbScalar::Text { value } => Scalar::Text(value.0.clone()),
                    DbScalar::Bytes { value } => {
                        let (bytes, _) = self
                            .artifacts
                            .read_exact(self.store.as_ref(), context, value, 1024 * 1024)
                            .map_err(|_| StorageError::ScopeDenied)?;
                        size = size.saturating_add(bytes.len());
                        Scalar::Bytes(bytes)
                    }
                };
                if size > 1024 * 1024 {
                    return Err(StorageError::InvalidRecord(
                        "database parameter bytes exceed 1 MiB".into(),
                    ));
                }
                Ok(value)
            })
            .collect()
    }

    pub(super) fn file_identity(
        &self,
        config: &ConnectionConfig,
    ) -> Result<Option<crate::authority::destructive::FileIdentity>, StorageError> {
        if matches!(config.engine, DbEngine::Sqlite) {
            let path = self
                .filesystem
                .resolve(config.endpoint.0.as_str(), None)
                .map_err(|_| StorageError::ScopeDenied)?;
            if path.entry_kind != EntryKind::File || path.parent_traversal {
                return Err(StorageError::ScopeDenied);
            }
            Ok(Some(path.entry.identity.ok_or(StorageError::ScopeDenied)?))
        } else {
            Ok(None)
        }
    }

    pub(super) fn request(
        &self,
        selected: &Selected,
        statements: Vec<CheckedStatement>,
        parameters: Vec<Scalar>,
        limits: DataLimits,
        cancellation: Cancellation,
    ) -> Result<DataRequest, StorageError> {
        let config = &selected.config;
        let file_authority = self.file_identity(config)?;
        Ok(DataRequest {
            id: selected.id.clone(),
            mode: selected.mode,
            config: config.clone(),
            password: self
                .schemas
                .target_password(config.credential_ref.as_ref())?,
            ca_certificate: selected.certificate.clone(),
            statements,
            parameters,
            expected_schema_digest: None,
            include_schema_digest: false,
            limits,
            cancellation,
            file_authority,
        })
    }

    pub(super) fn read_guard(
        &self,
        context: InvocationContext,
        authorization: Authorization,
        selected: Arc<Selected>,
    ) -> ReadGuard {
        ReadGuard {
            store: Arc::clone(&self.store),
            context,
            authorization,
            selected,
        }
    }
}

pub(super) struct Selected {
    pub(super) id: Id,
    pub(super) revision: Revision,
    pub(super) mode: DataMode,
    pub(super) config: ConnectionConfig,
    configuration: i64,
    certificate: Option<Vec<u8>>,
    global: SensitivePolicy,
    local: SensitivePolicy,
}

impl Selected {
    pub(super) fn current(
        snapshot: &dyn ReadSnapshot,
        context: &InvocationContext,
        id: &Id,
        revision: Revision,
        mode: DataMode,
    ) -> Result<Self, StorageError> {
        let source = data::source(snapshot, context, id, &revision, mode)?;
        Self::from_source(snapshot.schemas(), source, revision, mode)
    }

    pub(super) fn from_source(
        schemas: &dyn crate::ports::storage::SchemaRead,
        source: crate::ports::storage::StoredSchemaSource,
        revision: Revision,
        mode: DataMode,
    ) -> Result<Self, StorageError> {
        let id = source.connection.id.clone();
        let (global, local) = data::sensitive_policy(schemas, &source)?;
        let certificate = data::certificate(schemas, &source)?;
        let config = source.connection.config.ok_or(StorageError::ScopeDenied)?;
        Ok(Self {
            id: id.clone(),
            revision,
            mode,
            config,
            configuration: source.configuration_revision,
            certificate,
            global,
            local,
        })
    }

    pub(super) fn still_current(
        &self,
        snapshot: &dyn ReadSnapshot,
        context: &InvocationContext,
    ) -> Result<(), StorageError> {
        let source = data::source(snapshot, context, &self.id, &self.revision, self.mode)?;
        let (global, local) = data::sensitive_policy(snapshot.schemas(), &source)?;
        if source.configuration_revision != self.configuration
            || global.revision != self.global.revision
            || local.revision != self.local.revision
        {
            return Err(StorageError::ScopeDenied);
        }
        Ok(())
    }

    pub(super) fn revisions(&self) -> (i64, i64, i64) {
        (
            self.configuration,
            self.global.revision,
            self.local.revision,
        )
    }

    pub(super) fn check_columns(&self, columns: &[PhysicalColumn]) -> Result<(), DataFailure> {
        sensitive::enforce(&self.global, &self.local, columns)
    }
}

pub(super) fn scope_context(
    context: &InvocationContext,
    scope: Option<&crate::protocol::data::DataScope>,
) -> Result<InvocationContext, StorageError> {
    use crate::protocol::data::DataScope;
    match scope {
        Some(DataScope::Workspace) if context.project_id().is_some() => {
            Err(StorageError::ScopeDenied)
        }
        Some(DataScope::Project { project_id })
            if context
                .project_id()
                .is_some_and(|current| current != project_id) =>
        {
            Err(StorageError::ScopeDenied)
        }
        Some(DataScope::Project { project_id }) => Ok(context.in_project(project_id.clone())),
        _ => Ok(context.clone()),
    }
}

pub(super) fn check_fences(
    read: &dyn ReadSnapshot,
    context: &InvocationContext,
    authorization: &Authorization,
) -> Result<(), StorageError> {
    let stamp = read.stamp();
    if &stamp.store_id != context.store_id()
        || stamp.restore_epoch != context.restore_epoch().get()
        || !authority::authorization_holds(read, authorization)?
    {
        return Err(StorageError::ScopeDenied);
    }
    Ok(())
}

pub(super) struct ReadGuard {
    store: Arc<dyn ApplicationStore>,
    context: InvocationContext,
    authorization: Authorization,
    selected: Arc<Selected>,
}

impl DispatchGuard for ReadGuard {
    fn check_columns(&self, columns: &[PhysicalColumn]) -> Result<(), DataFailure> {
        self.selected.check_columns(columns)
    }

    fn before_statement(&self, _: usize) -> Result<(), DataFailure> {
        read_snapshot(self.store.as_ref(), &ReadOptions::default(), |snapshot| {
            check_fences(snapshot, &self.context, &self.authorization)?;
            self.selected.still_current(snapshot, &self.context)
        })
        .map_err(native_storage)
    }

    fn before_commit(&self) -> Result<(), DataFailure> {
        Err(DataFailure::Refused(FailureKind::PermissionDenied))
    }

    fn observed_statement(&self, _: usize, _: MutationResult) -> Result<(), DataFailure> {
        Err(DataFailure::Refused(FailureKind::PermissionDenied))
    }
}
