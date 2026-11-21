//! Retained mysql_readonly_query argument and row shape. All SQL runs on the ordinary data path.
use super::executor::scope_context;
use super::statement::DataMode;
use crate::context::InvocationContext;
use crate::operations::database::{MysqlReadonlyQueryInput, ReadInput};
use crate::ports::storage::{ReadSnapshot, StorageError, StoredSchemaSource};
use crate::protocol::records::{
    DatabaseLegacyRows, DbEngine, DbReadResult, DbScalar, SchemaExposureMode,
};
use crate::protocol::support::{BoundedList, DynamicJson, Limit, PositiveCounter};
use crate::services::kernel::counter;
use crate::services::schemas::data;
use serde_json::{Value, json};
use std::collections::BTreeMap;

pub(super) fn select(
    snapshot: &dyn ReadSnapshot,
    context: &InvocationContext,
    input: &MysqlReadonlyQueryInput,
) -> Result<ReadInput, StorageError> {
    let scoped = scope_context(context, input.scope.as_ref())?;
    let name = input.source.0.trim();
    if name.is_empty() {
        return Err(StorageError::ScopeDenied);
    }
    let mut choices = Vec::new();
    for candidate in snapshot.schemas().mysql_readonly_sources()? {
        let id = &candidate.connection.id;
        match data::source(
            snapshot,
            &scoped,
            id,
            &candidate.connection.revision,
            DataMode::Read,
        ) {
            Ok(source) => choices.push(source),
            Err(StorageError::ScopeDenied) => {}
            Err(error) => return Err(error),
        }
    }
    let mut by_rank = BTreeMap::<u8, Vec<&StoredSchemaSource>>::new();
    for source in &choices {
        let Some(config) = source.connection.config.as_ref() else {
            continue;
        };
        if !matches!(config.engine, DbEngine::MySql | DbEngine::MariaDb)
            || !matches!(config.exposure_mode, Some(SchemaExposureMode::ReadOnly))
        {
            continue;
        };
        let Some((host, database)) = logical_endpoint(config.endpoint.0.as_str()) else {
            continue;
        };
        let rank = if source.connection.id.as_str() == name {
            Some(0)
        } else if config
            .display_name
            .as_ref()
            .is_some_and(|display| display.0.eq_ignore_ascii_case(name))
        {
            Some(1)
        } else if format!("{host}/{database}").eq_ignore_ascii_case(name) {
            Some(2)
        } else if !name.eq_ignore_ascii_case("default") && database.eq_ignore_ascii_case(name) {
            Some(3)
        } else if !name.eq_ignore_ascii_case("default") && host.eq_ignore_ascii_case(name) {
            Some(4)
        } else {
            None
        };
        if let Some(rank) = rank {
            by_rank.entry(rank).or_default().push(source);
        }
    }
    let selected = by_rank
        .values()
        .next()
        .filter(|candidates| candidates.len() == 1)
        .map(|candidates| candidates[0])
        .ok_or(StorageError::ScopeDenied)?;
    let count = input
        .limit
        .as_ref()
        .and_then(|limit| limit.0.parse::<i64>().ok())
        .unwrap_or(100)
        .clamp(1, 500);
    Ok(ReadInput {
        scope: input.scope.clone(),
        connection_id: selected.connection.id.clone(),
        expected_connection_revision: selected.connection.revision.clone(),
        statement: input.sql.clone(),
        parameters: BoundedList(Vec::new()),
        row_limit: Limit::new(count as u32).ok_or(StorageError::ScopeDenied)?,
        timeout_ms: PositiveCounter("30000".into()),
        include_schema_digest: None,
    })
}

fn logical_endpoint(endpoint: &str) -> Option<(String, String)> {
    let url = url::Url::parse(endpoint).ok()?;
    let host = url.host_str()?.to_owned();
    let encoded = url.path().strip_prefix('/')?;
    let database = percent_encoding::percent_decode_str(encoded)
        .decode_utf8()
        .ok()?
        .into_owned();
    Some((host, database))
}

pub(super) fn rows(result: &DbReadResult) -> DatabaseLegacyRows {
    let rows: Vec<_> = result
        .rows
        .0
        .iter()
        .map(|row| {
            let mut map = BTreeMap::new();
            for (column, cell) in result.columns.0.iter().zip(&row.cells.0) {
                let value = match cell {
                    DbScalar::Null {} => Value::Null,
                    DbScalar::Boolean { value } => json!(value),
                    DbScalar::Integer { value } => value
                        .0
                        .parse::<i64>()
                        .map_or_else(|_| json!(value.0), |value| json!(value)),
                    DbScalar::Decimal { value } => serde_json::from_str::<Value>(&value.0)
                        .ok()
                        .filter(|value| value.is_number())
                        .unwrap_or_else(|| json!(value.0)),
                    DbScalar::Text { value } => json!(value.0),
                    DbScalar::Bytes { value } => json!(value),
                };
                // The retained source treats duplicate names as one field, case-insensitively.
                let name = &column.name.0;
                let entry = map
                    .entry(name.to_lowercase())
                    .or_insert_with(|| (name.clone(), Value::Null));
                entry.1 = value;
            }
            DynamicJson(Value::Object(map.into_values().collect()))
        })
        .collect();
    DatabaseLegacyRows {
        row_count: counter(rows.len() as i64),
        rows: BoundedList(rows),
    }
}
