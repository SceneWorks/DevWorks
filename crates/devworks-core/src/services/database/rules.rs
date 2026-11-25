use crate::context::{InvocationContext, PrincipalKind};
use crate::operations::database as ops;
use crate::ports::IdKind;
use crate::ports::storage::{
    NewEvent, SchemaRead, SensitivePolicy, SensitiveRule, StorageError, WriteTransaction,
};
use crate::protocol::records::{
    DatabaseSensitiveColumnRule, DatabaseSensitiveRuleEdit, DatabaseSensitiveRuleSet,
};
use crate::protocol::results::{DomainDecision, OperationResult};
use crate::protocol::support::{BoundedList, BoundedText};
use crate::protocol::{EventId, Id, RevisionDimension};
use crate::services::kernel::{Failure, rev};
use serde_json::json;

pub(super) fn administrator(
    context: &InvocationContext,
    schemas: &dyn SchemaRead,
    connection: Option<&Id>,
) -> Result<(), StorageError> {
    if context.principal_kind() != PrincipalKind::Human
        || context.project_id().is_some()
        || context.root_id().is_some()
    {
        return Err(StorageError::ScopeDenied);
    }
    if let Some(id) = connection
        && !schemas
            .schema_source(id)?
            .is_some_and(|row| !row.deleted && row.owner_id == *context.principal())
    {
        return Err(StorageError::ScopeDenied);
    }
    Ok(())
}

pub(super) fn project(connection: Option<Id>, policy: SensitivePolicy) -> DatabaseSensitiveRuleSet {
    DatabaseSensitiveRuleSet {
        connection_id: connection,
        revision: rev(policy.revision.saturating_add(1)),
        rules: BoundedList(
            policy
                .rules
                .into_iter()
                .map(|rule| DatabaseSensitiveColumnRule {
                    id: rule.id,
                    catalog: rule.catalog.map(BoundedText),
                    schema: rule.schema.map(BoundedText),
                    table: rule.table.map(BoundedText),
                    column: BoundedText(rule.column),
                    allow: rule.allow,
                    reason: rule.reason.map(BoundedText),
                })
                .collect(),
        ),
    }
}

pub(super) fn edit(
    transaction: &mut dyn WriteTransaction,
    context: &InvocationContext,
    input: &ops::EditSensitiveColumnRulesInput,
) -> Result<DomainDecision, Failure> {
    let id = input.connection_id.as_ref();
    administrator(context, transaction.schemas(), id)?;
    let mut policy = transaction.schemas().sensitive_policy(id)?;
    let actual = rev(policy.revision.saturating_add(1));
    let key = id
        .cloned()
        .unwrap_or_else(|| Id::new("workspace").expect("static workspace id"));
    if actual != input.expected_revision {
        return Err(StorageError::conflict(
            RevisionDimension::Configuration,
            "database_sensitive_policy",
            key,
            input.expected_revision.clone(),
            actual,
        )
        .into());
    }
    match &input.edit {
        DatabaseSensitiveRuleEdit::Delete { id } => {
            policy.legacy_columns.retain(|column| column != id);
            policy.rules.retain(|rule| &rule.id != id);
        }
        DatabaseSensitiveRuleEdit::Upsert { rule } => {
            if policy.legacy_columns.contains(&rule.id)
                || rule.column.0.trim().is_empty()
                || rule.column.0.chars().any(char::is_control)
                || [&rule.catalog, &rule.schema, &rule.table]
                    .iter()
                    .any(|selector| {
                        selector.as_ref().is_some_and(|name| {
                            name.0.trim().is_empty() || name.0.chars().any(char::is_control)
                        })
                    })
            {
                return Err(StorageError::InvalidRecord(
                    "invalid sensitive database column selector".into(),
                )
                .into());
            }
            let value = SensitiveRule {
                id: rule.id.clone(),
                catalog: rule.catalog.as_ref().map(|name| name.0.clone()),
                schema: rule.schema.as_ref().map(|name| name.0.clone()),
                table: rule.table.as_ref().map(|name| name.0.clone()),
                column: rule.column.0.clone(),
                allow: rule.allow,
                reason: rule.reason.as_ref().map(|reason| reason.0.clone()),
            };
            if let Some(existing) = policy
                .rules
                .iter_mut()
                .find(|current| current.id == rule.id)
            {
                *existing = value;
            } else {
                policy.rules.push(value);
            }
        }
    }
    let prior = policy.revision;
    policy.revision = prior
        .checked_add(1)
        .ok_or_else(|| StorageError::InvalidRecord("database policy revision overflow".into()))?;
    transaction
        .schemas()
        .set_sensitive_policy(id, prior, &policy)?;
    let event = EventId(transaction.new_id(IdKind::Event));
    transaction.history().append(NewEvent {
        id: event,
        actor: context.principal().clone(),
        project_id: None,
        root_id: None,
        operation_id: None,
        aggregate_kind: "database_sensitive_policy".into(),
        aggregate_id: key.as_str().into(),
        aggregate_revision: policy.revision,
        kind: "DatabaseSensitiveColumnRuleEdited".into(),
        payload: json!({"connectionId":id,"ruleCount":policy.rules.len()}),
    })?;
    Ok(DomainDecision::Applied {
        result: OperationResult::DatabaseEditSensitiveColumnRules {
            value: project(input.connection_id.clone(), policy),
        },
    })
}
