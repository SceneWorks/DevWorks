//! Data tests use an explicitly supplied disposable native database, never a user connection.
use devworks_adapters::database::network::data::NetworkDataAdapter;
use devworks_core::ports::database::data::*;
use devworks_core::ports::secrets::Secret;
use devworks_core::protocol::Id;
use devworks_core::protocol::records::{ConnectionConfig, DbEngine};
use devworks_core::services::database::statement::{self, DataMode};
use serde_json::json;
use std::sync::Arc;
use std::time::{Duration, Instant};

#[derive(Default)]
struct Guard {
    reject_commit: bool,
}

impl DispatchGuard for Guard {
    fn check_columns(&self, columns: &[PhysicalColumn]) -> Result<(), DataFailure> {
        if columns
            .iter()
            .any(|column| column.table == "dw_private" && column.column == "secret")
        {
            return Err(DataFailure::Refused(FailureKind::PermissionDenied));
        }
        Ok(())
    }

    fn before_statement(&self, _: usize) -> Result<(), DataFailure> {
        Ok(())
    }

    fn before_commit(&self) -> Result<(), DataFailure> {
        if self.reject_commit {
            return Err(DataFailure::Refused(FailureKind::PermissionDenied));
        }
        Ok(())
    }

    fn observed_statement(&self, _: usize, _: MutationResult) -> Result<(), DataFailure> {
        Ok(())
    }
}

fn request(engine: &DbEngine, mode: DataMode, sql: &str, params: Vec<Scalar>) -> DataRequest {
    let endpoint = std::env::var("DW37_TARGET").expect("explicit disposable native database");
    let username = std::env::var("DW37_USER").expect("disposable target principal");
    let password = std::env::var("DW37_PASSWORD").expect("disposable target password");
    assert!(endpoint.contains("://127.0.0.1:") && endpoint.ends_with("/dw37_fixture"));
    let config: ConnectionConfig = serde_json::from_value(json!({
        "name":"Isolated data fixture",
        "engine":engine,
        "endpoint":endpoint,
        "catalogs":["dw37_fixture"],
        "schemas":[],
        "includePatterns":["dw_*"],
        "excludePatterns":[],
        "writesEnabled":true,
        "enabled":true,
        "authentication":"Password",
        "username":username,
        "tls":"LoopbackPlaintext"
    }))
    .unwrap();
    let plans = statement::check(engine, mode, sql).unwrap();
    let targets = if matches!(mode, DataMode::Schema | DataMode::Migration) {
        statement::schema_targets(&plans)
    } else {
        Vec::new()
    };
    DataRequest {
        id: Id::new("database-fixture").unwrap(),
        mode,
        config,
        password: Some(Secret::new(password)),
        ca_certificate: None,
        statements: plans,
        parameters: params,
        expected_schema_digest: None,
        include_schema_digest: false,
        schema_targets: targets,
        limits: DataLimits {
            rows: 1,
            columns: 8,
            bytes: 4096,
            deadline: Instant::now() + Duration::from_secs(8),
        },
        cancellation: Default::default(),
        file_authority: None,
    }
}

fn fixture(engine: DbEngine, bind: &str) {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let run = |mode, sql: &str, parameters, guard| {
        runtime.block_on(
            NetworkDataAdapter.execute(request(&engine, mode, sql, parameters), Arc::new(guard)),
        )
    };
    let sql = format!("SELECT id, secret FROM dw_public WHERE id >= {bind} ORDER BY id");
    let result = run(
        DataMode::Read,
        &sql,
        vec![Scalar::Integer(1)],
        Guard::default(),
    );
    let rows = match result {
        Ok(DataOutcome::Read(value)) => value,
        Err(error) => panic!("native read: {error:?}"),
        _ => panic!("native read returned a mutation"),
    };
    assert!(rows.truncated);
    assert!(rows.rows == vec![vec![Scalar::Integer(1), Scalar::Text("public".into())]]);

    for sql in [
        "SELECT x.secret FROM dw_private x",
        "SELECT x.secret FROM dw_public x JOIN dw_private y ON x.id=y.id WHERE y.secret IS NOT NULL",
        "SELECT secret FROM dw_view",
        "SELECT * FROM dw_private",
    ] {
        let refusal = run(DataMode::Read, sql, vec![], Guard::default());
        assert!(
            matches!(
                refusal,
                Err(DataFailure::Refused(FailureKind::PermissionDenied))
            ),
            "{sql}: {:?}",
            refusal.err()
        );
    }

    if matches!(&engine, DbEngine::SqlServer) {
        let allowed = run(
            DataMode::Read,
            "SELECT dbo.dw_allowed_nested.item FROM dbo.dw_allowed_nested",
            vec![],
            Guard::default(),
        );
        match allowed {
            Ok(DataOutcome::Read(value)) => assert!(value.rows == [vec![Scalar::Integer(1)]]),
            Err(failure) => panic!("checked nested view: {failure:?}"),
            _ => panic!("checked view returned a mutation"),
        }
    }
    if matches!(&engine, DbEngine::Postgres) {
        for sql in [
            "SELECT x FROM dw_private AS x",
            "SELECT x.visible FROM dw_private AS x(visible, renamed)",
        ] {
            assert!(matches!(
                run(DataMode::Read, sql, vec![], Guard::default()),
                Err(DataFailure::Refused(FailureKind::PermissionDenied))
            ));
        }
        let cascade = run(
            DataMode::Write,
            "DELETE FROM dw_fk_parent WHERE id = 1",
            vec![],
            Guard::default(),
        );
        assert!(matches!(
            cascade,
            Err(DataFailure::Refused(FailureKind::UnsupportedStatement)
                | DataFailure::RolledBack(FailureKind::UnsupportedStatement))
        ));
        for table in ["dw_fk_parent", "dw_fk_child"] {
            let remaining = run(
                DataMode::Read,
                &format!("SELECT id FROM {table}"),
                vec![],
                Guard::default(),
            );
            assert!(matches!(remaining, Ok(DataOutcome::Read(value)) if
                value.rows == [vec![Scalar::Integer(1)]]));
        }
    }

    let insert = "INSERT INTO dw_public (id, secret) VALUES (3, 'committed')";
    let result = run(
        DataMode::Write,
        insert,
        vec![],
        Guard {
            reject_commit: true,
        },
    );
    assert!(
        matches!(
            result,
            Err(DataFailure::RolledBack(FailureKind::PermissionDenied))
        ),
        "rollback: {:?}",
        result.err()
    );
    let result = run(DataMode::Write, insert, vec![], Guard::default());
    assert!(
        matches!(
            result,
            Ok(DataOutcome::Mutation(MutationResult {
                affected_rows: Some(1),
                ..
            }))
        ),
        "commit: {:?}",
        result.err()
    );
    let result = run(DataMode::Write, insert, vec![], Guard::default());
    assert!(
        matches!(
            result,
            Err(DataFailure::RolledBack(FailureKind::Constraint))
        ),
        "constraint: {:?}",
        result.err()
    );
    let read_schema = |preview: &str| {
        let mut query = request(&engine, DataMode::Read, "SELECT 1 AS check_source", vec![]);
        query.include_schema_digest = true;
        query.schema_targets = statement::schema_targets(
            &statement::check(&engine, DataMode::Migration, preview).unwrap(),
        );
        match runtime.block_on(NetworkDataAdapter.execute(query, Arc::new(Guard::default()))) {
            Ok(DataOutcome::Read(value)) => {
                value.schema_digest.expect("native schema precondition")
            }
            Err(failure) => panic!("schema digest: {failure:?}"),
            _ => panic!("unexpected schema inspection result"),
        }
    };
    let prepared = |mode, sql: &str, expected| {
        let mut command = request(&engine, mode, sql, vec![]);
        command.expected_schema_digest = Some(expected);
        runtime.block_on(NetworkDataAdapter.execute(command, Arc::new(Guard::default())))
    };
    let change = "CREATE TABLE dw_ddl (id INTEGER PRIMARY KEY)";
    let batch = "ALTER TABLE dw_ddl ADD added INT; INSERT INTO dw_ddl (id,added) VALUES (1,2)";
    let original = read_schema(change);
    let stale = prepared(
        DataMode::Schema,
        "CREATE TABLE dw_ddl (id INTEGER PRIMARY KEY)",
        "stale".into(),
    );
    assert!(
        matches!(
            stale,
            Err(DataFailure::Refused(FailureKind::StaleSchema)
                | DataFailure::RolledBack(FailureKind::StaleSchema))
        ),
        "{:?}",
        stale.err()
    );
    let direct = prepared(
        DataMode::Schema,
        "CREATE TABLE dw_ddl (id INTEGER PRIMARY KEY)",
        original.clone(),
    );
    assert!(
        matches!(
            direct,
            Ok(DataOutcome::Mutation(MutationResult {
                schema_changed: true,
                observed_statements: 1,
                ..
            }))
        ),
        "{:?}",
        direct.err()
    );
    let changed_since_preview = prepared(DataMode::Schema, change, original.clone());
    assert!(matches!(
        changed_since_preview,
        Err(DataFailure::Refused(FailureKind::StaleSchema)
            | DataFailure::RolledBack(FailureKind::StaleSchema))
    ));
    let new = read_schema(batch);
    assert_ne!(original, new);
    let migration = prepared(
        DataMode::Migration,
        "ALTER TABLE dw_ddl ADD added INT; INSERT INTO dw_ddl (id,added) VALUES (1,2)",
        new,
    );
    assert!(
        matches!(
            migration,
            Ok(DataOutcome::Mutation(MutationResult {
                schema_changed: true,
                observed_statements: 2,
                ..
            }))
        ),
        "{:?}",
        migration.err()
    );
}

#[test]
#[ignore = "explicit native PostgreSQL fixture"]
fn postgres_data() {
    fixture(DbEngine::Postgres, "$1");
}

#[test]
#[ignore = "explicit native MariaDB fixture"]
fn mariadb_data() {
    fixture(DbEngine::MariaDb, "?");
}

#[test]
#[ignore = "explicit native MySQL fixture"]
fn mysql_data() {
    fixture(DbEngine::MySql, "?");
}

#[test]
#[ignore = "explicit x86 SQL Server fixture"]
fn sql_server_data() {
    fixture(DbEngine::SqlServer, "@P1");
}
