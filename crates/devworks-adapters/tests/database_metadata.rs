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
        assert!(
            matches!(result.coverage.state, CoverageState::Complete),
            "{:?}",
            result.coverage
        );
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
