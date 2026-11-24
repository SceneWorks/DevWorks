use super::super::data::*;
use super::{connect, qualified_release};
use crate::database::{NameScope, network::Target};
use devworks_core::fingerprint::sha256_hex;
use devworks_core::ports::database::data::*;
use devworks_core::protocol::records::DbEngine;
use devworks_core::services::database::sql_server_view::{Bindings, Definition};
use devworks_core::services::database::statement::{CheckedStatement, DataMode};
use futures_util::TryStreamExt;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use tiberius::numeric::Numeric;
use tiberius::{Client, ColumnData, ColumnType, Query, QueryItem, Row};
use tokio::net::TcpStream;
use tokio_util::compat::Compat;

type Session = Client<Compat<TcpStream>>;

const MAX_RELATIONS: usize = 128;
const MAX_COLUMNS: usize = 1024;

fn classify(error: tiberius::error::Error) -> FailureKind {
    match error.code() {
        Some(229 | 916 | 262 | 18456) => FailureKind::PermissionDenied,
        Some(207 | 208 | 2760) => FailureKind::StaleSchema,
        Some(515 | 547 | 2601 | 2627) => FailureKind::Constraint,
        Some(1222) => FailureKind::Deadline,
        Some(102 | 156 | 206 | 8120) => FailureKind::UnsupportedStatement,
        _ => FailureKind::Unavailable,
    }
}

fn quote(name: &str) -> String {
    format!("[{}]", name.replace(']', "]]"))
}

async fn control(conn: &mut Session, sql: &str) -> Result<(), FailureKind> {
    conn.simple_query(sql)
        .await
        .map_err(classify)?
        .into_results()
        .await
        .map_err(classify)?;
    Ok(())
}

pub(crate) async fn execute(
    request: DataRequest,
    target: Target,
    guard: &dyn DispatchGuard,
    boundary: &Boundary,
) -> Result<DataOutcome, DataFailure> {
    let mut conn = connect(credentials(&request), &target, &target.catalog)
        .await
        .map_err(metadata_failure)?;
    let info = conn.simple_query("SELECT DB_NAME(), CAST(SERVERPROPERTY('ProductVersion') AS nvarchar(128)), SCHEMA_NAME()")
        .await.map_err(|error| refused(classify(error)))?.into_row().await.map_err(|error| refused(classify(error)))?
        .ok_or_else(|| refused(FailureKind::InvalidTarget))?;
    if info.get::<&str, _>(0) != Some(&target.catalog) {
        return Err(refused(FailureKind::InvalidTarget));
    }
    qualified_release(
        info.get(1).unwrap_or_default(),
        1,
        &["16"],
        "SQL Server 2022 (major 16)",
    )
    .map_err(metadata_failure)?;
    let default_schema: &str = info.get(2).unwrap_or("dbo");
    let scope = NameScope::new(&request.config).map_err(metadata_failure)?;
    control(
        &mut conn,
        "SET XACT_ABORT ON; SET LOCK_TIMEOUT 500; BEGIN TRANSACTION;",
    )
    .await
    .map_err(refused)?;
    let outcome = transaction(&mut conn, &target, &request, guard, &scope, default_schema).await;
    match outcome {
        Ok(DataOutcome::Read(rows)) => {
            control(&mut conn, "ROLLBACK TRANSACTION;")
                .await
                .map_err(refused)?;
            Ok(DataOutcome::Read(rows))
        }
        Ok(DataOutcome::Mutation(result)) => {
            if let Err(error) = guard.before_commit() {
                return Err(rollback(&mut conn, error).await);
            }
            boundary.committing();
            control(&mut conn, "COMMIT TRANSACTION;")
                .await
                .map_err(|error| boundary.interrupted(error))?;
            boundary.observed(guard, request.statements.len() - 1, result)?;
            Ok(DataOutcome::Mutation(result))
        }
        Err(error) if read_only(&request) => Err(error),
        Err(error) => Err(rollback(&mut conn, error).await),
    }
}

async fn rollback(conn: &mut Session, failure: DataFailure) -> DataFailure {
    match (
        control(conn, "IF @@TRANCOUNT > 0 ROLLBACK TRANSACTION;").await,
        failure,
    ) {
        (Ok(()), DataFailure::Refused(reason)) => DataFailure::RolledBack(reason),
        (_, failure) => failure,
    }
}

async fn transaction(
    conn: &mut Session,
    target: &Target,
    request: &DataRequest,
    guard: &dyn DispatchGuard,
    scope: &NameScope,
    default_schema: &str,
) -> Result<DataOutcome, DataFailure> {
    let mut result = MutationResult {
        affected_rows: None,
        observed_statements: 0,
        schema_changed: false,
    };
    for (position, checked) in request.statements.iter().enumerate() {
        let views = authorize(conn, request, target, scope, default_schema, checked, guard).await?;
        if position == 0
            && let Some(expected) = &request.expected_schema_digest
        {
            let selected = schema_scope(request, target, default_schema, scope)?;
            lock_schema_targets(conn, &selected).await?;
            if expected != &digest(conn, &selected).await? {
                return Err(refused(FailureKind::StaleSchema));
            }
        }
        let sql = if read_only(request) {
            views
                .freeze(checked)
                .map_err(|_| refused(FailureKind::UnsupportedStatement))?
        } else {
            checked.sql().to_owned()
        };
        let mut query = Query::new(sql);
        if request.mode != DataMode::Migration {
            bind(&mut query, &request.parameters)?;
        }
        guard.before_statement(position)?;
        if read_only(request) {
            let mut rows = read(conn, query, &request.limits).await?;
            if request.include_schema_digest {
                let selected = schema_scope(request, target, default_schema, scope)?;
                lock_schema_targets(conn, &selected).await?;
                rows.schema_digest = Some(digest(conn, &selected).await?);
            }
            return Ok(DataOutcome::Read(rows));
        }
        let changed = query
            .execute(conn)
            .await
            .map_err(|error| refused(classify(error)))?
            .total();
        result.observed_statements += 1;
        result.schema_changed |= checked.schema_change;
        if !checked.schema_change {
            result.affected_rows = result.affected_rows.unwrap_or(0).checked_add(
                i64::try_from(changed).map_err(|_| refused(FailureKind::ResultLimit))?,
            );
        }
    }
    Ok(DataOutcome::Mutation(result))
}

const RELATION: &str = "SELECT o.object_id, s.name, o.name, CAST(o.type AS nvarchar(2)),
    CAST(CASE WHEN OBJECTPROPERTYEX(o.object_id,'TableHasActiveFulltextIndex')=1
        OR EXISTS (SELECT 1 FROM sys.security_predicates p WHERE p.target_object_id=o.object_id)
        OR EXISTS (SELECT 1 FROM sys.external_tables e WHERE e.object_id=o.object_id)
        OR EXISTS (SELECT 1 FROM sys.computed_columns c WHERE c.object_id=o.object_id AND c.is_persisted=0)
        THEN 1 ELSE 0 END AS bit),
    CAST(CASE WHEN EXISTS (SELECT 1 FROM sys.triggers t WHERE t.parent_id=o.object_id AND t.is_disabled=0)
        OR EXISTS (SELECT 1 FROM sys.foreign_keys k WHERE k.referenced_object_id=o.object_id
                    AND (k.delete_referential_action<>0 OR k.update_referential_action<>0))
        THEN 1 ELSE 0 END AS bit)
FROM sys.objects o JOIN sys.schemas s ON s.schema_id=o.schema_id
WHERE s.name=@P1 AND o.name=@P2 AND o.is_ms_shipped=0 AND o.type IN('U','V')";

type Identity = (i32, String, String, String, bool, bool);

async fn relation(
    conn: &mut Session,
    schema: &str,
    name: &str,
) -> Result<Option<Identity>, DataFailure> {
    let row = conn
        .query(RELATION, &[&schema, &name])
        .await
        .map_err(|e| refused(classify(e)))?
        .into_row()
        .await
        .map_err(|e| refused(classify(e)))?;
    Ok(row.map(|row| {
        (
            row.get(0).expect("object_id"),
            row.get::<&str, _>(1).expect("schema name").to_owned(),
            row.get::<&str, _>(2).expect("object name").to_owned(),
            row.get::<&str, _>(3)
                .expect("object type")
                .trim()
                .to_owned(),
            row.get::<bool, _>(4).unwrap_or(true),
            row.get::<bool, _>(5).unwrap_or(true),
        )
    }))
}

async fn authorize(
    conn: &mut Session,
    request: &DataRequest,
    target: &Target,
    scope: &NameScope,
    default_schema: &str,
    statement: &CheckedStatement,
    guard: &dyn DispatchGuard,
) -> Result<Bindings, DataFailure> {
    let engine = &DbEngine::SqlServer;
    let mut queue = VecDeque::from([statement.read.clone()]);
    let mut definitions = BTreeMap::<i32, Definition>::new();
    let mut bindings = Bindings::default();
    let mut resolved = 0;
    while let Some(footprint) = queue.pop_front() {
        let mut tables = Vec::new();
        for requested in &footprint.relations {
            let (schema, name) = names(engine, requested, &target.catalog, default_schema)?;
            if schema.eq_ignore_ascii_case("sys")
                || schema.eq_ignore_ascii_case("information_schema")
                || name.starts_with('#')
            {
                return Err(refused(FailureKind::PermissionDenied));
            }
            let found = relation(conn, &schema, &name).await?;
            let Some((object, schema, name, kind, unsafe_read, unsafe_write)) = found.as_ref()
            else {
                // SQL Server resolves unqualified objects in the default schema and then dbo.
                // Require that fallback to be explicit so the checked name stays the executed one.
                let is_cte = requested.path.len() == 1
                    && default_schema == "dbo"
                    && footprint
                        .common_tables
                        .iter()
                        .any(|cte| cte.value.eq_ignore_ascii_case(&name));
                if is_cte || statement.created_relation.as_ref() == Some(&requested.path) {
                    check_scope(request, scope, &schema, &name)?;
                    continue;
                }
                return Err(refused(FailureKind::StaleSchema));
            };
            check_scope(request, scope, schema, name)?;
            if *unsafe_read
                || (!read_only(request)
                    && (*unsafe_write || (kind == "V" && !statement.schema_change)))
            {
                return Err(refused(FailureKind::UnsupportedStatement));
            }
            if kind != "V" {
                let qualified = format!("{}.{}", quote(schema), quote(name));
                // Only a verified base table is touched by this fixed lock query. Views use
                // their checked definition and pinned dependencies, even if the view changes.
                let lock = format!("SELECT TOP (1) 1 FROM {qualified} WITH (TABLOCK,HOLDLOCK)");
                control(conn, &lock).await.map_err(refused)?;
            }
            if relation(conn, schema, name).await? != found {
                return Err(refused(FailureKind::StaleSchema));
            }
            let output = conn.query(
                "SELECT TOP (1025) name FROM sys.columns WHERE object_id=@P1 ORDER BY column_id",
                &[object],
            ).await.map_err(|e| refused(classify(e)))?.into_first_result().await.map_err(|e| refused(classify(e)))?;
            if output.is_empty() || output.len() > MAX_COLUMNS {
                return Err(refused(FailureKind::ResultLimit));
            }
            resolved += 1;
            if resolved > MAX_RELATIONS {
                return Err(refused(FailureKind::ResultLimit));
            }
            let columns: Vec<String> = output
                .iter()
                .map(|row| row.get::<&str, _>(0).unwrap_or_default().to_owned())
                .collect();
            if kind == "V" && read_only(request) {
                let view = match definitions.get(object) {
                    Some(view) => view.clone(),
                    None => {
                        let row = conn
                            .query("SELECT OBJECT_DEFINITION(@P1)", &[object])
                            .await
                            .map_err(|e| refused(classify(e)))?
                            .into_row()
                            .await
                            .map_err(|e| refused(classify(e)))?
                            .ok_or_else(|| refused(FailureKind::PermissionDenied))?;
                        let definition: &str = row
                            .get(0)
                            .ok_or_else(|| refused(FailureKind::PermissionDenied))?;
                        let view = Definition::checked(definition, &columns)
                            .map_err(|_| refused(FailureKind::UnsupportedStatement))?;
                        if schema != default_schema
                            && view
                                .statement
                                .read
                                .relations
                                .iter()
                                .any(|relation| relation.path.len() == 1)
                        {
                            return Err(refused(FailureKind::UnsupportedStatement));
                        }
                        queue.push_back(view.statement.read.clone());
                        definitions.insert(*object, view.clone());
                        view
                    }
                };
                bindings.bind(&requested.path, view);
            }
            tables.push(PhysicalTable {
                schema: schema.clone(),
                name: name.clone(),
                columns,
                aliases: requested
                    .alias
                    .iter()
                    .map(|alias| alias.value.clone())
                    .collect(),
            });
        }
        check_columns(engine, &target.catalog, &footprint, &tables, guard)?;
    }
    Ok(bindings)
}

async fn lock_schema_targets(
    conn: &mut Session,
    selected: &BTreeSet<(String, String)>,
) -> Result<(), DataFailure> {
    for (schema, name) in selected {
        if schema.eq_ignore_ascii_case("sys")
            || schema.eq_ignore_ascii_case("information_schema")
            || name.starts_with('#')
        {
            return Err(refused(FailureKind::PermissionDenied));
        }
        let Some(identity) = relation(conn, schema, name).await? else {
            continue;
        };
        let (_, physical_schema, physical_name, kind, unsafe_read, _) = &identity;
        if kind != "U" || *unsafe_read {
            return Err(refused(FailureKind::UnsupportedStatement));
        }
        let lock = format!(
            "SELECT TOP (1) 1 FROM {}.{} WITH (TABLOCK,HOLDLOCK)",
            quote(physical_schema),
            quote(physical_name)
        );
        control(conn, &lock).await.map_err(refused)?;
        if relation(conn, schema, name).await? != Some(identity) {
            return Err(refused(FailureKind::StaleSchema));
        }
    }
    Ok(())
}

async fn digest(
    conn: &mut Session,
    selected: &BTreeSet<(String, String)>,
) -> Result<String, DataFailure> {
    let mut material =
        serde_json::to_vec(selected).map_err(|_| refused(FailureKind::InvalidTarget))?;
    let input = std::str::from_utf8(&material).map_err(|_| refused(FailureKind::InvalidTarget))?;
    let mut rows = conn.query(
        "SELECT (SELECT o.object_id,s.name AS [schema],o.name,CAST(o.type AS nvarchar(2)) AS kind,
          OBJECT_DEFINITION(o.object_id) AS [definition],
          JSON_QUERY((SELECT c.name,TYPE_NAME(c.user_type_id) AS [type],c.max_length,c.precision,c.scale,c.is_nullable,
            c.collation_name,c.is_identity,c.is_computed,
            OBJECT_DEFINITION(c.default_object_id) AS defaultValue, cc.definition AS computedDefinition
            FROM sys.columns c LEFT JOIN sys.computed_columns cc ON cc.object_id=c.object_id AND cc.column_id=c.column_id
            WHERE c.object_id=o.object_id ORDER BY c.column_id FOR JSON PATH)) AS columns,
          JSON_QUERY((SELECT i.name,i.type_desc,i.is_unique,i.is_disabled,i.filter_definition,
            JSON_QUERY((SELECT ic.column_id,ic.key_ordinal,ic.is_descending_key,ic.is_included_column
              FROM sys.index_columns ic WHERE ic.object_id=i.object_id AND ic.index_id=i.index_id
              ORDER BY ic.index_column_id FOR JSON PATH)) AS columns
            FROM sys.indexes i WHERE i.object_id=o.object_id AND i.index_id>0 ORDER BY i.name FOR JSON PATH)) AS indexes,
          JSON_QUERY((SELECT k.name,CAST(k.type AS nvarchar(2)) AS kind,OBJECT_DEFINITION(k.object_id) AS [definition],
            cc.is_disabled,cc.is_not_trusted
            FROM sys.objects k LEFT JOIN sys.check_constraints cc ON cc.object_id=k.object_id
            WHERE k.parent_object_id=o.object_id AND k.type IN('C','D','F','PK','UQ','TR')
            ORDER BY k.name FOR JSON PATH)) AS constraints,
          JSON_QUERY((SELECT f.name,f.referenced_object_id,f.delete_referential_action,f.update_referential_action,
            f.is_disabled,f.is_not_trusted,
            JSON_QUERY((SELECT fc.parent_column_id,fc.referenced_column_id FROM sys.foreign_key_columns fc
              WHERE fc.constraint_object_id=f.object_id ORDER BY fc.constraint_column_id FOR JSON PATH)) AS columns
            FROM sys.foreign_keys f WHERE f.parent_object_id=o.object_id ORDER BY f.name FOR JSON PATH)) AS foreignKeys
          FOR JSON PATH, WITHOUT_ARRAY_WRAPPER)
        FROM sys.objects o JOIN sys.schemas s ON s.schema_id=o.schema_id
        WHERE o.is_ms_shipped=0 AND EXISTS (SELECT 1 FROM OPENJSON(@P1)
          WITH (schema_name nvarchar(128) '$[0]', relation_name nvarchar(128) '$[1]') p
          WHERE p.schema_name COLLATE DATABASE_DEFAULT=s.name AND p.relation_name COLLATE DATABASE_DEFAULT=o.name)
        ORDER BY s.name,o.name,o.type",
        &[&input],
    ).await.map_err(|e| refused(classify(e)))?;
    let mut definitions = Vec::new();
    while let Some(item) = rows.try_next().await.map_err(|e| refused(classify(e)))? {
        let QueryItem::Row(row) = item else { continue };
        let value = row
            .get::<&str, _>(0)
            .ok_or_else(|| refused(FailureKind::StaleSchema))?;
        if value.len().saturating_add(definitions.len()) > 16 * 1024 * 1024 {
            return Err(refused(FailureKind::ResultLimit));
        }
        definitions.extend_from_slice(&(value.len() as u64).to_le_bytes());
        definitions.extend_from_slice(value.as_bytes());
    }
    drop(rows);
    material.extend(definitions);
    Ok(sha256_hex(&material))
}

fn bind<'a>(query: &mut Query<'a>, values: &'a [Scalar]) -> Result<(), DataFailure> {
    for value in values {
        match value {
            Scalar::Null => query.bind(None::<&str>),
            Scalar::Boolean(value) => query.bind(*value),
            Scalar::Integer(value) => query.bind(*value),
            Scalar::Text(value) => query.bind(value),
            Scalar::Bytes(value) => query.bind(value.as_slice()),
            Scalar::Decimal(value) => {
                let (whole, decimal) = value.split_once('.').unwrap_or((value, ""));
                if decimal.len() > 37 || whole.trim_start_matches('-').len() + decimal.len() > 38 {
                    return Err(refused(FailureKind::UnsupportedValue));
                }
                let number = format!("{whole}{decimal}")
                    .parse::<i128>()
                    .map_err(|_| refused(FailureKind::UnsupportedValue))?;
                query.bind(Numeric::new_with_scale(number, decimal.len() as u8));
            }
        }
    }
    Ok(())
}

async fn read(
    conn: &mut Session,
    query: Query<'_>,
    limits: &DataLimits,
) -> Result<ReadResult, DataFailure> {
    let mut stream = query
        .query(conn)
        .await
        .map_err(|error| refused(classify(error)))?;
    let columns = stream
        .columns()
        .await
        .map_err(|error| refused(classify(error)))?
        .ok_or_else(|| refused(FailureKind::UnsupportedStatement))?
        .iter()
        .map(|column| Column {
            name: column.name().into(),
            data_type: format!("{:?}", column.column_type()),
        })
        .collect();
    let mut answer = BoundedRows::new(columns, limits)?;
    while let Some(item) = stream
        .try_next()
        .await
        .map_err(|error| refused(classify(error)))?
    {
        let QueryItem::Row(row) = item else { continue };
        if answer.full() {
            break;
        }
        let values = row
            .columns()
            .iter()
            .enumerate()
            .map(|(index, column)| decode(&row, index, column.column_type()))
            .collect::<Result<Vec<_>, _>>()?;
        answer.push(values)?;
    }
    Ok(answer.result)
}

fn decode(row: &Row, index: usize, kind: ColumnType) -> Result<Scalar, DataFailure> {
    let bad = |_| refused(FailureKind::UnsupportedValue);
    let value = match kind {
        ColumnType::Null => None,
        ColumnType::Bit | ColumnType::Bitn => row
            .try_get::<bool, _>(index)
            .map_err(bad)?
            .map(Scalar::Boolean),
        ColumnType::Int1 => row
            .try_get::<u8, _>(index)
            .map_err(bad)?
            .map(|number| Scalar::Integer(number.into())),
        ColumnType::Int2 => row
            .try_get::<i16, _>(index)
            .map_err(bad)?
            .map(|number| Scalar::Integer(number.into())),
        ColumnType::Int4 => row
            .try_get::<i32, _>(index)
            .map_err(bad)?
            .map(|number| Scalar::Integer(number.into())),
        ColumnType::Int8 => row
            .try_get::<i64, _>(index)
            .map_err(bad)?
            .map(Scalar::Integer),
        // Intn carries the integer width in the decoded cell.
        ColumnType::Intn => match row.get_column_data(index).map_err(bad)? {
            ColumnData::U8(number) => number.map(|number| Scalar::Integer(number.into())),
            ColumnData::I16(number) => number.map(|number| Scalar::Integer(number.into())),
            ColumnData::I32(number) => number.map(|number| Scalar::Integer(number.into())),
            ColumnData::I64(number) => number.map(Scalar::Integer),
            _ => return Err(refused(FailureKind::UnsupportedValue)),
        },
        ColumnType::BigVarChar
        | ColumnType::BigChar
        | ColumnType::NVarchar
        | ColumnType::NChar
        | ColumnType::Text
        | ColumnType::NText => row
            .try_get::<&str, _>(index)
            .map_err(bad)?
            .map(|value| Scalar::Text(value.to_owned())),
        ColumnType::BigVarBin | ColumnType::BigBinary | ColumnType::Image => row
            .try_get::<&[u8], _>(index)
            .map_err(bad)?
            .map(|value| Scalar::Bytes(value.to_vec())),
        ColumnType::Decimaln | ColumnType::Numericn => row
            .try_get::<Numeric, _>(index)
            .map_err(bad)?
            .map(|value| Scalar::Decimal(value.to_string())),
        _ => return Err(refused(FailureKind::UnsupportedValue)),
    };
    Ok(value.unwrap_or(Scalar::Null))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn intn_decodes_each_cell_width_and_its_null() {
        let values = [
            (
                ColumnData::U8(Some(u8::MIN)),
                Scalar::Integer(u8::MIN.into()),
            ),
            (
                ColumnData::U8(Some(u8::MAX)),
                Scalar::Integer(u8::MAX.into()),
            ),
            (
                ColumnData::I16(Some(i16::MIN)),
                Scalar::Integer(i16::MIN.into()),
            ),
            (
                ColumnData::I16(Some(i16::MAX)),
                Scalar::Integer(i16::MAX.into()),
            ),
            (
                ColumnData::I32(Some(i32::MIN)),
                Scalar::Integer(i32::MIN.into()),
            ),
            (
                ColumnData::I32(Some(i32::MAX)),
                Scalar::Integer(i32::MAX.into()),
            ),
            (ColumnData::I64(Some(i64::MIN)), Scalar::Integer(i64::MIN)),
            (ColumnData::I64(Some(i64::MAX)), Scalar::Integer(i64::MAX)),
            (ColumnData::U8(None), Scalar::Null),
            (ColumnData::I16(None), Scalar::Null),
            (ColumnData::I32(None), Scalar::Null),
            (ColumnData::I64(None), Scalar::Null),
        ];
        for (value, expected) in values {
            let row = Row::builder()
                .column("number", ColumnType::Intn, value)
                .build();
            assert!(decode(&row, 0, ColumnType::Intn).unwrap() == expected);
        }
        let row = Row::builder()
            .column("number", ColumnType::Intn, ColumnData::Bit(Some(true)))
            .build();
        assert!(matches!(
            decode(&row, 0, ColumnType::Intn),
            Err(DataFailure::Refused(FailureKind::UnsupportedValue))
        ));
    }
}
