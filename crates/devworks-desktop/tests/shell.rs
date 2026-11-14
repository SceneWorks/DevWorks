//! The native shell over the real runtime, driven headlessly (no window or GPU) and asserted
//! through the AccessKit tree a screen reader sees. Each test boots the production composition
//! root in its own temporary data root.
mod common;

use common::*;
use devworks_core::protocol::provider::{TranscriptBody, UserContent};
use devworks_core::settings::catalog::{Category, descriptor};
use devworks_desktop::backend::Backend;
use devworks_desktop::chat::{ATTACH_LABEL, COMPOSER_LABEL, NO_COORDINATOR, SEND_LABEL};
use devworks_desktop::host::{Engine, StartError};
use devworks_desktop::notifications::CompletionCard;
use devworks_desktop::ops;
use devworks_desktop::route::{NAV_ORDER, Route};
use devworks_desktop::shell::Lifecycle;
use eframe::egui::{self, Key, Modifiers, accesskit::Role};
use egui_kittest::kittest::{NodeT, Queryable};
use serde_json::json;
use std::sync::Arc;
use std::time::{Duration, Instant};

fn type_into(
    h: &mut egui_kittest::Harness<'_, devworks_desktop::shell::Shell>,
    label: &str,
    text: &str,
) {
    h.get_by_role_and_label(Role::TextInput, label).focus();
    h.step();
    h.get_by_role_and_label(Role::TextInput, label)
        .type_text(text);
    h.step();
}

fn transcript(backend: &dyn Backend, session: &str) -> Vec<TranscriptBody> {
    ops::transcript_page(backend.query(
        "sessions.transcript",
        &ops::transcript(backend, session, None),
    ))
    .expect("transcript")
    .entries
    .into_iter()
    .map(|e| e.body)
    .collect()
}

// ---------------------------------------------------------------------------------------------
// A3 / CONTROL-08: one composition root, recovery before admission, bounded close

#[test]
fn the_window_close_runs_the_runtime_shutdown_and_releases_the_data_root() {
    let root = Root::new("lifecycle");
    let engine = start(&root);
    let report = engine.host().report();
    assert!(report.queue.requeued.is_empty() && report.queue.held.is_empty());
    // Only one process owns a data root: a second composition root is refused, and its shell
    // would run offline with that reason instead of guessing.
    match Engine::start(config(&root.0)) {
        Err(StartError::Store(e)) => assert!(format!("{e:?}").contains("DataRootInUse"), "{e:?}"),
        Err(other) => panic!("{other:?}"),
        Ok(_) => panic!("a second store opened the same data root"),
    }
    let backend = engine.backend();
    let shell = shell_with(
        Ok(backend),
        &root,
        Script::default(),
        Some(Box::new(engine)),
    );
    let mut h = harness(shell, 1440.0, 900.0);
    settle(&mut h);
    assert_eq!(h.state().lifecycle(), &Lifecycle::Open);

    // The window's close button: the close is cancelled until the runtime has shut down.
    h.input_mut()
        .viewports
        .entry(egui::ViewportId::ROOT)
        .or_default()
        .events
        .push(egui::ViewportEvent::Close);
    h.step();
    let commands = &h.output().viewport_output[&egui::ViewportId::ROOT].commands;
    assert!(
        commands.contains(&egui::ViewportCommand::CancelClose),
        "{commands:?}"
    );
    assert!(!matches!(h.state().lifecycle(), Lifecycle::Open));
    assert!(
        has_text(&h, "Closing DevWorks"),
        "a responsive progress surface"
    );
    // Failure-only bound: shutdown finishes on the engine thread long before this.
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut closed = false;
    while Instant::now() < deadline {
        h.step();
        let commands = &h.output().viewport_output[&egui::ViewportId::ROOT].commands;
        if commands.contains(&egui::ViewportCommand::Close) {
            closed = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    assert!(closed, "the window closes once the runtime stopped");
    assert_eq!(h.state().lifecycle(), &Lifecycle::Closed);
    assert!(
        h.state()
            .shutdown_summary()
            .is_some_and(|s| s.contains("Runtime stopped")),
        "{:?}",
        h.state().shutdown_summary()
    );
    assert!(
        root.memory().exists(),
        "view state is persisted while closing"
    );
    drop(h);
    // Nothing keeps running: the data root is free and the next boot has nothing to recover.
    let again = start(&root);
    let r = again.host().report();
    assert!(r.queue.requeued.is_empty() && r.queue.held.is_empty());
    assert!(r.artifacts.completed.is_empty() && r.artifacts.failed.is_empty());
}

#[test]
fn the_composition_root_reports_storage_health_through_workspace_diagnostics() {
    let root = Root::new("diagnostics");
    let engine = start(&root);
    let backend = engine.backend();
    let mut frame: serde_json::Value = serde_json::from_slice(&ops::query_frame(
        &*backend,
        "workspace.diagnostics",
        json!({"page": {"limit": 200}}),
    ))
    .unwrap();
    frame["schemaRevision"] = json!(2);
    let (result, _) =
        ops::query_result(backend.query("workspace.diagnostics", frame.to_string().as_bytes()))
            .expect("workspace.diagnostics is served");
    let value = serde_json::to_value(&result).unwrap();
    let items = value["value"]["items"]
        .as_array()
        .expect("a diagnostics page");
    let disk = items
        .iter()
        .find(|c| c["component"] == "storage.disk")
        .unwrap_or_else(|| panic!("no storage.disk in {items:#?}"));
    assert_ne!(
        disk["health"], "Unavailable",
        "the store's probe is attached: {disk:#}"
    );
}

#[test]
fn without_a_runtime_every_route_shows_a_designed_offline_state() {
    let root = Root::new("offline");
    let shell = shell_with(
        Err("another DevWorks process owns this data root.".into()),
        &root,
        Script::default(),
        None,
    );
    let mut h = harness(shell, 1440.0, 900.0);
    settle(&mut h);
    for r in Route::all() {
        h.state_mut().navigate(r);
        settle(&mut h);
        assert!(
            has_text(&h, "another DevWorks process owns this data root"),
            "{r:?}"
        );
        // Offline is never rendered as zero values.
        assert!(h.query_by_label("0 Features").is_none(), "{r:?}");
    }
    h.state_mut().navigate(Route::Chat);
    settle(&mut h);
    let send = h.get_by_role_and_label(Role::Button, SEND_LABEL);
    assert!(
        send.accesskit_node().is_disabled(),
        "nothing pretends to send"
    );
    let attach = h.get_by_role_and_label(Role::Button, ATTACH_LABEL);
    assert!(attach.accesskit_node().is_disabled());
    h.get_by_role_and_label(Role::Button, "New Chat");
}

// ---------------------------------------------------------------------------------------------
// UI-14 / UI-15: navigation, integrated headers, inner sidebars, summary strips, footer

#[test]
fn navigation_puts_approvals_below_chat_keeps_intelligence_collapsible_and_the_footer_left() {
    let root = Root::new("nav");
    let engine = start(&root);
    let mut h = harness(shell(&engine, &root, Script::default()), 1440.0, 900.0);
    settle(&mut h);
    let y = |h: &egui_kittest::Harness<'_, _>, l: &str| nav(h, l).rect().center().y;
    let order: Vec<f32> = NAV_ORDER.iter().map(|r| y(&h, r.nav_label())).collect();
    assert!(
        order.windows(2).all(|w| w[0] < w[1]),
        "navigation order {order:?}"
    );
    assert!(
        y(&h, "Approvals") - y(&h, "Chat") < 40.0,
        "Approvals sits directly below Chat"
    );
    assert!(
        h.query_by_role_and_label(Role::Button, "Activity")
            .is_none(),
        "Activity lives inside Overview"
    );
    click(&mut h, Role::Button, "Intelligence");
    assert!(!h.state().intelligence_open());
    assert!(
        h.query_by_role_and_label(Role::Button, "Knowledge")
            .is_none()
    );
    click(&mut h, Role::Button, "Intelligence");
    click_nav(&mut h, "Knowledge");
    assert_eq!(h.state().route(), Route::Knowledge);
    // The footer: version and update message, left-aligned at the bottom of the sidebar.
    let version = h.get_by_label("DevWorks 0.1.0-test").rect();
    let update = h
        .get_by_role_and_label(Role::Button, "Updates: not checked")
        .rect();
    let settings = nav(&h, "Settings").rect();
    assert!(version.top() > settings.bottom() && update.top() > version.top());
    assert!(
        update.left() < 40.0 && version.left() < 40.0,
        "{update:?} {version:?}"
    );
}

#[test]
fn every_route_has_its_integrated_header_subviews_inner_sidebar_and_honest_states() {
    let root = Root::new("routes");
    let engine = start(&root);
    let mut h = harness(shell(&engine, &root, Script::default()), 1440.0, 900.0);
    settle(&mut h);
    for r in Route::all() {
        h.state_mut().navigate(r);
        settle(&mut h);
        let screen = r.screen();
        let title = match r {
            Route::Settings => "All settings",
            _ => screen.title,
        };
        let heading = h
            .query_all_by_label(title)
            .map(|n| n.rect())
            .find(|rect| rect.left() > 206.0)
            .unwrap_or_else(|| panic!("{r:?}: header {title}"));
        for (i, sub) in screen.subviews.iter().enumerate() {
            let tab = h
                .query_all_by_role_and_label(Role::Button, sub)
                .find(|n| n.rect().left() > devworks_desktop::theme::NAV)
                .unwrap_or_else(|| panic!("{r:?}: tab {sub}"));
            assert!(
                tab.rect().top() > heading.top(),
                "{r:?}: {sub} sits in the header"
            );
            if i == 0 {
                assert!(is_on(&tab), "{r:?}: {sub} is selected");
            }
        }
        if let Some(side) = screen.inner_sidebar.filter(|_| r != Route::Chat) {
            let pane = h.get_by_label(side).rect();
            assert!(
                pane.left() > heading.right(),
                "{r:?}: {side} is on the right"
            );
            assert!(
                (pane.top() - heading.top()).abs() < 12.0,
                "{r:?}: aligned with the header"
            );
        }
        for label in screen.summary_strip {
            assert!(
                h.query_by_label(&format!("— {label}")).is_some(),
                "{r:?}: {label}"
            );
        }
        if let Some(p) = screen.pending {
            assert!(has_text(&h, "Not available in this build"), "{r:?}");
            assert!(has_text(&h, p.owner), "{r:?}: names {}", p.owner);
        }
    }
    // A header action reaches its secondary route and keeps the parent selected in navigation.
    h.state_mut().navigate(Route::Agents);
    settle(&mut h);
    click(&mut h, Role::Button, "Resource reservations");
    assert_eq!(h.state().route(), Route::Resources);
    assert!(
        has_text(&h, "desktop"),
        "the boot-registered desktop resource is listed"
    );
    // Owned browser sessions are reached from Resources and listed through browser.sessions.
    click(&mut h, Role::Button, "Browser sessions");
    assert_eq!(h.state().route(), Route::Browser);
    settle(&mut h);
    assert!(
        has_text(&h, "No browser sessions"),
        "the workspace has no browser sessions yet"
    );
}

// ---------------------------------------------------------------------------------------------
// UI-01..05: the Home chat layout, tabs, New Chat, the composer, honest coordinator state

#[test]
fn home_chat_has_current_first_history_second_and_new_chat_in_the_header() {
    let root = Root::new("home");
    let engine = start(&root);
    let backend = engine.backend();
    let mut h = harness(shell(&engine, &root, Script::default()), 1440.0, 900.0);
    settle(&mut h);
    let current = h.get_by_role_and_label(Role::Button, "Current");
    let history = h.get_by_role_and_label(Role::Button, "Chat History");
    assert!(is_on(&current), "Current is selected first");
    assert!(current.rect().left() < history.rect().left());
    let heading = h
        .get_by_role_and_label(Role::Label, "Workspace coordinator")
        .rect();
    assert!(
        current.rect().left() > heading.right(),
        "the sidebar is on the right"
    );
    assert!(has_text(&h, "What would you like to work on?"));
    assert!(has_text(&h, NO_COORDINATOR));
    assert!(has_text(&h, "Needs you") && has_text(&h, "Recently completed"));

    // A draft survives tab switches; New Chat works with either tab selected.
    type_into(&mut h, COMPOSER_LABEL, "first draft");
    click(&mut h, Role::Button, "Chat History");
    click(&mut h, Role::Button, "New Chat");
    let first = h.state().chat.selected.clone().expect("a started session");
    assert!(has_text(&h, "Conversation 1"));
    click(&mut h, Role::Button, "Current");
    assert_eq!(
        h.get_by_role_and_label(Role::TextInput, COMPOSER_LABEL)
            .value()
            .as_deref(),
        Some("first draft"),
        "the draft moved into the started conversation"
    );
    click(&mut h, Role::Button, "New Chat");
    let second = h.state().chat.selected.clone().unwrap();
    assert_ne!(first, second);
    assert_eq!(
        h.get_by_role_and_label(Role::TextInput, COMPOSER_LABEL)
            .value()
            .as_deref(),
        Some(""),
        "each conversation keeps its own draft"
    );
    // Selecting a conversation in Chat History restores its draft.
    click(&mut h, Role::Button, "Chat History");
    click(&mut h, Role::Button, "Conversation 1");
    assert_eq!(h.state().chat.selected.as_deref(), Some(first.as_str()));
    assert_eq!(
        h.get_by_role_and_label(Role::TextInput, COMPOSER_LABEL)
            .value()
            .as_deref(),
        Some("first draft")
    );
    // History search filters by title.
    type_into(&mut h, "Search conversations", "conversation 2");
    settle(&mut h);
    assert!(
        h.query_by_role_and_label(Role::Button, "Conversation 1")
            .is_none()
    );
    assert!(
        h.query_by_role_and_label(Role::Button, "Conversation 2")
            .is_some()
    );

    // Send records exactly one user message through sessions.append_message; nothing replies.
    click(&mut h, Role::Button, SEND_LABEL);
    let bodies = transcript(&*backend, &first);
    assert_eq!(bodies.len(), 1, "{bodies:?}");
    match &bodies[0] {
        TranscriptBody::UserMessage { content } => {
            assert!(matches!(&content[0], UserContent::Text { text } if text == "first draft"));
        }
        other => panic!("{other:?}"),
    }
    assert!(
        has_text(&h, "first draft"),
        "the message is shown in the transcript"
    );
    assert_eq!(
        h.get_by_role_and_label(Role::TextInput, COMPOSER_LABEL)
            .value()
            .as_deref(),
        Some(""),
        "the sent draft is cleared"
    );
    // The send shortcut works from the focused composer.
    type_into(&mut h, COMPOSER_LABEL, "by keyboard");
    h.key_press_modifiers(Modifiers::COMMAND, Key::Enter);
    settle(&mut h);
    assert_eq!(transcript(&*backend, &first).len(), 2);
    assert!(
        !transcript(&*backend, &first)
            .iter()
            .any(|b| matches!(b, TranscriptBody::AssistantAttempt { .. })),
        "no reply is fabricated"
    );
    // Coordinator instructions are inspectable in Settings (UI-05).
    click(&mut h, Role::Button, "Coordinator instructions");
    assert_eq!(h.state().route(), Route::Settings);
    assert_eq!(h.state().settings.category, Some(Category::Instructions));
    assert!(
        h.query_by_role_and_label(Role::TextInput, "Workspace coordinator instructions")
            .is_some()
    );
}

#[test]
fn the_coordinator_notice_names_the_databases_and_repositories_graph_selection() {
    let root = Root::new("screen-context");
    let engine = start(&root);
    let mut h = harness(shell(&engine, &root, Script::default()), 1440.0, 900.0);
    settle(&mut h);
    click(&mut h, Role::Button, "New Chat");
    click_nav(&mut h, "Settings");
    click(&mut h, Role::Button, "Workspace coordinator");
    let digest = "0".repeat(64);
    let node: devworks_core::protocol::records::CodeNode = serde_json::from_value(json!({
        "nodeId": "graph-node-1", "repositoryId": "repo-1", "viewId": "view-1",
        "unitKey": "unit-1", "nodeKind": "function", "name": "run", "qualifiedName": "app::run",
        "properties": [], "propertiesComplete": true, "doNotTrust": false,
        "trustRevision": "1", "viewDigest": digest, "contentIdentity": "content-1"
    }))
    .expect("code node");
    let object: devworks_core::protocol::records::SchemaObject = serde_json::from_value(json!({
        "id": "schema-object-1", "viewId": "view-1", "kind": "Table",
        "qualifiedName": "public.orders",
        "coverage": {"state": "Complete", "gaps": [], "moreGaps": false}
    }))
    .expect("schema object");

    h.state_mut().navigate(Route::Databases);
    settle(&mut h);
    h.state_mut().graph.tab = 2;
    h.state_mut().graph.selected = Some(node.clone());
    h.state_mut().databases.tab = 1;
    h.state_mut().databases.selected_object = Some(object);
    settle(&mut h);
    assert_eq!(
        h.query_all_by_label_contains("Subview: Relationships\nSelection: schema-object-1")
            .count(),
        1,
        "Databases reports its own tab and schema object, not the graph's"
    );

    h.state_mut().navigate(Route::Repositories);
    h.state_mut().graph.opened = true;
    settle(&mut h);
    assert_eq!(
        h.query_all_by_label_contains("Current screen: Repositories")
            .count(),
        1
    );
    assert_eq!(
        h.query_all_by_label_contains("Selection: graph-node-1")
            .count(),
        1,
        "Repositories with the graph open names the graph node"
    );
}

// UI-11..13: the toolbar chat icon and the app-level coordinator sidebar.
#[test]
fn the_coordinator_sidebar_is_the_home_conversation_and_stays_open_until_closed_or_home() {
    let root = Root::new("sidebar");
    let engine = start(&root);
    let backend = engine.backend();
    let mut h = harness(shell(&engine, &root, Script::default()), 1440.0, 900.0);
    settle(&mut h);
    click(&mut h, Role::Button, "New Chat");
    let session = h.state().chat.selected.clone().unwrap();
    type_into(&mut h, COMPOSER_LABEL, "shared ");
    click_nav(&mut h, "Settings");
    assert!(!h.state().coordinator_open(), "navigation never opens it");
    assert!(
        transcript(&*backend, &session).is_empty(),
        "closed-sidebar navigation appends nothing"
    );
    click(&mut h, Role::Button, "Workspace coordinator");
    assert!(h.state().coordinator_open());
    assert_eq!(
        transcript(&*backend, &session).len(),
        1,
        "opening appends current screen once"
    );
    assert!(has_text(&h, "same conversation as Chat"));
    // Same draft, same session: typing in the sidebar continues the Home draft.
    type_into(&mut h, COMPOSER_LABEL, "draft");
    click_nav(&mut h, "Artifacts");
    assert!(
        h.state().coordinator_open(),
        "it stays open across navigation"
    );
    assert_eq!(
        transcript(&*backend, &session).len(),
        2,
        "meaningful navigation appends once"
    );
    settle(&mut h);
    assert_eq!(
        transcript(&*backend, &session).len(),
        2,
        "unchanged frames add no notice"
    );
    assert_eq!(
        h.query_all_by_label_contains("Current screen: Artifacts")
            .count(),
        1
    );
    assert_eq!(
        h.query_all_by_label_contains("Current screen: Settings")
            .count(),
        0,
        "the UI projects only the latest notice"
    );
    click(&mut h, Role::Button, SEND_LABEL);
    let bodies = transcript(&*backend, &session);
    assert!(
        matches!(&bodies[..],
        [TranscriptBody::HostNotice{..},TranscriptBody::HostNotice{..},TranscriptBody::UserMessage { content }]
        if matches!(&content[0], UserContent::Text { text } if text == "shared draft")),
        "{bodies:?}"
    );
    // Returning Home closes it and shows the same conversation in the centre.
    click_nav(&mut h, "Chat");
    assert!(!h.state().coordinator_open());
    assert_eq!(
        transcript(&*backend, &session).len(),
        3,
        "returning Home adds no notice"
    );
    assert_eq!(h.state().chat.selected.as_deref(), Some(session.as_str()));
    assert!(has_text(&h, "shared draft"));
    assert_eq!(
        h.query_all_by_label_contains("Current screen: Artifacts")
            .count(),
        1,
        "Home retains only the latest recorded screen context"
    );
    // Closing explicitly preserves the session; later navigation does not reopen it.
    click_nav(&mut h, "Approvals");
    click(&mut h, Role::Button, "Workspace coordinator");
    assert_eq!(transcript(&*backend, &session).len(), 4);
    h.query_all_by_role_and_label(Role::Button, "History")
        .find(|n| n.rect().left() > devworks_desktop::theme::NAV)
        .expect("Approvals history subview")
        .click();
    settle(&mut h);
    assert_eq!(
        transcript(&*backend, &session).len(),
        5,
        "changing a subview while open appends its context"
    );
    assert_eq!(h.query_all_by_label_contains("Subview: History").count(), 1);
    type_into(&mut h, COMPOSER_LABEL, "kept");
    click(
        &mut h,
        Role::Button,
        "Close the workspace coordinator sidebar",
    );
    assert!(!h.state().coordinator_open());
    click_nav(&mut h, "Knowledge");
    assert!(!h.state().coordinator_open());
    assert_eq!(
        transcript(&*backend, &session).len(),
        5,
        "closed-sidebar browsing adds no notice"
    );
    click_nav(&mut h, "Chat");
    assert_eq!(
        h.get_by_role_and_label(Role::TextInput, COMPOSER_LABEL)
            .value()
            .as_deref(),
        Some("kept")
    );
}

// ---------------------------------------------------------------------------------------------
// UI-08 / UI-09: attachments

#[test]
fn a_pasted_image_refused_by_the_artifact_quota_keeps_its_file() {
    let root = Root::new("paste-refused");
    let engine = start(&root);
    let backend = engine.backend();
    let d = descriptor(devworks_core::settings::catalog::ARTIFACT_QUOTA_KEY).unwrap();
    let body = json!({"scope": {"kind": "Workspace"}, "expectedRevision": "1", "entries": [
        {"key": d.key, "value": {"schema": d.schema_ref(), "value": 1}}]});
    ops::settings_saved(backend.command(
        "settings.save_revision",
        &ops::command_frame(&*backend, "settings.save_revision", body),
    ))
    .expect("a one-byte quota");
    let mut h = harness(shell(&engine, &root, Script::default()), 1440.0, 900.0);
    settle(&mut h);
    click(&mut h, Role::Button, "New Chat");
    h.get_by_role_and_label(Role::TextInput, COMPOSER_LABEL)
        .focus();
    h.step();
    let pasted = egui::ColorImage::new([3, 3], vec![egui::Color32::GREEN; 9]);
    h.event(egui::Event::PasteImage(Arc::new(pasted)));
    settle(&mut h);

    let draft = h.state().chat.draft().expect("draft");
    let [a] = draft.attachments.as_slice() else {
        panic!("one attachment: {:?}", draft.attachments.len())
    };
    assert!(
        matches!(a.state, devworks_desktop::chat::AttachState::Failed(_)),
        "{:?}",
        a.state
    );
    let path = a.path.clone().expect("the paste was materialized");
    assert!(path.is_file(), "the refused paste is kept");
}

#[test]
fn attachments_from_the_dialog_a_drop_and_a_paste_are_stored_with_their_conversation() {
    let root = Root::new("attach");
    let engine = start(&root);
    let backend = engine.backend();
    let script = Script::default();
    let picture = root.file("diagram.png", &png());
    let notes = root.file("notes.md", b"# notes\n");
    let extra = root.file("extra.txt", b"remove me");
    script.files.lock().unwrap().push(vec![picture.clone()]);
    let mut h = harness(shell(&engine, &root, script), 1440.0, 900.0);
    settle(&mut h);

    // The plus-only control has its accessible name and tooltip.
    let plus = h.get_by_role_and_label(Role::Button, ATTACH_LABEL);
    assert!(plus.rect().width() < 40.0, "plus-only");
    // Attaching before any conversation exists starts one (its ID is the provenance).
    click(&mut h, Role::Button, ATTACH_LABEL);
    let session = h
        .state()
        .chat
        .selected
        .clone()
        .expect("started for the attachment");
    // Drag and drop.
    h.input_mut()
        .dropped_files
        .push(Arc::new(Dropped(notes.clone())));
    h.input_mut()
        .dropped_files
        .push(Arc::new(Dropped(extra.clone())));
    settle(&mut h);
    // Paste an image into the focused composer.
    h.get_by_role_and_label(Role::TextInput, COMPOSER_LABEL)
        .focus();
    h.step();
    let pasted = egui::ColorImage::new([3, 3], vec![egui::Color32::GREEN; 9]);
    h.event(egui::Event::PasteImage(Arc::new(pasted)));
    settle(&mut h);

    let draft = h.state().chat.draft().expect("draft");
    let names: Vec<&str> = draft.attachments.iter().map(|a| a.name.as_str()).collect();
    assert_eq!(names.len(), 4, "{names:?}");
    for a in &draft.attachments {
        assert!(
            matches!(a.state, devworks_desktop::chat::AttachState::Stored(_)),
            "{}: {:?}",
            a.name,
            a.state
        );
    }
    let image = draft
        .attachments
        .iter()
        .find(|a| a.name == "diagram.png")
        .unwrap();
    assert!(image.preview.is_some(), "image previews are decoded");
    assert!(h.query_by_label("Preview of diagram.png").is_some());
    let md = draft
        .attachments
        .iter()
        .find(|a| a.name == "notes.md")
        .unwrap();
    assert!(
        md.preview.is_none()
            && md
                .preview_note
                .as_deref()
                .is_some_and(|n| n.contains("not analyzed"))
    );
    // The pasted image was materialized only until the artifact service had its own copy.
    assert!(
        std::fs::read_dir(backend.scratch_dir())
            .map(|d| d.count())
            .unwrap_or(0)
            == 0
    );
    // Remove one before sending.
    click(&mut h, Role::Button, "Remove attachment extra.txt");
    assert_eq!(h.state().chat.draft().unwrap().attachments.len(), 3);

    // Durable copies: workspace artifacts tagged with the conversation.
    let (listed, _) =
        ops::artifacts(backend.query("artifacts.list", &ops::workspace_artifacts(&*backend)))
            .unwrap();
    let tag = ops::conversation_tag(&session).unwrap();
    let tagged = listed
        .iter()
        .filter(|a| a.tags.as_ref().is_some_and(|t| t.0.contains(&tag)))
        .count();
    assert_eq!(
        tagged, 3,
        "every kept attachment has its conversation scope; the removed one was deleted"
    );
    assert!(!listed.iter().any(|a| a.name.0 == "extra.txt"));

    // Sending associates exactly the remaining attachments with the message.
    type_into(&mut h, COMPOSER_LABEL, "see attached");
    click(&mut h, Role::Button, SEND_LABEL);
    let bodies = transcript(&*backend, &session);
    let TranscriptBody::UserMessage { content } = &bodies[0] else {
        panic!("{bodies:?}")
    };
    let attached = content
        .iter()
        .filter(|c| matches!(c, UserContent::Attachment { .. }))
        .count();
    assert_eq!(attached, 3);
    assert!(h.state().chat.draft().unwrap().attachments.is_empty());
    assert!(has_text(&h, "Attachment · image/png"));
    // The Artifacts screen lists them through artifacts.list.
    click_nav(&mut h, "Artifacts");
    assert!(has_text(&h, "diagram.png"));
    type_into(&mut h, "Filter artifacts", "notes");
    settle(&mut h);
    assert!(has_text(&h, "notes.md") && !has_text(&h, "diagram.png"));
}

// ---------------------------------------------------------------------------------------------
// SET-01/02/03/07/08: Settings

#[test]
fn settings_are_categorized_searchable_validated_saved_and_keep_unsaved_changes() {
    let root = Root::new("settings");
    let engine = start(&root);
    let backend = engine.backend();
    let script = Script::default();
    let chosen = root.dir("repos");
    let second = root.dir("repos-2");
    script
        .folders
        .lock()
        .unwrap()
        .extend([chosen.clone(), second.clone()]);
    let mut h = harness(shell(&engine, &root, script), 1440.0, 900.0);
    settle(&mut h);
    click_nav(&mut h, "Settings");
    for c in Category::ALL {
        assert!(
            h.query_by_role_and_label(Role::Button, c.label()).is_some(),
            "{c:?}"
        );
    }
    assert!(has_text(&h, "Workspace") && has_text(&h, "Revision 1"));
    // Search narrows across categories.
    type_into(&mut h, "Search settings", "base directory");
    settle(&mut h);
    let key = "repositories.baseDirectory";
    let label = descriptor(key).unwrap().label;
    assert!(h.query_by_role_and_label(Role::TextInput, label).is_some());
    assert!(h.query_by_label("Tier Max members").is_none());
    assert!(
        has_text(&h, "Used for new clones only"),
        "the adoption boundary is shown"
    );
    // The native directory chooser drafts a value; health is separate from the value.
    click(
        &mut h,
        Role::Button,
        &format!("Choose directory for {label}"),
    );
    assert!(has_text(&h, "1 unsaved change"));
    // Navigation keeps unsaved changes.
    click_nav(&mut h, "Chat");
    click_nav(&mut h, "Settings");
    assert!(has_text(&h, "1 unsaved change"));
    click(&mut h, Role::Button, "Save changes");
    assert!(has_text(&h, "Settings saved."));
    assert!(has_text(&h, "Revision 2"));
    assert!(
        has_text(&h, "Runtime health: available"),
        "the chosen directory exists"
    );
    let (_, entries) = ops::settings_entries(
        backend.query("settings.read_effective", &ops::settings_read(&*backend)),
    )
    .unwrap();
    let saved = entries.iter().find(|e| e.key.0 == key).unwrap();
    assert_eq!(saved.value.value.0, json!(chosen.display().to_string()));

    // Another surface saves meanwhile: the UI's save is refused, drafts are kept, the conflict
    // is shown, and a second save succeeds at the new revision.
    click(
        &mut h,
        Role::Button,
        &format!("Choose directory for {label}"),
    );
    let d = descriptor("delivery.maxReviewFixCycles").unwrap();
    let body = json!({"scope": {"kind": "Workspace"}, "expectedRevision": "2", "entries": [
        {"key": d.key, "value": {"schema": d.schema_ref(), "value": 5}}]});
    ops::settings_saved(backend.command(
        "settings.save_revision",
        &ops::command_frame(&*backend, "settings.save_revision", body),
    ))
    .expect("another surface saves");
    click(&mut h, Role::Button, "Save changes");
    assert!(has_text(&h, "Settings changed elsewhere"));
    assert!(
        has_text(&h, "1 unsaved change"),
        "drafts survive the conflict"
    );
    click(&mut h, Role::Button, "Save changes");
    assert!(has_text(&h, "Revision 4"));
    // The effective value's source chain comes from settings.explain_source.
    click(&mut h, Role::Button, &format!("Explain source of {label}"));
    assert!(has_text(&h, "Workspace (revision 4): "), "configured layer");
    assert!(
        has_text(&h, "Default: ~/DevWorks/Repositories"),
        "shadowed default"
    );

    // Invalid values are flagged by the shared core validation and cannot be saved.
    h.get_by_role_and_label(Role::TextInput, label).focus();
    h.key_press_modifiers(Modifiers::COMMAND, Key::A);
    h.get_by_role_and_label(Role::TextInput, label)
        .type_text("relative/repos");
    settle(&mut h);
    assert!(has_text(&h, "Use an absolute path or one starting with ~/"));
    assert!(
        h.get_by_role_and_label(Role::Button, "Save changes")
            .accesskit_node()
            .is_disabled()
    );
    click(&mut h, Role::Button, &format!("Discard change to {label}"));
    assert!(!has_text(&h, "unsaved change"));

    // Keep-awake shows its runtime health beside the configured value.
    h.get_by_role_and_label(Role::TextInput, "Search settings")
        .focus();
    h.key_press_modifiers(Modifiers::COMMAND, Key::A);
    h.get_by_role_and_label(Role::TextInput, "Search settings")
        .type_text("awake");
    settle(&mut h);
    assert!(
        has_text(&h, "Runtime health:"),
        "keep-awake shows runtime health"
    );

    // Role instructions (SET-03) are editable; roles and skills have their own surface.
    h.get_by_role_and_label(Role::TextInput, "Search settings")
        .focus();
    h.key_press_modifiers(Modifiers::COMMAND, Key::A);
    h.key_press(Key::Backspace);
    settle(&mut h);
    click(&mut h, Role::Button, "Role instructions");
    for role in [
        "Workspace coordinator instructions",
        "Code review agent instructions",
    ] {
        assert!(
            h.query_by_role_and_label(Role::TextInput, role).is_some(),
            "{role}"
        );
    }
    // Roles and skills (DW-34) replaces the earlier unavailable placeholder.
    assert!(has_text(&h, "Roles and skills"));
    assert!(!has_text(&h, "DW-14 (approvals) and DW-34 (roles)"));
    assert!(
        h.query_by_role_and_label(Role::Button, "Open Approvals")
            .is_some(),
        "agent proposals are routed to Approvals"
    );
}

// ---------------------------------------------------------------------------------------------
// Pane restoration and preserved view state

#[test]
fn view_state_is_restored_after_a_restart_and_preserved_across_navigation() {
    let root = Root::new("restore");
    let engine = start(&root);
    let script = Script::default();
    script.folders.lock().unwrap().push(root.dir("restored"));
    {
        let mut h = harness(shell(&engine, &root, script.clone()), 1440.0, 900.0);
        settle(&mut h);
        click(&mut h, Role::Button, "New Chat");
        type_into(&mut h, COMPOSER_LABEL, "unsent");
        click_nav(&mut h, "Settings");
        click(
            &mut h,
            Role::Button,
            "Choose directory for Repository base directory",
        );
        click_nav(&mut h, "Artifacts");
        type_into(&mut h, "Filter artifacts", "pdf");
        click_nav(&mut h, "Knowledge");
        click(&mut h, Role::Button, "Documents");
        click(&mut h, Role::Button, "Intelligence");
        // In-memory preservation across navigation.
        click_nav(&mut h, "Artifacts");
        assert_eq!(
            h.get_by_role_and_label(Role::TextInput, "Filter artifacts")
                .value()
                .as_deref(),
            Some("pdf")
        );
        h.state_mut().on_exit();
    }
    let mut h = harness(shell(&engine, &root, script), 1440.0, 900.0);
    settle(&mut h);
    assert_eq!(h.state().route(), Route::Artifacts);
    assert!(!h.state().intelligence_open());
    assert_eq!(h.state().route_memory(Route::Knowledge).tab, 1);
    assert_eq!(
        h.get_by_role_and_label(Role::TextInput, "Filter artifacts")
            .value()
            .as_deref(),
        Some("pdf")
    );
    click_nav(&mut h, "Chat");
    assert_eq!(
        h.get_by_role_and_label(Role::TextInput, COMPOSER_LABEL)
            .value()
            .as_deref(),
        Some("unsent"),
        "the selected conversation and its draft are restored"
    );
    click_nav(&mut h, "Settings");
    assert!(
        has_text(&h, "1 unsaved change"),
        "unsaved settings survive a restart"
    );
}

#[test]
fn scroll_positions_are_kept_across_navigation() {
    let root = Root::new("scroll");
    let engine = start(&root);
    let mut h = harness(shell(&engine, &root, Script::default()), 1200.0, 700.0);
    settle(&mut h);
    click_nav(&mut h, "Settings");
    let before = h.state().scroll_offset("settings-items").unwrap_or(0.0);
    h.get_by_role_and_label(Role::Label, "Context compaction threshold (%)")
        .scroll_to_me();
    settle(&mut h);
    let scrolled = h.state().scroll_offset("settings-items").unwrap();
    assert!(scrolled > before + 100.0, "{before} -> {scrolled}");
    click_nav(&mut h, "Usage & costs");
    click_nav(&mut h, "Settings");
    let back = h.state().scroll_offset("settings-items").unwrap();
    assert!((back - scrolled).abs() < 1.0, "{scrolled} -> {back}");
}

// ---------------------------------------------------------------------------------------------
// UI-06/07: keyboard, focus, announcements, text scaling, responsive layout

#[test]
fn keyboard_focus_moves_through_controls_and_status_changes_are_announced() {
    let root = Root::new("keyboard");
    let engine = start(&root);
    let mut h = harness(shell(&engine, &root, Script::default()), 1440.0, 900.0);
    settle(&mut h);
    let focused = |h: &egui_kittest::Harness<'_, _>| {
        h.root()
            .children_recursive()
            .find(|n| n.accesskit_node().is_focused())
            .and_then(|n| n.accesskit_node().label())
    };
    h.key_press(Key::Tab);
    settle(&mut h);
    let first = focused(&h).expect("Tab focuses a named control");
    h.key_press(Key::Tab);
    settle(&mut h);
    let second = focused(&h).expect("Tab moves focus");
    assert_ne!(first, second);
    click(&mut h, Role::Button, "New Chat");
    let status = h
        .query_all_by_label_contains("New conversation started")
        .find(|n| n.accesskit_node().role() == Role::Label)
        .expect("the status is in the tree");
    assert_eq!(
        status.accesskit_node().live(),
        egui::accesskit::Live::Polite,
        "status changes are announced politely"
    );
}

#[test]
fn layouts_keep_the_conversation_and_essential_controls_at_every_width_and_text_size() {
    let root = Root::new("responsive");
    let engine = start(&root);
    for (w, hgt, zoom) in [
        (1440.0, 900.0, 1.0),
        (1000.0, 760.0, 1.0),
        (800.0, 600.0, 1.0),
        (1440.0, 900.0, 1.5),
    ] {
        let mut h = harness(shell(&engine, &root, Script::default()), w, hgt);
        h.ctx.set_zoom_factor(zoom);
        settle(&mut h);
        let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(w, hgt) / zoom);
        for label in [
            SEND_LABEL,
            ATTACH_LABEL,
            "New Chat",
            "Workspace coordinator",
            "Settings",
        ] {
            let r = h.get_by_role_and_label(Role::Button, label).rect();
            assert!(
                screen.contains_rect(r),
                "{w}x{hgt}@{zoom}: {label} at {r:?}"
            );
        }
        let input = h
            .get_by_role_and_label(Role::TextInput, COMPOSER_LABEL)
            .rect();
        assert!(screen.contains_rect(input), "{w}x{hgt}@{zoom}: composer");
        if w / zoom < 1100.0 {
            // Narrow: Current / Chat History overlay the right edge on request.
            click(&mut h, Role::Button, "Show Current and Chat History");
            assert!(
                h.query_by_role_and_label(Role::Button, "Chat History")
                    .is_some()
            );
            assert!(screen.contains_rect(h.get_by_role_and_label(Role::Button, SEND_LABEL).rect()));
        } else {
            assert!(
                h.query_by_role_and_label(Role::Button, "Chat History")
                    .is_some()
            );
        }
    }
}

// ---------------------------------------------------------------------------------------------
// NOTIFY-05 seam: the completion overlay

#[test]
fn completion_cards_float_below_the_toolbar_without_moving_content_or_taking_focus() {
    let root = Root::new("cards");
    let engine = start(&root);
    // Synthetic presentation cards must not be replaced by unrelated background inbox hints,
    // nor by the shell's periodic completion-feed re-query, which replaces the cards with the
    // real (empty) feed once 10 seconds pass without a refresh. That wall-clock bound is easy
    // to cross: opening Settings probes the host's container runtimes, and a `docker` on PATH
    // whose daemon does not answer holds that probe for its 10-second timeout. The test plays
    // the feed itself (`reconcile` below), so the real feed is refused. Keep the real online
    // backend and original focus/layout/ack assertions.
    let backend = Gate::new("notifications.list")
        .with_frame_field("/body/completionOnly")
        .refusing_queries()
        .missing_hints()
        .over(engine.backend());
    let mut h = harness(
        shell_with(Ok(backend), &root, Script::default(), None),
        1440.0,
        900.0,
    );
    settle(&mut h);
    h.get_by_role_and_label(Role::TextInput, COMPOSER_LABEL)
        .focus();
    settle(&mut h);
    let heading = h
        .get_by_role_and_label(Role::Label, "Workspace coordinator")
        .rect();
    let send = h.get_by_role_and_label(Role::Button, SEND_LABEL).rect();
    for id in ["a", "b"] {
        h.state_mut().completions.push(CompletionCard {
            id: id.into(),
            project: "Fixture".into(),
            kind: "Issue completed".into(),
            title: format!("Card {id}"),
            at: "20:22".into(),
            link: "completed issue".into(),
        });
    }
    settle(&mut h);
    assert_eq!(
        h.get_by_role_and_label(Role::Label, "Workspace coordinator")
            .rect(),
        heading,
        "no layout shift"
    );
    assert_eq!(
        h.get_by_role_and_label(Role::Button, SEND_LABEL).rect(),
        send
    );
    assert!(
        h.get_by_role_and_label(Role::TextInput, COMPOSER_LABEL)
            .is_focused(),
        "the overlay does not take focus"
    );
    let card = h.get_by_label("Card b").rect();
    assert!(card.top() > devworks_desktop::shell::TOOLBAR && card.top() < 120.0);
    assert!(
        card.left() > 720.0 - 260.0 && card.right() < 720.0 + 260.0,
        "top centre"
    );
    assert!(
        h.query_by_label("Card a").is_none(),
        "stacked: newest on top"
    );
    click(&mut h, Role::Button, "1 more completion");
    assert!(
        h.query_by_label("Card a").is_some(),
        "expanded to every message"
    );
    click(&mut h, Role::Button, "Dismiss Card b");
    assert!(
        h.query_by_label("Card b").is_some() && h.query_by_label("Card a").is_some(),
        "presentation waits for a durable acknowledgment"
    );
    assert_eq!(h.state_mut().completions.take_dismissed(), ["b"]);
    let remaining = h
        .state()
        .completions
        .cards()
        .iter()
        .filter(|c| c.id != "b")
        .cloned()
        .collect();
    h.state_mut().completions.reconcile(remaining);
    settle(&mut h);
    assert!(h.query_by_label("Card b").is_none());
    // Navigation keeps undismissed cards.
    click_nav(&mut h, "Settings");
    assert!(h.query_by_label("Card a").is_some());
}

// ---------------------------------------------------------------------------------------------
// A2 / ARCH-02: the UI admits through the same gateway as agent tools

#[test]
fn the_shell_reaches_only_validated_operations() {
    let root = Root::new("gateway");
    let engine = start(&root);
    let backend = engine.backend();
    // A frame that fails admission is rejected by the gateway before any use case.
    assert!(matches!(
        ops::command_result(backend.command("sessions.start", b"{\"not\": \"a frame\"}")),
        Err(ops::Failure::Other(_)) | Err(ops::Failure::Rejected(_))
    ));
    // An operation owned by a later item has no use case: nothing is admitted or recorded.
    assert!(matches!(
        ops::query_result(backend.query("runs.list", b"{}")),
        Err(ops::Failure::NoUseCase(_)) | Err(ops::Failure::Other(_))
    ));
    // The user's commands are receipted like any other principal's.
    let frame = ops::session_start(&*backend, "Receipted");
    let key = serde_json::from_slice::<serde_json::Value>(&frame).unwrap()["requestKey"]
        .as_str()
        .unwrap()
        .to_owned();
    ops::started(backend.command("sessions.start", &frame)).unwrap();
    let query = json!({"protocol": ops::PROTOCOL, "queryId": "q-receipt",
        "query": {"kind": "history.command_receipt", "requestKey": key}})
    .to_string();
    // The receipt is recorded at once; its durable reply is published by the required reply
    // consumer boot started. Failure-only bound: publication takes milliseconds.
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        match ops::query_result(backend.query("history.command_receipt", query.as_bytes())) {
            Ok(_) => break,
            Err(ops::Failure::Refused(f))
                if f.target_kind
                    .as_ref()
                    .is_some_and(|k| k.0 == "receipt_reply_artifact")
                    && Instant::now() < deadline =>
            {
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(e) => panic!("{e:?}"),
        }
    }
}

#[test]
fn closing_the_window_keeps_a_live_root_or_its_existing_policy_hold() {
    let root = Root::new("close-holds-runs");
    let engine = start(&root);
    // A standalone issue root (work items arrive with later items; seeded here directly).
    let db = rusqlite::Connection::open(root.0.join("workspace.sqlite")).unwrap();
    db.busy_timeout(Duration::from_secs(5)).unwrap();
    db.execute_batch(
        "INSERT INTO projects(id,title,lifecycle,revision,scope_revision,created_at) \
         VALUES('project-a','A','Active',1,1,0); \
         INSERT INTO work_items VALUES('root-i','project-a','issue',NULL,NULL,NULL,'root-i',1,0,1,1,1,'{}');",
    )
    .unwrap();
    drop(db);
    let backend = engine.backend();
    // `runs.*` frames use the specialized command and query envelopes.
    let start_frame = |key: &str, control: &str| {
        json!({"protocol": ops::PROTOCOL, "requestKey": key, "command": {"kind": "runs.start",
            "rootId": "root-i", "expectedWorkRevision": "1", "expectedControlRevision": control}})
        .to_string()
        .into_bytes()
    };
    let started = backend.command("runs.start", &start_frame("start-1", "1"));
    assert!(
        matches!(
            started,
            Ok(devworks_core::protocol::CommandReply::Recorded { .. })
        ),
        "{started:?}"
    );
    let shell = shell_with(
        Ok(backend),
        &root,
        Script::default(),
        Some(Box::new(engine)),
    );
    let mut h = harness(shell, 1440.0, 900.0);
    settle(&mut h);
    let held = || {
        let db = rusqlite::Connection::open(root.0.join("workspace.sqlite")).unwrap();
        db.busy_timeout(Duration::from_secs(5)).unwrap();
        db.query_row(
            "SELECT count(*) FROM blockers WHERE root_id='root-i' AND cause='RestartContinue' \
             AND state='Active'",
            [],
            |r| r.get::<_, i64>(0),
        )
        .unwrap()
    };
    assert_eq!(held(), 0);
    h.state_mut().request_close();
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut held_before_persisting = None;
    while h.state().lifecycle() != &Lifecycle::Closed {
        assert!(Instant::now() < deadline, "the window closes");
        h.step();
        // Quiescing ends only after the runtime held the live work (AD 20 step 1): by the
        // time the window state is persisted, the hold is committed.
        if held_before_persisting.is_none() && h.state().lifecycle() == &Lifecycle::Persisting {
            held_before_persisting = Some(held());
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    drop(h);
    // Reopen: the root is held, and only an explicit Continue runs it again.
    let engine = start(&root);
    let backend = engine.backend();
    let inspected = backend.query(
        "runs.inspect",
        json!({"protocol": ops::PROTOCOL, "queryId": "q-1",
            "query": {"kind": "runs.inspect", "rootId": "root-i"}})
        .to_string()
        .as_bytes(),
    );
    let text = format!("{inspected:?}");
    if held_before_persisting == Some(0) {
        // The native agent executor can reach a policy hold before the window closes.
        // A stopped activation already has a durable hold and needs no restart hold.
        assert!(
            text.contains("phase: Stopped") && text.contains("cause: Policy"),
            "{text}"
        );
        assert!(!text.contains("cause: RestartContinue"), "{text}");
        return;
    }
    assert_eq!(
        held_before_persisting,
        Some(1),
        "live root held before window persistence"
    );
    assert!(
        text.contains("desired_state: Paused") && text.contains("cause: RestartContinue"),
        "{text}"
    );
    let again = format!(
        "{:?}",
        backend.command("runs.start", &start_frame("start-2", "2"))
    );
    assert!(again.contains("code: ContinueRequired"), "{again}");
}

// ---------------------------------------------------------------------------------------------
// ADDON-02 / EXT-05: Settings → Add-ons over the shared addons.* use cases

/// An unsigned bundle for a platform this host is not (it installs, is never launched).
fn foreign_bundle(dir: &std::path::Path) -> std::path::PathBuf {
    foreign_bundle_with(dir, None)
}
/// [`foreign_bundle`], optionally shipping `config` as its configuration schema document.
fn foreign_bundle_with(dir: &std::path::Path, config: Option<&[u8]>) -> std::path::PathBuf {
    foreign_bundle_for(dir, config, "riscv64gc-unknown-none-elf")
}
/// [`foreign_bundle_with`] declaring `target` (its helper is never a real executable).
fn foreign_bundle_for(
    dir: &std::path::Path,
    config: Option<&[u8]>,
    target: &str,
) -> std::path::PathBuf {
    use devworks_core::fingerprint::sha256_hex;
    let helper: &[u8] = b"not an executable for this platform";
    let help: &[u8] = b"# Foreign pack\n";
    let config_digest = config.map_or("b".repeat(64), sha256_hex);
    let mut files = vec![
        json!({"path": "bin/helper", "sha256": sha256_hex(helper), "byteLength": helper.len().to_string()}),
        json!({"path": "help.md", "sha256": sha256_hex(help), "byteLength": help.len().to_string()}),
    ];
    let mut contents: Vec<(&str, &[u8])> = vec![("bin/helper", helper), ("help.md", help)];
    let mut assets = vec![];
    if let Some(c) = config {
        files.push(json!({"path": "config.json", "sha256": sha256_hex(c), "byteLength": c.len().to_string()}));
        contents.push(("config.json", c));
        assets.push(
            json!({"reference": {"schemaId": "foreign-pack/config", "revision": "1",
            "sha256": config_digest}, "path": "config.json"}),
        );
    }
    let manifest = json!({
        "packageId": "foreign-pack", "version": "1.0.0", "publisherId": "acme",
        "protocolMin": {"major": 1, "minor": 0}, "protocolMax": {"major": 1, "minor": 0},
        "minimumHostVersion": "0.1.0", "dependencies": [], "runtimes": [], "schemaAssets": assets,
        "requestedLimits": {"maxConcurrent": 1, "queueCapacity": 4,
            "maxMemoryBytes": "1048576", "streamCreditBytes": 4096},
        "targets": [{"targetTriple": target, "entrypoint": "bin/helper"}],
        "files": files,
        "stateSchema": {"schemaId": "foreign-pack/state", "revision": "1", "sha256": "a".repeat(64)},
        "configurationSchema": {"schemaId": "foreign-pack/config", "revision": "1",
            "sha256": config_digest},
        "helpFile": "help.md", "requestedPermissions": [{"kind": "WorktreeRead"}],
        "contributions": [{"kind": "Semantic", "meta": {"id": "foreign-pack/index",
            "inputSchema": {"schemaId": "foreign-pack/in", "revision": "1", "sha256": "c".repeat(64)},
            "outputSchema": {"schemaId": "foreign-pack/out", "revision": "1", "sha256": "d".repeat(64)},
            "helpTopic": "index", "required": true, "cancellation": "Cooperative"},
            "languages": ["csharp"], "format": "Scip"}]
    })
    .to_string();
    let bytes = devworks_adapters::packages::write_bundle(manifest.as_bytes(), &contents, None);
    std::fs::create_dir_all(dir).unwrap();
    let path = dir.join("foreign-pack.dwpkg");
    std::fs::write(&path, bytes).unwrap();
    path
}

#[test]
fn the_add_ons_screen_installs_a_bundle_and_shows_availability_apart_from_enablement() {
    let root = Root::new("addons");
    let engine = start(&root);
    let dialogs = Script::default();
    let bundle = foreign_bundle(&root.0.with_extension("bundles"));
    dialogs.files.lock().unwrap().push(vec![bundle]);
    let mut h = harness(shell(&engine, &root, dialogs), 1440.0, 900.0);
    settle(&mut h);
    h.state_mut().navigate(Route::AddOns);
    settle(&mut h);
    assert!(has_text(&h, "No add-ons installed"));
    click(&mut h, Role::Button, "Install from file…");
    step_until(&mut h, "the installed pack is listed", |s| {
        s.addons
            .items
            .iter()
            .any(|r| r.id.as_str() == "foreign-pack")
    });
    settle(&mut h);
    // Configured enablement and effective availability are separate.
    let rec = h.state().addons.items[0].clone();
    assert!(rec.enabled);
    assert_eq!(format!("{:?}", rec.availability.unwrap()), "Incompatible");
    assert!(has_text(&h, "Incompatible"));
    h.state_mut().addons.selected = Some("foreign-pack".into());
    settle(&mut h);
    assert!(has_text(&h, "Language pack"));
    assert!(has_text(
        &h,
        "riscv64gc-unknown-none-elf · not this platform"
    ));
    click(&mut h, Role::Button, "Disable");
    step_until(&mut h, "the pack is disabled", |s| {
        s.addons.items.first().is_some_and(|r| !r.enabled)
    });
    settle(&mut h);
    assert_eq!(
        format!(
            "{:?}",
            h.state().addons.items[0].availability.clone().unwrap()
        ),
        "Disabled"
    );
    click(&mut h, Role::Button, "Logs");
    step_until(&mut h, "the pack's logs arrive", |s| {
        s.addons
            .logs
            .get("foreign-pack")
            .is_some_and(|l| !l.is_empty())
    });
}

#[test]
fn the_add_ons_form_shows_configuration_field_errors_from_the_packages_schema() {
    let root = Root::new("addons-config");
    let engine = start(&root);
    let dialogs = Script::default();
    let schema = br#"{"type":"object","properties":{"threads":{"type":"integer","minimum":1}}}"#;
    let bundle = foreign_bundle_with(&root.0.with_extension("bundles"), Some(schema));
    dialogs.files.lock().unwrap().push(vec![bundle]);
    // The default window size, with the install notice banner above the form.
    let mut h = harness(shell(&engine, &root, dialogs), 1440.0, 900.0);
    settle(&mut h);
    h.state_mut().navigate(Route::AddOns);
    settle(&mut h);
    click(&mut h, Role::Button, "Install from file…");
    step_until(&mut h, "the installed pack is listed", |s| {
        s.addons
            .items
            .iter()
            .any(|r| r.id.as_str() == "foreign-pack")
    });
    h.state_mut().addons.selected = Some("foreign-pack".into());
    settle(&mut h);
    h.state_mut()
        .addons
        .config_text
        .insert("foreign-pack".into(), r#"{"threads": 0}"#.into());
    assert!(
        h.state().addons.notice.is_some(),
        "a notice banner is shown"
    );
    click(&mut h, Role::DisclosureTriangle, "Configuration");
    click(&mut h, Role::Button, "Save configuration");
    step_until(&mut h, "the field errors arrive", |s| {
        s.addons.config_errors.contains_key("foreign-pack")
    });
    settle(&mut h);
    assert!(h.state().addons.config_errors["foreign-pack"].contains("/threads"));
    assert!(has_text(&h, "/threads"));
    // A value the schema accepts is applied and clears the errors.
    h.state_mut()
        .addons
        .config_text
        .insert("foreign-pack".into(), r#"{"threads": 2}"#.into());
    click(&mut h, Role::Button, "Save configuration");
    step_until(&mut h, "the errors clear", |s| {
        !s.addons.config_errors.contains_key("foreign-pack")
    });
}

#[test]
fn the_add_ons_screen_follows_every_log_page_to_the_requested_field_errors() {
    let root = Root::new("addons-log-pages");
    let engine = start(&root);
    let backend = engine.backend();
    let dialogs = Script::default();
    let schema = br#"{"type":"object","properties":{"threads":{"type":"integer","minimum":1}}}"#;
    let bundle = foreign_bundle_with(&root.0.with_extension("bundles"), Some(schema));
    dialogs.files.lock().unwrap().push(vec![bundle]);
    let mut h = harness(shell(&engine, &root, dialogs), 1440.0, 900.0);
    settle(&mut h);
    h.state_mut().navigate(Route::AddOns);
    settle(&mut h);
    click(&mut h, Role::Button, "Install from file…");
    step_until(&mut h, "the installed pack is listed", |s| {
        s.addons
            .items
            .iter()
            .any(|r| r.id.as_str() == "foreign-pack")
    });
    // More refusals than one log page holds, before the one the form waits for.
    let rec = h.state().addons.items[0].clone();
    let schema_ref = serde_json::to_value(rec.configuration_schema.clone().unwrap()).unwrap();
    for _ in 0..(devworks_desktop::ops::PAGE_LIMIT as usize + 5) {
        let frame = devworks_desktop::addons::configure_frame(
            &*backend,
            "foreign-pack",
            rec.revision.get(),
            schema_ref.clone(),
            json!({"threads": -1}),
        );
        let _ = backend.command("addons.configure", &frame);
    }
    h.state_mut().addons.selected = Some("foreign-pack".into());
    settle(&mut h);
    h.state_mut()
        .addons
        .config_text
        .insert("foreign-pack".into(), r#"{"threads": 0}"#.into());
    click(&mut h, Role::DisclosureTriangle, "Configuration");
    click(&mut h, Role::Button, "Save configuration");
    step_until(&mut h, "the newest refusal's field errors arrive", |s| {
        s.addons.config_errors.contains_key("foreign-pack")
    });
    assert!(h.state().addons.config_errors["foreign-pack"].contains("/threads"));
    // The whole log was read: every refusal is listed, newest first in the view.
    settle(&mut h);
    let logs = &h.state().addons.logs["foreign-pack"];
    let refusals = logs
        .iter()
        .filter(|d| d.code.0 == "ConfigurationInvalid")
        .count();
    assert_eq!(refusals, devworks_desktop::ops::PAGE_LIMIT as usize + 6);
}

#[test]
fn the_add_ons_catalog_follows_every_page() {
    use ed25519_dalek::SigningKey;
    let root = Root::new("addons-catalog-pages");
    let engine = start(&root);
    let backend = engine.backend();
    // A local signed publisher index offering more packs than one catalog page holds.
    let dir = root.0.with_extension("index");
    std::fs::create_dir_all(&dir).unwrap();
    let key = SigningKey::from_bytes(&[9; 32]);
    let count = devworks_desktop::ops::PAGE_LIMIT as usize + 7;
    let entries: Vec<_> = (0..count)
        .map(|n| {
            json!({"packageId": format!("offered-{n:03}"), "version": "1.0.0",
                "manifestDigest": "a".repeat(64), "bundleDigest": "b".repeat(64), "bundleBytes": 10,
                "targets": ["x86_64-unknown-linux-gnu"], "kind": "HarnessPackage", "languages": [],
                "bundle": format!("offered-{n:03}.dwpkg")})
        })
        .collect();
    let index = json!({"publisherId": "acme", "generatedAtMs": 1, "entries": entries});
    std::fs::write(
        dir.join("devworks-index.json"),
        devworks_adapters::packages::write_index(&index, &key),
    )
    .unwrap();
    let hex: String = key
        .verifying_key()
        .as_bytes()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    let d = descriptor(devworks_core::settings::catalog::PACKAGE_INDEX_SOURCES_KEY).unwrap();
    let body = json!({"scope": {"kind": "Workspace"}, "expectedRevision": "1", "entries": [
        {"key": d.key, "value": {"schema": d.schema_ref(), "value": [{"location": dir.display().to_string(), "publicKey": hex}]}}]});
    ops::settings_saved(backend.command(
        "settings.save_revision",
        &ops::command_frame(&*backend, "settings.save_revision", body),
    ))
    .expect("an index source");
    let mut h = harness(shell(&engine, &root, Script::default()), 1440.0, 900.0);
    settle(&mut h);
    h.state_mut().navigate(Route::AddOns);
    settle(&mut h);
    click(&mut h, Role::Button, "Refresh");
    step_until(&mut h, "every offered pack is listed", |s| {
        s.addons.items.len() == count
    });
}

fn prepare_offered_pack(root: &Root, backend: &dyn Backend) -> String {
    use devworks_core::ports::packages::PackageInspector;
    use ed25519_dalek::SigningKey;
    // A local signed index offering one real bundle. Its location is long on every host (a
    // deep target directory makes it longer still), so the offer row must keep its install
    // button inside the window however long the folder path is.
    let dir = root
        .0
        .join("a-publisher-index-folder-whose-path-is-wider-than-the-add-ons-detail-pane")
        .join("nested-inside-another-long-folder-name-so-the-offer-label-overflows-one-row")
        .join("offer-index");
    // Declared for this platform, so the screen offers it (its helper never starts).
    let host = devworks_adapters::packages::host_target_triple();
    let bundle = foreign_bundle_for(&dir, None, &host);
    let bytes = std::fs::read(&bundle).unwrap();
    let inspected = devworks_adapters::packages::BundleInspector
        .inspect(&bytes)
        .unwrap()
        .inspection;
    let key = SigningKey::from_bytes(&[11; 32]);
    let index = json!({"publisherId": "acme", "generatedAtMs": 1, "entries": [{
        "packageId": "foreign-pack", "version": inspected.version,
        "manifestDigest": inspected.manifest_digest,
        "bundleDigest": devworks_core::fingerprint::sha256_hex(&bytes),
        "bundleBytes": bytes.len(), "targets": [host],
        "kind": "LanguagePack", "languages": ["csharp"], "bundle": "foreign-pack.dwpkg"}]});
    std::fs::write(
        dir.join("devworks-index.json"),
        devworks_adapters::packages::write_index(&index, &key),
    )
    .unwrap();
    let hex: String = key
        .verifying_key()
        .as_bytes()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    let d = descriptor(devworks_core::settings::catalog::PACKAGE_INDEX_SOURCES_KEY).unwrap();
    let body = json!({"scope": {"kind": "Workspace"}, "expectedRevision": "1", "entries": [
        {"key": d.key, "value": {"schema": d.schema_ref(), "value": [{"location": dir.display().to_string(), "publicKey": hex}]}}]});
    ops::settings_saved(backend.command(
        "settings.save_revision",
        &ops::command_frame(backend, "settings.save_revision", body),
    ))
    .expect("an index source");
    inspected.version.to_string()
}

fn open_offered_pack(
    root: &Root,
    backend: Arc<dyn Backend>,
) -> egui_kittest::Harness<'static, devworks_desktop::shell::Shell> {
    let mut h = harness(
        shell_with(Ok(backend), root, Script::default(), None),
        1440.0,
        900.0,
    );
    settle(&mut h);
    h.state_mut().navigate(Route::AddOns);
    settle(&mut h);
    click(&mut h, Role::Button, "Refresh");
    step_until(&mut h, "the offered pack is listed", |s| {
        s.addons
            .items
            .iter()
            .any(|r| r.id.as_str() == "foreign-pack")
    });
    h.state_mut().addons.selected = Some("foreign-pack".into());
    settle(&mut h);
    h
}

fn assert_offered_pack_installed(
    h: &mut egui_kittest::Harness<'_, devworks_desktop::shell::Shell>,
    root: &Root,
) {
    step_until(h, "the offered pack is installed", |s| {
        s.addons
            .items
            .iter()
            .any(|r| r.id.as_str() == "foreign-pack" && r.trust_state.0 != "Available")
    });
    settle(h);
    let downloads = root.0.join("staging-ui").join("packages");
    assert!(
        downloads.is_dir(),
        "the bundle was downloaded through the scratch directory"
    );
    let left: Vec<_> = std::fs::read_dir(&downloads)
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    assert!(left.is_empty(), "{left:?}");
}

#[test]
fn installing_an_offered_pack_leaves_no_download_behind() {
    let root = Root::new("addons-offer-download");
    let engine = start(&root);
    let backend = engine.backend();
    let version = prepare_offered_pack(&root, &*backend);
    let mut h = open_offered_pack(&root, backend);
    click(&mut h, Role::Button, &format!("Install {version}"));
    assert_offered_pack_installed(&mut h, &root);
}

#[test]
fn offered_download_survives_lost_save_requests_and_relaunch() {
    let root = Root::new("addons-offer-unknown");
    let engine = start(&root);
    let real = engine.backend();
    let version = prepare_offered_pack(&root, &*real);
    let lossy = LoseAcks::losing_requests(real.clone(), "artifacts.save", 4);
    let mut h = open_offered_pack(&root, lossy.clone());
    click(&mut h, Role::Button, &format!("Install {version}"));
    assert_eq!(h.state().unconfirmed_count(), 1);
    assert!(h.state().addons.busy);
    let frames = lossy.frames.lock().unwrap().clone();
    assert_eq!(frames.len(), 4);
    assert!(frames.iter().all(|f| f == &frames[0]));
    h.state_mut().on_exit();
    let saved = devworks_desktop::memory::Memory::load(&root.memory());
    let devworks_desktop::backend::Slot::AddonsSave {
        download: Some(path),
        ..
    } = &saved.pending.commands[0].slot
    else {
        panic!("download ownership persisted")
    };
    assert!(path.is_file(), "uncommitted save still needs the bundle");
    drop(h);
    let seen = LoseAcks::over(real, "artifacts.save", 0);
    let mut h = harness(
        shell_with(Ok(seen.clone()), &root, Script::default(), None),
        1440.0,
        900.0,
    );
    assert_offered_pack_installed(&mut h, &root);
    assert_eq!(seen.frames.lock().unwrap().as_slice(), &[frames[0].clone()]);
}

#[test]
fn offered_download_held_past_close_is_cleaned_after_confirmation() {
    let root = Root::new("addons-offer-close");
    let engine = start(&root);
    let real = engine.backend();
    let version = prepare_offered_pack(&root, &*real);
    let seen = LoseAcks::over(real.clone(), "artifacts.save", 0);
    let gate = Gate::holding_replies("artifacts.save");
    let mut h = open_offered_pack(&root, gate.over(seen.clone()));
    gate.close();
    h.get_by_role_and_label(Role::Button, &format!("Install {version}"))
        .click();
    step_until(&mut h, "bundle save admitted", |_| {
        !seen.frames.lock().unwrap().is_empty()
    });
    h.state_mut().request_close();
    step_until(&mut h, "closed with bundle save pending", |s| {
        matches!(s.lifecycle(), Lifecycle::Closed)
    });
    let saved = devworks_desktop::memory::Memory::load(&root.memory());
    let devworks_desktop::backend::Slot::AddonsSave {
        download: Some(path),
        ..
    } = &saved.pending.commands[0].slot
    else {
        panic!("download ownership persisted")
    };
    assert!(path.is_file(), "unknown save retains its source");
    gate.open();
    drop(h);
    let again = LoseAcks::over(real, "artifacts.save", 0);
    let mut h = harness(
        shell_with(Ok(again.clone()), &root, Script::default(), None),
        1440.0,
        900.0,
    );
    assert_offered_pack_installed(&mut h, &root);
    assert_eq!(
        again.frames.lock().unwrap()[0],
        saved.pending.commands[0].frame
    );
}

#[test]
fn index_sources_keep_paths_with_spaces() {
    let root = Root::new("index-sources");
    let engine = start(&root);
    let backend = engine.backend();
    let dir = root.dir("My Packs");
    let mut h = harness(shell(&engine, &root, Script::default()), 1440.0, 900.0);
    settle(&mut h);
    click_nav(&mut h, "Settings");
    let key = devworks_core::settings::catalog::PACKAGE_INDEX_SOURCES_KEY;
    let label = descriptor(key).unwrap().label;
    type_into(&mut h, "Search settings", "index sources");
    settle(&mut h);
    click(&mut h, Role::Button, &format!("Add {label}"));
    let location = dir.display().to_string();
    let publisher = "0123456789abcdef".repeat(4);
    type_into(&mut h, &format!("{label} 1 location"), &location);
    type_into(&mut h, &format!("{label} 1 key"), &publisher);
    settle(&mut h);
    click(&mut h, Role::Button, "Save changes");
    assert!(has_text(&h, "Settings saved."));
    let (_, entries) = ops::settings_entries(
        backend.query("settings.read_effective", &ops::settings_read(&*backend)),
    )
    .unwrap();
    let saved = entries.iter().find(|e| e.key.0 == key).unwrap();
    assert_eq!(
        saved.value.value.0,
        json!([{"location": location, "publicKey": publisher}])
    );
}

// ---------------------------------------------------------------------------------------------
// Frame work only when something changed

/// A real backend whose runtime hints the test sends itself, counting what the frame loop asks
/// of it: purge snapshot publications and the time of each query of one operation.
struct Counted {
    inner: Arc<dyn Backend>,
    hints: tokio::sync::watch::Receiver<u64>,
    operation: &'static str,
    queries: std::sync::Mutex<Vec<Instant>>,
    publications: std::sync::atomic::AtomicUsize,
}
impl Counted {
    fn queries(&self) -> Vec<Instant> {
        self.queries.lock().unwrap().clone()
    }
    fn publications(&self) -> usize {
        self.publications.load(std::sync::atomic::Ordering::SeqCst)
    }
}
impl Backend for Counted {
    fn query(&self, operation: &str, frame: &[u8]) -> devworks_core::gateway::QueryOutcome {
        if operation == self.operation {
            self.queries.lock().unwrap().push(Instant::now());
        }
        self.inner.query(operation, frame)
    }
    fn command(&self, operation: &str, frame: &[u8]) -> devworks_core::gateway::CommandOutcome {
        self.inner.command(operation, frame)
    }
    fn purge_client_publish(
        &self,
        memory: devworks_desktop::memory::Memory,
        live: devworks_desktop::project_purge::Live,
        save: bool,
    ) -> Result<Option<Vec<devworks_core::ports::storage::ProjectPurgeClientScope>>, String> {
        self.publications
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.inner.purge_client_publish(memory, live, save)
    }
    fn purge_client_acknowledge(
        &self,
        scope: &devworks_core::ports::storage::ProjectPurgeClientScope,
    ) {
        self.inner.purge_client_acknowledge(scope)
    }
    fn purge_client_freeze(&self, project: &str, frozen: bool) -> Result<(), String> {
        self.inner.purge_client_freeze(project, frozen)
    }
    fn mint_workspace_upload(
        &self,
        file: &std::path::Path,
    ) -> Result<devworks_core::protocol::Id, String> {
        self.inner.mint_workspace_upload(file)
    }
    fn mint_project_upload(
        &self,
        project: &devworks_core::protocol::ProjectId,
        file: &std::path::Path,
    ) -> Result<devworks_core::protocol::Id, String> {
        self.inner.mint_project_upload(project, file)
    }
    fn keep_awake_health(&self) -> devworks_core::settings::view::Health {
        self.inner.keep_awake_health()
    }
    fn hints(&self) -> Option<tokio::sync::watch::Receiver<u64>> {
        Some(self.hints.clone())
    }
    fn set_waker(&self, waker: devworks_desktop::backend::Waker) {
        self.inner.set_waker(waker)
    }
    fn token(&self, prefix: &str) -> String {
        self.inner.token(prefix)
    }
    fn scratch_dir(&self) -> std::path::PathBuf {
        self.inner.scratch_dir()
    }
    fn read_stream(
        &self,
        stream: &devworks_core::protocol::Id,
        max: u64,
    ) -> Result<Vec<u8>, String> {
        self.inner.read_stream(stream, max)
    }
}

fn counted(
    engine: &Engine,
    operation: &'static str,
) -> (Arc<Counted>, tokio::sync::watch::Sender<u64>) {
    let (send, hints) = tokio::sync::watch::channel(0);
    let backend = Arc::new(Counted {
        inner: engine.backend(),
        hints,
        operation,
        queries: Default::default(),
        publications: Default::default(),
    });
    (backend, send)
}

#[test]
fn a_hint_burst_reloads_the_visible_list_at_most_once_a_second_and_once_after_it() {
    let root = Root::new("hint-burst");
    let engine = start(&root);
    let (backend, hints) = counted(&engine, "artifacts.list");
    let mut h = harness(
        shell_with(Ok(backend.clone()), &root, Script::default(), None),
        1440.0,
        900.0,
    );
    settle(&mut h);
    h.state_mut().navigate(Route::Artifacts);
    settle(&mut h);

    // Two and a half seconds of hints, one every frame.
    let before = backend.queries().len();
    let start = Instant::now();
    let mut last_hint = start;
    while start.elapsed() < Duration::from_millis(2500) {
        hints.send_modify(|n| *n += 1);
        last_hint = Instant::now();
        h.step();
        assert!(h.state().wait_idle());
        std::thread::sleep(Duration::from_millis(10));
    }
    let during = backend.queries()[before..].to_vec();
    assert!(
        during.len() <= 3,
        "{} reloads in 2.5 s of hints",
        during.len()
    );
    for pair in during.windows(2) {
        assert!(
            pair[1] - pair[0] >= Duration::from_millis(900),
            "{:?} between reloads",
            pair[1] - pair[0]
        );
    }
    // The last hint of the burst is still shown: one reload follows it.
    settle(&mut h);
    let after: Vec<_> = backend.queries()[before..]
        .iter()
        .copied()
        .filter(|at| *at > last_hint)
        .collect();
    assert_eq!(after.len(), 1, "{after:?}");
    h.state_mut().on_exit();
}

#[test]
fn an_idle_shell_publishes_no_purge_snapshot_until_an_editor_changes_or_a_review_opens() {
    let root = Root::new("idle-snapshot");
    let engine = start(&root);
    let (backend, _hints) = counted(&engine, "artifacts.list");
    let mut h = harness(
        shell_with(Ok(backend.clone()), &root, Script::default(), None),
        1440.0,
        900.0,
    );
    settle(&mut h);

    // Idle frames read no editor and publish nothing.
    let idle = backend.publications();
    for _ in 0..30 {
        h.step();
    }
    assert_eq!(backend.publications(), idle);

    // Typing changes the composer's draft: the snapshot follows it.
    type_into(&mut h, COMPOSER_LABEL, "a draft the purge review must see");
    assert!(backend.publications() > idle);
    for _ in 0..3 {
        h.step();
    }
    let typed = backend.publications();
    for _ in 0..30 {
        h.step();
    }
    assert_eq!(backend.publications(), typed);

    // While a purge review is open, every frame publishes the current editors.
    h.state_mut().purge.frozen = true;
    for _ in 0..5 {
        h.step();
    }
    assert!(backend.publications() >= typed + 5);
    h.state_mut().purge.frozen = false;
    h.state_mut().on_exit();
}
