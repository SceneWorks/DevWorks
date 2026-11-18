//! Sample frames of the project purge requests (prepare, manifest page, status, confirm).
use super::{Silent, from};
use crate::ops;
use devworks_core::protocol::records::{ProjectPurgeManifest, ProjectRecord};
use serde_json::json;

pub fn frames() -> Vec<(&'static str, Vec<u8>)> {
    let b = &Silent;
    let project: ProjectRecord = from(json!({"id":"project-a","revision":"3","name":"Project A",
        "description":"About","repositoryIds":["repo-a"],"lifecycle":"Archived","accessMode":"Full"}));
    let manifest: ProjectPurgeManifest = from(json!({"id":"manifest-a","projectId":"project-a",
        "revision":"2","projectRevision":"3","digest":"e".repeat(64),"phase":"Prepared",
        "recordCount":"5","managedByteCount":"6","keptRecordCount":"1","keptByteCount":"2",
        "pendingEffects":"0","createdAtMs":"9"}));
    vec![
        ("purge_prepare", ops::purge_prepare(b, &project)),
        ("purge_status", ops::purge_status(b, &manifest)),
        (
            "purge_manifest_first",
            ops::purge_manifest_page(b, &manifest, None),
        ),
        (
            "purge_manifest_cursor",
            ops::purge_manifest_page(b, &manifest, Some("cursor-1")),
        ),
        ("purge_confirm", ops::purge_confirm(b, &manifest)),
    ]
}
