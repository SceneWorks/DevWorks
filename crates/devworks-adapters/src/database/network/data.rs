//! Shared checks and settlement for network data sessions. The engine-specific code must pin
//! its schema and report physical column identities before any caller statement runs.
use super::*;
use devworks_core::ports::database::data::*;
use devworks_core::services::database::statement::{
    ColumnAccess, DataMode, Footprint, Name, Relation,
};
use std::collections::BTreeSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

pub struct NetworkDataAdapter;

impl DataAdapter for NetworkDataAdapter {
    fn capabilities(&self, engine: &DbEngine) -> DataCapabilities {
        let supported = matches!(
            engine,
            DbEngine::Postgres | DbEngine::MariaDb | DbEngine::MySql | DbEngine::SqlServer
        );
        DataCapabilities {
            read: supported,
            write: supported,
            schema_change: supported,
            atomic_ddl: matches!(engine, DbEngine::Postgres | DbEngine::SqlServer),
        }
    }

    fn execute(&self, request: DataRequest, guard: Arc<dyn DispatchGuard>) -> DataFuture {
        Box::pin(async move {
            let _ = rustls::crypto::ring::default_provider().install_default();
            let boundary = Boundary::default();
            let token = request.cancellation.clone();
            let deadline = request.limits.deadline;
            let cancel = async {
                while !token.is_cancelled() {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            };
            let action = async {
                let target = Target::parse(&request.config, credentials(&request))
                    .map_err(metadata_failure)?;
                if target.catalog.is_empty() {
                    return Err(refused(FailureKind::InvalidTarget));
                }
                guard.before_statement(0)?;
                match request.config.engine {
                    DbEngine::MySql | DbEngine::MariaDb => {
                        super::mysql::data::execute(request, target, guard.as_ref(), &boundary)
                            .await
                    }
                    DbEngine::Postgres => {
                        super::postgres::data::execute(request, target, guard.as_ref(), &boundary)
                            .await
                    }
                    DbEngine::SqlServer => {
                        super::sql_server::data::execute(request, target, guard.as_ref(), &boundary)
                            .await
                    }
                    _ => Err(refused(FailureKind::UnsupportedProfile)),
                }
            };
            tokio::select! {
                biased;
                _ = cancel => Err(boundary.interrupted(FailureKind::Cancelled)),
                _ = tokio::time::sleep_until(deadline.into()) => Err(boundary.interrupted(FailureKind::Deadline)),
                outcome = action => outcome,
            }
        })
    }
}

pub(super) fn credentials(request: &DataRequest) -> WireCredentials<'_> {
    WireCredentials {
        password: request.password.as_ref(),
        ca: request.ca_certificate.as_deref(),
    }
}

pub(super) fn refused(reason: FailureKind) -> DataFailure {
    DataFailure::Refused(reason)
}

pub(super) fn metadata_failure(failure: MetadataFailure) -> DataFailure {
    refused(match failure {
        MetadataFailure::UnsupportedProfile | MetadataFailure::UnsupportedServerVersion { .. } => {
            FailureKind::UnsupportedProfile
        }
        MetadataFailure::CredentialsRequired | MetadataFailure::AccessDenied => {
            FailureKind::PermissionDenied
        }
        MetadataFailure::InvalidTarget | MetadataFailure::InvalidMetadata => {
            FailureKind::InvalidTarget
        }
        MetadataFailure::Cancelled => FailureKind::Cancelled,
        MetadataFailure::Deadline => FailureKind::Deadline,
        MetadataFailure::Unavailable => FailureKind::Unavailable,
    })
}

pub(super) struct Boundary {
    possibly_committing: AtomicBool,
    known: Mutex<MutationResult>,
}

impl Default for Boundary {
    fn default() -> Self {
        Self {
            possibly_committing: AtomicBool::new(false),
            known: Mutex::new(MutationResult {
                affected_rows: None,
                observed_statements: 0,
                schema_changed: false,
            }),
        }
    }
}

impl Boundary {
    pub(super) fn committing(&self) {
        self.possibly_committing.store(true, Ordering::SeqCst);
    }

    pub(super) fn observed(
        &self,
        guard: &dyn DispatchGuard,
        position: usize,
        result: MutationResult,
    ) -> Result<(), DataFailure> {
        *self.known.lock().expect("mutation boundary mutex") = result;
        guard
            .observed_statement(position, result)
            .map_err(|_| DataFailure::Unknown { known: result })?;
        self.possibly_committing.store(false, Ordering::SeqCst);
        Ok(())
    }

    pub(super) fn rejected(&self, reason: FailureKind) -> DataFailure {
        self.possibly_committing.store(false, Ordering::SeqCst);
        self.interrupted(reason)
    }

    pub(super) fn interrupted(&self, reason: FailureKind) -> DataFailure {
        let known = *self.known.lock().expect("mutation boundary mutex");
        if self.possibly_committing.load(Ordering::SeqCst) {
            DataFailure::Unknown { known }
        } else if known.observed_statements > 0 {
            DataFailure::Partial { reason, known }
        } else {
            refused(reason)
        }
    }
}

pub(super) struct PhysicalTable {
    pub(super) schema: String,
    pub(super) name: String,
    pub(super) columns: Vec<String>,
    pub(super) aliases: BTreeSet<String>,
}

pub(super) fn normalized(engine: &DbEngine, name: &Name) -> String {
    if matches!(engine, DbEngine::Postgres) && !name.quoted {
        name.value.to_ascii_lowercase()
    } else {
        name.value.clone()
    }
}

pub(super) fn names(
    engine: &DbEngine,
    relation: &Relation,
    catalog: &str,
    default_schema: &str,
) -> Result<(String, String), DataFailure> {
    let parts: Vec<_> = relation
        .path
        .iter()
        .map(|name| normalized(engine, name))
        .collect();
    let (schema, name) = match parts.as_slice() {
        [name] => (default_schema, name.as_str()),
        [schema, name] => (schema.as_str(), name.as_str()),
        [selected, schema, name] if selected == catalog => (schema.as_str(), name.as_str()),
        _ => return Err(refused(FailureKind::PermissionDenied)),
    };
    if matches!(engine, DbEngine::MySql | DbEngine::MariaDb) && schema != catalog {
        return Err(refused(FailureKind::PermissionDenied));
    }
    Ok((schema.to_owned(), name.to_owned()))
}

pub(super) fn check_scope(
    request: &DataRequest,
    scope: &NameScope,
    schema: &str,
    name: &str,
) -> Result<(), DataFailure> {
    let selected = &request.config.schemas.0;
    if (!selected.is_empty() && !selected.iter().any(|part| part.0 == schema))
        || !scope.includes(name, &format!("{schema}.{name}"))
    {
        return Err(refused(FailureKind::PermissionDenied));
    }
    Ok(())
}

pub(super) fn check_columns(
    engine: &DbEngine,
    catalog: &str,
    footprint: &Footprint,
    tables: &[PhysicalTable],
    guard: &dyn DispatchGuard,
) -> Result<(), DataFailure> {
    let mut checks = Vec::new();
    let is_name = |actual: &str, requested: &Name| match engine {
        DbEngine::Postgres => actual == normalized(engine, requested),
        _ => actual.eq_ignore_ascii_case(&requested.value),
    };
    for table in tables {
        for choice in &footprint.columns {
            let (name, path) = match choice {
                ColumnAccess::Named(path) => match path.split_last() {
                    Some((name, path)) => (Some(name), path),
                    None => return Err(refused(FailureKind::UnsupportedStatement)),
                },
                ColumnAccess::Wildcard(path) => (None, path.as_slice()),
            };
            let candidate = match path {
                [] => true,
                [qualifier] => {
                    is_name(&table.name, qualifier)
                        || table.aliases.iter().any(|alias| is_name(alias, qualifier))
                        || footprint
                            .derived_aliases
                            .iter()
                            .any(|alias| is_name(&alias.value, qualifier))
                        || footprint
                            .common_tables
                            .iter()
                            .any(|cte| is_name(&cte.value, qualifier))
                }
                [schema, name] => is_name(&table.schema, schema) && is_name(&table.name, name),
                [database, schema, name] => {
                    is_name(catalog, database)
                        && is_name(&table.schema, schema)
                        && is_name(&table.name, name)
                }
                _ => return Err(refused(FailureKind::UnsupportedStatement)),
            };
            if candidate {
                for column in &table.columns {
                    if name.is_none_or(|name| is_name(column, name)) {
                        checks.push(PhysicalColumn {
                            catalog: catalog.to_owned(),
                            schema: table.schema.clone(),
                            table: table.name.clone(),
                            column: column.clone(),
                        });
                    }
                }
            }
        }
    }
    guard.check_columns(&checks)
}

pub(super) struct BoundedRows {
    pub(super) result: ReadResult,
    pub(super) rows: usize,
    bytes: usize,
    used: usize,
}

impl BoundedRows {
    pub(super) fn new(columns: Vec<Column>, limits: &DataLimits) -> Result<Self, DataFailure> {
        let used = columns
            .iter()
            .map(|col| col.name.len() + col.data_type.len() + 32)
            .sum();
        if columns.len() > limits.columns || used > limits.bytes {
            return Err(refused(FailureKind::ResultLimit));
        }
        Ok(Self {
            result: ReadResult {
                columns,
                rows: Vec::new(),
                truncated: false,
                schema_digest: None,
            },
            rows: limits.rows,
            bytes: limits.bytes,
            used,
        })
    }

    pub(super) fn full(&mut self) -> bool {
        if self.result.rows.len() == self.rows {
            self.result.truncated = true;
        }
        self.result.truncated
    }

    pub(super) fn push(&mut self, values: Vec<Scalar>) -> Result<(), DataFailure> {
        for cell in &values {
            let len = match cell {
                Scalar::Bytes(value) => value.len(),
                Scalar::Text(value) | Scalar::Decimal(value) => value.len(),
                _ => 32,
            };
            self.used = self.used.saturating_add(len.saturating_add(16));
            if self.used > self.bytes {
                return Err(refused(FailureKind::ResultLimit));
            }
        }
        self.result.rows.push(values);
        Ok(())
    }
}

pub(super) fn read_only(request: &DataRequest) -> bool {
    request.mode == DataMode::Read
}
