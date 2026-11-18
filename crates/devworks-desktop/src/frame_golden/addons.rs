//! Sample frames of the Add-ons screen: catalog and log pages, lifecycle commands and
//! configuration saves.
use super::{Silent, from};
use crate::addons;
use devworks_core::protocol::addons::AddonCommand;
use devworks_core::protocol::provider::ArtifactRef;
use serde_json::json;

pub fn frames() -> Vec<(&'static str, Vec<u8>)> {
    let b = &Silent;
    let bundle: ArtifactRef = from(json!({"artifactId":"bundle-a","versionId":"version-a",
        "sha256":"a".repeat(64),"byteLength":"12","mediaType":"application/vnd.devworks.package"}));
    let digest = "b".repeat(64);
    let schema = from(json!({"schemaId":"pack-a/config","revision":"1","sha256":"c".repeat(64)}));
    let lifecycle = |command: Option<AddonCommand>| addons::lifecycle_frame(b, command.unwrap());
    vec![
        ("addons_catalog_first", addons::catalog_frame(b, None)),
        (
            "addons_catalog_after",
            addons::catalog_frame(b, Some("cursor-1")),
        ),
        ("addons_logs_first", addons::logs_frame(b, "pack-a", None)),
        (
            "addons_logs_after",
            addons::logs_frame(b, "pack-a", Some("cursor-1")),
        ),
        (
            "addons_install",
            lifecycle(addons::install_command(bundle.clone(), None, 4)),
        ),
        (
            "addons_update",
            lifecycle(addons::install_command(bundle.clone(), Some("pack-a"), 4)),
        ),
        (
            "addons_enable",
            lifecycle(addons::enable_command("pack-a", 4)),
        ),
        (
            "addons_disable",
            lifecycle(addons::disable_command("pack-a", 4)),
        ),
        (
            "addons_uninstall",
            lifecycle(addons::uninstall_command("pack-a", 4)),
        ),
        (
            "addons_rollback",
            lifecycle(addons::rollback_command("pack-a", &digest, 4, 2)),
        ),
        (
            "addons_configure",
            addons::configure_frame(b, "pack-a", 3, schema, json!({"threads":5})),
        ),
        ("addons_trust", addons::trust_frame(b, "pack-a", 3, true)),
        (
            "addons_projects",
            addons::project_frame(b, "pack-a", 0, &["project-a".into(), "project-b".into()]),
        ),
        (
            "addons_install_saved",
            addons::install_saved(b, &bundle, None, 0).1,
        ),
        (
            "addons_update_saved",
            addons::install_saved(b, &bundle, Some("pack-a"), 4).1,
        ),
    ]
}
