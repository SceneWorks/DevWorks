use super::super::data::*;
use super::{connect, qualified_release};
use crate::database::{NameScope, network::Target};
use devworks_core::fingerprint::sha256_hex;
use devworks_core::ports::database::data::*;
use devworks_core::protocol::records::DbEngine;
use devworks_core::services::database::statement::{CheckedStatement, DataMode, check};
use futures_util::TryStreamExt;
use std::collections::{BTreeSet, VecDeque};
use tiberius::numeric::Numeric;
use tiberius::{Client, ColumnType, Query, QueryItem, Row};
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
        authorize(conn, request, target, scope, default_schema, checked, guard).await?;
        if position == 0 {
            if let Some(expected) = &request.expected_schema_digest
                && expected != &digest(conn, request).await?
            {
                return Err(refused(FailureKind::StaleSchema));
            }
        }
        let mut query = Query::new(checked.sql());
        if request.mode != DataMode::Migration {
            bind(&mut query, &request.parameters)?;
        }
        guard.before_statement(position)?;
        if read_only(request) {
            let mut rows = read(conn, query, &request.limits).await?;
            if request.include_schema_digest {
                rows.schema_digest = Some(digest(conn, request).await?);
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
) -> Result<(), DataFailure> {
    let engine = &DbEngine::SqlServer;
    let mut queue = VecDeque::from([statement.read.clone()]);
    let mut views = BTreeSet::new();
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
                if is_cte || statement.schema_change {
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
            let qualified = format!("{}.{}", quote(schema), quote(name));
            // Take a transaction-held table lock using a fixed constant. The following catalog
            // check binds case-insensitive SQL resolution to the server's physical identity.
            let lock = format!("SELECT TOP (1) 1 FROM {qualified} WITH (TABLOCK,HOLDLOCK)");
            control(conn, &lock).await.map_err(refused)?;
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
            if kind == "V" && views.insert(*object) {
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
                let tree = check(engine, DataMode::Schema, definition)
                    .map_err(|_| refused(FailureKind::UnsupportedStatement))?;
                // The view's own CREATE target is already checked and locked.
                let mut dependencies = tree[0].read.clone();
                dependencies.relations.retain(|candidate| {
                    !names(engine, candidate, &target.catalog, default_schema).is_ok_and(
                        |(s, n)| s.eq_ignore_ascii_case(schema) && n.eq_ignore_ascii_case(name),
                    )
                });
                queue.push_back(dependencies);
            }
            tables.push(PhysicalTable {
                schema: schema.clone(),
                name: name.clone(),
                columns: output
                    .iter()
                    .map(|row| row.get::<&str, _>(0).unwrap_or_default().to_owned())
                    .collect(),
                aliases: requested
                    .alias
                    .iter()
                    .map(|alias| alias.value.clone())
                    .collect(),
            });
        }
        check_columns(engine, &target.catalog, &footprint, &tables, guard)?;
    }
    Ok(())
}

async fn digest(conn: &mut Session, request: &DataRequest) -> Result<String, DataFailure> {
    let schemas = request
        .config
        .schemas
        .0
        .iter()
        .map(|name| name.0.as_str())
        .collect::<Vec<_>>();
    let mut rows = conn.simple_query(
        "SELECT s.name, (SELECT s.name AS [schema],o.name,CAST(o.type AS nvarchar(2)) AS kind,
          OBJECT_DEFINITION(o.object_id) AS [definition],
          JSON_QUERY((SELECT c.name,TYPE_NAME(c.user_type_id) AS [type],c.max_length,c.precision,c.scale,c.is_nullable,
            OBJECT_DEFINITION(c.default_object_id) AS defaultValue
            FROM sys.columns c WHERE c.object_id=o.object_id ORDER BY c.column_id FOR JSON PATH)) AS columns,
          JSON_QUERY((SELECT i.name, i.type_desc, i.is_unique, i.filter_definition,
            JSON_QUERY((SELECT ic.column_id,ic.key_ordinal,ic.is_descending_key
              FROM sys.index_columns ic WHERE ic.object_id=i.object_id AND ic.index_id=i.index_id
              ORDER BY ic.index_column_id FOR JSON PATH)) AS columns
            FROM sys.indexes i WHERE i.object_id=o.object_id AND i.index_id>0 ORDER BY i.name FOR JSON PATH)) AS indexes
          FOR JSON PATH, WITHOUT_ARRAY_WRAPPER)
        FROM sys.objects o JOIN sys.schemas s ON s.schema_id=o.schema_id
        WHERE o.is_ms_shipped=0 ORDER BY s.name,o.name,o.type",
    ).await.map_err(|e| refused(classify(e)))?;
    let mut material = Vec::new();
    while let Some(item) = rows.try_next().await.map_err(|e| refused(classify(e)))? {
        let QueryItem::Row(row) = item else { continue };
        let schema = row
            .get::<&str, _>(0)
            .ok_or_else(|| refused(FailureKind::StaleSchema))?;
        if !schemas.is_empty() && !schemas.contains(&schema) {
            continue;
        }
        let value = row
            .get::<&str, _>(1)
            .ok_or_else(|| refused(FailureKind::StaleSchema))?;
        if value.len().saturating_add(material.len()) > 16 * 1024 * 1024 {
            return Err(refused(FailureKind::ResultLimit));
        }
        material.extend_from_slice(&(value.len() as u64).to_le_bytes());
        material.extend_from_slice(value.as_bytes());
    }
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
        ColumnType::Int8 | ColumnType::Intn => row
            .try_get::<i64, _>(index)
            .map_err(bad)?
            .map(Scalar::Integer),
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
