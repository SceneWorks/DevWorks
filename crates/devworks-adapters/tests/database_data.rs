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
    before_statement: Option<Box<dyn Fn() + Send + Sync>>,
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
        if let Some(check) = &self.before_statement {
            check();
        }
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
    fixture_request(engine, mode, sql, params, "DW37", "dw37_fixture")
}

fn fixture_request(
    engine: &DbEngine,
    mode: DataMode,
    sql: &str,
    params: Vec<Scalar>,
    env: &str,
    catalog: &str,
) -> DataRequest {
    let endpoint =
        std::env::var(format!("{env}_TARGET")).expect("explicit disposable native database");
    let username = std::env::var(format!("{env}_USER")).expect("disposable target principal");
    let password = std::env::var(format!("{env}_PASSWORD")).expect("disposable target password");
    let target = url::Url::parse(&endpoint).expect("valid disposable target");
    assert_eq!(target.host_str(), Some("127.0.0.1"));
    assert!(target.port().is_some());
    assert_eq!(target.path(), format!("/{catalog}"));
    let config: ConnectionConfig = serde_json::from_value(json!({
        "name":"Isolated data fixture",
        "engine":engine,
        "endpoint":endpoint,
        "catalogs":[catalog],
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

        let mut query = request(
            &engine,
            DataMode::Read,
            "SELECT tiny_value, small_value, int_value, big_value FROM dbo.dw_nullable_integers ORDER BY id",
            vec![],
        );
        query.limits.rows = 4;
        let result =
            runtime.block_on(NetworkDataAdapter.execute(query, Arc::new(Guard::default())));
        let rows = match result {
            Ok(DataOutcome::Read(rows)) => rows,
            Err(failure) => panic!("nullable integer read: {failure:?}"),
            _ => panic!("nullable integer read returned a mutation"),
        };
        assert!(!rows.truncated);
        assert!(
            rows.rows
                == [
                    [42, -42, -42_000, 4_200_000_000]
                        .map(Scalar::Integer)
                        .to_vec(),
                    [0, i16::MIN.into(), i32::MIN.into(), i64::MIN]
                        .map(Scalar::Integer)
                        .to_vec(),
                    [u8::MAX.into(), i16::MAX.into(), i32::MAX.into(), i64::MAX]
                        .map(Scalar::Integer)
                        .to_vec(),
                    vec![Scalar::Null; 4],
                ]
        );
        runtime.block_on(sql_server_check_digest());
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
            ..Guard::default()
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

async fn sql_server_check_digest() {
    use tiberius::{AuthMethod, Client, Config, EncryptionLevel};
    use tokio::net::TcpStream;
    use tokio_util::compat::{Compat, TokioAsyncWriteCompatExt};

    const CHANGE: &str = "ALTER TABLE dbo.dw_check_digest ADD marker INT;
        INSERT INTO dbo.dw_check_digest (id, marker) VALUES (-1, 7)";
    let engine = DbEngine::SqlServer;
    let capture = async || {
        let mut preview = request(
            &engine,
            DataMode::Read,
            "SELECT id FROM dbo.dw_check_digest",
            vec![],
        );
        preview.include_schema_digest = true;
        preview.schema_targets =
            request(&engine, DataMode::Migration, CHANGE, vec![]).schema_targets;
        match NetworkDataAdapter
            .execute(preview, Arc::new(Guard::default()))
            .await
        {
            Ok(DataOutcome::Read(rows)) => rows.schema_digest.expect("native CHECK digest"),
            Err(error) => panic!("CHECK digest preview: {error:?}"),
            Ok(DataOutcome::Mutation(_)) => panic!("CHECK digest preview returned a mutation"),
        }
    };
    async fn check_state(client: &mut Client<Compat<TcpStream>>) -> ((bool, bool), String) {
        let row = client
            .simple_query(
                "SELECT is_disabled, is_not_trusted, definition FROM sys.check_constraints
                 WHERE parent_object_id=OBJECT_ID(N'dbo.dw_check_digest')
                   AND name=N'ck_dw_check_digest_id'",
            )
            .await
            .unwrap()
            .into_row()
            .await
            .unwrap()
            .expect("fixture CHECK constraint");
        (
            (row.get(0).unwrap(), row.get(1).unwrap()),
            row.get::<&str, _>(2).unwrap().to_owned(),
        )
    }

    let endpoint = url::Url::parse(&std::env::var("DW37_TARGET").unwrap()).unwrap();
    let mut config = Config::new();
    config.host("127.0.0.1");
    config.port(endpoint.port().unwrap());
    config.database("dw37_fixture");
    config.encryption(EncryptionLevel::NotSupported);
    config.authentication(AuthMethod::sql_server(
        std::env::var("DW37_USER").unwrap(),
        std::env::var("DW37_PASSWORD").unwrap(),
    ));
    let tcp = TcpStream::connect(config.get_addr()).await.unwrap();
    let mut external = Client::connect(config, tcp.compat_write()).await.unwrap();
    let (flags, definition) = check_state(&mut external).await;
    assert_eq!(flags, (false, false));
    let trusted = capture().await;
    let mut previous = trusted.clone();
    for (transition, expected_flags) in [
        ("NOCHECK CONSTRAINT", (true, true)),
        ("WITH NOCHECK CHECK CONSTRAINT", (false, true)),
        ("WITH CHECK CHECK CONSTRAINT", (false, false)),
    ] {
        external
            .simple_query(format!(
                "ALTER TABLE dbo.dw_check_digest {transition} ck_dw_check_digest_id"
            ))
            .await
            .unwrap()
            .into_results()
            .await
            .unwrap();
        let (flags, current_definition) = check_state(&mut external).await;
        assert_eq!(flags, expected_flags, "{transition}");
        assert_eq!(current_definition, definition, "{transition}");

        let mut migration = request(&engine, DataMode::Migration, CHANGE, vec![]);
        migration.expected_schema_digest = Some(previous.clone());
        let outcome = NetworkDataAdapter
            .execute(migration, Arc::new(Guard::default()))
            .await;
        assert!(
            matches!(
                outcome,
                Err(DataFailure::RolledBack(FailureKind::StaleSchema))
            ),
            "{transition} must reject the stale migration: {:?}",
            outcome.err(),
        );
        let fresh = capture().await;
        assert_ne!(previous, fresh, "{transition} must change the digest");
        previous = fresh;
    }
    assert_eq!(previous, trusted);

    let row = external
        .simple_query(
            "SELECT COUNT(*), MAX(id),
               CAST(CASE WHEN COL_LENGTH(N'dbo.dw_check_digest', N'marker') IS NULL
                    THEN 1 ELSE 0 END AS bit)
             FROM dbo.dw_check_digest",
        )
        .await
        .unwrap()
        .into_row()
        .await
        .unwrap()
        .expect("fixture table state");
    assert_eq!(row.get::<i32, _>(0), Some(1));
    assert_eq!(row.get::<i32, _>(1), Some(1));
    assert_eq!(row.get::<bool, _>(2), Some(true));
}

fn check_digest_fixture(engine: DbEngine) {
    use sqlx::ConnectOptions;
    use sqlx::mysql::{MySqlConnectOptions, MySqlSslMode};

    const CATALOG: &str = "dw37_check_fixture";
    const CHANGE: &str = "ALTER TABLE dw_checks ADD marker INT";
    let request = |mode, sql| fixture_request(&engine, mode, sql, vec![], "DW37_CHECK", CATALOG);
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let capture = || {
        let mut preview = request(DataMode::Read, "SELECT id FROM dw_checks");
        preview.include_schema_digest = true;
        preview.schema_targets = statement::schema_targets(
            &statement::check(&engine, DataMode::Schema, CHANGE).unwrap(),
        );
        match runtime.block_on(NetworkDataAdapter.execute(preview, Arc::new(Guard::default()))) {
            Ok(DataOutcome::Read(rows)) => rows.schema_digest.expect("CHECK schema digest"),
            Err(error) => panic!("CHECK digest preview: {error:?}"),
            _ => panic!("CHECK digest preview returned a mutation"),
        }
    };
    let reject = |digest| {
        let mut change = request(DataMode::Schema, CHANGE);
        change.expected_schema_digest = Some(digest);
        let result =
            runtime.block_on(NetworkDataAdapter.execute(change, Arc::new(Guard::default())));
        assert!(
            matches!(result, Err(DataFailure::Refused(FailureKind::StaleSchema))),
            "the native adapter must reject the previous schema digest: {:?}",
            result.err()
        );
    };

    let before = capture();
    assert_eq!(before, capture());
    let endpoint = url::Url::parse(&std::env::var("DW37_CHECK_TARGET").unwrap()).unwrap();
    let mut external = runtime
        .block_on(
            MySqlConnectOptions::new()
                .host("127.0.0.1")
                .port(endpoint.port().unwrap())
                .database(CATALOG)
                .username(&std::env::var("DW37_CHECK_USER").unwrap())
                .password(&std::env::var("DW37_CHECK_PASSWORD").unwrap())
                .ssl_mode(MySqlSslMode::Disabled)
                .connect(),
        )
        .expect("separate fixture connection");
    let mysql = matches!(engine, DbEngine::MySql);
    let drop = if mysql {
        "ALTER TABLE dw_checks DROP CHECK ck_id"
    } else {
        "ALTER TABLE dw_checks DROP CONSTRAINT ck_id"
    };
    runtime.block_on(async {
        sqlx::raw_sql(drop).execute(&mut external).await.unwrap();
        sqlx::raw_sql("ALTER TABLE dw_checks ADD CONSTRAINT ck_id CHECK (id > 1)")
            .execute(&mut external)
            .await
            .unwrap();
    });
    let new_clause = capture();
    assert_ne!(
        before, new_clause,
        "changed CHECK expression must change the digest"
    );
    reject(before);

    if mysql {
        runtime
            .block_on(
                sqlx::raw_sql("ALTER TABLE dw_checks ALTER CHECK ck_id NOT ENFORCED")
                    .execute(&mut external),
            )
            .unwrap();
        assert_ne!(
            new_clause,
            capture(),
            "CHECK enforcement must change the digest"
        );
        reject(new_clause);
    }
    let markers: i64 = runtime
        .block_on(
            sqlx::query_scalar(
                "SELECT count(*) FROM INFORMATION_SCHEMA.COLUMNS WHERE TABLE_SCHEMA=DATABASE()
         AND TABLE_NAME='dw_checks' AND COLUMN_NAME='marker'",
            )
            .fetch_one(&mut external),
        )
        .unwrap();
    assert_eq!(
        markers, 0,
        "stale schema requests must leave the table unchanged"
    );
}

#[test]
#[ignore = "explicit native MySQL 8.4 CHECK fixture"]
fn mysql_check_digest() {
    check_digest_fixture(DbEngine::MySql);
}

#[test]
#[ignore = "explicit native MariaDB 11.4 CHECK fixture"]
fn mariadb_check_digest() {
    check_digest_fixture(DbEngine::MariaDb);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "explicit native PostgreSQL 17 index fixture"]
async fn postgres_index_digest() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio_postgres::{Config, NoTls, config::SslMode, error::SqlState};

    const CATALOG: &str = "dw37_index_fixture";
    const CHANGE: &str = "ALTER TABLE dw_index_target DROP COLUMN note";
    let engine = DbEngine::Postgres;
    let request = |mode, sql| fixture_request(&engine, mode, sql, vec![], "DW37_INDEX", CATALOG);
    let capture = async || {
        let mut preview = request(DataMode::Read, "SELECT id FROM dw_index_target");
        preview.include_schema_digest = true;
        preview.schema_targets = request(DataMode::Schema, CHANGE).schema_targets;
        match NetworkDataAdapter
            .execute(preview, Arc::new(Guard::default()))
            .await
        {
            Ok(DataOutcome::Read(rows)) => rows.schema_digest.expect("native schema digest"),
            Err(error) => panic!("index digest preview: {error:?}"),
            _ => panic!("index digest preview returned a mutation"),
        }
    };
    let apply = async |digest, guard| {
        let mut change = request(DataMode::Schema, CHANGE);
        change.expected_schema_digest = Some(digest);
        NetworkDataAdapter.execute(change, Arc::new(guard)).await
    };

    let before = capture().await;
    let endpoint = url::Url::parse(&std::env::var("DW37_INDEX_TARGET").unwrap()).unwrap();
    let (external, driver) = Config::new()
        .host("127.0.0.1")
        .port(endpoint.port().unwrap())
        .dbname(CATALOG)
        .user(std::env::var("DW37_INDEX_USER").unwrap())
        .password(std::env::var("DW37_INDEX_PASSWORD").unwrap())
        .ssl_mode(SslMode::Disable)
        .connect_timeout(Duration::from_secs(5))
        .options("-c lock_timeout=200 -c statement_timeout=3000")
        .connect(NoTls)
        .await
        .expect("open separate fixture connection");
    let driver = tokio::spawn(driver);
    external
        .batch_execute("CREATE INDEX note_index ON dw_index_target (note) WHERE id > 0")
        .await
        .unwrap();
    let fresh = capture().await;
    assert_ne!(
        before, fresh,
        "a new standalone index must change the table digest"
    );
    let stale = apply(before, Guard::default()).await;
    assert!(
        matches!(
            stale,
            Err(DataFailure::RolledBack(FailureKind::StaleSchema))
        ),
        "the old digest must be rolled back: {:?}",
        stale.err()
    );
    let intact: bool = external
        .query_one(
            "SELECT to_regclass('public.note_index') IS NOT NULL AND EXISTS(
            SELECT 1 FROM pg_catalog.pg_attribute WHERE attrelid='public.dw_index_target'::regclass
              AND attname='note' AND NOT attisdropped)",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert!(
        intact,
        "stale DDL must leave the column and the standalone index intact"
    );

    let external = Arc::new(external);
    let racing = Arc::clone(&external);
    let statement_checks = Arc::new(AtomicUsize::new(0));
    let checks = Arc::clone(&statement_checks);
    let guard = Guard {
        before_statement: Some(Box::new(move || {
            // The network adapter checks admission before opening a connection. Race DDL
            // at the PostgreSQL statement boundary, after it has checked the schema digest.
            if checks.fetch_add(1, Ordering::Relaxed) == 0 {
                return;
            }
            tokio::task::block_in_place(|| {
                tokio::runtime::Handle::current().block_on(async {
                    for statement in [
                        "CREATE INDEX race_index ON dw_index_target (note)",
                        "CREATE INDEX CONCURRENTLY race_concurrent_index ON dw_index_target (note)",
                    ] {
                        let error = racing
                            .batch_execute(statement)
                            .await
                            .expect_err("DDL lock must fence concurrent index creation");
                        assert_eq!(
                            error.code(),
                            Some(&SqlState::LOCK_NOT_AVAILABLE),
                            "{statement}: {error}"
                        );
                    }
                })
            });
        })),
        ..Guard::default()
    };
    let applied = apply(fresh, guard).await;
    assert!(
        matches!(
            applied,
            Ok(DataOutcome::Mutation(MutationResult {
                schema_changed: true,
                observed_statements: 1,
                ..
            }))
        ),
        "the fresh digest should permit the guarded DDL: {:?}",
        applied.err()
    );
    assert_eq!(statement_checks.load(Ordering::Relaxed), 2);
    drop(external);
    driver.await.unwrap().unwrap();
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
