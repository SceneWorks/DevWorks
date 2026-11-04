//! Scheduled jobs over the real runtime, headless (DW-50): a scheduled inference task is listed on
//! the Scheduled jobs screen with its schedule and state; Run now records a run whose truthful
//! outcome (no inference pipeline in this build: failed before dispatch, nothing sent) is shown
//! on Runs; Disable and Enable change it through the same commands an agent submits; Delete asks
//! for confirmation first. The project sidebar on All projects shows the project's job status.
//! The create form refuses incomplete input locally. No inference.
mod common;

use common::*;
use devworks_core::ports::storage::*;
use devworks_core::protocol::records::{ArtifactRecord, JobRecord};
use devworks_core::protocol::{ActorId, Counter, Id};
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

fn record(definition: Value) -> JobRecord {
    serde_json::from_value(
        json!({"id": "job-1", "revision": "3", "title": "Nightly triage",
        "schedule": {"kind": "Calendar", "expression": "0 3 * * *", "timezone": "Europe/Paris"},
        "definition": definition, "enabled": true}),
    )
    .unwrap()
}

/// The prompt as the artifact list now shows it: a newer version than the job pinned.
fn newer_prompt() -> ArtifactRecord {
    serde_json::from_value(
        json!({"artifact": {"artifactId": "prompt-1", "versionId": "prompt-v2",
        "sha256": "b".repeat(64), "byteLength": "20", "mediaType": "text/markdown"},
        "name": "Triage new issues", "createdAtMs": "2", "scope": {"kind": "Workspace"},
        "referenceCount": "1"}),
    )
    .unwrap()
}

#[test]
fn editing_an_inference_job_keeps_every_member_the_user_did_not_change() {
    let mut original = inference();
    original["route"] = json!({"kind": "Model", "modelId": "anthropic-claude-opus"});
    original["effort"] = json!("XHigh");
    original["projectId"] = json!("project-1");
    original["featureId"] = json!("feature-1");
    let job = record(original.clone());
    let artifacts = [newer_prompt()];
    let form = JobForm::edit(&job);
    let (schedule, definition) = form_values(&form, &[], &artifacts).unwrap();
    assert_eq!(
        definition, original,
        "an untouched edit submits the definition unchanged"
    );
    assert_eq!(
        schedule,
        json!({"kind": "Calendar", "expression": "0 3 * * *", "timezone": "Europe/Paris"})
    );

    for route in [
        json!({"kind": "OpenRouterAuto"}),
        json!({"kind": "Tier", "tier": "Low"}),
    ] {
        let mut o = original.clone();
        o["route"] = route;
        o["effort"] = json!("Minimal");
        let (_, d) = form_values(&JobForm::edit(&record(o.clone())), &[], &artifacts).unwrap();
        assert_eq!(d, o, "every route and effort round-trips");
    }

    // Changing one member changes only that member.
    let mut changed = JobForm::edit(&job);
    changed.effort = 0;
    let (_, d) = form_values(&changed, &[], &artifacts).unwrap();
    let mut expected = original.clone();
    expected["effort"] = json!("Default");
    assert_eq!(d, expected);

    // Clearing the target removes it; another prompt is taken at its current version.
    let mut cleared = JobForm::edit(&job);
    cleared.target_project.clear();
    cleared.feature.clear();
    cleared.prompt = Some("prompt-1".into());
    let (_, d) = form_values(&cleared, &[], &artifacts).unwrap();
    assert!(d.get("projectId").is_none() && d.get("featureId").is_none());
    assert_eq!(
        d["prompt"]["versionId"], "prompt-v1",
        "the pinned version is kept"
    );
    let mut fresh = JobForm {
        title: "New".into(),
        prompt: Some("prompt-1".into()),
        ..JobForm::default()
    };
    let (_, d) = form_values(&fresh, &[], &artifacts).unwrap();
    assert_eq!(
        d["prompt"]["versionId"], "prompt-v2",
        "a new job takes the current version"
    );
    assert_eq!(d["route"], json!({"kind": "Tier", "tier": "High"}));
    fresh.route_kind = 1;
    assert!(
        form_values(&fresh, &[], &artifacts)
            .unwrap_err()
            .contains("model")
    );
}

#[test]
fn editing_a_native_job_keeps_its_capability_when_the_build_no_longer_offers_it() {
    let original = json!({"kind": "Native", "capabilityId": "retired-capability",
        "arguments": {"schema": {"schemaId": "devworks.retired/args", "revision": "2",
            "sha256": "c".repeat(64)}, "value": {"days": 7}}});
    let form = JobForm::edit(&record(original.clone()));
    let (_, d) = form_values(&form, &[], &[]).unwrap();
    assert_eq!(
        d, original,
        "never replaced by another (or the first) capability"
    );
}

/// The Active run section follows the job's `activeRunId`, not its latest occurrence: while an
/// occurrence skipped for the active run is the newest record, the active run and its Cancel stay
/// shown, and Cancel reaches that run.
#[test]
fn the_active_run_stays_shown_when_a_skipped_occurrence_is_the_latest_run() {
    let root = Root::new("jobs-active-run");
    let engine = start(&root);
    seed_prompt(&engine);
    let backend = engine.backend();
    let Ok(ops::JobChange::Job(job)) = ops::job_changed(backend.command(
        "jobs.create",
        &ops::job_create(
            &*backend,
            None,
            "Issue triage",
            &json!({"kind": "Manual"}),
            &inference(),
            true,
        ),
    )) else {
        panic!("created")
    };
    // An active run waiting on an approval, then an occurrence skipped for it (the newest run).
    let jid = job.id.clone();
    let (active, skipped) = run_write(
        &**engine.host().runtime().store(),
        WriteOptions::default(),
        move |tx| {
            use devworks_core::services::jobs::runner::{Occurrence, queue_run};
            let stored = tx.jobs().job(&jid)?.expect("job");
            let host = tx.store().host_actor.clone();
            let queued = queue_run(tx, &host, &stored, Occurrence::Manual { at_ms: 1 }, true)?;
            let waiting = tx.jobs().update_run(
                &StoredJobRun {
                    hold: Some("ApprovalWait".into()),
                    ..queued.clone()
                },
                queued.revision,
            )?;
            let skipped = queue_run(tx, &host, &stored, Occurrence::Manual { at_ms: 2 }, true)?;
            Ok((waiting, skipped))
        },
    )
    .unwrap();
    assert_eq!(skipped.status, "Skipped");
    let mut h = harness(shell(&engine, &root, Script::default()), 1440.0, 900.0);
    settle(&mut h);
    click_nav(&mut h, "Scheduled jobs");
    h.query_all_by_label_contains("Issue triage")
        .next()
        .expect("the job row")
        .click();
    settle(&mut h);
    assert!(has_text(&h, "Active run"));
    assert!(has_text(&h, "waiting: ApprovalWait"));
    click(&mut h, Role::Button, "Cancel");
    settle(&mut h);
    let id = active.id.clone();
    let after = read_snapshot(
        &**engine.host().runtime().store(),
        &ReadOptions::default(),
        move |s| s.jobs().job_run(&id),
    )
    .unwrap()
    .unwrap();
    assert_eq!(after.status, "Cancelled", "Cancel reached the active run");
}

/// Seeds runs of `job` occurring at `at` (the first held active, every later one skipped for it).
fn seed_runs(engine: &devworks_desktop::host::Engine, job: &Id, at: Vec<i64>) {
    let job = job.clone();
    run_write(
        &**engine.host().runtime().store(),
        WriteOptions::default(),
        move |tx| {
            use devworks_core::services::jobs::runner::{Occurrence, queue_run};
            let stored = tx.jobs().job(&job)?.expect("job");
            let host = tx.store().host_actor.clone();
            for at_ms in at {
                let run = queue_run(tx, &host, &stored, Occurrence::Manual { at_ms }, true)?;
                if run.status == "Queued" {
                    tx.jobs().update_run(
                        &StoredJobRun {
                            hold: Some("ApprovalWait".into()),
                            ..run.clone()
                        },
                        run.revision,
                    )?;
                }
            }
            Ok(())
        },
    )
    .unwrap();
}

/// Refreshing the first history page after older pages were loaded leaves no gap: the run that
/// moved past the refreshed first page is still listed, before the older ones.
#[test]
fn a_refreshed_first_history_page_keeps_the_run_at_its_boundary() {
    let root = Root::new("jobs-history-boundary");
    let engine = start(&root);
    seed_prompt(&engine);
    let backend = engine.backend();
    let Ok(ops::JobChange::Job(job)) = ops::job_changed(backend.command(
        "jobs.create",
        &ops::job_create(
            &*backend,
            None,
            "Issue triage",
            &json!({"kind": "Manual"}),
            &inference(),
            true,
        ),
    )) else {
        panic!("created")
    };
    let minute = |n: i64| 1_800_000_000_000 + n * 3_600_000;
    let page = i64::from(ops::RUN_HISTORY_PAGE);
    // One more run than a page: the oldest is on the older page.
    seed_runs(&engine, &job.id, (1..=page + 1).map(minute).collect());
    let mut h = harness(shell(&engine, &root, Script::default()), 1440.0, 6000.0);
    settle(&mut h);
    click_nav(&mut h, "Scheduled jobs");
    h.query_all_by_label_contains("Issue triage")
        .next()
        .expect("the job row")
        .click();
    settle(&mut h);
    click(&mut h, Role::Button, "Runs");
    settle(&mut h);
    let shown = |h: &egui_kittest::Harness<'_, devworks_desktop::shell::Shell>, n: i64| {
        has_text(
            h,
            &devworks_desktop::shell::jobs::local_time(Some(&Counter::new(minute(n)).unwrap())),
        )
    };
    assert!(shown(&h, 2), "the first page ends at run 2");
    click(&mut h, Role::Button, "Older runs");
    settle(&mut h);
    assert!(shown(&h, 1) && shown(&h, 2));
    // A new run moves run 2 off the first page; the refreshed page must not drop it.
    seed_runs(&engine, &job.id, vec![minute(page + 2)]);
    click(&mut h, Role::Button, "Refresh");
    settle(&mut h);
    assert!(shown(&h, page + 2), "the refreshed first page");
    assert!(shown(&h, 2), "the run at the page boundary is still listed");
    assert!(shown(&h, 1));
}

/// A project job's prompt picker offers the project's own prompts besides the workspace's.
#[test]
fn a_project_jobs_prompt_picker_offers_the_projects_prompts() {
    let root = Root::new("jobs-project-prompts");
    let engine = start(&root);
    seed_prompt(&engine);
    let backend = engine.backend();
    let project = ops::project_changed(backend.command(
        "projects.create",
        &ops::project_create(&*backend, "Payments", "work", &[]),
    ))
    .expect("project");
    let pid = project.id.clone();
    run_write(
        &**engine.host().runtime().store(),
        WriteOptions::default(),
        move |tx| {
            let digest = "b".repeat(64);
            tx.artifacts()
                .put_blob(&digest, 12, "gen-2", "blobs/bb/prompt")?;
            tx.artifacts().insert_artifact(&StoredArtifact {
                id: id("prompt-p"),
                origin_project_id: Some(pid.clone()),
                origin_work_id: None,
                title: "Payments triage".into(),
                revision: 1,
                scope: ArtifactScope::Project(pid.clone()),
                owner: None,
                retention: ArtifactRetention::Explicit,
                scope_revision: 1,
                reference_revision: 1,
                current_version_id: None,
                state: ArtifactState::Live,
                created_at_ms: 1,
                provenance: json!({}),
            })?;
            tx.artifacts().insert_version(&StoredVersion {
                id: id("prompt-p-v1"),
                artifact_id: id("prompt-p"),
                version_no: 1,
                blob_digest: digest,
                byte_count: 12,
                blob_generation: "gen-2".into(),
                media_type: "text/markdown".into(),
                created_at_ms: 1,
                deleted_at_ms: None,
                source: json!({}),
            })?;
            // Listed once it has a current version.
            let mut a = tx.artifacts().artifact(&id("prompt-p"))?.expect("artifact");
            a.current_version_id = Some(id("prompt-p-v1"));
            tx.artifacts().update_artifact(&a)
        },
    )
    .unwrap();
    let mut h = harness(shell(&engine, &root, Script::default()), 1440.0, 900.0);
    settle(&mut h);
    click_nav(&mut h, "Scheduled jobs");
    choose(&mut h, "Scope", "Payments");
    click(&mut h, Role::Button, "New job");
    settle(&mut h);
    h.get_by_role_and_label(Role::ComboBox, "Saved prompt")
        .click();
    h.step();
    h.step();
    assert!(has_text(&h, "Payments triage"), "the project's prompt");
}

/// Opens the selector `combo` and picks `option`.
fn choose(
    h: &mut egui_kittest::Harness<'_, devworks_desktop::shell::Shell>,
    combo: &str,
    option: &str,
) {
    h.get_by_role_and_label(Role::ComboBox, combo).click();
    h.step();
    h.step();
    h.query_all_by_label(option)
        .find(|n| n.accesskit_node().role() != Role::ComboBox)
        .unwrap_or_else(|| panic!("option {option} in {combo}"))
        .click_accesskit();
    settle(h);
}

/// Creating a job selects it with none of the previously selected job's history: older pages
/// loaded for job A (and their cursor) never show, or page, as the new job B's runs.
#[test]
fn creating_a_job_selects_it_without_the_previous_jobs_history() {
    let root = Root::new("jobs-create-resets-history");
    let engine = start(&root);
    seed_prompt(&engine);
    // Listed (so the form offers it) once it has a current version.
    run_write(
        &**engine.host().runtime().store(),
        WriteOptions::default(),
        |tx| {
            let mut a = tx.artifacts().artifact(&id("prompt-1"))?.expect("artifact");
            a.current_version_id = Some(id("prompt-v1"));
            tx.artifacts().update_artifact(&a)
        },
    )
    .unwrap();
    let backend = engine.backend();
    let Ok(ops::JobChange::Job(job)) = ops::job_changed(backend.command(
        "jobs.create",
        &ops::job_create(
            &*backend,
            None,
            "Issue triage",
            &json!({"kind": "Manual"}),
            &inference(),
            true,
        ),
    )) else {
        panic!("created")
    };
    let minute = |n: i64| 1_800_000_000_000 + n * 3_600_000;
    let page = i64::from(ops::RUN_HISTORY_PAGE);
    seed_runs(&engine, &job.id, (1..=page + 1).map(minute).collect());
    let mut h = harness(shell(&engine, &root, Script::default()), 1440.0, 6000.0);
    settle(&mut h);
    click_nav(&mut h, "Scheduled jobs");
    h.query_all_by_label_contains("Issue triage")
        .next()
        .expect("the job row")
        .click();
    settle(&mut h);
    click(&mut h, Role::Button, "Runs");
    settle(&mut h);
    click(&mut h, Role::Button, "Older runs");
    settle(&mut h);
    let shown = |h: &egui_kittest::Harness<'_, devworks_desktop::shell::Shell>, n: i64| {
        has_text(
            h,
            &devworks_desktop::shell::jobs::local_time(Some(&Counter::new(minute(n)).unwrap())),
        )
    };
    assert!(shown(&h, 1), "job A's older page is loaded");
    assert!(!h.state().jobs.older.is_empty());
    // Create job B from the form: it becomes the selection.
    click(&mut h, Role::Button, "New job");
    settle(&mut h);
    {
        let form = h.state_mut().jobs.form.as_mut().expect("the form");
        form.title = "Release notes".into();
        form.schedule_kind = 0;
        form.prompt = Some("prompt-1".into());
    }
    click(&mut h, Role::Button, "Save");
    settle(&mut h);
    let b = jobs(&*backend)
        .into_iter()
        .find(|j| j.title.0 == "Release notes")
        .unwrap_or_else(|| panic!("job B: {:?}", h.state().jobs.form_error));
    let state = &h.state().jobs;
    assert_eq!(state.selected.as_deref(), Some(b.id.as_str()));
    assert!(state.older.is_empty(), "no run of A in B's history");
    assert!(state.older_cursor.is_none(), "no cursor of A's history");
    assert_eq!(
        state.inspection.as_ref().map(|i| i.job.id.as_str()),
        Some(b.id.as_str())
    );
    click(&mut h, Role::Button, "Runs");
    settle(&mut h);
    for n in 1..=page + 1 {
        assert!(!shown(&h, n), "run {n} of A is not listed as B's");
    }
}

/// Every selection change (Select, a created or deleted job, a scope change, opening a
/// project's jobs) goes through one reset: a different job never keeps the previous one's
/// paging or error, and reselecting the same job keeps its loaded pages.
#[test]
fn a_selection_change_drops_the_previous_jobs_paging() {
    use devworks_desktop::shell::jobs::JobsUi;
    let mut jobs = JobsUi {
        selected: Some("a".into()),
        older_cursor: Some("cursor-of-a".into()),
        inspection_error: Some("a failed".into()),
        list_cursor: Some("list".into()),
        ..JobsUi::default()
    };
    jobs.select(Some("a".into()));
    assert_eq!(jobs.older_cursor.as_deref(), Some("cursor-of-a"));
    jobs.select(Some("b".into()));
    assert_eq!(jobs.selected.as_deref(), Some("b"));
    assert!(jobs.older_cursor.is_none() && jobs.inspection_error.is_none());
    jobs.older_cursor = Some("cursor-of-b".into());
    jobs.set_scope(Some("project".into()));
    assert!(jobs.selected.is_none() && jobs.older_cursor.is_none());
    assert!(jobs.list_cursor.is_none() && !jobs.list.loaded);
}

/// Creates a disabled manual job titled `title` through `jobs.create`.
fn create_job(backend: &dyn Backend, title: &str) {
    let created = ops::job_changed(backend.command(
        "jobs.create",
        &ops::job_create(
            backend,
            None,
            title,
            &json!({"kind": "Manual"}),
            &inference(),
            false,
        ),
    ));
    assert!(matches!(created, Ok(ops::JobChange::Job(_))), "{title}");
}

/// An automatic refresh (a runtime change hint) of a list expanded with More jobs re-reads every
/// shown page: the list keeps its later pages, the job pushed off the first page by a new one
/// stays listed, and nothing collapses to the first page.
#[test]
fn an_automatic_refresh_keeps_the_expanded_jobs_list() {
    let root = Root::new("jobs-list-refresh-pages");
    let engine = start(&root);
    seed_prompt(&engine);
    let backend = engine.backend();
    let page = usize::from(ops::PAGE_LIMIT);
    let title = |n: usize| format!("Job {n:04}");
    for n in 1..=page + 5 {
        create_job(&*backend, &title(n));
    }
    let mut h = harness(shell(&engine, &root, Script::default()), 1440.0, 9000.0);
    settle(&mut h);
    click_nav(&mut h, "Scheduled jobs");
    settle(&mut h);
    assert_eq!(h.state().jobs.list.items.len(), page);
    click(&mut h, Role::Button, "More jobs");
    settle(&mut h);
    assert_eq!(h.state().jobs.list.items.len(), page + 5);
    // A job created elsewhere sorts first: its change hint refreshes the screen.
    create_job(&*backend, &title(0));
    step_until(&mut h, "the hint-driven refresh", |s| {
        s.jobs.list.items.iter().any(|j| j.title.0 == title(0))
    });
    settle(&mut h);
    let titles: Vec<String> = h
        .state()
        .jobs
        .list
        .items
        .iter()
        .map(|j| j.title.0.clone())
        .collect();
    assert_eq!(titles, (0..=page + 5).map(title).collect::<Vec<_>>());
    assert!(!h.state().jobs.list.more && h.state().jobs.list_pages == 2);
    assert!(
        has_text(&h, &title(page)),
        "the boundary job is still shown"
    );
    assert!(
        has_text(&h, &title(page + 5)),
        "the later page is still shown"
    );
}

#[test]
fn a_refresh_while_more_jobs_is_pending_keeps_the_requested_page() {
    let root = Root::new("jobs-more-during-refresh");
    let engine = start(&root);
    seed_prompt(&engine);
    let backend = engine.backend();
    let page = usize::from(ops::PAGE_LIMIT);
    let title = |n: usize| format!("Job {n:04}");
    for n in 1..=page + 5 {
        create_job(&*backend, &title(n));
    }
    let gate = Gate::new("jobs.list")
        .with_frame_field("/body/page/cursor")
        .missing_hints();
    let mut h = harness(
        shell_with(Ok(gate.over(backend)), &root, Script::default(), None),
        1440.0,
        9000.0,
    );
    struct Release(Gate);
    impl Drop for Release {
        fn drop(&mut self) {
            self.0.open();
        }
    }
    let _release = Release(gate.clone());
    settle(&mut h);
    click_nav(&mut h, "Scheduled jobs");
    assert_eq!(h.state().jobs.list.items.len(), page);
    let generation = h.state().jobs.list_generation;
    gate.close();
    h.get_by_role_and_label(Role::Button, "More jobs").click();
    step_until(&mut h, "the pending More jobs request", |s| {
        s.client().is_some_and(|c| {
            c.activities()
                .iter()
                .any(|a| a.label == "Loading more jobs")
        })
    });
    // Refresh's first page wins the race; later pages stay held until it is applied.
    h.state_mut().load_jobs();
    step_until(&mut h, "the refreshed first page", |s| {
        s.jobs.list_generation != generation || s.jobs.list_refresh.is_some()
    });
    gate.open();
    settle(&mut h);
    assert_eq!(h.state().jobs.list.items.len(), page + 5);
    let titles: Vec<String> = h
        .state()
        .jobs
        .list
        .items
        .iter()
        .map(|j| j.title.0.clone())
        .collect();
    assert_eq!(titles, (1..=page + 5).map(title).collect::<Vec<_>>());
    assert_eq!(h.state().jobs.list_pages, 2);
    assert!(!h.state().jobs.list.more && h.state().jobs.list_refresh.is_none());
}
