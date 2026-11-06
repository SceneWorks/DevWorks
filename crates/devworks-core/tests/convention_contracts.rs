use devworks_core::admission::{self, Selection};
use devworks_core::protocol::records::{
    ConventionPassage, ConventionRecordResult, ConventionSectionPath,
};
use devworks_core::protocol::support::{Check, Revisioned};
use devworks_core::registry::Mode;
use serde_json::{Value, json};

fn artifact() -> Value {
    json!({"artifactId":"artifact-doc","versionId":"version-doc","sha256":"a".repeat(64),"byteLength":"12","mediaType":"text/markdown"})
}
fn convention(lifecycle: &str) -> Value {
    json!({"id":"convention-one","revision":"2","scope":{"kind":"Workspace"},"slug":"one","title":"One","sortOrder":"-1","lifecycle":lifecycle,"content":artifact(),"applicability":[],"provenance":{"kind":"Authored","actorId":"human-one"},"authorId":"human-one","createdAtMs":"1","updatedAtMs":"2","legacyApproved":false,"index":{"status":"NotIndexed"}})
}
fn decode(operation: &'static str, mode: Mode, revision: u32, body: Value) -> bool {
    let frame = if mode == Mode::Query {
        json!({"mode":"Query","protocol":"1.0-draft.1","queryId":"query-one","schemaRevision":revision,"body":body})
    } else {
        json!({"mode":"Command","protocol":"1.0-draft.1","requestKey":"request-one","schemaRevision":revision,"body":body})
    };
    admission::decode(Selection { operation, mode }, frame.to_string().as_bytes()).is_ok()
}
#[test]
fn scoped_slug_and_legacy_id_are_distinct_strict_admission_paths() {
    let old = json!({"kind":"conventions.read","conventionId":"convention-one"});
    assert!(decode("conventions.read", Mode::Query, 1, old.clone()));
    assert!(decode("conventions.read", Mode::Query, 3, old));
    let mut slug = json!({"kind":"conventions.read","slug":"one","scope":{"kind":"Workspace"},"maxChars":"65536"});
    assert!(decode("conventions.read", Mode::Query, 3, slug.clone()));
    assert!(!decode("conventions.read", Mode::Query, 1, slug.clone()));
    slug["conventionId"] = Value::Null;
    assert!(!decode("conventions.read", Mode::Query, 3, slug.clone()));
    slug["conventionId"] = json!("convention-one");
    assert!(!decode("conventions.read", Mode::Query, 3, slug.clone()));
    slug.as_object_mut().unwrap().remove("conventionId");
    slug.as_object_mut().unwrap().remove("scope");
    assert!(!decode("conventions.read", Mode::Query, 3, slug));
}
#[test]
fn index_all_uses_real_scoped_revision_and_cannot_masquerade_as_one_resource() {
    let mut body =
        json!({"kind":"conventions.index","allScope":{"kind":"Workspace"},"expectedRevision":"7"});
    assert!(decode("conventions.index", Mode::Command, 3, body.clone()));
    assert!(!decode("conventions.index", Mode::Command, 1, body.clone()));
    body["resourceId"] = json!("resource-one");
    assert!(!decode("conventions.index", Mode::Command, 3, body.clone()));
    body.as_object_mut().unwrap().remove("resourceId");
    body["conventionId"] = json!("convention-one");
    assert!(!decode("conventions.index", Mode::Command, 3, body.clone()));
    body.as_object_mut().unwrap().remove("allScope");
    assert!(decode("conventions.index", Mode::Command, 1, body));
}
#[test]
fn actual_compatibility_approval_never_comes_from_activation_or_retirement() {
    for lifecycle in ["Active", "Retired"] {
        let mut value: ConventionRecordResult =
            serde_json::from_value(convention(lifecycle)).unwrap();
        value.check().unwrap();
        value.check_revision(3).unwrap();
        value.retain_revision(1);
        value.check_revision(1).unwrap();
        let wire = serde_json::to_value(value).unwrap();
        assert_eq!(wire["approved"], false);
        assert_eq!(wire["content"], artifact());
        assert_eq!(wire["name"], "One");
    }
}
#[test]
fn full_source_title_is_not_truncated_to_the_legacy_label_bound() {
    let mut row = convention("Active");
    row["title"] = json!("λ".repeat(500));
    let mut value: ConventionRecordResult = serde_json::from_value(row.clone()).unwrap();
    value.check().unwrap();
    value.retain_revision(1);
    assert!(value.check_revision(1).is_err());
    assert_eq!(serde_json::to_value(value).unwrap(), row);
}
#[test]
fn section_path_has_one_real_representation_and_rejects_null_erasure() {
    for wire in [
        json!({}),
        json!({"text":"Path","artifact":artifact()}),
        json!({"text":"Path","artifact":null}),
    ] {
        let valid =
            serde_json::from_value::<ConventionSectionPath>(wire).is_ok_and(|v| v.check().is_ok());
        assert!(!valid);
    }
    for wire in [
        json!({"text":"λ".repeat(65536)}),
        json!({"artifact":artifact()}),
    ] {
        serde_json::from_value::<ConventionSectionPath>(wire)
            .unwrap()
            .check()
            .unwrap();
    }
    assert!(
        serde_json::from_value::<ConventionSectionPath>(json!({"text":"λ".repeat(65537)})).is_err()
    );
}
#[test]
fn oversized_path_read_requires_its_owning_generation_and_chunk() {
    let mut body =
        json!({"kind":"conventions.read","conventionId":"convention-one","format":"SectionPath"});
    assert!(!decode("conventions.read", Mode::Query, 3, body.clone()));
    body["sectionPath"] = json!({"generationId":"generation-one","chunkId":"chunk-one"});
    assert!(decode("conventions.read", Mode::Query, 3, body.clone()));
    body["format"] = json!("Markdown");
    assert!(!decode("conventions.read", Mode::Query, 3, body));
}

#[test]
fn verbatim_source_preserves_nul_and_obeys_the_actual_utf8_passage_ceiling() {
    let mut row = convention("Active");
    row["title"] = json!("Title\0suffix");
    let mut record: ConventionRecordResult = serde_json::from_value(row.clone()).unwrap();
    record.check().unwrap();
    record.retain_revision(1);
    assert!(record.check_revision(1).is_err());
    assert_eq!(serde_json::to_value(record).unwrap(), row);
    let passage = json!({"text":format!("{}\0", "λ".repeat(32767)),"offset":"0","hasMore":false});
    let decoded: ConventionPassage = serde_json::from_value(passage.clone()).unwrap();
    decoded.check().unwrap();
    assert_eq!(serde_json::to_value(decoded).unwrap(), passage);
    assert!(
        serde_json::from_value::<ConventionPassage>(
            json!({"text":"λ".repeat(32769),"offset":"0","hasMore":false})
        )
        .unwrap()
        .check()
        .is_err()
    );
}
