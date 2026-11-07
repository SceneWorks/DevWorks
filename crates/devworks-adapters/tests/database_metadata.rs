//! Explicitly dispatched qualification against isolated real database servers, never user stores.
use devworks_adapters::database::network::NetworkMetadataReader;
use devworks_core::ports::database::*;
use devworks_core::ports::secrets::Secret;
use devworks_core::protocol::Id;
use devworks_core::protocol::data::CoverageState;
use serde_json::json;

fn request(engine: &str, endpoint: &str) -> MetadataRequest {
    MetadataRequest {connection_id:Id::new("metadata-fixture").unwrap(),config:serde_json::from_value(json!({"name":"Isolated metadata fixture","engine":engine,"endpoint":endpoint,"catalogs":[],"schemas":[],"includePatterns":[],"excludePatterns":[],"writesEnabled":false,"enabled":true,"authentication":"Password","username":"metadata_reader","tls":"LoopbackPlaintext"})).unwrap(),password:Some(Secret::new("devworks_metadata_reader")),ca_certificate:None,test_only:false,limits:MetadataLimits::default(),cancellation:Default::default()}
}
fn qualify(engine: &str, var: &str) {
    let endpoint = std::env::var(var).expect("explicit isolated fixture endpoint is required");
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let result = NetworkMetadataReader
            .read(request(engine, &endpoint))
            .await
            .expect("real metadata read");
        if matches!(engine, "Postgres" | "MariaDb") {
            assert!(matches!(result.coverage.state, CoverageState::Partial));
            assert_eq!(
                result
                    .coverage
                    .gaps
                    .iter()
                    .map(|g| g.as_str())
                    .collect::<Vec<_>>(),
                ["metadata_definition_unavailable"],
                "the restricted fixture hides only routine definitions"
            );
        } else {
            assert!(
                matches!(result.coverage.state, CoverageState::Complete),
                "{:?}",
                result.coverage
            );
        }
        for name in ["parent", "child", "read_parent"] {
            assert!(
                result.objects.iter().any(|v| v.metadata.name.0 == name),
                "missing {name}"
            );
        }
        assert!(
            result
                .objects
                .iter()
                .any(|v| v.kind == "Column" && v.metadata.name.0 == "parent_id")
        );
        assert!(
            result
                .objects
                .iter()
                .any(|v| v.kind == "Index" && v.metadata.name.0 == "child_pair")
        );
        assert!(result.objects.iter().any(|v| v.kind == "Procedure"));
        assert!(result.relationships.iter().any(|v| v.kind == "FOREIGN_KEY"));
        assert!(!format!("{result:?}").contains("SOURCE_ROW_SENTINEL_NEVER_INDEX"));
        let keys = result
            .objects
            .iter()
            .map(|v| v.key.clone())
            .collect::<Vec<_>>();
        let again = NetworkMetadataReader
            .read(request(engine, &endpoint))
            .await
            .unwrap();
        assert_eq!(
            keys,
            again
                .objects
                .iter()
                .map(|v| v.key.clone())
                .collect::<Vec<_>>()
        );
        let mut limited = request(engine, &endpoint);
        limited.limits.max_objects = 2;
        let limited = NetworkMetadataReader.read(limited).await.unwrap();
        assert!(matches!(limited.coverage.state, CoverageState::Partial));
        assert_eq!(limited.objects.len(), 2);
        let mut probe = request(engine, &endpoint);
        probe.test_only = true;
        assert!(
            NetworkMetadataReader
                .read(probe)
                .await
                .unwrap()
                .objects
                .is_empty()
        );
        let mut refused = request(engine, &endpoint);
        refused.password = Some(Secret::new("wrong-isolated-fixture-password"));
        assert!(matches!(
            NetworkMetadataReader.read(refused).await,
            Err(MetadataFailure::AccessDenied)
        ));
        let cancelled = request(engine, &endpoint);
        cancelled.cancellation.cancel();
        assert!(matches!(
            NetworkMetadataReader.read(cancelled).await,
            Err(MetadataFailure::Cancelled)
        ));
    });
}
#[test]
#[ignore = "explicit real MariaDB fixture dispatch"]
fn native_mariadb_structural_metadata() {
    qualify("MariaDb", "DEVWORKS_METADATA_MARIADB");
}
#[test]
#[ignore = "explicit real MySQL fixture dispatch"]
fn native_mysql_structural_metadata() {
    qualify("MySql", "DEVWORKS_METADATA_MYSQL");
}
#[test]
#[ignore = "explicit real PostgreSQL fixture dispatch"]
fn native_postgres_structural_metadata() {
    qualify("Postgres", "DEVWORKS_METADATA_POSTGRES");
}
#[test]
#[ignore = "explicit real x86 SQL Server fixture dispatch"]
fn native_sql_server_structural_metadata() {
    qualify("SqlServer", "DEVWORKS_METADATA_SQLSERVER");
}

#[test]
#[ignore = "explicit real PostgreSQL fixture with pgx_user schema"]
fn native_postgres_user_schema_prefix_is_visible() {
    let endpoint =
        std::env::var("DEVWORKS_METADATA_POSTGRES").expect("explicit isolated fixture endpoint");
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let mut query = request("Postgres", &endpoint);
    query.config.schemas = serde_json::from_value(json!(["pgx_user"])).unwrap();
    let snapshot = runtime.block_on(NetworkMetadataReader.read(query)).unwrap();
    assert!(
        snapshot
            .objects
            .iter()
            .any(|o| o.kind == "Table" && o.metadata.name.0 == "visible_metadata"),
        "user schema pgx_user was silently excluded: {:?}",
        snapshot.coverage
    );
    assert!(
        snapshot
            .objects
            .iter()
            .filter(|o| o.kind != "Catalog")
            .all(|o| o.metadata.schema.as_ref().is_none_or(|s| s.0 == "pgx_user"))
    );
}

#[test]
#[ignore = "explicit real PostgreSQL fixture with indexed materialized view"]
fn native_postgres_materialized_view_indexes_keep_view_parent_identity() {
    let endpoint =
        std::env::var("DEVWORKS_METADATA_POSTGRES").expect("explicit isolated fixture endpoint");
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let mut query = request("Postgres", &endpoint);
    query.config.schemas = serde_json::from_value(json!(["public"])).unwrap();
    let snapshot = runtime.block_on(NetworkMetadataReader.read(query)).unwrap();
    let view = snapshot
        .objects
        .iter()
        .find(|o| o.kind == "View" && o.metadata.name.0 == "materialized_parent")
        .expect("actual materialized view");
    let index = snapshot
        .objects
        .iter()
        .find(|o| o.kind == "Index" && o.metadata.name.0 == "materialized_parent_index")
        .expect("actual materialized view index");
    assert_eq!(
        index.parent_key.as_ref(),
        Some(&view.key),
        "index parent must identify the retained view"
    );
    let keys = snapshot
        .objects
        .iter()
        .map(|o| o.key.as_str())
        .collect::<std::collections::BTreeSet<_>>();
    assert!(
        snapshot.objects.iter().all(|o| o
            .parent_key
            .as_ref()
            .is_none_or(|p| keys.contains(p.as_str()))),
        "metadata contains an orphan parent"
    );
}

fn qualify_tls(engine: &str, var: &str) {
    use devworks_core::protocol::records::SchemaTlsMode;
    let numeric = std::env::var(var).expect("explicit isolated fixture endpoint");
    let endpoint = numeric.replace("127.0.0.1", "localhost");
    let ca =
        std::fs::read(std::env::var("DEVWORKS_METADATA_CA").expect("isolated fixture CA path"))
            .unwrap();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let mut verified = request(engine, &endpoint);
        verified.config.tls = Some(SchemaTlsMode::VerifyFull);
        verified.ca_certificate = Some(ca.clone());
        let snapshot = NetworkMetadataReader
            .read(verified)
            .await
            .expect("custom per-connection CA and matching DNS identity");
        assert!(
            snapshot
                .objects
                .iter()
                .any(|o| o.kind == "Table" && o.metadata.name.0 == "parent")
        );
        assert!(!format!("{snapshot:?}").contains("SOURCE_ROW_SENTINEL_NEVER_INDEX"));
        assert!(snapshot.profile.tls.0.contains("VerifyFull"));
        let mut unknown = request(engine, &endpoint);
        unknown.config.tls = Some(SchemaTlsMode::VerifyFull);
        assert!(
            NetworkMetadataReader.read(unknown).await.is_err(),
            "isolated CA must not become a trusted OS root"
        );
        let mut mismatch = request(engine, &numeric);
        mismatch.config.tls = Some(SchemaTlsMode::VerifyFull);
        mismatch.ca_certificate = Some(ca.clone());
        assert!(
            NetworkMetadataReader.read(mismatch).await.is_err(),
            "a DNS-only fixture certificate must not authorize an IP identity"
        );
        let mut chain_only = request(engine, &numeric);
        chain_only.config.tls = Some(SchemaTlsMode::VerifyCa);
        chain_only.ca_certificate = Some(ca);
        if engine == "SqlServer" {
            assert!(matches!(
                NetworkMetadataReader.read(chain_only).await,
                Err(MetadataFailure::UnsupportedProfile)
            ));
        } else {
            assert!(
                NetworkMetadataReader.read(chain_only).await.is_ok(),
                "VerifyCa preserves chain validation without DNS identity matching"
            );
        }
    });
}
#[test]
#[ignore = "explicit isolated MariaDB TLS fixture"]
fn native_mariadb_metadata_tls() {
    qualify_tls("MariaDb", "DEVWORKS_METADATA_MARIADB");
}
#[test]
#[ignore = "explicit isolated MySQL TLS fixture"]
fn native_mysql_metadata_tls() {
    qualify_tls("MySql", "DEVWORKS_METADATA_MYSQL");
}
#[test]
#[ignore = "explicit isolated PostgreSQL TLS fixture"]
fn native_postgres_metadata_tls() {
    qualify_tls("Postgres", "DEVWORKS_METADATA_POSTGRES");
}
#[test]
#[ignore = "explicit isolated SQLServer TLS fixture; x86 qualification is separate"]
fn native_sql_server_metadata_tls() {
    qualify_tls("SqlServer", "DEVWORKS_METADATA_SQLSERVER");
}

#[test]
#[ignore = "explicit isolated PostgreSQL metadata_limits catalog"]
fn native_postgres_long_default_reports_partial_object_coverage() {
    let endpoint = std::env::var("DEVWORKS_METADATA_POSTGRES_LIMITS")
        .expect("explicit isolated metadata-limits endpoint");
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let snapshot = runtime
        .block_on(NetworkMetadataReader.read(request("Postgres", &endpoint)))
        .unwrap();
    assert!(
        matches!(snapshot.coverage.state, CoverageState::Partial),
        "truncated defaults must not claim complete metadata"
    );
    let column = snapshot
        .objects
        .iter()
        .find(|o| o.kind == "Column" && o.metadata.name.0 == "long_default")
        .expect("retained bounded default metadata");
    assert!(
        matches!(column.coverage.state, CoverageState::Partial),
        "object coverage must disclose its own truncated field"
    );
    assert!(column.metadata.default_value.as_ref().unwrap().0.len() <= 4096);
    assert!(
        column
            .coverage
            .gaps
            .iter()
            .any(|id| id.as_str() == "metadata_definition_byte_limit")
    );
}

#[test]
#[ignore = "explicit isolated PostgreSQL restricted metadata role"]
fn native_postgres_hidden_routine_definition_reports_partial_not_confirmed_absent() {
    hidden_definition("Postgres", "DEVWORKS_METADATA_POSTGRES", "postgres");
}
#[test]
#[ignore = "explicit isolated MariaDB restricted metadata role"]
fn native_mariadb_hidden_routine_definition_reports_partial_not_confirmed_absent() {
    hidden_definition("MariaDb", "DEVWORKS_METADATA_MARIADB", "root");
}
fn hidden_definition(engine: &str, variable: &str, owner_user: &str) {
    let endpoint = std::env::var(variable).expect("isolated fixture endpoint");
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let snapshot = runtime
        .block_on(NetworkMetadataReader.read(request(engine, &endpoint)))
        .unwrap();
    let missing = snapshot
        .objects
        .iter()
        .find(|o| o.kind == "Procedure" && o.metadata.definition_sql.is_none())
        .expect("the explicitly restricted fixture role cannot view the owner routine definition");
    assert!(
        matches!(snapshot.coverage.state, CoverageState::Partial),
        "hidden definition is unavailable, not complete"
    );
    assert!(
        snapshot
            .coverage
            .gaps
            .iter()
            .any(|g| g.as_str() == "metadata_definition_unavailable")
    );
    assert!(matches!(missing.coverage.state, CoverageState::Partial));
    assert!(
        missing
            .coverage
            .gaps
            .iter()
            .any(|g| g.as_str() == "metadata_definition_unavailable")
    );
    // The owner sees actual structural definitions, so normal Complete coverage remains meaningful.
    let mut owner = request(engine, &endpoint);
    owner.config.username = Some(devworks_core::protocol::support::BoundedText(
        owner_user.into(),
    ));
    owner.password = Some(Secret::new("devworks_metadata_fixture"));
    let full = runtime.block_on(NetworkMetadataReader.read(owner)).unwrap();
    assert!(matches!(full.coverage.state, CoverageState::Complete));
    assert!(
        full.objects
            .iter()
            .filter(|o| o.kind == "Procedure")
            .all(|o| o.metadata.definition_sql.is_some())
    );
    assert!(!format!("{full:?}").contains("SOURCE_ROW_SENTINEL_NEVER_INDEX"));
}
