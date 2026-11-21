use super::statement::DataMode;
use super::{DataExecutor, Selected, check_fences, fault, native_failure, parameters, statement};
use crate::context::InvocationContext;
use crate::gateway::AdmittedQuery;
use crate::operations::{OperationInput, database as ops};
use crate::ports::database::data::{DataLimits, DataOutcome, Scalar};
use crate::ports::storage::{
    ArtifactScope, DatabaseArtifactAccess, ReadSnapshot, StorageError, WriteOptions, run_write,
};
use crate::ports::{Preparation, Prepared, QueryAnswer, QueryHandler};
use crate::protocol::provider::{ArtifactRef, Digest};
use crate::protocol::records::{DbColumn, DbReadResult, DbRow, DbScalar};
use crate::protocol::results::OperationResult;
use crate::protocol::support::{BoundedList, BoundedText, SignedCounter};
use crate::protocol::{Check, DomainErrorCode, DomainFault};
use crate::services::artifacts::{check_quota, host::HostArtifactPublication, version_ref};
use crate::services::schemas::data;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

const MAX_ROWS: usize = 500;
const MAX_BYTES: usize = 256 * 1024;
const MAX_COLUMNS: usize = 128;
const MAX_MS: i64 = 30_000;

struct PreparedRead {
    selected: Arc<Selected>,
    result: OperationResult,
}

impl QueryHandler for DataExecutor {
    fn prepare(
        &self,
        context: &InvocationContext,
        query: &AdmittedQuery,
        environment: &Preparation<'_>,
    ) -> Result<Option<Prepared>, DomainFault> {
        if matches!(
            query.input(),
            OperationInput::Database(ops::Input::SensitiveColumns(_))
        ) {
            return Ok(None);
        }
        let input = match query.input() {
            OperationInput::Database(ops::Input::Read(input)) => input.clone(),
            OperationInput::Database(ops::Input::MysqlReadonlyQuery(input)) => environment
                .read(|snapshot| super::mysql_compat::select(snapshot, context, input))
                .map_err(|error| super::store_fault(query, error))?,
            _ => return Err(fault(query, DomainErrorCode::UnsupportedCapability)),
        };
        let scoped = super::executor::scope_context(context, input.scope.as_ref())
            .map_err(|error| super::store_fault(query, error))?;
        let context = &scoped;
        let selected = environment
            .read(|read| {
                check_fences(read, context, query.authorization())?;
                Selected::current(
                    read,
                    context,
                    &input.connection_id,
                    input.expected_connection_revision.clone(),
                    DataMode::Read,
                )
            })
            .map_err(|error| super::store_fault(query, error))?;
        let selected = Arc::new(selected);

        if !self.adapter.capabilities(&selected.config.engine).read {
            return Err(fault(query, DomainErrorCode::UnsupportedTarget));
        }
        let statements =
            statement::check(&selected.config.engine, DataMode::Read, &input.statement.0)
                .map_err(|_| fault(query, DomainErrorCode::InvalidArgument))?;
        let order = parameters::order(
            &selected.config.engine,
            &statements[0].bindings,
            &input.parameters.0,
        )
        .map_err(|_| fault(query, DomainErrorCode::InvalidArgument))?;
        let values = self
            .parameters(context, &order, &input.parameters.0)
            .map_err(|error| super::store_fault(query, error))?;
        let expires = environment.deadline_ms.map_or(MAX_MS, |time| {
            time.saturating_sub(self.clock.now_ms().get())
        });
        let milliseconds = input
            .timeout_ms
            .0
            .parse::<i64>()
            .map_err(|_| fault(query, DomainErrorCode::InvalidArgument))?
            .min(MAX_MS)
            .min(expires);
        if milliseconds <= 0 {
            return Err(fault(query, DomainErrorCode::Cancelled));
        }
        let limits = DataLimits {
            deadline: Instant::now() + Duration::from_millis(milliseconds as u64),
            rows: (input.row_limit.get() as usize).min(MAX_ROWS),
            bytes: MAX_BYTES,
            columns: MAX_COLUMNS,
        };
        let mut request = self
            .request(
                &selected,
                statements,
                values,
                limits,
                environment.cancellation.cloned().unwrap_or_default(),
            )
            .map_err(|error| super::store_fault(query, error))?;
        request.include_schema_digest = input.include_schema_digest.unwrap_or(false);
        if let Some(preview) = &input.schema_preview {
            if !request.include_schema_digest {
                return Err(fault(query, DomainErrorCode::InvalidArgument));
            }
            let planned =
                statement::check(&selected.config.engine, DataMode::Migration, &preview.0)
                    .map_err(|_| fault(query, DomainErrorCode::InvalidArgument))?;
            if !planned.iter().any(|statement| statement.schema_change) {
                return Err(fault(query, DomainErrorCode::InvalidArgument));
            }
            request.schema_targets = statement::schema_targets(&planned);
        }
        if request.include_schema_digest
            && request.schema_targets.is_empty()
            && !matches!(
                selected.config.engine,
                crate::protocol::records::DbEngine::Sqlite
            )
        {
            return Err(fault(query, DomainErrorCode::InvalidArgument));
        }
        let guard = Arc::new(self.read_guard(
            context.clone(),
            query.authorization().clone(),
            Arc::clone(&selected),
        ));
        let DataOutcome::Read(rows) = self
            .reader
            .wait(self.adapter.execute(request, guard))
            .map_err(|error| fault(query, native_failure(error)))?
        else {
            return Err(fault(query, DomainErrorCode::ExternalFailure));
        };
        let legacy = matches!(
            query.input(),
            OperationInput::Database(ops::Input::MysqlReadonlyQuery(_))
        );
        if legacy && query.schema_revision().0 == 1 {
            let value = super::mysql_compat::rows(&rows, None)
                .ok_or_else(|| fault(query, DomainErrorCode::ExternalFailure))?;
            value
                .check()
                .map_err(|_| fault(query, DomainErrorCode::ExternalFailure))?;
            return Ok(Some(Prepared::new(PreparedRead {
                selected,
                result: OperationResult::DatabaseMysqlReadonlyQuery { value },
            })));
        }
        let binary = self.publish_cells(context, query, &selected, &rows.rows)?;
        if legacy {
            let value = super::mysql_compat::rows(&rows, Some(&binary))
                .ok_or_else(|| fault(query, DomainErrorCode::ExternalFailure))?;
            value
                .check()
                .map_err(|_| fault(query, DomainErrorCode::ExternalFailure))?;
            return Ok(Some(Prepared::new(PreparedRead {
                selected,
                result: OperationResult::DatabaseMysqlReadonlyQuery { value },
            })));
        }
        let answer = DbReadResult {
            columns: BoundedList(
                rows.columns
                    .into_iter()
                    .map(|column| DbColumn {
                        name: BoundedText(column.name),
                        data_type: BoundedText(column.data_type),
                        sensitive: false,
                    })
                    .collect(),
            ),
            rows: BoundedList(
                rows.rows
                    .into_iter()
                    .map(|cells| {
                        let cells = cells
                            .into_iter()
                            .map(|cell| match cell {
                                Scalar::Null => Ok(DbScalar::Null {}),
                                Scalar::Boolean(value) => Ok(DbScalar::Boolean { value }),
                                Scalar::Integer(value) => Ok(DbScalar::Integer {
                                    value: SignedCounter(value.to_string()),
                                }),
                                Scalar::Decimal(value) => Ok(DbScalar::Decimal {
                                    value: BoundedText(value),
                                }),
                                Scalar::Text(value) => Ok(DbScalar::Text {
                                    value: BoundedText(value),
                                }),
                                Scalar::Bytes(value) => binary
                                    .get(&crate::fingerprint::sha256_hex(&value))
                                    .cloned()
                                    .map(|value| DbScalar::Bytes { value })
                                    .ok_or_else(|| fault(query, DomainErrorCode::ExternalFailure)),
                            })
                            .collect::<Result<Vec<_>, _>>()?;
                        Ok(DbRow {
                            cells: BoundedList(cells),
                        })
                    })
                    .collect::<Result<Vec<_>, _>>()?,
            ),
            truncated: rows.truncated,
            next_cursor: None,
            schema_digest: rows
                .schema_digest
                .map(Digest::try_from)
                .transpose()
                .map_err(|_| fault(query, DomainErrorCode::ExternalFailure))?,
            source_revision: Some(input.expected_connection_revision.clone()),
        };
        answer
            .check()
            .map_err(|_| fault(query, DomainErrorCode::ExternalFailure))?;
        Ok(Some(Prepared::new(PreparedRead {
            selected,
            result: OperationResult::DatabaseRead { value: answer },
        })))
    }

    fn query(
        &self,
        context: &InvocationContext,
        query: &AdmittedQuery,
        snapshot: &dyn ReadSnapshot,
    ) -> Result<QueryAnswer, DomainFault> {
        if let OperationInput::Database(ops::Input::SensitiveColumns(input)) = query.input() {
            check_fences(snapshot, context, query.authorization())
                .and_then(|()| {
                    super::rules::administrator(
                        context,
                        snapshot.schemas(),
                        input.connection_id.as_ref(),
                    )
                })
                .map_err(|error| super::store_fault(query, error))?;
            let policy = snapshot
                .schemas()
                .sensitive_policy(input.connection_id.as_ref())
                .map_err(|error| super::store_fault(query, error))?;
            let value = super::rules::project(input.connection_id.clone(), policy);
            return Ok(QueryAnswer {
                stamp: snapshot
                    .stamp()
                    .read_stamp(context.policy_revision().clone(), value.revision.clone()),
                result: OperationResult::DatabaseSensitiveColumns { value },
            });
        }
        let scope = match query.input() {
            OperationInput::Database(ops::Input::Read(input)) => input.scope.as_ref(),
            OperationInput::Database(ops::Input::MysqlReadonlyQuery(input)) => input.scope.as_ref(),
            _ => return Err(fault(query, DomainErrorCode::UnsupportedCapability)),
        };
        let scoped = super::executor::scope_context(context, scope)
            .map_err(|error| super::store_fault(query, error))?;
        let context = &scoped;
        let read = query
            .prepared::<PreparedRead>()
            .ok_or_else(|| fault(query, DomainErrorCode::ExternalFailure))?;
        check_fences(snapshot, context, query.authorization())
            .and_then(|()| read.selected.still_current(snapshot, context))
            .map_err(|error| super::store_fault(query, error))?;
        Ok(QueryAnswer {
            stamp: snapshot.stamp().read_stamp(
                context.policy_revision().clone(),
                read.selected.revision.clone(),
            ),
            result: read.result.clone(),
        })
    }
}

impl DataExecutor {
    fn publish_cells(
        &self,
        context: &InvocationContext,
        query: &AdmittedQuery,
        selected: &Arc<Selected>,
        rows: &[Vec<Scalar>],
    ) -> Result<BTreeMap<String, ArtifactRef>, DomainFault> {
        let mut binary = BTreeMap::new();
        for values in rows {
            for cell in values {
                if let Scalar::Bytes(bytes) = cell {
                    binary
                        .entry(crate::fingerprint::sha256_hex(bytes))
                        .or_insert(bytes);
                    if binary.len() > 32 {
                        return Err(fault(query, DomainErrorCode::InvalidArgument));
                    }
                }
            }
        }
        if binary.is_empty() {
            return Ok(BTreeMap::new());
        }
        let mut staged = Vec::new();
        for (digest, bytes) in binary {
            match self.artifacts.stage_host_bytes(
                self.store.as_ref(),
                bytes,
                serde_json::json!({"kind":"DatabaseBinaryCell"}),
            ) {
                Ok(Ok(value)) => staged.push((digest, value)),
                _ => {
                    for (_, value) in &staged {
                        self.artifacts
                            .settle_host_bytes(self.store.as_ref(), value, false);
                    }
                    return Err(fault(query, DomainErrorCode::StorageUnavailable));
                }
            }
        }
        let context = context.clone();
        let selected = Arc::clone(selected);
        let authorization = query.authorization().clone();
        let artifacts = Arc::clone(&self.artifacts);
        let capability = query.descriptor().name.to_owned();
        let to_commit = staged.clone();
        let result = run_write(self.store.as_ref(), WriteOptions::settlement(), move |tx| {
            if !crate::authority::authorization_holds_in(tx, &context, &authorization)? {
                return Err(StorageError::ScopeDenied);
            }
            let source = data::transaction_source(
                tx,
                &context,
                &selected.id,
                &selected.revision,
                DataMode::Read,
            )?;
            let current = Selected::from_source(
                tx.schemas(),
                source,
                selected.revision.clone(),
                DataMode::Read,
            )?;
            let (configuration, global_policy, source_policy) = current.revisions();
            if current.revisions() != selected.revisions() {
                return Err(StorageError::ScopeDenied);
            }
            let scope = context.project_id().map_or(ArtifactScope::Workspace, |id| {
                ArtifactScope::Project(id.clone())
            });
            let access = DatabaseArtifactAccess {
                connection: current.id,
                revision: current.revision.get(),
                configuration,
                global_policy,
                source_policy,
                scope_policy: context.policy_revision().get(),
                restore_epoch: context.restore_epoch().get(),
                capability,
                fences: authorization.fences().to_vec(),
                grants: authorization
                    .authority_grants()
                    .iter()
                    .map(|grant| (grant.grant_id.clone(), grant.expected_revision))
                    .collect(),
            };
            let mut output = BTreeMap::new();
            for (digest, staged) in &to_commit {
                let version = artifacts.commit_host_bytes_in_scope(
                    tx,
                    staged,
                    HostArtifactPublication {
                        title: "Database binary value",
                        media_type: "application/octet-stream",
                        provenance: serde_json::json!({"purpose":"database_cell","access":access}),
                        scope: scope.clone(),
                        owner: context.principal().clone(),
                    },
                )?;
                output.insert(digest.clone(), version_ref(&version));
            }
            check_quota(tx, &scope).map_err(|_| StorageError::ScopeDenied)?;
            Ok(output)
        });
        if !matches!(result, Err(StorageError::CommitUnknown)) {
            for (_, item) in &staged {
                self.artifacts
                    .settle_host_bytes(self.store.as_ref(), item, result.is_ok());
            }
        }
        result.map_err(|error| super::store_fault(query, error))
    }
}
