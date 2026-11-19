//! The list pane binding (DW-10): every state a bound list shows, the control that reads the next
//! page, and the conversation sidebar reaching conversations beyond the first page.
//!
//! egui_kittest exposes a Label's text as the accesskit value, not its label, so rows and
//! captions are found by value and buttons by label.
mod common;

use common::*;
use devworks_desktop::chat::{self, Chat, ChatAction};
use devworks_desktop::memory::SidebarTab;
use devworks_desktop::ops;
use devworks_desktop::paged::{ListText, Page, Paged, list_body};
use devworks_desktop::virtual_list::Scrolls;
use eframe::egui::{self, accesskit::Role};
use egui_kittest::Harness;
use egui_kittest::kittest::{NodeT, Queryable};

/// What a bound list's body reported to its caller.
#[derive(Default)]
struct Pane {
    list: Paged<String>,
    online: Option<String>,
    reload: bool,
    more: bool,
    filter: String,
    selection: Option<String>,
    scrolls: Scrolls,
}

fn pane(list: Paged<String>) -> Harness<'static, Pane> {
    let state = Pane {
        list,
        ..Pane::default()
    };
    let mut h = Harness::builder()
        .with_size(egui::vec2(500.0, 500.0))
        .build_ui_state(
            |ui, p: &mut Pane| {
                let online = p.online.clone().map_or(Ok(()), Err);
                list_body(
                    ui,
                    &online,
                    &p.list,
                    ListText {
                        what: "widgets",
                        empty_title: "No widgets",
                        empty_body: "Make one.",
                        id_salt: "widgets",
                    },
                    &mut p.reload,
                    &mut p.more,
                    &mut p.scrolls,
                    |s: &String| s.clone(),
                    |s: &String| s.clone(),
                    &p.filter.clone(),
                    &mut p.selection,
                    |ui, s: &String, selected| ui.selectable_label(selected, s).clicked(),
                );
            },
            state,
        );
    h.run_steps(3);
    h
}

fn page(items: &[&str], next: Option<&str>) -> Page<String> {
    Page::new(
        items.iter().map(|s| (*s).to_owned()).collect(),
        next.map(str::to_owned),
    )
}

fn first_page(items: &[&str], next: Option<&str>) -> Paged<String> {
    let mut list = Paged::default();
    let fetch = list.reload();
    list.apply(fetch.generation, Ok(page(items, next)));
    list
}

/// Every shown text: labels (their accesskit value) and buttons (their label).
fn shown(h: &Harness<'_, Pane>) -> Vec<String> {
    let mut out = Vec::new();
    for role in [Role::Label, Role::Button] {
        for n in h.query_all_by_role(role) {
            out.extend(n.value());
            out.extend(n.accesskit_node().label());
        }
    }
    out
}
fn has(h: &Harness<'_, Pane>, text: &str) -> bool {
    shown(h).iter().any(|s| s.contains(text))
}

#[test]
fn an_unloaded_list_says_it_is_loading_and_an_offline_one_says_why() {
    let mut list = Paged::<String>::default();
    list.reload();
    let mut h = pane(list);
    assert!(has(&h, "Loading widgets"), "{:?}", shown(&h));
    h.state_mut().online = Some("the runtime stopped".into());
    h.run_steps(3);
    assert!(has(&h, "Offline"), "{:?}", shown(&h));
}

#[test]
fn a_failed_first_read_offers_a_retry_that_asks_to_reload() {
    let mut list = Paged::<String>::default();
    let fetch = list.reload();
    list.apply(fetch.generation, Err("the store is busy".into()));
    let mut h = pane(list);
    assert!(has(&h, "Could not load widgets"), "{:?}", shown(&h));
    h.get_by_role_and_label(Role::Button, "Retry").click();
    h.run();
    assert!(h.state().reload && !h.state().more);
}

#[test]
fn an_empty_list_and_an_unmatched_filter_are_told_apart() {
    let mut h = pane(first_page(&[], None));
    assert!(has(&h, "No widgets"), "{:?}", shown(&h));
    let mut h2 = pane(first_page(&["alpha"], None));
    h2.state_mut().filter = "zzz".into();
    h2.run();
    assert!(has(&h2, "Nothing matches the filter"), "{:?}", shown(&h2));
    h.run();
}

#[test]
fn a_page_with_more_offers_load_more_and_the_last_page_does_not() {
    let mut h = pane(first_page(&["alpha", "beta"], Some("c1")));
    assert!(has(&h, "alpha") && has(&h, "2 of 2 shown · more exist"));
    h.get_by_role_and_label(Role::Button, "Load more").click();
    h.run();
    assert!(h.state().more && !h.state().reload);

    let last = pane(first_page(&["alpha"], None));
    assert!(
        last.query_by_role_and_label(Role::Button, "Load more")
            .is_none()
    );
}

#[test]
fn load_more_is_disabled_while_the_next_page_is_read() {
    let mut list = first_page(&["alpha"], Some("c1"));
    list.more_page().expect("a page follows");
    let h = pane(list);
    assert!(has(&h, "Loading more…"), "{:?}", shown(&h));
    assert!(
        h.query_by_role_and_label(Role::Button, "Load more")
            .is_none()
    );
}

#[test]
fn an_unmatched_filter_still_offers_the_pages_that_may_match() {
    let mut h = pane(first_page(&["alpha"], Some("c1")));
    h.state_mut().filter = "zzz".into();
    h.run();
    h.get_by_role_and_label(Role::Button, "Load more").click();
    h.run();
    assert!(h.state().more);
}

#[test]
fn a_failed_further_page_keeps_the_rows_and_retries_that_page() {
    let mut list = first_page(&["alpha"], Some("c1"));
    let next = list.more_page().unwrap();
    list.apply(next.generation, Err("busy".into()));
    let mut h = pane(list);
    assert!(has(&h, "alpha"), "the rows stay: {:?}", shown(&h));
    assert!(has(&h, "Showing earlier widgets"));
    h.get_by_role_and_label(Role::Button, "Retry").click();
    h.run();
    assert!(
        h.state().more && !h.state().reload,
        "retries the page that failed"
    );
}

fn record(n: usize) -> serde_json::Value {
    serde_json::json!({"id": format!("s-{n:03}"), "revision": "1", "title": format!("Chat {n:03}"),
        "contextEpoch": "0", "closed": false})
}

#[test]
fn the_sidebar_offers_older_conversations_and_asks_for_the_next_page() {
    let mut chat = Chat::new();
    chat.tab = SidebarTab::History;
    let fetch = chat.sessions.reload();
    chat.sessions.apply(
        fetch.generation,
        Ok(Page::new(
            (0..3)
                .map(|n| serde_json::from_value(record(n)).unwrap())
                .collect(),
            Some("older".into()),
        )),
    );
    let mut h = Harness::builder()
        .with_size(egui::vec2(360.0, 700.0))
        .build_ui_state(
            |ui, state: &mut (Chat, Vec<ChatAction>)| {
                chat::sidebar(ui, &mut state.0, Ok(()), &mut state.1, &mut |_, _| {});
            },
            (chat, Vec::new()),
        );
    h.run();
    assert!(
        h.query_by_value("Older conversations are not listed yet.")
            .is_none()
    );
    h.get_by_role_and_label(Role::Button, "Load more").click();
    h.run();
    assert!(
        h.state()
            .1
            .iter()
            .any(|a| matches!(a, ChatAction::MoreSessions))
    );
}

#[test]
fn conversations_beyond_the_first_page_are_reached_with_load_more() {
    let root = Root::new("paging-sessions");
    let engine = start(&root);
    let backend = engine.backend();
    let page = usize::from(ops::PAGE_LIMIT);
    for n in 0..page + 5 {
        let started = ops::started(backend.command(
            "sessions.start",
            &ops::session_start(&*backend, &format!("Chat {n:03}")),
        ));
        assert!(started.is_ok(), "{n}");
    }
    let mut h = harness(shell(&engine, &root, Script::default()), 1440.0, 900.0);
    settle(&mut h);
    h.state_mut().chat.tab = SidebarTab::History;
    settle(&mut h);
    assert_eq!(h.state().chat.sessions.items.len(), page);
    assert!(h.state().chat.sessions.more());
    click(&mut h, Role::Button, "Load more");
    step_until(&mut h, "the next page", |s| !s.chat.sessions.more());
    assert_eq!(h.state().chat.sessions.items.len(), page + 5);
    let ids: std::collections::BTreeSet<_> = h
        .state()
        .chat
        .sessions
        .items
        .iter()
        .map(|s| s.id.as_str().to_owned())
        .collect();
    assert_eq!(ids.len(), page + 5, "no conversation is listed twice");
    assert!(
        h.query_all_by_role_and_label(Role::Button, "Load more")
            .next()
            .is_none(),
        "the last page has no control"
    );
}

#[test]
fn projects_beyond_the_first_page_are_reached_with_load_more() {
    let root = Root::new("paging-projects");
    let engine = start(&root);
    let backend = engine.backend();
    let page = usize::from(ops::PAGE_LIMIT);
    for n in 0..page + 3 {
        let made = ops::project_changed(backend.command(
            "projects.create",
            &ops::project_create(&*backend, &format!("Project {n:03}"), "", &[]),
        ));
        assert!(made.is_ok(), "{n}");
    }
    let mut h = harness(shell(&engine, &root, Script::default()), 1440.0, 900.0);
    settle(&mut h);
    click_nav(&mut h, "All projects");
    step_until(&mut h, "the first page", |s| s.projects.list.loaded);
    assert_eq!(h.state().projects.list.items.len(), page);
    assert!(h.state().projects.list.more());
    click(&mut h, Role::Button, "Load more");
    step_until(&mut h, "the next page", |s| !s.projects.list.more());
    assert_eq!(h.state().projects.list.items.len(), page + 3);
}

#[test]
fn a_projects_re_read_keeps_the_pages_load_more_added() {
    let root = Root::new("paging-projects-reread");
    let engine = start(&root);
    let backend = engine.backend();
    let page = usize::from(ops::PAGE_LIMIT);
    for n in 0..page + 3 {
        let made = ops::project_changed(backend.command(
            "projects.create",
            &ops::project_create(&*backend, &format!("Project {n:03}"), "", &[]),
        ));
        assert!(made.is_ok(), "{n}");
    }
    let mut h = harness(shell(&engine, &root, Script::default()), 1440.0, 900.0);
    settle(&mut h);
    click_nav(&mut h, "All projects");
    step_until(&mut h, "the first page", |s| s.projects.list.loaded);
    // A re-read (periodic, or after a mutation) while Load more is in flight still ends with
    // every page.
    click(&mut h, Role::Button, "Load more");
    h.state_mut().load_projects();
    step_until(&mut h, "both pages", |s| {
        !s.projects.list.loading() && !s.projects.list.more()
    });
    assert_eq!(h.state().projects.list.items.len(), page + 3);
    // And a re-read after it keeps them.
    h.state_mut().load_projects();
    step_until(&mut h, "the re-read", |s| !s.projects.list.loading());
    assert_eq!(h.state().projects.list.items.len(), page + 3);
    assert!(!h.state().projects.list.more());
}

#[test]
fn an_artifacts_re_read_keeps_the_pages_load_more_added() {
    let root = Root::new("paging-artifacts-reread");
    let engine = start(&root);
    let backend = engine.backend();
    let page = usize::from(ops::PAGE_LIMIT);
    for n in 0..page + 3 {
        let bytes = format!("artifact {n:03}").into_bytes();
        let path = root.file(&format!("a-{n:03}.txt"), &bytes);
        let ticket = backend.mint_workspace_upload(&path).unwrap();
        let digest = devworks_core::fingerprint::sha256_hex(&bytes);
        let saved = ops::saved_artifact(backend.command(
            "artifacts.save",
            &ops::save_workspace_artifact(
                &*backend,
                ticket.as_str(),
                &format!("Artifact {n:03}"),
                "text/plain",
                &digest,
                bytes.len() as u64,
                &[],
            ),
        ));
        assert!(saved.is_ok(), "{n}");
    }
    let mut h = harness(shell(&engine, &root, Script::default()), 1440.0, 900.0);
    settle(&mut h);
    click_nav(&mut h, "Artifacts");
    step_until(&mut h, "the first page", |s| s.artifacts.list.loaded);
    assert_eq!(h.state().artifacts.list.items.len(), page);
    assert!(h.state().artifacts.list.more());
    click(&mut h, Role::Button, "Load more");
    step_until(&mut h, "both pages", |s| {
        !s.artifacts.list.loading() && !s.artifacts.list.more()
    });
    let all = h.state().artifacts.list.items.len();
    assert!(all > page, "{all}");
    // The periodic re-read covers every page shown, and the reply of its first page asks for
    // the second.
    h.state_mut().refresh_visible();
    step_until(&mut h, "the re-read", |s| !s.artifacts.list.loading());
    assert_eq!(h.state().artifacts.list.items.len(), all);
    assert!(!h.state().artifacts.list.more());
}

#[test]
fn a_page_from_another_snapshot_is_refused_with_its_reason_and_retries_from_the_start() {
    let mut list = Paged::default();
    let first = list.reload();
    list.apply(
        first.generation,
        Ok(Page::new(vec!["alpha".to_owned()], Some("c1".into())).fenced("catalog-1")),
    );
    let next = list.more_page().unwrap();
    let applied = list.apply(
        next.generation,
        Ok(Page::new(vec!["zeta".to_owned()], None).fenced("catalog-2")),
    );
    assert_eq!(applied, devworks_desktop::paged::Applied::Fenced);
    list.fail("The catalog changed while paging. Refresh to load a consistent list.");
    let mut h = pane(list);
    assert!(
        has(&h, "alpha"),
        "the rows read before stay: {:?}",
        shown(&h)
    );
    assert!(!has(&h, "zeta"), "the refused page never shows");
    assert!(has(&h, "The catalog changed while paging"));
    h.get_by_role_and_label(Role::Button, "Retry").click();
    h.run_steps(2);
    assert!(
        h.state().reload && !h.state().more,
        "a refused cursor is not retried: the list is read again from the first page"
    );
}

#[test]
fn a_session_change_after_load_more_keeps_the_older_pages_and_the_selection() {
    let root = Root::new("paging-sessions-mutation");
    let engine = start(&root);
    let backend = engine.backend();
    let page = usize::from(ops::PAGE_LIMIT);
    for n in 0..page + 5 {
        let started = ops::started(backend.command(
            "sessions.start",
            &ops::session_start(&*backend, &format!("Chat {n:03}")),
        ));
        assert!(started.is_ok(), "{n}");
    }
    let mut h = harness(shell(&engine, &root, Script::default()), 1440.0, 900.0);
    settle(&mut h);
    h.state_mut().chat.tab = SidebarTab::History;
    settle(&mut h);
    click(&mut h, Role::Button, "Load more");
    step_until(&mut h, "the next page", |s| !s.chat.sessions.more());
    assert_eq!(h.state().chat.sessions.items.len(), page + 5);
    // An old conversation, listed only on the second page, is the selection.
    let old = h
        .state()
        .chat
        .sessions
        .items
        .last()
        .unwrap()
        .id
        .as_str()
        .to_owned();
    h.state_mut().chat.selected = Some(old.clone());
    // A session change makes the list read again, as every started or appended message does.
    h.state_mut().chat.session_mutations += 1;
    h.state_mut().load_sessions();
    settle(&mut h);
    assert_eq!(h.state().chat.selected.as_deref(), Some(old.as_str()));
    let ids: std::collections::BTreeSet<_> = h
        .state()
        .chat
        .sessions
        .items
        .iter()
        .map(|s| s.id.as_str().to_owned())
        .collect();
    assert_eq!(
        h.state().chat.sessions.items.len(),
        page + 5,
        "every page stays"
    );
    assert_eq!(ids.len(), page + 5, "no conversation is listed twice");
    assert!(ids.contains(&old));
}
