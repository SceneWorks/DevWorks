//! Sample frames of the scheduled job form: the `jobs.create` input each kind of schedule,
//! definition and route turns into.
use super::{Silent, from};
use crate::ops;
use crate::shell::jobs::{JobForm, form_values};
use devworks_core::protocol::records::ArtifactRecord;
use devworks_core::services::jobs::NativeCapability;
use serde_json::json;

pub fn frames() -> Vec<(&'static str, Vec<u8>)> {
    let b = &Silent;
    let capability = NativeCapability {
        capability_id: from(json!("capability-a")),
        operation: "maintenance.sample",
        revision: devworks_core::registry::SchemaRevision(1),
        schema: from(
            json!({"schemaId":"core/jobs-arguments","revision":"1","sha256":"b".repeat(64)}),
        ),
    };
    let prompt: ArtifactRecord = from(json!({"artifact":{"artifactId":"prompt-1",
        "versionId":"prompt-v2","sha256":"b".repeat(64),"byteLength":"20",
        "mediaType":"text/markdown"},"name":"Triage","createdAtMs":"2","scope":{"kind":"Workspace"},
        "referenceCount":"1"}));
    let create = |form: JobForm, project: Option<&str>| {
        let (schedule, definition) = form_values(
            &form,
            std::slice::from_ref(&capability),
            std::slice::from_ref(&prompt),
        )
        .expect("form");
        ops::job_create(
            b,
            project,
            form.title.trim(),
            &schedule,
            &definition,
            form.enabled,
        )
    };
    let native = JobForm {
        title: " Sweep ".into(),
        schedule_kind: 0,
        definition_kind: 0,
        capability: Some("capability-a".into()),
        arguments: "{\"days\":7}".into(),
        ..JobForm::default()
    };
    let interval = JobForm {
        title: "Triage".into(),
        schedule_kind: 1,
        seconds: " 90 ".into(),
        prompt: Some("prompt-1".into()),
        route_kind: 0,
        tier: 2,
        effort: 3,
        target_project: " project-a ".into(),
        feature: " feature-a ".into(),
        enabled: false,
        ..JobForm::default()
    };
    let calendar = JobForm {
        title: "Nightly".into(),
        schedule_kind: 2,
        expression: " 0 3 * * * ".into(),
        timezone: " Europe/Paris ".into(),
        prompt: Some("prompt-1".into()),
        route_kind: 1,
        model: " model-x ".into(),
        effort: 5,
        ..JobForm::default()
    };
    let auto = JobForm {
        title: "Auto".into(),
        schedule_kind: 0,
        prompt: Some("prompt-1".into()),
        route_kind: 2,
        effort: 6,
        ..JobForm::default()
    };
    vec![
        ("job_form_native_manual", create(native, None)),
        (
            "job_form_inference_interval",
            create(interval, Some("project-a")),
        ),
        ("job_form_inference_calendar_model", create(calendar, None)),
        ("job_form_inference_openrouter_auto", create(auto, None)),
    ]
}
