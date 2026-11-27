//! The Resources screen against the real runtime: who holds a resource and who waits for it,
//! with each lease's owner, purpose, priority and expiry.
mod common;
use common::*;
use devworks_core::protocol::records::{LeaseRecord, LeaseState};
use devworks_core::protocol::results::{DomainDecision, OperationResult};
use devworks_core::protocol::{CommandReply, Id, ProjectId};
use devworks_desktop::backend::Backend;
use devworks_desktop::route::Route;
use devworks_desktop::{ops, widgets};
use eframe::egui::accesskit::Role;
use egui_kittest::kittest::{NodeT, Queryable};
use serde_json::json;

fn session(b: &dyn Backend, project: &str, title: &str) -> Id {
    let mut frame: serde_json::Value =
        serde_json::from_slice(&devworks_desktop::frame::fixture::command(
            b,
            "sessions.start",
            json!({"scope": {"kind": "Project", "projectId": project}, "title": title}),
        ))
        .unwrap();
    frame["schemaRevision"] = json!(devworks_desktop::frame::latest_revision("sessions.start"));
    ops::started(b.command("sessions.start", frame.to_string().as_bytes()))
        .unwrap()
        .id
}

/// Acquires `resource` from `session` of `project`, with a purpose and a priority.
fn acquire(
    engine: &devworks_desktop::host::Engine,
    project: &ProjectId,
    session: Id,
    resource: &str,
    purpose: &str,
    priority: &str,
) -> LeaseRecord {
    let host = engine.host();
    let ctx = host
        .user()
        .clone()
        .with_lineage(Some(project.clone()), None, None)
        .with_session(session);
    let b = engine.backend();
    let mut frame: serde_json::Value =
        serde_json::from_slice(&devworks_desktop::frame::fixture::command(
            &*b,
            "resources.acquire",
            json!({"resourceId": resource, "expectedRevision": "1", "units": 1,
                "leaseMs": "600000", "purpose": purpose, "priority": priority,
                "capacity": 1}),
        ))
        .unwrap();
    frame["schemaRevision"] = json!(2);
    match host
        .runtime()
        .command(&ctx, "resources.acquire", frame.to_string().as_bytes())
        .expect("acquire reply")
    {
        CommandReply::Recorded { receipt, .. } => match receipt.decision {
            DomainDecision::Applied {
                result: OperationResult::ResourcesAcquire { value },
            } => value,
            other => panic!("acquire was not applied: {other:?}"),
        },
        other => panic!("{other:?}"),
    }
}

#[test]
fn the_resources_screen_shows_each_holder_and_waiter_with_owner_purpose_priority_and_expiry() {
    let root = Root::new("resources-leases");
    let engine = start(&root);
    let b = engine.backend();
    let project = ops::project_changed(b.command(
        "projects.create",
        &ops::project_create(&*b, "Shop", "", &[]),
    ))
    .unwrap()
    .id;
    let pid = project.0.as_str().to_owned();
    let first = session(&*b, &pid, "Migrate");
    let second = session(&*b, &pid, "Seed");
    let held = acquire(
        &engine,
        &project,
        first,
        "staging-db",
        "schema migration",
        "High",
    );
    let waiting = acquire(
        &engine,
        &project,
        second,
        "staging-db",
        "seed fixtures",
        "Low",
    );
    assert!(matches!(held.state, LeaseState::Held), "{held:?}");
    assert!(matches!(waiting.state, LeaseState::Waiting), "{waiting:?}");

    let mut h = harness(shell(&engine, &root, Script::default()), 1500.0, 1000.0);
    settle(&mut h);
    h.state_mut().navigate(Route::Resources);
    settle(&mut h);
    let row = h
        .query_all_by_label_contains("staging-db  ·  capacity 1")
        .filter_map(|n| n.accesskit_node().label())
        .find(|l| l.ends_with("staging-db  ·  capacity 1  ·  1 holding  ·  1 waiting"))
        .expect("the resource row shows one holder and one waiter");
    click(&mut h, Role::Button, &row);
    step_until(&mut h, "the selected resource's leases", |s| {
        s.resources
            .leases
            .as_ref()
            .is_some_and(|l| l.holders.result().is_some() && l.waiters.result().is_some())
    });
    settle(&mut h);

    let owner = engine.host().user().principal().0.as_str().to_owned();
    // A plain label carries its text as the node's value.
    let lines: Vec<String> = h
        .query_all_by(|n| n.value().is_some_and(|v| v.starts_with(&owner)))
        .filter_map(|n| n.accesskit_node().value())
        .collect();
    let holder = format!(
        "{owner}  ·  holding  ·  for schema migration  ·  high priority  ·  1 unit  ·  expires {}",
        widgets::local_time(held.expires_at_ms.get())
    );
    let waiter = format!(
        "{owner}  ·  waiting  ·  for seed fixtures  ·  low priority  ·  1 unit  ·  waits until {}",
        widgets::local_time(waiting.expires_at_ms.get())
    );
    assert!(lines.contains(&holder), "{holder} in {lines:?}");
    assert!(lines.contains(&waiter), "{waiter} in {lines:?}");
    h.state_mut().on_exit();
}
