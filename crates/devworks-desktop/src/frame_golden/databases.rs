//! Sample frames of the Databases screen's requests: source listing, schema object and
//! relationship reads, saving, removing and syncing sources.
use super::{Silent, from};
use crate::databases_ops::{self, ObjectsRead, command, query};
use devworks_core::protocol::records::{ConnectionConfig, SchemaConnection};
use serde_json::json;

pub fn frames() -> Vec<(&'static str, Vec<u8>)> {
    let b = &Silent;
    // A blank name or endpoint is outside the wire types' contract, so the sample fills both.
    let mut draft = crate::databases_screen::Draft::new(Some("project-a".into()));
    draft.config["name"] = json!("New source");
    draft.config["endpoint"] = json!("/data/new.db");
    let mut remote = crate::databases_screen::Draft::new(None);
    remote.config["engine"] = json!("Postgres");
    remote.config["name"] = json!("Orders");
    remote.config["endpoint"] = json!("db.example:5432");
    remote.config["authentication"] = json!("Password");
    remote.config["username"] = json!("reader");
    remote.config["tls"] = json!("VerifyFull");
    remote.config["catalogs"] = json!(["main"]);
    remote.config["credentialRef"] = json!("credential-a");
    let config: ConnectionConfig = from(remote.config.clone());
    let source: SchemaConnection = from(json!({"id":"source-a","revision":"4","name":"Orders",
        "engine":"Postgres","state":"Enabled","writesEnabled":false,"metadataOnly":true,
        "config":config}));
    let read =
        |project: Option<&str>, kind: &str, text: &str, ids: Option<(&str, &str)>, cursor| {
            ObjectsRead {
                project: project.map(str::to_owned),
                pin: "pin-a".into(),
                connection: source.id.clone(),
                kind: kind.into(),
                text: text.into(),
                object: ids.map(|(object, _)| object.to_owned()),
                parent: ids.map(|(_, parent)| parent.to_owned()),
                cursor,
            }
        };
    let ids = ["o1".to_owned(), "o2".to_owned()];
    vec![
        (
            "databases_connections_workspace",
            query(b, databases_ops::connections(None, None)),
        ),
        (
            "databases_connections_project_more",
            query(b, databases_ops::connections(Some("project-a"), Some("c1"))),
        ),
        (
            "databases_objects",
            query(b, read(None, "", "", None, None).input()),
        ),
        (
            "databases_objects_filtered",
            query(
                b,
                read(
                    Some("project-a"),
                    "Table",
                    "orders",
                    Some(("object-a", "parent-a")),
                    Some("c1".to_owned()),
                )
                .input(),
            ),
        ),
        (
            "databases_relationships",
            query(
                b,
                databases_ops::relationships(None, "pin-a", &source.id, &ids, None),
            ),
        ),
        (
            "databases_relationships_more",
            query(
                b,
                databases_ops::relationships(
                    Some("project-a"),
                    "pin-a",
                    &source.id,
                    &ids[..1],
                    Some("c1"),
                ),
            ),
        ),
        (
            "databases_save_new",
            command(
                b,
                databases_ops::save(Some("project-a"), None, &draft.config),
            ),
        ),
        (
            "databases_save_existing",
            command(
                b,
                databases_ops::save(None, Some(("source-a", 4)), &remote.config),
            ),
        ),
        (
            "databases_remove_source",
            command(b, databases_ops::remove_source(None, &source)),
        ),
        (
            "databases_remove_index",
            command(b, databases_ops::remove_index(&source)),
        ),
        (
            "databases_refresh_test",
            command(b, databases_ops::refresh("source-a", "4", None, Some(true))),
        ),
        (
            "databases_refresh_project",
            command(
                b,
                databases_ops::refresh("source-a", "4", Some("project-a"), Some(false)),
            ),
        ),
        (
            "databases_refresh_batch",
            command(
                b,
                databases_ops::refresh("source-a", "4", Some("project-a"), None),
            ),
        ),
    ]
}
