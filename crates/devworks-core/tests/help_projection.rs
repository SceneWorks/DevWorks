use devworks_core::protocol::records::{HelpReadResult, HelpTopicPageResult};
use devworks_core::protocol::support::{Check, Revisioned};
use serde_json::{Value, json};

fn artifact() -> Value {
    json!({"artifactId":"artifact-a","versionId":"version-a","sha256":"a".repeat(64),"byteLength":"12","mediaType":"text/markdown"})
}
fn legacy() -> Value {
    json!({"id":"help-workspace","version":"0.1.0","title":"Workspace","content":artifact(),"relatedCapabilityIds":[]})
}
fn snapshot() -> Value {
    json!({"appVersion":"0.1.0","helpRevision":"2","registryRevision":"3","canGenerate":false,"cannotGenerateReason":"host_maintenance_required"})
}
fn summary() -> Value {
    let mut value = legacy();
    value.as_object_mut().unwrap().extend(json!({"availability":"Available","relatedCapabilities":["help.read","package/tool"],"capabilities":[],"provenance":{"kind":"Bundled","appVersion":"0.1.0","contentDigest":"b".repeat(64)}}).as_object().unwrap().clone());
    value
}
fn available() -> Value {
    json!({"kind":"Available","topic":summary(),"passage":{"text":"Local Help, without artifact-tool permissions","offset":"0","hasMore":false},"snapshot":snapshot()})
}
#[test]
fn exact_recorded_legacy_topic_and_page_remain_readable() {
    let topic = legacy();
    let decoded: HelpReadResult = serde_json::from_value(topic.clone()).unwrap();
    decoded.check().unwrap();
    decoded.check_revision(1).unwrap();
    assert!(decoded.check_revision(3).is_err());
    assert_eq!(serde_json::to_value(decoded).unwrap(), topic);
    let page = json!({"items":[topic],"hasMore":false});
    let decoded: HelpTopicPageResult = serde_json::from_value(page.clone()).unwrap();
    decoded.check_revision(1).unwrap();
    assert_eq!(serde_json::to_value(decoded).unwrap(), page);
}
#[test]
fn real_published_artifacts_project_to_exact_basic_legacy_results() {
    let mut decoded: HelpReadResult = serde_json::from_value(available()).unwrap();
    decoded.check().unwrap();
    decoded.check_revision(3).unwrap();
    decoded.retain_revision(1);
    decoded.check_revision(1).unwrap();
    assert_eq!(serde_json::to_value(decoded).unwrap(), legacy());
    let mut page: HelpTopicPageResult = serde_json::from_value(json!({"items":[summary()],"hasMore":false,"availability":"Available","snapshot":snapshot()})).unwrap();
    page.check().unwrap();
    page.retain_revision(1);
    page.check_revision(1).unwrap();
    assert_eq!(
        serde_json::to_value(page).unwrap(),
        json!({"items":[legacy()],"hasMore":false})
    );
}
#[test]
fn missing_stale_and_inline_only_views_never_invent_artifact_references() {
    for availability in ["Missing", "Stale", "Unavailable"] {
        let wire = json!({"kind":"Unavailable","topicId":"help-workspace","requestedVersion":"old","availability":availability,"reason":"Exact local source unavailable","currentVersion":"0.1.0","snapshot":snapshot()});
        let mut decoded: HelpReadResult = serde_json::from_value(wire.clone()).unwrap();
        decoded.check().unwrap();
        decoded.retain_revision(1);
        assert!(decoded.check_revision(1).is_err());
        assert_eq!(serde_json::to_value(decoded).unwrap(), wire);
    }
    let mut inline = available();
    inline["topic"].as_object_mut().unwrap().remove("content");
    let mut decoded: HelpReadResult = serde_json::from_value(inline.clone()).unwrap();
    decoded.check().unwrap();
    decoded.retain_revision(1);
    assert!(decoded.check_revision(1).is_err());
    assert_eq!(serde_json::to_value(decoded).unwrap(), inline);
    let mut page: HelpTopicPageResult = serde_json::from_value(json!({"items":[inline["topic"]],"hasMore":false,"availability":"Available","snapshot":snapshot()})).unwrap();
    page.retain_revision(1);
    assert!(page.check_revision(1).is_err());
}
#[test]
fn full_unicode_passage_is_bounded_and_continuation_is_explicit() {
    let mut wire = available();
    wire["passage"] = json!({"text":"λ".repeat(65536),"offset":"0","nextCursor":"next-bound-to-source","hasMore":true});
    let decoded: HelpReadResult = serde_json::from_value(wire.clone()).unwrap();
    decoded.check().unwrap();
    assert_eq!(serde_json::to_value(decoded).unwrap(), wire);
    wire["passage"]["hasMore"] = json!(false);
    assert!(
        serde_json::from_value::<HelpReadResult>(wire.clone())
            .unwrap()
            .check()
            .is_err()
    );
    wire["passage"]["text"] = json!("λ".repeat(65537));
    assert!(serde_json::from_value::<HelpReadResult>(wire).is_err());
}

#[test]
fn screen_selector_null_cannot_disappear_during_actual_admission() {
    use devworks_core::admission::{self, Selection};
    use devworks_core::registry::Mode;
    let selection = Selection {
        operation: "help.topic_for_screen",
        mode: Mode::Query,
    };
    let mut frame = json!({"mode":"Query","protocol":"1.0-draft.1","queryId":"help-query","schemaRevision":3,"body":{"kind":"help.topic_for_screen","screenId":"workspace","current":true}});
    assert!(admission::decode(selection, frame.to_string().as_bytes()).is_ok());
    frame["body"]["capabilityRevision"] = Value::Null;
    assert!(admission::decode(selection, frame.to_string().as_bytes()).is_err());
    frame["body"]["capabilityRevision"] = json!("1");
    assert!(admission::decode(selection, frame.to_string().as_bytes()).is_err());
    frame["body"].as_object_mut().unwrap().remove("current");
    frame["schemaRevision"] = json!(1);
    assert!(admission::decode(selection, frame.to_string().as_bytes()).is_ok());
}
