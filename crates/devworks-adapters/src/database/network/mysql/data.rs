use super::super::data::*;
use super::{open, qualified_release};
use crate::database::{NameScope, network::Target};
use devworks_core::ports::database::data::{Column as DataColumn, *};
use devworks_core::protocol::records::DbEngine;
use devworks_core::services::database::statement::{CheckedStatement, DataMode, Inspection, check};
use futures_util::TryStreamExt;
use sqlx::SqlSafeStr;
use sqlx::mysql::{MySqlArguments, MySqlRow};
use sqlx::{
    Arguments, AssertSqlSafe, Column, Connection, Executor, MySqlConnection, Row, Statement,
    TypeInfo, ValueRef,
};
use std::collections::{BTreeMap, BTreeSet, VecDeque};

const MAX_RELATIONS: usize = 128;
const MAX_COLUMNS: usize = 1024;

fn classify(error: sqlx::Error) -> FailureKind {
    let code = error
        .as_database_error()
        .and_then(|error| error.try_downcast_ref::<sqlx::mysql::MySqlDatabaseError>())
        .map(sqlx::mysql::MySqlDatabaseError::number);
    match code {
        Some(1044 | 1045 | 1142 | 1143 | 1227 | 1792) => FailureKind::PermissionDenied,
        Some(1054 | 1146 | 1615) => FailureKind::StaleSchema,
        Some(1048 | 1062 | 1451 | 1452 | 3819 | 4025) => FailureKind::Constraint,
        Some(1205 | 1969 | 3024) => FailureKind::Deadline,
        Some(1064 | 1235 | 1295) => FailureKind::UnsupportedStatement,
        _ => FailureKind::Unavailable,
    }
}

fn quote(name: &str) -> String {
    format!("`{}`", name.replace('`', "``"))
}

pub(crate) async fn execute(
    request: DataRequest,
    target: Target,
    guard: &dyn DispatchGuard,
    boundary: &Boundary,
) -> Result<DataOutcome, DataFailure> {
    let maria = matches!(request.config.engine, DbEngine::MariaDb);
    let mut conn = open(&target, credentials(&request), Some(&target.catalog))
        .await
        .map_err(metadata_failure)?;
    let info: (String, Option<String>) = sqlx::query_as("SELECT VERSION(), DATABASE()")
        .fetch_one(&mut conn)
        .await
        .map_err(|error| refused(classify(error)))?;
    if maria != info.0.contains("MariaDB") || info.1.as_deref() != Some(target.catalog.as_str()) {
        return Err(refused(FailureKind::InvalidTarget));
    }
    if maria {
        qualified_release(&info.0, 2, &["11.4"], "MariaDB 11.4").map_err(metadata_failure)?;
    } else {
        qualified_release(&info.0, 2, &["8.4"], "MySQL 8.4").map_err(metadata_failure)?;
    }
    sqlx::raw_sql(
        "SET SESSION sql_mode='STRICT_ALL_TABLES,NO_ENGINE_SUBSTITUTION,NO_BACKSLASH_ESCAPES';
         SET SESSION lock_wait_timeout=2; SET SESSION innodb_lock_wait_timeout=2;",
    )
    .execute(&mut conn)
    .await
    .map_err(|error| refused(classify(error)))?;
    sqlx::raw_sql(if maria {
        "SET SESSION max_statement_time=30"
    } else {
        "SET SESSION max_execution_time=30000"
    })
    .execute(&mut conn)
    .await
    .map_err(|error| refused(classify(error)))?;
    let scope = NameScope::new(&request.config).map_err(metadata_failure)?;
    if matches!(request.mode, DataMode::Schema | DataMode::Migration) {
        return schema_change(&mut conn, &request, &target, guard, &scope, boundary).await;
    }
    sqlx::raw_sql(if read_only(&request) {
        "START TRANSACTION READ ONLY"
    } else {
        "START TRANSACTION READ WRITE"
    })
    .execute(&mut conn)
    .await
    .map_err(|error| refused(classify(error)))?;
    let mut result = run_statement(&mut conn, &request, &target, guard, &scope, 0).await;
    if request.include_schema_digest
        && let Ok(DataOutcome::Read(rows)) = &mut result
    {
        let selected = schema_scope(&request, &target, &target.catalog, &scope)?;
        prime_metadata_locks(&mut conn, &selected).await?;
        rows.schema_digest = Some(schema_digest(
            &signatures(&mut conn, &target, &selected, &request.config.engine).await?,
        )?);
    }
    match result {
        Ok(DataOutcome::Read(rows)) => {
            sqlx::raw_sql("ROLLBACK")
                .execute(&mut conn)
                .await
                .map_err(|error| refused(classify(error)))?;
            conn.close()
                .await
                .map_err(|error| refused(classify(error)))?;
            Ok(DataOutcome::Read(rows))
        }
        Ok(DataOutcome::Mutation(result)) => {
            if let Err(error) = guard.before_commit() {
                return Err(rollback(&mut conn, error).await);
            }
            boundary.committing();
            sqlx::raw_sql("COMMIT")
                .execute(&mut conn)
                .await
                .map_err(|error| boundary.interrupted(classify(error)))?;
            boundary.observed(guard, 0, result)?;
            let _ = conn.close().await;
            Ok(DataOutcome::Mutation(result))
        }
        Err(failure) if read_only(&request) => Err(failure),
        Err(failure) => Err(rollback(&mut conn, failure).await),
    }
}

async fn rollback(conn: &mut MySqlConnection, failure: DataFailure) -> DataFailure {
    match (sqlx::raw_sql("ROLLBACK").execute(conn).await, failure) {
        (Ok(_), DataFailure::Refused(reason)) => DataFailure::RolledBack(reason),
        (_, failure) => failure,
    }
}

async fn run_statement(
    conn: &mut MySqlConnection,
    request: &DataRequest,
    target: &Target,
    guard: &dyn DispatchGuard,
    scope: &NameScope,
    position: usize,
) -> Result<DataOutcome, DataFailure> {
    let statement = &request.statements[position];
    if let Some(Inspection::TableListing { catalog, .. }) = &statement.inspection {
        let selected = catalog
            .as_ref()
            .is_none_or(|path| path.len() == 1 && path[0].value == target.catalog);
        if !selected
            || (!request.config.schemas.0.is_empty()
                && !request
                    .config
                    .schemas
                    .0
                    .iter()
                    .any(|name| name.0 == target.catalog))
        {
            return Err(refused(FailureKind::PermissionDenied));
        }
    }
    authorize(conn, request, target, guard, scope, statement).await?;
    guard.before_statement(position)?;
    let values = if request.mode == DataMode::Migration {
        &[]
    } else {
        request.parameters.as_slice()
    };
    let arguments = arguments(values)?;
    if read_only(request) {
        let description = conn
            .prepare(AssertSqlSafe(statement.sql()).into_sql_str())
            .await
            .map_err(|error| refused(classify(error)))?;
        let columns: Vec<_> = description
            .columns()
            .iter()
            .map(|column| DataColumn {
                name: column.name().into(),
                data_type: column.type_info().name().into(),
            })
            .collect();
        let mut answer = BoundedRows::new(columns, &request.limits)?;
        let mut rows = sqlx::query_with(AssertSqlSafe(statement.sql()), arguments).fetch(conn);
        while let Some(row) = rows
            .try_next()
            .await
            .map_err(|error| refused(classify(error)))?
        {
            if !visible_listing(&statement.inspection, &row, request, target, scope) {
                continue;
            }
            if answer.full() {
                break;
            }
            let cells = row
                .columns()
                .iter()
                .enumerate()
                .map(|(index, column)| decode(&row, index, column.type_info().name()))
                .collect::<Result<Vec<_>, _>>()?;
            answer.push(cells)?;
        }
        Ok(DataOutcome::Read(answer.result))
    } else {
        let result = sqlx::query_with(AssertSqlSafe(statement.sql()), arguments)
            .execute(conn)
            .await
            .map_err(|error| refused(classify(error)))?;
        Ok(DataOutcome::Mutation(MutationResult {
            affected_rows: Some(
                i64::try_from(result.rows_affected())
                    .map_err(|_| refused(FailureKind::ResultLimit))?,
            ),
            observed_statements: 1,
            schema_changed: false,
        }))
    }
}

fn visible_listing(
    inspection: &Option<Inspection>,
    row: &MySqlRow,
    request: &DataRequest,
    target: &Target,
    scope: &NameScope,
) -> bool {
    let get = |index| row.try_get::<String, _>(index).ok();
    match inspection {
        Some(Inspection::TableListing { views_only, .. }) => {
            get(0).is_some_and(|name| check_scope(request, scope, &target.catalog, &name).is_ok())
                && (!views_only || get(1).as_deref() == Some("VIEW"))
        }
        Some(Inspection::CatalogListing) => get(0).as_deref() == Some(&target.catalog),
        Some(Inspection::RoutineListing) => {
            get(0).as_deref() == Some(&target.catalog)
                && get(1)
                    .is_some_and(|name| check_scope(request, scope, &target.catalog, &name).is_ok())
        }
        Some(Inspection::Variables(selected)) => get(0).is_some_and(|name| {
            selected.as_ref().is_none_or(|selected| selected == &name)
                && matches!(
                    name.as_str(),
                    "autocommit"
                        | "character_set_client"
                        | "character_set_connection"
                        | "character_set_results"
                        | "collation_connection"
                        | "sql_mode"
                        | "time_zone"
                        | "transaction_isolation"
                        | "tx_isolation"
                        | "version"
                )
        }),
        _ => true,
    }
}

const TABLE: &str = "SELECT TABLE_NAME,TABLE_TYPE,ENGINE
    FROM INFORMATION_SCHEMA.TABLES WHERE BINARY TABLE_SCHEMA=BINARY ? AND BINARY TABLE_NAME=BINARY ?";

async fn authorize(
    conn: &mut MySqlConnection,
    request: &DataRequest,
    target: &Target,
    guard: &dyn DispatchGuard,
    scope: &NameScope,
    statement: &CheckedStatement,
) -> Result<(), DataFailure> {
    let engine = &request.config.engine;
    let statement_table_inspection =
        matches!(statement.inspection, Some(Inspection::TableDefinition));
    let mut queue = VecDeque::from([statement.read.clone()]);
    let mut views = BTreeSet::new();
    let mut resolved = 0;
    while let Some(mut footprint) = queue.pop_front() {
        let mut tables = Vec::new();
        for relation in &footprint.relations {
            let (schema, name) = names(engine, relation, &target.catalog, &target.catalog)?;
            check_scope(request, scope, &schema, &name)?;
            let identity = sqlx::query_as::<_, (String, String, Option<String>)>(TABLE)
                .bind(&schema)
                .bind(&name)
                .fetch_optional(&mut *conn)
                .await
                .map_err(|e| refused(classify(e)))?;
            let Some(identity) = identity else {
                let cte = relation.path.len() == 1
                    && footprint
                        .common_tables
                        .iter()
                        .any(|cte| cte.value.eq_ignore_ascii_case(&name));
                if cte || statement.created_relation.as_ref() == Some(&relation.path) {
                    continue;
                }
                return Err(refused(FailureKind::StaleSchema));
            };
            let allowed_base =
                identity.1 == "BASE TABLE" && identity.2.as_deref() == Some("InnoDB");
            if !allowed_base && !(read_only(request) && identity.1 == "VIEW") {
                return Err(refused(FailureKind::UnsupportedStatement));
            }
            // PREPARE releases its metadata locks. A fixed query acquires an execution-duration
            // lock on the physical name, which the active transaction retains through settlement.
            let fence = format!(
                "SELECT 1 FROM {}.{} LIMIT 0",
                quote(&schema),
                quote(&identity.0)
            );
            sqlx::raw_sql(AssertSqlSafe(fence))
                .execute(&mut *conn)
                .await
                .map_err(|e| refused(classify(e)))?;
            let actual = sqlx::query_as::<_, (String, String, Option<String>)>(TABLE)
                .bind(&schema)
                .bind(&name)
                .fetch_optional(&mut *conn)
                .await
                .map_err(|e| refused(classify(e)))?;
            if actual.as_ref() != Some(&identity) {
                return Err(refused(FailureKind::StaleSchema));
            }
            if !read_only(request) {
                let effects: (i64, i64) = sqlx::query_as(
                    "SELECT
                     (SELECT count(*) FROM INFORMATION_SCHEMA.TRIGGERS WHERE EVENT_OBJECT_SCHEMA=? AND EVENT_OBJECT_TABLE=?),
                     (SELECT count(*) FROM INFORMATION_SCHEMA.REFERENTIAL_CONSTRAINTS WHERE UNIQUE_CONSTRAINT_SCHEMA=?
                      AND REFERENCED_TABLE_NAME=? AND (DELETE_RULE IN('CASCADE','SET NULL','SET DEFAULT')
                      OR UPDATE_RULE IN('CASCADE','SET NULL','SET DEFAULT')))"
                ).bind(&schema).bind(&name).bind(&schema).bind(&name)
                    .fetch_one(&mut *conn).await.map_err(|e| refused(classify(e)))?;
                if effects != (0, 0) {
                    return Err(refused(FailureKind::UnsupportedStatement));
                }
            }
            let columns: Vec<String> = sqlx::query_scalar(
                "SELECT COLUMN_NAME FROM INFORMATION_SCHEMA.COLUMNS
                 WHERE BINARY TABLE_SCHEMA=BINARY ? AND BINARY TABLE_NAME=BINARY ?
                 ORDER BY ORDINAL_POSITION LIMIT 1025",
            )
            .bind(&schema)
            .bind(&name)
            .fetch_all(&mut *conn)
            .await
            .map_err(|e| refused(classify(e)))?;
            if columns.is_empty() || columns.len() > MAX_COLUMNS {
                return Err(refused(FailureKind::ResultLimit));
            }
            resolved += 1;
            if resolved > MAX_RELATIONS {
                return Err(refused(FailureKind::ResultLimit));
            }
            if identity.1 == "VIEW" && views.insert((schema.clone(), name.clone())) {
                let definition: Option<String> = sqlx::query_scalar(
                    "SELECT VIEW_DEFINITION FROM INFORMATION_SCHEMA.VIEWS
                     WHERE BINARY TABLE_SCHEMA=BINARY ? AND BINARY TABLE_NAME=BINARY ?",
                )
                .bind(&schema)
                .bind(&name)
                .fetch_one(&mut *conn)
                .await
                .map_err(|e| refused(classify(e)))?;
                let definition =
                    definition.ok_or_else(|| refused(FailureKind::PermissionDenied))?;
                let planned = check(engine, DataMode::Read, &definition)
                    .map_err(|_| refused(FailureKind::UnsupportedStatement))?;
                queue.push_back(planned[0].read.clone());
            }
            tables.push(PhysicalTable {
                schema,
                name: identity.0,
                columns,
                aliases: relation
                    .alias
                    .iter()
                    .map(|name| name.value.clone())
                    .collect(),
            });
        }
        if statement_table_inspection {
            use devworks_core::services::database::statement::ColumnAccess;
            footprint.columns.insert(ColumnAccess::Wildcard(Vec::new()));
        }
        check_columns(engine, &target.catalog, &footprint, &tables, guard)?;
    }
    Ok(())
}

fn arguments(values: &[Scalar]) -> Result<MySqlArguments, DataFailure> {
    let mut args = MySqlArguments::default();
    for value in values {
        match value {
            Scalar::Null => args.add(None::<&str>),
            Scalar::Boolean(value) => args.add(*value),
            Scalar::Integer(value) => args.add(*value),
            Scalar::Text(value) => args.add(value),
            Scalar::Bytes(value) => args.add(value.as_slice()),
            Scalar::Decimal(value) => args.add(value),
        }
        .map_err(|_| refused(FailureKind::UnsupportedValue))?;
    }
    Ok(args)
}

fn decode(row: &MySqlRow, index: usize, data_type: &str) -> Result<Scalar, DataFailure> {
    let fail = |_| refused(FailureKind::UnsupportedValue);
    if row.try_get_raw(index).map_err(fail)?.is_null() {
        return Ok(Scalar::Null);
    }
    match data_type {
        "BOOLEAN" => row
            .try_get::<bool, _>(index)
            .map(Scalar::Boolean)
            .map_err(fail),
        "TINYINT" | "SMALLINT" | "MEDIUMINT" | "INT" | "BIGINT" => row
            .try_get::<i64, _>(index)
            .map(Scalar::Integer)
            .map_err(fail),
        "TINYINT UNSIGNED" | "SMALLINT UNSIGNED" | "MEDIUMINT UNSIGNED" | "INT UNSIGNED"
        | "BIGINT UNSIGNED" => row.try_get::<u64, _>(index).map_err(fail).map(|number| {
            i64::try_from(number)
                .map_or_else(|_| Scalar::Decimal(number.to_string()), Scalar::Integer)
        }),
        "CHAR" | "VARCHAR" | "TEXT" | "TINYTEXT" | "MEDIUMTEXT" | "LONGTEXT" | "ENUM" | "SET" => {
            row.try_get::<String, _>(index)
                .map(Scalar::Text)
                .map_err(fail)
        }
        "DECIMAL" => row
            .try_get_unchecked::<String, _>(index)
            .map(Scalar::Decimal)
            .map_err(fail),
        "FLOAT" => row
            .try_get::<f32, _>(index)
            .map_err(fail)
            .and_then(|value| {
                if value.is_finite() {
                    Ok(Scalar::Decimal(value.to_string()))
                } else {
                    Err(refused(FailureKind::UnsupportedValue))
                }
            }),
        "DOUBLE" => row
            .try_get::<f64, _>(index)
            .map_err(fail)
            .and_then(|value| {
                if value.is_finite() {
                    Ok(Scalar::Decimal(value.to_string()))
                } else {
                    Err(refused(FailureKind::UnsupportedValue))
                }
            }),
        "DATE" => row
            .try_get::<sqlx::types::chrono::NaiveDate, _>(index)
            .map(|value| Scalar::Text(value.to_string()))
            .map_err(fail),
        "DATETIME" | "TIMESTAMP" => row
            .try_get::<sqlx::types::chrono::NaiveDateTime, _>(index)
            .map(|value| Scalar::Text(value.format("%Y-%m-%dT%H:%M:%S%.f").to_string()))
            .map_err(fail),
        "TIME" => row
            .try_get::<sqlx::mysql::types::MySqlTime, _>(index)
            .map(|value| Scalar::Text(value.to_string()))
            .map_err(fail),
        "BINARY" | "VARBINARY" | "BLOB" | "TINYBLOB" | "MEDIUMBLOB" | "LONGBLOB" => row
            .try_get::<Vec<u8>, _>(index)
            .map(Scalar::Bytes)
            .map_err(fail),
        _ => Err(refused(FailureKind::UnsupportedValue)),
    }
}

type TargetNames = BTreeSet<(String, String)>;
type SchemaSignatures = BTreeMap<(String, String), String>;

async fn prime_metadata_locks(
    conn: &mut MySqlConnection,
    names: &TargetNames,
) -> Result<(), DataFailure> {
    for (schema, name) in names {
        let initial = sqlx::query_as::<_, (String, String, Option<String>)>(TABLE)
            .bind(schema)
            .bind(name)
            .fetch_optional(&mut *conn)
            .await
            .map_err(|e| refused(classify(e)))?;
        if let Some(identity) = initial {
            if identity.1 != "BASE TABLE" || identity.2.as_deref() != Some("InnoDB") {
                return Err(refused(FailureKind::UnsupportedStatement));
            }
            let statement = format!("SELECT 1 FROM {}.{} LIMIT 0", quote(schema), quote(name));
            sqlx::raw_sql(AssertSqlSafe(statement))
                .execute(&mut *conn)
                .await
                .map_err(|e| refused(classify(e)))?;
        }
    }
    Ok(())
}

fn schema_digest(values: &SchemaSignatures) -> Result<String, DataFailure> {
    let names = values.iter().collect::<Vec<_>>();
    let material = serde_json::to_vec(&names).map_err(|_| refused(FailureKind::InvalidTarget))?;
    Ok(devworks_core::fingerprint::sha256_hex(&material))
}

async fn signatures(
    conn: &mut MySqlConnection,
    target: &Target,
    selected: &TargetNames,
    engine: &DbEngine,
) -> Result<SchemaSignatures, DataFailure> {
    const PROBES: &[&str] = &[
        "SELECT TABLE_NAME, CAST(JSON_ARRAY(TABLE_NAME,TABLE_TYPE,ENGINE,TABLE_COLLATION) AS CHAR)
         FROM INFORMATION_SCHEMA.TABLES WHERE BINARY TABLE_SCHEMA=BINARY ? ORDER BY BINARY TABLE_NAME",
        "SELECT TABLE_NAME, CAST(JSON_ARRAY(TABLE_NAME,COLUMN_NAME,COLUMN_TYPE,IS_NULLABLE,COLUMN_DEFAULT,
          COLLATION_NAME, EXTRA,GENERATION_EXPRESSION) AS CHAR) FROM INFORMATION_SCHEMA.COLUMNS
         WHERE BINARY TABLE_SCHEMA=BINARY ? ORDER BY BINARY TABLE_NAME,ORDINAL_POSITION",
        "SELECT TABLE_NAME, CAST(JSON_ARRAY(TABLE_NAME,VIEW_DEFINITION,CHECK_OPTION,SECURITY_TYPE) AS CHAR)
         FROM INFORMATION_SCHEMA.VIEWS WHERE BINARY TABLE_SCHEMA=BINARY ? ORDER BY BINARY TABLE_NAME",
        "SELECT TABLE_NAME, CAST(JSON_ARRAY(TABLE_NAME,INDEX_NAME,COLUMN_NAME,NON_UNIQUE,SUB_PART,
          INDEX_TYPE,COLLATION) AS CHAR) FROM INFORMATION_SCHEMA.STATISTICS
         WHERE BINARY TABLE_SCHEMA=BINARY ? ORDER BY BINARY TABLE_NAME,BINARY INDEX_NAME,SEQ_IN_INDEX",
        "SELECT TABLE_NAME, CAST(JSON_ARRAY(TABLE_NAME,CONSTRAINT_NAME,COLUMN_NAME,
          REFERENCED_TABLE_SCHEMA,REFERENCED_TABLE_NAME,REFERENCED_COLUMN_NAME) AS CHAR)
          FROM INFORMATION_SCHEMA.KEY_COLUMN_USAGE WHERE BINARY TABLE_SCHEMA=BINARY ?
          ORDER BY BINARY TABLE_NAME,BINARY CONSTRAINT_NAME,ORDINAL_POSITION",
        // A foreign key recreated with the same name and columns but another delete or update
        // action must change the digest, so the native rules are part of it.
        "SELECT TABLE_NAME, CAST(JSON_ARRAY('FOREIGN KEY',TABLE_NAME,CONSTRAINT_NAME,UPDATE_RULE,
          DELETE_RULE,MATCH_OPTION) AS CHAR) FROM INFORMATION_SCHEMA.REFERENTIAL_CONSTRAINTS
          WHERE BINARY CONSTRAINT_SCHEMA=BINARY ? ORDER BY BINARY TABLE_NAME,BINARY CONSTRAINT_NAME",
    ];
    // Hash the full native clause bytes in the server so even a large expression contributes a
    // bounded row. A missing definition or enforcement value yields NULL and fails decoding.
    const MYSQL_CHECKS: &str = "SELECT TABLE_NAME,
        CASE WHEN clause_digest IS NOT NULL AND enforcement IN ('YES','NO')
             THEN CAST(JSON_ARRAY('CHECK',CONSTRAINT_NAME,clause_digest,enforcement) AS CHAR) END
        FROM (SELECT tc.TABLE_NAME,tc.CONSTRAINT_NAME,tc.ENFORCED AS enforcement,
                     SHA2(CAST(cc.CHECK_CLAUSE AS BINARY),256) AS clause_digest
              FROM INFORMATION_SCHEMA.TABLE_CONSTRAINTS tc
              LEFT JOIN INFORMATION_SCHEMA.CHECK_CONSTRAINTS cc
                ON BINARY cc.CONSTRAINT_CATALOG=BINARY tc.CONSTRAINT_CATALOG
               AND BINARY cc.CONSTRAINT_SCHEMA=BINARY tc.CONSTRAINT_SCHEMA
               AND BINARY cc.CONSTRAINT_NAME=BINARY tc.CONSTRAINT_NAME
              WHERE BINARY tc.TABLE_SCHEMA=BINARY ? AND tc.CONSTRAINT_TYPE='CHECK') checks
        ORDER BY BINARY TABLE_NAME,BINARY CONSTRAINT_NAME";
    // MariaDB stores no per-constraint enforcement flag. The effective session variable controls
    // both column and table checks. Include the level because their names may overlap.
    const MARIADB_CHECKS: &str = "SELECT TABLE_NAME,
        CASE WHEN clause_digest IS NOT NULL AND enforcement IN (0,1)
             THEN CAST(JSON_ARRAY('CHECK',CONSTRAINT_NAME,constraint_level,clause_digest,enforcement) AS CHAR) END
        FROM (SELECT TABLE_NAME,CONSTRAINT_NAME,`LEVEL` AS constraint_level,
                     SHA2(CAST(CHECK_CLAUSE AS BINARY),256) AS clause_digest,
                     @@SESSION.check_constraint_checks AS enforcement
              FROM INFORMATION_SCHEMA.CHECK_CONSTRAINTS
              WHERE BINARY CONSTRAINT_SCHEMA=BINARY ?) checks
        ORDER BY BINARY TABLE_NAME,BINARY CONSTRAINT_NAME,BINARY constraint_level,BINARY clause_digest";
    let checks = if matches!(engine, DbEngine::MariaDb) {
        MARIADB_CHECKS
    } else {
        MYSQL_CHECKS
    };
    let mut material: BTreeMap<_, Vec<u8>> = selected
        .iter()
        .map(|name| (name.clone(), Vec::new()))
        .collect();
    let mut size = 0usize;
    for probe in PROBES.iter().copied().chain([checks]) {
        let mut stream = sqlx::query_as::<_, (String, String)>(probe)
            .bind(&target.catalog)
            .fetch(&mut *conn);
        while let Some((name, row)) = stream
            .try_next()
            .await
            .map_err(|error| refused(classify(error)))?
        {
            let Some(entry) = material.get_mut(&(target.catalog.clone(), name)) else {
                continue;
            };
            size = size.saturating_add(row.len() + 8);
            if size > 16 * 1024 * 1024 {
                return Err(refused(FailureKind::ResultLimit));
            }
            entry.extend_from_slice(&(row.len() as u64).to_le_bytes());
            entry.extend_from_slice(row.as_bytes());
        }
    }
    Ok(material
        .into_iter()
        .map(|(key, bytes)| (key, devworks_core::fingerprint::sha256_hex(&bytes)))
        .collect())
}

async fn schema_change(
    conn: &mut MySqlConnection,
    request: &DataRequest,
    target: &Target,
    guard: &dyn DispatchGuard,
    scope: &NameScope,
    boundary: &Boundary,
) -> Result<DataOutcome, DataFailure> {
    if request.expected_schema_digest.is_none()
        || !request.statements.iter().any(|sql| sql.schema_change)
    {
        return Err(refused(FailureKind::UnsupportedStatement));
    }
    sqlx::raw_sql("SET SESSION default_storage_engine=InnoDB")
        .execute(&mut *conn)
        .await
        .map_err(|e| refused(classify(e)))?;
    let all = schema_scope(request, target, &target.catalog, scope)?;
    sqlx::raw_sql("START TRANSACTION READ ONLY")
        .execute(&mut *conn)
        .await
        .map_err(|e| refused(classify(e)))?;
    prime_metadata_locks(conn, &all).await?;
    let mut expected = signatures(conn, target, &all, &request.config.engine).await?;
    if request.expected_schema_digest.as_ref() != Some(&schema_digest(&expected)?) {
        return Err(refused(FailureKind::StaleSchema));
    }
    sqlx::raw_sql("ROLLBACK")
        .execute(&mut *conn)
        .await
        .map_err(|e| refused(classify(e)))?;
    let mut known = MutationResult {
        affected_rows: None,
        observed_statements: 0,
        schema_changed: false,
    };
    for (position, sql) in request.statements.iter().enumerate() {
        let step: TargetNames = sql
            .read
            .relations
            .iter()
            .map(|relation| {
                names(
                    &request.config.engine,
                    relation,
                    &target.catalog,
                    &target.catalog,
                )
            })
            .collect::<Result<_, _>>()?;
        let outcome = if sql.schema_change {
            schema_statement(
                conn, request, target, guard, scope, boundary, position, &step, &expected,
            )
            .await
        } else {
            async {
                sqlx::raw_sql("START TRANSACTION READ WRITE").execute(&mut *conn).await
                    .map_err(|error| refused(classify(error)))?;
                authorize(conn, request, target, guard, scope, sql).await?;
                if !same_schema(&signatures(conn, target, &step, &request.config.engine).await?, &expected) {
                    return Err(rollback(conn, refused(FailureKind::StaleSchema)).await);
                }
                let affected = match run_statement(conn, request, target, guard, scope, position).await {
                    Ok(DataOutcome::Mutation(value)) => value.affected_rows,
                    Ok(DataOutcome::Read(_)) => return Err(rollback(conn, refused(FailureKind::UnsupportedStatement)).await),
                    Err(error) => return Err(rollback(conn, error).await),
                };
                if let Err(error) = guard.before_commit() {
                    return Err(rollback(conn, error).await);
                }
                boundary.committing();
                sqlx::raw_sql("COMMIT").execute(&mut *conn).await.map_err(|e| boundary.interrupted(classify(e)))?;
                Ok(affected)
            }.await
        };
        let affected = match outcome {
            Ok(value) => value,
            Err(DataFailure::Refused(reason) | DataFailure::RolledBack(reason))
                if known.observed_statements > 0 =>
            {
                return Err(DataFailure::Partial { reason, known });
            }
            Err(error) => return Err(error),
        };
        known.observed_statements += 1;
        known.schema_changed |= sql.schema_change;
        if let Some(affected) = affected {
            known.affected_rows = known.affected_rows.unwrap_or(0).checked_add(affected);
        }
        boundary.observed(guard, position, known)?;
        if sql.schema_change {
            if position + 1 < request.statements.len() {
                expected.extend(
                    signatures(conn, target, &step, &request.config.engine)
                        .await
                        .map_err(|_| boundary.interrupted(FailureKind::Unavailable))?,
                );
            }
            sqlx::raw_sql("UNLOCK TABLES")
                .execute(&mut *conn)
                .await
                .map_err(|_| boundary.interrupted(FailureKind::Unavailable))?;
        }
    }
    Ok(DataOutcome::Mutation(known))
}

#[allow(clippy::too_many_arguments)]
async fn schema_statement(
    conn: &mut MySqlConnection,
    request: &DataRequest,
    target: &Target,
    guard: &dyn DispatchGuard,
    scope: &NameScope,
    boundary: &Boundary,
    position: usize,
    step: &TargetNames,
    expected: &SchemaSignatures,
) -> Result<Option<i64>, DataFailure> {
    let statement = &request.statements[position];
    let mut locks = BTreeSet::new();
    for relation in &statement.read.relations {
        let (schema, table) = names(
            &request.config.engine,
            relation,
            &target.catalog,
            &target.catalog,
        )?;
        check_scope(request, scope, &schema, &table)?;
        if sqlx::query_as::<_, (String, String, Option<String>)>(TABLE)
            .bind(&schema)
            .bind(&table)
            .fetch_optional(&mut *conn)
            .await
            .map_err(|e| refused(classify(e)))?
            .is_some()
        {
            locks.insert(format!("{}.{} WRITE", quote(&schema), quote(&table)));
        }
    }
    if !locks.is_empty() {
        let sql = format!(
            "LOCK TABLES {}",
            locks.into_iter().collect::<Vec<_>>().join(", ")
        );
        sqlx::raw_sql(AssertSqlSafe(sql))
            .execute(&mut *conn)
            .await
            .map_err(|e| refused(classify(e)))?;
    }
    if !same_schema(
        &signatures(conn, target, step, &request.config.engine).await?,
        expected,
    ) {
        return Err(refused(FailureKind::StaleSchema));
    }
    authorize(conn, request, target, guard, scope, statement).await?;
    let values = if request.mode == DataMode::Migration {
        &[]
    } else {
        request.parameters.as_slice()
    };
    let arguments = arguments(values)?;
    guard.before_statement(position)?;
    guard.before_commit()?;
    boundary.committing();
    if let Err(error) = sqlx::query_with(AssertSqlSafe(statement.sql()), arguments)
        .execute(&mut *conn)
        .await
    {
        let reason = classify(error);
        return Err(match reason {
            FailureKind::UnsupportedStatement
            | FailureKind::PermissionDenied
            | FailureKind::Constraint
            | FailureKind::StaleSchema => boundary.rejected(reason),
            _ => boundary.interrupted(reason),
        });
    }
    Ok(None)
}

fn same_schema(current: &SchemaSignatures, expected: &SchemaSignatures) -> bool {
    current
        .iter()
        .all(|(name, digest)| expected.get(name) == Some(digest))
}
