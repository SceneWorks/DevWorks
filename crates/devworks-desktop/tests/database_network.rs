//! Explicit isolated real-server qualification through the production desktop runtime.
//! Credentials use SessionSecrets; no native secret facility, inference or OS trust changes.
mod common;
use common::*;
use devworks_core::ports::secrets::Secret;
use devworks_core::protocol::{
    CommandReply,
    results::{DomainDecision, DomainQueryReply, OperationResult},
};
use serde_json::json;
fn qualify(engine: &str, variable: &str) {
    qualify_selected(engine, variable, None);
}
fn qualify_selected(engine: &str, variable: &str, selected: Option<&str>) {
    let _native_case = native_runtime_case();
    let endpoint = std::env::var(variable).expect("explicit isolated server endpoint required");
    let root = Root::new("production-network-metadata");
    let host = start(&root);
    let backend = host.backend();
    let credential = backend
        .stage_database_password(Secret::new("devworks_metadata_reader"))
        .unwrap();
    let frame=json!({"protocol":"1.0-draft.1","mode":"Command","schemaRevision":5,"requestKey":backend.token("native-network-source"),"body":{"kind":"schemas.configure","scope":{"kind":"Workspace"},"config":{"name":"Production runtime network fixture","engine":engine,"endpoint":endpoint,"catalogs":[],"schemas":[],"includePatterns":selected.into_iter().collect::<Vec<_>>(),"excludePatterns":if selected.is_some(){Vec::<&str>::new()}else{vec!["meta_large_default","meta_oversized_definition"]},"writesEnabled":false,"enabled":true,"projectAccess":[],"workspaceExposed":false,"exposureMode":"SchemaOnly","authentication":"Password","username":"metadata_reader","credentialRef":credential,"tls":"LoopbackPlaintext"}}}).to_string().into_bytes();
    let CommandReply::Recorded { receipt, .. } =
        backend.command("schemas.configure", &frame).unwrap()
    else {
        panic!("source receipt");
    };
    let DomainDecision::Applied {
        result: OperationResult::SchemasConfigure { value: source },
    } = receipt.decision
    else {
        panic!("source applied");
    };
    let frame=json!({"protocol":"1.0-draft.1","mode":"Command","schemaRevision":5,"requestKey":backend.token("native-network-index"),"body":{"kind":"schemas.index","connectionId":source.id,"expectedRevision":source.revision}}).to_string().into_bytes();
    assert!(
        matches!(backend.command("schemas.index", &frame).unwrap(),CommandReply::Recorded{receipt,..} if matches!(receipt.decision,DomainDecision::Accepted{..}))
    );
    let deadline = std::time::Instant::now() + SETTLE_BOUND;
    let published = loop {
        let frame=json!({"protocol":"1.0-draft.1","mode":"Query","schemaRevision":5,"queryId":backend.token("native-network-status"),"body":{"kind":"schemas.connections","scope":{"kind":"Workspace"},"page":{"limit":100}}}).to_string().into_bytes();
        let response = backend.query("schemas.connections", &frame).unwrap();
        if let DomainQueryReply::Result {
            result: OperationResult::SchemasConnections { value },
            ..
        } = response
        {
            let current = value
                .items
                .0
                .into_iter()
                .find(|v| v.id == source.id)
                .unwrap();
            if current.current_view_id.is_some() && current.active_operation_id.is_none() {
                break current;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "production network metadata never published: {current:?}"
            );
        } else {
            assert!(
                std::time::Instant::now() < deadline,
                "production metadata status unavailable: {response:?}"
            );
        }
        std::thread::sleep(std::time::Duration::from_millis(2));
    };
    if selected.is_some() {
        assert!(matches!(
            published.coverage.as_ref().unwrap().state,
            devworks_core::protocol::data::CoverageState::Partial
        ));
    }
    assert!(published.last_error.is_none());
    assert!(published.profile.is_some());
    let frame=json!({"protocol":"1.0-draft.1","mode":"Query","schemaRevision":5,"queryId":backend.token("native-network-objects"),"body":{"kind":"schemas.objects","scope":{"kind":"Workspace"},"connectionId":source.id,"views":{"kind":"Exact","viewIds":[published.current_view_id.unwrap()]},"kinds":[],"page":{"limit":100}}}).to_string().into_bytes();
    let DomainQueryReply::Result {
        result: OperationResult::SchemasObjects { value },
        ..
    } = backend.query("schemas.objects", &frame).unwrap()
    else {
        panic!("published native objects");
    };
    assert!(value.items.0.iter().any(|v| {
        v.metadata
            .as_ref()
            .is_some_and(|m| m.name.0 == selected.unwrap_or("parent"))
    }));
    assert!(
        !serde_json::to_string(&value)
            .unwrap()
            .contains("SOURCE_ROW_SENTINEL_NEVER_INDEX")
    );
}
#[test]
#[ignore = "explicit isolated x86 SQLServer production-engine dispatch"]
fn native_sql_server_production_engine_metadata() {
    qualify("SqlServer", "DEVWORKS_METADATA_SQLSERVER");
}
#[test]
#[ignore = "explicit isolated MySQL production-engine dispatch"]
fn native_mysql_production_engine_metadata() {
    qualify("MySql", "DEVWORKS_METADATA_MYSQL");
}
#[test]
#[ignore = "explicit isolated MariaDB production-engine dispatch"]
fn native_mariadb_production_engine_metadata() {
    qualify("MariaDb", "DEVWORKS_METADATA_MARIADB");
}
#[test]
#[ignore = "explicit isolated PostgreSQL production-engine dispatch"]
fn native_postgres_production_engine_metadata() {
    qualify("Postgres", "DEVWORKS_METADATA_POSTGRES");
}

#[test]
#[ignore = "explicit isolated PostgreSQL multibyte view through production engine"]
fn native_postgres_production_engine_multibyte_view_metadata() {
    qualify_selected(
        "Postgres",
        "DEVWORKS_METADATA_POSTGRES",
        Some("meta_oversized_definition"),
    );
}
