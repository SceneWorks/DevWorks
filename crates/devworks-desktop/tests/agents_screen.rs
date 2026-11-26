//! The Agents screen against the real runtime: a project's coordinator and workers with their
//! role, assignment, state and current step, and the selected agent's transcript and messages.
mod common;
use common::*;
use devworks_core::ports::storage::{
    StoredAgentAssignmentContext, StoredAssignment, WriteOptions, write_job,
};
use devworks_core::protocol::{Id, ProjectId, RootId};
use devworks_core::services::runs::substrate::NewRunAssignment;
use devworks_desktop::backend::Backend;
use devworks_desktop::ops;
use devworks_desktop::route::Route;
use eframe::egui::accesskit::Role;
use egui_kittest::kittest::{NodeT, Queryable};
use serde_json::json;

fn command(b: &dyn Backend, operation: &str, input: serde_json::Value) -> serde_json::Value {
    let frame = devworks_desktop::frame::fixture::command(b, operation, input);
    let result = ops::command_result(b.command(operation, &frame))
        .unwrap_or_else(|e| panic!("{operation}: {}", e.describe()));
    serde_json::to_value(result).unwrap()["value"].clone()
}

fn session(b: &dyn Backend, project: &str, root: &str, title: &str) -> Id {
    let mut frame: serde_json::Value =
        serde_json::from_slice(&devworks_desktop::frame::fixture::command(
            b,
            "sessions.start",
            json!({"scope": {"kind": "Project", "projectId": project}, "rootId": root,
                "title": title}),
        ))
        .unwrap();
    frame["schemaRevision"] = json!(devworks_desktop::frame::latest_revision("sessions.start"));
    ops::started(b.command("sessions.start", frame.to_string().as_bytes()))
        .unwrap()
        .id
}

/// Registers the coordinator and two workers of `root` through the run substrate, each with
/// the agent context a delegation records (no agent runs in this test).
fn assign(
    engine: &devworks_desktop::host::Engine,
    project: &ProjectId,
    root: &str,
    sessions: [Id; 3],
) -> Vec<StoredAssignment> {
    let control = std::sync::Arc::new(devworks_core::services::runs::ControlService::with_scope(
        std::sync::Arc::new(devworks_desktop::platform::HostIds::new()),
        std::sync::Arc::new(devworks_core::services::runs::scope::ScriptedScope::new()),
    ));
    let (project, root) = (project.clone(), RootId(Id::new(root).unwrap()));
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    engine
        .host()
        .runtime()
        .store()
        .submit_write(
            WriteOptions::fixture(),
            write_job(
                move |tx| {
                    let host = tx.store().host_actor.clone();
                    let [lead, first, second] = sessions;
                    let new = |session: Id, parent| NewRunAssignment {
                        root: root.clone(),
                        work: root.clone(),
                        parent,
                        session,
                        scope_key: None,
                    };
                    let coordinator = control
                        .register_assignment(tx, &host, new(lead, None))?
                        .unwrap();
                    let mut out = vec![coordinator.clone()];
                    for s in [first, second] {
                        out.push(
                            control
                                .register_child_assignment(
                                    tx,
                                    &host,
                                    new(s, Some(coordinator.id.clone())),
                                )?
                                .unwrap(),
                        );
                    }
                    let now = tx.now_ms();
                    for (a, role) in out.iter().zip(["coordinator", "worker", "reviewer"]) {
                        tx.runs().insert_agent_assignment_context(
                            &StoredAgentAssignmentContext {
                                assignment_id: a.id.clone(),
                                project_id: project.clone(),
                                role_id: Id::new(role).unwrap(),
                                role_instructions: String::new(),
                                role_revisions: json!({}),
                                brief: format!("The {role} of the issue"),
                                task_ids: json!([]),
                                source_checkpoint_id: None,
                                created_at_ms: now,
                            },
                        )?;
                    }
                    Ok(out)
                },
                move |result| tx.send(result).unwrap(),
            ),
        )
        .unwrap();
    rx.recv_timeout(std::time::Duration::from_secs(30))
        .unwrap()
        .unwrap()
}

#[test]
fn the_agents_screen_lists_a_coordinator_and_workers_and_opens_their_transcripts_and_messages() {
    let root = Root::new("agents-screen");
    let engine = start(&root);
    let b = engine.backend();
    let project = ops::project_changed(b.command(
        "projects.create",
        &ops::project_create(&*b, "Shop", "", &[]),
    ))
    .unwrap()
    .id;
    let pid = project.0.as_str().to_owned();
    let backlog = devworks_core::services::work::workflow::default_status_id(&project, "backlog");
    let issue = command(
        &*b,
        "issues.create",
        json!({"projectId": pid, "title": "Crash", "description": "", "repositoryIds": [],
            "statusId": backlog, "priority": 0}),
    );
    let issue = issue["id"].as_str().unwrap().to_owned();
    let sessions = [
        session(&*b, &pid, &issue, "Lead"),
        session(&*b, &pid, &issue, "Fix"),
        session(&*b, &pid, &issue, "Review"),
    ];
    let lead_session = sessions[0].clone();
    let agents = assign(&engine, &project, &issue, sessions);
    // A person's instruction to the first worker, through the assignment message command.
    let sent = command(
        &*b,
        "agents.messages",
        json!({"recipientAssignmentId": agents[1].id, "messageClass": "Instruction",
            "content": "Reproduce the crash first", "attachments": []}),
    );
    assert!(sent["id"].is_string(), "{sent}");

    let mut h = harness(shell(&engine, &root, Script::default()), 1500.0, 1000.0);
    settle(&mut h);
    h.state_mut().select_project(Some(pid.clone()));
    h.state_mut().navigate(Route::Agents);
    settle(&mut h);
    assert!(
        !has_text(&h, "Not available in this build"),
        "the placeholder is gone"
    );
    let rows: Vec<String> = h
        .query_all_by_label_contains("  ·  ")
        .filter_map(|n| n.accesskit_node().label())
        .filter(|l| l.starts_with("Coordinator") || l.starts_with("Worker"))
        .collect();
    assert_eq!(rows.len(), 3, "{rows:?}");
    // Role, assignment work, state and the root's current step.
    assert!(
        rows.iter()
            .any(|r| r.starts_with("Coordinator  ·  coordinator  ·  ")
                && r.contains(&issue)
                && r.contains("ready")
                && r.contains("not started")),
        "{rows:?}"
    );
    assert_eq!(rows.iter().filter(|r| r.starts_with("Worker")).count(), 2);

    // Selecting the first worker shows its assignment messages.
    let worker = rows
        .iter()
        .find(|r| r.starts_with("Worker  ·  worker"))
        .unwrap()
        .clone();
    click(&mut h, Role::Button, &worker);
    assert!(
        has_text(&h, "Reproduce the crash first"),
        "the assignment message is shown"
    );
    assert!(has_text(&h, "Instruction · from You to Worker worker"));

    // Selecting the coordinator and opening its transcript opens its session in Chat.
    let coordinator = rows
        .iter()
        .find(|r| r.starts_with("Coordinator"))
        .unwrap()
        .clone();
    click(&mut h, Role::Button, &coordinator);
    click(&mut h, Role::Button, "Open transcript");
    assert_eq!(h.state().route(), Route::Chat);
    assert_eq!(
        h.state().chat.selected.as_deref(),
        Some(lead_session.as_str()),
        "the coordinator's own session is opened"
    );
}

/// A project with one issue run by a coordinator and two workers.
fn staffed(
    engine: &devworks_desktop::host::Engine,
    b: &dyn Backend,
    name: &str,
) -> (String, String, Vec<StoredAssignment>) {
    let project =
        ops::project_changed(b.command("projects.create", &ops::project_create(b, name, "", &[])))
            .unwrap()
            .id;
    let pid = project.0.as_str().to_owned();
    let backlog = devworks_core::services::work::workflow::default_status_id(&project, "backlog");
    let issue = command(
        b,
        "issues.create",
        json!({"projectId": pid, "title": format!("{name} issue"), "description": "",
            "repositoryIds": [], "statusId": backlog, "priority": 0}),
    )["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let sessions = [
        session(b, &pid, &issue, "Lead"),
        session(b, &pid, &issue, "Fix"),
        session(b, &pid, &issue, "Review"),
    ];
    let agents = assign(engine, &project, &issue, sessions);
    (pid, issue, agents)
}

/// Clicks the row whose label starts with `prefix`, read afresh (its step can change).
fn click_row(h: &mut egui_kittest::Harness<'_, devworks_desktop::shell::Shell>, prefix: &str) {
    settle(h);
    let label = rows(h)
        .into_iter()
        .find(|r| r.starts_with(prefix))
        .unwrap_or_else(|| panic!("a row {prefix}: {:?}", rows(h)));
    click(h, Role::Button, &label);
}

fn rows(h: &egui_kittest::Harness<'_, devworks_desktop::shell::Shell>) -> Vec<String> {
    h.query_all_by_label_contains("  ·  ")
        .filter_map(|n| n.accesskit_node().label())
        .filter(|l| l.starts_with("Coordinator") || l.starts_with("Worker"))
        .collect()
}

/// More messages than one page are reached with Load more; a failed first read is shown.
#[test]
fn agent_messages_page_through_load_more_and_show_a_failed_read() {
    let root = Root::new("agents-screen-messages");
    let engine = start(&root);
    let b = engine.backend();
    let (pid, _, agents) = staffed(&engine, &*b, "Shop");
    for i in 0..=ops::PAGE_LIMIT {
        command(
            &*b,
            "agents.messages",
            json!({"recipientAssignmentId": agents[1].id, "messageClass": "Status",
                "content": format!("Message {i}"), "attachments": []}),
        );
    }
    let gate = Gate::new("agents.inbox").refusing_queries();
    let mut h = harness(
        shell_with(Ok(gate.over(b.clone())), &root, Script::default(), None),
        1500.0,
        1000.0,
    );
    settle(&mut h);
    h.state_mut().select_project(Some(pid.clone()));
    h.state_mut().navigate(Route::Agents);
    settle(&mut h);
    // The first read is refused: its failure is shown, not a spinner.
    click_row(&mut h, "Worker  ·  worker  ·  ");
    assert!(has_text(&h, "Messages could not be read"));
    h.state_mut().on_exit();
    drop(h);

    let mut h = harness(shell(&engine, &root, Script::default()), 1500.0, 1000.0);
    settle(&mut h);
    h.state_mut().select_project(Some(pid));
    h.state_mut().navigate(Route::Agents);
    settle(&mut h);
    click_row(&mut h, "Worker  ·  worker  ·  ");
    assert!(
        h.query_by_role_and_label(Role::Button, "Load more")
            .is_some(),
        "more than one page of messages exists"
    );
    click(&mut h, Role::Button, "Load more");
    assert!(
        h.query_by_role_and_label(Role::Button, "Load more")
            .is_none(),
        "every message has been read"
    );
}

/// Selecting another project while Agents is shown replaces its rows at once.
#[test]
fn the_agents_screen_follows_the_selected_project() {
    let root = Root::new("agents-screen-switch");
    let engine = start(&root);
    let b = engine.backend();
    let (first, first_issue, _) = staffed(&engine, &*b, "Shop");
    let (second, second_issue, _) = staffed(&engine, &*b, "Bank");
    let mut h = harness(shell(&engine, &root, Script::default()), 1500.0, 1000.0);
    settle(&mut h);
    h.state_mut().select_project(Some(first));
    h.state_mut().navigate(Route::Agents);
    settle(&mut h);
    assert!(
        rows(&h).iter().all(|r| r.contains(&first_issue)),
        "{:?}",
        rows(&h)
    );
    h.state_mut().select_project(Some(second));
    settle(&mut h);
    let shown = rows(&h);
    assert_eq!(shown.len(), 3, "{shown:?}");
    assert!(shown.iter().all(|r| r.contains(&second_issue)), "{shown:?}");
}
