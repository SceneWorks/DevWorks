use super::DataExecutor;
use crate::context::InvocationContext;
use crate::gateway::AdmittedCommand;
use crate::operations::{OperationInput, database as ops};
use crate::ports::storage::{NewEvent, StorageError, WriteTransaction};
use crate::ports::{CommandHandler, Decided, HandlerFailure, IdKind, Preparation, Prepared};
use crate::protocol::results::{DomainDecision, OperationResult};
use crate::protocol::{DomainErrorCode, EventId, RevisionDimension};
use crate::services::kernel::{Failure, Refusal, decided};
use crate::services::schemas::data;
use serde_json::json;
use std::collections::BTreeSet;

impl DataExecutor {
    fn replace_sensitive_columns(
        &self,
        transaction: &mut dyn WriteTransaction,
        context: &InvocationContext,
        input: &ops::ConfigureSensitiveColumnsInput,
    ) -> Result<DomainDecision, Failure> {
        let id = &input.connection_id;
        super::rules::administrator(context, transaction.schemas(), Some(id))?;
        let mut source = transaction
            .schemas()
            .schema_source(id)?
            .filter(|row| !row.deleted && row.owner_id == *context.principal())
            .ok_or(StorageError::ScopeDenied)?;
        if source.connection.revision != input.expected_revision {
            return Err(StorageError::conflict(
                RevisionDimension::Connection,
                "schema_connection",
                id.clone(),
                input.expected_revision.clone(),
                source.connection.revision.clone(),
            )
            .into());
        }
        let ids = &input.column_ids.0;
        if ids
            .iter()
            .map(|id| id.as_str())
            .collect::<BTreeSet<_>>()
            .len()
            != ids.len()
        {
            return Err(Refusal::new(
                DomainErrorCode::InvalidArgument,
                "database_sensitive_columns",
                None,
            )
            .into());
        }
        let rules = data::legacy_rules(transaction.schemas(), &source, ids)?;
        let mut local = transaction.schemas().sensitive_policy(Some(id))?;
        let previous = local.revision;
        let new_revision = source.connection.revision.checked_next().ok_or_else(|| {
            StorageError::InvalidRecord("database source revision overflow".into())
        })?;
        let existing: BTreeSet<_> = local.legacy_columns.iter().map(|id| id.as_str()).collect();
        local
            .rules
            .retain(|rule| !existing.contains(rule.id.as_str()));
        if local.rules.iter().any(|rule| ids.contains(&rule.id)) {
            return Err(StorageError::ScopeDenied.into());
        }
        local.legacy_columns.clone_from(ids);
        local.rules.extend(rules);
        local.revision = previous.checked_add(1).ok_or_else(|| {
            StorageError::InvalidRecord("database policy revision overflow".into())
        })?;
        transaction
            .schemas()
            .set_sensitive_policy(Some(id), previous, &local)?;
        source.connection.revision = new_revision;
        source.updated_at_ms = transaction.now_ms();
        transaction
            .schemas()
            .revise_schema_source(&source, input.expected_revision.get())?;
        let event = EventId(transaction.new_id(IdKind::Event));
        transaction.history().append(NewEvent {
            id: event,
            actor: context.principal().clone(),
            project_id: None,
            root_id: None,
            operation_id: None,
            aggregate_kind: "schema_connection".into(),
            aggregate_id: id.as_str().to_owned(),
            aggregate_revision: source.connection.revision.get(),
            kind: "DatabaseSensitiveColumnsConfigured".into(),
            payload: json!({"connectionId": id, "rules": local.legacy_columns.len()}),
        })?;
        Ok(DomainDecision::Applied {
            result: OperationResult::DatabaseConfigureSensitiveColumns {
                value: source.connection,
            },
        })
    }
}

impl CommandHandler for DataExecutor {
    fn prepare(
        &self,
        context: &InvocationContext,
        command: &AdmittedCommand,
        env: &Preparation<'_>,
    ) -> Result<Option<Prepared>, StorageError> {
        match command.input() {
            OperationInput::Database(
                input @ (ops::Input::ExecuteWrite(_)
                | ops::Input::ApplyMigration(_)
                | ops::Input::DirectSchemaChange(_)),
            ) => self.prepare_mutation(context, command, env, input),
            _ => Ok(None),
        }
    }

    fn settle(
        &self,
        _: &InvocationContext,
        prepared: Option<&Prepared>,
        _: Option<&crate::protocol::results::DomainCommandReply>,
        env: &Preparation<'_>,
    ) {
        if let Some(prepared) = prepared {
            self.settle_mutation(prepared, env);
        }
    }

    fn decide(
        &self,
        context: &InvocationContext,
        command: &AdmittedCommand,
        transaction: &mut dyn WriteTransaction,
    ) -> Result<Decided, HandlerFailure> {
        let result = match command.input() {
            OperationInput::Database(ops::Input::ConfigureSensitiveColumns(input)) => {
                self.replace_sensitive_columns(transaction, context, input)
            }
            OperationInput::Database(
                ops::Input::ExecuteWrite(_)
                | ops::Input::ApplyMigration(_)
                | ops::Input::DirectSchemaChange(_),
            ) => self.decide_mutation(transaction, context, command),
            OperationInput::Database(ops::Input::EditSensitiveColumnRules(input)) => {
                super::rules::edit(transaction, context, input)
            }
            _ => Err(Refusal::new(
                DomainErrorCode::UnsupportedCapability,
                "database_data",
                None,
            )
            .into()),
        };
        let mut outcome = decided(transaction, result)?;
        if let DomainDecision::Accepted { operation_id } = &outcome.decision {
            outcome.operation_id = Some(operation_id.clone());
        }
        Ok(outcome)
    }
}
