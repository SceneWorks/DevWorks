//! Retained mysql_readonly_query argument and row shape. All SQL runs on the ordinary data path.
use super::executor::scope_context;
use super::statement::DataMode;
use crate::context::InvocationContext;
use crate::operations::database::{MysqlReadonlyQueryInput, ReadInput};
use crate::ports::database::data::{ReadResult, Scalar};
use crate::ports::storage::{ReadSnapshot, StorageError, StoredSchemaSource};
use crate::protocol::provider::ArtifactRef;
use crate::protocol::records::{DatabaseLegacyRows, DbColumn, DbEngine, SchemaExposureMode};
use crate::protocol::support::{BoundedList, BoundedText, DynamicJson, Limit, PositiveCounter};
use crate::services::schemas::data;
use base64::{Engine, engine::general_purpose::STANDARD};
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
    let count = input.limit.unwrap_or(100).clamp(1, 500);
    Ok(ReadInput {
        scope: input.scope.clone(),
        connection_id: selected.connection.id.clone(),
        expected_connection_revision: selected.connection.revision.clone(),
        statement: input.sql.clone(),
        parameters: BoundedList(Vec::new()),
        row_limit: Limit::new(count as u32).ok_or(StorageError::ScopeDenied)?,
        timeout_ms: PositiveCounter("30000".into()),
        include_schema_digest: None,
        schema_preview: None,
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

pub(super) fn rows(
    result: &ReadResult,
    artifacts: Option<&BTreeMap<String, ArtifactRef>>,
) -> Option<DatabaseLegacyRows> {
    let mut rows = Vec::with_capacity(result.rows.len());
    for row in &result.rows {
        let mut map = BTreeMap::new();
        for (column, cell) in result.columns.iter().zip(row) {
            let value = match cell {
                Scalar::Null => Value::Null,
                Scalar::Boolean(value) => json!(value),
                Scalar::Integer(value) => json!(value),
                Scalar::Decimal(value) => serde_json::from_str::<Value>(value)
                    .ok()
                    .filter(|value| value.is_number())
                    .unwrap_or_else(|| json!(value)),
                Scalar::Text(value) => json!(value),
                Scalar::Bytes(value) => match artifacts {
                    Some(artifacts) => {
                        json!(artifacts.get(&crate::fingerprint::sha256_hex(value))?)
                    }
                    None => json!(STANDARD.encode(value)),
                },
            };
            // The retained source keeps the first spelling and last value for duplicate names.
            let name = &column.name;
            let entry = map
                .entry(name.to_lowercase())
                .or_insert_with(|| (name.clone(), Value::Null));
            entry.1 = value;
        }
        rows.push(DynamicJson(Value::Object(map.into_values().collect())));
    }
    Some(DatabaseLegacyRows {
        columns: artifacts.map(|_| {
            BoundedList(
                result
                    .columns
                    .iter()
                    .map(|column| DbColumn {
                        name: BoundedText(column.name.clone()),
                        data_type: BoundedText(column.data_type.clone()),
                        sensitive: false,
                    })
                    .collect(),
            )
        }),
        truncated: artifacts.map(|_| result.truncated),
        row_count: i32::try_from(rows.len()).ok()?,
        rows: BoundedList(rows),
    })
}
