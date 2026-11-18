//! Sample frames of the Knowledge screen's requests: listing pages, reads, diagnostics, entry
//! saves and edits, lifecycle changes, cleanup by source and file import.
use super::{Silent, from};
use crate::knowledge_screen::{
    self as ks, Change, Draft, ImportStage, KnowledgeScreen, SourcePreview,
};
use devworks_core::protocol::Id;
use devworks_core::protocol::provider::ArtifactRef;
use devworks_core::protocol::records::KnowledgeClaim;
use serde_json::json;

fn claim(id: &str, scope: serde_json::Value) -> KnowledgeClaim {
    from(json!({
        "id": id, "revision": "4", "kind": "Lesson", "subjectId": "entity-s",
        "predicate": "observed", "objectId": "entity-o", "value": "Old text",
        "confidence": 0.3, "state": "Active", "evidenceIds": ["ev-1", "ev-2"],
        "tags": ["lesson"], "scope": scope,
        "valueJson": {"steps": [1, 2]}, "normalizedText": "old text body",
        "effectiveAtMs": "9", "source": "run-7", "entryKind": "Lesson",
        "lifecycle": "Active", "originRef": "message-3",
        "lesson": {"ruleState": "Candidate", "source": "Agent",
            "applicability": {"activities": ["reviewing"], "paths": ["src/lib.rs"],
                "tools": ["gh"], "harness": "claude-code", "repositoryIds": ["repo-a"]},
            "recurrence": "1", "recurrenceThreshold": "3", "challenges": [],
            "addressedBy": [], "deliveredSessions": "0"}
    }))
}

fn screen(project: Option<&str>) -> KnowledgeScreen {
    let mut s = KnowledgeScreen::new();
    s.set_projects(vec![("project-x".into(), "Project X".into())]);
    s.project = project.map(str::to_owned);
    s
}

/// A screen listing `project` with `claim` selected.
fn showing(project: Option<&str>, claim: KnowledgeClaim) -> KnowledgeScreen {
    let mut s = screen(project);
    s.begin_listing(false);
    s.selected = Some(claim.id.clone());
    s.claims.items = vec![claim];
    s.reason = "checked".into();
    s.tag_edit = "lesson, edited".into();
    s
}

pub fn frames() -> Vec<(&'static str, Vec<u8>)> {
    let b = &Silent;
    let mut out = Vec::new();

    let mut s = screen(None);
    let (_, body) = s.begin_listing(false);
    out.push(("knowledge_search_workspace", ks::query(b, body)));

    let mut s = screen(Some("project-x"));
    s.text = "needle".into();
    s.tags = "a, b".into();
    s.entry_kind = Some("Gotcha");
    s.lifecycles = [true, true, false, true];
    s.lessons_only = true;
    let (_, body) = s.begin_listing(false);
    out.push(("knowledge_search_project_filtered", ks::query(b, body)));
    let first = s.claims.reload();
    s.claims.apply(
        first.generation,
        Ok(crate::paged::Page {
            items: Vec::new(),
            next: Some("cursor-1".into()),
        }),
    );
    s.project = None;
    let (_, body) = s.begin_listing(true);
    out.push(("knowledge_search_more", ks::query(b, body)));

    let mut s = screen(None);
    s.lifecycles = [false; 4];
    let (_, body) = s.begin_listing(false);
    out.push(("knowledge_search_no_state", ks::query(b, body)));

    let s = showing(
        Some("project-x"),
        claim("claim-p", json!({"kind": "Workspace"})),
    );
    let id: Id = from(json!("claim-p"));
    out.push(("knowledge_read", ks::query(b, s.read_input(&id))));
    out.push(("knowledge_diagnostics", ks::query(b, s.diagnostics_input())));

    let mut s = showing(None, claim("claim-g", json!({"kind": "Workspace"})));
    let mut d = Draft::default();
    d.value = " A fact ".into();
    d.tags = "one, two".into();
    let (_, body) = s.save_input(&d).unwrap();
    out.push(("knowledge_save_fact", ks::command(b, body)));
    let mut d = Draft::default();
    d.entry_kind = "Lesson";
    d.subject = "Topic".into();
    d.value = "Run the checks".into();
    d.activities = "implementing, testing".into();
    let (_, body) = s.save_input(&d).unwrap();
    out.push(("knowledge_save_lesson", ks::command(b, body)));
    let mut d = Draft::of(&s.claims.items[0]);
    d.value = "New text".into();
    d.tags = "lesson, edited".into();
    d.activities = "implementing".into();
    let (_, body) = s.save_input(&d).unwrap();
    out.push(("knowledge_revise_lesson", ks::command(b, body)));
    let plain: KnowledgeClaim = from(json!({
        "id": "claim-h", "revision": "2", "kind": "Fact", "subjectId": "entity-s",
        "predicate": "fact", "value": "Plain", "confidence": 0.8, "state": "Active",
        "evidenceIds": [], "tags": [], "scope": {"kind": "Project", "projectId": "project-x"},
        "entryKind": "Gotcha", "lifecycle": "Active"}));
    s.claims.items = vec![plain];
    s.selected = Some(from(json!("claim-h")));
    let d = Draft::of(&s.claims.items[0]);
    let (_, body) = s.save_input(&d).unwrap();
    out.push(("knowledge_revise_plain", ks::command(b, body)));

    for (name, scope) in [
        ("workspace", json!({"kind": "Workspace"})),
        (
            "project",
            json!({"kind": "Project", "projectId": "project-x"}),
        ),
    ] {
        let s = showing(Some("project-x"), claim("claim-c", scope));
        for (change, label) in [
            (Change::Verify, "verify"),
            (Change::Retract, "retract"),
            (Change::Challenge, "challenge"),
            (Change::Retire, "retire"),
            (Change::Promote, "promote"),
            (Change::Reject, "reject"),
            (Change::Delete, "delete"),
            (Change::MakeGeneral, "make_general"),
            (Change::SaveTags, "save_tags"),
        ] {
            let (_, body) = s.change_input(change).unwrap();
            let key: &'static str = Box::leak(format!("knowledge_{label}_{name}").into_boxed_str());
            out.push((key, ks::command(b, body)));
        }
    }

    let mut s = screen(Some("project-x"));
    s.begin_listing(false);
    s.cleanup_source = "run-1".into();
    let (_, body) = s.preview_input().unwrap();
    out.push(("knowledge_cleanup_preview", ks::query(b, body)));
    s.source_preview = Some(SourcePreview {
        source: "run-1".into(),
        digest: "d".repeat(64),
        scope_revision: 12,
        message: "Delete 3 entries".into(),
    });
    let (_, body) = s.delete_source_input().unwrap();
    out.push(("knowledge_cleanup_delete", ks::command(b, body)));

    let source: ArtifactRef = from(json!({"artifactId":"artifact-a","versionId":"version-a",
        "sha256":"a".repeat(64),"byteLength":"12","mediaType":"application/json"}));
    let mut s = screen(Some("project-x"));
    s.begin_listing(false);
    let scope = Some("project-x".to_owned());
    out.push((
        "knowledge_import_preview",
        ks::query(b, ks::import_preview_input(scope.as_deref(), &source)),
    ));
    s.import = Some(ImportStage::Ready {
        name: "notes.json".into(),
        source,
        scope,
        digest: "e".repeat(64),
        candidates: Vec::new(),
        summary: "2 entries".into(),
    });
    out.push((
        "knowledge_import_confirm",
        ks::command(b, s.import_input().unwrap()),
    ));
    out
}
