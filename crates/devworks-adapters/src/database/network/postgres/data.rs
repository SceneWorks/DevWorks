//! The selected PostgreSQL database stays on one physical session through rollback or commit.
use super::super::data::*;
use super::{open, qualified_release};
use crate::database::{NameScope, network::Target};
use devworks_core::fingerprint::sha256_hex;
use devworks_core::ports::database::data::*;
use devworks_core::protocol::records::DbEngine;
use devworks_core::services::database::statement::{CheckedStatement, DataMode, check};
use futures_util::{StreamExt, pin_mut};
use std::collections::{BTreeSet, VecDeque};
use tokio_postgres::types::{ToSql, Type};
use tokio_postgres::{Client, Error, Statement};

const RELEASES: &[&str] = &["17"];
const MAX_RELATIONS: usize = 128;
const MAX_COLUMNS: usize = 1024;

fn classify(error: Error) -> FailureKind {
    match error.code().map(|state| state.code()) {
        Some("42501" | "25006" | "28P01") => FailureKind::PermissionDenied,
        Some("42P01" | "42703" | "3F000") => FailureKind::StaleSchema,
        Some(code) if code.starts_with("23") => FailureKind::Constraint,
        Some("57014") => FailureKind::Deadline,
        Some(code) if code.starts_with("42") => FailureKind::UnsupportedStatement,
        _ => FailureKind::Unavailable,
    }
}

fn quote(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

pub(crate) async fn execute(
    request: DataRequest,
    target: Target,
    guard: &dyn DispatchGuard,
    boundary: &Boundary,
) -> Result<DataOutcome, DataFailure> {
    let (client, driver) = open(
        &target,
        credentials(&request),
        &target.catalog,
        "DevWorks data",
    )
    .await
    .map_err(metadata_failure)?;
    let work = session(&client, &target, &request, guard, boundary);
    pin_mut!(driver);
    tokio::select! {
        biased;
        result = work => result,
        _ = &mut driver => Err(boundary.interrupted(FailureKind::Unavailable)),
    }
}

async fn session(
    client: &Client,
    target: &Target,
    request: &DataRequest,
    guard: &dyn DispatchGuard,
    boundary: &Boundary,
) -> Result<DataOutcome, DataFailure> {
    let info = client
        .query_one(
            "SELECT current_database(), current_setting('server_version')",
            &[],
        )
        .await
        .map_err(|error| refused(classify(error)))?;
    if info.get::<_, &str>(0) != target.catalog {
        return Err(refused(FailureKind::InvalidTarget));
    }
    qualified_release(info.get(1), 1, RELEASES, "PostgreSQL 17").map_err(metadata_failure)?;
    let scope = NameScope::new(&request.config).map_err(metadata_failure)?;
    let default_schema = request
        .config
        .schemas
        .0
        .first()
        .map(|schema| schema.0.as_str())
        .unwrap_or("public");
    if matches!(default_schema, "pg_catalog" | "information_schema")
        || default_schema.starts_with("pg_")
    {
        return Err(refused(FailureKind::InvalidTarget));
    }
    let initial = if read_only(request) {
        "BEGIN READ ONLY"
    } else {
        "BEGIN READ WRITE"
    };
    client
        .batch_execute(initial)
        .await
        .map_err(|error| refused(classify(error)))?;
    let pending = transaction(client, target, request, guard, &scope, default_schema).await;
    let result = match pending {
        Ok(DataOutcome::Read(rows)) => {
            client
                .batch_execute("ROLLBACK")
                .await
                .map_err(|error| refused(classify(error)))?;
            return Ok(DataOutcome::Read(rows));
        }
        Ok(DataOutcome::Mutation(result)) => result,
        Err(failure) => {
            let rolled = client.batch_execute("ROLLBACK").await.is_ok();
            return Err(match failure {
                DataFailure::Refused(reason) if rolled && !read_only(request) => {
                    DataFailure::RolledBack(reason)
                }
                failure => failure,
            });
        }
    };
    if let Err(failure) = guard.before_commit() {
        let rolled = client.batch_execute("ROLLBACK").await.is_ok();
        return Err(match failure {
            DataFailure::Refused(reason) if rolled => DataFailure::RolledBack(reason),
            failure => failure,
        });
    }
    boundary.committing();
    if client.batch_execute("COMMIT").await.is_err() {
        return Err(boundary.interrupted(FailureKind::Unavailable));
    }
    boundary.observed(guard, request.statements.len() - 1, result)?;
    Ok(DataOutcome::Mutation(result))
}

async fn transaction(
    client: &Client,
    target: &Target,
    request: &DataRequest,
    guard: &dyn DispatchGuard,
    scope: &NameScope,
    default_schema: &str,
) -> Result<DataOutcome, DataFailure> {
    // PostgreSQL searches pg_catalog implicitly. Objects created without a schema must go into
    // the selected application schema. Callable built-ins are qualified in the checked syntax.
    let statement = format!("SET LOCAL search_path = {}", quote(default_schema));
    client
        .batch_execute(&statement)
        .await
        .map_err(|error| refused(classify(error)))?;
    let mut mutation = MutationResult {
        affected_rows: None,
        observed_statements: 0,
        schema_changed: false,
    };
    for (position, sql) in request.statements.iter().enumerate() {
        authorize(client, target, request, scope, default_schema, sql, guard).await?;
        if position == 0 {
            if let Some(expected) = &request.expected_schema_digest {
                if &digest(client, request).await? != expected {
                    return Err(refused(FailureKind::StaleSchema));
                }
            }
        }
        let statement = client
            .prepare(sql.sql())
            .await
            .map_err(|error| refused(classify(error)))?;
        let parameters = if request.mode == DataMode::Migration {
            &[]
        } else {
            request.parameters.as_slice()
        };
        let arguments = bind(&statement, parameters)?;
        guard.before_statement(position)?;
        if read_only(request) {
            let mut rows = read(client, &statement, arguments, &request.limits).await?;
            if request.include_schema_digest {
                rows.schema_digest = Some(digest(client, request).await?);
            }
            return Ok(DataOutcome::Read(rows));
        }
        let changed = client
            .execute_raw(
                &statement,
                arguments
                    .iter()
                    .map(|value| value.as_ref() as &(dyn ToSql + Sync)),
            )
            .await
            .map_err(|error| refused(classify(error)))?;
        mutation.observed_statements += 1;
        mutation.schema_changed |= sql.schema_change;
        if !sql.schema_change {
            mutation.affected_rows = mutation.affected_rows.unwrap_or(0).checked_add(
                i64::try_from(changed).map_err(|_| refused(FailureKind::ResultLimit))?,
            );
        }
    }
    Ok(DataOutcome::Mutation(mutation))
}

const RELATION: &str = "SELECT c.oid,c.relkind::text,c.relrowsecurity,
    EXISTS (SELECT 1 FROM pg_catalog.pg_inherits i WHERE i.inhparent=c.oid),
    EXISTS (SELECT 1 FROM pg_catalog.pg_trigger t WHERE t.tgrelid=c.oid AND t.tgenabled<>'D' AND NOT t.tgisinternal),
    EXISTS (SELECT 1 FROM pg_catalog.pg_rewrite r WHERE r.ev_class=c.oid AND r.rulename<>'_RETURN')
FROM pg_catalog.pg_class c JOIN pg_catalog.pg_namespace n ON n.oid=c.relnamespace
WHERE n.nspname=$1 AND c.relname=$2";

async fn authorize(
    client: &Client,
    target: &Target,
    request: &DataRequest,
    scope: &NameScope,
    default_schema: &str,
    statement: &CheckedStatement,
    guard: &dyn DispatchGuard,
) -> Result<(), DataFailure> {
    let mut queue = VecDeque::new();
    let mut seen = BTreeSet::new();
    queue.push_back(statement.read.clone());
    while let Some(footprint) = queue.pop_front() {
        let mut tables = Vec::new();
        for relation in &footprint.relations {
            let (schema, name) = names(
                &DbEngine::Postgres,
                relation,
                &target.catalog,
                default_schema,
            )?;
            if schema.starts_with("pg_") || schema == "information_schema" {
                return Err(refused(FailureKind::PermissionDenied));
            }
            check_scope(request, scope, &schema, &name)?;
            let rows = client
                .query_opt(RELATION, &[&schema, &name])
                .await
                .map_err(|e| refused(classify(e)))?;
            let Some(row) = rows else {
                let cte = relation.path.len() == 1
                    && footprint
                        .common_tables
                        .iter()
                        .any(|cte| normalized(&DbEngine::Postgres, cte) == name);
                if cte || statement.schema_change {
                    continue;
                }
                return Err(refused(FailureKind::StaleSchema));
            };
            let expected: u32 = row.get(0);
            let kind: &str = row.get(1);
            if !matches!(kind, "r" | "v")
                || row.get::<_, bool>(2)
                || row.get::<_, bool>(3)
                || row.get::<_, bool>(5)
                || (!read_only(request)
                    && (row.get::<_, bool>(4) || kind == "v" && !statement.schema_change))
            {
                return Err(refused(FailureKind::UnsupportedStatement));
            }
            let relation_name = format!("{}.{}", quote(&schema), quote(&name));
            let lock = format!("LOCK TABLE {relation_name} IN ACCESS SHARE MODE");
            client
                .batch_execute(&lock)
                .await
                .map_err(|e| refused(classify(e)))?;
            let actual = client
                .query_opt(RELATION, &[&schema, &name])
                .await
                .map_err(|e| refused(classify(e)))?;
            if !actual.is_some_and(|actual| {
                (0..6).all(|index| match index {
                    0 => actual.get::<_, u32>(index) == expected,
                    1 => actual.get::<_, &str>(index) == kind,
                    _ => actual.get::<_, bool>(index) == row.get::<_, bool>(index),
                })
            }) {
                return Err(refused(FailureKind::StaleSchema));
            }
            let columns = client
                .query(
                    "SELECT a.attname FROM pg_catalog.pg_attribute a
                 JOIN pg_catalog.pg_type t ON t.oid=a.atttypid
                 JOIN pg_catalog.pg_namespace n ON n.oid=t.typnamespace
                 WHERE a.attrelid=$1 AND a.attnum>0 AND NOT a.attisdropped
                 AND n.nspname='pg_catalog' ORDER BY a.attnum LIMIT 1025",
                    &[&expected],
                )
                .await
                .map_err(|e| refused(classify(e)))?;
            let all: i64 = client.query_one(
                "SELECT count(*) FROM pg_catalog.pg_attribute WHERE attrelid=$1 AND attnum>0 AND NOT attisdropped",
                &[&expected]
            ).await.map_err(|e| refused(classify(e)))?.get(0);
            if columns.len() > MAX_COLUMNS || columns.len() as i64 != all {
                return Err(refused(FailureKind::UnsupportedValue));
            }
            if seen.insert(expected) && kind == "v" {
                if seen.len() > MAX_RELATIONS {
                    return Err(refused(FailureKind::ResultLimit));
                }
                let source: String = client
                    .query_one(
                        "SELECT pg_catalog.pg_get_viewdef($1::oid, false)",
                        &[&expected],
                    )
                    .await
                    .map_err(|e| refused(classify(e)))?
                    .get(0);
                let definition = check(&DbEngine::Postgres, DataMode::Read, &source)
                    .map_err(|_| refused(FailureKind::UnsupportedStatement))?;
                queue.push_back(definition[0].read.clone());
            }
            tables.push(PhysicalTable {
                schema,
                name,
                columns: columns.into_iter().map(|row| row.get(0)).collect(),
                aliases: relation
                    .alias
                    .iter()
                    .map(|name| normalized(&DbEngine::Postgres, name))
                    .collect(),
            });
        }
        if tables.len() > MAX_RELATIONS {
            return Err(refused(FailureKind::ResultLimit));
        }
        check_columns(
            &DbEngine::Postgres,
            &target.catalog,
            &footprint,
            &tables,
            guard,
        )?;
    }
    Ok(())
}

async fn digest(client: &Client, request: &DataRequest) -> Result<String, DataFailure> {
    let schemas = request
        .config
        .schemas
        .0
        .iter()
        .map(|schema| schema.0.clone())
        .collect::<Vec<_>>();
    let statement = client.prepare(
        "SELECT jsonb_build_array(n.nspname,c.relname,c.relkind,a.attname,a.atttypid,
           a.atttypmod,a.attnotnull,pg_get_expr(d.adbin,d.adrelid),
           CASE c.relkind WHEN 'v' THEN pg_get_viewdef(c.oid,false) WHEN 'i' THEN pg_get_indexdef(c.oid) END,
           (SELECT jsonb_agg(pg_get_constraintdef(x.oid) ORDER BY x.conname)
            FROM pg_catalog.pg_constraint x WHERE x.conrelid=c.oid))::text
         FROM pg_catalog.pg_class c JOIN pg_catalog.pg_namespace n ON n.oid=c.relnamespace
         LEFT JOIN pg_catalog.pg_attribute a ON a.attrelid=c.oid AND a.attnum>0 AND NOT a.attisdropped
         LEFT JOIN pg_catalog.pg_attrdef d ON d.adrelid=c.oid AND d.adnum=a.attnum
         WHERE n.nspname NOT LIKE 'pg_%' AND n.nspname<>'information_schema'
           AND (cardinality($1::text[])=0 OR n.nspname=ANY($1))
         ORDER BY n.nspname,c.relname,a.attnum"
    ).await.map_err(|e| refused(classify(e)))?;
    let parameters: [&(dyn ToSql + Sync); 1] = [&schemas];
    let rows = client
        .query_raw(&statement, parameters)
        .await
        .map_err(|e| refused(classify(e)))?;
    pin_mut!(rows);
    let mut material = Vec::new();
    while let Some(row) = rows.next().await {
        let row = row.map_err(|e| refused(classify(e)))?;
        let value: &str = row.get(0);
        if value.len().saturating_add(material.len()) > 16 * 1024 * 1024 {
            return Err(refused(FailureKind::ResultLimit));
        }
        material.extend_from_slice(&(value.len() as u64).to_le_bytes());
        material.extend_from_slice(value.as_bytes());
    }
    Ok(sha256_hex(&material))
}

fn bind(
    statement: &Statement,
    parameters: &[Scalar],
) -> Result<Vec<Box<dyn ToSql + Sync + Send>>, DataFailure> {
    if statement.params().len() != parameters.len() {
        return Err(refused(FailureKind::UnsupportedValue));
    }
    statement
        .params()
        .iter()
        .zip(parameters)
        .map(|(kind, value)| {
            let data: Box<dyn ToSql + Sync + Send> = match (kind, value) {
                (&Type::BOOL, Scalar::Boolean(value)) => Box::new(*value),
                (&Type::INT2, Scalar::Integer(value)) => Box::new(
                    i16::try_from(*value).map_err(|_| refused(FailureKind::UnsupportedValue))?,
                ),
                (&Type::INT4, Scalar::Integer(value)) => Box::new(
                    i32::try_from(*value).map_err(|_| refused(FailureKind::UnsupportedValue))?,
                ),
                (&Type::INT8, Scalar::Integer(value)) => Box::new(*value),
                (
                    &Type::TEXT | &Type::VARCHAR | &Type::BPCHAR | &Type::NAME,
                    Scalar::Text(value),
                ) => Box::new(value.clone()),
                (&Type::BYTEA, Scalar::Bytes(value)) => Box::new(value.clone()),
                (&Type::BOOL, Scalar::Null) => Box::new(None::<bool>),
                (&Type::INT2, Scalar::Null) => Box::new(None::<i16>),
                (&Type::INT4, Scalar::Null) => Box::new(None::<i32>),
                (&Type::INT8, Scalar::Null) => Box::new(None::<i64>),
                (&Type::TEXT | &Type::VARCHAR | &Type::BPCHAR | &Type::NAME, Scalar::Null) => {
                    Box::new(None::<String>)
                }
                (&Type::BYTEA, Scalar::Null) => Box::new(None::<Vec<u8>>),
                _ => return Err(refused(FailureKind::UnsupportedValue)),
            };
            Ok(data)
        })
        .collect()
}

async fn read(
    client: &Client,
    statement: &Statement,
    values: Vec<Box<dyn ToSql + Sync + Send>>,
    limits: &DataLimits,
) -> Result<ReadResult, DataFailure> {
    let columns = statement
        .columns()
        .iter()
        .map(|column| Column {
            name: column.name().into(),
            data_type: column.type_().name().into(),
        })
        .collect();
    let mut answer = BoundedRows::new(columns, limits)?;
    let rows = client
        .query_raw(
            statement,
            values
                .iter()
                .map(|value| value.as_ref() as &(dyn ToSql + Sync)),
        )
        .await
        .map_err(|e| refused(classify(e)))?;
    pin_mut!(rows);
    while let Some(row) = rows.next().await {
        let row = row.map_err(|e| refused(classify(e)))?;
        if answer.full() {
            break;
        }
        let cells = row
            .columns()
            .iter()
            .enumerate()
            .map(|(index, column)| decode(&row, index, column.type_()))
            .collect::<Result<Vec<_>, _>>()?;
        answer.push(cells)?;
    }
    Ok(answer.result)
}

fn decode(row: &tokio_postgres::Row, index: usize, kind: &Type) -> Result<Scalar, DataFailure> {
    let failure = |_| refused(FailureKind::UnsupportedValue);
    let value = match *kind {
        Type::BOOL => row
            .try_get::<_, Option<bool>>(index)
            .map_err(failure)?
            .map(Scalar::Boolean),
        Type::INT2 => row
            .try_get::<_, Option<i16>>(index)
            .map_err(failure)?
            .map(|n| Scalar::Integer(n.into())),
        Type::INT4 => row
            .try_get::<_, Option<i32>>(index)
            .map_err(failure)?
            .map(|n| Scalar::Integer(n.into())),
        Type::INT8 => row
            .try_get::<_, Option<i64>>(index)
            .map_err(failure)?
            .map(Scalar::Integer),
        Type::TEXT | Type::VARCHAR | Type::BPCHAR | Type::NAME => row
            .try_get::<_, Option<String>>(index)
            .map_err(failure)?
            .map(Scalar::Text),
        Type::BYTEA => row
            .try_get::<_, Option<Vec<u8>>>(index)
            .map_err(failure)?
            .map(Scalar::Bytes),
        _ => return Err(refused(FailureKind::UnsupportedValue)),
    };
    Ok(value.unwrap_or(Scalar::Null))
}
