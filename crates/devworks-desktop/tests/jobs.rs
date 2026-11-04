//! Scheduled jobs over the real runtime, headless (DW-50): a scheduled inference task is listed on
//! the Scheduled jobs screen with its schedule and state; Run now records a run whose truthful
//! outcome (no inference pipeline in this build: failed before dispatch, nothing sent) is shown
//! on Runs; Disable and Enable change it through the same commands an agent submits; Delete asks
//! for confirmation first. The project sidebar on All projects shows the project's job status.
//! The create form refuses incomplete input locally. No inference.
mod common;

use common::*;
use devworks_core::ports::storage::*;
use devworks_core::protocol::records::JobRecord;
use devworks_core::protocol::{ActorId, Id};
use devworks_desktop::backend::Backend;
use devworks_desktop::ops;
use devworks_desktop::shell::jobs::{JobForm, form_values, status_line};
use eframe::egui::accesskit::Role;
use egui_kittest::kittest::{NodeT, Queryable};
use serde_json::{Value, json};

fn id(s: &str) -> Id {
    Id::new(s).unwrap()
}

fn seed_prompt(engine: &devworks_desktop::host::Engine) {
    run_write(
        &**engine.host().runtime().store(),
        WriteOptions::default(),
        |tx| {
            let digest = "a".repeat(64);
            tx.artifacts()
                .put_blob(&digest, 12, "gen-1", "blobs/aa/prompt")?;
            tx.artifacts().insert_artifact(&StoredArtifact {
                id: id("prompt-1"),
                origin_project_id: None,
                origin_work_id: None,
                title: "Triage new issues".into(),
                revision: 1,
                scope: ArtifactScope::Workspace,
                owner: Some(ActorId(id(devworks_desktop::host::USER_ACTOR))),
                retention: ArtifactRetention::Explicit,
                scope_revision: 1,
                reference_revision: 1,
                current_version_id: None,
                state: ArtifactState::Live,
                created_at_ms: 1,
                provenance: json!({}),
            })?;
            tx.artifacts().insert_version(&StoredVersion {
                id: id("prompt-v1"),
                artifact_id: id("prompt-1"),
                version_no: 1,
                blob_digest: digest,
                byte_count: 12,
                blob_generation: "gen-1".into(),
                media_type: "text/markdown".into(),
                created_at_ms: 1,
                deleted_at_ms: None,
                source: json!({}),
            })
        },
    )
    .unwrap();
}

fn inference() -> Value {
    json!({"kind": "Inference", "prompt": {"artifactId": "prompt-1", "versionId": "prompt-v1",
        "sha256": "a".repeat(64), "byteLength": "12", "mediaType": "text/markdown"},
        "route": {"kind": "Tier", "tier": "High"}, "effort": "Default"})
}

fn jobs(backend: &dyn Backend) -> Vec<JobRecord> {
    ops::jobs(backend.query("jobs.list", &ops::jobs_list(backend, None)))
        .unwrap()
        .0
}

#[test]
fn a_job_is_run_disabled_enabled_and_deleted_from_the_schedules_screen() {
    let root = Root::new("jobs-screen");
    let engine = start(&root);
    seed_prompt(&engine);
    let backend = engine.backend();
    let created = ops::job_changed(backend.command(
        "jobs.create",
        &ops::job_create(
            &*backend,
            None,
            "Issue triage",
            &json!({"kind": "Calendar", "expression": "0 3 * * *", "timezone": "UTC"}),
            &inference(),
            true,
        ),
    ));
    assert!(matches!(created, Ok(ops::JobChange::Job(_))));
    let mut h = harness(shell(&engine, &root, Script::default()), 1440.0, 900.0);
    settle(&mut h);
    click_nav(&mut h, "Scheduled jobs");
    assert!(has_text(&h, "Issue triage"));
    assert!(has_text(&h, "0 3 * * * (UTC)"));
    h.query_all_by_label_contains("Issue triage")
        .next()
        .expect("the job row")
        .click();
    settle(&mut h);
    assert!(has_text(&h, "Next run:"));
    assert!(has_text(&h, "Enabled"));
    // Run now: this build has no inference pipeline, so the run fails before dispatch and says so.
    click(&mut h, Role::Button, "Run now");
    let job = jobs(&*backend).pop().unwrap();
    assert!(
        job.last_run_id.is_some(),
        "Run now recorded a run through jobs.run_now"
    );
    let deadline = std::time::Instant::now() + SETTLE_BOUND;
    loop {
        let job = jobs(&*backend).pop().unwrap();
        if job.active_run_id.is_none() {
            break;
        }
        assert!(std::time::Instant::now() < deadline, "the run settles");
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    // The screen shows the settled run once it reloads (as its periodic refresh does).
    click(&mut h, Role::Button, "Refresh");
    click(&mut h, Role::Button, "Runs");
    settle(&mut h);
    assert!(has_text(&h, "Failed"));
    assert!(has_text(&h, "Nothing was sent to a model"));
    click(&mut h, Role::Button, "Schedules");
    click(&mut h, Role::Button, "Disable");
    assert!(!jobs(&*backend).pop().unwrap().enabled);
    click(&mut h, Role::Button, "Enable");
    assert!(jobs(&*backend).pop().unwrap().enabled);
    // Delete asks first; Keep leaves it.
    click(&mut h, Role::Button, "Delete…");
    assert!(has_text(&h, "its run history is kept"));
    click(&mut h, Role::Button, "Keep");
    assert_eq!(jobs(&*backend).len(), 1);
    click(&mut h, Role::Button, "Delete…");
    click(&mut h, Role::Button, "Delete");
    assert!(jobs(&*backend).is_empty());
}

#[test]
fn the_form_refuses_incomplete_jobs_locally_and_builds_the_shared_input() {
    let mut form = JobForm::default();
    assert!(form_values(&form, &[], &[]).is_err(), "a title is required");
    form.title = "Scan".into();
    assert!(
        form_values(&form, &[], &[])
            .unwrap_err()
            .contains("saved prompt"),
        "an inference task needs its prompt"
    );
    form.definition_kind = 0;
    assert!(
        form_values(&form, &[], &[])
            .unwrap_err()
            .contains("No maintenance capability"),
        "nothing unqualified is offered"
    );
    form.definition_kind = 1;
    form.schedule_kind = 1;
    form.seconds = "abc".into();
    assert!(form_values(&form, &[], &[]).is_err());
    assert_eq!(status_line(&[], false), "No scheduled jobs.");
}

#[test]
fn the_project_sidebar_shows_its_scheduled_jobs() {
    let root = Root::new("jobs-project");
    let engine = start(&root);
    seed_prompt(&engine);
    let backend = engine.backend();
    let created = ops::project_changed(backend.command(
        "projects.create",
        &ops::project_create(&*backend, "Engine work", "", &[]),
    ))
    .unwrap();
    let p = created.id.0.as_str().to_owned();
    let job = ops::job_changed(backend.command(
        "jobs.create",
        &ops::job_create(
            &*backend,
            Some(&p),
            "Nightly docs",
            &json!({"kind": "Interval", "seconds": "3600"}),
            &json!({"kind": "Inference", "prompt": {"artifactId": "prompt-1", "versionId": "prompt-v1",
                "sha256": "a".repeat(64), "byteLength": "12", "mediaType": "text/markdown"},
                "route": {"kind": "Tier", "tier": "High"}, "effort": "Default"}),
            false,
        ),
    ));
    // The prompt is a workspace artifact of the user, visible to a project job.
    assert!(
        matches!(job, Ok(ops::JobChange::Job(_))),
        "{:?}",
        job.err().map(|e| e.describe())
    );
    let mut h = harness(shell(&engine, &root, Script::default()), 1440.0, 900.0);
    settle(&mut h);
    click_nav(&mut h, "All projects");
    h.query_all_by_label_contains("Engine work")
        .find(|n| {
            n.accesskit_node()
                .label()
                .is_some_and(|l| l.starts_with("Engine work"))
        })
        .expect("the project row")
        .click();
    settle(&mut h);
    click(&mut h, Role::Button, "Refresh");
    assert!(
        has_text(&h, "1 jobs · 0 enabled"),
        "the project's job status"
    );
    click(&mut h, Role::Button, "Open scheduled jobs");
    assert!(has_text(&h, "Nightly docs"));
}
